//! Issue #15, item 1: a workspace has one name, the one its engine sees.
//!
//! The workspace root is reached through a symlink (the reporter's `/home`
//! -> `/data00/home`; every macOS temporary directory is under `/var` ->
//! `/private/var`). tmux starts the pane in the workspace, the kernel
//! resolves the symlink, and Claude Code files its transcript under the
//! resolved working directory with every character but ASCII letters and
//! digits replaced by `-`. The store mints the workspace id under the
//! resolved root, so the transcript the Bridge watches is the one the
//! engine writes; an id recorded under the root as configured (before the
//! root was resolved) still names the same directory.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use spawn_core::bridge::{Bridge, BridgeConfig, transcript_path};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{ENGINE, EventKind, GlobalPosition, NewEvent};
use spawn_core::id::{AgentId, BranchName, IdempotencyKey, Principal, SessionId, WorkspaceId};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{CommandQueue, Commit, EventLog, TerminalHost, WorkspaceStore};
use spawn_core::time::{ManualClock, Timestamp};
use spawn_host::{GitWorkspace, TmuxHost};

/// Serialize the external programs of this test binary (see api.rs: a fork
/// inherits the write handle of a script another thread is creating).
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A scratch directory (resolved itself, so the test's own temporary
/// directory being a link changes nothing) holding `real/ws`, the
/// workspace root, and `link` -> `real`: the root as configured is
/// `link/ws`.
struct World {
    base: PathBuf,
    repo: PathBuf,
}

impl World {
    fn new(tag: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("spawn-identity-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("real/ws")).expect("create the root");
        let base = fs::canonicalize(&base).expect("resolve the scratch directory");
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).expect("link the root");
        let checkout = base.join("repo");
        fs::create_dir_all(&checkout).expect("create the repository");
        git(&checkout, &["init", "-q", "--initial-branch=main"]);
        fs::write(checkout.join("README.md"), "base\n").expect("write");
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-qm", "base"]);
        Self {
            repo: checkout.join(".git"),
            base,
        }
    }

    /// The root as configured: through the symlink.
    fn configured(&self) -> PathBuf {
        self.base.join("link/ws")
    }

    /// The root as the kernel resolves it.
    fn resolved(&self) -> PathBuf {
        self.base.join("real/ws")
    }

    fn store(&self) -> GitWorkspace {
        GitWorkspace::new(&self.repo, self.configured())
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args([
            "-c",
            "user.name=spawn-test",
            "-c",
            "user.email=spawn@localhost",
        ])
        .args(arguments)
        .output()
        .expect("git is installed");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Claude Code's project directory for an engine whose working directory
/// is `cwd`: every character but ASCII letters and digits replaced by `-`.
fn claude_directory(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The working directory a process started in `directory` sees: what
/// Claude Code's `process.cwd()` (`getcwd`) gives.
fn seen_from(directory: &str) -> String {
    let output = Command::new("/bin/sh")
        .args(["-c", "pwd -P"])
        .current_dir(directory)
        .output()
        .expect("run pwd");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn branch() -> BranchName {
    BranchName::new("agent_one").expect("valid branch")
}

#[test]
fn a_workspace_under_a_symlinked_root_is_named_where_its_engine_writes() {
    let _exclusive = exclusive();
    let world = World::new("named");
    let store = world.store();
    store.ensure_branch(&branch(), None).expect("branch");
    let workspace = store.mount(&branch()).expect("mount");
    let resolved = world.resolved().join("agent_one");
    assert_eq!(
        Path::new(workspace.path()),
        resolved,
        "the id names the resolved directory"
    );
    // The engine starts where the id says (the pane's -c) and sees the
    // resolved directory; so does a process started through the link.
    let engine_cwd = seen_from(workspace.path());
    assert_eq!(engine_cwd, resolved.display().to_string());
    assert_eq!(
        seen_from(&world.configured().join("agent_one").display().to_string()),
        engine_cwd
    );
    let projects = world.base.join("projects").display().to_string();
    assert_eq!(
        transcript_path(&projects, &workspace, "native-1"),
        format!(
            "{projects}/{}/native-1.jsonl",
            claude_directory(&engine_cwd)
        ),
        "the Bridge watches the transcript the engine writes"
    );
    // The literal spelling would name another project directory: the test
    // means something only because the root is reached through a link.
    assert_ne!(
        claude_directory(&world.configured().join("agent_one").display().to_string()),
        claude_directory(&engine_cwd)
    );
    store.unmount(&workspace).expect("unmount");
}

#[test]
fn an_id_recorded_under_the_configured_root_still_names_the_workspace() {
    let _exclusive = exclusive();
    let world = World::new("legacy");
    let store = world.store();
    store.ensure_branch(&branch(), None).expect("branch");
    let workspace = store.mount(&branch()).expect("mount");
    // What a version that did not resolve the root recorded for it.
    let old = WorkspaceId::new(format!(
        "agent_one:{}",
        world.configured().join("agent_one").display()
    ))
    .expect("valid workspace");
    assert_ne!(old, workspace);
    assert_eq!(
        store.canonical(&old),
        workspace,
        "the same directory, its one name"
    );
    assert_eq!(store.canonical(&workspace), workspace);
    // Another directory keeps its name, and stays outside the store.
    let elsewhere = WorkspaceId::new(format!(
        "agent_one:{}",
        world.base.join("elsewhere/agent_one").display()
    ))
    .expect("valid workspace");
    assert_eq!(store.canonical(&elsewhere), elsewhere);
    assert!(
        store.commit(&elsewhere, "nope").is_err(),
        "outside the root"
    );
    // The old id checkpoints and unmounts the workspace as the new one does.
    fs::write(Path::new(workspace.path()).join("work.txt"), "work\n").expect("write");
    assert!(matches!(
        store
            .commit(&old, "work")
            .expect("commit through the old id"),
        Commit::Created(_)
    ));
    store.unmount(&old).expect("unmount through the old id");
    assert!(!Path::new(workspace.path()).exists());
}

/// Write an executable without ever holding the target open.
fn script(directory: &Path, name: &str, body: &str) -> String {
    let target = directory.join(name);
    let temporary = directory.join(format!("{name}.tmp"));
    {
        let mut file = fs::File::create(&temporary).expect("create script");
        file.write_all(body.as_bytes()).expect("write script");
        file.sync_all().expect("flush script");
    }
    let mut permissions = fs::metadata(&temporary).expect("stat").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&temporary, permissions).expect("chmod");
    fs::rename(&temporary, &target).expect("rename script");
    target.display().to_string()
}

/// A tmux server of this test's own, killed when dropped.
struct Server {
    program: String,
    socket: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn submit(store: &MemoryStore, key: &str, action: CommandAction) {
    store
        .submit(
            &Principal::new("tester").expect("valid principal"),
            &Submission {
                agent_id: AgentId::new("agent_one").expect("valid agent"),
                session_id: SessionId::new("session_one").expect("valid session"),
                idempotency_key: IdempotencyKey::new(key).expect("valid key"),
                action,
            },
        )
        .expect("submit succeeds");
}

fn events(store: &MemoryStore) -> Vec<NewEvent> {
    store
        .read_global(GlobalPosition(0), 1000)
        .expect("read succeeds")
        .into_iter()
        .map(|record| record.event)
        .collect()
}

/// End to end through the Bridge, a real tmux and a scripted engine that
/// files its transcript as Claude Code does (under its resolved working
/// directory): the engine's turn reaches the log. Before the root was
/// resolved the Bridge watched the literal directory's transcript, which
/// never received a line, and the turn was never read.
#[test]
fn the_bridge_reads_the_turn_of_an_engine_under_a_symlinked_root() {
    let _exclusive = exclusive();
    let world = World::new("bridge");
    let bin = world.base.join("bin");
    fs::create_dir_all(&bin).expect("bin");
    let projects = world.base.join("projects");
    let socket = format!("spawn-identity-{}", std::process::id());
    let server = Server {
        program: script(
            &bin,
            "tmux",
            &format!("#!/bin/sh\nexec tmux -L {socket} -f /dev/null \"$@\"\n"),
        ),
        socket,
    };
    // `spawn run -- <engine> ...`: run the engine, write its exit status.
    let runner = script(
        &bin,
        "runner",
        "#!/bin/sh\nshift 2\n\"$@\"\ncode=$?\nprintf %s \"$code\" >\"$SPAWN_EXIT_FILE\"\nexit $code\n",
    );
    // Claude Code's transcript lives under its resolved working directory.
    let engine = script(
        &bin,
        "claude",
        r#"#!/bin/bash
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=$2 ;; esac
  shift
done
dir=$PROJECTS/$(pwd -P | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
printf 'Welcome to Claude Code\n\n❯ '
n=0
while IFS= read -r line; do
  n=$((n + 1))
  at=$(date -u +%Y-%m-%dT%H:%M:%S.000Z)
  printf '{"type":"user","uuid":"u%s","timestamp":"%s","sessionId":"%s","message":{"role":"user","content":"prompt"}}\n' "$n" "$at" "$native" >>"$transcript"
  printf '{"type":"system","subtype":"turn_duration","uuid":"s%s","timestamp":"%s","sessionId":"%s","durationMs":1}\n' "$n" "$at" "$native" >>"$transcript"
  printf '\n❯ '
done
"#,
    );
    let mut config = BridgeConfig::new("tester");
    config.spawn_program = runner;
    config.claude_program = engine;
    config.claude_projects = projects.display().to_string();
    config.environment = vec![("PROJECTS".to_string(), projects.display().to_string())];
    let terminals = Arc::new(TmuxHost::local(&server.program));
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let memory = Arc::new(MemoryStore::new("shop", clock.clone()));
    let mut bridge = Bridge::new(
        memory.clone(),
        memory.clone(),
        Arc::new(world.store()),
        terminals.clone(),
        config,
    )
    .with_clock(clock);

    submit(
        &memory,
        "key_start",
        CommandAction::Start {
            engine: ENGINE.to_string(),
            prompt: "work".to_string(),
            parent_session_id: None,
            from: None,
            model: None,
        },
    );
    bridge.tick().expect("start");
    let started = events(&memory)
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::ExecutionStarted { workspace, .. } => Some(workspace),
            _ => None,
        });
    let workspace = started.expect("the execution started");
    let deadline = Instant::now() + Duration::from_secs(20);
    let turn_read = loop {
        bridge.tick().expect("tick");
        let read = events(&memory)
            .iter()
            .any(|event| matches!(event.kind, EventKind::TurnStopped { .. }));
        if read || Instant::now() > deadline {
            break read;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let transcripts: Vec<String> = events(&memory)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::NativeSessionStarted {
                transcript_path, ..
            } => transcript_path,
            _ => None,
        })
        .collect();
    let written: Vec<PathBuf> = fs::read_dir(&projects)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        turn_read,
        "the engine's turn reached the log: the Bridge watched {transcripts:?}, the engine wrote \
         under {written:?}"
    );
    assert_eq!(
        Path::new(workspace.path()),
        world.resolved().join("agent_one")
    );
    submit(&memory, "key_stop", CommandAction::Stop);
    bridge.tick().expect("stop");
    assert!(
        !terminals
            .is_alive(
                &spawn_core::id::TerminalName::new("spawn-exec_1").expect("valid terminal"),
                &workspace
            )
            .unwrap_or(true),
        "the stop killed the engine's terminal"
    );
}

/// The upgrade of a host whose root is reached through a symlink: an
/// execution started by a version that recorded the root as configured is
/// read, after the restart, through the store's name for its workspace, so
/// the Bridge finds the transcript its engine wrote under the resolved
/// directory (before, it watched the literal one, which never got a line).
#[test]
fn an_execution_recorded_before_the_upgrade_is_followed_where_its_engine_writes() {
    let _exclusive = exclusive();
    let world = World::new("upgrade");
    let store = world.store();
    store.ensure_branch(&branch(), None).expect("branch");
    let mounted = store.mount(&branch()).expect("mount");
    let old = WorkspaceId::new(format!(
        "agent_one:{}",
        world.configured().join("agent_one").display()
    ))
    .expect("valid workspace");
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let memory = Arc::new(MemoryStore::new("shop", clock.clone()));
    let execution = spawn_core::id::ExecutionId::for_command(spawn_core::id::CommandId(1));
    let terminal = spawn_core::id::TerminalName::for_execution(&execution);
    let agent = AgentId::new("agent_one").expect("valid agent");
    let session = SessionId::new("session_one").expect("valid session");
    let append = |key: &str, kind: EventKind| {
        memory
            .append(NewEvent {
                agent_id: agent.clone(),
                session_id: session.clone(),
                execution_id: Some(execution.clone()),
                source: spawn_core::event::Source::Bridge,
                source_event_id: Some(key.to_string()),
                kind,
            })
            .expect("append");
    };
    append(
        "exec_1:started",
        EventKind::ExecutionStarted {
            engine: ENGINE.to_string(),
            workspace: old.clone(),
            command_id: spawn_core::id::CommandId(1),
            fork: None,
            model: spawn_core::event::ModelProfile::Default,
            transcript_from: None,
        },
    );
    append(
        "exec_1:native",
        EventKind::NativeSessionStarted {
            engine: ENGINE.to_string(),
            native_session_id: "native-1".to_string(),
            transcript_path: None,
            engine_version: None,
        },
    );
    // The engine runs (its terminal is there) and wrote one turn where
    // Claude Code writes it.
    memory
        .start(&spawn_core::port::Launch {
            terminal: terminal.clone(),
            workspace: old.clone(),
            cwd: old.path().to_string(),
            program: "spawn".to_string(),
            arguments: Vec::new(),
            environment: Vec::new(),
        })
        .expect("terminal");
    let projects = "/projects";
    let written = format!(
        "{projects}/{}/native-1.jsonl",
        claude_directory(&seen_from(old.path()))
    );
    memory.append_file(
        &written,
        concat!(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-11-06T10:00:00.000Z","message":{"role":"user","content":"prompt"}}"#,
            "\n",
            r#"{"type":"system","subtype":"turn_duration","uuid":"s1","timestamp":"2026-11-06T10:00:01.000Z","durationMs":1}"#,
            "\n"
        ),
    );
    let mut config = BridgeConfig::new("tester");
    config.claude_projects = projects.to_string();
    let mut bridge = Bridge::new(
        memory.clone(),
        memory.clone(),
        Arc::new(world.store()),
        memory.clone(),
        config,
    )
    .with_clock(clock);
    bridge.recover().expect("recover");
    bridge.tick().expect("tick");
    assert!(
        events(&memory)
            .iter()
            .any(|event| matches!(event.kind, EventKind::TurnStopped { .. })),
        "the turn written under {} was read: {:?}",
        mounted.path(),
        events(&memory)
    );
    store.unmount(&mounted).expect("unmount");
}
