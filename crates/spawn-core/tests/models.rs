//! Engine model profiles: the settings each profile gives Claude Code, and
//! how the Bridge picks, records and keeps a session's profile.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use spawn_core::bridge::{Bridge, BridgeConfig, ModelRelay, TickReport, claude_settings};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{ENGINE, EventKind, GlobalPosition, ModelProfile, NewEvent};
use spawn_core::id::TerminalName;
use spawn_core::id::{AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{CommandQueue, EventLog, Launch, TerminalHost, WorkspaceStore};
use spawn_core::time::{ManualClock, Timestamp};

/// A ready pane: input box empty, status line below it.
const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

fn relay(url: &str) -> ModelRelay {
    ModelRelay {
        url: url.to_string(),
        cheap_model: "deepseek-v4-pro".to_string(),
        small_model: "deepseek-flash".to_string(),
    }
}

/// A relay address that accepts connections while the listener lives.
fn listening() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    (listener, url)
}

/// A relay address nothing listens on.
fn closed() -> String {
    let (listener, url) = listening();
    drop(listener);
    url
}

fn env(settings: &str) -> Option<serde_json::Map<String, Value>> {
    let value: Value = serde_json::from_str(settings).expect("settings are JSON");
    value.get("env").and_then(Value::as_object).cloned()
}

#[test]
fn without_a_relay_the_settings_name_no_models() {
    for profile in [ModelProfile::Default, ModelProfile::Cheap] {
        let settings = claude_settings(None, profile);
        assert!(env(&settings).is_none(), "{settings}");
        assert!(settings.contains("skipDangerousModePermissionPrompt"));
    }
}

#[test]
fn the_default_profile_moves_only_the_small_model() {
    let relay = relay("http://127.0.0.1:8791");
    let env = env(&claude_settings(Some(&relay), ModelProfile::Default)).expect("an env");
    assert_eq!(env.len(), 2, "{env:?}");
    assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8791");
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "deepseek-flash");
}

#[test]
fn the_cheap_profile_moves_every_slot() {
    let relay = relay("http://127.0.0.1:8791");
    let env = env(&claude_settings(Some(&relay), ModelProfile::Cheap)).expect("an env");
    assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8791");
    for slot in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
    ] {
        assert_eq!(env[slot], "deepseek-v4-pro", "{slot}");
    }
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "deepseek-flash");
    // No credentials: the gateway's token comes from the user's settings,
    // which inline settings merge over; DeepSeek's stays in the relay.
    assert!(
        env.keys()
            .all(|name| !name.contains("TOKEN") && !name.contains("KEY"))
    );
}

#[test]
fn profiles_round_trip_and_the_default_is_left_out_of_records() {
    assert_eq!(ModelProfile::parse("cheap"), Some(ModelProfile::Cheap));
    assert_eq!(ModelProfile::parse("default"), Some(ModelProfile::Default));
    assert_eq!(ModelProfile::parse("opus"), None);
    let start = |model| CommandAction::Start {
        engine: ENGINE.to_string(),
        prompt: "p".to_string(),
        parent_session_id: None,
        from: None,
        model,
    };
    let plain = serde_json::to_value(start(None)).expect("serialize");
    assert!(plain.get("model").is_none(), "{plain}");
    let cheap = serde_json::to_value(start(Some(ModelProfile::Cheap))).expect("serialize");
    assert_eq!(cheap["model"], "cheap");
    let back: CommandAction = serde_json::from_value(cheap).expect("deserialize");
    assert_eq!(back, start(Some(ModelProfile::Cheap)));
    let resume: CommandAction =
        serde_json::from_str(r#"{"type":"resume","prompt":"go on"}"#).expect("an old resume");
    assert_eq!(
        resume,
        CommandAction::Resume {
            prompt: Some("go on".to_string()),
            model: None
        }
    );
}

struct Fleet {
    store: Arc<MemoryStore>,
    bridge: Bridge,
    commands: u64,
}

impl Fleet {
    fn new(model_relay: Option<ModelRelay>) -> Self {
        let clock: Arc<ManualClock> = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock));
        for command in 1..=6 {
            let execution = ExecutionId::for_command(CommandId(command));
            store.set_pane(&TerminalName::for_execution(&execution), READY);
        }
        let log: Arc<dyn EventLog> = store.clone();
        let queue: Arc<dyn CommandQueue> = store.clone();
        let workspaces: Arc<dyn WorkspaceStore> = store.clone();
        let terminals: Arc<dyn TerminalHost> = store.clone();
        let mut config = BridgeConfig::new("tester");
        config.ready_interval = Duration::from_millis(1);
        config.model_relay = model_relay;
        let bridge = Bridge::new(log, queue, workspaces, terminals, config);
        Self {
            store,
            bridge,
            commands: 0,
        }
    }

    fn send(&mut self, action: CommandAction) -> TickReport {
        self.commands += 1;
        self.store
            .submit(
                &Principal::new("tester").expect("valid principal"),
                &Submission {
                    agent_id: AgentId::new("agent_worker_docs_1").expect("valid agent"),
                    session_id: SessionId::new("session_worker_docs_1").expect("valid session"),
                    idempotency_key: IdempotencyKey::new(format!("key_{}", self.commands))
                        .expect("valid key"),
                    action,
                },
            )
            .expect("submit succeeds");
        self.bridge.tick().expect("tick succeeds")
    }

    fn start(&mut self, model: Option<ModelProfile>) -> TickReport {
        self.send(CommandAction::Start {
            engine: ENGINE.to_string(),
            prompt: "write the docs".to_string(),
            parent_session_id: None,
            from: None,
            model,
        })
    }

    fn resume(&mut self, model: Option<ModelProfile>) -> TickReport {
        self.send(CommandAction::Resume {
            prompt: Some("go on".to_string()),
            model,
        })
    }

    fn events(&self) -> Vec<NewEvent> {
        self.store
            .read_global(GlobalPosition(0), 500)
            .expect("read succeeds")
            .into_iter()
            .map(|record| record.event)
            .collect()
    }

    /// The profile of every recorded execution start, in order.
    fn started(&self) -> Vec<ModelProfile> {
        self.events()
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::ExecutionStarted { model, .. } => Some(model),
                _ => None,
            })
            .collect()
    }

    /// The last command's failure, if it failed.
    fn failure(&self) -> Option<String> {
        self.events()
            .into_iter()
            .rev()
            .find_map(|event| match event.kind {
                EventKind::CommandFinished { error, .. } => Some(error),
                _ => None,
            })?
    }

    fn settings(launch: &Launch) -> String {
        let at = launch
            .arguments
            .iter()
            .position(|argument| argument == "--settings")
            .expect("claude gets --settings");
        launch.arguments[at + 1].clone()
    }

    /// The main model slot each launch set, in order (`None`: not set).
    fn main_models(&self) -> Vec<Option<String>> {
        self.store
            .launches()
            .iter()
            .map(|launch| {
                env(&Self::settings(launch)).and_then(|env| {
                    env.get("ANTHROPIC_MODEL")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
            })
            .collect()
    }
}

#[test]
fn a_cheap_worker_without_its_relay_fails_visibly_and_starts_nothing() {
    for relay in [None, Some(relay(&closed()))] {
        let mut fleet = Fleet::new(relay);
        fleet.start(Some(ModelProfile::Cheap));
        let failure = fleet.failure().expect("the start failed");
        assert!(failure.contains("model relay"), "{failure}");
        assert!(fleet.started().is_empty());
        assert!(fleet.store.launches().is_empty());
    }
}

#[test]
fn the_default_profile_falls_back_on_claudes_own_settings_when_the_relay_is_down() {
    let url = closed();
    let mut fleet = Fleet::new(Some(relay(&url)));
    let report = fleet.start(None);
    assert!(fleet.failure().is_none());
    assert_eq!(fleet.started(), [ModelProfile::Default]);
    let launches = fleet.store.launches();
    assert!(env(&Fleet::settings(&launches[0])).is_none());
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains(&url) && warning.contains("unreachable")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn a_resumed_cheap_worker_stays_cheap_until_a_command_names_another_profile() {
    let (_listener, url) = listening();
    let mut fleet = Fleet::new(Some(relay(&url)));
    fleet.start(Some(ModelProfile::Cheap));
    assert!(fleet.failure().is_none(), "{:?}", fleet.failure());
    fleet.send(CommandAction::Stop);
    fleet.resume(None);
    assert!(fleet.failure().is_none(), "{:?}", fleet.failure());
    fleet.send(CommandAction::Stop);
    fleet.resume(Some(ModelProfile::Default));
    assert!(fleet.failure().is_none(), "{:?}", fleet.failure());
    assert_eq!(
        fleet.started(),
        [
            ModelProfile::Cheap,
            ModelProfile::Cheap,
            ModelProfile::Default
        ]
    );
    let cheap = Some("deepseek-v4-pro".to_string());
    assert_eq!(fleet.main_models(), [cheap.clone(), cheap, None]);
    // The default profile still goes through the relay, for its small model.
    let launches = fleet.store.launches();
    let last = env(&Fleet::settings(&launches[2])).expect("an env");
    assert_eq!(last["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "deepseek-flash");
}

#[test]
fn a_session_that_never_ran_starts_on_the_default_profile() {
    let (_listener, url) = listening();
    let mut fleet = Fleet::new(Some(relay(&url)));
    fleet.start(None);
    assert_eq!(fleet.started(), [ModelProfile::Default]);
    assert_eq!(fleet.main_models(), [None]);
}
