//! Agent drivers: what the Bridge needs to know about one engine (issue
//! #91 / #113 D1), behind a narrow 3-hook seam plus static declarative specs.
//!
//! An [`AgentDriver`] exposes its static [`DriverSpec`] (metadata and
//! declarative [`ScreenSpec`] rules for screen reading, startup dialogs,
//! excerpt masking, startup refusals, dialog resolution, and hard blocks) and
//! three runtime hooks: [`AgentDriver::launch`], [`AgentDriver::read_line`],
//! and [`AgentDriver::session_binding`] ([`NativeSessionHook::Assigned`] vs
//! [`NativeSessionHook::Reported`]). Drivers perform no IO and read no clock.
//!
//! The registry is static: [`ClaudeDriver`] (Claude Code, [`DEFAULT`]),
//! [`AgyDriver`] (the Antigravity CLI, `agy`), and [`CodexDriver`] (OpenAI
//! Codex CLI, `codex`). A command naming any other engine does not parse
//! ([`crate::command`]), and a logged record naming one falls back to Claude
//! Code ([`driver_or_default`]). A registered engine runs only where the host
//! enables it (`SPAWN_ENGINES`, Claude Code alone by default).

use crate::bridge::{
    ModelRelay, claude_arguments, claude_session_id, claude_settings_in,
    transcript_path as claude_transcript_path,
};
use crate::event::ModelProfile;
use crate::id::{ExecutionId, WorkspaceId};
use crate::ingest::{Normalized, agy_line, claude_line, claude_usage, claude_version};
use crate::screen::{
    AGY_TRUST_KEYS, ScreenReading, TAIL_LINES, TRUST_KEYS, agy_unknown_model,
    drop_agy_account_lines, is_agy_trust_dialog, is_trust_dialog, read_agy_screen,
    read_claude_screen, strip_ansi,
};

/// The engine a record or a command that names none runs, and the one an
/// unknown name falls back on: [`crate::event::ENGINE`].
pub const DEFAULT: &str = crate::event::ENGINE;

/// How an engine's native session gets its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBinding {
    /// The Bridge names it before the engine starts
    /// ([`NativeSessionHook::Assigned`]), so its transcript is known first.
    Assigned,
    /// The engine names it, and its output or host reports which
    /// ([`NativeSessionHook::Reported`]).
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
    /// Nothing of the engine's own: the sandbox alone, which is a
    /// filesystem view (decision C2, N6): the Bridge starts such an engine
    /// only inside the host's view ([`crate::port::EngineView`], `bwrap`),
    /// and refuses it (`engine_unavailable`) on a host that requires one
    /// (`SPAWN_ENGINE_VIEW=required`, the default) and has none
    /// ([`crate::bridge::EngineViewPolicy`]).
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
    /// Optional off-home data root (`SPAWN_AGY_DATA_DIR`) where per-worker
    /// isolated engine state and/or shared `brain/` storage live.
    pub data_dir: Option<String>,
    /// Whether each worker gets an isolated `brain/` directory under
    /// [`Self::data_dir`] (`SPAWN_AGY_ISOLATE_BRAIN=1|true|yes|on` when
    /// `data_dir` is set; default `false`).
    pub isolate_brain: bool,
}

/// Sanitize a worker id or workspace basename into a safe single path
/// component keeping only `[A-Za-z0-9._-]`.
#[must_use]
pub fn sanitize_worker_key(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        "worker".to_string()
    } else {
        sanitized
    }
}

fn clean_root_path(raw: &str) -> std::path::PathBuf {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() && raw.starts_with('/') {
        std::path::PathBuf::from("/")
    } else {
        std::path::PathBuf::from(trimmed)
    }
}

impl EngineHome {
    /// Directory under which engine transcripts and session logs are scanned
    /// (`data_dir` when redirected off `$HOME`, else `sessions`).
    #[must_use]
    pub fn transcript_root(&self) -> &str {
        self.data_dir.as_deref().unwrap_or(&self.sessions)
    }

    /// The deterministic per-worker `brain/` directory under [`Self::data_dir`]
    /// when both `data_dir` and [`Self::isolate_brain`] are enabled.
    #[must_use]
    pub fn worker_brain_dir(&self, worker_id: &str) -> Option<std::path::PathBuf> {
        let root = self
            .data_dir
            .as_deref()
            .map(str::trim)
            .filter(|dir| !dir.is_empty())?;
        if !self.isolate_brain {
            return None;
        }
        Some(
            clean_root_path(root)
                .join("workers")
                .join(sanitize_worker_key(worker_id))
                .join("home")
                .join("brain"),
        )
    }

    /// The shared `brain/` directory under [`Self::data_dir`] when `data_dir`
    /// is set and [`Self::isolate_brain`] is false.
    #[must_use]
    pub fn shared_brain_dir(&self) -> Option<std::path::PathBuf> {
        let root = self
            .data_dir
            .as_deref()
            .map(str::trim)
            .filter(|dir| !dir.is_empty())?;
        (!self.isolate_brain).then(|| clean_root_path(root).join("brain"))
    }

    /// All transcript roots associated with this engine home, for credential
    /// masking (`CollectorMask`) and archive path checks.
    #[must_use]
    pub fn transcript_roots(&self, worker_id: Option<&str>) -> Vec<std::path::PathBuf> {
        let mut roots = vec![std::path::PathBuf::from(&self.sessions)];
        if let Some(root) = self
            .data_dir
            .as_deref()
            .map(str::trim)
            .filter(|dir| !dir.is_empty())
        {
            let root_path = clean_root_path(root);
            let extra = if let Some(worker_id) = worker_id {
                if self.isolate_brain {
                    vec![
                        root_path
                            .join("workers")
                            .join(sanitize_worker_key(worker_id))
                            .join("home"),
                    ]
                } else {
                    vec![root_path.join("brain")]
                }
            } else {
                vec![root_path.join("workers"), root_path.join("brain")]
            };
            for path in extra {
                if !roots.contains(&path) {
                    roots.push(path);
                }
            }
        }
        roots
    }
}

/// The engine's native session for one launch: exactly one of three, so a
/// resume always names the session it resumes.
///
/// ```compile_fail,E0308
/// use spawn_core::driver::NativeSession;
/// let resume_nothing: NativeSession<'static> = NativeSession::Resume;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSession<'a> {
    /// A new session the engine names itself (agy, Codex) or, for an
    /// engine the Bridge names, none named yet.
    Unnamed,
    /// A new session with the id the Bridge assigned ([`AgentDriver::new_session_id`]).
    New(&'a str),
    /// The earlier session with this id, resumed.
    Resume(&'a str),
}

impl<'a> NativeSession<'a> {
    /// The id of the session resumed, for an engine that only takes an id
    /// to resume (agy, Codex): a new session, named or not, passes none.
    pub fn resumed(self) -> Option<&'a str> {
        match self {
            NativeSession::Resume(id) => Some(id),
            NativeSession::Unnamed | NativeSession::New(_) => None,
        }
    }
}

/// What one launch asks of an engine, decided by the Bridge.
#[derive(Debug, Clone, Copy)]
pub struct EngineLaunch<'a> {
    /// The native session: unnamed, new with an assigned id, or resumed.
    pub session: NativeSession<'a>,
    /// The model relay the engine goes through: the reachable one, or none.
    pub relay: Option<&'a ModelRelay>,
    /// The models it runs on.
    pub profile: ModelProfile,
    /// The engine's own configuration directory, when resolved (`CLAUDE_CONFIG_DIR`
    /// for Claude Code — isolated by default, or overridden via `SPAWN_CLAUDE_CONFIG_DIR`).
    pub config_dir: Option<&'a str>,
    /// The directory it starts in (the workspace), which an engine that
    /// asks whether it trusts it is told it does (agy).
    pub cwd: Option<&'a str>,
    /// The model it runs, by the engine's own label, for an engine told
    /// its model at launch: the start's `model_label` (the task's graded
    /// preset's), else [`EngineHome::model`]. Claude Code never reads it.
    pub model: Option<&'a str>,
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

/// A pending agy tool call that asks to cancel or kill background work,
/// resolved only when its matching `GENERIC` step succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingCancel {
    /// `manage_task` killing, cancelling, or stopping a specific task id.
    Task {
        /// Task identifier named in the tool call arguments.
        job_id: String,
        /// End reason recorded if the call succeeds.
        how: crate::event::JobEnd,
    },
    /// `manage_subagents` killing specific subagent conversation ids.
    Subagents {
        /// Subagent conversation identifiers named in the tool call arguments.
        ids: Vec<String>,
        /// End reason recorded if the call succeeds.
        how: crate::event::JobEnd,
    },
    /// `manage_subagents` killing all active subagents (`Action: "kill_all"`).
    AllSubagents {
        /// End reason recorded if the call succeeds.
        how: crate::event::JobEnd,
    },
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
    /// agy's pending kill/cancel calls, keyed by `(step_index, call_index)`
    /// from [`Self::pending_tools`], applied only when their matching
    /// `GENERIC` step succeeds.
    pub pending_cancels: std::collections::BTreeMap<(u64, usize), PendingCancel>,
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

/// Declarative startup trust dialog match and keys (`#113` D1).
#[derive(Debug, Clone, Copy)]
pub struct StartupDialogRule {
    /// Pure predicate checking whether the trailing pane shows this dialog.
    pub matches: fn(&str) -> bool,
    /// Keys (tmux / Herdr names) pressed in order when `matches` holds.
    pub keys: &'static [&'static str],
}

/// Declarative fatal startup refusal evaluation rule (`#113` D1).
#[derive(Debug, Clone, Copy)]
pub enum StartRefusalRule {
    /// This engine defines no deterministic startup refusal rule.
    None,
    /// Match lowercased substrings against the trailing non-empty masked lines.
    TailNeedles {
        /// Lowercased substrings that cause immediate startup refusal.
        needles: &'static [&'static str],
        /// Error prefix prepended to the matched line excerpt.
        prefix: &'static str,
        /// Maximum evidence characters copied from the matched line.
        max_chars: usize,
    },
    /// Run a dedicated pure pane classifier returning the raw offending line.
    Classifier {
        /// Classifier returning the raw offending line if startup is refused.
        detect: fn(&str) -> Option<String>,
        /// Error prefix prepended to the detected line.
        prefix: &'static str,
    },
}

/// Declarative screen, dialog, masking, and hard-block specification (`#113` D1).
#[derive(Debug, Clone, Copy)]
pub struct ScreenSpec {
    /// Tail pane state classifier.
    pub read_screen: fn(&str) -> ScreenReading,
    /// Optional startup trust dialog rule answered before the first prompt.
    pub startup_dialog: Option<StartupDialogRule>,
    /// Optional pane sanitizer applied before external excerpts leave the host.
    pub excerpt_mask: Option<fn(&str) -> String>,
    /// Startup refusal rule checked while waiting for the initial prompt.
    pub start_refusal: StartRefusalRule,
    /// Declarative run-time dialog resolution rows for this engine.
    pub dialog_rules: &'static [DialogRule],
    /// Declarative hard-block tail rules for this engine.
    pub hard_block_rules: &'static [HardBlockRule],
}

impl ScreenSpec {
    /// Classify the tail of `pane`.
    pub fn read_screen(&self, pane: &str) -> ScreenReading {
        (self.read_screen)(pane)
    }

    /// Keys answering a startup dialog on `pane`, if configured and shown.
    pub fn startup_answer(&self, pane: &str) -> Option<&'static [&'static str]> {
        self.startup_dialog
            .and_then(|rule| (rule.matches)(pane).then_some(rule.keys))
    }

    /// Pane masked per this engine's fail-closed excerpt policy.
    pub fn excerpt_source(&self, pane: &str) -> String {
        match self.excerpt_mask {
            Some(mask) => mask(pane),
            None => pane.to_string(),
        }
    }

    /// Fatal startup refusal reason for `pane`, if any.
    pub fn start_refusal(&self, pane: &str) -> Option<String> {
        match self.start_refusal {
            StartRefusalRule::None => None,
            StartRefusalRule::TailNeedles {
                needles,
                prefix,
                max_chars,
            } => {
                let masked = self.excerpt_source(pane);
                tail_non_empty_lines(&masked)
                    .into_iter()
                    .find(|line| {
                        let lower = line.to_lowercase();
                        needles.iter().any(|needle| lower.contains(needle))
                    })
                    .map(|line| {
                        let trimmed: String = line.trim().chars().take(max_chars).collect();
                        format!("{prefix}: {trimmed}")
                    })
            }
            StartRefusalRule::Classifier { detect, prefix } => {
                detect(pane).map(|line| format!("{prefix}: {line}"))
            }
        }
    }

    /// Resolve a run-time `dialog` using this spec's declarative table.
    pub fn dialog_answer(
        &self,
        engine: &str,
        dialog: &Dialog<'_>,
        workspace: &WorkspaceId,
    ) -> Answer {
        eval_dialog_rules(self.dialog_rules, engine, dialog, workspace)
    }

    /// Check hard-block tail rules or exit-only dialog choices.
    pub fn hard_block(&self, pane: &str, choices: &[String]) -> Option<&'static str> {
        eval_hard_block_rules(self.hard_block_rules, pane, choices)
    }
}

/// Static declarative specification of one [`AgentDriver`] (`#113` D1).
#[derive(Debug, Clone, Copy)]
pub struct DriverSpec {
    /// Wire identifier carried in events and commands.
    pub id: &'static str,
    /// Human-readable title.
    pub title: &'static str,
    /// Capability traits read by the Bridge and API.
    pub traits: DriverTraits,
    /// `major.minor` releases verified against Spawn's parser and screen rules.
    pub tested_versions: &'static [&'static str],
    /// Declarative screen, dialog, masking, and hard-block specification.
    pub screen: ScreenSpec,
}

/// Hook 3: native session ID binding and transcript/scan discovery (`#91` / `#113` D1).
#[derive(Debug, Clone, Copy)]
pub enum NativeSessionHook {
    /// The Bridge generates and passes the native session ID before launch.
    Assigned {
        /// Generate the native session ID for `execution`.
        new_session_id: fn(&ExecutionId) -> String,
        /// Resolve the transcript path for `(home, workspace, worker_id, native)`.
        transcript_path: fn(&EngineHome, &WorkspaceId, Option<&str>, &str) -> String,
    },
    /// The engine chooses its native session ID; Spawn discovers it from output or disk.
    Reported {
        /// Resolve the transcript path for `(home, workspace, worker_id, native)`.
        transcript_path: fn(&EngineHome, &WorkspaceId, Option<&str>, &str) -> String,
        /// Extract a reported native session ID from an output or JSONL line.
        reported_session: fn(&str) -> Option<String>,
        /// Resolve the conversation scan directory for `(home, worker_id)`.
        scan_dir: fn(&EngineHome, Option<&str>) -> Option<String>,
    },
}

impl NativeSessionHook {
    /// Binding mode implied by this hook variant.
    pub const fn binding(&self) -> SessionBinding {
        match self {
            Self::Assigned { .. } => SessionBinding::Assigned,
            Self::Reported { .. } => SessionBinding::Reported,
        }
    }

    /// Assigned native session ID for `execution`, or `None` when [`Self::Reported`].
    pub fn new_session_id(&self, execution: &ExecutionId) -> Option<String> {
        match self {
            Self::Assigned { new_session_id, .. } => Some(new_session_id(execution)),
            Self::Reported { .. } => None,
        }
    }

    /// Resolve transcript path for `(home, workspace, worker_id, native)`.
    pub fn transcript_path_for(
        &self,
        home: &EngineHome,
        workspace: &WorkspaceId,
        worker_id: Option<&str>,
        native: &str,
    ) -> String {
        match self {
            Self::Assigned {
                transcript_path, ..
            }
            | Self::Reported {
                transcript_path, ..
            } => transcript_path(home, workspace, worker_id, native),
        }
    }

    /// Extract a reported native session ID from `line`, or `None` when [`Self::Assigned`].
    pub fn reported_session(&self, line: &str) -> Option<String> {
        match self {
            Self::Assigned { .. } => None,
            Self::Reported {
                reported_session, ..
            } => reported_session(line),
        }
    }

    /// Conversation scan directory for `(home, worker_id)`, or `None` when [`Self::Assigned`].
    pub fn session_scan_dir_for(
        &self,
        home: &EngineHome,
        worker_id: Option<&str>,
    ) -> Option<String> {
        match self {
            Self::Assigned { .. } => None,
            Self::Reported { scan_dir, .. } => scan_dir(home, worker_id),
        }
    }
}

/// One engine, as the Bridge sees it (`#91` / `#113` D1 3-hook seam).
/// Implementations are pure: no IO, no clock.
pub trait AgentDriver: Send + Sync + std::fmt::Debug {
    /// Static metadata and declarative screen/dialog/hard-block spec.
    fn spec(&self) -> &'static DriverSpec;

    /// Wire name, as `execution.started` and commands carry it.
    fn id(&self) -> &'static str {
        self.spec().id
    }

    /// Hook 1: how to start the engine from `home` for `launch`.
    fn launch(&self, home: &EngineHome, launch: &EngineLaunch<'_>) -> LaunchPlan;

    /// Hook 2: what one transcript line says.
    fn read_line(&self, line: &str, cursor: &mut LineCursor) -> LineFacts;

    /// Hook 3: how native sessions are bound and located on disk.
    fn session_binding(&self) -> NativeSessionHook;
}

impl dyn AgentDriver + '_ {
    /// Human-readable engine title.
    pub fn title(&self) -> &'static str {
        self.spec().title
    }

    /// Engine capability traits.
    pub fn traits(&self) -> DriverTraits {
        self.spec().traits
    }

    /// Verified `major.minor` releases for this engine.
    pub fn tested_versions(&self) -> &'static [&'static str] {
        self.spec().tested_versions
    }

    /// Native session ID pre-assigned for `execution`, if [`SessionBinding::Assigned`].
    pub fn new_session_id(&self, execution: &ExecutionId) -> Option<String> {
        self.session_binding().new_session_id(execution)
    }

    /// Transcript path for `native` in `workspace`.
    pub fn transcript_path(
        &self,
        home: &EngineHome,
        workspace: &WorkspaceId,
        native: &str,
    ) -> String {
        self.transcript_path_for(home, workspace, None, native)
    }

    /// Transcript path for `native` and `worker_id` in `workspace`.
    pub fn transcript_path_for(
        &self,
        home: &EngineHome,
        workspace: &WorkspaceId,
        worker_id: Option<&str>,
        native: &str,
    ) -> String {
        self.session_binding()
            .transcript_path_for(home, workspace, worker_id, native)
    }

    /// Native session ID reported by `line`, if [`SessionBinding::Reported`].
    pub fn reported_session(&self, line: &str) -> Option<String> {
        self.session_binding().reported_session(line)
    }

    /// Base conversation scan directory below `home`, if supported.
    pub fn session_scan_dir(&self, home: &EngineHome) -> Option<String> {
        self.session_scan_dir_for(home, None)
    }

    /// Conversation scan directory for `worker_id`, falling back to `home`.
    pub fn session_scan_dir_for(
        &self,
        home: &EngineHome,
        worker_id: Option<&str>,
    ) -> Option<String> {
        self.session_binding().session_scan_dir_for(home, worker_id)
    }

    /// Classify the tail of `pane`.
    pub fn read_screen(&self, pane: &str) -> ScreenReading {
        self.spec().screen.read_screen(pane)
    }

    /// Keys that answer a startup trust dialog shown on `pane`, if any.
    pub fn startup_answer(&self, pane: &str) -> Option<&'static [&'static str]> {
        self.spec().screen.startup_answer(pane)
    }

    /// Pane masked per this driver's fail-closed excerpt policy.
    pub fn excerpt_source(&self, pane: &str) -> String {
        self.spec().screen.excerpt_source(pane)
    }

    /// Fatal startup refusal reason for `pane`, if any.
    pub fn start_refusal(&self, pane: &str) -> Option<String> {
        self.spec().screen.start_refusal(pane)
    }

    /// Resolve a run-time `dialog` via this driver's declarative table.
    pub fn dialog_answer(&self, dialog: &Dialog<'_>, workspace: &WorkspaceId) -> Answer {
        self.spec()
            .screen
            .dialog_answer(self.id(), dialog, workspace)
    }

    /// Declarative dialog resolution table entries for this engine.
    pub fn dialog_rules(&self) -> &'static [DialogRule] {
        self.spec().screen.dialog_rules
    }

    /// Declarative hard-block tail rules evaluated on panes from this engine.
    pub fn hard_block_rules(&self) -> &'static [HardBlockRule] {
        self.spec().screen.hard_block_rules
    }

    /// Check this engine's hard-block tail rules or exit-only dialog choices.
    pub fn hard_block(&self, pane: &str, choices: &[String]) -> Option<&'static str> {
        self.spec().screen.hard_block(pane, choices)
    }
}

/// Extract trailing non-empty ANSI-stripped lines from `pane` up to [`TAIL_LINES`].
pub(crate) fn tail_non_empty_lines(pane: &str) -> Vec<String> {
    let all: Vec<String> = pane
        .lines()
        .map(|line| strip_ansi(line).trim_end().to_string())
        .collect();
    let end = all
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |at| at + 1);
    let start = end.saturating_sub(TAIL_LINES);
    all[start..end]
        .iter()
        .filter(|line| !line.trim().is_empty())
        .cloned()
        .collect()
}

/// Parse a dotted semantic version (`1.5.17`, `0.171.0`) from CLI `--version` output.
#[must_use]
pub fn parse_cli_version(answer: &str) -> Option<String> {
    answer
        .split_whitespace()
        .map(|word| word.trim_start_matches('v'))
        .find(|word| {
            word.contains('.')
                && word
                    .split('.')
                    .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        })
        .map(str::to_string)
}

/// Whether `version`'s `major.minor` matches one of `tested`.
#[must_use]
pub fn version_matches_tested(version: &str, tested: &[&str]) -> bool {
    let mut parts = version.split('.');
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
        return false;
    };
    tested.contains(&format!("{major}.{minor}").as_str())
}

/// How a [`DialogRule`] matches `Dialog::rule`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleMatch {
    /// Matches any `dialog.rule` (`Some(_)` or `None`).
    Any,
    /// Matches only `dialog.rule == None`.
    None,
    /// Matches `dialog.rule == Some(id)`.
    Exact(&'static str),
}

/// Guard checked after matching a [`DialogRule`]'s engine/rule/kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogGuard {
    /// Always applies once matched.
    Always,
    /// Applies only when `dialog.body` shows the workspace folder and
    /// `dialog.cursor` stands on `"Yes, I trust this folder"`; otherwise
    /// escalates with `mismatch_reason`.
    WorkspaceTrustedFolder {
        /// Human escalation reason returned when folder/cursor checks fail.
        mismatch_reason: &'static str,
    },
}

/// Outcome returned when a [`DialogRule`] passes its guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogOutcome {
    /// Press these keys (terminal key names), in order, once.
    Keys(&'static [&'static str]),
    /// Escalate to a human with this reason.
    Escalate(&'static str),
}

/// Declarative screen/dialog resolution rule (`#113` D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialogRule {
    /// Engine identifier (`"agy"`, `"claude"`, `"codex"`, `"opencode"`).
    pub engine: &'static str,
    /// How `dialog.rule` is matched against the host manifest rule id.
    pub rule: RuleMatch,
    /// Optional prompt kind filter (`Some("trust")`, `Some("permission")`, etc.).
    pub kind: Option<&'static str>,
    /// Precondition guard evaluated after matching `engine`, `rule`, and `kind`.
    pub guard: DialogGuard,
    /// Resolution outcome applied when the guard succeeds.
    pub outcome: DialogOutcome,
}

/// Declarative hard-block tail text rule (`#113` D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardBlockRule {
    /// Lowercased substring matched against the trailing pane lines.
    pub needle: &'static str,
    /// Short human-facing reason reported when `needle` matches.
    pub reason: &'static str,
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
/// seen work. Every keyed row is one key: the Bridge presses a resolver's
/// key only while its dialog shows and nothing else went to the pane since,
/// so a second key of the same answer would find the first already pressed.
///
/// | engine | dialog | guard | answer | evidence |
/// |---|---|---|---|---|
/// | agy | `trust_prompt` | the folder shown is the workspace, cursor on "Yes, I trust this folder" | Enter | probed |
/// | agy | `permission_prompt` | — | escalate | manifest text only |
/// | agy | `question_prompt` | — | escalate | manifest text only |
/// | claude | trust | — | escalate (the Bridge's startup answer, `y` Enter) | production, at startup |
/// | claude | numbered permission | — | escalate | approvals fixtures only (hand-written) |
/// | claude | question form, MCP elicitation, dynamic workflow | — | escalate | not captured |
/// | codex | `trust_directory`, `startup_update`, `live_strong_blocker` | — | escalate | not verified |
/// | opencode | `permission_required` | — | escalate | not verified |
/// | any | no row | — | escalate | — |
/// Declarative dialog rules for Claude Code (`#113` D1).
pub const CLAUDE_DIALOG_RULES: &[DialogRule] = &[
    // The Bridge answers it before the first prompt (two keys, audited);
    // at run time the resolver leaves it to a person.
    DialogRule {
        engine: "claude",
        rule: RuleMatch::Any,
        kind: Some("trust"),
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(
            "Claude Code's trust dialog is the Bridge's startup answer: a person answers it later",
        ),
    },
    DialogRule {
        engine: "claude",
        rule: RuleMatch::None,
        kind: Some("permission"),
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
    DialogRule {
        engine: "claude",
        rule: RuleMatch::Exact("bash_permission_prompt"),
        kind: Some("permission"),
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
    DialogRule {
        engine: "claude",
        rule: RuleMatch::Exact("generic_permission_prompt"),
        kind: Some("permission"),
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
    DialogRule {
        engine: "claude",
        rule: RuleMatch::Exact("live_blocked_form"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
    DialogRule {
        engine: "claude",
        rule: RuleMatch::Exact("mcp_elicitation_prompt"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
    DialogRule {
        engine: "claude",
        rule: RuleMatch::Exact("dynamic_workflow_prompt"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
];

/// Declarative dialog rules for Antigravity CLI (`#113` D1).
pub const AGY_DIALOG_RULES: &[DialogRule] = &[
    DialogRule {
        engine: "agy",
        rule: RuleMatch::Exact("trust_prompt"),
        kind: None,
        guard: DialogGuard::WorkspaceTrustedFolder {
            mismatch_reason: "agy's trust dialog names another folder, or its cursor is not on trust",
        },
        outcome: DialogOutcome::Keys(&crate::screen::AGY_TRUST_KEYS),
    },
    DialogRule {
        engine: "agy",
        rule: RuleMatch::Exact("permission_prompt"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
    DialogRule {
        engine: "agy",
        rule: RuleMatch::Exact("question_prompt"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
];

/// Declarative resolver rules across all engines (`#113` D1; design section 6.3).
/// Evaluated in order by [`eval_dialog_rules`] when an engine has no registered driver.
pub const DIALOG_RULES: &[DialogRule] = &[
    AGY_DIALOG_RULES[0],
    CLAUDE_DIALOG_RULES[0],
    CLAUDE_DIALOG_RULES[1],
    CLAUDE_DIALOG_RULES[2],
    CLAUDE_DIALOG_RULES[3],
    AGY_DIALOG_RULES[1],
    AGY_DIALOG_RULES[2],
    CLAUDE_DIALOG_RULES[4],
    CLAUDE_DIALOG_RULES[5],
    CLAUDE_DIALOG_RULES[6],
    DialogRule {
        engine: "codex",
        rule: RuleMatch::Exact("trust_directory"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
    DialogRule {
        engine: "codex",
        rule: RuleMatch::Exact("startup_update"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
    DialogRule {
        engine: "codex",
        rule: RuleMatch::Exact("live_strong_blocker"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
    DialogRule {
        engine: "codex",
        rule: RuleMatch::Exact("permission_prompt"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
    DialogRule {
        engine: "codex",
        rule: RuleMatch::Exact("approval_prompt"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
    DialogRule {
        engine: "opencode",
        rule: RuleMatch::Exact("permission_required"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(UNCAPTURED),
    },
];

/// Evaluate a slice of declarative [`DialogRule`] rows for `(engine, dialog, workspace)`.
pub fn eval_dialog_rules(
    rules: &[DialogRule],
    engine: &str,
    dialog: &Dialog<'_>,
    workspace: &WorkspaceId,
) -> Answer {
    for row in rules {
        if row.engine != engine {
            continue;
        }
        let rule_matches = match row.rule {
            RuleMatch::Any => true,
            RuleMatch::None => dialog.rule.is_none(),
            RuleMatch::Exact(id) => dialog.rule == Some(id),
        };
        if !rule_matches || row.kind.is_some_and(|kind| dialog.kind != kind) {
            continue;
        }
        match row.guard {
            DialogGuard::Always => {}
            DialogGuard::WorkspaceTrustedFolder { mismatch_reason } => {
                let folder = workspace.path().trim_end_matches('/');
                let shown = dialog
                    .body
                    .iter()
                    .any(|line| line.trim().trim_end_matches('/') == folder);
                let trusting = dialog
                    .cursor
                    .is_some_and(|cursor| cursor.trim() == "Yes, I trust this folder");
                if !shown || !trusting {
                    return Answer::Escalate(mismatch_reason);
                }
            }
        }
        return match row.outcome {
            DialogOutcome::Keys(keys) => Answer::Keys(keys),
            DialogOutcome::Escalate(reason) => Answer::Escalate(reason),
        };
    }
    Answer::Escalate("no row of the resolver's table covers this dialog")
}

/// Look up `(engine, dialog)` via the registered driver's [`ScreenSpec`], else [`DIALOG_RULES`].
pub fn table_answer(engine: &str, dialog: &Dialog<'_>, workspace: &WorkspaceId) -> Answer {
    match driver(engine) {
        Some(driver) => driver
            .spec()
            .screen
            .dialog_answer(engine, dialog, workspace),
        None => eval_dialog_rules(DIALOG_RULES, engine, dialog, workspace),
    }
}

/// Screens no key may answer: the engine cannot go on without a person
/// (design section 6.3, "Hard blocks"). Lowercased text; every one is from
/// the engines' own strings and docs, none captured on a live pane yet.
pub const HARD_BLOCK_RULES: &[HardBlockRule] = &[
    HardBlockRule {
        needle: "please run /login",
        reason: "login required",
    },
    HardBlockRule {
        needle: "login required",
        reason: "login required",
    },
    HardBlockRule {
        needle: "not logged in",
        reason: "login required",
    },
    HardBlockRule {
        needle: "oauth token has expired",
        reason: "login expired",
    },
    HardBlockRule {
        needle: "authentication_error",
        reason: "login expired",
    },
    HardBlockRule {
        needle: "rate limit reached",
        reason: "rate limit",
    },
    HardBlockRule {
        needle: "rate_limit_error",
        reason: "rate limit",
    },
    HardBlockRule {
        needle: "usage limit reached",
        reason: "quota reached",
    },
    HardBlockRule {
        needle: "quota exceeded",
        reason: "quota reached",
    },
    HardBlockRule {
        needle: "resource_exhausted",
        reason: "quota reached",
    },
    HardBlockRule {
        needle: "update required",
        reason: "update required",
    },
    HardBlockRule {
        needle: "please update to continue",
        reason: "update required",
    },
];

/// Evaluate [`HARD_BLOCK_RULES`] (or exit-only dialog choices) on `pane`.
pub fn eval_hard_block_rules(
    rules: &[HardBlockRule],
    pane: &str,
    choices: &[String],
) -> Option<&'static str> {
    let lines: Vec<&str> = pane.lines().collect();
    let tail = lines[lines.len().saturating_sub(crate::screen::TAIL_LINES)..]
        .join("\n")
        .to_lowercase();
    if let Some(rule) = rules.iter().find(|rule| tail.contains(rule.needle)) {
        return Some(rule.reason);
    }
    let exits = |label: &String| {
        let lower = label.to_lowercase();
        ["exit", "quit"].iter().any(|word| {
            lower
                .split(|c: char| !c.is_alphanumeric())
                .any(|w| w == *word)
        })
    };
    (!choices.is_empty() && choices.iter().all(exits)).then_some("only exit choices")
}

/// Why the pane's tail is a hard block, if it is one: a login required or
/// expired, a quota or rate limit reached, an update the engine needs
/// before it runs, or a dialog whose only choices exit. Read from the last
/// [`crate::screen::TAIL_LINES`] lines only; the resolver escalates it at
/// once and never presses a key.
pub fn hard_block(pane: &str, choices: &[String]) -> Option<&'static str> {
    eval_hard_block_rules(HARD_BLOCK_RULES, pane, choices)
}

/// Evaluate hard-block rules using `engine`'s [`ScreenSpec`] when registered,
/// falling back fail-closed to [`hard_block`] for unknown or unset engines.
#[must_use]
pub fn hard_block_for(
    engine: Option<&str>,
    pane: &str,
    choices: &[String],
) -> Option<&'static str> {
    match engine.and_then(driver) {
        Some(driver) => driver.spec().screen.hard_block(pane, choices),
        None => hard_block(pane, choices),
    }
}

fn claude_transcript_for(
    home: &EngineHome,
    workspace: &WorkspaceId,
    _worker_id: Option<&str>,
    native: &str,
) -> String {
    claude_transcript_path(&home.sessions, workspace, native)
}

/// Declarative specification for [`ClaudeDriver`].
pub const CLAUDE_SPEC: DriverSpec = DriverSpec {
    id: crate::event::ENGINE,
    title: "Claude Code",
    traits: DriverTraits {
        session: SessionBinding::Assigned,
        turns: TurnSource::Transcript,
        guard: Guard::Settings,
    },
    tested_versions: &[],
    screen: ScreenSpec {
        read_screen: read_claude_screen,
        startup_dialog: Some(StartupDialogRule {
            matches: is_trust_dialog,
            keys: &TRUST_KEYS,
        }),
        excerpt_mask: None,
        start_refusal: StartRefusalRule::None,
        dialog_rules: CLAUDE_DIALOG_RULES,
        hard_block_rules: HARD_BLOCK_RULES,
    },
};

/// Claude Code, interactive in a terminal: everything it does is the
/// Bridge's existing Claude functions, unchanged.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeDriver;

impl AgentDriver for ClaudeDriver {
    fn spec(&self) -> &'static DriverSpec {
        &CLAUDE_SPEC
    }

    fn launch(&self, home: &EngineHome, launch: &EngineLaunch<'_>) -> LaunchPlan {
        let settings = claude_settings_in(launch.relay, launch.profile, launch.config_dir);
        LaunchPlan {
            program: home.program.clone(),
            arguments: claude_arguments(launch.session, &settings),
            environment: Vec::new(),
            settings,
        }
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

    fn session_binding(&self) -> NativeSessionHook {
        NativeSessionHook::Assigned {
            new_session_id: claude_session_id,
            transcript_path: claude_transcript_for,
        }
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
/// it off the host's credentials, so the sandbox does ([`Guard::Sandbox`]):
/// it runs inside the host's filesystem view, or not at all.
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

fn agy_brain_dir(home: &EngineHome, worker_id: Option<&str>) -> String {
    if let Some(worker_id) = worker_id
        && let Some(worker_brain) = home.worker_brain_dir(worker_id)
    {
        return worker_brain.display().to_string();
    }
    if let Some(shared) = home.shared_brain_dir() {
        return shared.display().to_string();
    }
    format!("{}/brain", home.sessions.trim_end_matches('/'))
}

#[allow(clippy::unnecessary_wraps)]
fn agy_scan_dir_for(home: &EngineHome, worker_id: Option<&str>) -> Option<String> {
    Some(agy_brain_dir(home, worker_id))
}

fn agy_transcript_for(
    home: &EngineHome,
    _workspace: &WorkspaceId,
    worker_id: Option<&str>,
    native: &str,
) -> String {
    let brain = agy_brain_dir(home, worker_id);
    format!(
        "{}/{native}/.system_generated/logs/transcript_full.jsonl",
        brain.trim_end_matches('/')
    )
}

fn agy_reported_session(_line: &str) -> Option<String> {
    None
}

/// Declarative specification for [`AgyDriver`].
pub const AGY_SPEC: DriverSpec = DriverSpec {
    id: "agy",
    title: "Antigravity CLI",
    traits: DriverTraits {
        session: SessionBinding::Reported,
        turns: TurnSource::Transcript,
        guard: Guard::Sandbox,
    },
    tested_versions: &AGY_TESTED,
    screen: ScreenSpec {
        read_screen: read_agy_screen,
        startup_dialog: Some(StartupDialogRule {
            matches: is_agy_trust_dialog,
            keys: &AGY_TRUST_KEYS,
        }),
        excerpt_mask: Some(drop_agy_account_lines),
        start_refusal: StartRefusalRule::Classifier {
            detect: agy_unknown_model,
            prefix: "agy does not know the model it was started on, and would run its own",
        },
        dialog_rules: AGY_DIALOG_RULES,
        hard_block_rules: HARD_BLOCK_RULES,
    },
};

impl AgentDriver for AgyDriver {
    fn spec(&self) -> &'static DriverSpec {
        &AGY_SPEC
    }

    /// The model is the launch's (the start's `model_label`), else the
    /// home's (the Bridge refuses a start with neither); the settings are
    /// the inline JSON the arguments carry.
    fn launch(&self, home: &EngineHome, launch: &EngineLaunch<'_>) -> LaunchPlan {
        let arguments = agy_arguments(
            launch.model.or(home.model.as_deref()).unwrap_or_default(),
            launch.cwd.unwrap_or_default(),
            launch.session.resumed(),
        );
        let settings = arguments.last().cloned().unwrap_or_default();
        LaunchPlan {
            program: home.program.clone(),
            arguments,
            environment: Vec::new(),
            settings,
        }
    }

    fn read_line(&self, line: &str, cursor: &mut LineCursor) -> LineFacts {
        cursor.lines = cursor.lines.saturating_add(1);
        LineFacts {
            events: agy_line(line, cursor),
            version: None,
        }
    }

    fn session_binding(&self) -> NativeSessionHook {
        NativeSessionHook::Reported {
            transcript_path: agy_transcript_for,
            reported_session: agy_reported_session,
            scan_dir: agy_scan_dir_for,
        }
    }
}

/// The `major.minor` versions of agy Spawn's readings were checked against
/// (1.5.14 probed, 1.5.17 seen on the VM). agy updates itself: another one
/// is reported (`agy_version_untested`, `GET /api/capabilities`) and refuses
/// nothing, since a screen Spawn cannot read reads `Unknown`, which types
/// nothing.
pub const AGY_TESTED: [&str; 1] = ["1.5"];

impl AgyDriver {
    /// The model labels an `agy models` listing names, sorted, once each.
    /// A pure reading of its output: each line's first word, past a list
    /// mark (`-`, `*`, `•`, `>`) and before a `(default)`-like note or a
    /// `:`, is a label when it is a model label
    /// ([`crate::command::is_model_label`]) holding a digit or a `-` (a
    /// header or a sentence holds neither in its first word). The status
    /// lines agy prints around the list (`Fetching available models...`,
    /// `Error: ...`) and a column header are no labels.
    pub fn known_models(listing: &str) -> Vec<String> {
        let mut known = std::collections::BTreeSet::new();
        for line in listing.lines() {
            let line = crate::screen::strip_ansi(line);
            let line = line
                .trim()
                .trim_start_matches(['-', '*', '•', '>', '│', '|'])
                .trim();
            let Some(first) = line.split_whitespace().next() else {
                continue;
            };
            let word = first.trim_end_matches([':', ',', '*']);
            let lower = line.to_lowercase();
            if lower.starts_with("error")
                || lower.starts_with("fetching")
                || lower.starts_with("available")
                || lower.starts_with("usage")
            {
                continue;
            }
            if crate::command::is_model_label(word)
                && word.chars().any(|c| c.is_ascii_digit() || c == '-')
                && word.chars().any(|c| c.is_ascii_alphabetic())
            {
                known.insert(word.to_string());
            }
        }
        known.into_iter().collect()
    }

    /// The version an `agy --version` answer names (`1.5.17`): its first
    /// word that is digits and dots with at least one dot, a leading `v`
    /// dropped.
    pub fn version(answer: &str) -> Option<String> {
        parse_cli_version(answer)
    }

    /// Whether `version`'s `major.minor` is one Spawn was checked against
    /// ([`AGY_TESTED`]).
    pub fn version_tested(version: &str) -> bool {
        version_matches_tested(version, &AGY_TESTED)
    }
}

/// The pane as an excerpt that leaves the host may quote it, for an
/// execution whose engine the log names `engine` (`None`: the lookup failed
/// or found nothing). **Fail-closed** (decision C2, N4): only a registered
/// driver whose [`ScreenSpec`] defines no excerpt mask keeps its pane byte for
/// byte; another registered engine's pane goes through its driver's
/// [`ScreenSpec::excerpt_source`], and an unregistered or unknown engine goes
/// through agy's mask ([`drop_agy_account_lines`]).
pub fn excerpt_for(engine: Option<&str>, pane: &str) -> String {
    match engine.and_then(driver) {
        Some(driver) => driver.spec().screen.excerpt_source(pane),
        None => drop_agy_account_lines(pane),
    }
}

pub use crate::driver_codex::{
    CODEX_DIALOG_RULES, CODEX_ENV_ALLOWLIST, CODEX_HARD_BLOCK_RULES, CODEX_HOME, CODEX_ID,
    CODEX_SPEC, CODEX_TESTED, CodexDriver, codex_arguments, codex_line, read_codex_screen,
};

static CLAUDE: ClaudeDriver = ClaudeDriver;

static AGY: AgyDriver = AgyDriver;

static CODEX: CodexDriver = CodexDriver;

static DRIVERS: [&dyn AgentDriver; 3] = [&CLAUDE, &AGY, &CODEX];

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
        Guard, LineCursor, NativeSession, SessionBinding, TurnSource, agy_arguments, driver,
        driver_or_default, drivers, excerpt_for, plain_session_id,
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
            data_dir: None,
            isolate_brain: false,
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
                    for session in [
                        NativeSession::Unnamed,
                        NativeSession::New("4f1c2b3a-0000-4000-8000-000000000001"),
                        NativeSession::Resume("4f1c2b3a-0000-4000-8000-000000000001"),
                    ] {
                        let plan = driver.launch(
                            &home(),
                            &EngineLaunch {
                                session,
                                relay: relay.as_ref(),
                                profile,
                                config_dir,
                                cwd: Some("/workspaces/agent_a"),
                                model: Some("ignored-by-claude"),
                            },
                        );
                        let settings = claude_settings_in(relay.as_ref(), profile, config_dir);
                        assert_eq!(plan.settings, settings);
                        assert_eq!(plan.arguments, claude_arguments(session, &settings));
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

    fn claude() -> &'static dyn AgentDriver {
        driver("claude").expect("claude registered")
    }

    fn agy() -> &'static dyn AgentDriver {
        driver("agy").expect("agy registered")
    }

    #[test]
    fn new_session_id_is_claude_session_id() {
        for command in [0, 1, 42, u64::MAX] {
            let execution = ExecutionId::for_command(CommandId(command));
            assert_eq!(
                claude().new_session_id(&execution),
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
                data_dir: None,
                isolate_brain: false,
            };
            assert_eq!(
                claude().transcript_path(&home, &workspace, "abc"),
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
        assert_eq!(claude().reported_session(lines[0]), None);
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
            assert_eq!(claude().read_screen(pane), read_claude_screen(pane));
            assert_eq!(
                claude().startup_answer(pane),
                is_trust_dialog(pane).then_some(&TRUST_KEYS[..])
            );
        }
        assert_eq!(claude().startup_answer(trust), Some(&TRUST_KEYS[..]));
    }

    /// Claude Code is the registry's first driver and the default every
    /// unknown name falls back on (`agy` is checked in
    /// `registry_holds_agy_beside_claude`; `codex` is checked in `codex_driver`).
    #[test]
    fn registry_holds_claude_alone() {
        assert_eq!(DEFAULT, ENGINE);
        assert_eq!(drivers().len(), 3);
        assert_eq!(drivers()[0].id(), ENGINE);
        assert_eq!(driver("claude").map(AgentDriver::id), Some("claude"));
        assert_eq!(driver("codex").map(AgentDriver::id), Some("codex"));
        assert!(driver("opencode").is_none());
        assert!(driver("").is_none());
        assert!(driver("Claude").is_none());
        assert_eq!(driver_or_default("opencode").id(), ENGINE);
        assert_eq!(driver_or_default("").id(), ENGINE);
        assert_eq!(driver_or_default(ENGINE).id(), ENGINE);
    }

    #[test]
    fn claude_traits_and_title() {
        assert_eq!(claude().title(), "Claude Code");
        assert_eq!(
            claude().traits(),
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
            data_dir: None,
            isolate_brain: false,
        }
    }

    #[test]
    fn registry_holds_agy_beside_claude() {
        assert_eq!(drivers()[1].id(), "agy");
        assert_eq!(driver("agy").map(AgentDriver::id), Some("agy"));
        assert_eq!(driver_or_default("agy").id(), "agy");
        assert!(driver("AGY").is_none());
        assert_eq!(agy().title(), "Antigravity CLI");
        assert_eq!(
            agy().traits(),
            DriverTraits {
                session: SessionBinding::Reported,
                turns: TurnSource::Transcript,
                guard: Guard::Sandbox,
            }
        );
        let execution = ExecutionId::for_command(CommandId(7));
        assert_eq!(agy().new_session_id(&execution), None);
        assert_eq!(agy().reported_session("{}"), None);
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
        for session in [
            NativeSession::Unnamed,
            NativeSession::Resume("conv-1"),
            NativeSession::New("conv-1"),
        ] {
            let plan = AgyDriver.launch(
                &agy_home(),
                &EngineLaunch {
                    session,
                    relay: None,
                    profile: ModelProfile::Cheap,
                    config_dir: Some("/x"),
                    cwd: Some("/workspaces/agent_a"),
                    model: None,
                },
            );
            assert_eq!(plan.program, "/opt/agy/bin/agy");
            let mut expected: Vec<String> = Vec::new();
            match session {
                NativeSession::Resume(id) => {
                    expected.extend(["--conversation".to_string(), id.to_string()]);
                }
                NativeSession::Unnamed | NativeSession::New(_) => {}
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

    /// A start's `model_label` reaches agy as `--model`, over the home's.
    #[test]
    fn agy_launch_passes_the_starts_model_label_over_the_homes() {
        let plan = AgyDriver.launch(
            &agy_home(),
            &EngineLaunch {
                session: NativeSession::Unnamed,
                relay: None,
                profile: ModelProfile::Default,
                config_dir: None,
                cwd: Some("/workspaces/agent_a"),
                model: Some("gemini-3.8-pro"),
            },
        );
        assert_eq!(plan.arguments[..2], ["--model", "gemini-3.8-pro"]);
    }

    #[test]
    fn agy_known_models_reads_a_listing_and_skips_status_lines() {
        let listing = "Fetching available models...\n\
                       Available models:\n\
                       \u{1b}[1m* gemini-3.8-flash-low (default)\u{1b}[0m\n\
                         - gemini-3.8-pro: the strongest\n\
                       claude-sonnet-4.5\n\
                       MODEL    DESCRIPTION\n\
                       Use one with --model\n\
                       \n\
                       gemini-3.8-pro\n";
        assert_eq!(
            AgyDriver::known_models(listing),
            [
                "claude-sonnet-4.5",
                "gemini-3.8-flash-low",
                "gemini-3.8-pro"
            ]
        );
        assert!(
            AgyDriver::known_models(
                "Fetching available models...\nError: Please sign in to view available models.\n"
            )
            .is_empty()
        );
        assert!(AgyDriver::known_models("").is_empty());
    }

    #[test]
    fn agy_version_is_read_and_only_tested_major_minors_are_tested() {
        assert_eq!(AgyDriver::version("1.5.17\n").as_deref(), Some("1.5.17"));
        assert_eq!(
            AgyDriver::version("agy version v1.6.0 (abc)").as_deref(),
            Some("1.6.0")
        );
        assert_eq!(AgyDriver::version("no version here"), None);
        assert!(AgyDriver::version_tested("1.5.14"));
        assert!(AgyDriver::version_tested("1.5.17"));
        assert!(!AgyDriver::version_tested("1.6.0"));
        assert!(!AgyDriver::version_tested("2"));
    }

    #[test]
    fn agy_refuses_a_start_on_its_unknown_model_line_and_claude_never_does() {
        let pane = "Warning: model \"x-1\" is not recognized as a known model\n>\n";
        let why = agy().start_refusal(pane).expect("refused");
        assert!(why.contains("is not recognized as a known model"), "{why}");
        assert_eq!(claude().start_refusal(pane), None);
        assert_eq!(agy().start_refusal(">\n"), None);
    }

    /// Fail-closed (N4): only a pane positively Claude Code's stays as it
    /// is; agy's, an unknown engine's and an unknown one's lose the
    /// account line.
    #[test]
    fn excerpt_for_masks_unless_the_engine_is_positively_claude() {
        let pane = "▀▀▀▀▀▀ someone@example.invalid (Pro)\nok\n";
        assert_eq!(excerpt_for(Some("claude"), pane), pane);
        for engine in [Some("agy"), Some("codex"), Some(""), None] {
            assert_eq!(excerpt_for(engine, pane), "ok\n", "{engine:?}");
        }
    }

    /// The full transcript, never the short one agy truncates.
    #[test]
    fn agy_transcript_is_the_full_one_below_its_home() {
        let workspace = WorkspaceId::new("3:/workspaces/agent_a").expect("workspace");
        assert_eq!(
            agy().transcript_path(&agy_home(), &workspace, "11111111-2222"),
            "/home/a/.gemini/antigravity-cli/brain/11111111-2222/.system_generated/logs/transcript_full.jsonl"
        );
        assert!(plain_session_id("11111111-2222-4333-8444-555555555555"));
        for bad in ["", "../x", "a/b", "a b", &"x".repeat(129)] {
            assert!(!plain_session_id(bad), "{bad:?}");
        }
    }

    /// agy's conversations are the directories of `<home>/brain`, which
    /// the Bridge scans when the host reports none; Claude Code's sessions
    /// are never scanned (the Bridge names them).
    #[test]
    fn agy_scans_its_brain_and_claude_scans_nothing() {
        assert_eq!(
            agy().session_scan_dir(&agy_home()).as_deref(),
            Some("/home/a/.gemini/antigravity-cli/brain")
        );
        let mut trailing = agy_home();
        trailing.sessions.push('/');
        assert_eq!(
            agy().session_scan_dir(&trailing).as_deref(),
            Some("/home/a/.gemini/antigravity-cli/brain")
        );
        assert_eq!(claude().session_scan_dir(&home()), None);
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
        assert_eq!(agy().startup_answer(trust), Some(&["Enter"][..]));
        assert_eq!(claude().startup_answer(trust), None);
        let rule = "──────────";
        let idle = format!("{rule}\n>\n{rule}\n? for shortcuts\n");
        assert_eq!(
            agy().read_screen(&idle).state,
            crate::screen::ScreenState::Ready
        );
        assert_eq!(agy().startup_answer(&idle), None);
        let banner = "▀▀▀▀▀▀ someone@example.invalid (Pro)\nok\n";
        assert_eq!(agy().excerpt_source(banner), "ok\n");
        assert_eq!(claude().excerpt_source(banner), banner);
    }

    // --- the resolver's table (design section 6.3): one test a row -------

    fn dialog<'a>(
        rule: Option<&'a str>,
        kind: &'a str,
        cursor: Option<&'a str>,
        choices: &'a [String],
        body: &'a [String],
    ) -> super::Dialog<'a> {
        super::Dialog {
            rule,
            kind,
            cursor,
            choices,
            body,
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn ws() -> WorkspaceId {
        WorkspaceId::new("agent_a:/workspaces/agent_a").expect("workspace")
    }

    #[test]
    fn row_agy_trust_prompt_probed_answers_enter_for_the_workspace_only() {
        use super::{Answer, table_answer};
        let choices = strings(&["Yes, I trust this folder", "No, exit"]);
        let body = strings(&["Accessing workspace:", "/workspaces/agent_a"]);
        let ours = dialog(
            Some("trust_prompt"),
            "trust",
            Some("Yes, I trust this folder"),
            &choices,
            &body,
        );
        assert_eq!(table_answer("agy", &ours, &ws()), Answer::Keys(&["Enter"]));
        assert_eq!(agy().dialog_answer(&ours, &ws()), Answer::Keys(&["Enter"]));
        let elsewhere = strings(&["Accessing workspace:", "/home/someone"]);
        let other = dialog(
            Some("trust_prompt"),
            "trust",
            Some("Yes, I trust this folder"),
            &choices,
            &elsewhere,
        );
        assert!(matches!(
            table_answer("agy", &other, &ws()),
            Answer::Escalate(_)
        ));
        let off_cursor = dialog(
            Some("trust_prompt"),
            "trust",
            Some("No, exit"),
            &choices,
            &body,
        );
        assert!(matches!(
            table_answer("agy", &off_cursor, &ws()),
            Answer::Escalate(_)
        ));
    }

    #[test]
    fn row_agy_permission_prompt_manifest_text_only_escalates() {
        use super::{Answer, table_answer};
        let choices = strings(&["Yes", "No"]);
        let body = strings(&["Run command?", "cargo test"]);
        let shown = dialog(
            Some("permission_prompt"),
            "permission",
            Some("Yes"),
            &choices,
            &body,
        );
        assert!(matches!(
            table_answer("agy", &shown, &ws()),
            Answer::Escalate(_)
        ));
    }

    #[test]
    fn row_agy_question_prompt_manifest_text_only_escalates() {
        use super::{Answer, table_answer};
        let choices = strings(&["A", "B"]);
        let shown = dialog(Some("question_prompt"), "choice", Some("A"), &choices, &[]);
        assert!(matches!(
            table_answer("agy", &shown, &ws()),
            Answer::Escalate(_)
        ));
    }

    #[test]
    fn row_claude_trust_dialog_is_the_bridges_startup_answer_and_escalates_at_run_time() {
        use super::{Answer, table_answer};
        let shown = dialog(None, "trust", None, &[], &[]);
        assert!(matches!(
            table_answer("claude", &shown, &ws()),
            Answer::Escalate(_)
        ));
        assert!(matches!(
            claude().dialog_answer(&shown, &ws()),
            Answer::Escalate(_)
        ));
        // The Bridge's own startup answer is unchanged.
        let trust = "Do you trust the files in this folder?\n\ny. Yes, I trust this folder\n\
                     n. No, exit\nEnter y/n:\nEnter to confirm · Esc to cancel\n";
        assert_eq!(claude().startup_answer(trust), Some(&TRUST_KEYS[..]));
    }

    #[test]
    fn row_claude_numbered_permission_not_captured_escalates() {
        use super::{Answer, table_answer};
        let yes = strings(&["Yes", "Yes, and don't ask again", "No"]);
        for rule in [
            None,
            Some("bash_permission_prompt"),
            Some("generic_permission_prompt"),
        ] {
            let shown = dialog(rule, "permission", Some("Yes"), &yes, &[]);
            assert!(
                matches!(table_answer("claude", &shown, &ws()), Answer::Escalate(_)),
                "{rule:?}"
            );
        }
    }

    /// Every row that answers with keys answers with one key.
    #[test]
    fn every_keyed_row_of_the_table_is_one_key() {
        use super::{Answer, table_answer};
        let choices = strings(&["Yes, I trust this folder", "No, exit", "Yes", "No"]);
        let body = strings(&["Accessing workspace:", "/workspaces/agent_a"]);
        let mut keyed = 0;
        for engine in ["agy", "claude", "codex", "opencode", "other"] {
            for rule in [
                None,
                Some("trust_prompt"),
                Some("permission_prompt"),
                Some("question_prompt"),
                Some("bash_permission_prompt"),
                Some("generic_permission_prompt"),
                Some("live_blocked_form"),
                Some("trust_directory"),
                Some("permission_required"),
            ] {
                for kind in ["trust", "permission", "choice", "yes_no", "confirm"] {
                    for cursor in [None, Some("Yes, I trust this folder"), Some("Yes")] {
                        let shown = dialog(rule, kind, cursor, &choices, &body);
                        if let Answer::Keys(keys) = table_answer(engine, &shown, &ws()) {
                            keyed += 1;
                            assert_eq!(keys.len(), 1, "{engine} {rule:?} {kind}: {keys:?}");
                        }
                    }
                }
            }
        }
        assert!(keyed > 0, "the agy trust row answers");
    }

    #[test]
    fn row_claude_question_forms_not_captured_escalate() {
        use super::{Answer, table_answer};
        for rule in [
            "live_blocked_form",
            "mcp_elicitation_prompt",
            "dynamic_workflow_prompt",
        ] {
            let shown = dialog(Some(rule), "choice", None, &[], &[]);
            assert!(matches!(
                table_answer("claude", &shown, &ws()),
                Answer::Escalate(_)
            ));
        }
    }

    #[test]
    fn row_codex_dialogs_not_verified_escalate() {
        use super::{Answer, table_answer};
        for rule in ["trust_directory", "startup_update", "live_strong_blocker"] {
            let shown = dialog(Some(rule), "confirm", None, &[], &[]);
            assert!(matches!(
                table_answer("codex", &shown, &ws()),
                Answer::Escalate(_)
            ));
        }
    }

    #[test]
    fn row_opencode_permission_required_not_verified_escalates() {
        use super::{Answer, table_answer};
        let shown = dialog(Some("permission_required"), "permission", None, &[], &[]);
        assert!(matches!(
            table_answer("opencode", &shown, &ws()),
            Answer::Escalate(_)
        ));
    }

    #[test]
    fn row_any_engine_with_no_row_escalates() {
        use super::{Answer, table_answer};
        let yes = strings(&["Yes", "No"]);
        for (engine, rule, kind) in [
            ("agy", None, "permission"),
            ("agy", Some("idle"), "confirm"),
            ("claude", Some("unknown_rule"), "permission"),
            ("codex", None, "permission"),
            ("someone", Some("trust_prompt"), "trust"),
        ] {
            let shown = dialog(rule, kind, Some("Yes"), &yes, &[]);
            assert!(
                matches!(table_answer(engine, &shown, &ws()), Answer::Escalate(_)),
                "{engine} {rule:?} {kind}"
            );
        }
    }

    // --- hard blocks: every text from the engines' strings and docs, none
    // captured on a live pane (marked `unverified` in the test's name) ----

    #[test]
    fn hard_block_login_required_texts_unverified() {
        for pane in [
            "Invalid API key · Please run /login",
            "Error: login required",
            "You are not logged in.",
        ] {
            assert_eq!(
                super::hard_block(pane, &[]),
                Some("login required"),
                "{pane}"
            );
        }
    }

    #[test]
    fn hard_block_login_expired_texts_unverified() {
        for pane in [
            "API Error: 401 OAuth token has expired",
            "{\"type\":\"authentication_error\"}",
        ] {
            assert_eq!(
                super::hard_block(pane, &[]),
                Some("login expired"),
                "{pane}"
            );
        }
    }

    #[test]
    fn hard_block_rate_limit_texts_unverified() {
        for pane in ["Rate limit reached for requests", "rate_limit_error"] {
            assert_eq!(super::hard_block(pane, &[]), Some("rate limit"), "{pane}");
        }
    }

    #[test]
    fn hard_block_quota_texts_unverified() {
        for pane in [
            "Claude usage limit reached. Your limit will reset at 5pm",
            "Quota exceeded for this model",
            "429 RESOURCE_EXHAUSTED",
        ] {
            assert_eq!(
                super::hard_block(pane, &[]),
                Some("quota reached"),
                "{pane}"
            );
        }
    }

    #[test]
    fn hard_block_update_required_texts_unverified() {
        for pane in [
            "Update required: version 1.2 is too old",
            "Please update to continue",
        ] {
            assert_eq!(
                super::hard_block(pane, &[]),
                Some("update required"),
                "{pane}"
            );
        }
    }

    #[test]
    fn hard_block_only_exit_choices_and_none_on_ordinary_dialogs() {
        assert_eq!(
            super::hard_block("Something failed", &strings(&["Exit", "Quit Spawn"])),
            Some("only exit choices")
        );
        assert_eq!(
            super::hard_block("Run command?", &strings(&["Yes", "No, exit"])),
            None
        );
        assert_eq!(super::hard_block("Run command?", &[]), None);
        // Only the tail counts: a limit far above it is history.
        let old = format!("Rate limit reached\n{}", "line\n".repeat(60));
        assert_eq!(super::hard_block(&old, &[]), None);
    }

    #[test]
    fn sanitize_worker_key_keeps_safe_chars_and_rejects_dot_segments() {
        assert_eq!(super::sanitize_worker_key("agent_1-a.b"), "agent_1-a.b");
        assert_eq!(super::sanitize_worker_key("../evil/id:9"), ".._evil_id_9");
        assert_eq!(super::sanitize_worker_key("."), "worker");
        assert_eq!(super::sanitize_worker_key(".."), "worker");
        assert_eq!(super::sanitize_worker_key(""), "worker");
    }

    #[test]
    fn engine_home_and_agy_driver_resolve_isolated_and_shared_brain_paths() {
        use std::path::PathBuf;

        let ws = WorkspaceId::new("local:/tmp/ws").unwrap();

        let plain = agy_home();
        assert_eq!(plain.worker_brain_dir("agent_1"), None);
        assert_eq!(plain.shared_brain_dir(), None);
        assert_eq!(
            plain.transcript_roots(None),
            vec![PathBuf::from("/home/a/.gemini/antigravity-cli/")]
        );
        assert_eq!(
            agy()
                .session_scan_dir_for(&plain, Some("agent_1"))
                .as_deref(),
            Some("/home/a/.gemini/antigravity-cli/brain")
        );
        assert_eq!(
            agy().transcript_path_for(&plain, &ws, Some("agent_1"), "conv-1"),
            "/home/a/.gemini/antigravity-cli/brain/conv-1/.system_generated/logs/transcript_full.jsonl"
        );

        let isolated = EngineHome {
            data_dir: Some("/data01/agy/".to_string()),
            isolate_brain: true,
            ..agy_home()
        };
        assert_eq!(
            isolated.worker_brain_dir("agent_1"),
            Some(PathBuf::from("/data01/agy/workers/agent_1/home/brain"))
        );
        assert_eq!(isolated.shared_brain_dir(), None);
        assert_eq!(
            isolated.transcript_roots(Some("agent_1")),
            vec![
                PathBuf::from("/home/a/.gemini/antigravity-cli/"),
                PathBuf::from("/data01/agy/workers/agent_1/home"),
            ]
        );
        assert_eq!(
            isolated.transcript_roots(None),
            vec![
                PathBuf::from("/home/a/.gemini/antigravity-cli/"),
                PathBuf::from("/data01/agy/workers"),
                PathBuf::from("/data01/agy/brain"),
            ]
        );
        assert_eq!(
            agy()
                .session_scan_dir_for(&isolated, Some("agent_1"))
                .as_deref(),
            Some("/data01/agy/workers/agent_1/home/brain")
        );
        assert_eq!(
            agy().transcript_path_for(&isolated, &ws, Some("agent_1"), "conv-1"),
            "/data01/agy/workers/agent_1/home/brain/conv-1/.system_generated/logs/transcript_full.jsonl"
        );
        assert_eq!(
            agy().transcript_path_for(&isolated, &ws, None, "conv-1"),
            "/home/a/.gemini/antigravity-cli/brain/conv-1/.system_generated/logs/transcript_full.jsonl"
        );

        let shared = EngineHome {
            data_dir: Some("/data01/agy".to_string()),
            isolate_brain: false,
            ..agy_home()
        };
        assert_eq!(shared.worker_brain_dir("agent_1"), None);
        assert_eq!(
            shared.shared_brain_dir(),
            Some(PathBuf::from("/data01/agy/brain"))
        );
        assert_eq!(
            shared.transcript_roots(Some("agent_1")),
            vec![
                PathBuf::from("/home/a/.gemini/antigravity-cli/"),
                PathBuf::from("/data01/agy/brain"),
            ]
        );
        assert_eq!(
            agy().session_scan_dir(&shared).as_deref(),
            Some("/data01/agy/brain")
        );
        assert_eq!(
            agy()
                .session_scan_dir_for(&shared, Some("agent_1"))
                .as_deref(),
            Some("/data01/agy/brain")
        );
        assert_eq!(
            agy().transcript_path_for(&shared, &ws, Some("agent_1"), "conv-1"),
            "/data01/agy/brain/conv-1/.system_generated/logs/transcript_full.jsonl"
        );
    }

    #[test]
    fn declarative_dialog_and_hard_block_tables_match_driver_defaults() {
        use super::{
            AGY_DIALOG_RULES, CLAUDE_DIALOG_RULES, CODEX_DIALOG_RULES, CODEX_HARD_BLOCK_RULES,
            DIALOG_RULES, DialogGuard, DialogOutcome, HARD_BLOCK_RULES, eval_dialog_rules,
            eval_hard_block_rules, hard_block, hard_block_for, table_answer,
        };
        let agy = agy();
        let claude = claude();
        let codex = driver("codex").expect("codex registered");
        assert_eq!(agy.dialog_rules(), AGY_DIALOG_RULES);
        assert_eq!(claude.dialog_rules(), CLAUDE_DIALOG_RULES);
        assert_eq!(codex.dialog_rules(), CODEX_DIALOG_RULES);
        assert_eq!(agy.hard_block_rules(), HARD_BLOCK_RULES);
        assert_eq!(claude.hard_block_rules(), HARD_BLOCK_RULES);
        assert_eq!(codex.hard_block_rules(), CODEX_HARD_BLOCK_RULES);
        assert!(AGY_DIALOG_RULES.iter().any(|row| row.engine == "agy"
            && matches!(row.guard, DialogGuard::WorkspaceTrustedFolder { .. })
            && matches!(row.outcome, DialogOutcome::Keys(_))));
        let choices = strings(&["Yes, I trust this folder", "No, exit"]);
        let body = strings(&["/workspaces/agent_a"]);
        let trust = dialog(
            Some("trust_prompt"),
            "trust",
            Some("Yes, I trust this folder"),
            &choices,
            &body,
        );
        assert_eq!(
            agy.dialog_answer(&trust, &ws()),
            eval_dialog_rules(DIALOG_RULES, "agy", &trust, &ws())
        );
        assert_eq!(
            table_answer("claude", &trust, &ws()),
            claude.dialog_answer(&trust, &ws())
        );
        assert_eq!(
            hard_block("Please run /login", &[]),
            eval_hard_block_rules(HARD_BLOCK_RULES, "Please run /login", &[])
        );
        assert_eq!(
            hard_block_for(Some("codex"), "insufficient_quota", &[]),
            Some("quota reached")
        );
        assert_eq!(
            hard_block_for(None, "Please run /login", &[]),
            Some("login required")
        );
    }
}
