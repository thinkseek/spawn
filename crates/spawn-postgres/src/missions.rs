//! Missions, tasks and residents (docs/fleet-architecture.md §3.2), and
//! the integrator's lock (§10.3).
//!
//! Every decision goes through the checked SQL functions, so the supervisor's
//! restricted login and the owner's CLI share one set of rules. The
//! dispatcher's delivery bookkeeping (delivered, reported) runs as the owner.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// One task the dispatcher has to look at, with what it needs to deliver it.
/// A closed ephemeral task stays for a day, and for as long as its worker
/// still runs: its worker is stopped, and it is managed until then.
pub(crate) const DELIVERY_TASKS: &str = r"
select coalesce(jsonb_agg(row_to_json(t) order by t.id), '[]'::jsonb)
from (
  select k.id, k.mission_id, k.kind, k.agent_id, k.session_id, k.brief, k.state,
         k.revisions, k.review_note, k.delivered_at_ms, k.model,
         m.text as mission_text,
         m.gate is not null as gated,
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
/// - `told`: a notice of the cancel (`cancel-notice-<task>-<n>`) reached the
///   worker, however long ago;
/// - `closed_by` and `reason`: the task's last control record (`task.aborted`
///   carries the integrator's reason).
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
         start.commit as start, aside.commit as set_aside,
         exists (select 1 from spawn_commands c
                  where c.session_id = k.session_id and c.principal = 'spawn-dispatch'
                    and c.idempotency_key like 'cancel-notice-' || k.id || '-%'
                    and c.succeeded) as told,
         last.type as closed_by, last.reason
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
             e.kind -> 'detail' -> 'record' ->> 'reason' as reason
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
///   closed it), and `stale`, the latest `stale` verdict on it, which moves
///   no step;
/// - `verdicts`: per gated mission with a task waiting for the gate, the
///   latest verdict on any of its tasks that is not `stale`;
/// - `sessions`: per session with a delivered task, its latest fact (a
///   record that is neither the Observer's nor storage's, as
///   [`DISPATCH_FACTS`](crate::fleet::DISPATCH_FACTS)'s `evidence_seq`) and
///   the evidence of its latest screen reading;
/// - `halted`: every halted mission with open tasks, its latest
///   `mission.halted` record and those tasks.
pub(crate) const STALL_FACTS: &str = r"
with now as (select spawn_now_ms() as ms),
open as (
  select k.id, k.mission_id, k.kind, k.session_id, k.state, k.revisions,
         m.gate is not null as gated, coalesce(m.accept, 'review') as accept,
         m.halted is not null as halted
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
      'halted', o.halted, 'record', rec.record, 'stale', st.stale) order by o.id)
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
       order by e.session_seq desc limit 1) st on true), '[]'::jsonb),
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
      'at_ms', f.recorded_at_ms, 'age_ms', (select ms from now) - f.recorded_at_ms,
      'screen', sc.evidence) order by s.session_id)
    from (select distinct session_id from open where state = 'delivered') s
    left join lateral (
      select e.session_seq, e.event_name, e.recorded_at_ms from spawn_events e
       where e.session_id = s.session_id and e.source <> 'observer'
         and e.event_name not in ('artifact.committed', 'merge.conflicted')
       order by e.session_seq desc limit 1) f on true
    left join lateral (
      select e.kind -> 'detail' ->> 'evidence' as evidence from spawn_events e
       where e.session_id = s.session_id and e.event_name = 'screen.observed'
       order by e.id desc limit 1) sc on true), '[]'::jsonb),
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

/// The board of missions with their tasks, newest first; each task carries
/// its stages ([`PostgresStore::task_stages`]).
pub(crate) const MISSION_BOARD: &str = r"
select jsonb_build_object(
  'missions', coalesce((select jsonb_agg(row_to_json(x) order by x.id desc) from (
      select m.id, m.text, m.origin, m.sources, m.state, m.summary,
             m.gate, m.accept, m.budget, m.baseline, m.halted, m.objectives,
             m.created_at_ms, m.updated_at_ms,
             coalesce((select jsonb_agg(jsonb_build_object(
                 'id', k.id, 'kind', k.kind, 'agent_id', k.agent_id,
                 'session_id', k.session_id, 'brief', left(k.brief, 600),
                 'state', k.state, 'revisions', k.revisions,
                 'review_note', k.review_note, 'report', k.report,
                 'candidate', k.candidate, 'objective', k.objective, 'metric', k.metric,
                 'model', k.model, 'stages', spawn_task_stages(k.id),
                 'created_by', k.created_by, 'created_at_ms', k.created_at_ms,
                 'updated_at_ms', k.updated_at_ms) order by k.id)
               from mission_tasks k where k.mission_id = m.id), '[]'::jsonb) as tasks
        from mission_missions m
       order by m.id desc limit $1) x), '[]'::jsonb),
  'residents', coalesce((select jsonb_agg(row_to_json(r) order by r.name) from (
      select name, agent_id, session_id, charter, state, proposed_by, approved_by,
             created_at_ms, updated_at_ms from mission_residents) r), '[]'::jsonb)
)";

// Checked decisions, one SQL function each.

/// An objective mission's landed tasks: `[{objective, metric}]`, newest
/// first. `$1` mission.
pub(crate) const OBJECTIVE_LANDINGS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object('objective', k.objective, 'metric', k.metric)
                          order by k.updated_at_ms desc, k.id desc), '[]'::jsonb)
  from mission_tasks k
 where k.mission_id = $1 and k.state = 'landed' and k.objective is not null";

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

/// Missions with a gate that are open, or closed with work the closure rule
/// (`spawn_task_may_continue`) still lets go on, with that work: every task
/// the integrator may judge, land, or hold to its budget, with the brief and
/// the report a landing's message is written from.
pub(crate) const GATED: &str = r"
select coalesce(jsonb_agg(row_to_json(x) order by x.id), '[]'::jsonb)
from (
  select m.id, m.state, m.gate, m.accept, m.budget, m.baseline, m.halted, m.scenario_floor,
         m.gate_rejections, m.gate_aborts, m.gate_room_task, m.gate_room_since_ms,
         m.gate_room_last_ms,
         m.gate_room_waits, m.gate_room_escalated, m.objectives, m.sources ->> 0 as source,
         coalesce(t.tasks, '[]'::jsonb) as tasks
    from mission_missions m
    left join lateral (
      select jsonb_agg(jsonb_build_object(
               'id', k.id, 'agent_id', k.agent_id,
               'session_id', k.session_id, 'state', k.state, 'base', k.base,
               'candidate', k.candidate, 'scenario', k.scenario,
               'delivered_at_ms', k.delivered_at_ms, 'objective', k.objective,
               'revisions', k.revisions, 'brief', k.brief, 'report', k.report,
               'updated_at_ms', k.updated_at_ms)
               order by k.id) as tasks
        from mission_tasks k
        left join mission_residents r on r.agent_id = k.agent_id
       where k.mission_id = m.id
         and k.state not in ('cancelled', 'landed')
         and spawn_task_may_continue(k.state, m.state, true, r.state)) t on true
   where m.gate is not null
     and (m.state in ('open', 'active') or t.tasks is not null)
) x";

/// The `no_room` gate records that carry `escalate` (the mission's gate
/// went without room past the integrator's limit) and that the supervisor
/// was not told of yet, oldest first, each with its `event_id`, `at_ms` and
/// `mission`. Told is a record, not a window: a digest that reached the
/// supervisor carrying the tag `host gate-room-<event_id>` (as
/// [`MISSION_NEWS`]'s `told`, however long ago). So a supervisor that was away
/// for days is still told once.
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
   and not exists (select 1 from told where tag = 'host gate-room-' || e.id)";

/// The last `task.gated` record of every task, keyed by task id:
/// `{"<id>": {verdict, reason, metric, candidate, at_ms}}`. Kept out of
/// [`MISSION_BOARD`]: the supervisor's login reads the board and may not
/// read `spawn_events`.
pub(crate) const LAST_GATES: &str = r"
select coalesce(jsonb_object_agg(z.task, z.gate), '{}'::jsonb)
  from (select distinct on (r ->> 'id') r ->> 'id' as task,
               jsonb_build_object('verdict', r ->> 'verdict', 'reason', r ->> 'reason',
                                  'metric', r ->> 'metric', 'candidate', r ->> 'candidate',
                                  'at_ms', e.recorded_at_ms) as gate
          from spawn_events e
         cross join lateral (select e.kind -> 'detail' -> 'record' as r) g
         where e.session_id = 'session_control' and e.event_name = 'control.recorded'
           and r ->> 'type' = 'task.gated'
         order by r ->> 'id', e.session_seq desc) z";

/// Open objective missions (`objectives` = `{backlog, resident}`), for the
/// RSI page: settings, the resident, and every task newest first with its
/// gate verdicts from the control stream (oldest first, without logs).
pub(crate) const OBJECTIVE_MISSIONS: &str = r"
select coalesce(jsonb_agg(row_to_json(x) order by x.id), '[]'::jsonb)
from (
  select m.id, m.text, m.state, m.sources, m.gate, m.accept, m.budget, m.baseline, m.halted,
         m.scenario_floor, m.gate_rejections, m.gate_aborts, m.objectives,
         m.created_at_ms, m.updated_at_ms,
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

/// The integrator's advisory lock: one integrator per store (database and
/// schema). The two-key form pairs this class with the schema's name, so
/// stores in other schemas of the same database (tests, the e2e) never
/// contend for it.
pub(crate) const INTEGRATOR_LOCK_KEY: i32 = 1_399_957_336;

/// Tool calls a session started after a moment. `$1` session, `$2` ms.
pub(crate) const TOOL_CALLS_SINCE: &str = "select count(*) from spawn_events
                  where session_id = $1 and event_name = 'tool.started' and recorded_at_ms > $2";

/// The commit of an agent's latest checkpoint (or merge of main): its last
/// `artifact.committed` event.
pub(crate) const LATEST_CHECKPOINT: &str = "select kind -> 'detail' -> 'artifact' ->> 'commit'
                   from spawn_events
                  where agent_id = $1 and event_name = 'artifact.committed'
                  order by id desc limit 1";

/// The `commit` of a task's latest `task.reported` control record. `$1`
/// the task id, as text (the record's `id`).
pub(crate) const REPORTED_COMMIT: &str = "select kind -> 'detail' -> 'record' ->> 'commit'
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

// Delivery bookkeeping, run by the dispatcher as the owner.

/// A landing held by [`PostgresStore::landing_hold`]; released when dropped
/// (and with the connection, if the process dies first).
pub struct LandingHold<'store> {
    store: &'store PostgresStore,
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
            &[&text, sources, &origin],
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
            &[&mission, &state, &summary],
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

    /// Assign an objective mission's next task to its resident (owner only).
    pub fn objective_assign(
        &self,
        mission: i64,
        objective: i32,
        scenario: Option<&str>,
        brief: &str,
    ) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_objective_assign($1, $2, $3, $4)",
                &[&mission, &objective, &scenario, &brief],
            )?
            .get(0))
    }

    /// An objective mission's landed tasks, newest first.
    pub fn objective_landings(&self, mission: i64) -> Result<Value, StoreError> {
        self.query_json(OBJECTIVE_LANDINGS, &[&mission])
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

    /// Clear a halted mission. A human's decision: owner only.
    pub fn mission_resume(&self, mission: i64) -> Result<(), StoreError> {
        self.execute("select spawn_mission_resume($1)", &[&mission])?;
        Ok(())
    }

    /// The owner moved `source`'s main from `from` to `to` outside the gate
    /// (owner only; the caller checks `to` descends from `from`). Returns how
    /// many missions took `to` as their baseline.
    pub fn source_moved(&self, source: &str, from: &str, to: &str) -> Result<i32, StoreError> {
        Ok(self
            .query_one(
                "select spawn_source_moved($1, $2, $3)",
                &[&source, &from, &to],
            )?
            .get(0))
    }

    /// Record a gate verdict, a landing, an abort or a halt (owner only).
    pub fn integrator_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one("select spawn_integrator_record($1)", &[record])?
            .get(0))
    }

    /// Missions with a gate and the tasks the integrator may act on: open
    /// ones, and closed ones while the closure rule lets their work go on.
    pub fn gated_missions(&self) -> Result<Value, StoreError> {
        self.query_json(GATED, &[])
    }

    /// The `no_room` gate records that escalate and were not told to the
    /// supervisor yet ([`GATE_ROOM_ESCALATIONS`]).
    pub fn gate_room_escalations(&self) -> Result<Value, StoreError> {
        self.query_json(GATE_ROOM_ESCALATIONS, &[])
    }

    /// Tool calls `session` started after `since_ms`: a task's budget.
    pub fn tool_calls_since(&self, session: &str, since_ms: i64) -> Result<i64, StoreError> {
        Ok(self
            .query_one(TOOL_CALLS_SINCE, &[&session, &since_ms])?
            .get(0))
    }

    /// The commit of the agent's latest `artifact.committed` event, if it
    /// has one: what its pushed branch must contain to be landed.
    pub fn latest_checkpoint(&self, agent: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .query_opt(LATEST_CHECKPOINT, &[&agent])?
            .and_then(|row| row.get::<_, Option<String>>(0)))
    }

    /// Assign a task, run on the model profile `model`; returns its id.
    pub fn task_assign(
        &self,
        mission: i64,
        kind: &str,
        worker: &str,
        brief: &str,
        model: &str,
    ) -> Result<i64, StoreError> {
        let row = self.query_one(
            "select spawn_task_assign($1, $2, $3, $4, $5)",
            &[&mission, &kind, &worker, &brief, &model],
        )?;
        Ok(row.get(0))
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
            &[&task, &accept, &note, &revision],
        )?;
        Ok(())
    }

    /// Cancel a task that is not closed yet.
    pub fn task_cancel(&self, task: i64) -> Result<(), StoreError> {
        self.execute("select spawn_task_cancel($1)", &[&task])?;
        Ok(())
    }

    /// Propose a resident (or revise a pending proposal).
    pub fn resident_propose(&self, name: &str, charter: &str) -> Result<(), StoreError> {
        self.execute("select spawn_resident_propose($1, $2)", &[&name, &charter])?;
        Ok(())
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

    /// Tasks the dispatcher has to deliver, watch or wind down.
    pub fn delivery_rows(&self) -> Result<Value, StoreError> {
        self.query_json(DELIVERY_TASKS, &[])
    }

    /// Residents whose latest delivered task is cancelled, with the
    /// checkpoint it started from ([`CANCELLED_DELIVERIES`]).
    pub fn cancelled_deliveries(&self) -> Result<Value, StoreError> {
        self.query_json(CANCELLED_DELIVERIES, &[])
    }

    /// What the dispatcher measures stalls from ([`STALL_FACTS`]).
    pub fn stall_facts(&self) -> Result<Value, StoreError> {
        self.query_json(STALL_FACTS, &[])
    }

    /// Missions, reports, approvals and human messages for the digest.
    pub fn mission_news(&self) -> Result<Value, StoreError> {
        self.query_json(MISSION_NEWS, &[])
    }

    /// Approved residents that have run before: the ones the liveness rule
    /// keeps, as (agent, session, charter).
    pub fn residents_to_keep(&self) -> Result<Vec<(String, String, String)>, StoreError> {
        let rows = self.query(RESIDENTS_TO_KEEP, &[])?;
        Ok(rows
            .iter()
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect())
    }

    /// Residents that are retired, for winding down their sessions.
    pub fn retired_residents(&self) -> Result<Vec<(String, String)>, StoreError> {
        let rows = self.query(RETIRED_RESIDENTS, &[])?;
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
        let row = self.query_one("select spawn_task_reported($1, $2)", &[&task, &report])?;
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
            &[&task, &report, &commit],
        )?;
        Ok(row.get(0))
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
