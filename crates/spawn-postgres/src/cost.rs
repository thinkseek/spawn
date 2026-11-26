//! What the fleet's model calls cost (objective 041): the loop's cost step
//! folds the Bridge's `model.usage` events into `spawn_cost_messages`,
//! priced when recorded (`spawn_cost_record`, owner only); `/api/cost` reads
//! the totals of a window, and the alerts step each budgeted mission's spend
//! of the UTC day. Every sum is the database's (`numeric`): exact, never
//! rounded to cents.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// The messages timed in `[$1, $2)`, summed by model, mission, task, agent
/// and execution (an execution with its models), and `counted_since_ms`:
/// the time of the earliest message the record holds (null: none). `usd` is the sum of the
/// priced messages' costs, 0 for an entry with only unpriced ones (`null`
/// for a model that is priced nowhere in the window); `unpriced_tokens` the
/// four counts of the messages with no price (on every row: a model's, an
/// execution's models' too, 0 when all of them are priced). Most costly
/// first.
///
/// `landed` (objective 060): the tasks that landed or were accepted in the
/// window, as the scorecard counts a landing (`spawn_task_spans`: an
/// assignment closed by `task.landed`, or by an accepting review on a
/// mission without a gate then), each task once however often it was sent
/// back or assigned again; `usd_per_landing` the window's priced dollars
/// over them (null: none landed). Each mission's row carries its own, from
/// its tasks and its dollars.
const WINDOW: &str = r"
with m as materialized (
  select c.*, c.input_tokens + c.output_tokens + c.cache_write_tokens
              + c.cache_read_tokens as total
    from spawn_cost_messages c
   where c.at_ms >= $1 and c.at_ms < $2
), landings as materialized (
  select s.mission_id, count(distinct s.task_id) as landed
    from spawn_task_spans s
   where s.outcome in ('landed', 'accepted')
     and s.closed_at_ms >= $1 and s.closed_at_ms < $2
   group by s.mission_id
), by_execution_model as (
  select execution_id, model, sum(usd) as usd,
         jsonb_build_object('input', sum(input_tokens), 'output', sum(output_tokens),
                            'cache_write', sum(cache_write_tokens),
                            'cache_read', sum(cache_read_tokens)) as tokens,
         coalesce(sum(total) filter (where usd is null), 0) as unpriced_tokens
    from m group by execution_id, model
), execution_models as (
  -- Each execution's models, grouped once and joined below (P30: a
  -- subquery per execution re-read by_execution_model for each of them).
  select execution_id,
         jsonb_agg(jsonb_build_object('model', model, 'usd', usd, 'tokens', tokens,
                                      'unpriced_tokens', unpriced_tokens)
                   order by usd desc nulls last, model) as models
    from by_execution_model group by execution_id
)
select jsonb_build_object(
  'total', (select jsonb_build_object(
              'usd', coalesce(sum(usd), 0),
              'tokens', jsonb_build_object(
                  'input', coalesce(sum(input_tokens), 0),
                  'output', coalesce(sum(output_tokens), 0),
                  'cache_write', coalesce(sum(cache_write_tokens), 0),
                  'cache_read', coalesce(sum(cache_read_tokens), 0)),
              'unpriced_tokens', coalesce(sum(total) filter (where usd is null), 0))
              from m),
  'models', coalesce((select jsonb_agg(jsonb_build_object(
                'model', model, 'usd', usd, 'tokens', tokens,
                'unpriced_tokens', unpriced_tokens)
                order by usd desc nulls last, model)
              from (select model, sum(usd) as usd,
                           jsonb_build_object('input', sum(input_tokens),
                                              'output', sum(output_tokens),
                                              'cache_write', sum(cache_write_tokens),
                                              'cache_read', sum(cache_read_tokens)) as tokens,
                           coalesce(sum(total) filter (where usd is null), 0) as unpriced_tokens
                      from m group by model) g), '[]'::jsonb),
  'landed', (select coalesce(sum(landed), 0) from landings),
  'usd_per_landing', (select coalesce(sum(m.usd), 0)
                             / nullif((select sum(landed) from landings), 0)
                        from m),
  'missions', coalesce((select jsonb_agg(jsonb_build_object(
                'mission', g.mission_id, 'usd', g.usd, 'tokens', g.tokens,
                'unpriced_tokens', g.unpriced_tokens,
                'landed', coalesce(l.landed, 0),
                'usd_per_landing', g.usd / nullif(l.landed, 0))
                order by g.usd desc, g.unpriced_tokens desc, g.mission_id)
              from (select mission_id, coalesce(sum(usd), 0) as usd,
                           jsonb_build_object('input', sum(input_tokens),
                                              'output', sum(output_tokens),
                                              'cache_write', sum(cache_write_tokens),
                                              'cache_read', sum(cache_read_tokens)) as tokens,
                           coalesce(sum(total) filter (where usd is null), 0) as unpriced_tokens
                      from m where mission_id is not null group by mission_id) g
              left join landings l on l.mission_id = g.mission_id), '[]'::jsonb),
  'tasks', coalesce((select jsonb_agg(jsonb_build_object(
                'task', task_id, 'mission', mission_id, 'usd', usd, 'tokens', tokens,
                'unpriced_tokens', unpriced_tokens)
                order by usd desc, unpriced_tokens desc, task_id)
              from (select task_id, max(mission_id) as mission_id, coalesce(sum(usd), 0) as usd,
                           jsonb_build_object('input', sum(input_tokens),
                                              'output', sum(output_tokens),
                                              'cache_write', sum(cache_write_tokens),
                                              'cache_read', sum(cache_read_tokens)) as tokens,
                           coalesce(sum(total) filter (where usd is null), 0) as unpriced_tokens
                      from m where task_id is not null group by task_id) g), '[]'::jsonb),
  'agents', coalesce((select jsonb_agg(jsonb_build_object(
                'agent', agent_id, 'usd', usd, 'tokens', tokens,
                'unpriced_tokens', unpriced_tokens)
                order by usd desc, unpriced_tokens desc, agent_id)
              from (select agent_id, coalesce(sum(usd), 0) as usd,
                           jsonb_build_object('input', sum(input_tokens),
                                              'output', sum(output_tokens),
                                              'cache_write', sum(cache_write_tokens),
                                              'cache_read', sum(cache_read_tokens)) as tokens,
                           coalesce(sum(total) filter (where usd is null), 0) as unpriced_tokens
                      from m group by agent_id) g), '[]'::jsonb),
  'executions', coalesce((select jsonb_agg(jsonb_build_object(
                'execution', g.execution_id, 'agent', g.agent_id, 'task', g.task_id,
                'usd', g.usd, 'tokens', g.tokens, 'unpriced_tokens', g.unpriced_tokens,
                'models', coalesce(e.models, '[]'::jsonb))
                order by g.usd desc, g.unpriced_tokens desc, g.execution_id)
              from (select execution_id, max(agent_id) as agent_id,
                           (array_agg(task_id order by at_ms desc, message_id))[1] as task_id,
                           coalesce(sum(usd), 0) as usd,
                           jsonb_build_object('input', sum(input_tokens),
                                              'output', sum(output_tokens),
                                              'cache_write', sum(cache_write_tokens),
                                              'cache_read', sum(cache_read_tokens)) as tokens,
                           coalesce(sum(total) filter (where usd is null), 0) as unpriced_tokens
                      from m group by execution_id) g
              left join execution_models e on e.execution_id = g.execution_id), '[]'::jsonb),
  'counted_since_ms', (select min(at_ms) from spawn_cost_messages))";

/// [`WINDOW`]'s `total`, `landed` and `usd_per_landing` alone, by the same
/// sums, for a reader that needs no breakdown (the Core lines card's
/// performance line): [`WINDOW`]'s executions with their models run a
/// per-model subquery for each execution, about 0.55 s of its 0.68 s on the
/// live record of 2026-11-12 (258 executions in 7 days), none of it here.
const SUMMARY: &str = r"
with m as materialized (
  select c.usd, c.input_tokens, c.output_tokens, c.cache_write_tokens, c.cache_read_tokens,
         c.input_tokens + c.output_tokens + c.cache_write_tokens
              + c.cache_read_tokens as total
    from spawn_cost_messages c
   where c.at_ms >= $1 and c.at_ms < $2
), landings as materialized (
  select s.mission_id, count(distinct s.task_id) as landed
    from spawn_task_spans s
   where s.outcome in ('landed', 'accepted')
     and s.closed_at_ms >= $1 and s.closed_at_ms < $2
   group by s.mission_id
)
select jsonb_build_object(
  'total', (select jsonb_build_object(
              'usd', coalesce(sum(usd), 0),
              'tokens', jsonb_build_object(
                  'input', coalesce(sum(input_tokens), 0),
                  'output', coalesce(sum(output_tokens), 0),
                  'cache_write', coalesce(sum(cache_write_tokens), 0),
                  'cache_read', coalesce(sum(cache_read_tokens), 0)),
              'unpriced_tokens', coalesce(sum(total) filter (where usd is null), 0))
              from m),
  'landed', (select coalesce(sum(landed), 0) from landings),
  'usd_per_landing', (select coalesce(sum(m.usd), 0)
                             / nullif((select sum(landed) from landings), 0)
                        from m))";

/// The messages timed in `[$1, $2)` in steps of `$3` ms from `$1`, oldest
/// first (GitHub #64, the cost page's trend): each step's `since_ms`, its
/// priced dollars `usd` (0 for a step with none) and its `unpriced_tokens`
/// (the four counts of its messages with no price). A step with no message
/// is there, with zeros; the last step ends at `$2`, so it may be shorter.
const TREND: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
         'since_ms', $1::bigint + s.i * $3::bigint,
         'usd', coalesce(b.usd, 0),
         'unpriced_tokens', coalesce(b.unpriced_tokens, 0))
         order by s.i), '[]'::jsonb)
  from generate_series(0::bigint, ($2::bigint - $1::bigint - 1) / $3::bigint) as s(i)
  left join (select (c.at_ms - $1::bigint) / $3::bigint as i,
                    coalesce(sum(c.usd), 0) as usd,
                    coalesce(sum(c.input_tokens + c.output_tokens + c.cache_write_tokens
                                 + c.cache_read_tokens) filter (where c.usd is null), 0)
                      as unpriced_tokens
               from spawn_cost_messages c
              where c.at_ms >= $1::bigint and c.at_ms < $2::bigint
              group by 1) b on b.i = s.i";

/// The other Claude Code sessions' messages timed in `[$1, $2)` (cost
/// coverage; `spawn_cost_session_messages`, apart from the engines'
/// record): `total` (`usd`, `tokens`, `unpriced_tokens`, as [`WINDOW`]'s),
/// `rows` one per project directory, most costly first, and
/// `counted_since_ms`, the earliest message the sessions' record holds
/// (null: none yet).
const SESSIONS: &str = r"
with m as materialized (
  select c.project, c.usd, c.input_tokens, c.output_tokens, c.cache_write_tokens,
         c.cache_read_tokens,
         c.input_tokens + c.output_tokens + c.cache_write_tokens + c.cache_read_tokens as total
    from spawn_cost_session_messages c
   where c.at_ms >= $1 and c.at_ms < $2
)
select jsonb_build_object(
  'total', (select jsonb_build_object(
              'usd', coalesce(sum(usd), 0),
              'tokens', jsonb_build_object(
                  'input', coalesce(sum(input_tokens), 0),
                  'output', coalesce(sum(output_tokens), 0),
                  'cache_write', coalesce(sum(cache_write_tokens), 0),
                  'cache_read', coalesce(sum(cache_read_tokens), 0)),
              'unpriced_tokens', coalesce(sum(total) filter (where usd is null), 0))
              from m),
  'rows', coalesce((select jsonb_agg(jsonb_build_object(
                'project', project, 'usd', usd, 'tokens', tokens,
                'unpriced_tokens', unpriced_tokens)
                order by usd desc, unpriced_tokens desc, project)
              from (select project, coalesce(sum(usd), 0) as usd,
                           jsonb_build_object('input', sum(input_tokens),
                                              'output', sum(output_tokens),
                                              'cache_write', sum(cache_write_tokens),
                                              'cache_read', sum(cache_read_tokens)) as tokens,
                           coalesce(sum(total) filter (where usd is null), 0) as unpriced_tokens
                      from m group by project) g), '[]'::jsonb),
  'counted_since_ms', (select min(at_ms) from spawn_cost_session_messages))";

/// Every mission with a daily budget (`budget.daily_usd`) and its cost of
/// the messages timed in `[$1, $2)` (the UTC day so far).
const BUDGETS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
         'mission', m.id,
         'budget_usd', m.budget -> 'daily_usd',
         'spent_usd', coalesce((select sum(c.usd) from spawn_cost_messages c
                                 where c.mission_id = m.id
                                   and c.at_ms >= $1 and c.at_ms < $2), 0))
         order by m.id), '[]'::jsonb)
  from mission_missions m
 where jsonb_typeof(m.budget -> 'daily_usd') = 'number'";

impl PostgresStore {
    /// Fold the `model.usage` events not yet folded, pricing each new
    /// message by `prices` (`spawn_cost_record`, owner only): how many
    /// messages it recorded or changed.
    pub fn cost_record(&self, prices: &Value) -> Result<i64, StoreError> {
        let count: i32 = self
            .query_one("select spawn_cost_record($1)", &[prices])?
            .get(0);
        Ok(i64::from(count))
    }

    /// The cost of the messages timed in `[since_ms, until_ms)` (`WINDOW`):
    /// `{total, landed, usd_per_landing, models, missions, tasks, agents,
    /// executions, counted_since_ms}`.
    pub fn cost_window(&self, since_ms: i64, until_ms: i64) -> Result<Value, StoreError> {
        self.query_json(WINDOW, &[&since_ms, &until_ms])
    }

    /// The cost of the messages timed in `[since_ms, until_ms)` without its
    /// breakdown (`SUMMARY`): `{total, landed, usd_per_landing}`, each as
    /// [`Self::cost_window`] gives it.
    pub fn cost_summary(&self, since_ms: i64, until_ms: i64) -> Result<Value, StoreError> {
        self.query_json(SUMMARY, &[&since_ms, &until_ms])
    }

    /// The cost of the messages timed in `[since_ms, until_ms)` in steps
    /// of `step_ms` (`TREND`): `[{since_ms, usd, unpriced_tokens}]`,
    /// oldest first, one per step. An empty window or a step under 1 ms
    /// is no step: `[]`.
    pub fn cost_trend(
        &self,
        since_ms: i64,
        until_ms: i64,
        step_ms: i64,
    ) -> Result<Value, StoreError> {
        if step_ms < 1 || until_ms <= since_ms {
            return Ok(Value::Array(Vec::new()));
        }
        self.query_json(TREND, &[&since_ms, &until_ms, &step_ms])
    }

    /// Each mission with a daily budget and its spend in `[since_ms,
    /// until_ms)` (`BUDGETS`): `[{mission, budget_usd, spent_usd}]`.
    pub fn cost_budgets(&self, since_ms: i64, until_ms: i64) -> Result<Value, StoreError> {
        self.query_json(BUDGETS, &[&since_ms, &until_ms])
    }

    /// The other Claude Code sessions' cost in `[since_ms, until_ms)`
    /// (`SESSIONS`): `{total, rows, counted_since_ms}`.
    pub fn cost_sessions(&self, since_ms: i64, until_ms: i64) -> Result<Value, StoreError> {
        self.query_json(SESSIONS, &[&since_ms, &until_ms])
    }

    /// Record the sessions' `messages` and how far `files` were read, in
    /// one transaction (`spawn_cost_session_record`, owner only), each new
    /// message priced by `prices`.
    pub fn cost_session_record(
        &self,
        prices: &Value,
        messages: &[SessionMessage],
        files: &[SessionFile],
    ) -> Result<SessionCounted, StoreError> {
        let to_value = |what: &str, value: Result<Value, serde_json::Error>| {
            value.map_err(|error| StoreError::Rejected(format!("the {what}: {error}")))
        };
        let messages = to_value("messages", serde_json::to_value(messages))?;
        let files = to_value("files", serde_json::to_value(files))?;
        let answer: Value = self
            .query_one(
                "select spawn_cost_session_record($1, $2, $3)",
                &[prices, &messages, &files],
            )?
            .get(0);
        let count = |name: &str| answer[name].as_i64().unwrap_or_default();
        Ok(SessionCounted {
            counted: count("counted"),
            changed: count("changed"),
            engine: count("engine"),
            invalid: count("invalid"),
        })
    }

    /// Every sessions' transcript file read so far, and how far: by file.
    pub fn cost_session_files(&self) -> Result<Vec<SessionFile>, StoreError> {
        let rows = self.query(
            "select file, inode, offset_bytes, length, modified_ms
               from spawn_cost_session_files order by file",
            &[],
        )?;
        Ok(rows
            .iter()
            .map(|row| SessionFile {
                file: row.get(0),
                inode: row.get(1),
                offset_bytes: row.get(2),
                length: row.get(3),
                modified_ms: row.get(4),
            })
            .collect())
    }

    /// Count archived messages the record lacks
    /// (`spawn_cost_archive_record`, owner only), each priced by `prices`.
    pub fn cost_archive_record(
        &self,
        prices: &Value,
        messages: &[ArchivedMessage],
    ) -> Result<ArchiveCounted, StoreError> {
        let messages = serde_json::to_value(messages)
            .map_err(|error| StoreError::Rejected(format!("the messages: {error}")))?;
        let answer: Value = self
            .query_one(
                "select spawn_cost_archive_record($1, $2)",
                &[prices, &messages],
            )?
            .get(0);
        let count = |name: &str| answer[name].as_i64().unwrap_or_default();
        Ok(ArchiveCounted {
            counted: count("counted"),
            unplaced: count("unplaced"),
        })
    }

    /// The archive files of `agent` the loop counted, and what each was
    /// then: `(file, length, modified_ms)`, by file.
    pub fn cost_archive_files(&self, agent: &str) -> Result<Vec<ArchiveFile>, StoreError> {
        let rows = self.query(
            "select file, length, modified_ms from spawn_cost_archive_files
              where agent_id = $1 order by file",
            &[&agent],
        )?;
        Ok(rows
            .iter()
            .map(|row| ArchiveFile {
                file: row.get(0),
                length: row.get(1),
                modified_ms: row.get(2),
            })
            .collect())
    }

    /// Mark `files` of `agent` counted as they are now; the agent's other
    /// marks (files gone from its directory) are dropped.
    pub fn cost_archive_mark(&self, agent: &str, files: &[ArchiveFile]) -> Result<(), StoreError> {
        let files = serde_json::to_value(files)
            .map_err(|error| StoreError::Rejected(format!("the files: {error}")))?;
        self.execute(
            "with given as (
                 select x.* from jsonb_to_recordset($2::jsonb)
                     as x(file text, length bigint, modified_ms bigint)
             ), gone as (
                 delete from spawn_cost_archive_files f
                  where f.agent_id = $1 and f.file not in (select file from given)
             )
             insert into spawn_cost_archive_files as f
                 (agent_id, file, length, modified_ms, counted_at_ms)
             select $1, g.file, g.length, g.modified_ms, spawn_now_ms() from given g
             on conflict (agent_id, file) do update
                 set length = excluded.length, modified_ms = excluded.modified_ms,
                     counted_at_ms = excluded.counted_at_ms",
            &[&agent, &files],
        )?;
        Ok(())
    }
}

/// One assistant message of the transcript archive, folded over its lines
/// and copies (its largest usage, its earliest line's time and model).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ArchivedMessage {
    /// Agent id that owns the archived transcript.
    pub agent_id: String,
    /// Upstream provider message identifier.
    pub message_id: String,
    /// Model name reported by the provider.
    pub model: String,
    /// Earliest message timestamp in Unix epoch milliseconds.
    pub at_ms: i64,
    /// Input token count.
    pub input: i64,
    /// Output token count.
    pub output: i64,
    /// Prompt-cache creation token count.
    pub cache_write: i64,
    /// Prompt-cache read token count.
    pub cache_read: i64,
}

/// What [`PostgresStore::cost_archive_record`] did: the messages it added,
/// and those it could not place (their agent has no execution).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArchiveCounted {
    /// Number of archived messages inserted or updated.
    pub counted: i64,
    /// Number of archived messages skipped because no execution matched the agent/time.
    pub unplaced: i64,
}

/// An archive file as the loop counted it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ArchiveFile {
    /// Transcript archive relative path (`<agent>/<file>`).
    pub file: String,
    /// Last processed file length in bytes.
    pub length: i64,
    /// File modification timestamp in Unix epoch milliseconds.
    pub modified_ms: i64,
}

/// One assistant message of another Claude Code session on this host, folded
/// over its lines read so far (its largest usage, its earliest line's time
/// and model): `project` is its project directory's name, `session_id` its
/// transcript file's stem.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SessionMessage {
    /// Encoded Claude Code project directory name.
    pub project: String,
    /// Session transcript file stem.
    pub session_id: String,
    /// Upstream provider message identifier.
    pub message_id: String,
    /// Model name reported by the provider.
    pub model: String,
    /// Earliest message timestamp in Unix epoch milliseconds.
    pub at_ms: i64,
    /// Input token count.
    pub input: i64,
    /// Output token count.
    pub output: i64,
    /// Prompt-cache creation token count.
    pub cache_write: i64,
    /// Prompt-cache read token count.
    pub cache_read: i64,
}

/// A sessions' transcript file and how far it was read: `file` is
/// `<project>/<session>.jsonl`, `offset_bytes` the end of its last whole
/// line read, with its inode, length and modification time then.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SessionFile {
    /// Relative path `<project>/<session>.jsonl`.
    pub file: String,
    /// Filesystem inode number at the last scan.
    pub inode: i64,
    /// Byte offset of the end of the last complete line read.
    pub offset_bytes: i64,
    /// File length in bytes at the last scan.
    pub length: i64,
    /// File modification timestamp in Unix epoch milliseconds.
    pub modified_ms: i64,
}

/// What [`PostgresStore::cost_session_record`] did: messages added, held
/// messages that grew or moved earlier, messages left to the engines'
/// record, and lines not counted (no message id, project, session, model or
/// time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionCounted {
    /// Newly recorded external session messages.
    pub counted: i64,
    /// Existing messages whose token counts or timestamps were updated.
    pub changed: i64,
    /// Messages already covered by Spawn's own engine event accounting.
    pub engine: i64,
    /// Lines skipped because required fields could not be parsed.
    pub invalid: i64,
}
