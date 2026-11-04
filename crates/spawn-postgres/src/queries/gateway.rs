//! SQL that goes through the grant-filtered readers and the
//! `agent_commands` gateway ([`crate::gateway`]), so a restricted login
//! sees and sends exactly what `spawn_grants` allows.

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

/// Submit as the database login. No RETURNING: that needs SELECT on the
/// view, which a restricted login does not hold.
/// `$1` agent, `$2` session, `$3` idempotency key, `$4` action.
pub(crate) const SUBMIT: &str =
    "insert into agent_commands (agent_id, session_id, idempotency_key, action)
                 values ($1, $2, $3, $4) returning id";
