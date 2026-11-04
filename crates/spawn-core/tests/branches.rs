//! The Bridge's part in branching and merging (docs/storage.md): a start's
//! `from` reaches the workspace store and the fork point the log; the base is
//! merged into an idle workspace right after a turn's checkpoint, and never
//! while a turn runs.

use std::sync::Arc;

use spawn_core::bridge::{Bridge, BridgeConfig, TickReport, claude_session_id};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{CheckpointReason, Engine, EventKind, Fork, GlobalPosition, NewEvent};
use spawn_core::id::{
    AgentId, BranchName, CommandId, CommitId, ExecutionId, IdempotencyKey, Principal, SessionId,
    TerminalName,
};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{BaseMerge, CommandQueue, EventLog, WorkspaceStore};
use spawn_core::time::{ManualClock, Timestamp};

/// A ready pane: input box empty, status line below it.
const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

const AT: &str = r#""timestamp":"2099-01-01T00:00:00.000Z""#;

fn store() -> Arc<MemoryStore> {
    let clock: Arc<ManualClock> = Arc::new(ManualClock::new(Timestamp(1_000)));
    let store = Arc::new(MemoryStore::new("shop", clock));
    for command in 1..=4 {
        let execution = ExecutionId::for_command(CommandId(command));
        store.set_pane(&TerminalName::for_execution(&execution), READY);
    }
    store
}

fn bridge(store: &Arc<MemoryStore>) -> Bridge {
    Bridge::new(
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        BridgeConfig::new("tester"),
    )
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

fn start(store: &MemoryStore, from: Option<&str>) {
    submit(
        store,
        "key_start",
        CommandAction::Start {
            engine: Engine::Claude,
            prompt: "work".to_string(),
            parent_session_id: None,
            from: from.map(str::to_string),
        },
    );
}

fn events(store: &MemoryStore) -> Vec<NewEvent> {
    store
        .read_global(GlobalPosition(0), 500)
        .expect("read succeeds")
        .into_iter()
        .map(|record| record.event)
        .collect()
}

/// The fork the first `execution.started` records; it must exist.
fn fork(store: &MemoryStore) -> Option<Fork> {
    events(store)
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::ExecutionStarted { fork, .. } => Some(fork),
            _ => None,
        })
        .expect("an execution started")
}

fn checkpoints(store: &MemoryStore) -> Vec<(CheckpointReason, String)> {
    events(store)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::ArtifactCommitted { reason, artifact } => {
                Some((reason, artifact.commit.as_str().to_string()))
            }
            _ => None,
        })
        .collect()
}

/// Append lines to the transcript of the first execution, as the engine
/// would.
fn transcript(store: &MemoryStore, lines: &[String]) {
    let execution = ExecutionId::for_command(CommandId(1));
    let path = spawn_core::bridge::transcript_path(
        &BridgeConfig::new("tester").claude_projects,
        &spawn_core::id::WorkspaceId::new("1:/workspaces/1").expect("valid workspace"),
        &claude_session_id(&execution),
    );
    store.append_file(&path, &(lines.join("\n") + "\n"));
}

fn tool_call(id: &str) -> Vec<String> {
    vec![
        format!(
            r#"{{"type":"assistant","uuid":"a-{id}",{AT},"message":{{"content":[{{"type":"tool_use","id":"{id}","name":"Bash"}}]}}}}"#
        ),
        format!(
            r#"{{"type":"user","uuid":"r-{id}",{AT},"message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}"}}]}}}}"#
        ),
    ]
}

fn turn_end(id: &str) -> String {
    format!(r#"{{"type":"system","subtype":"turn_duration","uuid":"t-{id}",{AT}}}"#)
}

fn prompt(id: &str) -> String {
    format!(r#"{{"type":"user","uuid":"p-{id}",{AT},"message":{{"content":"next"}}}}"#)
}

fn quiet(report: &TickReport) {
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

/// Workspace calls after the start's `ensure_branch` and `mount`.
fn calls_after_start(store: &MemoryStore) -> Vec<String> {
    store.workspace_calls().into_iter().skip(2).collect()
}

#[test]
fn a_start_from_another_agent_forks_its_head_and_records_the_fork() {
    let store = store();
    let source = BranchName::new("agent_source").expect("valid branch");
    store.ensure_branch(&source, None).expect("source branch");
    start(&store, Some("agent_source"));
    quiet(&bridge(&store).tick().expect("tick"));
    assert_eq!(
        store.workspace_calls()[1..3],
        ["ensure_branch agent_one agent_source", "mount agent_one"]
    );
    assert_eq!(
        fork(&store),
        Some(Fork {
            from: "agent_source".to_string(),
            commit: CommitId::new("b".repeat(40)).expect("valid commit"),
        })
    );
}

#[test]
fn an_existing_branch_ignores_from_with_a_warning() {
    let store = store();
    let own = BranchName::new("agent_one").expect("valid branch");
    store.ensure_branch(&own, None).expect("own branch");
    start(&store, Some(&"b".repeat(40)));
    let report = bridge(&store).tick().expect("tick");
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("ignored")),
        "{:?}",
        report.warnings
    );
    assert_eq!(fork(&store), None, "no fork: the branch was there");
}

#[test]
fn a_from_that_names_no_commit_fails_the_start() {
    let store = store();
    start(&store, Some("-rf"));
    bridge(&store).tick().expect("tick");
    let names: Vec<&str> = events(&store)
        .iter()
        .map(|event| event.kind.name())
        .collect();
    assert_eq!(names, ["command.accepted", "command.finished"]);
}

#[test]
fn the_base_is_merged_right_after_a_turns_checkpoint() {
    let store = store();
    let mut bridge = bridge(&store);
    start(&store, None);
    quiet(&bridge.tick().expect("start"));

    // A tool's checkpoint takes no merge.
    transcript(&store, &tool_call("toolu_1"));
    quiet(&bridge.tick().expect("read the tool call"));
    quiet(&bridge.tick().expect("checkpoint the tool call"));
    assert_eq!(calls_after_start(&store), ["commit"]);

    let merged = CommitId::new("e".repeat(40)).expect("valid commit");
    store.set_base_merge(BaseMerge::Merged {
        commit: merged.clone(),
        files: vec!["README.md".to_string()],
    });
    transcript(&store, &[turn_end("1")]);
    quiet(&bridge.tick().expect("read the turn end"));
    quiet(&bridge.tick().expect("checkpoint the turn"));
    assert_eq!(
        calls_after_start(&store),
        ["commit", "commit", "merge_base"]
    );
    assert_eq!(
        checkpoints(&store),
        [(CheckpointReason::BaseMerged, merged.as_str().to_string())]
    );
    // Recorded once: the next ticks have nothing to commit or merge.
    quiet(&bridge.tick().expect("idle"));
    assert_eq!(calls_after_start(&store).len(), 3);
}

#[test]
fn a_conflict_is_a_warning_and_records_nothing() {
    let store = store();
    let mut bridge = bridge(&store);
    start(&store, None);
    bridge.tick().expect("start");
    store.set_base_merge(BaseMerge::Conflicted {
        files: vec!["README.md".to_string()],
        marked: true,
    });
    transcript(&store, &[prompt("1"), turn_end("1")]);
    bridge.tick().expect("read the turn");
    let report = bridge.tick().expect("checkpoint the turn");
    let warned = |report: &TickReport, words: &str| {
        report.warnings.iter().any(|warning| {
            warning.contains("conflicts")
                && warning.contains("README.md")
                && warning.contains(words)
        })
    };
    assert!(warned(&report, "markers there"), "{:?}", report.warnings);
    assert!(checkpoints(&store).is_empty());

    // A conflict markers cannot show is said to be left unmerged.
    store.set_base_merge(BaseMerge::Conflicted {
        files: vec!["README.md".to_string()],
        marked: false,
    });
    transcript(&store, &[prompt("2"), turn_end("2")]);
    bridge.tick().expect("read the turn");
    let report = bridge.tick().expect("checkpoint the turn");
    assert!(warned(&report, "left unmerged"), "{:?}", report.warnings);
    assert!(checkpoints(&store).is_empty());
}

#[test]
fn no_merge_while_a_turn_runs() {
    let store = store();
    let mut bridge = bridge(&store);
    start(&store, None);
    bridge.tick().expect("start");
    store.set_base_merge(BaseMerge::Merged {
        commit: CommitId::new("e".repeat(40)).expect("valid commit"),
        files: Vec::new(),
    });

    // The engine began its next turn in the transcript before the Bridge
    // took the last turn's checkpoint.
    transcript(&store, &[turn_end("1")]);
    bridge.tick().expect("read the turn end");
    transcript(&store, &[prompt("2")]);
    bridge.tick().expect("checkpoint the turn");
    assert_eq!(
        calls_after_start(&store),
        ["commit"],
        "checkpoint, no merge"
    );

    // The Bridge typed the next prompt in the same tick.
    transcript(&store, &[turn_end("2")]);
    bridge.tick().expect("read the turn end");
    submit(
        &store,
        "key_input",
        CommandAction::Input {
            text: "more".to_string(),
        },
    );
    bridge.tick().expect("type, then checkpoint the turn");
    assert_eq!(calls_after_start(&store), ["commit", "commit"]);
    assert!(checkpoints(&store).is_empty());

    // The turn ends: now the merge.
    transcript(&store, &[prompt("3"), turn_end("3")]);
    bridge.tick().expect("read the turn");
    bridge.tick().expect("checkpoint the turn");
    assert_eq!(
        calls_after_start(&store),
        ["commit", "commit", "commit", "merge_base"]
    );
}
