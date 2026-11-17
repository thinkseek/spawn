//! Alerts: what Spawn tells about itself.
//!
//! The supervisor once ran a hand-written monitor that printed into one
//! pane; nothing of it was recorded, its state lived in its process, and no
//! view of Spawn showed an alert. The loop now raises its own
//! (docs/fleet-architecture.md, "Alerts"): each transition is a control
//! record, `alert.fired` or `alert.cleared`, written through the owner-only
//! `spawn_alert_record`, folded into `spawn_alerts` and shown by
//! `spawn alerts --once`, `spawn alerts --follow` and `GET /api/alerts`.
//!
//! An alert fires once per occurrence and clears at most once, whatever
//! restarts: the key names the occurrence, the store refuses a second
//! `alert.fired` for a key, and the step reads the keys back on start, so a
//! restarted loop fires nothing twice and still knows what is open. It
//! clears only on evidence after what it fired on (objective 050; [`after`]):
//! a record later in the log than the alert's own `event_id`, or a reading
//! at or after its `reading_ms`. A restart reads the last week of records
//! again, and an older one clears nothing; the records a pass reads are
//! recorded one at a time, so a halt and its resume in one read end the
//! alert cleared, naming the resume. A
//! notice is an `alert.fired` with `notice: true`: it is news, never a
//! condition, and never clears.
//!
//! Every alert rests on a named record (its `event_id`) or on a recorded
//! reading (its measure and threshold): a task's state change, a mission's
//! counters, a judgement, an admission's wait, a backup's age, a crash line
//! in the database's log, MemAvailable under its threshold, the used space
//! of the filesystem holding `SPAWN_ALERT_TMPFS_PATH`. Nothing rests on the
//! step's memory (it is rebuilt from the record on start), on absence (no
//! backup record means no alert, never "no backup"), or on time without a
//! record.
//!
//! The kinds, their keys, when they fire and clear:
//!
//! Notices (`notice: true`, never cleared), for every mission:
//! - `task_state`, `task #<id>`, key `task_state:<task>:<event>`: a control
//!   record after which the task's state differs from before it (its
//!   `task.assigned`: from none). A record that leaves it as it was (an
//!   `aborted` or `stale` gating) is no notice.
//! - `mission_state`, `mission #<id>`, key `mission_state:<mission>:<event>`:
//!   a record after which any of the mission's `state`, `halted`,
//!   `gate_rejections`, `gate_aborts`, `gate_room_escalated` differs. The
//!   step folds `gate_rejections` as the store does, with the run's
//!   objective (`gate_rejection_objective`, read back on start): a cancel of
//!   a task of the objective every rejection of the run was ends the run
//!   (objective 053).
//! - `gate_base_disagrees`, `mission #<id>`, key
//!   `gate_base_disagrees:<event>`: a `task.gated` host fault with
//!   `base_disagrees` and `alert` true (objective 033).
//! - `command_failed`, `command #<id>`, key `command_failed:<command>`: a
//!   command that finished failed, an owner's note (`owner-note-…`, which a
//!   person sent) excepted.
//!
//! Conditions:
//! - `scenario_red`, `mission #<id>`, key
//!   `scenario_red:<mission>:<scenario>:<the first task.gated naming it>`:
//!   a gate of the mission found main red on the scenario (the record's
//!   `main_red_scenarios`: solved, as the same scenario, on an ancestor of
//!   the base, and unsolved on it; scripts/rsi/gate.sh, "Main red on a
//!   scenario"), where an unsolved objective says nothing. Once per mission
//!   and scenario while it stays red; clears at the first later `accepted`
//!   gating of the mission that does not name it (an accepted gate judged
//!   every scenario on the base; a rejection may come before the base, so
//!   it clears nothing).
//! - `validator_defect`: a validator (a regression) that fails on the base
//!   too, run again in the gate (scripts/rsi/gate.sh, "Three classes of a
//!   failing regression"), in either of two records, each its own alert
//!   (evidence `owner`, and `missions`, the missions it is about):
//!   - a task's `task.gated` `host_fault` with evidence `validator_defect`,
//!     `mission #<id>`, key `validator_defect:<mission>:<scenario>:<event>`:
//!     no candidate that fails it gets a verdict. Also clears at the
//!     mission's next `accepted` or `rejected` gating;
//!   - an owner's accepted landing (`source.moved` with
//!     `validator_defects`), `<source> (mission #<id>, ...)`, key
//!     `validator_defect:<source>:<scenario>:<event>`: one per source and
//!     scenario, about the moved missions that are not closed (none: no
//!     alert), naming `source` and `missions` (objective 056).
//!
//!   Either clears from the record (`cleared_evidence` `cause` and
//!   `event_id`): `passed`, a later gating of a mission it names (or a
//!   landing of its source) lists the scenario in `passed` or `reused`;
//!   `gone`, a later gating of a mission it names carries `scenario_names`
//!   without it, or at a reading no gate directory of an open mission on its
//!   source holds it (`reading_ms` and `gates`, no `event_id`); `closed`, every mission it names is closed; `quarantined`,
//!   a gate names it so.
//! - `task_parked`, `task #<id>`, key `task_parked:<task>:<event>`
//!   (sync/design-load-sensitive-bound.md): the integrator's load-sensitive
//!   bound parked the task, its `task.gated` `host_fault` with evidence
//!   `load_sensitive_parked`: its pushed commit was load-sensitive twice, so
//!   it is not judged again until a new commit is pushed or a person
//!   releases it. Names the scenarios and each one's root-fix owner, and the
//!   command that releases it (`spawn task release`). One open per task.
//!   Clears at the task's `task.released`, at a later verdict on it but
//!   `no_room` and `stale` (which judge nothing, as the store's fold keeps
//!   it parked then), or at its `task.landed`, `task.cancelled`,
//!   `task.aborted` or `task.over_budget` (each ends the task, and the
//!   store's fold takes it out of the bound); and at start, when the mirror
//!   finds the task no longer parked by that record (one parked while the
//!   step was down fires then).
//! - `task_undelivered`, `task #<id>`, key
//!   `task_undelivered:<task>:<its task.assigned event>`: in `assigned`
//!   longer than `SPAWN_ALERT_UNDELIVERED_SECS` (default 300) while its
//!   agent holds no delivered or reported task; clears at the record that
//!   moves it.
//! - `mission_halted`, `mission #<id>`, key
//!   `mission_halted:<mission>:<its mission.halted event>`: clears at the
//!   mission's `mission.resumed`.
//! - `postgres_crash`, `test` or `live`, key
//!   `postgres_crash:<subject>:<the line's stamp and [pid] as written, its
//!   spaces folded out so the key is one word>`: a
//!   log line holding `terminated by signal`, `No space left` or `could not
//!   resize`, stamped within the day; clears at the first `database system
//!   is ready to accept connections` line after it, or after
//!   `SPAWN_ALERT_PG_QUIET_SECS` (default 900) with no such line.
//! - `source_away`, `mission #<id>`, key `source_away:<mission>:<reading
//!   ms>`, evidence its `source`: at two readings in a row, a source of an
//!   open or active mission that is a path on this host (absolute) is not a
//!   directory: an unmounted volume, a moved checkout (one reading only
//!   notes it, so a path that comes and goes does not flap). Its agents' branches
//!   are not published to it until it is back (the pusher retries, and says
//!   so only once, in its log: fix-core-b item 6). One per mission and
//!   source while it is away; clears at the first reading that finds the
//!   directory, or once the mission no longer has the source or is no
//!   longer open or active. The supervisor hears of each, and of its end
//!   (dispatch's digest, `source_away_news`).
//! - `memory_low`, `host`, key `memory_low:<reading ms>`: MemAvailable
//!   under `SPAWN_ALERT_MEMORY_LOW_MB` (default 4096); clears at or above
//!   `SPAWN_ALERT_MEMORY_OK_MB` (default 8192), the margin that keeps a host
//!   hovering at the threshold from flapping.
//! - `tmpfs_high`, `host`, key `tmpfs_high:<reading ms>`: the used space of
//!   the filesystem holding `SPAWN_ALERT_TMPFS_PATH` (default `/tmp`) over
//!   `SPAWN_ALERT_TMPFS_HIGH_MB` (default 8000); clears under
//!   `SPAWN_ALERT_TMPFS_OK_MB` (default 6000). A threshold of 0 disables its
//!   check.
//! - `disk_low`, `host`, key `disk_low:<reading ms>` (objective 204): the
//!   free space of the filesystem holding the workspace root
//!   (`SPAWN_WORKSPACE_ROOT`, the Core lines' free disk) under
//!   `SPAWN_ALERT_DISK_LOW_GB` GiB; clears at or above
//!   `SPAWN_ALERT_DISK_OK_GB` (default 45). Off by default (0): the gate's
//!   scenarios run on a 15 GiB tmpfs, so the live loop's unit sets 40 and
//!   45. No workspace root, nothing read. Apart from the loop's own disk
//!   watch (crate disk.rs: `host.disk` under `SPAWN_DISK_WARN_GB`, dispatch
//!   held under `SPAWN_DISK_FLOOR_GB`), which it leaves as it is.
//! - `database_size`, `database <name>`, key `database_size:<name>:<reading
//!   ms>` (objective 074): every `SPAWN_ALERT_DATABASE_SECS` (default 600)
//!   the step reads the live database's size and its event log's bytes (the
//!   sum of `spawn_events.kind`'s stored size), and the size and schemas
//!   (other than `pg_*`, `information_schema` and `public`) of each test
//!   database it is given: the gates' (`SPAWN_TEST_DATABASE_URL`, role
//!   `test`) and the engines' (`SPAWN_FLEET_TEST_DATABASE_URL`, role
//!   `fleet_test`), each when set. A database at or above
//!   `SPAWN_ALERT_DATABASE_FLOOR_MB` (default 1024) and above
//!   `SPAWN_ALERT_DATABASE_RATIO` (default 10) times the log's bytes fires,
//!   evidence `{database, role, bytes, log_bytes, ratio, schemas}`; one open
//!   per database. Clears at the first reading that finds it no longer so,
//!   that reading its evidence. A test database that cannot be read is a
//!   line; its alert stays as it was. The fleet's test database once grew to
//!   2.7 GB (2533 schemas tests left) unmeasured, and was read as live bloat.
//! - `guardrail_out_of_range`, `guardrail <figure key>`, key
//!   `guardrail_out_of_range:<figure key>:<out since ms>` (objective 089):
//!   every `SPAWN_ALERT_GUARDRAIL_SECS` (default 300) the step reads the
//!   Core lines figures `GET /api/core-lines` gives, by the same code, the
//!   ranges file read at each reading. A figure out of its range at every
//!   reading for `SPAWN_GUARDRAIL_RAISE_SECS` (default 7200) fires, evidence
//!   `{line, key, name, value, range, out_since_ms, reading_ms}`; one open
//!   per figure. Clears once not out at any reading for
//!   `SPAWN_GUARDRAIL_CLEAR_SECS` (default 3600), evidence `{key, state,
//!   value, range, reading_ms}`, or at the next reading when the figure is
//!   gone (`state` `gone`). The holds live in the step's memory
//!   (alerts/guardrails.rs).
//! - `agent_stuck`, the session id, key `agent_stuck:<session>:<its
//!   observation>`: the session's latest `observation.recorded` says `stuck`
//!   at or above [`LIKELY`] (0.5), whatever the dispatcher does with it; a
//!   later observation still stuck fires nothing, and a session whose
//!   current execution has ended fires nothing; clears when the latest
//!   one is under 0.5, or when the session's current execution has ended,
//!   whatever the cause (stopped, exited, lost), with that end as evidence.
//!   An alert is a claim that someone must act (objective 057): none fires
//!   for an ephemeral worker (`agent_worker_*`; a resident outlives its
//!   tasks and takes other work) whose task is past its part (reported, gated, accepted,
//!   landing or held, landed: it waits on the gate, a reviewer or the
//!   owner), its open one clearing at the task record that moved it (that
//!   record's id its `event_id`), nor for the supervisor with nothing to
//!   dispatch (no question to the owner, no pending or told escalation of
//!   the attention view but a worker's own judgement while it holds its
//!   task, no mission of its own waiting for its first task), its open one
//!   clearing at the first pass that finds so. A worker's alert names its
//!   `task` and `command`, the one command that acts on it: `spawn task
//!   restart --id <task>` (a delivered task's).
//! - `judge_down`, the judge's name (`jev`), key `judge_down:<its
//!   host.judge down event>`: the model judge's watcher recorded `down` (two
//!   calls in a row failed after its last answer, crate judge_watch.rs) and
//!   no `up` after it, `SPAWN_ALERT_JUDGE_DOWN_SECS` (default 900) after the
//!   first of them failed (`first_failed_ms`): no call answered since. It
//!   rests on that record, not on the absence of judgements (a judge that
//!   is down writes few: the Observer records a failure once, not each
//!   retry). Clears at the `up` record, the first call answered, with its
//!   `failed_calls`; an `up` whose `down` is older than the history (a loop
//!   restarted during a week-long outage) clears the judge's open alert.
//! - `admission_wait`, an execution's `agent_id` or a gate's `task #<id>`,
//!   key `admission_wait:<its admission.waiting event>`: an
//!   `admission.waiting` (029) older than `SPAWN_ALERT_ADMISSION_SECS`
//!   (default 600) with no `admission.admitted` of the same kind and agent
//!   or task after it; clears at that admitted.
//! - `backup_stale`, `dump` or `bundle`, key
//!   `backup_stale:<subject>:<the newest backup's event>`: the newest
//!   `backup.dumped` or `backup.bundled` older than
//!   `SPAWN_ALERT_BACKUP_DUMP_MAX_AGE_SECS` (default 7200, the hourly dump's
//!   schedule twice) or `SPAWN_ALERT_BACKUP_BUNDLE_MAX_AGE_SECS` (default
//!   93600, the daily bundle's day plus two hours); with no such record at
//!   all there is no alert; clears at a newer record, which growing old is a
//!   new occurrence.
//! - `backup_budget`, `r2`, key `backup_budget:<its record>`: a `backup.*`
//!   record with `over_alert` true (031) while none is open; clears at the
//!   first later one with `over_alert` false.
//! - `drill_failed`, `drill`, key `drill_failed:<its record>`: a
//!   `backup.verified` with `ok` false; clears at the next with `ok` true.
//! - `deploy_failed`, `deploy`, key `deploy_failed:<its record>`: a
//!   `deploy.finished` with `ok` false (deploy/deploy.sh, #54), while none
//!   is open; clears at the next with `ok` true, from any deploy.
//! - `integrator_down`, `spawn-integrate`, key `integrator_down:<the first
//!   reading's ms>`: no backend holds the integrator's lock on this store
//!   (`pg_locks`, the lock `integrator_try_lock` takes) at every reading for
//!   `SPAWN_ALERT_INTEGRATOR_DOWN_SECS` (default 300, longer than a deploy's
//!   restart; 0 disables it) while a gated mission has a reported task that
//!   waits for a gate: open or active, not halted, its source held by no
//!   owner (a deploy and an owner's push hold it). Clears at the first
//!   reading that finds the lock held. It rests on readings: the first one
//!   that found the lock free is kept by the step, so a restarted loop
//!   starts the bound again (later, never sooner, never twice). The lock is
//!   read only while the step's fold has a reported task of a mission that
//!   is open and not halted, or this alert is open: an idle loop reads
//!   nothing more (objective 023).
//! - `controller_behind`, `mission #<id>`, key `controller_behind:<mission>:
//!   <the first reading's ms>`: at every reading for
//!   `SPAWN_ALERT_CONTROLLER_BEHIND_SECS` (default 1800; 0 disables it), the
//!   owner's latest landing on a gated mission's source (the latest
//!   `source.moved` record's `to`; the mission open or active, not halted,
//!   with a reported task) is newer than the controller its gate's
//!   scenarios come from (`controllers/<name>-<commit>`): a descendant of
//!   that commit. The integrator holds such a mission's gates until the
//!   deploy switches the controller (P22), and every owner's landing is
//!   followed by its deploy within a minute or so: a wait this long is a
//!   landing not deployed (deploy.sh's exit 5). A task's landing needs no
//!   deploy and counts for nothing. Clears at the first reading that finds
//!   the controller has the landing, or the mission no longer waiting
//!   (closed, halted, nothing reported). Read on the step's readings, only
//!   while its fold has such a reported task or this alert is open; the
//!   first reading behind is kept by the step, so a restarted loop starts
//!   the bound again (later, never sooner).
//! - `mission_idle`, `mission #<id>`, key `mission_idle:<mission>:<the
//!   record that ended its last task>` (objective 058): at a reading, a
//!   mission not closed that takes its work from an objective backlog
//!   (`objectives` on its record) has had no task in work since its last
//!   task ended (landed, accepted or cancelled), for
//!   `SPAWN_ALERT_MISSION_IDLE_SECS` (default 900; 0 disables it). Never for
//!   owner work, and never while the owner has the mission paused
//!   (`mission.paused`, objective 099). Clears at the mission's next
//!   `task.assigned`, its close or its pause (the cleared evidence names the
//!   record). The owner's resume of a pause (`mission.unpaused`) starts the
//!   idle time of a mission with no task in work again, from that record. A
//!   restart reads when the last task ended (or the unpause) from the store.
//! - `owner_request_waiting`, the source, key
//!   `owner_request_waiting:<request>` (S23): an owner's landing request
//!   (`source.push_requested`, `spawn mission push`) that has had no outcome
//!   (no `source.moved` or `source.push_refused` names it) for
//!   `SPAWN_ALERT_OWNER_REQUEST_SECS` (default 3600, longer than a gate and
//!   its backoff; 0 disables it): deploy-on-landing waits for such a
//!   request (deploy/on-landing.sh), so the integrator down or a judging
//!   that never ends stops every deploy. Evidence the request, its commit
//!   and the withdrawal that ends it (`spawn mission push --withdraw
//!   <request>`). Once per request; clears at its outcome. A restart reads
//!   the pending requests from the store (the integrator's queue) and
//!   clears an open one whose request ended meanwhile, naming the outcome.
//! - `cost_budget`, `mission #<id>`, key `cost_budget:<mission>:<the UTC
//!   day, YYYY-MM-DD>`: at a reading (every `SPAWN_ALERT_CHECK_SECS`), the
//!   mission's cost of the model calls timed in the current UTC day
//!   (crate::cost, priced when recorded) has reached its daily budget
//!   (`budget.daily_usd`, `spawn mission configure --daily-usd`). Once per
//!   mission and day, whatever restarts and however much more it spends;
//!   clears when the day ends, or at a reading that finds the budget removed
//!   or above the day's spend. A mission with no daily budget, or under it,
//!   raises none. It rests on the readings of the recorded costs.
//! - `unverified_landings`, `objectives`, key
//!   `unverified_landings:<the newest old landing's ms>:<the reading's ms>`
//!   (objective 135, lead-mac's review §2.8): at a reading (every
//!   `SPAWN_ALERT_CHECK_SECS`), objectives whose newest landing is on or
//!   after 2026-11-13 00:00 +08:00 (crate::verify's `LISTED_SINCE_MS`,
//!   when "Verified by:" became required) and whose newest verification
//!   after it is not a pass (`spawn verify list`'s rule) have landed
//!   `SPAWN_ALERT_UNVERIFIED_SECS` (default 86400, a day; 0 disables it and
//!   reads nothing) or more ago. Evidence how many (`old`), how many are
//!   unverified at all, the bound, the newest of the old landings, the
//!   listing's start and the reading. One open at a time: more growing old
//!   while it is open fire nothing. Clears at the first reading with none
//!   old; a fail recorded after a pass makes an objective unverified
//!   again, and a later reading fires a new one.
//! - `message_not_taken`, the session id, key `message_not_taken:<command>`,
//!   evidence the command, agent, session and whether it was typed
//!   (objective 045): a message (what the Bridge types for a command, with
//!   its marker) that no `message.taken` names `SPAWN_MESSAGE_TAKEN_SECS`
//!   (default 120) after it was recorded, or, if it waited for its turn,
//!   after the turn it waited behind ended (`spawn_command_message`): typed
//!   and never taken, or never typed. A typed one's evidence says how many
//!   transcript lines the Bridge recorded since the typing
//!   (`read_since_typed`, at most 100 counted, S17): with none, its text
//!   says Spawn has read nothing of the transcript since and cannot tell
//!   whether the agent took it, rather than that it did not. A command an
//!   older Spawn typed carries
//!   no marker (no `message.typed`) and is never one, nor is one sent while
//!   its session's transcript had held no marker yet. Once per message;
//!   clears when it is taken; else, typed, when a later message of its
//!   session is taken (objective 138: the agent went on past it, its marker
//!   missed; evidence `later_taken`, that command, and its `taken_at_ms`;
//!   another session's taking counts for nothing, and an untyped message is
//!   behind nothing the agent took); else when its session ends with it
//!   untaken (the execution it was typed into, or its own start's, ended).
//!   A message whose session ended first, or a later message of whose
//!   session was taken first, fires nothing. Read on start (every message
//!   that can be one, and the open alerts), then when the log moved (the
//!   new commands, the pending messages due, the open alerts whose session
//!   took a message or ended since) and when a pending one falls due.
//! - `model_auth`, the session id, key
//!   `model_auth:<its execution.interrupted>`: an `execution.interrupted`
//!   (028) with cause `model_auth`; clears at that execution's
//!   `execution.cleared`.
//!
//! When: a record-borne alert is recorded within one pass of its record; a
//! host reading or a log line within `SPAWN_ALERT_CHECK_SECS` (default 30)
//! and one pass; a bound within one pass of falling due, never before,
//! measured on the database's clock from the record's `recorded_at_ms`. The
//! step runs when the event watermark moved or one of its timers is due, and
//! an idle pass reads the watermark once and nothing else (objective 023).
//! Like the disk watch it never fails a pass: an error is said and tried at
//! the next check.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use spawn_core::port::StoreError;
use spawn_postgres::PostgresStore;

use crate::config::{env_value, open_store};

/// The Observer's own attention threshold: a `stuck` judgement at or above
/// it says the agent loops or makes no progress.
pub(crate) const LIKELY: f64 = 0.5;

/// A day in milliseconds: the daily budget's day is the UTC day.
const DAY_MS: i64 = 86_400_000;

/// The UTC day of `ms`, `YYYY-MM-DD`.
fn utc_day(ms: i64) -> String {
    let (year, month, day, ..) = civil(ms);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The integrator-down alert's subject: the unit that runs the integrator.
const INTEGRATOR: &str = "spawn-integrate";

/// The most control records one pass handles; the next pass goes on.
const BATCH: i64 = 500;

/// How far back a restarted loop reads the alert-family records: a condition
/// older than this has long been recorded (or is a new one).
const HISTORY_MS: i64 = 7 * 86_400_000;
const HISTORY_LIMIT: i64 = 5_000;

/// How far back a restarted loop looks for commands that finished failed.
const COMMAND_LOOKBACK_MS: i64 = 600_000;

/// How far before the last reading of the messages the next looks for a
/// taking or an end: an append that committed after that reading may carry
/// an earlier time.
const MESSAGES_MARGIN_MS: i64 = 60_000;

/// On start, the last part of a log is read, not all of it.
const LOG_TAIL_BYTES: u64 = 1024 * 1024;
/// The most one reading takes from a log that grew a lot.
const LOG_READ_LIMIT: u64 = 4 * 1024 * 1024;
/// The most lines one crash occurrence quotes.
const CRASH_LINES: usize = 10;
/// A crash line older than this is history, not a reading.
const CRASH_MAX_AGE_MS: i64 = 86_400_000;

/// The line a recovered server writes.
const READY_LINE: &str = "database system is ready to accept connections";

/// The three lines the fleet's monitor called a crash.
const CRASH_PATTERNS: [&str; 3] = ["terminated by signal", "No space left", "could not resize"];

/// The thresholds, bounds and paths the step runs with (`SPAWN_ALERT_*`).
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// How often the host and the logs are read.
    pub(crate) check_ms: i64,
    /// How long a task may stay assigned with its agent free.
    pub(crate) undelivered_ms: i64,
    /// How long an admission may wait.
    pub(crate) admission_ms: i64,
    /// How long the model judge may answer no call.
    pub(crate) judge_down_ms: i64,
    /// How long a message may go untaken (SPAWN_MESSAGE_TAKEN_SECS).
    pub(crate) message_taken_ms: i64,
    /// The dump's and the bundle's schedules, with room for one late run.
    pub(crate) dump_max_age_ms: i64,
    pub(crate) bundle_max_age_ms: i64,
    /// MemAvailable under this, and the margin it clears at.
    pub(crate) memory_low_mb: u64,
    pub(crate) memory_ok_mb: u64,
    /// The filesystem watched, over this, and the margin it clears under.
    pub(crate) tmpfs_path: PathBuf,
    pub(crate) tmpfs_high_mb: u64,
    pub(crate) tmpfs_ok_mb: u64,
    /// The workspace root, whose filesystem's free space is watched
    /// (`disk_low`); none, nothing read.
    pub(crate) disk_root: Option<PathBuf>,
    /// Free space under this many GiB (0: never read), and the margin it
    /// clears at.
    pub(crate) disk_low_gb: u64,
    pub(crate) disk_ok_gb: u64,
    /// How long a crash line clears itself by silence.
    pub(crate) quiet_ms: i64,
    /// How long no integrator may hold its lock while reported work waits
    /// (0: never alerted).
    pub(crate) integrator_down_ms: i64,
    /// How long a mission's main may be ahead of its controller while a
    /// task waits for it (0: never alerted).
    pub(crate) controller_behind_ms: i64,
    /// How long a mission that takes its work from an objective backlog may
    /// have no task in work (0: never alerted).
    pub(crate) mission_idle_ms: i64,
    /// How long an owner's landing request may have no outcome (0: never
    /// alerted).
    pub(crate) owner_request_ms: i64,
    /// How long a landed objective may stay unverified (0: never alerted,
    /// nothing read).
    pub(crate) unverified_ms: i64,
    /// The window `spawn alerts --once` shows.
    pub(crate) window_ms: i64,
    /// The two database logs, when named.
    pub(crate) test_log: Option<PathBuf>,
    pub(crate) live_log: Option<PathBuf>,
    /// How often the databases' sizes are read (`database_size`).
    pub(crate) database_ms: i64,
    /// A database under this is never alerted, whatever its ratio.
    pub(crate) database_floor_mb: u64,
    /// A database over this many times the live log's bytes is alerted.
    pub(crate) database_ratio: u64,
    /// The gates' test database (`SPAWN_TEST_DATABASE_URL`) and the
    /// engines' (`SPAWN_FLEET_TEST_DATABASE_URL`), when set.
    pub(crate) test_databases: Vec<(&'static str, Url)>,
    /// How often the Core lines figures are read (`guardrail_out_of_range`).
    pub(crate) guardrail_ms: i64,
    /// How long a figure must stay out of its range before it raises.
    pub(crate) guardrail_raise_ms: i64,
    /// How long it must stay back before its alert clears.
    pub(crate) guardrail_clear_ms: i64,
}

/// A database URL, which holds a password: never shown, not even by
/// `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Url(pub(crate) String);

impl std::fmt::Debug for Url {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Url(..)")
    }
}

/// The test databases the step reads, by role, from the environment: the
/// gates' (`SPAWN_TEST_DATABASE_URL`, "test") first, then the engines'
/// (`SPAWN_FLEET_TEST_DATABASE_URL`, "fleet_test"), each when set.
pub(crate) fn test_database_urls() -> Vec<(&'static str, Url)> {
    [
        ("test", "SPAWN_TEST_DATABASE_URL"),
        ("fleet_test", "SPAWN_FLEET_TEST_DATABASE_URL"),
    ]
    .into_iter()
    .filter_map(|(role, name)| {
        env_value(name)
            .filter(|url| !url.trim().is_empty())
            .map(|url| (role, Url(url)))
    })
    .collect()
}

/// A number from `SPAWN_ALERT_*`, its default when unset, or its default
/// and one line saying it cannot be read (said once, at start).
fn number(name: &str, default: u64, warnings: &mut Vec<String>) -> u64 {
    match env_value(name) {
        None => default,
        Some(text) => {
            if let Ok(value) = text.trim().parse() {
                value
            } else {
                warnings.push(format!(
                    "{name}: {text:?} is not a number of seconds or megabytes; using {default}"
                ));
                default
            }
        }
    }
}

impl Settings {
    pub(crate) fn from_env() -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let secs = |name: &str, default: u64, warnings: &mut Vec<String>| -> i64 {
            number(name, default, warnings)
                .saturating_mul(1000)
                .cast_signed()
        };
        let settings = Self {
            check_ms: secs("SPAWN_ALERT_CHECK_SECS", 30, &mut warnings).max(1),
            undelivered_ms: secs("SPAWN_ALERT_UNDELIVERED_SECS", 300, &mut warnings),
            admission_ms: secs("SPAWN_ALERT_ADMISSION_SECS", 600, &mut warnings),
            judge_down_ms: secs("SPAWN_ALERT_JUDGE_DOWN_SECS", 900, &mut warnings),
            message_taken_ms: {
                let (bound, warning) = crate::config::message_taken_bound(
                    env_value("SPAWN_MESSAGE_TAKEN_SECS").as_deref(),
                );
                warnings.extend(warning);
                bound
            },
            dump_max_age_ms: secs("SPAWN_ALERT_BACKUP_DUMP_MAX_AGE_SECS", 7200, &mut warnings),
            bundle_max_age_ms: secs(
                "SPAWN_ALERT_BACKUP_BUNDLE_MAX_AGE_SECS",
                93_600,
                &mut warnings,
            ),
            memory_low_mb: number("SPAWN_ALERT_MEMORY_LOW_MB", 4096, &mut warnings),
            memory_ok_mb: number("SPAWN_ALERT_MEMORY_OK_MB", 8192, &mut warnings),
            tmpfs_path: env_value("SPAWN_ALERT_TMPFS_PATH")
                .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from),
            tmpfs_high_mb: number("SPAWN_ALERT_TMPFS_HIGH_MB", 8000, &mut warnings),
            tmpfs_ok_mb: number("SPAWN_ALERT_TMPFS_OK_MB", 6000, &mut warnings),
            disk_root: env_value("SPAWN_WORKSPACE_ROOT").map(PathBuf::from),
            disk_low_gb: number("SPAWN_ALERT_DISK_LOW_GB", 0, &mut warnings),
            disk_ok_gb: number("SPAWN_ALERT_DISK_OK_GB", 45, &mut warnings),
            quiet_ms: secs("SPAWN_ALERT_PG_QUIET_SECS", 900, &mut warnings),
            integrator_down_ms: secs("SPAWN_ALERT_INTEGRATOR_DOWN_SECS", 300, &mut warnings),
            controller_behind_ms: secs("SPAWN_ALERT_CONTROLLER_BEHIND_SECS", 1800, &mut warnings),
            mission_idle_ms: secs("SPAWN_ALERT_MISSION_IDLE_SECS", 900, &mut warnings),
            owner_request_ms: secs("SPAWN_ALERT_OWNER_REQUEST_SECS", 3600, &mut warnings),
            unverified_ms: secs("SPAWN_ALERT_UNVERIFIED_SECS", 86_400, &mut warnings),
            window_ms: secs("SPAWN_ALERT_WINDOW_SECS", 86_400, &mut warnings),
            test_log: env_value("SPAWN_TEST_DATABASE_LOG").map(PathBuf::from),
            live_log: env_value("SPAWN_DATABASE_LOG").map(PathBuf::from),
            database_ms: secs("SPAWN_ALERT_DATABASE_SECS", 600, &mut warnings),
            database_floor_mb: number("SPAWN_ALERT_DATABASE_FLOOR_MB", 1024, &mut warnings),
            database_ratio: number("SPAWN_ALERT_DATABASE_RATIO", 10, &mut warnings),
            test_databases: test_database_urls(),
            guardrail_ms: secs("SPAWN_ALERT_GUARDRAIL_SECS", 300, &mut warnings),
            guardrail_raise_ms: secs("SPAWN_GUARDRAIL_RAISE_SECS", 7200, &mut warnings),
            guardrail_clear_ms: secs("SPAWN_GUARDRAIL_CLEAR_SECS", 3600, &mut warnings),
        };
        (settings, warnings)
    }
}

/// One alert transition the step decided on, before it is recorded.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    Fire {
        key: String,
        kind: &'static str,
        subject: String,
        evidence: Value,
        text: String,
        notice: bool,
    },
    Clear {
        key: String,
        evidence: Value,
    },
}

impl Event {
    fn key(&self) -> &str {
        match self {
            Self::Fire { key, .. } | Self::Clear { key, .. } => key,
        }
    }
}

/// One recorded alert, as the store has it.
#[derive(Debug, Clone)]
struct Row {
    kind: String,
    subject: String,
    state: String,
    evidence: Value,
    fired_event_id: i64,
    fired_at_ms: i64,
}

impl Row {
    fn open(&self) -> bool {
        self.state == "open"
    }

    fn from_value(value: &Value) -> Option<(String, Self)> {
        let key = value["key"].as_str()?.to_string();
        Some((
            key,
            Self {
                kind: value["kind"].as_str().unwrap_or("alert").to_string(),
                subject: value["subject"].as_str().unwrap_or("").to_string(),
                state: value["state"].as_str().unwrap_or("open").to_string(),
                evidence: value["evidence"].clone(),
                fired_event_id: value["fired_event_id"].as_i64().unwrap_or(0),
                fired_at_ms: value["fired_at_ms"].as_i64().unwrap_or(0),
            },
        ))
    }
}

/// A task, as the fold would have it: the state the records left, and what
/// the delivery bound needs.
#[derive(Debug, Clone)]
struct Task {
    mission: i64,
    state: Option<String>,
    agent: String,
    objective: Option<i64>,
    revisions: i64,
    /// Its `task.assigned` record and time.
    assigned_event: Option<i64>,
    assigned_at: i64,
    /// Whether its agent holds no delivered or reported task.
    agent_free: bool,
}

/// A mission's five watched fields, as the fold would have them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mission {
    state: String,
    halted: Option<String>,
    rejections: i64,
    /// The objective of every rejection in the run of `rejections`, as the
    /// fold keeps it (`gate_rejection_objective`): a cancel of a task of it
    /// ends the run.
    run_objective: Option<i64>,
    aborts: i64,
    escalated: bool,
    accept: Option<String>,
    /// Its latest `mission.closed` record's event.
    closed_event: Option<i64>,
    /// It takes its work from an objective backlog (`objectives` on its
    /// record: `--backlog`, `--resident`); owner work does not.
    backlog: bool,
    /// When its last task ended with none in work after it, and the record
    /// that ended it (`at_ms`, `event_id`); none while a task is in work.
    /// The owner's resume of a pause starts it again, at its record.
    idle: Option<(i64, i64)>,
    /// The owner has paused it (`mission.paused`, objective 099): no
    /// `mission_idle` while the pause lasts.
    paused: bool,
}

impl Mission {
    /// Closed: `done`, `cancelled` or any state but `open` and `active`.
    fn closed(&self) -> bool {
        !matches!(self.state.as_str(), "open" | "active")
    }
}

/// An open (or pending) `validator_defect` alert: a task's (one mission) or
/// an owner landing's (one source, the open missions it moved).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Defect {
    key: String,
    owner: bool,
    /// The owner landing's source; none for a task's, or an owner's alert
    /// fired before alerts were per source.
    source: Option<String>,
    scenario: String,
    missions: Vec<i64>,
}

/// An `admission.waiting` with no `admission.admitted` after it yet.
#[derive(Debug, Clone)]
struct Pending {
    event_id: i64,
    at_ms: i64,
    subject: String,
    evidence: Value,
}

/// The newest backup record of one subject (`dump` or `bundle`).
#[derive(Debug, Clone)]
struct Backup {
    event_id: i64,
    at_ms: i64,
    key: Option<String>,
}

/// One database log the crash alert reads.
#[derive(Debug, Clone)]
struct LogWatch {
    subject: &'static str,
    path: Option<PathBuf>,
    offset: u64,
    seated: bool,
    carry: String,
    said: bool,
    last_match_ms: i64,
    last_line: Option<String>,
}

/// The loop's alert step: what it knows, and what it records.
pub(crate) struct Monitor {
    settings: Settings,
    started: bool,
    /// The last event id the step looked at.
    watermark: i64,
    /// When the host and the logs are read next, on the database's clock.
    next_reading_ms: i64,
    /// The earliest bound that has not fired yet.
    next_bound_ms: Option<i64>,
    /// Every alert the store holds, by key: the record a restart reads back.
    known: BTreeMap<String, Row>,
    tasks: HashMap<i64, Task>,
    missions: HashMap<i64, Mission>,
    /// The pending admission waits, by kind and agent or task.
    admitted: HashMap<String, Pending>,
    /// The model judge's `down` records with no `up` after them, by judge.
    judge_down: HashMap<String, Pending>,
    /// The owner's landing requests with no outcome yet, by request (its
    /// event id): subject the source, evidence its commit
    /// (`owner_request_waiting`).
    owner_requests: BTreeMap<i64, Pending>,
    /// An open (or pending) model-login alert key, by execution id.
    auth: HashMap<String, String>,
    /// The open (or pending) `scenario_red` key, by mission and scenario.
    red: HashMap<(i64, String), String>,
    /// The open (or pending) `validator_defect` alerts.
    defects: Vec<Defect>,
    /// The open (or pending) `task_parked` key, by task.
    parked: BTreeMap<i64, String>,
    /// The newest dump and bundle records.
    backups: BTreeMap<&'static str, Backup>,
    commands_after: i64,
    logs: Vec<LogWatch>,
    /// The first reading of the current run that found no integrator
    /// holding its lock while reported work waited (`integrator_down`).
    unlocked_since: Option<i64>,
    /// The first reading of the current run that found each mission's main
    /// ahead of its controller (`controller_behind`).
    behind_since: BTreeMap<i64, i64>,
    /// The (mission, source) pairs the last reading found away with no
    /// alert yet, and when it did (`source_away` fires on the second
    /// reading in a row, so a path that comes and goes does not flap). A
    /// restart forgets them: its first reading starts the count again.
    away_once: BTreeMap<(i64, String), i64>,
    /// When the earliest pending message reaches its bound, as the last
    /// reading of the messages found (`message_not_taken`).
    messages_due: Option<i64>,
    /// The newest command the messages' reading has seen: the next reads
    /// the commands after it.
    messages_after: i64,
    /// The tracked messages not taken, not alerted and whose session had not
    /// ended with them when last read, by the time each falls due: read
    /// again then.
    pending: BTreeMap<i64, i64>,
    /// From when the next reading looks for a `message.taken` or an
    /// `execution.ended` that may clear an open alert: the last reading's
    /// time, less a margin for an append that committed late.
    messages_since: i64,
    /// What the supervisor had to dispatch when last read
    /// ([`crate::serve::attention::supervisor_dispatch`]), read only while
    /// its latest judgement is stuck or its `agent_stuck` is open; `None`
    /// before a read (or after one that failed): its judgement alone
    /// decides, as before objective 057.
    supervisor: Option<Dispatch>,
    /// When the supervisor's dispatch may be read again: at most once per
    /// `SPAWN_ALERT_CHECK_SECS`.
    supervisor_due_ms: i64,
    /// When the databases' sizes are read next (`database_size`).
    database_due_ms: i64,
    /// When the Core lines figures are read next (`guardrail_out_of_range`).
    guardrail_due_ms: i64,
    /// Each figure out of its range at every reading since, by key: the
    /// first reading of this stretch. A restart forgets it.
    guardrail_out: BTreeMap<String, i64>,
    /// Each figure with an open alert that has not been out since, by key:
    /// the first reading of this stretch.
    guardrail_back: BTreeMap<String, i64>,
    /// The Core lines reading in flight on a thread of its own, and one
    /// given up that may still run (cc-astra A18: never inside a pass).
    guardrail_reads: guardrails::Reads,
    /// Whether this loop's first answered reading has seeded the counts
    /// from the hourly samples (objective 192): once per loop, whatever it
    /// found.
    guardrail_seeded: bool,
    /// Each figure whose newest `guardrail_out_of_range` alert the owner
    /// judged while open, with that alert's out-since (objective 192 (5)):
    /// taken by the first fresh reading that finds the figure out, ended by
    /// positive evidence it was back in range.
    guardrail_judged: BTreeMap<String, i64>,
}

/// What the supervisor has to dispatch, as one read of the attention view
/// found it (`agent_stuck`, objective 057).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Dispatch {
    /// It asks the owner a question (the attention view's `question`).
    pub(crate) question: bool,
    /// The plan's pending and told escalations.
    pub(crate) items: Vec<DispatchItem>,
}

/// One escalation of the supervisor's plan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DispatchItem {
    pub(crate) tag: String,
    /// The agent it is about, when it is about one.
    pub(crate) agent: Option<String>,
    /// It is a worker's own judgement (attention, idle, a nudge that did
    /// not help), not a record the supervisor must act on.
    pub(crate) judgement: bool,
}

impl Dispatch {
    /// From [`crate::serve::attention::supervisor_dispatch`]'s answer.
    pub(crate) fn from_value(value: &Value) -> Self {
        Self {
            question: value["question"].as_bool().unwrap_or(false),
            items: value["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| DispatchItem {
                    tag: item["tag"].as_str().unwrap_or_default().to_string(),
                    agent: item["agent_id"].as_str().map(str::to_string),
                    judgement: item["judgement"].as_bool().unwrap_or(false),
                })
                .collect(),
        }
    }
}

/// A worker's task states in which its part is done: what it waits on is
/// the gate, a reviewer, the landing or the owner's hold (objective 057). A
/// `reported` task waits for its gate or its review, `accepted` for its
/// landing, `landing` for its push (or behind a hold), `landed` for nothing.
const PART_DONE: [&str; 4] = ["reported", "accepted", "landing", "landed"];

/// The task states in which a worker holds a task: delivered or later, its
/// work (or done), never the supervisor's to assign.
const HELD: [&str; 6] = [
    "delivered",
    "revise",
    "reported",
    "accepted",
    "landing",
    "landed",
];

/// Whether `agent` is an ephemeral worker (`agent_worker_*`), which lives
/// for its task: once its part is done it has nothing else to do. A
/// resident outlives its tasks and takes other work (the owner's, the
/// supervisor's), so a task of its past its part says nothing of what it
/// may be stuck on.
fn ephemeral(agent: &str) -> bool {
    agent.starts_with("agent_worker_")
}

/// The one command that acts on a worker stuck in its task.
fn restart_command(task: i64) -> String {
    format!("spawn task restart --id {task}")
}

impl Monitor {
    /// The step with the environment's settings; the warnings are said once.
    pub(crate) fn from_env() -> (Self, Vec<String>) {
        let (settings, warnings) = Settings::from_env();
        (Self::with(settings), warnings)
    }

    pub(crate) fn with(settings: Settings) -> Self {
        let logs = vec![
            LogWatch::new("test", settings.test_log.clone()),
            LogWatch::new("live", settings.live_log.clone()),
        ];
        Self {
            settings,
            started: false,
            watermark: 0,
            next_reading_ms: 0,
            next_bound_ms: None,
            known: BTreeMap::new(),
            tasks: HashMap::new(),
            missions: HashMap::new(),
            admitted: HashMap::new(),
            judge_down: HashMap::new(),
            owner_requests: BTreeMap::new(),
            auth: HashMap::new(),
            red: HashMap::new(),
            defects: Vec::new(),
            parked: BTreeMap::new(),
            backups: BTreeMap::new(),
            commands_after: 0,
            logs,
            unlocked_since: None,
            behind_since: BTreeMap::new(),
            away_once: BTreeMap::new(),
            messages_due: None,
            messages_after: 0,
            pending: BTreeMap::new(),
            messages_since: 0,
            supervisor: None,
            supervisor_due_ms: 0,
            database_due_ms: 0,
            guardrail_due_ms: 0,
            guardrail_out: BTreeMap::new(),
            guardrail_back: BTreeMap::new(),
            guardrail_reads: guardrails::Reads::default(),
            guardrail_seeded: false,
            guardrail_judged: BTreeMap::new(),
        }
    }

    /// One pass: read what is new, decide, record. Returns lines for the
    /// loop's log. Never fails the loop: the caller says the error and tries
    /// again at the next check.
    pub(crate) fn check(&mut self, store: &PostgresStore) -> Result<Vec<String>, String> {
        let mut lines = Vec::new();
        if !self.started {
            self.start(store, &mut lines)?;
        }
        let (watermark, now) = store.alert_clock().map_err(|error| error.to_string())?;
        let reading_due = now >= self.next_reading_ms;
        let bound_due = self.next_bound_ms.is_some_and(|due| now >= due);
        // A Core lines reading in flight is looked at by every check.
        let guardrails_due = self.guardrail_reads.in_flight();
        if watermark <= self.watermark && !reading_due && !bound_due && !guardrails_due {
            return Ok(lines);
        }
        let mut events = Vec::new();
        let moved = watermark > self.watermark;
        if watermark > self.watermark {
            let batch = store
                .alert_control(self.watermark, BATCH)
                .map_err(|error| error.to_string())?;
            let records = batch["records"].as_array().map_or(&[][..], Vec::as_slice);
            // One record at a time, as at the start: a halt and its resume
            // in one batch end cleared, naming the resume.
            for entry in records {
                let id = entry["event_id"].as_i64().unwrap_or_default();
                let at = entry["at_ms"].as_i64().unwrap_or(now);
                let mut fed = Vec::new();
                self.feed(id, at, &entry["record"], &mut fed);
                self.flush(store, now, &fed, &mut lines);
            }
            // What the step has read: the last control record when the
            // batch was cut at its limit (the rest waits for the next
            // check), and otherwise everything up to the clock, which is
            // what makes an idle check one statement (objective 023: the
            // log's watermark, never the clock, says whether there is work).
            self.watermark = if i64::try_from(records.len()).unwrap_or(i64::MAX) < BATCH {
                watermark
            } else {
                batch["next"].as_i64().unwrap_or(self.watermark)
            };
            let observations = store
                .alert_observations()
                .map_err(|error| error.to_string())?;
            self.read_supervisor(store, now, &observations, &mut lines);
            self.observe(&observations, &mut events);
            let commands = store
                .alert_failed_commands(self.commands_after)
                .map_err(|error| error.to_string())?;
            self.commands(&commands, &mut events);
            self.commands_after = now.saturating_sub(1000);
        }
        // The messages are read when the log moved (a new command, a taking,
        // an end all append) and when a pending one falls due: the new
        // commands, the pending ones due, and the open alerts' whose session
        // took a message or ended since, so an idle pass reads next to
        // nothing (objective 023).
        if moved || self.messages_due.is_some_and(|due| now >= due) {
            self.read_messages(store, now, &mut events, &mut lines);
        }
        if reading_due {
            self.next_reading_ms = now.saturating_add(self.settings.check_ms);
            self.read_host(now, &mut events, &mut lines);
            self.read_integrator(store, now, &mut events, &mut lines);
            self.read_controllers(store, now, &mut events, &mut lines);
            self.read_sources(store, now, &mut events, &mut lines);
            self.read_defect_gates(store, now, &mut events, &mut lines);
            self.read_budgets(store, now, &mut events, &mut lines);
            self.read_unverified(store, now, &mut events, &mut lines);
            self.read_idle(now, &mut events);
            if now >= self.database_due_ms {
                self.database_due_ms = now.saturating_add(self.settings.database_ms);
                self.read_databases(store, now, &mut events, &mut lines);
            }
            if now >= self.guardrail_due_ms {
                self.guardrail_due_ms = now.saturating_add(self.settings.guardrail_ms);
                self.start_guardrails(now);
            }
        }
        self.poll_guardrails(now, &mut events, &mut lines);
        self.bounds(now, &mut events);
        self.flush(store, now, &events, &mut lines);
        self.plan(now);
        Ok(lines)
    }

    /// The earliest time the step waits for even when no record is
    /// written: its next host reading (`SPAWN_ALERT_CHECK_SECS`), a
    /// bound's due time, or, while a Core lines reading runs on its thread,
    /// the next look at it (alerts/guardrails.rs). Before the first check it
    /// is `0`, so a fresh loop runs it once.
    pub(crate) fn due_ms(&self) -> i64 {
        let due = self.next_bound_ms.map_or(self.next_reading_ms, |bound| {
            bound.min(self.next_reading_ms)
        });
        // While a Core lines reading runs, look again shortly.
        self.guardrail_reads
            .poll_ms()
            .map_or(due, |poll| poll.min(due))
    }

    /// Read what the step needs back: its alerts, the missions and tasks, the
    /// pending records, and where the event log stands.
    fn start(&mut self, store: &PostgresStore, lines: &mut Vec<String>) -> Result<(), String> {
        let mirror = store.alert_mirror().map_err(|error| error.to_string())?;
        // Where the event log stood when the missions and tasks were read,
        // from the same snapshot (objective 106): every record after it is
        // the checks' to feed, whatever commits while the start runs, and
        // none the mirror holds is fed again.
        let watermark = mirror["watermark"]
            .as_i64()
            .ok_or_else(|| "the alert mirror holds no watermark".to_string())?;
        let now = mirror["now_ms"]
            .as_i64()
            .unwrap_or_else(|| store.now_ms().unwrap_or_default());
        self.seed_mirror(&mirror);
        let rows = store.alert_rows().map_err(|error| error.to_string())?;
        for row in rows["rows"].as_array().into_iter().flatten() {
            if let Some((key, row)) = Row::from_value(row) {
                self.known.insert(key, row);
            }
        }
        self.seed_judged(&rows["rows"]);
        self.seed_red();
        self.seed_defects();
        self.seed_parked();
        let history = store
            .alert_history(now.saturating_sub(HISTORY_MS), HISTORY_LIMIT)
            .map_err(|error| error.to_string())?;
        // Each record is recorded before the next is read, so a later record
        // in the same history (a halt's resume, a good drill after a failed
        // one) finds the alert its predecessor fired and clears it, naming
        // itself; a record older than an open alert's own clears nothing
        // ([`after`]).
        // A record after the watermark is the checks' (objective 106): fed
        // here too, it would fold twice.
        for entry in history.as_array().into_iter().flatten() {
            let id = entry["event_id"].as_i64().unwrap_or_default();
            if id > watermark {
                continue;
            }
            let at = entry["at_ms"].as_i64().unwrap_or(now);
            let mut fed = Vec::new();
            self.feed(id, at, &entry["record"], &mut fed);
            self.flush(store, now, &fed, lines);
        }
        let mut events = Vec::new();
        // An alert read back whose missions all closed while the loop was
        // down, or before it fired (an owner alert of before objective 056,
        // one per mission), names no open mission: it clears now.
        self.closed_defects(None, now, &mut events);
        self.start_parked(&mirror, now, &mut events);
        let observations = store
            .alert_observations()
            .map_err(|error| error.to_string())?;
        self.read_supervisor(store, now, &observations, lines);
        self.observe(&observations, &mut events);
        self.commands_after = now.saturating_sub(COMMAND_LOOKBACK_MS);
        let commands = store
            .alert_failed_commands(self.commands_after)
            .map_err(|error| error.to_string())?;
        self.commands(&commands, &mut events);
        // The messages, read back as they stand: an alert the store holds
        // fires nothing again, and one due while the loop was down fires.
        let messages = store
            .alert_messages_all(self.settings.message_taken_ms, &self.open_messages())
            .map_err(|error| error.to_string())?;
        self.messages(&messages, &mut events);
        self.watermark = watermark;
        // Read after the watermark: a request or an outcome recorded since
        // is in both reads, and folds the same.
        self.seed_owner_requests(store, now, &mut events)?;
        for log in &mut self.logs {
            let open = self
                .known
                .iter()
                .filter(|(_, row)| {
                    row.open() && row.kind == "postgres_crash" && row.subject == log.subject
                })
                .max_by_key(|(_, row)| row.fired_at_ms);
            if let Some((_, row)) = open {
                log.last_match_ms = row.fired_at_ms;
                log.last_line = row.evidence["lines"]
                    .as_array()
                    .and_then(|lines| lines.last())
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
        self.flush(store, now, &events, lines);
        self.started = true;
        self.next_reading_ms = now;
        self.plan(now);
        Ok(())
    }

    /// The missions and tasks as the start's mirror read them.
    fn seed_mirror(&mut self, mirror: &Value) {
        for mission in mirror["missions"].as_array().into_iter().flatten() {
            let id = mission["id"].as_i64().unwrap_or_default();
            self.missions.insert(
                id,
                Mission {
                    state: mission["state"].as_str().unwrap_or("open").to_string(),
                    halted: mission["halted"].as_str().map(str::to_string),
                    rejections: mission["gate_rejections"].as_i64().unwrap_or_default(),
                    run_objective: mission["gate_rejection_objective"].as_i64(),
                    aborts: mission["gate_aborts"].as_i64().unwrap_or_default(),
                    escalated: mission["gate_room_escalated"].as_bool().unwrap_or(false),
                    accept: mission["accept"].as_str().map(str::to_string),
                    closed_event: mission["closed_event"].as_i64(),
                    backlog: mission["backlog"].as_bool().unwrap_or(false),
                    idle: mission["idle"]["at_ms"]
                        .as_i64()
                        .zip(mission["idle"]["event_id"].as_i64()),
                    paused: mission["paused"].as_bool().unwrap_or(false),
                },
            );
        }
        for task in mirror["tasks"].as_array().into_iter().flatten() {
            let id = task["id"].as_i64().unwrap_or_default();
            self.tasks.insert(
                id,
                Task {
                    mission: task["mission"].as_i64().unwrap_or_default(),
                    state: task["state"].as_str().map(str::to_string),
                    agent: task["agent_id"].as_str().unwrap_or("").to_string(),
                    objective: task["objective"].as_i64(),
                    revisions: task["revisions"].as_i64().unwrap_or_default(),
                    assigned_event: task["assigned_event"].as_i64(),
                    assigned_at: task["assigned_at_ms"].as_i64().unwrap_or_default(),
                    agent_free: task["agent_free"].as_bool().unwrap_or(true),
                },
            );
        }
    }

    /// One control record the step has not seen: fold it into what the step
    /// knows and decide the alerts it makes.
    fn feed(&mut self, event_id: i64, at_ms: i64, record: &Value, out: &mut Vec<Event>) {
        let Some(kind) = record["type"].as_str() else {
            return;
        };
        match kind {
            "task.assigned" | "task.delivered" | "task.reported" | "task.gated"
            | "task.reviewed" | "task.landed" | "task.aborted" | "task.over_budget"
            | "task.cancelled" => self.task_record(event_id, at_ms, record, out),
            "task.released" => {
                if let Some(task) = record["id"].as_i64() {
                    self.parked_record(event_id, task, record, out);
                }
            }
            "mission.added" | "mission.closed" | "mission.halted" | "mission.resumed"
            | "mission.configured" => self.mission_record(event_id, record, out),
            "mission.paused" | "mission.unpaused" => {
                self.pause_record(event_id, at_ms, record, out);
            }
            "admission.waiting" | "admission.admitted" => {
                self.admission_record(event_id, at_ms, record, out);
            }
            "backup.dumped" | "backup.bundled" => self.backup_record(event_id, at_ms, record, out),
            "backup.verified" => self.verified_record(event_id, record, out),
            "deploy.finished" => self.deploy_record(event_id, record, out),
            "execution.interrupted" | "execution.cleared" => {
                self.auth_record(event_id, record, out);
            }
            "host.judge" => self.judge_record(event_id, at_ms, record, out),
            "source.moved" => {
                self.owner_request_ended(event_id, record, out);
                self.owner_landing(event_id, record, out);
            }
            "source.push_requested" => self.owner_request(event_id, at_ms, record),
            "source.push_refused" => self.owner_request_ended(event_id, record, out),
            "alert.judged" => self.judged_record(record),
            _ => {}
        }
    }

    /// The owner judged an alert (`spawn alert judge`, objective 092): an
    /// open one is cleared by it, so the step clears it no more, and a later
    /// occurrence of its kind and subject fires anew. Judged `right`
    /// (objective 148) it stays open: its condition still clears it.
    fn judged_record(&mut self, record: &Value) {
        if record["verdict"] == "right" {
            return;
        }
        if let Some(row) = record["key"]
            .as_str()
            .and_then(|key| self.known.get_mut(key))
            && row.open()
        {
            row.state = "cleared".to_string();
        }
    }

    /// The open `task_parked` alerts the store holds, by the task each
    /// names.
    fn seed_parked(&mut self) {
        for (key, row) in &self.known {
            if row.open()
                && row.kind == "task_parked"
                && let Some(task) = row.evidence["task"].as_i64()
            {
                self.parked.insert(task, key.clone());
            }
        }
    }

    /// At start, the tasks the mirror finds parked (each with the
    /// `task.gated` record that parked it, `parked`): an open `task_parked`
    /// whose task is not parked by its record any more (released, judged or
    /// ended while the step was down) clears, naming the reading; a task
    /// parked by a record no alert names (parked while the step was down)
    /// fires.
    fn start_parked(&mut self, mirror: &Value, now: i64, out: &mut Vec<Event>) {
        let parked: BTreeMap<i64, (i64, &Value)> = mirror["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|task| {
                let by = &task["parked"];
                Some((
                    task["id"].as_i64()?,
                    (by["event_id"].as_i64()?, &by["record"]),
                ))
            })
            .collect();
        for (task, key) in std::mem::take(&mut self.parked) {
            match parked.get(&task) {
                Some((event_id, _)) if key == format!("task_parked:{task}:{event_id}") => {
                    self.parked.insert(task, key);
                }
                _ => out.push(Event::Clear {
                    key,
                    evidence: json!({"cause": "not_parked", "reading_ms": now}),
                }),
            }
        }
        for (task, (event_id, record)) in parked {
            let key = format!("task_parked:{task}:{event_id}");
            if !self.parked.contains_key(&task) && !self.known.contains_key(&key) {
                self.fire_parked(event_id, task, record, out);
            }
        }
    }

    /// A record about task `task` for `task_parked`: the bound's parking
    /// fires one (none while one is open for the task); its release, a
    /// later verdict but `no_room` and `stale`, or its end clears it,
    /// naming the record.
    fn parked_record(&mut self, event_id: i64, task: i64, record: &Value, out: &mut Vec<Event>) {
        let kind = record["type"].as_str().unwrap_or_default();
        let verdict = record["verdict"].as_str();
        let parks = kind == "task.gated"
            && verdict == Some("host_fault")
            && record["evidence"].as_str() == Some("load_sensitive_parked");
        let clears = match kind {
            "task.released" | "task.landed" | "task.cancelled" | "task.aborted"
            | "task.over_budget" => true,
            "task.gated" => !parks && !matches!(verdict, Some("no_room" | "stale")),
            _ => false,
        };
        if clears && let Some(key) = self.parked.remove(&task) {
            let mut evidence = json!({"event_id": event_id, "record": kind});
            if kind == "task.released" {
                evidence["by"] = record["by"].clone();
                evidence["reason"] = record["reason"].clone();
            } else if kind == "task.gated" {
                evidence["verdict"] = json!(verdict);
            }
            out.push(Event::Clear { key, evidence });
        }
        if parks && !self.parked.contains_key(&task) {
            self.fire_parked(event_id, task, record, out);
        }
    }

    /// Fire `task_parked` for `task` from the record `event_id` that parked
    /// it: its pushed commit, the scenarios and each one's root-fix owner
    /// (the record's `owners`), and the command that releases it.
    fn fire_parked(&mut self, event_id: i64, task: i64, record: &Value, out: &mut Vec<Event>) {
        let key = format!("task_parked:{task}:{event_id}");
        let mission = self.tasks.get(&task).map(|known| known.mission);
        let pushed = record["pushed"].as_str().unwrap_or("?");
        let owners = record["owners"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| {
                let scenario = entry["scenario"].as_str().unwrap_or("a scenario");
                match entry["owner"].as_str() {
                    Some(owner) => format!("{scenario} (root fix: {owner})"),
                    None => format!("{scenario} (no root-fix owner named in scenario-owners)"),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let command = format!("spawn task release --id {task} --reason <why>");
        self.parked.insert(task, key.clone());
        out.push(Event::Fire {
            key,
            kind: "task_parked",
            subject: format!("task #{task}"),
            evidence: json!({
                "task": task, "mission": mission, "pushed": pushed,
                "scenarios": record["sensitive"], "owners": record["owners"],
                "event_id": event_id, "command": command,
            }),
            text: format!(
                "task #{task}{}: its pushed commit {} was load-sensitive twice: {owners}. It is \
                 not judged again until a new commit is pushed or a person releases it \
                 ({command}); the mission's other tasks go on",
                mission.map_or_else(String::new, |mission| format!(" (mission #{mission})")),
                pushed.get(..12).unwrap_or(pushed)
            ),
            notice: false,
        });
    }

    /// The open `validator_defect` alerts the store holds: a task's names
    /// its `mission`; an owner's its `source` and `missions` (one before
    /// objective 056 its `mission` alone, and no source).
    fn seed_defects(&mut self) {
        for (key, row) in &self.known {
            if !row.open() || row.kind != "validator_defect" {
                continue;
            }
            let Some(scenario) = row.evidence["scenario"].as_str() else {
                continue;
            };
            let missions: Vec<i64> = match row.evidence["missions"].as_array() {
                Some(list) => list.iter().filter_map(Value::as_i64).collect(),
                None => row.evidence["mission"].as_i64().into_iter().collect(),
            };
            self.defects.push(Defect {
                key: key.clone(),
                owner: row.evidence["owner"].as_bool().unwrap_or(false),
                source: row.evidence["source"].as_str().map(str::to_string),
                scenario: scenario.to_string(),
                missions,
            });
        }
        self.defects.sort_by(|left, right| left.key.cmp(&right.key));
    }

    /// Take the open defects `clears` decides, each with the evidence of its
    /// clearing.
    fn clear_defects(
        &mut self,
        out: &mut Vec<Event>,
        mut clears: impl FnMut(&Defect) -> Option<Value>,
    ) {
        let mut kept = Vec::with_capacity(self.defects.len());
        for defect in std::mem::take(&mut self.defects) {
            match clears(&defect) {
                Some(evidence) => out.push(Event::Clear {
                    key: defect.key,
                    evidence,
                }),
                None => kept.push(defect),
            }
        }
        self.defects = kept;
    }

    /// `validator_defect` from an owner's accepted landing that the gate
    /// judged without a validator failing on the base too (its
    /// `validator_defects`): one per source and scenario, naming the
    /// missions the move names that are not closed (none open: none fires;
    /// one already open for the source and scenario: nothing new). The
    /// move clears its source's alerts for a scenario it lists in `passed`
    /// or `reused`, and the moved missions' for one it names `quarantined`.
    fn owner_landing(&mut self, event_id: i64, record: &Value, out: &mut Vec<Event>) {
        let moved: Vec<i64> = record["missions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .collect();
        let source = record["source"].as_str().map(str::to_string);
        let passed = passed_names(record);
        let quarantined = names(&record["quarantined"]);
        self.clear_defects(out, |defect| {
            if defect.source.is_some()
                && defect.source == source
                && passed.contains(&defect.scenario)
            {
                return Some(json!({"cause": "passed", "event_id": event_id,
                                   "record": "source.moved", "scenario": defect.scenario}));
            }
            (quarantined.contains(&defect.scenario)
                && defect
                    .missions
                    .iter()
                    .any(|mission| moved.contains(mission)))
            .then(|| {
                json!({"cause": "quarantined", "event_id": event_id,
                       "record": "source.moved", "quarantined": defect.scenario})
            })
        });
        let Some(source) = source else {
            return;
        };
        let mut missions: Vec<i64> = moved
            .iter()
            .copied()
            .filter(|mission| !self.missions.get(mission).is_some_and(Mission::closed))
            .collect();
        missions.sort_unstable();
        missions.dedup();
        if missions.is_empty() {
            return;
        }
        let request = record["request"].as_i64();
        let named = missions
            .iter()
            .map(|mission| format!("mission #{mission}"))
            .collect::<Vec<_>>()
            .join(", ");
        for scenario in names(&record["validator_defects"]) {
            if self.defects.iter().any(|defect| {
                defect.owner
                    && defect.source.as_deref() == Some(source.as_str())
                    && defect.scenario == scenario
            }) {
                continue;
            }
            let key = format!("validator_defect:{source}:{scenario}:{event_id}");
            self.defects.push(Defect {
                key: key.clone(),
                owner: true,
                source: Some(source.clone()),
                scenario: scenario.clone(),
                missions: missions.clone(),
            });
            out.push(Event::Fire {
                key,
                kind: "validator_defect",
                subject: format!("{source} ({named})"),
                evidence: json!({
                    "source": source, "scenario": scenario, "owner": true,
                    "missions": missions, "request": request, "event_id": event_id,
                    "to": record["to"],
                }),
                text: format!(
                    "{named}: the owner's landing (request #{}) on {source} was accepted \
                     without {scenario}, which failed on main too: until it passes again, \
                     nothing checks what it checks. It clears once a gate passes it, it \
                     leaves the gate directory, or these missions are closed",
                    request.unwrap_or_default()
                ),
                notice: false,
            });
        }
    }

    /// `validator_defect` from a task's gate that ended on one (`host_fault`,
    /// evidence `validator_defect`): one per mission and scenario. A gating
    /// of a mission an open alert names clears it when it names the
    /// scenario `quarantined`, lists it in `passed` or `reused`, or carries
    /// `scenario_names` without it (gone: the missions on one source share
    /// a gate directory); a task's alert also at the mission's next
    /// `accepted` or `rejected` gating (its gates judge again).
    fn task_defects(
        &mut self,
        event_id: i64,
        task: i64,
        mission: i64,
        record: &Value,
        out: &mut Vec<Event>,
    ) {
        let quarantined = names(&record["quarantined"]);
        let passed = passed_names(record);
        let present = record["scenario_names"]
            .as_array()
            .map(|_| names(&record["scenario_names"]));
        let judged = matches!(record["verdict"].as_str(), Some("accepted" | "rejected"));
        self.clear_defects(out, |defect| {
            if !defect.missions.contains(&mission) {
                return None;
            }
            let base = json!({"event_id": event_id, "record": "task.gated", "task": task});
            let with = |cause: &str, extra: Value| {
                let mut evidence = base.clone();
                if let (Value::Object(map), Value::Object(more)) = (&mut evidence, extra) {
                    map.insert("cause".to_string(), json!(cause));
                    map.extend(more);
                }
                evidence
            };
            if quarantined.contains(&defect.scenario) {
                Some(with("quarantined", json!({"quarantined": defect.scenario})))
            } else if passed.contains(&defect.scenario) {
                Some(with("passed", json!({"scenario": defect.scenario})))
            } else if present
                .as_ref()
                .is_some_and(|present| !present.contains(&defect.scenario))
            {
                Some(with("gone", json!({"scenario": defect.scenario})))
            } else if judged && !defect.owner {
                Some(with("judged", json!({"verdict": record["verdict"]})))
            } else {
                None
            }
        });
        if record["verdict"].as_str() != Some("host_fault")
            || record["evidence"].as_str() != Some("validator_defect")
        {
            return;
        }
        for scenario in names(&record["validator_defects"]) {
            if self.defects.iter().any(|defect| {
                !defect.owner && defect.scenario == scenario && defect.missions == [mission]
            }) {
                continue;
            }
            let key = format!("validator_defect:{mission}:{scenario}:{event_id}");
            self.defects.push(Defect {
                key: key.clone(),
                owner: false,
                source: None,
                scenario: scenario.clone(),
                missions: vec![mission],
            });
            out.push(Event::Fire {
                key,
                kind: "validator_defect",
                subject: format!("mission #{mission}"),
                evidence: json!({
                    "mission": mission, "missions": [mission], "scenario": scenario,
                    "owner": false, "task": task, "event_id": event_id,
                }),
                text: format!(
                    "mission #{mission}: {scenario} fails on the base too (task #{task}'s gate): \
                     no task's candidate that fails it gets a verdict until it is fixed or \
                     quarantined"
                ),
                notice: false,
            });
        }
    }

    /// Clear each open `validator_defect` whose missions are all closed:
    /// at a `mission.closed` (`closing`, its event) of one of them, or at
    /// start (`None`) for those read back, naming the last closing then
    /// (or, when that is not later than the alert, the start's reading).
    fn closed_defects(&mut self, closing: Option<(i64, i64)>, now: i64, out: &mut Vec<Event>) {
        let missions = &self.missions;
        let all_closed = |defect: &Defect| {
            !defect.missions.is_empty()
                && defect
                    .missions
                    .iter()
                    .all(|mission| missions.get(mission).is_some_and(Mission::closed))
        };
        let last_closing = |defect: &Defect| {
            defect
                .missions
                .iter()
                .filter_map(|mission| missions.get(mission).and_then(|row| row.closed_event))
                .max()
        };
        let known = &self.known;
        let mut clears = Vec::new();
        let mut kept = Vec::new();
        for defect in std::mem::take(&mut self.defects) {
            let evidence = match closing {
                Some((mission, event_id)) => {
                    (defect.missions.contains(&mission) && all_closed(&defect)).then(|| {
                        json!({"cause": "closed", "event_id": event_id,
                                "record": "mission.closed"})
                    })
                }
                None if all_closed(&defect) => {
                    let fired = known
                        .get(&defect.key)
                        .and_then(|row| row.evidence["event_id"].as_i64());
                    Some(match (last_closing(&defect), fired) {
                        (Some(last), Some(fired)) if last > fired => {
                            json!({"cause": "closed", "event_id": last, "record": "mission.closed"})
                        }
                        (last, _) => json!({"cause": "closed", "closed_event_id": last,
                                            "reading_ms": now}),
                    })
                }
                None => None,
            };
            match evidence {
                Some(evidence) => clears.push(Event::Clear {
                    key: defect.key,
                    evidence,
                }),
                None => kept.push(defect),
            }
        }
        self.defects = kept;
        out.extend(clears);
    }

    /// The notices and the halt of one mission's record.
    fn mission_record(&mut self, event_id: i64, record: &Value, out: &mut Vec<Event>) {
        let kind = record["type"].as_str().unwrap_or_default();
        let Some(id) = record["id"].as_i64() else {
            return;
        };
        if kind == "mission.added" {
            self.missions.entry(id).or_insert_with(|| Mission {
                state: "open".to_string(),
                halted: None,
                rejections: 0,
                run_objective: None,
                aborts: 0,
                escalated: false,
                accept: None,
                closed_event: None,
                backlog: false,
                idle: None,
                paused: false,
            });
        }
        let Some(mission) = self.missions.get_mut(&id) else {
            return;
        };
        let before = snapshot(mission);
        apply_mission(mission, record);
        if kind == "mission.closed" {
            mission.closed_event = Some(event_id);
        }
        let after = snapshot(mission);
        let counters = (mission.rejections, mission.aborts);
        if before != after {
            out.push(Event::Fire {
                key: format!("mission_state:{id}:{event_id}"),
                kind: "mission_state",
                subject: format!("mission #{id}"),
                evidence: json!({
                    "mission": id, "record": kind, "event_id": event_id,
                    "before": before, "after": after,
                }),
                text: format!("mission #{id}: {} (by {kind})", changed(&before, &after)),
                notice: true,
            });
        }
        match kind {
            "mission.halted" => {
                let reason = record["reason"].as_str().unwrap_or("halted");
                out.push(Event::Fire {
                    key: format!("mission_halted:{id}:{event_id}"),
                    kind: "mission_halted",
                    subject: format!("mission #{id}"),
                    evidence: json!({
                        "reason": reason, "gate_rejections": counters.0,
                        "gate_aborts": counters.1, "event_id": event_id,
                    }),
                    text: format!("mission #{id} halted: {reason}"),
                    notice: false,
                });
            }
            "mission.closed" => {
                self.closed_defects(Some((id, event_id)), 0, out);
                self.clear_idle(id, event_id, "mission.closed", out);
            }
            "mission.resumed" => {
                for key in self.open_keys("mission_halted", &format!("mission #{id}")) {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({"event_id": event_id, "record": "mission.resumed"}),
                    });
                }
            }
            _ => {}
        }
    }

    /// The owner's pause or its resume (objective 099). A pause clears the
    /// mission's open `mission_idle` alert, naming its record, and none
    /// fires while it lasts; the resume (`mission.unpaused`) starts a
    /// mission with no task in work idle again from its own record.
    fn pause_record(&mut self, event_id: i64, at_ms: i64, record: &Value, out: &mut Vec<Event>) {
        let Some(id) = record["id"].as_i64() else {
            return;
        };
        let Some(mission) = self.missions.get_mut(&id) else {
            return;
        };
        if record["type"] == "mission.paused" {
            mission.paused = true;
            self.clear_idle(id, event_id, "mission.paused", out);
            return;
        }
        mission.paused = false;
        if mission.idle.is_some() {
            mission.idle = Some((at_ms, event_id));
        }
    }

    /// The open `scenario_red` alerts the store holds, by mission and
    /// scenario: a main still red on a scenario stays one alert, however old
    /// its first record is (the history read back on start is a week).
    fn seed_red(&mut self) {
        for (key, row) in &self.known {
            if row.open() && row.kind == "scenario_red" {
                let mission = row.evidence["mission"].as_i64();
                let scenario = row.evidence["scenario"].as_str();
                if let (Some(mission), Some(scenario)) = (mission, scenario) {
                    self.red
                        .insert((mission, scenario.to_string()), key.clone());
                }
            }
        }
    }

    /// `scenario_red`: opened by a gating that names a scenario main is red
    /// on, once per mission and scenario; cleared by a later accepted gating
    /// of the mission that does not name it.
    fn red_scenarios(
        &mut self,
        event_id: i64,
        task: i64,
        mission_id: i64,
        record: &Value,
        out: &mut Vec<Event>,
    ) {
        let named: Vec<String> = record["main_red_scenarios"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        for scenario in &named {
            let at = (mission_id, scenario.clone());
            if self.red.contains_key(&at) {
                continue;
            }
            let key = format!("scenario_red:{mission_id}:{scenario}:{event_id}");
            self.red.insert(at, key.clone());
            out.push(Event::Fire {
                key,
                kind: "scenario_red",
                subject: format!("mission #{mission_id}"),
                evidence: json!({
                    "mission": mission_id, "scenario": scenario, "task": task,
                    "base": record["base"], "event_id": event_id,
                }),
                text: format!(
                    "mission #{mission_id}: main is red on {scenario}: solved on an ancestor of \
the base, unsolved on it (task #{task}'s gate)"
                ),
                notice: false,
            });
        }
        if record["verdict"].as_str() != Some("accepted") {
            return;
        }
        let green: Vec<(i64, String)> = self
            .red
            .keys()
            .filter(|(mission, scenario)| *mission == mission_id && !named.contains(scenario))
            .cloned()
            .collect();
        for at in green {
            if let Some(key) = self.red.remove(&at) {
                out.push(Event::Clear {
                    key,
                    evidence: json!({
                        "event_id": event_id, "record": "task.gated", "task": task,
                        "base": record["base"],
                    }),
                });
            }
        }
    }

    /// A task record: its state notice, its mission's counters when it was
    /// gated, 033's notice, and the delivery bound's clearing.
    fn task_record(&mut self, event_id: i64, at_ms: i64, record: &Value, out: &mut Vec<Event>) {
        let kind = record["type"].as_str().unwrap_or_default();
        let Some(id) = record["id"].as_i64() else {
            return;
        };
        self.parked_record(event_id, id, record, out);
        if kind == "task.assigned" {
            self.tasks.entry(id).or_insert_with(|| Task {
                mission: record["mission_id"].as_i64().unwrap_or_default(),
                state: None,
                agent: record["agent_id"].as_str().unwrap_or("").to_string(),
                objective: record["objective"].as_i64(),
                revisions: 0,
                assigned_event: Some(event_id),
                assigned_at: at_ms,
                agent_free: true,
            });
        }
        let Some(task) = self.tasks.get(&id).cloned() else {
            return;
        };
        let accept_gate = self
            .missions
            .get(&task.mission)
            .is_some_and(|mission| mission.accept.as_deref() == Some("gate"));
        let guarded = record["revision"]
            .as_i64()
            .is_none_or(|revision| revision == task.revisions);
        if kind == "task.gated" && guarded {
            self.gating(event_id, id, task.mission, record, out);
        }
        if kind == "task.cancelled" {
            // Whoever cancels it (a yield, the owner, an objective solved on
            // the base), as the fold: a cancel of the run's objective ends it.
            self.mission_change(event_id, task.mission, kind, out, |mission| {
                apply_cancel(mission, task.objective);
            });
        }
        let after = task_state_after(record, accept_gate, task.revisions);
        if let Some(after) = after {
            if bumps_revision(record)
                && let Some(entry) = self.tasks.get_mut(&id)
            {
                entry.revisions += 1;
            }
            if task.state.as_deref() != Some(after.as_str()) {
                if let Some(entry) = self.tasks.get_mut(&id) {
                    entry.state = Some(after.clone());
                }
                out.push(Event::Fire {
                    key: format!("task_state:{id}:{event_id}"),
                    kind: "task_state",
                    subject: format!("task #{id}"),
                    evidence: json!({
                        "task": id, "mission": task.mission, "objective": task.objective,
                        "agent_id": task.agent.clone(), "from": task.state.clone(),
                        "to": after, "record": kind, "event_id": event_id,
                    }),
                    text: format!(
                        "task #{id}: {} -> {after} (by {kind})",
                        task.state.as_deref().unwrap_or("none")
                    ),
                    notice: true,
                });
                if PART_DONE.contains(&after.as_str()) && ephemeral(&task.agent) {
                    self.part_done(event_id, id, &task.agent, kind, &after, out);
                }
            }
        }
        self.track_idle(event_id, at_ms, kind, task.mission, out);
        if self
            .tasks
            .get(&id)
            .is_some_and(|task| task.state.as_deref() != Some("assigned"))
            && let Some(assigned) = task.assigned_event
            && self.is_open(&format!("task_undelivered:{id}:{assigned}"))
        {
            out.push(Event::Clear {
                key: format!("task_undelivered:{id}:{assigned}"),
                evidence: json!({"event_id": event_id, "record": kind}),
            });
        }
    }

    /// A worker's task moved past its part ([`PART_DONE`]) by the record
    /// `event_id`: the worker's open `agent_stuck` (on this task, or one
    /// that names none) clears at it, naming it (objective 057).
    fn part_done(
        &mut self,
        event_id: i64,
        task: i64,
        agent: &str,
        record: &str,
        state: &str,
        out: &mut Vec<Event>,
    ) {
        let open: Vec<String> = self
            .known
            .iter()
            .filter(|(_, row)| {
                row.open()
                    && row.kind == "agent_stuck"
                    && row.evidence["agent_id"].as_str() == Some(agent)
                    && row.evidence["task"]
                        .as_i64()
                        .is_none_or(|named| named == task)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in open {
            out.push(Event::Clear {
                key,
                evidence: json!({
                    "event_id": event_id, "record": record, "task": task, "task_state": state,
                }),
            });
        }
    }

    /// The `mission_state` notice of a record after which a mission's
    /// watched fields differ (`change` applies the record's fold).
    fn mission_change(
        &mut self,
        event_id: i64,
        mission_id: i64,
        kind: &str,
        out: &mut Vec<Event>,
        change: impl FnOnce(&mut Mission),
    ) {
        let Some(mission) = self.missions.get_mut(&mission_id) else {
            return;
        };
        let before = snapshot(mission);
        change(mission);
        let after = snapshot(mission);
        if before != after {
            out.push(Event::Fire {
                key: format!("mission_state:{mission_id}:{event_id}"),
                kind: "mission_state",
                subject: format!("mission #{mission_id}"),
                evidence: json!({
                    "mission": mission_id, "record": kind, "event_id": event_id,
                    "before": before, "after": after,
                }),
                text: format!(
                    "mission #{mission_id}: {} (by {kind})",
                    changed(&before, &after)
                ),
                notice: true,
            });
        }
    }

    /// A gating's mission counters, and 033's base disagreement.
    fn gating(
        &mut self,
        event_id: i64,
        task: i64,
        mission_id: i64,
        record: &Value,
        out: &mut Vec<Event>,
    ) {
        let objective = self.tasks.get(&task).and_then(|task| task.objective);
        self.mission_change(event_id, mission_id, "task.gated", out, |mission| {
            apply_gating(mission, record, objective);
        });
        self.red_scenarios(event_id, task, mission_id, record, out);
        self.task_defects(event_id, task, mission_id, record, out);
        if base_disagrees(record).is_some() {
            out.push(Event::Fire {
                key: format!("gate_base_disagrees:{event_id}"),
                kind: "gate_base_disagrees",
                subject: format!("mission #{mission_id}"),
                evidence: json!({
                    "task": task, "mission": mission_id,
                    "scenario": record["scenario"], "exits": record["exits"],
                    "base_disagrees": record["base_disagrees"],
                    "setup_s": record.get("setup_s").cloned().unwrap_or(Value::Null),
                    "due_ms": record["due_ms"], "event_id": event_id,
                }),
                text: format!(
                    "mission #{mission_id}: the base disagrees with itself (task #{task}, {} \
times in a row)",
                    record["base_disagrees"].as_i64().unwrap_or_default()
                ),
                notice: true,
            });
        }
    }

    /// An admission's wait and its clear.
    fn admission_record(
        &mut self,
        event_id: i64,
        at_ms: i64,
        record: &Value,
        out: &mut Vec<Event>,
    ) {
        let kind = record["kind"].as_str().unwrap_or("execution");
        let matched = if kind == "execution" {
            format!(
                "execution:{}",
                record["agent_id"].as_str().unwrap_or_default()
            )
        } else {
            format!("gate:{}", record["task"].as_i64().unwrap_or_default())
        };
        match record["type"].as_str().unwrap_or_default() {
            "admission.waiting" => {
                let subject = if kind == "execution" {
                    record["agent_id"]
                        .as_str()
                        .unwrap_or("an execution")
                        .to_string()
                } else {
                    record["task"]
                        .as_i64()
                        .map_or_else(|| "a gate".to_string(), |task| format!("task #{task}"))
                };
                self.admitted.insert(
                    matched,
                    Pending {
                        event_id,
                        at_ms,
                        subject,
                        evidence: json!({
                            "kind": kind, "envelope_bytes": record["envelope_bytes"],
                            "measure": record["measure"],
                            "measured_bytes": record["measured_bytes"],
                            "threshold_bytes": record["threshold_bytes"],
                            "event_id": event_id,
                        }),
                    },
                );
            }
            "admission.admitted" => {
                if let Some(pending) = self.admitted.remove(&matched) {
                    out.push(Event::Clear {
                        key: format!("admission_wait:{}", pending.event_id),
                        evidence: json!({
                            "waited_ms": record["waited_ms"], "event_id": event_id,
                        }),
                    });
                }
            }
            _ => {}
        }
    }

    /// The model judge's crossing: a `down` waits for its bound, an `up`
    /// clears it (or forgets the `down` before it fired).
    fn judge_record(&mut self, event_id: i64, at_ms: i64, record: &Value, out: &mut Vec<Event>) {
        let judge = record["judge"].as_str().unwrap_or("judge").to_string();
        match record["state"].as_str() {
            Some("down") => {
                // A second `down` with no `up` between keeps the first: the
                // judge has answered nothing since it.
                // The bound runs from the first failed call, when known.
                let first = record["first_failed_ms"]
                    .as_i64()
                    .unwrap_or(at_ms)
                    .min(at_ms);
                self.judge_down.entry(judge.clone()).or_insert(Pending {
                    event_id,
                    at_ms: first,
                    subject: judge,
                    evidence: json!({"error": scrub(record["error"].as_str().unwrap_or_default())}),
                });
            }
            Some("up") => {
                if let Some(pending) = self.judge_down.remove(&judge) {
                    out.push(Event::Clear {
                        key: format!("judge_down:{}", pending.event_id),
                        evidence: json!({
                            "event_id": event_id,
                            "failed_calls": record["failed_calls"],
                            "down_ms": at_ms.saturating_sub(pending.at_ms),
                        }),
                    });
                } else {
                    // Its `down` fell out of the history: the open alert
                    // still clears, without a span.
                    for key in self.open_keys("judge_down", &judge) {
                        out.push(Event::Clear {
                            key,
                            evidence: json!({
                                "event_id": event_id,
                                "failed_calls": record["failed_calls"],
                            }),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// A backup record: the stale one it clears, and 031's over-alert budget.
    fn backup_record(&mut self, event_id: i64, at_ms: i64, record: &Value, out: &mut Vec<Event>) {
        let subject = if record["type"] == "backup.dumped" {
            "dump"
        } else {
            "bundle"
        };
        let key = record["keys"]
            .as_array()
            .and_then(|keys| keys.first())
            .and_then(Value::as_str)
            .map(str::to_string);
        self.backups.insert(
            subject,
            Backup {
                event_id,
                at_ms,
                key: key.clone(),
            },
        );
        for open in self.open_keys("backup_stale", subject) {
            out.push(Event::Clear {
                key: open,
                evidence: json!({"key": key, "event_id": event_id}),
            });
        }
        match record["over_alert"].as_bool() {
            Some(true) if self.open_keys("backup_budget", "r2").is_empty() => {
                out.push(Event::Fire {
                    key: format!("backup_budget:{event_id}"),
                    kind: "backup_budget",
                    subject: "r2".to_string(),
                    evidence: json!({
                        "held_bytes": record["held_bytes"],
                        "written_bytes": record["written_bytes"],
                        "alert_bytes": record["alert_bytes"],
                        "limit_bytes": record["limit_bytes"],
                        "skipped": record["skipped"], "event_id": event_id,
                    }),
                    text: format!(
                        "R2 holds {} bytes, over its alert threshold of {} (031)",
                        record["held_bytes"].as_i64().unwrap_or_default(),
                        record["alert_bytes"].as_i64().unwrap_or_default()
                    ),
                    notice: false,
                });
            }
            Some(false) => {
                for open in self.open_keys("backup_budget", "r2") {
                    out.push(Event::Clear {
                        key: open,
                        evidence: json!({
                            "held_bytes": record["held_bytes"], "event_id": event_id,
                        }),
                    });
                }
            }
            _ => {}
        }
    }

    /// A restore drill's verdict.
    fn verified_record(&mut self, event_id: i64, record: &Value, out: &mut Vec<Event>) {
        if record["ok"].as_bool() == Some(false) {
            out.push(Event::Fire {
                key: format!("drill_failed:{event_id}"),
                kind: "drill_failed",
                subject: "drill".to_string(),
                evidence: json!({
                    "key": record["key"], "reason": record["reason"],
                    "events": record["events"], "max_event_id": record["max_event_id"],
                    "expected": record["expected"], "event_id": event_id,
                }),
                text: format!(
                    "the restore drill failed: {}",
                    record["reason"].as_str().unwrap_or("no reason given")
                ),
                notice: false,
            });
        } else {
            for open in self.open_keys("drill_failed", "drill") {
                out.push(Event::Clear {
                    key: open,
                    evidence: json!({"key": record["key"], "event_id": event_id}),
                });
            }
        }
    }

    /// How a deploy of the live system ended (#54, deploy/deploy.sh): a
    /// failure fires `deploy_failed` while none is open (deploy-on-landing
    /// tries a failed main again after a while, and its failure then is the
    /// same occurrence), and a success, from any deploy, clears it.
    fn deploy_record(&mut self, event_id: i64, record: &Value, out: &mut Vec<Event>) {
        let open = self.open_keys("deploy_failed", "deploy");
        if record["ok"].as_bool() == Some(false) {
            if open.is_empty() {
                let head = record["head"].as_str().unwrap_or_default();
                out.push(Event::Fire {
                    key: format!("deploy_failed:{event_id}"),
                    kind: "deploy_failed",
                    subject: "deploy".to_string(),
                    evidence: json!({"head": record["head"], "by": record["by"],
                                     "reason": record["reason"], "event_id": event_id}),
                    text: format!(
                        "{} deploy of {} failed: {}",
                        if record["by"] == "landing" {
                            "the landing's"
                        } else {
                            "the owner's"
                        },
                        head.get(..12).unwrap_or(head),
                        record["reason"]
                            .as_str()
                            .filter(|reason| !reason.is_empty())
                            .unwrap_or("no reason given")
                    ),
                    notice: false,
                });
            }
        } else if record["ok"].as_bool() == Some(true) {
            for key in open {
                out.push(Event::Clear {
                    key,
                    evidence: json!({"head": record["head"], "by": record["by"],
                                     "event_id": event_id}),
                });
            }
        }
    }

    /// A model login that expired, and the clear that a person fixed it.
    fn auth_record(&mut self, event_id: i64, record: &Value, out: &mut Vec<Event>) {
        let execution = record["execution_id"].as_str().unwrap_or_default();
        match record["type"].as_str().unwrap_or_default() {
            "execution.interrupted" if record["cause"].as_str() == Some("model_auth") => {
                let key = format!("model_auth:{event_id}");
                self.auth.insert(execution.to_string(), key.clone());
                out.push(Event::Fire {
                    key,
                    kind: "model_auth",
                    subject: record["session_id"]
                        .as_str()
                        .unwrap_or("an execution")
                        .to_string(),
                    evidence: json!({
                        "agent_id": record["agent_id"], "execution_id": record["execution_id"],
                        "evidence": record["evidence"], "event_id": event_id,
                    }),
                    text: "the model login expired: a person must log the engine in again"
                        .to_string(),
                    notice: false,
                });
            }
            "execution.cleared" => {
                if let Some(key) = self.auth.remove(execution) {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "waited_ms": record["waited_ms"], "event_id": event_id,
                        }),
                    });
                }
            }
            _ => {}
        }
    }

    /// Every session's latest judgement: a stuck one fires once, a calm one
    /// clears, and so does the end of the session's current execution,
    /// whatever its cause. An ended session fires nothing: the Observer
    /// judges once more after every end, so a judgement can be newer than
    /// the end it follows, and a restart that reads an old stuck judgement
    /// of a session that has ended must not open it again.
    ///
    /// A stuck judgement is a claim that someone must act (objective 057),
    /// so it fires nothing for an agent that waits because nothing is its
    /// to do: an ephemeral worker ([`ephemeral`]) whose task is past its part ([`PART_DONE`]: it waits
    /// for the gate, a reviewer, its landing or the owner's hold; its open
    /// alert cleared at the task record that moved it, [`Self::task_record`])
    /// and the supervisor with nothing to dispatch ([`Self::supervisor_work`];
    /// its open alert clears then). A worker's alert names its task and the
    /// one command that acts on it, `spawn task restart --id <task>` (a
    /// delivered task: the only one a restart takes).
    fn observe(&mut self, observations: &Value, out: &mut Vec<Event>) {
        for observation in observations.as_array().into_iter().flatten() {
            let session = observation["session_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let event_id = observation["event_id"].as_i64().unwrap_or_default();
            let judgement = &observation["observation"];
            let stuck = judgement["stuck"].as_f64();
            let open = self.open_keys("agent_stuck", &session);
            let ended = observation["ended_event"].as_i64();
            let is_supervisor = session == crate::supervise::SUPERVISOR_SESSION;
            let held = if is_supervisor {
                None
            } else {
                observation["agent_id"]
                    .as_str()
                    .and_then(|agent| self.held_task(agent))
            };
            let done = held.as_ref().is_some_and(|(_, state)| {
                PART_DONE.contains(&state.as_str())
                    && observation["agent_id"].as_str().is_some_and(ephemeral)
            });
            // A resident's task past its part is history, not what it is
            // stuck on: its alert names a task in work only.
            let held = held.filter(|(_, state)| done || !PART_DONE.contains(&state.as_str()));
            let dispatch = if is_supervisor {
                self.supervisor_work()
            } else {
                None
            };
            let idle_supervisor = dispatch.as_ref().is_some_and(Vec::is_empty);
            if let Some(ended) = ended {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "event_id": ended,
                            "cause": observation["ended_cause"],
                        }),
                    });
                }
            } else if stuck.is_some_and(|stuck| stuck < LIKELY) {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({"stuck": stuck, "event_id": event_id}),
                    });
                }
            } else if done {
                // Its task's record cleared it; one still open (a record
                // read before a restart) clears at a later judgement.
                let (task, state) = held.clone().unwrap_or_default();
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({"event_id": event_id, "task": task, "task_state": state}),
                    });
                }
            } else if idle_supervisor {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "event_id": self.watermark, "nothing_to_dispatch": true,
                        }),
                    });
                }
            } else if open.is_empty() && stuck.is_some_and(|stuck| stuck >= LIKELY) {
                let mut evidence = json!({
                    "agent_id": observation["agent_id"], "stuck": stuck,
                    "state": judgement["state"], "judge": judgement["judge"],
                    "event_id": event_id,
                });
                let mut text = format!(
                    "{session} is stuck ({:.2}): {}",
                    stuck.unwrap_or_default(),
                    judgement["state"].as_str().unwrap_or("unknown")
                );
                if let Some((task, state)) = &held {
                    let command = (state == "delivered").then(|| restart_command(*task));
                    evidence["task"] = json!(task);
                    evidence["task_state"] = json!(state);
                    evidence["command"] = json!(command);
                    text = format!("{text} on task #{task} ({state})");
                    if let Some(command) = &command {
                        text = format!("{text}; `{command}` gives it a fresh execution");
                    }
                }
                if let Some(dispatch) = &dispatch {
                    evidence["dispatch"] = json!(dispatch);
                    text = format!("{text}; it has to dispatch: {}", dispatch.join(", "));
                }
                out.push(Event::Fire {
                    key: format!("agent_stuck:{session}:{event_id}"),
                    kind: "agent_stuck",
                    subject: session.clone(),
                    evidence,
                    text,
                    notice: false,
                });
            }
        }
    }

    /// The task `agent` holds: its latest delivered or later ([`HELD`]),
    /// with its state. A task only assigned is not with the worker yet.
    fn held_task(&self, agent: &str) -> Option<(i64, String)> {
        self.tasks
            .iter()
            .filter(|(_, task)| task.agent == agent)
            .filter_map(|(id, task)| {
                task.state
                    .as_deref()
                    .filter(|state| HELD.contains(state))
                    .map(|state| (*id, state.to_string()))
            })
            .max_by_key(|(id, _)| *id)
    }

    /// What the supervisor has to dispatch, from the last read of its plan
    /// ([`Self::read_supervisor`]) and the step's own fold: a question to
    /// the owner, a pending or told escalation (a worker's own judgement
    /// excepted while that worker holds a task: its work, which its own
    /// alert names), and a mission waiting for it to assign its first task
    /// (open or active, not halted, not the integrator's gate, no task
    /// yet). Empty: nothing to dispatch. `None` when its plan was not read.
    fn supervisor_work(&self) -> Option<Vec<String>> {
        let read = self.supervisor.as_ref()?;
        let mut work = Vec::new();
        if read.question {
            work.push("a question to the owner".to_string());
        }
        for item in &read.items {
            let workers = item.judgement
                && item.agent.as_deref().is_some_and(|agent| {
                    self.held_task(agent).is_some_and(|(_, state)| {
                        ephemeral(agent) || !PART_DONE.contains(&state.as_str())
                    })
                });
            if !workers {
                work.push(format!("escalation {}", item.tag));
            }
        }
        let mut missions: Vec<(&i64, &Mission)> = self.missions.iter().collect();
        missions.sort_by_key(|(id, _)| **id);
        for (id, mission) in missions {
            let waiting = matches!(mission.state.as_str(), "open" | "active")
                && mission.halted.is_none()
                && mission.accept.as_deref() != Some("gate")
                && !self.tasks.values().any(|task| task.mission == *id);
            if waiting {
                work.push(format!("mission #{id} waits for its first task"));
            }
        }
        Some(work)
    }

    /// Read what the supervisor has to dispatch, when its `agent_stuck`
    /// depends on it: its latest judgement (in `observations`) is stuck and
    /// its execution has not ended, or its alert is open. At most once per
    /// `SPAWN_ALERT_CHECK_SECS`; a read that fails is said, and its
    /// judgement alone decides until the next.
    fn read_supervisor(
        &mut self,
        store: &PostgresStore,
        now: i64,
        observations: &Value,
        lines: &mut Vec<String>,
    ) {
        let session = crate::supervise::SUPERVISOR_SESSION;
        let stuck = observations
            .as_array()
            .into_iter()
            .flatten()
            .filter(|observation| observation["session_id"] == session)
            .any(|observation| {
                observation["ended_event"].is_null()
                    && observation["observation"]["stuck"]
                        .as_f64()
                        .is_some_and(|stuck| stuck >= LIKELY)
            });
        if !stuck && self.open_keys("agent_stuck", session).is_empty() {
            self.supervisor_due_ms = 0;
            return;
        }
        if now < self.supervisor_due_ms {
            return;
        }
        self.supervisor_due_ms = now.saturating_add(self.settings.check_ms);
        match crate::serve::attention::supervisor_dispatch(store) {
            Ok(value) => {
                // An order no task can meet is said once in this process,
                // by the first read of the plan: this one, at times.
                lines.extend(
                    value["lines"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .filter(|line| !line.starts_with("dispatch: stalls:"))
                        .map(str::to_string),
                );
                self.supervisor = Some(Dispatch::from_value(&value));
            }
            Err(error) => {
                self.supervisor = None;
                lines.push(format!("alerts: the supervisor's dispatch: {error}"));
            }
        }
    }

    /// Commands that finished failed, an owner's note excepted.
    fn commands(&mut self, commands: &Value, out: &mut Vec<Event>) {
        for command in commands.as_array().into_iter().flatten() {
            let id = command["command"].as_i64().unwrap_or_default();
            let key = command["key"].as_str().unwrap_or_default();
            if key.starts_with("owner-note-")
                || self.known.contains_key(&format!("command_failed:{id}"))
            {
                continue;
            }
            let error = scrub(command["error"].as_str().unwrap_or_default());
            out.push(Event::Fire {
                key: format!("command_failed:{id}"),
                kind: "command_failed",
                subject: format!("command #{id}"),
                evidence: json!({
                    "command": id, "agent_id": command["agent_id"],
                    "session_id": command["session_id"],
                    "principal": command["principal"],
                    "action": command["action"], "key": command["key"], "error": error,
                }),
                text: format!("command #{id} failed: {error}"),
                notice: true,
            });
        }
    }

    /// The commands of the open `message_not_taken` alerts.
    fn open_messages(&self) -> Vec<i64> {
        self.known
            .iter()
            .filter(|(_, row)| row.open() && row.kind == "message_not_taken")
            .filter_map(|(key, _)| key.strip_prefix("message_not_taken:")?.parse().ok())
            .collect()
    }

    /// Read the commands after the last one seen, the pending messages due
    /// at `now`, and the open alerts' that may have cleared
    /// ([`PostgresStore::alert_messages_since`]), and decide. An error is
    /// said and read again at the next pass.
    fn read_messages(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        let due: Vec<i64> = self
            .pending
            .iter()
            .filter(|(_, due)| **due <= now)
            .map(|(id, _)| *id)
            .collect();
        match store.alert_messages_since(
            self.settings.message_taken_ms,
            self.messages_after,
            &due,
            &self.open_messages(),
            self.messages_since,
        ) {
            Ok(messages) => self.messages(&messages, out),
            Err(error) => lines.push(format!("alerts: messages: {error}")),
        }
    }

    /// `message_not_taken`: a tracked message no `message.taken` names past
    /// its bound, its session not ended with it and no later message of its
    /// session taken, fires once; an open one clears once it is taken, else
    /// once a later message of its session is taken (`later_taken`), else
    /// once its session ended with it untaken.
    /// One not due yet is pending, read again until it is taken, ends or
    /// fires; the next bound is the earliest pending one's.
    fn messages(&mut self, messages: &Value, out: &mut Vec<Event>) {
        for message in messages["messages"].as_array().into_iter().flatten() {
            let Some(id) = message["command_id"].as_i64() else {
                continue;
            };
            let key = format!("message_not_taken:{id}");
            let taken = message["taken"].as_i64().unwrap_or_default();
            let ended = message["ended"].as_bool() != Some(false);
            // A later message of its session taken (objective 138): the
            // agent went on past it, its marker missed.
            let later_taken = message["later_taken"].as_i64();
            if self.known.contains_key(&key) {
                self.pending.remove(&id);
                if !self.is_open(&key) {
                    continue;
                }
                // Taken; else a later message of its session taken; else
                // its session ended with it.
                let evidence = if taken > 0 {
                    json!({"taken": taken, "taken_at_ms": message["taken_at_ms"]})
                } else if let Some(later) = later_taken {
                    json!({"later_taken": later, "taken_at_ms": message["later_taken_at_ms"]})
                } else if ended {
                    json!({"ended": true, "state": message["state"]})
                } else {
                    continue;
                };
                out.push(Event::Clear { key, evidence });
                continue;
            }
            if message["tracked"].as_bool() != Some(true)
                || taken > 0
                || later_taken.is_some()
                || ended
            {
                self.pending.remove(&id);
                continue;
            }
            // Pending until its alert is in the store: a fire the store
            // refused for an error is tried again at the next reading.
            let now = messages["now_ms"].as_i64().unwrap_or_default();
            let due = message["due_ms"].as_i64().unwrap_or(now);
            if message["not_taken"].as_bool() != Some(true) {
                self.pending.insert(id, due);
                continue;
            }
            self.pending.insert(id, now);
            let typed = message["typed"].as_bool() == Some(true);
            let agent = message["agent_id"].as_str().unwrap_or_default();
            let action = message["action"].as_str().unwrap_or("input");
            let secs = self.settings.message_taken_ms / 1000;
            // The Bridge's own record (S17): the transcript lines it read
            // after the typing. None read: Spawn cannot tell whether the
            // agent took it, so the alert says that and not that it did not.
            let read = message["read_since_typed"].as_i64();
            out.push(Event::Fire {
                key,
                kind: "message_not_taken",
                subject: message["session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                evidence: json!({
                    "command": id, "agent_id": agent, "session_id": message["session_id"],
                    "action": action, "typed": typed,
                    "recorded_at_ms": message["created_at_ms"],
                    "bound_ms": self.settings.message_taken_ms,
                    "read_since_typed": read,
                }),
                text: if typed && read == Some(0) {
                    format!(
                        "message #{id} to {agent} (its {action}) was typed, and {secs} s after \
                         it could be typed Spawn has read no line of {agent}'s transcript since the \
                         typing: whether {agent} took it is unknown. Either Spawn is not reading \
                         that transcript (the Bridge follows it), or the engine has written \
                         nothing since"
                    )
                } else if typed {
                    format!(
                        "{agent} has not taken message #{id} (its {action}): typed, and the \
                         transcript lines Spawn has read since ({}) hold no marker of it {secs} s \
                         after it could be typed",
                        match read {
                            Some(100) => "100 or more".to_string(),
                            Some(lines) => lines.to_string(),
                            None => "a number not read".to_string(),
                        }
                    )
                } else {
                    format!(
                        "{agent} has not taken message #{id} (its {action}): never typed \
                         {secs} s after it could be typed (sent to a ready session, or the turn \
                         it waited behind ended)"
                    )
                },
                notice: false,
            });
        }
        if let Some(max) = messages["max_id"].as_i64() {
            self.messages_after = self.messages_after.max(max);
        }
        if let Some(now) = messages["now_ms"].as_i64() {
            self.messages_since = now.saturating_sub(MESSAGES_MARGIN_MS);
        }
        self.messages_due = self.pending.values().copied().min();
    }

    /// The owner's landing requests with no outcome, as the store has them
    /// on start (`source_pushes_pending`, the integrator's queue); an open
    /// `owner_request_waiting` whose request ended while the loop was down
    /// clears, naming that outcome.
    fn seed_owner_requests(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
    ) -> Result<(), String> {
        let pending = store
            .source_pushes_pending()
            .map_err(|error| error.to_string())?;
        for request in pending.as_array().into_iter().flatten() {
            let Some(id) = request["request"].as_i64() else {
                continue;
            };
            self.owner_requests.entry(id).or_insert(Pending {
                event_id: id,
                at_ms: request["at_ms"].as_i64().unwrap_or(now),
                subject: request["source"].as_str().unwrap_or_default().to_string(),
                evidence: json!({"commit": request["commit"]}),
            });
        }
        let ended: Vec<(String, i64)> = self
            .known
            .iter()
            .filter(|(_, row)| row.open() && row.kind == "owner_request_waiting")
            .filter_map(|(key, row)| Some((key.clone(), row.evidence["request"].as_i64()?)))
            .filter(|(_, request)| !self.owner_requests.contains_key(request))
            .collect();
        for (key, request) in ended {
            let outcome = store
                .source_push_outcome(request)
                .map_err(|error| error.to_string())?;
            if let Some(outcome) = outcome {
                out.push(Event::Clear {
                    key,
                    evidence: json!({"event_id": outcome["event_id"], "record": outcome["type"],
                                     "request": request}),
                });
            }
        }
        Ok(())
    }

    /// A `source.push_requested`: the owner's landing request, pending
    /// until its outcome (`owner_request_waiting`).
    fn owner_request(&mut self, event_id: i64, at_ms: i64, record: &Value) {
        self.owner_requests.entry(event_id).or_insert(Pending {
            event_id,
            at_ms,
            subject: record["source"].as_str().unwrap_or_default().to_string(),
            evidence: json!({"commit": record["commit"]}),
        });
    }

    /// A `source.moved` or `source.push_refused` that names a request: its
    /// outcome. The request is no longer pending, and its alert clears.
    fn owner_request_ended(&mut self, event_id: i64, record: &Value, out: &mut Vec<Event>) {
        let Some(request) = record["request"].as_i64() else {
            return;
        };
        self.owner_requests.remove(&request);
        let key = format!("owner_request_waiting:{request}");
        if self.is_open(&key) {
            out.push(Event::Clear {
                key,
                evidence: json!({"event_id": event_id, "record": record["type"],
                                 "request": request}),
            });
        }
    }

    /// The owner's landing requests that have had no outcome for
    /// `SPAWN_ALERT_OWNER_REQUEST_SECS` (`owner_request_waiting`), once each.
    fn owner_request_bounds(&self, now: i64, out: &mut Vec<Event>) {
        let bound = self.settings.owner_request_ms;
        if bound <= 0 {
            return;
        }
        for (request, pending) in &self.owner_requests {
            let key = format!("owner_request_waiting:{request}");
            if now < pending.at_ms.saturating_add(bound) || self.known.contains_key(&key) {
                continue;
            }
            let waited = now.saturating_sub(pending.at_ms);
            let commit = pending.evidence["commit"].as_str().unwrap_or_default();
            let withdraw = format!("spawn mission push --withdraw {request}");
            out.push(Event::Fire {
                key,
                kind: "owner_request_waiting",
                subject: pending.subject.clone(),
                evidence: json!({
                    "request": request, "source": pending.subject, "commit": commit,
                    "requested_ms": pending.at_ms, "waited_ms": waited,
                    "threshold_ms": bound, "event_id": request, "withdraw": withdraw,
                }),
                text: format!(
                    "the owner's landing request #{request} ({}) on {} has had no outcome for \
                     {}: deploy-on-landing waits for it unless main holds its commit already \
                     (S23). Its verdict ends it, or `{withdraw}`",
                    commit.get(..12).unwrap_or(commit),
                    pending.subject,
                    idle_words(waited)
                ),
                notice: false,
            });
        }
    }

    /// The model judge's `down` records that reached their bound
    /// (`judge_down`), once each.
    fn judge_bounds(&self, now: i64, out: &mut Vec<Event>) {
        for pending in self.judge_down.values() {
            let due = pending.at_ms.saturating_add(self.settings.judge_down_ms);
            let key = format!("judge_down:{}", pending.event_id);
            if now < due || self.known.contains_key(&key) {
                continue;
            }
            let error = pending.evidence["error"].as_str().unwrap_or_default();
            out.push(Event::Fire {
                key,
                kind: "judge_down",
                subject: pending.subject.clone(),
                evidence: json!({
                    "judge": pending.subject, "error": error,
                    "down_ms": now.saturating_sub(pending.at_ms),
                    "threshold_ms": self.settings.judge_down_ms,
                    "event_id": pending.event_id,
                }),
                text: format!(
                    "the model judge {} failed two calls in a row, the first {} s ago, \
                     and has answered none since: {error}",
                    pending.subject,
                    now.saturating_sub(pending.at_ms) / 1000
                ),
                notice: false,
            });
        }
    }

    /// The bounds that fall due: an undelivered task, an admission's wait, a
    /// judge that answers nothing, a backup's age, a crash line gone quiet.
    fn bounds(&mut self, now: i64, out: &mut Vec<Event>) {
        for (id, task) in &self.tasks {
            let Some(assigned) = task.assigned_event else {
                continue;
            };
            if task.state.as_deref() != Some("assigned") || !task.agent_free {
                continue;
            }
            let due = task
                .assigned_at
                .saturating_add(self.settings.undelivered_ms);
            if now < due {
                continue;
            }
            let key = format!("task_undelivered:{id}:{assigned}");
            if self.known.contains_key(&key) {
                continue;
            }
            out.push(Event::Fire {
                key,
                kind: "task_undelivered",
                subject: format!("task #{id}"),
                evidence: json!({
                    "task": id, "mission": task.mission, "agent_id": task.agent,
                    "assigned_event_id": assigned, "assigned_at_ms": task.assigned_at,
                    "waited_ms": now.saturating_sub(task.assigned_at),
                    "threshold_ms": self.settings.undelivered_ms,
                    "event_id": assigned,
                }),
                text: format!(
                    "task #{id} has been assigned for {} s with its agent free",
                    now.saturating_sub(task.assigned_at) / 1000
                ),
                notice: false,
            });
        }
        for (matched, pending) in &self.admitted {
            let due = pending.at_ms.saturating_add(self.settings.admission_ms);
            let key = format!("admission_wait:{}", pending.event_id);
            if now < due || self.known.contains_key(&key) {
                continue;
            }
            out.push(Event::Fire {
                key,
                kind: "admission_wait",
                subject: pending.subject.clone(),
                evidence: json!({
                    "kind": pending.evidence["kind"],
                    "envelope_bytes": pending.evidence["envelope_bytes"],
                    "measure": pending.evidence["measure"],
                    "measured_bytes": pending.evidence["measured_bytes"],
                    "threshold_bytes": pending.evidence["threshold_bytes"],
                    "waited_ms": now.saturating_sub(pending.at_ms),
                    "threshold_ms": self.settings.admission_ms,
                    "event_id": pending.event_id,
                }),
                text: format!(
                    "{} has waited {} s to be admitted",
                    matched,
                    now.saturating_sub(pending.at_ms) / 1000
                ),
                notice: false,
            });
        }
        self.judge_bounds(now, out);
        self.owner_request_bounds(now, out);
        for (subject, max_age) in [
            ("dump", self.settings.dump_max_age_ms),
            ("bundle", self.settings.bundle_max_age_ms),
        ] {
            let Some(backup) = self.backups.get(subject) else {
                continue;
            };
            let due = backup.at_ms.saturating_add(max_age);
            let key = format!("backup_stale:{subject}:{}", backup.event_id);
            if now < due || self.known.contains_key(&key) {
                continue;
            }
            let age = now.saturating_sub(backup.at_ms);
            out.push(Event::Fire {
                key,
                kind: "backup_stale",
                subject: subject.to_string(),
                evidence: json!({
                    "key": backup.key, "age_secs": age / 1000,
                    "max_age_secs": max_age / 1000, "event_id": backup.event_id,
                }),
                text: format!(
                    "the newest {subject} is {} s old (its schedule allows {} s)",
                    age / 1000,
                    max_age / 1000
                ),
                notice: false,
            });
        }
        self.quiet_crashes(now, out);
    }

    /// A crash line that no ready line followed, and that has been silent
    /// for `SPAWN_ALERT_PG_QUIET_SECS`, clears itself.
    fn quiet_crashes(&self, now: i64, out: &mut Vec<Event>) {
        for log in &self.logs {
            let open = self.open_keys("postgres_crash", log.subject);
            let quiet = now.saturating_sub(log.last_match_ms);
            if open.is_empty() || quiet < self.settings.quiet_ms {
                continue;
            }
            for key in open {
                out.push(Event::Clear {
                    key,
                    evidence: json!({
                        "quiet_secs": quiet / 1000, "last_line": log.last_line,
                        "reading_ms": now,
                    }),
                });
            }
        }
    }

    /// The host and the logs, read every `SPAWN_ALERT_CHECK_SECS`.
    fn read_host(&mut self, now: i64, out: &mut Vec<Event>, lines: &mut Vec<String>) {
        self.read_memory(now, out, lines);
        self.read_tmpfs(now, out, lines);
        self.read_disk(now, out, lines);
        for index in 0..self.logs.len() {
            self.read_log(index, now, out, lines);
        }
    }

    /// The integrator-down reading (`integrator_down`): read only while
    /// the fold has a reported task of a mission open and not halted, or the
    /// alert is open.
    /// The open and active missions' sources that are paths on this host,
    /// and whether each is a directory now (`source_away`).
    fn read_sources(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        let listed = match store.alert_mission_sources() {
            Ok(listed) => listed,
            Err(error) => {
                lines.push(format!("alerts: mission sources: {error}"));
                return;
            }
        };
        let watched = host_sources(&listed);
        out.extend(self.source_events(&watched, |path| Path::new(path).is_dir(), now));
    }

    /// The open missions' gate directories, when a `validator_defect` is
    /// open: one whose scenario none of them holds any more clears `gone`
    /// ([`Monitor::gone_defects`]).
    fn read_defect_gates(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        if self.defects.is_empty() {
            return;
        }
        let listed = match store.alert_mission_gates() {
            Ok(listed) => listed,
            Err(error) => {
                lines.push(format!("alerts: mission gates: {error}"));
                return;
            }
        };
        let gates: Vec<(i64, Option<String>, Option<String>)> = listed
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                Some((
                    entry["mission"].as_i64()?,
                    entry["gate"].as_str().map(str::to_string),
                    entry["source"].as_str().map(str::to_string),
                ))
            })
            .collect();
        self.gone_defects(
            &gates,
            |path| Path::new(path).is_dir(),
            |path| Path::new(path).exists(),
            now,
            out,
        );
    }

    /// Clear each open `validator_defect` whose scenario is in none of the
    /// gate directories its source's open missions judge with now (`gates`:
    /// each open mission, its gate directory and first source): the
    /// deployed scenario set no longer has it (`cause` `gone`, at this
    /// reading, naming the directories read), so no gating is needed to
    /// tell. The source is the alert's own (an owner landing's), else its
    /// missions' (a task's, or an owner's from before alerts were per source,
    /// whose mission may have no gate of its own: a review mission). Kept
    /// when no gate directory is found for it, or one of them is not a
    /// directory now (nothing can be told).
    fn gone_defects(
        &mut self,
        gates: &[(i64, Option<String>, Option<String>)],
        is_dir: impl Fn(&str) -> bool,
        exists: impl Fn(&str) -> bool,
        now: i64,
        out: &mut Vec<Event>,
    ) {
        let source_of = |mission: i64| {
            gates
                .iter()
                .find(|(id, _, _)| *id == mission)
                .and_then(|(_, _, source)| source.clone())
        };
        self.clear_defects(out, |defect| {
            let sources: BTreeSet<String> = match &defect.source {
                Some(source) => BTreeSet::from([source.clone()]),
                None => defect.missions.iter().filter_map(|m| source_of(*m)).collect(),
            };
            let dirs: BTreeSet<&str> = gates
                .iter()
                .filter(|(_, _, source)| source.as_ref().is_some_and(|s| sources.contains(s)))
                .filter_map(|(_, gate, _)| gate.as_deref())
                .collect();
            if dirs.is_empty() || !dirs.iter().all(|dir| is_dir(dir)) {
                return None;
            }
            let held = dirs.iter().any(|dir| {
                exists(&Path::new(dir).join(&defect.scenario).to_string_lossy())
            });
            (!held).then(|| {
                json!({"cause": "gone", "reading_ms": now, "gates": dirs.iter().collect::<Vec<_>>()})
            })
        });
    }

    /// What a reading of `watched` (mission, source) decides, given what is
    /// a directory: fire for each source away at this reading and the one
    /// before with none open, clear each open one whose source is back or
    /// no longer watched.
    fn source_events(
        &mut self,
        watched: &BTreeSet<(i64, String)>,
        present: impl Fn(&str) -> bool,
        now: i64,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        let mut open = BTreeSet::new();
        for (key, row) in &self.known {
            if !row.open() || row.kind != "source_away" {
                continue;
            }
            let mission = row
                .subject
                .strip_prefix("mission #")
                .and_then(|id| id.parse::<i64>().ok());
            let source = row.evidence["source"].as_str().map(str::to_string);
            let (Some(mission), Some(source)) = (mission, source) else {
                continue;
            };
            let entry = (mission, source);
            if !watched.contains(&entry) {
                out.push(Event::Clear {
                    key: key.clone(),
                    evidence: json!({
                        "source": entry.1, "reason": "no longer watched", "reading_ms": now,
                    }),
                });
            } else if present(&entry.1) {
                out.push(Event::Clear {
                    key: key.clone(),
                    evidence: json!({"source": entry.1, "present": true, "reading_ms": now}),
                });
            } else {
                open.insert(entry);
            }
        }
        let mut away = BTreeMap::new();
        for (mission, source) in watched {
            let entry = (*mission, source.clone());
            if open.contains(&entry) || present(source) {
                continue;
            }
            // The first reading that finds it away only notes it.
            let Some(since) = self.away_once.get(&entry).copied() else {
                away.insert(entry, now);
                continue;
            };
            out.push(Event::Fire {
                key: format!("source_away:{mission}:{now}"),
                kind: "source_away",
                subject: format!("mission #{mission}"),
                evidence: json!({
                    "source": source, "present": false, "away_since_ms": since,
                    "reading_ms": now,
                }),
                text: format!(
                    "{source} is not a directory on this host: mission #{mission}'s \
                     branches are not published to it until it is back"
                ),
                notice: false,
            });
        }
        self.away_once = away;
        out
    }

    fn read_integrator(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        if self.settings.integrator_down_ms <= 0 {
            return;
        }
        let waits = self.tasks.values().any(|task| {
            task.state.as_deref() == Some("reported")
                && self.missions.get(&task.mission).is_some_and(|mission| {
                    mission.halted.is_none() && matches!(mission.state.as_str(), "open" | "active")
                })
        });
        if !waits && self.open_keys("integrator_down", INTEGRATOR).is_empty() {
            self.unlocked_since = None;
            return;
        }
        match store.alert_integrator() {
            Ok(reading) => self.integrator(now, &reading, out),
            Err(error) => lines.push(format!("alerts: the integrator's lock: {error}")),
        }
    }

    /// One reading of the integrator's lock and the work waiting for it
    /// (the store's `INTEGRATOR`): fire after the bound of readings with
    /// the lock free and work waiting, clear when the lock is held.
    fn integrator(&mut self, now: i64, reading: &Value, out: &mut Vec<Event>) {
        let Some(locked) = reading["locked"].as_bool() else {
            return;
        };
        let at = reading["now_ms"].as_i64().unwrap_or(now);
        let open = self.open_keys("integrator_down", INTEGRATOR);
        if locked {
            self.unlocked_since = None;
            for key in open {
                out.push(Event::Clear {
                    key,
                    evidence: json!({"locked": true, "reading_ms": at}),
                });
            }
            return;
        }
        let waiting = reading["waiting"].as_array().map_or(&[][..], Vec::as_slice);
        if waiting.is_empty() {
            // Nothing waits: no bound runs (an open alert stays open until
            // the integrator runs again).
            self.unlocked_since = None;
            return;
        }
        let since = *self.unlocked_since.get_or_insert(at);
        let down = at.saturating_sub(since);
        if !open.is_empty() || down < self.settings.integrator_down_ms {
            return;
        }
        let tasks: Vec<String> = waiting
            .iter()
            .take(5)
            .map(|entry| {
                format!(
                    "task #{} of mission #{}",
                    entry["task"].as_i64().unwrap_or_default(),
                    entry["mission"].as_i64().unwrap_or_default()
                )
            })
            .collect();
        out.push(Event::Fire {
            key: format!("integrator_down:{since}"),
            kind: "integrator_down",
            subject: INTEGRATOR.to_string(),
            evidence: json!({
                "since_ms": since, "reading_ms": at, "down_ms": down,
                "threshold_ms": self.settings.integrator_down_ms,
                "waiting": waiting.iter().take(20).collect::<Vec<_>>(),
                "waiting_count": waiting.len(),
            }),
            text: format!(
                "spawn-integrate is not running: no integrator has held its lock for {}s while \
                 {} reported task{} wait{} for a gate ({}{})",
                down / 1000,
                waiting.len(),
                if waiting.len() == 1 { "" } else { "s" },
                if waiting.len() == 1 { "s" } else { "" },
                tasks.join(", "),
                if waiting.len() > tasks.len() {
                    ", …"
                } else {
                    ""
                }
            ),
            notice: false,
        });
    }

    /// The controller-behind reading (`controller_behind`): read only while
    /// the fold has a reported task of a mission open and not halted, or the
    /// alert is open. Each listed mission's owner's latest landing is
    /// compared with its controller in its source, the controller's commit
    /// read through the commit map (objective 178).
    fn read_controllers(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        if self.settings.controller_behind_ms <= 0 {
            return;
        }
        let waits = self.tasks.values().any(|task| {
            task.state.as_deref() == Some("reported")
                && self.missions.get(&task.mission).is_some_and(|mission| {
                    mission.halted.is_none() && matches!(mission.state.as_str(), "open" | "active")
                })
        });
        if !waits
            && !self
                .known
                .values()
                .any(|row| row.open() && row.kind == "controller_behind")
        {
            self.behind_since.clear();
            return;
        }
        let reading = match store.alert_controllers() {
            Ok(reading) => reading,
            Err(error) => {
                lines.push(format!("alerts: the missions' controllers: {error}"));
                return;
            }
        };
        let at = reading["now_ms"].as_i64().unwrap_or(now);
        let mut behind = BTreeMap::new();
        for entry in reading["missions"].as_array().into_iter().flatten() {
            let (Some(mission), Some(gate), Some(source), Some(landed)) = (
                entry["mission"].as_i64(),
                entry["gate"].as_str(),
                entry["source"].as_str(),
                entry["landed"].as_str(),
            ) else {
                continue;
            };
            if !Path::new(source).is_absolute() {
                continue;
            }
            // The controller's commit read through the commit map
            // (objective 178): one dropped by a history rewrite, or unknown,
            // is never behind.
            let lag = crate::integrate::controller_behind(
                Path::new(source),
                Path::new(gate),
                landed,
                |commit| {
                    crate::commit_now::commit_now(store, Path::new(source), commit)
                        .map_err(|error| error.to_string())
                },
            );
            match lag {
                Ok(crate::integrate::ControllerLag::Behind(controller)) => {
                    behind.insert(mission, (controller, landed.to_string()));
                }
                Ok(_) => {}
                // Said like the reading's other errors: nothing is decided
                // from a reading the store could not finish.
                Err(error) => {
                    lines.push(format!("alerts: the missions' controllers: {error}"));
                    return;
                }
            }
        }
        out.extend(self.controller_events(&behind, at));
    }

    /// What a reading decides, given each waiting mission whose owner's
    /// latest landing is newer than its controller (`behind`: mission to
    /// controller and landing):
    /// fire for each behind for the bound with none open, clear each open
    /// one whose mission is no longer behind.
    fn controller_events(
        &mut self,
        behind: &BTreeMap<i64, (String, String)>,
        now: i64,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        let mut open = BTreeSet::new();
        for (key, row) in &self.known {
            if !row.open() || row.kind != "controller_behind" {
                continue;
            }
            let Some(mission) = row
                .subject
                .strip_prefix("mission #")
                .and_then(|id| id.parse::<i64>().ok())
            else {
                continue;
            };
            if behind.contains_key(&mission) {
                open.insert(mission);
            } else {
                out.push(Event::Clear {
                    key: key.clone(),
                    evidence: json!({"behind": false, "reading_ms": now}),
                });
            }
        }
        self.behind_since
            .retain(|mission, _| behind.contains_key(mission));
        for (mission, (controller, landed)) in behind {
            let since = *self.behind_since.entry(*mission).or_insert(now);
            let behind_ms = now.saturating_sub(since);
            if open.contains(mission) || behind_ms < self.settings.controller_behind_ms {
                continue;
            }
            out.push(Event::Fire {
                key: format!("controller_behind:{mission}:{since}"),
                kind: "controller_behind",
                subject: format!("mission #{mission}"),
                evidence: json!({
                    "controller": controller, "landed": landed, "since_ms": since,
                    "reading_ms": now, "behind_ms": behind_ms,
                    "threshold_ms": self.settings.controller_behind_ms,
                }),
                text: format!(
                    "mission #{mission}'s controller {} has not had the owner's landing {} \
                     for {}s: its gates wait for the deploy that switches the controller (a \
                     landing not deployed?)",
                    &controller[..controller.len().min(8)],
                    &landed[..landed.len().min(8)],
                    behind_ms / 1000
                ),
                notice: false,
            });
        }
        out
    }

    /// Whether a task of `mission` is in work: assigned, delivered,
    /// reported, sent back, gated, landing or held (not landed, accepted or
    /// cancelled).
    fn in_work(&self, mission: i64) -> bool {
        self.tasks.values().any(|task| {
            task.mission == mission
                && !matches!(
                    task.state.as_deref(),
                    Some("landed" | "accepted" | "cancelled")
                )
        })
    }

    /// A task record's part in `mission_idle`: an assignment ends the
    /// mission's idle time and clears its alert; a record that ends a task
    /// (or follows an ended one) with no task of the mission in work after
    /// it starts it, at that record.
    fn track_idle(
        &mut self,
        event_id: i64,
        at_ms: i64,
        kind: &str,
        mission: i64,
        out: &mut Vec<Event>,
    ) {
        if kind == "task.assigned" {
            if let Some(entry) = self.missions.get_mut(&mission) {
                entry.idle = None;
            }
            self.clear_idle(mission, event_id, kind, out);
            return;
        }
        let ended = matches!(
            kind,
            "task.landed"
                | "task.cancelled"
                | "task.aborted"
                | "task.over_budget"
                | "task.reviewed"
                | "task.gated"
        );
        if !ended || self.in_work(mission) {
            return;
        }
        // The latest such record, as the store's mirror reads it on start
        // (a restart's history is read with the tasks' states as they are).
        if let Some(entry) = self.missions.get_mut(&mission) {
            entry.idle = Some((at_ms, event_id));
        }
    }

    /// Clear the mission's open `mission_idle` alert, naming the record.
    fn clear_idle(&self, mission: i64, event_id: i64, record: &str, out: &mut Vec<Event>) {
        for key in self.open_keys("mission_idle", &format!("mission #{mission}")) {
            out.push(Event::Clear {
                key,
                evidence: json!({"event_id": event_id, "record": record}),
            });
        }
    }

    /// The idle missions' reading (`mission_idle`): a mission not closed
    /// that takes its work from an objective backlog, with no task in work
    /// since its last task ended `SPAWN_ALERT_MISSION_IDLE_SECS` ago or
    /// more, fires once (its key names the record that ended that task).
    fn read_idle(&self, now: i64, out: &mut Vec<Event>) {
        let bound = self.settings.mission_idle_ms;
        if bound <= 0 {
            return;
        }
        let mut ids: Vec<&i64> = self.missions.keys().collect();
        ids.sort_unstable();
        for id in ids {
            let mission = &self.missions[id];
            let Some((since_ms, event_id)) = mission.idle else {
                continue;
            };
            let subject = format!("mission #{id}");
            let key = format!("mission_idle:{id}:{event_id}");
            if mission.closed()
                || !mission.backlog
                || mission.paused
                || now.saturating_sub(since_ms) < bound
                || self.in_work(*id)
                || self.known.contains_key(&key)
                || !self.open_keys("mission_idle", &subject).is_empty()
            {
                continue;
            }
            let idle_ms = now.saturating_sub(since_ms);
            out.push(Event::Fire {
                key,
                kind: "mission_idle",
                subject: subject.clone(),
                evidence: json!({
                    "mission": id, "since_ms": since_ms, "event_id": event_id,
                    "idle_ms": idle_ms, "reading_ms": now,
                }),
                text: format!(
                    "{subject} has had no task in work for {} since its last task ended: \
                     its objective backlog has nothing open to assign",
                    idle_words(idle_ms)
                ),
                notice: false,
            });
        }
    }

    /// The daily budgets' reading (`cost_budget`): each budgeted mission's
    /// spend of the UTC day so far, on the database's clock.
    fn read_budgets(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        let day_start = now - now.rem_euclid(DAY_MS);
        match store.cost_budgets(day_start, now.saturating_add(1)) {
            Ok(reading) => self.budgets(now, &reading, out),
            Err(error) => lines.push(format!("alerts: the daily budgets: {error}")),
        }
    }

    /// One reading of the daily budgets (`[{mission, budget_usd,
    /// spent_usd}]`): fire for a mission whose day's spend reached its
    /// budget, once a day; clear an open one of another day, of a mission
    /// with no budget now, or of one back under it.
    fn budgets(&mut self, now: i64, reading: &Value, out: &mut Vec<Event>) {
        let day = utc_day(now);
        let rows = reading.as_array().map_or(&[][..], Vec::as_slice);
        let mut over: HashMap<String, (f64, f64)> = HashMap::new();
        let mut read: HashMap<String, (f64, f64)> = HashMap::new();
        for row in rows {
            let (Some(mission), Some(budget)) =
                (row["mission"].as_i64(), row["budget_usd"].as_f64())
            else {
                continue;
            };
            let spent = row["spent_usd"].as_f64().unwrap_or_default();
            let subject = format!("mission #{mission}");
            read.insert(subject.clone(), (spent, budget));
            if budget > 0.0 && spent >= budget {
                over.insert(subject.clone(), (spent, budget));
                let key = format!("cost_budget:{mission}:{day}");
                if !self.known.contains_key(&key) {
                    out.push(Event::Fire {
                        key,
                        kind: "cost_budget",
                        subject: subject.clone(),
                        evidence: json!({
                            "day": day, "spent_usd": spent, "budget_usd": budget,
                            "reading_ms": now,
                        }),
                        text: format!(
                            "{subject} spent {} today (UTC), over its {} daily budget",
                            crate::cost::dollars(spent),
                            crate::cost::dollars(budget)
                        ),
                        notice: false,
                    });
                }
            }
        }
        let open: Vec<(String, String)> = self
            .known
            .iter()
            .filter(|(_, row)| row.open() && row.kind == "cost_budget")
            .map(|(key, row)| (key.clone(), row.subject.clone()))
            .collect();
        for (key, subject) in open {
            let today = key.ends_with(&format!(":{day}"));
            if today && over.contains_key(&subject) {
                continue;
            }
            let evidence = match read.get(&subject) {
                _ if !today => json!({"day": day, "reading_ms": now, "why": "the day ended"}),
                Some((spent, budget)) => json!({
                    "day": day, "spent_usd": spent, "budget_usd": budget, "reading_ms": now,
                    "why": "under the daily budget",
                }),
                None => json!({"day": day, "reading_ms": now, "why": "no daily budget"}),
            };
            out.push(Event::Clear { key, evidence });
        }
    }

    /// The landed objectives' reading (`unverified_landings`): those since
    /// [`crate::verify::LISTED_SINCE_MS`] not verified, and how many of
    /// them landed the bound or more before `now`. A read that fails is a
    /// line; the alert stays as it was.
    fn read_unverified(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        let Some(old_at) = self.unverified_old_at(now) else {
            return;
        };
        match store.unverified_landings(crate::verify::LISTED_SINCE_MS, old_at) {
            Ok(reading) => self.unverified(now, &reading, out),
            Err(error) => lines.push(format!("alerts: the unverified landings: {error}")),
        }
    }

    /// The time at or before which a landing is old at `now`: `None` when
    /// the bound is 0 (never alerted, nothing read).
    fn unverified_old_at(&self, now: i64) -> Option<i64> {
        (self.settings.unverified_ms != 0).then(|| now.saturating_sub(self.settings.unverified_ms))
    }

    /// One reading of the landed objectives (`{unverified, old,
    /// newest_old_ms}`): fire when any is old and none is open; clear the
    /// open one when none is old.
    fn unverified(&mut self, now: i64, reading: &Value, out: &mut Vec<Event>) {
        const KIND: &str = "unverified_landings";
        const SUBJECT: &str = "objectives";
        let old = reading["old"].as_i64().unwrap_or_default();
        let unverified = reading["unverified"].as_i64().unwrap_or_default();
        let open = self.open_keys(KIND, SUBJECT);
        if old == 0 {
            for key in open {
                out.push(Event::Clear {
                    key,
                    evidence: json!({"old": 0, "unverified": unverified, "reading_ms": now}),
                });
            }
            return;
        }
        if !open.is_empty() {
            return;
        }
        let newest = reading["newest_old_ms"].as_i64().unwrap_or_default();
        let bound = self.settings.unverified_ms;
        let span = if bound == DAY_MS {
            "a day".to_string()
        } else {
            idle_words(bound)
        };
        let (objectives, them) = if old == 1 {
            ("objective", "it")
        } else {
            ("objectives", "them")
        };
        out.push(Event::Fire {
            key: format!("{KIND}:{newest}:{now}"),
            kind: KIND,
            subject: SUBJECT.to_string(),
            evidence: json!({
                "old": old, "unverified": unverified, "unverified_ms": bound,
                "newest_landed_ms": newest, "listed_since_ms": crate::verify::LISTED_SINCE_MS,
                "reading_ms": now,
            }),
            text: format!(
                "{old} landed {objectives} unverified for over {span}: spawn verify list names \
                 {them}"
            ),
            notice: false,
        });
    }

    fn read_memory(&mut self, now: i64, out: &mut Vec<Event>, lines: &mut Vec<String>) {
        if self.settings.memory_low_mb == 0 {
            return;
        }
        let measured = match available_mb() {
            Ok(measured) => measured,
            Err(error) => {
                lines.push(format!("alerts: {error}"));
                return;
            }
        };
        let open = self.open_keys("memory_low", "host");
        let (low, ok) = (self.settings.memory_low_mb, self.settings.memory_ok_mb);
        match crossing(!open.is_empty(), measured, low, ok, true) {
            Crossing::Fire => out.push(Event::Fire {
                key: format!("memory_low:{now}"),
                kind: "memory_low",
                subject: "host".to_string(),
                evidence: json!({
                    "measured_mb": measured, "threshold_mb": low, "top": top_processes(),
                    "reading_ms": now,
                }),
                text: format!("MemAvailable is {measured} MiB, under {low}"),
                notice: false,
            }),
            Crossing::Clear => {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "measured_mb": measured, "threshold_mb": ok, "reading_ms": now,
                        }),
                    });
                }
            }
            Crossing::Nothing => {}
        }
    }

    fn read_tmpfs(&mut self, now: i64, out: &mut Vec<Event>, lines: &mut Vec<String>) {
        if self.settings.tmpfs_high_mb == 0 {
            return;
        }
        let path = self.settings.tmpfs_path.clone();
        let measured = match used_mb(&path) {
            Ok(measured) => measured,
            Err(error) => {
                lines.push(format!("alerts: {error}"));
                return;
            }
        };
        let open = self.open_keys("tmpfs_high", "host");
        let (high, ok) = (self.settings.tmpfs_high_mb, self.settings.tmpfs_ok_mb);
        match crossing(!open.is_empty(), measured, high, ok, false) {
            Crossing::Fire => out.push(Event::Fire {
                key: format!("tmpfs_high:{now}"),
                kind: "tmpfs_high",
                subject: "host".to_string(),
                evidence: json!({
                    "path": path.display().to_string(),
                    "measured_mb": measured, "threshold_mb": high, "reading_ms": now,
                }),
                text: format!("{} holds {measured} MiB, over {high}", path.display()),
                notice: false,
            }),
            Crossing::Clear => {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "path": path.display().to_string(),
                            "measured_mb": measured, "threshold_mb": ok, "reading_ms": now,
                        }),
                    });
                }
            }
            Crossing::Nothing => {}
        }
    }

    /// The free space under the workspace root (`disk_low`): read with
    /// `df` when the threshold is set and there is a root; a `df` that
    /// fails is a line in the loop's log.
    fn read_disk(&mut self, now: i64, out: &mut Vec<Event>, lines: &mut Vec<String>) {
        if self.settings.disk_low_gb == 0 {
            return;
        }
        let Some(path) = self.settings.disk_root.clone() else {
            return;
        };
        match crate::disk::free_bytes(&path) {
            Ok(free) => self.disk_reading(now, &path, free, out),
            Err(error) => lines.push(format!("alerts: {error}")),
        }
    }

    /// One reading of `free` bytes under `path`: fires under the threshold,
    /// one open at a time; clears at or above the margin; between the two,
    /// nothing either way.
    fn disk_reading(&mut self, now: i64, path: &Path, free: u64, out: &mut Vec<Event>) {
        const GIB: u64 = 1 << 30;
        let (low, ok) = (self.settings.disk_low_gb, self.settings.disk_ok_gb);
        let (low_bytes, ok_bytes) = (low.saturating_mul(GIB), ok.saturating_mul(GIB));
        let open = self.open_keys("disk_low", "host");
        let shown = path.display().to_string();
        match crossing(!open.is_empty(), free, low_bytes, ok_bytes, true) {
            Crossing::Fire => {
                // Tenths of a GiB, rounded down, as whole numbers.
                let tenths = free / (GIB / 10);
                out.push(Event::Fire {
                    key: format!("disk_low:{now}"),
                    kind: "disk_low",
                    subject: "host".to_string(),
                    evidence: json!({
                        "path": shown, "free_bytes": free, "threshold_bytes": low_bytes,
                        "reading_ms": now,
                    }),
                    text: format!(
                        "{shown} has {}.{} GiB free, under {low} GiB",
                        tenths / 10,
                        tenths % 10
                    ),
                    notice: false,
                });
            }
            Crossing::Clear => {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "path": shown, "free_bytes": free, "threshold_bytes": ok_bytes,
                            "reading_ms": now,
                        }),
                    });
                }
            }
            Crossing::Nothing => {}
        }
    }

    /// The databases' sizes, every `SPAWN_ALERT_DATABASE_SECS`: the live
    /// one's (with its event log's bytes) through the store, each test
    /// database's through a connection of its own ([`Monitor::database_events`]).
    fn read_databases(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        let live = store.database_size().map_err(|error| error.to_string());
        let tests: Vec<(&'static str, Result<Value, String>)> = self
            .settings
            .test_databases
            .iter()
            .map(|(role, url)| {
                (
                    *role,
                    spawn_postgres::test_database_size(&url.0).map_err(|error| error.to_string()),
                )
            })
            .collect();
        self.database_events(now, live, &tests, out, lines);
    }

    /// `database_size`, from one reading: the live database's (`{name,
    /// bytes, schemas, log_bytes}`) and each test database's (`{name, bytes,
    /// schemas}`, by role). A database at or above the floor and above the
    /// ratio times the live log's bytes opens one alert (one open per
    /// database); one open whose database this reading finds no longer so
    /// clears, the reading its evidence. Without the live reading nothing
    /// is decided (there is no log to measure against); a test database
    /// that cannot be read is a line, and its alert, if any, stays as it
    /// was.
    fn database_events(
        &self,
        now: i64,
        live: Result<Value, String>,
        tests: &[(&'static str, Result<Value, String>)],
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        let live = match live {
            Ok(live) => live,
            Err(error) => {
                lines.push(format!(
                    "alerts: the live database's size: {}",
                    scrub(&error)
                ));
                return;
            }
        };
        let Some(log_bytes) = live["log_bytes"].as_u64() else {
            lines.push("alerts: the live database's size: no event log bytes".to_string());
            return;
        };
        let mut readings: Vec<DatabaseSize> = Vec::new();
        let mut read = |role: &'static str, value: &Value, lines: &mut Vec<String>| {
            match DatabaseSize::from_value(role, value) {
                Some(reading) => {
                    if readings.iter().all(|seen| seen.name != reading.name) {
                        readings.push(reading);
                    }
                }
                None => lines.push(format!("alerts: the {role} database's size: unreadable")),
            }
        };
        read("live", &live, lines);
        for (role, reading) in tests {
            match reading {
                Ok(value) => read(role, value, lines),
                Err(error) => lines.push(format!(
                    "alerts: the {role} database's size: {}",
                    scrub(error)
                )),
            }
        }
        let floor = u128::from(self.settings.database_floor_mb) * 1024 * 1024;
        let ratio = u128::from(self.settings.database_ratio);
        for reading in readings {
            let subject = format!("database {}", reading.name);
            let bytes = u128::from(reading.bytes);
            let over = bytes >= floor && bytes > ratio * u128::from(log_bytes);
            let evidence = json!({
                "database": reading.name, "role": reading.role, "bytes": reading.bytes,
                "log_bytes": log_bytes, "ratio": ratio_of(reading.bytes, log_bytes),
                "schemas": reading.schemas, "floor_mb": self.settings.database_floor_mb,
                "max_ratio": self.settings.database_ratio, "reading_ms": now,
            });
            let open = self.open_keys("database_size", &subject);
            if over {
                if open.is_empty() {
                    let against = ratio_of(reading.bytes, log_bytes).map_or_else(
                        || "and the live event log is empty".to_string(),
                        |ratio| {
                            format!(
                                "{ratio} times its live event log of {}",
                                size_text(log_bytes)
                            )
                        },
                    );
                    out.push(Event::Fire {
                        key: format!("database_size:{}:{now}", reading.name),
                        kind: "database_size",
                        text: format!(
                            "database {} ({}) holds {}, {against}: {} schemas",
                            reading.name,
                            reading.role,
                            size_text(reading.bytes),
                            reading.schemas
                        ),
                        subject,
                        evidence,
                        notice: false,
                    });
                }
            } else {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: evidence.clone(),
                    });
                }
            }
        }
    }

    /// Read what the log `index` appended since the last reading.
    fn read_log(&mut self, index: usize, now: i64, out: &mut Vec<Event>, lines: &mut Vec<String>) {
        let subject = self.logs[index].subject;
        let Some(path) = self.logs[index].path.clone() else {
            let message = if subject == "test" {
                "alerts: SPAWN_TEST_DATABASE_LOG names no log: the test database's crash lines are not read"
            } else {
                "alerts: SPAWN_DATABASE_LOG names no log: the live database's crash lines are not read"
            };
            self.say_once(index, message, lines);
            return;
        };
        let read = read_appended(&path, &mut self.logs[index]);
        let (text, truncated) = match read {
            Ok(Some(read)) => read,
            Ok(None) => {
                self.logs[index].said = false;
                return;
            }
            Err(error) => {
                self.say_once(
                    index,
                    &format!("alerts: {}: {error}", path.display()),
                    lines,
                );
                return;
            }
        };
        self.logs[index].said = false;
        self.lines_of_log(index, &path, now, &text, out);
        if truncated {
            lines.push(format!(
                "alerts: {} grew more than the step reads at once; the rest is read next time",
                path.display()
            ));
        }
    }

    /// Say `message` once per log, until it reads again.
    fn say_once(&mut self, index: usize, message: &str, lines: &mut Vec<String>) {
        if !self.logs[index].said {
            self.logs[index].said = true;
            lines.push(message.to_string());
        }
    }

    /// The crash lines of one reading: the occurrences that fire, and the
    /// ready line that clears an open one.
    fn lines_of_log(
        &mut self,
        index: usize,
        path: &Path,
        now: i64,
        text: &str,
        out: &mut Vec<Event>,
    ) {
        let subject = self.logs[index].subject;
        let mut matching: Vec<String> = Vec::new();
        for line in text.lines() {
            if let Some((_stamp, prefix)) = crash_stamp(line, now) {
                self.logs[index].last_match_ms = now;
                self.logs[index].last_line = Some(line.to_string());
                if matching.len() < CRASH_LINES && matching.last() != Some(&line.to_string()) {
                    matching.push(line.to_string());
                }
                let key = format!("postgres_crash:{subject}:{prefix}");
                if !self.known.contains_key(&key) {
                    out.push(Event::Fire {
                        key,
                        kind: "postgres_crash",
                        subject: subject.to_string(),
                        evidence: json!({
                            "log": path.display().to_string(),
                            "lines": matching, "reading_ms": now,
                        }),
                        text: format!("the {subject} database's log: {}", line.trim()),
                        notice: false,
                    });
                }
            } else if line.contains(READY_LINE) {
                for key in self.open_keys("postgres_crash", subject) {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "line": line.trim(),
                            "log": path.display().to_string(), "reading_ms": now,
                        }),
                    });
                }
            }
        }
    }

    /// Record `events`, in order, each once: the store refuses a second
    /// `alert.fired` for a key, so two loops at once still fire once.
    fn flush(
        &mut self,
        store: &PostgresStore,
        now: i64,
        events: &[Event],
        lines: &mut Vec<String>,
    ) {
        for event in events {
            let key = event.key().to_string();
            let record = match event {
                Event::Fire {
                    kind,
                    subject,
                    evidence,
                    text,
                    notice,
                    ..
                } => {
                    if self.known.contains_key(&key) {
                        continue;
                    }
                    json!({
                        "type": "alert.fired", "key": key, "kind": kind,
                        "subject": subject, "evidence": scrub_value(evidence),
                        "text": text, "notice": notice,
                    })
                }
                Event::Clear { evidence, .. } => {
                    if !self.clears(&key, evidence, now) {
                        continue;
                    }
                    let Some(row) = self.known.get(&key) else {
                        continue;
                    };
                    json!({
                        "type": "alert.cleared", "key": key, "kind": row.kind,
                        "subject": row.subject, "evidence": scrub_value(evidence),
                    })
                }
            };
            match store.alert_record(&record) {
                Ok(id) => {
                    let kind = record["kind"].as_str().unwrap_or_default().to_string();
                    let subject = record["subject"].as_str().unwrap_or_default().to_string();
                    let fired = record["type"] == "alert.fired";
                    if fired {
                        self.known.insert(
                            key.clone(),
                            Row {
                                kind,
                                subject: subject.clone(),
                                state: if record["notice"].as_bool() == Some(true) {
                                    "notice".to_string()
                                } else {
                                    "open".to_string()
                                },
                                evidence: record["evidence"].clone(),
                                fired_event_id: id,
                                fired_at_ms: now,
                            },
                        );
                        lines.push(format!("alerts: fired {key} ({subject})"));
                    } else if let Some(row) = self.known.get_mut(&key) {
                        row.state = "cleared".to_string();
                        lines.push(format!("alerts: cleared {key} ({subject})"));
                    }
                }
                Err(StoreError::Rejected(why)) => {
                    // Another loop recorded it (or the record was refused):
                    // the store's word stands, and the step goes on.
                    let fired = record["type"] == "alert.fired";
                    if fired {
                        self.known.entry(key.clone()).or_insert_with(|| Row {
                            kind: record["kind"].as_str().unwrap_or_default().to_string(),
                            subject: record["subject"].as_str().unwrap_or_default().to_string(),
                            state: if record["notice"].as_bool() == Some(true) {
                                "notice".to_string()
                            } else {
                                "open".to_string()
                            },
                            evidence: record["evidence"].clone(),
                            fired_event_id: 0,
                            fired_at_ms: 0,
                        });
                    } else if let Some(row) = self.known.get_mut(&key) {
                        // A clear the store refused: the alert is not open
                        // there (cleared or judged), so it is not retried.
                        row.state = "cleared".to_string();
                    }
                    lines.push(format!("alerts: {key}: {why}"));
                }
                Err(error) => {
                    lines.push(format!("alerts: {key}: {error}"));
                }
            }
        }
    }

    /// The earliest bound that has not fired yet, for the next due time.
    fn plan(&mut self, _now: i64) {
        let mut next = i64::MAX;
        for (id, task) in &self.tasks {
            if task.state.as_deref() != Some("assigned") || !task.agent_free {
                continue;
            }
            if let Some(assigned) = task.assigned_event
                && !self
                    .known
                    .contains_key(&format!("task_undelivered:{id}:{assigned}"))
            {
                next = next.min(
                    task.assigned_at
                        .saturating_add(self.settings.undelivered_ms),
                );
            }
        }
        for pending in self.admitted.values() {
            if !self
                .known
                .contains_key(&format!("admission_wait:{}", pending.event_id))
            {
                next = next.min(pending.at_ms.saturating_add(self.settings.admission_ms));
            }
        }
        for pending in self.judge_down.values() {
            if !self
                .known
                .contains_key(&format!("judge_down:{}", pending.event_id))
            {
                next = next.min(pending.at_ms.saturating_add(self.settings.judge_down_ms));
            }
        }
        if self.settings.owner_request_ms > 0 {
            for (request, pending) in &self.owner_requests {
                if !self
                    .known
                    .contains_key(&format!("owner_request_waiting:{request}"))
                {
                    next = next.min(pending.at_ms.saturating_add(self.settings.owner_request_ms));
                }
            }
        }
        for (subject, max_age) in [
            ("dump", self.settings.dump_max_age_ms),
            ("bundle", self.settings.bundle_max_age_ms),
        ] {
            if let Some(backup) = self.backups.get(subject)
                && !self
                    .known
                    .contains_key(&format!("backup_stale:{subject}:{}", backup.event_id))
            {
                next = next.min(backup.at_ms.saturating_add(max_age));
            }
        }
        for log in &self.logs {
            if !self.open_keys("postgres_crash", log.subject).is_empty() && log.last_match_ms > 0 {
                next = next.min(log.last_match_ms.saturating_add(self.settings.quiet_ms));
            }
        }
        if let Some(due) = self.messages_due {
            next = next.min(due);
        }
        self.next_bound_ms = (next != i64::MAX).then_some(next);
    }

    /// The keys of the open alerts of one kind and subject.
    fn open_keys(&self, kind: &str, subject: &str) -> Vec<String> {
        self.known
            .iter()
            .filter(|(_, row)| row.open() && row.kind == kind && row.subject == subject)
            .map(|(key, _)| key.clone())
            .collect()
    }

    fn is_open(&self, key: &str) -> bool {
        self.known.get(key).is_some_and(Row::open)
    }

    /// Whether a clear of `key` with `evidence`, decided at `now`, is
    /// recorded: the alert is open in the store, and the evidence comes
    /// after what it fired on ([`after`]). One that does not clears
    /// nothing, and is no error.
    fn clears(&self, key: &str, evidence: &Value, now: i64) -> bool {
        self.known
            .get(key)
            .is_some_and(|row| row.open() && row.fired_event_id != 0 && after(row, evidence, now))
    }
}

/// One database's size reading (`database_size`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct DatabaseSize {
    /// "live", "test" (the gates') or "fleet_test" (the engines').
    role: &'static str,
    name: String,
    bytes: u64,
    /// Its schemas other than `pg_*`, `information_schema` and `public`.
    schemas: u64,
}

impl DatabaseSize {
    fn from_value(role: &'static str, value: &Value) -> Option<Self> {
        Some(Self {
            role,
            name: value["name"].as_str()?.to_string(),
            bytes: value["bytes"].as_u64()?,
            schemas: value["schemas"].as_u64()?,
        })
    }
}

/// `bytes` over `log_bytes` to one decimal (half up); none for an empty
/// log.
fn ratio_of(bytes: u64, log_bytes: u64) -> Option<f64> {
    if log_bytes == 0 {
        return None;
    }
    let (bytes, log) = (u128::from(bytes), u128::from(log_bytes));
    let tenths = (bytes * 20 + log) / (2 * log);
    format!("{}.{}", tenths / 10, tenths % 10).parse().ok()
}

/// "12.0 MB" under 1024 MB, "3.0 GB" from there, as the storage card says
/// a size (half up).
fn size_text(bytes: u64) -> String {
    const MIB: u128 = 1024 * 1024;
    let bytes = u128::from(bytes);
    let (unit, scale) = if bytes < 1024 * MIB {
        ("MB", MIB)
    } else {
        ("GB", 1024 * MIB)
    };
    let tenths = (bytes * 20 + scale) / (2 * scale);
    format!("{}.{} {unit}", tenths / 10, tenths % 10)
}

/// Whether a clear's `evidence` comes after what the alert `row` fired on,
/// read from the alert's own evidence (what the store holds, never the
/// step's memory, which a restart empties): a record-borne clear names a
/// control record or event (`event_id`) later in the log than the one the
/// alert fired on; any other (a reading, `reading_ms`, or else the time it
/// is decided, `now`) is taken at or after the alert's reading, or its
/// firing.
fn after(row: &Row, evidence: &Value, now: i64) -> bool {
    if let (Some(by), Some(on)) = (
        evidence["event_id"].as_i64(),
        row.evidence["event_id"].as_i64(),
    ) {
        return by > on;
    }
    let read = evidence["reading_ms"].as_i64().unwrap_or(now);
    let fired = row.evidence["reading_ms"]
        .as_i64()
        .unwrap_or(row.fired_at_ms);
    read >= fired
}

impl LogWatch {
    fn new(subject: &'static str, path: Option<PathBuf>) -> Self {
        Self {
            subject,
            path,
            offset: 0,
            seated: false,
            carry: String::new(),
            said: false,
            last_match_ms: 0,
            last_line: None,
        }
    }
}

/// What one reading of a condition does: the crossing in, the crossing out
/// past the margin, or nothing (a reading between the two thresholds does
/// neither, so a host hovering at the threshold does not flap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Crossing {
    Fire,
    Clear,
    Nothing,
}

/// Where a reading leaves a condition: `turn_on` is the threshold it fires
/// over (or under, when `under`), `turn_off` the one it clears at, on the
/// other side of the margin.
/// The strings of a record's list, in order.
/// A span in the words an alert's text holds: "45 s", "12 min", "3 h 20 min".
fn idle_words(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    if seconds < 60 {
        format!("{seconds} s")
    } else if seconds < 3600 {
        format!("{} min", seconds / 60)
    } else {
        let minutes = (seconds % 3600) / 60;
        if minutes == 0 {
            format!("{} h", seconds / 3600)
        } else {
            format!("{} h {minutes} min", seconds / 3600)
        }
    }
}

/// The scenarios a gate's verdict says passed in it: run on the candidate
/// and passed (`passed`), or taken from the base's cache as solved
/// (`reused`).
fn passed_names(record: &Value) -> Vec<String> {
    let mut passed = names(&record["passed"]);
    passed.extend(names(&record["reused"]));
    passed
}

fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn crossing(open: bool, measured: u64, turn_on: u64, turn_off: u64, under: bool) -> Crossing {
    let bad = if under {
        measured < turn_on
    } else {
        measured > turn_on
    };
    let good = if under {
        measured >= turn_off
    } else {
        measured < turn_off
    };
    match (open, bad, good) {
        (false, true, _) => Crossing::Fire,
        (true, _, true) => Crossing::Clear,
        _ => Crossing::Nothing,
    }
}

/// The five fields a mission's notices compare.
fn snapshot(mission: &Mission) -> Value {
    json!({
        "state": mission.state,
        "halted": mission.halted,
        "gate_rejections": mission.rejections,
        "gate_aborts": mission.aborts,
        "gate_room_escalated": mission.escalated,
    })
}

/// What changed between two snapshots, in one phrase for a person.
fn changed(before: &Value, after: &Value) -> String {
    let mut parts = Vec::new();
    for field in [
        "state",
        "halted",
        "gate_rejections",
        "gate_aborts",
        "gate_room_escalated",
    ] {
        if before[field] != after[field] {
            parts.push(format!(
                "{field} {} -> {}",
                text_of(&before[field]),
                text_of(&after[field])
            ));
        }
    }
    parts.join(", ")
}

fn text_of(value: &Value) -> String {
    match value {
        Value::Null => "none".to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The mission's fold of one record, as far as the five watched fields go.
fn apply_mission(mission: &mut Mission, record: &Value) {
    match record["type"].as_str().unwrap_or_default() {
        "mission.closed" => {
            if let Some(state) = record["state"].as_str() {
                mission.state = state.to_string();
            }
        }
        "mission.halted" => {
            mission.halted = Some(record["reason"].as_str().unwrap_or("halted").to_string());
        }
        "mission.resumed" => {
            mission.halted = None;
            mission.rejections = 0;
            mission.run_objective = None;
            mission.aborts = 0;
            mission.escalated = false;
        }
        "mission.configured" => {
            mission.accept = record["accept"].as_str().map(str::to_string);
            mission.backlog = record["objectives"].is_object();
        }
        "task.assigned" if mission.state == "open" => {
            mission.state = "active".to_string();
        }
        "task.aborted" => mission.aborts += 1,
        _ => {}
    }
}

/// The fold's counter changes of a gating of a task of `objective`.
fn apply_gating(mission: &mut Mission, record: &Value, objective: Option<i64>) {
    match record["verdict"].as_str().unwrap_or_default() {
        "rejected" => {
            // The run's objective: the first rejection's task's, kept while
            // every later one is a task of the same objective.
            mission.run_objective = if mission.rejections == 0 {
                objective
            } else {
                mission.run_objective.filter(|run| Some(*run) == objective)
            };
            mission.rejections += 1;
        }
        "accepted" => {
            mission.rejections = 0;
            mission.run_objective = None;
        }
        _ => {}
    }
    match record["verdict"].as_str().unwrap_or_default() {
        "aborted" => mission.aborts += 1,
        "stale" | "no_room" | "host_fault" => {}
        _ => mission.aborts = 0,
    }
    match record["verdict"].as_str().unwrap_or_default() {
        "no_room" => {
            mission.escalated |= record["escalate"].as_bool().unwrap_or(false);
        }
        "stale" => {}
        _ => mission.escalated = false,
    }
}

/// The fold's change of a cancel of a task of `objective`: one of the
/// objective every rejection of the mission's run was ends that run.
fn apply_cancel(mission: &mut Mission, objective: Option<i64>) {
    if objective.is_some() && mission.run_objective == objective {
        mission.rejections = 0;
        mission.run_objective = None;
    }
}

/// The task state a record leaves, or `None` when it leaves it as it was.
fn task_state_after(record: &Value, accept_gate: bool, revisions: i64) -> Option<String> {
    if record["revision"]
        .as_i64()
        .is_some_and(|revision| revision != revisions)
    {
        return None;
    }
    let state = match record["type"].as_str()? {
        "task.assigned" => "assigned",
        "task.delivered" => "delivered",
        "task.reported" => "reported",
        "task.gated" => match record["verdict"].as_str()? {
            "accepted" => "landing",
            "rejected" | "conflict" | "incompatible" => "revise",
            "stale" => {
                if accept_gate {
                    "reported"
                } else {
                    "accepted"
                }
            }
            _ => return None,
        },
        "task.reviewed" => {
            if record["accepted"].as_bool()? {
                "accepted"
            } else {
                "revise"
            }
        }
        "task.landed" => "landed",
        "task.aborted" | "task.over_budget" | "task.cancelled" => "cancelled",
        _ => return None,
    };
    Some(state.to_string())
}

/// Whether the record moves the task's revision count (a rejection, a
/// conflict, an incompatible head or a send-back).
fn bumps_revision(record: &Value) -> bool {
    match record["type"].as_str().unwrap_or_default() {
        "task.gated" => matches!(
            record["verdict"].as_str(),
            Some("rejected" | "conflict" | "incompatible")
        ),
        "task.reviewed" => record["accepted"].as_bool() == Some(false),
        _ => false,
    }
}

/// 033's record: a host fault that says the base disagrees with itself, with
/// `alert` true.
fn base_disagrees(record: &Value) -> Option<()> {
    (record["type"] == "task.gated"
        && record["verdict"] == "host_fault"
        && record["evidence"] == "base_disagrees"
        && record["alert"].as_bool() == Some(true))
    .then_some(())
}

/// MemAvailable of `/proc/meminfo`, in MiB.
fn available_mb() -> Result<u64, String> {
    let text = std::fs::read_to_string("/proc/meminfo")
        .map_err(|error| format!("/proc/meminfo: {error}"))?;
    let line = text
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))
        .ok_or_else(|| "/proc/meminfo: no MemAvailable".to_string())?;
    let kb: u64 = line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("/proc/meminfo: cannot read {line:?}"))?;
    Ok(kb / 1024)
}

/// The used MiB of the filesystem that holds `path`, as `df -Pk` reports it.
fn used_mb(path: &Path) -> Result<u64, String> {
    let output = std::process::Command::new("df")
        .arg("-Pk")
        .arg("--")
        .arg(path)
        .output()
        .map_err(|error| format!("df {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "df {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .nth(1)
        .ok_or_else(|| format!("df {}: no filesystem in its output", path.display()))?;
    let kb: u64 = line
        .split_whitespace()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("df {}: cannot read {line:?}", path.display()))?;
    Ok(kb / 1024)
}

/// The three largest processes by RSS: `pid`, `rss_mb`, and the command name
/// only (never its arguments).
fn top_processes() -> Value {
    let mut processes: Vec<(u64, u64, String)> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Value::Array(Vec::new());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<u64>().ok()) else {
            continue;
        };
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let mut rss_kb = 0;
        let mut command = String::new();
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                rss_kb = rest
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("Name:") {
                command = rest.trim().to_string();
            }
        }
        processes.push((rss_kb, pid, command));
    }
    processes.sort_unstable_by(|left, right| right.cmp(left));
    Value::Array(
        processes
            .into_iter()
            .take(3)
            .map(|(rss_kb, pid, name)| json!({"pid": pid, "rss_mb": rss_kb / 1024, "name": name}))
            .collect(),
    )
}

/// The bytes appended to `log`'s file since its last reading, as text, and
/// whether more than one reading takes was left. On start, the last
/// [`LOG_TAIL_BYTES`] only.
fn read_appended(path: &Path, log: &mut LogWatch) -> Result<Option<(String, bool)>, String> {
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    if !log.seated {
        log.offset = length.saturating_sub(LOG_TAIL_BYTES);
        log.seated = true;
    }
    if length < log.offset {
        // Rotated or truncated: read the new file from its start.
        log.offset = 0;
        log.carry.clear();
    }
    if length == log.offset {
        return Ok(None);
    }
    let start = length.saturating_sub(LOG_READ_LIMIT).max(log.offset);
    if start > log.offset {
        // Too far behind: skip ahead, and say nothing about what was skipped.
        log.offset = start;
        log.carry.clear();
    }
    file.seek(SeekFrom::Start(log.offset))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(LOG_READ_LIMIT)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    log.offset = log.offset.saturating_add(bytes.len() as u64);
    let truncated = log.offset < length;
    log.carry.push_str(&String::from_utf8_lossy(&bytes));
    // A line the reading cut waits for its end in the next one.
    let (complete, carry) = match log.carry.rfind('\n') {
        Some(at) => (
            log.carry[..=at].to_string(),
            log.carry[at + 1..].to_string(),
        ),
        None => (String::new(), std::mem::take(&mut log.carry)),
    };
    log.carry = carry;
    Ok(Some((complete, truncated)))
}

/// The stamp and `[pid]` of a crash line stamped within the day, as written,
/// with the stamp's spaces folded out so the key is one word:
/// `2026-11-08T18:25:00.123UTC[1]`. `None` for any other line.
fn crash_stamp(line: &str, now: i64) -> Option<(i64, String)> {
    if !CRASH_PATTERNS.iter().any(|pattern| line.contains(pattern)) {
        return None;
    }
    let bytes = line.as_bytes();
    if bytes.len() < 26 {
        return None;
    }
    let date = line.get(0..10)?;
    let time = line.get(11..19)?;
    if date.as_bytes()[4] != b'-'
        || date.as_bytes()[7] != b'-'
        || time.as_bytes()[2] != b':'
        || time.as_bytes()[5] != b':'
        || !date
            .chars()
            .chain(time.chars())
            .all(|c| c.is_ascii_digit() || c == '-' || c == ':')
    {
        return None;
    }
    let year: i64 = date.get(0..4)?.parse().ok()?;
    let month: u32 = date.get(5..7)?.parse().ok()?;
    let day: u32 = date.get(8..10)?.parse().ok()?;
    let hour: i64 = time.get(0..2)?.parse().ok()?;
    let minute: i64 = time.get(3..5)?.parse().ok()?;
    let second: i64 = time.get(6..8)?.parse().ok()?;
    let mut rest = line.get(19..)?;
    let mut millis: i64 = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits: String = fraction.chars().take_while(char::is_ascii_digit).collect();
        if digits.len() != 3 {
            return None;
        }
        millis = digits.parse().ok()?;
        rest = fraction.get(3..)?;
    }
    let rest = rest.strip_prefix(' ')?;
    let (offset_min, rest) = zone(rest)?;
    let rest = rest.strip_prefix(' ')?;
    if !rest.starts_with('[') {
        return None;
    }
    let end = rest.find(']')?;
    if !rest.get(1..end)?.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let written = line.get(..line.len() - rest.len() + end + 1)?;
    // The key is one word: the stamp's spaces are folded out, its text (the
    // date, the time, the zone, the pid) as the line wrote it.
    let prefix = format!(
        "{}T{}",
        written.get(..10)?,
        written.get(11..)?.replace(' ', "")
    );
    let stamp = days_from_civil(year, month, day)
        .checked_mul(86_400)?
        .checked_add(hour * 3600 + minute * 60 + second)?;
    let stamp = stamp * 1000 + millis - i64::from(offset_min) * 60_000;
    (stamp >= now - CRASH_MAX_AGE_MS).then_some((stamp, prefix))
}

/// A postgres log's time zone (`UTC`, `+08`, `-05:30`, `+0800`), as minutes
/// east of UTC, and what follows.
fn zone(text: &str) -> Option<(i32, &str)> {
    if let Some(rest) = text.strip_prefix("UTC") {
        return Some((0, rest));
    }
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = text
        .get(1..)?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let (hours, minutes, rest) = match digits.len() {
        2 => (digits.parse::<i32>().ok()?, 0, text.get(3..)?),
        4 => (
            digits.get(..2)?.parse::<i32>().ok()?,
            digits.get(2..4)?.parse::<i32>().ok()?,
            text.get(5..)?,
        ),
        _ => return None,
    };
    let (minutes, rest) = if let Some(rest) = rest.strip_prefix(':') {
        let more: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if more.len() != 2 {
            return None;
        }
        (more.parse::<i32>().ok()?, rest.get(2..)?)
    } else {
        (minutes, rest)
    };
    (hours <= 23 && minutes < 60).then_some((sign * (hours * 60 + minutes), rest))
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400).cast_unsigned();
    let mp = u64::from(if month > 2 { month - 3 } else { month + 9 });
    let doy = (153 * mp + 2) / 5 + u64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe.cast_signed() - 719_468
}

/// The civil date and time of an epoch in ms, in UTC.
fn civil(ms: i64) -> (i64, u32, u32, u32, u32, u32) {
    let seconds = ms.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097).cast_unsigned();
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe.cast_signed() + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
        u32::try_from(rest / 3600).unwrap_or(0),
        u32::try_from(rest % 3600 / 60).unwrap_or(0),
        u32::try_from(rest % 60).unwrap_or(0),
    )
}

/// An epoch in ms as `2026-11-08 18:25:00` (UTC).
pub(crate) fn time_text(ms: i64) -> String {
    let (year, month, day, hour, minute, second) = civil(ms);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

/// A password in a URL is a secret: evidence never holds one. `scheme://
/// user:password@host` becomes `scheme://user:***@host`.
pub(crate) fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("://") {
        let before = &rest[..index];
        let start = before
            .rfind(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .map_or(0, |at| at + 1);
        let after = &rest[index..];
        let end = after
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(after.len());
        out.push_str(&rest[..start]);
        out.push_str(&redact(&rest[start..index + end]));
        rest = &rest[index + end..];
    }
    out.push_str(rest);
    out
}

fn redact(token: &str) -> String {
    let Some((scheme, rest)) = token.split_once("://") else {
        return token.to_string();
    };
    let Some(at) = rest.rfind('@') else {
        return token.to_string();
    };
    let Some(colon) = rest[..at].find(':') else {
        return token.to_string();
    };
    format!("{scheme}://{}:***{}", &rest[..colon], &rest[at..])
}

/// [`scrub`] applied to every string an evidence object holds.
fn scrub_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(scrub(text)),
        Value::Array(items) => Value::Array(items.iter().map(scrub_value).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), scrub_value(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A span (`90s`, `30m`, `2h`, `1d`, or a number of ms) in ms.
fn parse_span(text: &str) -> Option<i64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: i64 = number.parse().ok()?;
    let factor = match unit {
        "" | "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    Some(value.saturating_mul(factor))
}

/// The window `spawn alerts --once` and `/api/alerts` show, in ms: `since`
/// is a span (`90s`, `30m`, `2h`, `1d`, or a number of ms), else the
/// configured window.
pub(crate) fn window_ms(since: Option<&str>) -> Result<i64, String> {
    match since {
        None => Ok(Settings::from_env().0.window_ms),
        Some(text) if text.trim().is_empty() => Ok(Settings::from_env().0.window_ms),
        Some(text) => parse_span(text)
            .ok_or_else(|| format!("--since {text:?} is not a span (90s, 30m, 2h, 1d) or ms")),
    }
}

/// The alert window as `spawn alerts --once --json` and `/api/alerts`
/// answer it: the one array both show, with the notice fold (objectives 151
/// and 166, [`notices::fold`]): each task's or mission's notice that its
/// subject's next one follows says `superseded_by`, that one's key. Every
/// row stays; nothing is recorded.
pub(crate) fn window(store: &PostgresStore, since: Option<&str>) -> Result<Value, String> {
    let span = window_ms(since)?;
    let now = store.now_ms().map_err(|error| error.to_string())?;
    let mut rows = store
        .alerts(now.saturating_sub(span))
        .map_err(|error| error.to_string())?;
    notices::fold(&mut rows);
    Ok(rows)
}

/// `spawn alerts --once` in text (objective 166), in the window's order:
/// each open and cleared alert's [`line`], and of the notices each
/// subject's newest only ([`notices::current`]), with " · N earlier" after
/// its line when notices came before it, as the Alerts view shows them.
fn lines(alerts: &[Value]) -> Vec<String> {
    let current = notices::current(alerts);
    alerts
        .iter()
        .filter_map(|alert| {
            if alert["state"].as_str() != Some("notice") {
                return Some(line(alert));
            }
            let (_, earlier) = current.iter().find(|(row, _)| std::ptr::eq(*row, alert))?;
            Some(if *earlier > 0 {
                format!("{} · {earlier} earlier", line(alert))
            } else {
                line(alert)
            })
        })
        .collect()
}

const USAGE: &str = "usage: spawn alerts --once [--json] [--since <90s|30m|2h|1d|ms>]\n       spawn alerts --follow [--json] [--since <event id>]";

/// `spawn alerts`: the alerts now, or each transition as it is recorded.
pub(crate) fn command(arguments: &[String]) -> Result<i32, String> {
    let mut once = false;
    let mut follow = false;
    let mut json = false;
    let mut since: Option<String> = None;
    let mut words = arguments.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--once" => once = true,
            "--follow" => follow = true,
            "--json" => json = true,
            "--since" => {
                since = Some(
                    words
                        .next()
                        .ok_or_else(|| format!("--since needs a value\n{USAGE}"))?
                        .clone(),
                );
            }
            other => return Err(format!("unexpected argument {other}\n{USAGE}")),
        }
    }
    if once == follow {
        return Err(USAGE.to_string());
    }
    let store = open_store()?;
    if once {
        let alerts = window(&store, since.as_deref())?;
        let alerts = alerts.as_array().cloned().unwrap_or_default();
        if json {
            println!("{}", Value::Array(alerts.clone()));
        } else {
            for line in lines(&alerts) {
                println!("{line}");
            }
        }
        return Ok(0);
    }
    let after = if let Some(text) = since.as_deref() {
        text.trim()
            .parse::<i64>()
            .map_err(|_| format!("--since {text:?} is not an event id\n{USAGE}"))?
    } else {
        let rows = store.alert_rows().map_err(|error| error.to_string())?;
        rows["cursor"].as_i64().unwrap_or_default()
    };
    follow_stream(&store, after, json)
}

/// The verdicts an owner may give an alert (objective 092; `right`, a real
/// need, objective 148).
pub(crate) const VERDICTS: [&str; 4] = ["right", "false", "stale", "duplicate"];

const JUDGE_USAGE: &str = "usage: spawn alert judge <key> right|false|stale|duplicate\n       \
                           spawn alert record < <one alert.fired or alert.cleared JSON object>";

/// What `spawn alert record` takes, as its refusals name it.
const RECORD_TAKES: &str =
    "spawn alert record takes one alert.fired or alert.cleared JSON object on stdin";

/// The record `spawn alert record` was given on stdin (`text`): one JSON
/// object whose `type` is `alert.fired` or `alert.cleared`, or why it is
/// not. Anything else is refused here, before the store (objective 172);
/// the store checks the record's shape and its rules.
pub(crate) fn record_argument(text: &str) -> Result<Value, String> {
    let record: Value = serde_json::from_str(text.trim())
        .map_err(|error| format!("not JSON ({error}): {RECORD_TAKES}"))?;
    match record.get("type").and_then(Value::as_str) {
        Some("alert.fired" | "alert.cleared") if record.is_object() => Ok(record),
        Some(other) if record.is_object() => Err(format!("{other:?} is not one: {RECORD_TAKES}")),
        _ => Err(format!("not an alert record object: {RECORD_TAKES}")),
    }
}

/// The arguments of `spawn alert`: `judge <key> <verdict>`, the key and
/// the verdict, or why they are not.
pub(crate) fn judge_arguments(arguments: &[String]) -> Result<(String, String), String> {
    match arguments {
        [verb, key, verdict] if verb == "judge" => {
            if key.trim().is_empty() {
                return Err(format!("the key is empty\n{JUDGE_USAGE}"));
            }
            if !VERDICTS.contains(&verdict.as_str()) {
                return Err(format!(
                    "{verdict:?} is not a verdict (right, false, stale or duplicate)\n{JUDGE_USAGE}"
                ));
            }
            Ok((key.clone(), verdict.clone()))
        }
        _ => Err(JUDGE_USAGE.to_string()),
    }
}

/// `spawn alert judge <key> right|false|stale|duplicate`: the owner's
/// judgement of an alert, a real need (`right`, objective 148) or one that
/// should not have been raised. It records `alert.judged` through
/// `spawn_alert_record`; an open alert is cleared by any verdict but
/// `right`. The store refuses a key no alert has, a notice and an alert
/// judged already.
pub(crate) fn alert(arguments: &[String]) -> Result<i32, String> {
    if matches!(arguments, [verb] if verb == "record") {
        return record_from_stdin();
    }
    let (key, verdict) = judge_arguments(arguments)?;
    let store = open_store()?;
    let id = store
        .alert_judge(&key, &verdict)
        .map_err(|error| format!("{key} was not judged: {error}"))?;
    println!("judged {key} {verdict} (event {id})");
    Ok(0)
}

/// `spawn alert record`: one alert.fired or alert.cleared on stdin
/// ([`record_argument`]), recorded through the store's `alert_record`, which
/// masks its free text (objective 170) and checks its shape and rules (a
/// second fired, an unknown cleared, the owner). scripts/rsi/nightly.sh
/// records its alerts so, never by psql (objective 172). Prints `{event}`.
fn record_from_stdin() -> Result<i32, String> {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
        .map_err(|error| format!("read the alert record on stdin: {error}"))?;
    let record = record_argument(&text)?;
    let store = open_store()?;
    let event = store
        .alert_record(&record)
        .map_err(|error| format!("the alert was not recorded: {error}"))?;
    println!("{}", serde_json::json!({"event": event}));
    Ok(0)
}

/// One alert as a line for a person: its state, kind, subject, time and
/// text; an open `agent_stuck` also says when the session was last scored
/// (`scored_at_ms`, read with the window): a judge that has been down
/// since leaves the alert on an old score.
fn line(alert: &Value) -> String {
    let state = alert["state"].as_str().unwrap_or("open");
    let at = alert
        .get("cleared_at_ms")
        .and_then(Value::as_i64)
        .or_else(|| alert["fired_at_ms"].as_i64())
        .unwrap_or_default();
    let scored = alert["scored_at_ms"]
        .as_i64()
        .map(|scored| format!(" (last scored {})", time_text(scored)))
        .unwrap_or_default();
    format!(
        "{state} {} {} {} {}{scored}",
        alert["kind"].as_str().unwrap_or("alert"),
        alert["subject"].as_str().unwrap_or(""),
        time_text(at),
        alert["text"].as_str().unwrap_or("")
    )
}

/// Print each transition after `after` as it is recorded, until stopped.
fn follow_stream(store: &PostgresStore, after: i64, json: bool) -> Result<i32, String> {
    let mut cursor = after;
    let mut out = std::io::stdout();
    loop {
        let transitions = match store.alert_transitions(cursor, BATCH) {
            Ok(transitions) => transitions,
            Err(error) => {
                // A read that failed is tried again: a stream that ends on
                // one glitch tells nobody what happened after it.
                eprintln!("spawn alerts: {error}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        for transition in transitions.as_array().into_iter().flatten() {
            let event_id = transition["event_id"].as_i64().unwrap_or_default();
            cursor = cursor.max(event_id);
            if json {
                let _ = writeln!(out, "{transition}");
            } else {
                let _ = writeln!(
                    out,
                    "{} {} {} {} {}",
                    transition["transition"].as_str().unwrap_or("fired"),
                    transition["kind"].as_str().unwrap_or("alert"),
                    transition["subject"].as_str().unwrap_or(""),
                    time_text(transition["at_ms"].as_i64().unwrap_or_default()),
                    transition["key"].as_str().unwrap_or("")
                );
            }
            let _ = out.flush();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The (mission, source) pairs of the store's `alert_mission_sources` whose
/// source is a path on this host: absolute (a URL or `host:path` never is,
/// and is another host's to check).
fn host_sources(listed: &Value) -> BTreeSet<(i64, String)> {
    let mut out = BTreeSet::new();
    for entry in listed.as_array().into_iter().flatten() {
        let Some(mission) = entry["mission"].as_i64() else {
            continue;
        };
        for source in entry["sources"].as_array().into_iter().flatten() {
            if let Some(source) = source.as_str()
                && Path::new(source).is_absolute()
            {
                out.insert((mission, source.to_string()));
            }
        }
    }
    out
}

mod guardrails;
mod notices;

#[cfg(test)]
mod tests {
    use super::{
        Crossing, Event, LIKELY, Monitor, Row, Settings, civil, crash_stamp, crossing,
        days_from_civil, host_sources, line, parse_span, scrub, scrub_value, time_text, window_ms,
    };
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    fn settings() -> Settings {
        Settings {
            check_ms: 30_000,
            undelivered_ms: 300_000,
            admission_ms: 600_000,
            judge_down_ms: 900_000,
            message_taken_ms: 120_000,
            dump_max_age_ms: 7_200_000,
            bundle_max_age_ms: 93_600_000,
            memory_low_mb: 4096,
            memory_ok_mb: 8192,
            tmpfs_path: PathBuf::from("/tmp"),
            tmpfs_high_mb: 8000,
            tmpfs_ok_mb: 6000,
            disk_root: None,
            disk_low_gb: 0,
            disk_ok_gb: 45,
            quiet_ms: 900_000,
            integrator_down_ms: 300_000,
            controller_behind_ms: 1_800_000,
            mission_idle_ms: 900_000,
            owner_request_ms: 3_600_000,
            unverified_ms: 86_400_000,
            window_ms: 86_400_000,
            test_log: None,
            live_log: None,
            database_ms: 600_000,
            database_floor_mb: 1024,
            database_ratio: 10,
            test_databases: Vec::new(),
            guardrail_ms: 300_000,
            guardrail_raise_ms: 7_200_000,
            guardrail_clear_ms: 3_600_000,
        }
    }

    fn monitor() -> Monitor {
        Monitor::with(settings())
    }

    /// What `flush` would record: the events whose key is not already there
    /// (and the clearings of open alerts only).
    fn decided(monitor: &Monitor, events: Vec<Event>) -> Vec<Event> {
        events
            .into_iter()
            .filter(|event| match event {
                Event::Fire { .. } => !monitor.known.contains_key(event.key()),
                Event::Clear { key, evidence } => monitor.clears(key, evidence, i64::MAX),
            })
            .collect()
    }

    /// The store's side of a flush: the keys it now holds.
    fn commit(monitor: &mut Monitor, events: &[Event]) {
        for event in events {
            match event {
                Event::Fire {
                    key,
                    kind,
                    subject,
                    evidence,
                    ..
                } => {
                    monitor.known.insert(
                        key.clone(),
                        Row {
                            kind: (*kind).to_string(),
                            subject: subject.clone(),
                            state: "open".to_string(),
                            evidence: evidence.clone(),
                            fired_event_id: 1,
                            fired_at_ms: 0,
                        },
                    );
                }
                Event::Clear { key, .. } => {
                    if let Some(row) = monitor.known.get_mut(key) {
                        row.state = "cleared".to_string();
                    }
                }
            }
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn feed(monitor: &mut Monitor, event_id: i64, at_ms: i64, record: Value) -> Vec<Event> {
        let mut out = Vec::new();
        monitor.feed(event_id, at_ms, &record, &mut out);
        out
    }

    fn fires(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Fire { key, .. } => Some(key.as_str()),
                Event::Clear { .. } => None,
            })
            .collect()
    }

    /// Objective 092: `spawn alert judge <key> right|false|stale|duplicate`
    /// (`right`, objective 148), its usage and what it refuses before
    /// reaching the store.
    #[test]
    fn the_judge_verb_takes_a_key_and_one_of_three_verdicts() {
        let words =
            |list: &[&str]| -> Vec<String> { list.iter().map(ToString::to_string).collect() };
        assert_eq!(super::VERDICTS, ["right", "false", "stale", "duplicate"]);
        for verdict in super::VERDICTS {
            assert_eq!(
                super::judge_arguments(&words(&["judge", "bb:a1", verdict])),
                Ok(("bb:a1".to_string(), verdict.to_string()))
            );
        }
        let usage = "usage: spawn alert judge <key> right|false|stale|duplicate\n       \
                     spawn alert record < <one alert.fired or alert.cleared JSON object>";
        for wrong in [
            &[][..],
            &["judge"][..],
            &["judge", "bb:a1"][..],
            &["judge", "bb:a1", "false", "more"][..],
            &["clear", "bb:a1", "false"][..],
        ] {
            assert_eq!(
                super::judge_arguments(&words(wrong)),
                Err(usage.to_string()),
                "{wrong:?}"
            );
        }
        let refused = super::judge_arguments(&words(&["judge", "bb:a1", "wrong"]))
            .expect_err("another verdict");
        assert!(
            refused.contains("not a verdict") && refused.ends_with(usage),
            "{refused}"
        );
        assert!(super::judge_arguments(&words(&["judge", " ", "false"])).is_err());
    }

    /// Objective 172: `spawn alert record` takes one alert.fired or
    /// alert.cleared object; anything else is refused before the store,
    /// naming what it takes.
    #[test]
    fn the_record_verb_takes_one_alert_record() {
        for taken in [
            r#"{"type": "alert.fired", "key": "k", "kind": "nightly_red", "subject": "s"}"#,
            "{\"type\": \"alert.cleared\", \"key\": \"k\"}\n",
        ] {
            let record = super::record_argument(taken).expect("taken");
            assert!(
                record["type"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("alert."))
            );
        }
        for refused in [
            "",
            "not json",
            r#"[{"type": "alert.fired"}]"#,
            r#"{"type": "alert.judged", "key": "k"}"#,
            r#"{"type": "task.gated", "id": 1}"#,
            r#""alert.fired""#,
        ] {
            let error = super::record_argument(refused).expect_err(refused);
            assert!(
                error.contains("alert.fired or alert.cleared"),
                "{refused}: {error}"
            );
        }
    }

    /// Objective 148: a need judged right stays open in the step's memory,
    /// so the step still clears it when its condition does; any other
    /// verdict clears it there.
    #[test]
    fn a_need_judged_right_stays_open_for_the_step() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            10,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let defect = json!({"type": "task.gated", "id": 7, "verdict": "host_fault",
                            "revision": 0, "base": "b", "evidence": "validator_defect",
                            "validator_defects": ["028.sh"],
                            "fact": {"scenario": "028.sh", "on": "candidate and base"}});
        let first = feed(&mut monitor, 11, 0, defect);
        commit(&mut monitor, &first);
        let key = "validator_defect:4:028.sh:11";
        assert!(monitor.is_open(key), "{first:?}");
        // Judged right: still open, so its condition clears it.
        let judged = feed(
            &mut monitor,
            12,
            0,
            json!({"type": "alert.judged", "key": key, "verdict": "right"}),
        );
        assert!(judged.is_empty(), "{judged:?}");
        assert!(monitor.is_open(key), "a need judged right stays open");
        let accepted = feed(
            &mut monitor,
            13,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "accepted", "revision": 0,
                   "base": "c"}),
        );
        let cleared = decided(&monitor, accepted);
        assert!(
            cleared
                .iter()
                .any(|event| matches!(event, Event::Clear { key: k, .. } if k == key)),
            "its condition clears it: {cleared:?}"
        );
        // Judged false, an open need is cleared in the step's memory.
        let mut other = Monitor::with(settings());
        feed(
            &mut other,
            10,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let first = feed(
            &mut other,
            11,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "host_fault",
                   "revision": 0, "base": "b", "evidence": "validator_defect",
                   "validator_defects": ["028.sh"],
                   "fact": {"scenario": "028.sh", "on": "candidate and base"}}),
        );
        commit(&mut other, &first);
        feed(
            &mut other,
            12,
            0,
            json!({"type": "alert.judged", "key": key, "verdict": "false"}),
        );
        assert!(!other.is_open(key), "judged false, it is cleared");
    }

    #[test]
    fn a_task_records_each_state_change_once_with_from_and_to() {
        let mut monitor = monitor();
        let assigned = feed(
            &mut monitor,
            10,
            1000,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4,
                   "agent_id": "agent_x", "objective": 26, "revision": 0}),
        );
        assert_eq!(fires(&assigned), ["task_state:7:10"]);
        let Event::Fire {
            kind,
            subject,
            evidence,
            notice,
            ..
        } = &assigned[0]
        else {
            panic!("a fire");
        };
        assert_eq!(*kind, "task_state");
        assert_eq!(subject, "task #7");
        assert!(notice);
        assert_eq!(evidence["from"], json!(null));
        assert_eq!(evidence["to"], "assigned");
        assert_eq!(evidence["record"], "task.assigned");
        assert_eq!(evidence["event_id"], 10);
        assert_eq!(evidence["mission"], 4);
        commit(&mut monitor, &assigned);

        let delivered = feed(
            &mut monitor,
            11,
            2000,
            json!({"type": "task.delivered", "id": 7, "command_id": 3}),
        );
        assert_eq!(fires(&delivered), ["task_state:7:11"]);
        let Event::Fire { evidence, .. } = &delivered[0] else {
            panic!("a fire");
        };
        assert_eq!(evidence["from"], "assigned");
        assert_eq!(evidence["to"], "delivered");
        commit(&mut monitor, &delivered);

        let again = feed(
            &mut monitor,
            11,
            2000,
            json!({"type": "task.delivered", "id": 7, "command_id": 3}),
        );
        assert!(
            decided(&monitor, again).is_empty(),
            "a replay records nothing"
        );
    }

    #[test]
    fn a_gating_that_leaves_the_task_is_no_notice_and_moves_the_missions_counters() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            1,
            0,
            json!({"type": "mission.added", "id": 4, "text": "m"}),
        );
        feed(
            &mut monitor,
            2,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4,
                   "agent_id": "agent_x", "revision": 0}),
        );
        for (event, aborts) in [(3, 1), (4, 2), (5, 3)] {
            let events = feed(
                &mut monitor,
                event,
                0,
                json!({"type": "task.gated", "id": 7, "verdict": "aborted", "revision": 0}),
            );
            assert!(
                !fires(&events)
                    .iter()
                    .any(|key| key.starts_with("task_state:")),
                "an aborted gating leaves the task as it was"
            );
            let notice = events
                .iter()
                .find(|event| {
                    matches!(
                        event,
                        Event::Fire {
                            kind: "mission_state",
                            ..
                        }
                    )
                })
                .expect("the mission's abort count moved");
            let Event::Fire { evidence, .. } = notice else {
                panic!("a fire");
            };
            assert_eq!(evidence["record"], "task.gated");
            assert_eq!(evidence["after"]["gate_aborts"], aborts);
            assert_eq!(evidence["before"]["gate_aborts"], aborts - 1);
        }
    }

    /// The `gate_rejections` change a record's notices say: "<before> ->
    /// <after> (by <record>)", or `None` when none says it.
    fn rejections_change(events: &[Event]) -> Option<String> {
        events.iter().find_map(|event| match event {
            Event::Fire {
                kind: "mission_state",
                evidence,
                ..
            } if evidence["before"]["gate_rejections"] != evidence["after"]["gate_rejections"] => {
                Some(format!(
                    "{} -> {} (by {})",
                    evidence["before"]["gate_rejections"],
                    evidence["after"]["gate_rejections"],
                    evidence["record"].as_str().unwrap_or_default()
                ))
            }
            _ => None,
        })
    }

    /// Objective 053: a cancel of a task of the objective every rejection
    /// of the run was ends the run, with its notice; a cancel outside the
    /// run (another objective, a run of two, a task with none) changes
    /// nothing, as the store's fold.
    #[test]
    fn a_cancel_that_ends_the_run_of_rejections_is_a_notice_as_the_fold() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            1,
            0,
            json!({"type": "mission.added", "id": 4, "text": "m"}),
        );
        for (task, objective) in [
            (1, json!(1)),
            (2, json!(2)),
            (3, json!(3)),
            (4, Value::Null),
        ] {
            feed(
                &mut monitor,
                1 + task,
                0,
                json!({"type": "task.assigned", "id": task, "mission_id": 4,
                       "agent_id": format!("agent_{task}"), "objective": objective}),
            );
        }
        let mut event = 10;
        let mut gate = |monitor: &mut Monitor, task: i64, verdict: &str| {
            event += 1;
            feed(
                monitor,
                event,
                0,
                json!({"type": "task.gated", "id": task, "verdict": verdict}),
            )
        };
        let cancel = |monitor: &mut Monitor, event: i64, task: i64| {
            feed(
                monitor,
                event,
                0,
                json!({"type": "task.cancelled", "id": task}),
            )
        };
        for want in ["0 -> 1", "1 -> 2", "2 -> 3"] {
            let events = gate(&mut monitor, 1, "rejected");
            assert_eq!(
                rejections_change(&events),
                Some(format!("{want} (by task.gated)"))
            );
        }
        // A cancel of another objective's task: nothing.
        assert_eq!(rejections_change(&cancel(&mut monitor, 20, 2)), None);
        // The yield of objective 1: the run ends.
        let yielded = cancel(&mut monitor, 21, 1);
        assert_eq!(
            rejections_change(&yielded),
            Some("3 -> 0 (by task.cancelled)".into())
        );
        let Some(Event::Fire { text, .. }) = yielded.first() else {
            panic!("a notice: {yielded:?}");
        };
        assert_eq!(
            text,
            "mission #4: gate_rejections 3 -> 0 (by task.cancelled)"
        );
        // Cancelled again: the run is gone, nothing more.
        assert_eq!(rejections_change(&cancel(&mut monitor, 22, 1)), None);

        // A run of two objectives: a cancel of either ends nothing.
        gate(&mut monitor, 2, "rejected");
        gate(&mut monitor, 3, "rejected");
        assert_eq!(rejections_change(&cancel(&mut monitor, 23, 3)), None);
        assert_eq!(rejections_change(&cancel(&mut monitor, 24, 2)), None);
        let events = gate(&mut monitor, 3, "rejected");
        assert_eq!(
            rejections_change(&events),
            Some("2 -> 3 (by task.gated)".into())
        );
        // An accept ends it; a run of a task with no objective is no cancel's.
        gate(&mut monitor, 3, "accepted");
        gate(&mut monitor, 4, "rejected");
        assert_eq!(rejections_change(&cancel(&mut monitor, 25, 4)), None);
        // A resume ends the run and its objective: a cancel after says nothing.
        gate(&mut monitor, 4, "accepted");
        gate(&mut monitor, 3, "rejected");
        feed(
            &mut monitor,
            26,
            0,
            json!({"type": "mission.resumed", "id": 4}),
        );
        assert_eq!(rejections_change(&cancel(&mut monitor, 27, 3)), None);
    }

    /// Missions for `mission_idle` (objective 058): 1 and 3 take their work
    /// from a backlog, 2 is owner work, 4 a backlog mission whose task stays
    /// in work; each has one task (its id the mission's), assigned at event
    /// 10 + id. Returns the events of the cancels of tasks 1, 2 and 3.
    fn idle_world(monitor: &mut Monitor) -> Vec<i64> {
        for id in 1..=4 {
            feed(
                monitor,
                id,
                0,
                json!({"type": "mission.added", "id": id, "text": "m"}),
            );
            let objectives = if id == 2 {
                Value::Null
            } else {
                json!({"backlog": "b", "resident": "r"})
            };
            feed(
                monitor,
                5 + id,
                0,
                json!({"type": "mission.configured", "id": id, "accept": "gate",
                       "objectives": objectives}),
            );
            feed(
                monitor,
                10 + id,
                0,
                json!({"type": "task.assigned", "id": id, "mission_id": id, "agent_id": "a"}),
            );
        }
        feed(
            monitor,
            20,
            0,
            json!({"type": "mission.closed", "id": 3, "state": "done"}),
        );
        [1, 2, 3]
            .into_iter()
            .map(|id| {
                feed(
                    monitor,
                    20 + id,
                    1_000 * id,
                    json!({"type": "task.cancelled", "id": id}),
                );
                20 + id
            })
            .collect()
    }

    fn read_idle(monitor: &Monitor, now: i64) -> Vec<Event> {
        let mut out = Vec::new();
        monitor.read_idle(now, &mut out);
        out
    }

    /// Objective 058 (a), (c): a backlog mission with no task in work fires
    /// once, at the reading the bound has passed since its last task ended;
    /// owner work, a closed mission and one with a task in work do not.
    #[test]
    fn a_backlog_mission_without_work_past_the_bound_is_one_alert() {
        let mut monitor = monitor();
        let cancels = idle_world(&mut monitor);
        assert!(
            read_idle(&monitor, 1_000 + 900_000 - 1).is_empty(),
            "not before the bound"
        );
        let fired = read_idle(&monitor, 1_000 + 900_000);
        assert_eq!(fires(&fired), ["mission_idle:1:21"], "{fired:?}");
        let Event::Fire {
            subject,
            evidence,
            text,
            notice,
            ..
        } = &fired[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "mission #1");
        assert_eq!(evidence["mission"], 1);
        assert_eq!(evidence["since_ms"], 1_000);
        assert_eq!(evidence["event_id"], cancels[0]);
        assert_eq!(
            text,
            "mission #1 has had no task in work for 15 min since its last task ended: its objective backlog has nothing open to assign"
        );
        assert!(!notice, "an alert: Needs you");
        commit(&mut monitor, &fired);
        assert!(
            read_idle(&monitor, 10_000_000).is_empty(),
            "once while open"
        );
    }

    /// Objective 058 (b): the mission's next assignment clears it, naming
    /// that record, and so does its close; the bound starts again at the
    /// next task's end.
    #[test]
    fn a_missions_next_task_or_its_close_clears_its_idle_alert() {
        let mut monitor = monitor();
        idle_world(&mut monitor);
        let fired = read_idle(&monitor, 2_000_000);
        commit(&mut monitor, &fired);
        let assigned = feed(
            &mut monitor,
            30,
            0,
            json!({"type": "task.assigned", "id": 5, "mission_id": 1, "agent_id": "a"}),
        );
        let cleared = decided(&monitor, assigned);
        assert!(
            matches!(&cleared[..], [.., Event::Clear { key, evidence }]
                if key == "mission_idle:1:21" && evidence["event_id"] == 30),
            "{cleared:?}"
        );
        commit(&mut monitor, &cleared);
        assert!(read_idle(&monitor, 9_000_000).is_empty(), "a task in work");
        feed(
            &mut monitor,
            31,
            3_000_000,
            json!({"type": "task.cancelled", "id": 5}),
        );
        let again = read_idle(&monitor, 3_000_000 + 900_000);
        assert_eq!(fires(&again), ["mission_idle:1:31"]);
        commit(&mut monitor, &again);
        let closed = feed(
            &mut monitor,
            32,
            0,
            json!({"type": "mission.closed", "id": 1, "state": "done"}),
        );
        let cleared = decided(&monitor, closed);
        assert!(
            cleared
                .iter()
                .any(|event| matches!(event, Event::Clear { key, evidence }
                if key == "mission_idle:1:31" && evidence["event_id"] == 32)),
            "{cleared:?}"
        );
    }

    /// Objective 099: a mission the owner paused fires no `mission_idle`,
    /// however long it has had no task in work; a pause clears an open one,
    /// naming the `mission.paused` record; the owner's resume starts the
    /// idle time again from its own record.
    #[test]
    fn a_paused_mission_fires_no_idle_alert_and_its_pause_clears_one() {
        let mut monitor = monitor();
        idle_world(&mut monitor);
        // Mission 4 has a task in work; cancel it, then pause it before
        // the bound: it never fires, past the bound or long after.
        feed(
            &mut monitor,
            24,
            4_000,
            json!({"type": "task.cancelled", "id": 4}),
        );
        let paused = feed(
            &mut monitor,
            25,
            5_000,
            json!({"type": "mission.paused", "id": 4, "reason": "owner hold"}),
        );
        assert!(
            decided(&monitor, paused).is_empty(),
            "nothing open to clear"
        );
        let fired = read_idle(&monitor, 50_000_000);
        assert_eq!(
            fires(&fired),
            ["mission_idle:1:21"],
            "mission 1 is idle, paused mission 4 is not: {fired:?}"
        );
        commit(&mut monitor, &fired);
        // Mission 1's open alert clears at its pause, naming the record.
        let pause = feed(
            &mut monitor,
            40,
            60_000_000,
            json!({"type": "mission.paused", "id": 1, "reason": null}),
        );
        let cleared = decided(&monitor, pause);
        assert!(
            matches!(&cleared[..], [Event::Clear { key, evidence }]
                if key == "mission_idle:1:21" && evidence["event_id"] == 40
                    && evidence["record"] == "mission.paused"),
            "{cleared:?}"
        );
        commit(&mut monitor, &cleared);
        assert!(read_idle(&monitor, 90_000_000).is_empty(), "both paused");
        // The resume of mission 4: idle again from the unpause, not from
        // its last task's end.
        feed(
            &mut monitor,
            41,
            100_000_000,
            json!({"type": "mission.unpaused", "id": 4, "pause_event": 25}),
        );
        assert!(read_idle(&monitor, 100_000_000 + 900_000 - 1).is_empty());
        let again = read_idle(&monitor, 100_000_000 + 900_000);
        assert_eq!(fires(&again), ["mission_idle:4:41"], "{again:?}");
    }

    /// Objective 058: `SPAWN_ALERT_MISSION_IDLE_SECS` is 900 s by default,
    /// and 0 turns the alert off.
    #[test]
    fn the_idle_bound_defaults_to_fifteen_minutes_and_zero_disables_it() {
        if std::env::var_os("SPAWN_ALERT_MISSION_IDLE_SECS").is_none() {
            assert_eq!(super::Settings::from_env().0.mission_idle_ms, 900_000);
        }
        assert_eq!(settings().mission_idle_ms, 900_000);
        let mut off = Monitor::with(Settings {
            mission_idle_ms: 0,
            ..settings()
        });
        idle_world(&mut off);
        assert!(read_idle(&off, i64::MAX / 2).is_empty(), "disabled");
    }

    /// Objective 058 after a restart: an open alert read back fires nothing
    /// again, and the next assignment still clears it.
    #[test]
    fn a_restart_keeps_a_mission_idle_alert_one_alert() {
        let mut restarted = monitor();
        idle_world(&mut restarted);
        restarted.known.insert(
            "mission_idle:1:21".to_string(),
            Row {
                kind: "mission_idle".to_string(),
                subject: "mission #1".to_string(),
                state: "open".to_string(),
                evidence: json!({"mission": 1, "since_ms": 1_000, "event_id": 21}),
                fired_event_id: 21,
                fired_at_ms: 0,
            },
        );
        assert!(read_idle(&restarted, 5_000_000).is_empty());
        let assigned = feed(
            &mut restarted,
            40,
            0,
            json!({"type": "task.assigned", "id": 6, "mission_id": 1, "agent_id": "a"}),
        );
        let cleared = decided(&restarted, assigned);
        assert!(
            cleared.iter().any(
                |event| matches!(event, Event::Clear { key, .. } if key == "mission_idle:1:21")
            ),
            "{cleared:?}"
        );
    }

    #[test]
    fn a_halt_is_a_notice_and_an_alert_and_a_resume_clears_it() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            1,
            0,
            json!({"type": "mission.added", "id": 4, "text": "m"}),
        );
        let halted = feed(
            &mut monitor,
            2,
            0,
            json!({"type": "mission.halted", "id": 4, "reason": "three rejections"}),
        );
        assert_eq!(fires(&halted), ["mission_state:4:2", "mission_halted:4:2"]);
        let Event::Fire {
            evidence, notice, ..
        } = &halted[1]
        else {
            panic!("a fire");
        };
        assert_eq!(evidence["reason"], "three rejections");
        assert!(!notice);
        commit(&mut monitor, &halted);

        let resumed = feed(
            &mut monitor,
            3,
            0,
            json!({"type": "mission.resumed", "id": 4}),
        );
        let cleared = decided(&monitor, resumed);
        assert_eq!(cleared.len(), 2, "{cleared:?}");
        assert!(matches!(cleared[1], Event::Clear { ref key, .. } if key == "mission_halted:4:2"));
        let Event::Clear { evidence, .. } = &cleared[1] else {
            panic!("a clear");
        };
        assert_eq!(evidence["event_id"], 3);
    }

    /// What a restart reads again: an open halt of record 20, then the
    /// records of the history in order. A resume older than the halt, a
    /// good drill older than the failed one, an `up` older than its `down`
    /// clear nothing; a later one clears.
    #[test]
    fn a_record_older_than_an_alerts_own_clears_nothing() {
        let mut monitor = monitor();
        let open = |kind: &str, subject: &str, evidence: Value| super::Row {
            kind: kind.to_string(),
            subject: subject.to_string(),
            state: "open".to_string(),
            evidence,
            fired_event_id: 100,
            fired_at_ms: 0,
        };
        monitor.known.insert(
            "mission_halted:4:20".into(),
            open("mission_halted", "mission #4", json!({"event_id": 20})),
        );
        monitor.known.insert(
            "drill_failed:30".into(),
            open("drill_failed", "drill", json!({"event_id": 30})),
        );
        monitor.known.insert(
            "judge_down:40".into(),
            open("judge_down", "jev", json!({"event_id": 40})),
        );
        feed(
            &mut monitor,
            1,
            0,
            json!({"type": "mission.added", "id": 4, "text": "m"}),
        );
        for (event_id, record) in [
            (15, json!({"type": "mission.resumed", "id": 4})),
            (
                25,
                json!({"type": "backup.verified", "ok": true, "key": "old"}),
            ),
            (
                30,
                json!({"type": "backup.verified", "ok": false, "key": "same"}),
            ),
            (
                35,
                json!({"type": "host.judge", "judge": "jev", "state": "up"}),
            ),
        ] {
            let events = feed(&mut monitor, event_id, 0, record);
            let cleared: Vec<Event> = decided(&monitor, events)
                .into_iter()
                .filter(|event| matches!(event, Event::Clear { .. }))
                .collect();
            assert!(cleared.is_empty(), "record {event_id} cleared {cleared:?}");
        }
        for (event_id, record, key) in [
            (
                50,
                json!({"type": "mission.resumed", "id": 4}),
                "mission_halted:4:20",
            ),
            (
                51,
                json!({"type": "backup.verified", "ok": true, "key": "new"}),
                "drill_failed:30",
            ),
            (
                52,
                json!({"type": "host.judge", "judge": "jev", "state": "up"}),
                "judge_down:40",
            ),
        ] {
            let events = feed(&mut monitor, event_id, 0, record);
            let cleared = decided(&monitor, events);
            assert!(
                cleared
                    .iter()
                    .any(|event| matches!(event, Event::Clear { key: at, evidence }
                        if at == key && evidence["event_id"] == event_id)),
                "record {event_id} clears {key}: {cleared:?}"
            );
        }
    }

    /// A reading-borne clear: a reading at or after the one the alert fired
    /// on, or, when the alert names none, after its firing.
    #[test]
    fn a_reading_clears_only_after_the_reading_it_fired_on() {
        let row = |evidence: Value, fired_at_ms: i64| super::Row {
            kind: "memory_low".to_string(),
            subject: "host".to_string(),
            state: "open".to_string(),
            evidence,
            fired_event_id: 7,
            fired_at_ms,
        };
        let fired = row(json!({"reading_ms": 1_000}), 1_200);
        assert!(!super::after(&fired, &json!({"reading_ms": 900}), 5_000));
        assert!(super::after(&fired, &json!({"reading_ms": 1_500}), 5_000));
        let unnamed = row(json!({}), 1_200);
        assert!(!super::after(&unnamed, &json!({}), 1_100));
        assert!(super::after(&unnamed, &json!({}), 1_300));
        // An event names the order whatever the clocks say.
        let record = row(json!({"event_id": 9}), 0);
        assert!(!super::after(
            &record,
            &json!({"event_id": 9, "reading_ms": 9_999}),
            0
        ));
        assert!(super::after(&record, &json!({"event_id": 10}), 0));
    }

    #[test]
    fn a_restarted_loop_reads_its_alerts_back_and_fires_nothing_twice() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            1,
            0,
            json!({"type": "mission.added", "id": 4, "text": "m"}),
        );
        let events = feed(
            &mut monitor,
            2,
            0,
            json!({"type": "mission.halted", "id": 4, "reason": "halted"}),
        );
        let recorded = decided(&monitor, events);
        commit(&mut monitor, &recorded);
        // The restart: the same records are read again with the keys the
        // store holds; the decision may repeat, but nothing is recorded.
        let replayed = feed(
            &mut monitor,
            2,
            0,
            json!({"type": "mission.halted", "id": 4, "reason": "halted"}),
        );
        assert!(decided(&monitor, replayed).is_empty());
        // And the open alert is still known, so a quiet re-reading of a
        // threshold does not fire it again.
        assert!(monitor.is_open("mission_halted:4:2"));
    }

    /// A `task.gated` of task 7 (mission 4), at the revision the integrator
    /// writes (a rejection bumps the task's), naming the scenarios main is
    /// red on.
    fn red_gated(verdict: &str, revision: i64, red: &[&str]) -> Value {
        let mut record = json!({"type": "task.gated", "id": 7, "verdict": verdict,
                                "revision": revision, "base": "b"});
        if !red.is_empty() {
            record["main_red_scenarios"] = json!(red);
        }
        record
    }

    /// `scenario_red`: once per mission and scenario while red, even when
    /// two gatings naming it are read in one batch; a rejection clears
    /// nothing (it may come before the base phase), nor does another
    /// mission's acceptance; an accepted gating that no longer names it
    /// clears it, and only it.
    #[test]
    fn main_red_on_a_scenario_is_one_alert_until_an_accepted_gate_clears_it() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            10,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let first = feed(&mut monitor, 11, 0, red_gated("accepted", 0, &["046.sh"]));
        assert!(
            fires(&first).contains(&"scenario_red:4:046.sh:11"),
            "{first:?}"
        );
        // Not committed yet: the same batch reads a second gating.
        let second = feed(
            &mut monitor,
            12,
            0,
            red_gated("rejected", 0, &["046.sh", "047.sh"]),
        );
        let red: Vec<&str> = fires(&second)
            .into_iter()
            .filter(|key| key.starts_with("scenario_red:"))
            .collect();
        assert_eq!(red, ["scenario_red:4:047.sh:12"], "{second:?}");
        commit(&mut monitor, &first);
        commit(&mut monitor, &second);
        let Event::Fire {
            evidence, notice, ..
        } = first
            .iter()
            .find(|event| event.key() == "scenario_red:4:046.sh:11")
            .expect("the fire")
        else {
            panic!("a fire")
        };
        assert!(!notice, "a condition, not a notice");
        assert_eq!(evidence["scenario"], "046.sh");
        assert_eq!(evidence["mission"], 4);
        // Another mission's accepted gate is no word on mission 4's main.
        feed(
            &mut monitor,
            30,
            0,
            json!({"type": "task.assigned", "id": 8, "mission_id": 5, "agent_id": "b"}),
        );
        let elsewhere = feed(
            &mut monitor,
            31,
            0,
            json!({"type": "task.gated", "id": 8, "verdict": "accepted", "revision": 0, "base": "c"}),
        );
        assert!(
            !elsewhere
                .iter()
                .any(|event| matches!(event, Event::Clear { .. })),
            "{elsewhere:?}"
        );
        let rejected = feed(&mut monitor, 13, 0, red_gated("rejected", 1, &[]));
        assert!(
            !rejected
                .iter()
                .any(|event| matches!(event, Event::Clear { .. })),
            "{rejected:?}"
        );
        let accepted = feed(&mut monitor, 14, 0, red_gated("accepted", 2, &["047.sh"]));
        let cleared: Vec<&str> = accepted
            .iter()
            .filter(|event| matches!(event, Event::Clear { .. }))
            .map(Event::key)
            .collect();
        assert_eq!(cleared, ["scenario_red:4:046.sh:11"], "{accepted:?}");
        commit(&mut monitor, &accepted);
        assert!(monitor.is_open("scenario_red:4:047.sh:12"));
    }

    /// `task_parked` (sync/design-load-sensitive-bound.md): the bound's
    /// parking fires it once per task, naming the pushed commit, each
    /// scenario's root-fix owner and the release command; `no_room` and
    /// `stale` leave it open, and the task's release clears it, naming who
    /// and why. Parked again after: a new alert, which a later verdict
    /// clears; parked again and cancelled: the cancel clears it.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_parked_task_is_an_alert_until_released_judged_or_ended() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            10,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let parked = || {
            json!({"type": "task.gated", "id": 7, "revision": 0, "verdict": "host_fault",
                   "evidence": "load_sensitive_parked", "pushed": "ab".repeat(20),
                   "sensitive": ["099.sh", "046.sh"], "parked": true, "escalate": true,
                   "owners": [{"scenario": "099.sh", "owner": "rev-f"},
                              {"scenario": "046.sh", "owner": null}]})
        };
        let first = feed(&mut monitor, 11, 0, parked());
        assert_eq!(fires(&first), ["task_parked:7:11"]);
        let Event::Fire {
            subject,
            evidence,
            text,
            notice,
            ..
        } = &first[0]
        else {
            panic!("{first:?}");
        };
        assert_eq!(subject, "task #7");
        assert!(!notice, "it needs a person");
        assert_eq!(
            evidence["command"],
            "spawn task release --id 7 --reason <why>"
        );
        assert_eq!(evidence["mission"], 4);
        assert_eq!(evidence["scenarios"], json!(["099.sh", "046.sh"]));
        for wanted in [
            "task #7 (mission #4)",
            "pushed commit abababababab",
            "099.sh (root fix: rev-f)",
            "046.sh (no root-fix owner named in scenario-owners)",
            "spawn task release --id 7",
        ] {
            assert!(text.contains(wanted), "{wanted:?} in {text}");
        }
        commit(&mut monitor, &first);
        assert!(
            fires(&feed(&mut monitor, 12, 0, parked())).is_empty(),
            "one per task"
        );
        for verdict in ["no_room", "stale"] {
            let kept = feed(
                &mut monitor,
                13,
                0,
                json!({"type": "task.gated", "id": 7, "revision": 0, "verdict": verdict}),
            );
            assert!(
                !decided(&monitor, kept)
                    .iter()
                    .any(|event| event.key().starts_with("task_parked:")),
                "{verdict} judges nothing"
            );
        }
        let released = feed(
            &mut monitor,
            14,
            0,
            json!({"type": "task.released", "id": 7, "by": "ckl", "reason": "099 fixed"}),
        );
        let released = decided(&monitor, released);
        let [Event::Clear { key, evidence }] = released.as_slice() else {
            panic!("{released:?}");
        };
        assert_eq!(key, "task_parked:7:11");
        assert_eq!(
            *evidence,
            json!({"event_id": 14, "record": "task.released", "by": "ckl", "reason": "099 fixed"})
        );
        commit(&mut monitor, &released);
        // Another task's release, or a release with none open, clears nothing.
        let none = feed(
            &mut monitor,
            15,
            0,
            json!({"type": "task.released", "id": 7, "by": "ckl", "reason": "again"}),
        );
        assert!(decided(&monitor, none).is_empty());

        let second = feed(&mut monitor, 16, 0, parked());
        assert_eq!(
            fires(&second),
            ["task_parked:7:16"],
            "parked again: a new alert"
        );
        commit(&mut monitor, &second);
        let judged = feed(
            &mut monitor,
            17,
            0,
            json!({"type": "task.gated", "id": 7, "revision": 0, "verdict": "rejected"}),
        );
        let judged = decided(&monitor, judged);
        assert!(
            judged.iter().any(|event| matches!(event,
                Event::Clear { key, evidence }
                    if key == "task_parked:7:16" && evidence["verdict"] == "rejected")),
            "{judged:?}"
        );
        commit(&mut monitor, &judged);
        let third = feed(&mut monitor, 18, 0, parked());
        commit(&mut monitor, &third);
        let cancelled = feed(
            &mut monitor,
            19,
            0,
            json!({"type": "task.cancelled", "id": 7}),
        );
        assert!(
            decided(&monitor, cancelled)
                .iter()
                .any(|event| matches!(event,
                Event::Clear { key, evidence }
                    if key == "task_parked:7:18" && evidence["record"] == "task.cancelled")),
            "the task's end clears it"
        );
        let mut monitor = monitor_parked(parked());
        let over = feed(
            &mut monitor,
            21,
            0,
            json!({"type": "task.over_budget", "id": 7, "reason": "r"}),
        );
        assert!(
            decided(&monitor, over)
                .iter()
                .any(|event| event.key() == "task_parked:7:20"),
            "over budget ends it too"
        );
    }

    /// A monitor that knows task 7 of mission 4, parked by `record` (event 20).
    fn monitor_parked(record: Value) -> Monitor {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            10,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let parked = feed(&mut monitor, 20, 0, record);
        commit(&mut monitor, &parked);
        monitor
    }

    /// `validator_defect` (review-fix-gate.md F1 and N1): a task's gate that
    /// ended on one opens it once per mission and scenario, and the
    /// mission's next verdict clears it; an owner's accepted landing that
    /// names one opens one for its source, naming the missions it moves,
    /// which a verdict that says nothing of the scenario leaves open and a
    /// gate naming it `quarantined` clears (objective 056).
    #[test]
    fn a_validator_defect_is_an_alert_until_judged_again_or_quarantined() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            10,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let defect = |revision: i64| {
            json!({"type": "task.gated", "id": 7, "verdict": "host_fault", "revision": revision,
                   "base": "b", "evidence": "validator_defect",
                   "validator_defects": ["028.sh"],
                   "fact": {"scenario": "028.sh", "on": "candidate and base"}})
        };
        let first = feed(&mut monitor, 11, 0, defect(0));
        assert_eq!(
            fires(&first)
                .into_iter()
                .filter(|key| key.starts_with("validator_defect:"))
                .collect::<Vec<_>>(),
            ["validator_defect:4:028.sh:11"],
            "{first:?}"
        );
        commit(&mut monitor, &first);
        let again = feed(&mut monitor, 12, 0, defect(0));
        assert!(
            !fires(&again)
                .iter()
                .any(|key| key.starts_with("validator_defect:")),
            "once while open: {again:?}"
        );
        commit(&mut monitor, &again);
        // The owner's landing accepted without 028.sh and 031.sh: its own
        // alert for mission 4 (and mission 9, which it moved too).
        let moved = feed(
            &mut monitor,
            13,
            0,
            json!({"type": "source.moved", "source": "/s", "request": 77, "from": "b",
                   "to": "c", "missions": [4, 9], "verdict": "accepted",
                   "validator_defects": ["028.sh"]}),
        );
        assert_eq!(
            fires(&moved),
            ["validator_defect:/s:028.sh:13"],
            "{moved:?}"
        );
        commit(&mut monitor, &moved);
        // The mission's next verdict clears the task's, not the owner's.
        let judged = feed(
            &mut monitor,
            14,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "accepted", "revision": 0,
                   "base": "c"}),
        );
        let cleared: Vec<&str> = judged
            .iter()
            .filter(|event| matches!(event, Event::Clear { .. }))
            .map(Event::key)
            .collect();
        assert_eq!(cleared, ["validator_defect:4:028.sh:11"], "{judged:?}");
        commit(&mut monitor, &judged);
        assert!(monitor.is_open("validator_defect:/s:028.sh:13"));
        // A gate of mission 4 that names 028.sh quarantined: the owner chose.
        let quarantined = feed(
            &mut monitor,
            15,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "rejected", "revision": 0,
                   "base": "c", "quarantined": ["028.sh"]}),
        );
        let cleared: Vec<&str> = quarantined
            .iter()
            .filter(|event| matches!(event, Event::Clear { .. }))
            .map(Event::key)
            .collect();
        assert_eq!(
            cleared,
            ["validator_defect:/s:028.sh:13"],
            "{quarantined:?}"
        );
        let Some(Event::Clear { evidence, .. }) = quarantined
            .iter()
            .find(|event| matches!(event, Event::Clear { .. }))
        else {
            panic!("a clear");
        };
        assert_eq!(evidence["cause"], "quarantined");
        assert_eq!(evidence["event_id"], 15);
    }

    /// The clearings of `validator_defect` (objective 056) and the evidence
    /// each names.
    fn clears_of(events: &[Event]) -> Vec<(String, Value, Value)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Clear { key, evidence } => Some((
                    key.clone(),
                    evidence["cause"].clone(),
                    evidence["event_id"].clone(),
                )),
                Event::Fire { .. } => None,
            })
            .collect()
    }

    /// A monitor that knows missions 1 and 2 (open) and 3 (closed), and a
    /// task of each of 1 and 2 (tasks 1 and 2).
    fn three_missions() -> Monitor {
        let mut monitor = monitor();
        for id in 1..=3 {
            feed(
                &mut monitor,
                id,
                0,
                json!({"type": "mission.added", "id": id, "text": "m"}),
            );
        }
        feed(
            &mut monitor,
            4,
            0,
            json!({"type": "mission.closed", "id": 3, "state": "done"}),
        );
        for id in 1..=2 {
            feed(
                &mut monitor,
                4 + id,
                0,
                json!({"type": "task.assigned", "id": id, "mission_id": id, "agent_id": "a"}),
            );
        }
        monitor
    }

    fn landing(missions: &[i64], defects: &[&str]) -> Value {
        json!({"type": "source.moved", "source": "/s", "request": 9, "from": "b", "to": "c",
               "missions": missions, "verdict": "accepted", "validator_defects": defects})
    }

    /// Objective 056 (b): an owner landing's defect is one alert per source
    /// and scenario, naming the moved missions that are open; none when
    /// none is; a second landing of the same source and scenario fires
    /// nothing new.
    #[test]
    fn an_owner_landings_defect_is_one_alert_per_source_about_open_missions() {
        let mut monitor = three_missions();
        let moved = feed(&mut monitor, 10, 0, landing(&[1, 2, 3], &["v1.sh"]));
        let fired: Vec<(&str, &str, &Value, String)> = moved
            .iter()
            .filter_map(|event| match event {
                Event::Fire {
                    key,
                    subject,
                    evidence,
                    text,
                    ..
                } => Some((key.as_str(), subject.as_str(), evidence, text.clone())),
                Event::Clear { .. } => None,
            })
            .collect();
        assert_eq!(fired.len(), 1, "{moved:?}");
        let (key, subject, evidence, text) = &fired[0];
        assert_eq!(*key, "validator_defect:/s:v1.sh:10");
        assert_eq!(*subject, "/s (mission #1, mission #2)");
        assert_eq!(evidence["missions"], json!([1, 2]));
        assert_eq!(evidence["source"], "/s");
        assert_eq!(evidence["owner"], true);
        assert_eq!(evidence["request"], 9);
        assert!(
            text.contains("mission #1, mission #2") && !text.contains("#3"),
            "{text}"
        );
        commit(&mut monitor, &moved);
        let again = feed(&mut monitor, 11, 0, landing(&[2], &["v1.sh"]));
        assert!(fires(&again).is_empty(), "{again:?}");
        let closed_only = feed(&mut monitor, 12, 0, landing(&[3], &["v0.sh"]));
        assert!(fires(&closed_only).is_empty(), "{closed_only:?}");
        // A task's defect stays one per mission.
        let task = feed(
            &mut monitor,
            13,
            0,
            json!({"type": "task.gated", "id": 1, "verdict": "host_fault",
                   "evidence": "validator_defect", "validator_defects": ["v1.sh"]}),
        );
        assert_eq!(fires(&task), ["validator_defect:1:v1.sh:13"]);
    }

    /// Objective 056 (c): a gating of a mission it names clears it when the
    /// scenario passed (`passed`, `reused`) or left the gate directory
    /// (`scenario_names` without it); one that holds it and passed nothing
    /// of it leaves it; a later landing of the source that passed it clears
    /// it; closing every mission it names clears it, one of two does not.
    #[test]
    fn a_validator_defect_clears_once_passed_gone_or_its_missions_closed() {
        let mut monitor = three_missions();
        let moved = feed(
            &mut monitor,
            10,
            0,
            landing(&[1, 2], &["v2.sh", "v3.sh", "v4.sh"]),
        );
        commit(&mut monitor, &moved);
        let gated = feed(
            &mut monitor,
            11,
            0,
            json!({"type": "task.gated", "id": 1, "verdict": "rejected",
                   "scenario_names": ["o.sh", "v2.sh", "v3.sh", "v4.sh"],
                   "passed": ["o.sh", "v2.sh"], "reused": ["v3.sh"]}),
        );
        let mut cleared = clears_of(&gated);
        cleared.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            cleared,
            [
                (
                    "validator_defect:/s:v2.sh:10".to_string(),
                    json!("passed"),
                    json!(11)
                ),
                (
                    "validator_defect:/s:v3.sh:10".to_string(),
                    json!("passed"),
                    json!(11)
                ),
            ]
        );
        commit(&mut monitor, &gated);
        let gone = feed(
            &mut monitor,
            12,
            0,
            json!({"type": "task.gated", "id": 2, "verdict": "rejected",
                   "scenario_names": ["o.sh", "v2.sh"]}),
        );
        assert_eq!(
            clears_of(&gone),
            [(
                "validator_defect:/s:v4.sh:10".to_string(),
                json!("gone"),
                json!(12)
            )]
        );
        commit(&mut monitor, &gone);

        // Passed in a later landing of the source.
        let moved = feed(&mut monitor, 13, 0, landing(&[1, 2], &["v5.sh", "v6.sh"]));
        commit(&mut monitor, &moved);
        let mut passed = landing(&[1], &[]);
        passed["passed"] = json!(["v6.sh"]);
        let relanded = feed(&mut monitor, 14, 0, passed);
        assert_eq!(
            clears_of(&relanded),
            [(
                "validator_defect:/s:v6.sh:13".to_string(),
                json!("passed"),
                json!(14)
            )]
        );
        commit(&mut monitor, &relanded);
        // Closed: one of two leaves it, both clear it.
        let one = feed(
            &mut monitor,
            15,
            0,
            json!({"type": "mission.closed", "id": 1, "state": "done"}),
        );
        assert!(clears_of(&one).is_empty(), "{one:?}");
        let both = feed(
            &mut monitor,
            16,
            0,
            json!({"type": "mission.closed", "id": 2, "state": "done"}),
        );
        assert_eq!(
            clears_of(&both),
            [(
                "validator_defect:/s:v5.sh:13".to_string(),
                json!("closed"),
                json!(16)
            )]
        );
        commit(&mut monitor, &both);
        assert!(decided(&monitor, both).is_empty(), "each clears once");
    }

    /// A restart that reads an open `scenario_red` back fires nothing again
    /// for it, and still clears it at the accepted gating that no longer
    /// names it.
    #[test]
    fn a_restart_keeps_main_red_one_alert() {
        // The restart: the open alert read back, with its evidence.
        let mut restarted = super::Monitor::with(settings());
        restarted.known.insert(
            "scenario_red:4:047.sh:12".to_string(),
            Row {
                kind: "scenario_red".to_string(),
                subject: "mission #4".to_string(),
                state: "open".to_string(),
                evidence: json!({"mission": 4, "scenario": "047.sh"}),
                fired_event_id: 12,
                fired_at_ms: 0,
            },
        );
        restarted.seed_red();
        feed(
            &mut restarted,
            20,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let again = feed(&mut restarted, 21, 0, red_gated("rejected", 0, &["047.sh"]));
        assert!(
            !fires(&again)
                .iter()
                .any(|key| key.starts_with("scenario_red:")),
            "{again:?}"
        );
        let green = feed(&mut restarted, 22, 0, red_gated("accepted", 1, &[]));
        assert!(
            green
                .iter()
                .any(|event| matches!(event, Event::Clear { key, .. } if key == "scenario_red:4:047.sh:12")),
            "{green:?}"
        );
    }

    /// A restart that reads an open `message_not_taken` alert back: the
    /// message, still not taken, fires nothing again; taken, it clears,
    /// once. A message not due yet is pending, sets the step's next bound,
    /// and fires once at it.
    #[test]
    fn a_restart_keeps_a_message_not_taken_one_alert() {
        let message = |id: i64, due: i64, not_taken: bool, taken: i64| {
            json!({"command_id": id, "agent_id": "agent_worker_a",
                   "session_id": "session_worker_a", "action": "input",
                   "created_at_ms": due - 120_000, "due_ms": due, "tracked": true,
                   "not_taken": not_taken, "typed": true, "taken": taken,
                   "taken_at_ms": if taken > 0 { json!(due) } else { Value::Null },
                   "ended": false, "state": if taken > 0 { "taken" } else { "typed" }})
        };
        let mut restarted = super::Monitor::with(settings());
        restarted.known.insert(
            "message_not_taken:41".to_string(),
            Row {
                kind: "message_not_taken".to_string(),
                subject: "session_worker_a".to_string(),
                state: "open".to_string(),
                evidence: json!({"command": 41, "typed": true}),
                fired_event_id: 12,
                fired_at_ms: 0,
            },
        );
        assert_eq!(restarted.open_messages(), [41]);
        let still = json!({"now_ms": 500_000, "max_id": 42, "messages": [
            message(41, 121_000, true, 0), message(42, 610_000, false, 0)]});
        let mut out = Vec::new();
        restarted.messages(&still, &mut out);
        assert!(decided(&restarted, out).is_empty(), "not fired again");
        assert_eq!(restarted.messages_due, Some(610_000));
        assert_eq!(restarted.messages_after, 42);
        assert_eq!(
            restarted
                .pending
                .iter()
                .map(|(id, due)| (*id, *due))
                .collect::<Vec<_>>(),
            [(42, 610_000)]
        );
        assert_eq!(restarted.messages_since, 440_000);
        restarted.plan(500_000);
        assert_eq!(restarted.next_bound_ms, Some(610_000));

        let due = json!({"now_ms": 610_000, "max_id": 42, "messages": [
            message(41, 121_000, true, 1), message(42, 610_000, true, 0)]});
        let mut out = Vec::new();
        restarted.messages(&due, &mut out);
        let events = decided(&restarted, out);
        assert!(
            matches!(&events[..], [Event::Clear { key, evidence }, Event::Fire { key: fired, .. }]
                if key == "message_not_taken:41" && evidence["taken"] == 1
                    && fired == "message_not_taken:42"),
            "{events:?}"
        );
        commit(&mut restarted, &events);
        let mut out = Vec::new();
        restarted.messages(&due, &mut out);
        assert!(decided(&restarted, out).is_empty(), "each once");
        assert!(restarted.pending.is_empty());
        assert_eq!(restarted.messages_due, None);
    }

    /// Objective 056 after a restart: an owner alert read back (per source)
    /// fires nothing again for its source and clears when a gating passes
    /// it; one of before 056 (per mission, no source) whose mission is
    /// closed clears at the start, and one whose mission is open clears
    /// once its scenario leaves the gate directory.
    #[test]
    fn a_restart_clears_the_validator_defects_it_reads_back_alike() {
        let mut restarted = three_missions();
        for (key, evidence, event) in [
            (
                "validator_defect:/s:v1.sh:20",
                json!({"source": "/s", "missions": [1, 2], "scenario": "v1.sh",
                       "owner": true, "event_id": 20}),
                20,
            ),
            (
                "validator_defect:3:v7.sh:8",
                json!({"mission": 3, "scenario": "v7.sh", "owner": true, "event_id": 8}),
                8,
            ),
            (
                "validator_defect:2:v8.sh:8",
                json!({"mission": 2, "scenario": "v8.sh", "owner": true, "event_id": 8}),
                8,
            ),
        ] {
            restarted.known.insert(
                key.to_string(),
                Row {
                    kind: "validator_defect".to_string(),
                    subject: "x".to_string(),
                    state: "open".to_string(),
                    evidence,
                    fired_event_id: event,
                    fired_at_ms: 0,
                },
            );
        }
        restarted.seed_defects();
        let mut out = Vec::new();
        restarted.closed_defects(None, 1_000, &mut out);
        let at_start = decided(&restarted, out);
        assert_eq!(
            at_start.iter().map(Event::key).collect::<Vec<_>>(),
            ["validator_defect:3:v7.sh:8"],
            "the closed mission's, at the start"
        );
        let Event::Clear { evidence, .. } = &at_start[0] else {
            panic!("a clear");
        };
        assert_eq!(evidence["cause"], "closed");
        commit(&mut restarted, &at_start);
        let again = feed(&mut restarted, 30, 0, landing(&[1], &["v1.sh"]));
        assert!(fires(&again).is_empty(), "{again:?}");
        let gated = feed(
            &mut restarted,
            31,
            0,
            json!({"type": "task.gated", "id": 2, "verdict": "rejected",
                   "scenario_names": ["v1.sh"], "passed": ["v1.sh"]}),
        );
        let mut cleared = clears_of(&decided(&restarted, gated));
        cleared.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            cleared,
            [
                (
                    "validator_defect:/s:v1.sh:20".to_string(),
                    json!("passed"),
                    json!(31)
                ),
                (
                    "validator_defect:2:v8.sh:8".to_string(),
                    json!("gone"),
                    json!(31)
                ),
            ]
        );
    }

    /// The live shape of 2026-11-11 23:33 (lead-mac): two owner alerts from
    /// before objective 056, one per mission (#4, gated, and #7, a review
    /// mission with no gate of its own), both about 003.sh, which the source's
    /// deployed scenario set no longer holds. A reading of the open missions'
    /// gate directories clears both `gone` at once, naming the directory read;
    /// no gating is needed. A scenario the directory still holds stays open,
    /// and so does one whose source has no gate directory to read, or whose
    /// directory is not there now (nothing can be told).
    #[test]
    fn a_reading_clears_a_validator_defect_whose_scenario_left_the_deployed_set() {
        let mut restarted = monitor();
        let open = |key: &str, evidence: Value| {
            (
                key.to_string(),
                Row {
                    kind: "validator_defect".to_string(),
                    subject: "x".to_string(),
                    state: "open".to_string(),
                    evidence,
                    fired_event_id: 130_068,
                    fired_at_ms: 0,
                },
            )
        };
        for (key, row) in [
            open(
                "validator_defect:4:003.sh:130068",
                json!({"mission": 4, "scenario": "003.sh", "owner": true, "event_id": 130_068}),
            ),
            open(
                "validator_defect:7:003.sh:130068",
                json!({"mission": 7, "scenario": "003.sh", "owner": true, "event_id": 130_068}),
            ),
            open(
                "validator_defect:4:041.sh:9",
                json!({"mission": 4, "scenario": "041.sh", "owner": false, "event_id": 9}),
            ),
            open(
                "validator_defect:9:003.sh:12",
                json!({"mission": 9, "scenario": "003.sh", "owner": false, "event_id": 12}),
            ),
        ] {
            restarted.known.insert(key, row);
        }
        restarted.seed_defects();
        // Missions 4 (its gate) and 7 (none) on the Spawn source; 9 on a
        // source whose open missions have no gate.
        let gates = [
            (
                4,
                Some("/ctl/rsi/scenarios".to_string()),
                Some("/m/spawn.git".to_string()),
            ),
            (7, None, Some("/m/spawn.git".to_string())),
            (9, None, Some("/m/other.git".to_string())),
        ];
        let held = |path: &str| path == "/ctl/rsi/scenarios/041.sh";
        let mut out = Vec::new();
        restarted.gone_defects(
            &gates,
            |dir| dir == "/ctl/rsi/scenarios",
            held,
            5_000,
            &mut out,
        );
        let mut cleared: Vec<(String, Value)> = out
            .iter()
            .filter_map(|event| match event {
                Event::Clear { key, evidence } => Some((key.clone(), evidence.clone())),
                Event::Fire { .. } => None,
            })
            .collect();
        cleared.sort_by(|left, right| left.0.cmp(&right.0));
        let gone = json!({"cause": "gone", "reading_ms": 5_000, "gates": ["/ctl/rsi/scenarios"]});
        assert_eq!(
            cleared,
            [
                ("validator_defect:4:003.sh:130068".to_string(), gone.clone()),
                ("validator_defect:7:003.sh:130068".to_string(), gone),
            ],
            "both of 003.sh's, at one reading"
        );
        let left: Vec<&str> = restarted
            .defects
            .iter()
            .map(|defect| defect.key.as_str())
            .collect();
        assert_eq!(
            left,
            [
                "validator_defect:4:041.sh:9",
                "validator_defect:9:003.sh:12"
            ],
            "a scenario still deployed, and one with no gate directory to read, stay open"
        );
        // A gate directory that is not there now tells nothing.
        let mut out = Vec::new();
        restarted.gone_defects(&gates, |_| false, |_| false, 6_000, &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    /// A restart that reads open `validator_defect` alerts back (rev-b F1),
    /// one of each origin: a repeat of the task's defect fires nothing, the
    /// mission's next verdict clears only the task's, and a gate naming the
    /// scenario quarantined clears the owner's.
    #[test]
    fn a_restart_keeps_each_validator_defect_one_alert() {
        let mut restarted = super::Monitor::with(settings());
        for (key, owner, event) in [
            ("validator_defect:4:028.sh:11", false, 11),
            ("validator_defect:4:028.sh:13", true, 13),
        ] {
            restarted.known.insert(
                key.to_string(),
                Row {
                    kind: "validator_defect".to_string(),
                    subject: "mission #4".to_string(),
                    state: "open".to_string(),
                    evidence: json!({"mission": 4, "scenario": "028.sh", "owner": owner}),
                    fired_event_id: event,
                    fired_at_ms: 0,
                },
            );
        }
        restarted.seed_defects();
        feed(
            &mut restarted,
            20,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let again = feed(
            &mut restarted,
            21,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "host_fault", "revision": 0,
                   "base": "b", "evidence": "validator_defect",
                   "validator_defects": ["028.sh"],
                   "fact": {"scenario": "028.sh", "on": "candidate and base"}}),
        );
        assert!(
            !fires(&again)
                .iter()
                .any(|key| key.starts_with("validator_defect:")),
            "{again:?}"
        );
        let cleared = |events: &[Event]| -> Vec<String> {
            events
                .iter()
                .filter(|event| matches!(event, Event::Clear { .. }))
                .map(|event| event.key().to_string())
                .collect()
        };
        let judged = feed(
            &mut restarted,
            22,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "accepted", "revision": 0,
                   "base": "c"}),
        );
        assert_eq!(
            cleared(&judged),
            ["validator_defect:4:028.sh:11"],
            "{judged:?}"
        );
        commit(&mut restarted, &judged);
        let quarantined = feed(
            &mut restarted,
            23,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "accepted", "revision": 0,
                   "base": "c", "quarantined": ["028.sh"]}),
        );
        assert_eq!(
            cleared(&quarantined),
            ["validator_defect:4:028.sh:13"],
            "{quarantined:?}"
        );
    }

    #[test]
    fn the_keys_name_the_occurrence() {
        let mut monitor = monitor();
        let events = feed(
            &mut monitor,
            12,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        assert_eq!(fires(&events), ["task_state:7:12"]);
        feed(
            &mut monitor,
            13,
            0,
            json!({"type": "mission.added", "id": 4}),
        );
        let gated = feed(
            &mut monitor,
            14,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "host_fault",
                   "evidence": "base_disagrees", "alert": true, "revision": 0,
                   "scenario": "011.sh", "base_disagrees": 3}),
        );
        assert!(
            fires(&gated).contains(&"gate_base_disagrees:14"),
            "{gated:?}"
        );
        let drilled = feed(
            &mut monitor,
            15,
            0,
            json!({"type": "backup.verified", "key": "k", "ok": false, "reason": "r"}),
        );
        assert_eq!(fires(&drilled), ["drill_failed:15"]);
        let auth = feed(
            &mut monitor,
            16,
            0,
            json!({"type": "execution.interrupted", "cause": "model_auth",
                   "execution_id": "exec_1", "session_id": "session_x", "agent_id": "a"}),
        );
        assert_eq!(fires(&auth), ["model_auth:16"]);
    }

    /// A failed deploy (#54) fires one `deploy_failed`, naming the head and
    /// the reason; a failure again while it is open fires nothing more (the
    /// landing's deploy is tried again later); a success from any deploy
    /// clears it, and the next failure is a new occurrence.
    #[test]
    fn a_failed_deploy_fires_once_until_a_deploy_succeeds() {
        let mut monitor = monitor();
        let head = "c".repeat(40);
        let failed = feed(
            &mut monitor,
            20,
            0,
            json!({"type": "deploy.finished", "ok": false, "head": head, "by": "landing",
                   "reason": "cargo build failed"}),
        );
        assert_eq!(fires(&failed), ["deploy_failed:20"]);
        let Event::Fire {
            kind,
            subject,
            text,
            evidence,
            notice,
            ..
        } = &failed[0]
        else {
            panic!("a fire: {failed:?}");
        };
        assert_eq!(
            (*kind, subject.as_str(), *notice),
            ("deploy_failed", "deploy", false)
        );
        assert_eq!(
            text,
            "the landing's deploy of cccccccccccc failed: cargo build failed"
        );
        assert_eq!(evidence["head"], json!(head));
        commit(&mut monitor, &failed);
        let again = feed(
            &mut monitor,
            21,
            0,
            json!({"type": "deploy.finished", "ok": false, "head": head, "by": "landing",
                   "reason": "cargo build failed"}),
        );
        assert!(again.is_empty(), "one alert while it is open: {again:?}");
        let fixed = feed(
            &mut monitor,
            22,
            0,
            json!({"type": "deploy.finished", "ok": true, "head": "d".repeat(40),
                   "by": "owner", "reason": ""}),
        );
        assert!(
            matches!(fixed.as_slice(), [Event::Clear { key, .. }] if key == "deploy_failed:20"),
            "{fixed:?}"
        );
        commit(&mut monitor, &fixed);
        let next = feed(
            &mut monitor,
            23,
            0,
            json!({"type": "deploy.finished", "ok": false, "head": head, "by": "owner",
                   "reason": ""}),
        );
        assert_eq!(fires(&next), ["deploy_failed:23"]);
        let Event::Fire { text, .. } = &next[0] else {
            panic!("a fire")
        };
        assert_eq!(
            text,
            "the owner's deploy of cccccccccccc failed: no reason given"
        );
    }

    /// A mission whose controller lacks the owner's latest landing (P22) fires one
    /// `controller_behind` once it has been behind for the bound, from the
    /// first reading that found it so; nothing more while it stays behind;
    /// clears when the controller catches up (or the mission no longer
    /// waits); a mission caught up before the bound fires nothing.
    #[test]
    fn a_controller_behind_main_fires_after_the_bound_and_clears_when_caught_up() {
        let mut monitor = monitor();
        let c1 = "1".repeat(40);
        let m2 = "2".repeat(40);
        let behind = |missions: &[i64]| -> BTreeMap<i64, (String, String)> {
            missions
                .iter()
                .map(|mission| (*mission, (c1.clone(), m2.clone())))
                .collect()
        };
        assert!(monitor.controller_events(&behind(&[4, 6]), 1000).is_empty());
        assert!(
            monitor
                .controller_events(&behind(&[4, 6]), 900_000)
                .is_empty()
        );
        // Mission 6 caught up before the bound (its deploy came): it starts
        // again from its next reading behind.
        assert!(
            monitor
                .controller_events(&behind(&[4]), 1_000_000)
                .is_empty()
        );
        assert!(
            monitor
                .controller_events(&behind(&[4, 6]), 1_200_000)
                .is_empty()
        );
        let fired = monitor.controller_events(&behind(&[4, 6]), 1_801_000);
        assert_eq!(fires(&fired), ["controller_behind:4:1000"]);
        let Event::Fire {
            subject,
            evidence,
            text,
            notice,
            ..
        } = &fired[0]
        else {
            panic!("a fire: {fired:?}");
        };
        assert_eq!(subject, "mission #4");
        assert_eq!(evidence["controller"], c1.as_str());
        assert_eq!(evidence["landed"], m2.as_str());
        assert_eq!(evidence["since_ms"], 1000);
        assert_eq!(evidence["behind_ms"], 1_800_000);
        assert!(
            text.contains("controller 11111111 has not had the owner's landing 22222222 for 1800s")
                && !notice,
            "{text}"
        );
        commit(&mut monitor, &fired);
        assert!(
            monitor
                .controller_events(&behind(&[4, 6]), 2_000_000)
                .is_empty(),
            "once per occurrence; 6 is not yet behind for the bound"
        );
        let caught_up = monitor.controller_events(&behind(&[6]), 2_100_000);
        assert_eq!(
            caught_up.iter().map(Event::key).collect::<Vec<_>>(),
            ["controller_behind:4:1000"]
        );
        let Event::Clear { evidence, .. } = &caught_up[0] else {
            panic!("a clear: {caught_up:?}");
        };
        assert_eq!(evidence["behind"], false);
        commit(&mut monitor, &caught_up);
        let six = monitor.controller_events(&behind(&[4, 6]), 3_000_000);
        assert_eq!(fires(&six), ["controller_behind:6:1200000"]);
    }

    /// P12: a host-path source of an open or active mission that is not a
    /// directory fires one `source_away` per mission and source while it is
    /// away, clears when it is back or no longer watched, and fires anew
    /// when it goes again.
    #[test]
    fn a_source_away_fires_once_and_clears_when_back_or_unwatched() {
        let mut monitor = monitor();
        let watched: BTreeSet<(i64, String)> = [
            (4, "/mnt/vol/a.git".to_string()),
            (4, "/srv/b.git".to_string()),
            (6, "/mnt/vol/a.git".to_string()),
        ]
        .into();
        let gone = |path: &str| !path.starts_with("/mnt/vol");
        // The first reading that finds them away only notes it.
        assert!(monitor.source_events(&watched, gone, 500).is_empty());
        let first = monitor.source_events(&watched, gone, 1000);
        assert_eq!(fires(&first), ["source_away:4:1000", "source_away:6:1000"]);
        let Event::Fire {
            subject,
            evidence,
            text,
            notice,
            ..
        } = &first[0]
        else {
            panic!("a fire: {first:?}");
        };
        assert_eq!(subject, "mission #4");
        assert_eq!(evidence["source"], "/mnt/vol/a.git");
        assert_eq!(
            evidence["away_since_ms"], 500,
            "since the first reading that found it"
        );
        assert!(
            text.contains("/mnt/vol/a.git is not a directory") && !notice,
            "{text}"
        );
        commit(&mut monitor, &first);
        // Still away: nothing more, however many readings.
        assert!(monitor.source_events(&watched, gone, 2000).is_empty());
        // Back for mission 4's and 6's, which share the path: both clear.
        let back = monitor.source_events(&watched, |_| true, 3000);
        assert_eq!(
            back.iter().map(Event::key).collect::<Vec<_>>(),
            ["source_away:4:1000", "source_away:6:1000"]
        );
        let Event::Clear { evidence, .. } = &back[0] else {
            panic!("a clear: {back:?}");
        };
        assert_eq!(evidence["present"], true);
        commit(&mut monitor, &back);
        // Away again: a new occurrence, a new key, after two readings again.
        assert!(monitor.source_events(&watched, gone, 3500).is_empty());
        let again = monitor.source_events(&watched, gone, 4000);
        assert_eq!(fires(&again), ["source_away:4:4000", "source_away:6:4000"]);
        commit(&mut monitor, &again);
        // Mission 6 closed (or dropped the source): its alert clears, still
        // away or not; mission 4's stays.
        let fewer: BTreeSet<(i64, String)> = watched
            .iter()
            .filter(|(mission, _)| *mission == 4)
            .cloned()
            .collect();
        let closed = monitor.source_events(&fewer, gone, 5000);
        assert_eq!(
            closed.iter().map(Event::key).collect::<Vec<_>>(),
            ["source_away:6:4000"]
        );
        let Event::Clear { evidence, .. } = &closed[0] else {
            panic!("a clear: {closed:?}");
        };
        assert_eq!(evidence["reason"], "no longer watched");
    }

    /// rev-f's N2: a path that is away at one reading and back at the next,
    /// again and again, fires nothing; one away at two readings in a row
    /// fires once.
    #[test]
    fn a_source_that_comes_and_goes_does_not_flap() {
        let mut flapping = monitor();
        let watched: BTreeSet<(i64, String)> = [(4, "/mnt/vol/a.git".to_string())].into();
        for reading in 0..6 {
            let away = reading % 2 == 0;
            let events = flapping.source_events(&watched, |_| !away, reading * 1000);
            assert!(events.is_empty(), "reading {reading}: {events:?}");
        }
        assert!(flapping.source_events(&watched, |_| false, 6000).is_empty());
        let fired = flapping.source_events(&watched, |_| false, 7000);
        assert_eq!(fires(&fired), ["source_away:4:7000"]);
        // Unwatched in between: the count starts again.
        let mut other = Monitor::with(settings());
        assert!(other.source_events(&watched, |_| false, 0).is_empty());
        assert!(
            other
                .source_events(&BTreeSet::new(), |_| false, 1000)
                .is_empty()
        );
        assert!(other.source_events(&watched, |_| false, 2000).is_empty());
        assert_eq!(
            fires(&other.source_events(&watched, |_| false, 3000)),
            ["source_away:4:3000"]
        );
    }

    #[test]
    fn only_a_path_on_this_host_is_a_watched_source() {
        let listed = json!([
            {"mission": 4, "sources": ["/srv/a.git", "https://example.com/b.git", "relative/c",
                                       "git@example.com:d.git", 7]},
            {"mission": 6, "sources": ["/srv/a.git"]},
            {"mission": null, "sources": ["/srv/e.git"]},
        ]);
        let watched: Vec<(i64, String)> = host_sources(&listed).into_iter().collect();
        assert_eq!(
            watched,
            [(4, "/srv/a.git".to_string()), (6, "/srv/a.git".to_string())]
        );
    }

    #[test]
    fn memory_and_tmpfs_do_not_flap_between_their_thresholds() {
        // MemAvailable: fire under 4096, clear at 8192; 5000 in between is
        // neither, whichever side it comes from.
        assert_eq!(crossing(false, 2000, 4096, 8192, true), Crossing::Fire);
        assert_eq!(crossing(true, 5000, 4096, 8192, true), Crossing::Nothing);
        assert_eq!(crossing(true, 9000, 4096, 8192, true), Crossing::Clear);
        assert_eq!(crossing(false, 9000, 4096, 8192, true), Crossing::Nothing);
        assert_eq!(crossing(true, 4096, 4096, 8192, true), Crossing::Nothing);
        // The filesystem: fire over 8000, clear under 6000.
        assert_eq!(crossing(false, 9000, 8000, 6000, false), Crossing::Fire);
        assert_eq!(crossing(true, 7000, 8000, 6000, false), Crossing::Nothing);
        assert_eq!(crossing(true, 5000, 8000, 6000, false), Crossing::Clear);
        assert_eq!(crossing(false, 5000, 8000, 6000, false), Crossing::Nothing);
        // A threshold the host never crosses: both equal, the usual run's
        // 1/1 and 100000000/100000000.
        assert_eq!(crossing(false, 19_000, 1, 1, true), Crossing::Nothing);
        assert_eq!(crossing(true, 19_000, 1, 1, true), Crossing::Clear);
        assert_eq!(
            crossing(false, 4000, 100_000_000, 100_000_000, false),
            Crossing::Nothing
        );
    }

    /// Objective 204: free disk under its threshold fires one `disk_low`
    /// with its evidence and text; a lower reading fires no second; one
    /// between the threshold and the margin holds it; one at the margin
    /// clears it, the margin in its evidence; a threshold of 0 reads
    /// nothing; and a threshold over any disk fires on the real root's
    /// reading.
    #[test]
    fn free_disk_under_its_threshold_alerts_once_and_clears_at_its_margin() {
        const GIB: u64 = 1 << 30;
        let root = Path::new("/srv/ws");
        let mut monitor = Monitor::with(Settings {
            disk_root: Some(root.to_path_buf()),
            disk_low_gb: 40,
            disk_ok_gb: 45,
            ..settings()
        });
        let step = |monitor: &mut Monitor, now: i64, free: u64| {
            let mut out = Vec::new();
            monitor.disk_reading(now, root, free, &mut out);
            let events = decided(monitor, out);
            commit(monitor, &events);
            events
        };
        assert_eq!(step(&mut monitor, 1, 50 * GIB), [], "50 GiB: nothing");
        let fired = step(&mut monitor, 2, 39 * GIB + GIB / 2);
        assert_eq!(
            fired,
            [Event::Fire {
                key: "disk_low:2".to_string(),
                kind: "disk_low",
                subject: "host".to_string(),
                evidence: json!({
                    "path": "/srv/ws", "free_bytes": 39 * GIB + GIB / 2,
                    "threshold_bytes": 40 * GIB, "reading_ms": 2,
                }),
                text: "/srv/ws has 39.5 GiB free, under 40 GiB".to_string(),
                notice: false,
            }]
        );
        assert_eq!(step(&mut monitor, 3, 30 * GIB), [], "no second while open");
        assert_eq!(step(&mut monitor, 4, 42 * GIB), [], "held between the two");
        assert_eq!(
            step(&mut monitor, 5, 45 * GIB),
            [Event::Clear {
                key: "disk_low:2".to_string(),
                evidence: json!({
                    "path": "/srv/ws", "free_bytes": 45 * GIB,
                    "threshold_bytes": 45 * GIB, "reading_ms": 5,
                }),
            }]
        );
        assert_eq!(step(&mut monitor, 6, 50 * GIB), [], "cleared stays cleared");

        // A threshold of 0 reads nothing, whatever the root holds.
        let mut off = Monitor::with(Settings {
            disk_root: Some(std::env::temp_dir()),
            ..settings()
        });
        let (mut out, mut lines) = (Vec::new(), Vec::new());
        off.read_disk(7, &mut out, &mut lines);
        assert_eq!((out.len(), lines.len()), (0, 0));

        // A threshold over any disk fires on the real root's reading.
        let real = std::env::temp_dir();
        let mut over = Monitor::with(Settings {
            disk_root: Some(real.clone()),
            disk_low_gb: 1_000_000,
            disk_ok_gb: 2_000_000,
            ..settings()
        });
        over.read_disk(8, &mut out, &mut lines);
        assert_eq!(lines, Vec::<String>::new());
        let [Event::Fire { kind, evidence, .. }] = out.as_slice() else {
            panic!("one fire: {out:?}");
        };
        assert_eq!(*kind, "disk_low");
        assert_eq!(evidence["path"], real.display().to_string());
        assert!(evidence["free_bytes"].is_u64(), "{evidence}");
        assert_eq!(evidence["threshold_bytes"], 1_000_000 * GIB);
    }

    #[test]
    fn a_command_failure_is_a_notice_and_an_owner_note_is_not() {
        let mut monitor = monitor();
        let commands = json!([
            {"command": 5, "agent_id": "agent_a", "session_id": "session_a",
             "principal": "h", "action": "input", "key": "k-5", "error": "no execution"},
            {"command": 6, "agent_id": "owner", "session_id": "session_owner",
             "principal": "owner", "action": "input", "key": "owner-note-1", "error": "sent"},
        ]);
        let mut out = Vec::new();
        monitor.commands(&commands, &mut out);
        assert_eq!(fires(&out), ["command_failed:5"]);
        let Event::Fire {
            subject,
            evidence,
            notice,
            ..
        } = &out[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "command #5");
        assert!(notice);
        assert_eq!(evidence["error"], "no execution");
        assert_eq!(evidence["principal"], "h");
    }

    #[test]
    fn a_crash_line_is_parsed_with_utc_and_offset_stamps() {
        let base = days_from_civil(2026, 11, 8) * 86_400_000 + (18 * 3600 + 25 * 60) * 1000 + 123;
        let now = base + 5_000;
        let (stamp, prefix) = crash_stamp(
            "2026-11-08 18:25:00.123 UTC [1] LOG:  server process (PID 4242) was \
terminated by signal 11: Segmentation fault",
            now,
        )
        .expect("a crash stamp");
        assert_eq!(stamp, base);
        assert_eq!(prefix, "2026-11-08T18:25:00.123UTC[1]");
        // +08:00 is eight hours ahead of UTC.
        let (stamp, prefix) = crash_stamp(
            "2026-11-09 02:25:00.123 +08:00 [77] ERROR:  could not resize shared memory \
segment \"/PostgreSQL.2051\" to 8388608 bytes: No space left on device",
            now,
        )
        .expect("an offset stamp");
        assert_eq!(stamp, base);
        assert_eq!(prefix, "2026-11-09T02:25:00.123+08:00[77]");
        // The third pattern, and a line that is not one at all.
        assert!(
            crash_stamp(
                "2026-11-08 18:25:00.123 UTC [3] FATAL:  No space left on device",
                now
            )
            .is_some()
        );
        assert!(
            crash_stamp(
                "2026-11-08 18:25:00.123 UTC [3] LOG:  checkpoint complete",
                now
            )
            .is_none()
        );
        assert!(crash_stamp("not a log line", now).is_none());
        // Older than the day: history, not a reading.
        assert!(
            crash_stamp(
                "2026-11-06 18:25:00.123 UTC [1] LOG:  terminated by signal 11",
                now
            )
            .is_none()
        );
    }

    #[test]
    fn a_crash_clears_at_the_ready_line_or_by_quiet() {
        let mut monitor = monitor();
        let now = 1_000_000;
        let crash = "2026-11-08 18:25:00.123 UTC [1] LOG:  server process (PID 4242) was \
terminated by signal 11: Segmentation fault\n";
        let mut out = Vec::new();
        monitor.lines_of_log(0, Path::new("/var/log/pg.log"), now, crash, &mut out);
        assert_eq!(
            fires(&out),
            ["postgres_crash:test:2026-11-08T18:25:00.123UTC[1]"]
        );
        let recorded = decided(&monitor, out);
        commit(&mut monitor, &recorded);

        // The same lines read again: nothing fires twice.
        let mut again = Vec::new();
        monitor.lines_of_log(0, Path::new("/var/log/pg.log"), now, crash, &mut again);
        assert!(decided(&monitor, again).is_empty());

        // Quiet: no ready line for the bound clears it, with the last line.
        monitor.logs[0].last_match_ms = now;
        let mut quiet = Vec::new();
        monitor.quiet_crashes(now + settings().quiet_ms + 1, &mut quiet);
        assert_eq!(quiet.len(), 1);
        assert!(matches!(&quiet[0], Event::Clear { evidence, .. }
            if evidence["quiet_secs"] == 900));

        // A ready line clears it at once, quoting the line.
        let ready = "2026-11-08 18:26:00.000 UTC [1] LOG:  database system is ready to accept \
connections\n";
        let mut cleared = Vec::new();
        monitor.lines_of_log(0, Path::new("/var/log/pg.log"), now, ready, &mut cleared);
        assert_eq!(cleared.len(), 1);
        assert!(matches!(&cleared[0], Event::Clear { evidence, .. }
            if evidence["line"].as_str().is_some_and(|line| line.contains("ready to accept"))));
    }

    #[test]
    fn backup_ages_fire_without_a_record_not_at_all_and_again_after_the_new_one_ages() {
        let mut monitor = monitor();
        let mut out = Vec::new();
        monitor.bounds(10_000_000, &mut out);
        assert!(out.is_empty(), "no backup record at all is no alert");

        let dumped = feed(
            &mut monitor,
            20,
            1000,
            json!({"type": "backup.dumped", "keys": ["postgres/hourly/a.dump"],
                   "bytes": 1, "events": 2}),
        );
        assert!(dumped.is_empty(), "a fresh dump is no alert");
        let mut early = Vec::new();
        monitor.bounds(1000 + 7_199_999, &mut early);
        assert!(early.is_empty(), "never before its age");

        let mut due = Vec::new();
        monitor.bounds(1000 + 7_200_000, &mut due);
        assert_eq!(fires(&due), ["backup_stale:dump:20"]);
        let Event::Fire {
            evidence, subject, ..
        } = &due[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "dump");
        assert_eq!(evidence["age_secs"], 7200);
        assert_eq!(evidence["max_age_secs"], 7200);
        assert_eq!(evidence["key"], "postgres/hourly/a.dump");
        let recorded = decided(&monitor, due);
        commit(&mut monitor, &recorded);

        let newer = feed(
            &mut monitor,
            21,
            1000 + 7_200_000,
            json!({"type": "backup.dumped", "keys": ["postgres/hourly/b.dump"], "bytes": 1}),
        );
        let cleared = decided(&monitor, newer);
        assert_eq!(cleared.len(), 1);
        assert!(matches!(&cleared[0], Event::Clear { key, evidence }
            if key == "backup_stale:dump:20" && evidence["event_id"] == 21));
        commit(&mut monitor, &cleared);

        // The newer dump growing old is a new occurrence with its own key.
        let mut later = Vec::new();
        monitor.bounds(1000 + 2 * 7_200_000, &mut later);
        assert_eq!(fires(&later), ["backup_stale:dump:21"]);
    }

    #[test]
    fn landed_objectives_unverified_over_the_bound_alert_once_and_clear_when_verified() {
        let mut monitor = monitor();
        let since = crate::verify::LISTED_SINCE_MS;
        let now = since + 3 * super::DAY_MS;
        let reading = |unverified: i64, old: i64, newest: Option<i64>| json!({"unverified": unverified, "old": old, "newest_old_ms": newest});
        let read = |monitor: &mut Monitor, at: i64, value: Value| {
            let mut out = Vec::new();
            monitor.unverified(at, &value, &mut out);
            let out = decided(monitor, out);
            commit(monitor, &out);
            out
        };
        assert!(
            read(&mut monitor, now, reading(3, 0, None)).is_empty(),
            "none old: nothing"
        );
        let landed = now - super::DAY_MS - 5000;
        let fired = read(&mut monitor, now + 2000, reading(3, 2, Some(landed)));
        let key = format!("unverified_landings:{landed}:{}", now + 2000);
        assert_eq!(fires(&fired), [key.as_str()]);
        let Event::Fire {
            kind,
            subject,
            text,
            evidence,
            notice,
            ..
        } = &fired[0]
        else {
            panic!("a fire");
        };
        assert_eq!(*kind, "unverified_landings");
        assert_eq!(subject, "objectives");
        assert!(!notice);
        assert_eq!(
            text,
            "2 landed objectives unverified for over a day: spawn verify list names them"
        );
        assert_eq!(
            evidence,
            &json!({"old": 2, "unverified": 3, "unverified_ms": super::DAY_MS,
                    "newest_landed_ms": landed, "listed_since_ms": since,
                    "reading_ms": now + 2000})
        );
        // More readings, more grown old: nothing more while one is open.
        assert!(read(&mut monitor, now + 4000, reading(3, 2, Some(landed))).is_empty());
        assert!(read(&mut monitor, now + 6000, reading(4, 3, Some(landed + 1))).is_empty());
        // None old: cleared, with what the reading saw.
        let cleared = read(&mut monitor, now + 8000, reading(1, 0, None));
        assert!(
            matches!(&cleared[..], [Event::Clear { key: cleared, evidence }]
                if *cleared == key
                    && *evidence == json!({"old": 0, "unverified": 1, "reading_ms": now + 8000})),
            "{cleared:?}"
        );
        assert!(read(&mut monitor, now + 10_000, reading(1, 0, None)).is_empty());
        // A later one is a new occurrence, in the singular.
        let again = read(&mut monitor, now + 12_000, reading(1, 1, Some(landed)));
        assert_eq!(
            fires(&again),
            [format!("unverified_landings:{landed}:{}", now + 12_000).as_str()]
        );
        let Event::Fire { text, .. } = &again[0] else {
            panic!("a fire");
        };
        assert_eq!(
            text,
            "1 landed objective unverified for over a day: spawn verify list names it"
        );
    }

    #[test]
    fn another_unverified_bound_says_itself_and_zero_reads_nothing() {
        assert_eq!(monitor().unverified_old_at(100_000_000), Some(13_600_000));
        let off = Monitor::with(Settings {
            unverified_ms: 0,
            ..settings()
        });
        assert_eq!(off.unverified_old_at(100_000_000), None, "0: nothing read");
        let mut monitor = Monitor::with(Settings {
            unverified_ms: 10_000,
            ..settings()
        });
        let mut out = Vec::new();
        monitor.unverified(
            5_000_000,
            &json!({"unverified": 1, "old": 1, "newest_old_ms": 4_000_000}),
            &mut out,
        );
        let Some(Event::Fire { text, evidence, .. }) = out.first() else {
            panic!("a fire: {out:?}");
        };
        assert_eq!(
            text,
            "1 landed objective unverified for over 10 s: spawn verify list names it"
        );
        assert_eq!(evidence["unverified_ms"], 10_000);
    }

    #[test]
    fn a_missions_daily_budget_alerts_once_a_day_and_clears_with_the_day_or_the_budget() {
        let mut monitor = monitor();
        // 2026-11-10 12:00 UTC.
        let noon = 1_794_312_000_000_i64;
        let reading = |spent: f64, budget: f64| {
            json!([{"mission": 4, "budget_usd": budget, "spent_usd": spent},
                   {"mission": 5, "budget_usd": 1.0, "spent_usd": 0.06}])
        };
        let read = |monitor: &mut Monitor, now: i64, value: Value| {
            let mut out = Vec::new();
            monitor.budgets(now, &value, &mut out);
            let out = decided(monitor, out);
            commit(monitor, &out);
            out
        };
        assert!(
            read(&mut monitor, noon, reading(0.4855, 0.5)).is_empty(),
            "under: none"
        );
        let fired = read(&mut monitor, noon + 2000, reading(0.9355, 0.5));
        assert_eq!(fires(&fired), ["cost_budget:4:2026-11-10"]);
        let Event::Fire {
            subject,
            text,
            evidence,
            ..
        } = &fired[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "mission #4");
        assert_eq!(
            text,
            "mission #4 spent $0.94 today (UTC), over its $0.50 daily budget"
        );
        assert_eq!(evidence["day"], "2026-11-10");
        // More spending the same day: nothing more.
        assert!(read(&mut monitor, noon + 4000, reading(1.2355, 0.5)).is_empty());
        // The budget raised above the spend clears it, and the day's key is
        // spent: going over again that day fires nothing.
        let raised = read(&mut monitor, noon + 6000, reading(1.2355, 2.0));
        assert!(
            matches!(&raised[..], [Event::Clear { key, .. }] if key == "cost_budget:4:2026-11-10")
        );
        assert!(read(&mut monitor, noon + 8000, reading(2.5, 2.0)).is_empty());
        // The next UTC day is a new occurrence.
        let tomorrow = noon + super::DAY_MS;
        let next = read(&mut monitor, tomorrow, reading(0.6, 0.5));
        assert_eq!(fires(&next), ["cost_budget:4:2026-11-11"]);
        // Its day over: cleared at the next day's first reading.
        let after = read(&mut monitor, tomorrow + super::DAY_MS, reading(0.1, 0.5));
        assert!(
            matches!(&after[..], [Event::Clear { key, .. }] if key == "cost_budget:4:2026-11-11")
        );
        // A budget removed clears its open alert.
        let third = tomorrow + super::DAY_MS + 1000;
        assert_eq!(
            fires(&read(&mut monitor, third, reading(0.7, 0.5))),
            ["cost_budget:4:2026-11-12"]
        );
        let removed = read(&mut monitor, third + 1000, json!([]));
        assert!(
            matches!(&removed[..], [Event::Clear { key, .. }] if key == "cost_budget:4:2026-11-12")
        );
    }

    #[test]
    fn a_budget_over_alert_fires_once_and_clears_under_it() {
        let mut monitor = monitor();
        let over = feed(
            &mut monitor,
            30,
            1000,
            json!({"type": "backup.dumped", "keys": ["k1"], "over_alert": true,
                   "held_bytes": 8_100_000_000_i64, "written_bytes": 2_400_100_i64,
                   "alert_bytes": 8_000_000_000_i64, "limit_bytes": 10_000_000_000_i64,
                   "skipped": []}),
        );
        assert_eq!(fires(&over), ["backup_budget:30"]);
        let Event::Fire {
            subject, evidence, ..
        } = &over[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "r2");
        assert_eq!(evidence["held_bytes"], 8_100_000_000_i64);
        assert_eq!(evidence["alert_bytes"], 8_000_000_000_i64);
        let recorded = decided(&monitor, over);
        commit(&mut monitor, &recorded);

        let still = feed(
            &mut monitor,
            31,
            2000,
            json!({"type": "backup.dumped", "keys": ["k2"], "over_alert": true,
                   "held_bytes": 8_200_000_000_i64}),
        );
        assert!(
            !fires(&still)
                .iter()
                .any(|key| key.starts_with("backup_budget:")),
            "one alert while it is open"
        );

        let under = feed(
            &mut monitor,
            32,
            3000,
            json!({"type": "backup.dumped", "keys": ["k3"], "over_alert": false,
                   "held_bytes": 7_900_000_000_i64}),
        );
        let cleared = decided(&monitor, under);
        assert!(cleared.iter().any(|event| matches!(event,
            Event::Clear { key, evidence } if key == "backup_budget:30"
                && evidence["event_id"] == 32)));
    }

    #[test]
    fn a_base_disagreement_is_a_notice_only_when_its_record_says_alert() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            39,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let record = json!({"type": "task.gated", "id": 7, "verdict": "host_fault",
                            "evidence": "base_disagrees", "alert": true, "revision": 0,
                            "scenario": "011.sh", "exits": [1, 0], "setup_s": 244,
                            "base_disagrees": 3, "due_ms": 5});
        let events = feed(&mut monitor, 40, 0, record.clone());
        assert!(fires(&events).contains(&"gate_base_disagrees:40"));
        let quiet = feed(
            &mut monitor,
            41,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "host_fault",
                   "evidence": "base_disagrees", "revision": 0}),
        );
        assert!(
            !fires(&quiet)
                .iter()
                .any(|key| key.starts_with("gate_base_disagrees:")),
            "no alert without the record's own flag"
        );
    }

    #[test]
    fn an_admission_fires_after_its_bound_and_clears_at_its_admission() {
        let mut monitor = monitor();
        let waiting = feed(
            &mut monitor,
            50,
            1000,
            json!({"type": "admission.waiting", "kind": "execution",
                   "agent_id": "agent_worker_bb_wait", "session_id": "s",
                   "envelope_bytes": 6_442_450_944_i64, "measure": "host",
                   "measured_bytes": 2_147_483_648_i64,
                   "threshold_bytes": 6_442_450_944_i64}),
        );
        assert!(waiting.is_empty(), "a wait is not yet an alert");
        let mut early = Vec::new();
        monitor.bounds(1000 + 599_999, &mut early);
        assert!(early.is_empty(), "never before its bound");
        let mut due = Vec::new();
        monitor.bounds(1000 + 600_000, &mut due);
        assert_eq!(fires(&due), ["admission_wait:50"]);
        let Event::Fire {
            subject, evidence, ..
        } = &due[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "agent_worker_bb_wait");
        assert_eq!(evidence["measure"], "host");
        let recorded = decided(&monitor, due);
        commit(&mut monitor, &recorded);

        let admitted = feed(
            &mut monitor,
            51,
            1000 + 610_000,
            json!({"type": "admission.admitted", "kind": "execution",
                   "agent_id": "agent_worker_bb_wait", "waited_ms": 610_000}),
        );
        let cleared = decided(&monitor, admitted);
        assert_eq!(cleared.len(), 1);
        assert!(matches!(&cleared[0], Event::Clear { key, evidence }
            if key == "admission_wait:50" && evidence["waited_ms"] == 610_000));
    }

    #[test]
    fn a_stuck_judgement_fires_once_and_a_calm_one_clears_it() {
        let mut monitor = monitor();
        let stuck = json!([{"session_id": "session_resident_r1", "agent_id": "agent_resident_r1",
                            "event_id": 60, "at_ms": 0,
                            "observation": {"stuck": 0.9, "state": "WAITING", "judge": "jev"}}]);
        let mut out = Vec::new();
        monitor.observe(&stuck, &mut out);
        assert_eq!(fires(&out), ["agent_stuck:session_resident_r1:60"]);
        let Event::Fire { evidence, .. } = &out[0] else {
            panic!("a fire");
        };
        assert_eq!(evidence["stuck"], 0.9);
        assert!(evidence["stuck"].as_f64().unwrap_or_default() >= LIKELY);
        let recorded = decided(&monitor, out);
        commit(&mut monitor, &recorded);

        let still = json!([{"session_id": "session_resident_r1", "agent_id": "agent_resident_r1",
                            "event_id": 61, "at_ms": 0,
                            "observation": {"stuck": 0.8, "state": "WAITING", "judge": "jev"}}]);
        let mut again = Vec::new();
        monitor.observe(&still, &mut again);
        assert!(
            decided(&monitor, again).is_empty(),
            "a later stuck fires nothing"
        );

        let calm = json!([{"session_id": "session_resident_r1", "agent_id": "agent_resident_r1",
                           "event_id": 62, "at_ms": 0,
                           "observation": {"stuck": 0.05, "state": "WAITING", "judge": "jev"}}]);
        let mut cleared = Vec::new();
        monitor.observe(&calm, &mut cleared);
        let cleared = decided(&monitor, cleared);
        assert!(matches!(&cleared[0], Event::Clear { evidence, .. }
            if evidence["stuck"] == 0.05 && evidence["event_id"] == 62));
    }

    /// One mission (`accept`: `gate` or `review`) and its task #7 with
    /// `agent_worker_w`, moved by `records` (each a control record, its
    /// event id 10, 11, ...).
    fn worker_monitor(accept: &str, records: &[Value]) -> Monitor {
        let mut monitor = monitor();
        let mut all = vec![
            (
                1,
                json!({"type": "mission.added", "id": 4, "text": "m", "origin": "human"}),
            ),
            (
                2,
                json!({"type": "mission.configured", "id": 4, "accept": accept}),
            ),
            (
                10,
                json!({"type": "task.assigned", "id": 7, "mission_id": 4,
                       "agent_id": "agent_worker_w", "objective": 26}),
            ),
            (
                11,
                json!({"type": "task.delivered", "id": 7, "command_id": 3}),
            ),
        ];
        all.extend((12..).zip(records.iter().cloned()));
        for (id, record) in all {
            let events = feed(&mut monitor, id, id * 1000, record);
            commit(&mut monitor, &events);
        }
        monitor
    }

    /// A judgement of `session` (`agent`) at event 100, stuck 0.9, WAITING.
    fn stuck_of(session: &str, agent: &str) -> Value {
        json!([{"session_id": session, "agent_id": agent, "event_id": 100, "at_ms": 0,
                "observation": {"stuck": 0.9, "state": "WAITING", "judge": "jev"}}])
    }

    fn observed(monitor: &mut Monitor, observations: &Value) -> Vec<Event> {
        let mut out = Vec::new();
        monitor.observe(observations, &mut out);
        decided(monitor, out)
    }

    /// Objective 057 (a): a worker whose task is past its part fires
    /// nothing, whatever the judge's score: reported (waiting for its gate,
    /// or for a review), gated stale (reported again), accepted by its
    /// review (waiting for its landing), landing after an accepted gate (or
    /// held behind the owner's hold: `task.held` leaves it landing), landed.
    #[test]
    fn a_worker_whose_part_is_done_is_never_alerted_stuck() {
        let reported = json!({"type": "task.reported", "id": 7, "commit": "c"});
        let cases: Vec<(&str, &str, Vec<Value>)> = vec![
            (
                "reported, waiting for its gate",
                "gate",
                vec![reported.clone()],
            ),
            (
                "reported, waiting for a review",
                "review",
                vec![reported.clone()],
            ),
            (
                "gated stale: reported again",
                "gate",
                vec![
                    reported.clone(),
                    json!({"type": "task.gated", "id": 7, "verdict": "stale"}),
                ],
            ),
            (
                "gated accepted: landing",
                "gate",
                vec![
                    reported.clone(),
                    json!({"type": "task.gated", "id": 7, "verdict": "accepted"}),
                ],
            ),
            (
                "landing, held behind the owner's hold",
                "gate",
                vec![
                    reported.clone(),
                    json!({"type": "task.gated", "id": 7, "verdict": "accepted"}),
                    json!({"type": "task.held", "id": 7, "hold": 2, "source": "/s"}),
                ],
            ),
            (
                "reviewed accepted",
                "review",
                vec![
                    reported.clone(),
                    json!({"type": "task.reviewed", "id": 7, "accepted": true}),
                ],
            ),
            (
                "landed",
                "gate",
                vec![
                    reported.clone(),
                    json!({"type": "task.gated", "id": 7, "verdict": "accepted"}),
                    json!({"type": "task.landed", "id": 7}),
                ],
            ),
        ];
        for (what, accept, records) in cases {
            let mut monitor = worker_monitor(accept, &records);
            let out = observed(
                &mut monitor,
                &stuck_of("session_worker_w", "agent_worker_w"),
            );
            assert!(out.is_empty(), "{what}: {out:?}");
        }
    }

    /// Objective 057 (b): a worker stuck in its own work (its task
    /// delivered) fires once, naming its task and the one command that acts
    /// on it; one whose task went back for revision names its task, and no
    /// command (a restart takes a delivered task only).
    #[test]
    fn a_worker_stuck_in_its_task_names_the_task_and_its_command() {
        let mut monitor = worker_monitor("gate", &[]);
        let out = observed(
            &mut monitor,
            &stuck_of("session_worker_w", "agent_worker_w"),
        );
        assert_eq!(fires(&out), ["agent_stuck:session_worker_w:100"]);
        let Event::Fire { evidence, text, .. } = &out[0] else {
            panic!("a fire");
        };
        assert_eq!(evidence["task"], 7);
        assert_eq!(evidence["task_state"], "delivered");
        assert_eq!(evidence["command"], "spawn task restart --id 7");
        assert_eq!(evidence["event_id"], 100);
        assert!(text.contains("spawn task restart --id 7"), "{text}");
        commit(&mut monitor, &out);
        let again = observed(
            &mut monitor,
            &stuck_of("session_worker_w", "agent_worker_w"),
        );
        assert!(again.is_empty(), "one alert: {again:?}");

        let mut revising = worker_monitor(
            "gate",
            &[
                json!({"type": "task.reported", "id": 7, "commit": "c"}),
                json!({"type": "task.gated", "id": 7, "verdict": "rejected", "revision": 0}),
            ],
        );
        let out = observed(
            &mut revising,
            &stuck_of("session_worker_w", "agent_worker_w"),
        );
        let Event::Fire { evidence, .. } = &out[0] else {
            panic!("a fire: {out:?}");
        };
        assert_eq!(evidence["task"], 7);
        assert_eq!(evidence["task_state"], "revise");
        assert_eq!(evidence["command"], json!(null));
    }

    /// Objective 057 (a): an open stuck alert of a worker clears at the
    /// record that moved its task past its part, that record's id its
    /// `event_id`: its `task.reported`, and as well a `task.gated` or a
    /// `task.landed` for one still open.
    #[test]
    fn a_workers_stuck_alert_clears_at_its_tasks_report() {
        for (records, clearing) in [
            (
                vec![json!({"type": "task.reported", "id": 7, "commit": "c"})],
                "task.reported",
            ),
            (
                vec![
                    json!({"type": "task.reported", "id": 7, "commit": "c"}),
                    json!({"type": "task.gated", "id": 7, "verdict": "stale"}),
                ],
                "task.reported",
            ),
        ] {
            let mut monitor = worker_monitor("gate", &[]);
            let fired = observed(
                &mut monitor,
                &stuck_of("session_worker_w", "agent_worker_w"),
            );
            commit(&mut monitor, &fired);
            let mut cleared = Vec::new();
            for (offset, record) in records.iter().enumerate() {
                let id = 200 + i64::try_from(offset).unwrap_or_default();
                let events = feed(&mut monitor, id, 0, record.clone());
                let events = decided(&monitor, events);
                commit(&mut monitor, &events);
                cleared.extend(
                    events
                        .into_iter()
                        .filter(|event| matches!(event, Event::Clear { .. })),
                );
            }
            assert_eq!(cleared.len(), 1, "{cleared:?}");
            assert!(matches!(&cleared[0], Event::Clear { key, evidence }
                if key == "agent_stuck:session_worker_w:100" && evidence["event_id"] == 200
                    && evidence["record"] == clearing && evidence["task"] == 7));
            let after = observed(
                &mut monitor,
                &stuck_of("session_worker_w", "agent_worker_w"),
            );
            assert!(after.is_empty(), "nothing fires again: {after:?}");
        }
        // Landed by its gate's landing: cleared at the gated accepted.
        let mut monitor = worker_monitor(
            "gate",
            &[json!({"type": "task.reported", "id": 7, "commit": "c"})],
        );
        monitor.known.insert(
            "agent_stuck:session_worker_w:100".to_string(),
            Row {
                kind: "agent_stuck".to_string(),
                subject: "session_worker_w".to_string(),
                state: "open".to_string(),
                evidence: json!({"agent_id": "agent_worker_w", "event_id": 100}),
                fired_event_id: 101,
                fired_at_ms: 0,
            },
        );
        let events = feed(
            &mut monitor,
            300,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "accepted"}),
        );
        let cleared: Vec<Event> = decided(&monitor, events)
            .into_iter()
            .filter(|event| matches!(event, Event::Clear { .. }))
            .collect();
        assert!(matches!(&cleared[..], [Event::Clear { evidence, .. }]
            if evidence["event_id"] == 300 && evidence["record"] == "task.gated"
                && evidence["task_state"] == "landing"));
    }

    /// A resident outlives its tasks: one whose task is reported is still
    /// alerted stuck (it takes other work), naming no task; its task's
    /// report clears nothing.
    #[test]
    fn a_resident_whose_task_is_reported_is_still_alerted() {
        let mut monitor = monitor();
        for (id, record) in [
            (
                1,
                json!({"type": "mission.added", "id": 4, "text": "m", "origin": "human"}),
            ),
            (
                10,
                json!({"type": "task.assigned", "id": 8, "mission_id": 4,
                       "agent_id": "agent_resident_r1", "objective": null}),
            ),
            (
                11,
                json!({"type": "task.delivered", "id": 8, "command_id": 3}),
            ),
        ] {
            let events = feed(&mut monitor, id, 0, record);
            commit(&mut monitor, &events);
        }
        let out = observed(
            &mut monitor,
            &stuck_of("session_resident_r1", "agent_resident_r1"),
        );
        assert_eq!(fires(&out), ["agent_stuck:session_resident_r1:100"]);
        commit(&mut monitor, &out);
        let reported = feed(
            &mut monitor,
            120,
            0,
            json!({"type": "task.reported", "id": 8, "commit": "c"}),
        );
        assert!(
            !reported
                .iter()
                .any(|event| matches!(event, Event::Clear { .. })),
            "{reported:?}"
        );
        commit(&mut monitor, &reported);
        monitor.known.clear();
        let out = observed(
            &mut monitor,
            &stuck_of("session_resident_r1", "agent_resident_r1"),
        );
        let Event::Fire { evidence, .. } = &out[0] else {
            panic!("a fire: {out:?}");
        };
        assert_eq!(evidence["task"], json!(null));
        assert_eq!(evidence["command"], json!(null));
    }

    fn supervisor_stuck() -> Value {
        stuck_of("session_supervisor", "agent_supervisor")
    }

    fn item(tag: &str, agent: Option<&str>, judgement: bool) -> super::DispatchItem {
        super::DispatchItem {
            tag: tag.to_string(),
            agent: agent.map(str::to_string),
            judgement,
        }
    }

    /// Objective 057 (a): the supervisor with nothing to dispatch fires
    /// nothing, whatever the judge's score; each of its three conditions
    /// fires one: a question to the owner, a pending or told escalation
    /// (a worker's own judgement excepted while it holds its task), a
    /// mission waiting for its first task. Its plan unread: its judgement
    /// alone, as before.
    #[test]
    fn the_supervisor_is_alerted_stuck_only_with_something_to_dispatch() {
        // Its one mission's task with a worker: nothing to dispatch.
        let mut monitor = worker_monitor("gate", &[]);
        monitor.supervisor = Some(super::Dispatch::default());
        assert!(observed(&mut monitor, &supervisor_stuck()).is_empty());
        // A worker's judgement about its task is the worker's (its own alert).
        monitor.supervisor = Some(super::Dispatch {
            question: false,
            items: vec![item("session_worker_w 99", Some("agent_worker_w"), true)],
        });
        assert!(observed(&mut monitor, &supervisor_stuck()).is_empty());

        let fired = |dispatch: super::Dispatch, monitor: &mut Monitor| {
            monitor.supervisor = Some(dispatch);
            let out = observed(monitor, &supervisor_stuck());
            assert_eq!(fires(&out), ["agent_stuck:session_supervisor:100"]);
            let Event::Fire { evidence, .. } = &out[0] else {
                panic!("a fire");
            };
            evidence["dispatch"].clone()
        };
        // 1. A question to the owner.
        let mut asking = worker_monitor("gate", &[]);
        let why = fired(
            super::Dispatch {
                question: true,
                items: Vec::new(),
            },
            &mut asking,
        );
        assert_eq!(why, json!(["a question to the owner"]));
        // 2. A pending or told escalation: a report to review, a stall, a
        // worker's judgement while it holds no task.
        for escalation in [
            item("session_worker_w report-7-0", Some("agent_worker_w"), false),
            item("missions stall-7-halted-3", None, false),
            item("session_worker_x 98", Some("agent_worker_x"), true),
        ] {
            let mut escalated = worker_monitor("gate", &[]);
            let tag = escalation.tag.clone();
            let why = fired(
                super::Dispatch {
                    question: false,
                    items: vec![escalation],
                },
                &mut escalated,
            );
            assert_eq!(why, json!([format!("escalation {tag}")]));
        }
        // 3. A mission of its own waiting for its first task (a gated
        // mission's objectives are the integrator's).
        let mut waiting = worker_monitor("gate", &[]);
        let mut events = Vec::new();
        waiting.feed(
            50,
            0,
            &json!({"type": "mission.added", "id": 5, "text": "n", "origin": "human"}),
            &mut events,
        );
        waiting.feed(
            51,
            0,
            &json!({"type": "mission.added", "id": 6, "text": "g", "origin": "human"}),
            &mut events,
        );
        waiting.feed(
            52,
            0,
            &json!({"type": "mission.configured", "id": 6, "accept": "gate"}),
            &mut events,
        );
        let why = fired(super::Dispatch::default(), &mut waiting);
        assert_eq!(why, json!(["mission #5 waits for its first task"]));
        // Unread: the judgement alone.
        let mut unread = worker_monitor("gate", &[]);
        unread.supervisor = None;
        assert_eq!(
            fires(&observed(&mut unread, &supervisor_stuck())),
            ["agent_stuck:session_supervisor:100"]
        );
    }

    /// Objective 057 (a): the supervisor's open stuck alert clears once it
    /// has nothing to dispatch, at the latest record the step read.
    #[test]
    fn the_supervisors_stuck_alert_clears_once_nothing_is_to_dispatch() {
        let mut monitor = worker_monitor("gate", &[]);
        monitor.supervisor = Some(super::Dispatch {
            question: true,
            items: Vec::new(),
        });
        let fired = observed(&mut monitor, &supervisor_stuck());
        commit(&mut monitor, &fired);
        monitor.supervisor = Some(super::Dispatch::default());
        monitor.watermark = 120;
        let cleared = observed(&mut monitor, &supervisor_stuck());
        assert!(matches!(&cleared[..], [Event::Clear { key, evidence }]
            if key == "agent_stuck:session_supervisor:100" && evidence["event_id"] == 120
                && evidence["nothing_to_dispatch"] == true));
    }

    /// `owner_request_waiting` (S23): an owner's landing request with no
    /// outcome fires once at SPAWN_ALERT_OWNER_REQUEST_SECS after it was
    /// recorded, never before, naming the withdrawal that ends it; the step
    /// plans its pass for that time; its outcome (a `source.moved` naming
    /// it) clears it; one refused before its bound fires nothing; a
    /// `source.push_no_verdict` is no outcome; 0 disables the check.
    #[test]
    fn an_owner_request_with_no_outcome_for_its_bound_fires_once_and_its_outcome_clears_it() {
        let mut monitor = monitor();
        let source = "/live/missions/spawn.git";
        let commit_a = "a".repeat(40);
        for (event_id, record) in [
            (
                10,
                json!({"type": "source.push_requested", "source": source, "commit": commit_a,
                       "ref": format!("refs/spawn-owner/{commit_a}")}),
            ),
            (
                11,
                json!({"type": "source.push_requested", "source": source, "commit": "b".repeat(40),
                       "ref": "refs/spawn-owner/b"}),
            ),
            (
                12,
                json!({"type": "source.push_refused", "request": 11, "reason": "a conflict"}),
            ),
            (
                13,
                json!({"type": "source.push_no_verdict", "request": 10, "reason": "host fault"}),
            ),
        ] {
            assert!(feed(&mut monitor, event_id, 1000, record).is_empty());
        }
        monitor.plan(1000);
        assert_eq!(
            monitor.next_bound_ms,
            Some(1000 + 3_600_000),
            "planned at its bound"
        );
        let mut early = Vec::new();
        monitor.bounds(1000 + 3_599_999, &mut early);
        assert!(early.is_empty(), "never before its bound: {early:?}");
        let mut due = Vec::new();
        monitor.bounds(1000 + 3_600_000, &mut due);
        assert_eq!(
            fires(&due),
            ["owner_request_waiting:10"],
            "the refused one fires nothing"
        );
        let Event::Fire {
            subject,
            evidence,
            text,
            ..
        } = &due[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, source);
        assert_eq!(evidence["request"], 10);
        assert_eq!(evidence["commit"], commit_a.as_str());
        assert_eq!(evidence["event_id"], 10);
        assert_eq!(evidence["waited_ms"], 3_600_000);
        assert_eq!(evidence["withdraw"], "spawn mission push --withdraw 10");
        assert!(
            text.contains(
                "request #10 (aaaaaaaaaaaa) on /live/missions/spawn.git has had no outcome for 1 h"
            ) && text.contains("`spawn mission push --withdraw 10`"),
            "{text}"
        );
        let recorded = decided(&monitor, due);
        commit(&mut monitor, &recorded);
        let mut again = Vec::new();
        monitor.bounds(1000 + 7_200_000, &mut again);
        assert!(decided(&monitor, again).is_empty(), "once per request");
        monitor.plan(1000 + 7_200_000);
        assert_eq!(monitor.next_bound_ms, None, "nothing left to plan");

        let moved = feed(
            &mut monitor,
            20,
            9000,
            json!({"type": "source.moved", "source": source, "from": "c".repeat(40),
                   "to": commit_a, "request": 10, "by": "human"}),
        );
        let cleared = decided(&monitor, moved);
        assert!(
            matches!(&cleared[..], [Event::Clear { key, evidence }]
                if key == "owner_request_waiting:10" && evidence["event_id"] == 20
                    && evidence["record"] == "source.moved"),
            "{cleared:?}"
        );
        assert!(monitor.owner_requests.is_empty());

        let mut off = Monitor::with(Settings {
            owner_request_ms: 0,
            ..settings()
        });
        feed(
            &mut off,
            10,
            1000,
            json!({"type": "source.push_requested", "source": source, "commit": commit_a}),
        );
        let mut none = Vec::new();
        off.bounds(i64::MAX / 2, &mut none);
        assert!(none.is_empty(), "0 disables it");
    }

    /// The stuck alert against the real store: the session's events are
    /// appended to a schema of its own in the test database
    /// (`SPAWN_TEST_DATABASE_URL`, never the live one) and read back by
    /// `alert_observations`, as the loop's step reads them.
    /// An alert clears only on evidence after what it fired on, whatever
    /// restarts (objective 050): the step meets a history with a halt, its
    /// resume and a second halt, two dumps and a good then a failed drill.
    mod restarts {
        use super::super::Monitor;
        use super::settings;
        use serde_json::Value;
        use spawn_postgres::PostgresStore;

        const HOUR: i64 = 3_600_000;

        /// Run `case` on a migrated schema of its own, with an owner
        /// connection to it; dropped afterwards.
        fn in_schema(name: &str, case: impl FnOnce(&PostgresStore, &mut postgres::Client)) {
            let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
                "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
            );
            let schema = spawn_postgres::testing::TestSchema::new(name);
            let store = PostgresStore::connect_in_schema(&url, &schema)
                .expect("a schema of its own, migrated");
            let separator = if url.contains('?') { '&' } else { '?' };
            let owner = format!("{url}{separator}options=-csearch_path%3D{schema}");
            let mut client =
                postgres::Client::connect(&owner, postgres::NoTls).expect("the owner connects");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                case(&store, &mut client);
            }));
            drop(client);
            drop(store);
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
        }

        /// A control record `hours` before now, as the store holds it; its
        /// event id.
        fn record(client: &mut postgres::Client, hours: i64, record: &str) -> i64 {
            let record: Value = serde_json::from_str(record).expect("a record");
            client
                .query_one(
                    "insert into spawn_events (agent_id, session_id, execution_id, source,
                                               source_event_id, kind, session_seq, recorded_at_ms)
                     select 'agent_control', 'session_control', null, 'control', null,
                            jsonb_build_object('name', 'control.recorded',
                                               'detail', jsonb_build_object('record', $1::jsonb)),
                            coalesce((select max(session_seq) from spawn_events
                                       where session_id = 'session_control'), 0) + 1,
                            spawn_now_ms() - $2::bigint
                     returning id",
                    &[&record, &(hours * HOUR)],
                )
                .expect("a control record")
                .get(0)
        }

        fn refold(client: &mut postgres::Client) {
            client
                .batch_execute("select spawn_control_refold()")
                .expect("refold");
        }

        /// The alert `key`'s state and the event its clear named.
        fn alert(client: &mut postgres::Client, key: &str) -> (String, Option<i64>) {
            client
                .query_opt(
                    "select state, (cleared_evidence ->> 'event_id')::bigint
                       from spawn_alerts where key = $1",
                    &[&key],
                )
                .expect("the alert reads")
                .map_or(("none".to_string(), None), |row| (row.get(0), row.get(1)))
        }

        fn count(client: &mut postgres::Client) -> i64 {
            client
                .query_one(
                    "select count(*) from spawn_alerts
                      where kind in ('mission_halted', 'backup_stale', 'drill_failed')",
                    &[],
                )
                .expect("count")
                .get(0)
        }

        /// A loop's step, started fresh, after one check.
        fn started(store: &PostgresStore) -> Monitor {
            let mut monitor = Monitor::with(settings());
            monitor.check(store).expect("the step checks");
            monitor
        }

        /// `owner_request_waiting` on a store, across a restart (S23): two
        /// owner requests recorded two hours ago with no outcome fire on the
        /// first check (a third, refused, fires nothing); a `source.moved`
        /// naming one clears it at the next check; the other's refusal,
        /// recorded while the loop is down, clears it when a new step
        /// starts, naming the refusal; nothing fires twice. The alert's
        /// evidence goes through the store's `spawn_alert_record`.
        #[test]
        fn an_owner_request_waiting_fires_on_the_store_and_clears_at_its_outcome_across_a_restart()
        {
            in_schema("alerts_owner_request", |store, client| {
                let source = "/live/missions/spawn.git";
                let ask = |client: &mut postgres::Client, digit: char| {
                    let commit = digit.to_string().repeat(40);
                    record(
                        client,
                        2,
                        &format!(
                            r#"{{"type":"source.push_requested","source":"{source}",
                                "commit":"{commit}","ref":"refs/spawn-owner/{commit}"}}"#
                        ),
                    )
                };
                let a = ask(client, 'a');
                let c = ask(client, 'c');
                let refused = ask(client, 'e');
                record(
                    client,
                    2,
                    &format!(
                        r#"{{"type":"source.push_refused","request":{refused},"reason":"r"}}"#
                    ),
                );
                let mut monitor = started(store);
                let key_a = format!("owner_request_waiting:{a}");
                let key_c = format!("owner_request_waiting:{c}");
                assert_eq!(alert(client, &key_a).0, "open");
                assert_eq!(alert(client, &key_c).0, "open");
                assert_eq!(
                    alert(client, &format!("owner_request_waiting:{refused}")).0,
                    "none"
                );

                let moved = record(
                    client,
                    0,
                    &format!(
                        r#"{{"type":"source.moved","source":"{source}","from":"{}",
                            "to":"{}","request":{a},"by":"human"}}"#,
                        "0".repeat(40),
                        "a".repeat(40)
                    ),
                );
                monitor.check(store).expect("the step checks");
                assert_eq!(alert(client, &key_a), ("cleared".to_string(), Some(moved)));
                assert_eq!(alert(client, &key_c).0, "open");
                drop(monitor);

                let refusal = record(
                    client,
                    0,
                    &format!(
                        r#"{{"type":"source.push_refused","request":{c},"reason":"withdrawn by the owner"}}"#
                    ),
                );
                let _restarted = started(store);
                assert_eq!(
                    alert(client, &key_c),
                    ("cleared".to_string(), Some(refusal))
                );
                let fired: i64 = client
                    .query_one(
                        "select count(*) from spawn_alerts where kind = 'owner_request_waiting'",
                        &[],
                    )
                    .expect("count")
                    .get(0);
                assert_eq!(fired, 2, "once per request, whatever restarts");
            });
        }

        /// A rejection and an abort the store's missions and tasks already
        /// hold fold once (objective 106): the step goes on from the
        /// watermark its mirror read in the same statement, so its first
        /// check feeds none of the records the mirror holds again, and a
        /// record after the start is fed by the checks.
        #[test]
        fn a_rejection_and_an_abort_the_mirror_holds_fold_once() {
            in_schema("alerts_mirror_once", |store, client| {
                record(
                    client,
                    1,
                    r#"{"type":"mission.added","id":4,"text":"m","origin":"human"}"#,
                );
                record(
                    client,
                    1,
                    r#"{"type":"mission.configured","id":4,"accept":"gate","gate":"/g"}"#,
                );
                let assign = |client: &mut postgres::Client, task: i64| {
                    record(
                        client,
                        1,
                        &format!(
                            r#"{{"type":"task.assigned","id":{task},"mission_id":4,
                                "kind":"ephemeral","agent_id":"agent_worker_{task}",
                                "session_id":"session_{task}","brief":"b",
                                "created_by":"agent_supervisor","objective":1}}"#
                        ),
                    )
                };
                assign(client, 7);
                record(
                    client,
                    1,
                    r#"{"type":"task.gated","id":7,"verdict":"rejected","revision":0}"#,
                );
                assign(client, 8);
                let last = record(
                    client,
                    1,
                    r#"{"type":"task.aborted","id":8,"reason":"over"}"#,
                );
                refold(client);
                let gates = |client: &mut postgres::Client| -> (i32, i32, i32) {
                    let row = client
                        .query_one(
                            "select m.gate_rejections, m.gate_aborts, k.revisions
                               from mission_missions m join mission_tasks k on k.id = 7
                              where m.id = 4",
                            &[],
                        )
                        .expect("the fold reads");
                    (row.get(0), row.get(1), row.get(2))
                };
                assert_eq!(gates(client), (1, 1, 1), "the store holds them once");

                let mut monitor = started(store);
                assert!(monitor.watermark >= last, "the mirror's watermark");
                let mission = &monitor.missions[&4];
                assert_eq!((mission.rejections, mission.aborts), (1, 1));
                assert_eq!(monitor.tasks[&7].revisions, 1);
                monitor.check(store).expect("the step checks");
                let mission = &monitor.missions[&4];
                assert_eq!(
                    (mission.rejections, mission.aborts),
                    (1, 1),
                    "no check feeds a record the mirror holds again"
                );
                assert_eq!(monitor.tasks[&7].revisions, 1);

                assign(client, 9);
                record(
                    client,
                    0,
                    r#"{"type":"task.gated","id":9,"verdict":"aborted","revision":0}"#,
                );
                monitor.check(store).expect("the step checks");
                assert_eq!(
                    monitor.missions[&4].aborts, 2,
                    "a record after the start is the checks'"
                );
                assert!(monitor.tasks.contains_key(&9));
            });
        }

        /// `task_parked` across restarts (sync/design-load-sensitive-
        /// bound.md): a started step fires one for a task the store holds
        /// parked, and none for a task released since its parking. Released
        /// while the step is down, its alert clears at the next start
        /// (`not_parked`, the start's reading); a task parked while the step
        /// is down fires then, named by the record that parked it.
        #[test]
        fn a_started_step_says_each_parked_task_and_clears_a_released_one() {
            in_schema("alerts_task_parked", |store, client| {
                record(
                    client,
                    1,
                    r#"{"type":"mission.added","id":4,"text":"m","origin":"human"}"#,
                );
                record(
                    client,
                    1,
                    r#"{"type":"mission.configured","id":4,"accept":"gate","gate":"/g"}"#,
                );
                for task in [7, 8] {
                    record(
                        client,
                        1,
                        &format!(
                            r#"{{"type":"task.assigned","id":{task},"mission_id":4,
                                "kind":"ephemeral","agent_id":"agent_worker_{task}",
                                "session_id":"session_{task}","brief":"b",
                                "created_by":"agent_supervisor","objective":1}}"#
                        ),
                    );
                }
                let park = |client: &mut postgres::Client, task: i64| {
                    record(
                        client,
                        1,
                        &format!(
                            r#"{{"type":"task.gated","id":{task},"revision":0,
                                "verdict":"host_fault","evidence":"load_sensitive_parked",
                                "scenario":"099.sh","sensitive":["099.sh"],
                                "pushed":"{}","parked":true,"escalate":true,
                                "owners":[{{"scenario":"099.sh","owner":"rev-f"}}]}}"#,
                            "c".repeat(40)
                        ),
                    )
                };
                let release = |client: &mut postgres::Client, task: i64| {
                    record(
                        client,
                        0,
                        &format!(
                            r#"{{"type":"task.released","id":{task},"by":"ckl","reason":"r"}}"#
                        ),
                    )
                };
                let seven = park(client, 7);
                park(client, 8);
                release(client, 8);
                refold(client);
                let monitor = started(store);
                let key = format!("task_parked:7:{seven}");
                assert_eq!(alert(client, &key).0, "open");
                assert_eq!(monitor.parked.keys().copied().collect::<Vec<_>>(), [7]);
                let fired: i64 = client
                    .query_one(
                        "select count(*) from spawn_alerts where kind = 'task_parked'",
                        &[],
                    )
                    .expect("count")
                    .get(0);
                assert_eq!(fired, 1, "none for the released task");
                drop(monitor);

                // Down: 7 released, 8 parked again.
                release(client, 7);
                let eight = park(client, 8);
                refold(client);
                let monitor = started(store);
                assert_eq!(alert(client, &key).0, "cleared");
                let cause: String = client
                    .query_one(
                        "select cleared_evidence ->> 'cause' from spawn_alerts where key = $1",
                        &[&key],
                    )
                    .expect("the clear")
                    .get(0);
                assert_eq!(cause, "not_parked");
                assert_eq!(alert(client, &format!("task_parked:8:{eight}")).0, "open");
                assert_eq!(monitor.parked.keys().copied().collect::<Vec<_>>(), [8]);
                drop(monitor);
                // A start with nothing changed fires and clears nothing.
                let monitor = started(store);
                assert_eq!(alert(client, &format!("task_parked:8:{eight}")).0, "open");
                assert_eq!(monitor.parked.keys().copied().collect::<Vec<_>>(), [8]);
            });
        }

        /// The live shape of 2026-11-11 23:33 (lead-mac), on a store: two
        /// owner `validator_defect` alerts from before objective 056, one per
        /// mission (#4, gated; #7, a review mission with no gate), both about
        /// 003.sh, which the gate directory no longer holds (retired). A step
        /// started on that store clears both `gone` on its first check, no
        /// gating needed, naming the directory it read; a third alert, about
        /// a scenario the directory still holds, stays open.
        #[test]
        fn a_started_step_clears_old_per_mission_defects_whose_scenario_is_retired() {
            in_schema("alerts_defect_gone", |store, client| {
                let gate =
                    std::env::temp_dir().join(format!("spawn-alerts-gone-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&gate);
                std::fs::create_dir_all(&gate).expect("the gate directory");
                std::fs::write(gate.join("041.sh"), "").expect("a deployed scenario");
                let source = "/m/spawn.git";
                for (id, configured) in [
                    (
                        4,
                        serde_json::json!({"type": "mission.configured", "id": 4, "accept": "gate",
                               "gate": gate.to_string_lossy()}),
                    ),
                    (
                        7,
                        serde_json::json!({"type": "mission.configured", "id": 7, "accept": "review"}),
                    ),
                ] {
                    record(
                        client,
                        3,
                        &serde_json::json!({"type": "mission.added", "id": id, "text": "m",
                                "origin": "human", "sources": [source]})
                        .to_string(),
                    );
                    record(client, 3, &configured.to_string());
                }
                refold(client);
                let fire = |key: &str, mission: i64, scenario: &str, owner: bool| {
                    store
                        .alert_record(&serde_json::json!({
                            "type": "alert.fired", "key": key, "kind": "validator_defect",
                            "subject": format!("mission #{mission}"),
                            "evidence": {"event_id": 130_068, "mission": mission,
                                         "to": "e2f68bdae04e5d37f3ce5ed59357776644d18772",
                                         "owner": owner, "request": 129_675,
                                         "scenario": scenario},
                            "text": format!("mission #{mission}: {scenario} failed on main too"),
                            "notice": false,
                        }))
                        .expect("an old per-mission alert");
                };
                fire("validator_defect:4:003.sh:130068", 4, "003.sh", true);
                fire("validator_defect:7:003.sh:130068", 7, "003.sh", true);
                fire("validator_defect:4:041.sh:130068", 4, "041.sh", false);
                let _monitor = started(store);
                let mut gone = |key: &str| -> (String, Option<Value>) {
                    client
                        .query_one(
                            "select state, cleared_evidence from spawn_alerts where key = $1",
                            &[&key],
                        )
                        .map(|row| (row.get(0), row.get(1)))
                        .expect("the alert reads")
                };
                for key in [
                    "validator_defect:4:003.sh:130068",
                    "validator_defect:7:003.sh:130068",
                ] {
                    let (state, evidence) = gone(key);
                    assert_eq!(state, "cleared", "{key}");
                    let evidence = evidence.expect("its clearing");
                    assert_eq!(evidence["cause"], "gone", "{key}: {evidence}");
                    assert_eq!(
                        evidence["gates"],
                        serde_json::json!([gate.to_string_lossy()]),
                        "{evidence}"
                    );
                }
                assert_eq!(
                    gone("validator_defect:4:041.sh:130068").0,
                    "open",
                    "still deployed"
                );
                let _ = std::fs::remove_dir_all(&gate);
            });
        }

        /// Objective 058: a step started while a backlog mission has had no
        /// task in work for longer than the bound reads when its last task
        /// ended from the store and fires once; a restart fires nothing
        /// again; the mission's next assignment clears it. Owner work fires
        /// nothing.
        #[test]
        fn a_started_step_reads_a_missions_idle_time_from_the_store() {
            in_schema("alerts_mission_idle", |store, client| {
                for id in [1, 2] {
                    record(
                        client,
                        3,
                        &format!(
                            r#"{{"type":"mission.added","id":{id},"text":"m","origin":"human","sources":[]}}"#
                        ),
                    );
                    record(
                        client,
                        3,
                        &format!(
                            r#"{{"type":"task.assigned","id":{id},"mission_id":{id},"kind":"ephemeral",
                                "agent_id":"agent_w{id}","session_id":"session_w{id}","brief":"b",
                                "created_by":"spawn-integrate","objective":{id}}}"#
                        ),
                    );
                }
                record(
                    client,
                    3,
                    r#"{"type":"mission.configured","id":1,"accept":"gate",
                        "objectives":{"backlog":"b","resident":"r"}}"#,
                );
                let ended = record(client, 2, r#"{"type":"task.cancelled","id":1}"#);
                record(client, 2, r#"{"type":"task.cancelled","id":2}"#);
                refold(client);
                let key = format!("mission_idle:1:{ended}");
                let monitor = started(store);
                assert_eq!(
                    monitor.missions[&1].idle.map(|(_, event)| event),
                    Some(ended)
                );
                assert!(monitor.missions[&1].backlog && !monitor.missions[&2].backlog);
                assert_eq!(alert(client, &key).0, "open");
                let idle: i64 = client
                    .query_one(
                        "select count(*) from spawn_alerts where kind = 'mission_idle'",
                        &[],
                    )
                    .expect("count")
                    .get(0);
                assert_eq!(idle, 1, "none for owner work");
                let mut restarted = started(store);
                let fired: i64 = client
                    .query_one(
                        "select count(*) from spawn_alerts where kind = 'mission_idle'",
                        &[],
                    )
                    .expect("count")
                    .get(0);
                assert_eq!(fired, 1, "a restart fires nothing again");
                let assigned = record(
                    client,
                    0,
                    r#"{"type":"task.assigned","id":3,"mission_id":1,"kind":"ephemeral",
                        "agent_id":"agent_w3","session_id":"session_w3","brief":"b",
                        "created_by":"spawn-integrate","objective":3}"#,
                );
                refold(client);
                restarted.check(store).expect("the step checks");
                assert_eq!(alert(client, &key), ("cleared".to_string(), Some(assigned)));
            });
        }

        /// Objective 053: a step started on a run of rejections (a restart)
        /// knows the run's objective from the store, so the cancel of its
        /// task fires the notice the store's fold agrees with.
        #[test]
        fn a_restarted_step_knows_the_runs_objective_and_its_cancel() {
            in_schema("alerts_run_cancel", |store, client| {
                record(
                    client,
                    3,
                    r#"{"type":"mission.added","id":1,"text":"m","origin":"human","sources":[]}"#,
                );
                record(
                    client,
                    3,
                    r#"{"type":"task.assigned","id":5,"mission_id":1,"kind":"ephemeral",
                        "agent_id":"agent_w","session_id":"session_w","brief":"b",
                        "created_by":"spawn-integrate","objective":5}"#,
                );
                for _ in 0..2 {
                    record(
                        client,
                        2,
                        r#"{"type":"task.gated","id":5,"verdict":"rejected"}"#,
                    );
                }
                refold(client);
                let mut monitor = started(store);
                let cancelled = record(client, 0, r#"{"type":"task.cancelled","id":5}"#);
                refold(client);
                monitor.check(store).expect("the step checks");
                let (text, folded): (String, i32) = client
                    .query_one(
                        "select a.text, m.gate_rejections
                           from spawn_alerts a, mission_missions m
                          where a.key = $1 and m.id = 1",
                        &[&format!("mission_state:1:{cancelled}")],
                    )
                    .map(|row| (row.get(0), row.get(1)))
                    .expect("the cancel's notice");
                assert_eq!(
                    text,
                    "mission #1: gate_rejections 2 -> 0 (by task.cancelled)"
                );
                assert_eq!(folded, 0, "the store's count");
            });
        }

        #[test]
        fn an_older_record_clears_nothing_at_a_first_start_or_after_a_restart() {
            in_schema("alerts_restarts", |store, client| {
                record(
                    client,
                    5,
                    r#"{"type":"mission.added","id":1,"text":"m","origin":"human","sources":[]}"#,
                );
                let d1 = record(client, 5, r#"{"type":"backup.dumped","keys":["d1"]}"#);
                let v1 = record(
                    client,
                    4,
                    r#"{"type":"backup.verified","ok":true,"key":"v1"}"#,
                );
                let h1 = record(
                    client,
                    3,
                    r#"{"type":"mission.halted","id":1,"reason":"first"}"#,
                );
                let d2 = record(client, 3, r#"{"type":"backup.dumped","keys":["d2"]}"#);
                let r1 = record(client, 2, r#"{"type":"mission.resumed","id":1}"#);
                let h2 = record(
                    client,
                    1,
                    r#"{"type":"mission.halted","id":1,"reason":"second"}"#,
                );
                let v2 = record(
                    client,
                    1,
                    r#"{"type":"backup.verified","ok":false,"key":"v2","reason":"bad"}"#,
                );
                refold(client);
                assert!(d1 < d2 && v1 < v2 && h1 < r1 && r1 < h2);
                let halt = format!("mission_halted:1:{h2}");
                let stale = format!("backup_stale:dump:{d2}");
                let drill = format!("drill_failed:{v2}");

                // The first start: the second halt, the newer dump and the
                // failed drill are open; the first halt is cleared on its
                // resume, which the same history holds.
                let mut monitor = started(store);
                for key in [&halt, &stale, &drill] {
                    assert_eq!(alert(client, key), ("open".to_string(), None), "{key}");
                }
                assert_eq!(
                    alert(client, &format!("mission_halted:1:{h1}")),
                    ("cleared".to_string(), Some(r1)),
                    "fired and cleared naming its resume"
                );

                // Restarts: the older resume, dump and drill clear nothing.
                for _ in 0..2 {
                    monitor = started(store);
                    for key in [&halt, &stale, &drill] {
                        assert_eq!(alert(client, key).0, "open", "{key} after a restart");
                    }
                }

                // Later evidence clears each, naming it.
                let r2 = record(client, 0, r#"{"type":"mission.resumed","id":1}"#);
                let d3 = record(client, 0, r#"{"type":"backup.dumped","keys":["d3"]}"#);
                let v3 = record(
                    client,
                    0,
                    r#"{"type":"backup.verified","ok":true,"key":"v3"}"#,
                );
                refold(client);
                monitor.check(store).expect("the step checks");
                assert_eq!(alert(client, &halt), ("cleared".to_string(), Some(r2)));
                assert_eq!(alert(client, &stale), ("cleared".to_string(), Some(d3)));
                assert_eq!(alert(client, &drill), ("cleared".to_string(), Some(v3)));

                // A further restart fires nothing again.
                let before = count(client);
                drop(started(store));
                assert_eq!(count(client), before);

                // A halt and its resume, and a failed drill and a good one,
                // written while the loop was down: neither opens.
                let h3 = record(
                    client,
                    0,
                    r#"{"type":"mission.halted","id":1,"reason":"third"}"#,
                );
                let r3 = record(client, 0, r#"{"type":"mission.resumed","id":1}"#);
                let v4 = record(
                    client,
                    0,
                    r#"{"type":"backup.verified","ok":false,"key":"v4","reason":"bad"}"#,
                );
                let v5 = record(
                    client,
                    0,
                    r#"{"type":"backup.verified","ok":true,"key":"v5"}"#,
                );
                refold(client);
                drop(started(store));
                assert_eq!(
                    alert(client, &format!("mission_halted:1:{h3}")),
                    ("cleared".to_string(), Some(r3))
                );
                assert_eq!(
                    alert(client, &format!("drill_failed:{v4}")),
                    ("cleared".to_string(), Some(v5))
                );
            });
        }
    }

    mod through_the_store {
        use super::super::{Event, Monitor};
        use super::{commit, decided, fires, monitor};
        use serde_json::{Value, json};
        use spawn_core::command::Outcome;
        use spawn_core::event::{
            ENGINE, EndCause, EventKind, ModelProfile, NewEvent, SessionSeq, Source,
        };
        use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, WorkspaceId};
        use spawn_core::observation::{Confidence, Observation, SemanticState};
        use spawn_core::port::EventLog;
        use spawn_postgres::PostgresStore;

        /// Run `case` on a migrated schema of its own, dropped afterwards.
        fn in_schema(name: &str, case: impl FnOnce(&PostgresStore)) {
            let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
                "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
            );
            let schema = spawn_postgres::testing::TestSchema::new(name);
            let store = PostgresStore::connect_in_schema(&url, &schema)
                .expect("a schema of its own, migrated");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| case(&store)));
            drop(store);
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
        }

        /// One worker session's records, appended in order; each start
        /// begins an execution of its own (`exec_<name>_<n>`), as the
        /// Bridge's do.
        struct Session {
            name: &'static str,
            next: u64,
            execution: u32,
        }

        impl Session {
            fn new(name: &'static str) -> Self {
                Self {
                    name,
                    next: 0,
                    execution: 0,
                }
            }

            fn append(&mut self, store: &PostgresStore, source: Source, kind: EventKind) -> i64 {
                let execution = self.execution;
                self.append_to(store, execution, source, kind)
            }

            /// A record of execution `n` of this session.
            fn append_to(
                &mut self,
                store: &PostgresStore,
                n: u32,
                source: Source,
                kind: EventKind,
            ) -> i64 {
                self.next += 1;
                let execution = ExecutionId::new(format!("exec_{}_{n}", self.name)).expect("valid");
                let appended = store
                    .append(NewEvent {
                        agent_id: AgentId::new(format!("agent_{}", self.name)).expect("valid"),
                        session_id: SessionId::new(format!("session_{}", self.name))
                            .expect("valid"),
                        execution_id: Some(execution),
                        source,
                        source_event_id: Some(format!("{}:{}", self.name, self.next)),
                        kind,
                    })
                    .expect("append succeeds");
                i64::try_from(appended.record.position.0).expect("an id")
            }

            fn started(&mut self, store: &PostgresStore) -> i64 {
                self.execution += 1;
                self.append(
                    store,
                    Source::Bridge,
                    EventKind::ExecutionStarted {
                        engine: ENGINE.to_string(),
                        workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
                        command_id: CommandId(1),
                        fork: None,
                        model: ModelProfile::Default,
                        transcript_from: None,
                    },
                )
            }

            fn ended(&mut self, store: &PostgresStore, cause: EndCause) -> i64 {
                self.append(
                    store,
                    Source::Bridge,
                    EventKind::ExecutionEnded {
                        cause,
                        exit_code: None,
                        signal: None,
                        evidence: None,
                    },
                )
            }

            /// A judgement: the model judge's with a `stuck` score, or the
            /// rules' (no `stuck`), as the Observer records after an end.
            fn judged(&mut self, store: &PostgresStore, stuck: Option<f64>) -> i64 {
                let mut observation = Observation::basic(
                    if stuck.is_some() {
                        SemanticState::Unknown
                    } else {
                        SemanticState::Completed
                    },
                    None,
                    SessionSeq(self.next),
                    Confidence::new(1.0).expect("valid confidence"),
                    if stuck.is_some() { "jev" } else { "rules" },
                );
                observation.stuck = stuck;
                self.append(
                    store,
                    Source::Observer,
                    EventKind::ObservationRecorded { observation },
                )
            }
        }

        fn observe(monitor: &mut Monitor, store: &PostgresStore) -> Vec<Event> {
            let observations: Value = store.alert_observations().expect("the judgements");
            let mut out = Vec::new();
            monitor.observe(&observations, &mut out);
            decided(monitor, out)
        }

        fn cleared_by(events: &[Event]) -> Vec<(String, Value)> {
            events
                .iter()
                .filter_map(|event| match event {
                    Event::Clear { key, evidence } => Some((key.clone(), evidence.clone())),
                    Event::Fire { .. } => None,
                })
                .collect()
        }

        /// 106's shape on the live log: stuck, stopped, then the rules'
        /// COMPLETED judgement with no `stuck` after the end. The end is the
        /// session's current state, so it clears, with the end's id; and a
        /// lost execution clears the same way, its cause named.
        #[test]
        fn a_stuck_alert_clears_at_its_executions_end_whatever_judgement_follows() {
            in_schema("stuck_end", |store| {
                let mut monitor = monitor();
                let mut stopped = Session::new("worker_stop");
                let mut lost = Session::new("worker_lost");
                stopped.started(store);
                lost.started(store);
                let stuck = stopped.judged(store, Some(0.64));
                let stuck_lost = lost.judged(store, Some(0.7));
                let fired = observe(&mut monitor, store);
                assert_eq!(
                    fires(&fired),
                    [
                        format!("agent_stuck:session_worker_lost:{stuck_lost}"),
                        format!("agent_stuck:session_worker_stop:{stuck}"),
                    ]
                );
                commit(&mut monitor, &fired);

                let end = stopped.ended(store, EndCause::Stopped);
                let end_lost = lost.ended(store, EndCause::Lost);
                stopped.judged(store, None);
                lost.judged(store, None);
                let cleared = observe(&mut monitor, store);
                assert_eq!(
                    cleared_by(&cleared),
                    [
                        (
                            format!("agent_stuck:session_worker_lost:{stuck_lost}"),
                            json!({"event_id": end_lost, "cause": "lost"})
                        ),
                        (
                            format!("agent_stuck:session_worker_stop:{stuck}"),
                            json!({"event_id": end, "cause": "stopped"})
                        ),
                    ]
                );
                commit(&mut monitor, &cleared);
                assert!(observe(&mut monitor, store).is_empty(), "cleared once");
            });
        }

        /// An end and then a start (a resume, as the supervisor's lost
        /// execution and the next): the end is not current, so a stuck
        /// alert of the new execution stays open, and an unscored judgement
        /// changes nothing.
        #[test]
        fn an_end_followed_by_a_start_clears_nothing() {
            in_schema("stuck_resume", |store| {
                let mut monitor = monitor();
                let mut session = Session::new("supervisor");
                session.started(store);
                session.ended(store, EndCause::Lost);
                session.started(store);
                let stuck = session.judged(store, Some(0.52));
                let fired = observe(&mut monitor, store);
                assert_eq!(
                    fires(&fired),
                    [format!("agent_stuck:session_supervisor:{stuck}")]
                );
                commit(&mut monitor, &fired);
                session.judged(store, None);
                assert!(observe(&mut monitor, store).is_empty(), "nothing clears");
            });
        }

        /// A runner's late report of the older execution's end, recorded
        /// after the resume's start (only old logs hold one: bridge.rs), is
        /// no end: the store counts the Bridge's alone (schema.sql's "a
        /// runner's exit report is no end"). The live execution's stuck
        /// judgement fires, and nothing clears it. rev-f's probe on
        /// 4897d444, which read that report as the current end.
        #[test]
        fn a_runners_late_end_report_of_an_older_execution_is_no_end() {
            in_schema("stuck_late_report", |store| {
                let mut monitor = monitor();
                let mut session = Session::new("late");
                session.started(store);
                session.ended(store, EndCause::Lost);
                session.started(store);
                session.append_to(
                    store,
                    1,
                    Source::Claude,
                    EventKind::ExecutionEnded {
                        cause: EndCause::Exited,
                        exit_code: Some(0),
                        signal: None,
                        evidence: None,
                    },
                );
                let stuck = session.judged(store, Some(0.8));
                let fired = observe(&mut monitor, store);
                assert_eq!(fires(&fired), [format!("agent_stuck:session_late:{stuck}")]);
                commit(&mut monitor, &fired);
                session.judged(store, None);
                assert!(observe(&mut monitor, store).is_empty(), "nothing clears it");
            });
        }

        /// The model judge's `down` record (crate judge_watch.rs), read back
        /// through the store's history as a restarted loop reads it: no
        /// alert before SPAWN_ALERT_JUDGE_DOWN_SECS after its first failed
        /// call (30 s before the record), `judge_down` at it, once, and its
        /// `up` record, read as the next pass reads the control stream,
        /// clears it with `failed_calls`. A loop whose history no longer
        /// holds the `down` (restarted a week into the outage) clears the
        /// open alert at the `up` all the same.
        #[test]
        fn a_judge_down_for_its_bound_fires_once_and_its_up_clears_it() {
            in_schema("judge_down", |store| {
                let mut monitor = monitor();
                let first = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(0))
                    - 30_000;
                let down = store
                    .host_record(&json!({"type": "host.judge", "judge": "jev",
                                         "state": "down", "error": "judge unavailable: 502",
                                         "first_failed_ms": first}))
                    .expect("record down");
                let history = store.alert_history(0, 100).expect("history");
                let mut fed = Vec::new();
                for entry in history.as_array().into_iter().flatten() {
                    monitor.feed(
                        entry["event_id"].as_i64().unwrap_or_default(),
                        entry["at_ms"].as_i64().unwrap_or_default(),
                        &entry["record"],
                        &mut fed,
                    );
                }
                assert!(fed.is_empty(), "a down is not yet an alert");
                assert!(history[0]["at_ms"].as_i64().is_some_and(|at| at > first));
                let at = first;
                let mut early = Vec::new();
                monitor.bounds(at + 899_999, &mut early);
                assert!(early.is_empty(), "never before its bound");
                let mut due = Vec::new();
                monitor.bounds(at + 900_000, &mut due);
                let key = format!("judge_down:{down}");
                assert_eq!(fires(&due), [key.as_str()]);
                let Event::Fire {
                    subject,
                    evidence,
                    text,
                    ..
                } = &due[0]
                else {
                    panic!("a fire");
                };
                assert_eq!(subject, "jev");
                assert_eq!(evidence["down_ms"], 900_000);
                assert_eq!(evidence["error"], "judge unavailable: 502");
                assert!(
                    text.contains("failed two calls in a row, the first 900 s ago"),
                    "{text}"
                );
                let recorded = decided(&monitor, due);
                commit(&mut monitor, &recorded);
                let mut restarted = super::monitor();
                commit(&mut restarted, &recorded);
                let mut again = Vec::new();
                monitor.bounds(at + 1_800_000, &mut again);
                assert!(decided(&monitor, again).is_empty(), "once per occurrence");

                let up = store
                    .host_record(&json!({"type": "host.judge", "judge": "jev",
                                         "state": "up", "failed_calls": 41}))
                    .expect("record up");
                let batch = store.alert_control(down, 100).expect("the control stream");
                let mut cleared = Vec::new();
                for entry in batch["records"].as_array().into_iter().flatten() {
                    monitor.feed(
                        entry["event_id"].as_i64().unwrap_or_default(),
                        entry["at_ms"].as_i64().unwrap_or_default(),
                        &entry["record"],
                        &mut cleared,
                    );
                }
                let cleared = cleared_by(&decided(&monitor, cleared));
                assert_eq!(cleared.len(), 1, "{cleared:?}");
                assert_eq!(cleared[0].0, key);
                assert_eq!(cleared[0].1["event_id"], up);
                assert_eq!(cleared[0].1["failed_calls"], 41);
                assert!(
                    cleared[0].1["down_ms"]
                        .as_i64()
                        .is_some_and(|ms| ms >= 30_000)
                );

                let mut orphan = Vec::new();
                for entry in batch["records"].as_array().into_iter().flatten() {
                    restarted.feed(
                        entry["event_id"].as_i64().unwrap_or_default(),
                        entry["at_ms"].as_i64().unwrap_or_default(),
                        &entry["record"],
                        &mut orphan,
                    );
                }
                let orphan = cleared_by(&decided(&restarted, orphan));
                assert_eq!(orphan.len(), 1, "the up with no down known: {orphan:?}");
                assert_eq!(orphan[0].0, key);
                assert_eq!(orphan[0].1["event_id"], up);
            });
        }

        /// Objective 166: the window both `--once --json` and /api/alerts
        /// answer folds the notices (every row kept, each but a subject's
        /// newest superseded by its next one), and `--once` in text shows
        /// each subject's newest with how many came before.
        #[test]
        fn the_window_folds_notices_and_the_text_shows_each_subjects_newest() {
            in_schema("window_fold", |store| {
                let fire = |key: &str, kind: &str, subject: &str, text: &str, notice: bool| {
                    store
                        .alert_record(&json!({
                            "type": "alert.fired", "key": key, "kind": kind,
                            "subject": subject, "evidence": {}, "text": text,
                            "notice": notice,
                        }))
                        .expect("fire");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                };
                fire(
                    "memory_low:1",
                    "memory_low",
                    "host",
                    "MemAvailable is low",
                    false,
                );
                for (event, state) in [(10, "assigned"), (12, "reported"), (11, "delivered")] {
                    fire(
                        &format!("task_state:7:{event}"),
                        "task_state",
                        "task #7",
                        &format!("task #7 {state}"),
                        true,
                    );
                }
                fire(
                    "task_state:8:15",
                    "task_state",
                    "task #8",
                    "task #8 assigned",
                    true,
                );
                let window = super::super::window(store, Some("1d")).expect("the window");
                let rows = window.as_array().expect("an array");
                assert_eq!(rows.len(), 5, "every row kept: {window}");
                let mut folded: Vec<(String, Value)> = rows
                    .iter()
                    .map(|row| {
                        (
                            row["key"].as_str().expect("a key").to_string(),
                            row.get("superseded_by").cloned().unwrap_or(Value::Null),
                        )
                    })
                    .collect();
                folded.sort_by(|left, right| left.0.cmp(&right.0));
                assert_eq!(
                    folded,
                    [
                        ("memory_low:1".to_string(), Value::Null),
                        ("task_state:7:10".to_string(), json!("task_state:7:11")),
                        ("task_state:7:11".to_string(), json!("task_state:7:12")),
                        ("task_state:7:12".to_string(), Value::Null),
                        ("task_state:8:15".to_string(), Value::Null),
                    ]
                );
                let lines = super::super::lines(rows);
                assert_eq!(lines.len(), 3, "{lines:?}");
                assert!(lines[0].starts_with("open memory_low host "), "{lines:?}");
                assert!(
                    lines[1].starts_with("notice task_state task #7 ")
                        && lines[1].ends_with("task #7 reported · 2 earlier"),
                    "{lines:?}"
                );
                assert!(lines[2].ends_with("task #8 assigned"), "{lines:?}");
            });
        }

        /// An open agent_stuck says when its session was last scored, read
        /// with the window: an unscored judgement after it (a judge that is
        /// down) leaves that time where it was.
        #[test]
        fn an_open_stuck_alert_carries_the_time_of_its_sessions_last_score() {
            in_schema("stuck_scored_at", |store| {
                let mut session = Session::new("scored");
                session.started(store);
                let stuck = session.judged(store, Some(0.7));
                let scored_at = store.alert_observations().expect("the judgements")[0]["at_ms"]
                    .as_i64()
                    .expect("its time");
                store
                    .alert_record(&json!({
                        "type": "alert.fired", "key": format!("agent_stuck:session_scored:{stuck}"),
                        "kind": "agent_stuck", "subject": "session_scored",
                        "evidence": {"stuck": 0.7, "event_id": stuck},
                        "text": "session_scored is stuck (0.70): UNKNOWN", "notice": false,
                    }))
                    .expect("fire");
                std::thread::sleep(std::time::Duration::from_millis(5));
                session.judged(store, None);
                let window = store.alerts(0).expect("the window");
                assert_eq!(window[0]["scored_at_ms"], scored_at, "{window}");
                assert!(super::super::line(&window[0]).contains("(last scored "));
            });
        }

        /// A loop that starts after the session ended, its latest scored
        /// judgement stuck: nothing fires. And an alert already open on a
        /// judgement made after the end (one fired before this rule) clears
        /// at that end.
        #[test]
        fn an_ended_session_fires_nothing_and_its_open_alert_clears() {
            in_schema("stuck_ended", |store| {
                let mut session = Session::new("worker_done");
                session.started(store);
                let end = session.ended(store, EndCause::Exited);
                let stuck = session.judged(store, Some(0.9));
                let mut fresh = monitor();
                assert!(
                    observe(&mut fresh, store).is_empty(),
                    "an ended session fires nothing"
                );

                let key = format!("agent_stuck:session_worker_done:{stuck}");
                let mut monitor = monitor();
                commit(
                    &mut monitor,
                    &[Event::Fire {
                        key: key.clone(),
                        kind: "agent_stuck",
                        subject: "session_worker_done".to_string(),
                        evidence: json!({}),
                        text: String::new(),
                        notice: false,
                    }],
                );
                assert_eq!(
                    cleared_by(&observe(&mut monitor, store)),
                    [(key, json!({"event_id": end, "cause": "exited"}))]
                );
            });
        }

        /// One record with the key its producer gives it.
        fn keyed(
            store: &PostgresStore,
            session: &str,
            execution: &str,
            source: Source,
            key: &str,
            kind: EventKind,
        ) -> i64 {
            let appended = store
                .append(NewEvent {
                    agent_id: AgentId::new(format!("agent_{session}")).expect("valid"),
                    session_id: SessionId::new(format!("session_{session}")).expect("valid"),
                    execution_id: Some(ExecutionId::new(execution.to_string()).expect("valid")),
                    source,
                    source_event_id: Some(key.to_string()),
                    kind,
                })
                .expect("append succeeds");
            i64::try_from(appended.record.position.0).expect("an id")
        }

        /// A person's input into `session` (`key` its idempotency key).
        fn input(store: &PostgresStore, session: &str, key: &str, text: &str) -> CommandId {
            use spawn_core::command::{CommandAction, Submission};
            use spawn_core::id::{IdempotencyKey, Principal};
            use spawn_core::port::CommandQueue;
            store
                .submit(
                    &Principal::new("human").expect("principal"),
                    &Submission {
                        agent_id: AgentId::new(format!("agent_{session}")).expect("agent"),
                        session_id: SessionId::new(format!("session_{session}")).expect("session"),
                        idempotency_key: IdempotencyKey::new(key).expect("key"),
                        action: CommandAction::Input {
                            text: text.to_string(),
                            tags: Vec::new(),
                        },
                    },
                )
                .expect("submit")
        }

        /// The Bridge's records of typing `command` into `execution`:
        /// `message.typed` when the text carried the marker, then `typing`
        /// and `typed`.
        fn typed(
            store: &PostgresStore,
            session: &str,
            execution: &str,
            command: CommandId,
            marked: bool,
        ) {
            use spawn_core::event::CommandStep;
            let id = command.0;
            if marked {
                keyed(
                    store,
                    session,
                    execution,
                    Source::Bridge,
                    &format!("cmd_{id}:message"),
                    EventKind::MessageTyped {
                        command_id: command,
                    },
                );
            }
            for (step, name) in [
                (CommandStep::Typing, "typing"),
                (CommandStep::Typed, "typed"),
            ] {
                keyed(
                    store,
                    session,
                    execution,
                    Source::Bridge,
                    &format!("cmd_{id}:{name}"),
                    EventKind::CommandStepped {
                        command_id: command,
                        step,
                        workspace: None,
                    },
                );
            }
        }

        /// The transcript's line `line` took `command`.
        fn taken(store: &PostgresStore, session: &str, execution: &str, line: &str, id: u64) {
            taken_at(store, session, execution, line, id);
        }

        /// [`taken`], and the time the records hold for it.
        fn taken_at(
            store: &PostgresStore,
            session: &str,
            execution: &str,
            line: &str,
            id: u64,
        ) -> i64 {
            store
                .append(NewEvent {
                    agent_id: AgentId::new(format!("agent_{session}")).expect("valid"),
                    session_id: SessionId::new(format!("session_{session}")).expect("valid"),
                    execution_id: Some(ExecutionId::new(execution.to_string()).expect("valid")),
                    source: Source::Claude,
                    source_event_id: Some(format!("{line}:taken:{id}")),
                    kind: EventKind::MessageTaken {
                        command_id: CommandId(id),
                    },
                })
                .expect("append succeeds")
                .record
                .recorded_at
                .0
        }

        /// The clearings among `events`, by key, in key order.
        fn clears_of_messages(events: &[Event]) -> Vec<(String, Value)> {
            let mut clears: Vec<(String, Value)> = events
                .iter()
                .filter_map(|event| match event {
                    Event::Clear { key, evidence } => Some((key.clone(), evidence.clone())),
                    Event::Fire { .. } => None,
                })
                .collect();
            clears.sort_by(|a, b| a.0.cmp(&b.0));
            clears
        }

        fn turn_ended(store: &PostgresStore, session: &str, execution: &str, line: &str) -> i64 {
            keyed(
                store,
                session,
                execution,
                Source::Claude,
                line,
                EventKind::TurnStopped {
                    input_tokens: None,
                    output_tokens: None,
                },
            )
        }

        /// A monitor whose messages are due at once.
        fn prompt_monitor() -> Monitor {
            let mut settings = super::settings();
            settings.message_taken_ms = 0;
            Monitor::with(settings)
        }

        /// The step's own reading of the messages: the commands after the
        /// last it saw, its pending messages and its open alerts'.
        fn read(monitor: &mut Monitor, store: &PostgresStore) -> Vec<Event> {
            let mut out = Vec::new();
            let mut lines = Vec::new();
            let now = store.now_ms().expect("the clock");
            monitor.read_messages(store, now, &mut out, &mut lines);
            assert!(lines.is_empty(), "{lines:?}");
            decided(monitor, out)
        }

        /// The commands of one session, as an older Spawn and this one left
        /// them: `old`, typed with no marker before any loop typed markers;
        /// `answered`, taken twice and answered after its first taking;
        /// `deaf`, typed and never taken; `untyped`, never typed. All four
        /// carry the same text.
        struct Messages {
            worker: Session,
            old: CommandId,
            answered: CommandId,
            deaf: CommandId,
            untyped: CommandId,
            refused: CommandId,
        }

        const MSG_EXECUTION: &str = "exec_msg_1";

        fn messages_world(store: &PostgresStore) -> Messages {
            let session = "msg";
            let mut worker = Session::new(session);
            let old = input(store, session, "old", "same text");
            worker.started(store);
            typed(store, session, MSG_EXECUTION, old, false);
            store
                .host_record(&json!({"type": "host.messages_marked", "started_at_ms": 1}))
                .expect("the first marking loop");
            finish(store, old, &Outcome::Succeeded);
            // The first message the transcript holds: from then on the
            // session's messages are tracked.
            let answered = input(store, session, "answered", "same text");
            typed(store, session, MSG_EXECUTION, answered, true);
            taken(store, session, MSG_EXECUTION, "u1", answered.0);
            taken(store, session, MSG_EXECUTION, "u1-again", answered.0);
            turn_ended(store, session, MSG_EXECUTION, "t1");
            finish(store, answered, &Outcome::Succeeded);
            // One the Bridge refused before typing it, which a loop sends
            // again as a new command.
            let refused = input(store, session, "refused", "same text");
            finish(
                store,
                refused,
                &Outcome::Failed("pane state Busy does not accept input".to_string()),
            );
            let deaf = input(store, session, "deaf", "same text");
            let untyped = input(store, session, "untyped", "same text");
            typed(store, session, MSG_EXECUTION, deaf, true);
            Messages {
                worker,
                old,
                answered,
                deaf,
                untyped,
                refused,
            }
        }

        /// Claim `command` (the oldest unfinished one) and finish it.
        fn finish(store: &PostgresStore, command: CommandId, outcome: &Outcome) {
            use spawn_core::port::CommandQueue;
            let claimed = store
                .claim("tester", std::time::Duration::from_mins(1))
                .expect("claim")
                .expect("a command");
            assert_eq!(claimed.command_id, command);
            store.finish(&claimed, outcome).expect("finish");
        }

        /// Each command's message read by its id from the records, by the
        /// commands API: answered and taken twice; typed and not taken; never
        /// typed and not taken; an older Spawn's, typed and never not taken.
        /// The first marking loop's record is written once.
        #[test]
        fn each_command_reads_its_message_by_its_id() {
            in_schema("messages_read", |store| {
                let world = messages_world(store);
                let again = store
                    .host_record(&json!({"type": "host.messages_marked", "started_at_ms": 2}))
                    .expect("a later loop");
                let first = store
                    .host_record(&json!({"type": "host.messages_marked", "started_at_ms": 3}))
                    .expect("and another");
                assert_eq!(first, again, "the first record stands");
                let commands = store
                    .commands_json("session_msg", 50, 0)
                    .expect("the commands");
                let message = |id: CommandId| {
                    commands
                        .as_array()
                        .expect("an array")
                        .iter()
                        .find(|command| command["command_id"] == json!(id.0))
                        .expect("the command")["message"]
                        .clone()
                };
                let answer = message(world.answered);
                assert_eq!(answer["state"], "answered", "{answer}");
                assert_eq!(answer["taken"], 2);
                assert_eq!(answer["not_taken"], false);
                assert!(answer["typed_at_ms"].as_i64() <= answer["taken_at_ms"].as_i64());
                assert!(answer["taken_at_ms"].as_i64() <= answer["answered_at_ms"].as_i64());
                let mut names: Vec<String> = answer
                    .as_object()
                    .map(|fields| fields.keys().cloned().collect())
                    .unwrap_or_default();
                names.sort_unstable();
                assert_eq!(
                    names,
                    [
                        "acted",
                        "acted_at_ms",
                        "answered_at_ms",
                        "not_taken",
                        "state",
                        "taken",
                        "taken_at_ms",
                        "typed_at_ms"
                    ]
                );
                assert_eq!(answer["acted"], Value::Null, "never acked: {answer}");
                let deaf = message(world.deaf);
                assert_eq!(
                    (&deaf["state"], &deaf["not_taken"], &deaf["taken"]),
                    (&json!("typed"), &json!(true), &json!(0)),
                    "{deaf}"
                );
                assert_eq!(deaf["answered_at_ms"], Value::Null);
                let never = message(world.untyped);
                assert_eq!(
                    (&never["state"], &never["not_taken"], &never["typed_at_ms"]),
                    (&json!("recorded"), &json!(true), &Value::Null),
                    "{never}"
                );
                let refused = message(world.refused);
                assert_eq!(
                    (&refused["state"], &refused["not_taken"]),
                    (&json!("recorded"), &json!(false)),
                    "one refused before typing is no message not taken: {refused}"
                );
                let older = message(world.old);
                assert_eq!(
                    (&older["state"], &older["not_taken"]),
                    (&json!("typed"), &json!(false)),
                    "an older Spawn's input is never not taken: {older}"
                );
                // Within its bound, none is not taken yet.
                let patient = store
                    .commands_json("session_msg", 50, 3_600_000)
                    .expect("the commands");
                assert!(
                    patient
                        .as_array()
                        .into_iter()
                        .flatten()
                        .all(|command| command["message"]["not_taken"] == false),
                    "{patient}"
                );
            });
        }

        /// S17: a typed message's alert says what the Bridge recorded of the
        /// transcript since the typing. `deaf` has a transcript line read
        /// after it (a turn's end): the agent has not taken it, and the text
        /// says so with the count. `silent`, typed after that line, has none
        /// read since: the text says whether the agent took it is unknown,
        /// and why (Spawn not reading the transcript, or nothing written).
        #[test]
        fn a_typed_message_not_taken_says_whether_spawn_read_the_transcript_since() {
            in_schema("messages_read_since", |store| {
                let world = messages_world(store);
                let deaf = world.deaf;
                // Recorded after the typing, at a later millisecond.
                std::thread::sleep(std::time::Duration::from_millis(20));
                turn_ended(store, "msg", MSG_EXECUTION, "t-after-deaf");
                std::thread::sleep(std::time::Duration::from_millis(20));
                let silent = input(store, "msg", "silent", "same text");
                typed(store, "msg", MSG_EXECUTION, silent, true);
                let mut monitor = prompt_monitor();
                let fired = read(&mut monitor, store);
                let alert = |id: CommandId| {
                    fired
                        .iter()
                        .find_map(|event| match event {
                            Event::Fire {
                                key,
                                evidence,
                                text,
                                ..
                            } if *key == format!("message_not_taken:{}", id.0) => {
                                Some((evidence.clone(), text.clone()))
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| panic!("an alert for {}: {fired:?}", id.0))
                };
                let (heard, heard_text) = alert(deaf);
                assert_eq!(heard["read_since_typed"], 1);
                assert!(
                    heard_text.starts_with(&format!(
                        "agent_msg has not taken message #{} (its input): typed, and the \
                         transcript lines Spawn has read since (1) hold no marker of it",
                        deaf.0
                    )),
                    "{heard_text}"
                );
                let (unread, unread_text) = alert(silent);
                assert_eq!(unread["read_since_typed"], 0);
                assert!(
                    unread_text.contains(
                        "Spawn has read no line of agent_msg's transcript since the typing: \
                         whether agent_msg took it is unknown. Either Spawn is not reading that \
                         transcript (the Bridge follows it), or the engine has written nothing since"
                    ),
                    "{unread_text}"
                );
                assert!(!unread_text.contains("has not taken"), "{unread_text}");
            });
        }

        /// The alert fires for the two not taken (whether typed in its
        /// evidence), once; the one taken later clears, and so does the deaf
        /// one before it (objective 138: a later message of its session was
        /// taken, `later_taken`), so the session's end clears nothing; a
        /// fresh step then fires nothing. Before 138 the deaf one stayed open
        /// until the session's end.
        #[test]
        fn a_message_not_taken_is_one_alert_until_taken_or_its_session_ends() {
            in_schema("messages_alert", |store| {
                let mut world = messages_world(store);
                let (deaf, untyped) = (world.deaf, world.untyped);
                let mut monitor = prompt_monitor();
                let fired = read(&mut monitor, store);
                assert_eq!(
                    fires(&fired),
                    [
                        format!("message_not_taken:{}", deaf.0),
                        format!("message_not_taken:{}", untyped.0)
                    ]
                );
                let evidence = |id: CommandId| {
                    fired
                        .iter()
                        .find_map(|event| match event {
                            Event::Fire {
                                key,
                                evidence,
                                subject,
                                ..
                            } if *key == format!("message_not_taken:{}", id.0) => {
                                Some((subject.clone(), evidence.clone()))
                            }
                            _ => None,
                        })
                        .expect("its alert")
                };
                let (subject, deaf_evidence) = evidence(deaf);
                assert_eq!(subject, "session_msg");
                assert_eq!(deaf_evidence["command"], json!(deaf.0));
                assert_eq!(deaf_evidence["agent_id"], "agent_msg");
                assert_eq!(deaf_evidence["typed"], true);
                assert_eq!(
                    deaf_evidence["read_since_typed"], 0,
                    "no transcript line recorded since it was typed"
                );
                assert_eq!(evidence(untyped).1["typed"], false);
                assert_eq!(evidence(untyped).1["read_since_typed"], Value::Null);
                commit(&mut monitor, &fired);
                assert!(read(&mut monitor, store).is_empty(), "once");

                // The untyped one is typed and taken: its alert clears, and
                // the deaf one's before it (a later message taken).
                typed(store, "msg", MSG_EXECUTION, untyped, true);
                let at = taken_at(store, "msg", MSG_EXECUTION, "u2", untyped.0);
                let cleared = read(&mut monitor, store);
                assert_eq!(
                    clears_of_messages(&cleared),
                    [
                        (
                            format!("message_not_taken:{}", deaf.0),
                            json!({"later_taken": untyped.0, "taken_at_ms": at})
                        ),
                        (
                            format!("message_not_taken:{}", untyped.0),
                            json!({"taken": 1, "taken_at_ms": at})
                        ),
                    ],
                    "{cleared:?}"
                );
                commit(&mut monitor, &cleared);

                // The session ends: nothing is left open to clear.
                world.worker.ended(store, EndCause::Stopped);
                let ended = read(&mut monitor, store);
                assert!(ended.is_empty(), "{ended:?}");
                assert!(read(&mut monitor, store).is_empty());
                // A fresh step finds nothing to fire for an ended session.
                assert!(read(&mut prompt_monitor(), store).is_empty());
            });
        }

        /// A live session `name` (execution `exec_<name>_1`) whose
        /// transcript already holds a marker: a first input typed and
        /// taken, after the first marking loop started. Its messages from
        /// then on are tracked.
        fn marked_session(store: &PostgresStore, name: &'static str) -> Session {
            let mut session = Session::new(name);
            session.started(store);
            store
                .host_record(&json!({"type": "host.messages_marked", "started_at_ms": 1}))
                .expect("the first marking loop");
            let execution = format!("exec_{name}_1");
            let first = input(store, name, &format!("{name}-first"), "hello");
            typed(store, name, &execution, first, true);
            taken(store, name, &execution, &format!("{name}-u0"), first.0);
            session
        }

        /// The alert (fire or clear) of `id` among `events`.
        fn message_alert(events: &[Event], id: CommandId) -> Option<&Event> {
            events
                .iter()
                .find(|event| event.key() == format!("message_not_taken:{}", id.0))
        }

        /// Objective 138: a message whose marker was missed fires; two later
        /// ones of its session taken clear it, the evidence naming the first
        /// taking (`later_taken`, its time); a message after those, not
        /// taken, fires and stays open, nothing later being taken; the
        /// session's end clears it.
        #[test]
        fn a_message_not_taken_clears_when_a_later_one_is_taken() {
            in_schema("messages_later_taken", |store| {
                let mut world = marked_session(store, "later");
                let exec = "exec_later_1";
                let missed = input(store, "later", "missed", "a");
                typed(store, "later", exec, missed, true);
                let mut monitor = prompt_monitor();
                let fired = read(&mut monitor, store);
                assert_eq!(fires(&fired), [format!("message_not_taken:{}", missed.0)]);
                commit(&mut monitor, &fired);
                assert!(read(&mut monitor, store).is_empty(), "once");

                // The agent goes on: two later messages, both taken before
                // the step reads again.
                let next = input(store, "later", "next", "b");
                let after = input(store, "later", "after", "c");
                typed(store, "later", exec, next, true);
                typed(store, "later", exec, after, true);
                let first = taken_at(store, "later", exec, "u-next", next.0);
                std::thread::sleep(std::time::Duration::from_millis(20));
                taken(store, "later", exec, "u-after", after.0);
                let cleared = read(&mut monitor, store);
                assert_eq!(
                    clears_of_messages(&cleared),
                    [(
                        format!("message_not_taken:{}", missed.0),
                        json!({"later_taken": next.0, "taken_at_ms": first})
                    )],
                    "{cleared:?}"
                );
                assert!(fires(&cleared).is_empty(), "{cleared:?}");
                commit(&mut monitor, &cleared);

                // One after those, never taken, nothing later taken: it
                // fires and stays open.
                let last = input(store, "later", "last", "d");
                typed(store, "later", exec, last, true);
                let fired = read(&mut monitor, store);
                assert_eq!(fires(&fired), [format!("message_not_taken:{}", last.0)]);
                commit(&mut monitor, &fired);
                assert!(read(&mut monitor, store).is_empty(), "still open");
                let message = store
                    .commands_json("session_later", 10, 0)
                    .expect("the commands");
                let row = message
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|command| command["command_id"] == json!(last.0))
                    .expect("the last command");
                assert_eq!(row["message"]["not_taken"], true, "{row}");

                // Its session's end clears it.
                world.ended(store, EndCause::Stopped);
                let ended = read(&mut monitor, store);
                assert!(
                    matches!(&ended[..], [Event::Clear { key, evidence }]
                        if *key == format!("message_not_taken:{}", last.0)
                            && evidence["ended"] == true),
                    "{ended:?}"
                );
            });
        }

        /// Objective 138: a message whose later one of its session was taken
        /// before it fell due never fires, and leaves the pending set.
        #[test]
        fn a_message_whose_later_one_was_taken_first_never_fires() {
            in_schema("messages_later_first", |store| {
                let _world = marked_session(store, "first");
                let exec = "exec_first_1";
                let missed = input(store, "first", "missed", "a");
                typed(store, "first", exec, missed, true);
                // Read within its bound: pending, nothing fired.
                let mut monitor = Monitor::with(super::settings());
                assert!(read(&mut monitor, store).is_empty());
                assert!(
                    monitor
                        .pending
                        .contains_key(&(i64::try_from(missed.0).expect("an id")))
                );
                let next = input(store, "first", "next", "b");
                typed(store, "first", exec, next, true);
                taken(store, "first", exec, "u-next", next.0);
                // Due at once from here on: still nothing.
                let mut prompt = prompt_monitor();
                let fired = read(&mut prompt, store);
                assert!(message_alert(&fired, missed).is_none(), "{fired:?}");
                assert!(fired.is_empty(), "{fired:?}");
                assert!(prompt.pending.is_empty(), "{:?}", prompt.pending);
                // A restarted step reads it again and fires nothing either.
                assert!(read(&mut prompt_monitor(), store).is_empty());
            });
        }

        /// Objective 138 (rev-f R1 F1, N1): another session's later taking
        /// neither clears an open alert nor keeps one from firing; an untyped
        /// message fires though a later message of its session was taken
        /// (it is behind nothing the agent took).
        #[test]
        fn a_later_taking_of_another_session_or_before_the_typing_counts_for_nothing() {
            in_schema("messages_later_other", |store| {
                let _own = marked_session(store, "own");
                let _other = marked_session(store, "other");
                let _behind = marked_session(store, "behind");
                let missed = input(store, "own", "missed", "a");
                typed(store, "own", "exec_own_1", missed, true);
                // Another session takes a later command before the read.
                let elsewhere = input(store, "other", "elsewhere", "b");
                typed(store, "other", "exec_other_1", elsewhere, true);
                taken(store, "other", "exec_other_1", "u-else", elsewhere.0);
                // An untyped message, and a later one of its session typed
                // and taken.
                let untyped = input(store, "behind", "untyped", "c");
                let next = input(store, "behind", "next", "d");
                typed(store, "behind", "exec_behind_1", next, true);
                taken(store, "behind", "exec_behind_1", "u-next", next.0);
                let mut monitor = prompt_monitor();
                let fired = read(&mut monitor, store);
                assert_eq!(
                    fires(&fired),
                    [
                        format!("message_not_taken:{}", missed.0),
                        format!("message_not_taken:{}", untyped.0)
                    ],
                    "{fired:?}"
                );
                commit(&mut monitor, &fired);
                // Another session takes again: the open one stays open.
                let again = input(store, "other", "again", "e");
                typed(store, "other", "exec_other_1", again, true);
                taken(store, "other", "exec_other_1", "u-again", again.0);
                // The untyped one's session takes again: it stays open.
                let more = input(store, "behind", "more", "f");
                typed(store, "behind", "exec_behind_1", more, true);
                taken(store, "behind", "exec_behind_1", "u-more", more.0);
                let read_again = read(&mut monitor, store);
                assert!(read_again.is_empty(), "{read_again:?}");
            });
        }

        /// The loop's own pass, twice, the second a restarted loop (a
        /// session whose transcript held no marker yet tracks nothing): the
        /// first records the alert of a message never taken, the restarted
        /// one reads it back on start, fires nothing again, and clears it
        /// when the message is taken.
        #[test]
        fn a_restarted_loop_reads_its_message_alert_back_and_clears_it_once() {
            in_schema("messages_restart", |store| {
                let session = "msg_restart";
                let mut worker = Session::new(session);
                worker.started(store);
                let execution = "exec_msg_restart_1";
                store
                    .host_record(&json!({"type": "host.messages_marked", "started_at_ms": 1}))
                    .expect("the first marking loop");
                // Its task, typed: until the transcript holds a marker, the
                // session's silence says nothing.
                let task = input(store, session, "task", "hello");
                typed(store, session, execution, task, true);
                let silent = store
                    .commands_json(&format!("session_{session}"), 10, 0)
                    .expect("the commands");
                assert!(
                    silent
                        .as_array()
                        .into_iter()
                        .flatten()
                        .all(|command| command["message"]["not_taken"] == false),
                    "{silent}"
                );
                taken(store, session, execution, "u0", task.0);
                // The next input is tracked, and the engine never takes it.
                let deaf = input(store, session, "deaf", "hello");
                typed(store, session, execution, deaf, true);
                let key = format!("message_not_taken:{}", deaf.0);
                let rows = |store: &PostgresStore| {
                    store.alert_rows().expect("the alerts")["rows"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|row| row["key"] == json!(key))
                        .map(|row| row["state"].as_str().unwrap_or_default().to_string())
                        .collect::<Vec<_>>()
                };

                let mut first = prompt_monitor();
                first.check(store).expect("a pass");
                assert_eq!(rows(store), ["open"]);

                let mut restarted = prompt_monitor();
                restarted.check(store).expect("a restarted pass");
                assert!(restarted.is_open(&key), "read back on start");
                assert_eq!(rows(store), ["open"], "not fired again");

                taken(store, session, execution, "u1", deaf.0);
                restarted.next_reading_ms = 0;
                restarted.check(store).expect("a pass");
                assert_eq!(rows(store), ["cleared"]);
                restarted.next_reading_ms = 0;
                restarted.check(store).expect("a pass");
                assert_eq!(rows(store), ["cleared"], "cleared once");
            });
        }
    }

    #[test]
    fn a_model_login_that_expired_clears_when_its_execution_does() {
        let mut monitor = monitor();
        let interrupted = feed(
            &mut monitor,
            70,
            0,
            json!({"type": "execution.interrupted", "agent_id": "agent_worker_a",
                   "session_id": "session_worker_a", "execution_id": "exec_auth",
                   "cause": "model_auth",
                   "evidence": {"error": "authentication_failed", "status": 401}}),
        );
        assert_eq!(fires(&interrupted), ["model_auth:70"]);
        let Event::Fire {
            subject, evidence, ..
        } = &interrupted[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "session_worker_a");
        assert_eq!(evidence["execution_id"], "exec_auth");
        let recorded = decided(&monitor, interrupted);
        commit(&mut monitor, &recorded);

        let cleared = feed(
            &mut monitor,
            71,
            0,
            json!({"type": "execution.cleared", "execution_id": "exec_auth",
                   "session_id": "session_worker_a", "waited_ms": 9000}),
        );
        let cleared = decided(&monitor, cleared);
        assert!(matches!(&cleared[0], Event::Clear { key, evidence }
            if key == "model_auth:70" && evidence["waited_ms"] == 9000));
    }

    #[test]
    fn evidence_never_holds_a_url_password() {
        let secret = "postgres://spawn:pa55w0rd@db:5432/spawn_test";
        let scrubbed = scrub(&format!("cannot connect to {secret}: refused"));
        assert!(!scrubbed.contains("pa55w0rd"), "{scrubbed}");
        assert!(scrubbed.contains("postgres://spawn:***@db:5432/spawn_test"));
        let value = scrub_value(&json!({"error": format!("{secret} refused"), "n": 3}));
        assert!(!value.to_string().contains("pa55w0rd"), "{value}");
        assert_eq!(value["n"], 3);
        // A URL without a password, and plain text, are left as they are.
        assert_eq!(
            scrub("see postgres://db:5432/x"),
            "see postgres://db:5432/x"
        );
        assert_eq!(scrub("no url here"), "no url here");
    }

    #[test]
    fn the_text_form_names_the_kind_and_the_subject_of_each_alert() {
        let alerts = json!([
            {"key": "mission_halted:4:9", "kind": "mission_halted", "subject": "mission #4",
             "state": "open", "fired_at_ms": 1_760_000_000_000_i64,
             "text": "mission #4 halted: three rejections"},
            {"key": "task_state:7:10", "kind": "task_state", "subject": "task #7",
             "state": "notice", "fired_at_ms": 1_760_000_000_000_i64,
             "text": "task #7: none -> assigned (by task.assigned)"},
        ]);
        let lines: Vec<String> = alerts.as_array().unwrap().iter().map(line).collect();
        assert!(lines[0].contains("mission_halted") && lines[0].contains("mission #4"));
        assert!(lines[0].starts_with("open "));
        assert!(lines[1].contains("task_state") && lines[1].contains("task #7"));
        assert!(lines[1].starts_with("notice "));
        // One line per alert, and the same array the API answers: both are
        // `window`'s answer, shaped by the store's one method.
        assert_eq!(lines.len(), alerts.as_array().unwrap().len());
    }

    #[test]
    fn the_window_is_a_span_or_the_configured_default() {
        assert_eq!(window_ms(Some("90s")), Ok(90_000));
        assert_eq!(window_ms(Some("30m")), Ok(1_800_000));
        assert_eq!(window_ms(Some("2h")), Ok(7_200_000));
        assert_eq!(window_ms(Some("1d")), Ok(86_400_000));
        assert_eq!(window_ms(Some("1500")), Ok(1500));
        assert_eq!(parse_span("500ms"), Some(500));
        assert!(window_ms(Some("soon")).is_err());
    }

    #[test]
    fn a_time_is_rendered_in_utc() {
        let (year, month, day, hour, minute, second) = civil(1_760_000_000_000);
        assert_eq!((year, month, day), (2025, 10, 9));
        assert_eq!((hour, minute, second), (8, 53, 20));
        assert_eq!(time_text(0), "1970-01-01 00:00:00");
    }

    /// Objective 074: a database that outgrows what the live event log
    /// justifies (at or above the floor, above the ratio times the log's
    /// bytes) opens one `database_size` alert; it clears at the first
    /// reading that finds it no longer so.
    mod database_size {
        use super::super::{Event, Monitor, Settings, Url, ratio_of, size_text};
        use super::{commit, decided, fires, monitor, settings};
        use serde_json::{Value, json};

        const MB: u64 = 1024 * 1024;

        fn live(bytes: u64, log_bytes: u64) -> Value {
            json!({"name": "spawn", "bytes": bytes, "schemas": 0, "log_bytes": log_bytes,
                      "tables": []})
        }

        fn test_db(name: &str, bytes: u64, schemas: u64) -> Value {
            json!({"name": name, "bytes": bytes, "schemas": schemas})
        }

        /// One reading at `now`: what it decides, and its lines.
        fn read(
            monitor: &Monitor,
            now: i64,
            live: Result<Value, String>,
            tests: &[(&'static str, Result<Value, String>)],
        ) -> (Vec<Event>, Vec<String>) {
            let (mut out, mut lines) = (Vec::new(), Vec::new());
            monitor.database_events(now, live, tests, &mut out, &mut lines);
            (decided(monitor, out), lines)
        }

        #[test]
        fn the_settings_default_to_ten_minutes_a_gigabyte_and_ten_times() {
            let (from_env, _) = Settings::from_env();
            if std::env::var_os("SPAWN_ALERT_DATABASE_SECS").is_none() {
                assert_eq!(from_env.database_ms, 600_000);
            }
            if std::env::var_os("SPAWN_ALERT_DATABASE_FLOOR_MB").is_none() {
                assert_eq!(from_env.database_floor_mb, 1024);
            }
            if std::env::var_os("SPAWN_ALERT_DATABASE_RATIO").is_none() {
                assert_eq!(from_env.database_ratio, 10);
            }
            // The gates' first, each only when set; the URL never shown.
            let roles: Vec<&str> = from_env
                .test_databases
                .iter()
                .map(|(role, _)| *role)
                .collect();
            let mut want = Vec::new();
            if std::env::var("SPAWN_TEST_DATABASE_URL").is_ok_and(|url| !url.trim().is_empty()) {
                want.push("test");
            }
            if std::env::var("SPAWN_FLEET_TEST_DATABASE_URL")
                .is_ok_and(|url| !url.trim().is_empty())
            {
                want.push("fleet_test");
            }
            assert_eq!(roles, want);
            let shown = format!("{:?}", Url("postgres://u:secret@h/d".to_string()));
            assert!(!shown.contains("secret"), "{shown}");
        }

        #[test]
        fn the_ratio_and_the_size_are_said_to_one_decimal() {
            assert_eq!(ratio_of(253, 10), Some(25.3));
            assert_eq!(ratio_of(255, 10), Some(25.5));
            assert_eq!(ratio_of(2, 3), Some(0.7));
            assert_eq!(ratio_of(5, 0), None, "an empty log has no ratio");
            assert_eq!(size_text(12 * MB), "12.0 MB");
            assert_eq!(size_text(1024 * MB), "1.0 GB");
            assert_eq!(size_text(2765 * MB), "2.7 GB");
        }

        #[test]
        fn a_database_under_the_floor_fires_nothing_whatever_its_ratio() {
            let monitor = monitor();
            // 1023 MB against a 1 MB log: 1023 times, but under 1024 MB.
            let (events, lines) = read(
                &monitor,
                1000,
                Ok(live(50 * MB, MB)),
                &[(
                    "fleet_test",
                    Ok(test_db("spawn_fleet_test", 1023 * MB, 900)),
                )],
            );
            assert!(events.is_empty(), "{events:?}");
            assert!(lines.is_empty(), "{lines:?}");
        }

        #[test]
        fn a_database_within_the_ratio_fires_nothing_however_large() {
            let monitor = monitor();
            // 3 GB live for a 400 MB log: over the floor, 7.7 times.
            let (events, _) = read(&monitor, 1000, Ok(live(3 * 1024 * MB, 400 * MB)), &[]);
            assert!(events.is_empty(), "{events:?}");
            // Exactly ten times is within.
            let (events, _) = read(&monitor, 1000, Ok(live(4000 * MB, 400 * MB)), &[]);
            assert!(events.is_empty(), "{events:?}");
        }

        #[test]
        fn over_both_opens_one_alert_with_its_evidence_and_words() {
            let mut monitor = monitor();
            let fleet = 2765 * MB;
            let log = 35 * MB;
            let (events, lines) = read(
                &monitor,
                1000,
                Ok(live(159 * MB, log)),
                &[
                    ("test", Ok(test_db("spawn_test", 20 * MB, 3))),
                    ("fleet_test", Ok(test_db("spawn_fleet_test", fleet, 2533))),
                ],
            );
            assert!(lines.is_empty(), "{lines:?}");
            assert_eq!(fires(&events), ["database_size:spawn_fleet_test:1000"]);
            let Event::Fire {
                kind,
                subject,
                evidence,
                text,
                notice,
                ..
            } = &events[0]
            else {
                panic!("a fire: {events:?}");
            };
            assert_eq!(*kind, "database_size");
            assert_eq!(subject, "database spawn_fleet_test");
            assert!(!notice);
            assert_eq!(evidence["database"], "spawn_fleet_test");
            assert_eq!(evidence["role"], "fleet_test");
            assert_eq!(evidence["bytes"], json!(fleet));
            assert_eq!(evidence["log_bytes"], json!(log));
            assert_eq!(evidence["ratio"], json!(79.0));
            assert_eq!(evidence["schemas"], 2533);
            assert_eq!(evidence["reading_ms"], 1000);
            for word in ["spawn_fleet_test", "2.7 GB", "79", "2533 schemas"] {
                assert!(text.contains(word), "{word} in {text}");
            }
            // One open per database: the next reading, still over, fires
            // nothing.
            commit(&mut monitor, &events);
            let (again, _) = read(
                &monitor,
                2000,
                Ok(live(159 * MB, log)),
                &[(
                    "fleet_test",
                    Ok(test_db("spawn_fleet_test", fleet + MB, 2534)),
                )],
            );
            assert!(again.is_empty(), "{again:?}");
            // Another database over both opens its own.
            let (other, _) = read(
                &monitor,
                3000,
                Ok(live(159 * MB, log)),
                &[
                    ("test", Ok(test_db("spawn_test", 2048 * MB, 40))),
                    ("fleet_test", Ok(test_db("spawn_fleet_test", fleet, 2533))),
                ],
            );
            assert_eq!(fires(&other), ["database_size:spawn_test:3000"]);
        }

        #[test]
        fn it_clears_at_the_first_reading_no_longer_over_with_that_reading() {
            let mut monitor = monitor();
            let log = 35 * MB;
            let over = [(
                "fleet_test",
                Ok(test_db("spawn_fleet_test", 2765 * MB, 2533)),
            )];
            let (events, _) = read(&monitor, 1000, Ok(live(159 * MB, log)), &over);
            commit(&mut monitor, &events);
            // Still over at the next reading: nothing.
            let (events, _) = read(&monitor, 2000, Ok(live(159 * MB, log)), &over);
            assert!(events.is_empty());
            // The schemas dropped: it clears, the reading its evidence.
            let (events, _) = read(
                &monitor,
                3000,
                Ok(live(159 * MB, log)),
                &[("fleet_test", Ok(test_db("spawn_fleet_test", 300 * MB, 5)))],
            );
            assert_eq!(events.len(), 1, "{events:?}");
            let Event::Clear { key, evidence } = &events[0] else {
                panic!("a clear: {events:?}");
            };
            assert_eq!(key, "database_size:spawn_fleet_test:1000");
            assert_eq!(evidence["schemas"], 5);
            assert_eq!(evidence["bytes"], json!(300 * MB));
            assert_eq!(evidence["log_bytes"], json!(log));
            assert_eq!(evidence["reading_ms"], 3000);
            commit(&mut monitor, &events);
            let (events, _) = read(
                &monitor,
                4000,
                Ok(live(159 * MB, log)),
                &[("fleet_test", Ok(test_db("spawn_fleet_test", 300 * MB, 5)))],
            );
            assert!(events.is_empty(), "cleared once: {events:?}");
            // A larger log clears it too: the data now justifies the size.
            let (events, _) = read(&monitor, 5000, Ok(live(159 * MB, log)), &over);
            assert_eq!(fires(&events), ["database_size:spawn_fleet_test:5000"]);
            commit(&mut monitor, &events);
            let (events, _) = read(&monitor, 6000, Ok(live(400 * MB, 300 * MB)), &over);
            assert!(matches!(&events[..], [Event::Clear { .. }]), "{events:?}");
        }

        #[test]
        fn an_unreachable_test_database_is_a_line_and_its_alert_stays() {
            let mut monitor = monitor();
            let log = 35 * MB;
            let over = [(
                "fleet_test",
                Ok(test_db("spawn_fleet_test", 2765 * MB, 2533)),
            )];
            let (events, _) = read(&monitor, 1000, Ok(live(159 * MB, log)), &over);
            commit(&mut monitor, &events);
            let down = [
                (
                    "test",
                    Err("error connecting to server: Connection refused".to_string()),
                ),
                ("fleet_test", Err("db error: timeout".to_string())),
            ];
            let (events, lines) = read(&monitor, 2000, Ok(live(159 * MB, log)), &down);
            assert!(events.is_empty(), "no alert, no clear: {events:?}");
            assert_eq!(lines.len(), 2, "{lines:?}");
            assert!(lines[0].contains("the test database's size"), "{lines:?}");
            assert!(
                lines[1].contains("the fleet_test database's size"),
                "{lines:?}"
            );
            // Without the live reading nothing is decided.
            let (events, lines) = read(
                &monitor,
                3000,
                Err("down".to_string()),
                &[("fleet_test", Ok(test_db("spawn_fleet_test", MB, 0)))],
            );
            assert!(events.is_empty(), "{events:?}");
            assert_eq!(lines.len(), 1, "{lines:?}");
        }

        #[test]
        fn with_each_url_unset_only_the_others_are_read() {
            let monitor = monitor();
            let log = MB;
            // Neither set: the live database alone, which may fire itself.
            let (events, lines) = read(&monitor, 1000, Ok(live(2048 * MB, log)), &[]);
            assert_eq!(fires(&events), ["database_size:spawn:1000"]);
            assert!(lines.is_empty());
            let Event::Fire { evidence, .. } = &events[0] else {
                panic!("a fire");
            };
            assert_eq!(evidence["role"], "live");
            // The fleet's unset: the gates' alone.
            let (events, _) = read(
                &monitor,
                1000,
                Ok(live(10 * MB, log)),
                &[("test", Ok(test_db("spawn_test", 2048 * MB, 12)))],
            );
            assert_eq!(fires(&events), ["database_size:spawn_test:1000"]);
            // The gates' unset: the fleet's alone.
            let (events, _) = read(
                &monitor,
                1000,
                Ok(live(10 * MB, log)),
                &[("fleet_test", Ok(test_db("spawn_fleet_test", 2048 * MB, 12)))],
            );
            assert_eq!(fires(&events), ["database_size:spawn_fleet_test:1000"]);
            // The step reads only the URLs it is given.
            let given = Monitor::with(Settings {
                test_databases: vec![("fleet_test", Url("postgres://x@127.0.0.1:1/none".into()))],
                ..settings()
            });
            assert_eq!(given.settings.test_databases.len(), 1);
            assert!(monitor.settings.test_databases.is_empty());
        }

        /// Through the real store: the live reading, and a test database
        /// that cannot be reached (a line, no alert).
        #[test]
        fn through_the_store_an_unreachable_test_database_is_a_line() {
            let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
                "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
            );
            let schema = spawn_postgres::testing::TestSchema::new("alert_dbsize");
            let store = spawn_postgres::PostgresStore::connect_in_schema(&url, &schema)
                .expect("a schema of its own, migrated");
            let mut monitor = Monitor::with(Settings {
                test_databases: vec![
                    ("test", Url(url.clone())),
                    (
                        "fleet_test",
                        Url("postgres://spawn:secret-074@127.0.0.1:1/none".into()),
                    ),
                ],
                database_floor_mb: 0,
                database_ratio: u64::MAX,
                ..settings()
            });
            store
                .host_record(&json!({"type": "host.storage", "archive_bytes": 1,
                    "archive_files": 1, "projects_bytes": 1, "projects_files": 1}))
                .expect("a record in the log");
            let (mut out, mut lines) = (Vec::new(), Vec::new());
            monitor.read_databases(&store, 1000, &mut out, &mut lines);
            assert!(out.is_empty(), "within u64::MAX times: {out:?}");
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(
                lines[0].starts_with("alerts: the fleet_test database's size: "),
                "{lines:?}"
            );
            assert!(!lines[0].contains("secret-074"), "{lines:?}");
            // At a ratio of 0 and no floor the live database fires (the
            // gates' is the same database here: one alert for it).
            monitor.settings.database_ratio = 0;
            let (mut out, mut lines) = (Vec::new(), Vec::new());
            monitor.read_databases(&store, 2000, &mut out, &mut lines);
            assert_eq!(out.len(), 1, "{out:?}");
            let Event::Fire { evidence, .. } = &out[0] else {
                panic!("a fire: {out:?}");
            };
            assert_eq!(evidence["role"], "live");
            assert!(evidence["log_bytes"].as_u64().is_some(), "{evidence}");
            drop(store);
        }
    }
}
