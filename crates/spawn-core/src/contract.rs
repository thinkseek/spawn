//! Conformance suite shared by every backend.
//!
//! The same checks run against the memory backend and the PostgreSQL
//! backend, so the two cannot drift. Each check owns its identifiers.

use std::time::Duration;

use crate::command::{Claimed, CommandAction, Outcome, Submission};
use crate::event::{Appended, Engine, EventKind, GlobalPosition, NewEvent, SessionSeq, Source};
use crate::id::{
    AgentId, BranchName, IdempotencyKey, Principal, SessionId, TerminalName, WorkspaceId,
};
use crate::port::{CommandQueue, Commit, EventLog, Launch, TerminalHost, WorkspaceStore};

fn agent(tag: &str) -> AgentId {
    AgentId::new(format!("agent_{tag}")).expect("tag forms a valid agent id")
}

fn session(tag: &str, number: u32) -> SessionId {
    SessionId::new(format!("session_{tag}{number}")).expect("tag forms a valid session id")
}

fn message(tag: &str, number: u32) -> NewEvent {
    NewEvent {
        agent_id: agent(tag),
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

    let read = log
        .read_session(&session(tag, 1), SessionSeq(0), 10)
        .expect("read succeeds");
    assert_eq!(read.len(), 3);
    assert!(
        read.windows(2)
            .all(|pair| pair[0].session_seq < pair[1].session_seq)
    );

    let after = log
        .read_session(&session(tag, 1), SessionSeq(1), 10)
        .expect("read succeeds");
    assert_eq!(after.len(), 2);

    let limited = log
        .read_session(&session(tag, 1), SessionSeq(0), 1)
        .expect("read succeeds");
    assert_eq!(limited.len(), 1);

    let global = log
        .read_global(GlobalPosition(0), 100)
        .expect("read succeeds");
    assert!(
        global
            .windows(2)
            .all(|pair| pair[0].position < pair[1].position)
    );
    let tail = log
        .read_global(second.record.position, 100)
        .expect("read succeeds");
    assert!(
        tail.iter()
            .all(|record| record.position > second.record.position)
    );
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
            engine: Engine::Claude,
            prompt: "first task".to_string(),
            parent_session_id: None,
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
        .finish(&renewed, &Outcome::Failed("worker stopped".to_string()))
        .expect("finish succeeds");

    let events = log
        .read_global(GlobalPosition(0), 100)
        .expect("read succeeds");
    let finished = events
        .iter()
        .filter(|record| record.event.kind.name() == "command.finished")
        .count();
    assert_eq!(finished, 2);
}

/// Workspace rules: an unknown branch is refused, an untouched workspace
/// produces no commit.
pub fn workspace_store(tag: &str, workspaces: &dyn WorkspaceStore) {
    let branch = BranchName::new(format!("branch_{tag}")).expect("valid branch");
    assert!(
        workspaces.mount(&branch).is_err(),
        "mounting an unknown branch must fail"
    );
    workspaces.ensure_branch(&branch).expect("fork succeeds");
    workspaces
        .ensure_branch(&branch)
        .expect("fork is idempotent");
    let workspace: WorkspaceId = workspaces.mount(&branch).expect("mount succeeds");
    assert!(
        matches!(
            workspaces.commit(&workspace, "unchanged"),
            Ok(Commit::Unchanged)
        ),
        "an untouched workspace produces no commit"
    );
    workspaces.unmount(&workspace).expect("unmount succeeds");
}

/// Terminal rules: a name is unique, and a stopped terminal is gone.
pub fn terminal(tag: &str, terminals: &dyn TerminalHost) {
    let terminal = TerminalName::new(format!("spawn-test-{tag}")).expect("valid terminal");
    let launch = Launch {
        terminal: terminal.clone(),
        workspace: WorkspaceId::new("1:/workspaces/1").expect("valid workspace"),
        program: "true".to_string(),
        arguments: Vec::new(),
        environment: Vec::new(),
    };
    terminals.start(&launch).expect("start succeeds");
    assert!(
        terminals.start(&launch).is_err(),
        "a terminal name is unique"
    );
    assert!(terminals.is_alive(&terminal).expect("query succeeds"));
    let tail = terminals
        .capture_tail(&terminal, 10)
        .expect("capture succeeds");
    assert!(tail.lines().count() <= 10);
    terminals.terminate(&terminal).expect("terminate succeeds");
    assert!(!terminals.is_alive(&terminal).expect("query succeeds"));
    terminals
        .terminate(&terminal)
        .expect("terminating a missing terminal is harmless");
}
