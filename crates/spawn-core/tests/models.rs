//! Engine model profiles: the settings each profile gives Claude Code, and
//! how the Bridge picks, records and keeps a session's profile.

use std::collections::BTreeSet;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use spawn_core::bridge::{
    Bridge, BridgeConfig, CLAUDE_DIR_SENSITIVE, CREDENTIAL_DENY, CROSS_SESSION_INBOUND,
    LaunchEnvironment, ModelRelay, TickReport, claude_dir_deny, claude_dir_rules, claude_settings,
};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{ENGINE, EventKind, GlobalPosition, LaunchSummary, ModelProfile, NewEvent};
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
        ..ModelRelay::default()
    }
}

/// A relay whose default profile names its own models (issue #79).
fn configured_relay(url: &str) -> ModelRelay {
    ModelRelay {
        default_model: Some("claude-opus-5-5".to_string()),
        sonnet_model: Some("claude-sonnet-5-5".to_string()),
        subagent_model: Some("claude-haiku-5-5".to_string()),
        ..relay(url)
    }
}

fn top_level_model(settings: &str) -> Option<String> {
    let value: Value = serde_json::from_str(settings).expect("settings are JSON");
    value
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
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

/// With none of its own models named, the default profile moves only the
/// small model (the haiku slot and `ANTHROPIC_SMALL_FAST_MODEL`): Claude
/// Code's own main model, and its token, stand.
#[test]
fn the_default_profile_moves_only_the_small_model() {
    let relay = relay("http://127.0.0.1:8791");
    let settings = claude_settings(Some(&relay), ModelProfile::Default);
    let env = env(&settings).expect("an env");
    assert_eq!(env.len(), 3, "{env:?}");
    assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8791");
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "deepseek-flash");
    assert_eq!(env["ANTHROPIC_SMALL_FAST_MODEL"], "deepseek-flash");
    assert_eq!(top_level_model(&settings), None, "{settings}");
    // The gateway's token comes from the user's own settings, which the
    // inline env merges over name by name: never set, so never emptied.
    assert!(
        env.keys()
            .all(|name| !name.contains("TOKEN") && !name.contains("KEY"))
    );
}

/// Issue #79: the default profile pins what the relay names, each slot
/// falling back on its main model, and nothing else.
#[test]
fn the_default_profile_pins_the_slots_the_relay_names() {
    let relay = configured_relay("http://127.0.0.1:8791");
    let settings = claude_settings(Some(&relay), ModelProfile::Default);
    let env = env(&settings).expect("an env");
    assert_eq!(
        top_level_model(&settings).as_deref(),
        Some("claude-opus-5-5")
    );
    assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8791");
    assert_eq!(env["ANTHROPIC_MODEL"], "claude-opus-5-5");
    assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "claude-sonnet-5-5");
    // No opus slot of its own: the main model.
    assert_eq!(env["ANTHROPIC_DEFAULT_OPUS_MODEL"], "claude-opus-5-5");
    assert_eq!(env["CLAUDE_CODE_SUBAGENT_MODEL"], "claude-haiku-5-5");
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "deepseek-flash");
    assert_eq!(env["ANTHROPIC_SMALL_FAST_MODEL"], "deepseek-flash");
    assert_eq!(env.len(), 7, "{env:?}");
    assert!(
        env.keys()
            .all(|name| !name.contains("TOKEN") && !name.contains("KEY"))
    );

    // Slots alone, no main model: those slots, Claude Code's own main.
    let slots = ModelRelay {
        default_model: None,
        opus_model: Some("claude-opus-5-5".to_string()),
        ..configured_relay("http://127.0.0.1:8791")
    };
    let settings = claude_settings(Some(&slots), ModelProfile::Default);
    let env = self::env(&settings).expect("an env");
    assert_eq!(top_level_model(&settings), None, "{settings}");
    assert!(env.get("ANTHROPIC_MODEL").is_none(), "{env:?}");
    assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "claude-sonnet-5-5");
    assert_eq!(env["ANTHROPIC_DEFAULT_OPUS_MODEL"], "claude-opus-5-5");
    assert_eq!(env["CLAUDE_CODE_SUBAGENT_MODEL"], "claude-haiku-5-5");

    // The cheap profile ignores the default profile's names.
    let cheap = claude_settings(Some(&relay), ModelProfile::Cheap);
    assert_eq!(top_level_model(&cheap).as_deref(), Some("deepseek-v4-pro"));
    assert!(!cheap.contains("claude-opus-5-5"), "{cheap}");
}

#[test]
fn the_cheap_profile_moves_every_slot() {
    let relay = relay("http://127.0.0.1:8791");
    let settings = claude_settings(Some(&relay), ModelProfile::Cheap);
    let env = env(&settings).expect("an env");
    assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8791");
    // Issue #79: the top-level model too, so the user's own settings'
    // `model` cannot take the main slot back to the gateway.
    assert_eq!(
        top_level_model(&settings).as_deref(),
        Some("deepseek-v4-pro")
    );
    for slot in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ] {
        assert_eq!(env[slot], "deepseek-v4-pro", "{slot}");
    }
    for slot in [
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
    ] {
        assert_eq!(env[slot], "deepseek-flash", "{slot}");
    }
    assert_eq!(env.len(), 7, "{env:?}");
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
        charter: None,
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
        Self::configured(|config| config.model_relay = model_relay)
    }

    fn configured(configure: impl FnOnce(&mut BridgeConfig)) -> Self {
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
        configure(&mut config);
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
            charter: None,
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

/// The deny rules of one launch's settings, as written.
fn denied(launch: &Launch) -> Vec<String> {
    let value: Value = serde_json::from_str(&Fleet::settings(launch)).expect("settings are JSON");
    assert_eq!(value["permissions"]["defaultMode"], "bypassPermissions");
    value["permissions"]["deny"]
        .as_array()
        .expect("a deny list")
        .iter()
        .map(|rule| rule.as_str().expect("a rule").to_string())
        .collect()
}

#[test]
fn every_engine_the_bridge_starts_is_denied_the_credential_files() {
    let (_listener, up) = listening();
    // Through the relay (cheap, then default resumed), without one, and on
    // the fallback when the relay is down: every launch carries the rules.
    let mut fleet = Fleet::new(Some(relay(&up)));
    fleet.start(Some(ModelProfile::Cheap));
    fleet.send(CommandAction::Stop);
    fleet.resume(Some(ModelProfile::Default));
    let mut launches = fleet.store.launches();
    for relay in [None, Some(relay(&closed()))] {
        let mut fleet = Fleet::new(relay);
        fleet.start(None);
        launches.extend(fleet.store.launches());
    }
    assert_eq!(launches.len(), 4);
    for launch in &launches {
        let rules = denied(launch);
        assert_eq!(rules, CREDENTIAL_DENY, "{}", Fleet::settings(launch));
    }
    // Each credential the owner named is denied to the Read and Edit tools
    // and to any Bash command that names it.
    let rules = denied(&launches[0]);
    for (tool_rule, bash_rule) in [
        ("Read(~/.local/share/spawn-rsi/*.env*)", "Bash(*spawn.env*)"),
        (
            "Read(~/.local/share/spawn-rsi/env.sh)",
            "Bash(*spawn-rsi/env.sh*)",
        ),
        ("Read(~/.config/spawn/**)", "Bash(*.config/spawn*)"),
        ("Edit(~/.config/spawn/**)", "Bash(*test-database.url*)"),
        ("Read(~/.hermes/**)", "Bash(*.hermes*)"),
        ("Read(~/.claude/settings*)", "Bash(*.claude/*)"),
        ("Edit(~/.claude/settings*)", "Bash(*/.claude*)"),
        ("Read(~/.claude/**/*credential*)", "Bash(*cd .claude*)"),
        ("Read(~/.claude/sessions/**)", "Bash(*.claude/*)"),
        ("Read(~/.claude.json*)", "Bash(*.claude.json*)"),
        ("Edit(~/.claude.json*)", "Bash(*.claude.json*)"),
        ("Read(~/.ssh/**)", "Bash(*/.ssh*)"),
        ("Read(~/.gnupg/**)", "Bash(*.gnupg*)"),
        ("Read(~/.config/spawn/**)", "Bash(*r2.env*)"),
        ("Read(//data/tmp/ccd-pg/**)", "Bash(*/data/tmp/ccd*)"),
        ("Read(//var/tmp/spawn/**/*.sh)", "Bash(*livepsql*)"),
        (
            "Read(~/.local/share/spawn-rsi/fleet-pg/**)",
            "Bash(*fleet-pg*)",
        ),
    ] {
        assert!(rules.iter().any(|rule| rule == tool_rule), "{tool_rule}");
        assert!(rules.iter().any(|rule| rule == bash_rule), "{bash_rule}");
    }
    // The programs that read them for whoever runs them, wherever the
    // checkout is, and other processes' environments.
    for program in [
        "Bash(*/hold.sh*)",
        "Bash(*/deploy.sh*)",
        "Bash(*test-deploy.sh*)",
        "Bash(*cutover-controller*)",
        "Bash(*pg-backup*)",
        "Bash(*pg-restore-drill*)",
        "Bash(*backup/lib.sh*)",
        "Bash(*/var/tmp/spawn/*.sh*)",
        "Bash(*/environ*)",
    ] {
        assert!(rules.iter().any(|rule| rule == program), "{program}");
    }
    // Killing by name or pattern; never `kill` itself, which stops the
    // engine's own job by its id.
    for kill in [
        "Bash(pkill:*)",
        "Bash(killall:*)",
        "Bash(*pkill *)",
        "Bash(*killall *)",
        "Bash(*kill *$(pgrep*)",
        "Bash(*pgrep *|*kill*)",
    ] {
        assert!(rules.iter().any(|rule| rule == kill), "{kill}");
    }
    assert!(
        !rules
            .iter()
            .any(|rule| rule == "Bash(kill:*)" || rule == "Bash(*kill *)"),
        "kill <pid> stays allowed"
    );
    // `kill -1` (every process the user may signal), never `kill -15`:
    // the patterns want -1 as a whole argument.
    for rule in [
        "Bash(*kill -1)",
        "Bash(*kill -1 *)",
        "Bash(*kill -- -1)",
        "Bash(*kill -- -1 *)",
        "Bash(*kill * -1)",
        "Bash(*kill * -1 *)",
    ] {
        assert!(rules.iter().any(|known| known == rule), "{rule}");
    }
    // The pipeline that hides the id: ps | grep | awk | xargs kill.
    for rule in ["Bash(*xargs kill*)", "Bash(*xargs * kill*)"] {
        assert!(rules.iter().any(|known| known == rule), "{rule}");
    }
    // Not the fleet's TMPDIR itself, only the scripts in it.
    assert!(
        !rules
            .iter()
            .any(|rule| rule.ends_with("/var/tmp/spawn/**)"))
    );
    // None of them names a workspace: the workspaces live under spawn-rsi.
    assert!(
        !rules
            .iter()
            .any(|rule| rule.contains("spawn-rsi*") || rule.contains("/ws"))
    );
}

/// Objective 206 (issue #42): every engine the Bridge starts refuses
/// cross-session peer messages, through the relay (cheap, then default
/// resumed), without one, and on the fallback when the relay is down, its
/// other settings as before: the deny list, no startup dialog, and the
/// relay's models where the relay is up.
#[test]
fn every_launch_refuses_cross_session_peer_messages() {
    assert_eq!(CROSS_SESSION_INBOUND, "refuse");
    let (_listener, up) = listening();
    let mut fleet = Fleet::new(Some(relay(&up)));
    fleet.start(Some(ModelProfile::Cheap));
    fleet.send(CommandAction::Stop);
    fleet.resume(Some(ModelProfile::Default));
    let through_relay = fleet.store.launches();
    let mut launches = through_relay.clone();
    for relay in [None, Some(relay(&closed()))] {
        let mut fleet = Fleet::new(relay);
        fleet.start(None);
        assert!(fleet.failure().is_none(), "{:?}", fleet.failure());
        launches.extend(fleet.store.launches());
    }
    assert_eq!(launches.len(), 4);
    for launch in &launches {
        let settings = Fleet::settings(launch);
        let value: Value = serde_json::from_str(&settings).expect("settings are JSON");
        assert_eq!(value["crossSessionInbound"], "refuse", "{settings}");
        assert_eq!(
            value["skipDangerousModePermissionPrompt"], true,
            "{settings}"
        );
        assert_eq!(denied(launch), CREDENTIAL_DENY, "{settings}");
    }
    // The relay's models are still set where it is up; none on the others.
    let cheap = env(&Fleet::settings(&through_relay[0])).expect("an env");
    assert_eq!(cheap["ANTHROPIC_BASE_URL"], up.as_str());
    assert_eq!(cheap["ANTHROPIC_MODEL"], "deepseek-v4-pro");
    let default = env(&Fleet::settings(&through_relay[1])).expect("an env");
    assert_eq!(default["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "deepseek-flash");
    assert!(default.get("ANTHROPIC_MODEL").is_none());
    for launch in &launches[2..] {
        assert!(env(&Fleet::settings(launch)).is_none());
    }
}

/// A launch environment withheld at the first launch and given after.
#[derive(Debug, Default)]
struct Later {
    asked: std::sync::atomic::AtomicUsize,
}

impl LaunchEnvironment for Later {
    fn environment(&self) -> Vec<(String, String)> {
        if self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Vec::new();
        }
        vec![(
            "SPAWN_TEST_DATABASE_URL".to_string(),
            "postgres://fleet@%2Ftmp%2Ffleet/spawn_fleet_test".to_string(),
        )]
    }
}

#[test]
fn the_launch_environment_is_asked_at_every_launch() {
    let later = Arc::new(Later::default());
    let launch: Arc<dyn LaunchEnvironment> = later.clone();
    let mut fleet = Fleet::configured(|config| {
        // A stale value of the same name is replaced, never doubled.
        config.environment.push((
            "SPAWN_TEST_DATABASE_URL".to_string(),
            "postgres://stale@%2Ftmp%2Fstale/x".to_string(),
        ));
        config.launch_environment = Some(launch);
    });
    fleet.start(None);
    fleet.send(CommandAction::Stop);
    fleet.resume(None);
    let launches = fleet.store.launches();
    assert_eq!(launches.len(), 2);
    assert_eq!(later.asked.load(std::sync::atomic::Ordering::SeqCst), 2);
    let urls = |launch: &Launch| -> Vec<String> {
        launch
            .environment
            .iter()
            .filter(|(name, _)| name == "SPAWN_TEST_DATABASE_URL")
            .map(|(_, value)| value.clone())
            .collect()
    };
    // Withheld at the first launch: the config's own value stays (the loop
    // puts none there), and at the second the given one replaces it.
    assert_eq!(urls(&launches[0]), ["postgres://stale@%2Ftmp%2Fstale/x"]);
    assert_eq!(
        urls(&launches[1]),
        ["postgres://fleet@%2Ftmp%2Ffleet/spawn_fleet_test"]
    );
    // Never on the command line.
    for launch in &launches {
        assert!(
            !launch
                .arguments
                .iter()
                .any(|argument| argument.contains("postgres://"))
        );
    }
}

/// The `kill` rules, run the way Claude Code matches them (a `*` is any run
/// of characters; the command is matched as a whole): `-1` as a whole
/// argument matches, `-15` and `-19` do not, and `kill <pid>` of an own
/// job matches nothing.
#[test]
fn the_kill_rules_catch_one_as_a_whole_argument_only() {
    fn matches(rule: &str, command: &str) -> bool {
        // Bash(<pattern>) or Bash(<prefix>:*)
        let body = &rule["Bash(".len()..rule.len() - 1];
        if let Some(prefix) = body.strip_suffix(":*") {
            return command.starts_with(prefix);
        }
        let pattern: Vec<char> = body.chars().collect();
        let text: Vec<char> = command.chars().collect();
        // Glob with `*`: dynamic programming over (pattern, text).
        let (n, m) = (pattern.len(), text.len());
        let mut reach = vec![vec![false; m + 1]; n + 1];
        reach[0][0] = true;
        for i in 1..=n {
            if pattern[i - 1] == '*' {
                reach[i][0] = reach[i - 1][0];
            }
        }
        for i in 1..=n {
            for j in 1..=m {
                reach[i][j] = match pattern[i - 1] {
                    '*' => reach[i - 1][j] || reach[i][j - 1],
                    c => reach[i - 1][j - 1] && c == text[j - 1],
                };
            }
        }
        reach[n][m]
    }
    let rules: Vec<&str> = CREDENTIAL_DENY
        .iter()
        .copied()
        .filter(|rule| rule.contains("kill"))
        .collect();
    let denied = |command: &str| rules.iter().any(|rule| matches(rule, command));
    for command in [
        "kill -1",
        "kill -1 1234",
        "kill -- -1",
        "kill -9 1234 -1",
        "kill -s TERM -1",
        "sudo kill -1",
    ] {
        assert!(denied(command), "{command}");
    }
    for command in [
        "kill 1234",
        "kill -15 1234",
        "kill -19 1234",
        "kill -TERM 1234",
        "kill -- 1234",
        "kill -9 1234",
    ] {
        assert!(!denied(command), "{command}");
    }
    for command in [
        "ps aux | grep worker | awk '{print $2}' | xargs kill",
        "ps -ef | awk '{print $2}' | xargs -r kill",
    ] {
        assert!(denied(command), "{command}");
    }
}

/// The launch summary (issue #79) each recorded execution start carries,
/// in order.
fn recorded_launches(fleet: &Fleet) -> Vec<Option<LaunchSummary>> {
    fleet
        .events()
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::ExecutionStarted { launch, .. } => Some(launch),
            _ => None,
        })
        .collect()
}

/// Issue #79: every start records what its engine was launched with, as
/// far as it is no secret: the relay's address, the main and small models
/// the inline settings pin, and the engine's own Claude Code directory.
#[test]
fn every_start_records_what_its_engine_was_launched_with() {
    let (_listener, up) = listening();
    let mut fleet = Fleet::new(Some(configured_relay(&up)));
    fleet.start(Some(ModelProfile::Cheap));
    fleet.send(CommandAction::Stop);
    fleet.resume(Some(ModelProfile::Default));
    assert!(fleet.failure().is_none(), "{:?}", fleet.failure());
    assert_eq!(
        recorded_launches(&fleet),
        [
            Some(LaunchSummary {
                relay_url: Some(up.clone()),
                model: Some("deepseek-v4-pro".to_string()),
                small_model: Some("deepseek-flash".to_string()),
                config_dir: None,
            }),
            Some(LaunchSummary {
                relay_url: Some(up.clone()),
                model: Some("claude-opus-5-5".to_string()),
                small_model: Some("deepseek-flash".to_string()),
                config_dir: None,
            }),
        ]
    );
    // A default relay that names no main model: Claude Code's own.
    let mut fleet = Fleet::new(Some(relay(&up)));
    fleet.start(None);
    assert_eq!(
        recorded_launches(&fleet),
        [Some(LaunchSummary {
            relay_url: Some(up.clone()),
            model: None,
            small_model: Some("deepseek-flash".to_string()),
            config_dir: None,
        })]
    );
}

/// The default profile's fallback when its relay is down names no relay:
/// the engine runs on Claude Code's own settings, and the record says so
/// by leaving the summary out, as one with no relay configured does.
#[test]
fn a_start_on_claude_s_own_settings_records_no_relay() {
    for relay in [None, Some(configured_relay(&closed()))] {
        let mut fleet = Fleet::new(relay);
        fleet.start(None);
        assert!(fleet.failure().is_none(), "{:?}", fleet.failure());
        assert_eq!(recorded_launches(&fleet), [None]);
        let settings = Fleet::settings(&fleet.store.launches()[0]);
        assert!(env(&settings).is_none(), "{settings}");
        assert_eq!(top_level_model(&settings), None, "{settings}");
    }
}

/// Issue #79: an engine on a Claude Code directory of its own
/// (`CLAUDE_CONFIG_DIR`, from `SPAWN_CLAUDE_CONFIG_DIR`) is denied that
/// directory's secrets as it is `~/.claude`'s, and its start records the
/// directory. Without one, the rules are exactly [`CREDENTIAL_DENY`].
#[test]
fn an_engine_on_its_own_claude_directory_is_denied_its_secrets() {
    let mut fleet = Fleet::configured(|config| {
        config.environment.push((
            "CLAUDE_CONFIG_DIR".to_string(),
            "/data/fleet-claude".to_string(),
        ));
    });
    fleet.start(None);
    assert!(fleet.failure().is_none(), "{:?}", fleet.failure());
    let launches = fleet.store.launches();
    assert!(
        launches[0]
            .environment
            .iter()
            .any(|(name, value)| name == "CLAUDE_CONFIG_DIR" && value == "/data/fleet-claude")
    );
    let rules = denied(&launches[0]);
    assert_eq!(&rules[..CREDENTIAL_DENY.len()], CREDENTIAL_DENY);
    let extra = &rules[CREDENTIAL_DENY.len()..];
    assert_eq!(extra, claude_dir_deny("/data/fleet-claude"));
    for rule in [
        "Read(//data/fleet-claude/settings*)",
        "Edit(//data/fleet-claude/settings*)",
        "Read(//data/fleet-claude/**/*credential*)",
        "Read(//data/fleet-claude/sessions/**)",
        "Read(//data/fleet-claude/.claude.json*)",
        "Edit(//data/fleet-claude/.claude.json*)",
        "Bash(*/data/fleet-claude*)",
        "Bash(*/fleet-claude*)",
        "Bash(*fleet-claude/*)",
        "Bash(*cd fleet-claude*)",
        "Bash(*CLAUDE_CONFIG_DIR*)",
    ] {
        assert!(extra.iter().any(|known| known == rule), "{rule}: {extra:?}");
    }
    assert_eq!(
        recorded_launches(&fleet),
        [Some(LaunchSummary {
            config_dir: Some("/data/fleet-claude".to_string()),
            ..LaunchSummary::default()
        })]
    );
}

/// The `~/.claude` Read and Edit rules of [`CREDENTIAL_DENY`] are
/// [`CLAUDE_DIR_SENSITIVE`] under `~/.claude`, and nothing else, both ways:
/// one list of what is sensitive, two places it is read, never drifting.
#[test]
fn the_claude_directory_rules_are_the_credential_rules_for_claude_s_own() {
    let listed: BTreeSet<&str> = CREDENTIAL_DENY
        .iter()
        .copied()
        .filter(|rule| rule.starts_with("Read(~/.claude/") || rule.starts_with("Edit(~/.claude/"))
        .collect();
    let from_list: BTreeSet<String> = ["Read", "Edit"]
        .iter()
        .flat_map(|tool| {
            CLAUDE_DIR_SENSITIVE
                .iter()
                .map(move |below| format!("{tool}(~/.claude/{below})"))
        })
        .collect();
    let generated: BTreeSet<String> = claude_dir_rules("~/.claude").into_iter().collect();
    assert_eq!(from_list, generated);
    assert_eq!(
        listed,
        generated
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
    );
    // Bash stays closed on all of `~/.claude`.
    for rule in ["Bash(*.claude/*)", "Bash(*/.claude*)", "Bash(*cd .claude*)"] {
        assert!(CREDENTIAL_DENY.contains(&rule), "{rule}");
    }
}

/// A relay's address in a record or a message is its scheme, host and
/// port: never a user, a password, a path or a query.
#[test]
fn a_relay_address_is_recorded_without_credentials() {
    for (url, origin) in [
        ("http://127.0.0.1:8791", Some("http://127.0.0.1:8791")),
        ("http://127.0.0.1:8791/", Some("http://127.0.0.1:8791")),
        ("http://localhost", Some("http://localhost")),
        (
            "https://relay.example:8443/v1?q=1#x",
            Some("https://relay.example:8443"),
        ),
        ("http://[::1]:8791/p", Some("http://[::1]:8791")),
        ("http://[fe80::1]", Some("http://[fe80::1]")),
        // Any user or password: nothing is shown.
        ("http://user:[redacted]@host:1", None),
        ("http://user:s3cret@host:1", None),
        ("http://u:se?cret@h", None),
        ("http://u:se#cret@h", None),
        ("http://u:se/cret@h", None),
        ("http://tok@[::1]:8791/p", None),
        ("http://a@b@host:1", None),
        // Not a host and a port.
        ("http://host:", None),
        ("http://host:80:81", None),
        ("http://host:8o", None),
        ("http://[::1", None),
        ("http://[::1]x", None),
        ("http://ho_st:1", None),
        ("127.0.0.1:8791", None),
        ("http://", None),
    ] {
        assert_eq!(LaunchSummary::relay_origin(url).as_deref(), origin, "{url}");
    }
}

/// Records made before the summary was kept read as before, and one
/// that says nothing is left out of the record.
#[test]
fn an_execution_start_without_a_launch_summary_reads_as_before() {
    let old = r#"{"name":"execution.started","detail":{"engine":"claude","workspace":"1:/workspaces/1","command_id":7}}"#;
    let kind: EventKind = serde_json::from_str(old).expect("an old record reads");
    let EventKind::ExecutionStarted { launch, model, .. } = &kind else {
        panic!("{kind:?}");
    };
    assert_eq!(*launch, None);
    assert_eq!(*model, ModelProfile::Default);
    let written = serde_json::to_value(&kind).expect("serialize");
    assert!(written["detail"].get("launch").is_none(), "{written}");

    let summary = LaunchSummary {
        relay_url: Some("http://127.0.0.1:8791".to_string()),
        small_model: Some("deepseek-flash".to_string()),
        ..LaunchSummary::default()
    };
    let EventKind::ExecutionStarted {
        engine,
        workspace,
        command_id,
        fork,
        model,
        transcript_from,
        ..
    } = kind
    else {
        unreachable!()
    };
    let new = EventKind::ExecutionStarted {
        engine,
        workspace,
        command_id,
        fork,
        model,
        transcript_from,
        launch: Some(summary.clone()),
    };
    let written = serde_json::to_value(&new).expect("serialize");
    assert_eq!(
        written["detail"]["launch"],
        serde_json::json!({"relay_url": "http://127.0.0.1:8791", "small_model": "deepseek-flash"})
    );
    let back: EventKind = serde_json::from_value(written).expect("deserialize");
    assert_eq!(back, new);
    assert!(LaunchSummary::default().is_empty());
    assert!(!summary.is_empty());
}
