//! Issue #11, through the Bridge: main's copies for a conflict markers
//! cannot show (`.spawn/main`, never committed) outlive the remount a resume
//! does. The engine is lost after the copies were made; the Bridge's resume
//! merges main only when it is clean (it is not) and mounts the branch
//! afresh, and the copies the persisted note names are there again. The
//! Bridge types into the engine while it holds the workspace's lock (issue
//! #7), which a real git workspace takes: the start's prompt and the
//! resume's are typed, so the hold and the store's own use of the lock do
//! not deadlock.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use spawn_core::bridge::{Bridge, BridgeConfig};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{ENGINE, GlobalPosition, NewEvent};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
    WorkspaceId,
};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{BaseMerge, CommandQueue, EventLog, WorkspaceStore};
use spawn_core::time::{ManualClock, Timestamp};
use spawn_host::{GitWorkspace, MAIN_COPIES};

/// A ready pane: input box empty, status line below it.
const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = spawn_host::test_git_command()
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
        .read_global(GlobalPosition(0), 500)
        .expect("read succeeds")
        .into_iter()
        .map(|record| record.event)
        .collect()
}

#[test]
fn mains_copies_are_back_after_a_resume_remounts_the_workspace() {
    let base = std::env::temp_dir().join(format!("spawn-resume-copies-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let checkout = base.join("repo");
    fs::create_dir_all(&checkout).expect("create the repository");
    git(&checkout, &["init", "-q", "--initial-branch=main"]);
    fs::write(checkout.join("README.md"), "base\n").expect("write");
    fs::write(checkout.join("notes.txt"), "notes\n").expect("write");
    git(&checkout, &["add", "-A"]);
    git(&checkout, &["commit", "-qm", "base"]);
    let root = base.join("work");
    fs::create_dir_all(&root).expect("root");
    let workspaces = Arc::new(GitWorkspace::new(checkout.join(".git"), &root));

    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let memory = Arc::new(MemoryStore::new("shop", clock.clone()));
    for command in 1..=4 {
        let execution = ExecutionId::for_command(CommandId(command));
        memory.set_pane(&TerminalName::for_execution(&execution), READY);
    }
    let mut bridge = Bridge::new(
        memory.clone(),
        memory.clone(),
        workspaces.clone(),
        memory.clone(),
        BridgeConfig::new("tester"),
    )
    .with_clock(clock.clone());

    submit(
        &memory,
        "key_start",
        CommandAction::Start {
            engine: ENGINE.to_string(),
            prompt: "work".to_string(),
            parent_session_id: None,
            from: None,
            model: None,
            charter: None,
        },
    );
    bridge.tick().expect("start");
    let directory: PathBuf = root.join("agent_one");
    assert!(directory.is_dir(), "mounted: {:?}", events(&memory));
    assert_eq!(memory.submitted_held().len(), 1, "the prompt was typed");
    let workspace =
        WorkspaceId::new(format!("agent_one:{}", directory.display())).expect("valid workspace");

    // The agent deletes notes.txt (checkpointed); main changes it.
    fs::remove_file(directory.join("notes.txt")).expect("delete");
    workspaces.commit(&workspace, "work").expect("checkpoint");
    fs::write(checkout.join("notes.txt"), "notes, revised\n").expect("write on main");
    git(&checkout, &["commit", "-qam", "main moves"]);
    // The integrator's conflict verdict merges main into the idle workspace.
    assert_eq!(
        workspaces.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted {
            files: vec!["notes.txt".to_string()],
            marked: false,
        }
    );
    let copy = directory.join(MAIN_COPIES).join("notes.txt");
    assert_eq!(fs::read_to_string(&copy).expect("copy"), "notes, revised\n");

    // The engine is lost (here: stopped) and the workspace unmounted.
    submit(&memory, "key_stop", CommandAction::Stop);
    for _ in 0..4 {
        bridge.tick().expect("stop");
        clock.advance(std::time::Duration::from_secs(5));
        if !directory.exists() {
            break;
        }
    }
    assert!(!directory.exists(), "unmounted: {:?}", events(&memory));

    // The revision goes by a resume, into a freshly mounted workspace.
    submit(
        &memory,
        "key_resume",
        CommandAction::Resume {
            prompt: Some("resolve the conflict".to_string()),
            model: None,
        },
    );
    bridge.tick().expect("resume");
    assert!(directory.is_dir(), "mounted again: {:?}", events(&memory));
    assert!(
        !directory.join("notes.txt").exists(),
        "the branch as it was"
    );
    assert_eq!(
        fs::read_to_string(&copy).expect("main's copy is back after the remount"),
        "notes, revised\n"
    );
    assert_eq!(
        memory.submitted_held().len(),
        2,
        "the resume's prompt was typed"
    );
    let _ = fs::remove_dir_all(&base);
}
