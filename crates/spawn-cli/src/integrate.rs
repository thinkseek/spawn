//! `spawn integrate`: the one landing path (docs/fleet-architecture.md
//! §10.3).
//!
//! A pass first records, before any mission's step, `source.hold_expired`
//! for every owner's hold whose recorded `expires_ms` the database's clock
//! (`spawn_now_ms()`) has reached ([`expire_holds`]; the hold, below at 3).
//! Only that record ends a hold: past `expires_ms` without it nothing lands,
//! and a hold that expired while the integrator was down is recorded at its
//! first pass, before anything lands. Then, for every open mission with a
//! gate, and every done one while the closure rule
//! (`spawn_task_may_continue`) lets its work go on, one step:
//!
//! 1. a halt rule that fired stops the mission's landings until a human
//!    resumes it: three gate rejections in a row, three aborts in a row
//!    (neither a `no_room` nor a `host_fault` counts toward either, and no
//!    rule counts host faults: a halt needs a human), the
//!    scenario count below its floor, a remote source (agents' branches
//!    never reach it), or the source's main not where the last landing left
//!    it. That last one is read only from a fetch that succeeded this pass,
//!    and only while no other mission's landing on the source is unresolved
//!    (its push may have moved main before its record);
//! 2. a delivered task over its mission's budget is aborted and its worker
//!    stopped;
//! 3. a task whose gate passed is landed (again, after a crash), unless the
//!    owner holds its source (`spawn mission hold`, a `source.held` record):
//!    [`land`] reads the hold from the store under the landing lock, before
//!    its push, never from memory, and on a held source pushes nothing. A
//!    candidate already under main landed before the hold and is recorded
//!    `task.landed`; any other records `task.held` (once per task and hold)
//!    and stays `landing`, so a hold never splits a push from its record.
//!    A task waiting behind the hold ends nothing early: the step goes on to
//!    the baseline check and the assignment (5), its source is not counted
//!    as moved, and the mission's other ready tasks wait behind it. After a
//!    record ends the hold (`source.released`, `source.hold_expired`) it
//!    lands at the next pass by the rules below (`stale` and judged again
//!    when main moved meanwhile);
//! 4. otherwise the next ready task is judged: accepted by the supervisor
//!    (`accept` = review), or also just reported (`gate`). The candidate is
//!    what the agent published: the source's `refs/heads/spawn/<agent>`
//!    (pushed by the Bridge's pusher, [`crate::push`]), fetched into this mirror's
//!    own `refs/spawn-integrate/<agent>`. The integrator never commits a
//!    mounted workspace and never moves the agent's `refs/heads/<agent>`,
//!    so it works the same on a host that never ran the agent, and a file
//!    written after the last checkpoint never lands.
//!
//!    **A landing is one commit per task on main's first-parent line.**
//!    The candidate is a new merge commit C ([`landing_candidate`]), never
//!    the worker's commit and never a fast-forward to one: first parent the
//!    source's main (the task's base), second parent the pushed head
//!    judged fresh; its tree is the head's when main is in it, and the merge
//!    by main's merge rules when main moved (a conflict is `conflict`). Its
//!    message ([`landing_message`]) says what landed, from the record alone:
//!    the objective's title with the objective and the task on the first
//!    line (a task without one: its brief's first line and the task), the
//!    report's `## What changed` (or the whole report, bounded) after a
//!    blank line, with no line of it able to pass for a `Spawn-` trailer,
//!    and the integrator's `Spawn-Task`, `Spawn-Objective` and `Spawn-Head`
//!    trailers. C is judged by `scripts/rsi/gate.sh` with the mission's
//!    scenarios, named in the accepted `task.gated` (`candidate`), pushed
//!    as it is, and named in `task.landed` (`to`). So `git log
//!    --first-parent` is one line per landed task, `git bisect start
//!    --first-parent` visits only gated landings, `git revert -m 1 <C>`
//!    undoes one task, and every checkpoint stays reachable from main
//!    through the second parents. Every verdict
//!    names the revision of the report it judged (the task's `revisions`),
//!    and the store refuses it once the task has moved on. A pass records
//!    the verdict before git moves, then moves the source's main with a
//!    compare-and-swap push and records the landing. A rejection sends the
//!    task back to its worker with the gate's output as the note;
//!
//!    **A head broken only by main's moves is sent back, not rejected.**
//!    When the gate rejects C and C is a real merge (the pushed head does
//!    not contain main, so C's tree is not the head's), the integrator
//!    judges the head alone before it records anything ([`rejection`]): the
//!    same gate, scenarios and task scenario, base `head_base` = `git
//!    merge-base <head> <main>` (the main the head last took in), candidate
//!    the pushed head itself. The head passes: `incompatible`, which, like
//!    `conflict`, sends the task back (`revise`, one more revision) and does
//!    not count toward the three-rejections halt (`gate_rejections` stays,
//!    `gate_aborts` resets as on `conflict`). Its record names `candidate`
//!    (C), `head`, `head_base`, `head_verdict` (`accepted`), `main_commits`
//!    (main's first-parent commits in `head_base..base`, oldest first: what
//!    the work must adapt to), C's `reason` and `log`, and a `note` that
//!    quotes C's gate output and lists those commits by hash and subject;
//!    the dispatcher's delivery of the revision merges main into the idle
//!    workspace, as every delivery does. The head fails too: `rejected`, as
//!    before, counted, naming `head`, `head_base` and `head_verdict`
//!    (`rejected`). The head's run gives no verdict: `aborted` (or
//!    `no_room`) naming the head, never `incompatible` on no evidence. When
//!    the head contains main, a rejection of C is a rejection, with no
//!    second run. The decision rests on two recorded gate verdicts on named
//!    commits, never on timing or the text of a reason. Cost: the second
//!    gate run happens only when C is rejected and is a real merge, and
//!    doubles that pass's gate time; nothing else changes;
//!
//!    **Stale pushes do not land.** The pushed head must be the commit of
//!    the agent's latest `artifact.committed` event (its last checkpoint or
//!    merge of main) or a descendant of it. A push that lags (behind,
//!    diverged, missing that commit, or no `spawn/<agent>` at all while the
//!    agent has a checkpoint) is waited for briefly ([`PUSH_WAIT`], the
//!    pusher coalesces pushes), then gated `stale`: the gate does not run,
//!    the task goes back to `reported` (`accepted` under review), no
//!    rejection is counted, nothing reaches the worker, and a later pass
//!    judges again. An agent with neither a checkpoint nor a pushed branch
//!    has no branch to land;
//!
//!    **No room is not a verdict.** A gate that finds TMPDIR short of room
//!    (exit [`gate::NO_ROOM_EXIT`] with its no-room fact,
//!    [`GateFailure::NoRoom`]) is gated `no_room`: the task stays where it
//!    was, the verdict names the free space and TMPDIR, and neither the
//!    abort nor the rejection count moves. The store folds the mission's
//!    run of them (`gate_room_*`); the next try waits [`room_backoff`] after
//!    the latest, and the first `no_room` more than [`ROOM_ESCALATE_AFTER`]
//!    into a run carries `escalate`, which the dispatcher's digest takes to
//!    the supervisor once ([`crate::disk::gate_room_escalations`]). Every
//!    other gate failure, the gate's own exit 3 included (a missing bind
//!    directory, a bad SPAWN_GATE_MIN_FREE_GB), is an abort and counts;
//!
//!    **A host fault is not a verdict.** When the gate rejects C with
//!    [`host::CHECK_TESTS_REGRESSION`], the integrator reads the failures
//!    check-tests.sh printed in the gate's recorded output
//!    ([`host::check_tests_failures`]: its `FAILED <test>: <message>` lines
//!    when the section holds one, else the panics and `Error:` lines of its
//!    excerpt). A failure is the host's when its message is a connection
//!    error to the test database ([`host::CONNECTION_ERRORS`]), or a
//!    database error of any kind (`db error`, `Error { kind: Db`) while the
//!    test database's log (`SPAWN_TEST_DATABASE_LOG`, optional; unset or
//!    unreadable is no evidence, said once) shows a crash stamped within the
//!    gate's run as this process measured it, give or take a second
//!    ([`host::crash_lines`]). At least one failure read, and every one the
//!    host's: `host_fault`, not `rejected`, and C's head is not judged
//!    alone. A mission whose gate needs no test database
//!    ([`NO_TEST_DATABASE`]) has no host faults: its rejections stay
//!    rejections. The task stays where it was, its revisions and note do not
//!    move, nothing goes back to the worker, and neither `gate_rejections`
//!    nor `gate_aborts` grows or resets. The record names `candidate`,
//!    `reason` (`host fault: ...`, saying which evidence), `log`,
//!    `evidence` (`failures` when every failure is a connection error, else
//!    `server_log`), `failures` (each `test`, first message line, `class`),
//!    `crash` (the log lines, when used), `host_faults` (the mission's count
//!    in a row, this one included, folded by the store as
//!    `gate_host_faults`), `backoff_ms` ([`host_fault_backoff`]:
//!    `SPAWN_GATE_HOST_FAULT_BACKOFF_SECS`, 300 by default, doubled per host
//!    fault in a row, at most an hour), `due_ms` (the record's own time plus
//!    the backoff, written by the store) and `escalate` (the second in a
//!    row only; the dispatcher's digest takes it to the supervisor once,
//!    [`crate::dispatch::digest::host_fault_escalations`]). No pass gates
//!    the task before `due_ms` ([`host_wait`], read from the store, never
//!    from memory; its mission's other tasks wait behind it). A real failure
//!    among them, or none read, keeps it `rejected`; any other rejection is
//!    judged as before. The decision rests on the failures and the crash
//!    lines named in the record, never on the reason's text alone; the wait
//!    rests on the recorded `due_ms`;
//!
//!    **The host's room is measured before the gate, never learned from
//!    its crash** ([`gate::Room`]). At the start of each judging, after the
//!    candidate is built and before the gate starts (only for a task that
//!    got a gate slot: a task that waits is neither checked nor recorded),
//!    the integrator checks, in this order, stopping at the first that falls
//!    short: the free KiB (`df`) of the gate's TMPDIR against
//!    SPAWN_GATE_MIN_FREE_GB (default 10, the floor `scripts/disk.sh` reads:
//!    one floor, one meaning); `MemAvailable` in MiB against
//!    SPAWN_GATE_MIN_FREE_MEMORY_MB (default 2048, the integrator's only);
//!    and, when SPAWN_TEST_DATABASE_URL is set in the integrator's
//!    environment (whatever [`gate::withhold_owners_cluster`] then decides
//!    for the gate), a connection to the test database within 10 s and a
//!    forced parallel query (a Gather with a worker, so its dynamic shared
//!    memory is exercised) within 10 s; not for a mission marked
//!    [`NO_TEST_DATABASE`], whose gate never gets the URL, so a test server
//!    that is down never holds back its work. A check that falls short runs no
//!    gate: the task is gated `no_room`, the path above, unchanged in its
//!    meaning (no verdict, no count, the same `gate_room_*` run, backoff and
//!    escalation), and the record names `check` (`disk`, `memory`,
//!    `database`), `measured` and `threshold` (free KiB and the floor in
//!    KiB, with `free_kb`, `min_gb` and `tmpdir`; available MiB and the
//!    floor in MiB; the error and the probe, with the server named
//!    host:port/database, never the URL's password), and a reason that names
//!    the three ([`short_of_room`]). A threshold of 0 disables its check; one
//!    that does not parse is an abort. Every check met: the gate runs as
//!    before, its own floor and exit 5 included. The decision rests on
//!    measures taken at the judging, recorded with their thresholds (design
//!    §14): a host whose test database cannot run a parallel query is found
//!    in seconds, not by a gate whose every database test fails 25 minutes
//!    in and reads as a regression;
//!
//!    **A report is judged against the checkpoint of the turn that produced
//!    it.** The dispatcher collects a report only after the turn that wrote
//!    it ended in the log, and the Bridge records a turn's checkpoint before
//!    its end; the `task.reported` record names that checkpoint in `commit`.
//!    The pushed head must also be that commit or a descendant of it
//!    ([`report_freshness`]); otherwise the verdict is `stale`, as above. A
//!    report that names a commit is never judged for lack of a checkpoint
//!    (the branch pushed when the workspace was mounted is only the task's
//!    base). A report with a null commit (a record from before reports named
//!    one) is judged by the rule above alone;
//! 5. an objective mission's free slots are filled ([`objectives`]): it keeps
//!    up to its worker limit N (`--workers`, default 1) of its tasks in
//!    flight, each on its own agent (its resident, then per-task workers),
//!    and never two for one objective. Filling happens on every pass, for
//!    every mission that is not halted, whatever its gate or any other
//!    mission's gate is doing. A halted mission gets no new task.
//!
//! **Every mission gets its step in every pass.** A pass visits every
//! mission: halts, budgets, landings and assignments never wait for another
//! mission. Only a landing ends anything early: it moves its source's main
//! and the baselines of the missions on it, so the rest of that source
//! waits for the next pass, which reads everything again; other sources go
//! on. A task waiting behind the owner's hold is no landing: it moved
//! nothing, and once its `task.held` names the active hold it is no landing
//! in flight either ([`landing_elsewhere`]), so the other missions on the
//! held source are judged as usual (their accepted tasks wait too). The ready tasks the pass found are given gates after every mission
//! had its step ([`Integrator::start_ready`]): the one that has waited
//! longest first, from when it was reported or from when its mission's
//! previous judging ended, whichever is later. The mission id decides
//! nothing, so two missions that are always ready take turns.
//!
//! **Gates run per mission, on threads.** A judging (step 4, from the
//! fetch of the pushed branch to the landing) runs on a thread of its own:
//! - a mission has at most one judging in flight, its lowest ready task:
//!   its tasks stay serial, each landing on the main the next is judged
//!   against. While it runs, the pass leaves the mirror of that source
//!   alone (no landing, fetch or judging for the mission or any other on
//!   its source: they share the mirror and the main it judges against);
//!   halts, budgets and assignments go on;
//! - at most `--gates N` (`SPAWN_INTEGRATE_GATES`) judgings run at once
//!   across missions, and N is [`DEFAULT_GATES`], one: the gates share one
//!   test database server whose gate login is a superuser, so a second
//!   gate's candidate could disturb the first's base runs. N up to
//!   [`MAX_GATES`] is allowed only with [`PER_GATE_SERVER`]`=1`, once each
//!   gate has a server of its own. A ready task over the cap waits for a
//!   later pass (said once). A mission whose scenarios directory holds
//!   [`NO_TEST_DATABASE`] gets no SPAWN_TEST_DATABASE_URL, so its judgings
//!   cannot reach the shared server and do not count against the cap: they
//!   run beside the others, still one per mission and per source.
//!
//! Every judging records its own outcome (`task.gated`, `task.landed`), as
//! before; the pass reads those records, never the thread's result. A
//! finished thread is reaped only at the start of a pass, before the
//! missions are read, so no pass acts on a snapshot older than a judging's
//! records. A judging that panics before any record of its own is recorded
//! `aborted` ([`judged_or_aborted`]), so the three-aborts halt bounds a bug.
//! One that returns an error records nothing, as before threads: the error
//! is the operator's to read (a git that cannot run a merge is the host's,
//! not the work's; objective 008), and a later pass judges the task again. A restart mid-gate behaves as before: the process (and the
//! gate under it) dies before any record, so the next integrator judges the
//! task again.
//!
//! Like the dispatcher, it keeps no state of its own beyond the gates it is
//! running: the control stream says what was judged and landed. An advisory
//! lock allows one integrator per database; it belongs to the connection,
//! which the integrator keeps while any of its gates runs.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::mission::TaskState;
use spawn_core::port::CommandQueue;
use spawn_host::GitWorkspace;
use spawn_postgres::PostgresStore;

use crate::config::{loop_interval, workspace_root};
use crate::gate::{self, GateFailure, RoomFailure, Shortfall};
use crate::{Flags, env_or, open_store};
use crate::{main_merge, objectives, sources};

mod host;

/// Rejections, or aborts, in a row that halt a mission.
const HALT_AFTER: u32 = 3;

/// The wait before the first retry of a gate that had no room; it doubles
/// with each `no_room` in a row, up to [`ROOM_BACKOFF_MAX`].
const ROOM_BACKOFF: Duration = Duration::from_mins(5);

/// The longest wait between two tries of a gate that had no room.
const ROOM_BACKOFF_MAX: Duration = Duration::from_mins(40);

/// How long a mission's gate may go without room before the supervisor is
/// told, once per run.
const ROOM_ESCALATE_AFTER: Duration = Duration::from_hours(2);

/// The wait after the first host fault in a row, unless
/// `SPAWN_GATE_HOST_FAULT_BACKOFF_SECS` says otherwise; it doubles with each
/// host fault in a row, up to [`HOST_FAULT_BACKOFF_MAX`].
const HOST_FAULT_BACKOFF: Duration = Duration::from_mins(5);

/// The longest wait after a host fault.
const HOST_FAULT_BACKOFF_MAX: Duration = Duration::from_hours(1);

/// The host fault in a row that is escalated to the supervisor (once per
/// run: the later ones are not).
const HOST_FAULT_ESCALATE_AT: u32 = 2;

/// The test database server's log (`log_line_prefix` `%m [%p] `), read for
/// a crash in a gate's window. Optional.
const TEST_DATABASE_LOG: &str = "SPAWN_TEST_DATABASE_LOG";

/// Wall clock of one gate run: `SPAWN_GATE_WALL_CLOCK_MINUTES`, 120 by
/// default. The gate grows with every objective whose scenario it runs (an
/// unsolved one runs twice on the base), so the bound is a setting: on
/// 2026-11-06 a fixed 45 minutes aborted a healthy gate still in its
/// regression phase, and three such aborts halt a mission. Keep it below the
/// scratch age at which spawn-maintenance removes a gate's leftovers
/// (SPAWN_MAINTENANCE_SCRATCH_AGE_MIN, 180 in
/// deploy/systemd/spawn-maintenance.service), or a live gate's scratch could go.
fn gate_wall_clock() -> Duration {
    wall_clock_minutes(crate::config::env_value("SPAWN_GATE_WALL_CLOCK_MINUTES").as_deref())
}

/// [`gate_wall_clock`] from the setting's text: a whole number of minutes
/// from 1 up; anything else is the default.
fn wall_clock_minutes(text: Option<&str>) -> Duration {
    const DEFAULT_MINUTES: u64 = 120;
    let minutes = text
        .and_then(|text| text.trim().parse::<u64>().ok())
        .filter(|minutes| *minutes > 0)
        .unwrap_or(DEFAULT_MINUTES);
    Duration::from_mins(minutes)
}

/// Guard against runaway candidates.
const MAX_PATCH_BYTES: usize = 1 << 20;

/// Characters of gate output quoted back to the worker.
const NOTE_LOG_CHARS: usize = 4000;

/// How long a pass waits for a pushed branch that lags the agent's latest
/// checkpoint before it gates the task `stale`: the pusher pushes a branch at
/// most once per [`crate::push::INTERVAL`].
const PUSH_WAIT: Duration = Duration::from_secs(12);

/// Pause between two looks at a lagging pushed branch.
const PUSH_POLL: Duration = Duration::from_millis(500);

/// Who the integrator's commands come from.
const PRINCIPAL: &str = "spawn-integrate";

const USAGE: &str = "usage: spawn integrate [--once] [--root <dir>] [--gates <n>]";

/// Gates that run at once, across missions, unless `--gates` or
/// `SPAWN_INTEGRATE_GATES` says otherwise. One: concurrent gates share one
/// test PostgreSQL server whose gate login is a superuser, so one gate's
/// candidate code could disturb another gate's base runs (and an
/// "unsolved" would be cached) or its candidate's checks.
pub(crate) const DEFAULT_GATES: usize = 1;

/// The opt-in that allows more than one gate: set to `1` only when each
/// gate gets a test database server of its own.
pub(crate) const PER_GATE_SERVER: &str = "SPAWN_TEST_DATABASE_PER_GATE";

/// The most gates `--gates` allows: each holds two trees, their builds and
/// a share of the test database.
pub(crate) const MAX_GATES: usize = 8;

/// How many gates may run at once: `--gates`, else `SPAWN_INTEGRATE_GATES`,
/// else [`DEFAULT_GATES`]; a whole number from 1 to [`MAX_GATES`], and more
/// than one only with [`PER_GATE_SERVER`]`=1` (`per_gate`).
pub(crate) fn gate_slots(
    flag: Option<String>,
    env: Option<String>,
    per_gate: Option<&str>,
) -> Result<usize, String> {
    let Some((name, text)) = flag
        .map(|text| ("--gates", text))
        .or_else(|| env.map(|text| ("SPAWN_INTEGRATE_GATES", text)))
    else {
        return Ok(DEFAULT_GATES);
    };
    match text.trim().parse::<usize>() {
        Ok(1) => Ok(1),
        Ok(slots) if (2..=MAX_GATES).contains(&slots) => {
            if per_gate.map(str::trim) == Some("1") {
                Ok(slots)
            } else {
                Err(format!(
                    "{name}={slots}: more than one gate at once is unsafe while the gates share \
                     one test database server (its gate login is a superuser); set \
                     {PER_GATE_SERVER}=1 only once each gate has a server of its own"
                ))
            }
        }
        _ => Err(format!(
            "{name} is a whole number from 1 to {MAX_GATES}, not '{text}'"
        )),
    }
}

/// One mission with a gate the integrator visits, from `gated_missions`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Mission {
    pub(crate) id: i64,
    /// `open` or `active`; `done` while the closure rule lets its work go
    /// on (`spawn_task_may_continue`).
    #[serde(default)]
    pub(crate) state: String,
    pub(crate) gate: String,
    pub(crate) accept: String,
    #[serde(default)]
    pub(crate) budget: Option<Value>,
    #[serde(default)]
    pub(crate) baseline: Option<String>,
    #[serde(default)]
    pub(crate) halted: Option<String>,
    #[serde(default)]
    pub(crate) scenario_floor: u32,
    #[serde(default)]
    pub(crate) gate_rejections: u32,
    #[serde(default)]
    pub(crate) gate_aborts: u32,
    /// The mission's current run of `no_room` gate outcomes, folded by the
    /// store: the task it is about (a run is one task's; it ends when that
    /// task leaves the gate), when it began, when the latest was recorded,
    /// how many, and whether one carried `escalate`.
    #[serde(default)]
    pub(crate) gate_room_task: Option<i64>,
    #[serde(default)]
    pub(crate) gate_room_since_ms: Option<i64>,
    #[serde(default)]
    pub(crate) gate_room_last_ms: Option<i64>,
    #[serde(default)]
    pub(crate) gate_room_waits: u32,
    #[serde(default)]
    pub(crate) gate_room_escalated: bool,
    /// The mission's current run of `host_fault` gate outcomes, folded by
    /// the store: how many in a row, the task the latest was about, and the
    /// latest record's `due_ms` (its own time plus its backoff).
    #[serde(default)]
    pub(crate) gate_host_faults: u32,
    #[serde(default)]
    pub(crate) gate_host_task: Option<i64>,
    #[serde(default)]
    pub(crate) gate_host_due_ms: Option<i64>,
    #[serde(default)]
    pub(crate) objectives: Option<Value>,
    #[serde(default)]
    pub(crate) source: Option<String>,
    /// The owner's active hold on its source (the `source.held` record's
    /// event id), if one, as the pass read it: which of its landing tasks
    /// wait behind it ([`Mission::in_flight`]). A landing never trusts it:
    /// [`land`] reads the hold from the store under the landing lock.
    #[serde(default)]
    pub(crate) hold: Option<i64>,
    /// Its tasks the closure rule lets go on: every open task of an open
    /// mission; of a done one, delivered, reported, accepted and landing.
    #[serde(default)]
    pub(crate) tasks: Vec<Task>,
}

/// One of its tasks the integrator may act on.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Task {
    pub(crate) id: i64,
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
    pub(crate) state: TaskState,
    #[serde(default)]
    pub(crate) base: Option<String>,
    #[serde(default)]
    pub(crate) candidate: Option<String>,
    #[serde(default)]
    pub(crate) scenario: Option<String>,
    #[serde(default)]
    pub(crate) delivered_at_ms: Option<i64>,
    /// The backlog objective it pursues, for an objective mission's task.
    #[serde(default)]
    pub(crate) objective: Option<u32>,
    /// The revision of its report (`mission_tasks.revisions`): every
    /// verdict names the one it judged, and the store refuses it once the
    /// task has moved on to another.
    #[serde(default)]
    pub(crate) revisions: i32,
    /// Its brief (`mission_tasks.brief`): an objective task's first line is
    /// `Objective <id>: <title>`.
    #[serde(default)]
    pub(crate) brief: String,
    /// The report the gate's verdict is for (`mission_tasks.report`), as the
    /// record holds it: the landing's message summarises it.
    #[serde(default)]
    pub(crate) report: Option<String>,
    /// When it last changed state (reported, accepted, or a verdict that
    /// left it ready): how long it has waited for a gate.
    #[serde(default)]
    pub(crate) updated_at_ms: i64,
    /// The owner's hold its landing last waited behind (its latest
    /// `task.held`), if any.
    #[serde(default)]
    pub(crate) held_by: Option<i64>,
}

impl Mission {
    /// The task the gate judges next: one the supervisor accepted, or, when
    /// the gate alone accepts, also one just reported.
    fn ready(&self) -> Option<&Task> {
        self.tasks.iter().find(|task| {
            task.state == TaskState::Accepted
                || (self.accept == "gate" && task.state == TaskState::Reported)
        })
    }

    /// A task whose gate passed and whose landing is not recorded yet.
    fn landing(&self) -> Option<&Task> {
        self.tasks
            .iter()
            .find(|task| task.state == TaskState::Landing)
    }

    /// A landing in flight: a task in `landing` whose push may have moved
    /// main before its record. Not one whose `task.held` names the active
    /// hold on its source: its `land()` pushed nothing, so main says nothing
    /// about it.
    fn in_flight(&self) -> Option<&Task> {
        self.tasks.iter().find(|task| {
            task.state == TaskState::Landing
                && !(task.held_by.is_some() && task.held_by == self.hold)
        })
    }
}

/// The halt rule that fires before anything is judged, if one does.
pub(crate) fn halt_reason(mission: &Mission, scenarios: u32) -> Option<String> {
    if mission.gate_rejections >= HALT_AFTER {
        return Some(format!(
            "{} gate rejections in a row",
            mission.gate_rejections
        ));
    }
    if mission.gate_aborts >= HALT_AFTER {
        return Some(format!("{} gate aborts in a row", mission.gate_aborts));
    }
    // A candidate that deletes the scenario that judges it passes by removing
    // the work, so the count may not fall.
    if mission.scenario_floor > 0 && scenarios < mission.scenario_floor {
        return Some(format!(
            "the scenarios fell from {} to {scenarios}",
            mission.scenario_floor
        ));
    }
    None
}

/// The wait after the `waits`-th `no_room` in a row: [`ROOM_BACKOFF`],
/// doubled each time, at most [`ROOM_BACKOFF_MAX`]. None before the first.
pub(crate) fn room_backoff(waits: u32) -> Duration {
    if waits == 0 {
        return Duration::ZERO;
    }
    ROOM_BACKOFF
        .saturating_mul(2u32.saturating_pow(waits - 1))
        .min(ROOM_BACKOFF_MAX)
}

/// How long `task`'s gate still waits for room at `now_ms`, read from the
/// store's record of its latest `no_room` (never from memory). None when the
/// gate may run: also when the mission's run is another task's, which never
/// lends its backoff.
pub(crate) fn room_wait(mission: &Mission, task: i64, now_ms: i64) -> Option<Duration> {
    if mission.gate_room_task != Some(task) {
        return None;
    }
    let last = mission.gate_room_last_ms?;
    let backoff = i64::try_from(room_backoff(mission.gate_room_waits).as_millis()).ok()?;
    let left = last.saturating_add(backoff).saturating_sub(now_ms);
    (left > 0).then(|| Duration::from_millis(left.unsigned_abs()))
}

/// The wait after the `faults`-th host fault in a row: `base`
/// (`SPAWN_GATE_HOST_FAULT_BACKOFF_SECS`, [`HOST_FAULT_BACKOFF`] by
/// default), doubled with each, at most [`HOST_FAULT_BACKOFF_MAX`]. Zero
/// before the first.
pub(crate) fn host_fault_backoff(base: Duration, faults: u32) -> Duration {
    if faults == 0 {
        return Duration::ZERO;
    }
    base.saturating_mul(2u32.saturating_pow(faults - 1))
        .min(HOST_FAULT_BACKOFF_MAX)
}

/// [`host_fault_backoff`]'s base from the setting's text: a whole number of
/// seconds from 1 up; anything else is [`HOST_FAULT_BACKOFF`].
pub(crate) fn host_fault_base(text: Option<&str>) -> Duration {
    text.and_then(|text| text.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or(HOST_FAULT_BACKOFF, Duration::from_secs)
}

/// How long `task`'s gate still waits after a host fault at `now_ms`, read
/// from the store's record of the latest one (its `due_ms`, folded into the
/// mission), never from memory. None when the gate may run: no run of host
/// faults, or the latest was about another task.
pub(crate) fn host_wait(mission: &Mission, task: i64, now_ms: i64) -> Option<Duration> {
    if mission.gate_host_faults == 0 || mission.gate_host_task != Some(task) {
        return None;
    }
    let left = mission.gate_host_due_ms?.saturating_sub(now_ms);
    (left > 0).then(|| Duration::from_millis(left.unsigned_abs()))
}

/// A wait as the integrator says it: whole minutes, else seconds.
fn spoken(wait: Duration) -> String {
    let seconds = wait.as_secs();
    if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds} s")
    }
}

/// The record of a gate whose rejection was the host's (`fault`), the
/// `faults`-th host fault in a row on the mission, with its `backoff`: the
/// reason naming the evidence, the evidence itself, the gate's output, the
/// count and `escalate` (the second in a row only). Its `due_ms` is the
/// record's own time plus the backoff, written by the store.
pub(crate) fn host_fault_fields(
    fault: &host::HostFault,
    faults: u32,
    backoff: Duration,
    log: &str,
) -> Value {
    let count = fault.failures.len();
    let evidence = if fault.crash.is_empty() {
        format!(
            "every failure check-tests.sh printed ({count}) is a connection error to the test \
             database"
        )
    } else {
        format!(
            "the test database's log shows a crash during the gate ({}), and every failure \
             check-tests.sh printed ({count}) is a database error",
            fault.crash[0]
        )
    };
    let mut fields = json!({
        "reason": format!(
            "host fault: {evidence}; not a verdict on the work, judged again in {}",
            spoken(backoff)
        ),
        "log": log,
        "evidence": fault.evidence(),
        "failures": fault.failures_value(),
        "host_faults": faults,
        "backoff_ms": u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
        "escalate": faults == HOST_FAULT_ESCALATE_AT,
    });
    if !fault.crash.is_empty() {
        fields["crash"] = json!(fault.crash);
    }
    fields
}

/// The record of a gate that had no room to judge `task`, at `now_ms`: the
/// reason with the free space and TMPDIR, the facts themselves, and
/// `escalate` when the run of them on this task began more than
/// [`ROOM_ESCALATE_AFTER`] ago and none escalated yet. Another task's run
/// gives it no age.
pub(crate) fn no_room(
    mission: &Mission,
    task: i64,
    free_kb: u64,
    min_gb: u64,
    tmpdir: &str,
    log: &str,
    now_ms: i64,
) -> Value {
    let disk = Shortfall::Disk {
        free_kb,
        min_gb,
        tmpdir: tmpdir.to_string(),
    };
    short_of_room(mission, task, &disk, log, now_ms)
}

/// The record of a judging of `task` that found the host short of room at
/// `now_ms`, by the gate's own floor or by the integrator's checks before
/// it ([`gate::Room`]): `check`, `measured` and `threshold`; for disk also
/// `free_kb`, `min_gb` and `tmpdir`, for the database its `server` (never
/// the URL, so never its password); a reason that names the check, the
/// measure and the threshold; `log`; and `escalate` as [`no_room`] says.
pub(crate) fn short_of_room(
    mission: &Mission,
    task: i64,
    short: &Shortfall,
    log: &str,
    now_ms: i64,
) -> Value {
    let after = i64::try_from(ROOM_ESCALATE_AFTER.as_millis()).unwrap_or(i64::MAX);
    let escalate = mission.gate_room_task == Some(task)
        && !mission.gate_room_escalated
        && mission
            .gate_room_since_ms
            .is_some_and(|since| now_ms.saturating_sub(since) > after);
    let reason = match short {
        Shortfall::Disk {
            free_kb,
            min_gb,
            tmpdir,
        } => {
            #[allow(clippy::cast_precision_loss)]
            let free_gib = *free_kb as f64 / 1_048_576.0;
            format!(
                "no room to judge now: {free_gib:.1} GiB free in {tmpdir} (TMPDIR), under \
                 SPAWN_GATE_MIN_FREE_GB={min_gb}; not a verdict, judged again later"
            )
        }
        Shortfall::Memory {
            available_mb,
            min_mb,
        } => format!(
            "no room to judge now: the host's memory has {available_mb} MiB available \
             (MemAvailable), under SPAWN_GATE_MIN_FREE_MEMORY_MB={min_mb}; not a verdict, judged \
             again later"
        ),
        Shortfall::Database {
            server,
            probe,
            error,
        } => format!(
            "no room to judge now: the test database ({server}) failed the probe \"{probe}\": \
             {error}; not a verdict, judged again later"
        ),
    };
    let mut record = json!({
        "reason": reason,
        "check": short.check(),
        "measured": short.measured(),
        "threshold": short.threshold(),
        "log": log,
        "escalate": escalate,
    });
    let extra = match short {
        Shortfall::Disk {
            free_kb,
            min_gb,
            tmpdir,
        } => json!({"free_kb": free_kb, "min_gb": min_gb, "tmpdir": tmpdir}),
        Shortfall::Memory { .. } => json!({}),
        Shortfall::Database { server, .. } => json!({ "server": server }),
    };
    if let (Some(target), Some(extra)) = (record.as_object_mut(), extra.as_object()) {
        target.extend(extra.clone());
    }
    record
}

/// Whether a delivered task ran past its mission's budget
/// (`{"minutes": n, "tool_calls": n}`, either optional).
pub(crate) fn over_budget(budget: &Value, elapsed_ms: i64, tool_calls: i64) -> Option<String> {
    if let Some(minutes) = budget.get("minutes").and_then(Value::as_i64)
        && elapsed_ms > minutes.saturating_mul(60_000)
    {
        return Some(format!("the task ran past its {minutes}-minute budget"));
    }
    if let Some(limit) = budget.get("tool_calls").and_then(Value::as_i64)
        && tool_calls > limit
    {
        return Some(format!("the task made more than {limit} tool calls"));
    }
    None
}

fn tail(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Whether a pushed branch may be judged, against the agent's latest
/// checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Freshness {
    /// The pushed head is the checkpoint, a descendant of it, or there is
    /// no checkpoint to compare with: judge this commit.
    Fresh(String),
    /// The push lags the checkpoint: wait for the pusher.
    Stale(String),
    /// Neither a checkpoint nor a pushed branch: nothing to land.
    Nothing,
}

/// The stale rule. `contains(ancestor, commit)` says whether `ancestor` is
/// reachable from `commit` in the integrator's mirror; a commit the mirror
/// lacks is contained by nothing.
pub(crate) fn freshness(
    checkpoint: Option<&str>,
    pushed: Option<&str>,
    contains: impl Fn(&str, &str) -> bool,
) -> Freshness {
    match (checkpoint, pushed) {
        (None, None) => Freshness::Nothing,
        (None, Some(pushed)) => Freshness::Fresh(pushed.to_string()),
        (Some(checkpoint), None) => Freshness::Stale(format!(
            "the source has no spawn/<agent> yet, and the latest checkpoint is {checkpoint}"
        )),
        (Some(checkpoint), Some(pushed)) => {
            if pushed == checkpoint || contains(checkpoint, pushed) {
                Freshness::Fresh(pushed.to_string())
            } else if contains(pushed, checkpoint) {
                Freshness::Stale(format!(
                    "the pushed branch ({pushed}) is behind the latest checkpoint ({checkpoint})"
                ))
            } else {
                Freshness::Stale(format!(
                    "the pushed branch ({pushed}) does not contain the latest checkpoint \
                     ({checkpoint})"
                ))
            }
        }
    }
}

/// The stale rule for a reported task: the pushed head must be the
/// report's commit (`reported`, the agent's checkpoint the report's turn
/// ended with) or a descendant of it, and pass [`freshness`] against the
/// agent's latest checkpoint as well. A report that names a commit is never
/// judged for lack of a checkpoint: with no pushed branch, or one that lacks
/// the commit, it is stale. A report that names none (a record from before
/// reports named their checkpoint) is judged by [`freshness`] alone.
pub(crate) fn report_freshness(
    reported: Option<&str>,
    checkpoint: Option<&str>,
    pushed: Option<&str>,
    contains: impl Fn(&str, &str) -> bool,
) -> Freshness {
    if let Some(reported) = reported {
        match pushed {
            None => {
                return Freshness::Stale(format!(
                    "the source has no spawn/<agent> yet, and the report's checkpoint is \
                     {reported}"
                ));
            }
            Some(pushed) if pushed != reported && !contains(reported, pushed) => {
                return Freshness::Stale(if contains(pushed, reported) {
                    format!(
                        "the pushed branch ({pushed}) is behind the report's checkpoint \
                         ({reported})"
                    )
                } else {
                    format!(
                        "the pushed branch ({pushed}) does not contain the report's checkpoint \
                         ({reported})"
                    )
                });
            }
            Some(_) => {}
        }
    }
    freshness(checkpoint, pushed, contains)
}

/// The mirror's own copy of an agent's pushed branch: never
/// `refs/heads/<agent>`, which on the Bridge's host is the Bridge's.
fn integrator_ref(agent: &str) -> String {
    format!("refs/spawn-integrate/{agent}")
}

fn git(arguments: &[&str]) -> Result<String, String> {
    sources::git(arguments).map_err(|error| error.to_string())
}

/// The source's default branch and its commit, as `git ls-remote` sees it.
pub(crate) fn source_head(source: &str) -> Result<(String, String), String> {
    let source = sources::MissionSource::parse(source)?;
    let listing = git(&["ls-remote", "--symref", "--", source.as_str(), "HEAD"])?;
    let mut branch = "refs/heads/main".to_string();
    let mut commit = String::new();
    for line in listing.lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            if let Some(name) = rest.split_whitespace().next() {
                branch = name.to_string();
            }
        } else if let Some(id) = line.split_whitespace().next() {
            commit = id.to_string();
        }
    }
    if commit.is_empty() {
        return Err(format!("{} has no HEAD", source.as_str()));
    }
    Ok((branch, commit))
}

/// `spawn integrate [--once]`: pass after pass, or one.
pub(crate) fn integrate(rest: &[String]) -> Result<i32, String> {
    let once = rest.iter().any(|word| word == "--once");
    let valued: Vec<String> = rest
        .iter()
        .filter(|word| *word != "--once")
        .cloned()
        .collect();
    let flags = Flags::parse(&valued).map_err(|error| format!("{error}\n{USAGE}"))?;
    crate::config::require_git()?;
    let root = workspace_root(&flags)
        .map(PathBuf::from)
        .ok_or_else(|| format!("--root or SPAWN_WORKSPACE_ROOT is required\n{USAGE}"))?;
    // Resolved against the working directory: the gate must not resolve
    // inside the tree it judges.
    let gate = std::path::absolute(env_or("SPAWN_GATE", "scripts/rsi/gate.sh"))
        .map_err(|error| format!("SPAWN_GATE: {error}"))?;
    // The gate gets an allowlist of this environment, never all of it: the
    // owner's SPAWN_DATABASE_URL must not reach candidate code.
    let gate_env = gate::gate_env(|name| std::env::var(name).ok());
    if std::env::var_os("SPAWN_TEST_DATABASE_URL").is_some()
        && !gate_env
            .iter()
            .any(|(name, _)| name == "SPAWN_TEST_DATABASE_URL")
    {
        eprintln!(
            "integrate: SPAWN_TEST_DATABASE_URL names this integrator's own database; \
             the gate does not get it"
        );
    }
    let gates = gate_slots(
        flags.get("gates"),
        crate::config::env_value("SPAWN_INTEGRATE_GATES"),
        crate::config::env_value(PER_GATE_SERVER).as_deref(),
    )
    .map_err(|error| format!("{error}\n{USAGE}"))?;
    // What the host must have before a gate starts, read once like the
    // gate's environment (gate::Room).
    let host_room = gate::Room::from_env(|name| std::env::var(name).ok());
    let integrator = Integrator::new(root, gate, gate_env, host_room, gates);
    if integrator.judge.host.server_log.is_none() {
        no_server_log(&format!("{TEST_DATABASE_LOG} is unset"));
    }
    loop {
        let pass = integrator.pass();
        if once {
            // One pass, and the gates it started run to their records.
            integrator.settle();
        }
        if let Err(error) = pass {
            eprintln!("integrate: {error}");
            if once {
                return Err(error);
            }
        }
        if once {
            return Ok(0);
        }
        std::thread::sleep(loop_interval().max(Duration::from_secs(5)));
    }
}

struct Integrator {
    judge: Judge,
    /// Gates that may run at once ([`gate_slots`]).
    gates: usize,
    /// The judgings in flight, one per mission at most.
    running: RefCell<Vec<Running>>,
    /// The store whose connection holds the integrator's lock, kept while
    /// a judging runs: its thread records through it.
    held: RefCell<Option<Arc<PostgresStore>>>,
    /// Tasks already said to wait for a gate: said again only once they
    /// have had one.
    waiting: RefCell<std::collections::HashSet<i64>>,
    /// When each mission's last judging ended (this process's reading):
    /// its next task waits for a gate from then.
    last_judged: RefCell<std::collections::HashMap<i64, i64>>,
}

/// What a judging needs, owned, so it runs on a thread of its own.
#[derive(Clone)]
struct Judge {
    root: PathBuf,
    gate: PathBuf,
    /// The gate's whole environment ([`gate::gate_env`]).
    gate_env: Vec<(String, String)>,
    /// How a rejection is told from a host fault.
    host: HostSettings,
    /// The host's room, checked before each gate starts ([`gate::Room`]).
    room: gate::Room,
}

/// The settings of host faults: the test database's log
/// ([`TEST_DATABASE_LOG`], none when unset) and the first backoff
/// (`SPAWN_GATE_HOST_FAULT_BACKOFF_SECS`, [`host_fault_base`]).
#[derive(Clone, Debug)]
struct HostSettings {
    server_log: Option<PathBuf>,
    backoff: Duration,
}

impl HostSettings {
    fn from_env() -> Self {
        Self {
            server_log: crate::config::env_value(TEST_DATABASE_LOG).map(PathBuf::from),
            backoff: host_fault_base(
                crate::config::env_value("SPAWN_GATE_HOST_FAULT_BACKOFF_SECS").as_deref(),
            ),
        }
    }
}

/// Whether the integrator said it has no log evidence: said once.
static SAID_NO_SERVER_LOG: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Say once, in the integrator's output, that the test database's log gives
/// no evidence, and why.
fn no_server_log(why: &str) {
    if !SAID_NO_SERVER_LOG.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "integrate: {why}: no log evidence of a test database crash; a gate failure is read \
             as the host's only when every failure is a connection error"
        );
    }
}

/// A task a mission may judge now, waiting for a gate.
struct Ready {
    mission: i64,
    source: String,
    mirror: PathBuf,
    main: String,
    /// The mission as read this pass, its gate the scenarios directory
    /// pinned once ([`pinned`]): the judging reads its no-room run too.
    gated: Mission,
    count: u32,
    task: Task,
}

/// One judging in flight.
struct Running {
    mission: i64,
    source: String,
    task: i64,
    /// Whether this judging's gate gets the shared test server
    /// ([`needs_test_database`]): only those count against `--gates`.
    shares_server: bool,
    handle: std::thread::JoinHandle<()>,
}

/// The marker a mission's owner puts in its scenarios directory when its gate
/// needs no test database (Supply PA's tests run on PGlite in process).
pub(crate) const NO_TEST_DATABASE: &str = "no-test-database";

/// Whether the gate of a mission whose scenarios are `scenarios` gets
/// SPAWN_TEST_DATABASE_URL: yes unless its owner put [`NO_TEST_DATABASE`]
/// there. One that does not get it cannot reach the shared server, whose gate
/// login is a superuser, so its judging does not count against `--gates`:
/// it may run beside one that does (still one per mission and per source).
pub(crate) fn needs_test_database(scenarios: &Path) -> bool {
    !scenarios.join(NO_TEST_DATABASE).exists()
}

/// The gate's variables a mission marked [`NO_TEST_DATABASE`] does not get:
/// the shared test server's URL, and the programs (a docker image, or the
/// host's directory of them) the gate would run a server of its own from.
fn withheld_without_test_database(name: &str) -> bool {
    matches!(
        name,
        "SPAWN_TEST_DATABASE_URL" | "SPAWN_GATE_PG_IMAGE" | "SPAWN_GATE_PG_BIN"
    )
}

/// Whether a judging must wait for a gate: only one that shares the test
/// server, when `shared_running` such judgings already fill the `cap`.
fn waits_for_gate(shares_server: bool, shared_running: usize, cap: usize) -> bool {
    shares_server && shared_running >= cap
}

impl Integrator {
    fn new(
        root: PathBuf,
        gate: PathBuf,
        gate_env: Vec<(String, String)>,
        host_room: gate::Room,
        gates: usize,
    ) -> Self {
        Self {
            judge: Judge {
                root,
                gate,
                gate_env,
                host: HostSettings::from_env(),
                room: host_room,
            },
            gates,
            running: RefCell::new(Vec::new()),
            held: RefCell::new(None),
            waiting: RefCell::new(std::collections::HashSet::new()),
            last_judged: RefCell::new(std::collections::HashMap::new()),
        }
    }

    fn pass(&self) -> Result<(), String> {
        self.reap();
        // A store per pass while no gate runs: the advisory lock belongs to
        // the connection, and is released with it. While one runs, its
        // thread holds the store, so the lock stays this integrator's.
        if self.running.borrow().is_empty() {
            self.held.borrow_mut().take();
        }
        let held = self.held.borrow().clone();
        let store = if let Some(store) = held {
            store
        } else {
            let store = Arc::new(open_store()?);
            if !store
                .integrator_try_lock()
                .map_err(|error| error.to_string())?
            {
                return Ok(());
            }
            store
        };
        let pass = self.pass_on(&store);
        // A pass that failed may have lost the connection: the next pass
        // asks for the lock again (refused while the old connection lives).
        *self.held.borrow_mut() =
            (pass.is_ok() && !self.running.borrow().is_empty()).then_some(store);
        pass
    }

    /// Join the judgings that ended. Their records are in the log already;
    /// a thread that panicked recorded nothing, so its task is judged again.
    fn reap(&self) {
        let finished: Vec<Running> = {
            let mut running = self.running.borrow_mut();
            let (finished, still) = running
                .drain(..)
                .partition(|gate: &Running| gate.handle.is_finished());
            *running = still;
            finished
        };
        for gate in finished {
            self.join(gate);
        }
    }

    /// Wait for every judging in flight (`--once`, and tests).
    fn settle(&self) {
        let running: Vec<Running> = self.running.borrow_mut().drain(..).collect();
        for gate in running {
            self.join(gate);
        }
    }

    fn join(&self, gate: Running) {
        // The mission's next task waits for a gate from now.
        let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
        self.last_judged.borrow_mut().insert(gate.mission, now);
        if gate.handle.join().is_err() {
            eprintln!(
                "integrate: mission {}: the judging of task {} panicked; it is judged again",
                gate.mission, gate.task
            );
        }
    }

    /// Whether a judging in flight keeps `mission` from the mirror this
    /// pass: its own, or one on its source.
    fn busy(&self, mission: &Mission) -> bool {
        self.running.borrow().iter().any(|gate| {
            gate.mission == mission.id || mission.source.as_deref() == Some(gate.source.as_str())
        })
    }

    /// One pass over every gated mission, holding the integrator's lock.
    fn pass_on(&self, store: &Arc<PostgresStore>) -> Result<(), String> {
        // A hold past its bound ends by its record, before any mission's
        // step: nothing lands on its source until then.
        expire_holds(store)?;
        let missions: Vec<Mission> =
            serde_json::from_value(store.gated_missions().map_err(|error| error.to_string())?)
                .map_err(|error| format!("gated missions: {error}"))?;
        // Every mission gets its step in every pass: halts, budgets,
        // landings and objective assignments never wait for another
        // mission's gate. A landing moves its source's main and the
        // baselines of the missions on it, so the rest of that source waits
        // for the next pass, which reads everything again; other sources go
        // on.
        let mut moved = std::collections::HashSet::new();
        let mut ready = Vec::new();
        for mission in &missions {
            let source = mission.source.clone();
            if source.as_ref().is_some_and(|source| moved.contains(source)) {
                continue;
            }
            match self.mission(store, &missions, mission, &mut ready) {
                Ok(true) => {
                    if let Some(source) = source {
                        moved.insert(source);
                    }
                }
                Ok(false) => {}
                Err(error) => eprintln!("integrate: mission {}: {error}", mission.id),
            }
        }
        ready.retain(|candidate: &Ready| !moved.contains(&candidate.source));
        self.start_ready(store, ready);
        Ok(())
    }

    /// One mission's step. True when it landed a task (its source's main
    /// moved). A ready task it may judge goes to `ready`; the gates are
    /// given out after every mission had its step ([`Self::start_ready`]).
    /// `missions` is every mission of the pass, for the landings on a
    /// shared source.
    fn mission(
        &self,
        store: &Arc<PostgresStore>,
        missions: &[Mission],
        mission: &Mission,
        ready: &mut Vec<Ready>,
    ) -> Result<bool, String> {
        if mission.halted.is_some() {
            return Ok(false);
        }
        let scenarios = pinned(Path::new(&mission.gate));
        let count = gate::count_scenarios(&scenarios)?;
        if let Some(reason) = halt_reason(mission, count) {
            return halt(store, mission, &reason).map(|()| false);
        }
        Self::enforce_budget(store, mission)?;
        let Some(source) = mission.source.as_deref() else {
            return Ok(false);
        };
        if let Some(reason) = unlandable(source) {
            return halt(store, mission, &reason).map(|()| false);
        }
        // The mirror and main, when this mission may judge now: not while a
        // gate of its own, or of another mission on its source, runs (they
        // share the mirror and the main it judges against).
        let mut judgeable = None;
        // A task that waits behind the owner's hold on the source keeps the
        // mission's other ready tasks waiting (one task per mission between
        // its gate and its landing) and ends nothing early.
        let mut held = false;
        if !self.busy(mission) {
            let mirror = sources::ensure_mirror(&self.judge.root, Some(source))
                .map_err(|error| error.to_string())?;
            if let Some(task) = mission.landing() {
                match land(store, &mirror, source, task)? {
                    Landing::Held(_) => held = true,
                    Landing::Landed | Landing::Stale | Landing::Gone => return Ok(true),
                }
            }
            // Another mission's landing on this source may have moved main
            // without its record yet (a crash between the push and
            // `task.landed`): main says nothing about this mission until
            // that landing is resolved, by its own mission's next step.
            if let Some((other, task)) = landing_elsewhere(missions, mission) {
                println!(
                    "integrate: mission {}: waits for task {task} of mission {other} to land on \
                     its source",
                    mission.id
                );
            } else {
                // A fetch that failed leaves the mirror's last copy of main,
                // which is no reading of the source's main: no halt, no
                // verdict by it.
                match sources::refresh(&mirror, source).and_then(|()| {
                    git(&[
                        "--git-dir",
                        &mirror.to_string_lossy(),
                        "rev-parse",
                        "refs/heads/main",
                    ])
                }) {
                    Err(error) => eprintln!("integrate: mission {}: {error}", mission.id),
                    Ok(main) => {
                        if let Some(baseline) = &mission.baseline
                            && *baseline != main
                        {
                            return halt(
                                store,
                                mission,
                                &format!(
                                    "the source's main is {main}, but the last landing left \
                                     {baseline}"
                                ),
                            )
                            .map(|()| false);
                        }
                        judgeable = Some((mirror, main));
                    }
                }
            }
        }
        // An objective mission's free slots get their objectives, whatever
        // its gate is doing; a closed mission gets none.
        if mission.state != "done"
            && let Err(error) = objectives::assign_next(store, mission)
        {
            eprintln!("integrate: mission {}: {error}", mission.id);
        }
        if held {
            return Ok(false);
        }
        if let (Some((mirror, main)), Some(task)) = (judgeable, mission.ready()) {
            if gate_waits(mission, task) {
                return Ok(false);
            }
            let mut gated = mission.clone();
            gated.gate = scenarios.to_string_lossy().into_owned();
            ready.push(Ready {
                mission: mission.id,
                source: source.to_string(),
                mirror,
                main,
                gated,
                count,
                task: task.clone(),
            });
        }
        Ok(false)
    }

    /// Give the free gates to the ready tasks that have waited longest:
    /// since they were reported (`updated_at_ms`, which every verdict also
    /// moves), or since their mission's last judging ended, whichever is
    /// later (a task cannot have a gate while its mission's previous one
    /// runs). Two missions that are always ready thus take turns, and the
    /// mission id decides nothing. At most one judging per mission and per
    /// source.
    fn start_ready(&self, store: &Arc<PostgresStore>, mut ready: Vec<Ready>) {
        let since = |candidate: &Ready| {
            let ended = self
                .last_judged
                .borrow()
                .get(&candidate.mission)
                .copied()
                .unwrap_or(0);
            candidate.task.updated_at_ms.max(ended)
        };
        ready.sort_by_key(|candidate| (since(candidate), candidate.task.id));
        for candidate in ready {
            let taken =
                self.running.borrow().iter().any(|gate| {
                    gate.mission == candidate.mission || gate.source == candidate.source
                });
            if taken {
                continue;
            }
            let shares_server = needs_test_database(Path::new(&candidate.gated.gate));
            let running = self
                .running
                .borrow()
                .iter()
                .filter(|gate| gate.shares_server)
                .count();
            if waits_for_gate(shares_server, running, self.gates) {
                if self.waiting.borrow_mut().insert(candidate.task.id) {
                    println!(
                        "integrate: mission {}: task {} waits for a gate ({running} of {} \
                         running)",
                        candidate.mission, candidate.task.id, self.gates
                    );
                }
                continue;
            }
            self.waiting.borrow_mut().remove(&candidate.task.id);
            let mission = candidate.mission;
            if let Err(error) = self.start_judging(store, candidate, running, shares_server) {
                eprintln!("integrate: mission {mission}: {error}");
            }
        }
    }

    /// Judge a ready task on a thread of its own.
    fn start_judging(
        &self,
        store: &Arc<PostgresStore>,
        candidate: Ready,
        running: usize,
        shares_server: bool,
    ) -> Result<(), String> {
        let Ready {
            mission: mission_id,
            source,
            mirror,
            main,
            gated,
            count,
            task,
        } = candidate;
        let judge = self.judge.clone();
        let task_id = task.id;
        let (store, source_owned) = (Arc::clone(store), source.clone());
        let handle = std::thread::Builder::new()
            .name(format!("gate-{task_id}"))
            .spawn(move || {
                judged_or_aborted(&store, &task, &main, mission_id, || {
                    judge.judge(&store, &mirror, &source_owned, &main, &gated, count, &task)
                });
            })
            .map_err(|error| format!("start the judging of task {task_id}: {error}"))?;
        if shares_server {
            println!(
                "integrate: mission {mission_id}: judging task {task_id} ({} of {} gates)",
                running + 1,
                self.gates
            );
        } else {
            println!(
                "integrate: mission {mission_id}: judging task {task_id} (no test database: \
                 beside the {running} of {} gates)",
                self.gates
            );
        }
        self.running.borrow_mut().push(Running {
            mission: mission_id,
            source,
            task: task_id,
            shares_server,
            handle,
        });
        Ok(())
    }

    /// Abort delivered tasks over budget. True when it aborted one.
    fn enforce_budget(store: &PostgresStore, mission: &Mission) -> Result<bool, String> {
        let Some(budget) = &mission.budget else {
            return Ok(false);
        };
        let mut aborted = false;
        let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
        for task in &mission.tasks {
            let (TaskState::Delivered, Some(at)) = (task.state, task.delivered_at_ms) else {
                continue;
            };
            let calls = store
                .tool_calls_since(&task.session_id, at)
                .map_err(|error| error.to_string())?;
            if let Some(reason) = over_budget(budget, now - at, calls) {
                // The Stop first, under its fixed key: if the record then
                // fails, the next pass finds the task still delivered and
                // over budget, and submits (a no-op) and records again.
                stop(store, task)?;
                record(
                    store,
                    &json!({"type": "task.aborted", "id": task.id, "reason": reason}),
                )?;
                println!("integrate: task {} aborted: {reason}", task.id);
                aborted = true;
            }
        }
        Ok(aborted)
    }
}

impl Judge {
    /// The agent's pushed branch, fetched into the mirror, checked against
    /// the checkpoint the task's report names and the agent's latest
    /// checkpoint ([`report_freshness`]). A lagging push is fetched again for
    /// up to [`PUSH_WAIT`] before it counts as stale.
    fn pushed_candidate(
        store: &PostgresStore,
        mirror: &Path,
        source: &str,
        task: &Task,
    ) -> Result<Freshness, String> {
        let agent = task.agent_id.as_str();
        let checkpoint = store
            .latest_checkpoint(agent)
            .map_err(|error| error.to_string())?;
        let reported = store
            .reported_commit(task.id)
            .map_err(|error| error.to_string())?;
        let mirror_path = mirror.to_string_lossy().to_string();
        let contains = |ancestor: &str, commit: &str| {
            git(&[
                "--git-dir",
                &mirror_path,
                "merge-base",
                "--is-ancestor",
                ancestor,
                commit,
            ])
            .is_ok()
        };
        let deadline = std::time::Instant::now() + PUSH_WAIT;
        let mut pushed_again = false;
        loop {
            let pushed = sources::fetch_pushed_into(mirror, source, agent, &integrator_ref(agent))
                .map_err(|error| error.to_string())?;
            let verdict = report_freshness(
                reported.as_deref(),
                checkpoint.as_deref(),
                pushed.as_deref(),
                contains,
            );
            if !matches!(verdict, Freshness::Stale(_)) || std::time::Instant::now() >= deadline {
                return Ok(verdict);
            }
            // A recorded checkpoint whose push was lost (issue #8) is pushed
            // again from this host's mirror, once a pass.
            if !pushed_again && let Some(checkpoint) = checkpoint.as_deref() {
                pushed_again = true;
                if let Some(again) = sources::push_recorded(mirror, source, agent, checkpoint) {
                    println!(
                        "integrate: task {}: {checkpoint} pushed again: {again:?}",
                        task.id
                    );
                }
            }
            std::thread::sleep(PUSH_POLL);
        }
    }

    /// Judge one task from its pushed branch, on its own thread. True when
    /// the gate ran; a stale push runs none.
    #[allow(clippy::too_many_arguments)]
    fn judge(
        &self,
        store: &Arc<PostgresStore>,
        mirror: &Path,
        source: &str,
        main: &str,
        mission: &Mission,
        count: u32,
        task: &Task,
    ) -> Result<bool, String> {
        let branch = match Self::pushed_candidate(store, mirror, source, task)? {
            Freshness::Fresh(branch) => Some(branch),
            Freshness::Nothing => None,
            Freshness::Stale(reason) => {
                // Not a verdict on the work: the pusher has not caught up.
                record(
                    store,
                    &json!({"type": "task.gated", "id": task.id, "verdict": "stale",
                            "revision": task.revisions, "reason": reason}),
                )?;
                println!("integrate: task {} stale: {reason}", task.id);
                return Ok(false);
            }
        };
        self.judge_pushed(
            store,
            mirror,
            source,
            main,
            mission,
            count,
            task,
            branch.as_deref(),
        )
        .map(|()| true)
    }

    /// Judge `branch`, the agent's pushed head already in the mirror (`None`:
    /// the agent has nothing to land).
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn judge_pushed(
        &self,
        store: &Arc<PostgresStore>,
        mirror: &Path,
        source: &str,
        main: &str,
        mission: &Mission,
        count: u32,
        task: &Task,
        branch: Option<&str>,
    ) -> Result<(), String> {
        let scenarios = Path::new(&mission.gate);
        // Read only: the integrator never commits a workspace.
        let repo = GitWorkspace::new(mirror, &self.root);
        let gated = |verdict: &str, extra: Value| -> Result<(), String> {
            let mut record_value = json!({"type": "task.gated", "id": task.id, "verdict": verdict,
                                          "revision": task.revisions, "base": main,
                                          "scenarios": count});
            if let (Some(target), Some(extra)) = (record_value.as_object_mut(), extra.as_object()) {
                target.extend(extra.clone());
            }
            record(store, &record_value)?;
            println!("integrate: task {} gated: {verdict}", task.id);
            Ok(())
        };
        let rejected = |reason: &str, log: &str| gated("rejected", rejected_fields(reason, log));

        let mirror_path = mirror.to_string_lossy().to_string();
        let Some(branch) = branch.map(str::to_string) else {
            return rejected("the worker has no branch to land", "");
        };
        // A merge of main whose markers are not resolved is not judged: the
        // markers could land, and a file rewritten instead of resolved would
        // revert main.
        let marked = repo
            .conflict_markers(main, &branch)
            .map_err(|error| error.to_string())?;
        if let Some(note) = markers_note(repo.has_pending_merge(&task.agent_id), &marked) {
            return gated("conflict", json!({ "note": note }));
        }
        let message = landing_message(task, &branch);
        let Some(candidate) = landing_candidate(&mirror_path, main, &branch, &message)? else {
            // The worker cannot merge main itself: merge it into the
            // workspace now, while the worker waits for this verdict, so
            // the note names what it has to resolve.
            let merge = main_merge::merge_main(store, &self.root, &task.agent_id, &task.session_id);
            println!(
                "integrate: task {}: main into its workspace: {merge:?}",
                task.id
            );
            return gated(
                "conflict",
                json!({"note": main_merge::conflict_note(main, &merge)}),
            );
        };
        let tree_of = |commit: &str| {
            git(&[
                "--git-dir",
                &mirror_path,
                "rev-parse",
                &format!("{commit}^{{tree}}"),
            ])
        };
        if tree_of(&candidate)? == tree_of(main)? {
            return rejected("the work changes nothing on main", "");
        }
        let patch = repo
            .patch_between(main, &candidate)
            .map_err(|error| error.to_string())?;
        if patch.len() > MAX_PATCH_BYTES {
            return rejected("the patch is larger than 1 MiB", "");
        }

        let scenario = task_scenario(scenarios, task.scenario.as_deref());
        // The host's room, measured now, before any gate starts: a gate
        // must never be how the integrator learns the host had none. A gate
        // that gets no test database has none to probe.
        let room = if needs_test_database(scenarios) {
            self.room.clone()
        } else {
            self.room.without_test_database()
        };
        match room.check(&self.gate_env) {
            Ok(checks) => println!(
                "integrate: task {}: room to judge: {}",
                task.id,
                checks.lines().collect::<Vec<_>>().join("; ")
            ),
            Err(RoomFailure::Short { short, log }) => {
                println!(
                    "integrate: task {}: no room to judge now ({}); the gate does not run",
                    task.id,
                    short.check()
                );
                return gated(
                    "no_room",
                    short_of_room(mission, task.id, &short, &log, now_ms()),
                );
            }
            Err(RoomFailure::Bad(reason)) => {
                return gated("aborted", json!({ "reason": reason }));
            }
        }
        // The test server must not be the owner's, by identity, asked now:
        // a socket mount or a DNS name can change between gate runs.
        let owner = std::env::var("SPAWN_DATABASE_URL").ok();
        let (mut gate_env, warning) =
            gate::withhold_owners_cluster(&self.gate_env, owner.as_deref(), |url| {
                spawn_postgres::cluster_identity(url).map_err(|error| error.to_string())
            });
        if !needs_test_database(scenarios) {
            gate_env.retain(|(name, _)| !withheld_without_test_database(name));
        }
        if let Some(warning) = warning {
            eprintln!("integrate: {warning}");
        }
        let gate_path = pinned(&self.gate);
        let run_gate = |base: &str, judged: &str| {
            gate::judge(
                &gate_path,
                mirror,
                base,
                judged,
                scenarios,
                scenario.as_deref(),
                &gate_env,
                gate_wall_clock(),
            )
        };
        // Only a gate that runs on the test database can fail because it
        // did: a mission whose gate needs none ([`NO_TEST_DATABASE`]) has no
        // host faults, and the test server's log says nothing about it.
        let test_database = needs_test_database(scenarios);
        // The candidate's gate run, measured: a crash in the test
        // database's log counts only inside this window.
        let started = now_ms();
        let judged = run_gate(main, &candidate);
        let ended = now_ms();
        match judged {
            Ok(verdict) if verdict.accepted => {
                // The verdict is stored before git moves, so a crash lands
                // on the next pass instead of judging again.
                gated(
                    "accepted",
                    json!({"candidate": candidate, "log": verdict.output, "metric": verdict.metric}),
                )?;
                let landing = Task {
                    state: TaskState::Landing,
                    base: Some(main.to_string()),
                    candidate: Some(candidate),
                    ..task.clone()
                };
                // On a held source this records its `task.held`: it waits.
                land(store, mirror, source, &landing).map(|_| ())
            }
            // A rejection whose every failure is the host's (the test
            // database failed) is no verdict on the work: `host_fault`, not
            // counted, judged again after a backoff.
            Ok(verdict)
                if test_database
                    && let Some(fault) =
                        host::host_fault(&verdict.reason, &verdict.output, || {
                            self.crash_in(started, ended)
                        }) =>
            {
                let faults = mission.gate_host_faults.saturating_add(1);
                let backoff = host_fault_backoff(self.host.backoff, faults);
                let mut extra = host_fault_fields(&fault, faults, backoff, &verdict.output);
                extra["candidate"] = json!(candidate);
                println!(
                    "integrate: task {}: {}",
                    task.id,
                    extra["reason"].as_str().unwrap_or_default()
                );
                gated("host_fault", extra)
            }
            // A rejected merge is judged again as the head alone, against
            // the main the head last took in: main's moves alone may have
            // broken it (`incompatible`, never counted).
            Ok(verdict) => {
                match rejection(&mirror_path, main, &candidate, &branch, &verdict, &run_gate)? {
                    Rejection::Record(verdict, extra) => gated(verdict, extra),
                    Rejection::HeadNoRoom {
                        free_kb,
                        min_gb,
                        tmpdir,
                        log,
                        head,
                    } => {
                        let mut extra =
                            no_room(mission, task.id, free_kb, min_gb, &tmpdir, &log, now_ms());
                        if let (Some(target), Some(head)) =
                            (extra.as_object_mut(), head.as_object())
                        {
                            target.extend(head.clone());
                        }
                        gated("no_room", extra)
                    }
                }
            }
            // No room in TMPDIR: a fact about the host, never a count.
            Err(GateFailure::NoRoom {
                free_kb,
                min_gb,
                tmpdir,
                log,
            }) => gated(
                "no_room",
                no_room(mission, task.id, free_kb, min_gb, &tmpdir, &log, now_ms()),
            ),
            Err(failure) => gated("aborted", aborted(&failure)),
        }
    }
}

impl Judge {
    /// The test database's log lines that show a crash stamped inside the
    /// gate's window [`started`, `ended`] (with [`host::WINDOW_SLACK_MS`]
    /// around it). None to read (unset, unreadable): no evidence, said once.
    fn crash_in(&self, started: i64, ended: i64) -> Vec<String> {
        let Some(path) = &self.host.server_log else {
            no_server_log(&format!("{TEST_DATABASE_LOG} is unset"));
            return Vec::new();
        };
        match host::read_log(path) {
            Ok(text) => host::crash_lines(&text, started, ended),
            Err(error) => {
                no_server_log(&format!(
                    "{TEST_DATABASE_LOG} ({}) cannot be read: {error}",
                    path.display()
                ));
                Vec::new()
            }
        }
    }
}

/// Run a judging. When it panics, record its abort unless it recorded
/// something already: a panic that ends a judging without a record of its
/// own would recur on every pass, forever, and an abort counts toward the
/// mission's three-aborts halt. An error is shown and records nothing: no
/// verdict on the work (objective 008), and a later pass judges again.
fn judged_or_aborted(
    store: &PostgresStore,
    task: &Task,
    main: &str,
    mission: i64,
    judging: impl FnOnce() -> Result<bool, String>,
) {
    let reason = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(judging)) {
        Ok(Ok(_)) => return,
        // An error is no verdict: shown, not recorded, judged again later.
        Ok(Err(error)) => {
            eprintln!("integrate: mission {mission}: task {}: {error}", task.id);
            return;
        }
        Err(panic) => format!(
            "the judging panicked: {}",
            panic
                .downcast_ref::<&str>()
                .map(ToString::to_string)
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default()
        ),
    };
    eprintln!("integrate: mission {mission}: task {}: {reason}", task.id);
    match abort_unrecorded(store, task, main, &reason) {
        Ok(true) => println!("integrate: task {} gated: aborted", task.id),
        Ok(false) => {}
        Err(error) => eprintln!(
            "integrate: mission {mission}: task {}: record the abort: {error}",
            task.id
        ),
    }
}

/// Record `task.gated aborted` for a judging that ended in an error, when
/// the log shows it recorded nothing: the task is still in the state and at
/// the revision it was judged from. True when it recorded the abort.
fn abort_unrecorded(
    store: &PostgresStore,
    task: &Task,
    main: &str,
    reason: &str,
) -> Result<bool, String> {
    let missions: Vec<Mission> =
        serde_json::from_value(store.gated_missions().map_err(|error| error.to_string())?)
            .map_err(|error| format!("gated missions: {error}"))?;
    let unrecorded = missions
        .iter()
        .flat_map(|mission| &mission.tasks)
        .any(|now| now.id == task.id && now.state == task.state && now.revisions == task.revisions);
    if !unrecorded {
        return Ok(false);
    }
    record(
        store,
        &json!({"type": "task.gated", "id": task.id, "verdict": "aborted",
                "revision": task.revisions, "base": main, "reason": reason}),
    )?;
    Ok(true)
}

/// The commit that lands `branch` (the agent's pushed head, in the mirror)
/// on `main`, with `message` ([`landing_message`]), or `None` when the two
/// conflict. Always one new merge commit: first parent `main` (the base the
/// gate judges against), second parent `branch`, so main's first-parent line
/// is one commit per landing and every checkpoint stays reachable through
/// the second parents. Its tree is the branch's when main is in it, and the
/// merge of both by main's merge rules otherwise. Never the branch head
/// itself, so a head that names a large-file manifest lands without it.
/// Only objects are written, never a ref: the agent's `refs/heads/<agent>`
/// stays the Bridge's, and `spawn/<agent>` catches up with a landing through
/// the Bridge's next merge of main, which its pusher pushes like any
/// checkpoint.
fn landing_candidate(
    mirror_path: &str,
    main: &str,
    branch: &str,
    message: &str,
) -> Result<Option<String>, String> {
    let main_is_in = git(&[
        "--git-dir",
        mirror_path,
        "merge-base",
        "--is-ancestor",
        main,
        branch,
    ])
    .is_ok();
    let tree = if main_is_in {
        git(&[
            "--git-dir",
            mirror_path,
            "rev-parse",
            &format!("{branch}^{{tree}}"),
        ])?
    } else {
        // Main moved since the worker started: land the merge of both, by
        // main's merge rules (a bare repository reads no in-tree
        // .gitattributes otherwise), as main is merged into agents' branches.
        let output = sources::controller_git()
            .args([
                "--git-dir",
                mirror_path,
                &format!("--attr-source={main}"),
                "merge-tree",
                "--write-tree",
                main,
                branch,
            ])
            .output()
            .map_err(|error| format!("run git merge-tree: {error}"))?;
        let Some(tree) = merged_tree(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )?
        else {
            return Ok(None);
        };
        tree
    };
    commit_landing(
        mirror_path,
        &[&tree, "-p", main, "-p", branch, "-m", message],
    )
    .map(Some)
}

/// Characters of a report's summary a landing's message holds at most.
const SUMMARY_CHARS: usize = 3000;

/// Characters of a brief's first line a landing's first line holds at most.
const TITLE_CHARS: usize = 100;

/// The message of `task`'s landing of `head`: what landed, from the record
/// alone (never a workspace). The first line holds the objective's title
/// (the brief's first line, `Objective <id>: <title>`) and names the
/// objective and the task, or, for a task without an objective, the brief's
/// first non-empty line and the task. After a blank line, the report's
/// `## What changed` section (the whole report when it has none), bounded,
/// with every line that could read as a `Spawn-` trailer indented. Last, the
/// integrator's own trailers.
pub(crate) fn landing_message(task: &Task, head: &str) -> String {
    let first = task
        .brief
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut subject = match task.objective {
        Some(objective) => {
            let title = first
                .strip_prefix(&format!("Objective {objective}:"))
                .map_or(first, str::trim);
            let title = cut(title, TITLE_CHARS);
            if title.is_empty() {
                format!("Land objective {objective}, task {}", task.id)
            } else {
                format!("{title} (objective {objective}, task {})", task.id)
            }
        }
        None if first.is_empty() => format!("Land task {}", task.id),
        None => format!("{} (task {})", cut(first, TITLE_CHARS), task.id),
    };
    if subject.to_ascii_lowercase().starts_with("spawn checkpoint") {
        subject = format!("Land {subject}");
    }
    let mut message = subject;
    message.push_str("\n\n");
    let summary = report_summary(task.report.as_deref().unwrap_or_default());
    if summary.is_empty() {
        message.push_str("The task's report is empty.");
    } else {
        for line in summary.lines() {
            if line.trim_start().to_ascii_lowercase().starts_with("spawn-") {
                message.push_str("    ");
            }
            message.push_str(line);
            message.push('\n');
        }
    }
    let message = message.trim_end().to_string();
    let mut trailers = format!("Spawn-Task: {}\n", task.id);
    if let Some(objective) = task.objective {
        let _ = writeln!(trailers, "Spawn-Objective: {objective}");
    }
    let _ = writeln!(trailers, "Spawn-Head: {head}");
    format!("{message}\n\n{trailers}")
}

/// What a landing's message quotes of `report`: its `## What changed`
/// section, or the whole report when it has none (or an empty one),
/// trimmed and cut to [`SUMMARY_CHARS`].
fn report_summary(report: &str) -> String {
    fn heading(line: &str) -> Option<&str> {
        let level = line.bytes().take_while(|&byte| byte == b'#').count();
        (1..=2)
            .contains(&level)
            .then(|| line[level..].trim().trim_end_matches(':').trim())
    }
    let mut lines = report.lines();
    let mut section = None;
    if lines.any(|line| heading(line).is_some_and(|text| text.eq_ignore_ascii_case("what changed")))
    {
        let body: Vec<&str> = lines.take_while(|line| heading(line).is_none()).collect();
        let body = body.join("\n");
        if !body.trim().is_empty() {
            section = Some(body);
        }
    }
    let text = section.as_deref().unwrap_or(report).trim();
    let summary = cut(text, SUMMARY_CHARS);
    if summary.len() < text.len() {
        format!("{summary}\n[the report continues]")
    } else {
        summary.to_string()
    }
}

/// `text` cut to at most `limit` characters, on a character boundary.
fn cut(text: &str, limit: usize) -> &str {
    text.char_indices()
        .nth(limit)
        .map_or(text, |(at, _)| text[..at].trim_end())
}

/// What `git merge-tree --write-tree` said, from its exit code (`None`: no
/// code, killed by a signal) and output: `Ok(Some(tree))` for a clean merge,
/// `Ok(None)` for a conflict (exit 1, the only code git gives one), and any
/// other failure an error with git's own message. A git that cannot run the
/// merge (an unknown option, a crash) says nothing about the work: it must
/// reach the operator, never send the worker after conflicts that do not
/// exist.
fn merged_tree(code: Option<i32>, stdout: &str, stderr: &str) -> Result<Option<String>, String> {
    match code {
        Some(0) => Ok(Some(
            stdout.lines().next().unwrap_or_default().trim().to_string(),
        )),
        Some(1) => Ok(None),
        failure => {
            let how = failure.map_or_else(
                || "was killed by a signal".to_string(),
                |code| format!("exited {code}"),
            );
            Err(format!(
                "git merge-tree {how}, so the landing could not be merged (this is not a \
                 conflict; Spawn needs git {}.{} or later): {}",
                crate::config::MIN_GIT.0,
                crate::config::MIN_GIT.1,
                stderr.trim()
            ))
        }
    }
}

/// Name and email of the author and committer of every landing commit.
const LANDING_IDENTITY: (&str, &str) = ("spawn-integrate", "integrate@localhost");

/// `git commit-tree <arguments>` in the mirror, authored and committed as
/// [`LANDING_IDENTITY`] and unsigned: a landing never depends on the host's
/// git configuration (no identity in `~/.gitconfig`, another one there or in
/// the environment, `commit.gpgSign`). The identity goes in the environment,
/// which git takes over any configuration.
fn commit_landing(mirror_path: &str, arguments: &[&str]) -> Result<String, String> {
    let (name, email) = LANDING_IDENTITY;
    let output = sources::controller_git()
        .args(["--git-dir", mirror_path, "commit-tree", "--no-gpg-sign"])
        .args(arguments)
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_NAME", name)
        .env("GIT_COMMITTER_EMAIL", email)
        .output()
        .map_err(|error| format!("run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git commit-tree: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The note that sends work back before it is judged, when a merge of main
/// waits for its markers (`pending`) or the branch carries marker blocks in
/// the files it changes (`marked`).
fn markers_note(pending: bool, marked: &[String]) -> Option<String> {
    if !pending && marked.is_empty() {
        return None;
    }
    let files = if marked.is_empty() {
        "the files the merge listed".to_string()
    } else {
        marked.join(", ")
    };
    Some(format!(
        "Your work still carries conflict markers from merging the source's main ({files}). \
         Resolve them first: edit the marked regions (<<<<<<< / ======= / >>>>>>>), keeping \
         both sides' intent; do not rewrite the files from memory. Then report again."
    ))
}

/// What a candidate the gate rejected comes to, once its head was judged
/// alone when that can say something ([`rejection`]).
#[derive(Debug)]
enum Rejection {
    /// Record this verdict with these fields.
    Record(&'static str, Value),
    /// The head's own run found no room in TMPDIR: `no_room`, as any, with
    /// `head` (the fields naming the head and the rejected merge) added.
    HeadNoRoom {
        free_kb: u64,
        min_gb: u64,
        tmpdir: String,
        log: String,
        head: Value,
    },
}

/// The record of a gate rejection: the reason, the gate's output and the
/// note that quotes its tail.
fn rejected_fields(reason: &str, log: &str) -> Value {
    json!({"log": log, "reason": reason, "note": format!(
        "The gate rejected this work: {reason}.\n\nGate output (tail):\n{}",
        tail(log, NOTE_LOG_CHARS))})
}

/// The main the pushed `head` last took in, `git merge-base <head> <main>`,
/// when the landing candidate is a real merge: `None` when `head` contains
/// `main` (the candidate's tree is the head's, so a rejection of it is a
/// rejection of the head) or the two share no history.
fn head_base(mirror_path: &str, main: &str, head: &str) -> Option<String> {
    let contains = git(&[
        "--git-dir",
        mirror_path,
        "merge-base",
        "--is-ancestor",
        main,
        head,
    ])
    .is_ok();
    if contains {
        return None;
    }
    git(&["--git-dir", mirror_path, "merge-base", head, main])
        .ok()
        .filter(|base| !base.is_empty())
}

/// Main's first-parent commits in `head_base..main`, oldest first:
/// `(full hash, subject)`. The commits a head judged on `head_base` must
/// adapt to.
fn main_commits(
    mirror_path: &str,
    head_base: &str,
    main: &str,
) -> Result<Vec<(String, String)>, String> {
    let listing = git(&[
        "--git-dir",
        mirror_path,
        "log",
        "--first-parent",
        "--reverse",
        "--format=%H %s",
        &format!("{head_base}..{main}"),
    ])?;
    Ok(listing
        .lines()
        .filter_map(|line| {
            let (hash, subject) = line.split_once(' ').unwrap_or((line, ""));
            (!hash.is_empty()).then(|| (hash.to_string(), subject.to_string()))
        })
        .collect())
}

/// Main's commits a note lists at most; the record lists them all.
const NOTE_COMMITS: usize = 40;

/// Main's commits the note's first line names at most.
const HEADLINE_COMMITS: usize = 5;

/// Characters of the gate output's failure lines the note's first line
/// quotes at most.
const HEADLINE_FAILURE_CHARS: usize = 600;

/// The lines of a gate's output that say what failed, for the first line of
/// a note (a reader that shows one line shows the failure): those that name
/// an error or a failure, the verdict line left out, up to three, joined;
/// without any, the last line before the verdict. Only quoted: no verdict is
/// decided by it.
fn failure_lines(log: &str) -> String {
    let lines: Vec<&str> = log
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('{'))
        .collect();
    let failing: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("error") || lower.contains("fail") || lower.contains("panicked")
        })
        .take(3)
        .collect();
    let quoted = if failing.is_empty() {
        lines.last().copied().unwrap_or_default().to_string()
    } else {
        failing.join(" / ")
    };
    cut(&quoted, HEADLINE_FAILURE_CHARS).to_string()
}

/// The note an `incompatible` verdict sends back, from the record alone.
/// Its first line says it all for a reader that shows one line: the work
/// passes on its own base and fails merged with main, main's commits since
/// that base by their 12-digit prefix, the rejection's reason and the gate
/// output's failure lines. Then main's commits with their subjects, what to
/// do (main is merged into the workspace with this revision), and the
/// merge's gate output (its tail, as `rejected` quotes it).
fn incompatible_note(
    reason: &str,
    log: &str,
    head_base: &str,
    main: &str,
    commits: &[(String, String)],
) -> String {
    let short = |hash: &str| hash.chars().take(12).collect::<String>();
    let mut named: Vec<String> = commits
        .iter()
        .take(HEADLINE_COMMITS)
        .map(|(hash, _)| short(hash))
        .collect();
    if commits.len() > HEADLINE_COMMITS {
        named.push(format!("{} more", commits.len() - HEADLINE_COMMITS));
    }
    let failure = failure_lines(log);
    let mut note = format!(
        "Main moved under this work: it passes the gate on its own base {} but not merged with \
         main {}, which took in {} since: {reason}{}",
        short(head_base),
        short(main),
        if named.is_empty() {
            "nothing".to_string()
        } else {
            named.join(", ")
        },
        if failure.is_empty() {
            String::new()
        } else {
            format!(": {failure}")
        },
    );
    note.push_str(
        "\n\nThe merge was clean as text; what main's commits changed breaks it. Main's \
         commits since your work's base, oldest first (adapt the work to them):\n",
    );
    for (hash, subject) in commits.iter().take(NOTE_COMMITS) {
        let _ = writeln!(note, "- {} {subject}", short(hash));
    }
    if commits.len() > NOTE_COMMITS {
        let _ = writeln!(
            note,
            "- and {} more (`git log --first-parent {}..{}`)",
            commits.len() - NOTE_COMMITS,
            short(head_base),
            short(main)
        );
    }
    let _ = write!(
        note,
        "\nMain is merged into your workspace with this revision. Adapt the work to those \
         commits (the gate output below names what fails), check it, then report again. \
         This is not counted as a rejection of your work.\n\nGate output on the merge \
         (tail):\n{}",
        tail(log, NOTE_LOG_CHARS)
    );
    note
}

/// The verdict on `candidate`, the landing merge of `main` and the pushed
/// `head`, which the gate rejected (`merged`). When the merge is real (the
/// head does not contain main), the head alone is judged by the same gate
/// (`run_gate(base, candidate)`) against [`head_base`]: it passes, and the
/// work is `incompatible` (sent back with main's commits, not counted); it
/// fails, and the work is `rejected`, naming the head's verdict; its run
/// gives no verdict, and the pass is `aborted` (or `no_room`), naming the
/// head. The decision rests on the two verdicts alone, never on the text
/// of a reason.
fn rejection(
    mirror_path: &str,
    main: &str,
    candidate: &str,
    head: &str,
    merged: &gate::Verdict,
    run_gate: &dyn Fn(&str, &str) -> Result<gate::Verdict, GateFailure>,
) -> Result<Rejection, String> {
    let fields = rejected_fields(&merged.reason, &merged.output);
    let Some(head_base) = head_base(mirror_path, main, head) else {
        return Ok(Rejection::Record("rejected", fields));
    };
    let named = json!({"candidate": candidate, "head": head, "head_base": head_base});
    let with = |mut value: Value, extra: &Value| {
        if let (Some(target), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        value
    };
    Ok(match run_gate(&head_base, head) {
        Ok(alone) if alone.accepted => {
            let commits = main_commits(mirror_path, &head_base, main)?;
            let note =
                incompatible_note(&merged.reason, &merged.output, &head_base, main, &commits);
            let hashes: Vec<&str> = commits.iter().map(|(hash, _)| hash.as_str()).collect();
            Rejection::Record(
                "incompatible",
                with(
                    named,
                    &json!({"head_verdict": "accepted", "main_commits": hashes,
                            "reason": merged.reason, "log": merged.output, "note": note}),
                ),
            )
        }
        Ok(alone) => Rejection::Record(
            "rejected",
            with(
                with(fields, &named),
                &json!({"head_verdict": "rejected", "head_reason": alone.reason}),
            ),
        ),
        Err(GateFailure::NoRoom {
            free_kb,
            min_gb,
            tmpdir,
            log,
        }) => Rejection::HeadNoRoom {
            free_kb,
            min_gb,
            tmpdir,
            log,
            head: named,
        },
        Err(failure) => {
            let mut record = aborted(&failure);
            if let Some(reason) = record["reason"].as_str() {
                record["reason"] = json!(format!(
                    "judging the head {head} alone against {head_base}, after the gate \
                     rejected the merge with main ({}): {reason}",
                    merged.reason
                ));
            }
            Rejection::Record("aborted", with(record, &named))
        }
    })
}

/// The record of a gate run without a verdict to believe. A gate that never
/// returns, cannot run, or dies (killed, or exiting non-zero, whatever it
/// printed) says nothing about the work: an abort, never an acceptance.
fn aborted(failure: &GateFailure) -> Value {
    match failure {
        GateFailure::Timeout => json!({"reason": "the gate exceeded its wall clock"}),
        GateFailure::Failed(error) => json!({"reason": format!("the gate failed: {error}")}),
        GateFailure::Aborted { reason, log } => {
            json!({"reason": format!("the gate failed: {reason}"), "log": log})
        }
        // Recorded as `no_room` by the caller; counted here only if a
        // caller forgets that.
        GateFailure::NoRoom { tmpdir, log, .. } => {
            json!({"reason": format!("the gate had no room in {tmpdir}"), "log": log})
        }
    }
}

fn now_ms() -> i64 {
    spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0
}

/// Whether `task`'s gate waits this pass, said when it does: the gate had
/// no room last time (it waits out the backoff, then tries), or its last run
/// on it was a host fault (it waits until the recorded due time, then judges
/// it again). The mission's other tasks wait behind it.
fn gate_waits(mission: &Mission, task: &Task) -> bool {
    if let Some(wait) = room_wait(mission, task.id, now_ms()) {
        println!(
            "integrate: mission {}: the gate had no room; task {} is judged again in {}s",
            mission.id,
            task.id,
            wait.as_secs()
        );
        return true;
    }
    if let Some(wait) = host_wait(mission, task.id, now_ms()) {
        println!(
            "integrate: mission {}: the gate's last run on task {} was a host fault ({} in a \
             row); it is judged again in {}s",
            mission.id,
            task.id,
            mission.gate_host_faults,
            wait.as_millis().div_ceil(1000)
        );
        return true;
    }
    false
}

/// What [`land`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Landing {
    /// Recorded `task.landed`: the push moved main, or the candidate was
    /// already under it.
    Landed,
    /// Main moved past the task's base without it (or it had none): gated
    /// `stale`, judged again.
    Stale,
    /// No longer landing (its mission was dropped).
    Gone,
    /// Its source is held (the hold's event id): nothing was pushed, and the
    /// task waits behind the hold, `task.held` recorded once per hold.
    Held(i64),
}

/// Move the source's default branch from the task's base to its candidate,
/// only if it is still at the base, and record the landing. A branch that
/// moved meanwhile sends the task back to be judged again, unless the
/// candidate is already in its history: a push of an earlier pass that was
/// never recorded (the integrator stopped in between), with main moved on
/// top of it since. That landing happened, and is recorded so.
///
/// The owner's hold on the source is read from the store under the landing
/// lock, before any push, never from memory: on a held source nothing is
/// pushed. A candidate already under main landed before the hold and is
/// recorded `task.landed`; any other records `task.held` (once per task and
/// hold) and stays `landing`. So a hold never splits a push from its record.
fn land(
    store: &PostgresStore,
    mirror: &Path,
    source: &str,
    task: &Task,
) -> Result<Landing, String> {
    let (Some(base), Some(candidate)) = (task.base.as_deref(), task.candidate.as_deref()) else {
        return record(
            store,
            &json!({"type": "task.gated", "id": task.id, "verdict": "stale",
                    "revision": task.revisions}),
        )
        .map(|()| Landing::Stale);
    };
    // Held until the landing is recorded: dropping the mission waits for it,
    // and so does an owner's hold.
    let Some(_lock) = store
        .landing_hold(task.id)
        .map_err(|error| error.to_string())?
    else {
        println!(
            "integrate: task {} is no longer landing (its mission was dropped)",
            task.id
        );
        return Ok(Landing::Gone);
    };
    // The owner's hold, read now, under the lock: no push on a held source.
    if let Some(hold) = store
        .active_hold(source)
        .map_err(|error| error.to_string())?
    {
        return land_held(store, mirror, source, task, (base, candidate), hold);
    }
    let mirror_path = mirror.to_string_lossy().to_string();
    let (branch, _) = source_head(source)?;
    let pushed = git(&[
        "--git-dir",
        &mirror_path,
        "push",
        "--quiet",
        &format!("--force-with-lease={branch}:{base}"),
        "--",
        source,
        &format!("{candidate}:{branch}"),
    ]);
    // A push that failed may still have happened (a crash, a lost reply).
    let (_, now) = source_head(source)?;
    if pushed.is_err() && now != candidate {
        // Main moved on: past this candidate (it landed before), or past
        // its base without it. The fetch brings `now` into the mirror; one
        // that fails decides nothing this pass.
        sources::refresh(mirror, source)?;
        let under_main = git(&[
            "--git-dir",
            &mirror_path,
            "merge-base",
            "--is-ancestor",
            candidate,
            &now,
        ])
        .is_ok();
        if !under_main {
            println!(
                "integrate: task {}: {branch} moved to {now}; judging again",
                task.id
            );
            return record(
                store,
                &json!({"type": "task.gated", "id": task.id, "verdict": "stale",
                        "revision": task.revisions}),
            )
            .map(|()| Landing::Stale);
        }
        println!(
            "integrate: task {}: {candidate} is already under {branch} ({now})",
            task.id
        );
    } else {
        let _ = git(&[
            "--git-dir",
            &mirror_path,
            "update-ref",
            "refs/heads/main",
            candidate,
        ]);
    }
    record(
        store,
        &json!({"type": "task.landed", "id": task.id, "from": base, "to": candidate}),
    )?;
    println!("integrate: task {} landed {candidate} on {source}", task.id);
    Ok(Landing::Landed)
}

/// [`land`] on a source the owner holds (`hold`, read under the landing
/// lock the caller holds): nothing is pushed. A candidate already in the
/// source's main landed before the hold, and is recorded `task.landed`; any
/// other waits behind the hold, `task.held` recorded once per task and hold.
/// A fetch that fails shows nothing under main: the task waits.
fn land_held(
    store: &PostgresStore,
    mirror: &Path,
    source: &str,
    task: &Task,
    (base, candidate): (&str, &str),
    hold: i64,
) -> Result<Landing, String> {
    let (branch, now) = source_head(source)?;
    let under_main = now == candidate
        || (sources::refresh(mirror, source).is_ok()
            && git(&[
                "--git-dir",
                &mirror.to_string_lossy(),
                "merge-base",
                "--is-ancestor",
                candidate,
                &now,
            ])
            .is_ok());
    if under_main {
        println!(
            "integrate: task {}: {candidate} is already under {branch} ({now}), from before \
             hold {hold}",
            task.id
        );
        record(
            store,
            &json!({"type": "task.landed", "id": task.id, "from": base, "to": candidate}),
        )?;
        return Ok(Landing::Landed);
    }
    if store
        .task_held(task.id, hold, source)
        .map_err(|error| error.to_string())?
        .is_some()
    {
        println!(
            "integrate: task {} waits behind the owner's hold {hold} on {source}: nothing lands \
             there until a record ends it",
            task.id
        );
    }
    Ok(Landing::Held(hold))
}

/// One active owner's hold, as [`PostgresStore::source_holds`] reads it.
#[derive(Debug, Clone, Deserialize)]
struct SourceHold {
    source: String,
    hold: i64,
    expires_ms: i64,
}

/// The active holds and the database's clock when they were read.
#[derive(Debug, Clone, Deserialize)]
struct SourceHolds {
    now_ms: i64,
    #[serde(default)]
    holds: Vec<SourceHold>,
}

/// The holds whose `expires_ms` the database's clock (`now_ms`) has reached.
fn expired(holds: &SourceHolds) -> impl Iterator<Item = &SourceHold> {
    holds
        .holds
        .iter()
        .filter(move |hold| hold.expires_ms <= holds.now_ms)
}

/// Record `source.hold_expired` for every active hold whose `expires_ms` the
/// database's clock has reached, whichever missions are on its source. Only
/// that record ends the hold: past `expires_ms` without it nothing lands.
/// The store refuses it for a hold released or renewed meanwhile (the
/// record first wins), which is said and changes nothing.
fn expire_holds(store: &PostgresStore) -> Result<(), String> {
    let holds: SourceHolds =
        serde_json::from_value(store.source_holds().map_err(|error| error.to_string())?)
            .map_err(|error| format!("source holds: {error}"))?;
    for hold in expired(&holds) {
        match store.hold_expired(&hold.source, hold.hold) {
            Ok(event) => println!(
                "integrate: the owner's hold {} on {} reached its bound: expired (record {event}); \
                 landings there resume",
                hold.hold, hold.source
            ),
            Err(error) => eprintln!(
                "integrate: the owner's hold {} on {}: {error}",
                hold.hold, hold.source
            ),
        }
    }
    Ok(())
}

/// Why a mission on `source` can never land, if it cannot: agents' branches
/// reach only a directory on this host (the pusher's one destination since
/// the cross-host cut), so a gate on a URL would wait for a push nobody
/// makes, `stale` on every pass. `spawn_mission_configure` refuses such a
/// gate; one configured before that is halted, so a human sees it. A local
/// path that is missing now is not this: its fetch fails, and the pass
/// skips the mission.
fn unlandable(source: &str) -> Option<String> {
    (!source.starts_with('/')).then(|| {
        format!(
            "its source {source} is remote: agents' branches reach only a directory on this \
             host, so its work never lands"
        )
    })
}

/// Another mission's task landing on `mission`'s source, if one is:
/// `(mission, task)`. Its push may have moved main before its record. A task
/// waiting behind the active hold on the source is not one
/// ([`Mission::in_flight`]).
fn landing_elsewhere(missions: &[Mission], mission: &Mission) -> Option<(i64, i64)> {
    missions
        .iter()
        .filter(|other| other.id != mission.id && other.source == mission.source)
        .find_map(|other| other.in_flight().map(|task| (other.id, task.id)))
}

fn record(store: &PostgresStore, value: &Value) -> Result<(), String> {
    store
        .integrator_record(value)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn halt(store: &PostgresStore, mission: &Mission, reason: &str) -> Result<(), String> {
    record(
        store,
        &json!({"type": "mission.halted", "id": mission.id, "reason": reason}),
    )?;
    println!("integrate: mission {} HALTED: {reason}", mission.id);
    Ok(())
}

/// Submit the Stop of an aborted task's worker. Its outcome is the
/// command's: one that fails is pursued and escalated by the dispatcher
/// (`dispatch::stops`), like every Stop.
fn stop(store: &PostgresStore, task: &Task) -> Result<(), String> {
    let error = |error: &dyn std::fmt::Display| format!("stop task {}'s worker: {error}", task.id);
    let submission = Submission {
        agent_id: AgentId::new(task.agent_id.clone()).map_err(|e| error(&e))?,
        session_id: SessionId::new(task.session_id.clone()).map_err(|e| error(&e))?,
        idempotency_key: IdempotencyKey::new(format!("integrate-abort-{}", task.id))
            .map_err(|e| error(&e))?,
        action: CommandAction::Stop,
    };
    let principal = Principal::new(PRINCIPAL).map_err(|e| error(&e))?;
    store
        .submit(&principal, &submission)
        .map(|_| ())
        .map_err(|e| error(&e))
}

/// A path through the controller's symlink (`controllers/<name>` in the
/// live directory, switched by deploy/controller.sh), resolved once for a
/// step: what counts the scenarios, the gate and its scenarios are then one
/// deployed commit's, even if a switch happens while the gate runs (the
/// previous controller stays until nothing uses it). A path that does not
/// resolve stays as named; the step that reads it reports it.
fn pinned(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The scenario that judges an objective task: its file (`<objective>.sh`,
/// named when the task was assigned) in the mission's scenarios directory as
/// it is now. Not the recorded path: a mission moved to another directory (the
/// owner's checkout to the deployed controller) must not keep judging its
/// tasks in flight with a scenario from the old one, which nothing deploys.
fn task_scenario(scenarios: &Path, recorded: Option<&str>) -> Option<PathBuf> {
    let name = Path::new(recorded?).file_name()?;
    Some(scenarios.join(name))
}

#[cfg(test)]
mod tests {
    use super::{
        Freshness, Mission, NO_TEST_DATABASE, aborted, freshness, halt_reason, markers_note,
        merged_tree, needs_test_database, no_room, over_budget, pinned, report_freshness,
        room_backoff, room_wait, short_of_room, task_scenario, waits_for_gate, wall_clock_minutes,
        withheld_without_test_database,
    };
    use crate::gate::{GateFailure, Shortfall};
    use serde_json::json;

    /// One gate at a time unless the test server is per gate: more is
    /// refused without the opt-in, whatever asks for it.
    #[test]
    fn the_gate_cap_is_one_and_more_needs_a_server_per_gate() {
        use super::{DEFAULT_GATES, MAX_GATES, gate_slots};
        let per_gate = || Some("1");
        assert_eq!(gate_slots(None, None, None), Ok(1));
        assert_eq!(DEFAULT_GATES, 1);
        assert_eq!(gate_slots(Some("1".into()), None, None), Ok(1));
        for (flag, env) in [(Some("2"), None), (None, Some("3")), (Some("8"), Some("1"))] {
            let refused = gate_slots(flag.map(Into::into), env.map(Into::into), None);
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|error| error.contains("SPAWN_TEST_DATABASE_PER_GATE=1")),
                "{flag:?} {env:?}: {refused:?}"
            );
            assert!(gate_slots(flag.map(Into::into), env.map(Into::into), Some("yes")).is_err());
        }
        assert_eq!(gate_slots(None, Some("3".into()), per_gate()), Ok(3));
        assert_eq!(
            gate_slots(Some("1".into()), Some("3".into()), per_gate()),
            Ok(1),
            "the flag first"
        );
        for wrong in ["0", "-1", "two", ""] {
            assert!(
                gate_slots(Some(wrong.into()), None, per_gate()).is_err(),
                "{wrong}"
            );
        }
        assert!(gate_slots(None, Some((MAX_GATES + 1).to_string()), per_gate()).is_err());
    }

    /// Only exit 1 is a conflict: a merge-tree git cannot run (an unknown
    /// option, a crash, a signal) is an error carrying git's message.
    #[test]
    fn a_merge_git_cannot_run_is_an_error_not_a_conflict() {
        assert_eq!(
            merged_tree(Some(0), "abc123\n", ""),
            Ok(Some("abc123".to_string()))
        );
        assert_eq!(merged_tree(Some(1), "def456\nCONFLICT", ""), Ok(None));
        let unknown = merged_tree(
            Some(129),
            "",
            "error: unknown option `attr-source=abc'\nusage: git [-v | --version]",
        )
        .expect_err("exit 129 is no conflict");
        assert!(unknown.contains("unknown option"), "{unknown}");
        assert!(unknown.contains("exited 129"), "{unknown}");
        assert!(unknown.contains("not a conflict"), "{unknown}");
        assert!(unknown.contains("2.43"), "{unknown}");
        let crash = merged_tree(None, "", "").expect_err("a signal is no conflict");
        assert!(crash.contains("killed by a signal"), "{crash}");
        assert!(merged_tree(Some(139), "", "Segmentation fault").is_err());
        assert!(merged_tree(Some(128), "", "fatal: bad object").is_err());
        assert!(merged_tree(Some(2), "", "").is_err());
    }

    #[test]
    fn a_task_scenario_is_read_from_the_mission_s_scenarios_as_they_are_now() {
        let now = std::path::Path::new("/live/controllers/spawn-abc/rsi/scenarios");
        assert_eq!(
            task_scenario(now, Some("/home/owner/projects/Spawn/rsi/scenarios/007.sh")),
            Some(now.join("007.sh"))
        );
        assert_eq!(task_scenario(now, None), None);
        assert_eq!(task_scenario(now, Some("/")), None);
    }

    #[test]
    fn pinned_resolves_the_controller_symlink_once() {
        let base = std::env::temp_dir().join(format!("spawn-pinned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("spawn-one/rsi/scenarios")).expect("one");
        std::fs::create_dir_all(base.join("spawn-two/rsi/scenarios")).expect("two");
        std::os::unix::fs::symlink("spawn-one", base.join("spawn")).expect("link");
        let through = base.join("spawn/rsi/scenarios");
        let held = pinned(&through);
        assert!(
            held.ends_with("spawn-one/rsi/scenarios"),
            "{}",
            held.display()
        );
        // The switch: what was pinned stays the first controller's.
        std::fs::remove_file(base.join("spawn")).expect("unlink");
        std::os::unix::fs::symlink("spawn-two", base.join("spawn")).expect("relink");
        assert!(held.ends_with("spawn-one/rsi/scenarios"));
        assert!(pinned(&through).ends_with("spawn-two/rsi/scenarios"));
        // A path that does not resolve stays as named.
        let missing = base.join("missing/scenarios");
        assert_eq!(pinned(&missing), missing);
        std::fs::remove_dir_all(&base).expect("clean");
    }

    /// Commit everything in the checkout `directory`.
    pub(super) fn commit_in(directory: &str, message: &str) {
        let git = crate::sources::test_git;
        git(&["-C", directory, "add", "-A"]).expect("add");
        git(&[
            "-C",
            directory,
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--no-gpg-sign",
            "-qm",
            message,
        ])
        .expect("commit");
    }

    /// What the integrator does with an agent's push that passed: fetch it
    /// into its own ref, build the landing ([`super::landing_candidate`]),
    /// move the source's main and the mirror's as `land` does.
    fn land_as_the_integrator(mirror: &std::path::Path, source: &str, agent: &str, task: i64) {
        let git = crate::sources::test_git;
        let mirror_path = mirror.to_string_lossy().to_string();
        let tracking = format!("refs/spawn-integrate/{agent}");
        let pushed = crate::sources::fetch_pushed_into(mirror, source, agent, &tracking)
            .expect("fetch")
            .expect("pushed");
        let main = git(&["--git-dir", &mirror_path, "rev-parse", "refs/heads/main"]).expect("main");
        let message = format!("Work (task {task})\n\nThe work.\n");
        let candidate = super::landing_candidate(&mirror_path, &main, &pushed, &message)
            .expect("candidate")
            .expect("no conflict");
        assert_ne!(candidate, pushed, "a landing of its own");
        let to_main = format!("{candidate}:refs/heads/main");
        git(&["--git-dir", &mirror_path, "push", "-q", source, &to_main]).expect("land");
        git(&[
            "--git-dir",
            &mirror_path,
            "update-ref",
            "refs/heads/main",
            &candidate,
        ])
        .expect("mirror main");
    }

    /// After a landing, the agent's pushed branch is not left behind: the
    /// integrator writes no ref of the agent, and the Bridge's next merge of
    /// main is a new commit (which `MissionWorkspaces` marks for the
    /// pusher), so `spawn/<agent>` contains main again once pushed. Both
    /// landing shapes that are not the branch head itself: a commit without
    /// the large-file trailer, and a merge with a main that moved.
    #[test]
    fn a_landing_reaches_the_agents_pushed_branch_through_the_next_merge() {
        use crate::push::{Pushed, push_branch};
        use crate::sources::{ensure_mirror, test_git as git};
        use spawn_core::id::BranchName;
        use spawn_core::port::{BaseMerge, Commit, WorkspaceStore};
        use spawn_host::GitWorkspace;

        let base = std::env::temp_dir().join(format!("spawn-land-push-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        commit_in(&seed_path, "base");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
        let root = base.join("host");
        let mirror = ensure_mirror(&root, Some(&source)).expect("mirror");
        let mirror_path = mirror.to_string_lossy().to_string();
        let store = GitWorkspace::new(&mirror, &root);
        let rev = |reference: &str| {
            git(&["--git-dir", &mirror_path, "rev-parse", reference]).expect("rev-parse")
        };
        let source_rev = |reference: &str| {
            git(&["--git-dir", &source, "rev-parse", reference]).expect("rev-parse on source")
        };

        for (task, agent, main_moves) in [(1, "agent_large", false), (2, "agent_late", true)] {
            let branch = BranchName::new(agent).expect("valid branch");
            store.ensure_branch(&branch, None).expect("branch");
            let workspace = store.mount(&branch).expect("mount");
            let directory = std::path::Path::new(workspace.path());
            std::fs::write(directory.join(format!("{agent}.txt")), "work\n").expect("write");
            assert!(matches!(
                store.commit(&workspace, "work").expect("commit"),
                Commit::Created(_)
            ));
            if !main_moves {
                // A head an earlier version wrote, naming large files kept
                // outside git: the landing is a commit of its own, without
                // the trailer.
                let head = rev(&format!("refs/heads/{agent}"));
                let message = format!("work\n\nSpawn-Large: {}", "a".repeat(64));
                let named = git(&[
                    "--git-dir",
                    &mirror_path,
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit-tree",
                    &format!("{head}^{{tree}}"),
                    "-p",
                    &head,
                    "-m",
                    &message,
                ])
                .expect("a head with the trailer");
                let reference = format!("refs/heads/{agent}");
                git(&[
                    "--git-dir",
                    &mirror_path,
                    "update-ref",
                    &reference,
                    &named,
                    &head,
                ])
                .expect("move the branch");
            }
            assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
            if main_moves {
                // Main moved since the worker started.
                let clone = base.join(format!("clone-{agent}"));
                let clone_path = clone.to_string_lossy().to_string();
                git(&["clone", "-q", &source, &clone_path]).expect("clone");
                std::fs::write(clone.join("other.txt"), "theirs\n").expect("write");
                commit_in(&clone_path, "elsewhere");
                git(&["-C", &clone_path, "push", "-q", "origin", "main"]).expect("push main");
                crate::sources::refresh(&mirror, &source).expect("refresh");
            }

            let agent_head = rev(&format!("refs/heads/{agent}"));
            land_as_the_integrator(&mirror, &source, agent, task);
            assert_eq!(
                rev(&format!("refs/heads/{agent}")),
                agent_head,
                "the integrator moves no ref of the agent"
            );

            // The Bridge's next merge of main is a new commit, and once
            // pushed the agent's branch on the source holds the landing.
            let BaseMerge::Merged { commit, .. } = store.merge_base(&workspace).expect("merge")
            else {
                panic!("the landing is new to the branch");
            };
            assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
            let pushed_now = source_rev(&format!("refs/heads/spawn/{agent}"));
            assert_eq!(pushed_now, commit.as_str());
            git(&[
                "--git-dir",
                &source,
                "merge-base",
                "--is-ancestor",
                &source_rev("refs/heads/main"),
                &pushed_now,
            ])
            .expect("spawn/<agent> contains main");
            store.unmount(&workspace).expect("unmount");
        }
    }

    /// A source with `README.md` and `notes.txt` on main, its mirror under a
    /// host root, and that host's store: `(base, source, root, mirror, store)`.
    fn world(
        tag: &str,
    ) -> (
        std::path::PathBuf,
        String,
        std::path::PathBuf,
        std::path::PathBuf,
        spawn_host::GitWorkspace,
    ) {
        use crate::sources::{ensure_mirror, test_git as git};
        let base = std::env::temp_dir().join(format!("spawn-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "one\ntwo\nthree\n").expect("write");
        std::fs::write(seed.join("notes.txt"), "notes\n").expect("write");
        commit_in(&seed_path, "base");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
        let root = base.join("host");
        let mirror = ensure_mirror(&root, Some(&source)).expect("mirror");
        let store = spawn_host::GitWorkspace::new(&mirror, &root);
        (base, source, root, mirror, store)
    }

    /// Main moves on the source by `change` in a clone, and the mirror
    /// follows; main's new commit.
    fn main_moves(
        base: &std::path::Path,
        source: &str,
        mirror: &std::path::Path,
        change: impl Fn(&std::path::Path),
    ) -> String {
        use crate::sources::test_git as git;
        let clone = base.join(format!("clone-{}", fastrand_name()));
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", source, &clone_path]).expect("clone");
        change(&clone);
        commit_in(&clone_path, "main moves");
        git(&["-C", &clone_path, "push", "-q", "origin", "main"]).expect("push main");
        crate::sources::refresh(mirror, source).expect("refresh");
        git(&[
            "--git-dir",
            &mirror.to_string_lossy(),
            "rev-parse",
            "refs/heads/main",
        ])
        .expect("main")
    }

    fn fastrand_name() -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed).to_string()
    }

    /// An agent's branch with one checkpoint by `work`, mounted, and pushed
    /// as `spawn/<agent>`: its workspace.
    fn agent_with_work(
        store: &spawn_host::GitWorkspace,
        mirror: &std::path::Path,
        source: &str,
        agent: &str,
        work: impl Fn(&std::path::Path),
    ) -> spawn_core::id::WorkspaceId {
        use crate::push::{Pushed, push_branch};
        use spawn_core::id::BranchName;
        use spawn_core::port::{Commit, WorkspaceStore};
        let branch = BranchName::new(agent).expect("valid branch");
        store.ensure_branch(&branch, None).expect("branch");
        let workspace = store.mount(&branch).expect("mount");
        work(std::path::Path::new(workspace.path()));
        assert!(matches!(
            store.commit(&workspace, "work").expect("commit"),
            Commit::Created(_)
        ));
        assert_eq!(push_branch(mirror, source, agent), Pushed::Done);
        workspace
    }

    /// What one integrator pass judges of `agent`'s push against the
    /// mirror's main: the landing candidate, `None` for a conflict.
    fn judged(mirror: &std::path::Path, source: &str, agent: &str) -> Option<String> {
        let mirror_path = mirror.to_string_lossy().to_string();
        let pushed = crate::sources::fetch_pushed_into(
            mirror,
            source,
            agent,
            &format!("refs/spawn-integrate/{agent}"),
        )
        .expect("fetch")
        .expect("pushed");
        let main =
            crate::sources::test_git(&["--git-dir", &mirror_path, "rev-parse", "refs/heads/main"])
                .expect("main");
        super::landing_candidate(&mirror_path, &main, &pushed, "Work (task 1)\n\nThe work.\n")
            .expect("candidate")
    }

    /// The whole loop of a conflict: the verdict merges main into the idle
    /// workspace with markers and leaves the merge pending, its note names
    /// the marked file, a report before they are resolved is sent back, the
    /// checkpoint that resolves them has main as its second parent, and that
    /// work lands.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_conflict_verdict_leaves_markers_the_resolution_merges_main_and_lands() {
        use crate::main_merge::{Merge, conflict_note, into_workspace};
        use crate::push::{Pushed, push_branch};
        use crate::sources::test_git as git;
        use spawn_core::port::{Commit, WorkspaceStore};

        let (base, source, root, mirror, store) = world("conflict-loop");
        let mirror_path = mirror.to_string_lossy().to_string();
        let agent = "agent_resident";
        let workspace = agent_with_work(&store, &mirror, &source, agent, |directory| {
            std::fs::write(
                directory.join("README.md"),
                "one\nTWO by the agent\nthree\n",
            )
            .expect("write");
        });
        let main = main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("README.md"), "one\nTWO on main\nthree\n").expect("write");
        });
        assert_eq!(judged(&mirror, &source, agent), None, "a conflict verdict");

        // The verdict's merge, before the note is written.
        let merge = into_workspace(&store, &mirror, &root, agent);
        assert_eq!(
            merge,
            Merge::Marked {
                files: vec!["README.md".to_string()]
            }
        );
        assert!(
            store.has_pending_merge(agent),
            "main waits as the second parent"
        );
        let readme = std::fs::read_to_string(root.join(agent).join("README.md")).expect("read");
        assert!(
            readme.contains("<<<<<<<") && readme.contains("TWO on main"),
            "{readme}"
        );
        let note = conflict_note(&main, &merge);
        assert!(
            note.contains("README.md")
                && note.contains("carry conflict markers")
                && note.contains(&main)
                && !note.contains("within about 30 s"),
            "{note}"
        );
        // Asked again (the next delivery), the merge is still the same one.
        assert_eq!(into_workspace(&store, &mirror, &root, agent), merge);

        // A report with the markers in: the judge sends it back unjudged.
        assert!(matches!(
            store.commit(&workspace, "markers").expect("commit"),
            Commit::Created(_)
        ));
        assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
        let head = git(&[
            "--git-dir",
            &mirror_path,
            "rev-parse",
            &format!("refs/heads/{agent}"),
        ])
        .expect("head");
        let marked = store.conflict_markers(&main, &head).expect("markers");
        assert!(
            super::markers_note(store.has_pending_merge(agent), &marked).is_some(),
            "unresolved markers are not judged"
        );

        // The agent resolves them; that checkpoint records the merge.
        std::fs::write(
            root.join(agent).join("README.md"),
            "one\nTWO by the agent, on main\nthree\n",
        )
        .expect("resolve");
        let Commit::Created(resolved) = store.commit(&workspace, "resolved").expect("commit")
        else {
            panic!("the resolution is a commit");
        };
        assert_eq!(
            git(&[
                "--git-dir",
                &mirror_path,
                "rev-parse",
                &format!("{resolved}^2")
            ])
            .expect("second parent"),
            main,
            "main is the resolved checkpoint's second parent"
        );
        assert!(!store.has_pending_merge(agent));
        assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
        assert!(
            store
                .conflict_markers(&main, resolved.as_str())
                .expect("markers")
                .is_empty()
        );
        assert_eq!(super::markers_note(false, &[]), None);

        // Judged again: it contains main, so it lands as a commit of its
        // tree on top of main, with the resolution as its second parent.
        let candidate = judged(&mirror, &source, agent).expect("no conflict now");
        assert_ne!(candidate, resolved.as_str(), "never the worker's commit");
        let rev = |reference: String| {
            git(&["--git-dir", &mirror_path, "rev-parse", &reference]).expect("rev-parse")
        };
        assert_eq!(
            rev(format!("{candidate}^1")),
            main,
            "main is the first parent"
        );
        assert_eq!(rev(format!("{candidate}^2")), resolved.as_str());
        assert_eq!(
            rev(format!("{candidate}^{{tree}}")),
            rev(format!("{resolved}^{{tree}}"))
        );
        git(&[
            "--git-dir",
            &mirror_path,
            "push",
            "-q",
            &source,
            &format!("{candidate}:refs/heads/main"),
        ])
        .expect("lands with one commit on top of main");
        assert_eq!(
            git(&["--git-dir", &source, "show", "main:README.md"]).expect("main's README"),
            "one\nTWO by the agent, on main\nthree"
        );
        store.unmount(&workspace).expect("unmount");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A conflict markers cannot show: nothing in the workspace changes but
    /// main's version of each file, copied aside, and the note says what to
    /// do with each one. Done as it says, the work lands.
    #[test]
    fn a_conflict_without_markers_is_explained_and_resolvable_without_git() {
        use crate::main_merge::{MAIN_COPIES, Merge, conflict_note, into_workspace};
        use crate::push::{Pushed, push_branch};
        use spawn_core::port::{Commit, WorkspaceStore};

        let (base, source, root, mirror, store) = world("conflict-unmarked");
        let agent = "agent_unmarked";
        // The agent deletes notes.txt and changes README.md; main changes
        // notes.txt and deletes README.md.
        let workspace = agent_with_work(&store, &mirror, &source, agent, |directory| {
            std::fs::remove_file(directory.join("notes.txt")).expect("delete");
            std::fs::write(directory.join("README.md"), "one\nmine\nthree\n").expect("write");
            std::fs::write(directory.join("work.txt"), "my work\n").expect("write");
        });
        let main = main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("notes.txt"), "notes, revised\n").expect("write");
            std::fs::remove_file(clone.join("README.md")).expect("delete");
        });
        assert_eq!(judged(&mirror, &source, agent), None, "a conflict verdict");

        let merge = into_workspace(&store, &mirror, &root, agent);
        let Merge::Unmarked { files, copied } = &merge else {
            panic!("no markers can show this: {merge:?}");
        };
        assert_eq!(files, &["README.md".to_string(), "notes.txt".to_string()]);
        assert_eq!(copied, &["notes.txt".to_string()]);
        assert!(!store.has_pending_merge(agent));
        let directory = root.join(agent);
        assert_eq!(
            std::fs::read_to_string(directory.join(MAIN_COPIES).join("notes.txt")).expect("copy"),
            "notes, revised\n"
        );
        assert!(
            !directory.join("notes.txt").exists(),
            "the workspace is unchanged"
        );
        let note = conflict_note(&main, &merge);
        assert!(
            note.contains("cannot show")
                && note.contains(".spawn/main")
                && note.contains("make each of notes.txt exactly main's version")
                && note.contains("delete README.md"),
            "{note}"
        );

        // What the note says, done by hand: then it merges and lands.
        std::fs::copy(
            directory.join(MAIN_COPIES).join("notes.txt"),
            directory.join("notes.txt"),
        )
        .expect("copy main's version");
        std::fs::remove_file(directory.join("README.md")).expect("delete");
        assert!(matches!(
            store.commit(&workspace, "resolved").expect("commit"),
            Commit::Created(_)
        ));
        assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
        assert!(
            judged(&mirror, &source, agent).is_some(),
            "the resolved work merges with main"
        );
        store.unmount(&workspace).expect("unmount");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A task delivered to a worker whose branch does not contain main gets
    /// main first: a clean merge is a merge commit (the `base_merged`
    /// checkpoint) and the brief names what changed; a conflicting one
    /// leaves markers and the brief names the marked files. A worker with
    /// no workspace on this host gets nothing.
    #[test]
    fn a_delivery_to_a_stale_branch_merges_main_first() {
        use crate::main_merge::{Merge, delivery_note, into_workspace};
        use crate::sources::test_git as git;
        use spawn_core::port::WorkspaceStore;

        let (base, source, root, mirror, store) = world("delivery-merge");
        let mirror_path = mirror.to_string_lossy().to_string();
        let clean = agent_with_work(&store, &mirror, &source, "agent_clean", |directory| {
            std::fs::write(directory.join("clean.txt"), "mine\n").expect("write");
        });
        let marked = agent_with_work(&store, &mirror, &source, "agent_marked", |directory| {
            std::fs::write(
                directory.join("README.md"),
                "one\nTWO by the agent\nthree\n",
            )
            .expect("write");
        });
        let main = main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("README.md"), "one\nTWO on main\nthree\n").expect("write");
            std::fs::write(clone.join("added.txt"), "from main\n").expect("write");
        });

        let merge = into_workspace(&store, &mirror, &root, "agent_clean");
        let Merge::Merged { commit, files } = &merge else {
            panic!("a clean merge: {merge:?}");
        };
        assert_eq!(files, &["README.md".to_string(), "added.txt".to_string()]);
        assert_eq!(
            git(&[
                "--git-dir",
                &mirror_path,
                "rev-parse",
                "refs/heads/agent_clean"
            ])
            .expect("head"),
            *commit
        );
        assert_eq!(
            git(&[
                "--git-dir",
                &mirror_path,
                "rev-parse",
                &format!("{commit}^2")
            ])
            .expect("second parent"),
            main
        );
        assert!(root.join("agent_clean").join("added.txt").exists());
        let note = delivery_note(&merge, &[]).expect("the brief says so");
        assert!(
            note.contains("added.txt") && note.contains("without a conflict"),
            "{note}"
        );
        assert_eq!(
            into_workspace(&store, &mirror, &root, "agent_clean"),
            Merge::Current,
            "a branch that contains main is left alone"
        );
        // The merge is read back from the branch: a record whose append
        // failed is written at the next merge (causality audit #8).
        assert_eq!(
            crate::main_merge::head_merge(&mirror, "agent_clean"),
            Some(merge.clone())
        );
        assert_eq!(
            crate::main_merge::head_merge(&mirror, "agent_marked"),
            None,
            "a checkpoint is no merge"
        );
        assert_eq!(delivery_note(&Merge::Current, &[]), None);
        // The first delivery was refused before typing (a pane not ready);
        // its retry finds main already in, and still tells what the
        // recorded merge changed (causality audit #7).
        let note = delivery_note(&Merge::Current, &["added.txt".to_string()])
            .expect("the retry's brief says so");
        assert!(
            note.contains("added.txt") && note.contains("without a conflict"),
            "{note}"
        );

        let merge = into_workspace(&store, &mirror, &root, "agent_marked");
        assert_eq!(
            merge,
            Merge::Marked {
                files: vec!["README.md".to_string()]
            }
        );
        assert!(store.has_pending_merge("agent_marked"));
        let note = delivery_note(&merge, &[]).expect("the brief says so");
        assert!(
            note.contains("README.md") && note.contains("conflict markers"),
            "{note}"
        );

        assert_eq!(
            into_workspace(&store, &mirror, &root, "agent_elsewhere"),
            Merge::NotMounted
        );
        store.unmount(&clean).expect("unmount");
        store.unmount(&marked).expect("unmount");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// History c1 <- c2 <- c3, and d2 forked from c1: `contains(a, b)` when
    /// `a` is reachable from `b`.
    fn history(ancestor: &str, commit: &str) -> bool {
        let parents = |commit: &str| match commit {
            "c2" | "d2" => Some("c1"),
            "c3" => Some("c2"),
            _ => None,
        };
        let mut at = Some(commit);
        while let Some(current) = at {
            if current == ancestor {
                return true;
            }
            at = parents(current);
        }
        false
    }

    #[test]
    fn a_push_at_the_checkpoint_or_past_it_is_judged() {
        assert_eq!(
            freshness(Some("c2"), Some("c2"), history),
            Freshness::Fresh("c2".to_string()),
            "equal"
        );
        assert_eq!(
            freshness(Some("c2"), Some("c3"), history),
            Freshness::Fresh("c3".to_string()),
            "descendant"
        );
        assert_eq!(
            freshness(None, Some("c1"), history),
            Freshness::Fresh("c1".to_string()),
            "no checkpoint to compare with"
        );
    }

    #[test]
    fn a_push_that_lags_the_checkpoint_is_stale() {
        let behind = freshness(Some("c2"), Some("c1"), history);
        assert!(
            matches!(&behind, Freshness::Stale(reason) if reason.contains("behind")),
            "{behind:?}"
        );
        let diverged = freshness(Some("c2"), Some("d2"), history);
        assert!(
            matches!(&diverged, Freshness::Stale(reason)
                if reason == "the pushed branch (d2) does not contain the latest checkpoint (c2)"),
            "{diverged:?}"
        );
        let unknown = freshness(Some("elsewhere"), Some("c3"), history);
        assert!(
            matches!(unknown, Freshness::Stale(_)),
            "a checkpoint the mirror lacks"
        );
        let absent = freshness(Some("c2"), None, history);
        assert!(
            matches!(&absent, Freshness::Stale(reason) if reason.contains("no spawn/<agent>")),
            "{absent:?}"
        );
    }

    #[test]
    fn a_report_is_judged_against_its_own_checkpoint() {
        assert_eq!(
            report_freshness(Some("c2"), Some("c2"), Some("c2"), history),
            Freshness::Fresh("c2".to_string()),
            "equal"
        );
        assert_eq!(
            report_freshness(Some("c2"), Some("c2"), Some("c3"), history),
            Freshness::Fresh("c3".to_string()),
            "descendant"
        );
        let behind = report_freshness(Some("c2"), None, Some("c1"), history);
        assert!(
            matches!(&behind, Freshness::Stale(reason)
                if reason.contains("behind the report's checkpoint")),
            "the base pushed at the mount, with no checkpoint recorded: {behind:?}"
        );
        let diverged = report_freshness(Some("c2"), Some("c2"), Some("d2"), history);
        assert!(
            matches!(&diverged, Freshness::Stale(reason)
                if reason.contains("does not contain the report's checkpoint")),
            "{diverged:?}"
        );
        let absent = report_freshness(Some("c2"), None, None, history);
        assert!(
            matches!(&absent, Freshness::Stale(reason) if reason.contains("no spawn/<agent>")),
            "no pushed branch: {absent:?}"
        );
        // Past the report's checkpoint but behind a later one: the latest
        // checkpoint still rules.
        let lagging = report_freshness(Some("c1"), Some("c3"), Some("c2"), history);
        assert!(
            matches!(&lagging, Freshness::Stale(reason) if reason.contains("behind")),
            "{lagging:?}"
        );
        // A report that names no commit is judged as before.
        assert_eq!(
            report_freshness(None, None, Some("c1"), history),
            Freshness::Fresh("c1".to_string())
        );
        assert_eq!(
            report_freshness(None, None, None, history),
            Freshness::Nothing
        );
    }

    #[test]
    fn no_checkpoint_and_no_pushed_branch_is_nothing_to_land() {
        assert_eq!(freshness(None, None, history), Freshness::Nothing);
    }

    /// A scratch repository for the semantic-conflict tests: main M0 with
    /// `struct.txt` naming `a b`; a work branch from M0 whose head adds
    /// `use.txt` naming `uses`; main then moves to M1 (`struct.txt` names
    /// `a b c`, subject `Add field c (objective 1, task 36)`). With
    /// `head_on_main`, the work branch starts from M1 instead (its head
    /// contains main). `(base, mirror, m0, m1, head)`.
    fn semantic_world(
        tag: &str,
        uses: &str,
        head_on_main: bool,
    ) -> (std::path::PathBuf, String, String, String, String) {
        let git = crate::sources::test_git;
        let base =
            std::env::temp_dir().join(format!("spawn-semantic-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("struct.txt"), "a b\n").expect("write");
        commit_in(&seed_path, "base: Facts {a, b}");
        let m0 = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("m0");
        let work = |from: &str| {
            git(&["-C", &seed_path, "checkout", "-qb", "work", from]).expect("branch");
            std::fs::write(seed.join("use.txt"), format!("{uses}\n")).expect("write");
            commit_in(&seed_path, "spawn checkpoint exec_1");
            let head = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("head");
            git(&["-C", &seed_path, "checkout", "-q", "main"]).expect("main");
            head
        };
        let early = (!head_on_main).then(|| work(&m0));
        std::fs::write(seed.join("struct.txt"), "a b c\n").expect("write");
        commit_in(&seed_path, "Add field c (objective 1, task 36)");
        let m1 = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("m1");
        let head = early.unwrap_or_else(|| work(&m1));
        (base, format!("{seed_path}/.git"), m0, m1, head)
    }

    /// A fake gate that "builds" a tree: `use.txt`, when there, must name
    /// what `struct.txt` names, or it rejects with `candidate tests do not
    /// build` and a compiler line. It notes `<base> <candidate>` per run in
    /// `<base>/judged`. With `abort_alone`, a candidate that is not a merge
    /// (a head judged alone) gets no verdict.
    fn semantic_gate(base: &std::path::Path, abort_alone: bool) -> std::path::PathBuf {
        let judged = base.join("judged");
        let abort = if abort_alone {
            "git --git-dir=\"$1\" rev-parse -q --verify \"$3^2\" >/dev/null || { echo 'dies'; exit 3; }\n"
        } else {
            ""
        };
        let script = format!(
            "echo \"$2 $3\" >>'{}'\n{abort}\
             s=$(git --git-dir=\"$1\" show \"$3:struct.txt\")\n\
             u=$(git --git-dir=\"$1\" show \"$3:use.txt\" 2>/dev/null) || u=$s\n\
             if [ \"$s\" = \"$u\" ]; then\n\
               printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               echo '{{\"verdict\":\"accepted\"}}'\n\
             else\n\
               echo 'error[E0063]: missing field `c` in initializer of `Facts`'\n\
               printf '{{\"verdict\":\"rejected\",\"reason\":\"candidate tests do not build\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               echo '{{\"verdict\":\"rejected\",\"reason\":\"candidate tests do not build\"}}'\n\
             fi\n",
            judged.display()
        );
        let path = base.join("gate.sh");
        std::fs::write(&path, script).expect("gate");
        path
    }

    /// The runs the semantic gate noted: `(base, candidate)`.
    fn semantic_runs(base: &std::path::Path) -> Vec<(String, String)> {
        std::fs::read_to_string(base.join("judged"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                line.split_once(' ')
                    .map(|(a, b)| (a.to_string(), b.to_string()))
            })
            .collect()
    }

    /// Judge the landing of `head` on `main` with the semantic gate, as
    /// `judge_pushed` does once the merge is rejected: the rejected merge's
    /// verdict (it must be a rejection), the landing candidate and what
    /// [`super::rejection`] makes of it.
    fn judge_semantic(
        base: &std::path::Path,
        mirror: &str,
        main: &str,
        head: &str,
        abort_alone: bool,
    ) -> (String, super::Rejection) {
        let gate_path = semantic_gate(base, abort_alone);
        let scenarios = base.join("scenarios");
        std::fs::create_dir_all(&scenarios).expect("scenarios");
        let env: Vec<(String, String)> = std::env::var("PATH")
            .map(|path| vec![("PATH".to_string(), path)])
            .unwrap_or_default();
        let run_gate = |against: &str, judged: &str| {
            crate::gate::judge(
                &gate_path,
                std::path::Path::new(mirror),
                against,
                judged,
                &scenarios,
                None,
                &env,
                std::time::Duration::from_mins(1),
            )
        };
        let candidate = super::landing_candidate(mirror, main, head, "Work (task 48)\n")
            .expect("a candidate")
            .expect("a clean merge");
        let merged = run_gate(main, &candidate).expect("a verdict on the merge");
        assert!(
            !merged.accepted,
            "the merge fails the gate: {}",
            merged.output
        );
        let rejection =
            super::rejection(mirror, main, &candidate, head, &merged, &run_gate).expect("judged");
        (candidate, rejection)
    }

    /// Objective 16: a head right on its own base, broken only by what
    /// landed on main beside it, is `incompatible`: sent back with the
    /// gate's output and main's commits, judged by the head's own verdict
    /// against its merge-base with main.
    #[test]
    fn a_rejected_merge_whose_head_passes_alone_is_incompatible() {
        let (base, mirror, m0, m1, head) = semantic_world("incompatible", "a b", false);
        let (candidate, rejection) = judge_semantic(&base, &mirror, &m1, &head, false);
        let super::Rejection::Record(verdict, record) = rejection else {
            panic!("a record: {rejection:?}");
        };
        assert_eq!(verdict, "incompatible", "{record}");
        assert_eq!(record["candidate"], candidate.as_str());
        assert_eq!(record["head"], head.as_str());
        assert_eq!(record["head_base"], m0.as_str());
        assert_eq!(record["head_verdict"], "accepted");
        assert_eq!(record["main_commits"], json!([m1]));
        assert_eq!(record["reason"], "candidate tests do not build");
        assert!(
            record["log"]
                .as_str()
                .is_some_and(|log| log.contains("missing field `c`")),
            "{record}"
        );
        let note = record["note"].as_str().expect("a note");
        let first = note.lines().next().unwrap_or_default();
        assert!(
            first.contains("missing field `c` in initializer of `Facts`")
                && first.contains(&m1[..12]),
            "the first line says it all: {first}"
        );
        for wanted in [
            "error[E0063]: missing field `c` in initializer of `Facts`",
            &m1[..12],
            "Add field c (objective 1, task 36)",
            "passes the gate on its own base",
            "Main is merged into your workspace with this revision",
        ] {
            assert!(note.contains(wanted), "{wanted:?} in {note}");
        }
        assert_eq!(
            semantic_runs(&base),
            vec![(m1.clone(), candidate), (m0, head)],
            "the merge against main, then the head alone against its base"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The head fails the gate on its own base too: a rejection, counted,
    /// that names the head's verdict.
    #[test]
    fn a_rejected_merge_whose_head_fails_alone_is_rejected() {
        let (base, mirror, m0, m1, head) = semantic_world("head-fails", "a b z", false);
        let (candidate, rejection) = judge_semantic(&base, &mirror, &m1, &head, false);
        let super::Rejection::Record(verdict, record) = rejection else {
            panic!("a record: {rejection:?}");
        };
        assert_eq!(verdict, "rejected", "{record}");
        assert_eq!(record["head"], head.as_str());
        assert_eq!(record["head_base"], m0.as_str());
        assert_eq!(record["head_verdict"], "rejected");
        assert_eq!(record["candidate"], candidate.as_str());
        assert!(
            record["note"]
                .as_str()
                .is_some_and(|note| note.starts_with("The gate rejected this work")),
            "{record}"
        );
        assert_eq!(semantic_runs(&base).len(), 2, "the head was judged alone");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A head that contains main is what the gate judged: a rejection of
    /// the candidate is a rejection, with no second run.
    #[test]
    fn a_rejected_candidate_whose_head_contains_main_is_rejected_without_a_second_run() {
        let (base, mirror, _m0, m1, head) = semantic_world("head-on-main", "a b", true);
        let (candidate, rejection) = judge_semantic(&base, &mirror, &m1, &head, false);
        let super::Rejection::Record(verdict, record) = rejection else {
            panic!("a record: {rejection:?}");
        };
        assert_eq!(verdict, "rejected", "{record}");
        assert!(record.get("head_verdict").is_none(), "{record}");
        assert!(record.get("head").is_none(), "{record}");
        assert_eq!(semantic_runs(&base), vec![(m1, candidate)], "one run");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The head's own run gives no verdict: an abort naming the head, never
    /// `incompatible` on no evidence.
    #[test]
    fn a_head_run_without_a_verdict_is_aborted() {
        let (base, mirror, m0, m1, head) = semantic_world("head-aborts", "a b", false);
        let (candidate, rejection) = judge_semantic(&base, &mirror, &m1, &head, true);
        let super::Rejection::Record(verdict, record) = rejection else {
            panic!("a record: {rejection:?}");
        };
        assert_eq!(verdict, "aborted", "{record}");
        assert_eq!(record["head"], head.as_str());
        assert_eq!(record["head_base"], m0.as_str());
        assert_eq!(record["candidate"], candidate.as_str());
        assert!(record.get("head_verdict").is_none(), "{record}");
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains(&head) && reason.contains("gate failed")),
            "{record}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The note lists at most [`super::NOTE_COMMITS`] of main's commits and
    /// says how many more there are.
    #[test]
    fn an_incompatible_note_bounds_the_commits_it_lists() {
        let commits: Vec<(String, String)> = (0..super::NOTE_COMMITS + 3)
            .map(|n| {
                (
                    format!("{n:012x}{}", "a".repeat(28)),
                    format!("landing {n}"),
                )
            })
            .collect();
        let note = super::incompatible_note(
            "candidate tests do not build",
            "error: x\n",
            &"b".repeat(40),
            &"c".repeat(40),
            &commits,
        );
        assert!(note.contains("landing 0\n"), "{note}");
        let first = note.lines().next().unwrap_or_default();
        assert!(first.contains(&format!("{:012x}", 4)), "{first}");
        assert!(!first.contains(&format!("{:012x}", 5)), "{first}");
        assert!(
            first.contains("38 more") && first.ends_with(": error: x"),
            "{first}"
        );
        assert!(
            !note.contains(&format!("landing {}\n", super::NOTE_COMMITS)),
            "{note}"
        );
        assert!(note.contains("and 3 more"), "{note}");
        assert!(note.ends_with("error: x\n"), "{note}");
    }

    #[test]
    fn unresolved_markers_send_work_back_before_the_gate() {
        assert_eq!(markers_note(false, &[]), None);
        let note = markers_note(false, &["README.md".to_string()]).expect("a note");
        assert!(
            note.contains("README.md") && note.contains("do not rewrite"),
            "{note}"
        );
        let note = markers_note(true, &[]).expect("a pending merge is enough");
        assert!(note.contains("the files the merge listed"), "{note}");
    }

    #[test]
    fn a_gate_without_a_believable_verdict_is_recorded_as_an_abort_with_its_log() {
        let record = aborted(&GateFailure::Aborted {
            reason: "the gate ended with signal: 9 (SIGKILL)".to_string(),
            log: "{\"verdict\":\"accepted\"}\n".to_string(),
        });
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|r| r.contains("SIGKILL"))
        );
        assert!(
            record["log"]
                .as_str()
                .is_some_and(|l| l.contains("accepted"))
        );
        assert!(
            record.get("verdict").is_none(),
            "the caller records it as aborted"
        );
        assert!(
            aborted(&GateFailure::Timeout)["reason"]
                .as_str()
                .is_some_and(|r| r.contains("wall clock"))
        );
    }

    fn mission(rejections: u32, aborts: u32, floor: u32) -> Mission {
        serde_json::from_value(json!({
            "id": 1, "gate": "/g", "accept": "review",
            "gate_rejections": rejections, "gate_aborts": aborts, "scenario_floor": floor,
        }))
        .expect("a mission")
    }

    #[test]
    fn three_in_a_row_halt_and_the_scenario_floor_holds() {
        assert_eq!(halt_reason(&mission(2, 2, 7), 7), None);
        assert!(halt_reason(&mission(3, 0, 0), 7).is_some_and(|r| r.contains("rejections")));
        assert!(halt_reason(&mission(0, 3, 0), 7).is_some_and(|r| r.contains("aborts")));
        assert!(halt_reason(&mission(0, 0, 7), 6).is_some_and(|r| r.contains("fell")));
        assert_eq!(halt_reason(&mission(0, 0, 0), 0), None, "no floor yet");
    }

    const MINUTE: i64 = 60_000;

    #[test]
    fn no_room_backs_off_doubling_to_a_cap() {
        let minutes: Vec<u64> = (0..=6)
            .map(|waits| room_backoff(waits).as_secs() / 60)
            .collect();
        assert_eq!(minutes, [0, 5, 10, 20, 40, 40, 40]);
        assert_eq!(room_backoff(u32::MAX).as_secs(), 40 * 60, "no overflow");
    }

    #[test]
    fn the_gate_waits_out_its_backoff_from_the_recorded_no_room() {
        let mut waiting = mission(0, 0, 0);
        assert_eq!(
            room_wait(&waiting, 4, 0),
            None,
            "no run of no_room: judge now"
        );
        waiting.gate_room_task = Some(4);
        waiting.gate_room_since_ms = Some(100 * MINUTE);
        waiting.gate_room_last_ms = Some(110 * MINUTE);
        waiting.gate_room_waits = 2;
        assert_eq!(
            room_wait(&waiting, 4, 112 * MINUTE).map(|wait| wait.as_secs()),
            Some(8 * 60)
        );
        assert_eq!(room_wait(&waiting, 4, 120 * MINUTE), None, "backoff over");
        assert_eq!(
            room_wait(&waiting, 5, 112 * MINUTE),
            None,
            "another task does not inherit the backoff"
        );
    }

    #[test]
    fn no_room_names_the_reason_and_the_free_space_and_escalates_once_after_two_hours() {
        let mut waiting = mission(0, 2, 0);
        let first = no_room(
            &waiting,
            4,
            3 * 1_048_576 + 104_858,
            10,
            "/data/tmp",
            "log",
            0,
        );
        assert_eq!(
            first["reason"],
            "no room to judge now: 3.1 GiB free in /data/tmp (TMPDIR), under \
             SPAWN_GATE_MIN_FREE_GB=10; not a verdict, judged again later"
        );
        assert_eq!(first["free_kb"], 3 * 1_048_576 + 104_858);
        assert_eq!(first["tmpdir"], "/data/tmp");
        assert_eq!(first["min_gb"], 10);
        assert_eq!(first["escalate"], false, "the first of a run");
        waiting.gate_room_task = Some(4);
        waiting.gate_room_since_ms = Some(0);
        let at = |mission: &Mission, minutes: i64| {
            no_room(mission, 4, 1, 10, "/t", "", minutes * MINUTE)["escalate"].clone()
        };
        assert_eq!(
            no_room(&waiting, 5, 1, 10, "/t", "", 500 * MINUTE)["escalate"],
            false,
            "a later task's first no_room starts its own run"
        );
        assert_eq!(
            at(&waiting, 120),
            false,
            "two hours exactly is not more than two"
        );
        assert_eq!(at(&waiting, 121), true);
        waiting.gate_room_escalated = true;
        assert_eq!(at(&waiting, 500), false, "told once per run");
        // And it never counts toward the halt the mission already nears.
        assert_eq!(halt_reason(&waiting, 0), None);
    }

    /// Objective 018: every `no_room` names its check, what it measured and
    /// the threshold, in its fields and in its reason; a disk one keeps the
    /// fields and wording it had; the database's names the server, never
    /// the URL; and escalation is the run's, whatever the check.
    #[test]
    fn a_no_room_names_its_check_measure_and_threshold() {
        let waiting = mission(0, 0, 0);
        let disk = no_room(&waiting, 4, 3 * 1_048_576, 10, "/data/tmp", "log", 0);
        assert_eq!(
            (&disk["check"], &disk["measured"], &disk["threshold"]),
            (
                &json!("disk"),
                &json!(3 * 1_048_576),
                &json!(10 * 1_048_576)
            )
        );
        assert_eq!(
            (&disk["free_kb"], &disk["min_gb"], &disk["tmpdir"]),
            (&json!(3 * 1_048_576), &json!(10), &json!("/data/tmp"))
        );

        let memory = short_of_room(
            &waiting,
            4,
            &Shortfall::Memory {
                available_mb: 1500,
                min_mb: 2048,
            },
            "memory: 1500 MiB available\n",
            0,
        );
        assert_eq!(
            memory["reason"],
            "no room to judge now: the host's memory has 1500 MiB available (MemAvailable), \
             under SPAWN_GATE_MIN_FREE_MEMORY_MB=2048; not a verdict, judged again later"
        );
        assert_eq!(
            (&memory["check"], &memory["measured"], &memory["threshold"]),
            (&json!("memory"), &json!(1500), &json!(2048))
        );
        assert_eq!(memory["log"], "memory: 1500 MiB available\n");
        assert_eq!(memory["escalate"], false);
        assert!(memory.get("free_kb").is_none() && memory.get("tmpdir").is_none());

        let error = "ERROR: could not resize shared memory segment \"/PostgreSQL.2\" to 8388608 \
                     bytes: No space left on device (53100)";
        let database = Shortfall::Database {
            server: "/var/run/test-pg:5433/spawn_test".to_string(),
            probe: "a parallel query within 10 s".to_string(),
            error: error.to_string(),
        };
        let record = short_of_room(&waiting, 4, &database, "", 0);
        assert_eq!(
            (&record["check"], &record["measured"], &record["threshold"]),
            (
                &json!("database"),
                &json!(error),
                &json!("a parallel query within 10 s")
            )
        );
        assert_eq!(record["server"], "/var/run/test-pg:5433/spawn_test");
        let reason = record["reason"].as_str().expect("a reason");
        assert!(
            reason.starts_with(
                "no room to judge now: the test database (/var/run/test-pg:5433/spawn_test) \
                 failed the probe \"a parallel query within 10 s\": ERROR: could not resize"
            ) && reason.ends_with("not a verdict, judged again later"),
            "{reason}"
        );

        // Two hours into the task's run: the first record escalates,
        // whichever check fell short.
        let mut long = mission(0, 0, 0);
        long.gate_room_task = Some(4);
        long.gate_room_since_ms = Some(0);
        assert_eq!(
            short_of_room(&long, 4, &database, "", 121 * MINUTE)["escalate"],
            true
        );
    }

    #[test]
    fn a_gate_without_the_test_database_does_not_wait_for_one_that_has_it() {
        // One shared-server gate allowed: a second one waits, a gate that gets
        // no test database never does, however many share the server.
        assert!(!waits_for_gate(true, 0, 1));
        assert!(waits_for_gate(true, 1, 1));
        assert!(!waits_for_gate(false, 1, 1));
        assert!(!waits_for_gate(false, 8, 1));
        assert!(!waits_for_gate(true, 2, 3));
    }

    #[test]
    fn the_owner_marks_a_mission_whose_gate_needs_no_test_database() {
        let dir = std::env::temp_dir().join(format!("spawn-no-test-db-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        assert!(
            needs_test_database(&dir),
            "the default is the shared server"
        );
        std::fs::write(dir.join(NO_TEST_DATABASE), "").expect("the marker");
        assert!(!needs_test_database(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_gate_without_the_test_database_gets_no_server_of_its_own_either() {
        for name in [
            "SPAWN_TEST_DATABASE_URL",
            "SPAWN_GATE_PG_IMAGE",
            "SPAWN_GATE_PG_BIN",
        ] {
            assert!(withheld_without_test_database(name), "{name}");
        }
        for name in [
            "PATH",
            "TMPDIR",
            "SPAWN_GATE_JOBS",
            "SPAWN_GATE_MIN_FREE_GB",
        ] {
            assert!(!withheld_without_test_database(name), "{name}");
        }
    }

    #[test]
    fn the_gate_wall_clock_is_a_setting_with_a_default() {
        let minutes = |text: Option<&str>| wall_clock_minutes(text).as_secs() / 60;
        assert_eq!(minutes(None), 120);
        assert_eq!(minutes(Some("90")), 90);
        assert_eq!(minutes(Some(" 240 ")), 240);
        // Nothing that is not a whole number of minutes from 1 up is taken.
        for bad in ["", "0", "-5", "1.5", "two hours"] {
            assert_eq!(minutes(Some(bad)), 120, "{bad:?}");
        }
    }

    #[test]
    fn a_budget_bounds_minutes_and_tool_calls() {
        let budget = json!({"minutes": 45, "tool_calls": 60});
        assert_eq!(over_budget(&budget, 44 * 60_000, 60), None);
        assert!(over_budget(&budget, 46 * 60_000, 0).is_some());
        assert!(over_budget(&budget, 0, 61).is_some());
        assert_eq!(over_budget(&json!({}), i64::MAX, i64::MAX), None);
    }

    /// Objective 34: a task whose `task.held` names the active hold on its
    /// source pushed nothing, so it is no landing in flight: the other
    /// missions on the source do not wait for it. A `landing` task without
    /// one (not held yet, or held by a hold that ended) still makes them
    /// wait, as before.
    #[test]
    fn landing_elsewhere_ignores_a_task_waiting_behind_the_active_hold() {
        let on_source = |id: i64, hold: Option<i64>, held_by: Option<i64>| {
            let mut found = mission(0, 0, 0);
            found.id = id;
            found.source = Some("/srv/s.git".to_string());
            found.hold = hold;
            found.tasks = serde_json::from_value(json!([
                {"id": id * 10, "agent_id": "agent_a", "session_id": "session_a",
                 "state": "landing", "held_by": held_by},
            ]))
            .expect("tasks");
            found
        };
        let waiting = on_source(2, None, None);
        for (hold, held_by, waits) in [
            (Some(7), Some(7), false),
            (Some(7), None, true),
            (Some(9), Some(7), true),
            (None, Some(7), true),
            (None, None, true),
        ] {
            let missions = [waiting.clone(), on_source(1, hold, held_by)];
            assert_eq!(
                super::landing_elsewhere(&missions, &missions[0]),
                waits.then_some((1, 10)),
                "hold {hold:?}, held_by {held_by:?}"
            );
        }
    }

    #[test]
    fn the_ready_task_follows_the_acceptance_rule() {
        let mut gated = mission(0, 0, 0);
        gated.tasks = serde_json::from_value(json!([
            {"id": 1, "agent_id": "agent_a", "session_id": "session_a", "state": "reported"},
            {"id": 2, "agent_id": "agent_b", "session_id": "session_b", "state": "accepted"},
        ]))
        .expect("tasks");
        assert_eq!(gated.ready().map(|task| task.id), Some(2));
        gated.accept = "gate".to_string();
        assert_eq!(gated.ready().map(|task| task.id), Some(1));
        // A report the supervisor accepted anyway is judged too, not left
        // accepted forever.
        gated.tasks.remove(0);
        assert_eq!(gated.ready().map(|task| task.id), Some(2));
    }

    /// A task as `GATED` reads it, with a brief and a report.
    fn task_of(id: i64, objective: Option<u32>, brief: &str, report: Option<&str>) -> super::Task {
        serde_json::from_value(json!({
            "id": id, "agent_id": "agent_a", "session_id": "session_a", "state": "landing",
            "objective": objective, "brief": brief, "report": report,
        }))
        .expect("a task")
    }

    const REPORT: &str = "## Outcome\ndone\n\n## What changed\nsummary: HARBOUR.txt now holds \
                          kestrel\n\n### Details\nOne file.\n\n## Open issues\nnone\n";

    #[test]
    fn an_objective_landing_names_its_title_objective_and_task_and_summarises_the_report() {
        let task = task_of(
            42,
            Some(931),
            "Objective 931: Greet the harbour with a kestrel\n\nWrite the file.\n\
             Earlier gate verdicts on this objective, newest first:\n- task 41: rejected\n",
            Some(REPORT),
        );
        let message = super::landing_message(&task, "abc123");
        let mut lines = message.lines();
        assert_eq!(
            lines.next(),
            Some("Greet the harbour with a kestrel (objective 931, task 42)")
        );
        assert_eq!(lines.next(), Some(""));
        let body: String = lines.collect::<Vec<_>>().join("\n");
        assert!(
            body.starts_with("summary: HARBOUR.txt now holds kestrel\n\n### Details\nOne file."),
            "{body}"
        );
        assert!(
            !body.contains("## Outcome") && !body.contains("Open issues"),
            "{body}"
        );
        assert!(!message.contains("task 41"), "no other task: {message}");
        assert!(
            message.ends_with("\n\nSpawn-Task: 42\nSpawn-Objective: 931\nSpawn-Head: abc123\n"),
            "{message}"
        );
        assert!(!spawn_host::names_files_outside_git(&message));
    }

    #[test]
    fn a_landing_without_an_objective_names_its_brief_and_task() {
        let task = task_of(
            5,
            None,
            "\n\n  Fix the parser's error messages  \nMore.\n",
            Some(REPORT),
        );
        let message = super::landing_message(&task, "abc123");
        assert_eq!(
            message.lines().next(),
            Some("Fix the parser's error messages (task 5)")
        );
        assert!(!message.contains("Spawn-Objective"), "{message}");
        assert!(message.contains("\nsummary: HARBOUR.txt now holds kestrel\n"));
        // A long first line is cut; an empty brief still names the task;
        // nothing reads like a checkpoint.
        let long = task_of(6, None, &"word ".repeat(100), None);
        let first = super::landing_message(&long, "h")
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        assert!(first.len() < 120 && first.ends_with(" (task 6)"), "{first}");
        assert!(
            super::landing_message(&task_of(7, None, "", None), "h").starts_with("Land task 7\n\n")
        );
        let checkpoint = task_of(8, None, "spawn checkpoint exec_3", None);
        assert!(super::landing_message(&checkpoint, "h").starts_with("Land spawn checkpoint"));
    }

    #[test]
    fn a_report_without_what_changed_is_quoted_whole_and_a_long_one_is_bounded() {
        let task = task_of(
            3,
            Some(7),
            "Objective 7: Title",
            Some("\n  I changed the parser.\n\nAll green.\n\n"),
        );
        let message = super::landing_message(&task, "h");
        assert!(
            message.starts_with("Title (objective 7, task 3)\n\nI changed the parser.\n\nAll green.\n\nSpawn-Task: 3\n"),
            "{message}"
        );
        let report = format!(
            "## What changed\n{}\n## Open issues\nnone\n",
            "é long line\n".repeat(2000)
        );
        let long = super::landing_message(
            &task_of(3, Some(7), "Objective 7: Title", Some(&report)),
            "h",
        );
        assert!(
            long.chars().count() < super::SUMMARY_CHARS + 300,
            "{}",
            long.len()
        );
        assert!(long.contains("[the report continues]"));
        assert!(!long.contains("Open issues"));
        let empty = super::landing_message(&task_of(3, Some(7), "Objective 7: Title", None), "h");
        assert!(
            empty.contains("\n\nThe task's report is empty.\n\nSpawn-Task: 3"),
            "{empty}"
        );
    }

    /// A worker's report cannot make main unmountable, nor sign the landing
    /// with trailers it did not earn.
    #[test]
    fn a_report_naming_a_large_file_manifest_leaves_the_message_clean() {
        let manifest = format!("Spawn-Large: {}", "a".repeat(64));
        for report in [
            format!("## What changed\nwork\n\n{manifest}\n"),
            format!("{manifest}\nSpawn-Task: 99\n"),
            manifest.clone(),
        ] {
            let message = super::landing_message(&task_of(4, None, "Brief", Some(&report)), "h");
            assert!(!spawn_host::names_files_outside_git(&message), "{message}");
            assert!(!message.lines().any(|line| line == manifest), "{message}");
            assert!(
                !message.lines().any(|line| line == "Spawn-Task: 99"),
                "{message}"
            );
            assert!(
                message.contains(&format!("    {manifest}")),
                "indented: {message}"
            );
            let trailers = message.rsplit("\n\n").next().unwrap_or_default();
            assert_eq!(trailers, "Spawn-Task: 4\nSpawn-Head: h\n");
        }
    }

    /// Every landing is one merge commit: main first, the pushed head
    /// second, the message given. Its tree is the head's when main is in it
    /// (here a head naming a large-file manifest, which lands with the
    /// landing's own message), and the merge's when main moved.
    #[test]
    fn a_landing_is_one_two_parent_commit_on_main_with_its_message() {
        let git = crate::sources::test_git;
        let base = std::env::temp_dir().join(format!("spawn-land-shape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        commit_in(&seed_path, "base");
        let main = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("main");
        git(&["-C", &seed_path, "checkout", "-qb", "agent_a"]).expect("branch");
        std::fs::write(seed.join("one.txt"), "one\n").expect("write");
        commit_in(&seed_path, "spawn checkpoint exec_1");
        let checkpoint = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("checkpoint");
        std::fs::write(seed.join("two.txt"), "two\n").expect("write");
        commit_in(
            &seed_path,
            &format!("spawn checkpoint exec_2\n\nSpawn-Large: {}", "b".repeat(64)),
        );
        let head = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("head");
        git(&["-C", &seed_path, "checkout", "-q", "main"]).expect("main");
        std::fs::write(seed.join("other.txt"), "theirs\n").expect("write");
        commit_in(&seed_path, "elsewhere");
        let moved = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("moved");
        let mirror = format!("{seed_path}/.git");
        let rev = |reference: String| {
            git(&["--git-dir", &mirror, "rev-parse", &reference]).expect("rev-parse")
        };
        let task = task_of(
            9,
            Some(12),
            "Objective 12: Count the gulls",
            Some("## What changed\ngulls\n"),
        );
        let message = super::landing_message(&task, &head);

        for (main, merged) in [(main.as_str(), false), (moved.as_str(), true)] {
            let landing = super::landing_candidate(&mirror, main, &head, &message)
                .expect("a landing")
                .expect("no conflict");
            assert_eq!(rev(format!("{landing}^1")), main, "first parent: the base");
            assert_eq!(rev(format!("{landing}^2")), head, "second parent: the head");
            assert!(
                git(&[
                    "--git-dir",
                    &mirror,
                    "rev-parse",
                    "--verify",
                    "-q",
                    &format!("{landing}^3")
                ])
                .is_err(),
                "two parents"
            );
            let said = git(&["--git-dir", &mirror, "show", "-s", "--format=%B", &landing])
                .expect("message");
            assert_eq!(said, message.trim_end());
            assert!(
                said.starts_with("Count the gulls (objective 12, task 9)\n\ngulls\n"),
                "{said}"
            );
            assert!(
                !spawn_host::names_files_outside_git(&said),
                "the manifest stays off main"
            );
            let files =
                git(&["--git-dir", &mirror, "ls-tree", "--name-only", &landing]).expect("tree");
            if merged {
                assert_eq!(files, "README.md\none.txt\nother.txt\ntwo.txt", "the merge");
            } else {
                assert_eq!(
                    rev(format!("{landing}^{{tree}}")),
                    rev(format!("{head}^{{tree}}"))
                );
            }
            // One commit on the first-parent line; the checkpoints off it.
            let line = git(&[
                "--git-dir",
                &mirror,
                "rev-list",
                "--first-parent",
                &format!("{main}..{landing}"),
            ])
            .expect("first parents");
            assert_eq!(line, landing);
            git(&[
                "--git-dir",
                &mirror,
                "merge-base",
                "--is-ancestor",
                &checkpoint,
                &landing,
            ])
            .expect("the checkpoints stay reachable");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Both landing commits [`super::landing_candidate`] writes (a branch
    /// head with the large-file trailer, a merge with a main that moved) are
    /// by [`super::LANDING_IDENTITY`], as author and as committer. Run by
    /// [`landing_commits_ignore_the_hosts_git_configuration`] under a home
    /// without or with a hostile git configuration; alone it checks the same
    /// under the caller's.
    #[test]
    #[ignore = "run by landing_commits_ignore_the_hosts_git_configuration"]
    fn landing_commits_are_by_the_integrator() {
        let git = crate::sources::test_git;
        let base = std::env::temp_dir().join(format!(
            "spawn-land-identity-{}-{}",
            std::process::id(),
            std::env::var("SPAWN_TEST_LANDING_CASE").unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        commit_in(&seed_path, "base");
        let main = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("main");
        // A branch head on main that names a large-file manifest.
        git(&["-C", &seed_path, "checkout", "-qb", "agent_large"]).expect("branch");
        std::fs::write(seed.join("work.txt"), "work\n").expect("write");
        let trailer = format!("work\n\nSpawn-Large: {}", "a".repeat(64));
        commit_in(&seed_path, &trailer);
        let large = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("large");
        // A branch beside a main that moved on.
        git(&["-C", &seed_path, "checkout", "-qb", "agent_late", &main]).expect("branch");
        std::fs::write(seed.join("late.txt"), "late\n").expect("write");
        commit_in(&seed_path, "late");
        let late = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("late");
        git(&["-C", &seed_path, "checkout", "-q", "main"]).expect("main");
        std::fs::write(seed.join("other.txt"), "theirs\n").expect("write");
        commit_in(&seed_path, "elsewhere");
        let moved = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("moved");
        let mirror = format!("{seed_path}/.git");

        for (main, branch) in [(main.as_str(), large.as_str()), (&moved, &late)] {
            let message = "Work (task 7)\n\nThe work.\n\nSpawn-Task: 7\n";
            let landing = super::landing_candidate(&mirror, main, branch, message)
                .expect("a landing")
                .expect("no conflict");
            assert_ne!(landing, branch, "a commit of the integrator's own");
            let said = git(&["--git-dir", &mirror, "show", "-s", "--format=%B", &landing])
                .expect("message");
            assert_eq!(said, message.trim_end(), "the integrator's message");
            assert!(!spawn_host::names_files_outside_git(&said));
            let who = git(&[
                "--git-dir",
                &mirror,
                "show",
                "-s",
                "--format=%an <%ae>%n%cn <%ce>",
                &landing,
            ])
            .expect("show");
            let expected = format!(
                "{0} <{1}>",
                super::LANDING_IDENTITY.0,
                super::LANDING_IDENTITY.1
            );
            assert_eq!(who, format!("{expected}\n{expected}"));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Under the git configuration of this process: a plain `git commit`
    /// with the controller's git (the control: it must not get through
    /// when signing is forced), then Spawn's own commits, a checkpoint (the
    /// store's `commit-tree`) and a landing (the integrator's), neither of
    /// them signed. The seed is made with the test's own git, which that
    /// configuration does not reach. Run by
    /// [`spawns_own_commits_ignore_forced_signing`].
    #[test]
    #[ignore = "run by spawns_own_commits_ignore_forced_signing"]
    fn own_commits_under_forced_signing() {
        use spawn_core::port::{Commit, WorkspaceStore};
        let git = crate::sources::test_git;
        let base =
            std::env::temp_dir().join(format!("spawn-forced-signing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        commit_in(&seed_path, "base");
        let main = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("main");

        std::fs::write(seed.join("control.txt"), "control\n").expect("write");
        git(&["-C", &seed_path, "add", "-A"]).expect("add");
        let control = crate::sources::controller_git()
            .args([
                "-C",
                &seed_path,
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
            ])
            .args(["commit", "-qm", "control"])
            .output()
            .expect("git runs");
        assert!(
            !control.status.success(),
            "signing is not forced here: {control:?}"
        );

        let mirror = format!("{seed_path}/.git");
        let store = spawn_host::GitWorkspace::new(&mirror, base.join("work"));
        let branch = spawn_core::id::BranchName::new("agent_signed").expect("valid branch");
        store.ensure_branch(&branch, None).expect("branch");
        let workspace = store.mount(&branch).expect("mount");
        std::fs::write(
            std::path::Path::new(workspace.path()).join("work.txt"),
            "work\n",
        )
        .expect("write");
        let Commit::Created(checkpoint) = store.commit(&workspace, "work").expect("a checkpoint")
        else {
            panic!("a change");
        };
        store.unmount(&workspace).expect("unmount");
        let message = "Work (task 7)\n\nThe work.\n\nSpawn-Task: 7\n";
        let landing = super::landing_candidate(&mirror, &main, checkpoint.as_str(), message)
            .expect("a landing")
            .expect("no conflict");
        for commit in [checkpoint.as_str(), landing.as_str()] {
            let raw = git(&["--git-dir", &mirror, "cat-file", "commit", commit]).expect("commit");
            assert!(!raw.contains("gpgsig"), "{commit} is signed: {raw}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Spawn's own commits never sign, whatever the host's git
    /// configuration says: with signing forced through the environment
    /// (`GIT_CONFIG_COUNT`: `commit.gpgsign` and `tag.gpgsign` on,
    /// `gpg.format=ssh`, a `user.signingkey` that does not exist), a
    /// checkpoint commit and a landing commit are both made, unsigned,
    /// within a bounded wait, where the controller's `git commit` fails. A
    /// host whose `~/.gitconfig` signs with a key no agent holds made every
    /// porcelain commit wait forever on `ssh-keygen -Y sign`.
    #[test]
    fn spawns_own_commits_ignore_forced_signing() {
        use std::process::Stdio;
        use std::time::{Duration, Instant};
        let program = std::env::current_exe().expect("the test binary");
        let missing = std::env::temp_dir().join(format!(
            "spawn-forced-signing-{}-no-such-key.pub",
            std::process::id()
        ));
        let mut child = std::process::Command::new(&program)
            .args([
                "--exact",
                "integrate::tests::own_commits_under_forced_signing",
                "--ignored",
                "--nocapture",
            ])
            .env("GIT_CONFIG_COUNT", "4")
            .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
            .env("GIT_CONFIG_VALUE_0", "true")
            .env("GIT_CONFIG_KEY_1", "tag.gpgsign")
            .env("GIT_CONFIG_VALUE_1", "true")
            .env("GIT_CONFIG_KEY_2", "gpg.format")
            .env("GIT_CONFIG_VALUE_2", "ssh")
            .env("GIT_CONFIG_KEY_3", "user.signingkey")
            .env("GIT_CONFIG_VALUE_3", &missing)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run the test binary");
        let limit = Duration::from_mins(2);
        let started = Instant::now();
        while child
            .try_wait()
            .expect("wait for the test binary")
            .is_none()
        {
            if started.elapsed() >= limit {
                let _ = child.kill();
                let _ = child.wait();
                panic!("a commit under forced signing hung for {limit:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output().expect("its output");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn landing_commits_ignore_the_hosts_git_configuration() {
        let base = std::env::temp_dir().join(format!("spawn-land-homes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let empty = base.join("empty");
        std::fs::create_dir_all(&empty).expect("an empty home");
        // Another identity, no identity guessed, and every commit signed by
        // a gpg that does not exist.
        let hostile = base.join("hostile");
        std::fs::create_dir_all(&hostile).expect("a hostile home");
        std::fs::write(
            hostile.join(".gitconfig"),
            "[user]\n\tname = Someone Else\n\temail = else@example.com\n\tuseConfigOnly = true\n\
             [commit]\n\tgpgSign = true\n[gpg]\n\tprogram = /nonexistent/gpg\n",
        )
        .expect("write the hostile configuration");
        let program = std::env::current_exe().expect("the test binary");
        for (case, home) in [("empty", &empty), ("hostile", &hostile)] {
            let output = std::process::Command::new(&program)
                .args([
                    "--exact",
                    "integrate::tests::landing_commits_are_by_the_integrator",
                    "--ignored",
                    "--nocapture",
                ])
                .env("HOME", home)
                .env("XDG_CONFIG_HOME", home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("SPAWN_TEST_LANDING_CASE", case)
                .env_remove("GIT_CONFIG_GLOBAL")
                .env_remove("GIT_AUTHOR_NAME")
                .env_remove("GIT_AUTHOR_EMAIL")
                .env_remove("GIT_COMMITTER_NAME")
                .env_remove("GIT_COMMITTER_EMAIL")
                .env_remove("EMAIL")
                .output()
                .expect("run the test binary");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{case} home: {stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// Objective 20: a gate failure that is the host's. The failures
/// check-tests.sh printed, in both of its forms; whose each one is; the test
/// database's log over the gate's window; the backoff, the hold and the
/// escalation; and no halt.
#[cfg(test)]
mod host_fault_tests {
    use std::cell::Cell;
    use std::time::Duration;

    use serde_json::json;

    use super::host::{self, Class, Failure};
    use super::{
        Mission, halt_reason, host_fault_backoff, host_fault_base, host_fault_fields, host_wait,
    };

    const REFUSED: &str = "connect to the test database: error connecting to server: Connection \
                           refused (os error 111)";
    const CRASHED: &str = "called `Result::unwrap()` on an `Err` value: Error { kind: Db, cause: \
                           Some(DbError { severity: \"FATAL\", parsed_severity: Some(Fatal), \
                           code: SqlState(E57P02), message: \"terminating connection because \
                           of crash of another server process\" }) }";
    const RECOVERING: &str = "db error: FATAL: the database system is in recovery mode";
    const LOST_OID: &str = "called `Result::unwrap()` on an `Err` value: Database(\"db error: \
                            ERROR: could not open relation with OID 16421\")";
    const LOST_CACHE: &str = "db error: ERROR: cache lookup failed for relation 16388";
    const ASSERTION: &str = "assertion `left == right` failed: the landing moved main";
    const NOTE: &str = "note: run with `RUST_BACKTRACE=1` environment variable to display a \
                        backtrace";

    /// What gate.sh prints around check-tests.sh's output when it rejects
    /// the candidate for it: the stages before, the section's opening line,
    /// the check's lines (`| `-prefixed), the stage that closes it and the
    /// verdict.
    fn gate_output(lines: &[&str]) -> String {
        let mut log = String::from(
            "== candidate: build\n| build: ok\n| stage gate ok 900ms candidate: build\n\
             | == regression: check-tests.sh must still pass\n",
        );
        for line in lines {
            log.push_str("| ");
            log.push_str(line);
            log.push('\n');
        }
        log.push_str(
            "| stage gate rejected 1200ms regression: check-tests.sh must still pass\n\
             | gate time: base 0 s, candidate 1 s, setup 0 s, total 1 s\n\
             {\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the \
             candidate\"}\n",
        );
        log
    }

    /// One failing test as cargo prints it and the excerpt keeps it.
    fn panic_lines<'a>(test: &'a str, message: &[&'a str]) -> Vec<String> {
        let mut lines = vec![
            format!("---- {test} stdout ----"),
            String::new(),
            format!("thread '{test}' panicked at crates/spawn-postgres/tests/common/mod.rs:31:10:"),
        ];
        lines.extend(message.iter().map(ToString::to_string));
        lines.push(NOTE.to_string());
        lines.push("--".to_string());
        lines
    }

    fn output_of(lines: &[String]) -> String {
        gate_output(&lines.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn failure(test: &str, message: &str) -> Failure {
        Failure {
            test: test.to_string(),
            message: message.to_string(),
        }
    }

    fn tests_of(failures: &[Failure]) -> Vec<&str> {
        failures
            .iter()
            .map(|failure| failure.test.as_str())
            .collect()
    }

    /// The owner's form: the FAILED lines are the failures, all of them,
    /// even though the excerpt after them was cut after the first.
    #[test]
    fn the_failed_lines_win_over_a_cut_excerpt() {
        let mut lines = vec![
            format!("FAILED api::tests::claims_in_order: {REFUSED}"),
            format!("FAILED control::tests::the_fold_replays: {CRASHED}"),
            format!("FAILED stalls::tests::delivery_is_measured_from_its_record: {RECOVERING}"),
        ];
        lines.extend(panic_lines("api::tests::claims_in_order", &[REFUSED]));
        let failures = host::check_tests_failures(&output_of(&lines));
        assert_eq!(
            failures,
            [
                failure("api::tests::claims_in_order", REFUSED),
                failure("control::tests::the_fold_replays", CRASHED),
                failure(
                    "stalls::tests::delivery_is_measured_from_its_record",
                    RECOVERING
                ),
            ]
        );
        // check-tests.sh puts the panic's location before the message.
        let located = gate_output(&[
            "FAILED merges::tests::a_conflict: crates/spawn-cli/src/merges.rs:12:5: boom",
            "FAILED sources::tests::empty:",
        ]);
        assert_eq!(
            host::check_tests_failures(&located),
            [
                failure(
                    "merges::tests::a_conflict",
                    "crates/spawn-cli/src/merges.rs:12:5: boom"
                ),
                failure("sources::tests::empty", ""),
            ]
        );
    }

    /// check-tests.sh prints what a suite run by one test says of its own
    /// failures (the relay's Python unittest) as `  in <test>: <line>`,
    /// before the FAILED lines. Those lines are never failures of their own,
    /// whatever they say: the classification reads the FAILED lines alone.
    #[test]
    fn a_suites_own_lines_are_not_failures() {
        let relay = "the_model_relay_passes_its_self_test";
        let relay_failed =
            format!("FAILED {relay}: crates/spawn-cli/tests/model_relay.rs:19:5: ..F");
        let refused_in =
            format!("  in {relay}: ConnectionRefusedError: [Errno 111] Connection refused");
        let assertion_in = format!("  in {relay}: AssertionError: 2 != 3");
        let reason = host::CHECK_TESTS_REGRESSION;
        let no_log = || -> Vec<String> { panic!("connection errors need no log") };

        // A real failure, whose suite also printed a connection error: only
        // the FAILED line is read, and it stays a rejection.
        let work = output_of(&[refused_in.clone(), assertion_in.clone(), relay_failed]);
        assert_eq!(tests_of(&host::check_tests_failures(&work)), [relay]);
        assert_eq!(host::host_fault(reason, &work, Vec::new), None);
        // Connection errors only, beside a suite's assertion: still the
        // host's.
        let refused = output_of(&[
            assertion_in,
            refused_in,
            format!("FAILED api::tests::claims_in_order: {REFUSED}"),
            format!("FAILED control::tests::the_fold_replays: {REFUSED}"),
        ]);
        assert_eq!(
            tests_of(&host::check_tests_failures(&refused)),
            [
                "api::tests::claims_in_order",
                "control::tests::the_fold_replays"
            ]
        );
        assert!(host::host_fault(reason, &refused, no_log).is_some());
    }

    /// Today's form, one failure: the panic's test and its message, every
    /// line up to the note.
    #[test]
    fn the_excerpt_alone_names_one_failure_with_its_message() {
        let mut lines = vec![
            "test merges::tests::a_conflict ... FAILED".to_string(),
            String::new(),
            "failures:".to_string(),
        ];
        lines.extend(panic_lines(
            "merges::tests::a_conflict",
            &["assertion `left == right` failed", "  left: 1", " right: 2"],
        ));
        lines.push("test result: FAILED. 41 passed; 1 failed; 0 ignored".to_string());
        let failures = host::check_tests_failures(&output_of(&lines));
        assert_eq!(
            failures,
            [failure(
                "merges::tests::a_conflict",
                "assertion `left == right` failed\n  left: 1\n right: 2"
            )]
        );
        assert_eq!(host::classify(&failures[0].message), Class::Work);
        // Lines outside the check-tests section are never read.
        let elsewhere = format!(
            "| == regression: check-e2e.sh must still pass\n\
             | thread 'e2e' panicked at e2e.rs:1:1:\n| {REFUSED}\n{}",
            gate_output(&[])
        );
        assert!(host::check_tests_failures(&elsewhere).is_empty());
        assert!(host::check_tests_failures("{\"verdict\":\"rejected\"}\n").is_empty());
    }

    /// Today's form, several test binaries: each binary's failures, its
    /// summary and cargo's error line between them.
    #[test]
    fn the_excerpt_names_the_failures_of_several_binaries() {
        let mut lines = Vec::new();
        lines.extend(panic_lines(
            "sources::tests::lists_every_source",
            &[REFUSED],
        ));
        lines.push(
            "test result: FAILED. 41 passed; 1 failed; 0 ignored; 0 measured; finished in 3.02s"
                .to_string(),
        );
        lines.push("error: test failed, to rerun pass `-p spawn-cli --bin spawn`".to_string());
        lines.push("--".to_string());
        // Rust 1.88 on names the thread's id too.
        lines.extend([
            "---- a_gate_verdict_lands_only_on_its_revision stdout ----".to_string(),
            "thread 'a_gate_verdict_lands_only_on_its_revision' (2794460) panicked at \
             tests/control.rs:9:9:"
                .to_string(),
            CRASHED.to_string(),
            NOTE.to_string(),
        ]);
        lines.extend(panic_lines(
            "objective_tasks_keep_their_history",
            &[RECOVERING],
        ));
        lines.push("test result: FAILED. 20 passed; 2 failed; 0 ignored".to_string());
        lines.push("error: 2 targets failed:".to_string());
        let failures = host::check_tests_failures(&output_of(&lines));
        assert_eq!(
            tests_of(&failures),
            [
                "sources::tests::lists_every_source",
                "a_gate_verdict_lands_only_on_its_revision",
                "objective_tasks_keep_their_history"
            ]
        );
        assert_eq!(failures[1].message, CRASHED);
        assert!(
            failures
                .iter()
                .all(|failure| host::classify(&failure.message) == Class::Connection)
        );
    }

    /// Today's form cut at its 40 lines: a panic whose message was cut off
    /// is not seen; one cut inside its message keeps what was shown.
    #[test]
    fn a_truncated_excerpt_shows_what_it_kept() {
        let mut lines = panic_lines("api::tests::claims_in_order", &[REFUSED]);
        lines.extend([
            "---- control::tests::the_fold_replays stdout ----".to_string(),
            String::new(),
            "thread 'control::tests::the_fold_replays' panicked at tests/control.rs:9:9:"
                .to_string(),
            RECOVERING.to_string(),
            "---- stalls::tests::a_stall stdout ----".to_string(),
            String::new(),
            "thread 'stalls::tests::a_stall' panicked at tests/stalls.rs:4:4:".to_string(),
        ]);
        let failures = host::check_tests_failures(&output_of(&lines));
        assert_eq!(
            failures,
            [
                failure("api::tests::claims_in_order", REFUSED),
                failure("control::tests::the_fold_replays", RECOVERING),
            ],
            "the last panic's message was cut off: not seen"
        );
    }

    /// `cargo test` as Rust 1.95 prints it (thread ids, an `Error:` test),
    /// through check-tests.sh in both of its forms: the FAILED lines name all
    /// three; the excerpt, cut by grep, shows the two panics only.
    #[test]
    fn real_cargo_output_reads_in_both_forms() {
        let owners = [
            "FAILED tests::asserted: src/lib.rs:6:21: assertion `left == right` failed: the \
             landing moved main",
            "FAILED tests::returns: Error: \"db error: FATAL: the database system is in recovery \
             mode\"",
            "FAILED tests::refused: src/lib.rs:4:98: connect to the test database: \"error \
             connecting to server: Connection refused (os error 111)\"",
        ];
        let excerpt = [
            "tests::asserted --- FAILED",
            "tests::returns --- FAILED",
            "tests::refused --- FAILED",
            "",
            "failures:",
            "",
            "---- tests::asserted stdout ----",
            "",
            "thread 'tests::asserted' (2819485) panicked at src/lib.rs:6:21:",
            "assertion `left == right` failed: the landing moved main",
            "  left: 1",
            " right: 2",
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
            "",
            "--",
            "thread 'tests::refused' (2819487) panicked at src/lib.rs:4:98:",
            "connect to the test database: \"error connecting to server: Connection refused (os \
             error 111)\"",
            "",
            "",
            "failures:",
            "    tests::asserted",
            "--",
            "test result: FAILED. 1 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; \
             finished in 0.00s",
            "",
            "error: test failed, to rerun pass `--lib`",
        ];
        let both: Vec<&str> = owners.iter().chain(excerpt.iter()).copied().collect();
        let failures = host::check_tests_failures(&gate_output(&both));
        assert_eq!(
            tests_of(&failures),
            ["tests::asserted", "tests::returns", "tests::refused"]
        );
        let classes: Vec<Class> = failures
            .iter()
            .map(|failure| host::classify(&failure.message))
            .collect();
        assert_eq!(classes, [Class::Work, Class::Connection, Class::Connection]);
        let failures = host::check_tests_failures(&gate_output(&excerpt));
        assert_eq!(tests_of(&failures), ["tests::asserted", "tests::refused"]);
        assert_eq!(
            failures[0].message,
            "assertion `left == right` failed: the landing moved main\n  left: 1\n right: 2"
        );
        assert_eq!(host::classify(&failures[1].message), Class::Connection);
    }

    /// A test that returned an error prints `Error: ...` under its stdout
    /// header: a failure of that test, with that line as its message.
    #[test]
    fn an_error_line_is_the_failure_of_the_test_that_returned_it() {
        let lines = [
            "---- api::tests::returns_its_error stdout ----",
            "Error: Db(Error { kind: Db, cause: Some(DbError { code: SqlState(E57P01) }) })",
            "",
            "--",
            "Error: \"no header\"",
            "test result: FAILED. 9 passed; 2 failed; 0 ignored",
        ];
        let failures = host::check_tests_failures(&gate_output(&lines));
        assert_eq!(
            failures,
            [
                failure("api::tests::returns_its_error", lines[1]),
                failure("an unnamed test", lines[4]),
            ]
        );
        assert_eq!(host::classify(&failures[0].message), Class::Connection);
        assert_eq!(host::classify(&failures[1].message), Class::Work);
    }

    /// Every connection pattern makes a failure the host's, in any case,
    /// with no log needed; a database error is not one.
    #[test]
    fn every_connection_pattern_is_the_hosts() {
        assert_eq!(host::CONNECTION_ERRORS.len(), 12);
        for pattern in host::CONNECTION_ERRORS {
            for message in [
                format!("connect to the test database: {pattern} (os error 111)"),
                format!(
                    "called `Result::unwrap()`: {}",
                    pattern.to_ascii_uppercase()
                ),
            ] {
                assert_eq!(host::classify(&message), Class::Connection, "{message}");
                let line = format!("FAILED api::tests::one: {message}");
                let fault =
                    host::host_fault(host::CHECK_TESTS_REGRESSION, &gate_output(&[&line]), || {
                        panic!("connection errors need no log")
                    })
                    .unwrap_or_else(|| panic!("{pattern}: not a host fault"));
                assert_eq!(fault.evidence(), "failures");
                assert!(fault.crash.is_empty());
                assert_eq!(fault.failures_value()[0]["class"], "connection");
            }
        }
        assert_eq!(host::classify(LOST_OID), Class::Database);
        assert_eq!(host::classify(LOST_CACHE), Class::Database);
        assert_eq!(host::classify(ASSERTION), Class::Work);
        assert_eq!(host::classify("test timed out after 60s"), Class::Work);
    }

    /// A real failure among connection errors keeps the rejection, also
    /// when only its FAILED line shows it; and no failure read, or another
    /// reason, is judged as before. The test database's log is not read.
    #[test]
    fn a_real_failure_among_connection_errors_stays_rejected() {
        let asked = Cell::new(false);
        let crash = || {
            asked.set(true);
            vec![
                "2026-11-06 01:46:58.017 UTC [1] LOG:  server process (PID 9) was terminated \
                  by signal 11"
                    .to_string(),
            ]
        };
        let mut owners = vec![
            format!("FAILED api::tests::claims_in_order: {REFUSED}"),
            format!("FAILED control::tests::the_fold_replays: {CRASHED}"),
            format!("FAILED integrate::tests::a_landing_moves_main_once: {ASSERTION}"),
        ];
        owners.extend(panic_lines("api::tests::claims_in_order", &[REFUSED]));
        owners.extend(panic_lines("control::tests::the_fold_replays", &[CRASHED]));
        let reason = host::CHECK_TESTS_REGRESSION;
        assert_eq!(host::host_fault(reason, &output_of(&owners), crash), None);
        let mut today = panic_lines("api::tests::claims_in_order", &[REFUSED]);
        today.extend(panic_lines(
            "merges::tests::a_conflict",
            &[ASSERTION, "  left: 1"],
        ));
        assert_eq!(host::host_fault(reason, &output_of(&today), crash), None);
        assert!(!asked.get(), "a real failure needs no log");

        // Nothing read: a rejection, as today.
        let compile = gate_output(&["error[E0425]: cannot find value `x` in this scope"]);
        assert_eq!(host::host_fault(reason, &compile, crash), None);
        // Another reason is judged as today, whatever the output holds.
        let refused = gate_output(&[&format!("FAILED api::tests::one: {REFUSED}")]);
        assert_eq!(
            host::host_fault(
                "regression: check-e2e.sh fails on the candidate",
                &refused,
                crash
            ),
            None
        );
        assert_eq!(
            host::host_fault("scenario fails on the candidate", &refused, crash),
            None
        );
        assert!(host::host_fault(reason, &refused, crash).is_some());
    }

    fn at(text: &str) -> i64 {
        spawn_core::ingest::utc_ms(text).expect("a time")
    }

    /// The test database's log as postgres writes it: an old crash, then
    /// one at 01:46:58.017 UTC.
    fn server_log() -> String {
        [
            "2026-11-06 00:46:58.000 UTC [1] LOG:  server process (PID 311) was terminated by \
             signal 11: Segmentation fault",
            "2026-11-06 00:46:58.000 UTC [1] LOG:  terminating any other active server processes",
            "2026-11-06 01:40:00.000 UTC [77] ERROR:  relation \"x\" does not exist",
            "2026-11-06 01:46:58.017 UTC [1] LOG:  server process (PID 4242) was terminated by \
             signal 11: Segmentation fault",
            "2026-11-06 01:46:58.017 UTC [1] DETAIL:  Failed process was running: select 1",
            "2026-11-06 01:46:58.018 UTC [1] LOG:  terminating any other active server processes",
            "2026-11-06 01:46:58.020 UTC [1] LOG:  all server processes terminated; reinitializing",
            "\tcontinued statement text that was terminated by signal 9",
        ]
        .join("\n")
    }

    /// Database errors that are no connection errors are the host's only
    /// with a crash in the test database's log within the gate's window
    /// (give or take a second): one an hour before keeps the rejection.
    #[test]
    fn database_errors_are_the_hosts_only_with_a_crash_in_the_window() {
        let log = server_log();
        let crash = host::crash_lines(&log, at("2026-11-06T01:46:40Z"), at("2026-11-06T01:47:10Z"));
        assert_eq!(crash.len(), 3, "{crash:?}");
        assert!(crash[0].contains("(PID 4242) was terminated by signal 11"));
        assert!(crash[2].contains("all server processes terminated; reinitializing"));
        let output = gate_output(&[
            &format!("FAILED merges::tests::a_conflict_is_recorded: {LOST_OID}"),
            &format!("FAILED sources::tests::lists_every_source: {LOST_CACHE}"),
            &format!("FAILED api::tests::claims_in_order: {REFUSED}"),
        ]);
        let reason = host::CHECK_TESTS_REGRESSION;
        let fault = host::host_fault(reason, &output, || crash.clone()).expect("a host fault");
        assert_eq!(fault.evidence(), "server_log");
        assert_eq!(fault.crash, crash);
        let classes: Vec<serde_json::Value> = fault
            .failures_value()
            .as_array()
            .expect("failures")
            .iter()
            .map(|failure| failure["class"].clone())
            .collect();
        assert_eq!(
            classes,
            [json!("database"), json!("database"), json!("connection")]
        );

        // The gate ran an hour after the old crash, and before the new one.
        let before =
            host::crash_lines(&log, at("2026-11-06T01:30:00Z"), at("2026-11-06T01:46:57Z"));
        assert!(before.is_empty(), "{before:?}");
        assert_eq!(host::host_fault(reason, &output, || before.clone()), None);
        // Today's form, the same.
        let mut today = panic_lines("merges::tests::a_conflict_is_recorded", &[LOST_OID]);
        today.extend(panic_lines(
            "sources::tests::lists_every_source",
            &[LOST_CACHE],
        ));
        assert_eq!(host::host_fault(reason, &output_of(&today), Vec::new), None);
        assert!(host::host_fault(reason, &output_of(&today), || crash.clone()).is_some());

        // A second of slack on either side of the measured window, no more.
        let started = at("2026-11-06T01:46:59.020Z");
        assert_eq!(host::crash_lines(&log, started, started + 5000).len(), 1);
        assert!(host::crash_lines(&log, started + 1, started + 5000).is_empty());
        let ended = at("2026-11-06T01:46:57.017Z");
        assert_eq!(host::crash_lines(&log, ended - 5000, ended).len(), 1);
        assert!(host::crash_lines(&log, ended - 5000, ended - 1).is_empty());
    }

    /// The log's times: `UTC` (or `GMT`), or a numeric offset, with or
    /// without milliseconds; anything else is no time.
    #[test]
    fn the_logs_times_read_with_utc_and_with_an_offset() {
        let utc = at("2026-11-06T01:46:58.017Z");
        for line in [
            "2026-11-06 01:46:58.017 UTC [1] LOG:  database system is ready",
            "2026-11-06 01:46:58.017 GMT [1] LOG:  x",
            "2026-11-06 10:46:58.017 +09 [1] LOG:  x",
            "2026-11-06 10:46:58.017 +0900 [1] LOG:  x",
            "2026-11-06 07:16:58.017 +05:30 [1] LOG:  x",
            "2026-11-05 22:16:58.017 -0330 [1] LOG:  x",
        ] {
            assert_eq!(host::log_time_ms(line), Some(utc), "{line}");
        }
        assert_eq!(
            host::log_time_ms("2026-11-06 01:46:58 UTC [1] LOG:  x"),
            Some(utc - 17)
        );
        for line in [
            "",
            "\tcontinued statement",
            "2026-11-06 01:46:58.017 CET [1] LOG:  x",
            "2026-11-06 01:46:58.017 +9 [1] LOG:  x",
            "2026-11-06 01:46:58.017",
            "2026-13-06 01:46:58.017 UTC [1] LOG:  x",
            "LOG:  server process (PID 1) was terminated by signal 11",
        ] {
            assert_eq!(host::log_time_ms(line), None, "{line:?}");
        }
        // A crash stamped with an offset counts in the window as its instant.
        let shifted = "2026-11-06 10:46:58.017 +09 [1] LOG:  server process (PID 1) was \
                       terminated by signal 6: Aborted";
        assert_eq!(host::crash_lines(shifted, utc, utc).len(), 1);
    }

    fn with_host_faults(faults: u32, task: Option<i64>, due: Option<i64>) -> Mission {
        serde_json::from_value(json!({
            "id": 1, "gate": "/g", "accept": "gate", "gate_rejections": 2, "gate_aborts": 2,
            "gate_host_faults": faults, "gate_host_task": task, "gate_host_due_ms": due,
        }))
        .expect("a mission")
    }

    /// 5 minutes after the first host fault in a row, doubled after each,
    /// at most an hour; the first is a setting.
    #[test]
    fn host_faults_back_off_doubling_to_an_hour() {
        let base = host_fault_base(None);
        assert_eq!(base, Duration::from_mins(5));
        let minutes: Vec<u64> = (0..=7)
            .map(|faults| host_fault_backoff(base, faults).as_secs() / 60)
            .collect();
        assert_eq!(minutes, [0, 5, 10, 20, 40, 60, 60, 60]);
        assert_eq!(
            host_fault_backoff(base, u32::MAX),
            Duration::from_hours(1),
            "no overflow"
        );
        assert_eq!(host_fault_base(Some("5")), Duration::from_secs(5));
        assert_eq!(host_fault_base(Some(" 30 ")), Duration::from_secs(30));
        for bad in ["", "0", "-5", "1.5", "five"] {
            assert_eq!(host_fault_base(Some(bad)), base, "{bad:?}");
        }
        let short = Duration::from_secs(5);
        assert_eq!(host_fault_backoff(short, 1), Duration::from_secs(5));
        assert_eq!(host_fault_backoff(short, 2), Duration::from_secs(10));
    }

    /// The task waits until the recorded due time, read from the mission's
    /// fold; another task, or no run, does not wait.
    #[test]
    fn a_host_fault_holds_its_task_until_its_due_time() {
        let due = 10_000_000;
        let waiting = with_host_faults(1, Some(4), Some(due));
        assert_eq!(
            host_wait(&waiting, 4, due - 4_500),
            Some(Duration::from_millis(4_500))
        );
        assert_eq!(host_wait(&waiting, 4, due), None, "due: judged again");
        assert_eq!(host_wait(&waiting, 4, due + 1), None);
        assert_eq!(host_wait(&waiting, 5, 0), None, "another task's");
        assert_eq!(
            host_wait(&with_host_faults(0, Some(4), Some(due)), 4, 0),
            None,
            "the run ended"
        );
        assert_eq!(host_wait(&with_host_faults(2, Some(4), None), 4, 0), None);
    }

    fn connection_fault() -> host::HostFault {
        host::host_fault(
            host::CHECK_TESTS_REGRESSION,
            &gate_output(&[
                &format!("FAILED api::tests::claims_in_order: {REFUSED}"),
                &format!("FAILED control::tests::the_fold_replays: {CRASHED}"),
            ]),
            Vec::new,
        )
        .expect("a host fault")
    }

    /// The record: the reason naming the evidence, the failures, the count,
    /// the backoff, and `escalate` at the second in a row only.
    #[test]
    fn a_host_fault_escalates_only_at_the_second_in_a_row() {
        let fault = connection_fault();
        let escalations: Vec<bool> = (1..=5)
            .map(|faults| {
                host_fault_fields(&fault, faults, Duration::from_secs(5), "log")["escalate"]
                    .as_bool()
                    .expect("escalate")
            })
            .collect();
        assert_eq!(escalations, [false, true, false, false, false]);
        let fields = host_fault_fields(&fault, 1, Duration::from_mins(5), "the gate's log");
        assert_eq!(
            fields["reason"],
            "host fault: every failure check-tests.sh printed (2) is a connection error to the \
             test database; not a verdict on the work, judged again in 5 min"
        );
        assert_eq!(fields["evidence"], "failures");
        assert_eq!(fields["host_faults"], 1);
        assert_eq!(fields["backoff_ms"], 300_000);
        assert_eq!(fields["log"], "the gate's log");
        assert!(fields.get("crash").is_none() && fields.get("due_ms").is_none());
        assert_eq!(
            fields["failures"][0],
            json!({"test": "api::tests::claims_in_order", "message": REFUSED,
                   "class": "connection"})
        );

        // By the log: the crash lines named, the first in the reason.
        let crash = vec![
            "2026-11-06 01:46:58.017 UTC [1] LOG:  server process (PID 4242) was \
                          terminated by signal 11: Segmentation fault"
                .to_string(),
        ];
        let fault = host::host_fault(
            host::CHECK_TESTS_REGRESSION,
            &gate_output(&[&format!("FAILED merges::tests::a: {LOST_OID}\nwith more")]),
            || crash.clone(),
        )
        .expect("a host fault");
        let fields = host_fault_fields(&fault, 2, Duration::from_secs(10), "");
        assert_eq!(fields["evidence"], "server_log");
        assert_eq!(fields["crash"], json!(crash));
        assert_eq!(fields["escalate"], true);
        assert_eq!(fields["backoff_ms"], 10_000);
        assert!(
            fields["reason"]
                .as_str()
                .is_some_and(|reason| reason.starts_with(
                    "host fault: the test database's log shows a crash during the gate (2026-11-06 \
                 01:46:58.017 UTC [1] LOG:  server process (PID 4242) was terminated by signal \
                 11"
                ) && reason.ends_with(
                    "is a database error; not a verdict on the work, judged again in 10 s"
                )),
            "{fields}"
        );
        // A long message is bounded, its first line only.
        let long = "x".repeat(1000);
        assert_eq!(host::first_line(&format!("{long}\nsecond")).len(), 300);
    }

    /// No halt rule counts host faults, however many in a row.
    #[test]
    fn no_number_of_host_faults_halts_a_mission() {
        for faults in [1, 2, 3, 10, 1000, u32::MAX] {
            let mission = with_host_faults(faults, Some(4), Some(0));
            assert_eq!(halt_reason(&mission, 7), None, "{faults}");
        }
    }
}

/// The integrator's passes against the test database
/// (`SPAWN_TEST_DATABASE_URL`; like the spawn-postgres tests, they do not
/// skip), each in a schema of its own, dropped afterwards, with real git
/// sources and a stub gate.
#[cfg(test)]
mod database_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use serde_json::{Value, json};
    use spawn_postgres::PostgresStore;

    use super::Integrator;
    use crate::sources::test_git as git;

    struct Db {
        store: Arc<PostgresStore>,
        url: String,
        schema: String,
    }

    impl Db {
        fn new(case: &str) -> Self {
            let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
                "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
            );
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos());
            let schema = format!("spawn_test_{}_{nanos:x}_{case}", std::process::id());
            let store = PostgresStore::connect_in_schema(&url, &schema).expect("connect");
            Self {
                store: Arc::new(store),
                url,
                schema,
            }
        }

        /// An owner connection in the schema.
        fn admin(&self) -> postgres::Client {
            let mut client =
                postgres::Client::connect(&self.url, postgres::NoTls).expect("admin connection");
            client
                .batch_execute(&format!("set search_path to {}", self.schema))
                .expect("schema");
            client
        }

        /// A mission its gate alone accepts, on `source`.
        fn gated_mission(&self, source: &str, scenarios: &Path) -> i64 {
            let mission = self
                .store
                .mission_add("gated", &json!([source]), "human")
                .expect("add");
            self.store
                .mission_configure(
                    mission,
                    Some(&scenarios.to_string_lossy()),
                    "gate",
                    None,
                    None,
                )
                .expect("configure");
            mission
        }

        /// A task of `mission` for `worker`, delivered and reported.
        fn reported_task(&self, mission: i64, worker: &str) -> i64 {
            let task = self
                .store
                .task_assign(mission, "ephemeral", worker, "brief", "default")
                .expect("assign");
            self.store.task_delivered(task, 1).expect("delivered");
            assert!(self.store.task_reported(task, "done").expect("reported"));
            task
        }

        fn record(&self, record: &Value) {
            self.store.integrator_record(record).expect("record");
        }

        fn mission(&self, id: i64) -> Value {
            let board = self.store.mission_board(50).expect("board");
            board["missions"]
                .as_array()
                .expect("missions")
                .iter()
                .find(|mission| mission["id"] == id)
                .cloned()
                .expect("the mission")
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            if let Ok(mut client) = postgres::Client::connect(&self.url, postgres::NoTls) {
                let _ =
                    client.batch_execute(&format!("drop schema if exists {} cascade", self.schema));
            }
        }
    }

    /// A bare source with one commit on main, under a fresh directory, and
    /// an integrator whose workspace root is beside it: `(base, source,
    /// scenarios, integrator)`.
    fn world(tag: &str, gate: &str) -> (PathBuf, String, PathBuf, Integrator) {
        let base = std::env::temp_dir().join(format!("spawn-db-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        super::tests::commit_in(&seed_path, "base");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
        let scenarios = base.join("scenarios");
        std::fs::create_dir_all(&scenarios).expect("scenarios");
        let gate_path = base.join("gate.sh");
        std::fs::write(&gate_path, gate).expect("gate");
        let integrator = Integrator::new(
            base.join("host"),
            gate_path,
            test_gate_env(),
            room_to_spare(),
            super::DEFAULT_GATES,
        );
        (base, source, scenarios, integrator)
    }

    /// The checks before a gate with their defaults, on a host with room to
    /// spare: whatever this test host has, the stub gates run.
    fn room_to_spare() -> crate::gate::Room {
        crate::gate::Room::from_env(|_| None).measured_by(
            |_| Some(u64::MAX),
            || Some(u64::MAX),
            |_| Ok(()),
        )
    }

    /// Main moves on the source by one commit, pushed from a clone; the
    /// integrator's mirror does not follow. Main's new commit.
    fn main_moves(base: &Path, source: &str, file: &str) -> String {
        let clone = base.join(format!("clone-{file}"));
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", source, &clone_path]).expect("clone");
        std::fs::write(clone.join(file), "moved\n").expect("write");
        super::tests::commit_in(&clone_path, "main moves");
        git(&["-C", &clone_path, "push", "-q", "origin", "main"]).expect("push main");
        git(&["--git-dir", source, "rev-parse", "refs/heads/main"]).expect("main")
    }

    fn source_main(source: &str) -> String {
        git(&["--git-dir", source, "rev-parse", "refs/heads/main"]).expect("main")
    }

    /// The stub gates' environment: PATH only, for `sleep` and `cat`.
    fn test_gate_env() -> Vec<(String, String)> {
        std::env::var("PATH")
            .map(|path| vec![("PATH".to_string(), path)])
            .unwrap_or_default()
    }

    /// `agent`'s work on `source`'s main, pushed as `spawn/<agent>`: one new
    /// file. The pushed commit.
    fn push_work(base: &Path, source: &str, agent: &str, file: &str) -> String {
        let clone = base.join(format!("agent-{file}"));
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", source, &clone_path]).expect("clone");
        std::fs::write(clone.join(file), "work\n").expect("write");
        super::tests::commit_in(&clone_path, "work");
        git(&[
            "-C",
            &clone_path,
            "push",
            "-q",
            "origin",
            &format!("HEAD:refs/heads/spawn/{agent}"),
        ])
        .expect("push the agent's branch");
        git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("work")
    }

    /// A gate that notes the candidate it judges in `<marks>/started`, then
    /// holds until `<marks>/release` exists, and accepts.
    fn holding_gate(marks: &Path) -> String {
        let marks = marks.to_string_lossy();
        format!(
            "echo \"$3\" >>'{marks}/started'\n\
             while [ ! -e '{marks}/release' ]; do sleep 0.05; done\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n"
        )
    }

    /// Releases the holding gates when a test ends, a failed one too, so no
    /// stub gate outlives it.
    struct Release(PathBuf);

    impl Drop for Release {
        fn drop(&mut self) {
            let _ = std::fs::write(self.0.join("release"), "");
        }
    }

    fn started(marks: &Path) -> Vec<String> {
        std::fs::read_to_string(marks.join("started"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Wait until `count` gates started (a gate starts on its own thread).
    fn wait_started(marks: &Path, count: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let started = started(marks);
            if started.len() >= count || std::time::Instant::now() > deadline {
                return started;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn agent_of(db: &Db, mission: i64, task: i64) -> String {
        db.mission(mission)["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|row| row["id"] == task)
            .and_then(|row| row["agent_id"].as_str())
            .expect("the task's agent")
            .to_string()
    }

    /// An empty directory for a holding gate's marks.
    fn fresh_marks(name: &str) -> PathBuf {
        let marks = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&marks);
        std::fs::create_dir_all(&marks).expect("marks");
        marks
    }

    /// The integrator's mirror of `source`.
    fn mirror_of(integrator: &Integrator, source: &str) -> String {
        crate::sources::ensure_mirror(&integrator.judge.root, Some(source))
            .expect("mirror")
            .to_string_lossy()
            .to_string()
    }

    /// A landing commit's parent `n` (1: the main it lands on, 2: the agent's
    /// head), read from the first of `repos` that has the commit: a gate is
    /// given the landing commit, one new merge per landing.
    fn parent(repos: &[&str], commit: &str, n: u8) -> String {
        repos
            .iter()
            .find_map(|repo| git(&["--git-dir", repo, "rev-parse", &format!("{commit}^{n}")]).ok())
            .unwrap_or_else(|| panic!("no parent {n} of {commit} in {repos:?}"))
    }

    /// Gates run per mission, concurrently: mission A has two ready tasks,
    /// B one on a source of its own, and D one on A's source; three gates
    /// may run. The first pass starts A's first task and B's, and nothing
    /// else through any number of passes although a gate is free: A's second
    /// waits for A's first (a mission's tasks stay serial, each judged
    /// against the main the one before left), and D for A's (they share the
    /// mirror and main). Then D's runs, judged against the main A's first
    /// left: it has waited since it was reported, A's second only since A's
    /// first ended (the longest wait gets the gate). Then A's second.
    #[test]
    fn gates_run_per_mission_concurrently() {
        let marks = fresh_marks("spawn-db-gate-marks");
        let _release = Release(marks.clone());
        let (base_a, source_a, scenarios, mut integrator) =
            world("concurrent-a", &holding_gate(&marks));
        integrator.gates = 3;
        let (base_b, source_b, _, _) = world("concurrent-b", "exit 1\n");
        let db = Db::new("concurrent_gates");
        let a = db.gated_mission(&source_a, &scenarios);
        let b = db.gated_mission(&source_b, &scenarios);
        let d = db.gated_mission(&source_a, &scenarios);
        let a_one = db.reported_task(a, "a1");
        let a_two = db.reported_task(a, "a2");
        let b_task = db.reported_task(b, "b1");
        let d_task = db.reported_task(d, "d1");
        let pushed_a_one = push_work(&base_a, &source_a, &agent_of(&db, a, a_one), "a1.txt");
        let pushed_a_two = push_work(&base_a, &source_a, &agent_of(&db, a, a_two), "a2.txt");
        let pushed_b = push_work(&base_b, &source_b, &agent_of(&db, b, b_task), "b1.txt");
        let pushed_d = push_work(&base_a, &source_a, &agent_of(&db, d, d_task), "d1.txt");
        let hold = |count: usize| {
            for _ in 0..3 {
                integrator
                    .pass_on(&db.store)
                    .expect("a pass while gates run");
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
            let now = started(&marks);
            assert_eq!(now.len(), count, "nothing else starts: {now:?}");
            now
        };
        let release = || {
            std::fs::write(marks.join("release"), "").expect("release");
            integrator.settle();
            std::fs::remove_file(marks.join("release")).expect("hold again");
        };

        integrator.pass_on(&db.store).expect("the first pass");
        let (mirror_a, mirror_b) = (
            mirror_of(&integrator, &source_a),
            mirror_of(&integrator, &source_b),
        );
        let repos = [mirror_a.as_str(), mirror_b.as_str()];
        let mut first: Vec<String> = wait_started(&marks, 2)
            .iter()
            .map(|commit| parent(&repos, commit, 2))
            .collect();
        first.sort();
        let mut expected = vec![pushed_a_one.clone(), pushed_b.clone()];
        expected.sort();
        assert_eq!(first, expected, "A's first and B's, at once");
        hold(2);
        assert_eq!(db.mission(d)["tasks"][0]["state"], "reported");
        release();
        assert_eq!(
            parent(&[&source_b], &source_main(&source_b), 2),
            pushed_b,
            "B's landed"
        );
        let a_first_main = source_main(&source_a);
        assert_eq!(
            parent(&[&source_a], &a_first_main, 2),
            pushed_a_one,
            "A's first landed"
        );

        integrator.pass_on(&db.store).expect("the next pass");
        let second = wait_started(&marks, 3);
        assert_eq!(
            parent(&repos, &second[2], 2),
            pushed_d,
            "D's, the longest wait"
        );
        assert_eq!(
            parent(&repos, &second[2], 1),
            a_first_main,
            "judged against the main A's first left"
        );
        hold(3);
        release();
        assert_eq!(db.mission(d)["tasks"][0]["state"], "landed");
        let d_main = source_main(&source_a);

        integrator.pass_on(&db.store).expect("the pass after");
        let third = wait_started(&marks, 4);
        assert_eq!(parent(&repos, &third[3], 2), pushed_a_two, "A's second");
        assert_eq!(
            parent(&repos, &third[3], 1),
            d_main,
            "judged against the main D's left"
        );
        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        assert!(
            db.mission(a)["tasks"]
                .as_array()
                .expect("tasks")
                .iter()
                .all(|row| row["state"] == "landed"),
            "{}",
            db.mission(a)
        );
        for base in [base_a, base_b, marks] {
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// The gate goes to the ready task that has waited longest, across
    /// missions, not to the lowest mission id: A and B are always ready
    /// (two tasks each, reported A1, A2, B1, B2) and there is one gate. A2
    /// waits from the end of A1's judging, after B1 was reported, so the
    /// missions take turns: A1, B1, A2, B2. Neither waits more than one
    /// gate behind the other.
    #[test]
    fn two_missions_always_ready_take_turns_at_one_gate() {
        let marks = fresh_marks("spawn-db-gate-turns");
        let _release = Release(marks.clone());
        let (base_a, source_a, scenarios, mut integrator) = world("turns-a", &holding_gate(&marks));
        integrator.gates = 1;
        let (base_b, source_b, _, _) = world("turns-b", "exit 1\n");
        let db = Db::new("turns");
        let a = db.gated_mission(&source_a, &scenarios);
        let b = db.gated_mission(&source_b, &scenarios);
        let mut pushed = Vec::new();
        for (mission, base, source, worker) in [
            (a, &base_a, &source_a, "a1"),
            (a, &base_a, &source_a, "a2"),
            (b, &base_b, &source_b, "b1"),
            (b, &base_b, &source_b, "b2"),
        ] {
            let task = db.reported_task(mission, worker);
            pushed.push(push_work(
                base,
                source,
                &agent_of(&db, mission, task),
                &format!("{worker}.txt"),
            ));
            // Reported at distinct times.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        for turn in 1..=4 {
            integrator.pass_on(&db.store).expect("a pass");
            assert_eq!(
                wait_started(&marks, turn).len(),
                turn,
                "gate {turn} started"
            );
            integrator.pass_on(&db.store).expect("a pass while it runs");
            std::fs::write(marks.join("release"), "").expect("release");
            integrator.settle();
            std::fs::remove_file(marks.join("release")).expect("hold again");
        }
        let (mirror_a, mirror_b) = (
            mirror_of(&integrator, &source_a),
            mirror_of(&integrator, &source_b),
        );
        let repos = [mirror_a.as_str(), mirror_b.as_str()];
        let judged: Vec<String> = started(&marks)
            .iter()
            .map(|commit| parent(&repos, commit, 2))
            .collect();
        let order =
            |heads: &[&String]| heads.iter().map(|head| (*head).clone()).collect::<Vec<_>>();
        assert_eq!(
            judged,
            order(&[&pushed[0], &pushed[2], &pushed[1], &pushed[3]]),
            "A1, B1, A2, B2"
        );
        for base in [base_a, base_b, marks] {
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// An objective mission with a ready backlog, its resident, and up to
    /// `workers` tasks in flight.
    fn objective_mission(
        db: &Db,
        base: &Path,
        source: &str,
        scenarios: &Path,
        name: &str,
        workers: u32,
    ) -> i64 {
        let backlog = base.join(format!("backlog-{name}"));
        std::fs::create_dir_all(&backlog).expect("backlog");
        std::fs::write(
            backlog.join("001.json"),
            r#"{"id": 1, "title": "One", "prompt": "Do one."}"#,
        )
        .expect("objective");
        db.store
            .resident_propose(name, "Pursues objectives")
            .expect("propose");
        db.store.resident_approve(name).expect("approve");
        let mission = db
            .store
            .mission_add(name, &json!([source]), "human")
            .expect("add");
        db.store
            .mission_configure(
                mission,
                Some(&scenarios.to_string_lossy()),
                "gate",
                None,
                Some(
                    &json!({"backlog": backlog.to_string_lossy(), "resident": name,
                             "workers": workers}),
                ),
            )
            .expect("configure");
        mission
    }

    /// A mission with a ready task does not delay another mission's
    /// objective assignment: A (the lower id) assigns an objective and has a
    /// task to judge, whose gate holds; B, on another source, gets its
    /// objective in the same pass, and again while A's gate runs.
    #[test]
    fn a_ready_mission_does_not_delay_another_missions_assignment() {
        let marks = fresh_marks("spawn-db-gate-assign");
        let _release = Release(marks.clone());
        let (base_a, source_a, scenarios, mut integrator) =
            world("assign-a", &holding_gate(&marks));
        integrator.gates = 1;
        let (base_b, source_b, _, _) = world("assign-b", "exit 1\n");
        let db = Db::new("assign_not_delayed");
        let a = objective_mission(&db, &base_a, &source_a, &scenarios, "ra", 2);
        let ready = db.reported_task(a, "w");
        let pushed = push_work(&base_a, &source_a, &agent_of(&db, a, ready), "w.txt");
        let b = objective_mission(&db, &base_b, &source_b, &scenarios, "rb", 1);
        let tasks = |mission: i64| db.mission(mission)["tasks"].as_array().map_or(0, Vec::len);
        assert_eq!((tasks(a), tasks(b)), (1, 0));

        integrator.pass_on(&db.store).expect("one pass");
        assert_eq!(tasks(a), 2, "A assigned its objective");
        assert_eq!(tasks(b), 1, "B assigned its objective in the same pass");
        let mirror = crate::sources::ensure_mirror(&integrator.judge.root, Some(&source_a))
            .expect("mirror")
            .to_string_lossy()
            .to_string();
        let started = wait_started(&marks, 1);
        assert_eq!(parent(&[&mirror], &started[0], 2), pushed, "A's gate runs");
        integrator
            .pass_on(&db.store)
            .expect("a pass while A's gate holds");
        assert_eq!(tasks(b), 1, "B's one slot stays filled, nothing doubled");
        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        for base in [base_a, base_b, marks] {
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// A cap of one is one gate at a time across missions, as before.
    #[test]
    fn a_cap_of_one_gates_one_mission_at_a_time() {
        let marks = fresh_marks("spawn-db-gate-cap");
        let _release = Release(marks.clone());
        let (base_a, source_a, scenarios, mut integrator) =
            world("cap-one-a", &holding_gate(&marks));
        integrator.gates = 1;
        let (base_b, source_b, _, _) = world("cap-one-b", "exit 1\n");
        let db = Db::new("cap_one");
        let a = db.gated_mission(&source_a, &scenarios);
        let b = db.gated_mission(&source_b, &scenarios);
        let a_one = db.reported_task(a, "a1");
        let b_task = db.reported_task(b, "b1");
        let pushed_a_one = push_work(&base_a, &source_a, &agent_of(&db, a, a_one), "a1.txt");
        let pushed_b = push_work(&base_b, &source_b, &agent_of(&db, b, b_task), "b1.txt");

        integrator.pass_on(&db.store).expect("a pass");
        let (mirror_a, mirror_b) = (
            mirror_of(&integrator, &source_a),
            mirror_of(&integrator, &source_b),
        );
        let repos = [mirror_a.as_str(), mirror_b.as_str()];
        let heads = || -> Vec<String> {
            started(&marks)
                .iter()
                .map(|commit| parent(&repos, commit, 2))
                .collect()
        };
        integrator.pass_on(&db.store).expect("another pass");
        wait_started(&marks, 1);
        assert_eq!(heads(), vec![pushed_a_one.clone()]);
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert_eq!(heads(), vec![pushed_a_one], "B waits for the one gate");
        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        integrator.pass_on(&db.store).expect("the pass after");
        integrator.settle();
        assert_eq!(heads().last(), Some(&pushed_b));
        assert_eq!(db.mission(b)["tasks"][0]["state"], "landed");
        for base in [base_a, base_b, marks] {
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// The task as the integrator reads it.
    fn ready_task(db: &Db, task: i64) -> super::Task {
        let missions: Vec<super::Mission> =
            serde_json::from_value(db.store.gated_missions().expect("gated")).expect("missions");
        missions
            .into_iter()
            .flat_map(|mission| mission.tasks)
            .find(|row| row.id == task)
            .expect("the task")
    }

    fn verdicts(db: &Db, task: i64) -> Vec<String> {
        db.admin()
            .query(
                "select kind -> 'detail' -> 'record' ->> 'verdict' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                    and kind -> 'detail' -> 'record' ->> 'id' = $1
                  order by session_seq",
                &[&task.to_string()],
            )
            .expect("verdicts")
            .iter()
            .map(|row| row.get(0))
            .collect()
    }

    /// A judging that panics before any record of its own records an
    /// abort, so the three-aborts halt bounds a bug; one that returns an
    /// error records nothing (objective 008: an environment error is shown,
    /// not judged); one that recorded something gets no abort on top of it.
    #[test]
    fn a_judging_that_panics_before_its_verdict_is_recorded_aborted() {
        let (base, source, scenarios, integrator) = world("judging-fails", "exit 1\n");
        let db = Db::new("judging_fails");
        let main = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");

        super::judged_or_aborted(&db.store, &ready_task(&db, task), &main, mission, || {
            Err("fetch the pushed branch: no such repository".to_string())
        });
        assert!(verdicts(&db, task).is_empty(), "an error is no verdict");
        super::judged_or_aborted(&db.store, &ready_task(&db, task), &main, mission, || {
            panic!("a bug in the judging")
        });
        super::judged_or_aborted(&db.store, &ready_task(&db, task), &main, mission, || {
            panic!("another bug, {}", "formatted")
        });
        assert_eq!(verdicts(&db, task), vec!["aborted", "aborted"]);
        let reasons: Vec<String> = db
            .admin()
            .query(
                "select kind -> 'detail' -> 'record' ->> 'reason' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'verdict' = 'aborted'
                  order by session_seq",
                &[],
            )
            .expect("reasons")
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert!(reasons[0].contains("a bug in the judging"), "{reasons:?}");
        assert!(reasons[1].contains("another bug, formatted"), "{reasons:?}");

        // One that recorded its verdict, then failed: no abort on top.
        let judged = ready_task(&db, task);
        super::judged_or_aborted(&db.store, &judged, &main, mission, || {
            db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                              "verdict": "stale", "reason": "behind"}));
            db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                              "verdict": "rejected", "base": main, "note": "no"}));
            Err("then the landing failed".to_string())
        });
        assert_eq!(
            verdicts(&db, task),
            vec!["aborted", "aborted", "stale", "rejected"]
        );

        // The rejection reset the count: three aborts in a row from here
        // halt the mission.
        let other = db.reported_task(mission, "x");
        for _ in 0..3 {
            super::judged_or_aborted(&db.store, &ready_task(&db, other), &main, mission, || {
                panic!("again")
            });
        }
        assert_eq!(verdicts(&db, other), vec!["aborted"; 3]);
        integrator.pass_on(&db.store).expect("a pass");
        let halted = db.mission(mission)["halted"].clone();
        assert!(
            halted
                .as_str()
                .is_some_and(|reason| reason.contains("gate aborts in a row")),
            "halted: {halted}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Audit #17: a landing's push moved main, and the integrator stopped
    /// before it recorded `task.landed`. A lower-id mission on the same
    /// source runs first in the next pass: it must not read the moved main
    /// as an unexplained move (a halt) while the landing is unresolved. The
    /// landing then resolves (main holds its candidate), and its record
    /// moves the waiting mission's baseline with it.
    #[test]
    fn an_unrecorded_landing_holds_the_other_missions_on_its_source() {
        let (base, source, scenarios, integrator) = world("unrecorded-landing", "exit 1\n");
        let db = Db::new("unrecorded_landing");
        let b0 = source_main(&source);
        let waiting = db.gated_mission(&source, &scenarios);
        let landing = db.gated_mission(&source, &scenarios);
        let waiting_task = db.reported_task(waiting, "w");
        let landing_task = db.reported_task(landing, "l");
        db.record(
            &json!({"type": "task.gated", "id": waiting_task, "revision": 0,
                          "verdict": "aborted", "base": b0, "reason": "a gate that died"}),
        );
        let c1 = main_moves(&base, &source, "landed.txt");
        db.record(
            &json!({"type": "task.gated", "id": landing_task, "revision": 0,
                          "verdict": "accepted", "base": b0, "candidate": c1}),
        );
        // The mirror exists, as it did when the push went out.
        crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror");

        integrator.pass_on(&db.store).expect("a pass");
        let waited = db.mission(waiting);
        assert!(
            waited["halted"].is_null(),
            "the waiting mission halted: {}",
            waited["halted"]
        );
        let landed = db.mission(landing);
        assert_eq!(landed["tasks"][0]["state"], "landed");
        assert_eq!(landed["baseline"], c1.as_str());
        assert_eq!(
            db.mission(waiting)["baseline"],
            c1.as_str(),
            "the landing names the mission that waited at its from"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Audit #17, the other half: main moved past the landing's candidate
    /// (another landing went on top of it before the record), so the
    /// compare-and-swap push fails and main is not the candidate. The
    /// candidate is in main's history: it landed, and is recorded so,
    /// instead of judging landed work again.
    #[test]
    fn a_landing_already_under_main_is_recorded_landed() {
        let (base, source, scenarios, integrator) = world("landing-under-main", "exit 1\n");
        let db = Db::new("landing_under_main");
        let b0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "l");
        let c1 = main_moves(&base, &source, "landed.txt");
        let c2 = main_moves(&base, &source, "on-top.txt");
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "accepted", "base": b0, "candidate": c1}));
        crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror");

        integrator.pass_on(&db.store).expect("a pass");
        let after = db.mission(mission);
        assert_eq!(after["tasks"][0]["state"], "landed", "{after}");
        assert_eq!(after["baseline"], c1.as_str());
        assert_eq!(source_main(&source), c2, "main is left where it is");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Audit #13: the owner moved main F -> T and recorded it (the baseline
    /// is T), but the integrator's fetch fails, so its mirror still says F.
    /// That is no reading of the source's main: the mission is skipped this
    /// pass, not halted.
    #[test]
    fn a_failed_fetch_halts_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let (base, source, scenarios, integrator) = world("failed-fetch", "exit 1\n");
        let db = Db::new("failed_fetch");
        crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror at F");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let t = main_moves(&base, &source, "deploy.txt");
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "aborted", "base": t, "reason": "a gate that died"}));
        let permissions = |mode| {
            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(mode))
                .expect("permissions");
        };
        permissions(0o000);
        let pass = integrator.pass_on(&db.store);
        permissions(0o755);
        pass.expect("a pass");
        let after = db.mission(mission);
        assert!(after["halted"].is_null(), "halted: {}", after["halted"]);
        assert_eq!(after["baseline"], t.as_str());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Audit #14: only a directory on this host receives agents' branches.
    /// A gated mission on a remote source (configured before the gate was
    /// refused on one) would be gated `stale` on every pass, forever: it is
    /// halted with the reason instead.
    #[test]
    fn a_gated_mission_on_a_remote_source_is_halted() {
        let db = Db::new("remote_source");
        let source = "https://example.invalid/widget.git";
        let mission = db
            .store
            .mission_add("remote", &json!([source]), "human")
            .expect("add");
        db.admin()
            .execute(
                "select spawn_control_record($1)",
                &[&json!({"type": "mission.configured", "id": mission,
                          "gate": "/controller/rsi/scenarios", "accept": "gate"})],
            )
            .expect("a gate configured before the check");
        db.reported_task(mission, "w");
        let base = std::env::temp_dir().join(format!("spawn-db-remote-{}", std::process::id()));
        let integrator = Integrator::new(
            base.join("host"),
            base.join("gate.sh"),
            Vec::new(),
            room_to_spare(),
            super::DEFAULT_GATES,
        );
        integrator.pass_on(&db.store).expect("a pass");
        let halted = db.mission(mission)["halted"].clone();
        assert!(
            halted
                .as_str()
                .is_some_and(|reason| reason.contains("never lands")),
            "halted: {halted}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    fn gated(db: &Db, mission: i64) -> super::Mission {
        let missions: Vec<super::Mission> =
            serde_json::from_value(db.store.gated_missions().expect("gated")).expect("missions");
        missions
            .into_iter()
            .find(|found| found.id == mission)
            .expect("the mission")
    }

    /// The fleet view's gate room (`fleet_health`) shows `mission`'s run of
    /// `no_room` as the mission row folds it.
    fn assert_gate_room_shown(db: &Db, mission: i64) {
        let health = db.store.fleet_health().expect("health");
        assert!(
            health.get("attention").is_none(),
            "who needs a human is /api/attention's, not a count of judge flags"
        );
        let row = health["gate_room"]
            .as_array()
            .and_then(|missions| missions.iter().find(|row| row["mission"] == mission))
            .expect("the mission's gate room");
        let fold = gated(db, mission);
        assert_eq!(row["task"].as_i64(), fold.gate_room_task);
        assert_eq!(row["waits"].as_u64(), Some(u64::from(fold.gate_room_waits)));
        assert_eq!(row["escalated"].as_bool(), Some(fold.gate_room_escalated));
        assert_eq!(row["since_ms"].as_i64(), fold.gate_room_since_ms);
        assert_eq!(row["last_ms"].as_i64(), fold.gate_room_last_ms);
    }

    /// The dispatcher's plan over the gate-room records the store gives now,
    /// for a supervisor whose digests told it `tag`.
    fn gate_room_plan(db: &Db, tag: &str) -> crate::dispatch::Plan {
        let records = db.store.gate_room_escalations().expect("escalations");
        let items =
            crate::disk::gate_room_escalations(records.as_array().map_or(&[][..], Vec::as_slice));
        let supervisor: crate::dispatch::Facts = serde_json::from_value(json!({
            "session_id": "session_supervisor", "agent_id": "agent_supervisor", "told": [tag],
        }))
        .expect("facts");
        let work = crate::deliver::Work::default();
        crate::dispatch::decide_with(&[supervisor], &work, &items, false, 0, "spawn")
    }

    /// Task 40: the fold. `no_room` keeps the task where it was, names the
    /// free space, moves neither the abort nor the rejection count, and
    /// folds the mission's run of them; a gate that judged ends the run, and
    /// a real abort still counts. The fleet view's health carries the run,
    /// and a told escalation stays told and unresolved while the run goes on.
    #[test]
    fn no_room_counts_toward_no_halt_and_a_real_abort_still_does() {
        let (base, source, scenarios, _) = world("no-room-fold", "exit 1\n");
        let db = Db::new("no_room_fold");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let no_room = |escalate: bool| {
            json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "no_room",
                   "reason": "no room to judge now", "free_kb": 1024, "min_gb": 10,
                   "tmpdir": "/data/tmp", "log": "! gate: short", "escalate": escalate})
        };
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "aborted", "reason": "a gate that died"}));
        for _ in 0..4 {
            db.record(&no_room(false));
        }
        db.record(&no_room(true));
        let after = gated(&db, mission);
        assert_eq!(after.gate_aborts, 1, "no_room moves no abort count");
        assert_eq!(after.gate_rejections, 0);
        assert_eq!(after.gate_room_waits, 5);
        assert!(after.gate_room_escalated);
        assert!(after.gate_room_since_ms.is_some() && after.gate_room_last_ms.is_some());
        assert_eq!(
            after.tasks[0].state,
            spawn_core::mission::TaskState::Reported
        );
        assert!(after.halted.is_none());
        assert_eq!(after.gate_room_task, Some(task));
        assert_gate_room_shown(&db, mission);
        let escalations = db.store.gate_room_escalations().expect("escalations");
        assert_eq!(
            escalations.as_array().map(Vec::len),
            Some(1),
            "{escalations}"
        );
        assert_eq!(escalations[0]["mission"], mission);
        assert_eq!(escalations[0]["free_kb"], 1024);
        // Told is a record: a digest that reached the supervisor with the
        // item's tag, however long ago. Until then it stays, whatever its age.
        let tag = format!("host gate-room-{}", escalations[0]["event_id"]);
        let digest = |key: &str, succeeded: bool| {
            db.admin()
                .execute(
                    "insert into spawn_commands (principal, idempotency_key, agent_id, session_id,
                                                 action, state, succeeded, created_at_ms)
                     values ('spawn-dispatch', $1, 'agent_supervisor', 'session_supervisor',
                             jsonb_build_object('type', 'input', 'text', 'digest',
                                                'tags', jsonb_build_array($2::text)),
                             'finished', $3, 0)",
                    &[&key, &tag, &succeeded],
                )
                .expect("a digest");
        };
        digest("dispatch-digest-failed", false);
        assert_eq!(
            db.store
                .gate_room_escalations()
                .expect("escalations")
                .as_array()
                .map(Vec::len),
            Some(1),
            "a digest that failed told nothing"
        );
        digest("dispatch-digest-told", true);
        // Told once, by its record: the dispatcher's told set has its tag,
        // so it is never pending again. Its run goes on, so the plan holds
        // it as told and unresolved.
        let plan = gate_room_plan(&db, &tag);
        assert!(plan.pending.is_empty(), "told once: {:?}", plan.pending);
        let told: Vec<String> = plan
            .told
            .iter()
            .map(crate::dispatch::Escalation::tag)
            .collect();
        assert_eq!(told, [tag.as_str()], "told, and the gate still has no room");

        // A no_room that names no free space or TMPDIR is refused.
        assert!(
            db.store
                .integrator_record(&json!({"type": "task.gated", "id": task, "revision": 0,
                                           "verdict": "no_room", "reason": "?"}))
                .is_err()
        );

        // Two more real aborts make three in a row: the halt still fires.
        for _ in 0..2 {
            db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                              "verdict": "aborted", "reason": "bind dir missing"}));
        }
        let after = gated(&db, mission);
        assert_eq!(after.gate_aborts, 3);
        assert_eq!(after.gate_room_waits, 0, "a gate that ran ends the run");
        assert!(after.gate_room_since_ms.is_none() && !after.gate_room_escalated);
        let plan = gate_room_plan(&db, &tag);
        assert!(
            plan.told.is_empty() && plan.pending.is_empty(),
            "told, and its run is over: resolved"
        );
        assert_gate_room_shown(&db, mission);
        assert!(super::halt_reason(&after, 0).is_some_and(|reason| reason.contains("aborts")));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Task 40 review: a run of `no_room` is one task's. The task leaving
    /// the gate (cancelled, aborted over budget) ends it, and a later task's
    /// first `no_room` starts a new run: no inherited age, backoff or
    /// escalation.
    #[test]
    fn a_task_leaving_the_gate_ends_its_run_of_no_room() {
        let (base, source, scenarios, _) = world("no-room-leaves", "exit 1\n");
        let db = Db::new("no_room_leaves");
        let mission = db.gated_mission(&source, &scenarios);
        let no_room = |task: i64, escalate: bool| {
            json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "no_room",
                   "reason": "no room", "free_kb": 0, "min_gb": 10, "tmpdir": "/t",
                   "escalate": escalate})
        };
        let first = db.reported_task(mission, "a");
        for escalate in [false, false, true] {
            db.record(&no_room(first, escalate));
        }
        let before = gated(&db, mission);
        assert_eq!(
            (
                before.gate_room_task,
                before.gate_room_waits,
                before.gate_room_escalated
            ),
            (Some(first), 3, true)
        );
        db.store.task_cancel(first).expect("cancel");
        let after = gated(&db, mission);
        assert_eq!(
            (
                after.gate_room_task,
                after.gate_room_waits,
                after.gate_room_escalated
            ),
            (None, 0, false),
            "cancelled: the run ends"
        );
        assert!(after.gate_room_since_ms.is_none() && after.gate_room_last_ms.is_none());

        // A no_room on another task, while the run's task still waits (or
        // left without a record), starts a new run too.
        let second = db.reported_task(mission, "b");
        db.record(&no_room(second, true));
        let third = db.reported_task(mission, "c");
        db.record(&no_room(third, false));
        let after = gated(&db, mission);
        assert_eq!(
            (
                after.gate_room_task,
                after.gate_room_waits,
                after.gate_room_escalated
            ),
            (Some(third), 1, false),
            "another task's no_room starts a new run"
        );
        assert_eq!(after.gate_room_since_ms, after.gate_room_last_ms);
        assert_eq!(super::room_wait(&after, second, i64::MAX), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Task 40, end to end: a gate that exits 5 with its no-room fact is
    /// recorded `no_room` with the free space, the task stays reported, and
    /// the next pass waits out the backoff instead of running the gate.
    #[test]
    fn a_gate_without_room_is_recorded_no_room_and_retried_later() {
        let gate = "echo 'gate: 0.0 GiB free' >&2\n\
                    printf '{\"no_room\":true,\"free_kb\":2048,\"min_gb\":10,\"tmpdir\":\"/data/tmp\"}\\n' \
                    >\"$SPAWN_GATE_VERDICT\"\n\
                    echo ran >>\"$(dirname \"$0\")/runs\"\n\
                    exit 5\n";
        let (base, source, scenarios, integrator) = world("no-room-pass", gate);
        let db = Db::new("no_room_pass");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent_id = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        let clone = base.join("agent");
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", &source, &clone_path]).expect("clone");
        std::fs::write(clone.join("work.txt"), "work\n").expect("write");
        super::tests::commit_in(&clone_path, "work");
        git(&[
            "-C",
            &clone_path,
            "push",
            "-q",
            "origin",
            &format!("HEAD:refs/heads/spawn/{agent_id}"),
        ])
        .expect("push the agent's branch");

        // Judgings run on threads: settle each pass's before the next, as
        // `--once` does.
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        integrator.pass_on(&db.store).expect("a second pass");
        integrator.settle();
        let runs = std::fs::read_to_string(base.join("runs")).expect("the gate ran");
        assert_eq!(
            runs.lines().count(),
            1,
            "the second pass waits out the backoff"
        );
        let after = gated(&db, mission);
        assert_eq!(
            after.tasks[0].state,
            spawn_core::mission::TaskState::Reported
        );
        assert_eq!((after.gate_aborts, after.gate_rejections), (0, 0));
        assert_eq!(after.gate_room_waits, 1);
        let record: Value = db
            .admin()
            .query_one(
                "select kind -> 'detail' -> 'record' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                  order by session_seq desc limit 1",
                &[],
            )
            .expect("the verdict")
            .get(0);
        assert_eq!(record["id"], task);
        assert_eq!(record["verdict"], "no_room");
        assert_eq!(record["free_kb"], 2048);
        assert_eq!(record["tmpdir"], "/data/tmp");
        assert_eq!(record["escalate"], false);
        // The gate's own floor is the disk check, named as the
        // integrator's checks name theirs.
        assert_eq!(
            (&record["check"], &record["measured"], &record["threshold"]),
            (&json!("disk"), &json!(2048), &json!(10 * 1_048_576))
        );
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("0.0 GiB free in /data/tmp")),
            "{record}"
        );
        assert!(
            record["log"]
                .as_str()
                .is_some_and(|log| log.contains("! gate: 0.0 GiB free"))
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The stub gate that notes each run beside itself and accepts.
    const NOTING_GATE: &str = "echo ran >>\"$(dirname \"$0\")/runs\"\n\
                               printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";

    /// How many times the stub gate in `base` ran.
    fn gate_runs(base: &Path) -> usize {
        std::fs::read_to_string(base.join("runs")).map_or(0, |runs| runs.lines().count())
    }

    /// The latest `task.gated` record.
    fn last_gated(db: &Db) -> Value {
        db.admin()
            .query_one(
                "select kind -> 'detail' -> 'record' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                  order by session_seq desc limit 1",
                &[],
            )
            .expect("the verdict")
            .get(0)
    }

    /// One judging of a reported task with pushed work, on a host whose
    /// room `room` measures: the mission, the task, and the world's base.
    fn judged_on(tag: &str, room: crate::gate::Room) -> (Db, i64, i64, PathBuf) {
        judged_on_with(tag, room, false)
    }

    /// [`judged_on`], the mission's scenarios marked
    /// [`super::NO_TEST_DATABASE`] when `no_test_database`.
    fn judged_on_with(
        tag: &str,
        room: crate::gate::Room,
        no_test_database: bool,
    ) -> (Db, i64, i64, PathBuf) {
        let (base, source, scenarios, mut integrator) = world(tag, NOTING_GATE);
        if no_test_database {
            std::fs::write(scenarios.join(super::NO_TEST_DATABASE), "").expect("the marker");
        }
        integrator.judge.room = room;
        let db = Db::new(&tag.replace('-', "_"));
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        push_work(&base, &source, &agent_of(&db, mission, task), "work.txt");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        (db, mission, task, base)
    }

    /// A room with these integrator variables, measuring `free_kb`,
    /// `available_mb` and `probe`.
    fn measured_room(
        integrator: &[(&'static str, &'static str)],
        free_kb: u64,
        available_mb: u64,
        probe: Result<(), spawn_postgres::ProbeFailure>,
    ) -> crate::gate::Room {
        let integrator = integrator.to_vec();
        crate::gate::Room::from_env(move |name| {
            integrator
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        })
        .measured_by(
            move |_| Some(free_kb),
            move || Some(available_mb),
            move |_| probe.clone(),
        )
    }

    /// Objective 018: the host's room is measured before the gate starts.
    /// A check that falls short (disk, memory, the test database whose
    /// parallel query cannot get its shared memory) runs no gate and is
    /// recorded `no_room` with its check, measure and threshold: the task
    /// stays reported, neither count moves, and the mission's run of
    /// no-room names the task, so it is judged again after the backoff. No
    /// password reaches the record.
    #[test]
    fn a_check_short_before_the_gate_records_no_room_and_runs_no_gate() {
        const URL: &str = "postgres://gate:hunter2@%2Fvar%2Frun%2Ftest-pg:5433/spawn_test";
        let shared_memory = spawn_postgres::ProbeFailure {
            probe: "a parallel query within 10 s".to_string(),
            error: "ERROR: could not resize shared memory segment \"/PostgreSQL.2\" to 8388608 \
                    bytes: No space left on device (53100) for gate:hunter2"
                .to_string(),
        };
        let plenty = u64::MAX / 2;
        let cases = [
            (
                "room-short-disk",
                measured_room(&[], 1024, plenty, Ok(())),
                "disk",
                json!(1024),
                json!(10 * 1_048_576),
                "SPAWN_GATE_MIN_FREE_GB=10",
            ),
            (
                "room-short-memory",
                measured_room(&[], plenty, 512, Ok(())),
                "memory",
                json!(512),
                json!(2048),
                "512 MiB available",
            ),
            (
                "room-short-database",
                measured_room(
                    &[("SPAWN_TEST_DATABASE_URL", URL)],
                    plenty,
                    plenty,
                    Err(shared_memory),
                ),
                "database",
                json!(
                    "ERROR: could not resize shared memory segment \"/PostgreSQL.2\" to 8388608 \
                     bytes: No space left on device (53100) for gate:***"
                ),
                json!("a parallel query within 10 s"),
                "the test database (/var/run/test-pg:5433/spawn_test)",
            ),
        ];
        for (tag, room, check, measured, threshold, named) in cases {
            let (db, mission, task, base) = judged_on(tag, room);
            assert_eq!(gate_runs(&base), 0, "{check}: the gate never ran");
            let record = last_gated(&db);
            assert_eq!(
                (&record["id"], &record["verdict"], &record["check"]),
                (&json!(task), &json!("no_room"), &json!(check)),
                "{record}"
            );
            assert_eq!(
                (&record["measured"], &record["threshold"]),
                (&measured, &threshold),
                "{record}"
            );
            let reason = record["reason"].as_str().expect("a reason");
            assert!(
                reason.contains(named) && reason.ends_with("not a verdict, judged again later"),
                "{reason}"
            );
            assert!(
                record["log"]
                    .as_str()
                    .is_some_and(|log| log.contains("the gate did not run")),
                "{record}"
            );
            assert!(!record.to_string().contains("hunter2"), "{record}");
            if check == "disk" {
                assert_eq!(
                    (&record["free_kb"], &record["min_gb"], &record["tmpdir"]),
                    (&json!(1024), &json!(10), &json!("/tmp"))
                );
            }
            let after = gated(&db, mission);
            assert_eq!(
                after.tasks[0].state,
                spawn_core::mission::TaskState::Reported
            );
            assert_eq!((after.gate_aborts, after.gate_rejections), (0, 0));
            assert_eq!(
                (after.gate_room_task, after.gate_room_waits),
                (Some(task), 1)
            );
            assert!(
                super::room_wait(&after, task, super::now_ms()).is_some(),
                "judged again after the backoff"
            );
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// A mission whose gate needs no test database (`no-test-database`)
    /// is not held back by a test server that fails its probe: its gate
    /// never gets the URL, so the probe is not made and the gate runs.
    #[test]
    fn a_mission_without_a_test_database_is_not_held_back_by_its_probe() {
        let room = measured_room(
            &[(
                "SPAWN_TEST_DATABASE_URL",
                "postgres://t:pw@%2Frun%2Fpg/spawn_test",
            )],
            u64::MAX / 2,
            u64::MAX / 2,
            Err(spawn_postgres::ProbeFailure {
                probe: "connect within 10 s".to_string(),
                error: "connection refused".to_string(),
            }),
        );
        let (db, mission, task, base) = judged_on_with("room-no-db", room, true);
        assert_eq!(gate_runs(&base), 1, "the gate ran");
        assert_eq!(verdicts(&db, task), vec!["accepted"]);
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Every check met: the gate runs exactly as before, the test database
    /// probed by the integrator's URL; a threshold that does not parse is an
    /// abort, and runs no gate either.
    #[test]
    fn with_room_the_gate_runs_and_a_bad_threshold_is_an_abort() {
        let probed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&probed);
        let room = measured_room(
            &[(
                "SPAWN_TEST_DATABASE_URL",
                "postgres://t:pw@%2Frun%2Fpg/spawn_test",
            )],
            u64::MAX / 2,
            u64::MAX / 2,
            Ok(()),
        )
        .measured_by(
            |_| Some(u64::MAX / 2),
            || Some(u64::MAX / 2),
            move |url| {
                noted.lock().expect("probed").push(url.to_string());
                Ok(())
            },
        );
        let (db, mission, task, base) = judged_on("room-met", room);
        assert_eq!(gate_runs(&base), 1, "the gate ran once");
        assert_eq!(
            *probed.lock().expect("probed"),
            ["postgres://t:pw@%2Frun%2Fpg/spawn_test"]
        );
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
        assert_eq!(verdicts(&db, task), vec!["accepted"]);
        let _ = std::fs::remove_dir_all(&base);

        let room = measured_room(&[("SPAWN_GATE_MIN_FREE_MEMORY_MB", "lots")], 0, 0, Ok(()));
        let (db, mission, task, base) = judged_on("room-bad", room);
        assert_eq!(gate_runs(&base), 0);
        let record = last_gated(&db);
        assert_eq!(
            (&record["id"], &record["verdict"]),
            (&json!(task), &json!("aborted"))
        );
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("SPAWN_GATE_MIN_FREE_MEMORY_MB")),
            "{record}"
        );
        assert_eq!(gated(&db, mission).gate_aborts, 1, "an abort counts");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Audit #10: the integrator's verdict names the revision it judged. A
    /// task sent back once by the gate is reported again (revision 1); the
    /// stub gate accepts, and the verdict for revision 1 lands it.
    #[test]
    fn a_verdict_names_the_revision_it_judged_and_lands() {
        let gate = "printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                    echo '{\"verdict\":\"accepted\"}'\n";
        let (base, source, scenarios, integrator) = world("verdict-revision", gate);
        let db = Db::new("verdict_revision");
        let b0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent_id = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "rejected", "base": b0, "note": "again"}));
        db.store.task_delivered(task, 2).expect("delivered again");
        assert!(
            db.store
                .task_reported(task, "again")
                .expect("reported again")
        );
        // The agent's work, pushed as spawn/<agent> (no checkpoint recorded,
        // so the push is what lands).
        let clone = base.join("agent");
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", &source, &clone_path]).expect("clone");
        std::fs::write(clone.join("work.txt"), "work\n").expect("write");
        super::tests::commit_in(&clone_path, "work");
        git(&[
            "-C",
            &clone_path,
            "push",
            "-q",
            "origin",
            &format!("HEAD:refs/heads/spawn/{agent_id}"),
        ])
        .expect("push the agent's branch");
        let work = git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("work");

        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let after = db.mission(mission);
        assert_eq!(after["tasks"][0]["state"], "landed", "{after}");
        // One landing commit on top of the base, the work its second parent,
        // its message from the record (the brief and the report).
        let main = source_main(&source);
        let rev = |reference: String| {
            git(&["--git-dir", &source, "rev-parse", &reference]).expect("rev-parse")
        };
        assert_eq!(rev(format!("{main}^1")), b0);
        assert_eq!(rev(format!("{main}^2")), work);
        let message =
            git(&["--git-dir", &source, "show", "-s", "--format=%B", &main]).expect("message");
        assert!(
            message.starts_with(&format!(
                "brief (task {task})\n\nagain\n\nSpawn-Task: {task}\n"
            )),
            "{message}"
        );
        assert_eq!(after["baseline"], main.as_str());
        let revisions: Vec<Option<String>> = db
            .admin()
            .query(
                "select kind -> 'detail' -> 'record' ->> 'revision' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                  order by session_seq",
                &[],
            )
            .expect("verdicts")
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(
            revisions,
            vec![Some("0".to_string()), Some("1".to_string())]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #8: a reported task whose agent's latest recorded checkpoint
    /// the source never got (its push was lost with the process that queued
    /// it) is not judged `stale` on every pass forever: the stale check
    /// pushes that checkpoint again from this host's mirror, and the same
    /// check finds it fresh.
    #[test]
    fn a_recorded_checkpoint_whose_push_was_lost_is_pushed_again_and_fresh() {
        use spawn_core::event::{ArtifactRef, CheckpointReason, EventKind, NewEvent, Source};
        use spawn_core::id::{AgentId, BranchName, CommitId, SessionId};
        use spawn_core::port::{Commit, EventLog, WorkspaceStore};
        let db = Db::new("lost_push");
        let (base, source, scenarios, _integrator) = world("lost-push", "#!/bin/sh\nexit 0\n");
        let mission = db.gated_mission(&source, &scenarios);
        let task = ready_task(&db, db.reported_task(mission, "lost"));
        let root = base.join("host");
        let mirror = crate::sources::ensure_mirror(&root, Some(&source)).expect("mirror");
        let host = spawn_host::GitWorkspace::new(&mirror, &root);
        let branch = BranchName::new(task.agent_id.as_str()).expect("branch");
        host.ensure_branch(&branch, None).expect("branch");
        let workspace = host.mount(&branch).expect("mount");
        std::fs::write(Path::new(workspace.path()).join("work.txt"), "work\n").expect("write");
        let Commit::Created(commit) = host.commit(&workspace, "work").expect("commit") else {
            panic!("a checkpoint");
        };
        db.store
            .append(NewEvent {
                agent_id: AgentId::new(task.agent_id.as_str()).expect("agent"),
                session_id: SessionId::new(task.session_id.as_str()).expect("session"),
                execution_id: None,
                source: Source::Bridge,
                source_event_id: Some(format!("commit:{commit}")),
                kind: EventKind::ArtifactCommitted {
                    reason: CheckpointReason::BaseMerged,
                    artifact: ArtifactRef {
                        project: "missions".to_string(),
                        branch: branch.clone(),
                        commit: CommitId::new(commit.as_str()).expect("commit"),
                    },
                    files: vec!["work.txt".to_string()],
                },
            })
            .expect("recorded, never pushed");
        assert_eq!(
            super::Judge::pushed_candidate(&db.store, &mirror, &source, &task).expect("judged"),
            super::Freshness::Fresh(commit.as_str().to_string())
        );
        host.unmount(&workspace).expect("unmount");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A `host_fault` record as the integrator writes it, by its failures.
    fn host_fault_record(task: i64, revision: i32, faults: u32) -> Value {
        json!({"type": "task.gated", "id": task, "revision": revision, "verdict": "host_fault",
               "base": "b0", "scenarios": 0,
               "reason": "host fault: every failure check-tests.sh printed (1) is a connection \
                          error to the test database; not a verdict on the work, judged again \
                          in 5 min",
               "log": "| FAILED api::tests::one: Connection refused", "evidence": "failures",
               "failures": [{"test": "api::tests::one", "message": "Connection refused",
                             "class": "connection"}],
               "host_faults": faults, "backoff_ms": 300_000, "escalate": faults == 2})
    }

    /// Objective 20: a host fault between two rejections leaves the
    /// rejection run as it was (two in a row, not reset, not three), and
    /// between two aborts the abort run; the task stays where it was.
    #[test]
    fn a_host_fault_between_two_rejections_leaves_the_rejection_run() {
        let (base, source, scenarios, _) = world("host-fault-run", "exit 1\n");
        let db = Db::new("host_fault_run");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let rejected = |revision: i32| {
            json!({"type": "task.gated", "id": task, "revision": revision,
                   "verdict": "rejected", "base": "b0", "note": "fails"})
        };
        db.record(&rejected(0));
        db.store.task_delivered(task, 2).expect("delivered again");
        assert!(db.store.task_reported(task, "again").expect("reported"));
        db.record(&host_fault_record(task, 1, 1));
        let after = gated(&db, mission);
        assert_eq!(after.gate_rejections, 1, "neither grown nor reset");
        assert_eq!(after.gate_host_faults, 1);
        assert_eq!(
            (after.tasks[0].state, after.tasks[0].revisions),
            (spawn_core::mission::TaskState::Reported, 1),
            "nothing went back to the worker"
        );
        db.record(&rejected(1));
        let after = gated(&db, mission);
        assert_eq!(
            after.gate_rejections, 2,
            "the run went on from where it was"
        );
        assert_eq!(
            after.gate_host_faults, 0,
            "a real verdict ends the host faults"
        );
        assert_eq!(super::halt_reason(&after, 0), None);

        // The abort run, the same.
        db.store.task_delivered(task, 3).expect("delivered again");
        assert!(db.store.task_reported(task, "again").expect("reported"));
        let aborted = json!({"type": "task.gated", "id": task, "revision": 2,
                             "verdict": "aborted", "reason": "a gate that died"});
        db.record(&aborted);
        db.record(&host_fault_record(task, 2, 1));
        db.record(&host_fault_record(task, 2, 2));
        let after = gated(&db, mission);
        assert_eq!((after.gate_aborts, after.gate_rejections), (1, 2));
        assert_eq!(after.gate_host_faults, 2);
        db.record(&aborted);
        let after = gated(&db, mission);
        assert_eq!((after.gate_aborts, after.gate_host_faults), (2, 0));
        assert_eq!(super::halt_reason(&after, 0), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The task's latest `task.gated` record and its event's id and time.
    fn last_verdict(db: &Db) -> (Value, i64, i64) {
        let row = db
            .admin()
            .query_one(
                "select kind -> 'detail' -> 'record', id, recorded_at_ms from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                  order by session_seq desc limit 1",
                &[],
            )
            .expect("the verdict");
        (row.get(0), row.get(1), row.get(2))
    }

    /// Objective 20, end to end: a gate that rejects for check-tests.sh
    /// with connection errors only is recorded `host_fault` (by its
    /// failures), the task stays reported and is not judged again before the
    /// record's `due_ms`; the next, with database errors while the test
    /// database's log gains a crash during the gate, is a host fault by the
    /// log, the second in a row: escalated, its backoff doubled; the third
    /// run accepts and the work lands, ending the run.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_host_fault_is_held_until_due_then_judged_again() {
        let dir = std::env::temp_dir().join(format!("spawn-db-host-fault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let runs = dir.join("runs");
        let server_log = dir.join("postgres.log");
        std::fs::write(
            &server_log,
            "2026-01-01 00:00:00.000 UTC [1] LOG:  server process (PID 1) was terminated by \
             signal 9: Killed\n",
        )
        .expect("an old crash");
        let (runs_path, log_path) = (runs.display(), server_log.display());
        let gate = format!(
            "n=$(cat '{runs_path}' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >'{runs_path}'\n\
             reject() {{\n\
               echo '| stage gate rejected 5ms regression: check-tests.sh must still pass'\n\
               printf '{{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               exit 0\n\
             }}\n\
             echo '| == regression: check-tests.sh must still pass'\n\
             if [ \"$n\" -eq 1 ]; then\n\
               echo '| FAILED api::tests::claims_in_order: error connecting to server: Connection refused (os error 111)'\n\
               echo '| FAILED stalls::tests::delivery: db error: FATAL: the database system is in recovery mode'\n\
               reject\n\
             elif [ \"$n\" -eq 2 ]; then\n\
               printf '%s [1] LOG:  server process (PID 4242) was terminated by signal 11: Segmentation fault\\n' \"$(date -u '+%Y-%m-%d %H:%M:%S.%3N UTC')\" >>'{log_path}'\n\
               echo '| FAILED merges::tests::a_conflict: db error: ERROR: cache lookup failed for relation 16388'\n\
               reject\n\
             fi\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n"
        );
        let (base, source, scenarios, mut integrator) = world("host-fault-pass", &gate);
        integrator.judge.host = super::HostSettings {
            server_log: Some(server_log.clone()),
            backoff: std::time::Duration::from_secs(2),
        };
        let db = Db::new("host_fault_pass");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        push_work(&base, &source, &agent, "work.txt");
        let count = || {
            std::fs::read_to_string(&runs).map_or(0, |text| text.trim().parse::<u32>().unwrap_or(0))
        };
        let pass = || {
            integrator.pass_on(&db.store).expect("a pass");
            integrator.settle();
        };
        let wait_until = |due: i64| {
            let left = due - super::now_ms();
            if left > 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    u64::try_from(left).unwrap_or(0) + 50,
                ));
            }
        };

        pass();
        assert_eq!(count(), 1);
        let (record, event, at) = last_verdict(&db);
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["evidence"], "failures");
        assert_eq!(record["failures"].as_array().map(Vec::len), Some(2));
        assert_eq!(record["failures"][1]["test"], "stalls::tests::delivery");
        assert_eq!(record["failures"][1]["class"], "connection");
        assert_eq!(record["host_faults"], 1);
        assert_eq!(record["escalate"], false);
        assert_eq!(record["backoff_ms"], 2000);
        assert_eq!(record["due_ms"], at + 2000, "its own time plus the backoff");
        assert!(record["candidate"].is_string() && record.get("crash").is_none());
        assert!(
            record["log"]
                .as_str()
                .is_some_and(|log| log.contains("| FAILED api::tests::claims_in_order"))
        );
        let after = gated(&db, mission);
        assert_eq!(
            (
                after.gate_host_faults,
                after.gate_host_task,
                after.gate_host_due_ms
            ),
            (1, Some(task), Some(at + 2000))
        );
        assert_eq!((after.gate_rejections, after.gate_aborts), (0, 0));
        assert_eq!(
            (after.tasks[0].state, after.tasks[0].revisions),
            (spawn_core::mission::TaskState::Reported, 0)
        );
        assert!(after.halted.is_none());

        // Held until its due time.
        pass();
        assert_eq!(count(), 1, "not judged again before due_ms");
        assert_eq!(last_verdict(&db).1, event);
        wait_until(at + 2000);

        pass();
        assert_eq!(count(), 2);
        let (record, event, at) = last_verdict(&db);
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["evidence"], "server_log");
        assert_eq!(record["failures"][0]["class"], "database");
        let crash = record["crash"].as_array().expect("the crash lines");
        assert_eq!(
            crash.len(),
            1,
            "the old crash is outside the window: {record}"
        );
        assert!(
            crash[0]
                .as_str()
                .is_some_and(|line| line.contains("(PID 4242) was terminated by signal 11"))
        );
        assert_eq!(record["host_faults"], 2);
        assert_eq!(record["escalate"], true);
        assert_eq!(record["backoff_ms"], 4000);
        assert_eq!(record["due_ms"], at + 4000);
        let escalations = db.store.gate_host_fault_escalations().expect("escalations");
        assert_eq!(
            escalations.as_array().map(Vec::len),
            Some(1),
            "{escalations}"
        );
        assert_eq!(escalations[0]["event_id"], event);
        assert_eq!(escalations[0]["mission"], mission);
        assert!(
            escalations[0].get("log").is_none(),
            "the gate's log stays out"
        );
        assert_eq!(gated(&db, mission).gate_host_faults, 2);

        pass();
        assert_eq!(count(), 2, "held again, for the doubled backoff");
        wait_until(at + 4000);
        pass();
        assert_eq!(count(), 3);
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
        let after = db.mission(mission);
        assert!(after["halted"].is_null());
        let missions: Vec<super::Mission> =
            serde_json::from_value(db.store.gated_missions().expect("gated")).expect("missions");
        let after = missions
            .into_iter()
            .find(|found| found.id == mission)
            .expect("the mission");
        assert_eq!(
            (
                after.gate_host_faults,
                after.gate_host_task,
                after.gate_host_due_ms
            ),
            (0, None, None),
            "the landing ends the run"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Objective 20 beside main's `no-test-database`: a mission whose gate
    /// needs no test database has no host faults. Its check-tests.sh
    /// failures, connection errors though they are, stay a rejection.
    #[test]
    fn a_mission_without_the_test_database_has_no_host_faults() {
        let gate = "echo '| == regression: check-tests.sh must still pass'\n\
                    echo '| FAILED api::tests::one: error connecting to server: Connection refused (os error 111)'\n\
                    echo '| stage gate rejected 5ms regression: check-tests.sh must still pass'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
        let (base, source, scenarios, integrator) = world("host-fault-no-db", gate);
        std::fs::write(scenarios.join(super::NO_TEST_DATABASE), "").expect("the marker");
        let db = Db::new("host_fault_no_db");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        push_work(&base, &source, &agent, "work.txt");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["id"], task);
        assert_eq!(record["verdict"], "rejected", "{record}");
        let after = gated(&db, mission);
        assert_eq!((after.gate_rejections, after.gate_host_faults), (1, 0));
        let _ = std::fs::remove_dir_all(&base);
    }

    // --- the owner's hold (objective 034) --------------------------------

    /// A stub gate that accepts every candidate.
    const ACCEPTING_GATE: &str =
        "printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";

    /// Hold `source` for `bound` seconds as `spawn mission hold` does: the
    /// hold's event id.
    fn hold(db: &Db, source: &str, bound: i32) -> i64 {
        crate::missions::hold_source(&db.store, source, bound, Some("owner gate test"))
            .expect("the hold")["hold"]
            .as_i64()
            .expect("the hold's event id")
    }

    /// The event ids of the control records of type `kind` naming task
    /// `task` (`id`), oldest first.
    fn task_records(db: &Db, kind: &str, task: i64) -> Vec<i64> {
        db.admin()
            .query(
                "select id from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = $1
                    and kind -> 'detail' -> 'record' ->> 'id' = $2
                  order by session_seq",
                &[&kind, &task.to_string()],
            )
            .expect("records")
            .iter()
            .map(|row| row.get(0))
            .collect()
    }

    /// The control records of type `kind` (`source.%`), oldest first, as
    /// `(event id, record)`.
    fn source_records(db: &Db, kind: &str) -> Vec<(i64, Value)> {
        db.admin()
            .query(
                "select id, kind -> 'detail' -> 'record' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = $1
                  order by session_seq",
                &[&kind],
            )
            .expect("records")
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect()
    }

    fn task_state(db: &Db, mission: i64, task: i64) -> String {
        db.mission(mission)["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|row| row["id"] == task)
            .and_then(|row| row["state"].as_str())
            .expect("the task's state")
            .to_string()
    }

    /// `land()` on a held source pushes nothing: it records one `task.held`
    /// naming the hold (a second call, and the passes after, record none)
    /// and leaves the task `landing`.
    #[test]
    fn a_landing_on_a_held_source_pushes_nothing_and_waits_once_per_hold() {
        let (base, source, scenarios, integrator) = world("held-landing", "exit 1\n");
        let db = Db::new("held_landing");
        let b0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "l");
        let work = push_work(&base, &source, &agent_of(&db, mission, task), "w.txt");
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "accepted", "base": b0, "candidate": work}));
        let held = hold(&db, &source, 600);
        let mirror =
            crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror");
        let landing = ready_task(&db, task);
        for _ in 0..2 {
            assert_eq!(
                super::land(&db.store, &mirror, &source, &landing),
                Ok(super::Landing::Held(held))
            );
        }
        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(source_main(&source), b0, "nothing was pushed");
        assert_eq!(
            task_records(&db, "task.held", task).len(),
            1,
            "once per task and hold"
        );
        assert!(task_records(&db, "task.landed", task).is_empty());
        assert_eq!(task_state(&db, mission, task), "landing");
        assert!(db.mission(mission)["halted"].is_null());
        assert_eq!(db.mission(mission)["hold"], held);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A candidate already in the source's main (a push from before the hold
    /// whose record was lost) landed before the hold: recorded `task.landed`
    /// under the hold, as without one, and main is left where it is.
    #[test]
    fn a_landing_already_under_main_is_recorded_landed_under_a_hold() {
        let (base, source, scenarios, integrator) = world("held-under-main", "exit 1\n");
        let db = Db::new("held_under_main");
        let b0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "l");
        let c1 = main_moves(&base, &source, "landed.txt");
        let c2 = main_moves(&base, &source, "on-top.txt");
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "accepted", "base": b0, "candidate": c1}));
        hold(&db, &source, 600);
        crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror");

        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(task_state(&db, mission, task), "landed");
        assert!(task_records(&db, "task.held", task).is_empty());
        assert_eq!(db.mission(mission)["baseline"], c1.as_str());
        assert_eq!(source_main(&source), c2, "main is left where it is");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The hold is taken under the landing lock: while a landing holds it
    /// (its compare-and-swap push begun), the hold waits for the landing's
    /// record, then names the main that landing left.
    #[test]
    fn a_hold_taken_while_a_landing_holds_the_lock_waits_for_its_record() {
        let (base, source, scenarios, _integrator) = world("hold-waits", "exit 1\n");
        let db = Db::new("hold_waits");
        let b0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "l");
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "accepted", "base": b0, "candidate": "c"}));
        let owner = PostgresStore::connect_in_schema(&db.url, &db.schema).expect("the owner's CLI");
        let lock = db
            .store
            .landing_hold(task)
            .expect("the landing lock")
            .expect("still landing");
        let held_source = source.clone();
        let holder = std::thread::spawn(move || {
            crate::missions::hold_source(&owner, &held_source, 600, None)
        });
        std::thread::sleep(std::time::Duration::from_millis(700));
        assert!(
            !holder.is_finished(),
            "the hold waits for the landing's lock"
        );
        let c1 = main_moves(&base, &source, "landed.txt");
        db.record(&json!({"type": "task.landed", "id": task, "from": b0, "to": c1}));
        drop(lock);
        let held = holder.join().expect("the holder").expect("the hold");
        assert_eq!(held["main"], c1.as_str(), "the main the landing left");
        let landed = task_records(&db, "task.landed", task);
        assert!(
            held["hold"].as_i64() > landed.first().copied(),
            "the landing's record comes first: {held} after {landed:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A task waiting behind the hold ends nothing early: its mission's step
    /// goes on to assign its next objective, and its source is not counted
    /// as moved. Once its `task.held` names the active hold, another mission
    /// on the held source is judged as usual (its gate runs and records its
    /// verdict), and its accepted task waits behind the hold too.
    #[test]
    fn a_task_waiting_behind_a_hold_ends_nothing_early() {
        let marks = fresh_marks("spawn-db-held-step");
        let _release = Release(marks.clone());
        let (base, source, scenarios, integrator) = world("held-step", &holding_gate(&marks));
        let db = Db::new("held_step");
        let b0 = source_main(&source);
        let a = objective_mission(&db, &base, &source, &scenarios, "rh", 2);
        integrator
            .pass_on(&db.store)
            .expect("objective 1 is assigned");
        let tasks = |mission: i64| db.mission(mission)["tasks"].as_array().map_or(0, Vec::len);
        assert_eq!(tasks(a), 1);
        let first = db.mission(a)["tasks"][0]["id"].as_i64().expect("its task");
        db.store.task_delivered(first, 1).expect("delivered");
        assert!(db.store.task_reported(first, "done").expect("reported"));
        let work = push_work(&base, &source, &agent_of(&db, a, first), "a.txt");
        db.record(&json!({"type": "task.gated", "id": first, "revision": 0,
                          "verdict": "accepted", "base": b0, "candidate": work}));
        let c = db.gated_mission(&source, &scenarios);
        let ready = db.reported_task(c, "c");
        let pushed = push_work(&base, &source, &agent_of(&db, c, ready), "c.txt");
        let held = hold(&db, &source, 600);
        std::fs::write(
            base.join("backlog-rh").join("002.json"),
            r#"{"id": 2, "title": "Two", "prompt": "Do two."}"#,
        )
        .expect("objective 2");

        integrator
            .pass_on(&db.store)
            .expect("a pass under the hold");
        assert_eq!(
            tasks(a),
            2,
            "objective 2 is assigned while the source is held"
        );
        assert_eq!(task_records(&db, "task.held", first).len(), 1);
        assert!(
            started(&marks).is_empty(),
            "C waited for A's task, not held yet when read"
        );
        integrator.pass_on(&db.store).expect("the next pass");
        let started = wait_started(&marks, 1);
        let mirror = mirror_of(&integrator, &source);
        assert_eq!(
            parent(&[&mirror], &started[0], 2),
            pushed,
            "C's gate runs on the held source: A's waiting task is no landing in flight and \
             its source was not counted as moved"
        );
        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        assert_eq!(
            verdicts(&db, ready),
            ["accepted"],
            "the gate on a held source recorded"
        );
        assert_eq!(task_state(&db, c, ready), "landing");
        assert_eq!(
            task_records(&db, "task.held", ready).len(),
            1,
            "its judging's land()"
        );
        assert_eq!(task_state(&db, a, first), "landing");
        assert_eq!(
            source_main(&source),
            b0,
            "nothing landed on the held source"
        );
        for mission in [a, c] {
            assert!(db.mission(mission)["halted"].is_null());
            assert_eq!(db.mission(mission)["hold"], held);
        }
        for base in [base, marks] {
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// After the release a waiting task lands at the next pass, after the
    /// release's record. When main moved meanwhile (an owner deploy that
    /// recorded `source.moved`), its landing is `stale` and it is judged
    /// again against the new main: never discarded, never counted, no halt.
    #[test]
    fn after_the_release_a_waiting_task_lands_or_is_judged_again_on_a_moved_main() {
        let (base, source, scenarios, integrator) = world("held-release", ACCEPTING_GATE);
        let db = Db::new("held_release");
        let b0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let first = db.reported_task(mission, "one");
        push_work(&base, &source, &agent_of(&db, mission, first), "one.txt");
        hold(&db, &source, 600);
        integrator
            .pass_on(&db.store)
            .expect("judged under the hold");
        integrator.settle();
        assert_eq!(verdicts(&db, first), ["accepted"]);
        integrator
            .pass_on(&db.store)
            .expect("a pass under the hold");
        assert_eq!(task_state(&db, mission, first), "landing");
        assert_eq!(task_records(&db, "task.held", first).len(), 1);
        assert_eq!(source_main(&source), b0);
        let released = db.store.source_release(&source).expect("the release");
        assert_eq!(released["waiting"], json!([first]));
        integrator
            .pass_on(&db.store)
            .expect("the pass after the release");
        assert_eq!(task_state(&db, mission, first), "landed");
        let (release, _) = source_records(&db, "source.released")[0].clone();
        assert!(task_records(&db, "task.landed", first)[0] > release);
        let landed = source_main(&source);
        assert_ne!(landed, b0);

        // Held again; the owner moves main and records it; then the release.
        let second = db.reported_task(mission, "two");
        push_work(&base, &source, &agent_of(&db, mission, second), "two.txt");
        hold(&db, &source, 600);
        integrator
            .pass_on(&db.store)
            .expect("judged under the hold");
        integrator.settle();
        assert_eq!(task_state(&db, mission, second), "landing");
        let deployed = main_moves(&base, &source, "deploy.txt");
        assert_eq!(
            db.store
                .source_moved(&source, &landed, &deployed)
                .expect("moved"),
            1
        );
        db.store.source_release(&source).expect("the release");
        integrator
            .pass_on(&db.store)
            .expect("its landing finds main moved");
        assert_eq!(verdicts(&db, second), ["accepted", "stale"]);
        assert_eq!(
            task_state(&db, mission, second),
            "reported",
            "judged again, not discarded"
        );
        integrator
            .pass_on(&db.store)
            .expect("judged again on the new main");
        integrator.settle();
        assert_eq!(verdicts(&db, second), ["accepted", "stale", "accepted"]);
        assert_eq!(task_state(&db, mission, second), "landed");
        assert_eq!(parent(&[&source], &source_main(&source), 1), deployed);
        let after = gated(&db, mission);
        assert_eq!(after.halted, None);
        assert_eq!(
            (after.gate_rejections, after.gate_aborts),
            (0, 0),
            "nothing counted"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A hold past its bound is recorded expired at the start of a pass,
    /// before anything lands, once; the task waiting behind it is named in
    /// the record and lands after it.
    #[test]
    fn an_expired_hold_is_recorded_before_anything_lands_once() {
        let (base, source, scenarios, integrator) = world("held-expired", "exit 1\n");
        let db = Db::new("held_expired");
        let b0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "l");
        let work = push_work(&base, &source, &agent_of(&db, mission, task), "w.txt");
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "accepted", "base": b0, "candidate": work}));
        let held = hold(&db, &source, 1);
        integrator
            .pass_on(&db.store)
            .expect("a pass under the hold");
        assert_eq!(task_state(&db, mission, task), "landing");
        std::thread::sleep(std::time::Duration::from_millis(1200));
        integrator
            .pass_on(&db.store)
            .expect("the pass after its bound");
        let expired = source_records(&db, "source.hold_expired");
        assert_eq!(expired.len(), 1);
        let (event, record) = &expired[0];
        assert_eq!(record["hold"], held);
        assert_eq!(record["source"], source.as_str());
        assert_eq!(record["waiting"], json!([task]));
        assert_eq!(task_state(&db, mission, task), "landed");
        assert!(
            task_records(&db, "task.landed", task)[0] > *event,
            "the expiry first"
        );
        assert_eq!(source_main(&source), work);
        integrator.pass_on(&db.store).expect("another pass");
        assert_eq!(source_records(&db, "source.hold_expired").len(), 1, "once");
        let _ = std::fs::remove_dir_all(&base);
    }
}
