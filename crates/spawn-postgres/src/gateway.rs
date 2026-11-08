//! Access as a restricted login: reads through the grant-filtered
//! `spawn_read_*` functions and submissions through the `agent_commands`
//! gateway, so the caller sees and sends exactly what `spawn_grants` allows;
//! and a person's input from the web page as the `spawn_human` login,
//! through `spawn_human_input`, the one function that login may call.

use spawn_core::command::Submission;
use spawn_core::id::{CommandId, IdempotencyKey, SessionId};
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// The fleet as one JSON array.
pub(crate) const FLEET: &str =
    "select coalesce(jsonb_agg(row_to_json(f) order by f.session_id), '[]'::jsonb) as fleet
                   from (
                     select s.session_id, s.agent_id, s.state, s.live, s.engine,
                            s.execution_id, s.started_at_ms, s.ended_at_ms, s.last_event_at_ms,
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
/// session. `$1` session, `$2` idempotency key, `$3` text.
pub(crate) const HUMAN_INPUT: &str = "select spawn_human_input($1, $2, $3)";

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

    /// Type `text` into `session` as a person, through `spawn_human_input`:
    /// the input `spawn submit --principal human` would submit to the
    /// session's agent, with the same idempotency rule (a repeated `key`
    /// returns the first command). Only a store connected as the
    /// `spawn_human` login may call it (`role_url(owner, "spawn_human")`);
    /// that login can do nothing else. `None` when no agent has used the
    /// session.
    pub fn submit_human_input(
        &self,
        session: &SessionId,
        key: &IdempotencyKey,
        text: &str,
    ) -> Result<Option<CommandId>, StoreError> {
        let row = self.query_one(HUMAN_INPUT, &[&session.as_str(), &key.as_str(), &text])?;
        Ok(row
            .get::<_, Option<i64>>(0)
            .map(|id| CommandId(id.unsigned_abs())))
    }
}
