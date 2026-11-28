//! The run-time resolver (issue #91, Herdr PR 3; design section 6.3): a
//! step of `spawn loop` that answers a dialog an engine shows at run time
//! from a fixed table, as `key` commands of its own principal
//! (`spawn-resolve`), which the Bridge presses as it presses a person's.
//!
//! - **Switch.** `SPAWN_AUTO_APPROVE = off | startup | all`, default
//!   `startup`: the step reads and submits nothing unless it is `all`.
//!   `startup` is today's behaviour, the Bridge's own answer to an engine's
//!   startup trust dialog (a `key` command of `spawn-resolve` since PR 3, so
//!   it is audited too). `off` is no automatic answer at all: the resolver
//!   submits nothing, and the Bridge does not answer the startup trust
//!   dialog either (`BridgeConfig::auto_trust`, set in `runtime.rs`). An
//!   unknown value is warned about and read as `startup`.
//! - **Table.** `spawn_core::driver::table_answer`, pure: only rows whose
//!   screen was seen working answer, each with one key (today agy's
//!   `trust_prompt` for its own workspace, probed). Every other row, and a
//!   hard block (`spawn_core::driver::hard_block`: a login, a quota, a needed
//!   update, exit-only choices), escalates: nothing is pressed, the item
//!   stays in `/api/approvals` for a person, and the loop says so once.
//! - **Guards, all of them, before a key is submitted:** two pane reads in a
//!   row give the same dialog id (the approvals page's, `serve/approvals.rs`);
//!   on a host with agent detection (Herdr), for an engine other than Claude
//!   Code, its reading is `blocked` by a named rule with the blocker visible;
//!   the driver's own reading of both panes shows the dialog (`Permission`,
//!   or its startup trust dialog for `trust_prompt`); the row's own guard
//!   holds; no command of the session is unfinished (a person's answer, or
//!   ours, still in flight); the breaker is closed. The Bridge checks again
//!   when it presses the key: only while the dialog still shows and nothing
//!   else went to the pane since the key was accepted.
//! - **Trigger.** The resolver reads the live panes itself, twice; it does
//!   not wait for the Bridge's `screen.observed permission`, which the
//!   Bridge records on its own pass (normally before the answer).
//! - **Breaker, folded from the log, never from memory.** Every count is
//!   read from the commands of `spawn-resolve` (`commands_by`), and the
//!   per-dialog count from the idempotency of `submit`: an answer is keyed
//!   `resolve:<execution>:<dialog id>:1:1`, and a key submitted again returns
//!   the command already there and writes nothing. So a restart re-reads the
//!   same counts.
//!   - one automatic answer per dialog id per execution, never a second; an
//!     answered dialog that still shows [`STILL_SHOWS_MS`] after its answer
//!     is escalated once ("still shows after its answer");
//!   - [`EXECUTION_ANSWERS`] answers in an execution within
//!     [`EXECUTION_WINDOW_MS`] open it for that execution;
//!   - [`FLEET_ANSWERS`] answers across the fleet within a minute open it
//!     for the host.
//!
//!   A new execution starts with none of its own; a person's answer moves
//!   the dialog on.

use std::collections::HashSet;

use serde_json::Value;
use spawn_core::command::{CommandAction, NamedKey, Submission};
use spawn_core::driver::{Answer, Dialog, driver_or_default, hard_block, table_answer};
use spawn_core::id::{
    AgentId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName, WorkspaceId,
};
use spawn_core::port::{AgentStatus, TerminalHost};
use spawn_core::screen::ScreenState;
use spawn_core::tui::{
    KeyChord, KeyChordSequence, RawPlainSnapshot, ScreenGuard, TerminalLiveState, TuiSnapshot,
};
use spawn_host::herdr_global::{GlobalErrorKind, HerdrGlobalClient, SocketScope};

use crate::serve::approvals::{Prompt, detect, fnv1a, normalized};

/// The resolver's principal: the Bridge's ([`spawn_core::bridge::RESOLVE_PRINCIPAL`]).
pub(crate) const PRINCIPAL: &str = spawn_core::bridge::RESOLVE_PRINCIPAL;

/// An answered dialog still showing this long after its answer is
/// escalated (two reads of the loop's pass have seen it); it is never
/// answered again.
pub(crate) const STILL_SHOWS_MS: i64 = 5_000;
/// Answers in one execution within [`EXECUTION_WINDOW_MS`] that open the
/// breaker for it.
pub(crate) const EXECUTION_ANSWERS: usize = 5;
/// The window [`EXECUTION_ANSWERS`] counts in: 10 minutes.
pub(crate) const EXECUTION_WINDOW_MS: i64 = 600_000;
/// Answers across the fleet within a minute that open it for the host.
pub(crate) const FLEET_ANSWERS: usize = 20;
/// Keys of one answer are submitted together: commands of one session
/// closer than this are one answer when counted.
const ONE_ANSWER_MS: i64 = 1_000;

/// `SPAWN_AUTO_APPROVE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Nothing automatic from the resolver.
    Off,
    /// The startup trust dialog only, which the Bridge answers (default).
    Startup,
    /// The run-time dialogs of the table too.
    All,
}

impl Mode {
    /// The mode `value` names: unset or empty is [`Mode::Startup`].
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim) {
            None | Some("" | "startup") => Ok(Self::Startup),
            Some("off") => Ok(Self::Off),
            Some("all") => Ok(Self::All),
            Some(other) => Err(format!(
                "SPAWN_AUTO_APPROVE={other:?} is none of off, startup, all: read as startup"
            )),
        }
    }

    /// The mode the environment names, and a warning for a value it cannot
    /// read (then [`Mode::Startup`]: inert).
    pub(crate) fn from_env() -> (Self, Option<String>) {
        match Self::parse(crate::config::env_value("SPAWN_AUTO_APPROVE").as_deref()) {
            Ok(mode) => (mode, None),
            Err(warning) => (Self::Startup, Some(warning)),
        }
    }

    /// Its wire name.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Startup => "startup",
            Self::All => "all",
        }
    }
}

/// One command of the resolver's, as the log has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answered {
    pub(crate) id: i64,
    pub(crate) session: String,
    pub(crate) created_at_ms: i64,
}

/// The resolver's commands, newest first: what every count is folded from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Ledger {
    pub(crate) answered: Vec<Answered>,
}

impl Ledger {
    /// From `commands_by`'s JSON.
    pub(crate) fn from_json(commands: &Value) -> Self {
        Self {
            answered: commands
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|command| {
                    Some(Answered {
                        id: command["id"].as_i64()?,
                        session: command["session_id"].as_str()?.to_string(),
                        created_at_ms: command["created_at_ms"].as_i64().unwrap_or(0),
                    })
                })
                .collect(),
        }
    }

    fn get(&self, id: i64) -> Option<&Answered> {
        self.answered.iter().find(|answered| answered.id == id)
    }

    /// Answers (keys of one answer counted once) of `session` (any when
    /// `None`) made at or after `since_ms`.
    pub(crate) fn answers(&self, session: Option<&str>, since_ms: i64) -> usize {
        let mut times: Vec<(&str, i64)> = self
            .answered
            .iter()
            .filter(|answered| session.is_none_or(|session| answered.session == session))
            .filter(|answered| answered.created_at_ms >= since_ms)
            .map(|answered| (answered.session.as_str(), answered.created_at_ms))
            .collect();
        times.sort_unstable();
        let mut count = 0;
        let mut last: Option<(&str, i64)> = None;
        for (session, at) in times {
            if last.is_none_or(|(before, then)| before != session || at - then >= ONE_ANSWER_MS) {
                count += 1;
            }
            last = Some((session, at));
        }
        count
    }

    /// Why the breaker is open for `session`'s execution started at
    /// `started_ms`, or for the whole host, at `now_ms`; `None` when closed.
    pub(crate) fn breaker(
        &self,
        session: &str,
        started_ms: i64,
        now_ms: i64,
    ) -> Option<&'static str> {
        if self.answers(None, now_ms - 60_000) >= FLEET_ANSWERS {
            return Some("20 automatic answers across the fleet within a minute");
        }
        let since = started_ms.max(now_ms - EXECUTION_WINDOW_MS);
        (self.answers(Some(session), since) >= EXECUTION_ANSWERS)
            .then_some("5 automatic answers in this execution within 10 minutes")
    }
}

/// What the resolver makes of one pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Press `keys` for the dialog `dialog`, optionally guarded by `expected_guard`.
    Answer {
        dialog: String,
        keys: KeyChordSequence,
        expected_guard: Option<ScreenGuard>,
    },
    /// A person decides `dialog`: why.
    Escalate { dialog: String, why: String },
    /// Nothing to do (no dialog, a guard does not hold): why.
    Nothing(&'static str),
}

/// The dialog id of a pane the approvals page reads no prompt on (agy's
/// trust dialog): its bottom lines, normalized and hashed like an item id.
fn bottom_id(session: &str, rule: &str, pane: &str) -> String {
    let lines: Vec<String> = pane
        .lines()
        .map(normalized)
        .filter(|line| !line.is_empty())
        .collect();
    let bottom = lines[lines.len().saturating_sub(8)..].join("\n");
    format!("{session}:{:016x}", fnv1a(&format!("{rule}\n{bottom}")))
}

/// A dialog's choices, the label its cursor stands on, and its body: from
/// the approvals page's prompt, structured snapshot choices, or from the
/// pane itself for a dialog it reads no prompt on (agy's trust dialog).
fn dialog_parts(
    prompt: Option<&Prompt>,
    snapshot: &TuiSnapshot,
    second: &str,
) -> (Vec<String>, Option<String>, Vec<String>) {
    let choices: Vec<String> = prompt.map_or_else(
        || {
            let trust: Vec<String> = second
                .lines()
                .map(|line| {
                    line.trim()
                        .trim_start_matches(['>', '❯'])
                        .trim()
                        .to_string()
                })
                .filter(|line| line.starts_with("Yes, I trust") || line.starts_with("No, exit"))
                .collect();
            if trust.is_empty() {
                snapshot
                    .choices()
                    .iter()
                    .map(|choice| choice.label.clone())
                    .collect()
            } else {
                trust
            }
        },
        |prompt| {
            prompt
                .choices
                .iter()
                .map(|choice| choice.label.clone())
                .collect()
        },
    );
    let cursor: Option<String> = prompt.map_or_else(
        || {
            snapshot
                .selected_choice()
                .map(|choice| choice.label.clone())
                .or_else(|| {
                    second.lines().find_map(|line| {
                        line.trim()
                            .strip_prefix('>')
                            .map(|rest| rest.trim().to_string())
                    })
                })
        },
        |prompt| {
            prompt
                .cursor
                .as_ref()
                .and_then(|id| {
                    prompt
                        .choices
                        .iter()
                        .find(|choice| &choice.id == id)
                        .map(|choice| choice.label.clone())
                })
                .or_else(|| {
                    snapshot
                        .selected_choice()
                        .map(|choice| choice.label.clone())
                })
        },
    );
    let body: Vec<String> = prompt.map_or_else(
        || second.lines().map(|line| line.trim().to_string()).collect(),
        |prompt| prompt.body.clone(),
    );
    (choices, cursor, body)
}

/// What one pane, read twice as structured snapshots (`first`, then `second`),
/// calls for: pure. Enforces matching [`SnapshotFidelity`] and matching
/// [`ScreenGuard`] CAS tokens when `selection_observable()` holds.
pub(crate) fn decide_snapshots(
    session: &str,
    engine: &str,
    workspace: &WorkspaceId,
    first: &TuiSnapshot,
    second: &TuiSnapshot,
) -> Decision {
    if first.selection_observable() != second.selection_observable() {
        return Decision::Nothing("snapshot fidelity changed between reads");
    }
    if first.selection_observable() && first.guard() != second.guard() {
        return Decision::Nothing("screen guard changed between reads");
    }
    let expected_guard = second.selection_observable().then(|| second.guard());
    let first_text = first.plain_text();
    let second_text = second.plain_text();
    let driver = driver_or_default(engine);
    let screen = driver.read_screen(&second_text);
    let herdr_rule = (driver.id() != "claude").then(|| second.rule()).flatten();
    let trust_rule = herdr_rule == Some("trust_prompt");
    if trust_rule {
        // An independent reading of the trust dialog, on both reads: the
        // host's rule alone is never enough.
        if driver.startup_answer(&first_text).is_none()
            || driver.startup_answer(&second_text).is_none()
        {
            return Decision::Nothing("the engine's own reading shows no trust dialog");
        }
    } else if screen.state != ScreenState::Permission {
        return Decision::Nothing("the engine's own reading shows no dialog");
    }
    let prompts: (Option<Prompt>, Option<Prompt>) = (detect(&first_text), detect(&second_text));
    let (dialog, prompt) = match prompts {
        (Some(one), Some(two)) if one.id(session) == two.id(session) => {
            (two.id(session), Some(two))
        }
        (None, None)
            if trust_rule
                && bottom_id(session, "trust_prompt", &first_text)
                    == bottom_id(session, "trust_prompt", &second_text) =>
        {
            (bottom_id(session, "trust_prompt", &second_text), None)
        }
        (None, None) => {
            return Decision::Escalate {
                dialog: bottom_id(session, "unread", &second_text),
                why: "a dialog Spawn reads no prompt on".to_string(),
            };
        }
        _ => return Decision::Nothing("two reads in a row show two dialogs"),
    };
    if driver.id() != "claude" {
        let blocked = second.agent_status() == AgentStatus::Blocked
            && second.rule().is_some()
            && second.blocker_visible();
        if !blocked {
            return Decision::Nothing("the host does not read the agent blocked by a named rule");
        }
    }
    let (choices, cursor, body) = dialog_parts(prompt.as_ref(), second, &second_text);
    if let Some(why) = hard_block(&second_text, &choices) {
        return Decision::Escalate {
            dialog,
            why: format!("a hard block ({why})"),
        };
    }
    let kind = if trust_rule {
        "trust"
    } else {
        prompt.as_ref().map_or("unread", |prompt| prompt.kind)
    };
    let shown = Dialog {
        rule: herdr_rule,
        kind,
        cursor: cursor.as_deref(),
        choices: &choices,
        body: &body,
    };
    match table_answer(driver.id(), &shown, workspace) {
        Answer::Keys(keys) => match KeyChordSequence::from_legacy_keys(keys) {
            Ok(keys) => Decision::Answer {
                dialog,
                keys,
                expected_guard,
            },
            Err(_) => Decision::Escalate {
                dialog,
                why: "an answer with an unsupported VT key chord".to_string(),
            },
        },
        Answer::Escalate(why) => Decision::Escalate {
            dialog,
            why: why.to_string(),
        },
    }
}

/// One live execution, as the resolver sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Live {
    pub(crate) session: String,
    pub(crate) agent: String,
    pub(crate) execution: String,
    pub(crate) engine: String,
    pub(crate) started_ms: i64,
    /// A command of its session is unfinished (a person's key, ours).
    pub(crate) busy: bool,
}

/// Validated answer submission payload paired with optional CAS guard.
struct AnswerPlan<'a> {
    dialog: &'a str,
    keys: &'a KeyChordSequence,
    named: &'a [NamedKey],
    expected_guard: Option<ScreenGuard>,
}

/// What the step reads and writes: the store and its terminal host /
/// `HerdrGlobalClient` in the loop, a fake in the tests.
pub(crate) trait World {
    /// The store's clock.
    fn now_ms(&self) -> Result<i64, String>;
    /// The resolver's commands (`commands_by`).
    fn ledger(&self) -> Result<Ledger, String>;
    /// The live executions.
    fn live(&self) -> Result<Vec<Live>, String>;
    /// A structured [`TuiSnapshot`] of the live execution's terminal.
    fn capture_snapshot(&self, live: &Live) -> Result<TuiSnapshot, String>;
    /// Pre-submit optimistic CAS guard check against the live screen.
    fn verify_guard(&self, _live: &Live, _expected_guard: ScreenGuard) -> Result<bool, String> {
        Ok(true)
    }
    /// The pause between the two reads.
    fn pause(&self);
    /// The workspace the execution runs in.
    fn workspace(&self, live: &Live) -> Result<WorkspaceId, String>;
    /// Submit a key command; its id (the first one for a key used before).
    fn submit(&self, live: &Live, key: &str, named: NamedKey) -> Result<i64, String>;
}

/// The resolver: its mode, and the escalations already said (so each is
/// said once; no decision rests on this memory).
#[derive(Debug)]
pub(crate) struct Resolver {
    mode: Mode,
    said: HashSet<String>,
}

impl Resolver {
    pub(crate) fn new(mode: Mode) -> Self {
        Self {
            mode,
            said: HashSet::new(),
        }
    }

    /// Whether the step does anything at all: only under `all`.
    pub(crate) fn runs(&self) -> bool {
        self.mode == Mode::All
    }

    fn say(&mut self, lines: &mut Vec<String>, subject: &str, line: String) {
        if self.said.insert(subject.to_string()) {
            lines.push(line);
        }
    }

    /// One pass: every live pane read, every answer the table and the
    /// guards allow submitted. Under any mode but `all`, nothing is read and
    /// nothing is submitted.
    pub(crate) fn step(&mut self, world: &dyn World) -> Result<Vec<String>, String> {
        let mut lines = Vec::new();
        if !self.runs() {
            return Ok(lines);
        }
        let now = world.now_ms()?;
        let mut ledger = world.ledger()?;
        for live in world.live()? {
            if live.busy {
                continue;
            }
            let driver = driver_or_default(&live.engine);
            let Ok(first) = world.capture_snapshot(&live) else {
                continue;
            };
            let trust = first.rule() == Some("trust_prompt");
            if driver.read_screen(&first.plain_text()).state != ScreenState::Permission && !trust {
                continue;
            }
            world.pause();
            let Ok(second) = world.capture_snapshot(&live) else {
                continue;
            };
            let workspace = world.workspace(&live)?;
            match decide_snapshots(&live.session, &live.engine, &workspace, &first, &second) {
                Decision::Nothing(_) => {}
                Decision::Escalate { dialog, why } => {
                    let line = format!("resolve: {} escalated {dialog}: {why}", live.session);
                    self.say(&mut lines, &format!("{}:{dialog}", live.execution), line);
                }
                Decision::Answer {
                    dialog,
                    keys,
                    expected_guard,
                } => {
                    let Some(named) = keys
                        .as_slice()
                        .iter()
                        .map(KeyChord::to_named_key)
                        .collect::<Option<Vec<_>>>()
                    else {
                        let line = format!(
                            "resolve: {} escalated {dialog}: an answer with a VT key chord outside the audited key command set",
                            live.session
                        );
                        self.say(&mut lines, &format!("{}:{dialog}", live.execution), line);
                        continue;
                    };
                    if let Some(why) = ledger.breaker(&live.session, live.started_ms, now) {
                        let line = format!(
                            "resolve: {} breaker open, escalated {dialog}: {why}",
                            live.session
                        );
                        self.say(&mut lines, &format!("{}:open", live.execution), line);
                        continue;
                    }
                    let plan = AnswerPlan {
                        dialog: &dialog,
                        keys: &keys,
                        named: &named,
                        expected_guard,
                    };
                    if let Some(line) = self.answer(world, &mut ledger, &live, &plan, now)? {
                        lines.push(line);
                    }
                }
            }
        }
        Ok(lines)
    }

    /// Submit the one answer to `dialog` this execution may get, as the
    /// log allows ([`Ledger`], `submit`'s idempotency, and optional
    /// [`ScreenGuard`] CAS): the line to log, if any. Answered already:
    /// nothing, and once it still shows [`STILL_SHOWS_MS`] after, one escalation.
    fn answer(
        &mut self,
        world: &dyn World,
        ledger: &mut Ledger,
        live: &Live,
        plan: &AnswerPlan<'_>,
        now: i64,
    ) -> Result<Option<String>, String> {
        if let Some(want) = plan.expected_guard
            && !world.verify_guard(live, want)?
        {
            return Ok(None);
        }
        let known: HashSet<i64> = ledger.answered.iter().map(|answered| answered.id).collect();
        let key = |k: usize| format!("resolve:{}:{}:1:{k}", live.execution, plan.dialog);
        let first = world.submit(live, &key(1), plan.named[0])?;
        if !known.contains(&first) {
            for (at, named) in plan.named.iter().enumerate().skip(1) {
                world.submit(live, &key(at + 1), *named)?;
            }
            ledger.answered.insert(
                0,
                Answered {
                    id: first,
                    session: live.session.clone(),
                    created_at_ms: now,
                },
            );
            let pressed: Vec<String> = plan
                .keys
                .as_slice()
                .iter()
                .map(KeyChord::to_herdr_spec)
                .collect();
            return Ok(Some(format!(
                "resolve: {} answered {}: {}",
                live.session,
                plan.dialog,
                pressed.join(" ")
            )));
        }
        let age = now
            - ledger
                .get(first)
                .map_or(now, |answered| answered.created_at_ms);
        if age < STILL_SHOWS_MS {
            return Ok(None);
        }
        let message = format!(
            "resolve: {} escalated {}: it still shows after its answer ({} s); it is \
             never answered automatically again",
            live.session,
            plan.dialog,
            age / 1000
        );
        Ok(self
            .said
            .insert(format!("{}:{}:shows", live.execution, plan.dialog))
            .then_some(message))
    }
}

/// The workspace an agent's executions run in (as the approvals page names
/// it).
fn workspace_of(agent: &str) -> Result<WorkspaceId, String> {
    let root = crate::env_or("SPAWN_WORKSPACE_ROOT", "/tmp");
    WorkspaceId::new(format!("{agent}:{root}/{agent}")).map_err(|error| error.to_string())
}

/// What `/api/approvals` shows of the resolver on an item of `session`:
/// `{state, answered, breaker}`. `state` is the switch, `answered` its
/// answers in the session within the execution window, `breaker` `open`
/// or `closed` (`off` unless the switch is `all`).
pub(crate) fn auto_for(mode: Mode, ledger: &Ledger, session: &str, now_ms: i64) -> Value {
    let breaker = if mode != Mode::All {
        "off"
    } else if ledger.breaker(session, 0, now_ms).is_some() {
        "open"
    } else {
        "closed"
    };
    serde_json::json!({
        "state": mode.name(),
        "answered": ledger.answers(Some(session), now_ms - EXECUTION_WINDOW_MS),
        "breaker": breaker,
    })
}

/// The loop's world: its store and its terminal host (backed by global Herdr
/// snapshots & guard verification when available).
pub(crate) struct Fleet<'a> {
    pub(crate) store: &'a spawn_postgres::PostgresStore,
    pub(crate) host: &'a spawn_host::LocalTerminalHost,
}

impl Fleet<'_> {
    fn resolve_herdr(live: &Live) -> Result<Option<(HerdrGlobalClient, String)>, String> {
        let execution = ExecutionId::new(&live.execution).map_err(|error| error.to_string())?;
        let terminal = TerminalName::for_execution(&execution);
        let scope = SocketScope::resolve(None, &crate::config::herdr_discovery_env())
            .map_err(|error| error.to_string())?;
        HerdrGlobalClient::resolve_spawn_terminal(&scope, terminal.as_str())
            .map_err(|error| error.to_string())
    }
}

impl World for Fleet<'_> {
    fn now_ms(&self) -> Result<i64, String> {
        self.store.now_ms().map_err(|error| error.to_string())
    }

    fn ledger(&self) -> Result<Ledger, String> {
        self.store
            .commands_by(PRINCIPAL, 500)
            .map(|commands| Ledger::from_json(&commands))
            .map_err(|error| error.to_string())
    }

    fn live(&self) -> Result<Vec<Live>, String> {
        let board = self
            .store
            .fleet_board()
            .map_err(|error| error.to_string())?;
        let mut out = Vec::new();
        for row in board.as_array().into_iter().flatten() {
            let (Some(session), Some(agent), Some(execution)) = (
                row["session_id"].as_str(),
                row["agent_id"].as_str(),
                row["execution_id"].as_str(),
            ) else {
                continue;
            };
            if row["live"] != true {
                continue;
            }
            let id = SessionId::new(session).map_err(|error| error.to_string())?;
            let latest = self
                .store
                .executions(&id)
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|row| row.execution_id.as_str() == execution);
            out.push(Live {
                session: session.to_string(),
                agent: agent.to_string(),
                execution: execution.to_string(),
                engine: latest
                    .as_ref()
                    .and_then(|row| row.engine.clone())
                    .unwrap_or_else(|| spawn_core::driver::DEFAULT.to_string()),
                started_ms: latest.as_ref().map_or(0, |row| row.started_at.0),
                busy: row["open_command"] == true,
            });
        }
        Ok(out)
    }

    fn capture_snapshot(&self, live: &Live) -> Result<TuiSnapshot, String> {
        if let Some((client, addr)) = Self::resolve_herdr(live)? {
            return client.snapshot(&addr).map_err(|error| error.to_string());
        }
        let execution = ExecutionId::new(&live.execution).map_err(|error| error.to_string())?;
        let session = SessionId::new(&live.session).map_err(|error| error.to_string())?;
        let raw_text = crate::serve::approvals::capture(
            self.host,
            self.store,
            &session,
            &execution,
            &live.agent,
        )?;
        let terminal = TerminalName::for_execution(&execution);
        let reading = workspace_of(&live.agent)
            .ok()
            .and_then(|ws| self.host.agent(&terminal, &ws).ok().flatten());
        Ok(TuiSnapshot::from_plain_text(RawPlainSnapshot {
            address: terminal.as_str(),
            cols: 0,
            rows: 0,
            live_state: TerminalLiveState::Live { pid: None },
            how: "screen",
            complete: true,
            why: None,
            title: None,
            raw_text: &raw_text,
            agent: reading.as_ref().and_then(|r| r.agent.as_deref()),
            agent_status: reading.as_ref().map_or(AgentStatus::Unknown, |r| r.status),
            rule: reading.as_ref().and_then(|r| r.rule.as_deref()),
            blocker_visible: reading.as_ref().is_some_and(|r| r.blocker_visible),
        }))
    }

    fn verify_guard(&self, live: &Live, expected_guard: ScreenGuard) -> Result<bool, String> {
        let Some((client, addr)) = Self::resolve_herdr(live)? else {
            return Ok(false);
        };
        match client.verify_guard(&addr, expected_guard) {
            Ok(()) => Ok(true),
            Err(err)
                if matches!(
                    err.kind,
                    GlobalErrorKind::GuardMismatch
                        | GlobalErrorKind::UnobservableSelection
                        | GlobalErrorKind::NotFound
                ) =>
            {
                Ok(false)
            }
            Err(err) => Err(err.to_string()),
        }
    }

    fn pause(&self) {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    fn workspace(&self, live: &Live) -> Result<WorkspaceId, String> {
        workspace_of(&live.agent)
    }

    fn submit(&self, live: &Live, key: &str, named: NamedKey) -> Result<i64, String> {
        use spawn_core::port::CommandQueue;
        let submission = Submission {
            agent_id: AgentId::new(&live.agent).map_err(|error| error.to_string())?,
            session_id: SessionId::new(&live.session).map_err(|error| error.to_string())?,
            idempotency_key: IdempotencyKey::new(key).map_err(|error| error.to_string())?,
            action: CommandAction::Key { key: named },
        };
        let principal = Principal::new(PRINCIPAL).map_err(|error| error.to_string())?;
        self.store
            .submit(&principal, &submission)
            .map(|id| i64::try_from(id.0).unwrap_or(i64::MAX))
            .map_err(|error| format!("{key}: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Decision, Ledger, Live, Mode, PRINCIPAL, Resolver, STILL_SHOWS_MS, World, auto_for,
        decide_snapshots,
    };
    use serde_json::json;
    use spawn_core::command::NamedKey;
    use spawn_core::id::WorkspaceId;
    use spawn_core::port::{AgentReading, AgentStatus};
    use spawn_core::tui::{
        KeyChord, KeyChordSequence, RawPlainSnapshot, RawStyledSnapshot, SnapshotObservation,
        TerminalLiveState, TuiSnapshot,
    };
    use std::cell::{Cell, RefCell};

    fn plain_snapshot(pane: &str, reading: Option<&AgentReading>) -> TuiSnapshot {
        TuiSnapshot::from_plain_text(RawPlainSnapshot {
            address: "t1",
            cols: 120,
            rows: 40,
            live_state: TerminalLiveState::Live { pid: Some(101) },
            how: "screen",
            complete: true,
            why: None,
            title: None,
            raw_text: pane,
            agent: reading.and_then(|r| r.agent.as_deref()),
            agent_status: reading.map_or(AgentStatus::Unknown, |r| r.status),
            rule: reading.and_then(|r| r.rule.as_deref()),
            blocker_visible: reading.is_some_and(|r| r.blocker_visible),
        })
    }

    fn decide(
        session: &str,
        engine: &str,
        workspace: &WorkspaceId,
        first: &str,
        second: &str,
        reading: Option<&AgentReading>,
    ) -> Decision {
        let first_snap = plain_snapshot(first, reading);
        let second_snap = plain_snapshot(second, reading);
        decide_snapshots(session, engine, workspace, &first_snap, &second_snap)
    }

    /// Thirty seconds: once a second answer was allowed from then (Rev 0);
    /// never now.
    const AGAIN_AFTER_MS: i64 = 30_000;

    /// agy 1.5.14's trust dialog (captured), for the workspace below.
    const TRUST: &str = "Accessing workspace:\n\n/tmp/ws/agent_a\n\n\
                         Do you trust the contents of this project?\n\n\
                         Antigravity CLI requires permission to read, edit, and execute files here.\n\n\
                         > Yes, I trust this folder\n  No, exit\n\n  ↑/↓ Navigate · enter Confirm\n";
    const RULE: &str = "──────────────────────────────";

    fn agy_idle() -> String {
        format!("{RULE}\n>\n{RULE}\n? for shortcuts\n")
    }

    /// Claude Code's numbered permission dialog at its prompt.
    const PERMISSION: &str = "Bash command\n  cargo test\nDo you want to proceed?\n\
                              ❯ 1. Yes\n  2. Yes, and don't ask again\n  3. No\n";

    fn workspace() -> WorkspaceId {
        WorkspaceId::new("agent_a:/tmp/ws/agent_a").expect("workspace")
    }

    fn blocked(rule: &str) -> AgentReading {
        AgentReading {
            agent: Some("agy".to_string()),
            status: AgentStatus::Blocked,
            rule: Some(rule.to_string()),
            blocker_visible: true,
            session: None,
            seq: 1,
            manifest: Some("2026.10.05.1".to_string()),
        }
    }

    fn live(session: &str, busy: bool) -> Live {
        Live {
            session: session.to_string(),
            agent: "agent_a".to_string(),
            execution: format!("exec_{session}"),
            engine: "agy".to_string(),
            started_ms: 0,
            busy,
        }
    }

    /// A world in memory: one agy pane (or structured [`TuiSnapshot`]) and Herdr's
    /// reading of it, a clock, a command table with `submit`'s idempotency, and every read counted.
    struct Fake {
        now: Cell<i64>,
        lives: Vec<Live>,
        pane: RefCell<String>,
        styled_sequence: RefCell<Vec<TuiSnapshot>>,
        guard_ok: Cell<bool>,
        reading: Option<AgentReading>,
        commands: RefCell<Vec<(String, String, i64, i64)>>,
        reads: Cell<usize>,
    }

    impl Fake {
        fn new(lives: Vec<Live>, pane: &str) -> Self {
            Self {
                now: Cell::new(1_000_000),
                lives,
                pane: RefCell::new(pane.to_string()),
                styled_sequence: RefCell::new(Vec::new()),
                guard_ok: Cell::new(true),
                reading: Some(blocked("trust_prompt")),
                commands: RefCell::new(Vec::new()),
                reads: Cell::new(0),
            }
        }

        fn keys(&self) -> Vec<String> {
            self.commands
                .borrow()
                .iter()
                .map(|(key, ..)| key.clone())
                .collect()
        }

        fn read(&self) {
            self.reads.set(self.reads.get() + 1);
        }
    }

    impl World for Fake {
        fn now_ms(&self) -> Result<i64, String> {
            self.read();
            Ok(self.now.get())
        }
        fn ledger(&self) -> Result<Ledger, String> {
            self.read();
            let commands: Vec<serde_json::Value> = self
                .commands
                .borrow()
                .iter()
                .rev()
                .map(|(_, session, id, at)| {
                    json!({"id": id, "session_id": session, "created_at_ms": at})
                })
                .collect();
            Ok(Ledger::from_json(&json!(commands)))
        }
        fn live(&self) -> Result<Vec<Live>, String> {
            self.read();
            Ok(self.lives.clone())
        }
        fn capture_snapshot(&self, _live: &Live) -> Result<TuiSnapshot, String> {
            self.read();
            let mut seq = self.styled_sequence.borrow_mut();
            if !seq.is_empty() {
                return Ok(seq.remove(0));
            }
            Ok(plain_snapshot(&self.pane.borrow(), self.reading.as_ref()))
        }
        fn verify_guard(
            &self,
            _live: &Live,
            _expected_guard: spawn_core::tui::ScreenGuard,
        ) -> Result<bool, String> {
            Ok(self.guard_ok.get())
        }
        fn pause(&self) {}
        fn workspace(&self, _live: &Live) -> Result<WorkspaceId, String> {
            Ok(workspace())
        }
        fn submit(&self, live: &Live, key: &str, _named: NamedKey) -> Result<i64, String> {
            let mut commands = self.commands.borrow_mut();
            if let Some((_, _, id, _)) = commands.iter().find(|(known, ..)| known == key) {
                return Ok(*id);
            }
            let id = i64::try_from(commands.len()).expect("small") + 1;
            commands.push((key.to_string(), live.session.clone(), id, self.now.get()));
            Ok(id)
        }
    }

    #[test]
    fn the_switch_defaults_to_startup_and_reads_three_words() {
        assert_eq!(Mode::parse(None), Ok(Mode::Startup));
        assert_eq!(Mode::parse(Some("")), Ok(Mode::Startup));
        assert_eq!(Mode::parse(Some("startup")), Ok(Mode::Startup));
        assert_eq!(Mode::parse(Some("off")), Ok(Mode::Off));
        assert_eq!(Mode::parse(Some(" all ")), Ok(Mode::All));
        assert!(Mode::parse(Some("yes")).is_err());
        assert_eq!(PRINCIPAL, "spawn-resolve");
    }

    /// Landing PR 3 changes nothing: with the switch unset (or `off`, or
    /// `startup`) the step reads nothing and submits nothing, even with a
    /// dialog the table would answer on every pane.
    #[test]
    fn with_the_switch_unset_the_resolver_submits_nothing() {
        for value in [None, Some("off"), Some("startup")] {
            let world = Fake::new(vec![live("session_a", false)], TRUST);
            let mut resolver = Resolver::new(Mode::parse(value).expect("mode"));
            assert!(!resolver.runs());
            assert!(resolver.step(&world).expect("step").is_empty());
            assert!(world.keys().is_empty(), "{value:?}");
            assert_eq!(world.reads.get(), 0, "nothing read either: {value:?}");
        }
    }

    #[test]
    fn under_all_a_table_row_is_answered_once_keyed_by_execution_and_dialog() {
        let world = Fake::new(vec![live("session_a", false)], TRUST);
        let mut resolver = Resolver::new(Mode::All);
        let lines = resolver.step(&world).expect("step");
        let keys = world.keys();
        assert_eq!(keys.len(), 1, "{keys:?}");
        assert!(
            keys[0].starts_with("resolve:exec_session_a:session_a:"),
            "{keys:?}"
        );
        assert!(keys[0].ends_with(":1:1"), "{keys:?}");
        assert!(lines[0].contains("answered"), "{lines:?}");
        // The next pass, moments later, with the dialog still there: no
        // second answer (the log says it was answered).
        world.now.set(world.now.get() + 1_000);
        resolver.step(&world).expect("step");
        assert_eq!(world.keys().len(), 1);
    }

    /// The breaker is folded from the log: a new resolver (a restarted
    /// loop) over the same command table reads the same answer. One answer
    /// per dialog per execution, never a second: still showing after it,
    /// the dialog is escalated once and never pressed again.
    #[test]
    fn the_breaker_counts_from_the_log_and_survives_a_restart() {
        let world = Fake::new(vec![live("session_a", false)], TRUST);
        Resolver::new(Mode::All).step(&world).expect("step");
        world.now.set(world.now.get() + STILL_SHOWS_MS);
        let mut restarted = Resolver::new(Mode::All);
        let lines = restarted.step(&world).expect("step");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("still shows"), "{lines:?}");
        assert_eq!(world.keys().len(), 1);
        for later in [AGAIN_AFTER_MS, 10 * AGAIN_AFTER_MS] {
            world.now.set(1_000_000 + later);
            assert!(
                restarted.step(&world).expect("step").is_empty(),
                "said once"
            );
            assert_eq!(world.keys().len(), 1, "never a second answer");
        }
        // Another restart re-reads the same answer: still no second key.
        Resolver::new(Mode::All).step(&world).expect("step");
        assert_eq!(world.keys().len(), 1);
    }

    #[test]
    fn five_answers_in_an_execution_or_twenty_across_the_fleet_open_the_breaker() {
        let now = 10_000_000;
        let commands = |sessions: &[&str], count: i64| {
            let rows: Vec<serde_json::Value> = sessions
                .iter()
                .flat_map(|session| {
                    (0..count).map(move |n| {
                        json!({"id": n, "session_id": session, "created_at_ms": now - 50_000 + n * 2_000})
                    })
                })
                .collect();
            Ledger::from_json(&json!(rows))
        };
        assert_eq!(
            commands(&["session_a"], 4).breaker("session_a", 0, now),
            None
        );
        assert!(
            commands(&["session_a"], 5)
                .breaker("session_a", 0, now)
                .is_some()
        );
        // Only this execution's: one started after them counts none.
        assert_eq!(
            commands(&["session_a"], 5).breaker("session_a", now - 1, now),
            None
        );
        let fleet: Vec<String> = (0..10).map(|n| format!("session_{n}")).collect();
        let names: Vec<&str> = fleet.iter().map(String::as_str).collect();
        assert!(commands(&names, 2).breaker("session_x", 0, now).is_some());
        assert_eq!(commands(&names[..9], 2).breaker("session_x", 0, now), None);
        // Keys of one answer, submitted together, count once.
        let one = Ledger::from_json(&json!([
            {"id": 2, "session_id": "s", "created_at_ms": 1_100},
            {"id": 1, "session_id": "s", "created_at_ms": 1_000},
        ]));
        assert_eq!(one.answers(Some("s"), 0), 1);
    }

    /// An open breaker escalates the proven row too: nothing submitted.
    #[test]
    fn an_open_breaker_presses_nothing() {
        let world = Fake::new(vec![live("session_a", false)], TRUST);
        for n in 0..5 {
            world.commands.borrow_mut().push((
                format!("earlier-{n}"),
                "session_a".to_string(),
                100 + n,
                world.now.get() - 60_000 + n * 2_000,
            ));
        }
        let lines = Resolver::new(Mode::All).step(&world).expect("step");
        assert!(lines[0].contains("breaker open"), "{lines:?}");
        assert!(world.keys().iter().all(|key| key.starts_with("earlier-")));
    }

    /// A person's answer in flight (any unfinished command of the session)
    /// holds the resolver off; once the dialog is gone it presses nothing.
    #[test]
    fn a_session_with_a_command_in_flight_or_no_dialog_gets_nothing() {
        let busy = Fake::new(vec![live("session_a", true)], TRUST);
        Resolver::new(Mode::All).step(&busy).expect("step");
        assert!(busy.keys().is_empty());
        let mut ready = Fake::new(vec![live("session_a", false)], &agy_idle());
        ready.reading = Some(AgentReading {
            status: AgentStatus::Idle,
            rule: None,
            blocker_visible: false,
            ..blocked("trust_prompt")
        });
        Resolver::new(Mode::All).step(&ready).expect("step");
        assert!(ready.keys().is_empty());
    }

    #[test]
    fn decide_needs_two_equal_reads_and_escalates_rows_without_keys() {
        let claude = WorkspaceId::new("agent_a:/tmp/agent_a").expect("workspace");
        // Claude Code's numbered permission: not captured, escalated.
        assert!(matches!(
            decide("s", "claude", &claude, PERMISSION, PERMISSION, None),
            Decision::Escalate { .. }
        ));
        let other = PERMISSION.replace("cargo test", "rm -rf target");
        assert_eq!(
            decide("s", "claude", &claude, PERMISSION, &other, None),
            Decision::Nothing("two reads in a row show two dialogs")
        );
        let limited = format!("Claude usage limit reached\n{PERMISSION}");
        assert!(matches!(
            decide("s", "claude", &claude, &limited, &limited, None),
            Decision::Escalate { .. }
        ));
        // agy's permission prompt: blocked by its rule, still escalated
        // (manifest text only); without the host's blocked reading, nothing.
        let perm = "Run command?\n  cargo test\n> 1. Yes\n  2. No\n\
                    ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command\nesc to cancel\n";
        assert_eq!(
            decide("s", "agy", &workspace(), perm, perm, None),
            Decision::Nothing("the host does not read the agent blocked by a named rule")
        );
        assert!(matches!(
            decide(
                "s",
                "agy",
                &workspace(),
                perm,
                perm,
                Some(&blocked("permission_prompt"))
            ),
            Decision::Escalate { .. }
        ));
    }

    #[test]
    fn decide_answers_agys_trust_dialog_for_its_workspace_only() {
        let reading = blocked("trust_prompt");
        assert!(matches!(
            decide("s", "agy", &workspace(), TRUST, TRUST, Some(&reading)),
            Decision::Answer { keys, expected_guard: None, .. }
                if keys == KeyChordSequence::single(KeyChord::from(NamedKey::Enter))
        ));
        let elsewhere = WorkspaceId::new("agent_b:/tmp/ws/agent_b").expect("workspace");
        assert!(matches!(
            decide("s", "agy", &elsewhere, TRUST, TRUST, Some(&reading)),
            Decision::Escalate { .. }
        ));
        let mut hidden = reading.clone();
        hidden.blocker_visible = false;
        assert!(matches!(
            decide("s", "agy", &workspace(), TRUST, TRUST, Some(&hidden)),
            Decision::Nothing(_)
        ));
    }

    /// Guard 3 on agy's trust path (M6): Herdr's `trust_prompt` rule alone
    /// is not enough; the driver's own reading of the trust dialog must
    /// agree on both reads. A screen with the folder and the trusting
    /// choice but not the dialog's question gets no answer.
    #[test]
    fn a_trust_rule_without_the_drivers_own_trust_reading_gets_no_answer() {
        let reading = blocked("trust_prompt");
        let partial = "Accessing workspace:\n\n/tmp/ws/agent_a\n\n\
                       > Yes, I trust this folder\n  No, exit\n\n  ↑/↓ Navigate · enter Confirm\n";
        let decided = decide("s", "agy", &workspace(), partial, partial, Some(&reading));
        assert_eq!(
            decided,
            Decision::Nothing("the engine's own reading shows no trust dialog")
        );
        // One read of the dialog and one without it: nothing either.
        assert!(!matches!(
            decide("s", "agy", &workspace(), TRUST, partial, Some(&reading)),
            Decision::Answer { .. }
        ));
    }

    #[test]
    fn the_approvals_item_shows_the_switch_its_answers_and_the_breaker() {
        let ledger = Ledger::from_json(&json!([
            {"id": 1, "session_id": "session_a", "created_at_ms": 990_000},
        ]));
        assert_eq!(
            auto_for(Mode::Startup, &ledger, "session_a", 1_000_000),
            json!({"state": "startup", "answered": 1, "breaker": "off"})
        );
        assert_eq!(
            auto_for(Mode::All, &ledger, "session_b", 1_000_000),
            json!({"state": "all", "answered": 0, "breaker": "closed"})
        );
    }

    fn styled_trust_snapshot(select_yes: bool) -> TuiSnapshot {
        let raw_vt = if select_yes {
            "\x1b[H\x1b[2JAccessing workspace:\r\n\r\n/tmp/ws/agent_a\r\n\r\n\
             Do you trust the contents of this project?\r\n\r\n\
             Antigravity CLI requires permission to read, edit, and execute files here.\r\n\r\n\
             \x1b[0;7m> Yes, I trust this folder\x1b[0m\r\n  No, exit\r\n\r\n  \
             ↑/↓ Navigate · enter Confirm\r\n\x1b[9;3H\x1b[?9001l"
        } else {
            "\x1b[H\x1b[2JAccessing workspace:\r\n\r\n/tmp/ws/agent_a\r\n\r\n\
             Do you trust the contents of this project?\r\n\r\n\
             Antigravity CLI requires permission to read, edit, and execute files here.\r\n\r\n\
             > Yes, I trust this folder\r\n\x1b[0;7m  No, exit\x1b[0m\r\n\r\n  \
             ↑/↓ Navigate · enter Confirm\r\n\x1b[10;3H\x1b[?9001l"
        };
        TuiSnapshot::from_styled_vt(
            RawStyledSnapshot {
                address: "t1",
                cols: 120,
                rows: 40,
                live_state: TerminalLiveState::Live { pid: Some(101) },
                title: None,
                raw_vt,
            },
            SnapshotObservation {
                agent: Some("agy".to_string()),
                agent_status: AgentStatus::Blocked,
                rule: Some("trust_prompt".to_string()),
                blocker_visible: true,
            },
        )
    }

    #[test]
    fn decide_snapshots_rejects_guard_drift_and_fidelity_downgrade() {
        let snap_yes = styled_trust_snapshot(true);
        let snap_no_hl = styled_trust_snapshot(false);
        assert_eq!(snap_yes.plain_lines(), snap_no_hl.plain_lines());
        assert_ne!(snap_yes.guard(), snap_no_hl.guard());

        assert_eq!(
            decide_snapshots("s", "agy", &workspace(), &snap_yes, &snap_no_hl),
            Decision::Nothing("screen guard changed between reads")
        );

        let plain = plain_snapshot(TRUST, Some(&blocked("trust_prompt")));
        assert_eq!(
            decide_snapshots("s", "agy", &workspace(), &snap_yes, &plain),
            Decision::Nothing("snapshot fidelity changed between reads")
        );

        let decided = decide_snapshots("s", "agy", &workspace(), &snap_yes, &snap_yes);
        assert!(matches!(
            decided,
            Decision::Answer {
                keys,
                expected_guard: Some(guard),
                ..
            } if keys == KeyChordSequence::single(KeyChord::from(NamedKey::Enter))
                && guard == snap_yes.guard()
        ));
    }

    #[test]
    fn resolver_enforces_pre_submit_screen_guard_cas() {
        let snap = styled_trust_snapshot(true);

        let aborted = Fake::new(vec![live("session_a", false)], TRUST);
        *aborted.styled_sequence.borrow_mut() = vec![snap.clone(), snap.clone()];
        aborted.guard_ok.set(false);
        let lines = Resolver::new(Mode::All).step(&aborted).expect("step");
        assert!(lines.is_empty(), "{lines:?}");
        assert!(
            aborted.keys().is_empty(),
            "stale/unobservable pre-submit guard must abort submit"
        );

        let passed = Fake::new(vec![live("session_a", false)], TRUST);
        *passed.styled_sequence.borrow_mut() = vec![snap.clone(), snap];
        passed.guard_ok.set(true);
        let lines = Resolver::new(Mode::All).step(&passed).expect("step");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(passed.keys().len(), 1);
    }
}
