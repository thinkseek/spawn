//! The run-time resolver (issue #91, Herdr PR 3; design section 6.3): a
//! step of `spawn loop` that answers a dialog an engine shows at run time
//! from a fixed table, as `key` commands of its own principal
//! (`spawn-resolve`), which the Bridge presses as it presses a person's.
//!
//! - **Switch.** `SPAWN_AUTO_APPROVE = off | startup | all`, default
//!   `startup`: the step submits nothing unless it is `all`. `startup` is
//!   today's behaviour, the Bridge's own answer to an engine's startup trust
//!   dialog (itself a `key` command of `spawn-resolve` since PR 3, so it is
//!   audited too). `off` makes the resolver submit nothing as well; the
//!   Bridge's startup answer is part of a start and is not switched here.
//!   An unknown value is warned about and read as `startup`.
//! - **Table.** `spawn_core::driver::table_answer`, pure: only rows whose
//!   screen was seen working answer with keys; every other row, and a hard
//!   block (`spawn_core::driver::hard_block`: a login, a quota, a needed
//!   update, exit-only choices), escalates: nothing is pressed, the item
//!   stays in `/api/approvals` for a person, and the loop says so once.
//! - **Guards, all of them, before any key:** two pane reads in a row give
//!   the same dialog id (the approvals page's, `serve/approvals.rs`); on a
//!   host with agent detection (Herdr), for an engine other than Claude
//!   Code, its reading is `blocked` by a named rule with the blocker
//!   visible; the driver's own reading of the pane is `Permission`; the
//!   row's own guard holds; no command of the session is unfinished (a
//!   person's answer, or ours, still in flight); the breaker is closed.
//! - **Breaker, folded from the log, never from memory.** Every count is
//!   read from the commands of `spawn-resolve` (`commands_by`), and the
//!   per-dialog count from the idempotency of `submit`: an answer is keyed
//!   `resolve:<execution>:<dialog id>:<n>:<k>` (its `n`-th answer, its
//!   `k`-th key), and a key submitted again returns the command already
//!   there and writes nothing. So a restart re-reads the same counts.
//!   - an answered dialog that still shows [`STILL_SHOWS_MS`] after its
//!     answer, and before [`AGAIN_AFTER_MS`], opens it for that dialog;
//!   - a second answer to the same dialog only from [`AGAIN_AFTER_MS`]
//!     after the first; a third never (open);
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
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId, WorkspaceId};
use spawn_core::port::{AgentReading, AgentStatus};
use spawn_core::screen::ScreenState;

use crate::serve::approvals::{Prompt, detect, fnv1a, normalized};

/// The resolver's principal: the Bridge's ([`spawn_core::bridge::RESOLVE_PRINCIPAL`]).
pub(crate) const PRINCIPAL: &str = spawn_core::bridge::RESOLVE_PRINCIPAL;

/// An answered dialog still showing this long after its answer opens the
/// breaker for it (two reads of the loop's pass have seen it).
pub(crate) const STILL_SHOWS_MS: i64 = 5_000;
/// The earliest a second answer to the same dialog may follow the first.
pub(crate) const AGAIN_AFTER_MS: i64 = 30_000;
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
            None | Some("") | Some("startup") => Ok(Self::Startup),
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
    /// Press `keys` for the dialog `dialog`.
    Answer {
        dialog: String,
        keys: Vec<NamedKey>,
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

/// A tmux key name as a key command's key.
fn named(key: &str) -> Option<NamedKey> {
    NamedKey::ALL.iter().copied().find(|named| named.tmux() == key)
}

/// What one pane, read twice (`first`, then `second`), calls for: pure.
/// `reading` is the host's own reading of the agent (none on tmux).
pub(crate) fn decide(
    session: &str,
    engine: &str,
    workspace: &WorkspaceId,
    first: &str,
    second: &str,
    reading: Option<&AgentReading>,
) -> Decision {
    let driver = driver_or_default(engine);
    let screen = driver.read_screen(second);
    let herdr_rule = reading
        .filter(|_| driver.id() != "claude")
        .and_then(|reading| reading.rule.as_deref());
    let trust_rule = herdr_rule == Some("trust_prompt");
    if screen.state != ScreenState::Permission && !trust_rule {
        return Decision::Nothing("the engine's own reading shows no dialog");
    }
    let prompts: (Option<Prompt>, Option<Prompt>) = (detect(first), detect(second));
    let (dialog, prompt) = match prompts {
        (Some(one), Some(two)) if one.id(session) == two.id(session) => (two.id(session), Some(two)),
        (None, None) if trust_rule && bottom_id(session, "trust_prompt", first)
            == bottom_id(session, "trust_prompt", second) =>
        {
            (bottom_id(session, "trust_prompt", second), None)
        }
        (None, None) => {
            return Decision::Escalate {
                dialog: bottom_id(session, "unread", second),
                why: "a dialog Spawn reads no prompt on".to_string(),
            };
        }
        _ => return Decision::Nothing("two reads in a row show two dialogs"),
    };
    if driver.id() != "claude" {
        let blocked = reading.is_some_and(|reading| {
            reading.status == AgentStatus::Blocked
                && reading.rule.is_some()
                && reading.blocker_visible
        });
        if !blocked {
            return Decision::Nothing("the host does not read the agent blocked by a named rule");
        }
    }
    let choices: Vec<String> = prompt.as_ref().map_or_else(
        || {
            second
                .lines()
                .map(|line| line.trim().trim_start_matches(['>', '❯']).trim().to_string())
                .filter(|line| line.starts_with("Yes, I trust") || line.starts_with("No, exit"))
                .collect()
        },
        |prompt| prompt.choices.iter().map(|choice| choice.label.clone()).collect(),
    );
    if let Some(why) = hard_block(second, &choices) {
        return Decision::Escalate {
            dialog,
            why: format!("a hard block ({why})"),
        };
    }
    let cursor: Option<String> = prompt.as_ref().map_or_else(
        || {
            second
                .lines()
                .find_map(|line| line.trim().strip_prefix('>').map(|rest| rest.trim().to_string()))
        },
        |prompt| {
            prompt.cursor.as_ref().and_then(|id| {
                prompt
                    .choices
                    .iter()
                    .find(|choice| &choice.id == id)
                    .map(|choice| choice.label.clone())
            })
        },
    );
    let body: Vec<String> = prompt.as_ref().map_or_else(
        || second.lines().map(|line| line.trim().to_string()).collect(),
        |prompt| prompt.body.clone(),
    );
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
        Answer::Keys(keys) => match keys.iter().map(|key| named(key)).collect() {
            Some(keys) => Decision::Answer { dialog, keys },
            None => Decision::Escalate {
                dialog,
                why: "an answer with a key Spawn has no command for".to_string(),
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

/// What the step reads and writes: the store and the terminal host in the
/// loop, a fake in the tests.
pub(crate) trait World {
    /// The store's clock.
    fn now_ms(&self) -> Result<i64, String>;
    /// The resolver's commands (`commands_by`).
    fn ledger(&self) -> Result<Ledger, String>;
    /// The live executions.
    fn live(&self) -> Result<Vec<Live>, String>;
    /// The pane's tail.
    fn capture(&self, live: &Live) -> Result<String, String>;
    /// The pause between the two reads.
    fn pause(&self);
    /// The host's reading of the agent, if it has one.
    fn reading(&self, live: &Live) -> Option<AgentReading>;
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
            let Ok(first) = world.capture(&live) else {
                continue;
            };
            let reading = world.reading(&live);
            let trust = reading
                .as_ref()
                .is_some_and(|reading| reading.rule.as_deref() == Some("trust_prompt"));
            if driver.read_screen(&first).state != ScreenState::Permission && !trust {
                continue;
            }
            world.pause();
            let Ok(second) = world.capture(&live) else {
                continue;
            };
            let reading = world.reading(&live);
            let workspace = workspace_of(&live.agent)?;
            match decide(
                &live.session,
                &live.engine,
                &workspace,
                &first,
                &second,
                reading.as_ref(),
            ) {
                Decision::Nothing(_) => {}
                Decision::Escalate { dialog, why } => {
                    let line = format!("resolve: {} escalated {dialog}: {why}", live.session);
                    self.say(&mut lines, &format!("{}:{dialog}", live.execution), line);
                }
                Decision::Answer { dialog, keys } => {
                    if let Some(why) = ledger.breaker(&live.session, live.started_ms, now) {
                        let line = format!(
                            "resolve: {} breaker open, escalated {dialog}: {why}",
                            live.session
                        );
                        self.say(&mut lines, &format!("{}:open", live.execution), line);
                        continue;
                    }
                    if let Some(line) = self.answer(world, &mut ledger, &live, &dialog, &keys, now)?
                    {
                        lines.push(line);
                    }
                }
            }
        }
        Ok(lines)
    }

    /// Submit the `n`-th answer to `dialog` (1, else 2), as the log allows
    /// ([`Ledger`], `submit`'s idempotency): the line to log, if any.
    fn answer(
        &mut self,
        world: &dyn World,
        ledger: &mut Ledger,
        live: &Live,
        dialog: &str,
        keys: &[NamedKey],
        now: i64,
    ) -> Result<Option<String>, String> {
        let known: HashSet<i64> = ledger.answered.iter().map(|answered| answered.id).collect();
        for n in 1..=2 {
            let key = |k: usize| format!("resolve:{}:{dialog}:{n}:{k}", live.execution);
            let first = world.submit(live, &key(1), keys[0])?;
            if !known.contains(&first) {
                for (at, named) in keys.iter().enumerate().skip(1) {
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
                let pressed: Vec<&str> = keys.iter().map(|key| key.name()).collect();
                return Ok(Some(format!(
                    "resolve: {} answered {dialog} ({n}): {}",
                    live.session,
                    pressed.join(" ")
                )));
            }
            let age = now - ledger.get(first).map_or(now, |answered| answered.created_at_ms);
            if n == 2 {
                break;
            }
            if age < STILL_SHOWS_MS {
                return Ok(None);
            }
            if age < AGAIN_AFTER_MS {
                let line = format!(
                    "resolve: {} breaker open for {dialog}: it still shows {} s after its answer",
                    live.session,
                    age / 1000
                );
                self.say(&mut Vec::new(), "", String::new());
                return Ok(self
                    .said
                    .insert(format!("{}:{dialog}:shows", live.execution))
                    .then_some(line));
            }
        }
        let line = format!(
            "resolve: {} breaker open for {dialog}: answered twice already, never a third time",
            live.session
        );
        Ok(self
            .said
            .insert(format!("{}:{dialog}:third", live.execution))
            .then_some(line))
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

/// The loop's world: its store and its terminal host.
pub(crate) struct Fleet<'a> {
    pub(crate) store: &'a spawn_postgres::PostgresStore,
    pub(crate) host: &'a spawn_host::LocalTerminalHost,
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
        let board = self.store.fleet_board().map_err(|error| error.to_string())?;
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

    fn capture(&self, live: &Live) -> Result<String, String> {
        let execution =
            spawn_core::id::ExecutionId::new(&live.execution).map_err(|error| error.to_string())?;
        crate::serve::approvals::capture(self.host, &execution, &live.agent)
    }

    fn pause(&self) {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    fn reading(&self, live: &Live) -> Option<AgentReading> {
        use spawn_core::port::TerminalHost;
        let execution = spawn_core::id::ExecutionId::new(&live.execution).ok()?;
        let terminal = spawn_core::id::TerminalName::for_execution(&execution);
        self.host
            .agent(&terminal, &workspace_of(&live.agent).ok()?)
            .ok()
            .flatten()
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
mod tests;
