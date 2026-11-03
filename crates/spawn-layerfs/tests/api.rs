#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use spawn_layerfs::{ArtifactStore, CommitOutcome, LayerFsCli};
use spawn_process::SystemRunner;
use spawn_types::identifiers::BranchName;

fn temp_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let mut dir = std::env::temp_dir();
    // The nanosecond prefix keeps each run in a fresh directory so repeated
    // test processes never reopen a script that is still executing (ETXTBSY).
    dir.push(format!(
        "spawn-layerfs-test-{}-{}-{}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn install_fake_cli(dir: &Path) -> PathBuf {
    let path = dir.join("fake-layerfs");
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$CALL_LOG"
if [[ "$1" == "--json" && "$2" == "branch" && "$3" == "fork" ]]; then
  echo '{"name":"agent_001","head":"c1","base":"main","createdAt":"2026-01-01T00:00:00Z"}'
  exit 0
fi
if [[ "$1" == "--json" && "$2" == "sandbox" && "$3" == "create" ]]; then
  echo '{"sandbox":7}'
  exit 0
fi
if [[ "$1" == "--json" && "$2" == "workspace" && "$3" == "mount" ]]; then
  echo '{"sandbox":7,"path":"/workspace/agent","image":"ubuntu:24.04"}'
  exit 0
fi
if [[ "$1" == "--json" && "$2" == "workspace" && "$3" == "commit" ]]; then
  if [[ -f "$(dirname "$CALL_LOG")/CLEAN_MARKER" ]]; then
    echo '{"code":"NoCommit","message":"no changes to commit","detail":null}'
    exit 125
  else
    echo '{"id":"c_dirty_1","message":"m","createdAt":"2026-01-01T00:00:01Z"}'
    exit 0
  fi
fi
if [[ "$1" == "--json" && "$2" == "branch" && "$3" == "list_commits" ]]; then
  echo '{"items":[{"id":"c_dirty_1","message":"m","createdAt":"2026-01-01T00:00:01Z"}],"hasMore":false}'
  exit 0
fi
if [[ "$1" == "--json" && "$2" == "branch" && "$3" == "merge" ]]; then
  echo '{"code":"Conflict","message":"content conflict","detail":{"conflicts":[{"path":"a.txt","kind":"content"}]}}'
  exit 125
fi
echo "unexpected invocation: $*" >&2
exit 99
"#;
    atomic_executable(&path, script);
    path
}

// Write to a sibling temp file, mark it executable, then atomically rename it
// into place. The final path is never open for writing when the runner execs
// it, which avoids `ETXTBSY` under concurrent or repeated test runs.
fn atomic_executable(path: &Path, contents: &str) {
    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.sync_all().unwrap();
    }
    let mut permissions = std::fs::metadata(&tmp).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&tmp, permissions).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn client(dir: &Path) -> LayerFsCli<SystemRunner> {
    let call_log = dir.join("calls.log");
    let executable = install_fake_cli(dir);
    // The fake reads CALL_LOG from the environment; inject it through a wrapper.
    let wrapper = dir.join("layerfs-env");
    let wrapper_script = format!(
        "#!/usr/bin/env bash\nexport CALL_LOG={:?}\nexec {:?} \"$@\"\n",
        call_log.to_str().unwrap(),
        executable.to_str().unwrap()
    );
    atomic_executable(&wrapper, &wrapper_script);
    LayerFsCli::with_runner(wrapper.to_str().unwrap(), SystemRunner)
}

#[test]
fn parses_fork_mount_commit_and_history() {
    let dir = temp_dir();
    let store = client(&dir);
    let source = BranchName::new("main").unwrap();
    let target = BranchName::new("agent_001").unwrap();

    let fork = store.ensure_branch("demo", &source, &target).unwrap();
    assert!(matches!(fork, spawn_layerfs::BranchForkOutcome::Forked(_)));

    let sandbox = store.create_sandbox("demo", "ubuntu:24.04").unwrap();
    assert_eq!(sandbox.value(), 7);

    let mount = store
        .mount("demo", &target, sandbox, "/workspace/agent")
        .unwrap();
    assert_eq!(mount.mount_id().as_str(), "7:/workspace/agent");

    let committed = store.commit(&mount, "checkpoint").unwrap();
    assert!(matches!(committed, CommitOutcome::Committed(_)));

    let page = store.list_commits("demo", &target, 50, 0).unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(!page.has_more);

    let merge = store.merge("demo", &target, &source).unwrap();
    match merge {
        spawn_layerfs::MergeOutcome::Conflict { paths } => assert_eq!(paths, vec!["a.txt"]),
        spawn_layerfs::MergeOutcome::Merged(commit) => panic!("expected conflict, got {commit:?}"),
    }

    let logged = std::fs::read_to_string(dir.join("calls.log")).unwrap();
    assert!(logged.contains("branch fork demo"));
    assert!(logged.contains("--json workspace commit"));
}

#[test]
fn no_changes_commit_is_an_empty_outcome() {
    let dir = temp_dir();
    let store = client(&dir);
    let branch = BranchName::new("agent_001").unwrap();
    let sandbox = store.create_sandbox("demo", "ubuntu").unwrap();
    let mount = store
        .mount("demo", &branch, sandbox, "/workspace/agent")
        .unwrap();
    // Per-directory marker keeps the clean/dirty switch local instead of using
    // a process-global env var that races with the parallel dirty-commit test.
    std::fs::write(dir.join("CLEAN_MARKER"), b"").unwrap();
    let outcome = store.commit(&mount, "checkpoint").unwrap();
    assert_eq!(outcome, CommitOutcome::NoChanges);
}
