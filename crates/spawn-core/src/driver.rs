//! Agent drivers: what the Bridge needs to know about one engine (issue
//! #91), behind one seam.
//!
//! An [`AgentDriver`] says how an engine is launched, how its native session
//! is named and where its transcript lies, how a transcript line and a pane
//! read, and how a startup dialog is answered. It does nothing itself: no
//! IO, no clock. The Bridge does the IO and asks the driver what it means.
//!
//! The registry is static. [`ClaudeDriver`] (Claude Code, [`DEFAULT`]) is
//! the only driver registered: a command naming any other engine still does
//! not parse ([`crate::command`]), and a logged record naming one is read as
//! Claude Code's, as before ([`driver_or_default`]).

use crate::bridge::{
    ModelRelay, claude_arguments, claude_session_id, claude_settings_in,
    transcript_path as claude_transcript_path,
};
use crate::event::ModelProfile;
use crate::id::{ExecutionId, WorkspaceId};
use crate::ingest::{Normalized, claude_line, claude_usage, claude_version};
use crate::screen::{ScreenReading, TRUST_KEYS, is_trust_dialog, read_claude_screen};

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
    /// sees it (Claude Code's `projects` directory).
    pub sessions: String,
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
/// lines read alone, so it only counts them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineCursor {
    /// Lines read through this cursor.
    pub lines: u64,
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

static CLAUDE: ClaudeDriver = ClaudeDriver;

static DRIVERS: [&dyn AgentDriver; 1] = [&CLAUDE];

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
        AgentDriver, ClaudeDriver, DEFAULT, DriverTraits, EngineHome, EngineLaunch, Guard,
        LineCursor, SessionBinding, TurnSource, driver, driver_or_default, drivers,
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

    #[test]
    fn registry_holds_claude_alone() {
        assert_eq!(DEFAULT, ENGINE);
        assert_eq!(drivers().len(), 1);
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
}
