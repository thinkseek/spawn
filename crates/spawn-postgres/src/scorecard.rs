//! The scorecard's facts (`spawn scorecard`, `/api/scorecard`; docs/design.md
//! §15): the control records of `session_control` and the command rows, read
//! up to a time and projected to the fields the measures read. Nothing here
//! counts or judges: spawn-cli's `scorecard` module does, from these rows and
//! the archived transcripts, so a reader of the same record gets the same
//! numbers. Both reads are one statement, one snapshot. The reader login
//! (`spawn serve`) may run them: it selects every table.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// The control records recorded before `$1`, in stream order, each projected
/// to `{seq, at, type, ...}`: the fields a measure reads, big text (a brief,
/// a gate's log, a report) reduced to what is compared or read from it. A
/// brief becomes its md5 (a re-issue is found by its brief); a gate's log
/// becomes `stage_ms`, the wall clock its last `| stage gate <result> <ms>ms
/// total` line names (scripts/stage.sh, scripts/rsi/gate.sh). A
/// `legacy.imported` row (from before the control stream) lends its own
/// fields, plus its `state` and times. Records of no measure (a report's text,
/// residents' and halts' records, host facts) are left out; a report is
/// kept as its time (`task.reported`, where a landing's wait for live
/// starts) and a deploy as its `ok` (`deploy.finished`, objective 081).
/// `owner_runs_from_ms` and `live_from_ms` are added from these facts by
/// [`PostgresStore::scorecard_facts`], not read again here (each read was
/// a second pass over the whole control stream).
const FACTS: &str = r"
with h as materialized (
  select u.id, i.intent, i.at_ms
    from spawn_human_intents i
   cross join lateral unnest(i.commands) as u(id)
   where i.at_ms < $1)
select jsonb_build_object(
  -- The newest event of the log in this statement's snapshot (MARK): the
  -- boundary a later kept reading checks is still there (`spawn_events_pkey`
  -- read backward, one row).
  'mark', (select jsonb_build_object('id', m.id, 'at', m.recorded_at_ms,
                                     'session', m.session_id, 'seq', m.session_seq)
             from spawn_events m order by m.id desc limit 1),
  -- Each record's small object is built first and only then sorted and
  -- aggregated: sorted with its record (a gate's log, a brief) the rows
  -- spilled 28 MB to disk on every reading (the lead's plan on the live
  -- copy, 2026-11-13).
  'control', (
    select coalesce(jsonb_agg(q.o order by q.seq), '[]'::jsonb)
      from (
    select e.session_seq as seq, jsonb_strip_nulls(jsonb_build_object(
             'seq', e.session_seq,
             'at', e.recorded_at_ms,
             'type', x.r ->> 'type',
             'id', coalesce(x.r -> 'id', x.r -> 'row' -> 'id'),
             'mission_id', coalesce(x.r -> 'mission_id', x.r -> 'row' -> 'mission_id'),
             'agent_id', coalesce(x.r -> 'agent_id', x.r -> 'row' -> 'agent_id'),
             'session_id', coalesce(x.r -> 'session_id', x.r -> 'row' -> 'session_id'),
             'created_by', coalesce(x.r -> 'created_by', x.r -> 'row' -> 'created_by'),
             'objective', coalesce(x.r -> 'objective', x.r -> 'row' -> 'objective'),
             'brief', to_jsonb(md5(coalesce(x.r ->> 'brief', x.r -> 'row' ->> 'brief'))),
             'verdict', x.r -> 'verdict',
             'accepted', x.r -> 'accepted',
             'ok', x.r -> 'ok',
             'by', x.r -> 'by',
             'gate', coalesce(x.r -> 'gate', x.r -> 'row' -> 'gate'),
             'accept', coalesce(x.r -> 'accept', x.r -> 'row' -> 'accept'),
             'gate_ms', case when jsonb_typeof(x.r -> 'gate_ms') = 'number'
                             then x.r -> 'gate_ms' end,
             -- An owner judging's mission: the one whose gate judged it.
             'mission', case when jsonb_typeof(x.r -> 'mission') = 'number'
                             then x.r -> 'mission' end,
             'stage_ms', (select to_jsonb((s.m[1])::bigint)
                            from regexp_matches(x.r ->> 'log',
                                   '^[ \t]*\| stage gate [^ \t\n]+ ([0-9]{1,15})ms total[ \t\r]*$',
                                   'gn') with ordinality as s(m, i)
                           order by s.i desc limit 1),
             'table', x.r -> 'table',
             'state', x.r -> 'row' -> 'state',
             'created_at_ms', x.r -> 'row' -> 'created_at_ms',
             'updated_at_ms', x.r -> 'row' -> 'updated_at_ms',
             'delivered_at_ms', x.r -> 'row' -> 'delivered_at_ms')) as o
      from spawn_events e
     -- `offset 0` keeps the record a value of its own: inlined, each of the
     -- thirty `x.r` above would take it out of the event's `kind` (a gate's
     -- log, a brief) again, which took most of the statement's time.
     cross join lateral (select e.kind -> 'detail' -> 'record' as r offset 0) x
     where e.session_id = 'session_control' and e.event_name = 'control.recorded'
       and e.recorded_at_ms < $1
       and x.r ->> 'type' in ('mission.added', 'mission.configured', 'mission.resumed',
                              'task.assigned', 'task.delivered', 'task.gated', 'task.landed',
                              'task.reviewed', 'task.cancelled', 'task.aborted',
                              'task.over_budget', 'legacy.imported',
                              'source.moved', 'source.push_refused',
                              'task.reported', 'deploy.finished')
    offset 0) q),
  'commands', (
    select coalesce(jsonb_agg(jsonb_strip_nulls(jsonb_build_object(
             'id', c.id,
             'principal', c.principal,
             'key', c.idempotency_key,
             'session_id', c.session_id,
             'action', c.action ->> 'type',
             'created_at_ms', c.created_at_ms,
             'finished_at_ms', r.finished_at_ms,
             'succeeded', r.succeeded,
             'error', case when r.succeeded is false and c.principal = 'spawn-dispatch'
                                and c.idempotency_key like 'deliver-%' then c.error end,
             'stalls', t.stalls,
             'intent', h.intent,
             'intent_at_ms', h.at_ms))
             order by c.id), '[]'::jsonb)
      from spawn_commands c
     -- A command's result is a fact of the moment it finished: one that
     -- finished at or after `$1` is unfinished in these facts, so the same
     -- `$1` answers the same rows whenever it is asked.
     cross join lateral (
       select case when c.finished_at_ms < $1 then c.finished_at_ms end as finished_at_ms,
              case when c.finished_at_ms < $1 then c.succeeded end as succeeded) r
     cross join lateral (
       select case when c.principal = 'spawn-dispatch' and c.action ->> 'type' = 'input'
                        and jsonb_typeof(c.action -> 'tags') = 'array'
                   then (select jsonb_agg(g.tag order by g.n)
                           from jsonb_array_elements_text(c.action -> 'tags')
                                with ordinality as g(tag, n)
                          where g.tag ~ '(^| )stall-[0-9]+-[a-z_]+-[0-9]+$') end as stalls) t
     left join h on h.id = c.id
     where c.created_at_ms < $1
       and (c.principal in ('human', 'spawn')
            or r.succeeded is false
            or (c.principal = 'spawn-dispatch'
                and (c.idempotency_key like 'deliver-%' or t.stalls is not null)))))";

/// From when the owner's judgings are counted (objective 076): the earliest
/// owner judging that carries its gate's wall clock, over the whole log
/// whatever the window; null when none does. Read only when the facts hold
/// no such judging before their `until` (then the earliest is after it, or
/// there is none); otherwise it is the facts' own earliest.
const OWNER_RUNS_FROM: &str = r"
select min(e.recorded_at_ms)
  from spawn_events e
 cross join lateral (select e.kind -> 'detail' -> 'record' as r offset 0) x
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and x.r ->> 'type' in ('source.moved', 'source.push_refused')
   and jsonb_typeof(x.r -> 'gate_ms') = 'number'";

/// [`FACTS`] for what is new since a kept reading (design-facts-incremental.md,
/// the lead's timing on the live copy): the same projections, the control
/// records only of `seq > $3` (the last kept seq less [`OVERLAP`]) or
/// recorded at or after `$2` (the kept reading's end), the commands only of
/// `id > $4` (the last kept id less [`OVERLAP`]), created at or after `$2`
/// (a database clock stepped back makes a later id an earlier time) or
/// finished at or after `$2` (a finish is the one change a command row
/// gets, and what makes a failed one a fact). The control records are found
/// as two lists of ids, one per index (`spawn_events_session_seq`,
/// `spawn_events_name_time`), and read back from an array of them
/// (`id = any(...)` in the Index Cond of `spawn_events_session_name`, the
/// lead's plan): as one `or`, a generic plan walked every control record,
/// and joined back it scanned the table.
/// It is made from [`FACTS`]'s text, so the projections are one. It also
/// says, in its own snapshot, whether the kept reading's mark is still
/// there (`still`, [`Mark`]).
static FACTS_SINCE: LazyLock<String> = LazyLock::new(|| {
    let control = "      from spawn_events e\n";
    let commands = "     where c.created_at_ms < $1\n";
    let control_where =
        "     where e.session_id = 'session_control' and e.event_name = 'control.recorded'\n";
    let mark = "  'mark', (select";
    assert!(
        FACTS.matches(control).count() == 1
            && FACTS.matches(control_where).count() == 1
            && FACTS.matches(commands).count() == 1
            && FACTS.matches(mark).count() == 1
    );
    FACTS
        .replace(
            mark,
            "  -- STILL: whether the kept reading's mark ($5 id, $6 recorded_at, $7
  -- session, $8 seq) is still that event, in this same snapshot as the
  -- window (`spawn_events_pkey`, one probe).
  'still', exists (select 1 from spawn_events s
                    where s.id = $5 and s.recorded_at_ms = $6
                      and s.session_id = $7 and s.session_seq = $8),
  'mark', (select",
        )
        .replace(
            control,
            "      from spawn_events e
     -- WINDOW: by id, from the two lists (an array, so the rows are index
     -- probes on their ids: joined, a generic plan scanned the table).
     cross join (select array(
              select s.id from spawn_events s
               where s.session_id = 'session_control' and s.session_seq > $3
                 and s.recorded_at_ms < $1
              union
              select s.id from spawn_events s
               where s.event_name = 'control.recorded'
                 and s.recorded_at_ms >= $2 and s.recorded_at_ms < $1) as ids) w\n",
        )
        .replace(
            "     where e.session_id = 'session_control' and e.event_name = 'control.recorded'\n",
            "     where e.id = any(w.ids)
       and e.session_id = 'session_control' and e.event_name = 'control.recorded'\n",
        )
        .replace(
            commands,
            "     where c.created_at_ms < $1
       and (c.id > $4 or c.created_at_ms >= $2 or c.finished_at_ms >= $2)\n",
        )
});

/// How far below the last kept seq (and command id) a reading reads again:
/// a row whose transaction took a lower number and committed after the
/// kept reading (a sequence hands numbers out in order, commits land out of
/// order) is found while fewer than this many were written after it.
const OVERLAP: i64 = 256;

/// How long kept facts are trusted before a reading reads them all again
/// (the lead, on rev-e's review): a row the window cannot see (one that
/// committed more than [`OVERLAP`] numbers late) is then missing for at most
/// this long, against one whole reading (about 150 ms) an hour. Measured
/// on the readings' own ends, so a test passes its clock in.
const FULL_EVERY_MS: i64 = 3_600_000;

/// The newest event of the log a reading saw (its id, recorded_at,
/// session and seq), read in the same snapshot as the facts (cc-astra
/// A46): the one boundary of both kept maps.
///
/// Every write the facts read commits with an event of the log, in the same
/// transaction, under the append lock that hands out event ids
/// (`pg_advisory_xact_lock(1399957335)` held to commit): a control record
/// is its `control.recorded` event, a command's creation commits with
/// `command.accepted` (`spawn_submit`) and its finish with
/// `command.finished` (`spawn_finish`). So events commit in id order, and a
/// restore in place to any earlier point of the log (a point-in-time
/// restore, a dump of an earlier moment loaded under the same name) that
/// takes away anything a kept reading saw (a control record, a command, a
/// finish) takes away that reading's newest event too: the next kept
/// reading finds its mark gone or another event under its id, drops the
/// kept facts and reads them whole.
///
/// It cannot notice, and needs not: a restore to a point after the mark
/// (it takes away nothing kept). It does not notice: a change made around
/// the append lock (a row edited or deleted by hand, which is no restore);
/// a restored log whose event under the mark's id has the same recorded_at
/// in ms, session and seq (another history agreeing on all four). A
/// database dropped and made again under the name is another store by its
/// oid ([`PostgresStore::facts_key`]).
///
/// The check is in the kept reading's own statement ([`FACTS_SINCE`]'s
/// `still`), not a statement before it, so no restore can fall between the
/// check and the window: both see one snapshot. A restore after that
/// statement is the next reading's to notice.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mark {
    id: i64,
    at: i64,
    session: String,
    seq: i64,
}

impl Mark {
    /// The mark of a [`FACTS`] (or [`FACTS_SINCE`]) answer; `None` for an
    /// empty log.
    fn of(read: &Value) -> Option<Self> {
        let mark = &read["mark"];
        Some(Mark {
            id: mark["id"].as_i64()?,
            at: mark["at"].as_i64()?,
            session: mark["session"].as_str()?.to_string(),
            seq: mark["seq"].as_i64()?,
        })
    }
}

/// One store's facts as its last reading left them: its end, its control
/// facts by seq, its command facts by id.
struct Kept {
    until: i64,
    /// The end of the whole reading these facts began with.
    full_until: i64,
    /// The newest event the last reading saw ([`Mark`]).
    mark: Option<Mark>,
    control: BTreeMap<i64, Value>,
    commands: BTreeMap<i64, Value>,
}

impl Kept {
    /// The facts of a [`FACTS`] (or [`FACTS_SINCE`]) answer added to these,
    /// each replacing the one of its seq or id.
    fn merge(&mut self, read: &Value) {
        for (part, key, kept) in [
            ("control", "seq", &mut self.control),
            ("commands", "id", &mut self.commands),
        ] {
            for fact in read[part].as_array().into_iter().flatten() {
                if let Some(at) = fact[key].as_i64() {
                    kept.insert(at, fact.clone());
                }
            }
        }
    }

    /// The facts as [`FACTS`] gives them: each part in seq (id) order.
    fn answer(&self) -> Value {
        serde_json::json!({
            "control": self.control.values().collect::<Vec<_>>(),
            "commands": self.commands.values().collect::<Vec<_>>(),
        })
    }
}

/// The facts each store (by its URL, database, the database's oid and
/// schema) last read, in this process: the dashboard's and the loop's Core
/// lines readings read only what is new since ([`FACTS_SINCE`]). One lock
/// for every store, held through the reading's statements, a whole one
/// included: a process reads one store in production (serve, the loop), so
/// no reading waits on another store's; tests with many stores serialise.
static KEPT: Mutex<BTreeMap<(u64, String), Kept>> = Mutex::new(BTreeMap::new());

impl PostgresStore {
    /// The database's clock (`spawn_now_ms()`), in epoch milliseconds: the
    /// scorecard's "now", never this host's.
    pub fn database_now_ms(&self) -> Result<i64, StoreError> {
        Ok(self.query_one("select spawn_now_ms()", &[])?.get(0))
    }

    /// The scorecard's facts recorded before `until_ms`, in one snapshot:
    /// `{control: [...], commands: [...]}` ([`FACTS`]). `commands` holds the
    /// rows a measure reads: the owner's (`human`, `spawn`), every failed
    /// one, the dispatcher's deliveries (`deliver-<task>-<revision>` and their
    /// retries; `error` only on a failed one) and its digests that carry a
    /// stall tag (`stalls`: those tags, `<session> stall-<task>-<step>-<n>`).
    /// A person's command names its intent (`spawn_human_intents`: `intent`,
    /// its first command's id, and `intent_at_ms`, that one's creation): a
    /// refused command sent again continues the intent of the one before.
    /// A command is in them from its creation and its result (`succeeded`,
    /// `finished_at_ms`, `error`) from its finish: one created before
    /// `until_ms` that finished later is unfinished here.
    ///
    /// `live_from_ms` (the first `deploy.finished` before `until_ms`) and
    /// `owner_runs_from_ms` (the first owner judging with its gate's wall
    /// clock, whatever `until_ms`) are taken from the control facts, which
    /// hold every such record before `until_ms`, not read again; the latter
    /// is read ([`OWNER_RUNS_FROM`]) only when none is before `until_ms`.
    ///
    /// The facts are kept in this process, by store: a reading at or after
    /// the kept one's end reads only what is new since ([`FACTS_SINCE`]); an
    /// earlier end, a first reading or one after a failed one reads them all
    /// ([`FACTS`]), and an earlier end leaves the kept facts as they were.
    /// Kept facts are read again whole when their whole reading's end is an
    /// hour ([`FULL_EVERY_MS`]) behind, or when the newest event the last
    /// reading saw is no longer there ([`Mark`]: a restore in place).
    pub fn scorecard_facts(&self, until_ms: i64) -> Result<Value, StoreError> {
        self.scorecard_facts_with_overlap(until_ms, OVERLAP)
    }

    /// [`Self::scorecard_facts`] with the window's overlap given: the tests'
    /// seam, so a short history still has rows below the window (rev-e's
    /// F1). Every other caller uses [`OVERLAP`].
    #[doc(hidden)]
    pub fn scorecard_facts_with_overlap(
        &self,
        until_ms: i64,
        overlap: i64,
    ) -> Result<Value, StoreError> {
        let key = self.facts_key()?;
        let mut all = KEPT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut kept = all.remove(&key);
        if let Some(stale) = &kept
            && until_ms >= stale.until
            && until_ms.saturating_sub(stale.full_until) >= FULL_EVERY_MS
        {
            kept = None;
        }
        let mut facts = match kept {
            Some(kept) if until_ms < kept.until => {
                let facts = self.query_json(FACTS, &[&until_ms]);
                all.insert(key, kept);
                facts?
            }
            Some(mut kept) => 'since: {
                let Some(mark) = kept.mark.clone() else {
                    // Kept from an empty log: nothing to check against.
                    let (read, whole) = self.facts_whole(until_ms)?;
                    all.insert(key, whole);
                    break 'since read;
                };
                let since_seq = kept
                    .control
                    .keys()
                    .next_back()
                    .map_or(i64::MIN, |seq| seq - overlap);
                let since_id = kept
                    .commands
                    .keys()
                    .next_back()
                    .map_or(i64::MIN, |id| id - overlap);
                let read = self.query_json(
                    &FACTS_SINCE,
                    &[
                        &until_ms,
                        &kept.until,
                        &since_seq,
                        &since_id,
                        &mark.id,
                        &mark.at,
                        &mark.session,
                        &mark.seq,
                    ],
                )?;
                if read["still"] != true {
                    // The mark is gone: a restore took away something kept.
                    let (read, whole) = self.facts_whole(until_ms)?;
                    all.insert(key, whole);
                    break 'since read;
                }
                kept.merge(&read);
                kept.mark = Mark::of(&read);
                kept.until = until_ms;
                let facts = kept.answer();
                all.insert(key, kept);
                facts
            }
            None => {
                let (read, whole) = self.facts_whole(until_ms)?;
                all.insert(key, whole);
                read
            }
        };
        drop(all);
        if let Some(object) = facts.as_object_mut() {
            object.remove("mark");
            object.remove("still");
        }
        let earliest = |types: &[&str], gated: bool| {
            facts["control"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|fact| {
                    fact["type"]
                        .as_str()
                        .is_some_and(|kind| types.contains(&kind))
                        && (!gated || fact["gate_ms"].is_number())
                })
                .filter_map(|fact| fact["at"].as_i64())
                .min()
        };
        let live_from = earliest(&["deploy.finished"], false);
        let owner_runs_from = match earliest(&["source.moved", "source.push_refused"], true) {
            Some(at) => Some(at),
            None => self.query_one(OWNER_RUNS_FROM, &[])?.get(0),
        };
        facts["live_from_ms"] = serde_json::json!(live_from);
        facts["owner_runs_from_ms"] = serde_json::json!(owner_runs_from);
        Ok(facts)
    }

    /// The statement a kept reading runs ([`FACTS_SINCE`]): `$1` the end,
    /// `$2` the kept reading's end, `$3` and `$4` the last kept seq and
    /// command id less the overlap, `$5` to `$8` the kept mark's id,
    /// recorded_at, session and seq ([`Mark`]). For timing it at live size, and for the
    /// tests that read its plan.
    pub fn scorecard_facts_since_statement() -> &'static str {
        &FACTS_SINCE
    }

    /// The facts before `until_ms` read whole ([`FACTS`]), and kept from
    /// them, with the mark of the same snapshot.
    fn facts_whole(&self, until_ms: i64) -> Result<(Value, Kept), StoreError> {
        let read = self.query_json(FACTS, &[&until_ms])?;
        let mut kept = Kept {
            until: until_ms,
            full_until: until_ms,
            mark: Mark::of(&read),
            control: BTreeMap::new(),
            commands: BTreeMap::new(),
        };
        kept.merge(&read);
        Ok((read, kept))
    }

    /// Which store this is, for [`KEPT`]: a hash of the URL it connected
    /// with (never kept itself), its database, that database's oid (a
    /// database dropped and made again under the name is another store) and
    /// its schema.
    fn facts_key(&self) -> Result<(u64, String), StoreError> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        self.url.hash(&mut hasher);
        let place: String = self
            .query_one(
                "select current_database() || '.'
                        || (select d.oid from pg_database d
                             where d.datname = current_database())::text
                        || '.' || current_schema()",
                &[],
            )?
            .get(0);
        Ok((hasher.finish(), place))
    }

    /// When the live record starts: the first `deploy.finished` of any
    /// outcome (objective 095's record start), or `None` without one.
    pub fn first_deploy_finished_ms(&self) -> Result<Option<i64>, StoreError> {
        Ok(self
            .query_one(
                "select min(e.recorded_at_ms) from spawn_events e
                  where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                    and e.kind -> 'detail' -> 'record' ->> 'type' = 'deploy.finished'",
                &[],
            )?
            .get(0))
    }
}
