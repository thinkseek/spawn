//! Agent drivers: what the Bridge needs to know about one engine (issue
//! #91), behind one seam.
//!
//! An [`AgentDriver`] says how an engine is launched, how its native session
//! is named and where its transcript lies, how a transcript line and a pane
//! read, and how a startup dialog is answered. It does nothing itself: no
//! IO, no clock. The Bridge does the IO and asks the driver what it means.
//!
//! The registry is static: [`ClaudeDriver`] (Claude Code, [`DEFAULT`]) and
//! [`AgyDriver`] (the Antigravity CLI, `agy`). A command naming any other
//! engine does not parse ([`crate::command`]), and a logged record naming
//! one is read as Claude Code's, as before ([`driver_or_default`]). A
//! registered engine runs only where the host enables it (`SPAWN_ENGINES`,
//! Claude Code alone by default) and, for agy, only on a terminal host that
//! detects agents itself (Herdr): its session is named by the engine and
//! told by the host ([`SessionBinding::Reported`]).

use crate::bridge::{
    ModelRelay, claude_arguments, claude_session_id, claude_settings_in,
    transcript_path as claude_transcript_path,
};
use crate::event::ModelProfile;
use crate::id::{ExecutionId, WorkspaceId};
use crate::ingest::{Normalized, agy_line, claude_line, claude_usage, claude_version};
use crate::screen::{
    AGY_TRUST_KEYS, ScreenReading, TRUST_KEYS, drop_agy_account_lines, is_agy_trust_dialog,
    is_trust_dialog, read_agy_screen, read_claude_screen,
};

/// The engine a record or a command that names none runs, and the one an
/// unknown name falls back on: [`crate::event::ENGINE`].
pub const DEFAULT: &str = crate::event::ENGINE;

/// How an engine's native session gets its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBinding {
    /// The Bridge names it before the engine starts
    /// ([`AgentDriver::new_session_id`]), so its transcript is known first.
    Assigned,
    /// The engine names it, and its output says which
    /// ([`AgentDriver::reported_session`]).
    Reported,
}

/// Where the Bridge reads a turn's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnSource {
    /// The engine's transcript says it; the screen confirms it.
    Transcript,
    /// Only the screen says it.
    Screen,
}

/// What keeps an engine off the host's credentials besides the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// Deny rules the engine enforces, handed over at launch (Claude Code's
    /// inline `--settings`).
    Settings,
    /// Nothing of the engine's own: the sandbox alone.
    Sandbox,
}

/// What an engine is like, for the Bridge and the pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverTraits {
    /// How its native session gets its id.
    pub session: SessionBinding,
    /// Where a turn's end is read.
    pub turns: TurnSource,
    /// What keeps it off the host's credentials.
    pub guard: Guard,
}

/// Where an engine lives on the terminal host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineHome {
    /// The engine's executable.
    pub program: String,
    /// The directory its session transcripts go to, as the terminal host
    /// sees it (Claude Code's `projects` directory, agy's home).
    pub sessions: String,
    /// The model it runs, by the engine's own label, when the engine is
    /// told one at launch (agy, `SPAWN_AGY_MODEL`); Claude Code takes its
    /// models from its settings.
    pub model: Option<String>,
}

/// What one launch asks of an engine, decided by the Bridge.
#[derive(Debug, Clone, Copy)]
pub struct EngineLaunch<'a> {
    /// The native session: a new one with that id, or the one resumed.
    pub native: Option<&'a str>,
    /// `native` names an earlier session to resume, not a new one.
    pub resume: bool,
    /// The model relay the engine goes through: the reachable one, or none.
    pub relay: Option<&'a ModelRelay>,
    /// The models it runs on.
    pub profile: ModelProfile,
    /// The engine's own configuration directory, when it is not the user's
    /// default one (`CLAUDE_CONFIG_DIR` for Claude Code).
    pub config_dir: Option<&'a str>,
    /// The directory it starts in (the workspace), which an engine that
    /// asks whether it trusts it is told it does (agy).
    pub cwd: Option<&'a str>,
}

/// How to start an engine: the Bridge runs `program` with `arguments`
/// under the runner, with `environment` over its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// The engine's executable.
    pub program: String,
    /// Its arguments; never the prompt, which is typed into the pane.
    pub arguments: Vec<String>,
    /// Variables the engine needs on top of the Bridge's; none for Claude
    /// Code.
    pub environment: Vec<(String, String)>,
    /// The engine's inline settings, as `arguments` carry them (empty for
    /// an engine that takes none).
    pub settings: String,
}

/// What one transcript line says.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LineFacts {
    /// Its events, a model call's usage first.
    pub events: Vec<Normalized>,
    /// The engine version it names.
    pub version: Option<String>,
}

/// What a driver keeps between the lines of one transcript. Claude Code's
/// lines read alone, so it only counts them. agy's do not
/// ([`crate::ingest::agy_line`]): a tool's result is the next step after
/// the call, with no id tying them, and a turn ends only once the
/// background work it opened has reported back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineCursor {
    /// Lines read through this cursor.
    pub lines: u64,
    /// The native session whose transcript this is: the first part of the
    /// keys of agy's events (the Bridge sets it).
    pub native: String,
    /// agy's tool calls whose result has not come yet: the step that made
    /// each and its place among the step's calls, oldest first.
    pub pending_tools: Vec<(u64, usize)>,
    /// agy's background jobs started and not reported finished
    /// (`<conversation>/task-<n>`).
    pub open_jobs: std::collections::BTreeSet<String>,
    /// agy's subagents created and not reported back (their conversation
    /// ids).
    pub open_subagents: std::collections::BTreeSet<String>,
    /// The step of the last agy line read: steps an interrupt left out
    /// (Esc writes no line) are gaps, never waited for.
    pub last_step: Option<u64>,
}

impl LineCursor {
    /// Whether agy work the transcript opened is still open: a background
    /// job or a subagent that has not reported back. The turn stays open
    /// while it is, whatever the screen's host says.
    pub fn work_open(&self) -> bool {
        !self.open_jobs.is_empty() || !self.open_subagents.is_empty()
    }
}

/// One engine, as the Bridge sees it. Implementations are pure: no IO, no
/// clock.
pub trait AgentDriver: Send + Sync + std::fmt::Debug {
    /// Wire name, as `execution.started` and commands carry it.
    fn id(&self) -> &'static str;

    /// Name for people.
    fn title(&self) -> &'static str;

    /// What it is like.
    fn traits(&self) -> DriverTraits;

    /// How to start it from `home` for `launch`.
    fn launch(&self, home: &EngineHome, launch: &EngineLaunch<'_>) -> LaunchPlan;

    /// The id of the native session a new execution starts, when the Bridge
    /// names it ([`SessionBinding::Assigned`]).
    fn new_session_id(&self, execution: &ExecutionId) -> Option<String>;

    /// Where the transcript of `native` lies for an engine started in
    /// `workspace`.
    fn transcript_path(&self, home: &EngineHome, workspace: &WorkspaceId, native: &str) -> String;

    /// The native session a line of output names, when the engine names its
    /// own ([`SessionBinding::Reported`]).
    fn reported_session(&self, line: &str) -> Option<String>;

    /// What one transcript line says.
    fn read_line(&self, line: &str, cursor: &mut LineCursor) -> LineFacts;

    /// What the tail of its pane shows.
    fn read_screen(&self, pane: &str) -> ScreenReading;

    /// The keys that answer a dialog the engine shows before it starts up
    /// (Claude Code's workspace trust dialog), when the pane shows one.
    fn startup_answer(&self, pane: &str) -> Option<&'static [&'static str]>;

    /// The pane as an excerpt may quote it (a start whose pane never got
    /// ready says what it showed last): the pane itself, unless the engine
    /// draws something there that Spawn must not record (agy's account
    /// line).
    fn excerpt_source(&self, pane: &str) -> String {
        pane.to_string()
    }

    /// How the run-time resolver answers `dialog`, shown by this engine
    /// started in `workspace` ([`table_answer`] for this engine's id): a
    /// fixed table, never a guess.
    fn dialog_answer(&self, dialog: &Dialog<'_>, workspace: &WorkspaceId) -> Answer {
        table_answer(self.id(), dialog, workspace)
    }
}

/// A dialog the run-time resolver found on a pane (design section 6.3),
/// as the terminal host's detection and Spawn's own detection
/// (`serve/approvals.rs`, `detect`) describe it.
#[derive(Debug, Clone, Copy)]
pub struct Dialog<'a> {
    /// The host's rule that read it blocked (Herdr's manifest rule id:
    /// `trust_prompt`, `permission_prompt`, ...), when the host reads one.
    pub rule: Option<&'a str>,
    /// Spawn's prompt kind (`permission`, `choice`, `yes_no`, `confirm`),
    /// or `trust` for an engine's workspace trust dialog.
    pub kind: &'a str,
    /// The label of the choice the dialog's cursor stands on, if drawn.
    pub cursor: Option<&'a str>,
    /// The labels of its choices, in order.
    pub choices: &'a [String],
    /// What the dialog asks to approve, one entry a line (its body), and
    /// for a trust dialog the lines that name the folder.
    pub body: &'a [String],
}

/// What the resolver does with a dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Press these keys (tmux names), in order, once.
    Keys(&'static [&'static str]),
    /// Press nothing: a person decides. The reason is said.
    Escalate(&'static str),
}

/// Why a row that has no captured screen yet answers nothing.
const UNCAPTURED: &str = "no captured screen of this dialog yet: a person answers it";

/// The resolver's table (design section 6.3), pure. A row whose dialog
/// was never seen working live (manifest text only, not captured, not
/// verified) escalates: it gets keys only in a later change that adds the
/// captured screen as a test. The table never holds an answer nobody has
/// seen work.
///
/// | engine | dialog | guard | answer | evidence |
/// |---|---|---|---|---|
/// | agy | `trust_prompt` | the folder shown is the workspace, cursor on "Yes, I trust this folder" | Enter | probed |
/// | agy | `permission_prompt` | — | escalate | manifest text only |
/// | agy | `question_prompt` | — | escalate | manifest text only |
/// | claude | trust | Spawn's `is_trust_dialog` (kind `trust`) | y, Enter | production |
/// | claude | numbered permission | choice 1 reads "Yes" | 1 | approvals fixtures |
/// | claude | question form, MCP elicitation, dynamic workflow | — | escalate | not captured |
/// | codex | `trust_directory`, `startup_update`, `live_strong_blocker` | — | escalate | not verified |
/// | opencode | `permission_required` | — | escalate | not verified |
/// | any | no row | — | escalate | — |
pub fn table_answer(engine: &str, dialog: &Dialog<'_>, workspace: &WorkspaceId) -> Answer {
    match (engine, dialog.rule, dialog.kind) {
        ("agy", Some("trust_prompt"), _) => {
            let folder = workspace.path().trim_end_matches('/');
            let shown = dialog
                .body
                .iter()
                .any(|line| line.trim().trim_end_matches('/') == folder);
            let trusting = dialog
                .cursor
                .is_some_and(|cursor| cursor.trim() == "Yes, I trust this folder");
            if shown && trusting {
                Answer::Keys(&crate::screen::AGY_TRUST_KEYS)
            } else {
                Answer::Escalate("agy's trust dialog names another folder, or its cursor is not on trust")
            }
        }
        ("agy", Some("permission_prompt" | "question_prompt"), _) => Answer::Escalate(UNCAPTURED),
        ("claude", _, "trust") => Answer::Keys(&crate::screen::TRUST_KEYS),
        ("claude", None | Some("bash_permission_prompt" | "generic_permission_prompt"), "permission")
            if dialog
                .choices
                .first()
                .is_some_and(|first| first.trim() == "Yes") =>
        {
            Answer::Keys(&CLAUDE_YES)
        }
        (
            "claude",
            Some("live_blocked_form" | "mcp_elicitation_prompt" | "dynamic_workflow_prompt"),
            _,
        ) => Answer::Escalate(UNCAPTURED),
        (
            "codex",
            Some("trust_directory" | "startup_update" | "live_strong_blocker"),
            _,
        )
        | ("opencode", Some("permission_required"), _) => Answer::Escalate(UNCAPTURED),
        _ => Answer::Escalate("no row of the resolver's table covers this dialog"),
    }
}

/// Claude Code's first numbered choice ("1. Yes").
const CLAUDE_YES: [&str; 1] = ["1"];

/// Screens no key may answer: the engine cannot go on without a person
/// (design section 6.3, "Hard blocks"). Lowercased text; every one is from
/// the engines' own strings and docs, none captured on a live pane yet.
const HARD_BLOCKS: [(&str, &str); 12] = [
    ("please run /login", "login required"),
    ("login required", "login required"),
    ("not logged in", "login required"),
    ("oauth token has expired", "login expired"),
    ("authentication_error", "login expired"),
    ("rate limit reached", "rate limit"),
    ("rate_limit_error", "rate limit"),
    ("usage limit reached", "quota reached"),
    ("quota exceeded", "quota reached"),
    ("resource_exhausted", "quota reached"),
    ("update required", "update required"),
    ("please update to continue", "update required"),
];

/// Why the pane's tail is a hard block, if it is one: a login required or
/// expired, a quota or rate limit reached, an update the engine needs
/// before it runs, or a dialog whose only choices exit. Read from the last
/// [`crate::screen::TAIL_LINES`] lines only; the resolver escalates it at
/// once and never presses a key.
pub fn hard_block(pane: &str, choices: &[String]) -> Option<&'static str> {
    let lines: Vec<&str> = pane.lines().collect();
    let tail = lines[lines.len().saturating_sub(crate::screen::TAIL_LINES)..]
        .join("\n")
        .to_lowercase();
    if let Some((_, why)) = HARD_BLOCKS.iter().find(|(text, _)| tail.contains(text)) {
        return Some(why);
    }
    let exits = |label: &String| {
        let lower = label.to_lowercase();
        ["exit", "quit"]
            .iter()
            .any(|word| lower.split(|c: char| !c.is_alphanumeric()).any(|w| w == *word))
    };
    (!choices.is_empty() && choices.iter().all(exits)).then_some("only exit choices")
}

/// Claude Code, interactive in a terminal: everything it does is the
/// Bridge's existing Claude functions, unchanged.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeDriver;

impl AgentDriver for ClaudeDriver {
    fn id(&self) -> &'static str {
        crate::event::ENGINE
    }

    fn title(&self) -> &'static str {
        "Claude Code"
    }

    fn traits(&self) -> DriverTraits {
        DriverTraits {
            session: SessionBinding::Assigned,
            turns: TurnSource::Transcript,
            guard: Guard::Settings,
        }
    }

    fn launch(&self, home: &EngineHome, launch: &EngineLaunch<'_>) -> LaunchPlan {
        let settings = claude_settings_in(launch.relay, launch.profile, launch.config_dir);
        LaunchPlan {
            program: home.program.clone(),
            arguments: claude_arguments(launch.native, launch.resume, &settings),
            environment: Vec::new(),
            settings,
        }
    }

    fn new_session_id(&self, execution: &ExecutionId) -> Option<String> {
        Some(claude_session_id(execution))
    }

    fn transcript_path(&self, home: &EngineHome, workspace: &WorkspaceId, native: &str) -> String {
        claude_transcript_path(&home.sessions, workspace, native)
    }

    fn reported_session(&self, _line: &str) -> Option<String> {
        None
    }

    fn read_line(&self, line: &str, cursor: &mut LineCursor) -> LineFacts {
        cursor.lines = cursor.lines.saturating_add(1);
        let mut events = claude_line(line);
        if let Some(usage) = claude_usage(line) {
            events.insert(0, usage);
        }
        LineFacts {
            events,
            version: claude_version(line),
        }
    }

    fn read_screen(&self, pane: &str) -> ScreenReading {
        read_claude_screen(pane)
    }

    fn startup_answer(&self, pane: &str) -> Option<&'static [&'static str]> {
        is_trust_dialog(pane).then_some(&TRUST_KEYS[..])
    }
}

/// agy's default home, below the user's: `~/.gemini/antigravity-cli`
/// (`SPAWN_AGY_HOME` names another).
pub const AGY_HOME: &str = ".gemini/antigravity-cli";

/// The Antigravity CLI (`agy`), interactive in a terminal, on a host that
/// detects agents (Herdr). It names its own conversation, which the host
/// reports ([`SessionBinding::Reported`]); its full transcript
/// (`transcript_full.jsonl`, never the short one, which cuts long contents
/// at about 4 KB) says what it did ([`agy_line`]). Its permissions are
/// skipped and its workspace trusted at launch; nothing of its own keeps
/// it off the host's credentials, so the sandbox does ([`Guard::Sandbox`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct AgyDriver;

/// agy's launch arguments: always the model (agy falls back silently to
/// its own, costly, default on an omitted or unknown one), permissions
/// skipped, the workspace trusted and every tool allowed. A resume names
/// the conversation and repeats every flag: agy keeps none of them.
pub fn agy_arguments(model: &str, cwd: &str, resume: Option<&str>) -> Vec<String> {
    let settings = serde_json::json!({
        "trustedWorkspaces": [cwd],
        "toolPermission": "always-proceed",
    })
    .to_string();
    let mut arguments = Vec::new();
    if let Some(conversation) = resume {
        arguments.extend(["--conversation".to_string(), conversation.to_string()]);
    }
    arguments.extend([
        "--model".to_string(),
        model.to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--settings".to_string(),
        settings,
    ]);
    arguments
}

/// Whether `native` may name an agy conversation in a path: a plain id
/// (letters, digits, `-`, `_`), never a path of its own. The host reports
/// it; the Bridge builds a path from it.
pub fn plain_session_id(native: &str) -> bool {
    !native.is_empty()
        && native.len() <= 128
        && native
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

impl AgentDriver for AgyDriver {
    fn id(&self) -> &'static str {
        "agy"
    }

    fn title(&self) -> &'static str {
        "Antigravity CLI"
    }

    fn traits(&self) -> DriverTraits {
        DriverTraits {
            session: SessionBinding::Reported,
            turns: TurnSource::Transcript,
            guard: Guard::Sandbox,
        }
    }

    /// The model is the home's (the Bridge refuses a start with none);
    /// the settings are the inline JSON the arguments carry.
    fn launch(&self, home: &EngineHome, launch: &EngineLaunch<'_>) -> LaunchPlan {
        let arguments = agy_arguments(
            home.model.as_deref().unwrap_or_default(),
            launch.cwd.unwrap_or_default(),
            launch.native.filter(|_| launch.resume),
        );
        let settings = arguments.last().cloned().unwrap_or_default();
        LaunchPlan {
            program: home.program.clone(),
            arguments,
            environment: Vec::new(),
            settings,
        }
    }

    fn new_session_id(&self, _execution: &ExecutionId) -> Option<String> {
        None
    }

    /// `<home>/brain/<conversation>/.system_generated/logs/transcript_full.jsonl`.
    fn transcript_path(&self, home: &EngineHome, _workspace: &WorkspaceId, native: &str) -> String {
        format!(
            "{}/brain/{native}/.system_generated/logs/transcript_full.jsonl",
            home.sessions.trim_end_matches('/')
        )
    }

    /// agy's lines never name its conversation: the host reports it.
    fn reported_session(&self, _line: &str) -> Option<String> {
        None
    }

    fn read_line(&self, line: &str, cursor: &mut LineCursor) -> LineFacts {
        cursor.lines = cursor.lines.saturating_add(1);
        LineFacts {
            events: agy_line(line, cursor),
            version: None,
        }
    }

    fn read_screen(&self, pane: &str) -> ScreenReading {
        read_agy_screen(pane)
    }

    fn startup_answer(&self, pane: &str) -> Option<&'static [&'static str]> {
        is_agy_trust_dialog(pane).then_some(&AGY_TRUST_KEYS[..])
    }

    fn excerpt_source(&self, pane: &str) -> String {
        drop_agy_account_lines(pane)
    }
}

static CLAUDE: ClaudeDriver = ClaudeDriver;

static AGY: AgyDriver = AgyDriver;

static DRIVERS: [&dyn AgentDriver; 2] = [&CLAUDE, &AGY];

/// Every registered driver.
pub fn drivers() -> &'static [&'static dyn AgentDriver] {
    &DRIVERS
}

/// The driver registered as `id`.
pub fn driver(id: &str) -> Option<&'static dyn AgentDriver> {
    drivers().iter().copied().find(|driver| driver.id() == id)
}

/// The driver registered as `id`, else [`DEFAULT`]'s: a logged record that
/// names an engine no driver is registered for reads as it always did.
pub fn driver_or_default(id: &str) -> &'static dyn AgentDriver {
    driver(id).unwrap_or(&CLAUDE)
}

#[cfg(test)]
mod tests {
    use super::{
        AgentDriver, AgyDriver, ClaudeDriver, DEFAULT, DriverTraits, EngineHome, EngineLaunch,
        Guard, LineCursor, SessionBinding, TurnSource, agy_arguments, driver, driver_or_default,
        drivers, plain_session_id,
    };
    use crate::bridge::{
        ModelRelay, claude_arguments, claude_session_id, claude_settings, claude_settings_in,
        transcript_path,
    };
    use crate::event::{ENGINE, ModelProfile};
    use crate::id::{CommandId, ExecutionId, WorkspaceId};
    use crate::ingest::{claude_line, claude_usage, claude_version};
    use crate::screen::{TRUST_KEYS, is_trust_dialog, read_claude_screen};

    fn home() -> EngineHome {
        EngineHome {
            program: "/usr/local/bin/claude".to_string(),
            sessions: "/root/.claude/projects/".to_string(),
            model: None,
        }
    }

    fn relays() -> Vec<Option<ModelRelay>> {
        vec![
            None,
            Some(ModelRelay {
                url: "http://127.0.0.1:4100".to_string(),
                cheap_model: "deepseek-chat".to_string(),
                small_model: "deepseek-small".to_string(),
                ..ModelRelay::default()
            }),
            Some(ModelRelay {
                url: "http://127.0.0.1:4100".to_string(),
                cheap_model: "deepseek-chat".to_string(),
                small_model: "deepseek-small".to_string(),
                default_model: Some("claude-main".to_string()),
                sonnet_model: Some("claude-sonnet".to_string()),
                opus_model: Some("claude-opus".to_string()),
                subagent_model: Some("claude-sub".to_string()),
            }),
        ]
    }

    #[test]
    fn launch_is_claude_arguments_with_claude_settings_in() {
        let driver = ClaudeDriver;
        for relay in relays() {
            for profile in [ModelProfile::Default, ModelProfile::Cheap] {
                for config_dir in [None, Some("/home/agent/.claude-7")] {
                    for (native, resume) in [
                        (None, false),
                        (Some("4f1c2b3a-0000-4000-8000-000000000001"), false),
                        (Some("4f1c2b3a-0000-4000-8000-000000000001"), true),
                    ] {
                        let plan = driver.launch(
                            &home(),
                            &EngineLaunch {
                                native,
                                resume,
                                relay: relay.as_ref(),
                                profile,
                                config_dir,
                                cwd: Some("/workspaces/agent_a"),
                            },
                        );
                        let settings = claude_settings_in(relay.as_ref(), profile, config_dir);
                        assert_eq!(plan.settings, settings);
                        assert_eq!(plan.arguments, claude_arguments(native, resume, &settings));
                        assert_eq!(plan.program, "/usr/local/bin/claude");
                        assert!(plan.environment.is_empty(), "no new variables for Claude");
                        if config_dir.is_none() {
                            assert_eq!(plan.settings, claude_settings(relay.as_ref(), profile));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn new_session_id_is_claude_session_id() {
        for command in [0, 1, 42, u64::MAX] {
            let execution = ExecutionId::for_command(CommandId(command));
            assert_eq!(
                ClaudeDriver.new_session_id(&execution),
                Some(claude_session_id(&execution))
            );
        }
    }

    #[test]
    fn transcript_path_is_claude_transcript_path() {
        let workspace = WorkspaceId::new("3:/workspaces/agent_a.b").expect("workspace");
        for sessions in ["/root/.claude/projects", "/root/.claude/projects/", "/p"] {
            let home = EngineHome {
                program: "claude".to_string(),
                sessions: sessions.to_string(),
                model: None,
            };
            assert_eq!(
                ClaudeDriver.transcript_path(&home, &workspace, "abc"),
                transcript_path(sessions, &workspace, "abc")
            );
        }
    }

    #[test]
    fn read_line_is_claude_line_usage_first_and_version() {
        let lines = [
            r#"{"type":"assistant","version":"2.1.0","timestamp":"2026-11-25T10:00:00.000Z","uuid":"u1","message":{"id":"msg_1","model":"claude-x","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":4}}}"#,
            r#"{"type":"user","version":"2.1.0","timestamp":"2026-11-25T10:00:01.000Z","uuid":"u2","message":{"role":"user","content":"do it"}}"#,
            "not json",
            "",
        ];
        let mut cursor = LineCursor::default();
        let mut usage_seen = false;
        for line in lines {
            let facts = ClaudeDriver.read_line(line, &mut cursor);
            let mut expected = claude_line(line);
            if let Some(usage) = claude_usage(line) {
                usage_seen = true;
                expected.insert(0, usage);
            }
            assert_eq!(facts.events, expected);
            assert_eq!(facts.version, claude_version(line));
        }
        assert!(usage_seen, "the sample holds a model call's usage");
        assert_eq!(cursor.lines, 4);
        assert_eq!(ClaudeDriver.reported_session(lines[0]), None);
    }

    #[test]
    fn read_screen_and_startup_answer_are_claude_screen() {
        let trust = "Do you trust the files in this folder?\n\ny. Yes, I trust this folder\n\
                     n. No, exit\nEnter y/n:\nEnter to confirm · Esc to cancel\n";
        let panes = [
            trust,
            "╭────╮\n│ > │\n╰────╯\n",
            "some output\n* Thinking… (esc to interrupt)\n",
            "",
        ];
        for pane in panes {
            assert_eq!(ClaudeDriver.read_screen(pane), read_claude_screen(pane));
            assert_eq!(
                ClaudeDriver.startup_answer(pane),
                is_trust_dialog(pane).then_some(&TRUST_KEYS[..])
            );
        }
        assert_eq!(ClaudeDriver.startup_answer(trust), Some(&TRUST_KEYS[..]));
    }

    /// Claude Code is the registry's first driver and the default every
    /// unknown name falls back on (agy, registered beside it since issue
    /// #91 P2 PR 2, is checked in `registry_holds_agy_beside_claude`).
    #[test]
    fn registry_holds_claude_alone() {
        assert_eq!(DEFAULT, ENGINE);
        assert_eq!(drivers().len(), 2);
        assert_eq!(drivers()[0].id(), ENGINE);
        assert_eq!(driver("claude").map(AgentDriver::id), Some("claude"));
        assert!(driver("codex").is_none());
        assert!(driver("").is_none());
        assert!(driver("Claude").is_none());
        assert_eq!(driver_or_default("codex").id(), ENGINE);
        assert_eq!(driver_or_default("").id(), ENGINE);
        assert_eq!(driver_or_default(ENGINE).id(), ENGINE);
    }

    #[test]
    fn claude_traits_and_title() {
        assert_eq!(ClaudeDriver.title(), "Claude Code");
        assert_eq!(
            ClaudeDriver.traits(),
            DriverTraits {
                session: SessionBinding::Assigned,
                turns: TurnSource::Transcript,
                guard: Guard::Settings,
            }
        );
    }

    fn agy_home() -> EngineHome {
        EngineHome {
            program: "/opt/agy/bin/agy".to_string(),
            sessions: "/home/a/.gemini/antigravity-cli/".to_string(),
            model: Some("gemini-3.8-flash-low".to_string()),
        }
    }

    #[test]
    fn registry_holds_agy_beside_claude() {
        assert_eq!(drivers()[1].id(), "agy");
        assert_eq!(driver("agy").map(AgentDriver::id), Some("agy"));
        assert_eq!(driver_or_default("agy").id(), "agy");
        assert!(driver("AGY").is_none());
        assert_eq!(AgyDriver.title(), "Antigravity CLI");
        assert_eq!(
            AgyDriver.traits(),
            DriverTraits {
                session: SessionBinding::Reported,
                turns: TurnSource::Transcript,
                guard: Guard::Sandbox,
            }
        );
        let execution = ExecutionId::for_command(CommandId(7));
        assert_eq!(AgyDriver.new_session_id(&execution), None);
        assert_eq!(AgyDriver.reported_session("{}"), None);
    }

    /// Every launch names the model, skips permissions and trusts the
    /// workspace; a resume names the conversation and repeats every flag.
    #[test]
    fn agy_launch_always_names_the_model_and_repeats_its_flags_on_a_resume() {
        let settings =
            r#"{"toolPermission":"always-proceed","trustedWorkspaces":["/workspaces/agent_a"]}"#;
        let flags = [
            "--model",
            "gemini-3.8-flash-low",
            "--dangerously-skip-permissions",
            "--settings",
            settings,
        ];
        for (native, resume) in [
            (None, false),
            (Some("conv-1"), true),
            (Some("conv-1"), false),
        ] {
            let plan = AgyDriver.launch(
                &agy_home(),
                &EngineLaunch {
                    native,
                    resume,
                    relay: None,
                    profile: ModelProfile::Cheap,
                    config_dir: Some("/x"),
                    cwd: Some("/workspaces/agent_a"),
                },
            );
            assert_eq!(plan.program, "/opt/agy/bin/agy");
            let mut expected: Vec<String> = Vec::new();
            if resume {
                expected.extend(["--conversation".to_string(), "conv-1".to_string()]);
            }
            expected.extend(flags.iter().map(|flag| (*flag).to_string()));
            assert_eq!(plan.arguments, expected);
            assert_eq!(plan.settings, settings);
            assert!(plan.environment.is_empty());
            let parsed: serde_json::Value = serde_json::from_str(&plan.settings).expect("json");
            assert_eq!(parsed["trustedWorkspaces"][0], "/workspaces/agent_a");
        }
        assert_eq!(
            agy_arguments("m", "/w", Some("c"))[..4],
            ["--conversation", "c", "--model", "m"]
        );
    }

    /// The full transcript, never the short one agy truncates.
    #[test]
    fn agy_transcript_is_the_full_one_below_its_home() {
        let workspace = WorkspaceId::new("3:/workspaces/agent_a").expect("workspace");
        assert_eq!(
            AgyDriver.transcript_path(&agy_home(), &workspace, "11111111-2222"),
            "/home/a/.gemini/antigravity-cli/brain/11111111-2222/.system_generated/logs/transcript_full.jsonl"
        );
        assert!(plain_session_id("11111111-2222-4333-8444-555555555555"));
        for bad in ["", "../x", "a/b", "a b", &"x".repeat(129)] {
            assert!(!plain_session_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn agy_reads_its_lines_screen_and_trust_dialog() {
        let mut cursor = LineCursor {
            native: "c".to_string(),
            ..LineCursor::default()
        };
        let facts = AgyDriver.read_line(
            r#"{"step_index":0,"type":"USER_INPUT","status":"DONE","content":"hi"}"#,
            &mut cursor,
        );
        assert_eq!(facts.events.len(), 1);
        assert_eq!(facts.version, None);
        assert_eq!(cursor.lines, 1);
        let trust = "Do you trust the contents of this project?\n> Yes, I trust this folder\n  No, exit\n  ↑/↓ Navigate · enter Confirm\n";
        assert_eq!(AgyDriver.startup_answer(trust), Some(&["Enter"][..]));
        assert_eq!(ClaudeDriver.startup_answer(trust), None);
        let rule = "──────────";
        let idle = format!("{rule}\n>\n{rule}\n? for shortcuts\n");
        assert_eq!(
            AgyDriver.read_screen(&idle).state,
            crate::screen::ScreenState::Ready
        );
        assert_eq!(AgyDriver.startup_answer(&idle), None);
        let banner = "▀▀▀▀▀▀ someone@example.invalid (Pro)\nok\n";
        assert_eq!(AgyDriver.excerpt_source(banner), "ok\n");
        assert_eq!(ClaudeDriver.excerpt_source(banner), banner);
    }
}
