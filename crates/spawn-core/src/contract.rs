//! Conformance suite shared by every backend.
//!
//! The same checks run against the memory backend and the real adapters
//! (PostgreSQL for the log and the queue, `spawn-host` for workspaces and
//! terminals), so the fakes the Bridge tests rely on cannot drift from what
//! production runs. Each check owns its identifiers.

use std::time::Duration;

use crate::command::{Claimed, CommandAction, Outcome, Submission};
use crate::event::{Appended, ENGINE, EventKind, GlobalPosition, NewEvent, SessionSeq, Source};
use crate::id::{
    AgentId, BranchName, IdempotencyKey, Principal, SessionId, TerminalName, WorkspaceId,
};
use crate::port::{
    BaseMerge, Branch, CommandQueue, Commit, EventLog, Launch, TerminalHost, WorkspaceStore,
};

fn agent(tag: &str) -> AgentId {
    AgentId::new(format!("agent_{tag}")).expect("tag forms a valid agent id")
}

fn session(tag: &str, number: u32) -> SessionId {
    SessionId::new(format!("session_{tag}{number}")).expect("tag forms a valid session id")
}

/// An event of session `number`, by that session's agent: an agent has one
/// session (the PostgreSQL log refuses any other pairing).
fn message(tag: &str, number: u32) -> NewEvent {
    NewEvent {
        agent_id: agent(&format!("{tag}{number}")),
        session_id: session(tag, number),
        execution_id: None,
        source: Source::Claude,
        source_event_id: None,
        kind: EventKind::MessageReceived,
    }
}

/// Event log rules: dense session sequences, idempotent source keys and
/// monotonic global positions.
pub fn event_log(tag: &str, log: &dyn EventLog) {
    let first: Appended = log.append(message(tag, 1)).expect("append succeeds");
    assert!(first.inserted);
    assert_eq!(first.record.session_seq, SessionSeq(1));

    let second = log.append(message(tag, 1)).expect("append succeeds");
    assert_eq!(second.record.session_seq, SessionSeq(2));
    assert!(second.record.position > first.record.position);

    let other = log.append(message(tag, 2)).expect("append succeeds");
    assert_eq!(other.record.session_seq, SessionSeq(1));
    assert!(other.record.position > second.record.position);

    let mut keyed = message(tag, 1);
    keyed.source_event_id = Some(format!("{tag}:tool_1"));
    let kept = log.append(keyed.clone()).expect("append succeeds");
    assert!(kept.inserted);
    let repeated = log.append(keyed).expect("repeat append succeeds");
    assert!(!repeated.inserted);
    assert_eq!(repeated.record.position, kept.record.position);

    let global = log
        .read_global(GlobalPosition(0), 100)
        .expect("read succeeds");
    assert!(
        global
            .windows(2)
            .all(|pair| pair[0].position < pair[1].position)
    );
    let sequences: Vec<SessionSeq> = global
        .iter()
        .filter(|record| record.event.session_id == session(tag, 1))
        .map(|record| record.session_seq)
        .collect();
    assert_eq!(
        sequences,
        [SessionSeq(1), SessionSeq(2), SessionSeq(3)],
        "a session's events are dense and in global order"
    );

    let limited = log
        .read_global(GlobalPosition(0), 1)
        .expect("read succeeds");
    assert_eq!(limited.len(), 1);
    let tail = log
        .read_global(second.record.position, 100)
        .expect("read succeeds");
    assert!(
        tail.iter()
            .all(|record| record.position > second.record.position)
    );

    // Free text is stored with its credentials redacted.
    let mut failed = message(tag, 1);
    failed.kind = EventKind::ToolFailed {
        tool_use_id: format!("{tag}_tool_2"),
        error: "psql postgres://spawn:hunter2@db/spawn: PGPASSWORD=hunter2".to_string(),
    };
    let stored = log.append(failed).expect("append succeeds");
    let read = log
        .read_global(GlobalPosition(stored.record.position.0 - 1), 1)
        .expect("read succeeds");
    for kind in [&stored.record.event.kind, &read[0].event.kind] {
        assert!(
            matches!(kind, EventKind::ToolFailed { error, .. }
                if !error.contains("hunter2") && error.contains("[redacted]")),
            "stored redacted: {kind:?}"
        );
    }
}

/// One active execution per session, held by the log: `execution.started`
/// is refused while the session's latest execution has no `execution.ended`
/// the Bridge wrote. A runner's exit report frees nothing; the Bridge's end
/// (of a lost execution too) does; a repeated start key is no second start.
pub fn one_execution(tag: &str, log: &dyn EventLog) {
    let execution = |name: &str| {
        crate::id::ExecutionId::new(format!("exec_{tag}_{name}")).expect("valid execution")
    };
    let event = |name: &str, source: Source, key: &str, kind: EventKind| NewEvent {
        execution_id: Some(execution(name)),
        source,
        source_event_id: Some(format!("exec_{tag}_{name}:{key}")),
        kind,
        ..message(tag, 3)
    };
    let started = |name: &str| {
        event(
            name,
            Source::Bridge,
            "started",
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: WorkspaceId::new("w:/workspaces/w").expect("valid workspace"),
                command_id: crate::id::CommandId(1),
                fork: None,
                model: crate::event::ModelProfile::Default,
                transcript_from: None,
            },
        )
    };
    let ended = |name: &str, source: Source, cause| {
        event(
            name,
            source,
            "ended",
            EventKind::ExecutionEnded {
                cause,
                exit_code: None,
            },
        )
    };
    let refused = |name: &str| {
        matches!(
            log.append(started(name)),
            Err(crate::port::StoreError::Rejected(_))
        )
    };

    let first = log.append(started("a")).expect("the first start is taken");
    assert!(refused("b"), "a second start while a runs");
    let again = log
        .append(started("a"))
        .expect("a repeated start key is no second start");
    assert!(!again.inserted);
    assert_eq!(again.record.position, first.record.position);
    log.append(ended("a", Source::Claude, crate::event::EndCause::Exited))
        .expect("a runner report");
    assert!(refused("b"), "a runner's exit report is no end");
    log.append(ended("a", Source::Bridge, crate::event::EndCause::Lost))
        .expect("the Bridge ends a lost execution");
    log.append(started("b"))
        .expect("after the end, the next start (a resume) is taken");
    assert!(refused("c"), "and b is the active one now");
}

/// Command queue rules: idempotent intake, one live lease per session,
/// expired leases taken again, stale attempts rejected.
pub fn command_queue(tag: &str, queue: &dyn CommandQueue, log: &dyn EventLog) {
    let principal = Principal::new(format!("principal_{tag}")).expect("valid principal");
    let submission = Submission {
        agent_id: agent(tag),
        session_id: session(tag, 1),
        idempotency_key: IdempotencyKey::new(format!("key_{tag}_1")).expect("valid key"),
        action: CommandAction::Start {
            engine: ENGINE.to_string(),
            prompt: "first task".to_string(),
            parent_session_id: None,
            from: None,
            model: None,
        },
    };
    let id = queue
        .submit(&principal, &submission)
        .expect("submit succeeds");
    let repeated = queue
        .submit(&principal, &submission)
        .expect("repeat submit succeeds");
    assert_eq!(id, repeated);

    let claimed: Claimed = queue
        .claim(tag, Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("a command is ready");
    assert_eq!(claimed.command_id, id);
    assert_eq!(claimed.attempt, 1);
    assert_eq!(claimed.action().expect("action parses"), submission.action);
    assert!(
        queue
            .claim(tag, Duration::from_mins(1))
            .expect("claim succeeds")
            .is_none(),
        "one session must hold one live lease"
    );

    queue
        .finish(&claimed, &Outcome::Succeeded)
        .expect("finish succeeds");
    queue
        .finish(&claimed, &Outcome::Succeeded)
        .expect("a repeated finish is harmless");

    let second = Submission {
        idempotency_key: IdempotencyKey::new(format!("key_{tag}_2")).expect("valid key"),
        ..submission
    };
    let next = queue.submit(&principal, &second).expect("submit succeeds");
    let expired = queue
        .claim(tag, Duration::from_millis(0))
        .expect("claim succeeds")
        .expect("a command is ready");
    assert_eq!(expired.command_id, next);
    assert_eq!(expired.attempt, 1);

    let renewed = queue
        .claim(tag, Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("an expired lease is taken again");
    assert_eq!(renewed.attempt, 2);
    assert!(
        queue
            .finish(&expired, &Outcome::Failed("stale".to_string()))
            .is_err(),
        "an old attempt must not finish the command"
    );
    queue
        .finish(
            &renewed,
            &Outcome::Failed("worker stopped: Bearer abcdefghijkl0123".to_string()),
        )
        .expect("finish succeeds");

    let events = log
        .read_global(GlobalPosition(0), 100)
        .expect("read succeeds");
    let finished: Vec<&EventKind> = events
        .iter()
        .map(|record| &record.event.kind)
        .filter(|kind| kind.name() == "command.finished")
        .collect();
    assert_eq!(finished.len(), 2);
    assert!(
        matches!(finished[1], EventKind::CommandFinished { error: Some(error), .. }
            if error == "worker stopped: Bearer [redacted]"),
        "a failure is stored redacted: {:?}",
        finished[1]
    );
}

/// Workspace rules: an unknown branch is refused; a new branch starts at
/// the base, at another agent's head, or at a commit, and an existing one
/// ignores `from`; a `from` that names no commit is refused; an untouched
/// workspace produces no commit; a branch on an unmoved base is current.
pub fn workspace_store(tag: &str, workspaces: &dyn WorkspaceStore) {
    let branch = BranchName::new(format!("branch_{tag}")).expect("valid branch");
    assert!(
        workspaces.mount(&branch).is_err(),
        "mounting an unknown branch must fail"
    );
    let Ok(Branch::Created(base)) = workspaces.ensure_branch(&branch, None) else {
        panic!("a new branch is created");
    };
    assert_eq!(base.from, "main", "a branch without from starts at main");
    assert!(
        matches!(
            workspaces.ensure_branch(&branch, None),
            Ok(Branch::Existing)
        ),
        "fork is idempotent"
    );

    let source = BranchName::for_agent(&agent(tag));
    workspaces
        .ensure_branch(&source, None)
        .expect("the source agent's branch");
    let forked = BranchName::new(format!("agent_{tag}_fork")).expect("valid branch");
    match workspaces.ensure_branch(&forked, Some(source.as_str())) {
        Ok(Branch::Created(fork)) => {
            assert_eq!(fork.from, source.as_str());
            assert_eq!(fork.commit, base.commit, "the source agent's head");
        }
        other => panic!("a fork from an agent is created: {other:?}"),
    }
    let pinned = BranchName::new(format!("agent_{tag}_pinned")).expect("valid branch");
    match workspaces.ensure_branch(&pinned, Some(base.commit.as_str())) {
        Ok(Branch::Created(fork)) => assert_eq!(fork.commit, base.commit),
        other => panic!("a fork from a commit is created: {other:?}"),
    }
    assert!(
        matches!(
            workspaces.ensure_branch(&forked, Some(base.commit.as_str())),
            Ok(Branch::Existing)
        ),
        "an existing branch ignores from"
    );
    let unknown = "f".repeat(40);
    let nobody = format!("agent_{tag}_nobody");
    for bad in ["-x", "main", "HEAD~1", nobody.as_str(), unknown.as_str()] {
        let fresh = BranchName::new(format!("agent_{tag}_bad")).expect("valid branch");
        assert!(
            workspaces.ensure_branch(&fresh, Some(bad)).is_err(),
            "from {bad} names no commit"
        );
    }

    let workspace: WorkspaceId = workspaces.mount(&branch).expect("mount succeeds");
    assert!(
        matches!(
            workspaces.commit(&workspace, "unchanged"),
            Ok(Commit::Unchanged)
        ),
        "an untouched workspace produces no commit"
    );
    assert!(
        matches!(workspaces.merge_base(&workspace), Ok(BaseMerge::Current)),
        "a branch on an unmoved base is current"
    );
    workspaces.unmount(&workspace).expect("unmount succeeds");
}

/// Terminal rules: a name is unique, and a stopped terminal is gone.
pub fn terminal(tag: &str, terminals: &dyn TerminalHost) {
    let terminal = TerminalName::new(format!("spawn-test-{tag}")).expect("valid terminal");
    let workspace = WorkspaceId::new("1:/workspaces/1").expect("valid workspace");
    let launch = Launch {
        terminal: terminal.clone(),
        workspace: workspace.clone(),
        cwd: "/".to_string(),
        // Runs until terminated, so a real host still sees it alive.
        program: "/bin/cat".to_string(),
        arguments: Vec::new(),
        environment: Vec::new(),
    };
    terminals.start(&launch).expect("start succeeds");
    assert!(
        terminals.start(&launch).is_err(),
        "a terminal name is unique"
    );
    assert!(
        terminals
            .is_alive(&terminal, &workspace)
            .expect("query succeeds")
    );
    // No line count is asserted: tmux returns up to `lines` lines of
    // scrollback plus the whole visible screen, the memory fake only the
    // last `lines` lines.
    terminals
        .capture_tail(&terminal, &workspace, 10)
        .expect("capture succeeds");
    terminals
        .terminate(&terminal, &workspace)
        .expect("terminate succeeds");
    assert!(
        !terminals
            .is_alive(&terminal, &workspace)
            .expect("query succeeds")
    );
    terminals
        .terminate(&terminal, &workspace)
        .expect("terminating a missing terminal is harmless");
}

/// Files through the terminal host: `write_new` creates a file (and its
/// parent directories) that `read_from` then reads, and never replaces one
/// that exists. `directory` is a scratch directory the host can write.
pub fn terminal_files(tag: &str, terminals: &dyn TerminalHost, directory: &str) {
    let workspace = WorkspaceId::new("1:/workspaces/1").expect("valid workspace");
    let path = format!(
        "{}/{tag}/projects/n1.jsonl",
        directory.trim_end_matches('/')
    );
    assert!(
        terminals
            .read_from(&workspace, &path, 0)
            .expect("read succeeds")
            .is_none()
    );
    assert!(
        terminals
            .write_new(&workspace, &path, b"{\"a\":1}\n")
            .expect("write succeeds"),
        "a missing file is written"
    );
    assert!(
        !terminals
            .write_new(&workspace, &path, b"other")
            .expect("write succeeds"),
        "an existing file is not"
    );
    assert_eq!(
        terminals
            .read_from(&workspace, &path, 0)
            .expect("read succeeds"),
        Some(b"{\"a\":1}\n".to_vec()),
        "and keeps what it held"
    );
}

/// Transcript archive rules: copies are numbered from 0 and only grow; an
/// append at any length but a copy's own, or past the next copy, is
/// refused; reads return what was appended.
pub fn transcript_archive(tag: &str, archive: &dyn crate::port::TranscriptArchive) {
    let agent = agent(tag);
    let native = "0b7c1d2e-0000-4000-8000-000000000001";
    assert!(archive.copies(&agent, native).expect("copies").is_empty());
    archive
        .append(&agent, native, 0, 0, b"one\n")
        .expect("a first copy");
    archive
        .append(&agent, native, 0, 4, b"two\n")
        .expect("it grows");
    assert!(
        archive.append(&agent, native, 0, 4, b"again\n").is_err(),
        "archived bytes are never overwritten"
    );
    assert!(
        archive.append(&agent, native, 2, 0, b"x").is_err(),
        "no copy past the next"
    );
    assert!(
        archive.append(&agent, native, 1, 3, b"x").is_err(),
        "a new copy starts at 0"
    );
    archive
        .append(&agent, native, 1, 0, b"whole\n")
        .expect("the next copy");
    assert_eq!(archive.copies(&agent, native).expect("copies"), vec![8, 6]);
    assert_eq!(
        archive.read(&agent, native, 0, 2, 4).expect("read"),
        b"e\ntw".to_vec()
    );
    assert_eq!(
        archive.read(&agent, native, 1, 0, 100).expect("read"),
        b"whole\n".to_vec()
    );
    assert!(
        archive
            .copies(
                &crate::id::AgentId::new(format!("agent_{tag}_other")).expect("valid"),
                native
            )
            .expect("copies")
            .is_empty(),
        "copies are per agent"
    );
}
