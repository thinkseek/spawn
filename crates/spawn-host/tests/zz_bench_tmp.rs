use std::fs;
use std::path::PathBuf;
use std::process::Command;
use spawn_core::id::BranchName;
use spawn_core::port::{Commit, WorkspaceStore};
use spawn_host::GitWorkspace;

#[test]
#[ignore]
fn bench_unchanged_checkpoint() {
    let base = std::env::temp_dir().join(format!("spawn-bench-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let checkout = base.join("repo");
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = Command::new("git").args(["clone", "-q", "--no-local"]).arg("/home/ckl/.local/share/spawn-rsi/ws/.sources/spawn-b7fb2686353ed291.git").arg(&checkout).status().unwrap();
    assert!(status.success());
    let _ = src;
    let root = base.join("work");
    fs::create_dir_all(&root).unwrap();
    let store = GitWorkspace::new(checkout.join(".git"), &root);
    let branch = BranchName::new("agent_bench").unwrap();
    store.ensure_branch(&branch, None).unwrap();
    let workspace = store.mount(&branch).unwrap();
    let directory = PathBuf::from(workspace.path());
    fs::write(directory.join("x.txt"), "x").unwrap();
    let _ = store.commit(&workspace, "first").unwrap();
    let n = 200;
    let start = std::time::Instant::now();
    for _ in 0..n {
        assert_eq!(store.commit(&workspace, "again").unwrap(), Commit::Unchanged);
    }
    let unchanged = start.elapsed() / n;
    let start = std::time::Instant::now();
    for i in 0..n {
        fs::write(directory.join("x.txt"), format!("{i}")).unwrap();
        assert!(matches!(store.commit(&workspace, "edit").unwrap(), Commit::Created(_)));
    }
    let changed = start.elapsed() / n;
    eprintln!("BENCH unchanged {unchanged:?} changed {changed:?}");
    let _ = fs::remove_dir_all(&base);
}
