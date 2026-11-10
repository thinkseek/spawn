//! An agent's answer to a message, by the message's id (objective 046;
//! docs/fleet-architecture.md §3.4, "Messages"): `message.acted` in the
//! message's session, keyed `<command id>:acted`, so the first answer
//! stands and a later one records nothing, whatever restarts.
//!
//! Two things answer a message. An ack (`spawn ack <id> done|declined
//! [<why>]`), which the agent writes in its own workspace and the loop
//! reads (spawn-cli's `ack`): whose ack it is, is the workspace's, never an
//! id the ack carries, so [`PostgresStore::record_ack`] takes the agent the
//! loop found it with. And a task's report, which answers the message that
//! delivered the task (the command `task.delivered` names):
//! [`PostgresStore::report_answers`] reads the reports whose delivery has
//! no answer yet.

use serde_json::Value;
use spawn_core::event::{EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, CommandId, SessionId};
use spawn_core::port::{EventLog, StoreError};

use crate::PostgresStore;

/// What became of one ack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckRecord {
    /// Recorded: the message's first answer.
    Recorded {
        /// The message's session.
        session_id: String,
    },
    /// The message was answered before: nothing recorded.
    Answered,
    /// Not an answer this pane can give: nothing recorded, and why.
    Refused(String),
}

/// A task's report whose delivery has no answer yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportAnswer {
    /// The task.
    pub task: i64,
    /// The command that delivered it.
    pub command_id: i64,
    /// The report, as `task.reported` holds it.
    pub report: String,
}

/// The command an ack names: its agent and session, whether it types text
/// (an input, or a start or resume with a prompt) and whether it delivers a
/// task (a `deliver-` key, or a `task.delivered` naming it). `$1` the id.
const ACK_TARGET: &str = r"
select c.agent_id, c.session_id,
       c.action ->> 'type' = 'input'
         or (c.action ->> 'type' in ('start', 'resume')
             and coalesce(c.action ->> 'prompt', '') <> '') as types_text,
       c.idempotency_key like 'deliver-%'
         or exists (select 1 from spawn_events e
                     where e.session_id = 'session_control'
                       and e.event_name = 'control.recorded'
                       and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.delivered'
                       and e.kind -> 'detail' -> 'record' ->> 'command_id' = c.id::text)
         as delivery
  from spawn_commands c
 where c.id = $1";

/// The reports (`task.reported`) recorded at or after `$1` whose delivery
/// (the latest `task.delivered` of the task before the report) was a
/// tracked message (the Bridge typed its marker: `cmd_<id>:message`) with
/// no answer yet, oldest first.
const REPORT_ANSWERS: &str = r"
select (r.kind -> 'detail' -> 'record' ->> 'id')::bigint as task,
       d.command_id,
       coalesce(r.kind -> 'detail' -> 'record' ->> 'report', '') as report
  from spawn_events r
  cross join lateral (
    select (e.kind -> 'detail' -> 'record' ->> 'command_id')::bigint as command_id
      from spawn_events e
     where e.session_id = 'session_control' and e.event_name = 'control.recorded'
       and e.kind -> 'detail' -> 'record' ->> 'id' = r.kind -> 'detail' -> 'record' ->> 'id'
       and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.delivered'
       and e.session_seq < r.session_seq
       and jsonb_typeof(e.kind -> 'detail' -> 'record' -> 'command_id') = 'number'
     order by e.session_seq desc
     limit 1) d
  join spawn_commands c on c.id = d.command_id
 where r.session_id = 'session_control' and r.event_name = 'control.recorded'
   and r.recorded_at_ms >= $1
   and r.kind -> 'detail' -> 'record' ->> 'type' = 'task.reported'
   and exists (select 1 from spawn_events m
                where m.session_id = c.session_id and m.source = 'bridge'
                  and m.source_event_id = 'cmd_' || c.id || ':message')
   and not exists (select 1 from spawn_events a
                    where a.session_id = c.session_id and a.source = 'bridge'
                      and a.source_event_id = c.id || ':acted')
 order by r.session_seq";

/// The session and agent of command `$1`.
const COMMAND_SESSION: &str = "select agent_id, session_id from spawn_commands where id = $1";

impl PostgresStore {
    /// Record the ack of message `command` that the loop found in `agent`'s
    /// workspace: `outcome` (`done` or `declined`) and `why`. Refused, with
    /// nothing recorded, when no command has that id, when it went into
    /// another agent's session, types nothing, or delivers a task (its
    /// report answers it).
    pub fn record_ack(
        &self,
        agent: &str,
        command: i64,
        outcome: &str,
        why: Option<&str>,
    ) -> Result<AckRecord, StoreError> {
        let Some(row) = self.query_opt(ACK_TARGET, &[&command])? else {
            return Ok(AckRecord::Refused(format!("no message #{command}")));
        };
        let target_agent: String = row.get("agent_id");
        let session: String = row.get("session_id");
        if target_agent != agent {
            return Ok(AckRecord::Refused(format!(
                "message #{command} went into {session}, not this pane's session"
            )));
        }
        if !row.get::<_, Option<bool>>("types_text").unwrap_or(false) {
            return Ok(AckRecord::Refused(format!(
                "command #{command} types nothing: it is no message"
            )));
        }
        if row.get::<_, Option<bool>>("delivery").unwrap_or(false) {
            return Ok(AckRecord::Refused(format!(
                "message #{command} delivers a task: the task's report answers it"
            )));
        }
        self.acted(&target_agent, &session, command, outcome, why)
    }

    /// The reports recorded at or after `since_ms` whose delivery, a
    /// tracked message, has no answer yet ([`REPORT_ANSWERS`]).
    pub fn report_answers(&self, since_ms: i64) -> Result<Vec<ReportAnswer>, StoreError> {
        Ok(self
            .query(REPORT_ANSWERS, &[&since_ms])?
            .iter()
            .map(|row| ReportAnswer {
                task: row.get("task"),
                command_id: row.get("command_id"),
                report: row.get("report"),
            })
            .collect())
    }

    /// Record that task's report answered its delivery `command`.
    pub fn record_report_answer(
        &self,
        command: i64,
        outcome: &str,
        why: Option<&str>,
    ) -> Result<AckRecord, StoreError> {
        let Some(row) = self.query_opt(COMMAND_SESSION, &[&command])? else {
            return Ok(AckRecord::Refused(format!("no message #{command}")));
        };
        let agent: String = row.get("agent_id");
        let session: String = row.get("session_id");
        self.acted(&agent, &session, command, outcome, why)
    }

    /// Append `message.acted` for `command` into `session`, once.
    fn acted(
        &self,
        agent: &str,
        session: &str,
        command: i64,
        outcome: &str,
        why: Option<&str>,
    ) -> Result<AckRecord, StoreError> {
        let invalid = |error: spawn_core::id::IdError| StoreError::Corrupt(error.to_string());
        let appended = self.append(NewEvent {
            agent_id: AgentId::new(agent).map_err(invalid)?,
            session_id: SessionId::new(session).map_err(invalid)?,
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some(format!("{command}:acted")),
            kind: EventKind::MessageActed {
                command_id: CommandId(u64::try_from(command).unwrap_or_default()),
                outcome: outcome.to_string(),
                why: why.map(str::to_string),
            },
        })?;
        Ok(if appended.inserted {
            AckRecord::Recorded {
                session_id: session.to_string(),
            }
        } else {
            AckRecord::Answered
        })
    }
}

/// The detail of a recorded answer, as `message.acted` holds it.
pub fn acted_detail(outcome: &str, why: Option<&str>) -> Value {
    serde_json::json!({"outcome": outcome, "why": why})
}
