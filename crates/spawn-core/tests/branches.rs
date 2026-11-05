//! The Bridge's part in branching and merging (docs/storage.md): a start's
//! `from` reaches the workspace store and the fork point the log; the base is
//! merged into an idle workspace right after a turn's checkpoint, and never
//! while a turn runs.

use std::sync::Arc;

use spawn_core::bridge::{Bridge, BridgeConfig, TickReport, claude_session_id};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{
    CheckpointReason, ENGINE, EndCause, EventKind, Fork, GlobalPosition, NewEvent,
};
use spawn_core::id::{
    AgentId, BranchName, CommandId, CommitId, ExecutionId, IdempotencyKey, Principal, SessionId,
    TerminalName,
};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{BaseMerge, CommandQueue, EventLog, TerminalHost, WorkspaceStore};
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

/// A Bridge as `spawn` builds it: with a transcript archive, which puts a
/// resumed session's transcript where the next mount's engine reads it
/// (each memory mount is a new directory).
fn bridge(store: &Arc<MemoryStore>) -> Bridge {
    Bridge::new(
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        BridgeConfig::new("tester"),
    )
    .with_transcript_archive(store.clone())
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
            engine: ENGINE.to_string(),
            prompt: "work".to_string(),
            parent_session_id: None,
            from: from.map(str::to_string),
            model: None,
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
            EventKind::ArtifactCommitted {
                reason, artifact, ..
            } => Some((reason, artifact.commit.as_str().to_string())),
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

#[test]
fn a_start_from_another_agent_forks_its_head_and_records_the_fork() {
    let store = store();
    let source = BranchName::new("agent_source").expect("valid branch");
    store.ensure_branch(&source, None).expect("source branch");
    start(&store, Some("agent_source"));
    quiet(&bridge(&store).tick().expect("tick"));
    assert_eq!(
        store.workspace_calls()[1..4],
        [
            "ensure_branch agent_one agent_source",
            "merge_main agent_one",
            "mount agent_one"
        ]
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
fn main_is_merged_cleanly_when_an_execution_starts_and_never_while_it_runs() {
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let store = Arc::new(MemoryStore::new("shop", clock.clone()));
    for command in 1..=4 {
        let execution = ExecutionId::for_command(CommandId(command));
        store.set_pane(&TerminalName::for_execution(&execution), READY);
    }
    let mut bridge = bridge(&store).with_clock(clock.clone());
    let merged = CommitId::new("e".repeat(40)).expect("valid commit");
    store.set_base_merge(BaseMerge::Merged {
        commit: merged.clone(),
        files: vec!["README.md".to_string()],
    });
    // The branch takes main before the mount; the log refuses the record.
    start(&store, None);
    store.fail_next_branch_record();
    assert!(bridge.tick().is_err(), "the log is down");
    assert!(checkpoints(&store).is_empty());
    // The waiting record goes in; the start, back after its lease, runs.
    clock.advance(std::time::Duration::from_mins(3));
    quiet(&bridge.tick().expect("the log is back"));
    assert_eq!(
        checkpoints(&store),
        [(CheckpointReason::BaseMerged, merged.as_str().to_string())]
    );
    assert!(
        events(&store)
            .iter()
            .any(|event| matches!(event.kind, EventKind::ExecutionStarted { .. }))
    );
    let calls = store.workspace_calls();
    let merge = calls
        .iter()
        .rposition(|call| call.starts_with("merge_main"));
    let mount = calls.iter().rposition(|call| call.starts_with("mount"));
    assert!(merge < mount, "merged before the mount: {calls:?}");

    // A running execution is never merged into, however long it idles.
    let merge_calls = |store: &MemoryStore| {
        store
            .workspace_calls()
            .iter()
            .filter(|call| call.starts_with("merge"))
            .count()
    };
    let before = merge_calls(&store);
    store.set_base_merge(BaseMerge::Merged {
        commit: CommitId::new("f".repeat(40)).expect("valid commit"),
        files: vec!["README.md".to_string()],
    });
    transcript(&store, &[prompt("1"), turn_end("1")]);
    for _ in 0..4 {
        clock.advance(std::time::Duration::from_mins(1));
        quiet(&bridge.tick().expect("idle"));
    }
    assert_eq!(merge_calls(&store), before, "no merge while it runs");
}

#[test]
fn a_conflict_at_a_start_leaves_the_branch_and_is_recorded_once() {
    let store = store();
    let mut bridge = bridge(&store);
    store.set_base_merge(BaseMerge::Conflicted {
        files: vec!["README.md".to_string()],
        marked: true,
    });
    start(&store, None);
    let report = bridge.tick().expect("start");
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("README.md")),
        "{:?}",
        report.warnings
    );
    let conflicts = |store: &MemoryStore| -> Vec<(Vec<String>, bool)> {
        events(store)
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::MergeConflicted { files, marked } => Some((files, marked)),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        conflicts(&store),
        [(vec!["README.md".to_string()], false)],
        "recorded, unmarked: the branch stayed as it was"
    );
    assert!(checkpoints(&store).is_empty(), "the branch did not move");
    assert!(
        events(&store)
            .iter()
            .any(|event| matches!(event.kind, EventKind::ExecutionStarted { .. })),
        "the execution starts on the branch as it is"
    );
    // A resume over the same branch head records the same conflict once.
    submit(&store, "key_stop", CommandAction::Stop);
    quiet(&bridge.tick().expect("stop"));
    submit(
        &store,
        "key_resume",
        CommandAction::Resume {
            prompt: None,
            model: None,
        },
    );
    bridge.tick().expect("resume");
    assert_eq!(conflicts(&store).len(), 1);
}

fn ended(store: &MemoryStore) -> Vec<EndCause> {
    events(store)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::ExecutionEnded { cause, .. } => Some(cause),
            _ => None,
        })
        .collect()
}

fn unmounts(store: &MemoryStore) -> usize {
    store
        .workspace_calls()
        .iter()
        .filter(|call| *call == "unmount")
        .count()
}

#[test]
fn a_failed_final_checkpoint_keeps_the_workspace_and_is_tried_again() {
    let store = store();
    let mut bridge = bridge(&store);
    start(&store, None);
    bridge.tick().expect("start");
    store.fail_commits(true);
    submit(&store, "key_stop", CommandAction::Stop);
    bridge.tick().expect("stop");
    assert_eq!(unmounts(&store), 0, "the workspace stays");
    assert!(ended(&store).is_empty());
    let report = bridge.tick().expect("again");
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("final checkpoint failed (3 of 20)")),
        "{:?}",
        report.warnings
    );
    assert_eq!(unmounts(&store), 0);
    store.fail_commits(false);
    bridge.tick().expect("the checkpoint works");
    assert_eq!(ended(&store), vec![EndCause::Stopped]);
    assert_eq!(unmounts(&store), 1);
}

fn commits(store: &MemoryStore) -> usize {
    store
        .workspace_calls()
        .iter()
        .filter(|call| *call == "commit")
        .count()
}

fn stalls(store: &MemoryStore) -> Vec<(EndCause, u32)> {
    events(store)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::ExecutionEndStalled {
                cause, failures, ..
            } => Some((cause, failures)),
            _ => None,
        })
        .collect()
}

/// A Bridge on `clock` whose stop's final checkpoint has failed twenty
/// times: the end is stalled.
fn stalled(store: &Arc<MemoryStore>, clock: &Arc<ManualClock>) -> Bridge {
    let mut bridge = bridge(store).with_clock(clock.clone());
    start(store, None);
    bridge.tick().expect("start");
    store.fail_commits(true);
    submit(store, "key_stop", CommandAction::Stop);
    // The stop tries once, and the same tick's pass over executions again.
    bridge.tick().expect("stop: attempts 1 and 2");
    assert_eq!(commits(store), 2);
    while commits(store) < 19 {
        bridge.tick().expect("attempt");
        assert!(stalls(store).is_empty());
    }
    let report = bridge.tick().expect("attempt 20");
    assert_eq!(commits(store), 20);
    assert!(
        report.warnings.iter().any(
            |warning| warning.contains("FINAL CHECKPOINT FAILED 20 TIMES")
                && warning.contains("STAYS MOUNTED")
        ),
        "{:?}",
        report.warnings
    );
    bridge
}

#[test]
fn a_final_checkpoint_that_keeps_failing_stalls_the_end_and_never_unmounts() {
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let store = Arc::new(MemoryStore::new("shop", clock.clone()));
    let terminal = TerminalName::for_execution(&ExecutionId::for_command(CommandId(1)));
    store.set_pane(&terminal, READY);
    let mut bridge = stalled(&store, &clock);
    assert_eq!(
        stalls(&store),
        vec![(EndCause::Stopped, 20)],
        "recorded once"
    );
    assert_eq!(unmounts(&store), 0, "the workspace stays");
    assert!(ended(&store).is_empty());
    let workspace = spawn_core::id::WorkspaceId::new("1:/w").expect("valid");
    assert!(
        !store.is_alive(&terminal, &workspace).expect("alive?"),
        "the terminal is down"
    );

    // Slow from now on: nothing within 30 s, one attempt after, no second
    // loud warning.
    let wait = |seconds: u64| clock.advance(std::time::Duration::from_secs(seconds));
    wait(29);
    quiet(&bridge.tick().expect("waits"));
    assert_eq!(commits(&store), 20);
    wait(1);
    let report = bridge.tick().expect("attempt 21");
    assert_eq!(commits(&store), 21);
    assert!(
        !report
            .warnings
            .iter()
            .any(|warning| warning.contains("FINAL CHECKPOINT FAILED")),
        "{:?}",
        report.warnings
    );
    assert_eq!(unmounts(&store), 0);

    // A restarted Bridge keeps the end stalled, and slow.
    let mut bridge = self::bridge(&store).with_clock(clock.clone());
    bridge.recover().expect("recover");
    bridge.tick().expect("attempt 22");
    assert_eq!(commits(&store), 22);
    bridge.tick().expect("waits");
    assert_eq!(commits(&store), 22);
    assert_eq!(stalls(&store).len(), 1);
    assert_eq!(unmounts(&store), 0);

    // A plain stop does not give the work up either.
    submit(&store, "key_stop_again", CommandAction::Stop);
    bridge.tick().expect("stop again");
    assert_eq!(unmounts(&store), 0);

    // The cause is fixed: the next attempt ends it, as it was ending.
    store.fail_commits(false);
    wait(30);
    bridge.tick().expect("the checkpoint commits");
    assert_eq!(ended(&store), vec![EndCause::Stopped]);
    assert_eq!(unmounts(&store), 1);
}

#[test]
fn a_discard_unmounts_a_stalled_end() {
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let store = Arc::new(MemoryStore::new("shop", clock.clone()));
    let terminal = TerminalName::for_execution(&ExecutionId::for_command(CommandId(1)));
    store.set_pane(&terminal, READY);
    let mut bridge = stalled(&store, &clock);
    submit(&store, "key_discard", CommandAction::Discard);
    let report = bridge.tick().expect("discard");
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("discarded")),
        "{:?}",
        report.warnings
    );
    assert_eq!(ended(&store), vec![EndCause::Stopped]);
    assert_eq!(unmounts(&store), 1);
    let finished: Vec<bool> = events(&store)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::CommandFinished { succeeded, .. } => Some(succeeded),
            _ => None,
        })
        .collect();
    assert_eq!(finished.last(), Some(&true), "the discard succeeded");
}

/// Append lines to the transcript of the session's first native session as
/// the engine of the `mount`-th workspace writes it: a resumed session keeps
/// writing the file the earlier execution wrote.
fn transcript_in(store: &MemoryStore, mount: u64, lines: &[String]) {
    let path = spawn_core::bridge::transcript_path(
        &BridgeConfig::new("tester").claude_projects,
        &spawn_core::id::WorkspaceId::new(format!("{mount}:/workspaces/{mount}"))
            .expect("valid workspace"),
        &claude_session_id(&ExecutionId::for_command(CommandId(1))),
    );
    store.append_file(&path, &(lines.join("\n") + "\n"));
}

/// The session ran one turn on the first execution (merged after it), was
/// stopped, and resumed with a prompt: the second execution's turn is open.
fn resumed(clock: &Arc<ManualClock>) -> (Arc<MemoryStore>, Bridge) {
    let store = Arc::new(MemoryStore::new("shop", clock.clone()));
    for command in 1..=4 {
        let execution = ExecutionId::for_command(CommandId(command));
        store.set_pane(&TerminalName::for_execution(&execution), READY);
    }
    let mut bridge = bridge(&store).with_clock(clock.clone());
    start(&store, None);
    quiet(&bridge.tick().expect("start"));
    transcript(&store, &[prompt("1"), turn_end("1")]);
    quiet(&bridge.tick().expect("read the turn"));
    quiet(&bridge.tick().expect("checkpoint"));
    submit(&store, "key_stop", CommandAction::Stop);
    quiet(&bridge.tick().expect("stop"));
    submit(
        &store,
        "key_resume",
        CommandAction::Resume {
            prompt: Some("go on".to_string()),
            model: None,
        },
    );
    let report = bridge.tick().expect("resume and type the prompt");
    // Each memory mount is a new directory: the transcript is put back
    // there, and said so.
    assert!(
        report
            .warnings
            .iter()
            .all(|warning| warning.contains("restored")),
        "{:?}",
        report.warnings
    );
    (store, bridge)
}

#[test]
fn a_late_turn_end_of_an_earlier_execution_asks_no_checkpoint() {
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let (store, mut bridge) = resumed(&clock);
    // The resumed execution starts a tool, which still runs.
    transcript_in(&store, 2, &tool_call("toolu_9")[..1]);
    quiet(&bridge.tick().expect("read the tool start"));

    // A turn end of the first execution reaches the log only now.
    store
        .append(NewEvent {
            agent_id: AgentId::new("agent_one").expect("valid agent"),
            session_id: SessionId::new("session_one").expect("valid session"),
            execution_id: Some(ExecutionId::for_command(CommandId(1))),
            source: spawn_core::event::Source::Claude,
            source_event_id: Some("t-late".to_string()),
            kind: EventKind::TurnStopped {
                input_tokens: None,
                output_tokens: None,
            },
        })
        .expect("append succeeds");
    let commits_before = commits(&store);
    for _ in 0..3 {
        clock.advance(std::time::Duration::from_mins(1));
        quiet(&bridge.tick().expect("the tool runs"));
    }
    assert_eq!(
        commits(&store),
        commits_before,
        "the earlier execution's turn end asks no checkpoint of this one"
    );

    // Its own turn end asks for one.
    transcript_in(&store, 2, &[tool_call("toolu_9")[1].clone(), turn_end("9")]);
    quiet(&bridge.tick().expect("read the turn end"));
    quiet(&bridge.tick().expect("checkpoint"));
    assert!(commits(&store) > commits_before);
}

#[test]
fn a_resumed_execution_reads_its_own_lines_by_position_whatever_time_they_claim() {
    // A host clock in 2026; the engine's lines claim 1970, the year 99999…
    // and no time at all. None of that decides whose lines they are.
    let clock = Arc::new(ManualClock::new(Timestamp(1_793_771_172_345)));
    let (store, mut bridge) = resumed(&clock);
    let path = spawn_core::bridge::transcript_path(
        &BridgeConfig::new("tester").claude_projects,
        &spawn_core::id::WorkspaceId::new("2:/workspaces/2").expect("valid workspace"),
        &claude_session_id(&ExecutionId::for_command(CommandId(1))),
    );
    let length = store
        .read_from(
            &spawn_core::id::WorkspaceId::new("2:/workspaces/2").expect("valid"),
            &path,
            0,
        )
        .expect("read")
        .expect("the restored transcript")
        .len();
    let from = events(&store)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::ExecutionStarted {
                transcript_from, ..
            } => Some(transcript_from),
            _ => None,
        })
        .next_back()
        .expect("the resumed execution");
    assert_eq!(
        from,
        Some(u64::try_from(length).expect("small")),
        "the resumed execution starts after the earlier executions' lines"
    );
    let line = |kind: &str, id: &str, at: &str| {
        let at = if at.is_empty() {
            String::new()
        } else {
            format!(r#""timestamp":"{at}","#)
        };
        match kind {
            "prompt" => {
                format!(r#"{{"type":"user","uuid":"p-{id}",{at}"message":{{"content":"next"}}}}"#)
            }
            _ => format!(
                r#"{{"type":"system","subtype":"turn_duration","uuid":"t-{id}",{at}"x":1}}"#
            ),
        }
    };
    transcript_in(
        &store,
        2,
        &[
            line("prompt", "2", "1970-01-01T00:00:00.000Z"),
            line("prompt", "3", "99999999999999999-11-04T05:46:12.000Z"),
            line("turn", "2", ""),
        ],
    );
    quiet(&bridge.tick().expect("read the turn"));
    let received: Vec<Option<String>> = events(&store)
        .into_iter()
        // start, stop, resume: the resumed execution is the third command's.
        .filter(|event| event.execution_id == Some(ExecutionId::for_command(CommandId(3))))
        .filter(|event| matches!(event.kind, EventKind::MessageReceived))
        .map(|event| event.source_event_id)
        .collect();
    assert_eq!(
        received,
        [Some("p-2".to_string()), Some("p-3".to_string())],
        "both prompts are the resumed execution's"
    );
    let commits_before = commits(&store);
    quiet(&bridge.tick().expect("checkpoint"));
    assert!(
        commits(&store) > commits_before,
        "its untimed turn end asked for the turn's checkpoint"
    );
}

/// A checkpoint of the first execution whose `artifact.committed` the log
/// refused: the branch holds it, the log does not.
fn checkpoint_unrecorded(store: &MemoryStore, bridge: &mut Bridge) {
    start(store, None);
    quiet(&bridge.tick().expect("start"));
    transcript(store, &tool_call("toolu_1"));
    quiet(&bridge.tick().expect("read the tool call"));
    let workspace = spawn_core::id::WorkspaceId::new("1:/workspaces/1").expect("valid");
    store.mark_dirty(&workspace);
    store.fail_next_branch_record();
    assert!(bridge.tick().is_err(), "the log is down");
    assert!(checkpoints(store).is_empty());
}

fn kinds(store: &MemoryStore) -> Vec<&'static str> {
    events(store)
        .into_iter()
        .map(|event| event.kind.name())
        .filter(|name| matches!(*name, "artifact.committed" | "execution.ended"))
        .collect()
}

#[test]
fn a_stop_records_a_waiting_checkpoint_before_the_end() {
    let store = store();
    let mut bridge = bridge(&store);
    checkpoint_unrecorded(&store, &mut bridge);
    // The stop runs before the session's advance, which used to append it.
    submit(&store, "key_stop", CommandAction::Stop);
    quiet(&bridge.tick().expect("stop"));
    assert_eq!(
        kinds(&store),
        ["artifact.committed", "execution.ended"],
        "the checkpoint comes before the end"
    );
    quiet(&bridge.tick().expect("idle"));
    assert_eq!(checkpoints(&store).len(), 1, "recorded once");
}

#[test]
fn a_checkpoint_whose_record_waited_is_recorded_after_a_restart() {
    let store = store();
    let mut bridge = bridge(&store);
    checkpoint_unrecorded(&store, &mut bridge);
    let head = format!("{:040x}", 1);
    // The waiting record lived in the Bridge's memory only.
    drop(bridge);
    let mut restarted = self::bridge(&store);
    restarted.recover().expect("recover");
    quiet(&restarted.tick().expect("after the restart"));
    assert_eq!(
        checkpoints(&store),
        [(CheckpointReason::TurnStopped, head.clone())],
        "the branch head is recorded"
    );
    // A branch whose head the log records gets nothing more.
    let mut again = self::bridge(&store);
    again.recover().expect("recover");
    quiet(&again.tick().expect("after a second restart"));
    assert_eq!(checkpoints(&store).len(), 1);
}

#[test]
fn a_record_the_log_keeps_refusing_holds_up_only_its_session() {
    let store = store();
    let mut bridge = bridge(&store);
    start(&store, None);
    bridge.tick().expect("start one");
    let two = SessionId::new("session_two").expect("valid session");
    store
        .submit(
            &Principal::new("tester").expect("valid principal"),
            &Submission {
                agent_id: AgentId::new("agent_two").expect("valid agent"),
                session_id: two.clone(),
                idempotency_key: IdempotencyKey::new("key_start_two").expect("valid key"),
                action: CommandAction::Start {
                    engine: ENGINE.to_string(),
                    prompt: "work".to_string(),
                    parent_session_id: None,
                    from: None,
                    model: None,
                },
            },
        )
        .expect("submit succeeds");
    bridge.tick().expect("start two");

    // Session one's checkpoint is taken; the log never takes its record.
    let one = SessionId::new("session_one").expect("valid session");
    store.refuse_branch_records_of(&one, true);
    transcript(&store, &tool_call("toolu_1"));
    bridge.tick().expect("read the tool call");
    store.mark_dirty(&spawn_core::id::WorkspaceId::new("1:/workspaces/1").expect("valid"));
    let _ = bridge.tick();

    // Session two still checkpoints, tick after tick.
    let execution = ExecutionId::for_command(CommandId(2));
    let path = spawn_core::bridge::transcript_path(
        &BridgeConfig::new("tester").claude_projects,
        &spawn_core::id::WorkspaceId::new("2:/workspaces/2").expect("valid workspace"),
        &claude_session_id(&execution),
    );
    store.append_file(&path, &(tool_call("toolu_2").join("\n") + "\n"));
    let report = bridge.tick().expect("one refused record fails no tick");
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("session_one")),
        "{:?}",
        report.warnings
    );
    store.mark_dirty(&spawn_core::id::WorkspaceId::new("2:/workspaces/2").expect("valid"));
    bridge.tick().expect("one refused record fails no tick");
    let recorded: Vec<SessionId> = events(&store)
        .into_iter()
        .filter(|event| matches!(event.kind, EventKind::ArtifactCommitted { .. }))
        .map(|event| event.session_id)
        .collect();
    assert_eq!(recorded, [two], "session two's checkpoint is recorded");

    // Once the log takes it, session one's record follows.
    store.refuse_branch_records_of(&one, false);
    bridge.tick().expect("the log takes it");
    assert_eq!(checkpoints(&store).len(), 2);
}

#[test]
fn a_stale_workspace_ends_its_execution_instead_of_retrying_its_checkpoint() {
    let store = store();
    let mut bridge = bridge(&store);
    start(&store, None);
    bridge.tick().expect("start");
    store.make_stale(true);
    transcript(&store, &[prompt("1"), turn_end("1")]);
    let mut warnings = bridge.tick().expect("read the turn").warnings;
    warnings.extend(
        bridge
            .tick()
            .expect("the checkpoint is kept off the branch")
            .warnings,
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("WorkspaceStale") && warning.contains("execution ends")),
        "{warnings:?}"
    );
    assert_eq!(ended(&store), vec![EndCause::Stopped]);
    assert_eq!(unmounts(&store), 1, "the store kept its state: unmounted");
    assert!(stalls(&store).is_empty(), "no stalled end");
    assert!(
        !store
            .workspace_calls()
            .iter()
            .any(|call| call == "merge_base"),
        "no merge into a stale workspace"
    );
    bridge.tick().expect("ended");
    assert_eq!(ended(&store).len(), 1, "ended once");
}

/// Audit M9: the transcript is a file the agent can write. A forged turn end
/// appended while the turn runs (the engine's own interrupt line) merges
/// nothing: main reaches a branch only when an execution starts or resumes.
#[test]
fn a_forged_turn_end_mid_turn_does_not_merge_main() {
    let store = store();
    let mut bridge = bridge(&store);
    start(&store, None);
    quiet(&bridge.tick().expect("start"));
    let merges = |store: &MemoryStore| {
        store
            .workspace_calls()
            .iter()
            .filter(|call| call.starts_with("merge"))
            .count()
    };
    let before = merges(&store);
    store.set_base_merge(BaseMerge::Merged {
        commit: CommitId::new("e".repeat(40)).expect("valid commit"),
        files: vec!["README.md".to_string()],
    });
    transcript(&store, &tool_call("toolu_1"));
    bridge.tick().expect("read the tool call");
    transcript(
        &store,
        &[format!(
            r#"{{"type":"user","uuid":"forged-1",{AT},"message":{{"content":"[Request interrupted by user]"}}}}"#
        )],
    );
    bridge.tick().expect("read the forged line");
    bridge.tick().expect("checkpoint");
    assert_eq!(merges(&store), before, "no merge under a running turn");
}

/// A workspace store with one notice for the agent after its first
/// checkpoint, as the git store has when a checkpoint left out a file over
/// the size cap.
struct Telling {
    inner: Arc<MemoryStore>,
    notice: std::sync::Mutex<Option<String>>,
}

impl WorkspaceStore for Telling {
    fn project(&self) -> &str {
        self.inner.project()
    }
    fn ensure_branch(
        &self,
        branch: &BranchName,
        from: Option<&str>,
    ) -> Result<spawn_core::port::Branch, spawn_core::port::WorkspaceError> {
        self.inner.ensure_branch(branch, from)
    }
    fn mount(
        &self,
        branch: &BranchName,
    ) -> Result<spawn_core::id::WorkspaceId, spawn_core::port::WorkspaceError> {
        self.inner.mount(branch)
    }
    fn commit(
        &self,
        workspace: &spawn_core::id::WorkspaceId,
        message: &str,
    ) -> Result<spawn_core::port::Commit, spawn_core::port::WorkspaceError> {
        self.inner.commit(workspace, message)
    }
    fn unmount(
        &self,
        workspace: &spawn_core::id::WorkspaceId,
    ) -> Result<(), spawn_core::port::WorkspaceError> {
        self.inner.unmount(workspace)
    }
    fn merge_base(
        &self,
        workspace: &spawn_core::id::WorkspaceId,
    ) -> Result<BaseMerge, spawn_core::port::WorkspaceError> {
        self.inner.merge_base(workspace)
    }
    fn take_notices(&self, _workspace: &spawn_core::id::WorkspaceId) -> Vec<String> {
        self.notice
            .lock()
            .expect("the notice")
            .take()
            .into_iter()
            .collect()
    }
}

fn pane(store: &MemoryStore, command: u64) -> String {
    let terminal = TerminalName::for_execution(&ExecutionId::for_command(CommandId(command)));
    store
        .capture_tail(
            &terminal,
            &spawn_core::id::WorkspaceId::new("1:/w").expect("valid"),
            200,
        )
        .expect("a pane")
}

fn reset_pane(store: &MemoryStore, command: u64) {
    let terminal = TerminalName::for_execution(&ExecutionId::for_command(CommandId(command)));
    store.set_pane(&terminal, READY);
}

/// What was typed into the first execution since its pane last read ready.
fn typed_last(store: &MemoryStore) -> String {
    let pane = pane(store, 1);
    pane.rsplit("tok/s").next().unwrap_or_default().to_string()
}

fn input(store: &MemoryStore, key: &str, text: &str) {
    submit(
        store,
        key,
        CommandAction::Input {
            text: text.to_string(),
            tags: Vec::new(),
        },
    );
}

const LEFT_OUT: &str = "model.bin (200 MiB) is over the 100 MiB cap for one file in a checkpoint";

/// What the store has to tell the agent is a tick warning when the
/// checkpoint hands it over, and goes before the next prompt the Bridge
/// types, once.
#[test]
fn a_notice_of_the_workspace_store_is_logged_and_told_with_the_next_prompt_once() {
    let store = store();
    let telling = Arc::new(Telling {
        inner: store.clone(),
        notice: std::sync::Mutex::new(Some(LEFT_OUT.to_string())),
    });
    let mut bridge = Bridge::new(
        store.clone(),
        store.clone(),
        telling,
        store.clone(),
        BridgeConfig::new("tester"),
    )
    .with_transcript_archive(store.clone());
    start(&store, None);
    bridge.tick().expect("start");
    transcript(&store, &[prompt("1"), turn_end("1")]);
    let mut warnings = Vec::new();
    for _ in 0..2 {
        warnings.extend(bridge.tick().expect("checkpoint").warnings);
    }
    assert!(
        warnings.iter().any(|warning| warning.contains(LEFT_OUT)),
        "{warnings:?}"
    );
    assert!(
        !pane(&store, 1).contains(LEFT_OUT),
        "never typed unprompted"
    );

    reset_pane(&store, 1);
    input(&store, "key_input_1", "next");
    bridge.tick().expect("type");
    let typed = typed_last(&store);
    let told = typed.find(LEFT_OUT).expect("the notice was typed");
    assert!(typed[told..].contains("\n\nnext"), "{typed}");

    transcript(&store, &[prompt("2"), turn_end("2")]);
    bridge.tick().expect("read the turn");
    bridge.tick().expect("checkpoint");
    reset_pane(&store, 1);
    input(&store, "key_input_2", "again");
    bridge.tick().expect("type");
    let typed = typed_last(&store);
    assert!(
        typed.contains("again") && !typed.contains(LEFT_OUT),
        "{typed}"
    );
}
