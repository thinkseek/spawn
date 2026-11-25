//! Missions, tasks and residents (docs/fleet-architecture.md §3.2), and
//! the integrator's lock (§10.3).
//!
//! Every decision goes through the checked SQL functions, so the supervisor's
//! restricted login and the owner's CLI share one set of rules. The
//! dispatcher's delivery bookkeeping (delivered, reported) runs as the owner.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// Free text a SQL function writes into a control record from its
/// parameters (objective 171: a mission's text, a brief, a report, a note,
/// a reason, a charter, a summary), masked before SQL sees it: 170's mask
/// ([`crate::control_mask`]) covers only the records the Rust side builds
/// whole. The SQL compares what it is given, so its own comparisons (the
/// brief's md5 index, the interventions join) see the masked text on both
/// sides.
pub(crate) fn masked(text: &str) -> String {
    spawn_core::redact::redact(text)
}

/// `value` with every string in it masked ([`masked`]): a re-issue's
/// attempts and an unfinished report's markers.
pub(crate) fn masked_strings(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(masked(text)),
        Value::Array(items) => Value::Array(items.iter().map(masked_strings).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), masked_strings(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `text` masked, kept within the store's `limit` (in characters when
/// `chars`, else in bytes) when it was within it: masking can lengthen a
/// short secret ("PWD=x" becomes "PWD=[redacted]"), and the store must not
/// refuse what it took before. The cut falls on a char boundary. Text
/// already over its limit is not cut, so the store refuses it as before.
pub(crate) fn masked_within(text: &str, limit: usize, chars: bool) -> String {
    let size = |text: &str| {
        if chars {
            text.chars().count()
        } else {
            text.len()
        }
    };
    let mut out = masked(text);
    if size(text) > limit || size(&out) <= limit {
        return out;
    }
    let end = if chars {
        out.char_indices()
            .nth(limit)
            .map_or(out.len(), |(at, _)| at)
    } else {
        let mut end = limit;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        end
    };
    out.truncate(end);
    out
}

/// The store's limit on a hold's and a pause's reason, in characters.
const REASON_CHARS: usize = 500;
/// The store's limit on a push request's reason, in characters.
const PUSH_REASON_CHARS: usize = 2000;
/// The store's limit on a restart's and a reassign's reason, in bytes.
const RESTART_REASON_BYTES: usize = 2000;

/// One task the dispatcher has to look at, with what it needs to deliver it.
/// A closed ephemeral task stays for a day, and for as long as its worker
/// still runs: its worker is stopped, and it is managed until then. A
/// re-issue names the task it re-issues (`reissue_of`) and that task's
/// spent attempts, as its `task.cancelled` record keeps them
/// (`reissued_attempts`), for the escalation if the re-issue is spent too.
pub(crate) const DELIVERY_TASKS: &str = r"
select coalesce(jsonb_agg(row_to_json(t) order by t.id), '[]'::jsonb)
from (
  select k.id, k.mission_id, k.kind, k.agent_id, k.session_id, k.brief, k.state,
         k.revisions, k.review_note, k.delivered_at_ms, k.model, k.reissue_of,
         k.reassign_of, k.pinned,
         (select o.session_id from mission_tasks o where o.id = k.reassign_of)
           as reassign_from_session,
         (select n.id from mission_tasks n where n.reassign_of = k.id order by n.id limit 1)
           as reassigned_as,
         (select n.session_id from mission_tasks n where n.reassign_of = k.id order by n.id limit 1)
           as reassigned_to_session,
         -- A delivered task's latest restart (`task.restarted`): the
         -- dispatcher stops the execution it names and resumes the session.
         case when k.state = 'delivered' then
           (select jsonb_build_object(
                     'n', (e.kind -> 'detail' -> 'record' ->> 'n')::bigint,
                     'execution_id', e.kind -> 'detail' -> 'record' ->> 'execution_id',
                     'by', e.kind -> 'detail' -> 'record' ->> 'by',
                     'reason', e.kind -> 'detail' -> 'record' ->> 'reason')
              from spawn_events e
             where e.session_id = 'session_control' and e.event_name = 'control.recorded'
               and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.restarted'
               and e.kind -> 'detail' -> 'record' ->> 'task' = k.id::text
             order by e.session_seq desc limit 1)
         end as restart,
         case when k.reissue_of is not null then
           (select e.kind -> 'detail' -> 'record' -> 'attempts' from spawn_events e
             where e.session_id = 'session_control' and e.event_name = 'control.recorded'
               and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.cancelled'
               and e.kind -> 'detail' -> 'record' ->> 'id' = k.reissue_of::text
             order by e.session_seq desc limit 1)
         end as reissued_attempts,
         m.text as mission_text,
         m.gate is not null as gated,
         m.state as mission_state,
         m.halted is not null as mission_halted,
         r.charter, r.name as resident
    from mission_tasks k
    join mission_missions m on m.id = k.mission_id
    left join mission_residents r on r.agent_id = k.agent_id
   where k.state in ('assigned', 'delivered', 'reported', 'revise', 'landing')
      or (k.state = 'accepted' and m.gate is not null)
      or (k.state in ('accepted', 'cancelled', 'landed') and k.kind = 'ephemeral'
          and (k.updated_at_ms > spawn_now_ms() - 86400000
               or exists (select 1 from agent_status s
                           where s.session_id = k.session_id and s.live)))
) t";

/// The latest `source.moved` record (a deploy, `spawn mission moved`), as
/// its event's id and time on the database's clock, or null: after it, the
/// dispatcher tries every spent delivery once more.
pub(crate) const LATEST_SOURCE_MOVED: &str = r"
select coalesce((select jsonb_build_object('id', e.id, 'at_ms', e.recorded_at_ms)
                   from spawn_events e
                  where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                    and e.kind -> 'detail' -> 'record' ->> 'type' = 'source.moved'
                  order by e.session_seq desc limit 1), 'null'::jsonb)";

/// Where the latest `source.moved` record of source `$1` moved its main
/// (`to`): the owner's latest landing on it, which its deploy follows; null
/// when the owner never moved it. Read through the commit map (objective
/// 173): a move recorded before a history rewrite names its commit now.
pub(crate) const SOURCE_OWNER_LANDED: &str = r"
select spawn_commit_now(e.kind #>> '{detail,record,to}')
  from spawn_events e
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'source.moved'
   and e.kind #>> '{detail,record,source}' = $1
 order by e.session_seq desc limit 1";

/// The owner's landing requests (`source.push_requested`, `spawn mission
/// push`) that have no outcome yet, as two CTEs to follow a `with` (or a
/// comma): `owner`, the control records of the requests and their outcomes,
/// read through the `spawn_events_source_push` index; and `pending`, the
/// requests (`id`, `session_seq`, `recorded_at_ms`, the record `r`) that no
/// outcome (a `source.moved` or a `source.push_refused`) names by its
/// `request`. One text, so the integrator's queue ([`SOURCE_PUSHES_PENDING`])
/// and what deploy-on-landing waits for ([`MAIN_LANDING`]) are one notion of
/// pending. A `source.push_no_verdict` is no outcome: its request stays.
macro_rules! owner_requests_pending {
    () => {
        r"owner as (
  select e.id, e.session_seq, e.recorded_at_ms, e.kind -> 'detail' -> 'record' as r
    from spawn_events e
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type'
         in ('source.push_requested', 'source.push_refused', 'source.moved')
), pending as (
  select q.id, q.session_seq, q.recorded_at_ms, q.r
    from owner q
   where q.r ->> 'type' = 'source.push_requested'
     and not exists (select 1 from owner o
                      where o.r ->> 'type' in ('source.push_refused', 'source.moved')
                        and o.r ->> 'request' = q.id::text)
)"
    };
}

/// What deploy/on-landing.sh decides on (#54), for source `$1` whose main
/// is `$2`, the live controller's commit `$3` (null: unknown):
/// - `landing`: the newest gate-accepted landing of exactly `$2` on the
///   source, `{record, event, at_ms, task, request}`, or null. Gate-accepted
///   is a `task.landed` of a task of a mission on the source (the integrator
///   writes it only after the task's gate accepted it) or a `source.moved`
///   on the source with `verdict` accepted (an owner's gated push). A
///   deploy's own `source.moved` names no verdict and is no such landing.
/// - `first_undeployed_ms`: when the first gate-accepted landing after the
///   one that moved main to `$3` was recorded (any `task.landed` or
///   `source.moved` to `$3` is that one), or null when `$3` names none;
/// - `hold`: the source's hold, `{hold, by, reason, main, expires_ms}`, or
///   null: a row is a hold until a record ends it, past its bound too;
/// - `last_deploy`: the newest `deploy.finished`, `{ok, head, by, event,
///   at_ms}`, or null;
/// - `owner_requests`: the owner's landing requests on the source
///   (`source.push_requested`) with no outcome yet, as
///   [`owner_requests_pending!`] reads them, oldest first, each `{request,
///   commit, at_ms}` (`[]` when none): each waits for its judging or is in
///   it, holds nothing on the source until it is accepted, and a deploy
///   would restart its judging (S23). deploy/on-landing.sh waits for the
///   ones whose commit main does not hold already;
/// - `judging`: how many tasks of the source's gated missions are reported,
///   accepted or landing, i.e. between their report and their landing: each
///   may land within a gate's time. deploy/on-landing.sh keeps its quiet
///   spell only while one is (a burst can still come); at 0 no landing can
///   follow sooner than a report and a gate, so it deploys at once. Only
///   the gate's inputs and what is past them count (objective 168): a
///   mission without a gate never lands a task through the integrator (its
///   `accepted` is final, kept for good), and a gated mission that accepts
///   by review sends a task to its gate only once a person accepted it, so
///   its `reported` task waits for a person, not a gate. Counting either
///   would hold the quiet spell for ever;
/// - `now_ms`: the database's clock, which every `at_ms` is on.
///
/// Every recorded commit (a move's `to`, the anchor `$3`, a request's
/// commit) is read through the commit map (objective 173); a request for a
/// commit the rewrite dropped keeps its recorded one.
pub(crate) const MAIN_LANDING: &str = concat!(
    r#"
with on_source as (
  select e.session_seq as seq, e.id as event, e.recorded_at_ms as at_ms,
         e.kind #>> '{detail,record,type}' as type,
         spawn_commit_now(e.kind #>> '{detail,record,to}') as moved_to,
         e.kind #>> '{detail,record,type}' = 'task.landed'
           or e.kind #>> '{detail,record,verdict}' = 'accepted' as accepted,
         case when e.kind #>> '{detail,record,type}' = 'task.landed'
              then e.kind #> '{detail,record,id}' end as task,
         e.kind #> '{detail,record,request}' as request
    from spawn_events e
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type' in ('task.landed', 'source.moved',
                                                       'deploy.finished')
     and e.kind -> 'detail' -> 'record' ->> 'type' <> 'deploy.finished'
     and case when e.kind #>> '{detail,record,type}' = 'source.moved'
              then e.kind #>> '{detail,record,source}' = $1
              else jsonb_typeof(e.kind #> '{detail,record,id}') = 'number'
                   and exists (select 1
                                 from mission_tasks t
                                 join mission_missions m on m.id = t.mission_id
                                where t.id = (e.kind #>> '{detail,record,id}')::numeric::bigint
                                  and m.sources ->> 0 = $1)
         end
), anchor as (
  select max(seq) as seq from on_source where moved_to = spawn_commit_now($3::text)
), "#,
    owner_requests_pending!(),
    r#"
select jsonb_build_object(
  'now_ms', spawn_now_ms(),
  'landing', (select jsonb_build_object('record', o.type, 'event', o.event, 'at_ms', o.at_ms,
                                        'task', o.task, 'request', o.request)
                from on_source o
               where o.moved_to = $2 and o.accepted
               order by o.seq desc limit 1),
  'first_undeployed_ms', (select min(o.at_ms)
                            from on_source o, anchor a
                           where o.seq > a.seq and o.accepted),
  'last_deploy', (select jsonb_build_object('ok', e.kind #> '{detail,record,ok}',
                                            'head', e.kind #>> '{detail,record,head}',
                                            'by', e.kind #>> '{detail,record,by}',
                                            'event', e.id, 'at_ms', e.recorded_at_ms)
                    from spawn_events e
                   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                     and e.kind -> 'detail' -> 'record' ->> 'type' in ('task.landed',
                                                                       'source.moved',
                                                                       'deploy.finished')
                     and e.kind -> 'detail' -> 'record' ->> 'type' = 'deploy.finished'
                   order by e.session_seq desc limit 1),
  'hold', (select jsonb_build_object('hold', h.hold, 'by', h.by, 'reason', h.reason,
                                     'main', h.main, 'expires_ms', h.expires_ms)
             from mission_source_holds h
            where h.source = $1),
  'owner_requests', (select coalesce(jsonb_agg(jsonb_build_object('request', q.id,
                                                                  'commit',
                                                                  coalesce(spawn_commit_now(q.r ->> 'commit'),
                                                                           q.r ->> 'commit'),
                                                                  'at_ms', q.recorded_at_ms)
                                               order by q.session_seq), '[]'::jsonb)
                       from pending q
                      where q.r ->> 'source' = $1),
  'judging', (select count(*)
                from mission_tasks t
                join mission_missions m on m.id = t.mission_id
               where t.state in ('reported', 'accepted', 'landing')
                 and m.gate is not null
                 and (t.state <> 'reported' or m.accept = 'gate')
                 and m.sources ->> 0 = $1))"#
);

/// The owner's pauses as the dispatcher acts on them (objective 099): every
/// mission not closed that is paused, or was unpaused since its last pause,
/// `[{id, pause_event, unpause_event, unpause_at_ms, sessions}]`. `sessions`
/// are the workers the pause holds: its resident (approved, the one its
/// objectives name) and the worker of each of its delivered tasks, each
/// `{agent_id, session_id, started_at_ms, final, resumed}`: when its latest
/// execution started, whether the recovery recorded that execution's end
/// on the `final` path (a Stop's end: nothing else revives it), and whether
/// the dispatcher already sent the unpause's resume
/// (`unpause-<mission>-<unpause_event>-<session>`, however long ago).
pub(crate) const MISSION_PAUSES: &str = r"
with paused as materialized (
  select m.id, m.pause_event, m.unpause_event, m.unpause_at_ms, m.objectives ->> 'resident' as resident
    from mission_missions m
   where m.state in ('open', 'active')
     and (m.pause_event is not null or m.unpause_event is not null)
),
workers as (
  select p.id, r.agent_id, r.session_id
    from paused p join mission_residents r on r.name = p.resident and r.state = 'approved'
  union
  select p.id, k.agent_id, k.session_id
    from paused p join mission_tasks k on k.mission_id = p.id and k.state = 'delivered'
)
select coalesce(jsonb_agg(jsonb_build_object(
         'id', p.id, 'pause_event', p.pause_event, 'unpause_event', p.unpause_event,
         'unpause_at_ms', p.unpause_at_ms,
         'sessions', coalesce((
           select jsonb_agg(jsonb_build_object(
                    'agent_id', w.agent_id, 'session_id', w.session_id,
                    'started_at_ms', last.started_at_ms,
                    'final', exists (
                      select 1 from spawn_events r
                       where r.session_id = 'session_control' and r.event_name = 'control.recorded'
                         and r.kind -> 'detail' -> 'record' ->> 'type' = 'execution.interrupted'
                         and r.kind -> 'detail' -> 'record' ->> 'session_id' = w.session_id
                         and r.kind -> 'detail' -> 'record' ->> 'execution_id' = last.execution_id
                         and r.kind -> 'detail' -> 'record' ->> 'path' = 'final'),
                    'resumed', p.unpause_event is not null and exists (
                      select 1 from spawn_commands c
                       where c.principal = 'spawn-dispatch' and c.session_id = w.session_id
                         and c.idempotency_key = format('unpause-%s-%s-%s', p.id, p.unpause_event,
                                                        w.session_id)))
                  order by w.session_id)
             from workers w
             left join lateral (
               select x.execution_id, x.started_at_ms from agent_executions x
                where x.session_id = w.session_id
                order by x.started_at_ms desc, x.execution_id desc limit 1) last on true
            where w.id = p.id), '[]'::jsonb))
       order by p.id), '[]'::jsonb)
  from paused p";

/// Residents (not retired) whose latest delivered task is cancelled: the
/// dispatcher stops such a worker, sets the task's work aside and tells it
/// (docs/fleet-architecture.md, "When a task closes"). The latest delivery
/// is the session's task delivered last (`delivered_at_ms`, then the id); a
/// task never delivered, or one delivered before another, is not here. Each
/// row carries:
///
/// - `start`: the checkpoint the task started from, the agent's latest
///   `artifact.committed` before the task's first delivery was accepted
///   (`command.accepted` of the command its first `task.delivered` names),
///   or a merge of main recorded before that `task.delivered` (the one a
///   start or resume makes); `null` when the agent has none;
/// - `set_aside`: the commit of the set-aside already recorded for the task
///   (`source_event_id` `set-aside:<task>`), if any;
/// - `told`: a notice of the cancel (`cancel-notice-<task>-<n>`) command is
///   recorded, however long ago; its delivery outcome does not block the session;
/// - `closed_by` and `reason`: the task's last control record (`task.aborted`
///   carries the integrator's reason); `yielded` when that record yielded
///   its objective's slot, and `yield_reason`, an abort's reason for it.
pub(crate) const CANCELLED_DELIVERIES: &str = r"
with latest as materialized (
  -- At most one per resident: its latest delivery, when that is cancelled.
  -- The log is read below for these alone.
  select k.id, k.mission_id, k.agent_id, k.session_id, r.name
    from mission_tasks k
    join mission_residents r on r.agent_id = k.agent_id and r.state <> 'retired'
   where k.state = 'cancelled' and k.kind = 'resident' and k.delivered_at_ms is not null
     and not exists (select 1 from mission_tasks later
                      where later.session_id = k.session_id and later.id <> k.id
                        and later.delivered_at_ms is not null
                        and (later.delivered_at_ms, later.id) > (k.delivered_at_ms, k.id))
)
select coalesce(jsonb_agg(row_to_json(t) order by t.id), '[]'::jsonb)
from (
  select k.id, k.mission_id, k.agent_id, k.session_id, k.name as resident,
         -- Through the commit map (objective 177): an id the map cannot
         -- translate (dropped) reads as itself, which the set-aside refuses.
         coalesce(spawn_commit_now(start.commit), start.commit) as start,
         coalesce(spawn_commit_now(aside.commit), aside.commit) as set_aside,
         exists (select 1 from spawn_commands c
                  where c.session_id = k.session_id and c.principal = 'spawn-dispatch'
                    and c.idempotency_key like 'cancel-notice-' || k.id || '-%') as told,
         last.type as closed_by, last.reason, last.yield_reason,
         coalesce(last.yielded, false) as yielded,
         (select n.id from mission_tasks n where n.reassign_of = k.id order by n.id limit 1)
           as reassigned_as,
         (select n.session_id from mission_tasks n where n.reassign_of = k.id order by n.id limit 1)
           as reassigned_to_session
    from latest k
    left join lateral (
      select e.id, (e.kind -> 'detail' -> 'record' ->> 'command_id')::bigint as command_id
        from spawn_events e
       where e.session_id = 'session_control' and e.event_name = 'control.recorded'
         and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.delivered'
         and e.kind -> 'detail' -> 'record' ->> 'id' = k.id::text
       order by e.session_seq limit 1) first on true
    left join lateral (
      select e.id from spawn_events e
       where e.session_id = k.session_id and e.event_name = 'command.accepted'
         and (e.kind -> 'detail' ->> 'command_id')::bigint = first.command_id
       order by e.id limit 1) accepted on true
    left join lateral (
      select e.kind -> 'detail' -> 'artifact' ->> 'commit' as commit
        from spawn_events e
       where e.session_id = k.session_id and e.event_name = 'artifact.committed'
         and coalesce(e.kind -> 'detail' ->> 'reason', '') <> 'set_aside'
         and (e.id < coalesce(accepted.id, first.id)
              or (e.kind -> 'detail' ->> 'reason' = 'base_merged' and e.id < first.id))
       order by e.id desc limit 1) start on true
    left join lateral (
      select e.kind -> 'detail' -> 'artifact' ->> 'commit' as commit
        from spawn_events e
       where e.session_id = k.session_id and e.event_name = 'artifact.committed'
         and e.source_event_id = 'set-aside:' || k.id
       order by e.id desc limit 1) aside on true
    left join lateral (
      select e.kind -> 'detail' -> 'record' ->> 'type' as type,
             e.kind -> 'detail' -> 'record' ->> 'reason' as reason,
             e.kind -> 'detail' -> 'record' ->> 'yield_reason' as yield_reason,
             coalesce(e.kind -> 'detail' -> 'record' ->> 'yielded', '') = 'true' as yielded
        from spawn_events e
       where e.session_id = 'session_control' and e.event_name = 'control.recorded'
         and e.kind -> 'detail' -> 'record' ->> 'id' = k.id::text
         and e.kind -> 'detail' -> 'record' ->> 'type' like 'task.%'
       order by e.session_seq desc limit 1) last on true
   where first.id is not null
) t";

/// What the dispatcher measures stalls from (`dispatch/stall.rs`,
/// docs/fleet-architecture.md §3), all from the log and on the database's
/// clock: `now_ms` is `spawn_now_ms()`, and every `age_ms` is it minus the
/// record's `recorded_at_ms`.
///
/// - `tasks`: every open task (the rows [`DELIVERY_TASKS`] reads, but a
///   closed ephemeral task only while its worker runs) with `record`, the
///   latest control record that put it in its state (`task.assigned`; the
///   review or verdict that sent it back (`rejected`, `conflict` or
///   `incompatible`); `task.delivered`; `task.reported`;
///   the review that accepted it; the gate's `accepted`; the record that
///   closed it), `stale`, the latest `stale` verdict on it, which moves
///   no step, and, for a landing, `held` (its mission's source has an
///   active owner's hold: it waits behind it) and `hold_end` (the latest
///   `source.released` or `source.hold_expired` on its source);
/// - `verdicts`: per gated mission with a task waiting for the gate, the
///   latest verdict on any of its tasks that is not `stale`;
/// - `sessions`: per session with a delivered task, its latest fact (a
///   record that is neither the Observer's nor storage's, nor the Bridge's
///   `waiting` step of an input, which says nothing the worker did, as
///   [`DISPATCH_FACTS`](crate::fleet::DISPATCH_FACTS)'s `evidence_seq`);
///   whether its worker waits on a background job of its own is read from
///   that query's `jobs`, never from the screen;
/// - `halted`: every halted mission with open tasks, its latest
///   `mission.halted` record and those tasks.
pub(crate) const STALL_FACTS: &str = r"
with now as (select spawn_now_ms() as ms),
open as (
  select k.id, k.mission_id, k.kind, k.session_id, k.state, k.revisions,
         m.gate is not null as gated, coalesce(m.accept, 'review') as accept,
         m.halted is not null as halted, m.sources ->> 0 as source,
         exists (select 1 from mission_source_holds h where h.source = m.sources ->> 0) as held
    from mission_tasks k
    join mission_missions m on m.id = k.mission_id
   where k.state in ('assigned', 'delivered', 'reported', 'revise', 'landing')
      or (k.state = 'accepted' and m.gate is not null)
      or (k.state in ('accepted', 'cancelled', 'landed') and k.kind = 'ephemeral'
          and exists (select 1 from agent_status s where s.session_id = k.session_id and s.live))
)
select jsonb_build_object(
  'now_ms', (select ms from now),
  'tasks', coalesce((select jsonb_agg(jsonb_build_object(
      'id', o.id, 'mission_id', o.mission_id, 'kind', o.kind, 'session_id', o.session_id,
      'state', o.state, 'revisions', o.revisions, 'gated', o.gated, 'accept', o.accept,
      'halted', o.halted, 'record', rec.record, 'stale', st.stale,
      'held', o.state = 'landing' and o.held, 'hold_end', he.record) order by o.id)
    from open o
    left join lateral (
      select jsonb_build_object('id', e.id, 'type', x.r ->> 'type', 'at_ms', e.recorded_at_ms,
                                'age_ms', (select ms from now) - e.recorded_at_ms) as record
        from spawn_events e
       cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
       where e.session_id = 'session_control' and e.event_name = 'control.recorded'
         and e.kind -> 'detail' -> 'record' ->> 'id' = o.id::text
         and case o.state
               when 'assigned' then x.r ->> 'type' = 'task.assigned'
               when 'revise' then (x.r ->> 'type' = 'task.reviewed' and x.r ->> 'accepted' = 'false')
                               or (x.r ->> 'type' = 'task.gated'
                                   and x.r ->> 'verdict' in ('rejected', 'conflict', 'incompatible'))
               when 'delivered' then x.r ->> 'type' = 'task.delivered'
               when 'reported' then x.r ->> 'type' = 'task.reported'
               when 'accepted' then x.r ->> 'type' = 'task.reviewed' and x.r ->> 'accepted' = 'true'
               when 'landing' then x.r ->> 'type' = 'task.gated' and x.r ->> 'verdict' = 'accepted'
               when 'landed' then x.r ->> 'type' = 'task.landed'
               when 'cancelled' then x.r ->> 'type' in ('task.cancelled', 'task.aborted')
               else false
             end
       order by e.session_seq desc limit 1) rec on true
    left join lateral (
      select jsonb_build_object('id', e.id, 'reason', e.kind -> 'detail' -> 'record' ->> 'reason',
                                'at_ms', e.recorded_at_ms,
                                'age_ms', (select ms from now) - e.recorded_at_ms) as stale
        from spawn_events e
       where e.session_id = 'session_control' and e.event_name = 'control.recorded'
         and e.kind -> 'detail' -> 'record' ->> 'id' = o.id::text
         and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
         and e.kind -> 'detail' -> 'record' ->> 'verdict' = 'stale'
       order by e.session_seq desc limit 1) st on true
    left join lateral (
      select jsonb_build_object('id', e.id, 'type', e.kind -> 'detail' -> 'record' ->> 'type',
                                'at_ms', e.recorded_at_ms,
                                'age_ms', (select ms from now) - e.recorded_at_ms) as record
        from spawn_events e
       where o.state = 'landing'
         and e.session_id = 'session_control' and e.event_name = 'control.recorded'
         and e.kind -> 'detail' -> 'record' ->> 'type' in ('source.released', 'source.hold_expired')
         and e.kind -> 'detail' -> 'record' ->> 'source' = o.source
       order by e.session_seq desc limit 1) he on true), '[]'::jsonb),
  'verdicts', coalesce((select jsonb_agg(v order by v.mission_id) from (
      select distinct on (k.mission_id) k.mission_id, e.id,
             e.kind -> 'detail' -> 'record' ->> 'verdict' as verdict,
             e.kind -> 'detail' -> 'record' ->> 'id' as task,
             e.recorded_at_ms as at_ms, (select ms from now) - e.recorded_at_ms as age_ms
        from mission_tasks k
        join spawn_events e
          on e.session_id = 'session_control' and e.event_name = 'control.recorded'
         and e.kind -> 'detail' -> 'record' ->> 'id' = k.id::text
       where k.mission_id in (select mission_id from open
                               where gated and state in ('reported', 'accepted'))
         and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
         and e.kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale'
       order by k.mission_id, e.session_seq desc) v), '[]'::jsonb),
  'sessions', coalesce((select jsonb_agg(jsonb_build_object(
      'session_id', s.session_id, 'seq', f.session_seq, 'event_name', f.event_name,
      'at_ms', f.recorded_at_ms, 'age_ms', (select ms from now) - f.recorded_at_ms)
      order by s.session_id)
    from (select distinct session_id from open where state = 'delivered') s
    left join lateral (
      select e.session_seq, e.event_name, e.recorded_at_ms from spawn_events e
       where e.session_id = s.session_id and e.source <> 'observer'
         and e.event_name not in ('artifact.committed', 'artifact.branch_recreated',
                                'merge.conflicted', 'message.acted',
                                'archive.failed', 'archive.resumed')
         and coalesce(e.kind -> 'detail' ->> 'step', '') <> 'waiting'
       order by e.session_seq desc limit 1) f on true), '[]'::jsonb),
  'halted', coalesce((select jsonb_agg(jsonb_build_object(
      'mission_id', m.id, 'reason', m.halted, 'record', h.record, 'tasks', t.ids) order by m.id)
    from mission_missions m
    join lateral (
      select jsonb_agg(o.id order by o.id) as ids from open o
       where o.mission_id = m.id and o.state not in ('cancelled', 'landed')
         and not (o.state = 'accepted' and not o.gated)) t on t.ids is not null
    left join lateral (
      select jsonb_build_object('id', e.id, 'type', 'mission.halted', 'at_ms', e.recorded_at_ms,
                                'age_ms', (select ms from now) - e.recorded_at_ms) as record
        from spawn_events e
       where e.session_id = 'session_control' and e.event_name = 'control.recorded'
         and e.kind -> 'detail' -> 'record' ->> 'id' = m.id::text
         and e.kind -> 'detail' -> 'record' ->> 'type' = 'mission.halted'
       order by e.session_seq desc limit 1) h on true
   where m.halted is not null), '[]'::jsonb)
)";

/// What the supervisor should hear about, beyond worker states: open
/// missions, reports it reviews (not those of a mission its gate alone
/// accepts: the gate judges them, and `spawn_task_review` refuses them; a
/// report carries its `revisions`, which the review names), residents
/// approved, and human commands to workers, once finished, with their
/// outcome. An approval or a human command stays news until a digest that
/// carried it reached the supervisor (its tag,
/// [`DISPATCH_FACTS`](crate::fleet::DISPATCH_FACTS)'s `told`), however long
/// that takes; the dispatcher filters what was told by the same tags.
pub(crate) const MISSION_NEWS: &str = r"
with told as (
  select distinct tag
    from spawn_commands d
   cross join lateral jsonb_array_elements_text(
           case when jsonb_typeof(d.action -> 'tags') = 'array'
                then d.action -> 'tags' else '[]'::jsonb end) tag
   where d.session_id = 'session_supervisor' and d.principal = 'spawn-dispatch' and d.succeeded
)
select jsonb_build_object(
  'missions', coalesce((select jsonb_agg(jsonb_build_object(
                 'id', m.id, 'text', left(m.text, 400), 'origin', m.origin) order by m.id)
               from mission_missions m where m.state = 'open'), '[]'::jsonb),
  'reports', coalesce((select jsonb_agg(jsonb_build_object(
                 'id', k.id, 'mission_id', k.mission_id, 'kind', k.kind,
                 'session_id', k.session_id, 'revisions', k.revisions,
                 'report', left(k.report, 600)) order by k.id)
               from mission_tasks k join mission_missions m on m.id = k.mission_id
              where k.state = 'reported'
                and not (m.gate is not null and m.accept = 'gate')), '[]'::jsonb),
  'approved', coalesce((select jsonb_agg(jsonb_build_object('name', r.name) order by r.name)
               from mission_residents r
              where r.state = 'approved'
                and not exists (select 1 from told
                                 where tag = r.session_id || ' approved-' || r.name)), '[]'::jsonb),
  'human', coalesce((select jsonb_agg(jsonb_build_object(
                 'command_id', c.id, 'session_id', c.session_id,
                 'action', c.action ->> 'type',
                 'text', left(coalesce(c.action ->> 'text', c.action ->> 'prompt', c.action ->> 'type'), 200),
                 'succeeded', c.succeeded, 'error', left(c.error, 300))
                 order by c.id)
               from spawn_commands c
              where c.principal = 'human'
                and (c.agent_id like 'agent\_worker\_%' or c.agent_id like 'agent\_resident\_%')
                and c.state = 'finished'
                and not exists (select 1 from told
                                 where tag = c.session_id || ' human-' || c.id)), '[]'::jsonb)
)";

/// Every active owner's hold on a mission source, by source:
/// `[{source, hold, main, by, reason, held_at_ms, expires_ms, waiting}]`,
/// `waiting` the tasks waiting behind it (`landing`, `held_by` the hold),
/// oldest first. Read from the fold alone, so the supervisor's login reads
/// it too.
macro_rules! active_holds {
    () => {
        r"coalesce((select jsonb_agg(jsonb_build_object(
      'source', h.source, 'hold', h.hold, 'main', h.main, 'by', h.by, 'reason', h.reason,
      'held_at_ms', h.held_at_ms, 'expires_ms', h.expires_ms,
      'waiting', coalesce((select jsonb_agg(k.id order by k.id) from mission_tasks k
                            where k.state = 'landing' and k.held_by = h.hold), '[]'::jsonb))
      order by h.source)
    from mission_source_holds h), '[]'::jsonb)"
    };
}

/// The board of missions with their tasks, newest first; each task carries
/// its stages ([`PostgresStore::task_stages`]) and each mission the `hold`
/// on its source (the active hold's event id, or null) and the `halt` it
/// is in (`{event, reason, at_ms, gone}`: its `mission.halted` record's
/// event id and time, and the scenarios a halt on the floor names gone, else
/// null; null while it is not halted) and the objectives `retired` by hand
/// (objective 210: `[{objective, by, reason, caller, at_ms}]`, oldest first,
/// `[]` when none, read from `mission_retired_objectives`, never from
/// `spawn_events`, so the supervisor's login reads it); `holds` is every
/// active hold ([`SOURCE_HOLDS`]).
pub(crate) const MISSION_BOARD: &str = concat!(
    r"
select jsonb_build_object(
  'holds', ",
    active_holds!(),
    r",
  'missions', coalesce((select jsonb_agg(row_to_json(x) order by x.id desc) from (
      select m.id, m.text, m.origin, m.sources, m.state, m.summary,
             m.gate, m.accept, m.budget, m.baseline, m.halted, m.objectives,
             case when m.halted is not null then jsonb_build_object(
                 'event', m.halt_event, 'reason', m.halted, 'at_ms', m.halt_at_ms,
                 'gone', m.halt_gone) end as halt,
             case when m.pause_event is not null then jsonb_build_object(
                 'at_ms', m.pause_at_ms, 'event_id', m.pause_event,
                 'reason', m.pause_reason) end as paused,
             (select h.hold from mission_source_holds h where h.source = m.sources ->> 0) as hold,
             -- The objectives retired by hand (objective 210), oldest first.
             coalesce((select jsonb_agg(jsonb_build_object(
                 'objective', o.objective, 'by', o.by, 'reason', o.reason,
                 'caller', o.caller, 'at_ms', o.at_ms) order by o.at_ms, o.event)
               from mission_retired_objectives o where o.mission_id = m.id), '[]'::jsonb)
               as retired,
             m.created_at_ms, m.updated_at_ms,
             coalesce((select jsonb_agg(jsonb_build_object(
                 'id', k.id, 'kind', k.kind, 'agent_id', k.agent_id,
                 'session_id', k.session_id, 'brief', left(k.brief, 600),
                 'state', k.state, 'revisions', k.revisions,
                 'review_note', k.review_note, 'report', k.report,
                 'candidate', k.candidate, 'objective', k.objective, 'metric', k.metric,
                 'model', k.model, 'stages', spawn_task_stages(k.id),
                 'held_by', k.held_by,
                 -- Parked by the load-sensitive bound: its pushed commit
                 -- and every scenario found load-sensitive on it (columns
                 -- of the row already read; null for any other task).
                 'parked', case when k.gate_parked_pushed is not null then
                     jsonb_build_object('pushed', k.gate_parked_pushed,
                                        'scenarios', k.gate_load_sensitive_scenarios) end,
                 'created_by', k.created_by, 'created_at_ms', k.created_at_ms,
                 'updated_at_ms', k.updated_at_ms) order by k.id)
               from mission_tasks k where k.mission_id = m.id), '[]'::jsonb) as tasks
        from mission_missions m
       order by m.id desc limit $1) x), '[]'::jsonb),
  'residents', coalesce((select jsonb_agg(row_to_json(r) order by r.name) from (
      select name, agent_id, session_id, charter, state, proposed_by, approved_by,
             created_at_ms, updated_at_ms from mission_residents) r), '[]'::jsonb)
)"
);

/// One mission by id, with the same explicit fields and task stages as
/// [`MISSION_BOARD`]. Unlike the board this has no recency window: an old or
/// closed mission remains directly inspectable. `$1` is the mission id.
pub(crate) const MISSION_BY_ID: &str = r"
select row_to_json(x) from (
      select m.id, m.text, m.origin, m.sources, m.state, m.summary,
             m.gate, m.accept, m.budget, m.baseline, m.halted, m.objectives,
             case when m.halted is not null then jsonb_build_object(
                 'event', m.halt_event, 'reason', m.halted, 'at_ms', m.halt_at_ms,
                 'gone', m.halt_gone) end as halt,
             case when m.pause_event is not null then jsonb_build_object(
                 'at_ms', m.pause_at_ms, 'event_id', m.pause_event,
                 'reason', m.pause_reason) end as paused,
             (select h.hold from mission_source_holds h where h.source = m.sources ->> 0) as hold,
             coalesce((select jsonb_agg(jsonb_build_object(
                 'objective', o.objective, 'by', o.by, 'reason', o.reason,
                 'caller', o.caller, 'at_ms', o.at_ms) order by o.at_ms, o.event)
               from mission_retired_objectives o where o.mission_id = m.id), '[]'::jsonb)
               as retired,
             m.created_at_ms, m.updated_at_ms,
             coalesce((select jsonb_agg(jsonb_build_object(
                 'id', k.id, 'kind', k.kind, 'agent_id', k.agent_id,
                 'session_id', k.session_id, 'brief', left(k.brief, 600),
                 'state', k.state, 'revisions', k.revisions,
                 'review_note', k.review_note, 'report', k.report,
                 'candidate', k.candidate, 'objective', k.objective, 'metric', k.metric,
                 'model', k.model, 'stages', spawn_task_stages(k.id),
                 'held_by', k.held_by,
                 'parked', case when k.gate_parked_pushed is not null then
                     jsonb_build_object('pushed', k.gate_parked_pushed,
                                        'scenarios', k.gate_load_sensitive_scenarios) end,
                 'created_by', k.created_by, 'created_at_ms', k.created_at_ms,
                 'updated_at_ms', k.updated_at_ms) order by k.id)
               from mission_tasks k where k.mission_id = m.id), '[]'::jsonb) as tasks
        from mission_missions m
       where m.id = $1
) x";

/// What the labels of the tasks `$1` (an array of ids) are made of, read
/// as [`MISSION_BOARD`] reads them (the mission's whole text, a brief's
/// first 600 characters), so a label is the one `/api/missions` gives:
/// `{missions: [{id, text, sources, gate, accept, tasks: [{id, kind,
/// agent_id, session_id, brief, state, objective}]}]}`, only the missions
/// of those tasks and those named in `$2` (an array of ids; `tasks` null
/// for one holding none of them), and only those tasks. A task or mission
/// the record does not hold is not there.
pub(crate) const TASK_LABEL_FACTS: &str = r"
select jsonb_build_object(
  'missions', coalesce((select jsonb_agg(jsonb_build_object(
      'id', m.id, 'text', m.text, 'sources', m.sources, 'state', m.state,
      'gate', m.gate, 'accept', m.accept,
      'tasks', (select jsonb_agg(jsonb_build_object(
          'id', k.id, 'kind', k.kind, 'agent_id', k.agent_id, 'session_id', k.session_id,
          'brief', left(k.brief, 600), 'state', k.state, 'objective', k.objective)
          order by k.id)
        from mission_tasks k where k.mission_id = m.id and k.id = any($1)))
      order by m.id desc)
    from mission_missions m
   where m.id = any($2)
      or exists (select 1 from mission_tasks k where k.mission_id = m.id and k.id = any($1))),
   '[]'::jsonb))";

/// What the dashboard's labels are made of (`spawn serve`, serve/labels.rs):
/// every mission with its text's start, the tasks a label can name (each
/// session's latest task, and every task still open: not landed or
/// cancelled, nor accepted outside a gated mission), the resident roster,
/// and the latest input of the supervisor and of each resident:
/// `{missions: [{id, text, sources, state, gate, accept, objectives, tasks:
/// [{id, kind, agent_id, session_id, brief, state, objective,
/// created_at_ms, updated_at_ms}]}], residents: [{name, agent_id,
/// session_id, state}], inputs: [{session_id, principal, text,
/// created_at_ms}]}`, newest mission first, tasks by id. Bounded by the
/// sessions and the open work, not by every task ever made; no stages,
/// reports or review notes.
pub(crate) const LABEL_FACTS: &str = r"
with named as (
  select distinct on (k.session_id) k.id
    from mission_tasks k
   where k.session_id is not null
   order by k.session_id, k.id desc
), shown as (
  select k.* from mission_tasks k join mission_missions m on m.id = k.mission_id
   where k.id in (select id from named)
      or k.state not in ('landed', 'cancelled', 'accepted')
      or (k.state = 'accepted' and m.gate is not null)
)
select jsonb_build_object(
  'missions', coalesce((select jsonb_agg(jsonb_build_object(
      'id', m.id, 'text', left(m.text, 400), 'sources', m.sources, 'state', m.state,
      'gate', m.gate, 'accept', m.accept, 'objectives', m.objectives,
      'tasks', coalesce((select jsonb_agg(jsonb_build_object(
          'id', k.id, 'kind', k.kind, 'agent_id', k.agent_id, 'session_id', k.session_id,
          'brief', left(k.brief, 600), 'state', k.state, 'objective', k.objective,
          'created_at_ms', k.created_at_ms, 'updated_at_ms', k.updated_at_ms) order by k.id)
        from shown k where k.mission_id = m.id), '[]'::jsonb)) order by m.id desc)
    from mission_missions m), '[]'::jsonb),
  'residents', coalesce((select jsonb_agg(jsonb_build_object(
      'name', r.name, 'agent_id', r.agent_id, 'session_id', r.session_id, 'state', r.state)
      order by r.name)
    from mission_residents r), '[]'::jsonb),
  'inputs', coalesce((select jsonb_agg(jsonb_build_object(
      'session_id', c.session_id, 'principal', c.principal,
      'text', left(coalesce(c.action ->> 'text', c.action ->> 'prompt', ''), 600),
      'created_at_ms', c.created_at_ms) order by c.session_id)
    from (select distinct on (d.session_id) d.*
            from spawn_commands d
           where d.session_id = 'session_supervisor'
              or d.session_id in (select session_id from mission_residents)
           order by d.session_id, d.id desc) c), '[]'::jsonb)
)";

/// Every active owner's hold ([`MISSION_BOARD`]'s `holds`) and the
/// database's clock: `{now_ms, holds}`. The integrator ends a hold whose
/// `expires_ms` that clock reached, by record; the attention view shows the
/// ones with tasks waiting.
pub(crate) const SOURCE_HOLDS: &str = concat!(
    r"
select jsonb_build_object('now_ms', spawn_now_ms(), 'holds', ",
    active_holds!(),
    ")"
);

/// The `source.hold_expired` records the supervisor was not told of yet,
/// oldest first: `{event_id, at_ms, source, hold, expires_ms, reason,
/// held_at_ms, bound_s, main, waiting: [{task, mission}]}`, the hold's own
/// fields read from its `source.held` record. Told is a record, as for
/// [`GATE_HOST_FAULT_ESCALATIONS`]: a digest that reached the supervisor
/// carrying the tag `missions source-hold-expired-<event_id>`, however long
/// ago.
pub(crate) const HOLD_EXPIRIES: &str = r"
with told as (
  select distinct tag
    from spawn_commands d
   cross join lateral jsonb_array_elements_text(
           case when jsonb_typeof(d.action -> 'tags') = 'array'
                then d.action -> 'tags' else '[]'::jsonb end) tag
   where d.session_id = 'session_supervisor' and d.principal = 'spawn-dispatch' and d.succeeded
)
select coalesce(jsonb_agg(jsonb_build_object(
         'event_id', e.id, 'at_ms', e.recorded_at_ms, 'source', x.r ->> 'source',
         'hold', x.r -> 'hold', 'expires_ms', x.r -> 'expires_ms',
         'reason', held.r ->> 'reason', 'held_at_ms', held.at_ms, 'bound_s', held.r -> 'bound_s',
         'main', held.r ->> 'main',
         'waiting', coalesce((
            select jsonb_agg(jsonb_build_object('task', w.id::bigint, 'mission', k.mission_id)
                             order by w.n)
              from jsonb_array_elements_text(
                     case when jsonb_typeof(x.r -> 'waiting') = 'array'
                          then x.r -> 'waiting' else '[]'::jsonb end) with ordinality w(id, n)
              left join mission_tasks k on k.id = w.id::bigint), '[]'::jsonb))
         order by e.session_seq), '[]'::jsonb)
  from spawn_events e
 cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
  left join lateral (
    select h.kind -> 'detail' -> 'record' as r, h.recorded_at_ms as at_ms
      from spawn_events h
     where h.id = (x.r ->> 'hold')::bigint and h.session_id = 'session_control') held on true
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' in ('source.released', 'source.hold_expired')
   and x.r ->> 'type' = 'source.hold_expired'
   and not exists (select 1 from told where tag = 'missions source-hold-expired-' || e.id)";

// Checked decisions, one SQL function each.

/// An objective mission's landed tasks: `[{objective, metric}]`, newest
/// first, then its objectives solved on the base and those retired by hand
/// (objective 210, `{objective, metric: null, retired: true}`: closed
/// whatever their kind; no other row carries `retired`). `$1` mission.
pub(crate) const OBJECTIVE_LANDINGS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object('objective', x.objective, 'metric', x.metric)
                          || case when x.retired then jsonb_build_object('retired', true)
                                  else '{}'::jsonb end
                          order by x.at_ms desc, x.id desc), '[]'::jsonb)
  from (
   select k.objective, k.metric, false as retired, k.updated_at_ms as at_ms, k.id
     from mission_tasks k
    where k.mission_id = $1 and k.state = 'landed' and k.objective is not null
   union all
   -- An objective solved on the base (its scenario passes at the base
   -- commit, so no task of it can change anything) is solved as a landing
   -- solves it: no task for it is assigned again.
   select (e.kind -> 'detail' -> 'record' ->> 'objective')::integer, null, false,
          e.recorded_at_ms, e.id
     from spawn_events e
    where e.session_id = 'session_control' and e.event_name = 'control.recorded'
      and e.kind -> 'detail' -> 'record' ->> 'type' = 'objective.solved'
      and e.kind -> 'detail' -> 'record' ->> 'id' = $1::text
   union all
   -- An objective retired by hand (objective 210): closed whatever its
   -- kind, never by a metric it has no value for.
   select o.objective, null, true, o.at_ms, o.event
     from mission_retired_objectives o
    where o.mission_id = $1) x";

/// The outcomes of an objective mission's tasks, oldest first, from the
/// control stream: `[{seq, type, task, objective, verdict, merge, measure, yielded}]`, the
/// gate verdicts, the aborts and the yields (a `task.cancelled` that names
/// `yielded`). What spawn-cli's `objectives::attempts` counts. `$1` mission.
/// Each task's records by the record-id index, the record taken out of
/// `kind` once per row (`offset 0` keeps the subquery whole): a gate
/// verdict's record carries its log, and reading a field of `kind` per
/// field and per task joined made it 180 ms on the live log (2026-11-08),
/// 5 ms so.
pub(crate) const OBJECTIVE_OUTCOMES: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
         'seq', o.seq, 'type', o.r ->> 'type', 'task', k.id, 'objective', k.objective,
         'verdict', o.r ->> 'verdict', 'merge', o.r ->> 'merge',
         'measure', o.r ->> 'measure', 'yielded', o.r -> 'yielded',
         'event_id', o.event_id, 'at_ms', o.at_ms,
         'continues', (o.r ->> 'continues')::bigint, 'chain', o.r -> 'chain',
         'branch', o.r ->> 'branch', 'commit', o.r ->> 'commit', 'diff', o.r ->> 'diff',
         'tool_calls', o.r -> 'tool_calls', 'budget', o.r -> 'budget',
         'reason', o.r ->> 'reason')
         order by o.seq), '[]'::jsonb)
  from mission_tasks k
 cross join lateral (
   select e.id as event_id, e.recorded_at_ms as at_ms, e.session_seq as seq,
          e.kind -> 'detail' -> 'record' as r
     from spawn_events e
    where e.session_id = 'session_control' and e.event_name = 'control.recorded'
      and e.kind -> 'detail' -> 'record' ->> 'id' = k.id::text
   offset 0) o
 where k.mission_id = $1 and k.objective is not null
   and (o.r ->> 'type' in ('task.gated', 'task.aborted', 'task.over_budget')
        or (o.r ->> 'type' = 'task.cancelled' and o.r ->> 'yielded' = 'true'))";

/// The yields not told to the supervisor yet: a `task.cancelled` of the
/// integrator's, or a `task.aborted` that was the objective's last failed
/// attempt, naming `yielded` (its `yield_reason` is the reason told), that
/// no digest which reached the supervisor carried the tag
/// `missions objective-yielded-<event_id>` of, however long ago. Read from
/// the cancelled objective tasks (`mission_tasks`), each one's records by
/// the record-id index with the record taken out once (`offset 0`), and the
/// supervisor's digests checked for the few yields found: not every control
/// record (16 ms and growing with the log; rev-b on 226fa4a).
pub(crate) const OBJECTIVE_YIELDS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
         'event_id', x.id, 'at_ms', x.at_ms, 'id', k.id,
         'mission', k.mission_id, 'objective', x.r -> 'objective',
         'failures', x.r -> 'failures', 'limit', x.r -> 'limit',
         'reason', coalesce(x.r ->> 'yield_reason', x.r ->> 'reason'))
         order by x.seq), '[]'::jsonb)
  from mission_tasks k
 cross join lateral (
   select e.id, e.recorded_at_ms as at_ms, e.session_seq as seq,
          e.kind -> 'detail' -> 'record' as r
     from spawn_events e
    where e.session_id = 'session_control' and e.event_name = 'control.recorded'
      and e.kind -> 'detail' -> 'record' ->> 'id' = k.id::text
   offset 0) x
 where k.state = 'cancelled' and k.objective is not null
   and x.r ->> 'type' in ('task.cancelled', 'task.aborted') and x.r ->> 'yielded' = 'true'
   and not exists (
     select 1 from spawn_commands d
      where d.session_id = 'session_supervisor' and d.principal = 'spawn-dispatch'
        and d.succeeded and jsonb_typeof(d.action -> 'tags') = 'array'
        and d.action -> 'tags' ? ('missions objective-yielded-' || x.id))";

/// The objectives that closed solved on the base and the supervisor was not
/// told of yet, oldest first: an `objective.solved` record of the
/// integrator's (its `solved_on_base`: the objective's scenario passes at a
/// base commit main already holds, so its task ends as a solved cancel and
/// no task for it is assigned again) that no digest which reached the
/// supervisor carried the tag `missions objective-solved-<event_id>` of.
/// Each names its event, time, mission and the record's own fields
/// (`objective`, `task`, `scenario`, `base`); the gate's `log` is left out.
pub(crate) const OBJECTIVE_SOLVED_NEWS: &str = r"
select coalesce(jsonb_agg(
         ((e.kind -> 'detail' -> 'record') - 'log'::text)
           || jsonb_build_object('event_id', e.id, 'at_ms', e.recorded_at_ms,
                                 'mission', (e.kind -> 'detail' -> 'record' ->> 'id')::bigint)
         order by e.session_seq), '[]'::jsonb)
  from spawn_events e
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'objective.solved'
   and not exists (
     select 1 from spawn_commands d
      where d.session_id = 'session_supervisor' and d.principal = 'spawn-dispatch'
        and d.succeeded and jsonb_typeof(d.action -> 'tags') = 'array'
        and d.action -> 'tags' ? ('missions objective-solved-' || e.id))";

/// Gate verdicts on one objective's tasks, newest first, from the control
/// stream: `[{task, verdict, reason, metric, at_ms}]`, `incompatible` (sent
/// back because main moved, not a rejection) among them. `$1` mission,
/// `$2` objective, `$3` limit.
pub(crate) const OBJECTIVE_HISTORY: &str = r"
select coalesce(jsonb_agg(z.h order by z.seq desc), '[]'::jsonb)
  from (select e.session_seq as seq,
               jsonb_build_object('task', (r ->> 'id')::bigint, 'verdict', r ->> 'verdict',
                                  'reason', r ->> 'reason', 'metric', r ->> 'metric',
                                  'at_ms', e.recorded_at_ms) as h
          from spawn_events e
         cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
         where e.session_id = 'session_control' and e.event_name = 'control.recorded'
           and r ->> 'type' = 'task.gated' and r ->> 'verdict' in ('accepted', 'rejected', 'incompatible', 'aborted')
           and (r ->> 'id')::bigint in (select id from mission_tasks
                                         where mission_id = $1 and objective = $2)
         order by e.session_seq desc
         limit $3) z";

/// The earlier tasks of one objective that ended without landing (cancelled
/// by a human, aborted over budget, or yielded: all `cancelled`), newest
/// first, with the work each left: `[{task, branch, commit, outcome,
/// reason, report}]`. `branch` and `commit` are the last checkpoint its
/// session recorded from its assignment until the session's next task (a
/// resident's), not a set-aside; null when it recorded none. `outcome` and
/// `reason` are its last gate verdict, abort or cancel (`yielded` for a
/// yield). Read from the records the fold keeps (`task.assigned`'s task
/// row, `artifact.committed`, the control stream), nothing new. `$1`
/// mission, `$2` objective, `$3` limit.
pub(crate) const OBJECTIVE_PRIOR_WORK: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
         'task', k.id, 'branch', c.branch, 'commit', coalesce(spawn_commit_now(c.commit), c.commit),
         'outcome', o.outcome,
         'reason', o.reason, 'report', k.report) order by k.id desc), '[]'::jsonb)
  from (select id, session_id, report, created_at_ms from mission_tasks
         where mission_id = $1 and objective = $2 and state = 'cancelled'
         order by id desc limit $3) k
  left join lateral (
    select a.kind -> 'detail' -> 'artifact' ->> 'branch' as branch,
           a.kind -> 'detail' -> 'artifact' ->> 'commit' as commit
      from spawn_events a
     where a.session_id = k.session_id and a.event_name = 'artifact.committed'
       and a.recorded_at_ms >= k.created_at_ms
       and a.recorded_at_ms < coalesce((select min(n.created_at_ms) from mission_tasks n
                                         where n.session_id = k.session_id
                                           and n.created_at_ms > k.created_at_ms),
                                        9223372036854775807)
       and a.kind -> 'detail' ->> 'reason' is distinct from 'set_aside'
     order by a.id desc limit 1) c on true
  left join lateral (
    select case x.r ->> 'type'
             when 'task.gated' then x.r ->> 'verdict'
             when 'task.aborted' then 'aborted'
             else case when x.r ->> 'yielded' = 'true' then 'yielded' else 'cancelled' end
           end as outcome,
           x.r ->> 'reason' as reason
      from (select e.session_seq as seq, e.kind -> 'detail' -> 'record' as r
              from spawn_events e
             where e.session_id = 'session_control' and e.event_name = 'control.recorded'
               and e.kind -> 'detail' -> 'record' ->> 'id' = k.id::text
            offset 0) x
     where x.r ->> 'type' in ('task.gated', 'task.aborted', 'task.cancelled')
     order by x.seq desc limit 1) o on true";

/// [`PostgresStore::task_host_faults`]: a task's `host_fault` verdicts at
/// one report revision, but a base-only one: the gate's fact with `on`
/// `base` (a scenario's harness error before any candidate code ran),
/// objective 033's `base_disagrees`, or the `cannot_judge` a scenario says
/// of a host that cannot judge it at all (029). Those count toward no halt,
/// so they never count toward the cap either. Nor does the gate's own test
/// server that did not start (`on` `test server`), whichever start it was:
/// no candidate code makes a server start.
/// Nor does a regression the gate found load-sensitive (`load_sensitive`,
/// and the bound's `load_sensitive_parked`): it passed alone, as the base
/// does, and its own bound ends its re-judging.
pub(crate) const TASK_HOST_FAULTS: &str = r"
select count(*)
  from spawn_events e
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'id' = $1
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
   and e.kind -> 'detail' -> 'record' ->> 'verdict' = 'host_fault'
   and e.kind -> 'detail' -> 'record' ->> 'revision' = $2
   and e.kind -> 'detail' -> 'record' ->> 'base' = $3
   and coalesce(e.kind -> 'detail' -> 'record' -> 'fact' ->> 'on', '') not in ('base', 'candidate and base', 'main', 'test server')
   and not (coalesce(e.kind -> 'detail' -> 'record' -> 'fact' ->> 'on', '') = 'sandbox'
            and coalesce(e.kind -> 'detail' -> 'record' -> 'fact' ->> 'tree', '') <> 'candidate')
   and coalesce(e.kind -> 'detail' -> 'record' ->> 'evidence', '') not in
       ('base_disagrees', 'cannot_judge', 'main_red', 'load_sensitive', 'load_sensitive_parked')";

/// The work an ephemeral objective task starts from: the newest earlier
/// task of its objective that ended without landing (cancelled, aborted or
/// yielded) and left work, a checkpoint of its own (`tool_completed`,
/// `turn_stopped`, `execution_ended`) recorded while it ran, as
/// `{task, commit}` with its last checkpoint that is not a set-aside; null
/// for a resident's task, a task with no objective, or none such. The
/// window: from the predecessor's assignment until its session's next task
/// or its own set-aside, whichever comes first (a resident's set-aside, and
/// the merge of main on top of it for its next delivery, hold none of the
/// work: rev-e on a114a9e, objective 55's task 149). spawn-cli checks in
/// git that the commit holds changes of its own. A reassigned task (its
/// `reassign_of`, `spawn_task_reassign`), objective or not, starts from the
/// task it takes over, before any other: `reassign` is then true (absent
/// otherwise). `$1` the
/// task.
pub(crate) const OBJECTIVE_FORK: &str = r"
select coalesce((
  select jsonb_build_object('task', p.id, 'commit', coalesce(spawn_commit_now(c.commit), c.commit))
         || case when p.id = k.reassign_of then jsonb_build_object('reassign', true)
                 else '{}'::jsonb end
    from mission_tasks k
    join mission_tasks p on p.mission_id = k.mission_id
                        and (p.id = k.reassign_of or p.objective = k.objective)
                        and p.id < k.id and p.state = 'cancelled'
   cross join lateral (
     -- A resident's window ends at its task's set-aside: what is recorded
     -- on top of it (the main merge of its next delivery) is not the work.
     select least(
              coalesce((select min(n.created_at_ms) from mission_tasks n
                         where n.session_id = p.session_id
                           and n.created_at_ms > p.created_at_ms),
                       9223372036854775807),
              coalesce((select min(a.recorded_at_ms) from spawn_events a
                         where a.session_id = p.session_id
                           and a.event_name = 'artifact.committed'
                           and a.source_event_id = 'set-aside:' || p.id),
                       9223372036854775807)) as until) w
   cross join lateral (
     select a.kind -> 'detail' -> 'artifact' ->> 'commit' as commit
       from spawn_events a
      where a.session_id = p.session_id and a.event_name = 'artifact.committed'
        and a.recorded_at_ms >= p.created_at_ms and a.recorded_at_ms < w.until
        and a.kind -> 'detail' ->> 'reason' is distinct from 'set_aside'
      order by a.id desc limit 1) c
   where k.id = $1 and k.kind = 'ephemeral'
     and (k.objective is not null or k.reassign_of is not null)
     and exists (select 1 from spawn_events a
                  where a.session_id = p.session_id and a.event_name = 'artifact.committed'
                    and a.recorded_at_ms >= p.created_at_ms and a.recorded_at_ms < w.until
                    and a.kind -> 'detail' ->> 'reason'
                        in ('tool_completed', 'turn_stopped', 'execution_ended'))
   order by (p.id = k.reassign_of) is true desc, p.id desc limit 1), 'null'::jsonb)";

/// What replaced what in one session (`GET /api/sessions/<id>`), as
/// `{restart_of, reassigned_to, reassigned_from}`:
///
/// - `restart_of`: for each execution a restart's resume started (the
///   dispatcher's command keyed `restart-<task>-<n>`), the execution of the
///   same session it replaced: the one that `task.restarted` record names,
///   else the session's execution before it; an object by execution id;
/// - `reassigned_to`: `{task, session_id}` of the task this session's
///   latest reassigned task went to, and its worker's session, or null;
/// - `reassigned_from`: `{task, session_id}` of the task this session's
///   latest task took over, and its worker's session, or null.
///
/// `$1` the session.
pub(crate) const SESSION_LINKS: &str = r"
select jsonb_build_object(
  'restart_of', (
    select coalesce(jsonb_object_agg(e.execution_id, coalesce(r.execution_id, p.execution_id)),
                    '{}'::jsonb)
      from agent_executions e
      join spawn_commands c on c.id = e.command_id
       and c.principal = 'spawn-dispatch' and c.idempotency_key ~ '^restart-[0-9]+-[0-9]+$'
      left join lateral (
        select x.kind -> 'detail' -> 'record' ->> 'execution_id' as execution_id
          from spawn_events x
         where x.session_id = 'session_control' and x.event_name = 'control.recorded'
           and x.kind -> 'detail' -> 'record' ->> 'type' = 'task.restarted'
           and x.kind -> 'detail' -> 'record' ->> 'task' = split_part(c.idempotency_key, '-', 2)
           and x.kind -> 'detail' -> 'record' ->> 'n' = split_part(c.idempotency_key, '-', 3)
         order by x.session_seq desc limit 1) r on true
      left join lateral (
        select b.execution_id from agent_executions b
         where b.session_id = e.session_id
           and (b.started_at_ms, b.execution_id) < (e.started_at_ms, e.execution_id)
         order by b.started_at_ms desc, b.execution_id desc limit 1) p on true
     where e.session_id = $1),
  'reassigned_to', (
    select jsonb_build_object('task', n.id, 'session_id', n.session_id)
      from mission_tasks o join mission_tasks n on n.reassign_of = o.id
     where o.session_id = $1
     order by n.id desc limit 1),
  'reassigned_from', (
    select jsonb_build_object('task', o.id, 'session_id', o.session_id)
      from mission_tasks n join mission_tasks o on o.id = n.reassign_of
     where n.session_id = $1
     order by n.id desc limit 1))";

/// Missions with a gate that are open, or closed with work the closure rule
/// (`spawn_task_may_continue`) still lets go on, with that work: every task
/// the integrator may judge, land, or hold to its budget, with the brief and
/// the report a landing's message is written from. Each names `hold`, the
/// active owner's hold on its source (or null), `red` its source's red main
/// (`mission_source_reds`: main, scenario, since_ms, latest_ms, probed_ms; or
/// null), and each task `held_by`. The mission's baseline and each task's
/// base and candidate are read through the commit map (objective 173): the
/// commits they are now, after a history rewrite (null for a dropped one),
/// through `spawn_commit_now_rows` joined LATERAL, which PostgreSQL inlines
/// (objective 190: no function call per row on this polled read).
pub(crate) const GATED: &str = r"
select coalesce(jsonb_agg(row_to_json(x) order by x.id), '[]'::jsonb)
from (
  select m.id, m.state, m.gate, m.accept, m.budget, mb.now as baseline,
         m.halted, m.scenario_floor,
         m.scenario_names, spawn_now_ms() as now_ms, m.pause_event is not null as paused,
         m.gate_rejections, m.gate_aborts, m.gate_room_task, m.gate_room_since_ms,
         m.gate_room_last_ms,
         m.gate_room_waits, m.gate_room_escalated, m.gate_host_faults, m.gate_host_task,
         m.gate_host_due_ms, m.gate_base_disagrees, m.gate_rejection_objective,
         m.objectives, m.sources ->> 0 as source,
         (select h.hold from mission_source_holds h where h.source = m.sources ->> 0) as hold,
         (select jsonb_build_object('main', coalesce(rm.now, r.main), 'scenario', r.scenario,
                                    'since_ms', r.since_ms, 'latest_ms', r.red_ms,
                                    'probed_ms', r.probed_ms)
            from mission_source_reds r
           cross join lateral spawn_commit_now_rows(r.main) rm
           where r.source = m.sources ->> 0) as red,
         (select max(e.recorded_at_ms) from spawn_events e
           where e.session_id = 'session_control' and e.event_name = 'control.recorded'
             and e.kind -> 'detail' -> 'record' ->> 'type' = 'mission.configured'
             and e.kind -> 'detail' -> 'record' ->> 'id' = m.id::text) as configured_ms,
         coalesce(t.tasks, '[]'::jsonb) as tasks
    from mission_missions m
    cross join lateral spawn_commit_now_rows(m.baseline) mb
    left join lateral (
      select jsonb_agg(jsonb_build_object(
               'id', k.id, 'agent_id', k.agent_id,
               'session_id', k.session_id, 'state', k.state,
               'base', kb.now,
               'candidate', kc.now, 'scenario', k.scenario,
               'delivered_at_ms', k.delivered_at_ms, 'objective', k.objective,
               'revisions', k.revisions, 'brief', k.brief, 'report', k.report,
               'updated_at_ms', k.updated_at_ms, 'held_by', k.held_by,
               'continues', k.continues,
               'gate_load_sensitive_pushed',
               coalesce(kl.now, k.gate_load_sensitive_pushed),
               'gate_load_sensitive_scenarios', k.gate_load_sensitive_scenarios,
               'gate_parked_pushed', coalesce(kp.now, k.gate_parked_pushed))
               order by k.id) as tasks
        from mission_tasks k
        left join mission_residents r on r.agent_id = k.agent_id
        cross join lateral spawn_commit_now_rows(k.base) kb
        cross join lateral spawn_commit_now_rows(k.candidate) kc
        cross join lateral spawn_commit_now_rows(k.gate_load_sensitive_pushed) kl
        cross join lateral spawn_commit_now_rows(k.gate_parked_pushed) kp
       where k.mission_id = m.id
         and k.state not in ('cancelled', 'landed')
         and spawn_task_may_continue(k.state, m.state, true, r.state)) t on true
   where m.gate is not null
     and (m.state in ('open', 'active') or t.tasks is not null)
) x";

/// The `no_room` gate records that carry `escalate` (the mission's gate
/// went without room past the integrator's limit), oldest first, each with
/// its `event_id`, `at_ms` and `mission`: those the supervisor was not told
/// of yet, and those it was told of whose run of `no_room` still goes on
/// (the mission's `gate_room_*` name the record's task, escalated, since
/// the record or before), which the dispatcher keeps as told and
/// unresolved. Told is a record, not a window: a digest that reached the
/// supervisor carrying the tag `host gate-room-<event_id>` (as
/// [`MISSION_NEWS`]'s `told`, however long ago). So a supervisor that was
/// away for days is still told once.
pub(crate) const GATE_ROOM_ESCALATIONS: &str = r"
with told as (
  select distinct tag
    from spawn_commands d
   cross join lateral jsonb_array_elements_text(
           case when jsonb_typeof(d.action -> 'tags') = 'array'
                then d.action -> 'tags' else '[]'::jsonb end) tag
   where d.session_id = 'session_supervisor' and d.principal = 'spawn-dispatch' and d.succeeded
)
select coalesce(jsonb_agg(r || jsonb_build_object('event_id', e.id, 'at_ms', e.recorded_at_ms,
                                                  'mission', k.mission_id)
                          order by e.session_seq), '[]'::jsonb)
  from spawn_events e
 cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
  left join mission_tasks k on k.id = (r ->> 'id')::bigint
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
   and e.kind -> 'detail' -> 'record' ->> 'verdict' = 'no_room'
   and e.kind -> 'detail' -> 'record' ->> 'escalate' = 'true'
   and (not exists (select 1 from told where tag = 'host gate-room-' || e.id)
        or exists (select 1 from mission_missions m
                    where m.id = k.mission_id and m.gate_room_escalated
                      and m.gate_room_task = (r ->> 'id')::bigint
                      and m.gate_room_since_ms <= e.recorded_at_ms))";

/// The `host_fault` gate records that carry `escalate` (the mission's second
/// host fault in a row) and that the supervisor was not told of yet, oldest
/// first, each with its `event_id`, `at_ms` and `mission`, without the
/// gate's `log`. Told is a record, as for [`GATE_ROOM_ESCALATIONS`]: a digest
/// that reached the supervisor carrying the tag
/// `host gate-host-fault-<event_id>`, however long ago.
pub(crate) const GATE_HOST_FAULT_ESCALATIONS: &str = r"
with told as (
  select distinct tag
    from spawn_commands d
   cross join lateral jsonb_array_elements_text(
           case when jsonb_typeof(d.action -> 'tags') = 'array'
                then d.action -> 'tags' else '[]'::jsonb end) tag
   where d.session_id = 'session_supervisor' and d.principal = 'spawn-dispatch' and d.succeeded
)
select coalesce(jsonb_agg((r - 'log') || jsonb_build_object('event_id', e.id,
                                                           'at_ms', e.recorded_at_ms,
                                                           'mission', k.mission_id)
                          order by e.session_seq), '[]'::jsonb)
  from spawn_events e
 cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
  left join mission_tasks k on k.id = (r ->> 'id')::bigint
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
   and e.kind -> 'detail' -> 'record' ->> 'verdict' = 'host_fault'
   and e.kind -> 'detail' -> 'record' ->> 'escalate' = 'true'
   and not exists (select 1 from told where tag = 'host gate-host-fault-' || e.id)";

/// The `source.main_red` records that start a red (`alert`: the first of
/// the source's red, the store's) and the `source.main_green` records that
/// end one, that the supervisor was not told of yet, oldest first, each with
/// its `event_id` and `at_ms`. Told is a record, as for
/// [`GATE_HOST_FAULT_ESCALATIONS`]: a digest that reached the supervisor
/// carrying the tag `host main-red-<event_id>` or `host main-green-<event_id>`,
/// however long ago.
pub(crate) const MAIN_RED_NEWS: &str = r"
with told as (
  select distinct tag
    from spawn_commands d
   cross join lateral jsonb_array_elements_text(
           case when jsonb_typeof(d.action -> 'tags') = 'array'
                then d.action -> 'tags' else '[]'::jsonb end) tag
   where d.session_id = 'session_supervisor' and d.principal = 'spawn-dispatch' and d.succeeded
)
select coalesce(jsonb_agg(r || jsonb_build_object('event_id', e.id, 'at_ms', e.recorded_at_ms)
                          order by e.session_seq), '[]'::jsonb)
  from spawn_events e
 cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and ((r ->> 'type' = 'source.main_red' and r ->> 'alert' = 'true'
         and not exists (select 1 from told where tag = 'host main-red-' || e.id))
        or (r ->> 'type' = 'source.main_green'
            and not exists (select 1 from told where tag = 'host main-green-' || e.id)))";

/// The `source_away` alerts' records (P12) the supervisor was not told of
/// yet, oldest first, each with its `event_id` and `at_ms`: an
/// `alert.fired` (a source of a mission is not a directory on this host)
/// and an `alert.cleared` (it is back, or no longer watched). Told is a
/// record, as for [`MAIN_RED_NEWS`]: a digest that reached the supervisor
/// carrying the tag `host source-away-<event_id>` or
/// `host source-back-<event_id>`, however long ago.
pub(crate) const SOURCE_AWAY_NEWS: &str = r"
with told as (
  select distinct tag
    from spawn_commands d
   cross join lateral jsonb_array_elements_text(
           case when jsonb_typeof(d.action -> 'tags') = 'array'
                then d.action -> 'tags' else '[]'::jsonb end) tag
   where d.session_id = 'session_supervisor' and d.principal = 'spawn-dispatch' and d.succeeded
)
select coalesce(jsonb_agg(r || jsonb_build_object('event_id', e.id, 'at_ms', e.recorded_at_ms)
                          order by e.session_seq), '[]'::jsonb)
  from spawn_events e
 cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and r ->> 'kind' = 'source_away'
   and ((r ->> 'type' = 'alert.fired'
         and not exists (select 1 from told where tag = 'host source-away-' || e.id))
        or (r ->> 'type' = 'alert.cleared'
            and not exists (select 1 from told where tag = 'host source-back-' || e.id)))";

/// The `host_fault` gate records whose base disagreed with itself and that
/// carry `alert` (the mission's third such base in a row, objective 033),
/// and that the supervisor was not told of yet, oldest first, each with its
/// `event_id`, `at_ms` and `mission`, without the gate's `log`. Told is a
/// record, as for [`GATE_HOST_FAULT_ESCALATIONS`]: a digest that reached the
/// supervisor carrying the tag `host gate-base-disagrees-<event_id>`,
/// however long ago.
pub(crate) const GATE_BASE_DISAGREES_ALERTS: &str = r"
with told as (
  select distinct tag
    from spawn_commands d
   cross join lateral jsonb_array_elements_text(
           case when jsonb_typeof(d.action -> 'tags') = 'array'
                then d.action -> 'tags' else '[]'::jsonb end) tag
   where d.session_id = 'session_supervisor' and d.principal = 'spawn-dispatch' and d.succeeded
)
select coalesce(jsonb_agg((r - 'log') || jsonb_build_object('event_id', e.id,
                                                           'at_ms', e.recorded_at_ms,
                                                           'mission', k.mission_id)
                          order by e.session_seq), '[]'::jsonb)
  from spawn_events e
 cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
  left join mission_tasks k on k.id = (r ->> 'id')::bigint
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
   and e.kind -> 'detail' -> 'record' ->> 'verdict' = 'host_fault'
   and e.kind -> 'detail' -> 'record' ->> 'evidence' = 'base_disagrees'
   and e.kind -> 'detail' -> 'record' ->> 'alert' = 'true'
   and not exists (select 1 from told where tag = 'host gate-base-disagrees-' || e.id)";

/// The `task.shadow_gated` records whose runner disagreed with the primary
/// on the same inputs (`agrees` false) and that the supervisor was not told
/// of yet, oldest first, each with its `event_id`, `at_ms` and `mission`.
/// Told is a record, as for [`GATE_HOST_FAULT_ESCALATIONS`]: a digest that
/// reached the supervisor carrying the tag `host shadow-disagrees-<event_id>`,
/// however long ago. An agreement, and a record that compared nothing
/// (`agrees` null: unequal inputs, or no verdict on one side), are never
/// news.
pub(crate) const SHADOW_DISAGREEMENTS: &str = r"
with told as (
  select distinct tag
    from spawn_commands d
   cross join lateral jsonb_array_elements_text(
           case when jsonb_typeof(d.action -> 'tags') = 'array'
                then d.action -> 'tags' else '[]'::jsonb end) tag
   where d.session_id = 'session_supervisor' and d.principal = 'spawn-dispatch' and d.succeeded
)
select coalesce(jsonb_agg(r || jsonb_build_object('event_id', e.id,
                                                  'at_ms', e.recorded_at_ms,
                                                  'mission', k.mission_id)
                          order by e.session_seq), '[]'::jsonb)
  from spawn_events e
 cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
  left join mission_tasks k on k.id = (r ->> 'id')::bigint
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.shadow_gated'
   and e.kind -> 'detail' -> 'record' -> 'agrees' = 'false'::jsonb
   and not exists (select 1 from told where tag = 'host shadow-disagrees-' || e.id)";

/// The `task.over_budget` records that are a second overrun in a chain (a
/// task that continues an overrun task of its objective, with no configure
/// between: its `chain` is true), escalated to the supervisor once. A digest that reached the supervisor
/// carrying the tag `host over-budget-<event_id>` means it was told,
/// however long ago. Each carries its `event_id`, `at_ms`, its `mission`,
/// and the record's fields (`id`, `continues`, `objective`, `branch`,
/// `commit`, `diff`, `tool_calls`, `budget`, `reason`).
pub(crate) const OVER_BUDGET_ESCALATIONS: &str = r"
with told as (
  select distinct tag
    from spawn_commands d
   cross join lateral jsonb_array_elements_text(
           case when jsonb_typeof(d.action -> 'tags') = 'array'
                then d.action -> 'tags' else '[]'::jsonb end) tag
   where d.session_id = 'session_supervisor' and d.principal = 'spawn-dispatch' and d.succeeded
)
select coalesce(jsonb_agg(r || jsonb_build_object('event_id', e.id, 'at_ms', e.recorded_at_ms,
                                                  'mission', k.mission_id)
                          order by e.session_seq), '[]'::jsonb)
  from spawn_events e
 cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
  left join mission_tasks k on k.id = (r ->> 'id')::bigint
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.over_budget'
   and e.kind -> 'detail' -> 'record' ->> 'chain' = 'true'
   and not exists (select 1 from told where tag = 'host over-budget-' || e.id)";

/// The integrator's expired drains (`integrator.drain_expired`) no
/// delivered supervisor digest names yet (its tag `host
/// integrator-drain-expired-<event id>`), each with its event id and time.
pub(crate) const DRAIN_EXPIRIES: &str = r"
with told as (
  select distinct tag
    from spawn_commands d
   cross join lateral jsonb_array_elements_text(
           case when jsonb_typeof(d.action -> 'tags') = 'array'
                then d.action -> 'tags' else '[]'::jsonb end) tag
   where d.session_id = 'session_supervisor' and d.principal = 'spawn-dispatch' and d.succeeded
)
select coalesce(jsonb_agg(r || jsonb_build_object('event_id', e.id, 'at_ms', e.recorded_at_ms)
                          order by e.session_seq), '[]'::jsonb)
  from spawn_events e
 cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'integrator.drain_expired'
   and not exists (select 1 from told where tag = 'host integrator-drain-expired-' || e.id)";

/// The last `task.gated` record of every task, keyed by task id:
/// `{"<id>": {verdict, reason, metric, candidate, at_ms}}`, with the
/// verdict's `class`, `inputs` and `env` (objective 035) when it names them
/// (a record from before them has none). Kept out of
/// [`MISSION_BOARD`]: the supervisor's login reads the board and may not
/// read `spawn_events`.
pub(crate) const LAST_GATES: &str = r"
select coalesce(jsonb_object_agg(z.task, z.gate), '{}'::jsonb)
  from (select distinct on (r ->> 'id') r ->> 'id' as task,
               jsonb_build_object('verdict', r ->> 'verdict', 'reason', r ->> 'reason',
                                  'metric', r ->> 'metric', 'candidate', r ->> 'candidate',
                                  'at_ms', e.recorded_at_ms)
               || (r - array(select k from jsonb_object_keys(r) k
                              where k not in ('class', 'inputs', 'env', 'head_inputs'))) as gate
          from spawn_events e
         cross join lateral (select e.kind -> 'detail' -> 'record' as r) g
         where e.session_id = 'session_control' and e.event_name = 'control.recorded'
           and r ->> 'type' = 'task.gated'
         order by r ->> 'id', e.session_seq desc) z";

/// Open objective missions with their backlog directory, `[{mission,
/// backlog}]`: what the dispatcher checks the backlog's named order of.
pub(crate) const OBJECTIVE_BACKLOGS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object('mission', m.id,
                                             'backlog', m.objectives ->> 'backlog')
                          order by m.id), '[]'::jsonb)
  from mission_missions m
 where m.objectives ->> 'backlog' is not null and m.state in ('open', 'active')";

/// Open objective missions (`objectives` = `{backlog, resident}`), for the
/// RSI page: settings, the resident, and every task newest first with its
/// gate verdicts from the control stream (oldest first, without logs).
/// Also what the integrator's choice reads besides the tasks and their
/// outcomes: `configured_ms` (when the mission was last configured, as
/// [`GATED`] reads it: an overrun chain's hold ends with a newer one) and
/// `solved_on_base`, the mission's `objective.solved` records, oldest first
/// (`[{event, objective, task, base, at_ms}]`): the records
/// [`OBJECTIVE_LANDINGS`] counts as landings, by the same condition; and
/// `retired`, the objectives retired by hand (objective 210, `[{objective,
/// by, reason, caller, at_ms}]`, oldest first), as [`MISSION_BOARD`] gives
/// them.
pub(crate) const OBJECTIVE_MISSIONS: &str = r"
select coalesce(jsonb_agg(row_to_json(x) order by x.id), '[]'::jsonb)
from (
  select m.id, m.text, m.state, m.sources, m.gate, m.accept, m.budget, m.baseline, m.halted,
         m.scenario_floor, m.gate_rejections, m.gate_aborts, m.objectives,
         m.created_at_ms, m.updated_at_ms,
         (select max(e.recorded_at_ms) from spawn_events e
           where e.session_id = 'session_control' and e.event_name = 'control.recorded'
             and e.kind -> 'detail' -> 'record' ->> 'type' = 'mission.configured'
             and e.kind -> 'detail' -> 'record' ->> 'id' = m.id::text) as configured_ms,
         coalesce((select jsonb_agg(jsonb_build_object(
                     'event', e.id,
                     'objective', (e.kind -> 'detail' -> 'record' ->> 'objective')::integer,
                     'task', (e.kind -> 'detail' -> 'record' ->> 'task')::bigint,
                     'base', e.kind -> 'detail' -> 'record' ->> 'base',
                     'at_ms', e.recorded_at_ms) order by e.session_seq, e.id)
                     from spawn_events e
                    where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                      and e.kind -> 'detail' -> 'record' ->> 'type' = 'objective.solved'
                      and e.kind -> 'detail' -> 'record' ->> 'id' = m.id::text), '[]'::jsonb)
           as solved_on_base,
         coalesce((select jsonb_agg(jsonb_build_object(
                     'objective', o.objective, 'by', o.by, 'reason', o.reason,
                     'caller', o.caller, 'at_ms', o.at_ms) order by o.at_ms, o.event)
                     from mission_retired_objectives o where o.mission_id = m.id), '[]'::jsonb)
           as retired,
         (select jsonb_build_object('name', r.name, 'agent_id', r.agent_id,
                                    'session_id', r.session_id, 'state', r.state)
            from mission_residents r where r.name = m.objectives ->> 'resident') as resident,
         coalesce((select jsonb_agg(jsonb_build_object(
                     'id', k.id, 'kind', k.kind, 'agent_id', k.agent_id,
                     'session_id', k.session_id, 'state', k.state, 'revisions', k.revisions,
                     'objective', k.objective, 'candidate', k.candidate, 'base', k.base,
                     'metric', k.metric, 'scenario', k.scenario,
                     'brief', left(k.brief, 600), 'review_note', left(k.review_note, 2000),
                     'created_by', k.created_by, 'created_at_ms', k.created_at_ms,
                     'delivered_at_ms', k.delivered_at_ms, 'updated_at_ms', k.updated_at_ms,
                     'gates', coalesce((
                        select jsonb_agg(jsonb_build_object(
                                 'verdict', r ->> 'verdict', 'reason', r ->> 'reason',
                                 'metric', r ->> 'metric', 'candidate', r ->> 'candidate',
                                 'at_ms', e.recorded_at_ms) order by e.session_seq)
                          from spawn_events e
                         cross join lateral (select e.kind -> 'detail' -> 'record' as r) g
                         where e.session_id = 'session_control'
                           and e.event_name = 'control.recorded'
                           and r ->> 'type' = 'task.gated'
                           and r ->> 'id' = k.id::text), '[]'::jsonb))
                     order by k.id desc)
                   from mission_tasks k where k.mission_id = m.id), '[]'::jsonb) as tasks
    from mission_missions m
   where m.objectives is not null and m.state in ('open', 'active')
) x";

/// One task of an objective mission with its full brief, report, last gate
/// log, and every gate verdict with its log, oldest first; null when there
/// is no such task. `$1` task.
pub(crate) const OBJECTIVE_TASK: &str = r"
select (select jsonb_build_object(
          'id', k.id, 'mission_id', k.mission_id, 'kind', k.kind, 'agent_id', k.agent_id,
          'session_id', k.session_id, 'state', k.state, 'revisions', k.revisions,
          'objective', k.objective, 'candidate', k.candidate, 'base', k.base,
          'metric', k.metric, 'scenario', k.scenario, 'brief', k.brief,
          'review_note', k.review_note, 'report', k.report, 'gate_log', k.gate_log,
          'created_by', k.created_by, 'created_at_ms', k.created_at_ms,
          'delivered_at_ms', k.delivered_at_ms, 'updated_at_ms', k.updated_at_ms,
          'gates', coalesce((
             select jsonb_agg(jsonb_build_object(
                      'verdict', r ->> 'verdict', 'reason', r ->> 'reason',
                      'metric', r ->> 'metric', 'candidate', r ->> 'candidate',
                      'log', r ->> 'log', 'at_ms', e.recorded_at_ms) order by e.session_seq)
               from spawn_events e
              cross join lateral (select e.kind -> 'detail' -> 'record' as r) g
              where e.session_id = 'session_control'
                and e.event_name = 'control.recorded'
                and r ->> 'type' = 'task.gated'
                and r ->> 'id' = k.id::text), '[]'::jsonb))
          from mission_tasks k where k.id = $1)";

/// One task's texts in full, as the board's list sends only their first
/// lines: `{id, mission_id, brief, report, review_note, updated_at_ms}`;
/// null when there is no such task. `$1` task.
pub(crate) const TASK_TEXTS: &str = r"
select (select jsonb_build_object(
          'id', k.id, 'mission_id', k.mission_id, 'brief', k.brief, 'report', k.report,
          'review_note', k.review_note, 'updated_at_ms', k.updated_at_ms)
          from mission_tasks k where k.id = $1)";

/// The integrator's advisory lock: one integrator per store (database and
/// schema). The two-key form pairs this class with the schema's name, so
/// stores in other schemas of the same database (tests, the e2e) never
/// contend for it.
pub(crate) const INTEGRATOR_LOCK_KEY: i32 = 1_399_957_336;

/// Who holds the integrator's lock on this store (objective 221): the
/// granted `pg_locks` row of the two-key form, `$1` and the schema's
/// `hashtext`, in this database, with its connection's name and start.
const INTEGRATOR_LOCK_HOLDER: &str = r"
select jsonb_build_object(
         'pid', l.pid, 'name', coalesce(a.application_name, ''),
         'connected_ms', (extract(epoch from a.backend_start) * 1000)::bigint)
  from pg_locks l
  left join pg_stat_activity a on a.pid = l.pid
 where l.locktype = 'advisory' and l.granted
   and l.database = (select d.oid from pg_database d where d.datname = current_database())
   and l.classid = $1::int4::oid
   and l.objid = hashtext(current_schema())::oid
   and l.objsubid = 2
 limit 1";

/// Whether this connection holds the integrator's lock on this store.
const INTEGRATOR_HOLDS_LOCK: &str = r"
select exists (
  select 1 from pg_locks l
   where l.locktype = 'advisory' and l.granted and l.pid = pg_backend_pid()
     and l.database = (select d.oid from pg_database d where d.datname = current_database())
     and l.classid = $1::int4::oid
     and l.objid = hashtext(current_schema())::oid
     and l.objsubid = 2)";

/// Tool calls a session started after a moment. `$1` session, `$2` ms.
pub(crate) const TOOL_CALLS_SINCE: &str = "select count(*) from spawn_events
                  where session_id = $1 and event_name = 'tool.started' and recorded_at_ms > $2";

/// A session's events after a time, in log order, with the tool call's or
/// background job's id where they have one: what a task's active time is
/// measured from (spawn-cli's `integrate::active_ms`). Readings are left
/// out: the Bridge records a busy pane every 30 s and the observer each
/// reading, also while Claude Code only retries a model API that is down,
/// so they say the pane is busy, not that work moves. Read by the
/// session's own index: from its first event after `$2`.
pub(crate) const SESSION_ACTIVITY: &str = "select id, recorded_at_ms, event_name,
                        coalesce(kind -> 'detail' ->> 'tool_use_id',
                                 kind -> 'detail' ->> 'job_id')
                   from spawn_events
                  where session_id = $1
                    and session_seq > coalesce((select max(session_seq) from spawn_events
                                                 where session_id = $1
                                                   and recorded_at_ms <= $2), 0)
                    and recorded_at_ms > $2
                    and event_name not in ('screen.observed', 'observation.recorded')
                  order by session_seq";

/// How long the whole fleet was alive between two events of the log (ids
/// `$1` and `$2`): the gaps between consecutive events of any session, in
/// log order, each capped at `$3` ms. While the host is up, the fleet's
/// sessions, loops and control stream leave a gap over a few minutes
/// rarely; a stopped fleet or a frozen host writes nothing, so its outage
/// adds at most one cap. Scanned by the primary key: ids follow the log's
/// order.
pub(crate) const FLEET_ALIVE: &str = "select coalesce(sum(least(greatest(gap, 0), $3)), 0)::bigint
                   from (select recorded_at_ms - lag(recorded_at_ms) over (order by id) as gap
                           from spawn_events
                          where id between $1 and $2) gaps";

/// The commit of an agent's latest checkpoint (or merge of main): its last
/// `artifact.committed` event.
/// When task `$1` went over its budget (`task.over_budget`), or no row: the
/// chain of overruns a mission.configured resets.
pub(crate) const OVERRUN_AT: &str = "select recorded_at_ms from spawn_events
                   where session_id = 'session_control' and event_name = 'control.recorded'
                     and kind -> 'detail' -> 'record' ->> 'type' = 'task.over_budget'
                     and kind -> 'detail' -> 'record' ->> 'id' = $1
                   order by id desc limit 1";

/// An agent's latest checkpoint (`LATEST_CHECKPOINT`), read through the
/// commit map (objective 177): its new commit after a history rewrite, or
/// the recorded id when the map cannot translate it.
pub(crate) const LATEST_CHECKPOINT: &str = "select coalesce(spawn_commit_now(kind -> 'detail' -> 'artifact' ->> 'commit'), kind -> 'detail' -> 'artifact' ->> 'commit')
                   from spawn_events
                  where agent_id = $1 and event_name = 'artifact.committed'
                  order by id desc limit 1";

/// An agent's latest checkpoint and when it was recorded, for the work an
/// overrun task keeps: `{commit, at_ms}`, or no row. The commit through
/// the commit map (objective 177).
pub(crate) const CHECKPOINT_AT: &str = "select jsonb_build_object(
                   'commit', coalesce(spawn_commit_now(kind -> 'detail' -> 'artifact' ->> 'commit'), kind -> 'detail' -> 'artifact' ->> 'commit'),
                   'at_ms', recorded_at_ms)
                   from spawn_events
                  where agent_id = $1 and event_name = 'artifact.committed'
                  order by id desc limit 1";

/// The `commit` of a task's latest `task.reported` control record. `$1`
/// the task id, as text (the record's `id`). Through the commit map
/// (objective 177): an id the map cannot translate reads as itself.
pub(crate) const REPORTED_COMMIT: &str = "select coalesce(spawn_commit_now(kind -> 'detail' -> 'record' ->> 'commit'), kind -> 'detail' -> 'record' ->> 'commit')
                   from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.reported'
                    and kind -> 'detail' -> 'record' ->> 'id' = $1
                  order by id desc limit 1";

// Residents and workspaces.

/// Approved residents that have run before, as (agent, session, charter).
pub(crate) const RESIDENTS_TO_KEEP: &str = "select r.agent_id, r.session_id, r.charter
                   from mission_residents r
                  where r.state = 'approved'
                    and exists (select 1 from spawn_events e
                                 where e.session_id = r.session_id
                                   and e.event_name = 'execution.started')
                  order by r.name";

/// Retired residents, as (agent, session).
/// Every resident, by name, with its state and pool (objective 239).
pub(crate) const RESIDENT_ROSTER: &str = r"
select coalesce(jsonb_agg(jsonb_build_object('name', name, 'agent_id', agent_id,
                                             'session_id', session_id, 'state', state,
                                             'pool', pool) order by name), '[]'::jsonb)
  from mission_residents";

pub(crate) const RETIRED_RESIDENTS: &str =
    "select agent_id, session_id from mission_residents where state = 'retired'";

/// First source of the mission behind an agent's first task. `$1` agent.
///
/// An agent whose first execution started before its first task was
/// mounted from the default repository (`SPAWN_GIT_REPO`), where its branch
/// lives (a resident started before it had work): it stays there, read from
/// the records (`execution.started` against the task's creation, one clock),
/// so its checkpoints find the branch it was mounted on. No row then.
pub(crate) const AGENT_SOURCE: &str = "select m.sources ->> 0
                   from mission_tasks k join mission_missions m on m.id = k.mission_id
                  where k.agent_id = $1
                    and not exists (select 1 from spawn_events e
                                     where e.agent_id = $1 and e.event_name = 'execution.started'
                                       and e.recorded_at_ms < k.created_at_ms)
                  order by k.id limit 1";

/// Agents with a task still open (not cancelled, not landed) and the
/// source their workspace comes from, by [`AGENT_SOURCE`]'s rule (the
/// agent's first task decides, and an agent that ran before it stays on
/// the default repository, published by no mission): the branches a
/// restarted Bridge publishes again, since its pusher's queue lived in
/// memory.
pub(crate) const PUBLISHING_AGENTS: &str = "select a.agent_id,
                        (select m.sources ->> 0
                           from mission_tasks k join mission_missions m on m.id = k.mission_id
                          where k.agent_id = a.agent_id
                          order by k.id limit 1)
                   from (select distinct agent_id from mission_tasks
                          where state not in ('cancelled', 'landed')) a
                  where not exists (
                          select 1 from spawn_events e
                           where e.agent_id = a.agent_id and e.event_name = 'execution.started'
                             and e.recorded_at_ms < (select min(k.created_at_ms) from mission_tasks k
                                                      where k.agent_id = a.agent_id))
                  order by a.agent_id";

/// The owner's landing requests (`source.push_requested`, `spawn mission
/// push`) that have no outcome yet (a `source.moved` or a
/// `source.push_refused` naming them), oldest first: `[{request, source,
/// commit, ref, at_ms, gated, urgent, reason}]`, `request` the request's
/// event id (`gated` and `urgent` false and `reason` null when not given). Read through the
/// `spawn_events_source_push` index. The `commit` is read through the commit
/// map (objective 173): a request recorded before a history rewrite is
/// applied as its new commit; one for a commit the rewrite dropped keeps its
/// recorded id, which the integrator refuses (`owner.rs` `pre_rewrite`).
pub(crate) const SOURCE_PUSHES_PENDING: &str = concat!(
    "with ",
    owner_requests_pending!(),
    r"
select coalesce(jsonb_agg(jsonb_build_object('request', q.id, 'source', q.r ->> 'source',
                                             'commit', coalesce(spawn_commit_now(q.r ->> 'commit'),
                                                                q.r ->> 'commit'),
                                             'ref', q.r ->> 'ref',
                                             'at_ms', q.recorded_at_ms,
                                             'gated', coalesce((q.r ->> 'gated')::boolean, false),
                                             'urgent', coalesce((q.r ->> 'urgent')::boolean, false),
                                             'reason', q.r ->> 'reason')
                          order by q.session_seq), '[]'::jsonb)
  from pending q"
);

/// One source's owner landing requests (`spawn mission pushes`), newest
/// first, pending and decided alike: each as [`SOURCE_PUSHES_PENDING`] gives
/// it, with `state` (`pending`, `applied` for a `source.moved`, `refused`
/// for a `source.push_refused`, as `spawn mission push --wait` says it) and
/// `outcome` (that record with its `event_id` and `at_ms`, null while
/// pending). `$1` the source as the requests name it; `$2` a commit prefix
/// or null for every commit; `$3` how many. Read through the
/// `spawn_events_source_push` index.
pub(crate) const SOURCE_PUSHES: &str = r"
with owner as (
  select e.id, e.session_seq, e.recorded_at_ms, e.kind -> 'detail' -> 'record' as r
    from spawn_events e
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type'
         in ('source.push_requested', 'source.push_refused', 'source.moved')),
requests as (
  select q.id, q.session_seq, q.recorded_at_ms, q.r
    from owner q
   where q.r ->> 'type' = 'source.push_requested'
     and q.r ->> 'source' = $1::text
     and ($2::text is null or starts_with(q.r ->> 'commit', $2::text))
   order by q.session_seq desc
   limit $3::bigint)
select coalesce(jsonb_agg(jsonb_build_object('request', q.id, 'source', q.r ->> 'source',
                                             'commit', q.r ->> 'commit', 'ref', q.r ->> 'ref',
                                             'at_ms', q.recorded_at_ms,
                                             'gated', coalesce((q.r ->> 'gated')::boolean, false),
                                             'urgent', coalesce((q.r ->> 'urgent')::boolean, false),
                                             'reason', q.r ->> 'reason',
                                             'state', case when o.r is null then 'pending'
                                                           when o.r ->> 'type' = 'source.moved'
                                                           then 'applied'
                                                           else 'refused' end,
                                             'outcome', o.r || jsonb_build_object(
                                                 'event_id', o.id, 'at_ms', o.recorded_at_ms))
                          order by q.session_seq desc), '[]'::jsonb)
  from requests q
  left join lateral (select o.id, o.recorded_at_ms, o.r
                       from owner o
                      where o.r ->> 'type' in ('source.push_refused', 'source.moved')
                        and o.r ->> 'request' = q.id::text
                      order by o.session_seq
                      limit 1) o on true";

/// The outcome of the owner's landing request `$1` (its event id): the
/// `source.moved` or `source.push_refused` record naming it, with its own
/// `event_id` and `at_ms`; no row while it is pending.
pub(crate) const SOURCE_PUSH_OUTCOME: &str = r"
select e.kind -> 'detail' -> 'record' || jsonb_build_object('event_id', e.id,
                                                            'at_ms', e.recorded_at_ms)
  from spawn_events e
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type'
       in ('source.push_requested', 'source.push_refused', 'source.moved')
   and e.kind -> 'detail' -> 'record' ->> 'request' = ($1::bigint)::text
 order by e.session_seq
 limit 1";

/// Why the last owner judging of the request `$1` (its event id) that
/// recorded a `source.push_no_verdict` found no verdict: that record's
/// `reason`; no row when none did. A rare read (once per owner judging that
/// ends with no verdict), through the control session's events.
pub(crate) const SOURCE_PUSH_LAST_NO_VERDICT: &str = r"
select e.kind -> 'detail' -> 'record' ->> 'reason'
  from spawn_events e
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'source.push_no_verdict'
   and e.kind -> 'detail' -> 'record' ->> 'request' = ($1::bigint)::text
 order by e.session_seq desc
 limit 1";

/// How the integrator applies an owner's landing request
/// ([`PostgresStore::source_push_request`]; design-owner-push-between-gates.md).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushRequest {
    /// Judged with the source's gate first, as a judging of its own between
    /// the source's gates; otherwise applied as it is.
    pub gated: bool,
    /// First in line: the judging in flight on the source is stopped.
    pub urgent: bool,
    /// The owner's words, on the record.
    pub reason: Option<String>,
}

// Delivery bookkeeping, run by the dispatcher as the owner.

/// One event of a session, as a task's active time reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEvent {
    /// Its id in the log.
    pub id: i64,
    /// When it was recorded.
    pub at_ms: i64,
    /// Its event name (`tool.started`, `screen.observed`, ...).
    pub name: String,
    /// The tool call's `tool_use_id` or the background job's `job_id`.
    pub key: Option<String>,
}

/// A landing held by [`PostgresStore::landing_hold`] or
/// [`PostgresStore::source_push_hold`]; released when dropped
/// (and with the connection, if the process dies first).
pub struct LandingHold<'store> {
    store: &'store PostgresStore,
}

impl std::fmt::Debug for LandingHold<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LandingHold")
    }
}

impl Drop for LandingHold<'_> {
    fn drop(&mut self) {
        // A failure here is a broken connection, which releases it too.
        let _ = self.store.execute("select spawn_landing_release()", &[]);
    }
}

/// Checked decisions.
impl PostgresStore {
    /// Add a mission; returns its id. `origin` is ignored for the
    /// supervisor's login, which is always recorded as itself.
    pub fn mission_add(
        &self,
        text: &str,
        sources: &Value,
        origin: &str,
    ) -> Result<i64, StoreError> {
        let row = self.query_one(
            "select spawn_mission_add($1, $2, $3)",
            &[&masked(text), sources, &origin],
        )?;
        Ok(row.get(0))
    }

    /// Close a mission as `done` or `dropped`.
    pub fn mission_close(
        &self,
        mission: i64,
        state: &str,
        summary: &str,
    ) -> Result<(), StoreError> {
        self.execute(
            "select spawn_mission_close($1, $2, $3)",
            &[&mission, &state, &masked(summary)],
        )?;
        Ok(())
    }

    /// Give a mission a gate (or take it away) and say how its tasks are
    /// accepted: `review` or `gate`. A human's decision: owner only.
    pub fn mission_configure(
        &self,
        mission: i64,
        gate: Option<&str>,
        accept: &str,
        budget: Option<&Value>,
        objectives: Option<&Value>,
    ) -> Result<(), StoreError> {
        self.execute(
            "select spawn_mission_configure($1, $2, $3, $4, $5)",
            &[&mission, &gate, &accept, &budget, &objectives],
        )?;
        Ok(())
    }

    /// Change only the settings `change` names and keep the rest of the
    /// mission's (owner only; `spawn_mission_reconfigure`, objective 027):
    /// `gate` and `accept` replace theirs, `budget` and `objectives` merge
    /// key by key, a null clears. Returns `{mission, before, after}`; a
    /// change that leaves the settings as they were records nothing.
    pub fn mission_reconfigure(&self, mission: i64, change: &Value) -> Result<Value, StoreError> {
        Ok(self
            .query_one(
                "select spawn_mission_reconfigure($1, $2)",
                &[&mission, change],
            )?
            .get(0))
    }

    /// Assign an objective mission's next task to its resident (owner only).
    pub fn objective_assign(
        &self,
        mission: i64,
        objective: i32,
        scenario: Option<&str>,
        brief: &str,
    ) -> Result<i64, StoreError> {
        self.objective_assign_continuing(mission, objective, scenario, brief, None)
    }

    /// Objective `objective`'s task, continuing the overrun task `continues`
    /// (its work is what the task starts from), or a first task when none.
    pub fn objective_assign_continuing(
        &self,
        mission: i64,
        objective: i32,
        scenario: Option<&str>,
        brief: &str,
        continues: Option<i64>,
    ) -> Result<i64, StoreError> {
        self.objective_assign_classified(
            mission, objective, scenario, brief, continues, "default", 2, None, None,
        )
    }

    /// As [`Self::objective_assign_continuing`], through agent grading
    /// (docs/agent-grading.md), as `task_assign_classified` assigns: on the
    /// model profile `model`, at `difficulty` (1 to 3, `l1` to `l3`), on
    /// the graded preset `preset` the integrator routed it to (`model` its
    /// model profile) or on none, recording the difficulty classifier's
    /// decision (`classification`) when there is one. With `"default"`, 2,
    /// no preset and no classification it is
    /// [`Self::objective_assign_continuing`], record for record.
    #[allow(clippy::too_many_arguments)]
    pub fn objective_assign_classified(
        &self,
        mission: i64,
        objective: i32,
        scenario: Option<&str>,
        brief: &str,
        continues: Option<i64>,
        model: &str,
        difficulty: i32,
        preset: Option<&str>,
        classification: Option<&Value>,
    ) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_objective_assign($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                &[
                    &mission,
                    &objective,
                    &scenario,
                    &masked(brief),
                    &continues,
                    &model,
                    &difficulty,
                    &preset,
                    &classification,
                ],
            )?
            .get(0))
    }

    /// An objective mission's task outcomes, oldest first
    /// ([`OBJECTIVE_OUTCOMES`]).
    pub fn objective_outcomes(&self, mission: i64) -> Result<Value, StoreError> {
        self.query_json(OBJECTIVE_OUTCOMES, &[&mission])
    }

    /// The objective yields not told to the supervisor yet
    /// ([`OBJECTIVE_YIELDS`]).
    pub fn objective_yields(&self) -> Result<Value, StoreError> {
        self.query_json(OBJECTIVE_YIELDS, &[])
    }

    /// The objectives that closed solved on the base and were not told to the
    /// supervisor yet ([`OBJECTIVE_SOLVED_NEWS`]).
    pub fn objective_solved_news(&self) -> Result<Value, StoreError> {
        self.query_json(OBJECTIVE_SOLVED_NEWS, &[])
    }

    /// An objective mission's landed tasks, newest first.
    pub fn objective_landings(&self, mission: i64) -> Result<Value, StoreError> {
        self.query_json(OBJECTIVE_LANDINGS, &[&mission])
    }

    /// Retire an objective by hand (objective 210): one `objective.retired`
    /// through `spawn_objective_retire`, which checks the record's shape,
    /// the mission and the objective's state, and sets `caller` from the
    /// login (the owner's or the supervisor's; any other is refused).
    /// Returns the event's id.
    pub fn objective_retire(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_objective_retire($1)",
                &[&crate::control_mask::masked(record)],
            )?
            .get(0))
    }

    /// Gate verdicts on one objective's tasks, newest first.
    pub fn objective_history(
        &self,
        mission: i64,
        objective: i32,
        limit: i64,
    ) -> Result<Value, StoreError> {
        self.query_json(OBJECTIVE_HISTORY, &[&mission, &objective, &limit])
    }

    /// The earlier tasks of one objective that ended without landing, newest
    /// first, with the work each left ([`OBJECTIVE_PRIOR_WORK`]).
    pub fn objective_prior_work(
        &self,
        mission: i64,
        objective: i32,
        limit: i64,
    ) -> Result<Value, StoreError> {
        self.query_json(OBJECTIVE_PRIOR_WORK, &[&mission, &objective, &limit])
    }

    /// The predecessor's work an ephemeral objective task starts from, or
    /// null ([`OBJECTIVE_FORK`]).
    pub fn objective_fork(&self, task: i64) -> Result<Value, StoreError> {
        self.query_json(OBJECTIVE_FORK, &[&task])
    }

    /// Take the integrator's lock. False means another integrator runs now.
    ///
    /// The lock belongs to the connection, and the connection dies with the
    /// process, so a killed integrator never blocks the next one.
    pub fn integrator_try_lock(&self) -> Result<bool, StoreError> {
        Ok(self
            .query_one(
                "select pg_try_advisory_lock($1, hashtext(current_schema())) as held",
                &[&INTEGRATOR_LOCK_KEY],
            )?
            .get("held"))
    }

    /// Take the integrator's lock on this connection, named `name` first
    /// (`application_name`, objective 221: "spawn integrate pid=<pid>"), so
    /// a refused integrator can say who holds it. False means another
    /// connection holds it.
    pub fn integrator_lock(&self, name: &str) -> Result<bool, StoreError> {
        self.execute("select set_config('application_name', $1, false)", &[&name])?;
        self.integrator_try_lock()
    }

    /// This connection's server pid (`pg_backend_pid()`).
    pub fn backend_pid(&self) -> Result<i32, StoreError> {
        Ok(self.query_one("select pg_backend_pid()", &[])?.get(0))
    }

    /// Who holds the integrator's lock now, read once (objective 221):
    /// `{pid, name, connected_ms}` (the server pid, the connection's
    /// `application_name`, when it connected), or `None` when it is free.
    pub fn integrator_lock_holder(&self) -> Result<Option<Value>, StoreError> {
        Ok(self
            .query_opt(INTEGRATOR_LOCK_HOLDER, &[&INTEGRATOR_LOCK_KEY])?
            .map(|row| row.get::<_, Value>(0)))
    }

    /// Whether this connection holds the integrator's lock, read once
    /// (after a pass that failed, objective 221).
    pub fn integrator_holds_lock(&self) -> Result<bool, StoreError> {
        Ok(self
            .query_one(INTEGRATOR_HOLDS_LOCK, &[&INTEGRATOR_LOCK_KEY])?
            .get(0))
    }

    /// Hold `task`'s landing for its compare-and-swap push (owner only):
    /// `None` when the task is no longer landing or its mission was dropped
    /// (then it is recorded cancelled). Dropping the mission waits until the
    /// hold is dropped, so take it right before the push and keep it until
    /// the landing is recorded.
    pub fn landing_hold(&self, task: i64) -> Result<Option<LandingHold<'_>>, StoreError> {
        let held: bool = self
            .query_one("select spawn_landing_hold($1)", &[&task])?
            .get(0);
        Ok(held.then_some(LandingHold { store: self }))
    }

    /// Resume a mission (owner only): a paused one is unpaused
    /// (`mission.unpaused`, its halt kept), a halted one that is not paused
    /// is resumed (`mission.resumed`).
    pub fn mission_resume(&self, mission: i64) -> Result<(), StoreError> {
        self.execute("select spawn_mission_resume($1)", &[&mission])?;
        Ok(())
    }

    /// Pause a mission (`mission.paused`, objective 099): owner only, never
    /// a closed or an already paused mission. Returns the record's event id.
    pub fn mission_pause(&self, mission: i64, reason: Option<&str>) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_mission_pause($1, $2)",
                &[
                    &mission,
                    &reason.map(|reason| masked_within(reason, REASON_CHARS, true)),
                ],
            )?
            .get(0))
    }

    /// The owner moved `source`'s main from `from` to `to` outside the gate
    /// (owner only; the caller checks `to` descends from `from`). Returns how
    /// many missions took `to` as their baseline. The move is recorded
    /// (`source.moved`) even when none did: the dispatcher tries every spent
    /// delivery once more after it.
    pub fn source_moved(&self, source: &str, from: &str, to: &str) -> Result<i32, StoreError> {
        Ok(self
            .query_one(
                "select spawn_source_moved($1, $2, $3)",
                &[&source, &from, &to],
            )?
            .get(0))
    }

    /// Ask for `commit` on `source`'s main (`spawn mission push`, owner
    /// only): the commit waits in the source at `reference`
    /// (`refs/spawn-owner/<commit>`). Records `source.push_requested` and
    /// returns its event id; the integrator applies it, as `how` says.
    pub fn source_push_request(
        &self,
        source: &str,
        commit: &str,
        reference: &str,
        how: &PushRequest,
    ) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_source_push_request($1, $2, $3, $4, $5, $6)",
                &[
                    &source,
                    &commit,
                    &reference,
                    &how.gated,
                    &how.urgent,
                    &how.reason
                        .as_deref()
                        .map(|reason| masked_within(reason, PUSH_REASON_CHARS, true)),
                ],
            )?
            .get(0))
    }

    /// `source`'s owner landing requests, newest first, at most `limit`, each
    /// with its state and outcome; `commit` (a prefix) keeps that commit's
    /// only ([`SOURCE_PUSHES`]).
    pub fn source_pushes(
        &self,
        source: &str,
        commit: Option<&str>,
        limit: i64,
    ) -> Result<Value, StoreError> {
        self.query_json(SOURCE_PUSHES, &[&source, &commit, &limit])
    }

    /// The owner's landing requests without an outcome yet, oldest first
    /// ([`SOURCE_PUSHES_PENDING`]).
    pub fn source_pushes_pending(&self) -> Result<Value, StoreError> {
        self.query_json(SOURCE_PUSHES_PENDING, &[])
    }

    /// The outcome of the owner's landing request `request`, `None` while it
    /// is pending ([`SOURCE_PUSH_OUTCOME`]).
    pub fn source_push_outcome(&self, request: i64) -> Result<Option<Value>, StoreError> {
        Ok(self
            .query_opt(SOURCE_PUSH_OUTCOME, &[&request])?
            .map(|row| row.get(0)))
    }

    /// The reason of the last `source.push_no_verdict` recorded for the
    /// owner's landing request `request`, `None` when it has none
    /// ([`SOURCE_PUSH_LAST_NO_VERDICT`]).
    pub fn source_push_last_no_verdict(&self, request: i64) -> Result<Option<String>, StoreError> {
        Ok(self
            .query_opt(SOURCE_PUSH_LAST_NO_VERDICT, &[&request])?
            .and_then(|row| row.get(0)))
    }

    /// Hold an owner's landing (owner only): the lock of
    /// [`Self::landing_hold`], so no task landing's push and its record are
    /// split by it. Take it before reading the source's main and keep it
    /// until the outcome is recorded.
    pub fn source_push_hold(&self) -> Result<LandingHold<'_>, StoreError> {
        self.execute("select spawn_source_push_hold()", &[])?;
        Ok(LandingHold { store: self })
    }

    /// Record a gate verdict, a landing, an abort or a halt, or the outcome
    /// of an owner's landing request (`source.moved` naming its `request`,
    /// `source.push_refused`) (owner only). A record the store declines as
    /// already made (a second `task.held` for one task and hold:
    /// [`Self::task_held`]) is an error here.
    pub fn integrator_record(&self, record: &Value) -> Result<i64, StoreError> {
        let record = crate::control_mask::masked(record);
        self.query_one("select spawn_integrator_record($1)", &[&record])?
            .get::<_, Option<i64>>(0)
            .ok_or_else(|| StoreError::Rejected(format!("already recorded: {record}")))
    }

    /// Take the integrator's landing lock for an owner's hold on `source`
    /// (owner only; a source no mission names, or a remote one, is
    /// refused): a landing whose push has begun records its outcome first.
    /// Read the source's main while the guard lives, then record the hold
    /// with [`Self::source_hold`].
    pub fn source_hold_lock(&self, source: &str) -> Result<LandingHold<'_>, StoreError> {
        self.execute("select spawn_source_hold_lock($1)", &[&source])?;
        Ok(LandingHold { store: self })
    }

    /// Record the owner's hold on `source` at `main` (read under
    /// [`Self::source_hold_lock`]) for `bound_s` seconds, with a `reason`:
    /// `{hold, source, main, expires_ms, renews}`. Owner only.
    pub fn source_hold(
        &self,
        source: &str,
        main: &str,
        bound_s: i32,
        reason: Option<&str>,
    ) -> Result<Value, StoreError> {
        Ok(self
            .query_one(
                "select spawn_source_hold($1, $2, $3, $4)",
                &[
                    &source,
                    &main,
                    &bound_s,
                    &reason.map(|reason| masked_within(reason, REASON_CHARS, true)),
                ],
            )?
            .get(0))
    }

    /// Release the active hold on `source`, if there is one:
    /// `{source, released, waiting}` (`released` null: nothing recorded).
    /// Owner only.
    pub fn source_release(&self, source: &str) -> Result<Value, StoreError> {
        Ok(self
            .query_one("select spawn_source_release($1)", &[&source])?
            .get(0))
    }

    /// The active hold on `source` (its `source.held` event id), read from
    /// the fold now: the integrator reads it under the landing lock, right
    /// before a push.
    pub fn active_hold(&self, source: &str) -> Result<Option<i64>, StoreError> {
        Ok(self
            .query_opt(
                "select hold from mission_source_holds where source = $1",
                &[&source],
            )?
            .map(|row| row.get(0)))
    }

    /// Record that `task`'s landing waits behind `hold` on `source` and
    /// pushed nothing (owner only): the event id, or `None` when one was
    /// recorded for this task and hold already (nothing is recorded).
    pub fn task_held(&self, task: i64, hold: i64, source: &str) -> Result<Option<i64>, StoreError> {
        let record = crate::control_mask::masked(&serde_json::json!({"type": "task.held",
            "id": task, "hold": hold, "source": source}));
        Ok(self
            .query_one("select spawn_integrator_record($1)", &[&record])?
            .get(0))
    }

    /// Record that `hold`, the active hold on `source`, reached its
    /// `expires_ms` on the database's clock (owner only); the store names
    /// its bound and the tasks waiting behind it, and refuses it before then
    /// or for a hold that is not active. The record's event id.
    pub fn hold_expired(&self, source: &str, hold: i64) -> Result<i64, StoreError> {
        self.integrator_record(&serde_json::json!({"type": "source.hold_expired",
                                                   "source": source, "hold": hold}))
    }

    /// Missions with a gate and the tasks the integrator may act on: open
    /// ones, and closed ones while the closure rule lets their work go on.
    pub fn gated_missions(&self) -> Result<Value, StoreError> {
        self.query_json(GATED, &[])
    }

    /// The `no_room` gate records that escalate and were not told to the
    /// supervisor yet, or were and still hold ([`GATE_ROOM_ESCALATIONS`]).
    pub fn gate_room_escalations(&self) -> Result<Value, StoreError> {
        self.query_json(GATE_ROOM_ESCALATIONS, &[])
    }

    /// The `host_fault` gate records that escalate and were not told to the
    /// supervisor yet ([`GATE_HOST_FAULT_ESCALATIONS`]).
    pub fn gate_host_fault_escalations(&self) -> Result<Value, StoreError> {
        self.query_json(GATE_HOST_FAULT_ESCALATIONS, &[])
    }

    /// The base disagreements that carry `alert` and were not told to the
    /// supervisor yet ([`GATE_BASE_DISAGREES_ALERTS`]).
    pub fn gate_base_disagrees_alerts(&self) -> Result<Value, StoreError> {
        self.query_json(GATE_BASE_DISAGREES_ALERTS, &[])
    }

    /// The shadow verdicts that disagreed with the primary's on the same
    /// inputs, not told to the supervisor yet ([`SHADOW_DISAGREEMENTS`]).
    pub fn shadow_disagreements(&self) -> Result<Value, StoreError> {
        self.query_json(SHADOW_DISAGREEMENTS, &[])
    }

    /// The red mains that started and the ones that ended, not told to the
    /// supervisor yet ([`MAIN_RED_NEWS`]).
    pub fn main_red_news(&self) -> Result<Value, StoreError> {
        self.query_json(MAIN_RED_NEWS, &[])
    }

    /// The sources away from this host and back, not told to the supervisor
    /// yet ([`SOURCE_AWAY_NEWS`]).
    pub fn source_away_news(&self) -> Result<Value, StoreError> {
        self.query_json(SOURCE_AWAY_NEWS, &[])
    }

    /// The second overrun of an objective's chain that the supervisor was
    /// not told of yet ([`OVER_BUDGET_ESCALATIONS`]).
    pub fn over_budget_escalations(&self) -> Result<Value, StoreError> {
        self.query_json(OVER_BUDGET_ESCALATIONS, &[])
    }

    /// The integrator's expired drains not told to the supervisor yet
    /// ([`DRAIN_EXPIRIES`]).
    pub fn drain_expiries(&self) -> Result<Value, StoreError> {
        self.query_json(DRAIN_EXPIRIES, &[])
    }

    /// The `source.hold_expired` records not told to the supervisor yet,
    /// with their holds and waiting tasks ([`HOLD_EXPIRIES`]).
    pub fn hold_expiries(&self) -> Result<Value, StoreError> {
        self.query_json(HOLD_EXPIRIES, &[])
    }

    /// Every active owner's hold with the tasks waiting behind it, and the
    /// database's clock ([`SOURCE_HOLDS`]).
    pub fn source_holds(&self) -> Result<Value, StoreError> {
        self.query_json(SOURCE_HOLDS, &[])
    }

    /// `session`'s events recorded after `since_ms`, readings left out, in log order
    /// ([`SESSION_ACTIVITY`]).
    pub fn session_activity(
        &self,
        session: &str,
        since_ms: i64,
    ) -> Result<Vec<ActivityEvent>, StoreError> {
        Ok(self
            .query(SESSION_ACTIVITY, &[&session, &since_ms])?
            .iter()
            .map(|row| ActivityEvent {
                id: row.get(0),
                at_ms: row.get(1),
                name: row.get(2),
                key: row.get(3),
            })
            .collect())
    }

    /// Milliseconds the fleet was alive between the log's events `from_id`
    /// and `to_id`, each silence capped at `gap_ms` ([`FLEET_ALIVE`]).
    pub fn fleet_alive_ms(&self, from_id: i64, to_id: i64, gap_ms: i64) -> Result<i64, StoreError> {
        Ok(self
            .query_one(FLEET_ALIVE, &[&from_id, &to_id, &gap_ms])?
            .get(0))
    }

    /// Tool calls `session` started after `since_ms`: a task's budget.
    pub fn tool_calls_since(&self, session: &str, since_ms: i64) -> Result<i64, StoreError> {
        Ok(self
            .query_one(TOOL_CALLS_SINCE, &[&session, &since_ms])?
            .get(0))
    }

    /// The commit of the agent's latest `artifact.committed` event, if it
    /// has one: what its pushed branch must contain to be landed.
    /// When task `task` went over its budget (`[`OVERRUN_AT`]`), or null.
    pub fn overrun_at(&self, task: i64) -> Result<Option<i64>, StoreError> {
        Ok(self
            .query_opt(OVERRUN_AT, &[&task.to_string()])?
            .map(|row| row.get(0)))
    }

    /// An agent's latest checkpoint and when it was recorded
    /// ([`CHECKPOINT_AT`]), or null.
    pub fn checkpoint_at(&self, agent: &str) -> Result<Option<Value>, StoreError> {
        Ok(self
            .query_opt(CHECKPOINT_AT, &[&agent])?
            .map(|row| row.get::<_, Value>(0)))
    }

    pub fn latest_checkpoint(&self, agent: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .query_opt(LATEST_CHECKPOINT, &[&agent])?
            .and_then(|row| row.get::<_, Option<String>>(0)))
    }

    /// Assign a task, run on the model profile `model`; returns its id. The
    /// supervisor's duplicate of a task in flight is refused
    /// ([`Self::task_assign_again`] with `again` false).
    pub fn task_assign(
        &self,
        mission: i64,
        kind: &str,
        worker: &str,
        brief: &str,
        model: &str,
    ) -> Result<i64, StoreError> {
        self.task_assign_again(mission, kind, worker, brief, model, false)
    }

    /// As [`Self::task_assign`]; with `again`, a brief a task of the mission
    /// in flight already has is assigned on purpose, from the supervisor too
    /// (objective 212, `spawn task assign --again`). Whenever there is such
    /// a task, the `task.assigned` record names it (`duplicate_of`).
    pub fn task_assign_again(
        &self,
        mission: i64,
        kind: &str,
        worker: &str,
        brief: &str,
        model: &str,
        again: bool,
    ) -> Result<i64, StoreError> {
        self.task_assign_with(mission, kind, worker, brief, model, again, false)
    }

    /// As [`Self::task_assign_again`]; with `pin`, the task stays with its
    /// worker, never stolen (objective 239, `spawn task assign --pin`).
    #[allow(clippy::too_many_arguments)]
    pub fn task_assign_with(
        &self,
        mission: i64,
        kind: &str,
        worker: &str,
        brief: &str,
        model: &str,
        again: bool,
        pin: bool,
    ) -> Result<i64, StoreError> {
        let row = self.query_one(
            "select spawn_task_assign($1, $2, $3, $4, $5, $6, $7)",
            &[
                &mission,
                &kind,
                &worker,
                &masked(brief),
                &model,
                &again,
                &pin,
            ],
        )?;
        Ok(row.get(0))
    }

    /// Move `task`, queued and unstarted, to the idle resident `to` of its
    /// worker's pool (objective 239, `spawn_task_steal`): `by` names who,
    /// `reason` why. Refused, recording nothing, naming the first rule that
    /// fails. Returns `{task, from_session, to_session, agent_id}`.
    pub fn task_steal(
        &self,
        task: i64,
        to: &str,
        by: &str,
        reason: Option<&str>,
    ) -> Result<Value, StoreError> {
        Ok(self
            .query_one(
                "select spawn_task_steal($1, $2, $3, $4)",
                &[&task, &to, &by, &reason.map(masked)],
            )?
            .get(0))
    }

    /// Accept a report, or send the task back with a note. `revision` names
    /// the revision of the report the reviewer read; `None`, the current one.
    pub fn task_review(
        &self,
        task: i64,
        accept: bool,
        note: &str,
        revision: Option<i32>,
    ) -> Result<(), StoreError> {
        self.execute(
            "select spawn_task_review($1, $2, $3, $4)",
            &[&task, &accept, &masked(note), &revision],
        )?;
        Ok(())
    }

    /// Release `task`, parked by the integrator's load-sensitive bound
    /// (`spawn task release`, owner only): records `task.released` with
    /// `by` and `reason`, and returns what it released, read in the same
    /// statement: `{task, pushed, scenarios, event}`, the parked pushed
    /// commit and every scenario found load-sensitive on it. A task that is
    /// not parked (or none) is refused by the record's check, never a
    /// silent no-op.
    pub fn task_release(&self, task: i64, by: &str, reason: &str) -> Result<Value, StoreError> {
        let record = crate::control_mask::masked(&serde_json::json!({"type": "task.released",
            "id": task, "by": by, "reason": reason}));
        self.query_json(
            "with parked as (
                 select gate_parked_pushed, gate_load_sensitive_scenarios
                   from mission_tasks where id = $1 for update)
             select jsonb_build_object(
                      'task', $1::bigint,
                      'pushed', parked.gate_parked_pushed,
                      'scenarios', parked.gate_load_sensitive_scenarios,
                      'event', spawn_integrator_record($2))
               from (select 1) one left join parked on true",
            &[&task, &record],
        )
    }

    /// Cancel a task still in flight: assigned, delivered, reported or sent
    /// back, or accepted in a gated mission while its gate may still run.
    /// Accepted work without a gate, and cancelled, landing or landed work,
    /// is closed and refused.
    pub fn task_cancel(&self, task: i64) -> Result<(), StoreError> {
        self.execute("select spawn_task_cancel($1)", &[&task])?;
        Ok(())
    }

    /// Re-issue `task`, a first delivery spent without typing (owner only,
    /// the dispatcher): cancel it naming `attempts` (every attempt of its
    /// delivery, in order, each `{command, key, error}`) and assign it again,
    /// in one transaction. Returns the new task's id.
    pub fn task_reissue(&self, task: i64, attempts: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_task_reissue($1, $2)",
                &[&task, &masked_strings(attempts)],
            )?
            .get(0))
    }

    /// What replaced what in `session`: the executions restarts started,
    /// and the reassigns to and from it ([`SESSION_LINKS`]).
    pub fn session_links(&self, session: &str) -> Result<Value, StoreError> {
        self.query_json(SESSION_LINKS, &[&session])
    }

    /// Restart `task`'s worker (`spawn task restart`, the owner and the
    /// supervisor): `task.restarted` names the running execution, the
    /// reason and `n`; the dispatcher stops and resumes it. Only a delivered
    /// task; any other is refused, its state named. Returns `{task,
    /// session_id, agent_id}`.
    pub fn task_restart(&self, task: i64, reason: Option<&str>) -> Result<Value, StoreError> {
        Ok(self
            .query_one(
                "select spawn_task_restart($1, $2)",
                &[
                    &task,
                    &reason.map(|reason| masked_within(reason, RESTART_REASON_BYTES, false)),
                ],
            )?
            .get(0))
    }

    /// Reassign `task` to a fresh worker (`spawn task reassign`, the owner
    /// and the supervisor): cancelled naming `reassigned_as`, assigned again
    /// naming `reassign_of`, in one transaction. Only a delivered task; any
    /// other is refused, its state named. Returns `{task, session_id,
    /// agent_id}` of the new task.
    pub fn task_reassign(&self, task: i64, reason: Option<&str>) -> Result<Value, StoreError> {
        Ok(self
            .query_one(
                "select spawn_task_reassign($1, $2)",
                &[
                    &task,
                    &reason.map(|reason| masked_within(reason, RESTART_REASON_BYTES, false)),
                ],
            )?
            .get(0))
    }

    /// Propose a resident (or revise a pending proposal).
    pub fn resident_propose(&self, name: &str, charter: &str) -> Result<(), StoreError> {
        self.resident_propose_in(name, charter, None)
    }

    /// As [`Self::resident_propose`], in `pool` (objective 239): approved,
    /// its members may take each other's queued tasks.
    pub fn resident_propose_in(
        &self,
        name: &str,
        charter: &str,
        pool: Option<&str>,
    ) -> Result<(), StoreError> {
        self.execute(
            "select spawn_resident_propose($1, $2, $3)",
            &[&name, &masked(charter), &pool],
        )?;
        Ok(())
    }

    /// Every resident, by name: `[{name, agent_id, session_id, state,
    /// pool}]` ([`RESIDENT_ROSTER`]): the dispatcher's retired residents and
    /// pools, in one read.
    pub fn resident_roster(&self) -> Result<Value, StoreError> {
        self.query_json_prepared(RESIDENT_ROSTER, &[])
    }

    /// Approve a proposed resident. Only the owner's login (a human) may.
    pub fn resident_approve(&self, name: &str) -> Result<(), StoreError> {
        self.execute("select spawn_resident_approve($1)", &[&name])?;
        Ok(())
    }

    /// Retire a resident; its work the closure rule drops is cancelled.
    pub fn resident_retire(&self, name: &str) -> Result<(), StoreError> {
        self.execute("select spawn_resident_retire($1)", &[&name])?;
        Ok(())
    }
}

/// Reads for the board, the dispatcher and the supervisor's digest.
impl PostgresStore {
    /// Missions with their tasks, and the resident roster.
    pub fn mission_board(&self, limit: i64) -> Result<Value, StoreError> {
        self.query_json(MISSION_BOARD, &[&limit])
    }

    /// One mission with its tasks by id, not through the board's recency
    /// window. `None` when there is no such mission.
    pub fn mission_by_id(&self, id: i64) -> Result<Option<Value>, StoreError> {
        self.query_opt_json(MISSION_BY_ID, &[&id])
    }

    /// What the dashboard's labels are made of ([`LABEL_FACTS`]).
    pub fn label_facts(&self) -> Result<Value, StoreError> {
        self.query_json(LABEL_FACTS, &[])
    }

    /// What the labels of `tasks` are made of ([`TASK_LABEL_FACTS`]).
    pub fn task_label_facts(&self, tasks: &[i64]) -> Result<Value, StoreError> {
        self.label_facts_of(tasks, &[])
    }

    /// What the labels of `tasks` and the names of `missions` are made of
    /// ([`TASK_LABEL_FACTS`]): each mission named or holding one of the
    /// tasks, with those of its tasks alone.
    pub fn label_facts_of(&self, tasks: &[i64], missions: &[i64]) -> Result<Value, StoreError> {
        self.query_json(TASK_LABEL_FACTS, &[&tasks, &missions])
    }

    /// A task's stages from the control stream, oldest first:
    /// `[{stage, verdict?, at_ms, took_ms?}]`, `took_ms` the time since the
    /// task's previous record. `[]` for a task with no records. The owner's
    /// and the supervisor's logins both read it.
    pub fn task_stages(&self, task: i64) -> Result<Value, StoreError> {
        self.query_json("select spawn_task_stages($1)", &[&task])
    }

    /// The last gate verdict of every task, keyed by task id. Needs read
    /// access to `spawn_events` (the owner, the dashboard's reader).
    pub fn last_gates(&self) -> Result<Value, StoreError> {
        self.query_json(LAST_GATES, &[])
    }

    /// Open objective missions with their backlog directory
    /// ([`OBJECTIVE_BACKLOGS`]).
    pub fn objective_backlogs(&self) -> Result<Value, StoreError> {
        self.query_json(OBJECTIVE_BACKLOGS, &[])
    }

    /// Open objective missions with their resident and their tasks' gate
    /// verdicts: the RSI page.
    pub fn objective_missions(&self) -> Result<Value, StoreError> {
        self.query_json(OBJECTIVE_MISSIONS, &[])
    }

    /// One task with its brief, report and every gate verdict with its log;
    /// `None` when there is no such task.
    pub fn objective_task(&self, task: i64) -> Result<Option<Value>, StoreError> {
        Ok(self.query_one(OBJECTIVE_TASK, &[&task])?.get(0))
    }

    /// One task's brief, report and review note in full ([`TASK_TEXTS`]);
    /// `None` when there is no such task.
    pub fn task_texts(&self, task: i64) -> Result<Option<Value>, StoreError> {
        Ok(self.query_one(TASK_TEXTS, &[&task])?.get(0))
    }

    /// Tasks the dispatcher has to deliver, watch or wind down.
    pub fn delivery_rows(&self) -> Result<Value, StoreError> {
        self.query_json_prepared(DELIVERY_TASKS, &[])
    }

    /// The latest `source.moved` record, `{id, at_ms}`, or null
    /// ([`LATEST_SOURCE_MOVED`]).
    pub fn latest_source_moved(&self) -> Result<Value, StoreError> {
        self.query_json(LATEST_SOURCE_MOVED, &[])
    }

    /// The owner's latest landing on `source`: the `to` of its latest
    /// `source.moved` record, if any ([`SOURCE_OWNER_LANDED`]).
    pub fn source_owner_landed(&self, source: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .query_opt(SOURCE_OWNER_LANDED, &[&source])?
            .and_then(|row| row.get::<_, Option<String>>(0)))
    }

    /// What deploy/on-landing.sh decides on ([`MAIN_LANDING`]): main's
    /// gate-accepted landing on `source`, when the first landing after the
    /// live commit `deployed` was, the source's hold, and how many of its
    /// tasks the gate can still land (`judging`): a gated mission's tasks
    /// between their report and their landing, a reported one only where
    /// the gate accepts (elsewhere a person comes first).
    pub fn main_landing(
        &self,
        source: &str,
        main: &str,
        deployed: Option<&str>,
    ) -> Result<Value, StoreError> {
        Ok(self
            .query_one(MAIN_LANDING, &[&source, &main, &deployed])?
            .get(0))
    }

    /// Record how a deploy ended, `deploy.finished` (owner only; the shape
    /// is checked by `spawn_deploy_record`). Returns the event's id.
    pub fn deploy_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_deploy_record($1)",
                &[&crate::control_mask::masked(record)],
            )?
            .get(0))
    }

    /// The owner's pauses and unpauses the dispatcher acts on, with the
    /// workers each holds ([`MISSION_PAUSES`]).
    pub fn mission_pauses(&self) -> Result<Value, StoreError> {
        self.query_json(MISSION_PAUSES, &[])
    }

    /// Residents whose latest delivered task is cancelled, with the
    /// checkpoint it started from ([`CANCELLED_DELIVERIES`]).
    pub fn cancelled_deliveries(&self) -> Result<Value, StoreError> {
        self.query_json_prepared(CANCELLED_DELIVERIES, &[])
    }

    /// What the dispatcher measures stalls from ([`STALL_FACTS`]).
    pub fn stall_facts(&self) -> Result<Value, StoreError> {
        self.query_json_prepared(STALL_FACTS, &[])
    }

    /// Missions, reports, approvals and human messages for the digest.
    pub fn mission_news(&self) -> Result<Value, StoreError> {
        self.query_json_prepared(MISSION_NEWS, &[])
    }

    /// Approved residents that have run before: the ones the liveness rule
    /// keeps, as (agent, session, charter).
    pub fn residents_to_keep(&self) -> Result<Vec<(String, String, String)>, StoreError> {
        let rows = self.query_prepared(RESIDENTS_TO_KEEP, &[])?;
        Ok(rows
            .iter()
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect())
    }

    /// Residents that are retired, for winding down their sessions.
    pub fn retired_residents(&self) -> Result<Vec<(String, String)>, StoreError> {
        let rows = self.query_prepared(RETIRED_RESIDENTS, &[])?;
        Ok(rows.iter().map(|row| (row.get(0), row.get(1))).collect())
    }

    /// Where an agent's workspace comes from: `None` when no mission owns
    /// the agent, `Some(None)` for a mission without sources (a scratch
    /// workspace), `Some(Some(source))` otherwise. The agent's first task
    /// decides, so a resident's workspace never jumps between sources; an
    /// agent that ran before its first task keeps the default repository it
    /// was mounted from (`None`).
    pub fn agent_source(&self, agent: &str) -> Result<Option<Option<String>>, StoreError> {
        let rows = self.query(AGENT_SOURCE, &[&agent])?;
        Ok(rows.first().map(|row| row.get::<_, Option<String>>(0)))
    }

    /// Agents with an open task, each with its workspace's source (`None`
    /// for a mission without one), as [`Self::agent_source`] decides it.
    pub fn publishing_agents(&self) -> Result<Vec<(String, Option<String>)>, StoreError> {
        Ok(self
            .query(PUBLISHING_AGENTS, &[])?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect())
    }
}

/// Delivery bookkeeping, run by the dispatcher as the owner.
impl PostgresStore {
    /// Record that a task's delivery command was submitted.
    pub fn task_delivered(&self, task: i64, command: i64) -> Result<(), StoreError> {
        self.execute("select spawn_task_delivered($1, $2)", &[&task, &command])?;
        Ok(())
    }

    /// Store a delivered task's report, naming no checkpoint.
    pub fn task_reported(&self, task: i64, report: &str) -> Result<bool, StoreError> {
        let row = self.query_one(
            "select spawn_task_reported($1, $2)",
            &[&task, &masked(report)],
        )?;
        Ok(row.get(0))
    }

    /// Store a delivered task's report with `commit`, the agent's checkpoint
    /// the report's turn ended with (`None`: the agent has none). The
    /// integrator judges the report against it.
    pub fn task_reported_with(
        &self,
        task: i64,
        report: &str,
        commit: Option<&str>,
    ) -> Result<bool, StoreError> {
        let row = self.query_one(
            "select spawn_task_reported($1, $2, $3)",
            &[&task, &masked(report), &commit],
        )?;
        Ok(row.get(0))
    }

    /// Whether a report of `task` with content digest `digest` was already
    /// found unfinished for the task's current revision: its
    /// `task.report_unfinished` record exists.
    pub fn report_unfinished_recorded(&self, task: i64, digest: &str) -> Result<bool, StoreError> {
        Ok(self
            .query_one(
                "select spawn_task_report_unfinished_recorded($1, $2)",
                &[&task, &digest],
            )?
            .get(0))
    }

    /// Record that a delivered task's report was not taken because it is
    /// still a draft: its content's `digest`, the `markers` found, and the
    /// note to its worker (`note`, its key, and `command`). False, and
    /// nothing recorded, when that content was recorded already for the
    /// task's revision, or the task is not delivered.
    pub fn task_report_unfinished(
        &self,
        task: i64,
        digest: &str,
        markers: &Value,
        note: &str,
        command: i64,
    ) -> Result<bool, StoreError> {
        Ok(self
            .query_one(
                "select spawn_task_report_unfinished($1, $2, $3, $4, $5)",
                &[
                    &task,
                    &digest,
                    &masked_strings(markers),
                    &masked(note),
                    &command,
                ],
            )?
            .get(0))
    }

    /// Record that the fork of assigned task `task`'s workspace from task
    /// `from`'s work (`commit`) failed with `error`: once per task. How long
    /// ago its first failure was recorded, in ms (0: now); `None` when the
    /// task is not assigned (nothing recorded).
    pub fn task_fork_failed(
        &self,
        task: i64,
        from: i64,
        commit: &str,
        error: &str,
    ) -> Result<Option<i64>, StoreError> {
        Ok(self
            .query_one(
                "select spawn_task_fork_failed($1, $2, $3, $4)",
                &[&task, &from, &commit, &masked(error)],
            )?
            .get(0))
    }

    /// How many `host_fault` gate verdicts task `task` has at its report
    /// revision `revision` judged against main `base` (the record's `base`:
    /// a fault on an older main says nothing of this one), but a base-only
    /// one (the gate's fact `on` `base`, before any candidate code ran):
    /// read from its control records by the record-id index. The integrator
    /// counts the next one past its cap as an abort.
    pub fn task_host_faults(
        &self,
        task: i64,
        revision: i64,
        base: &str,
    ) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                TASK_HOST_FAULTS,
                &[&task.to_string(), &revision.to_string(), &base],
            )?
            .get(0))
    }

    /// The checkpoint the task's latest report names (`commit` of its latest
    /// `task.reported`): `None` for a report that names none (one recorded
    /// before reports named their checkpoint), or no report.
    pub fn reported_commit(&self, task: i64) -> Result<Option<String>, StoreError> {
        Ok(self
            .query_opt(REPORTED_COMMIT, &[&task.to_string()])?
            .and_then(|row| row.get::<_, Option<String>>(0)))
    }
}

#[cfg(test)]
mod tests {
    use spawn_core::mission::TaskState;

    /// Objective 171: free text for a control record is masked; a reason
    /// within its limit stays within it once masked (in chars or in bytes,
    /// as the store counts it), cut at a char boundary; text already over
    /// its limit is not cut; every string of a JSON value is masked.
    #[test]
    fn text_for_a_control_record_is_masked_within_its_limit() {
        use super::{masked, masked_strings, masked_within};
        use serde_json::json;
        let token = "ghp_171madeup0123456789abcdefghijklmnopq";
        let said = masked(&format!("curl -H 'Authorization: Bearer {token}'"));
        assert!(
            !said.contains(token) && said.contains("[redacted]"),
            "{said}"
        );
        assert_eq!(masked("no secret here"), "no secret here");

        // 494 é and " PWD=x": 500 chars, masked longer, cut back to 500.
        let reason = format!("{} PWD=x", "é".repeat(494));
        assert_eq!(reason.chars().count(), 500);
        let kept = masked_within(&reason, 500, true);
        assert_eq!(kept.chars().count(), 500, "{kept}");
        assert!(!kept.contains("PWD=x"), "{kept}");
        // 997 é and " PWD=x": 2000 bytes, kept within 2000 bytes.
        let reason = format!("{} PWD=x", "é".repeat(997));
        assert_eq!(reason.len(), 2000);
        let kept = masked_within(&reason, 2000, false);
        assert!(kept.len() <= 2000 && kept.len() >= 1998, "{}", kept.len());
        assert!(!kept.contains("PWD=x"));
        // Short text is masked whole, not cut.
        assert_eq!(masked_within("PWD=x", 500, true), masked("PWD=x"));
        // Already over: not cut, so the store refuses it as before.
        let over = format!("{} PWD=x", "r".repeat(600));
        assert_eq!(masked_within(&over, 500, true), masked(&over));
        assert!(masked_within(&over, 500, true).chars().count() > 500);

        let nested = masked_strings(&json!({"attempts": [
            {"command": 3, "error": format!("token {token}")}, "PGPASSWORD=hunter171"],
            "n": 2}));
        let text = nested.to_string();
        assert!(
            !text.contains(token) && !text.contains("hunter171"),
            "{text}"
        );
        assert_eq!(nested["n"], 2);
        assert_eq!(nested["attempts"][0]["command"], 3);
    }

    /// Objective 190: the integrator's polled read of the gated missions
    /// reads each baseline, base and candidate, the red's main and the
    /// gate's two pushed commits (objective 177's) through the commit map
    /// inlined: its plan reads the map's primary key and calls no
    /// `spawn_commit_now`, so no function runs per row.
    #[test]
    fn the_gated_read_reads_the_commit_map_inlined() {
        let schema = crate::testing::TestSchema::new("gated_inlined");
        let store = schema.store();
        let plan = store
            .query_json(
                &format!("explain (verbose, costs off, format json) {}", super::GATED),
                &[],
            )
            .expect("explain")
            .to_string();
        assert!(!plan.contains("spawn_commit_now"), "{plan}");
        assert_eq!(
            plan.matches(r#""Relation Name":"spawn_commit_map""#)
                .count(),
            6,
            "the baseline, the base, the candidate, the red's main and the two pushed: {plan}"
        );
    }

    /// The lead's live size of 22:51: a map of 4,840 pairs (a tenth of
    /// them dropped), 9 gated missions, 398 tasks of which 61 open, half
    /// their commits named by the map; a third of the tasks with a parked push, a quarter with a
    /// load-sensitive one (objective 177's two).
    fn seed_live_size(schema: &crate::testing::TestSchema, store: &crate::PostgresStore) {
        // Every tenth commit dropped by the rewrite (no new id).
        let pairs: Vec<(String, Option<String>)> = (0..4840)
            .map(|i| {
                let new = (i % 10 != 0).then(|| format!("{:040x}", i + 1_000_000));
                (format!("{i:040x}"), new)
            })
            .collect();
        store.commit_map_load(&pairs).expect("the map");
        let mut client = postgres::Client::connect(schema.url(), postgres::NoTls).expect("connect");
        client
            .batch_execute(&format!(
                "set search_path to {name};
                 insert into mission_missions (text, origin, created_at_ms, updated_at_ms, gate, accept, baseline)
                   select 'm' || i, 'human', 0, 0, '/gate', 'gate', lpad(to_hex(i * 2), 40, '0')
                     from generate_series(1, 9) i;
                 insert into mission_tasks (mission_id, kind, agent_id, session_id, brief, state,
                                            created_by, created_at_ms, updated_at_ms, base, candidate)
                   select 1 + i % 9, 'ephemeral', 'agent_worker_t' || i, 'session_worker_t' || i, 'b',
                          case when i <= 61 then 'delivered' else 'landed' end, 'human', 0, 0,
                          lpad(to_hex(i * 4), 40, '0'), lpad(to_hex(i * 4 + 9000), 40, '0')
                     from generate_series(1, 398) i;
                 update mission_tasks set gate_parked_pushed = base where id % 3 = 0;
                 update mission_tasks set gate_load_sensitive_pushed = base where id % 4 = 0;
                 analyze mission_missions; analyze mission_tasks;",
                name = schema.name()
            ))
            .expect("seed");
        client
            .batch_execute("vacuum analyze spawn_commit_map")
            .expect("vacuum");
    }

    /// The gated read's answer with the translations as main reads them
    /// (objective 177's scalar `spawn_commit_now` calls), for comparing.
    fn gated_scalar_form() -> String {
        let mut text = super::GATED.to_string();
        for (lateral, scalar) in [
            (
                "mb.now as baseline",
                "spawn_commit_now(m.baseline) as baseline",
            ),
            ("'base', kb.now", "'base', spawn_commit_now(k.base)"),
            (
                "'candidate', kc.now",
                "'candidate', spawn_commit_now(k.candidate)",
            ),
            (
                "coalesce(rm.now, r.main)",
                "coalesce(spawn_commit_now(r.main), r.main)",
            ),
            (
                "coalesce(kl.now, k.gate_load_sensitive_pushed)",
                "coalesce(spawn_commit_now(k.gate_load_sensitive_pushed), k.gate_load_sensitive_pushed)",
            ),
            (
                "coalesce(kp.now, k.gate_parked_pushed)",
                "coalesce(spawn_commit_now(k.gate_parked_pushed), k.gate_parked_pushed)",
            ),
            (
                "    cross join lateral spawn_commit_now_rows(m.baseline) mb\n",
                "",
            ),
            (
                "           cross join lateral spawn_commit_now_rows(r.main) rm\n",
                "",
            ),
            (
                "        cross join lateral spawn_commit_now_rows(k.base) kb\n",
                "",
            ),
            (
                "        cross join lateral spawn_commit_now_rows(k.candidate) kc\n",
                "",
            ),
            (
                "        cross join lateral spawn_commit_now_rows(k.gate_load_sensitive_pushed) kl\n",
                "",
            ),
            (
                "        cross join lateral spawn_commit_now_rows(k.gate_parked_pushed) kp\n",
                "",
            ),
        ] {
            text = text.replace(lateral, scalar);
        }
        text
    }

    /// A read's answer, the time of the read (`now_ms`) left out, as text,
    /// and its md5: the same bytes from another form or build.
    fn gated_answer(store: &crate::PostgresStore, sql: &str) -> (String, String) {
        let answer = store
            .query_json(
                &format!(
                    "select coalesce(jsonb_agg(e - 'now_ms' order by (e ->> 'id')::bigint), '[]')
                       from jsonb_array_elements(({sql})) e"
                ),
                &[],
            )
            .expect("the gated read")
            .to_string();
        let md5 = store
            .query_one("select md5($1::text)", &[&answer])
            .expect("md5")
            .get(0);
        (answer, md5)
    }

    /// Objective 190: at live size, with dropped commits in the map, the
    /// lateral form answers byte for byte as the scalar calls main makes
    /// (each translation, a dropped id coalesced to itself where 177 reads
    /// it so, a missing one kept, null kept null).
    #[test]
    fn the_gated_read_answers_as_the_scalar_translation_does() {
        let schema = crate::testing::TestSchema::new("gated_same");
        let store = schema.store();
        seed_live_size(&schema, &store);
        let scalar = gated_scalar_form();
        assert!(!scalar.contains("spawn_commit_now_rows"), "{scalar}");
        let (lateral, lateral_md5) = gated_answer(&store, super::GATED);
        let (scalar, scalar_md5) = gated_answer(&store, &scalar);
        assert_eq!(lateral, scalar);
        assert_eq!(lateral_md5, scalar_md5);
        assert!(
            lateral.contains(&format!("{:040x}", 1_000_004)),
            "a translated base read"
        );
    }

    /// Each of `reads` timed (EXPLAIN ANALYZE's execution time), interleaved
    /// so a loaded host weighs on every read alike: 9 rounds, each read once
    /// a round; each read's times printed with their minimum (its own cost)
    /// and median (as the host gave it); the minima returned, NaN for a read
    /// this build does not have.
    fn timed(store: &crate::PostgresStore, reads: &[(&str, String)]) -> Vec<f64> {
        let mut runs: Vec<Vec<f64>> = vec![Vec::new(); reads.len()];
        for _ in 0..9 {
            for (at, (_, sql)) in reads.iter().enumerate() {
                // A form this build does not have (spawn_commit_now_rows
                // before objective 190) is left out.
                if let Ok(explained) = store.query_json(
                    &format!("explain (analyze, timing off, summary on, format json) {sql}"),
                    &[],
                ) {
                    runs[at].push(explained[0]["Execution Time"].as_f64().expect("a time"));
                }
            }
        }
        let mut least = Vec::new();
        for ((name, _), times) in reads.iter().zip(&mut runs) {
            if times.is_empty() {
                println!("{name}: not on this build");
                least.push(f64::NAN);
                continue;
            }
            times.sort_by(f64::total_cmp);
            println!(
                "{name}: min {:.3} ms, median {:.3} ms, of {times:?}",
                times[0], times[4]
            );
            least.push(times[0]);
        }
        least
    }

    /// Objective 190's timing, at live size (the lead's 22:51 reading: 9
    /// missions, 398 tasks of which 61 open, a map of about 4,840 pairs):
    /// EXPLAIN ANALYZE of the real gated read and the same read with the
    /// commits as recorded, 9 interleaved rounds ([`timed`]), each printed;
    /// the translation's part (the least gated minus the least as
    /// recorded) under 1 ms. Ignored: a timing is the proof's
    /// measurement (`cargo test -- --ignored --nocapture`), not a gate check
    /// on a loaded host.
    #[test]
    #[ignore = "a timing: run by hand for the proof"]
    fn the_gated_read_at_live_size_takes_under_a_millisecond() {
        let schema = crate::testing::TestSchema::new("gated_timing");
        let store = schema.store();
        seed_live_size(&schema, &store);
        let tasks = store.gated_missions().expect("the gated read");
        let read_open: usize = tasks
            .as_array()
            .expect("missions")
            .iter()
            .map(|mission| mission["tasks"].as_array().map_or(0, Vec::len))
            .sum();
        assert_eq!(read_open, 61, "the open tasks read");
        // The answer's bytes, to compare with the same test on another
        // build (main's GATED before objective 190).
        println!("gated answer md5 {}", gated_answer(&store, super::GATED).1);
        // The gated read; the same with the commits as recorded (no
        // translation); the open tasks' two commits through each form.
        // The same text with no translation: this build's lateral form
        // (objective 190) or main's scalar calls (before it), so the test
        // copied onto main times main's read alike.
        let mut plain = super::GATED.to_string();
        for (form, recorded) in [
            ("mb.now as baseline", "m.baseline"),
            ("'base', kb.now", "'base', k.base"),
            ("'candidate', kc.now", "'candidate', k.candidate"),
            ("coalesce(rm.now, r.main)", "r.main"),
            (
                "coalesce(kl.now, k.gate_load_sensitive_pushed)",
                "k.gate_load_sensitive_pushed",
            ),
            (
                "coalesce(kp.now, k.gate_parked_pushed)",
                "k.gate_parked_pushed",
            ),
            (
                "    cross join lateral spawn_commit_now_rows(m.baseline) mb\n",
                "",
            ),
            (
                "           cross join lateral spawn_commit_now_rows(r.main) rm\n",
                "",
            ),
            (
                "        cross join lateral spawn_commit_now_rows(k.base) kb\n",
                "",
            ),
            (
                "        cross join lateral spawn_commit_now_rows(k.candidate) kc\n",
                "",
            ),
            (
                "        cross join lateral spawn_commit_now_rows(k.gate_load_sensitive_pushed) kl\n",
                "",
            ),
            (
                "        cross join lateral spawn_commit_now_rows(k.gate_parked_pushed) kp\n",
                "",
            ),
            ("spawn_commit_now(m.baseline) as baseline", "m.baseline"),
            ("coalesce(spawn_commit_now(r.main), r.main)", "r.main"),
            ("spawn_commit_now(k.base)", "k.base"),
            ("spawn_commit_now(k.candidate)", "k.candidate"),
            (
                "coalesce(spawn_commit_now(k.gate_load_sensitive_pushed), k.gate_load_sensitive_pushed)",
                "k.gate_load_sensitive_pushed",
            ),
            (
                "coalesce(spawn_commit_now(k.gate_parked_pushed), k.gate_parked_pushed)",
                "k.gate_parked_pushed",
            ),
        ] {
            plain = plain.replace(form, recorded);
        }
        assert!(
            !plain.contains("spawn_commit_now"),
            "no translation left: {plain}"
        );
        let open = "from mission_tasks k where k.state not in ('cancelled', 'landed')";
        let reads = [
            ("gated", super::GATED.to_string()),
            ("gated, commits as recorded", plain),
            (
                "open tasks, spawn_commit_now",
                format!("select spawn_commit_now(k.base), spawn_commit_now(k.candidate) {open}"),
            ),
            (
                "open tasks, spawn_commit_now_rows",
                format!(
                    "select b.now, c.now {open_from} \
                     cross join lateral spawn_commit_now_rows(k.base) b \
                     cross join lateral spawn_commit_now_rows(k.candidate) c {open_where}",
                    open_from = "from mission_tasks k",
                    open_where = "where k.state not in ('cancelled', 'landed')"
                ),
            ),
            (
                "open tasks, as recorded",
                format!("select k.base, k.candidate {open}"),
            ),
        ];
        let least = timed(&store, &reads);
        assert!(
            least[0] - least[1] < 1.0,
            "the translation's part of the gated read: {least:?}"
        );
    }

    #[test]
    fn mission_by_id_finds_an_old_closed_mission_outside_the_board_window() {
        use serde_json::json;
        let schema = crate::testing::TestSchema::new("mission_by_id");
        let store = schema.store();
        let old = store
            .mission_add("old mission", &json!(["/tmp/old source.git"]), "human")
            .expect("old mission");
        let task = store
            .task_assign(old, "ephemeral", "old-worker", "old brief", "default")
            .expect("old task");
        store
            .mission_close(old, "done", "finished long ago")
            .expect("close old mission");
        for n in 0..20 {
            store
                .mission_add(&format!("newer {n}"), &json!([]), "human")
                .expect("newer mission");
        }
        let board = store.mission_board(20).expect("board");
        assert!(
            board["missions"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|mission| mission["id"] != old)
        );
        let found = store
            .mission_by_id(old)
            .expect("direct read")
            .expect("old mission");
        assert_eq!(found["id"], old);
        assert_eq!(found["text"], "old mission");
        assert_eq!(found["state"], "done");
        assert_eq!(found["sources"], json!(["/tmp/old source.git"]));
        for field in [
            "gate",
            "accept",
            "baseline",
            "objectives",
            "hold",
            "halt",
            "paused",
        ] {
            assert!(found.get(field).is_some(), "missing {field}: {found}");
        }
        assert_eq!(found["tasks"][0]["id"], task);
        assert_eq!(found["tasks"][0]["state"], "cancelled");
        for field in [
            "kind",
            "agent_id",
            "session_id",
            "revisions",
            "candidate",
            "objective",
            "stages",
        ] {
            assert!(
                found["tasks"][0].get(field).is_some(),
                "missing task {field}"
            );
        }
        assert_eq!(store.mission_by_id(i64::MAX).expect("unknown"), None);
    }

    /// The values allowed by the check constraint on `mission_tasks.state`,
    /// read from the schema text.
    fn constrained_task_states() -> Vec<String> {
        let table = crate::SCHEMA
            .split_once("create table if not exists mission_tasks (")
            .expect("the schema creates mission_tasks")
            .1;
        let list = table
            .split_once("check (state in (")
            .expect("mission_tasks.state has a check constraint")
            .1
            .split_once(')')
            .expect("the constraint list is closed")
            .0;
        list.split(',')
            .map(|value| value.trim().trim_matches('\'').to_string())
            .collect()
    }

    #[test]
    fn task_states_match_the_schema_check_constraint() {
        let schema = constrained_task_states();
        let rust: Vec<&str> = TaskState::ALL.iter().map(|state| state.as_str()).collect();
        assert_eq!(schema, rust);
        for value in &schema {
            assert!(value.parse::<TaskState>().is_ok(), "{value} parses");
        }
    }
}
