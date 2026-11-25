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
    Answer { dialog: String, keys: Vec<NamedKey> },
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
    NamedKey::ALL
        .iter()
        .copied()
        .find(|named| named.tmux() == key)
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
        (Some(one), Some(two)) if one.id(session) == two.id(session) => {
            (two.id(session), Some(two))
        }
        (None, None)
            if trust_rule
                && bottom_id(session, "trust_prompt", first)
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
                .map(|line| {
                    line.trim()
                        .trim_start_matches(['>', '❯'])
                        .trim()
                        .to_string()
                })
                .filter(|line| line.starts_with("Yes, I trust") || line.starts_with("No, exit"))
                .collect()
        },
        |prompt| {
            prompt
                .choices
                .iter()
                .map(|choice| choice.label.clone())
                .collect()
        },
    );
    if let Some(why) = hard_block(second, &choices) {
        return Decision::Escalate {
            dialog,
            why: format!("a hard block ({why})"),
        };
    }
    let cursor: Option<String> = prompt.as_ref().map_or_else(
        || {
            second.lines().find_map(|line| {
                line.trim()
                    .strip_prefix('>')
                    .map(|rest| rest.trim().to_string())
            })
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
                    if let Some(line) =
                        self.answer(world, &mut ledger, &live, &dialog, &keys, now)?
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
            let age = now
                - ledger
                    .get(first)
                    .map_or(now, |answered| answered.created_at_ms);
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
mod tests {
    use super::{
        AGAIN_AFTER_MS, Decision, Ledger, Live, Mode, PRINCIPAL, Resolver, STILL_SHOWS_MS, World,
        auto_for, decide,
    };
    use serde_json::json;
    use spawn_core::command::NamedKey;
    use spawn_core::id::WorkspaceId;
    use spawn_core::port::{AgentReading, AgentStatus};
    use std::cell::{Cell, RefCell};

    /// Claude Code's numbered permission dialog at its prompt.
    const PERMISSION: &str = "Bash command\n  cargo test\nDo you want to proceed?\n\
                              ❯ 1. Yes\n  2. Yes, and don't ask again\n  3. No\n";
    const READY: &str = "╭────╮\n│ > │\n╰────╯\n";

    fn live(session: &str, busy: bool) -> Live {
        Live {
            session: session.to_string(),
            agent: session.replace("session_", "agent_"),
            execution: format!("exec_{session}"),
            engine: "claude".to_string(),
            started_ms: 0,
            busy,
        }
    }

    /// A world in memory: panes, a clock, a command table with
    /// `submit`'s idempotency, and every call counted.
    struct Fake {
        now: Cell<i64>,
        lives: Vec<Live>,
        pane: RefCell<String>,
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
                reading: None,
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
    }

    impl World for Fake {
        fn now_ms(&self) -> Result<i64, String> {
            self.reads.set(self.reads.get() + 1);
            Ok(self.now.get())
        }
        fn ledger(&self) -> Result<Ledger, String> {
            self.reads.set(self.reads.get() + 1);
            let commands: Vec<serde_json::Value> = self
                .commands
                .borrow()
                .iter()
                .rev()
                .map(|(_, session, id, at)| json!({"id": id, "session_id": session, "created_at_ms": at}))
                .collect();
            Ok(Ledger::from_json(&json!(commands)))
        }
        fn live(&self) -> Result<Vec<Live>, String> {
            self.reads.set(self.reads.get() + 1);
            Ok(self.lives.clone())
        }
        fn capture(&self, _live: &Live) -> Result<String, String> {
            self.reads.set(self.reads.get() + 1);
            Ok(self.pane.borrow().clone())
        }
        fn pause(&self) {}
        fn reading(&self, _live: &Live) -> Option<AgentReading> {
            self.reading.clone()
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
            let world = Fake::new(vec![live("session_a", false)], PERMISSION);
            let mut resolver = Resolver::new(Mode::parse(value).expect("mode"));
            assert!(!resolver.runs());
            assert!(resolver.step(&world).expect("step").is_empty());
            assert!(world.keys().is_empty(), "{value:?}");
            assert_eq!(world.reads.get(), 0, "nothing read either: {value:?}");
        }
    }

    #[test]
    fn under_all_a_table_row_is_answered_once_keyed_by_execution_and_dialog() {
        let world = Fake::new(vec![live("session_a", false)], PERMISSION);
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
    /// loop) over the same command table opens it exactly as the first.
    #[test]
    fn the_breaker_counts_from_the_log_and_survives_a_restart() {
        let world = Fake::new(vec![live("session_a", false)], PERMISSION);
        Resolver::new(Mode::All).step(&world).expect("step");
        // Still showing after STILL_SHOWS_MS: open for that dialog, nothing
        // pressed, said once.
        world.now.set(world.now.get() + STILL_SHOWS_MS);
        let mut restarted = Resolver::new(Mode::All);
        let lines = restarted.step(&world).expect("step");
        assert!(lines[0].contains("breaker open"), "{lines:?}");
        assert!(
            restarted.step(&world).expect("step").is_empty(),
            "said once"
        );
        assert_eq!(world.keys().len(), 1);
        // From AGAIN_AFTER_MS on: one second answer, then never a third.
        world.now.set(world.now.get() + AGAIN_AFTER_MS);
        Resolver::new(Mode::All).step(&world).expect("step");
        let keys = world.keys();
        assert_eq!(keys.len(), 2);
        assert!(keys[1].ends_with(":2:1"), "{keys:?}");
        world.now.set(world.now.get() + 10 * AGAIN_AFTER_MS);
        let lines = Resolver::new(Mode::All).step(&world).expect("step");
        assert!(lines[0].contains("never a third"), "{lines:?}");
        assert_eq!(world.keys().len(), 2);
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

    /// A person's answer in flight (any unfinished command of the session)
    /// holds the resolver off; once the dialog is gone it presses nothing.
    #[test]
    fn a_session_with_a_command_in_flight_or_no_dialog_gets_nothing() {
        let busy = Fake::new(vec![live("session_a", true)], PERMISSION);
        Resolver::new(Mode::All).step(&busy).expect("step");
        assert!(busy.keys().is_empty());
        let ready = Fake::new(vec![live("session_a", false)], READY);
        Resolver::new(Mode::All).step(&ready).expect("step");
        assert!(ready.keys().is_empty());
    }

    #[test]
    fn decide_needs_two_equal_reads_and_escalates_rows_without_keys() {
        let workspace = WorkspaceId::new("agent_a:/tmp/agent_a").expect("workspace");
        let answer = decide("s", "claude", &workspace, PERMISSION, PERMISSION, None);
        assert!(
            matches!(&answer, Decision::Answer { keys, .. } if keys == &vec![NamedKey::Digit1]),
            "{answer:?}"
        );
        let other = PERMISSION.replace("cargo test", "rm -rf target");
        assert_eq!(
            decide("s", "claude", &workspace, PERMISSION, &other, None),
            Decision::Nothing("two reads in a row show two dialogs")
        );
        let limited = format!("Claude usage limit reached\n{PERMISSION}");
        assert!(matches!(
            decide("s", "claude", &workspace, &limited, &limited, None),
            Decision::Escalate { .. }
        ));
        // agy's permission prompt: blocked by its rule, still escalated
        // (manifest text only); without the host's blocked reading, nothing.
        let perm = "Run command?\n  cargo test\n> 1. Yes\n  2. No\n\
                    ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command\nesc to cancel\n";
        assert_eq!(
            decide("s", "agy", &workspace, perm, perm, None),
            Decision::Nothing("the host does not read the agent blocked by a named rule")
        );
        let blocked = AgentReading {
            agent: Some("agy".to_string()),
            status: AgentStatus::Blocked,
            rule: Some("permission_prompt".to_string()),
            blocker_visible: true,
            session: None,
            seq: 1,
            manifest: Some("2026.10.05.1".to_string()),
        };
        assert!(matches!(
            decide("s", "agy", &workspace, perm, perm, Some(&blocked)),
            Decision::Escalate { .. }
        ));
    }

    #[test]
    fn decide_answers_agys_trust_dialog_for_its_workspace_only() {
        let workspace = WorkspaceId::new("agent_a:/tmp/ws/agent_a").expect("workspace");
        let trust = "Accessing workspace:\n\n/tmp/ws/agent_a\n\nDo you trust the contents of this project?\n\n\
                     > Yes, I trust this folder\n  No, exit\n\n  ↑/↓ Navigate · enter Confirm\n";
        let reading = AgentReading {
            agent: Some("agy".to_string()),
            status: AgentStatus::Blocked,
            rule: Some("trust_prompt".to_string()),
            blocker_visible: true,
            session: None,
            seq: 1,
            manifest: None,
        };
        assert!(matches!(
            decide("s", "agy", &workspace, trust, trust, Some(&reading)),
            Decision::Answer { keys, .. } if keys == vec![NamedKey::Enter]
        ));
        let elsewhere = WorkspaceId::new("agent_b:/tmp/ws/agent_b").expect("workspace");
        assert!(matches!(
            decide("s", "agy", &elsewhere, trust, trust, Some(&reading)),
            Decision::Escalate { .. }
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
}
