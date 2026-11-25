//! Access as a restricted login: reads through the grant-filtered
//! `spawn_read_*` functions and submissions through the `agent_commands`
//! gateway, so the caller sees and sends exactly what `spawn_grants` allows;
//! and a person's writes from the web page as the `spawn_human` login,
//! through the `spawn_human_*` functions, the only ones that login may call
//! (an input, a key, a restart or reassign, a mission's resume).

use spawn_core::command::{NamedKey, Submission};
use spawn_core::id::{CommandId, IdempotencyKey, SessionId};
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// The fleet as one JSON array: each session with its latest execution's
/// launch summary (`launch`, issue #79; no secret in it).
pub(crate) const FLEET: &str =
    "select coalesce(jsonb_agg(row_to_json(f) order by f.session_id), '[]'::jsonb) as fleet
                   from (
                     select s.session_id, s.agent_id, s.state, s.live, s.engine,
                            s.execution_id, s.started_at_ms, s.ended_at_ms, s.last_event_at_ms,
                            s.launch,
                            st.observation ->> 'judge' as judge,
                            (st.observation ->> 'confidence')::float8 as confidence,
                            coalesce((st.observation ->> 'attention')::boolean, false) as attention,
                            (st.observation ->> 'needs_human')::float8 as needs_human,
                            (st.observation ->> 'stuck')::float8 as stuck,
                            st.observation ->> 'ambiguity' as ambiguity,
                            (select left(t.prompt, 300) from spawn_read_tasks() t
                              where t.session_id = s.session_id and t.prompt is not null
                              order by t.command_id desc limit 1) as task
                       from spawn_read_status() s
                       left join spawn_read_state() st using (session_id)
                   ) f";

/// One session's latest judgement, newest events, judgements and commands.
/// `$1` session, `$2` number of events.
pub(crate) const TAIL: &str = "select jsonb_build_object(
                    'session_id', $1::text,
                    'judgement', (select j.observation from spawn_read_judgements($1) j
                                   order by j.session_seq desc limit 1),
                    'events', coalesce((select jsonb_agg(e order by e.session_seq)
                                 from (select t.session_seq, t.source, t.event_name, t.detail, t.recorded_at_ms
                                         from spawn_read_timeline($1) t
                                        order by t.session_seq desc limit $2) e), '[]'::jsonb),
                    'judgements', coalesce((select jsonb_agg(j order by j.session_seq)
                                 from (select k.session_seq, k.recorded_at_ms,
                                              k.observation ->> 'state' as state,
                                              k.observation ->> 'judge' as judge,
                                              k.observation ->> 'confidence' as confidence,
                                              coalesce((k.observation ->> 'attention')::boolean, false) as attention,
                                              k.observation ->> 'attention_reason' as attention_reason
                                         from spawn_read_judgements($1) k
                                        order by k.session_seq desc limit 10) j), '[]'::jsonb),
                    'commands', coalesce((select jsonb_agg(c order by c.command_id)
                                 from (select t.command_id, t.principal, t.action,
                                              left(t.prompt, 300) as text, t.state, t.succeeded,
                                              t.error, t.created_at_ms
                                         from spawn_read_tasks() t
                                        where t.session_id = $1
                                        order by t.command_id desc limit 20) c), '[]'::jsonb)
                 ) as tail";

/// Submit as the database login, through `spawn_gateway_submit`, which
/// returns the command made: no login may select from `agent_commands`.
/// `$1` agent, `$2` session, `$3` idempotency key, `$4` action.
pub(crate) const SUBMIT: &str = "select spawn_gateway_submit($1, $2, $3, $4)";

/// A person's input as the `spawn_human` login: the command made (or the one
/// an earlier call with this key made), null when no agent has used the
/// session, SQLSTATE 55000 when the session runs no execution. `$1`
/// session, `$2` idempotency key, `$3` text.
pub(crate) const HUMAN_INPUT: &str = "select spawn_human_input($1, $2, $3)";

/// A person's key as the `spawn_human` login (`spawn_human_key`): the
/// command made (or the one an earlier call with this key made), null when
/// no agent has used the session, SQLSTATE 55000 when the session runs no
/// execution. `$1` session, `$2` idempotency key, `$3` the key's name.
pub(crate) const HUMAN_KEY: &str = "select spawn_human_key($1, $2, $3)";

/// A person's restart or reassign from the agent page, through
/// `spawn_human_task`: `{task, session_id, agent_id}`, the task and worker
/// that now carry the work, or SQLSTATE 55000 when the session holds no
/// delivered task. `$1` session, `$2` `restart` or `reassign`, `$3` reason.
pub(crate) const HUMAN_TASK: &str = "select spawn_human_task($1, $2, $3)";

/// A person's resume of a halted mission from the page, through
/// `spawn_human_resume`: the `mission.resumed` record's event id (a new one,
/// or the one an earlier call with this key made), SQLSTATE P0002 for no
/// such mission, 55000 when the mission is not halted on that halt, 23505
/// for a key another resume used. `$1` mission, `$2` the halt's event, `$3`
/// idempotency key.
pub(crate) const HUMAN_RESUME: &str = "select spawn_human_resume($1, $2, $3)";

/// A person's message to the supervisor from Master Control, through
/// `spawn_human_master_message` (issue #91): `{command_id, wake_command_id,
/// stop_command_id, steer_command_id, session_id, mode, principal, woke,
/// replayed}`.
pub(crate) const HUMAN_MASTER: &str = "select spawn_human_master_message($1, $2, $3)";

/// The longest idempotency key of a message to the supervisor (and of an
/// approval's act), in bytes: its derived keys (`<key>:wake`, `<key>:9`)
/// stay within the 256 bytes every key may hold.
/// `spawn_human_master_message` holds the same bound.
pub const MAX_MASTER_KEY_BYTES: usize = 240;

/// How a message to the supervisor is delivered
/// (`spawn_human_master_message`): `Auto` and `Queue` type it at the
/// supervisor's turn, `Steer` interrupts a running turn first (Esc),
/// `Restart` stops a running supervisor first; each wakes a supervisor that
/// runs no execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MasterMessageMode {
    Auto,
    Steer,
    Queue,
    Restart,
}

impl MasterMessageMode {
    /// Every mode, in the order the page lists them.
    pub const ALL: [Self; 4] = [Self::Auto, Self::Steer, Self::Queue, Self::Restart];

    /// The mode's name (`auto`, `steer`, `queue`, `restart`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Steer => "steer",
            Self::Queue => "queue",
            Self::Restart => "restart",
        }
    }

    /// The mode named `name`, or why there is none.
    pub fn from_name(name: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.name() == name)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|mode| mode.name()).collect();
                format!("mode {name:?} is not one of {}", names.join(", "))
            })
    }
}

/// What became of a person's message to the supervisor
/// ([`PostgresStore::human_master_message`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanMaster {
    /// Submitted (or the same request's commands again, for a repeated
    /// key): the function's answer.
    Submitted(serde_json::Value),
    /// An idempotency key another request used (SQLSTATE 23505), with the
    /// reason; nothing was made.
    KeyUsed(String),
    /// A text, key or mode the function refuses (SQLSTATE 22023), with the
    /// reason; nothing was made.
    Refused(String),
}

/// What became of a person's resume of a halted mission
/// ([`PostgresStore::human_resume`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanResume {
    /// Recorded: the `mission.resumed` record's event id (the first one,
    /// for a repeated key).
    Resumed(i64),
    /// No such mission, in the store's words. Nothing was recorded.
    NoMission(String),
    /// The mission is not halted, or is halted on another halt, or the
    /// idempotency key is another resume's, in the store's words. Nothing
    /// was recorded.
    Refused(String),
}

/// What became of a person's restart or reassign
/// ([`PostgresStore::human_task`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanTask {
    /// Recorded: `{task, session_id, agent_id}` of the task and the worker
    /// that now carry the work.
    Moved(serde_json::Value),
    /// The session holds no delivered task (or its task may not move), in
    /// the store's words. Nothing was recorded.
    Refused(String),
}

/// What became of a person's write from the page, an input or a key
/// ([`PostgresStore::submit_human_input`], [`PostgresStore::submit_human_key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanWrite {
    /// The command that carries it (a new one, or the one an earlier call
    /// with this idempotency key made).
    Submitted(CommandId),
    /// No agent has used the session: there is nothing to type into.
    NoAgent,
    /// The session runs no execution, in the store's words: an input or a
    /// key needs a running engine. No command was made.
    NotRunning(String),
    /// The idempotency key is another request's (another session, another
    /// key name, or an input), in the store's words. No command was made.
    KeyUsed(String),
}

/// The store's words when `error` is its refusal of an idempotency key
/// another request used (`spawn_human_replay`, SQLSTATE 23505).
fn key_used(error: &postgres::Error) -> Option<String> {
    error
        .as_db_error()
        .filter(|db| *db.code() == postgres::error::SqlState::UNIQUE_VIOLATION)
        .map(|db| db.message().to_string())
}

/// What became of a person's key ([`HumanWrite`]).
pub type HumanKey = HumanWrite;

/// What became of a person's input ([`HumanWrite`]).
pub type HumanInput = HumanWrite;

/// The answer of `spawn_human_input` or `spawn_human_key`: its command, none
/// for a session no agent has used, the refusal of a session that runs
/// no execution (SQLSTATE 55000, its message the reason), or the refusal of
/// an idempotency key another request used (SQLSTATE 23505).
fn human_write(answer: Result<postgres::Row, postgres::Error>) -> Result<HumanWrite, StoreError> {
    match answer {
        Ok(row) => Ok(row
            .get::<_, Option<i64>>(0)
            .map_or(HumanWrite::NoAgent, |id| {
                HumanWrite::Submitted(CommandId(id.unsigned_abs()))
            })),
        Err(error)
            if error.as_db_error().is_some_and(|db| {
                *db.code() == postgres::error::SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE
            }) =>
        {
            let message = error
                .as_db_error()
                .map_or_else(String::new, |db| db.message().to_string());
            Ok(HumanWrite::NotRunning(message))
        }
        Err(error) => key_used(&error)
            .map(HumanWrite::KeyUsed)
            .ok_or_else(|| crate::backend(&error)),
    }
}

impl PostgresStore {
    /// The fleet as one JSON array, read through the grant-filtered
    /// `spawn_read_*` functions, so it shows exactly what the caller's login
    /// may see.
    pub fn fleet_json(&self) -> Result<serde_json::Value, StoreError> {
        self.query_json(FLEET, &[])
    }

    /// One session's latest judgement (with the screen the judge saw) and
    /// its newest events, through the grant-filtered readers.
    pub fn tail_json(
        &self,
        session: &SessionId,
        events: i64,
    ) -> Result<serde_json::Value, StoreError> {
        self.query_json(TAIL, &[&session.as_str(), &events])
    }

    /// Submit through the `agent_commands` gateway. The principal is the
    /// database login, checked against `spawn_grants`; the caller cannot name
    /// another one.
    pub fn submit_via_gateway(&self, submission: &Submission) -> Result<CommandId, StoreError> {
        let action = serde_json::to_value(&submission.action)
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        // The gateway's trigger returns the command it made (a new one, or
        // the one an earlier call with this key made), and RETURNING reads it.
        let row = self.query_one(
            SUBMIT,
            &[
                &submission.agent_id.as_str(),
                &submission.session_id.as_str(),
                &submission.idempotency_key.as_str(),
                &action,
            ],
        )?;
        Ok(CommandId(row.get::<_, i64>(0).unsigned_abs()))
    }

    /// Press `key` in `session` as a person, through `spawn_human_key`:
    /// the key command `spawn submit --principal human` would submit to the
    /// session's agent, with the same idempotency rule. Only a store
    /// connected as the `spawn_human` login may call it. A session that
    /// runs no execution is refused ([`HumanKey::NotRunning`]) and nothing
    /// is recorded; so is an idempotency key another request used
    /// ([`HumanKey::KeyUsed`]).
    pub fn submit_human_key(
        &self,
        session: &SessionId,
        key: &IdempotencyKey,
        name: NamedKey,
    ) -> Result<HumanKey, StoreError> {
        let answer = self
            .lock()
            .query_one(HUMAN_KEY, &[&session.as_str(), &key.as_str(), &name.name()]);
        human_write(answer)
    }

    /// Resume `mission`, halted on `halt` (its `mission.halted` event), as a
    /// person, through `spawn_human_resume`: the owner's `mission.resumed`,
    /// with `by` `human` and the `halt`, once per idempotency `key`. Only a
    /// store connected as the `spawn_human` login may call it. A mission
    /// that is not halted, or is halted on another halt, is refused
    /// ([`HumanResume::Refused`]) and nothing is recorded.
    pub fn human_resume(
        &self,
        mission: i64,
        halt: i64,
        key: &IdempotencyKey,
    ) -> Result<HumanResume, StoreError> {
        let answer = self
            .lock()
            .query_one(HUMAN_RESUME, &[&mission, &halt, &key.as_str()]);
        let error = match answer {
            Ok(row) => return Ok(HumanResume::Resumed(row.get::<_, i64>(0))),
            Err(error) => error,
        };
        let Some(db) = error.as_db_error() else {
            return Err(crate::backend(&error));
        };
        let message = db.message().to_string();
        match db.code() {
            code if *code == postgres::error::SqlState::NO_DATA_FOUND => {
                Ok(HumanResume::NoMission(message))
            }
            code if *code == postgres::error::SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE
                || *code == postgres::error::SqlState::UNIQUE_VIOLATION =>
            {
                Ok(HumanResume::Refused(message))
            }
            _ => Err(crate::backend(&error)),
        }
    }

    /// Restart (`restart`) or reassign (`reassign`) the delivered task of
    /// `session` as a person, through `spawn_human_task`, with `by` `human`.
    /// Only a store connected as the `spawn_human` login may call it. A
    /// session that holds no delivered task is refused
    /// ([`HumanTask::Refused`]) and nothing is recorded.
    pub fn human_task(
        &self,
        session: &SessionId,
        action: &str,
        reason: Option<&str>,
    ) -> Result<HumanTask, StoreError> {
        let answer = self
            .lock()
            .query_one(HUMAN_TASK, &[&session.as_str(), &action, &reason]);
        match answer {
            Ok(row) => Ok(HumanTask::Moved(row.get::<_, serde_json::Value>(0))),
            Err(error)
                if error.as_db_error().is_some_and(|db| {
                    *db.code() == postgres::error::SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE
                }) =>
            {
                let message = error
                    .as_db_error()
                    .map_or_else(String::new, |db| db.message().to_string());
                Ok(HumanTask::Refused(message))
            }
            Err(error) => Err(crate::backend(&error)),
        }
    }

    /// Send `text` to the supervisor as a person in `mode`, through
    /// `spawn_human_master_message`:
    /// a supervisor that runs no execution is woken first, never refused.
    /// Only a store connected as the `spawn_human` login may call it.
    pub fn human_master_message(
        &self,
        key: &IdempotencyKey,
        text: &str,
        mode: MasterMessageMode,
    ) -> Result<HumanMaster, StoreError> {
        let answer = self
            .lock()
            .query_one(HUMAN_MASTER, &[&key.as_str(), &text, &mode.name()]);
        match answer {
            Ok(row) => Ok(HumanMaster::Submitted(row.get::<_, serde_json::Value>(0))),
            Err(error)
                if error.as_db_error().is_some_and(|db| {
                    *db.code() == postgres::error::SqlState::INVALID_PARAMETER_VALUE
                }) =>
            {
                let message = error
                    .as_db_error()
                    .map_or_else(String::new, |db| db.message().to_string());
                Ok(HumanMaster::Refused(message))
            }
            Err(error) => key_used(&error)
                .map(HumanMaster::KeyUsed)
                .ok_or_else(|| crate::backend(&error)),
        }
    }

    /// Type `text` into `session` as a person, through `spawn_human_input`:
    /// the input `spawn submit --principal human` would submit to the
    /// session's agent, with the same idempotency rule (a repeated `key`
    /// returns the first command) for the same session and text only: a
    /// `key` another session's input used, or other text, is refused
    /// ([`HumanInput::KeyUsed`]) and nothing is recorded. Only a store
    /// connected as the `spawn_human` login may call it (`role_url(owner,
    /// "spawn_human")`); that login can do nothing else.
    /// [`HumanInput::NoAgent`] when no agent has used the session; a session
    /// that runs no execution is refused ([`HumanInput::NotRunning`]) and
    /// nothing is recorded, though a repeated key still returns its first
    /// command (objective 040).
    pub fn submit_human_input(
        &self,
        session: &SessionId,
        key: &IdempotencyKey,
        text: &str,
    ) -> Result<HumanInput, StoreError> {
        let answer = self
            .lock()
            .query_one(HUMAN_INPUT, &[&session.as_str(), &key.as_str(), &text]);
        human_write(answer)
    }
}
