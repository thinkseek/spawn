use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use spawn_bridge::{
    AgentBridge, CheckpointPolicy, RecoveredBinding, SessionBinding, SessionDirectory,
    StartExecutionRequest,
};
use spawn_engine::NormalizedEvent;
use spawn_layerfs::{
    ArtifactError, ArtifactStore, BranchForkOutcome, CommitOutcome, CommitPage, MergeOutcome,
    SandboxId, WorkspaceMount,
};
use spawn_store::{EventLog, InMemoryStore};
use spawn_terminal::{LaunchSpec, PaneCapture, TerminalError, TerminalHost};
use spawn_types::clock::DeterministicClock;
use spawn_types::engine::Engine;
use spawn_types::event::{AppendOutcome, CheckpointReason, EventKind, EventSource, GlobalPosition};
use spawn_types::identifiers::{
    AgentId, BranchName, CommitId, ExecutionId, SessionId, TerminalName,
};

fn ids() -> (AgentId, SessionId) {
    (
        AgentId::from_prefixed("agent_aaaa").unwrap(),
        SessionId::from_prefixed("session_bbbb").unwrap(),
    )
}

#[derive(Default)]
struct FakeArtifacts {
    next_sandbox: AtomicU64,
    commits: Mutex<u32>,
    dirty: Mutex<bool>,
}

impl ArtifactStore for FakeArtifacts {
    fn ensure_branch(
        &self,
        _project: &str,
        _source: &BranchName,
        target: &BranchName,
    ) -> Result<BranchForkOutcome, ArtifactError> {
        Ok(BranchForkOutcome::Forked(target.clone()))
    }

    fn create_sandbox(&self, _project: &str, _image: &str) -> Result<SandboxId, ArtifactError> {
        Ok(SandboxId::new(
            self.next_sandbox.fetch_add(1, Ordering::SeqCst) + 1,
        ))
    }

    fn mount(
        &self,
        _project: &str,
        _branch: &BranchName,
        sandbox: SandboxId,
        path: &str,
    ) -> Result<WorkspaceMount, ArtifactError> {
        Ok(WorkspaceMount {
            sandbox,
            path: path.to_string(),
            image: "test-image".to_string(),
        })
    }

    fn commit(
        &self,
        _mount: &WorkspaceMount,
        _message: &str,
    ) -> Result<CommitOutcome, ArtifactError> {
        let mut dirty = self.dirty.lock().unwrap();
        let mut count = self.commits.lock().unwrap();
        *count += 1;
        if *dirty {
            *dirty = false;
            Ok(CommitOutcome::Committed(
                CommitId::new(format!("commit_{}", *count)).unwrap(),
            ))
        } else {
            Ok(CommitOutcome::NoChanges)
        }
    }

    fn unmount(&self, _mount: &WorkspaceMount, _discard: bool) -> Result<(), ArtifactError> {
        Ok(())
    }

    fn list_commits(
        &self,
        _project: &str,
        _branch: &BranchName,
        _limit: u32,
        _offset: u32,
    ) -> Result<CommitPage, ArtifactError> {
        Ok(CommitPage::default())
    }

    fn merge(
        &self,
        _project: &str,
        source: &BranchName,
        _target: &BranchName,
    ) -> Result<MergeOutcome, ArtifactError> {
        Ok(MergeOutcome::Conflict {
            paths: vec![format!("{}/file.txt", source.as_str())],
        })
    }
}

#[derive(Default)]
struct FakeTerminal {
    started: Mutex<Vec<String>>,
}

impl TerminalHost for FakeTerminal {
    fn start(&self, spec: &LaunchSpec) -> Result<(), TerminalError> {
        self.started
            .lock()
            .unwrap()
            .push(spec.name.as_str().to_string());
        Ok(())
    }
    fn send_keys(
        &self,
        _name: &TerminalName,
        _keys: &str,
        _enter: bool,
    ) -> Result<(), TerminalError> {
        Ok(())
    }
    fn capture(&self, _name: &TerminalName) -> Result<PaneCapture, TerminalError> {
        Ok(PaneCapture {
            text: String::new(),
        })
    }
    fn is_alive(&self, _name: &TerminalName) -> Result<bool, TerminalError> {
        Ok(true)
    }
    fn terminate(&self, _name: &TerminalName) -> Result<(), TerminalError> {
        Ok(())
    }
}

fn request() -> StartExecutionRequest {
    StartExecutionRequest {
        engine: Engine::ClaudeCode,
        project: "demo".to_string(),
        branch: BranchName::new("agent_001").unwrap(),
        image: "ubuntu".to_string(),
        mount_path: "/workspace/agent".to_string(),
        prompt: String::new(),
        hook_forwarder: "spawn hook".to_string(),
        command_prefix: Vec::new(),
    }
}

fn bridge_with() -> (
    AgentBridge,
    Arc<InMemoryStore>,
    Arc<FakeArtifacts>,
    AgentId,
    SessionId,
) {
    let clock = Arc::new(DeterministicClock::starting_at(0));
    let log = Arc::new(InMemoryStore::new().with_clock(clock.clone()));
    let artifacts = Arc::new(FakeArtifacts::default());
    let terminals = Arc::new(FakeTerminal::default());
    let bridge = AgentBridge::new(log.clone(), artifacts.clone(), terminals, clock);
    let (agent, session) = ids();
    (bridge, log, artifacts, agent, session)
}

#[test]
fn checkpoint_policy_matches_lifecycle_events() {
    assert_eq!(
        CheckpointPolicy::reason_for(&EventKind::ToolCompleted {
            tool_correlation_id: "t1".to_string(),
            duration_ms: None,
        }),
        Some(CheckpointReason::ToolCompleted)
    );
    assert_eq!(
        CheckpointPolicy::reason_for(&EventKind::ToolStarted {
            tool_name: "Bash".into(),
            tool_correlation_id: "t1".into(),
        }),
        None
    );
}

#[test]
fn start_execution_records_binding_and_event() {
    let (bridge, log, _artifacts, agent, session) = bridge_with();
    bridge
        .open_session(
            &agent,
            &session,
            "demo",
            &BranchName::new("main").unwrap(),
            &BranchName::new("agent_001").unwrap(),
        )
        .unwrap();
    let execution = ExecutionId::from_prefixed("exec_aaaa").unwrap();
    let started = bridge
        .start_execution(&agent, &session, execution.clone(), &request())
        .unwrap();
    assert_eq!(started.execution_id, execution);
    assert_eq!(started.mount.sandbox.value(), 1);

    let events = log.read_global(GlobalPosition(0), 100).unwrap();
    assert!(
        events
            .iter()
            .any(|record| matches!(record.event.kind, EventKind::ExecutionStarted { .. }))
    );

    let recovered: Option<RecoveredBinding> = bridge.recover(&session).unwrap();
    let recovered = recovered.expect("recovered binding exists");
    assert_eq!(recovered.execution_id, execution);
}

#[test]
fn no_change_checkpoint_appends_no_event() {
    let (bridge, log, _artifacts, agent, session) = bridge_with();
    let execution = ExecutionId::from_prefixed("exec_bbbb").unwrap();
    bridge
        .start_execution(&agent, &session, execution.clone(), &request())
        .unwrap();
    let before = log.read_global(GlobalPosition(0), 100).unwrap().len();

    let normalized = NormalizedEvent {
        source_event_id: Some("turn-1".to_string()),
        kind: EventKind::TurnStopped {
            input_tokens: None,
            output_tokens: None,
        },
    };
    let outcome = bridge
        .ingest(
            &agent,
            &session,
            &execution,
            EventSource::ClaudeHooks,
            normalized,
        )
        .unwrap();
    assert!(matches!(outcome, AppendOutcome::Recorded(_)));

    // Fake artifacts report NoChanges, so only the turn event is appended.
    let after = log.read_global(GlobalPosition(0), 100).unwrap().len();
    assert_eq!(after, before + 1);
}

#[test]
fn directory_registers_and_removes_bindings() {
    let mut directory = SessionDirectory::default();
    let (_agent, session) = ids();
    let binding = SessionBinding {
        agent_id: AgentId::from_prefixed("agent_aaaa").unwrap(),
        session_id: session.clone(),
        execution_id: ExecutionId::from_prefixed("exec_cccc").unwrap(),
        engine: Engine::Codex,
        project: "demo".to_string(),
        branch: BranchName::new("agent_x").unwrap(),
        mount: WorkspaceMount {
            sandbox: SandboxId::new(3),
            path: "/ws".to_string(),
            image: "img".to_string(),
        },
        terminal_name: TerminalName::new("term-x").unwrap(),
        native_session_id: None,
    };
    assert!(directory.get(&session).is_none());
    directory.register(binding);
    assert!(directory.get(&session).is_some());
    assert!(directory.remove(&session).is_some());
    assert!(directory.get(&session).is_none());
}
