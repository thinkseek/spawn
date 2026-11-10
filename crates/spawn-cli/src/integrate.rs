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
//! 0. next, still before any mission's step, the owner's landings
//!    ([`owner`]): every pending `source.push_requested` (`spawn mission
//!    push`, a request no `source.moved` or `source.push_refused` names yet),
//!    oldest first, is applied for its source, between the integrator's own
//!    steps. The owner's hold (3) stops task landings, not the owner's own:
//!    a request on a held source is applied too. Under
//!    the landing hold (the store's `spawn_landing_hold` lock, on a
//!    connection of its own so a judging's [`land`] is never granted it
//!    back, so no task landing's push and its record are ever split by it),
//!    it reads the source's main M from the source (never
//!    the mirror, which a judging may be using), moves main from M to the
//!    commit when M is in its history, or to a merge commit (first parent M,
//!    second the commit, by main's merge rules and the landing identity, its
//!    message naming the owner's landing, the request and the commit's
//!    subject), with a compare-and-swap (`update-ref <branch> <new> <M>` in
//!    the source), and records, in the same hold, `source.moved` {source,
//!    from M, to, request, by: human}: its fold moves the baselines of the
//!    missions on the source that were at M (and gives the new main to those
//!    with none yet), so no baseline check of the pass sees a main the owner
//!    moved. A conflict, a commit the source does not have or a
//!    compare-and-swap that fails twice records `source.push_refused`
//!    {request, reason}: nothing moves, nothing halts. It never moves main
//!    under a gate of its source (design-owner-push-between-gates.md, D1):
//!    a request on a source with a judging in flight waits (said once), and
//!    the pass that reaps that judging applies it before any new gate
//!    starts, so the judging's verdict is on the main it ran against, never
//!    `stale` because of the owner. A judging of another source goes on. A
//!    judging in flight is never stopped but by an urgent request
//!    (`--urgent`, D2): a task's judging on its source is stopped (its gate
//!    killed) and records itself discarded by the request (`task.gated`
//!    `stale`, found `discarded`, the request and the owner's reason),
//!    counted nowhere, and the request is applied or judged at the pass
//!    that reaps it; an owner judging is never stopped. The integrator is
//!    never stopped for an owner's landing. A gated request (`spawn mission push
//!    --gated`) is not applied here: it is an owner judging, a judging of its
//!    source started ahead of the source's ready tasks (never in place of a
//!    running judging, within the same caps): the mission's gate judges the
//!    candidate (the owner's commit, or the owner's landing merge on M, built
//!    in the mirror) against M with the candidate's own scenarios, `protected`
//!    emptied, and no red check on the base. The regression rule holds only
//!    for what M solves: a new contract's scenarios are unsolved on M, and so
//!    is a check main is red on, for the tests main fails (the gate runs with
//!    SPAWN_GATE_RED_UNSOLVED; a test the candidate newly fails in it is a
//!    regression), so the push is not rejected or held for them, only for
//!    the checks, tests and scenarios M solves. Accepted, main moves by a compare-and-swap push
//!    under the landing hold and `source.moved` names the `verdict`;
//!    rejected: `source.push_refused` with the `verdict` and the `log`,
//!    counted by no mission; no verdict (the host's fault, an abort, no
//!    room): nothing recorded, judged again after the host-fault backoff
//!    ([`owner::Judged`]);
//! 1. a halt rule that fired stops the mission's landings until a human
//!    resumes it: three gate rejections in a row (in an objective mission,
//!    three of one objective's tasks yield that objective instead,
//!    `objectives::yields`, and only a run across objectives halts), three
//!    aborts in a row
//!    (neither a `no_room` nor a `host_fault` counts toward either, and no
//!    rule counts host faults: a halt needs a human), the
//!    scenario count below its floor, a remote source (agents' branches
//!    never reach it), or the source's main not where the last landing left
//!    it. That last one is read only from a fetch that succeeded this pass,
//!    only while no other mission's landing on the source is unresolved
//!    (its push may have moved main before its record), and only while no
//!    owner's landing on the source is pending after step 0 (one whose
//!    landing failed may have moved main before its record; it is applied
//!    or refused by a later pass). A main moved with no record at all (a
//!    push behind Spawn's back) still halts;
//! 2. a delivered task over its mission's budget keeps its work. Its worker
//!    is stopped (the Stop, and its final checkpoint on the task's branch)
//!    and the overrun is its own record, `task.over_budget` {`reason`,
//!    `tool_calls`, `elapsed_ms`, `budget`, and the work it keeps: `branch`,
//!    `commit`, `diff`} — never `task.aborted`, which keeps its other
//!    meaning (a landing gone stale after its mission was done). It counts
//!    toward no halt: `gate_aborts` and `gate_rejections` stay as they were,
//!    no run of either is broken or started, and the task leaves flight
//!    without a report or a gate. The record is written once the work has
//!    reached the source's `spawn/<agent>` (the pusher's head holds the
//!    final checkpoint, read from the source after the Stop, never
//!    assumed), so its `commit` and `diff` (`git diff --stat main
//!    <commit>`) name what the objective's next task continues; when the
//!    push failed, it says so instead. Its minutes are active time
//!    ([`active_ms`]): the gaps of its session's events, readings
//!    (`screen.observed`, `observation.recorded`) left out; a gap counts at
//!    most `SPAWN_BUDGET_GAP_SECS` (300 s), and only if it ends in the
//!    model's progress (a tool, a job, a stopped turn), not in a failed
//!    turn, a prompt or a command; a gap inside a tool or job that ended
//!    counts while the whole fleet was alive. So no outage (the fleet
//!    stopped, a frozen host, a dead engine, the model API down) can by
//!    itself put a task over budget (a subagent's job open across a model
//!    API outage adds up to its retry window, see [`active_ms`]);
//!    - the objective's next task continues it (`objectives.rs`): it is
//!      assigned once the overrun's work is on that branch, its
//!      `task.assigned` names `continues` (the overrun task) and its brief
//!      names that task, the branch, the commit and its diff stat, so its
//!      workspace starts from the work (`main_merge::fork_from_prior`) and
//!      not from main: 51 minutes of work are continued, not redone;
//!    - a second overrun of a task that continues one is escalated to the
//!      supervisor once (`dispatch::digest::over_budget_escalations`, the
//!      digest tag `host over-budget-<record>`), naming both tasks, the
//!      objective, the branch and its diff stat, the budget and the tool
//!      calls, and what can be done; the objective is then held — no task is
//!      issued for it — until the mission is configured again (a
//!      `mission.configured` record newer than the overrun's), while the
//!      mission's other objectives go on with the freed slot. A third
//!      overrun after such a configure is treated as a first;
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
//!    **The gate's own host faults.** A gate that exits
//!    [`gate::HOST_FAULT_EXIT`] with its fact ([`GateFailure::HostFault`]: a
//!    scenario's harness error on the base, before any candidate code ran,
//!    or a check that failed twice on the candidate and once on the base)
//!    is gated `host_fault` with `evidence` `gate` and the gate's `fact`
//!    ([`gate_host_fault_fields`]), under the backoff, escalation and counts
//!    below: never an abort. A scenario the host cannot judge at all (029:
//!    no systemd user manager in the sandbox) states its own fact
//!    ([`GateFailure::CannotJudge`], the same exit 6 and the key
//!    `cannot_judge`): `host_fault` with evidence [`CANNOT_JUDGE`], likewise
//!    never a rejection and never an abort, so the objective is judged
//!    outside the gate, by the owner;
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
//!    from memory, and measured by the database's clock, which stamped it,
//!    never the host's; its mission's other tasks wait behind it). A real failure
//!    among them, or none read, keeps it `rejected`; any other rejection is
//!    judged as before. The decision rests on the failures and the crash
//!    lines named in the record, never on the reason's text alone; the wait
//!    rests on the recorded `due_ms`;
//!
//!    **A base that disagrees with itself is the host's, not an abort.** The
//!    gate judges every objective scenario on the base before any candidate
//!    code runs, and an unsolved result needs two runs that agree. A first
//!    run that exits 1 and a second that exits 0 is no verdict on the
//!    candidate: no candidate code has run yet, and the base is code that
//!    already landed. It says the host was too loaded or too flaky to judge
//!    ([`GateFailure::BaseDisagrees`]: exit [`gate::BASE_DISAGREES_EXIT`]
//!    with the gate's own pair of lines at the end of its stdout, naming the
//!    scenario and both exits). The task is gated `host_fault` with the
//!    evidence [`BASE_DISAGREES`], never `aborted`: the task stays where it
//!    was, its revisions and note do not move, nothing goes back to the
//!    worker, and neither `gate_rejections` nor `gate_aborts` grows or
//!    resets. The record names, besides `base`, `candidate`, `revision`,
//!    `scenarios`, `reason` and `log`, `evidence` `base_disagrees`,
//!    `scenario`, `exits` ([1, 0]), `setup_s` (the gate's `| gate time:`
//!    line's `setup <n> s`, left out when it printed none), `host_faults`
//!    (020's run in a row, which both evidences share), `backoff_ms`,
//!    `due_ms` (the store's), `base_disagrees` (this evidence's own run in a
//!    row, folded by the store) and `alert`. A base disagreement never
//!    carries `escalate`: the one that makes the run
//!    [`BASE_DISAGREES_ALERT_AT`] carries `alert` true instead, and every
//!    other one, the later ones of the same run too, carries false. That
//!    record is the alert until objective 030 exists (named by its event
//!    id, no second record of it), and the dispatcher's digest takes it to
//!    the supervisor once ([`crate::dispatch::digest::base_disagrees_alerts`],
//!    the tag `host gate-base-disagrees-<event id>`); 030 raises its
//!    `gate_base_disagrees` notice from the same record. No halt counts a
//!    host fault, whatever its evidence, and the task waits its recorded
//!    `due_ms` like any ([`host_wait`]). Every other harness error on the
//!    base (a first run that exits 2, `1, then 2`, the pair with any other
//!    exit) stays an abort and counts. The decision rests on the gate's
//!    recorded exit status and the pair of lines named in the record, never
//!    on the reason's text, on time or on a scenario's name; the wait rests
//!    on the recorded `due_ms`;
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
//!    `database`, or `host scenario slot` for a scenario run that gave up
//!    waiting for one of the host's slots), `measured` and `threshold`
//!    (free KiB and the floor in KiB, with `free_kb`, `min_gb` and
//!    `tmpdir`; available MiB and the floor in MiB; the error and the
//!    probe, with the server named host:port/database, never the URL's
//!    password; the wait in milliseconds and the host's scenario budget),
//!    and a reason that names the three ([`short_of_room`]). A threshold of 0 disables its check; one
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
//!   run beside the others, still one per mission and per source;
//! - every gate gets its SPAWN_GATE_JOBS from the integrator, and starts
//!   only within the host's cores ([`gate::Cpu`]): the owner's
//!   SPAWN_GATE_JOBS when set, 1 on the shared test server, else the cores
//!   less SPAWN_GATE_WORKER_CORES (default a quarter, at least 1) divided
//!   among the gates that can run at once ([`gates_at_once`]: one per
//!   source, those on the shared server within `--gates`), at most 8. A
//!   gate whose jobs, with the running gates' jobs and the cores kept for
//!   the workers, would exceed the cores waits for a later pass (said
//!   once), as over the cap: no record, no count, nothing against the
//!   task. With no gate running, one always starts. (2026-11-08: each gate
//!   took half the 9 cores by gate.sh's default, two or three gates and
//!   the workers made a load of 18-20, and task 172's gate failed on its
//!   base.)
//!
//! A verdict a gate run gave binds what it judged and where (`inputs`:
//! base, candidate, controller commit, scenario-set digest, task scenario;
//! `env`: host, toolchain, runner; [`inputs`]). With
//! `SPAWN_GATE_SHADOW_RUNNER` set, an `accepted` or `rejected` one is judged
//! again on that gate runner in the background, and its end is recorded
//! beside it (`task.shadow_gated`, [`shadow`]); it never decides or delays
//! anything, and a disagreement on equal inputs is told to the supervisor
//! once.
//!
//! Every judging records its own outcome (`task.gated`, `task.landed`), as
//! before; the pass reads those records, never the thread's result. An
//! owner's landing (step 0) never touches a judging: one on another source
//! goes on and lands by the usual rules; one on the same source holds the
//! owner's request until the pass that reaps it (D1 above). A main moved
//! by anything else while a judging ran (`spawn mission moved`, a recorded
//! move behind Spawn's back) still makes its landing's compare-and-swap
//! fail: [`land`] records `stale` and a later pass judges the task again
//! against the new main (never discarded, never counted). A
//! finished thread is reaped only at the start of a pass, before the
//! missions are read, so no pass acts on a snapshot older than a judging's
//! records. A judging that panics before any record of its own is recorded
//! `aborted` ([`judged_or_aborted`]), so the three-aborts halt bounds a bug.
//! One that returns an error records nothing, as before threads: the error
//! is the operator's to read (a git that cannot run a merge is the host's,
//! not the work's; objective 008), and a later pass judges the task again.
//! TERM stops the process at once, as before: the gate under it dies
//! before any record, and the next integrator judges the task again from
//! nothing.
//!
//! **A drain finishes what the integrator started** (objective 37: a deploy
//! restart cost a running gate, 22-40 minutes each). A drain request
//! (SIGUSR1, or `spawn integrate --drain` with the same root, which finds
//! the integrator by its pid file `<root>/integrate.pid`, sends it the
//! signal and waits until that process is gone) records
//! `integrator.draining` {`gates`: the tasks whose gates run, `bound_s`}.
//! From then on no gate, owner judging, shadow judging or base warm
//! starts; the passes go
//! on otherwise (holds, step 0's owner landings, budgets, assignments, the
//! landing of a verdict that arrives). Once no judging of its runs and a
//! pass left no landing to act on (a task to land that landed, went stale
//! or failed is acted on by the next pass; one behind the owner's hold
//! waits for the hold, not for the drain), it records `integrator.drained`
//! and exits 0. `SPAWN_INTEGRATE_DRAIN_SECS` (default 7200, a whole number
//! from 1 to 86400, anything else refused at start) bounds it, counted from
//! the draining record: at the bound the gates still running are halted
//! ([`gate::Halt`]: their process groups killed, as a gate past its time
//! limit is, and their threads record nothing), `integrator.drain_expired`
//! names their tasks and the integrator exits 0; the dispatcher tells the
//! supervisor once (`dispatch::digest::drain_expiries`). So nothing the
//! integrator starts outlives it, as warm.rs requires.
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
use spawn_postgres::{ActivityEvent, PostgresStore};

use crate::config::{loop_interval, workspace_root};
use crate::gate::{self, GateFailure, RoomFailure, Shortfall};
use crate::{Flags, env_or, open_store};
use crate::{main_merge, objectives, sources};

mod host;
mod inputs;
pub(crate) mod owner;
mod shadow;
mod warm;

/// Rejections, or aborts, in a row that halt a mission.
pub(crate) const HALT_AFTER: u32 = 3;

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
/// run: the later ones are not). Only 020's evidence escalates: a base that
/// disagrees with itself raises an alert at [`BASE_DISAGREES_ALERT_AT`].
const HOST_FAULT_ESCALATE_AT: u32 = 2;

/// The base disagreement in a row that raises the mission's alert (once per
/// run, carried by that record): the owner's ruling of 2026-11-06, where
/// 020's evidence escalates at two.
const BASE_DISAGREES_ALERT_AT: u32 = 3;

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

/// The most one silence adds to a task's active time, unless
/// `SPAWN_BUDGET_GAP_SECS` says otherwise ([`active_ms`]).
const BUDGET_GAP: Duration = Duration::from_mins(5);

/// [`BUDGET_GAP`] from the setting's text: a whole number of seconds from 1
/// up; anything else is the default.
pub(crate) fn budget_gap(text: Option<&str>) -> Duration {
    text.and_then(|text| text.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or(BUDGET_GAP, Duration::from_secs)
}

/// The events that are the model's progress: a gap ending in one is time a
/// worker worked ([`active_ms`]). Not `message.received`: that is a prompt
/// arriving (ingest's user turn: a delivery, a nudge, a job's
/// notification), and the gap before it is the worker waiting for it.
const PROGRESS: [&str; 6] = [
    "tool.started",
    "tool.completed",
    "tool.failed",
    "job.started",
    "job.ended",
    "turn.stopped",
];

/// Milliseconds a task delivered at `delivered_at_ms` could work by
/// `now_ms`: what its minute budget measures. `events` are its session's
/// events since delivery, in log order, without the readings (the Bridge's
/// `screen.observed`, the observer's `observation.recorded`: a busy pane
/// is re-read every 30 s, also while Claude Code only retries a model API
/// that is down, so a reading says the pane is busy, not that work moves).
///
/// The points are the delivery, each event and now. A gap that ends in the
/// model's progress (a tool call starting or ending, a background job
/// starting or ending, a turn stopping: [`PROGRESS`]) counts, at most
/// `gap_ms`: a worker at work makes progress far more often. Any other gap
/// counts nothing: one that ends in a failed turn (the model API did not
/// answer), in a prompt arriving or a command (a delivery, a nudge, a
/// resume: the worker waited), or in the engine's own events (a start, an
/// unmount). The gap
/// from the last event to now counts at most `gap_ms`. A long tool still
/// counts: a gap inside a tool call or background job that later ended (its
/// `tool.completed`, `tool.failed` or `job.ended` with the same id) gets
/// the time the whole fleet was alive in it (`fleet_alive(from_id, to_id)`:
/// the log's gaps between those two events, all events, readings included,
/// each capped at `gap_ms`), at least the cap and at most the gap.
///
/// Why no outage can by itself put a task over budget. The fleet stopped,
/// the host frozen or rebooted, a dead engine: the session is silent, and
/// the gap ends in the restart's command, which counts nothing, or is the
/// gap to now, at most `gap_ms`; a tool it interrupted never ends, so its
/// gap is not inside one, and a frozen host writes nothing anywhere, so the
/// fleet's own gap is capped. A model API outage: Claude Code retries behind
/// a busy pane, whose readings are no points; each retried turn ends in
/// `turn.failed` and each nudge in a command, which count nothing; only the
/// model's first progress after it counts the gap before it, at most
/// `gap_ms`. So an outage of any length or cause adds at most `gap_ms`,
/// with one bound that is not the cap: a subagent (an agent job,
/// `job.started` kind `agent`) open across a model API outage retries in
/// sidechain lines the log does not carry, and once it fails its
/// `job.ended` pairs with its start, so its gap counts the fleet's alive
/// time, which a model API outage does not interrupt. That adds up to
/// Claude Code's retry window for the subagent, not the outage's length:
/// a failed subagent ends its job (rev-e, session 145 on 2026-11-07).
/// The safe side: a model that thinks or streams for longer than `gap_ms`
/// without a tool call counts `gap_ms`, and a hung tool that never ends
/// counts `gap_ms` (the stall rules own a hung session).
pub(crate) fn active_ms(
    delivered_at_ms: i64,
    now_ms: i64,
    events: &[ActivityEvent],
    gap_ms: i64,
    mut fleet_alive: impl FnMut(i64, i64) -> Result<i64, String>,
) -> Result<i64, String> {
    let capped = |from: i64, to: i64| (to - from).clamp(0, gap_ms);
    let (Some(first), Some(last)) = (events.first(), events.last()) else {
        return Ok(capped(delivered_at_ms, now_ms));
    };
    // covered[k] > 0: the gap after events[k] lies inside a tool or job
    // that ended.
    let mut covered = vec![0i64; events.len()];
    let mut open: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (index, event) in events.iter().enumerate() {
        let Some(key) = event.key.as_deref() else {
            continue;
        };
        match event.name.as_str() {
            "tool.started" | "job.started" => {
                open.insert(key, index);
            }
            "tool.completed" | "tool.failed" | "job.ended" => {
                if let Some(start) = open.remove(key) {
                    covered[start] += 1;
                    covered[index] -= 1;
                }
            }
            _ => {}
        }
    }
    let progress = |event: &ActivityEvent| PROGRESS.contains(&event.name.as_str());
    let first_gap = if progress(first) {
        capped(delivered_at_ms, first.at_ms)
    } else {
        0
    };
    let mut total = first_gap + capped(last.at_ms, now_ms);
    let mut inside = 0;
    for (index, pair) in events.windows(2).enumerate() {
        inside += covered[index];
        let gap = (pair[1].at_ms - pair[0].at_ms).max(0);
        total += if inside > 0 && gap > gap_ms {
            fleet_alive(pair[0].id, pair[1].id)?.clamp(gap_ms, gap)
        } else if inside > 0 || progress(&pair[1]) {
            gap.min(gap_ms)
        } else {
            0
        };
    }
    Ok(total)
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

const USAGE: &str = "usage: spawn integrate [--once | --drain] [--root <dir>] [--gates <n>]";

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
    /// The objective every rejection of the current `gate_rejections` run
    /// was a task of, folded by the store; none when there is no run or the
    /// run spans objectives (or a task without one). A run of one
    /// objective's rejections yields that objective
    /// ([`crate::objectives::yields`]) and does not halt the mission.
    #[serde(default)]
    pub(crate) gate_rejection_objective: Option<u32>,
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
    /// Main of the mission's source is red on a check ([`Red`], the store's
    /// `mission_source_reds` row): none while it is not.
    #[serde(default)]
    pub(crate) red: Option<Red>,
    /// The database's clock when the store read the mission
    /// (`spawn_now_ms()`, in the same query): the gate's due times are the
    /// store's (`gate_room_last_ms` is its record's time, `gate_host_due_ms`
    /// that plus the backoff), so a wait is measured against it, never the
    /// host's clock ([`gate_wait`]). Absent only in a mission built by hand.
    #[serde(default)]
    pub(crate) now_ms: Option<i64>,
    /// The mission's current run of `host_fault` gate outcomes, folded by
    /// the store: how many in a row, the task the latest was about, and the
    /// latest record's `due_ms` (its own time plus its backoff).
    #[serde(default)]
    pub(crate) gate_host_faults: u32,
    #[serde(default)]
    pub(crate) gate_host_task: Option<i64>,
    #[serde(default)]
    pub(crate) gate_host_due_ms: Option<i64>,
    /// The mission's current run of `host_fault` gate outcomes whose
    /// evidence is a base that disagreed with itself (objective 033), folded
    /// by the store: the record that makes it [`BASE_DISAGREES_ALERT_AT`]
    /// carries `alert`, and every other one does not.
    #[serde(default)]
    pub(crate) gate_base_disagrees: u32,
    #[serde(default)]
    pub(crate) objectives: Option<Value>,
    /// When the mission was last configured (`mission.configured`), on the
    /// database's clock: an overrun chain's hold ends with a configure newer
    /// than its escalated record ([`crate::objectives`]).
    #[serde(default)]
    pub(crate) configured_ms: Option<i64>,
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

/// Main of a source red on a check (docs/fleet-architecture.md §10.3, "Main
/// is red"): the store's fold of its `source.main_red` records less the
/// `source.main_green` that ends it. `main` is the main last found red,
/// `scenario` the check, `since_ms` the first red record's time, `latest_ms`
/// the latest's and `probed_ms` the latest probe gate's (the store's clock
/// all three).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Red {
    pub(crate) main: String,
    pub(crate) scenario: String,
    pub(crate) since_ms: i64,
    pub(crate) latest_ms: i64,
    #[serde(default)]
    pub(crate) probed_ms: Option<i64>,
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
    /// The overrun task whose work this one continues (its `task.assigned`
    /// names it), if any: a second overrun of such a task goes to the
    /// supervisor and holds its objective.
    #[serde(default)]
    pub(crate) continues: Option<i64>,
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
    // Three rejections of one objective's tasks yield that objective
    // (`objectives::yields`); only a run spanning objectives halts, or one
    // in a mission without objectives to yield.
    let yields = mission.gate_rejection_objective.is_some() && mission.objectives.is_some();
    if mission.gate_rejections >= HALT_AFTER && !yields {
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

/// How long `task`'s gate still waits for room at `now_ms` (the database's
/// clock, [`Mission::now_ms`]: the record's time is the store's), read from the
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

/// How long `task`'s gate still waits after a host fault at `now_ms` (the
/// database's clock, [`Mission::now_ms`]), read from the store's record of
/// the latest one (its `due_ms`, folded into the mission), never from
/// memory. None when the gate may run: no run of host
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

/// The record of a gate whose base disagreed with itself (objective 033):
/// the scenario the gate named, both of its exits, the gate's setup seconds
/// when it printed its timing, the `faults`-th host fault in a row on the
/// mission (020's run, which this evidence shares) and the
/// `disagreements`-th base disagreement in a row, with its `backoff`.
///
/// A base that disagrees with itself never escalates (`escalate` false, for
/// every one of the run): the record that makes the run
/// [`BASE_DISAGREES_ALERT_AT`] carries `alert` true, and every other one
/// false. That record is the alert until objective 030 exists, and the
/// dispatcher's digest takes it to the supervisor once
/// ([`crate::dispatch::digest::base_disagrees_alerts`]).
pub(crate) fn base_disagrees_fields(
    scenario: &str,
    exits: [u8; 2],
    setup_s: Option<u64>,
    faults: u32,
    disagreements: u32,
    backoff: Duration,
    log: &str,
) -> Value {
    let setup = setup_s.map_or_else(String::new, |seconds| {
        format!(", after a setup of {seconds} s")
    });
    let mut fields = json!({
        "reason": format!(
            "host fault: the base disagrees with itself: {scenario} exited {}, then {} on the base\
             {setup}; not a verdict, judged again later",
            exits[0], exits[1]
        ),
        "log": log,
        "evidence": BASE_DISAGREES,
        "scenario": scenario,
        "exits": exits,
        "host_faults": faults,
        "backoff_ms": u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
        "escalate": false,
        "base_disagrees": disagreements,
        "alert": disagreements == BASE_DISAGREES_ALERT_AT,
    });
    // The setup's seconds only when the gate printed its timing line: the
    // field is left out, not null.
    if let Some(seconds) = setup_s {
        fields["setup_s"] = json!(seconds);
    }
    fields
}

/// The evidence a `host_fault` record names when the base disagreed with
/// itself (its own scenario failing and then passing, before any candidate
/// code ran): objective 020's verdict with this objective's evidence.
pub(crate) const BASE_DISAGREES: &str = "base_disagrees";

/// The record of a host fault the gate reported itself
/// ([`GateFailure::HostFault`], exit [`gate::HOST_FAULT_EXIT`] with its
/// fact), the `faults`-th host fault in a row on the mission, with its
/// `backoff`: `evidence` `gate` and the gate's `fact` (`scenario`, `on`:
/// `base` or `candidate and base`, `runs`), and the rest as
/// [`host_fault_fields`] writes it. Neither `gate_aborts` nor
/// `gate_rejections` moves.
pub(crate) fn gate_host_fault_fields(
    failure: &GateFailure,
    faults: u32,
    backoff: Duration,
) -> Value {
    let GateFailure::HostFault {
        scenario,
        on,
        runs,
        reason,
        tree,
        log,
    } = failure
    else {
        return aborted(failure);
    };
    let place = if on == "base" {
        format!("{scenario} exited {runs} on the base, before any candidate code ran")
    } else if on == "sandbox" {
        format!("the gate's sandbox never started {scenario} ({reason})")
    } else if on == "test server" {
        format!("the gate's own test server never started ({reason})")
    } else {
        format!("{scenario} failed on the candidate and on the base ({runs})")
    };
    // A sandbox that never started has its own evidence, so a record says
    // which fault it was without reading its text.
    let evidence = if on == "sandbox" {
        gate::SANDBOX_NEVER_STARTED
    } else if on == "test server" {
        gate::TEST_SERVER_NEVER_STARTED
    } else {
        "gate"
    };
    json!({
        "reason": format!(
            "host fault: {place}; not a verdict on the work, judged again in {}",
            spoken(backoff)
        ),
        "log": log,
        "evidence": evidence,
        "fact": if let Some(tree) = tree {
            json!({"scenario": scenario, "on": on, "runs": runs, "reason": reason,
                   "tree": tree.as_str()})
        } else {
            json!({"scenario": scenario, "on": on, "runs": runs, "reason": reason})
        },
        "host_faults": faults,
        "backoff_ms": u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
        "escalate": faults == HOST_FAULT_ESCALATE_AT,
    })
}

/// The record of a gate that cannot judge `scenario` here, the gate's own
/// `cannot_judge` fact (exit [`gate::HOST_FAULT_EXIT`], the key that tells
/// it from the gate's `host_fault` fact): the `faults`-th host fault in a row
/// on the mission (020's run, which every evidence shares), with its
/// `backoff`, never that the work failed.
///
/// Its cause is the scenario's own, quoted from the gate's log
/// ([`cannot_judge_cause`]) into `cause` and the reason: 029 says it needs a
/// systemd user manager, Supply PA's 073 that it needs a PostgreSQL server,
/// and a record that named one cause for all of them sent a supervisor to
/// build a task on a cause nobody had seen (task 208). With no such line in
/// the log, the record says the gate gave no cause, and names none.
pub(crate) fn cannot_judge_fields(
    scenario: &str,
    faults: u32,
    backoff: Duration,
    log: &str,
) -> Value {
    let cause = cannot_judge_cause(scenario, log);
    let said = cause.as_deref().map_or_else(
        || "the gate's log gives no cause".to_string(),
        |cause| format!("it says: {cause}"),
    );
    json!({
        "reason": format!(
            "host fault: this host cannot judge {scenario} ({said}); not a verdict on the work, \
             judged again in {}",
            spoken(backoff)
        ),
        "cause": cause,
        "log": log,
        "evidence": CANNOT_JUDGE,
        "scenario": scenario,
        "host_faults": faults,
        "backoff_ms": u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
        "escalate": faults == HOST_FAULT_ESCALATE_AT,
    })
}

/// What `scenario` said when it could not be judged, from the gate's `log`:
/// the last line of its own block (from a `| == ` line naming it to the next
/// `| == ` line) that says it cannot judge ("073: cannot judge: …", 029's
/// "029: cannot be judged here: …"), on one line, without the log's
/// markers (the gate's `| `, the integrator's `! ` before the gate's stderr).
/// Outside any block (a line on the gate's stderr, after its blocks), the
/// last line that begins with the scenario's own name and says the same.
/// None when the log has neither: the scenario gave no cause.
pub(crate) fn cannot_judge_cause(scenario: &str, log: &str) -> Option<String> {
    let says = |text: &str| text.contains("cannot judge") || text.contains("cannot be judged");
    let stem = scenario.split('.').next().unwrap_or(scenario);
    let mut in_block = false;
    let mut in_own_block = None;
    let mut by_name = None;
    for line in log.lines() {
        if let Some(header) = line.strip_prefix("| == ") {
            in_block = header.split([' ', ':']).any(|word| word == scenario);
            continue;
        }
        // The gate's `| ` before a scenario's output, the integrator's `! `
        // before a line of the gate's stderr: markers of the log, not the
        // scenario's words.
        let text = line
            .strip_prefix('|')
            .or_else(|| line.strip_prefix('!'))
            .unwrap_or(line)
            .trim();
        // The gate's own stage lines ("stage gate cannot judge 120ms base:
        // 073.sh", an uncached base run that exited 6) name the outcome,
        // not a cause: never quoted as the scenario's words.
        if !says(text) || text.starts_with("stage ") {
            continue;
        }
        if in_block {
            in_own_block = Some(text);
        }
        if text.starts_with(&format!("{stem}:")) {
            by_name = Some(text);
        }
    }
    in_own_block
        .or(by_name)
        .map(|text| crate::deliver::one_line(text, 400))
}

/// The evidence a `host_fault` record names when the host cannot judge the
/// objective's scenario at all (029's systemd user scopes): a fact about the
/// host, which judges that objective outside the gate.
pub(crate) const CANNOT_JUDGE: &str = "cannot_judge";

/// The evidence of a task's `host_fault` whose gate found main of its source
/// red on a check ([`main_red_fields`]): no fault of the work, no run of host
/// faults, no count toward the cap or a halt.
pub(crate) const MAIN_RED: &str = "main_red";

/// The record of a gate that had no room to judge `task`, at `now_ms`: the
/// reason with the check, its measure and its threshold, the facts
/// themselves, and `escalate` when the run of them on this task began more
/// than [`ROOM_ESCALATE_AFTER`] ago and none escalated yet. Another task's
/// run gives it no age.
pub(crate) fn no_room(
    mission: &Mission,
    task: i64,
    short: &Shortfall,
    log: &str,
    now_ms: i64,
) -> Value {
    short_of_room(mission, task, short, log, now_ms)
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
        Shortfall::Slot { waited_ms, budget } => format!(
            "no room to judge now: a scenario run waited {} for a host scenario slot \
             (SPAWN_GATE_HOST_JOBS={budget} in all, shared by every gate on this host); \
             not a verdict, judged again later",
            gate::spoken_ms(*waited_ms)
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
        Shortfall::Memory { .. } | Shortfall::Slot { .. } => json!({}),
        Shortfall::Database { server, .. } => json!({ "server": server }),
    };
    if let (Some(target), Some(extra)) = (record.as_object_mut(), extra.as_object()) {
        target.extend(extra.clone());
    }
    record
}

/// Whether a delivered task ran past its mission's budget
/// (`{"minutes": n, "tool_calls": n}`, either optional). `active_ms` is the
/// time it could work ([`active_ms`]), never the wall clock.
pub(crate) fn over_budget(budget: &Value, active_ms: i64, tool_calls: i64) -> Option<String> {
    if let Some(minutes) = budget.get("minutes").and_then(Value::as_i64)
        && active_ms > minutes.saturating_mul(60_000)
    {
        return Some(format!(
            "the task worked past its {minutes}-minute budget ({} minutes active)",
            active_ms / 60_000
        ));
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

/// `spawn integrate [--once]`: pass after pass, or one. `spawn integrate
/// --drain`: ask the integrator running with the same root to drain
/// ([`request_drain`]).
pub(crate) fn integrate(rest: &[String]) -> Result<i32, String> {
    let once = rest.iter().any(|word| word == "--once");
    let drain = rest.iter().any(|word| word == "--drain");
    if once && drain {
        return Err(format!("--once and --drain do not go together\n{USAGE}"));
    }
    let valued: Vec<String> = rest
        .iter()
        .filter(|word| *word != "--once" && *word != "--drain")
        .cloned()
        .collect();
    let flags = Flags::parse(&valued).map_err(|error| format!("{error}\n{USAGE}"))?;
    let root = workspace_root(&flags)
        .map(PathBuf::from)
        .ok_or_else(|| format!("--root or SPAWN_WORKSPACE_ROOT is required\n{USAGE}"))?;
    if drain {
        return request_drain(&root);
    }
    // The drain's bound is checked before anything is judged or recorded.
    let drain_bound = drain_secs(crate::config::env_value(DRAIN_ENV).as_deref())?;
    // A drain request (SIGUSR1, `spawn integrate --drain`) from now on: its
    // default action would end the process like TERM.
    let drain_asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGUSR1, Arc::clone(&drain_asked))
        .map_err(|error| format!("cannot take drain requests (SIGUSR1): {error}"))?;
    crate::config::require_git()?;
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
    if let Some(path) = crate::config::env_value(gate::GATE_ENV_FILE)
        && let Err(error) = gate::write_gate_env(Path::new(&path), std::process::id(), &gate_env)
    {
        eprintln!(
            "integrate: {}: {error}; a deploy does not warm the gate's base cache",
            gate::GATE_ENV_FILE
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
    // The host's cores, shared among the gates (gate::Cpu), read once.
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let cpu = gate::Cpu::from_env(|name| std::env::var(name).ok(), cores)
        .map_err(|error| format!("{error}\n{USAGE}"))?;
    let mut integrator = Integrator::new(root, gate, gate_env, host_room, gates).with_cpu(cpu);
    // The base warm after each landing (warm.rs), unless the owner turned
    // it off; its logs and pid file under the user's cache, beside the base
    // cache it fills.
    match warm_dir(crate::config::env_value(WARM_ENV).as_deref()) {
        Ok(Some(dir)) => integrator = integrator.with_warm(dir),
        Ok(None) => println!("integrate: {WARM_ENV}=0: no base warm after a landing"),
        Err(error) => eprintln!("integrate: {error}; no base warm after a landing"),
    }
    // The shadow judging on a gate runner (shadow.rs), when one is named;
    // its directory is the integrator's own, beside the mirrors.
    match shadow::settings(
        crate::config::env_value(shadow::RUNNER_ENV).as_deref(),
        crate::config::env_value(shadow::EVERY_ENV).as_deref(),
    ) {
        Ok(Some(settings)) => {
            println!(
                "integrate: judged verdicts are shadowed on {} (every {}-th)",
                settings.runner, settings.every
            );
            let dir = integrator.judge.root.join(".shadow");
            integrator = integrator.with_shadow(settings, dir);
        }
        Ok(None) => {}
        Err(error) => eprintln!("integrate: {error}; no shadow judging"),
    }
    if integrator.judge.host.server_log.is_none() {
        no_server_log(&format!("{TEST_DATABASE_LOG} is unset"));
    }
    if once {
        let pass = integrator.pass();
        // One pass, and the gates it started run to their records.
        integrator.settle();
        return pass
            .map(|()| 0)
            .inspect_err(|error| eprintln!("integrate: {error}"));
    }
    write_pid_file(&integrator.judge.root);
    run_passes(&integrator, &drain_asked, drain_bound)
}

/// Pass after pass, until a drain asked by `drain_asked` ends (drained, or
/// at its bound of `drain_bound` seconds): then Ok(0). TERM ends the
/// process at once, wherever it is.
fn run_passes(
    integrator: &Integrator,
    drain_asked: &std::sync::atomic::AtomicBool,
    drain_bound: u64,
) -> Result<i32, String> {
    let asked = || drain_asked.load(std::sync::atomic::Ordering::SeqCst);
    let interval = loop_interval().max(Duration::from_secs(5));
    loop {
        if asked() && !integrator.draining() {
            integrator.begin_drain(integrator.own_store().as_deref(), drain_bound);
        }
        let pass = integrator.pass();
        if let Err(error) = &pass {
            eprintln!("integrate: {error}");
        }
        if integrator.draining()
            && integrator.drained(integrator.own_store().as_deref(), pass.is_ok())
        {
            return Ok(0);
        }
        // Between passes, a drain request is taken at once and a drain's
        // bound is kept to the fifth of a second.
        let next = std::time::Instant::now() + interval;
        loop {
            let now = std::time::Instant::now();
            if integrator.drain_expired(now) {
                integrator.expire_drain(integrator.own_store().as_deref());
                return Ok(0);
            }
            if now >= next || (asked() && !integrator.draining()) {
                break;
            }
            std::thread::sleep((next - now).min(Duration::from_millis(200)));
        }
    }
}

/// The drain's bound: `SPAWN_INTEGRATE_DRAIN_SECS`, seconds counted from
/// the `integrator.draining` record.
const DRAIN_ENV: &str = "SPAWN_INTEGRATE_DRAIN_SECS";

/// The drain's bound when [`DRAIN_ENV`] is unset: two hours, past the
/// longest gate seen (101 minutes).
const DRAIN_DEFAULT_SECS: u64 = 7200;

/// The longest drain [`DRAIN_ENV`] allows: a day.
const DRAIN_MAX_SECS: u64 = 86_400;

/// [`DRAIN_ENV`] from its text: a whole number of seconds from 1 to
/// [`DRAIN_MAX_SECS`], unset the default; anything else refuses to start.
pub(crate) fn drain_secs(text: Option<&str>) -> Result<u64, String> {
    let Some(text) = text else {
        return Ok(DRAIN_DEFAULT_SECS);
    };
    let trimmed = text.trim();
    trimmed
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| trimmed.parse::<u64>().ok())
        .flatten()
        .filter(|secs| (1..=DRAIN_MAX_SECS).contains(secs))
        .ok_or_else(|| {
            format!(
                "{DRAIN_ENV} is a whole number of seconds from 1 to {DRAIN_MAX_SECS}, not \
                 '{text}'; the integrator does not start"
            )
        })
}

/// The integrator's pid file in its root: its pid and start time, for
/// `spawn integrate --drain` with the same root ([`request_drain`]).
const PID_FILE: &str = "integrate.pid";

/// Write this process's [`PID_FILE`] under `root`; a failure is said, and
/// only `--drain` misses it (SIGUSR1 still drains).
fn write_pid_file(root: &Path) {
    let pid = std::process::id();
    let written = warm::start_ticks(pid)
        .ok_or_else(|| "cannot read this process's start time".to_string())
        .and_then(|ticks| {
            std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
            let path = root.join(PID_FILE);
            let fresh = root.join(format!("{PID_FILE}.{pid}"));
            std::fs::write(&fresh, format!("{pid} {ticks}\n"))
                .map_err(|error| error.to_string())?;
            std::fs::rename(&fresh, &path).map_err(|error| error.to_string())
        });
    if let Err(error) = written {
        eprintln!(
            "integrate: {}: {error}; `spawn integrate --drain` cannot find this integrator \
             (SIGUSR1 still drains it)",
            root.join(PID_FILE).display()
        );
    }
}

/// A pid file's pid and start time.
fn read_pid_file(text: &str) -> Option<(u32, u64)> {
    let mut words = text.split_whitespace();
    let pid = words.next()?.parse().ok()?;
    let ticks = words.next()?.parse().ok()?;
    words.next().is_none().then_some((pid, ticks))
}

/// Whether the process `pid` that started at `ticks` still runs: not gone,
/// not a zombie, and not another process under a pid used again.
fn still_runs(pid: u32, ticks: u64) -> bool {
    warm::start_ticks(pid) == Some(ticks) && !warm::exited_unreaped(pid)
}

/// `spawn integrate --drain`: send a drain request (SIGUSR1) to the
/// integrator whose [`PID_FILE`] is under `root`, then wait until that
/// process is gone (it exits once drained, or at its bound), and exit 0.
fn request_drain(root: &Path) -> Result<i32, String> {
    let path = root.join(PID_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("no integrator runs under {}: {error}", root.display()))?;
    let (pid, ticks) = read_pid_file(&text)
        .ok_or_else(|| format!("{} is not an integrator's pid file", path.display()))?;
    if !still_runs(pid, ticks) {
        return Err(format!(
            "no integrator runs under {}: process {pid} ({}) is gone",
            root.display(),
            path.display()
        ));
    }
    let sent = std::process::Command::new("kill")
        .args(["-USR1", &pid.to_string()])
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|error| format!("kill -USR1 {pid}: {error}"))?;
    if !sent.status.success() && still_runs(pid, ticks) {
        return Err(format!(
            "kill -USR1 {pid}: {}",
            String::from_utf8_lossy(&sent.stderr).trim()
        ));
    }
    println!("integrate: asked the integrator (pid {pid}) to drain; waiting until it is gone");
    while still_runs(pid, ticks) {
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("integrate: the integrator (pid {pid}) is gone");
    Ok(0)
}

/// `SPAWN_INTEGRATE_WARM=0` turns the base warm after each landing off
/// (warm.rs); unset or 1, it is on.
const WARM_ENV: &str = "SPAWN_INTEGRATE_WARM";

/// Where the warm's logs and pid file go, from `SPAWN_INTEGRATE_WARM`:
/// `$XDG_CACHE_HOME/spawn/gate-warm`, else `$HOME/.cache/spawn/gate-warm`;
/// None when it is off.
fn warm_dir(setting: Option<&str>) -> Result<Option<PathBuf>, String> {
    match setting.map(str::trim) {
        None | Some("" | "1") => {}
        Some("0") => return Ok(None),
        Some(other) => return Err(format!("{WARM_ENV} is 0 or 1, not {other:?}")),
    }
    let cache = crate::config::env_value("XDG_CACHE_HOME")
        .filter(|path| Path::new(path).is_absolute())
        .map(PathBuf::from)
        .or_else(|| crate::config::env_value("HOME").map(|home| Path::new(&home).join(".cache")))
        .ok_or_else(|| "neither XDG_CACHE_HOME nor HOME is set".to_string())?;
    Ok(Some(cache.join("spawn").join("gate-warm")))
}

struct Integrator {
    judge: Judge,
    /// Gates that may run at once ([`gate_slots`]).
    gates: usize,
    /// How the host's cores are shared among the gates ([`gate::Cpu`]):
    /// each gate's SPAWN_GATE_JOBS and whether it has room to start. None
    /// (tests) leaves the gate its own default and never waits for CPU.
    cpu: Option<gate::Cpu>,
    /// The judgings in flight, one per mission at most.
    running: RefCell<Vec<Running>>,
    /// The store whose connection holds the integrator's lock, kept while
    /// a judging runs: its thread records through it.
    held: RefCell<Option<Arc<PostgresStore>>>,
    /// The waits already said, by task and kind ([`Wait`]): each is said
    /// once, when the task starts waiting, and again only after the task
    /// has had a gate ([`Integrator::wait_over`]).
    waiting: RefCell<std::collections::HashSet<(i64, Wait)>>,
    /// What step 0 keeps of the owner's requests between passes: the waits
    /// already said, and the owner judgings' backoffs ([`owner::Kept`]).
    owner: RefCell<owner::Kept>,
    /// When each mission's last judging ended (this process's reading):
    /// its next task waits for a gate from then.
    last_judged: RefCell<std::collections::HashMap<i64, i64>>,
    /// The base warm after each landing ([`warm`]); None leaves the base
    /// cache to the gates and the deploy (tests, and a warm directory that
    /// cannot be named).
    warms: RefCell<Option<warm::Warms>>,
    /// The shadow judging of verdicts on a gate runner ([`shadow`]); None
    /// when no runner is named (`SPAWN_GATE_SHADOW_RUNNER`).
    shadows: RefCell<Option<shadow::Shadows>>,
    /// While a source's main is red, how long after the last red gate or
    /// probe one more gate on it runs anyway, a probe
    /// (`SPAWN_GATE_MAIN_RED_PROBE_MS`, [`main_red_probe_ms`]).
    red_probe_ms: i64,
    /// The drain asked of this integrator, once asked ([`Drain`]).
    drain: RefCell<Option<Drain>>,
    /// Whether the last pass left a landing to act on: a mission with a
    /// task to land that landed it (its source moved: the next pass reads
    /// it again), failed, or was not visited. A drain ends only after a
    /// pass with none.
    unlanded: std::cell::Cell<bool>,
}

/// A drain: no gate, owner judging or base warm starts any more; the
/// passes go on until no judging of this integrator runs and what they
/// recorded was acted on, then `integrator.drained` and exit 0; at the
/// bound, `integrator.drain_expired` ([`Integrator::expire_drain`]).
struct Drain {
    /// When the bound is counted from: the `integrator.draining` record.
    since: std::time::Instant,
    bound_s: u64,
}

/// How long a source red on a check waits between probe gates by default:
/// an hour. A probe lets a candidate that fixes main reach the gate before
/// main moves by other means.
const MAIN_RED_PROBE_MS: i64 = 3_600_000;

/// [`MAIN_RED_PROBE_MS`] from `SPAWN_GATE_MAIN_RED_PROBE_MS`'s text: a whole
/// How long a main has been red, `ms` on the database's clock, for the
/// integrator's log: "3 h 12 min", "4 min".
fn red_for(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    if minutes >= 60 {
        format!("{} h {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    }
}

/// number of milliseconds from 1 up; anything else is the default.
pub(crate) fn main_red_probe_ms(text: Option<&str>) -> i64 {
    text.and_then(|text| text.trim().parse::<i64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(MAIN_RED_PROBE_MS)
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
    /// The landings the judging threads made, for the main thread's base
    /// warm ([`Integrator::poll_warm`]): a thread cannot start the warm
    /// itself (its parent-death signal follows the thread that spawned it).
    landed: Arc<std::sync::Mutex<Vec<warm::Want>>>,
    /// The verdicts the judging threads recorded that a runner may judge
    /// again, for the main thread's shadow ([`Integrator::poll_shadow`]):
    /// as for the warm, a thread cannot start it itself.
    shadowed: Arc<std::sync::Mutex<Vec<shadow::Want>>>,
    /// The stop of the judging this clone runs (set on its thread's clone,
    /// [`Integrator::start_judging`]); None on the main thread's.
    stop: Option<Arc<Stop>>,
}

/// The stop of one judging in flight: an owner's urgent landing on its
/// source (design-owner-push-between-gates.md, D2) sets `by` and then
/// `flag`, which kills its gate ([`gate::stop_with`]); the judging then
/// records itself discarded by that request, never a verdict on the work.
#[derive(Default)]
struct Stop {
    flag: Arc<std::sync::atomic::AtomicBool>,
    /// The request that stopped it, and the owner's reason.
    by: std::sync::Mutex<Option<(i64, Option<String>)>>,
}

impl Stop {
    /// Stop the judging for `request` (the owner's `reason`); false when it
    /// was stopped already.
    fn stop(&self, request: i64, reason: Option<String>) -> bool {
        let mut by = self
            .by
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if by.is_some() {
            return false;
        }
        *by = Some((request, reason));
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        true
    }

    /// The request that stopped the judging, if one did.
    fn by(&self) -> Option<(i64, Option<String>)> {
        self.by
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The settings of host faults: the test database's log
/// ([`TEST_DATABASE_LOG`], none when unset), the first backoff
/// (`SPAWN_GATE_HOST_FAULT_BACKOFF_SECS`, [`host_fault_base`]) and how many
/// host faults one report revision of a task may have before the next counts
/// as an abort (`SPAWN_GATE_HOST_FAULT_CAP`, [`host_fault_cap`]).
#[derive(Clone, Debug)]
struct HostSettings {
    server_log: Option<PathBuf>,
    backoff: Duration,
    cap: u32,
}

impl HostSettings {
    fn from_env() -> Self {
        Self {
            server_log: crate::config::env_value(TEST_DATABASE_LOG).map(PathBuf::from),
            backoff: host_fault_base(
                crate::config::env_value("SPAWN_GATE_HOST_FAULT_BACKOFF_SECS").as_deref(),
            ),
            cap: host_fault_cap(crate::config::env_value("SPAWN_GATE_HOST_FAULT_CAP").as_deref()),
        }
    }
}

/// How many `host_fault` verdicts one report revision of a task may have; the
/// next one is recorded as `aborted` instead, so it counts toward the abort
/// halt. A host fault is never a verdict on the work, but one that recurs on
/// the same work may be the work's own doing (a candidate that leaves the
/// shared test server broken for the base run after it), and an uncounted
/// fault would hold that task for ever. 3 by default, like the halts.
const HOST_FAULT_CAP: u32 = 3;

/// [`HOST_FAULT_CAP`] from `SPAWN_GATE_HOST_FAULT_CAP`'s text: a whole number
/// from 1 up; anything else is the default.
pub(crate) fn host_fault_cap(text: Option<&str>) -> u32 {
    text.and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|cap| *cap > 0)
        .unwrap_or(HOST_FAULT_CAP)
}

/// Whether a gate `host_fault` record's fields say the fault is not the
/// candidate's doing: the base's alone (the gate's fact with `on` `base`: a
/// scenario's harness error before any candidate code ran), or a check that
/// failed on the base as well as on the candidate (`on` `candidate and
/// base`: main fails it without the candidate). Those never count toward a
/// halt, so the cap neither counts nor caps them ([`HOST_FAULT_CAP`]).
fn base_only_fault(fields: &Value) -> bool {
    // Objective 033's evidence is a base that disagreed with itself, before
    // any candidate code; the gate's own fact names `on` `base`; the
    // `cannot_judge` a scenario says of a host that cannot judge it at all
    // (029) is no fault of the work either. A check failing on the base too
    // is main's failure, whatever the candidate does: a red main (Supply PA's
    // check-tests, 2026-11-09 00:48-03:42) would otherwise cap every task's
    // next gate as an abort and feed the abort halt. No one of them counts
    // toward the cap nor is ever capped.
    // A red main (evidence `main_red`, `on` `main`) is main's, not the
    // candidate's, likewise, and so is the gate's own test server that did
    // not start (`on` `test server`, evidence `test_server_never_started`),
    // whichever start it was: no candidate code makes a server start.
    matches!(
        fields.get("evidence").and_then(Value::as_str),
        Some(BASE_DISAGREES | CANNOT_JUDGE | MAIN_RED)
    ) || matches!(
        fields
            .get("fact")
            .and_then(|fact| fact.get("on"))
            .and_then(Value::as_str),
        Some("base" | "candidate and base" | "main" | "test server")
    )
        // A sandbox that never started in the base's tree or the gate's own
        // (the socket scan): before any candidate code ran, as on the base.
        || fields.get("fact").is_some_and(|fact| {
            fact.get("on").and_then(Value::as_str) == Some("sandbox")
                && fact.get("tree").and_then(Value::as_str) != Some("candidate")
        })
}

/// The `aborted` record of a host fault past the cap: `faults` host faults
/// already at this revision, `cap` allowed; the fault's own record kept under
/// `host_fault` (its log at the top, as an abort's).
fn capped_host_fault(task: &Task, main: &str, faults: i64, cap: u32, mut fault: Value) -> Value {
    let log = fault
        .as_object_mut()
        .and_then(|fields| fields.remove("log"))
        .unwrap_or(Value::Null);
    // The fault as it was said, without its promise to judge again.
    let said = fault["reason"]
        .as_str()
        .unwrap_or_default()
        .split("; not a verdict on the work")
        .next()
        .unwrap_or_default()
        .to_string();
    json!({
        "reason": format!(
            "a host fault again: task {} has had {faults} at report revision {} on main {}, the \
             most SPAWN_GATE_HOST_FAULT_CAP allows is {cap}, so this one counts as an abort \
             ({said})",
            task.id,
            task.revisions,
            short_commit(main)
        ),
        "log": log,
        "host_fault": fault,
    })
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
    /// A probe of its source's red main ([`Integrator::red_holds`]): its
    /// `source.main_probe` is written when its gate starts, never before.
    probe: bool,
}

/// What a source's red main means for one ready task's gate
/// ([`Integrator::red_holds`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedHold {
    /// Main is not red, or not the red one: the gate runs as usual.
    No,
    /// Red, and a probe is due: the gate may run, as a probe.
    Probe,
    /// Red, no probe due: the gate waits.
    Held,
}

/// One judging in flight.
struct Running {
    mission: i64,
    source: String,
    task: i64,
    /// Whether this judging's gate gets the shared test server (it gets a
    /// test database, [`needs_test_database`], and the gate's environment
    /// names the shared server, [`gate::on_shared_server`], not a server of
    /// its own): only those count against `--gates`.
    shares_server: bool,
    /// The scenarios its gate runs at once (SPAWN_GATE_JOBS as given), 0
    /// when the integrator does not share the cores ([`gate::Cpu`]).
    jobs: usize,
    /// The owner's request this judging judges (an owner judging,
    /// [`Integrator::start_owner`]); None for a task's.
    owner: Option<i64>,
    /// Its stop, for an owner's urgent landing on its source ([`Stop`]).
    stop: Arc<Stop>,
    /// Its halt, for a drain past its bound ([`gate::Halt`]).
    halt: Arc<gate::Halt>,
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

/// How many gates can run at once: one per source (`sources`: each gated
/// mission's source and whether its gate gets the shared test server, as
/// [`Running::shares_server`] says), those
/// whose gate gets the shared test server at most `gates` (`--gates`); at
/// least 1. A source with both kinds counts as one that gets the server.
pub(crate) fn gates_at_once(sources: impl Iterator<Item = (String, bool)>, gates: usize) -> usize {
    let mut kinds: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for (source, shares) in sources {
        *kinds.entry(source).or_default() |= shares;
    }
    let shared = kinds.values().filter(|shares| **shares).count();
    let alone = kinds.len() - shared;
    (shared.min(gates) + alone).max(1)
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
                landed: Arc::new(std::sync::Mutex::new(Vec::new())),
                shadowed: Arc::new(std::sync::Mutex::new(Vec::new())),
                stop: None,
            },
            gates,
            cpu: None,
            running: RefCell::new(Vec::new()),
            held: RefCell::new(None),
            waiting: RefCell::new(std::collections::HashSet::new()),
            owner: RefCell::new(owner::Kept::default()),
            last_judged: RefCell::new(std::collections::HashMap::new()),
            warms: RefCell::new(None),
            shadows: RefCell::new(None),
            red_probe_ms: main_red_probe_ms(
                crate::config::env_value("SPAWN_GATE_MAIN_RED_PROBE_MS").as_deref(),
            ),
            drain: RefCell::new(None),
            unlanded: std::cell::Cell::new(false),
        }
    }

    /// Whether this integrator drains.
    fn draining(&self) -> bool {
        self.drain.borrow().is_some()
    }

    /// The task judgings in flight, by task: what a drain record names.
    fn gate_tasks(&self) -> Vec<i64> {
        let mut tasks: Vec<i64> = self
            .running
            .borrow()
            .iter()
            .filter(|gate| gate.owner.is_none())
            .map(|gate| gate.task)
            .collect();
        tasks.sort_unstable();
        tasks.dedup();
        tasks
    }

    /// The store the integrator's own records go through: the one its
    /// judgings use while one runs, else a new connection; None when none
    /// can be opened (said).
    fn own_store(&self) -> Option<Arc<PostgresStore>> {
        if let Some(store) = self.held.borrow().clone() {
            return Some(store);
        }
        match open_store() {
            Ok(store) => Some(Arc::new(store)),
            Err(error) => {
                eprintln!("integrate: no store for the integrator's own records: {error}");
                None
            }
        }
    }

    /// Record one of the integrator's own records (`integrator.*`); false
    /// when it could not be recorded (said).
    fn record_own(store: Option<&PostgresStore>, record: &Value) -> bool {
        let Some(store) = store else {
            eprintln!("integrate: cannot record {record}: no store");
            return false;
        };
        match store.integrator_record(record) {
            Ok(_) => true,
            Err(error) => {
                eprintln!("integrate: cannot record {record}: {error}");
                false
            }
        }
    }

    /// Begin the drain a request asked for (once): `integrator.draining`
    /// names the task judgings in flight and the bound, counted from now.
    /// From then on no gate, owner judging or base warm starts.
    fn begin_drain(&self, store: Option<&PostgresStore>, bound_s: u64) {
        if self.draining() {
            return;
        }
        self.reap();
        let gates = self.gate_tasks();
        let owners = self
            .running
            .borrow()
            .iter()
            .filter(|gate| gate.owner.is_some())
            .count();
        Self::record_own(
            store,
            &json!({
                "type": "integrator.draining", "gates": gates, "bound_s": bound_s,
            }),
        );
        *self.drain.borrow_mut() = Some(Drain {
            since: std::time::Instant::now(),
            bound_s,
        });
        println!(
            "integrate: draining: no gate starts any more; {} task gate{} and {owners} owner \
             judging{} run to their records and landings, then the integrator exits (at most \
             {bound_s} s)",
            gates.len(),
            if gates.len() == 1 { "" } else { "s" },
            if owners == 1 { "" } else { "s" },
        );
    }

    /// Whether the drain is over after a pass (`pass_ok`: it ran): no
    /// judging of this integrator runs and the pass left no landing to act
    /// on. Then `integrator.drained` is recorded, and the caller exits.
    fn drained(&self, store: Option<&PostgresStore>, pass_ok: bool) -> bool {
        if !self.draining() || !pass_ok || self.unlanded.get() {
            return false;
        }
        self.reap();
        if !self.running.borrow().is_empty() {
            return false;
        }
        let bound_s = self
            .drain
            .borrow()
            .as_ref()
            .map_or(0, |drain| drain.bound_s);
        if !Self::record_own(
            store,
            &json!({"type": "integrator.drained", "bound_s": bound_s}),
        ) {
            return false;
        }
        println!("integrate: drained: every gate it ran recorded and landed; exiting");
        true
    }

    /// Whether the drain has reached its bound at `now`.
    fn drain_expired(&self, now: std::time::Instant) -> bool {
        self.drain.borrow().as_ref().is_some_and(|drain| {
            now.saturating_duration_since(drain.since) >= Duration::from_secs(drain.bound_s)
        })
    }

    /// The drain's bound: the gates still running are halted (their
    /// process groups killed, as a gate past its time limit is, and their
    /// threads record nothing: [`gate::Halt`]), `integrator.drain_expired`
    /// names their tasks, and the caller exits.
    fn expire_drain(&self, store: Option<&PostgresStore>) {
        self.reap();
        let gates = self.gate_tasks();
        for gate in self.running.borrow().iter() {
            gate.halt.halt();
        }
        let bound_s = self
            .drain
            .borrow()
            .as_ref()
            .map_or(0, |drain| drain.bound_s);
        Self::record_own(
            store,
            &json!({
                "type": "integrator.drain_expired", "gates": gates, "bound_s": bound_s,
            }),
        );
        println!(
            "integrate: the drain reached its bound of {bound_s} s: the gates of {} stopped, \
             nothing recorded for them; exiting",
            if gates.is_empty() {
                "no task".to_string()
            } else {
                gates
                    .iter()
                    .map(|task| format!("task {task}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    }

    /// Whether `task`'s gate backs off this pass ([`gate_wait`]), saying
    /// so when it starts to: the line is printed once, not on every pass
    /// it goes on waiting, like every other wait.
    fn backs_off(&self, mission: &Mission, task: &Task) -> Option<Said> {
        let (kind, line) = gate_wait(mission, task)?;
        if self.waiting.borrow_mut().insert((task.id, kind)) {
            println!("{line}");
            return Some(Said::Now);
        }
        Some(Said::Before)
    }

    /// `task` has a gate now: each of its waits is said again when it
    /// starts again.
    fn wait_over(&self, task: i64) {
        self.waiting
            .borrow_mut()
            .retain(|(waiting, _)| *waiting != task);
    }

    /// Whether `task`'s gate waits because main of `source` is red on a
    /// check and has not moved (`main` is the red one), said when it does.
    /// Once [`Self::red_probe_ms`] has passed since the last red gate or
    /// probe on it, the gate may run anyway, a probe. Nothing is written
    /// here: the probe's `source.main_probe` is written when its gate
    /// starts ([`Self::start_judging`]), so a ready task that gets no gate
    /// this pass (another on its source, no gate free, no CPU room) spends
    /// no probe, and the store takes one probe per interval per source, so
    /// two missions on it never both probe. A main that moved is judged as
    /// usual (with the red check run on its base first).
    fn red_holds(&self, mission: &Mission, source: &str, main: &str, task: &Task) -> RedHold {
        let Some(red) = mission.red.as_ref().filter(|red| red.main == main) else {
            return RedHold::No;
        };
        // The database's clock, as the red's times are its own.
        let Some(now) = mission.now_ms else {
            return RedHold::No;
        };
        let last = red
            .probed_ms
            .map_or(red.latest_ms, |probed| probed.max(red.latest_ms));
        if now.saturating_sub(last) >= self.red_probe_ms {
            return RedHold::Probe;
        }
        if self.waiting.borrow_mut().insert((task.id, Wait::Red)) {
            println!(
                "integrate: mission {}: main of {source} ({}) is red on {} for {}; task {} \
                 waits until main moves (or the next probe)",
                mission.id,
                main.get(..12).unwrap_or(main),
                red.scenario,
                red_for(now.saturating_sub(red.since_ms)),
                task.id
            );
        }
        RedHold::Held
    }

    /// This integrator with the host's cores shared among its gates.
    fn with_cpu(mut self, cpu: gate::Cpu) -> Self {
        self.cpu = Some(cpu);
        self
    }

    /// This integrator warming the base cache after each landing, its logs
    /// and pid file in `dir`; a warm a predecessor left there is stopped
    /// first ([`warm::Warms::take_over`]).
    fn with_warm(self, dir: PathBuf) -> Self {
        let warms = warm::Warms::new(self.judge.gate.clone(), self.judge.gate_env.clone(), dir);
        warms.take_over();
        *self.warms.borrow_mut() = Some(warms);
        self
    }

    /// This integrator shadowing judged verdicts on a gate runner, its
    /// scratch and pid file in `dir`; a shadow a predecessor left there is
    /// stopped first ([`shadow::Shadows::take_over`]).
    fn with_shadow(self, settings: shadow::Settings, dir: PathBuf) -> Self {
        let shadows = shadow::Shadows::new(settings, dir);
        shadows.take_over();
        *self.shadows.borrow_mut() = Some(shadows);
        self
    }

    /// The pass's shadow, after its gates were given out: the one that
    /// ended is recorded (once: its directory is removed when the record is
    /// written, and a record that could not be written is tried again at
    /// the next pass), then the verdicts recorded since the last pass are
    /// offered, in order (one runs at a time: the others are skipped).
    /// Nothing here touches a task, a landing or a count.
    fn poll_shadow(&self, store: &PostgresStore) {
        let wanted: Vec<shadow::Want> = self
            .judge
            .shadowed
            .lock()
            .map(|mut wanted| wanted.drain(..).collect())
            .unwrap_or_default();
        let mut shadows = self.shadows.borrow_mut();
        let Some(shadows) = shadows.as_mut() else {
            return;
        };
        shadows.record_ended(|shadowed| {
            record(store, shadowed)?;
            println!(
                "integrate: task {} shadowed on {}: {} (the primary's {}), agrees {}",
                shadowed["id"],
                shadowed["runner"].as_str().unwrap_or_default(),
                shadowed["verdict"].as_str().unwrap_or("no verdict"),
                shadowed["primary"].as_str().unwrap_or_default(),
                shadowed["agrees"]
            );
            Ok(())
        });
        // A drain starts no background judging: the verdicts recorded
        // meanwhile are not shadowed (one already running is recorded when
        // it ends, or ends with the integrator).
        if self.draining() {
            return;
        }
        for want in wanted {
            shadows.offer(want);
        }
    }

    /// Main of `source` is `commit` now: warm it for the gates that judge
    /// with `scenarios`, at the pass's end ([`Self::poll_warm`]).
    fn want_warm(&self, source: &str, commit: &str, scenarios: &Path) {
        if let Some(warms) = self.warms.borrow_mut().as_mut() {
            warms.want(warm::Want {
                source: source.to_string(),
                commit: commit.to_string(),
                scenarios: scenarios.to_path_buf(),
            });
        }
    }

    /// The pass's warm, after its gates were given out: started when the
    /// source's mirror has the commit. The mirror is fetched only while no
    /// gate of that source uses it; otherwise a later pass starts it.
    fn poll_warm(&self) {
        // The judgings' landings since the last pass, in order: the last
        // one is the source's main now.
        let landed: Vec<warm::Want> = self
            .judge
            .landed
            .lock()
            .map(|mut landed| landed.drain(..).collect())
            .unwrap_or_default();
        // A drain starts no warm: the next integrator's gates warm their
        // base themselves.
        if self.draining() {
            return;
        }
        let mut warms = self.warms.borrow_mut();
        let Some(warms) = warms.as_mut() else {
            return;
        };
        for want in landed {
            warms.want(want);
        }
        let root = self.judge.root.clone();
        let busy = |source: &str| {
            self.running
                .borrow()
                .iter()
                .any(|gate| gate.source == source)
        };
        warms.poll(&|want: &warm::Want| {
            let mirror = sources::ensure_mirror(&root, Some(&want.source)).ok()?;
            let has = |mirror: &Path| {
                git(&[
                    "--git-dir",
                    &mirror.to_string_lossy(),
                    "cat-file",
                    "-e",
                    &format!("{}^{{commit}}", want.commit),
                ])
                .is_ok()
            };
            if has(&mirror) {
                return Some(mirror);
            }
            if busy(&want.source) || sources::refresh(&mirror, &want.source).is_err() {
                return None;
            }
            has(&mirror).then_some(mirror)
        });
    }

    fn pass(&self) -> Result<(), String> {
        self.unlanded.set(false);
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
            match gate.owner {
                Some(request) => eprintln!(
                    "integrate: the owner judging of request {request} on {} panicked; it is \
                     judged again",
                    gate.source
                ),
                None => eprintln!(
                    "integrate: mission {}: the judging of task {} panicked; it is judged again",
                    gate.mission, gate.task
                ),
            }
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
        self.unlanded.set(false);
        // A hold past its bound ends by its record, before any mission's
        // step: nothing lands on its source until then.
        expire_holds(store)?;
        // Step 0: the owner's landings, applied before any mission's step
        // reads its source's main, so no baseline check sees a main the
        // owner moved without its record. A source whose request is still
        // pending has its halts by baseline held this pass. Never under a
        // judging of the source (design-owner-push-between-gates.md, D1): a
        // request on a source with a judging in flight waits for the pass
        // after it, so no owner's landing moves main under a gate.
        let busy: std::collections::HashSet<String> = self
            .running
            .borrow()
            .iter()
            .map(|gate| gate.source.clone())
            .collect();
        let mut owner_moved = Vec::new();
        let mut owner_judge = Vec::new();
        let mut urgent = Vec::new();
        let pending = owner::apply_pending(
            store,
            &mut owner_moved,
            &busy,
            &mut self.owner.borrow_mut(),
            &mut owner_judge,
            &mut urgent,
        )?;
        self.stop_for(&urgent);
        let steps = self.steps(store, &pending, &owner_moved, owner_judge);
        self.poll_warm();
        self.poll_shadow(store);
        steps
    }

    /// Stop the task judgings in flight on the sources of `urgent` (the
    /// owner's urgent requests step 0 found waiting, D2), each once: its
    /// gate is killed, and it records itself discarded by the request
    /// ([`Stop`]). An owner judging is never stopped.
    fn stop_for(&self, urgent: &[owner::Request]) {
        for request in urgent {
            for gate in self.running.borrow().iter() {
                if gate.source == request.source
                    && gate.owner.is_none()
                    && gate.stop.stop(request.id, request.reason.clone())
                {
                    println!(
                        "integrate: the owner's urgent landing of {} on {} (request {}) stops the \
                         judging of task {} (mission {}): discarded, judged again on the new main",
                        request.commit, request.source, request.id, gate.task, gate.mission
                    );
                }
            }
        }
    }

    /// Every mission's step, after the holds past their bound and step 0:
    /// `pending` names the sources with an owner's landing still pending
    /// (each with its oldest request), whose missions do not halt by
    /// baseline this pass.
    fn steps(
        &self,
        store: &Arc<PostgresStore>,
        pending: &std::collections::HashMap<String, i64>,
        owner_moved: &[(String, String)],
        owner_judge: Vec<owner::Request>,
    ) -> Result<(), String> {
        let missions: Vec<Mission> =
            serde_json::from_value(store.gated_missions().map_err(|error| error.to_string())?)
                .map_err(|error| format!("gated missions: {error}"))?;
        // The owner's landings of step 0 are warmed for the scenarios of
        // the first mission on their source that is not halted.
        for (source, to) in owner_moved {
            if let Some(mission) = missions.iter().find(|mission| {
                mission.halted.is_none() && mission.source.as_deref() == Some(source.as_str())
            }) {
                self.want_warm(source, to, &pinned(Path::new(&mission.gate)));
            }
        }
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
            // A task to land that this pass does not land for good (not
            // visited, landed or gone stale so the source moved, or an
            // error) is left for the next pass: a drain waits for it.
            let landing = mission.halted.is_none() && mission.landing().is_some();
            if source.as_ref().is_some_and(|source| moved.contains(source)) {
                if landing {
                    self.unlanded.set(true);
                }
                continue;
            }
            match self.mission(store, &missions, mission, pending, &mut ready) {
                Ok(true) => {
                    if landing {
                        self.unlanded.set(true);
                    }
                    if let Some(source) = source {
                        moved.insert(source);
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    if landing {
                        self.unlanded.set(true);
                    }
                    eprintln!("integrate: mission {}: {error}", mission.id);
                }
            }
        }
        ready.retain(|candidate: &Ready| !moved.contains(&candidate.source));
        let at_once = gates_at_once(
            missions.iter().filter_map(|mission| {
                let source = mission.source.clone()?;
                Some((
                    source,
                    needs_test_database(&pinned(Path::new(&mission.gate)))
                        && gate::on_shared_server(&self.judge.gate_env),
                ))
            }),
            self.gates,
        );
        // The gated owner's requests first (design-owner-push-between-gates.md):
        // each takes its source's slot ahead of the source's ready tasks, never
        // a running judging's, within the same caps. Not on a source a task
        // landed on this pass: its main moved, and the next pass reads it.
        // A drain starts no judging: the ready tasks and the gated owner's
        // requests wait for the next integrator.
        if self.draining() {
            return Ok(());
        }
        for request in owner_judge {
            if !moved.contains(&request.source) {
                self.start_owner(store, &missions, request, at_once);
            }
        }
        self.start_ready(store, ready, at_once);
        Ok(())
    }

    /// Whether an owner judging fits the caps now: its jobs
    /// ([`gate::Cpu::jobs_for`], None when the cores are not shared), or
    /// what it waits for, `running` being the judgings on the shared test
    /// server.
    fn room_for_owner(
        &self,
        shares_server: bool,
        running: usize,
        at_once: usize,
    ) -> Result<Option<usize>, String> {
        if waits_for_gate(shares_server, running, self.gates) {
            return Err(format!("a gate ({running} of {} running)", self.gates));
        }
        let jobs = self
            .cpu
            .as_ref()
            .map(|cpu| cpu.jobs_for(shares_server, at_once));
        if let (Some(cpu), Some(jobs)) = (&self.cpu, jobs) {
            let busy: usize = self.running.borrow().iter().map(|gate| gate.jobs).sum();
            if !cpu.fits(busy, jobs) {
                return Err(format!(
                    "CPU room: its gate's {jobs} jobs and the running gates' {busy}"
                ));
            }
        }
        Ok(jobs)
    }

    /// Start the owner judging of `request` (a gated owner's request step 0
    /// handed over) when its source has no judging in flight and the caps
    /// leave room, judged with the scenarios of the first mission on its
    /// source that is not halted. Otherwise it stays pending, for a later
    /// pass.
    fn start_owner(
        &self,
        store: &Arc<PostgresStore>,
        missions: &[Mission],
        request: owner::Request,
        at_once: usize,
    ) {
        let said = format!(
            "integrate: the owner's landing of {} on {} (request {})",
            request.commit, request.source, request.id
        );
        if self
            .running
            .borrow()
            .iter()
            .any(|gate| gate.source == request.source)
        {
            return;
        }
        let Some(mission) = missions.iter().find(|mission| {
            mission.halted.is_none() && mission.source.as_deref() == Some(request.source.as_str())
        }) else {
            if self.owner.borrow_mut().said.insert(request.id) {
                println!("{said} waits: no mission on its source that is not halted judges it");
            }
            return;
        };
        let mut gated = mission.clone();
        gated.gate = pinned(Path::new(&mission.gate))
            .to_string_lossy()
            .into_owned();
        let shares_server = needs_test_database(Path::new(&gated.gate))
            && gate::on_shared_server(&self.judge.gate_env);
        let running = self
            .running
            .borrow()
            .iter()
            .filter(|gate| gate.shares_server)
            .count();
        let jobs = match self.room_for_owner(shares_server, running, at_once) {
            Ok(jobs) => jobs,
            Err(wait) => {
                if self.owner.borrow_mut().said.insert(request.id) {
                    println!("{said} waits for {wait}");
                }
                return;
            }
        };
        let mirror = match sources::ensure_mirror(&self.judge.root, Some(&request.source)) {
            Ok(mirror) => mirror,
            Err(error) => {
                eprintln!("{said}: {error}");
                return;
            }
        };
        self.owner.borrow_mut().said.remove(&request.id);
        let judge = self.judge.clone();
        let backoff = Arc::clone(&self.owner.borrow().backoff);
        let (store, source, id, mission_id, reason) = (
            Arc::clone(store),
            request.source.clone(),
            request.id,
            mission.id,
            request.reason.clone(),
        );
        let halt = Arc::new(gate::Halt::default());
        let thread_halt = Arc::clone(&halt);
        let handle = std::thread::Builder::new()
            .name(format!("owner-gate-{id}"))
            .spawn(move || {
                gate::halt_with(thread_halt);
                let judged = judge.judge_owner(&store, &mirror, &gated, &request, jobs);
                owner::judged(&said, id, &judged, &backoff, judge.host.backoff);
            });
        let handle = match handle {
            Ok(handle) => handle,
            Err(error) => {
                eprintln!("integrate: start the owner judging of request {id}: {error}");
                return;
            }
        };
        println!(
            "integrate: judging the owner's landing of request {id} on {source} ({} of {} \
             gates){}",
            running + usize::from(shares_server),
            self.gates,
            reason
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default()
        );
        self.running.borrow_mut().push(Running {
            mission: mission_id,
            source,
            task: 0,
            shares_server,
            jobs: jobs.unwrap_or(0),
            owner: Some(id),
            stop: Arc::new(Stop::default()),
            halt,
            handle,
        });
    }

    /// One mission's step. True when it landed a task (its source's main
    /// moved). A ready task it may judge goes to `ready`; the gates are
    /// given out after every mission had its step ([`Self::start_ready`]).
    /// `missions` is every mission of the pass, for the landings on a
    /// shared source; `pending`, the sources with an owner's landing
    /// pending.
    fn mission(
        &self,
        store: &Arc<PostgresStore>,
        missions: &[Mission],
        mission: &Mission,
        pending: &std::collections::HashMap<String, i64>,
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
        let aborted = Self::enforce_budget(store, mission)?;
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
                    Landing::Landed => {
                        // The next gates on this source judge against the
                        // new main: warm its base now, in the background.
                        if let Some(candidate) = task.candidate.as_deref() {
                            self.want_warm(source, candidate, &scenarios);
                        }
                        return Ok(true);
                    }
                    Landing::Stale | Landing::Gone => return Ok(true),
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
                match read_main(store, mission, source, &mirror, pending)? {
                    Reading::Judge(main) => judgeable = Some((mirror, main)),
                    Reading::Nothing => {}
                    Reading::Halted => return Ok(false),
                }
            }
        }
        // An objective mission's free slots get their objectives, whatever
        // its gate is doing; a closed mission gets none.
        // A task this pass aborted, or one whose gate runs now, yields
        // nothing: the first is over, the second is being judged.
        let mut settled: std::collections::BTreeSet<i64> = aborted.into_iter().collect();
        settled.extend(
            self.running
                .borrow()
                .iter()
                .filter(|gate| gate.mission == mission.id)
                .map(|gate| gate.task),
        );
        if mission.state != "done"
            && let Err(error) = objectives::assign_next(store, mission, &settled)
        {
            eprintln!("integrate: mission {}: {error}", mission.id);
        }
        if held {
            return Ok(false);
        }
        if let (Some((mirror, main)), Some(task)) = (judgeable, mission.ready()) {
            if self.backs_off(mission, task).is_some() {
                return Ok(false);
            }
            let red = self.red_holds(mission, source, &main, task);
            if red == RedHold::Held {
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
                probe: red == RedHold::Probe,
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
    /// Start the gates of `ready` that may run now: one per mission and per
    /// source, those on the shared test server within `--gates`, and each
    /// within the host's cores ([`gate::Cpu::fits`]), `at_once` being how
    /// many gates can run at once ([`gates_at_once`]) for the default jobs.
    fn start_ready(&self, store: &Arc<PostgresStore>, mut ready: Vec<Ready>, at_once: usize) {
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
            // A gate shares the test server only when it gets one and that
            // one is the shared server: with SPAWN_GATE_PG_IMAGE or
            // SPAWN_GATE_PG_BIN every gate runs a server of its own, and
            // `--gates` (a bound on the shared server) does not hold it.
            let shares_server = needs_test_database(Path::new(&candidate.gated.gate))
                && gate::on_shared_server(&self.judge.gate_env);
            let running = self
                .running
                .borrow()
                .iter()
                .filter(|gate| gate.shares_server)
                .count();
            if waits_for_gate(shares_server, running, self.gates) {
                if self
                    .waiting
                    .borrow_mut()
                    .insert((candidate.task.id, Wait::Gate))
                {
                    println!(
                        "integrate: mission {}: task {} waits for a gate ({running} of {} \
                         running)",
                        candidate.mission, candidate.task.id, self.gates
                    );
                }
                continue;
            }
            let jobs = self.cpu.as_ref().map(|cpu| {
                let shared = shares_server;
                cpu.jobs_for(shared, at_once)
            });
            if let (Some(cpu), Some(jobs)) = (&self.cpu, jobs) {
                let busy: usize = self.running.borrow().iter().map(|gate| gate.jobs).sum();
                if !cpu.fits(busy, jobs) {
                    if self
                        .waiting
                        .borrow_mut()
                        .insert((candidate.task.id, Wait::Cpu))
                    {
                        println!(
                            "integrate: mission {}: task {} waits for CPU room: its gate's {jobs} \
                             jobs, the running gates' {busy} and the {} cores kept for the \
                             workers (SPAWN_GATE_WORKER_CORES) exceed the {} cores",
                            candidate.mission, candidate.task.id, cpu.reserved, cpu.cores
                        );
                    }
                    continue;
                }
            }
            self.wait_over(candidate.task.id);
            let mission = candidate.mission;
            if let Err(error) = self.start_judging(store, candidate, running, shares_server, jobs) {
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
        jobs: Option<usize>,
    ) -> Result<(), String> {
        let Ready {
            mission: mission_id,
            source,
            mirror,
            main,
            gated,
            count,
            task,
            probe,
        } = candidate;
        // A probe of a red main is spent here, as its gate starts: the
        // store refuses one that is not due (another mission's on the same
        // source took it), and then the gate does not start.
        if probe {
            let red = gated
                .red
                .as_ref()
                .map(|red| red.scenario.clone())
                .unwrap_or_default();
            if let Err(error) = store.integrator_record(&json!({
                "type": "source.main_probe", "source": source, "main": main,
                "scenario": red, "task": task.id, "mission": mission_id,
                "probe_ms": self.red_probe_ms,
            })) {
                println!(
                    "integrate: mission {mission_id}: no probe of {source}'s red main now: {error}"
                );
                return Ok(());
            }
            println!(
                "integrate: mission {mission_id}: main of {source} is red on {red}; task {} is \
                 judged anyway, a probe",
                task.id
            );
        }
        let mut judge = self.judge.clone();
        let stop = Arc::new(Stop::default());
        judge.stop = Some(Arc::clone(&stop));
        let task_id = task.id;
        let (store, source_owned) = (Arc::clone(store), source.clone());
        let halt = Arc::new(gate::Halt::default());
        let thread_halt = Arc::clone(&halt);
        let handle = std::thread::Builder::new()
            .name(format!("gate-{task_id}"))
            .spawn(move || {
                gate::halt_with(thread_halt);
                if let Some(stop) = &judge.stop {
                    gate::stop_with(Arc::clone(&stop.flag));
                }
                judged_or_aborted(&store, &task, &main, mission_id, || {
                    judge.judge(
                        &store,
                        &mirror,
                        &source_owned,
                        &main,
                        &gated,
                        count,
                        &task,
                        jobs,
                    )
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
                "integrate: mission {mission_id}: judging task {task_id} (not on the shared \
                 test server: beside the {running} of {} gates)",
                self.gates
            );
        }
        self.running.borrow_mut().push(Running {
            mission: mission_id,
            source,
            task: task_id,
            shares_server,
            jobs: jobs.unwrap_or(0),
            owner: None,
            stop,
            halt,
            handle,
        });
        Ok(())
    }

    /// Stop the delivered tasks over budget and record each as its own
    /// `task.over_budget`, with the work it keeps; the ids of those it
    /// recorded. A task whose work has not reached its branch yet waits for
    /// the push ([`kept_work`]) and is recorded by a later pass.
    fn enforce_budget(store: &PostgresStore, mission: &Mission) -> Result<Vec<i64>, String> {
        let Some(budget) = &mission.budget else {
            return Ok(Vec::new());
        };
        let mut over: Vec<i64> = Vec::new();
        let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
        let gap_ms = budget_gap(crate::config::env_value("SPAWN_BUDGET_GAP_SECS").as_deref())
            .as_millis()
            .try_into()
            .unwrap_or(i64::MAX);
        for task in &mission.tasks {
            let (TaskState::Delivered, Some(at)) = (task.state, task.delivered_at_ms) else {
                continue;
            };
            let events = store
                .session_activity(&task.session_id, at)
                .map_err(|error| error.to_string())?;
            let calls = events
                .iter()
                .filter(|event| event.name == "tool.started")
                .count()
                .try_into()
                .unwrap_or(i64::MAX);
            let active = active_ms(at, now, &events, gap_ms, |from, to| {
                store
                    .fleet_alive_ms(from, to, gap_ms)
                    .map_err(|error| error.to_string())
            })?;
            let Some(reason) = over_budget(budget, active, calls) else {
                continue;
            };
            // The Stop first, under its fixed key: if the record then
            // fails, the next pass finds the task still delivered and
            // over budget, and submits (a no-op) and records again.
            stop(store, task)?;
            let (branch, commit, diff) = match kept_work(store, mission, task)? {
                // Its work has not reached the source yet: the next pass
                // records what the pusher's head will hold.
                Kept::Waiting => continue,
                Kept::Work {
                    branch,
                    commit,
                    diff,
                } => (branch, Some(commit), diff),
                Kept::Lost { branch } => (branch, None, String::new()),
            };
            // `measure` says the active-time budget made this overrun, with
            // what it measured. `Outcome::failure` (objectives.rs) does not
            // count it as a failed attempt: attempts are per objective since
            // its last yield, so a counted overrun would yield the
            // continuation task on assignment. The measure stays on the
            // record for a later rule.
            let mut overrun = json!({"type": "task.over_budget", "id": task.id,
                                     "reason": reason, "tool_calls": calls,
                                     "elapsed_ms": active, "budget": budget.clone(),
                                     "measure": objectives::ACTIVE_MEASURE,
                                     "branch": branch});
            if let Some(commit) = &commit {
                overrun["commit"] = json!(commit);
            }
            if !diff.is_empty() {
                overrun["diff"] = json!(diff);
            }
            if let Some(continues) = task.continues {
                overrun["continues"] = json!(continues);
                // A second overrun of a chain: a task that continues an
                // overrun task, while no configure came between (a third
                // overrun after a configure is a first).
                let earlier = store
                    .overrun_at(continues)
                    .map_err(|error| error.to_string())?;
                if earlier.is_some_and(|at| {
                    mission
                        .configured_ms
                        .is_none_or(|configured| configured <= at)
                }) {
                    overrun["chain"] = json!(true);
                }
            }
            if let Some(objective) = task.objective {
                overrun["objective"] = json!(objective);
            }
            if commit.is_none() {
                overrun["pushed"] = json!(false);
            }
            record(store, &overrun)?;
            println!(
                "integrate: task {} over its budget, its work kept on {branch}: {reason}",
                task.id
            );
            if commit.is_none() {
                eprintln!(
                    "integrate: task {}: its work never reached {branch} (the push failed): its \
                     next task is told so",
                    task.id
                );
            }
            over.push(task.id);
        }
        Ok(over)
    }
}

impl Judge {
    /// The gate's environment for a judging with `scenarios`, given `jobs`:
    /// the test server never the owner's, by identity, asked now (a socket
    /// mount or a DNS name can change between gate runs); none at all for a
    /// mission marked [`NO_TEST_DATABASE`]; SPAWN_GATE_JOBS the jobs it was
    /// given room for, never its own default.
    fn judging_env(&self, scenarios: &Path, jobs: Option<usize>) -> Vec<(String, String)> {
        let owner = std::env::var("SPAWN_DATABASE_URL").ok();
        let (mut gate_env, warning) =
            gate::withhold_owners_cluster(&self.gate_env, owner.as_deref(), |url| {
                spawn_postgres::cluster_identity(url).map_err(|error| error.to_string())
            });
        if !needs_test_database(scenarios) {
            gate_env.retain(|(name, _)| !withheld_without_test_database(name));
        }
        if let Some(jobs) = jobs {
            gate_env.retain(|(name, _)| name != "SPAWN_GATE_JOBS");
            gate_env.push(("SPAWN_GATE_JOBS".to_string(), jobs.to_string()));
        }
        if let Some(warning) = warning {
            eprintln!("integrate: {warning}");
        }
        gate_env
    }

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
        jobs: Option<usize>,
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
            jobs,
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
        jobs: Option<usize>,
    ) -> Result<(), String> {
        let scenarios = Path::new(&mission.gate);
        // Read only: the integrator never commits a workspace.
        let repo = GitWorkspace::new(mirror, &self.root);
        // The checks the candidate's gate run saw fail once and pass again
        // (its verdict's `flakes`), carried on whichever record this judging
        // writes: set once the run has a verdict.
        let flakes: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        // The scenarios the gate found main red on (its verdict's
        // `main_red_scenarios`: solved on an ancestor of this base, unsolved
        // on it), carried the same way: alerts.rs opens `scenario_red` from
        // them and clears it when a later gate of the mission no longer
        // names one.
        let red_scenarios: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        // A gate on a main that is red (design-main-red.md) runs only as a
        // probe ([`Integrator::red_holds`]): its record says so.
        let probe = mission.red.as_ref().is_some_and(|red| red.main == main);
        // Where main is now, read once per judging after the gate ends (one
        // `ls-remote`), by whichever of the main_red record and `gated` asks
        // first; the error when it could not be read.
        let moved: std::cell::OnceCell<Result<Option<String>, String>> = std::cell::OnceCell::new();
        let main_now = || moved.get_or_init(|| main_moved(source, main)).clone();
        // What the candidate's gate run judged and where (objective 035's
        // `inputs` and `env`, [`inputs`]), carried on the record of the
        // verdict it gave: set as that run starts (`env` once it ended).
        let bound: std::cell::RefCell<Option<(Value, Value)>> = std::cell::RefCell::new(None);
        let gated = |verdict: &str, extra: Value| -> Result<(), String> {
            // Stopped for an owner's urgent landing (D2): whatever the gate
            // found but an acceptance (which lands as usual), the judging is
            // discarded by that request, `stale`, counted nowhere, and the
            // task is judged again on the main the owner lands.
            if verdict != "accepted"
                && let Some((request, reason)) = self.stop.as_ref().and_then(|stop| stop.by())
            {
                let mut discarded = json!({"type": "task.gated", "id": task.id,
                "verdict": "stale", "found": "discarded", "revision": task.revisions,
                "base": main, "scenarios": count, "request": request,
                "reason": format!(
                    "discarded by the owner's urgent landing (request {request}){}",
                    reason.map(|reason| format!(": {reason}")).unwrap_or_default()
                )});
                if let Some(log) = extra.get("log") {
                    discarded["log"] = log.clone();
                }
                record(store, &discarded)?;
                println!(
                    "integrate: task {} gated: stale (discarded by the owner's request {request})",
                    task.id
                );
                return Ok(());
            }
            let mut unread = None;
            // Main moved while the gate ran: what it found is about a main
            // that is no longer main, so it is `stale`, judged again on the
            // new one, before anything is counted (the cap below, the abort
            // halt). An `accepted` keeps the landing's compare-and-swap as
            // its guard ([`land`]).
            let now = if verdict == "accepted" {
                None
            } else {
                main_now().unwrap_or_else(|error| {
                    eprintln!("integrate: task {}: where main is now: {error}", task.id);
                    unread = Some(error);
                    None
                })
            };
            if let Some(now) = now {
                let reason = extra["reason"].as_str().unwrap_or_default();
                let mut stale = stale_on_main_move(main, &now, verdict, reason);
                stale["id"] = json!(task.id);
                stale["revision"] = json!(task.revisions);
                stale["scenarios"] = json!(count);
                if let Some(log) = extra.get("log") {
                    stale["log"] = log.clone();
                }
                record(store, &stale)?;
                println!(
                    "integrate: task {} gated: stale ({verdict} on {}, main is {} now)",
                    task.id,
                    short_commit(main),
                    short_commit(&now)
                );
                return Ok(());
            }
            // A host fault past the cap for this revision counts as an abort
            // ([`HOST_FAULT_CAP`]). Only a fault that may be the candidate's
            // does: a base-only one (the gate's fact `on` `base`) comes
            // before any candidate code, and one on the candidate and the
            // base fails on main too; neither counts toward a halt, so
            // neither reaches the cap nor is ever capped.
            let (verdict, extra) = if verdict == "host_fault" && !base_only_fault(&extra) {
                let faults = store
                    .task_host_faults(task.id, i64::from(task.revisions), main)
                    .map_err(|error| error.to_string())?;
                if faults >= i64::from(self.host.cap) {
                    (
                        "aborted",
                        capped_host_fault(task, main, faults, self.host.cap, extra),
                    )
                } else {
                    (verdict, extra)
                }
            } else {
                (verdict, extra)
            };
            let mut record_value = json!({"type": "task.gated", "id": task.id, "verdict": verdict,
                                          "revision": task.revisions, "base": main,
                                          "scenarios": count});
            if let (Some(target), Some(extra)) = (record_value.as_object_mut(), extra.as_object()) {
                target.extend(extra.clone());
            }
            if !flakes.borrow().is_empty() {
                record_value["flakes"] = json!(*flakes.borrow());
            }
            if !red_scenarios.borrow().is_empty() {
                record_value["main_red_scenarios"] = json!(*red_scenarios.borrow());
            }
            if probe {
                record_value["probe"] = json!(true);
            }
            // Main could not be read: the verdict as found, and the record
            // says main was never checked.
            if let Some(error) = unread {
                record_value["main_read"] = json!(error);
            }
            if let Some((inputs, env)) = bound.borrow().as_ref() {
                record_value["inputs"] = inputs.clone();
                record_value["env"] = env.clone();
            }
            record(store, &record_value)?;
            println!("integrate: task {} gated: {verdict}", task.id);
            // A verdict a gate run gave may be judged again on a runner,
            // beside it ([`shadow`]): offered to the main thread, never
            // waited for.
            if let Some(want) = shadow::Want::of(&record_value, mirror, scenarios)
                && let Ok(mut shadowed) = self.shadowed.lock()
            {
                shadowed.push(want);
            }
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
            // Markers of an earlier merge, still in the branch: the
            // worker has them to resolve.
            return gated("conflict", json!({ "note": note, "merge": "pending" }));
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
                json!({"note": main_merge::conflict_note(main, &merge), "merge": merge.kind()}),
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
        // A candidate equal to main in an objective mission is not judged
        // here: the gate's base check answers what it means before it builds
        // anything (a scenario that passes on the base is the objective
        // solved there, not a failed task; a scenario that fails there is a
        // rejection like any other). Mission 4's task 182 was rejected three
        // times for "the work changes nothing on main" when the owner batch
        // had already landed the behaviour, and the third halted the mission.
        if tree_of(&candidate)? == tree_of(main)? && task.objective.is_none() {
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
                    short_of_room(mission, task.id, &short, &log, database_now(store)?),
                );
            }
            Err(RoomFailure::Bad(reason)) => {
                return gated("aborted", json!({ "reason": reason }));
            }
        }
        let mut gate_env = self.judging_env(scenarios, jobs);
        // Main of the source is red on a check: the gate runs it on the base
        // before the candidate, so a main that is green on it again says so
        // (`base_check`) and one still red stops there (`main_red`).
        gate_env.retain(|(name, _)| name != "SPAWN_GATE_BASE_CHECK");
        if let Some(red) = &mission.red {
            gate_env.push(("SPAWN_GATE_BASE_CHECK".to_string(), red.scenario.clone()));
        }
        let gate_path = pinned(&self.gate);
        // The head's re-judgings ([`rejection`]) run on an older base, where
        // the red check says nothing about main: they are not given it.
        let head_env: Vec<(String, String)> = gate_env
            .iter()
            .filter(|(name, _)| name != "SPAWN_GATE_BASE_CHECK")
            .cloned()
            .collect();
        let run_gate = |base: &str, judged: &str| {
            gate::judge(
                &gate_path,
                mirror,
                base,
                judged,
                scenarios,
                scenario.as_deref(),
                &head_env,
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
        let given = inputs::read(main, &candidate, scenarios, scenario.as_deref());
        bound.replace(Some((given.clone(), Value::Null)));
        let (judged, base_check) = gate::judge_seeing_base_check(
            &gate_path,
            mirror,
            main,
            &candidate,
            scenarios,
            scenario.as_deref(),
            &gate_env,
            gate_wall_clock(),
        );
        let ended = now_ms();
        bound.replace(Some((given, inputs::env_here(gate_log(&judged)))));
        if let Ok(verdict) = &judged {
            flakes.replace(verdict.flakes.clone());
            red_scenarios.replace(verdict.main_red_scenarios.clone());
        }
        // The red check passed on this base before the candidate ran: main
        // of the source is green on it again, whatever the gate ended with
        // after (a verdict, a host fault on another check, no room, an
        // abort). Recorded first, so a red the same gate found on another
        // check is the source's next red, not undone by this green.
        if let (Some(red), Some(passed)) = (&mission.red, base_check.as_deref())
            && passed == red.scenario
        {
            match store.integrator_record(&json!({
                "type": "source.main_green", "source": source, "main": main,
                "scenario": passed, "task": task.id, "mission": mission.id,
            })) {
                Ok(_) => println!(
                    "integrate: main of {source} ({}) passes {passed} again: green",
                    main.get(..12).unwrap_or(main)
                ),
                Err(error) => eprintln!("integrate: main of {source} green again: {error}"),
            }
        }
        match judged {
            Ok(verdict) if verdict.accepted => {
                // The verdict is stored before git moves, so a crash lands
                // on the next pass instead of judging again.
                gated(
                    "accepted",
                    json!({"candidate": candidate, "log": verdict.output, "metric": verdict.metric}),
                )?;
                let landed_commit = candidate.clone();
                let landing = Task {
                    state: TaskState::Landing,
                    base: Some(main.to_string()),
                    candidate: Some(candidate),
                    ..task.clone()
                };
                // On a held source this records its `task.held`: it waits.
                if let Landing::Landed = land(store, mirror, source, &landing)? {
                    // The next gates on this source judge against the new
                    // main: the main thread warms its base ([`warm`]).
                    if let Ok(mut landed) = self.landed.lock() {
                        landed.push(warm::Want {
                            source: source.to_string(),
                            commit: landed_commit,
                            scenarios: pinned(scenarios),
                        });
                    }
                }
                Ok(())
            }
            // The objective's scenario passes on the base: a fact about the
            // objective, not about the task's work. The objective is
            // recorded solved there and the task ends without a rejection.
            Ok(verdict)
                if verdict.reason.trim() == SOLVED_ON_BASE
                    && task.objective.is_some_and(|objective| {
                        crate::objectives::one_shot(mission, objective)
                    }) =>
            {
                solved_on_base(
                    store,
                    mission,
                    task,
                    scenario.as_deref(),
                    main,
                    &verdict.output,
                )
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
                    Rejection::HeadNoRoom { short, log, head } => {
                        let mut extra =
                            no_room(mission, task.id, &short, &log, database_now(store)?);
                        extra = on_the_head(extra, &head, main, &verdict.reason);
                        gated("no_room", extra)
                    }
                    Rejection::HeadBaseDisagrees {
                        scenario,
                        setup_s,
                        log,
                        head,
                    } => {
                        let faults = mission.gate_host_faults.saturating_add(1);
                        let disagreements = mission.gate_base_disagrees.saturating_add(1);
                        let backoff = host_fault_backoff(self.host.backoff, faults);
                        let mut extra = base_disagrees_fields(
                            &scenario,
                            [1, 0],
                            setup_s,
                            faults,
                            disagreements,
                            backoff,
                            &log,
                        );
                        extra = on_the_head(extra, &head, main, &verdict.reason);
                        gated("host_fault", extra)
                    }
                    Rejection::HeadHostFault { failure, head } => {
                        let mut extra = self.gate_host_fault(mission, task.id, &failure);
                        extra = on_the_head(extra, &head, main, &verdict.reason);
                        gated("host_fault", extra)
                    }
                    Rejection::HeadCannotJudge {
                        scenario,
                        log,
                        head,
                    } => {
                        let mut extra = self.cannot_judge(mission, task.id, &scenario, &log);
                        extra = on_the_head(extra, &head, main, &verdict.reason);
                        gated("host_fault", extra)
                    }
                }
            }
            // No room to judge: a fact about the host, never a count.
            Err(GateFailure::NoRoom { short, log }) => gated(
                "no_room",
                no_room(mission, task.id, &short, &log, database_now(store)?),
            ),
            // The base disagreed with itself: a scenario of it exited 1 and
            // then 0, before any candidate code ran. The host was too loaded
            // or too flaky to judge: `host_fault` with that evidence, never
            // a count toward a halt, judged again after the run's backoff.
            Err(GateFailure::BaseDisagrees {
                scenario,
                setup_s,
                log,
            }) => {
                let faults = mission.gate_host_faults.saturating_add(1);
                let disagreements = mission.gate_base_disagrees.saturating_add(1);
                let backoff = host_fault_backoff(self.host.backoff, faults);
                let mut extra = base_disagrees_fields(
                    &scenario,
                    [1, 0],
                    setup_s,
                    faults,
                    disagreements,
                    backoff,
                    &log,
                );
                extra["candidate"] = json!(candidate);
                println!(
                    "integrate: task {}: {}",
                    task.id,
                    extra["reason"].as_str().unwrap_or_default()
                );
                gated("host_fault", extra)
            }
            // Main is red on a check: it failed twice on the base with the
            // same failing tests. Believed only when a failure is the work's
            // and the test server's log shows no crash in the gate's window:
            // then `source.main_red` (one per source, the owner alerted once)
            // and the task's `host_fault` with evidence `main_red`, which
            // starts no run, no backoff and no count. Else it is the host's,
            // as a check failing on the candidate and on the base.
            Err(GateFailure::MainRed {
                scenario,
                runs,
                failures,
                log,
            }) => {
                let classes: Vec<host::Class> = failures
                    .iter()
                    .map(|failure| host::classify(&failure.message))
                    .collect();
                let crash = if test_database {
                    self.crash_in(started, ended)
                } else {
                    Vec::new()
                };
                if red_believed(&classes) && crash.is_empty() && !matches!(main_now(), Ok(Some(_)))
                {
                    let listed: Vec<Value> = failures
                        .iter()
                        .zip(&classes)
                        .map(|(failure, class)| {
                            json!({"test": failure.test, "message": crate::deliver::one_line(&failure.message, 200),
                                   "class": class.name()})
                        })
                        .collect();
                    store
                        .integrator_record(&json!({
                            "type": "source.main_red", "source": source, "main": main,
                            "scenario": scenario, "runs": runs, "failures": listed,
                            "task": task.id, "mission": mission.id,
                        }))
                        .map_err(|error| error.to_string())?;
                    let mut extra =
                        main_red_fields(mission, source, main, &scenario, &runs, &listed, &log);
                    extra["candidate"] = json!(candidate);
                    println!(
                        "integrate: task {}: {}",
                        task.id,
                        extra["reason"].as_str().unwrap_or_default()
                    );
                    gated("host_fault", extra)
                } else {
                    let failure = red_as_host_fault(scenario, runs, &crash, log);
                    let mut extra = self.gate_host_fault(mission, task.id, &failure);
                    extra["candidate"] = json!(candidate);
                    gated("host_fault", extra)
                }
            }
            // The gate found a host or harness fault, not a verdict: a
            // scenario's harness error on the base, or a check failing on
            // the candidate and the base. `host_fault`: retried after a
            // backoff, never counted toward a halt.
            Err(failure @ GateFailure::HostFault { .. }) => {
                let mut extra = self.gate_host_fault(mission, task.id, &failure);
                extra["candidate"] = json!(candidate);
                gated("host_fault", extra)
            }
            // The host cannot judge this objective's scenario at all: the
            // objective is judged outside the gate, by the owner.
            // `host_fault`: retried after a backoff, never counted toward a
            // halt, never a rejection and never an abort.
            Err(GateFailure::CannotJudge { scenario, log }) => {
                let mut extra = self.cannot_judge(mission, task.id, &scenario, &log);
                extra["candidate"] = json!(candidate);
                gated("host_fault", extra)
            }
            Err(failure) => gated("aborted", aborted(&failure)),
        }
    }
}

impl Judge {
    /// The record of a host fault the gate itself reported
    /// ([`GateFailure::HostFault`]) for `task`: the mission's next in a row,
    /// with its backoff, said on stdout as the other host faults are.
    fn gate_host_fault(&self, mission: &Mission, task: i64, failure: &GateFailure) -> Value {
        let faults = mission.gate_host_faults.saturating_add(1);
        let backoff = host_fault_backoff(self.host.backoff, faults);
        let extra = gate_host_fault_fields(failure, faults, backoff);
        println!(
            "integrate: task {task}: {}",
            extra["reason"].as_str().unwrap_or_default()
        );
        extra
    }

    /// The record of a scenario the host cannot judge
    /// ([`GateFailure::CannotJudge`]) for `task`: the mission's next host
    /// fault in a row, with its backoff, said on stdout as the others are.
    fn cannot_judge(&self, mission: &Mission, task: i64, scenario: &str, log: &str) -> Value {
        let faults = mission.gate_host_faults.saturating_add(1);
        let backoff = host_fault_backoff(self.host.backoff, faults);
        let extra = cannot_judge_fields(scenario, faults, backoff, log);
        println!(
            "integrate: task {task}: {}",
            extra["reason"].as_str().unwrap_or_default()
        );
        extra
    }

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
        Ok(Some(verdict)) => println!("integrate: task {} gated: {verdict}", task.id),
        Ok(None) => {}
        Err(error) => eprintln!(
            "integrate: mission {mission}: task {}: record the abort: {error}",
            task.id
        ),
    }
}

/// What a gate run printed, whatever it ended with; None for a gate that
/// never ran (it could not start, or ran past its wall clock).
fn gate_log(judged: &Result<gate::Verdict, GateFailure>) -> Option<&str> {
    match judged {
        Ok(verdict) => Some(&verdict.output),
        Err(GateFailure::Timeout | GateFailure::Failed(_)) => None,
        Err(
            GateFailure::Aborted { log, .. }
            | GateFailure::NoRoom { log, .. }
            | GateFailure::HostFault { log, .. }
            | GateFailure::MainRed { log, .. }
            | GateFailure::CannotJudge { log, .. }
            | GateFailure::BaseDisagrees { log, .. },
        ) => Some(log),
    }
}

/// Record `task.gated aborted` for a judging that ended in an error, when
/// the log shows it recorded nothing: the task is still in the state and at
/// the revision it was judged from. `stale` instead when main moved since
/// (`main` is no longer main). The verdict it recorded, if any.
fn abort_unrecorded(
    store: &PostgresStore,
    task: &Task,
    main: &str,
    reason: &str,
) -> Result<Option<&'static str>, String> {
    let missions: Vec<Mission> =
        serde_json::from_value(store.gated_missions().map_err(|error| error.to_string())?)
            .map_err(|error| format!("gated missions: {error}"))?;
    let unrecorded = missions
        .iter()
        .flat_map(|mission| &mission.tasks)
        .any(|now| now.id == task.id && now.state == task.state && now.revisions == task.revisions);
    if !unrecorded {
        return Ok(None);
    }
    // Main moved while it ran: no abort of a judging on a main that is no
    // longer main, counted toward the halt; `stale`, judged again.
    let source = missions
        .iter()
        .find(|mission| mission.tasks.iter().any(|now| now.id == task.id))
        .and_then(|mission| mission.source.clone());
    let mut aborted = json!({"type": "task.gated", "id": task.id, "verdict": "aborted",
                             "revision": task.revisions, "base": main, "reason": reason});
    match source.map(|source| main_moved(&source, main)) {
        Some(Ok(Some(now))) => {
            let mut stale = stale_on_main_move(main, &now, "aborted", reason);
            stale["id"] = json!(task.id);
            stale["revision"] = json!(task.revisions);
            record(store, &stale)?;
            return Ok(Some("stale"));
        }
        Some(Err(error)) => {
            eprintln!("integrate: task {}: where main is now: {error}", task.id);
            aborted["main_read"] = json!(error);
        }
        Some(Ok(None)) | None => {}
    }
    record(store, &aborted)?;
    Ok(Some("aborted"))
}

/// The main of `source` now, when it is no longer `main` (the base a gate
/// judged): one `ls-remote`, as the landing reads it ([`source_head`]).
/// `None` when main is still `main`. The read's error when it fails: then
/// the verdict is recorded as it was found, as before this check, with the
/// error as its `main_read` (main was never checked). A source that cannot
/// be read leaves no task unrecorded, and is no reason to judge again.
fn main_moved(source: &str, main: &str) -> Result<Option<String>, String> {
    let (_, now) = source_head(source)?;
    Ok((now != main).then_some(now))
}

/// The `stale` record of a gate whose base `main` is no longer main (`now`):
/// what it `found` (the verdict it would have been) and that verdict's
/// reason, never counted. Supply PA's mission 6 (2026-11-09): a gate started
/// on ab21a25, main moved to 513a34e at 09:00:35, and at 09:20:54 its host
/// fault was capped as the third abort in a row and halted the mission.
fn stale_on_main_move(main: &str, now: &str, found: &str, reason: &str) -> Value {
    let said = if reason.is_empty() {
        String::new()
    } else {
        format!(" ({})", crate::deliver::one_line(reason, 300))
    };
    json!({"type": "task.gated", "verdict": "stale", "base": main, "main_now": now,
    "found": found,
    "reason": format!(
        "main moved from {} to {} while the gate ran: its {found}{said} is no verdict on \
         the new main; judged again",
        short_commit(main),
        short_commit(now)
    )})
}

/// A commit's first 12 characters, as the integrator's messages name them.
fn short_commit(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
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
    /// The head's own run found the host short of room: `no_room`, as any,
    /// with `head` (the fields naming the head and the rejected merge)
    /// added.
    HeadNoRoom {
        short: Shortfall,
        log: String,
        head: Value,
    },
    /// The head's own run found the base disagreeing with itself:
    /// `host_fault` with that evidence, as any, with `head` (the fields
    /// naming the head and the rejected merge) added.
    HeadBaseDisagrees {
        scenario: String,
        setup_s: Option<u64>,
        log: String,
        head: Value,
    },
    /// The head's own run found a host fault (a scenario's harness error on
    /// the base, a check failing on both trees): `host_fault`, as any, with
    /// `head` added.
    HeadHostFault { failure: GateFailure, head: Value },
    /// The head's own run cannot judge the objective here: `host_fault` with
    /// that evidence, as any, with `head` added.
    HeadCannotJudge {
        scenario: String,
        log: String,
        head: Value,
    },
}

/// The record of a judging of the head alone ([`rejection`]) that gave no
/// verdict (no room, a base that disagreed with itself, a host fault, a
/// scenario the host cannot judge): `extra` with the head's fields
/// (`candidate`, `head`, `head_base`) and a reason that says what it covers.
/// Its log and fact are the head's run on `head_base`, the base the work was
/// built on, and never the merge's on `main`: Supply PA's task 207
/// (2026-11-09) recorded the head's run on a red head_base as "check-tests
/// failed on the candidate and on the base" under main's commit, and read as
/// main failing.
fn on_the_head(mut extra: Value, head: &Value, main: &str, merged: &str) -> Value {
    let short = |commit: &str| commit.get(..12).unwrap_or(commit).to_string();
    let head_base = head["head_base"]
        .as_str()
        .map_or_else(|| "its base".to_string(), short);
    let said = extra["reason"].as_str().unwrap_or_default().to_string();
    extra["reason"] = json!(format!(
        "the merge with main {} was rejected ({merged}); judging the work alone against the \
         base it was built on, {head_base}, gave no verdict: {said}",
        short(main)
    ));
    if let (Some(target), Some(head)) = (extra.as_object_mut(), head.as_object()) {
        target.extend(head.clone());
    }
    extra
}

/// The merge's rejection, standing: the head alone, judged against
/// `head_base` (the base it was built on), failed `scenario` there on the head
/// and on that base too (`runs`), so that base cannot judge it and the
/// gate's verdict on the merge with main is the one there is. `rejected`, with
/// the merge's reason, log and note, `head_verdict` `unjudged` and a
/// `head_reason` that says why; the note ends by saying it.
fn unjudged_head(
    fields: Value,
    named: &Value,
    head_base: &str,
    scenario: &str,
    runs: &str,
) -> Rejection {
    let short = head_base.get(..12).unwrap_or(head_base);
    let why = format!(
        "the work alone could not be judged against the base it was built on, {short}: \
         {scenario} fails there too ({runs})"
    );
    let mut record = fields;
    if let (Some(target), Some(named)) = (record.as_object_mut(), named.as_object()) {
        target.extend(named.clone());
    }
    let note = record["note"].as_str().unwrap_or_default().to_string();
    record["note"] = json!(format!(
        "{note}\n\nThe work alone could not be judged against the base it was built on \
         ({short}): {scenario} fails there too, so the gate's rejection of the merge with main \
         stands. Fix what fails on the merge."
    ));
    record["head_verdict"] = json!("unjudged");
    record["head_reason"] = json!(why);
    Rejection::Record("rejected", record)
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
        Err(GateFailure::NoRoom { short, log }) => Rejection::HeadNoRoom {
            short,
            log,
            head: named,
        },
        Err(GateFailure::BaseDisagrees {
            scenario,
            setup_s,
            log,
        }) => Rejection::HeadBaseDisagrees {
            scenario,
            setup_s,
            log,
            head: named,
        },
        // The head alone fails a check on its own base too (`candidate and
        // base`, or red there): the base it was built on cannot judge it, so
        // the merge's rejection stands, as the gate gave it on main, with a
        // word that the head could not be judged alone. Never a host fault:
        // retried, it would find that base failing every time (Supply PA's
        // task 207, 2026-11-09, on the red main fcf5e1e).
        Err(GateFailure::HostFault {
            scenario, on, runs, ..
        }) if on == "candidate and base" => {
            unjudged_head(fields, &named, &head_base, &scenario, &runs)
        }
        Err(GateFailure::MainRed { scenario, runs, .. }) => {
            unjudged_head(fields, &named, &head_base, &scenario, &runs)
        }
        Err(failure @ GateFailure::HostFault { .. }) => Rejection::HeadHostFault {
            failure,
            head: named,
        },
        Err(GateFailure::CannotJudge { scenario, log }) => Rejection::HeadCannotJudge {
            scenario,
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
        GateFailure::NoRoom { short, log } => {
            json!({"reason": format!("the gate had no room: {}", short.line()), "log": log})
        }
        // Recorded as `host_fault` with evidence `base_disagrees` by the
        // caller; counted here only if a caller forgets that.
        GateFailure::BaseDisagrees { scenario, log, .. } => json!({
            "reason": format!(
                "host fault: the base disagrees with itself: {scenario} exited 1, then 0 on the \
                 base; not a verdict, judged again later"
            ),
            "evidence": BASE_DISAGREES,
            "scenario": scenario,
            "log": log,
        }),
        // Recorded as `host_fault` by the caller, likewise.
        GateFailure::HostFault {
            scenario, on, log, ..
        } => {
            json!({"reason": format!("the gate found {scenario} failing on the {on}"), "log": log})
        }
        // Recorded as `host_fault` with evidence `main_red` (or as the
        // host's) by the caller; counted here only if a caller forgets that.
        GateFailure::MainRed { scenario, log, .. } => json!({
            "reason": format!("the gate found main red on {scenario}"),
            "log": log,
        }),
        // Recorded as `host_fault` with evidence `cannot_judge` by the
        // caller; counted here only if a caller forgets that.
        GateFailure::CannotJudge { scenario, log } => json!({
            "reason": format!(
                "host fault: this host cannot judge {scenario}; the objective is judged outside \
                 the gate, by the owner; not a verdict on the work"
            ),
            "evidence": CANNOT_JUDGE,
            "scenario": scenario,
            "log": log,
        }),
    }
}

/// The gate's red-main fact taken as the host's: no failure was the work's,
/// or the test server's log shows a crash in the gate's window (`crash`), or
/// it was about another base than main. A check failing on the candidate and
/// on the base, as the gate says it without the red rule.
fn red_as_host_fault(scenario: String, runs: String, crash: &[String], log: String) -> GateFailure {
    let why = crash.first().map_or_else(
        || {
            "not every shared failure is the work's (a connection or database error is the host's)"
                .to_string()
        },
        |line| format!("the test database's log shows a crash during the gate ({line})"),
    );
    // The base check (SPAWN_GATE_BASE_CHECK) runs before the candidate: no
    // candidate ran, so the fault is the base's alone.
    let (failed, on) = if before_the_candidate(&runs) {
        ("twice on the base, before the candidate", "base")
    } else {
        (
            "twice on the candidate and twice on the base",
            "candidate and base",
        )
    };
    GateFailure::HostFault {
        reason: format!(
            "{scenario} failed {failed} with the same tests, but {why}: a host or harness \
             fault, not a verdict on the candidate"
        ),
        scenario,
        on: on.to_string(),
        runs,
        tree: None,
        log,
    }
}

/// Whether a red's shared failures are main's own: at least one is the
/// work's (an assertion, a panic), and none is a connection or database
/// error, which a broken test server gives main and candidate alike
/// (design-main-red.md; §10.3).
fn red_believed(classes: &[host::Class]) -> bool {
    classes.contains(&host::Class::Work)
        && !classes
            .iter()
            .any(|class| matches!(class, host::Class::Connection | host::Class::Database))
}

/// Whether the gate's `runs` are its base check's, run before the
/// candidate (gate.sh's "…, then … on the base, before the candidate").
fn before_the_candidate(runs: &str) -> bool {
    runs.ends_with("on the base, before the candidate")
}

/// The task's record of a gate that found main red on `scenario`
/// (`host_fault`, evidence [`MAIN_RED`]): `fact` with `on` `main`, the
/// source, its main and the shared failures; no backoff of its own
/// (`backoff_ms` 0: the source's red holds the gates instead) and no
/// escalation (the owner is told once, of the source's `source.main_red`).
/// The mission's run of host faults is untouched (the store's fold).
pub(crate) fn main_red_fields(
    mission: &Mission,
    source: &str,
    main: &str,
    scenario: &str,
    runs: &str,
    failures: &[Value],
    log: &str,
) -> Value {
    json!({
        "reason": format!(
            "main of {source} ({}) is red on {scenario}: it failed twice on the base ({runs}) \
             with the same failing tests; not a verdict on the work, judged again when main \
             moves",
            main.get(..12).unwrap_or(main)
        ),
        "log": log,
        "evidence": MAIN_RED,
        "fact": {"scenario": scenario, "on": "main", "runs": runs, "source": source,
                 "main": main, "failures": failures},
        "host_faults": mission.gate_host_faults,
        "backoff_ms": 0,
        "escalate": false,
    })
}

fn now_ms() -> i64 {
    spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0
}

/// The database's clock now (`spawn_now_ms()`): what a no-room record's
/// escalation compares with `gate_room_since_ms`, the store's own time of
/// the run's first record, after a gate that took minutes (the mission's
/// `now_ms` was read before it).
fn database_now(store: &PostgresStore) -> Result<i64, String> {
    store.database_now_ms().map_err(|error| error.to_string())
}

/// Whether `task`'s gate waits this pass, with its kind and the line that
/// says why: the gate had no room last time (it waits out the backoff, then
/// tries), or its last run on it was a host fault (it waits until the
/// recorded due time, then judges it again). The mission's other tasks wait
/// behind it. [`Integrator::backs_off`] says the line once.
fn gate_wait(mission: &Mission, task: &Task) -> Option<(Wait, String)> {
    // One clock, the database's: the due times are its own. A mission read
    // without it (never from the store) waits for nothing.
    let now = mission.now_ms?;
    if let Some(wait) = room_wait(mission, task.id, now) {
        return Some((
            Wait::Room,
            format!(
                "integrate: mission {}: the gate had no room; task {} is judged again in {}s",
                mission.id,
                task.id,
                wait.as_secs()
            ),
        ));
    }
    host_wait(mission, task.id, now).map(|wait| {
        (
            Wait::HostFault,
            format!(
                "integrate: mission {}: the gate's last run on task {} was a host fault ({} in \
                 a row); it is judged again in {}s",
                mission.id,
                task.id,
                mission.gate_host_faults,
                wait.as_millis().div_ceil(1000)
            ),
        )
    })
}

/// Why a task waits, for saying each wait once ([`Integrator::waiting`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Wait {
    /// The gate had no room last time: its backoff.
    Room,
    /// The gate's last run on it was a host fault: until the due time.
    HostFault,
    /// Main of its source is red.
    Red,
    /// Every gate slot is taken.
    Gate,
    /// The CPU has no room for its gate.
    Cpu,
}

/// Whether a wait was said this pass or before, for [`Integrator::backs_off`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Said {
    /// It starts waiting: the line was printed.
    Now,
    /// It went on waiting: said when it started.
    Before,
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
    // and so does an owner's hold (an owner's landing takes the lock on a
    // connection of its own).
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

/// What a mission's step reads of its source's main.
enum Reading {
    /// Main, from a fetch that succeeded, where the last landing left it (or
    /// the mission has no baseline yet): the mission may be judged against it.
    Judge(String),
    /// No reading this pass: the fetch failed, or main is not at the
    /// baseline while an owner's landing on the source is pending.
    Nothing,
    /// Main moved with no record that explains it: the mission was halted.
    Halted,
}

/// Fetch `source`'s main into `mirror` and check it against `mission`'s
/// baseline. A fetch that failed leaves the mirror's last copy of main,
/// which is no reading of the source's main: no halt, no verdict by it. A
/// main not at the baseline halts the mission, unless an owner's landing on
/// the source is pending (`pending`): it may have moved main before its record
/// (an integrator stopped in between), so main says nothing until it is
/// applied or refused, and nothing is judged against it.
fn read_main(
    store: &PostgresStore,
    mission: &Mission,
    source: &str,
    mirror: &Path,
    pending: &std::collections::HashMap<String, i64>,
) -> Result<Reading, String> {
    let main = match sources::refresh(mirror, source).and_then(|()| {
        git(&[
            "--git-dir",
            &mirror.to_string_lossy(),
            "rev-parse",
            "refs/heads/main",
        ])
    }) {
        Ok(main) => main,
        Err(error) => {
            eprintln!("integrate: mission {}: {error}", mission.id);
            return Ok(Reading::Nothing);
        }
    };
    let Some(baseline) = mission
        .baseline
        .as_deref()
        .filter(|baseline| *baseline != main)
    else {
        return Ok(Reading::Judge(main));
    };
    if let Some(request) = pending.get(source) {
        println!(
            "integrate: mission {}: the source's main is {main}, its baseline {baseline}; no \
             halt while the owner's landing (request {request}) is pending",
            mission.id
        );
        return Ok(Reading::Nothing);
    }
    // The owner holds the source: the hold says the owner is moving its
    // main, so a main away from the baseline is what the hold is for, not
    // something no record explains. Nothing is judged against it until the
    // hold ends (and `spawn mission moved` takes the new main as the
    // baseline).
    if let Some(hold) = mission.hold {
        println!(
            "integrate: mission {}: the source's main is {main}, its baseline {baseline}; no \
             halt while the owner holds the source (hold {hold})",
            mission.id
        );
        return Ok(Reading::Nothing);
    }
    halt_moved(store, mission, source, baseline, &main)?;
    Ok(Reading::Halted)
}

/// Halt `mission` because its source's main moved with no record that
/// explains it: the record names the source and both commits, so `spawn
/// mission moved` can tell this halt from any other and clear it when the
/// owner records the move (`spawn_source_moved`).
fn halt_moved(
    store: &PostgresStore,
    mission: &Mission,
    source: &str,
    from: &str,
    to: &str,
) -> Result<(), String> {
    let reason = format!("the source's main is {to}, but the last landing left {from}");
    record(
        store,
        &json!({"type": "mission.halted", "id": mission.id, "reason": reason,
                "cause": "main_moved", "source": source, "from": from, "to": to}),
    )?;
    println!("integrate: mission {} HALTED: {reason}", mission.id);
    Ok(())
}

/// The gate's verdict when the objective's scenario passes at the base
/// commit (`scripts/rsi/gate.sh`'s base check, before any candidate build).
pub(crate) const SOLVED_ON_BASE: &str = "scenario already passes on the base";

/// Record that `mission`'s objective is solved on the base and end `task`
/// with it: the gate found its scenario passing at `base`, so no work of
/// the task could change anything (mission 4's task 182, objective 34, whose
/// owner batch had landed the behaviour first). The objective is a landing
/// for the assignment rule (`OBJECTIVE_LANDINGS` reads the record), and the
/// task is cancelled on that record — no rejection, no failed attempt, no
/// halt (`spawn_integrator_record` accepts this cancel and no other without
/// a yield).
fn solved_on_base(
    store: &PostgresStore,
    mission: &Mission,
    task: &Task,
    scenario: Option<&Path>,
    base: &str,
    log: &str,
) -> Result<(), String> {
    let objective = task
        .objective
        .ok_or("an objective task without its objective")?;
    let scenario = scenario
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let solved = store
        .integrator_record(&json!({"type": "objective.solved", "id": mission.id,
                                   "objective": objective, "task": task.id,
                                   "scenario": scenario, "base": base, "log": log}))
        .map_err(|error| error.to_string())?;
    // The Stop first, as for a yield: if the cancel then fails, the next
    // pass finds the task again and records both again.
    stop_worker(store, task, "integrate-solved")?;
    let reason = format!(
        "objective {objective} is solved on the base: {scenario} passes at {base}, so task \
         #{} ends without a rejection",
        task.id
    );
    store
        .integrator_record(
            &json!({"type": "task.cancelled", "id": task.id, "solved": solved,
                                   "reason": reason}),
        )
        .map_err(|error| error.to_string())?;
    println!("integrate: mission {}: {reason}", mission.id);
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

pub(crate) fn record(store: &PostgresStore, value: &Value) -> Result<(), String> {
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
    stop_worker(store, task, "integrate-abort")
}

/// Submit the Stop of `task`'s worker, keyed `<key>-<task>` so a retried
/// pass submits it once (an abort over budget, a yield of its slot).
pub(crate) fn stop_worker(store: &PostgresStore, task: &Task, key: &str) -> Result<(), String> {
    let error = |error: &dyn std::fmt::Display| format!("stop task {}'s worker: {error}", task.id);
    let submission = Submission {
        agent_id: AgentId::new(task.agent_id.clone()).map_err(|e| error(&e))?,
        session_id: SessionId::new(task.session_id.clone()).map_err(|e| error(&e))?,
        idempotency_key: IdempotencyKey::new(format!("{key}-{}", task.id))
            .map_err(|e| error(&e))?,
        action: CommandAction::Stop,
    };
    let principal = Principal::new(PRINCIPAL).map_err(|e| error(&e))?;
    store
        .submit(&principal, &submission)
        .map(|_| ())
        .map_err(|e| error(&e))
}

/// The work an overrun task keeps on its branch, once it reached the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kept {
    /// The source's `spawn/<agent>` holds the checkpoint: the branch, the
    /// commit and `git diff --stat main <commit>`.
    Work {
        branch: String,
        commit: String,
        diff: String,
    },
    /// The push has not happened yet: the next pass looks again.
    Waiting,
    /// The work never reached the branch (no checkpoint, no source, or the
    /// push failed): the branch alone is named, and the next task is told so.
    Lost { branch: String },
}

/// How long a Stop's final checkpoint may take to reach the source before
/// the work counts as lost (the pusher pushes within its interval, and
/// retries a failed push): `SPAWN_KEPT_WAIT_SECS`, a whole number of
/// seconds from 1 up; anything else is 30.
pub(crate) fn kept_wait_ms(text: Option<&str>) -> i64 {
    const DEFAULT: i64 = 30;
    text.and_then(|text| text.trim().parse::<i64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT)
        .saturating_mul(1_000)
}

/// Whether the source's `spawn/<agent>` holds `commit` (its ancestry), read
/// from the source itself, never assumed.
fn source_holds(source: &str, agent: &str, commit: &str) -> bool {
    sources::git(&[
        "--git-dir",
        source,
        "merge-base",
        "--is-ancestor",
        commit,
        &format!("refs/heads/spawn/{agent}"),
    ])
    .is_ok()
}

/// `git diff --stat main <commit>` in the source, the work's shape for the
/// task that continues it.
fn diff_stat(source: &str, commit: &str) -> String {
    sources::git(&["--git-dir", source, "diff", "--stat", "main", commit])
        .map(|text| text.trim_end().to_string())
        .unwrap_or_default()
}

/// The work `task` keeps on its branch: [`Kept`].
fn kept_work(store: &PostgresStore, mission: &Mission, task: &Task) -> Result<Kept, String> {
    let branch = format!("spawn/{}", task.agent_id);
    let Some(source) = mission.source.as_deref() else {
        return Ok(Kept::Lost { branch });
    };
    let wait_ms = kept_wait_ms(crate::config::env_value("SPAWN_KEPT_WAIT_SECS").as_deref());
    let Some(checkpoint) = store
        .checkpoint_at(&task.agent_id)
        .map_err(|error| error.to_string())?
    else {
        return Ok(Kept::Lost { branch });
    };
    let commit = checkpoint["commit"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let at_ms = checkpoint["at_ms"].as_i64().unwrap_or_default();
    if commit.is_empty() {
        return Ok(Kept::Lost { branch });
    }
    if source_holds(source, &task.agent_id, &commit) {
        return Ok(Kept::Work {
            diff: diff_stat(source, &commit),
            branch,
            commit,
        });
    }
    let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
    if now - at_ms >= wait_ms {
        return Ok(Kept::Lost { branch });
    }
    Ok(Kept::Waiting)
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
        BUDGET_GAP, Freshness, Mission, NO_TEST_DATABASE, aborted, active_ms, budget_gap,
        freshness, gates_at_once, halt_reason, markers_note, merged_tree, needs_test_database,
        no_room, over_budget, pinned, report_freshness, room_backoff, room_wait, short_of_room,
        task_scenario, waits_for_gate, wall_clock_minutes, withheld_without_test_database,
    };
    use crate::gate::{GateFailure, Shortfall};
    use serde_json::json;

    /// SPAWN_INTEGRATE_DRAIN_SECS: unset is two hours; a whole number of
    /// seconds from 1 to a day is taken; anything else refuses to start,
    /// naming the variable.
    #[test]
    fn the_drain_bound_is_a_whole_number_of_seconds_up_to_a_day() {
        assert_eq!(super::drain_secs(None), Ok(7200));
        assert_eq!(super::drain_secs(Some("3")), Ok(3));
        assert_eq!(super::drain_secs(Some(" 600 ")), Ok(600));
        assert_eq!(super::drain_secs(Some("86400")), Ok(86_400));
        for bad in [
            "0",
            "86401",
            "x",
            "1.5",
            "-5",
            "+5",
            "",
            " ",
            "1e3",
            "99999999999999999999",
        ] {
            let error = super::drain_secs(Some(bad)).expect_err(bad);
            assert!(error.contains("SPAWN_INTEGRATE_DRAIN_SECS"), "{error}");
        }
    }

    /// The integrator's pid file is its pid and start time, nothing else.
    #[test]
    fn a_pid_file_names_a_pid_and_its_start_time() {
        assert_eq!(super::read_pid_file("4242 991\n"), Some((4242, 991)));
        assert_eq!(super::read_pid_file("4242\n"), None);
        assert_eq!(super::read_pid_file("4242 991 7\n"), None);
        assert_eq!(super::read_pid_file("x 991\n"), None);
        assert_eq!(super::read_pid_file(""), None);
    }

    /// `spawn integrate --drain` finds the integrator by its pid file: this
    /// process runs, a start time that is not its own (a pid used again)
    /// is gone, and a root without the file has no integrator.
    #[test]
    fn a_drain_request_needs_the_running_integrator_of_its_root() {
        let pid = std::process::id();
        let ticks = super::warm::start_ticks(pid).expect("start time");
        assert!(super::still_runs(pid, ticks));
        assert!(!super::still_runs(pid, ticks + 1));
        let root = std::env::temp_dir().join(format!("spawn-drain-root-{pid}"));
        let _ = std::fs::remove_dir_all(&root);
        let error = super::request_drain(&root).expect_err("no pid file");
        assert!(error.contains("no integrator runs"), "{error}");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join(super::PID_FILE), format!("{pid} {}\n", ticks + 1))
            .expect("pid file");
        let error = super::request_drain(&root).expect_err("gone");
        assert!(error.contains("is gone"), "{error}");
        super::write_pid_file(&root);
        assert_eq!(
            super::read_pid_file(
                &std::fs::read_to_string(root.join(super::PID_FILE)).expect("written")
            ),
            Some((pid, ticks))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn disk_short(free_kb: u64, min_gb: u64, tmpdir: &str) -> Shortfall {
        Shortfall::Disk {
            free_kb,
            min_gb,
            tmpdir: tmpdir.to_string(),
        }
    }

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

    /// A request names its commit by its full hash only (SHA-1 or SHA-256),
    /// never by an option or a revision expression.
    #[test]
    fn an_owner_request_names_its_commit_by_full_hash() {
        use super::owner::{full_hash, owner_ref};
        assert!(full_hash(&"a1".repeat(20)));
        assert!(full_hash(&"0f".repeat(32)));
        for bad in [
            "HEAD",
            "--all",
            "main",
            &"A1".repeat(20),
            &"a1".repeat(19),
            "",
            &"g".repeat(40),
        ] {
            assert!(!full_hash(bad), "{bad}");
        }
        assert_eq!(owner_ref("abc"), "refs/spawn-owner/abc");
    }

    /// The merge that lands the owner's commit says it is the owner's
    /// landing, names the request and the commit's subject (bounded), and
    /// carries the request and the commit as trailers.
    #[test]
    fn an_owner_landing_merge_names_its_request_and_the_commits_subject() {
        let message = super::owner::owner_message(42, "c0ffee", "ba5e", "owner: the contract\n");
        assert!(
            message.starts_with("Owner landing (request 42): owner: the contract\n\n"),
            "{message}"
        );
        assert!(
            message.contains("c0ffee") && message.contains("ba5e"),
            "{message}"
        );
        assert!(
            message.ends_with("\n\nSpawn-Owner-Request: 42\nSpawn-Owner-Commit: c0ffee\n"),
            "{message}"
        );
        let long = super::owner::owner_message(1, "c", "m", &"x".repeat(500));
        let first = long.lines().next().expect("a first line");
        assert!(first.chars().count() < 140, "{first}");
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
        let mut one = mission(3, 0, 0);
        one.gate_rejection_objective = Some(19);
        assert!(
            halt_reason(&one, 7).is_some_and(|r| r.contains("rejections")),
            "no objectives to yield: it halts"
        );
        one.objectives = Some(json!({"backlog": "/b", "resident": "r"}));
        assert_eq!(halt_reason(&one, 7), None, "objective 19 yields instead");
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
            &disk_short(3 * 1_048_576 + 104_858, 10, "/data/tmp"),
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
            no_room(mission, 4, &disk_short(1, 10, "/t"), "", minutes * MINUTE)["escalate"].clone()
        };
        assert_eq!(
            no_room(&waiting, 5, &disk_short(1, 10, "/t"), "", 500 * MINUTE)["escalate"],
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
        let disk = no_room(
            &waiting,
            4,
            &disk_short(3 * 1_048_576, 10, "/data/tmp"),
            "log",
            0,
        );
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

        // A scenario run that gave up waiting for a host scenario slot is
        // the host's capacity too: the wait and the budget, the gate's own
        // fact, and no disk or database fields.
        let slot = short_of_room(
            &waiting,
            4,
            &Shortfall::Slot {
                waited_ms: 1_800_000,
                budget: 2,
            },
            "waited 30.0 min for a host scenario slot (all 2 taken)\n",
            0,
        );
        assert_eq!(
            (&slot["check"], &slot["measured"], &slot["threshold"]),
            (&json!("host scenario slot"), &json!(1_800_000), &json!(2))
        );
        assert_eq!(
            slot["reason"],
            "no room to judge now: a scenario run waited 30.0 min for a host scenario slot \
             (SPAWN_GATE_HOST_JOBS=2 in all, shared by every gate on this host); not a verdict, \
             judged again later"
        );
        assert_eq!(
            slot["log"],
            "waited 30.0 min for a host scenario slot (all 2 taken)\n"
        );
        assert!(slot.get("free_kb").is_none() && slot.get("server").is_none());

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

    /// The gates that can run at once, which the default jobs divide the
    /// cores among: one per source, those on the shared test server within
    /// `--gates`.
    #[test]
    fn gates_at_once_are_one_per_source_within_the_shared_cap() {
        let of = |pairs: &[(&str, bool)], gates| {
            gates_at_once(
                pairs
                    .iter()
                    .map(|(source, shares)| ((*source).to_string(), *shares)),
                gates,
            )
        };
        // Spawn (shared server) and Supply PA (no test database, two missions).
        assert_eq!(
            of(&[("spawn", true), ("supply", false), ("supply", false)], 1),
            2
        );
        assert_eq!(of(&[("a", true), ("b", true)], 1), 1);
        assert_eq!(of(&[("a", true), ("b", true)], 2), 2);
        assert_eq!(
            of(&[("a", true), ("a", false)], 1),
            1,
            "one gate per source"
        );
        assert_eq!(of(&[], 1), 1);
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

    const MIN: i64 = 60_000;
    const GAP: i64 = 5 * MIN;

    /// A session event at `at_ms` (its id follows its time), with a tool or
    /// job id when it has one.
    fn event(id: i64, at_ms: i64, name: &str, key: Option<&str>) -> spawn_postgres::ActivityEvent {
        spawn_postgres::ActivityEvent {
            id,
            at_ms,
            name: name.to_string(),
            key: key.map(str::to_string),
        }
    }

    /// Steady work from `from_ms` to `to_ms`: a tool call every 30 s.
    fn steady(from_ms: i64, to_ms: i64, first_id: i64) -> Vec<spawn_postgres::ActivityEvent> {
        let mut events = Vec::new();
        let (mut at, mut id) = (from_ms, first_id);
        while at <= to_ms {
            let key = format!("t{id}");
            events.push(event(id, at, "tool.started", Some(&key)));
            events.push(event(id + 1, at + 5_000, "tool.completed", Some(&key)));
            at += 30_000;
            id += 2;
        }
        events
    }

    fn never_asked(_: i64, _: i64) -> Result<i64, String> {
        panic!("no gap inside an ended tool is longer than the cap")
    }

    /// 2026-11-07 23:05:46: the fleet stopped 17:55-23:05, and the
    /// integrator's first pass after the restart aborted task 159 (a
    /// 90-minute budget, 13 minutes of work before the stop) and task 113
    /// (240 minutes; it stopped inside a tool call that never ended). The
    /// wall clock (main's rule) aborts both; their active time aborts
    /// neither.
    #[test]
    fn a_fleet_stopped_for_five_hours_puts_no_task_over_its_budget() {
        let delivered = 1_794_044_765_209;
        let first_pass = 1_794_063_946_655;
        let mut events = steady(delivered + 10_000, delivered + 13 * MIN, 1);
        assert!(over_budget(&json!({"minutes": 90}), first_pass - delivered, 0).is_some());
        let active = active_ms(delivered, first_pass, &events, GAP, never_asked).expect("active");
        assert!(active < 13 * MIN + 2 * GAP, "{active}");
        assert_eq!(over_budget(&json!({"minutes": 90}), active, 0), None);

        // Task 113: a tool started before the stop never ends; the restart's
        // first event comes five hours later.
        let last = events.last().expect("events").clone();
        events.push(event(
            last.id + 1,
            last.at_ms + 1_000,
            "tool.started",
            Some("cargo"),
        ));
        events.push(event(
            last.id + 2,
            first_pass - 2_000,
            "command.accepted",
            None,
        ));
        let active = active_ms(delivered, first_pass, &events, GAP, never_asked).expect("active");
        assert!(active < 13 * MIN + 3 * GAP, "{active}");
        assert_eq!(over_budget(&json!({"minutes": 240}), active, 0), None);
    }

    /// No silence of any length adds more than the cap: an outage alone
    /// puts nothing over budget, at delivery, between events, or up to now.
    #[test]
    fn an_outage_of_any_length_adds_at_most_the_cap() {
        let day = 24 * 60 * MIN;
        assert_eq!(active_ms(0, 30 * day, &[], GAP, never_asked), Ok(GAP));
        let events = [
            event(1, 30 * day, "tool.started", Some("a")),
            event(2, 60 * day, "tool.completed", Some("b")),
        ];
        assert_eq!(
            active_ms(0, 90 * day, &events, GAP, never_asked),
            Ok(3 * GAP)
        );
        // A clock that steps back counts nothing.
        let back = [
            event(1, 10 * MIN, "tool.started", Some("a")),
            event(2, MIN, "turn.stopped", None),
        ];
        assert_eq!(active_ms(0, MIN, &back, GAP, never_asked), Ok(GAP));
    }

    /// A model API outage (rev-e on 988c85d): Claude Code retries behind a
    /// busy pane for 30 minutes and the turn fails; the dispatcher nudges
    /// three times an hour and each nudged turn fails too. The readings are
    /// not in `events` (the store leaves them out); the failed turns and the
    /// commands count nothing, so two hours of it add nothing, and the
    /// model's first progress afterwards adds one cap.
    #[test]
    fn a_model_api_outage_counts_nothing_but_one_cap() {
        let mut events = steady(0, 10 * MIN, 1);
        let mut id = 1_000;
        let mut at = 10 * MIN;
        let mut next = |name: &str, step: i64| {
            id += 1;
            at += step;
            event(id, at, name, None)
        };
        events.push(next("turn.failed", 30 * MIN));
        for _ in 0..6 {
            events.push(next("command.accepted", 20 * MIN));
            events.push(next("command.step", 1_000));
            events.push(next("command.finished", 1_000));
            events.push(next("turn.failed", 9 * MIN));
        }
        let outage_end = events.last().expect("events").at_ms;
        let before = active_ms(0, outage_end, &events, GAP, never_asked).expect("active");
        assert_eq!(before, 10 * MIN + 5_000, "the outage itself counts nothing");
        // A nudge's prompt arriving is no progress: the worker waited for it.
        events.push(next("message.received", 12 * MIN));
        let last = |events: &[spawn_postgres::ActivityEvent]| events.last().expect("events").at_ms;
        let prompted = active_ms(0, last(&events), &events, GAP, never_asked).expect("active");
        assert_eq!(prompted, before, "a prompt arriving counts nothing");
        events.push(next("tool.started", 2 * MIN));
        let after = active_ms(0, last(&events), &events, GAP, never_asked).expect("active");
        assert_eq!(
            after,
            before + 2 * MIN,
            "the model's first progress counts the gap before it, at most a cap"
        );
        assert!(over_budget(&json!({"minutes": 20}), after, 0).is_none());
    }

    /// The fleet's restart after a stop: the gap ends in the restart's
    /// command, which counts nothing, then work goes on and counts.
    #[test]
    fn a_restart_counts_nothing_for_the_stop() {
        let mut events = steady(0, 5 * MIN, 1);
        let stop = events.last().expect("events").at_ms;
        events.push(event(500, stop + 300 * MIN, "command.accepted", None));
        events.push(event(
            501,
            stop + 300 * MIN + 2_000,
            "session.native_started",
            None,
        ));
        events.extend(steady(stop + 301 * MIN, stop + 311 * MIN, 600));
        let active = active_ms(0, stop + 311 * MIN, &events, GAP, never_asked).expect("active");
        // Five minutes before, ten after, and the first tool call after the
        // engine's start: about a minute.
        assert!(active < 17 * MIN, "{active}");
        assert!(active > 15 * MIN, "{active}");
    }

    #[test]
    fn steady_work_runs_past_the_budget() {
        let events = steady(0, 100 * MIN, 1);
        let active = active_ms(0, 100 * MIN, &events, GAP, never_asked).expect("active");
        // Every gap is short: all of it counts (to its last event, 5 s past now).
        assert_eq!(active, 100 * MIN + 5_000);
        assert!(over_budget(&json!({"minutes": 90}), active, 0).is_some());
    }

    /// A tool that runs an hour without an event of its session (a build, a
    /// gate in the worker) counts while the fleet was alive, at most its
    /// gap; a host frozen meanwhile counts only the cap. A job counts the
    /// same way.
    #[test]
    fn a_long_tool_counts_while_the_fleet_was_alive() {
        for (start, end, key) in [
            ("tool.started", "tool.completed", "toolu_1"),
            ("tool.started", "tool.failed", "toolu_1"),
            ("job.started", "job.ended", "job_1"),
        ] {
            let events = [
                event(10, MIN, start, Some(key)),
                event(90, 61 * MIN, end, Some(key)),
            ];
            let mut asked = Vec::new();
            let alive = active_ms(0, 61 * MIN, &events, GAP, |from, to| {
                asked.push((from, to));
                Ok(60 * MIN)
            });
            assert_eq!(alive, Ok(61 * MIN), "{start}");
            assert_eq!(asked, [(10, 90)]);
            let frozen = active_ms(0, 61 * MIN, &events, GAP, |_, _| Ok(MIN));
            assert_eq!(frozen, Ok(MIN + GAP), "{start}");
            // More than the gap is never credited.
            let over = active_ms(0, 61 * MIN, &events, GAP, |_, _| Ok(500 * MIN));
            assert_eq!(over, Ok(61 * MIN), "{start}");
        }
        // A tool still running counts the cap until it ends, and the fleet's
        // answer is never needed for it.
        let running = [event(1, 0, "tool.started", Some("t"))];
        assert_eq!(active_ms(0, 60 * MIN, &running, GAP, never_asked), Ok(GAP));
        // An end with another id, or one without an id, covers nothing.
        let unpaired = [
            event(1, 0, "tool.started", Some("t")),
            event(2, 60 * MIN, "tool.completed", Some("u")),
            event(3, 120 * MIN, "tool.completed", None),
        ];
        assert_eq!(
            active_ms(0, 120 * MIN, &unpaired, GAP, never_asked),
            Ok(2 * GAP)
        );
        // The fleet's error is the pass's.
        let errs = active_ms(
            0,
            61 * MIN,
            &[
                event(1, 0, "tool.started", Some("t")),
                event(2, 60 * MIN, "tool.completed", Some("t")),
            ],
            GAP,
            |_, _| Err("down".to_string()),
        );
        assert_eq!(errs, Err("down".to_string()));
    }

    #[test]
    fn the_budget_gap_is_a_whole_number_of_seconds() {
        assert_eq!(budget_gap(None), BUDGET_GAP);
        assert_eq!(budget_gap(Some(" 90 ")), std::time::Duration::from_secs(90));
        for bad in ["", "0", "-5", "1.5", "2m", "ten"] {
            assert_eq!(budget_gap(Some(bad)), BUDGET_GAP, "{bad:?}");
        }
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
mod main_red_tests {
    use super::host::{self, Class};
    use super::{GateFailure, before_the_candidate, red_as_host_fault, red_believed};

    const REFUSED: &str = "connect to the test database: error connecting to server: Connection \
                           refused (os error 111)";

    /// A red is main's own only when some shared failure is the work's and
    /// none is a connection or database error (§10.3 "Main is red"): a
    /// shared {assertion, connection refused} is the host's, as a broken
    /// test server fails main and candidate alike.
    #[test]
    fn a_red_with_a_connection_failure_among_its_shared_failures_is_the_hosts() {
        let assertion = host::classify("assertion `left == right` failed");
        assert_eq!(assertion, Class::Work);
        assert_eq!(host::classify(REFUSED), Class::Connection);
        assert!(red_believed(&[assertion]));
        assert!(red_believed(&[assertion, assertion]));
        assert!(
            !red_believed(&[assertion, host::classify(REFUSED)]),
            "mixed: the host's"
        );
        assert!(!red_believed(&[Class::Database, assertion]));
        assert!(!red_believed(&[Class::Connection]));
        assert!(!red_believed(&[]));
    }

    /// A red the gate's base check found (before the candidate) that is not
    /// believed is a fault of the base alone: `on` `base`, which the cap
    /// never counts, and no word of a candidate that never ran.
    #[test]
    fn a_base_check_red_not_believed_is_the_bases_host_fault() {
        let runs = "1, then 1 on the base, before the candidate";
        assert!(before_the_candidate(runs));
        let GateFailure::HostFault { on, reason, .. } =
            red_as_host_fault("check-tests.sh".into(), runs.into(), &[], String::new())
        else {
            panic!("a host fault");
        };
        assert_eq!(on, "base");
        assert!(
            reason.contains("twice on the base, before the candidate"),
            "{reason}"
        );
        assert!(!reason.contains("on the candidate and"), "{reason}");
        let runs = "1, then 1 on the candidate; 1, then 1 on the base";
        assert!(!before_the_candidate(runs));
        let GateFailure::HostFault { on, .. } =
            red_as_host_fault("check-tests.sh".into(), runs.into(), &[], String::new())
        else {
            panic!("a host fault");
        };
        assert_eq!(on, "candidate and base");
    }
}

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

    /// The wait follows the database's clock, the one that stamped the due
    /// time, whatever the host's says: a database clock before the due time
    /// holds the task even when the host's is long past it (the due time
    /// here is in 2001), and one past it lets the gate run even when the
    /// host's is before it (the due time here is in 2200).
    #[test]
    fn a_host_faults_wait_follows_the_databases_clock_not_the_hosts() {
        let task: super::Task = serde_json::from_value(json!({
            "id": 4, "agent_id": "a", "session_id": "s", "state": "reported",
        }))
        .expect("a task");
        let at = |due: i64, database_now: i64| {
            let mut mission = with_host_faults(1, Some(4), Some(due));
            mission.now_ms = Some(database_now);
            mission
        };
        let past = 1_000_000_000_000; // 2001-09-09, long before the host's now
        assert!(super::now_ms() > past + 60_000);
        assert!(
            super::gate_wait(&at(past, past - 60_000), &task).is_some(),
            "the database's clock is before it"
        );
        assert!(
            super::gate_wait(&at(past, past), &task).is_none(),
            "the database's clock is at it"
        );
        let future = 7_258_118_400_000; // 2200-01-01, after the host's now
        assert!(super::now_ms() < future);
        assert!(
            super::gate_wait(&at(future, future + 1), &task).is_none(),
            "the database's clock is past it"
        );
        assert!(super::gate_wait(&at(future, future - 1), &task).is_some());
        // A mission read without the database's clock (never from the store)
        // waits for nothing.
        assert!(super::gate_wait(&with_host_faults(1, Some(4), Some(future)), &task).is_none());
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

/// Objective 33: a base that disagrees with itself (a scenario of it exited
/// 1 and then 0, before any candidate code ran) is objective 020's
/// `host_fault` with the evidence `base_disagrees`. Its record, its share of
/// 020's run and backoff, its own count in a row and the alert at three, and
/// no halt.
#[cfg(test)]
mod base_disagrees_tests {
    use std::time::Duration;

    use serde_json::json;

    use super::{Mission, halt_reason, host_fault_backoff, host_fault_base, host_wait};

    fn fields(
        scenario: &str,
        setup_s: Option<u64>,
        faults: u32,
        disagreements: u32,
    ) -> serde_json::Value {
        super::base_disagrees_fields(
            scenario,
            [1, 0],
            setup_s,
            faults,
            disagreements,
            host_fault_backoff(host_fault_base(None), faults),
            "the gate's log",
        )
    }

    /// The record: the evidence, the scenario, both exits, the setup's
    /// seconds when the gate printed a timing line (left out when it did
    /// not), the shared count, its own count, its backoff and its alert.
    #[test]
    fn the_record_names_the_scenario_its_exits_and_the_setup() {
        let record = fields("011.sh", Some(244), 1, 1);
        assert_eq!(record["evidence"], "base_disagrees");
        assert_eq!(record["scenario"], "011.sh");
        assert_eq!(record["exits"], json!([1, 0]));
        assert_eq!(record["setup_s"], 244);
        assert_eq!(record["host_faults"], 1);
        assert_eq!(record["base_disagrees"], 1);
        assert_eq!(record["backoff_ms"], 300_000);
        assert_eq!(record["escalate"], false);
        assert_eq!(record["alert"], false);
        assert_eq!(record["log"], "the gate's log");
        assert!(record.get("due_ms").is_none(), "the store writes it");
        assert_eq!(
            record["reason"],
            "host fault: the base disagrees with itself: 011.sh exited 1, then 0 on the base, \
             after a setup of 244 s; not a verdict, judged again later"
        );

        // No timing line: the field is left out, not null.
        let record = fields("011.sh", None, 1, 1);
        assert!(record.get("setup_s").is_none(), "{record}");
        assert_eq!(
            record["reason"],
            "host fault: the base disagrees with itself: 011.sh exited 1, then 0 on the base; \
             not a verdict, judged again later"
        );

        // The record keeps no `candidate` of its own: the caller adds it.
        assert!(record.get("candidate").is_none());
    }

    /// A base disagreement never escalates, at any count: the record that
    /// makes the mission's run 3 carries the alert, and every other one
    /// (the later ones of the same run too) does not.
    #[test]
    fn a_base_disagrees_never_escalates_and_alerts_at_the_third() {
        let alerts: Vec<(bool, bool)> = (1..=6)
            .map(|disagreements| {
                let record = fields("011.sh", None, disagreements, disagreements);
                (
                    record["escalate"].as_bool().expect("escalate"),
                    record["alert"].as_bool().expect("alert"),
                )
            })
            .collect();
        assert_eq!(
            alerts,
            [
                (false, false),
                (false, false),
                (false, true),
                (false, false),
                (false, false),
                (false, false),
            ]
        );
        // 020's evidence still escalates at the second in a row.
        let fault = super::host::host_fault(
            super::host::CHECK_TESTS_REGRESSION,
            "| == regression: check-tests.sh must still pass\n\
             | FAILED api::tests::one: error connecting to server: Connection refused (os error \
             111)\n",
            Vec::new,
        )
        .expect("a host fault by its failures");
        let escalations: Vec<bool> = (1..=4)
            .map(|faults| {
                super::host_fault_fields(&fault, faults, Duration::from_secs(5), "")["escalate"]
                    .as_bool()
                    .expect("escalate")
            })
            .collect();
        assert_eq!(escalations, [false, true, false, false]);
    }

    /// Both evidences share 020's run and its backoff: five minutes,
    /// doubling with each host fault in a row whatever its evidence, at most
    /// an hour.
    #[test]
    fn both_evidences_share_the_run_and_its_backoff() {
        let base = host_fault_base(None);
        let minutes: Vec<u64> = (0..=7)
            .map(|faults| host_fault_backoff(base, faults).as_secs() / 60)
            .collect();
        assert_eq!(minutes, [0, 5, 10, 20, 40, 60, 60, 60]);
        assert_eq!(
            host_fault_backoff(Duration::from_secs(4), 3),
            Duration::from_secs(16),
            "the scenario's four seconds, doubled twice"
        );
        assert_eq!(
            host_fault_backoff(base, u32::MAX),
            Duration::from_hours(1),
            "no overflow"
        );
    }

    /// The hold until the recorded `due_ms` is 020's: a base disagreement
    /// folds into the same run (`gate_host_faults`, `gate_host_task`,
    /// `gate_host_due_ms`), so the task waits as behind any host fault.
    #[test]
    fn a_base_disagreement_holds_its_task_until_its_due_time() {
        let due = 10_000_000;
        let waiting: Mission = serde_json::from_value(json!({
            "id": 1, "gate": "/g", "accept": "gate",
            "gate_host_faults": 2, "gate_host_task": 4, "gate_host_due_ms": due,
            "gate_base_disagrees": 2,
        }))
        .expect("a mission");
        assert_eq!(
            host_wait(&waiting, 4, due - 8_000),
            Some(Duration::from_secs(8))
        );
        assert_eq!(host_wait(&waiting, 4, due), None, "due: judged again");
        assert_eq!(host_wait(&waiting, 5, 0), None, "another task's");
    }

    /// No halt rule counts a base disagreement, however many in a row.
    #[test]
    fn no_number_of_base_disagreements_halts_a_mission() {
        for count in [1, 2, 3, 10, 1000, u32::MAX] {
            let mission: Mission = serde_json::from_value(json!({
                "id": 1, "gate": "/g", "accept": "gate", "gate_rejections": 2,
                "gate_aborts": 2, "gate_host_faults": count, "gate_base_disagrees": count,
            }))
            .expect("a mission");
            assert_eq!(halt_reason(&mission, 7), None, "{count}");
        }
    }
}

/// A scenario the host cannot judge here (029's systemd user scopes): the
/// record says so, names the scenario, and is a host fault like any other.
#[cfg(test)]
mod cannot_judge_tests {
    use super::{
        CANNOT_JUDGE, cannot_judge_fields, halt_reason, host_fault_backoff, host_fault_base,
    };
    use serde_json::json;

    fn fields(faults: u32) -> serde_json::Value {
        cannot_judge_fields(
            "029.sh",
            faults,
            host_fault_backoff(host_fault_base(None), faults),
            "the gate's log",
        )
    }

    /// The record: the evidence, the scenario, the shared host-fault count,
    /// the backoff and escalation (the second in a row, as any host fault's).
    /// Never a rejection, never an abort.
    #[test]
    fn the_record_names_the_scenario_and_the_host_fault() {
        let record = fields(1);
        assert_eq!(record["evidence"], CANNOT_JUDGE);
        assert_eq!(record["evidence"], "cannot_judge");
        assert_eq!(record["scenario"], "029.sh");
        assert_eq!(record["host_faults"], 1);
        assert_eq!(record["backoff_ms"], 300_000);
        assert_eq!(record["escalate"], false);
        assert_eq!(record["log"], "the gate's log");
        assert!(record.get("due_ms").is_none(), "the store writes it");
        assert_eq!(
            record["reason"],
            "host fault: this host cannot judge 029.sh (the gate's log gives no cause); not a \
             verdict on the work, judged again in 5 min",
            "a log with no line of the scenario's names no cause"
        );
        assert_eq!(record["cause"], serde_json::Value::Null);
        assert!(record.get("candidate").is_none(), "the caller adds it");
        let second = fields(2);
        assert_eq!(second["escalate"], true, "the second in a row, as any");
        assert_eq!(second["backoff_ms"], 600_000);
    }

    /// The cause is the scenario's own line, quoted from its block in the
    /// gate's log: 029's systemd user manager, or Supply PA 073's
    /// PostgreSQL server — never one scenario's cause for another.
    #[test]
    fn the_reason_quotes_the_scenario_s_own_cannot_judge_line() {
        let line_073 = "073: cannot judge: SPAWN_TEST_DATABASE_URL names no PostgreSQL server, \
                        and the race needs two connections (the gate starts one from \
                        SPAWN_GATE_PG_BIN or SPAWN_GATE_PG_IMAGE)";
        let log = format!(
            "| server: none\n\
             | == candidate: 044.sh must pass\n\
             | 044: ok\n\
             | == candidate: 073.sh must pass\n\
             | 073: starting\n\
             | {line_073}\n\
             | == candidate: 075.sh must pass\n\
             | 075: cannot judge: something else entirely\n"
        );
        let record = cannot_judge_fields("073.sh", 1, std::time::Duration::from_mins(5), &log);
        assert_eq!(record["cause"], line_073);
        let reason = record["reason"].as_str().expect("a reason");
        assert!(reason.contains(&format!("it says: {line_073}")), "{reason}");
        assert!(!reason.contains("systemd"), "never 029's cause: {reason}");
        assert!(
            !reason.contains("something else"),
            "never another block's: {reason}"
        );

        // 029's own line, on the gate's stderr (the integrator's `! `).
        let log = "| == candidate: 029.sh must pass\n\
                   ! 029: cannot be judged here: no systemd user manager is reachable\n";
        let record = cannot_judge_fields("029.sh", 1, std::time::Duration::from_mins(5), log);
        assert_eq!(
            record["cause"],
            "029: cannot be judged here: no systemd user manager is reachable"
        );

        // The last line of the block wins; the log's `| ` is not quoted.
        assert_eq!(
            super::cannot_judge_cause(
                "073.sh",
                "| == base: 073.sh\n| 073: cannot judge: first\n| 073: cannot judge: last\n"
            )
            .as_deref(),
            Some("073: cannot judge: last")
        );
        // The gate's own stage line in the scenario's base block is no
        // cause: a candidate that exited 6 saying nothing has none.
        let log = "| == base: 073.sh\n\
                   | stage gate cannot judge 120ms base: 073.sh\n\
                   | == candidate: 073.sh must pass\n";
        let record = cannot_judge_fields("073.sh", 1, std::time::Duration::from_mins(5), log);
        assert_eq!(record["cause"], serde_json::Value::Null, "{record}");
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("the gate's log gives no cause")),
            "{record}"
        );
        // Another scenario's line is no cause of this one's.
        assert_eq!(
            super::cannot_judge_cause(
                "073.sh",
                "| == base: 029.sh\n| 029: cannot be judged here: x\n"
            ),
            None
        );
    }

    /// No halt rule counts it, however many in a row.
    #[test]
    fn no_number_of_cannot_judge_host_faults_halts_a_mission() {
        for count in [1, 2, 3, 10, 1000, u32::MAX] {
            let mission: super::Mission = serde_json::from_value(json!({
                "id": 1, "gate": "/g", "accept": "gate", "gate_rejections": 2,
                "gate_aborts": 2, "gate_host_faults": count,
            }))
            .expect("a mission");
            assert_eq!(halt_reason(&mission, 7), None, "{count}");
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

    /// The gate's environment names the shared test server (this test's
    /// own), so every gate that gets a test database shares it and
    /// `--gates` bounds them.
    fn on_the_shared_server(integrator: &mut Integrator) {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect("SPAWN_TEST_DATABASE_URL");
        integrator
            .judge
            .gate_env
            .push(("SPAWN_TEST_DATABASE_URL".to_string(), url));
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
        on_the_shared_server(&mut integrator);
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

    /// Gates with servers of their own run beside each other whatever the cap.
    #[test]
    fn gates_with_servers_of_their_own_are_not_held_by_the_cap() {
        // With SPAWN_GATE_PG_BIN every gate runs a PostgreSQL server of its
        // own, so `--gates` (a bound on the shared server) does not hold
        // them: two missions' gates run at once under a cap of one (Supply PA
        // and Spawn were serialized for no reason, 2026-11-09).
        let marks = fresh_marks("spawn-db-gate-own-servers");
        let _release = Release(marks.clone());
        let (base_a, source_a, scenarios, mut integrator) =
            world("own-servers-a", &holding_gate(&marks));
        integrator.gates = 1;
        on_the_shared_server(&mut integrator);
        integrator.judge.gate_env.push((
            "SPAWN_GATE_PG_BIN".to_string(),
            "/usr/lib/postgresql/16/bin".to_string(),
        ));
        let (base_b, source_b, _, _) = world("own-servers-b", "exit 1\n");
        let db = Db::new("own_servers");
        let a = db.gated_mission(&source_a, &scenarios);
        let b = db.gated_mission(&source_b, &scenarios);
        let a_one = db.reported_task(a, "a1");
        let b_one = db.reported_task(b, "b1");
        push_work(&base_a, &source_a, &agent_of(&db, a, a_one), "a1.txt");
        push_work(&base_b, &source_b, &agent_of(&db, b, b_one), "b1.txt");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.pass_on(&db.store).expect("another pass");
        assert_eq!(
            wait_started(&marks, 2).len(),
            2,
            "both missions' gates run at once"
        );
        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
    }

    /// A cap of one is one gate at a time across missions, as before.
    #[test]
    fn a_cap_of_one_gates_one_mission_at_a_time() {
        let marks = fresh_marks("spawn-db-gate-cap");
        let _release = Release(marks.clone());
        let (base_a, source_a, scenarios, mut integrator) =
            world("cap-one-a", &holding_gate(&marks));
        integrator.gates = 1;
        on_the_shared_server(&mut integrator);
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

    /// One event of `task`'s session at `at`, written straight into the log.
    fn session_event(db: &Db, task: &super::Task, at: i64, name: &str, detail: &Value) {
        db.admin()
            .execute(
                "insert into spawn_events (agent_id, session_id, source, kind, session_seq, recorded_at_ms)
                 values ($1, $2, 'test', jsonb_build_object('name', $3::text, 'detail', $4::jsonb),
                         (select coalesce(max(session_seq), 0) + 1 from spawn_events
                           where session_id = $2),
                         $5)",
                &[&task.agent_id, &task.session_id, &name, detail, &at],
            )
            .expect("write an event");
    }

    /// Tool calls every 30 s in `task`'s session from `from` to `to`, with
    /// the Bridge's screen readings between them.
    fn tool_work(db: &Db, task: &super::Task, from: i64, to: i64) {
        let mut at = from;
        let mut call = 0;
        while at < to {
            let id = json!({"tool_use_id": format!("toolu_{}_{call}", task.id)});
            session_event(db, task, at, "tool.started", &id);
            session_event(db, task, at + 10_000, "tool.completed", &id);
            session_event(db, task, at + 20_000, "screen.observed", &json!({}));
            at += 30_000;
            call += 1;
        }
    }

    /// The integrator's budget pass end to end, on the store: a task that
    /// worked 13 minutes, then sat through a five-hour stop of the fleet,
    /// is not aborted by the first pass after it (2026-11-07 23:05:46, task
    /// 159); one that worked past its budget is, with the worker stopped.
    #[test]
    fn the_budget_pass_aborts_an_overrun_and_not_a_stop() {
        let db = Db::new("budget_pass");
        let mission = db
            .store
            .mission_add("budget", &json!(["/nowhere.git"]), "human")
            .expect("add");
        db.store
            .mission_configure(
                mission,
                Some("/nowhere/scenarios"),
                "gate",
                Some(&json!({"minutes": 60})),
                None,
            )
            .expect("configure");
        let gated = || -> super::Mission {
            let missions: Vec<super::Mission> =
                serde_json::from_value(db.store.gated_missions().expect("gated"))
                    .expect("missions");
            missions
                .into_iter()
                .find(|row| row.id == mission)
                .expect("the mission")
        };
        let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
        let min = 60_000;

        let stopped = db
            .store
            .task_assign(mission, "ephemeral", "worker_stopped", "brief", "default")
            .expect("assign");
        db.store.task_delivered(stopped, 1).expect("delivered");
        let delivered = now - 5 * 60 * min - 13 * min;
        db.admin()
            .execute(
                "update mission_tasks set delivered_at_ms = $2 where id = $1",
                &[&stopped, &delivered],
            )
            .expect("deliver five hours ago");
        let task = ready_task(&db, stopped);
        tool_work(&db, &task, delivered + 5_000, delivered + 13 * min);
        // The fleet stopped with a tool open; the restart's commands.
        session_event(
            &db,
            &task,
            delivered + 13 * min,
            "tool.started",
            &json!({"tool_use_id": "toolu_open"}),
        );
        session_event(&db, &task, now - min, "command.accepted", &json!({}));
        assert_eq!(
            Integrator::enforce_budget(&db.store, &gated()).expect("the pass"),
            Vec::<i64>::new(),
            "five hours of wall clock, about 20 minutes of work"
        );
        assert_eq!(task_state(&db, mission, stopped), "delivered");

        let overran = db
            .store
            .task_assign(mission, "ephemeral", "worker_overran", "brief", "default")
            .expect("assign");
        db.store.task_delivered(overran, 1).expect("delivered");
        let delivered = now - 70 * min;
        db.admin()
            .execute(
                "update mission_tasks set delivered_at_ms = $2 where id = $1",
                &[&overran, &delivered],
            )
            .expect("deliver 70 minutes ago");
        tool_work(&db, &ready_task(&db, overran), delivered + 5_000, now);
        assert_eq!(
            Integrator::enforce_budget(&db.store, &gated()).expect("the pass"),
            vec![overran]
        );
        assert_eq!(
            task_state(&db, mission, stopped),
            "delivered",
            "the stop still counts once"
        );
        assert_eq!(task_state(&db, mission, overran), "cancelled");
        let stops: i64 = db
            .admin()
            .query_one(
                "select count(*) from spawn_commands where idempotency_key = $1",
                &[&format!("integrate-abort-{overran}")],
            )
            .expect("the stop")
            .get(0);
        assert_eq!(stops, 1, "the worker is stopped");
    }

    /// An objective mission on no real source: a backlog of `objectives`,
    /// an approved resident, `workers` slots and `budget`.
    fn backlog_mission(
        db: &Db,
        base: &Path,
        name: &str,
        objectives: &[u32],
        workers: u32,
        budget: Option<&Value>,
    ) -> i64 {
        let backlog = base.join("backlog");
        let scenarios = base.join("scenarios");
        std::fs::create_dir_all(&backlog).expect("backlog");
        std::fs::create_dir_all(&scenarios).expect("scenarios");
        for id in objectives {
            std::fs::write(
                backlog.join(format!("{id:03}.json")),
                json!({"id": id, "title": format!("Objective {id}"), "prompt": "Do it."})
                    .to_string(),
            )
            .expect("objective");
        }
        db.store
            .resident_propose(name, "Pursues objectives")
            .expect("propose");
        db.store.resident_approve(name).expect("approve");
        let mission = db
            .store
            .mission_add(
                name,
                &json!([base.join("source.git").to_string_lossy()]),
                "human",
            )
            .expect("add");
        db.store
            .mission_configure(
                mission,
                Some(&scenarios.to_string_lossy()),
                "gate",
                budget,
                Some(
                    &json!({"backlog": backlog.to_string_lossy(), "resident": name,
                             "workers": workers}),
                ),
            )
            .expect("configure");
        mission
    }

    /// The mission as the integrator reads it in a pass.
    fn snapshot(db: &Db, mission: i64) -> super::Mission {
        let missions: Vec<super::Mission> =
            serde_json::from_value(db.store.gated_missions().expect("gated")).expect("missions");
        missions
            .into_iter()
            .find(|row| row.id == mission)
            .expect("the mission")
    }

    /// `(task, objective)` of the mission's tasks in flight, by task.
    fn flying(db: &Db, mission: i64) -> Vec<(i64, u32)> {
        let mut tasks: Vec<(i64, u32)> = snapshot(db, mission)
            .tasks
            .iter()
            .filter_map(|task| Some((task.id, task.objective?)))
            .collect();
        tasks.sort_unstable();
        tasks
    }

    fn objective_task(db: &Db, mission: i64, objective: u32) -> super::Task {
        snapshot(db, mission)
            .tasks
            .into_iter()
            .find(|task| task.objective == Some(objective))
            .expect("the objective's task")
    }

    /// `task` delivered, reported, and sent back by `count` gate verdicts
    /// `verdict`, from revision `from` on: a rejection, or a conflict of a
    /// merge that left markers (main moved: not a failed attempt).
    fn sent_back(db: &Db, task: i64, verdict: &str, from: i32, count: i32) {
        for revision in from..from + count {
            db.store.task_delivered(task, 1).expect("delivered");
            assert!(
                db.store
                    .task_reported(task, "## Outcome\nhalf of it")
                    .expect("reported")
            );
            let record = if verdict == "conflict" {
                json!({"type": "task.gated", "id": task, "revision": revision,
                       "verdict": "conflict", "merge": "marked", "base": "b",
                       "reason": "main conflicts with your branch"})
            } else {
                json!({"type": "task.gated", "id": task, "revision": revision,
                       "verdict": verdict, "base": "b", "note": "a check failed"})
            };
            db.record(&record);
        }
    }

    /// A checkpoint of `task`'s session at `at`, written straight into the log.
    fn checkpoint(db: &Db, task: &super::Task, at: i64, commit: &str, reason: &str) {
        db.admin()
            .execute(
                "insert into spawn_events (agent_id, session_id, source, kind, session_seq,
                                           recorded_at_ms)
                 values ($1, $2, 'bridge',
                         jsonb_build_object('name', 'artifact.committed', 'detail',
                           jsonb_build_object('reason', $3::text, 'artifact',
                             jsonb_build_object('branch', $1::text, 'commit', $4::text,
                                                'project', 'missions'))),
                         (select coalesce(max(session_seq), 0) + 1 from spawn_events
                           where session_id = $2), $5)",
                &[&task.agent_id, &task.session_id, &reason, &commit, &at],
            )
            .expect("a checkpoint");
    }

    /// The integrator cancels only an objective's task being worked, to
    /// yield: not without `yielded`, not `reported` (an objective's task
    /// waiting for its gate), not a task of a mission without objectives.
    fn integrator_refuses_other_cancels(db: &Db, base: &Path, objective_task: i64) {
        let cancel = |task: i64, yielded: bool| {
            db.store
                .integrator_record(&json!({"type": "task.cancelled", "id": task,
                                           "yielded": yielded}))
        };
        assert!(cancel(objective_task, false).is_err(), "it does not yield");
        db.store
            .task_delivered(objective_task, 1)
            .expect("delivered");
        assert!(
            db.store
                .task_reported(objective_task, "again")
                .expect("reported")
        );
        assert!(
            cancel(objective_task, true).is_err(),
            "it waits for its gate"
        );
        let plain = db.gated_mission("/nowhere.git", &base.join("scenarios"));
        let plain_task = db
            .store
            .task_assign(plain, "ephemeral", "w", "brief", "default")
            .expect("assign");
        assert!(cancel(plain_task, true).is_err(), "no objective");
    }

    /// `task.cancelled` records by spawn-integrate for `tasks` alone, each
    /// with its worker stopped once.
    fn yields_recorded(db: &Db, tasks: &[i64]) {
        let mut admin = db.admin();
        let yielded: Vec<(i64, String)> = admin
            .query(
                "select (kind -> 'detail' -> 'record' ->> 'id')::bigint,
                        kind -> 'detail' -> 'record' ->> 'by'
                   from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.cancelled'
                  order by session_seq",
                &[],
            )
            .expect("the yields")
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        let expected: Vec<(i64, String)> = tasks
            .iter()
            .map(|task| (*task, "spawn-integrate".to_string()))
            .collect();
        assert_eq!(yielded, expected);
        let keys: Vec<String> = tasks
            .iter()
            .map(|task| format!("integrate-yield-{task}"))
            .collect();
        let stops: i64 = admin
            .query_one(
                "select count(*) from spawn_commands where idempotency_key = any($1)",
                &[&keys],
            )
            .expect("the stops")
            .get(0);
        assert_eq!(
            stops,
            i64::try_from(tasks.len()).expect("few"),
            "each worker stopped"
        );
    }

    /// Mission 4's stall (2026-11-07), on the store: three slots, two of
    /// them held by objectives at their limit of failed attempts, one by
    /// an objective under it. The pass yields the two (worker stopped,
    /// `task.cancelled` with `yielded` by spawn-integrate, the supervisor
    /// told once), leaves the third, and fills the slots fewest attempts
    /// first: 025, then 019 again, whose brief names the yielded task's
    /// branch and last checkpoint (not its set-aside). The store refuses
    /// any other cancel by the integrator.
    #[test]
    fn objectives_at_their_limit_yield_and_the_next_get_the_slots() {
        let base = std::env::temp_dir().join(format!("spawn-db-yield-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let db = Db::new("objective_yield");
        let mission = backlog_mission(&db, &base, "ry", &[19, 21, 23, 25], 3, None);
        let none = std::collections::BTreeSet::new();
        let pass = || {
            crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none)
                .expect("a pass")
        };
        pass();
        let first = flying(&db, mission);
        assert_eq!(
            first.iter().map(|(_, o)| *o).collect::<Vec<_>>(),
            [19, 21, 23]
        );
        let task_19 = objective_task(&db, mission, 19);
        let (t19, t21, t23) = (first[0].0, first[1].0, first[2].0);
        sent_back(&db, t19, "rejected", 0, 3);
        sent_back(&db, t21, "rejected", 0, 3);
        // 023: two rejections, then three conflicts (main moving), which do
        // not count: it stays under the limit.
        sent_back(&db, t23, "rejected", 0, 2);
        sent_back(&db, t23, "conflict", 2, 3);
        let assigned_at: i64 = db
            .admin()
            .query_one(
                "select created_at_ms from mission_tasks where id = $1",
                &[&t19],
            )
            .expect("the task")
            .get(0);
        let work = "1111111111111111111111111111111111111111";
        let before = "0000000000000000000000000000000000000000";
        checkpoint(&db, &task_19, assigned_at - 1, before, "tool_completed");
        checkpoint(&db, &task_19, assigned_at + 1, before, "tool_completed");
        checkpoint(&db, &task_19, assigned_at + 2, work, "tool_completed");
        integrator_refuses_other_cancels(&db, &base, t23);

        assert_eq!(pass().len(), 2);
        assert_eq!(task_state(&db, mission, t19), "cancelled");
        assert_eq!(task_state(&db, mission, t21), "cancelled");
        assert_eq!(
            task_state(&db, mission, t23),
            "reported",
            "under its limit, untouched"
        );
        let now_flying: Vec<u32> = flying(&db, mission)
            .iter()
            .filter(|(task, _)| *task != t19 && *task != t21)
            .map(|(_, objective)| *objective)
            .collect();
        assert_eq!(
            now_flying,
            [23, 25, 19],
            "by task: 023 kept, then 025, and 019 again"
        );
        yields_recorded(&db, &[t19, t21]);
        let told = db.store.objective_yields().expect("the yields to tell");
        assert_eq!(told.as_array().map(Vec::len), Some(2));

        let again = objective_task(&db, mission, 19).brief;
        for part in [
            "Their reports below say what each did and found".to_string(),
            format!("- task {t19}: yielded (objective 19 had 3 failed attempts"),
            format!("its last checkpoint {work}"),
            "> half of it".to_string(),
        ] {
            assert!(again.contains(&part), "{part}: {again}");
        }
        // The set-aside the dispatcher records after the cancel is not the work.
        checkpoint(&db, &task_19, assigned_at + 3, &"2".repeat(40), "set_aside");
        let prior = db
            .store
            .objective_prior_work(mission, 19, 3)
            .expect("prior work");
        assert_eq!(prior[0]["commit"], work);
        let fresh = objective_task(&db, mission, 25).brief;
        assert!(!fresh.contains("Earlier tasks"), "a first task has none");

        assert!(pass().is_empty(), "nothing more yields, no slot is free");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The run of gate rejections as the store folds it: its length and the
    /// objective all of it was (`gate_rejection_objective`), or none.
    fn rejection_run(db: &Db, mission: i64) -> (u32, Option<u32>) {
        let row = snapshot(db, mission);
        (row.gate_rejections, row.gate_rejection_objective)
    }

    /// The `task.cancelled` record of `task`, as the store keeps it.
    fn cancel_record(db: &Db, task: i64) -> Value {
        db.admin()
            .query_one(
                "select kind -> 'detail' -> 'record' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.cancelled'
                    and kind -> 'detail' -> 'record' ->> 'id' = $1::bigint::text",
                &[&task],
            )
            .expect("the cancel")
            .get(0)
    }

    /// Batch 11 (design-halt-per-objective.md), on the store: three gate
    /// rejections in a row, all objective 19's task, halt nothing. The pass
    /// yields 19 (worker stopped, `task.cancelled` with `yielded` and
    /// `rejections` 3, the reason saying why) and gives the slot to 21; the
    /// yield ends the run. 21's rejections then start a run of their own,
    /// and a `stale` verdict between two of them neither counts nor names
    /// another objective: the run is 2, still 21's.
    #[test]
    fn three_rejections_of_one_objective_yield_it_and_halt_nothing() {
        let base = std::env::temp_dir().join(format!("spawn-db-halt-obj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let db = Db::new("halt_per_objective");
        let mission = backlog_mission(&db, &base, "rh", &[19, 21], 1, None);
        let none = std::collections::BTreeSet::new();
        let pass = || {
            crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none)
                .expect("a pass")
        };
        pass();
        let t19 = objective_task(&db, mission, 19).id;
        sent_back(&db, t19, "rejected", 0, 2);
        assert_eq!(rejection_run(&db, mission), (2, Some(19)));
        sent_back(&db, t19, "rejected", 2, 1);
        assert_eq!(rejection_run(&db, mission), (3, Some(19)));
        assert_eq!(
            super::halt_reason(&snapshot(&db, mission), 0),
            None,
            "one objective's rejections: no halt"
        );

        assert_eq!(pass().len(), 1, "21 takes the slot");
        assert_eq!(task_state(&db, mission, t19), "cancelled");
        yields_recorded(&db, &[t19]);
        let cancel = cancel_record(&db, t19);
        assert_eq!(cancel["rejections"], 3, "{cancel}");
        assert_eq!(cancel["yielded"], true);
        assert_eq!(
            cancel["reason"].as_str(),
            Some(
                format!(
                    "3 gate rejections in a row, all objective 19's tasks: task #{t19} yields \
                     its slot to the next objective instead of halting the mission, its work \
                     set aside"
                )
                .as_str()
            )
        );
        assert_eq!(
            rejection_run(&db, mission),
            (0, None),
            "the yield ends the run"
        );
        assert_eq!(db.mission(mission)["halted"], Value::Null);

        let t21 = objective_task(&db, mission, 21).id;
        sent_back(&db, t21, "rejected", 0, 1);
        assert_eq!(
            rejection_run(&db, mission),
            (1, Some(21)),
            "a run of its own"
        );
        db.store.task_delivered(t21, 2).expect("delivered");
        assert!(db.store.task_reported(t21, "again").expect("reported"));
        db.record(&json!({"type": "task.gated", "id": t21, "revision": 1,
                          "verdict": "stale", "reason": "behind"}));
        assert_eq!(
            rejection_run(&db, mission),
            (1, Some(21)),
            "stale: counted nowhere"
        );
        db.record(&json!({"type": "task.gated", "id": t21, "revision": 1,
                          "verdict": "rejected", "base": "b", "note": "a check failed"}));
        assert_eq!(rejection_run(&db, mission), (2, Some(21)));
        assert_eq!(super::halt_reason(&snapshot(&db, mission), 0), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// rev-e on 67877e62, on the store: three rejections of objective 19,
    /// then the owner cancels 19's task directly (no yield). The cancel ends
    /// the run as a yield does, so the next rejection, of 21, is a run of
    /// its own (1, 21) and halts nothing: the run is never mixed with one
    /// whose objective is gone.
    #[test]
    fn an_owners_cancel_of_the_runs_objective_ends_the_run() {
        let base =
            std::env::temp_dir().join(format!("spawn-db-halt-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let db = Db::new("halt_owner_cancel");
        let mission = backlog_mission(&db, &base, "rc", &[19, 21], 2, None);
        let none = std::collections::BTreeSet::new();
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass");
        let (t19, t21) = (
            objective_task(&db, mission, 19).id,
            objective_task(&db, mission, 21).id,
        );
        sent_back(&db, t19, "rejected", 0, 3);
        assert_eq!(rejection_run(&db, mission), (3, Some(19)));
        db.store.task_cancel(t19).expect("the owner cancels");
        assert_eq!(task_state(&db, mission, t19), "cancelled");
        assert_eq!(
            rejection_run(&db, mission),
            (0, None),
            "the cancel ends the run"
        );
        sent_back(&db, t21, "rejected", 0, 1);
        assert_eq!(rejection_run(&db, mission), (1, Some(21)));
        assert_eq!(super::halt_reason(&snapshot(&db, mission), 0), None);
        assert_eq!(db.mission(mission)["halted"], Value::Null);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// On the store: rejections of 19, 21, 19 span two objectives, so the
    /// run names none and halts the mission as before; a yield of an
    /// objective that is not the run's leaves the run; an acceptance ends
    /// it; and a resume clears it.
    #[test]
    fn rejections_across_objectives_still_halt() {
        let base = std::env::temp_dir().join(format!("spawn-db-halt-mixed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let db = Db::new("halt_mixed");
        let mission = backlog_mission(&db, &base, "rm", &[19, 21], 2, None);
        let none = std::collections::BTreeSet::new();
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass");
        let (t19, t21) = (
            objective_task(&db, mission, 19).id,
            objective_task(&db, mission, 21).id,
        );
        sent_back(&db, t19, "rejected", 0, 1);
        assert_eq!(rejection_run(&db, mission), (1, Some(19)));
        sent_back(&db, t21, "rejected", 0, 1);
        assert_eq!(rejection_run(&db, mission), (2, None), "two objectives");
        sent_back(&db, t19, "rejected", 1, 1);
        assert_eq!(rejection_run(&db, mission), (3, None), "none stays none");
        let row = snapshot(&db, mission);
        assert!(
            super::halt_reason(&row, 0).is_some_and(|r| r == "3 gate rejections in a row"),
            "{:?}",
            super::halt_reason(&row, 0)
        );
        let attempts = std::collections::BTreeMap::new();
        assert!(
            crate::objectives::yields(&row, &attempts, 99, &none).is_empty(),
            "a mixed run yields nothing"
        );
        db.record(&json!({"type": "mission.halted", "id": mission,
                          "reason": "3 gate rejections in a row"}));
        db.store.mission_resume(mission).expect("resume");
        assert_eq!(rejection_run(&db, mission), (0, None), "resume clears it");

        // A run of 21's; 19 yields (an objective's own rule): the run stays.
        sent_back(&db, t21, "rejected", 1, 2);
        assert_eq!(rejection_run(&db, mission), (2, Some(21)));
        db.record(
            &json!({"type": "task.cancelled", "id": t19, "yielded": true,
                          "objective": 19, "failures": 2, "limit": 3, "reason": "a test"}),
        );
        assert_eq!(
            rejection_run(&db, mission),
            (2, Some(21)),
            "another objective's yield"
        );
        // An acceptance ends it.
        db.store.task_delivered(t21, 4).expect("delivered");
        assert!(db.store.task_reported(t21, "again").expect("reported"));
        db.record(&json!({"type": "task.gated", "id": t21, "revision": 3,
                          "verdict": "accepted", "base": "b"}));
        assert_eq!(rejection_run(&db, mission), (0, None));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// On the store: a mission without objectives (its tasks have none)
    /// halts on three rejections as before; the run names no objective.
    #[test]
    fn three_rejections_of_a_plain_mission_halt() {
        let (base, source, scenarios, _) = world("halt-plain", "exit 1\n");
        let db = Db::new("halt_plain");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        for revision in 0..3 {
            if revision > 0 {
                db.store
                    .task_delivered(task, revision + 1)
                    .expect("delivered");
                assert!(db.store.task_reported(task, "again").expect("reported"));
            }
            db.record(
                &json!({"type": "task.gated", "id": task, "revision": revision,
                              "verdict": "rejected", "base": "b0", "note": "fails"}),
            );
        }
        let after = gated(&db, mission);
        assert_eq!(
            (after.gate_rejections, after.gate_rejection_objective),
            (3, None)
        );
        assert!(super::halt_reason(&after, 0).is_some_and(|r| r.contains("rejections")));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// On the store: objective 023 has two failed attempts when its task
    /// goes over budget. An overrun is its own record, no abort and no
    /// yield: nothing is told to the supervisor as a yield, and the
    /// objective's next task continues the work instead (task 174, "A task
    /// over its budget keeps its work"). The record names the measure that
    /// made it, `active`.
    #[test]
    fn an_overrun_at_the_limit_is_no_yield() {
        let base =
            std::env::temp_dir().join(format!("spawn-db-overrun-yield-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let db = Db::new("overrun_yield");
        let mission = backlog_mission(
            &db,
            &base,
            "ra",
            &[23, 25],
            2,
            Some(&json!({"minutes": 60})),
        );
        let none = std::collections::BTreeSet::new();
        let pass = || {
            crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none)
                .expect("a pass")
        };
        pass();
        let first = objective_task(&db, mission, 23);
        let other = objective_task(&db, mission, 25);
        sent_back(&db, first.id, "rejected", 0, 2);
        let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
        let overrun = |task: &super::Task| {
            db.store.task_delivered(task.id, 1).expect("delivered");
            let delivered = now - 70 * 60_000;
            db.admin()
                .execute(
                    "update mission_tasks set delivered_at_ms = $2 where id = $1",
                    &[&task.id, &delivered],
                )
                .expect("deliver 70 minutes ago");
            tool_work(&db, task, delivered + 5_000, now);
        };
        overrun(&first);
        overrun(&other);
        let over =
            Integrator::enforce_budget(&db.store, &snapshot(&db, mission)).expect("the budget");
        assert_eq!(over, vec![first.id, other.id]);
        let record = |task: i64| -> Value {
            db.admin()
                .query_one(
                    "select kind -> 'detail' -> 'record' from spawn_events
                      where session_id = 'session_control'
                        and kind -> 'detail' -> 'record' ->> 'type' = 'task.over_budget'
                        and kind -> 'detail' -> 'record' ->> 'id' = $1",
                    &[&task.to_string()],
                )
                .expect("the overrun")
                .get(0)
        };
        // An overrun is its own record, at the attempt limit too: no abort,
        // no yield, no counter.
        for task in [first.id, other.id] {
            let overrun = record(task);
            assert!(overrun["tool_calls"].as_i64().unwrap_or_default() > 0);
            assert_eq!(overrun["budget"], json!({"minutes": 60}));
            assert_eq!(
                overrun["objective"].as_u64(),
                objective_of(&db, task).and_then(|objective| u64::try_from(objective).ok())
            );
        }
        let aborts: i64 = db
            .admin()
            .query_one(
                "select count(*) from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.aborted'",
                &[],
            )
            .expect("the aborts")
            .get(0);
        assert_eq!(aborts, 0, "an overrun is no abort");
        // The two rejections stand; the overrun adds no gate verdict.
        assert_eq!(
            gate_counters(&db, mission),
            (0, 2),
            "an overrun is no gate abort"
        );
        let told = db.store.objective_yields().expect("the yields to tell");
        assert!(
            told.as_array().expect("a list").is_empty(),
            "an overrun yields nothing"
        );

        // The next pass assigns 023's continuation of the overrun, and 025
        // gets its slot when one is free.
        pass();
        let mut flying: Vec<u32> = flying(&db, mission)
            .iter()
            .filter(|(task, _)| *task != first.id && *task != other.id)
            .map(|(_, objective)| *objective)
            .collect();
        flying.sort_unstable();
        assert_eq!(flying, [23, 25], "the continuation of the overrun, and 025");
        let again = objective_task(&db, mission, 23);
        assert_eq!(
            continues_of(&db, again.id),
            Some(first.id),
            "023's task continues the overrun"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The pass that aborts a task over budget does not re-issue its
    /// objective (task 113 aborted at 23:05:46, task 160 for objective 023
    /// at 23:05:51): its slot frees from the next pass on, and goes to the
    /// objective with the fewest attempts, 025, not 023.
    #[test]
    fn an_aborted_objective_is_not_reissued_at_once() {
        let base = std::env::temp_dir().join(format!("spawn-db-reissue-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let db = Db::new("objective_reissue");
        let mission = backlog_mission(
            &db,
            &base,
            "rr",
            &[23, 25],
            1,
            Some(&json!({"minutes": 60})),
        );
        let none = std::collections::BTreeSet::new();
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none)
            .expect("first fill");
        let task = objective_task(&db, mission, 23);
        db.store.task_delivered(task.id, 1).expect("delivered");
        let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
        let delivered = now - 70 * 60_000;
        db.admin()
            .execute(
                "update mission_tasks set delivered_at_ms = $2 where id = $1",
                &[&task.id, &delivered],
            )
            .expect("deliver 70 minutes ago");
        tool_work(&db, &task, delivered + 5_000, now);
        let pass = snapshot(&db, mission);
        let over = Integrator::enforce_budget(&db.store, &pass).expect("the budget");
        assert_eq!(over, vec![task.id]);
        // The overrun names the active-time measure, so it is the
        // objective's failed attempt (an abort without one, the old
        // budget's, is not).
        let raw = db.store.objective_outcomes(mission).expect("outcomes");
        assert_eq!(raw[0]["measure"], "active", "{raw}");
        let settled = over.into_iter().collect();
        assert!(
            crate::objectives::assign_next(&db.store, &pass, &settled)
                .expect("the same pass")
                .is_empty(),
            "nothing in the pass that overran"
        );
        // The next pass assigns the overrun's continuation, and it says so:
        // the work is continued, not redone.
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none)
            .expect("the next pass");
        let next: Vec<(i64, u32)> = flying(&db, mission)
            .into_iter()
            .filter(|(id, _)| *id != task.id)
            .collect();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].1, 23, "the overrun's objective, continued");
        let again = objective_task(&db, mission, 23);
        assert_eq!(continues_of(&db, again.id), Some(task.id));
        let brief = again.brief;
        assert!(
            brief.contains(&format!("continues task #{}", task.id)),
            "{brief}"
        );
        assert!(
            brief.contains(&format!("- task {}: cancelled", task.id)),
            "the overrun task is the work the brief says ended: {brief}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A mission's run of gate aborts and rejections.
    fn gate_counters(db: &Db, mission: i64) -> (i32, i32) {
        let row = db
            .admin()
            .query_one(
                "select gate_aborts, gate_rejections from mission_missions where id = $1",
                &[&mission],
            )
            .expect("the mission");
        (row.get(0), row.get(1))
    }

    /// The objective a task pursues, as the store folded it.
    fn objective_of(db: &Db, task: i64) -> Option<i64> {
        db.admin()
            .query_one(
                "select objective from mission_tasks where id = $1",
                &[&task],
            )
            .expect("the task")
            .get::<_, Option<i32>>(0)
            .map(i64::from)
    }

    /// The task a continuation continues, as the store folded it.
    fn continues_of(db: &Db, task: i64) -> Option<i64> {
        db.admin()
            .query_one(
                "select continues from mission_tasks where id = $1",
                &[&task],
            )
            .expect("the task")
            .get(0)
    }

    /// An overrun counts toward no halt: three in a row leave the mission's
    /// counters alone, and each task leaves flight.
    #[test]
    fn three_overruns_halt_nothing() {
        let db = Db::new("overrun_halt");
        let (base, source, scenarios, _integrator) = world("overrun-halt", "#!/bin/sh\nexit 0\n");
        let mission = db.gated_mission(&source, &scenarios);
        let mut tasks = Vec::new();
        for worker in ["a", "b", "c"] {
            let task = db
                .store
                .task_assign(mission, "ephemeral", worker, "brief", "default")
                .expect("assign");
            db.store.task_delivered(task, 1).expect("delivered");
            db.record(&json!({"type": "task.over_budget", "id": task,
                              "reason": "the task made more than 5 tool calls",
                              "tool_calls": 6, "elapsed_ms": 1_000,
                              "budget": {"tool_calls": 5}}));
            tasks.push(task);
        }
        let row = snapshot(&db, mission);
        assert_eq!((row.gate_aborts, row.gate_rejections), (0, 0));
        assert_eq!(
            super::halt_reason(&row, 0),
            None,
            "no halt counts an overrun"
        );
        for task in tasks {
            let state: String = db
                .admin()
                .query_one("select state from mission_tasks where id = $1", &[&task])
                .expect("the task")
                .get(0);
            assert_eq!(state, "cancelled", "an overrun leaves flight");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The work an overrun keeps: its branch's commit and that commit's
    /// diff stat against main; waited for while the push is pending, and
    /// lost (said so) when it never reaches the source.
    #[test]
    fn an_overrun_keeps_the_work_its_branch_holds() {
        let db = Db::new("overrun_kept");
        let (base, source, scenarios, _integrator) = world("overrun-kept", "#!/bin/sh\nexit 0\n");
        let mission = db.gated_mission(&source, &scenarios);
        let task = ready_task(&db, db.reported_task(mission, "kept"));
        let clone = base.join("kept-clone");
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", &source, &clone_path]).expect("clone");
        std::fs::write(clone.join("big.txt"), "line\n").expect("write");
        super::tests::commit_in(&clone_path, "the big work");
        let commit = git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("head");
        git(&[
            "-C",
            &clone_path,
            "push",
            "-q",
            "origin",
            &format!("{commit}:refs/heads/spawn/{}", task.agent_id),
        ])
        .expect("push the branch");
        let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
        checkpoint(&db, &task, now, &commit, "tool_completed");
        let row = snapshot(&db, mission);
        let kept = super::kept_work(&db.store, &row, &task).expect("the work");
        let super::Kept::Work {
            branch,
            commit: kept_commit,
            diff,
        } = kept
        else {
            panic!("the branch holds it: {kept:?}");
        };
        assert_eq!(branch, format!("spawn/{}", task.agent_id));
        assert_eq!(kept_commit, commit);
        assert!(
            diff.contains("big.txt") && diff.contains("insertion"),
            "{diff}"
        );
        // A checkpoint the pusher has not made it there yet: wait, until
        // the push has had its time, then say it was lost.
        let unpushed = "0123456789abcdef0123456789abcdef01234567";
        checkpoint(&db, &task, now, unpushed, "tool_completed");
        assert_eq!(
            super::kept_work(&db.store, &row, &task).expect("kept"),
            super::Kept::Waiting
        );
        checkpoint(&db, &task, now - 120_000, unpushed, "tool_completed");
        assert!(
            matches!(
                super::kept_work(&db.store, &row, &task).expect("kept"),
                super::Kept::Lost { .. }
            ),
            "a push that never came is lost"
        );
        let _ = std::fs::remove_dir_all(&base);
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
                super::room_wait(&after, task, after.now_ms.expect("the database's clock"))
                    .is_some(),
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

    /// The base warm after a landing (warm.rs): a task whose gate accepts
    /// lands in its judging's thread, and the pass after it starts one
    /// `gate.sh --base-only` of the new main, on the source's mirror, with
    /// the mission's scenarios — in the background, the landing never
    /// waiting for it.
    #[test]
    fn a_landing_starts_a_base_warm_of_the_new_main() {
        let dir = std::env::temp_dir().join(format!("spawn-db-warm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let args = dir.join("warm-args");
        let gate = format!(
            "if [ \"$1\" = --base-only ]; then printf '%s\\n' \"$@\" >'{}'; exit 0; fi\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
             echo '{{\"verdict\":\"accepted\"}}'\n",
            args.display()
        );
        let (base, source, scenarios, integrator) = world("warm", &gate);
        let integrator = integrator.with_warm(dir.join("gate-warm"));
        let db = Db::new("landing_warm");
        let mission = db.gated_mission(&source, &scenarios);
        db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        push_work(&base, &source, &agent, "work.txt");

        integrator.pass_on(&db.store).expect("the judging's pass");
        integrator.settle();
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
        let main = source_main(&source);
        // The landing was made in the judging's thread, after that pass
        // looked for warms: the next pass starts it.
        integrator.pass_on(&db.store).expect("the next pass");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !args.exists() {
            assert!(std::time::Instant::now() < deadline, "no warm started");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let said = std::fs::read_to_string(&args).expect("the warm's arguments");
        let words: Vec<&str> = said.lines().collect();
        assert_eq!(words.len(), 4, "{said}");
        assert_eq!(words[0], "--base-only");
        assert!(words[1].contains(".sources"), "the source's mirror: {said}");
        assert_eq!(words[2], main, "the new main");
        assert_eq!(
            Path::new(words[3]),
            super::pinned(&scenarios),
            "the mission's scenarios, as its gates read them"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&dir);
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

    /// The integrator-down alerts on this store, as `spawn alerts` has them:
    /// `(key, state)`, oldest first.
    fn integrator_alerts(db: &Db) -> Vec<(String, String)> {
        db.store
            .alerts(0)
            .expect("the alerts")
            .as_array()
            .expect("alerts")
            .iter()
            .filter(|row| row["kind"] == "integrator_down")
            .map(|row| {
                (
                    row["key"].as_str().unwrap_or_default().to_string(),
                    row["state"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    /// Batch 11 (3), on the store: a gated mission's reported task waits
    /// while no integrator holds its lock. The loop's alert step fires
    /// `integrator_down` once the lock has been free at every reading for
    /// the bound, once (not again while it is open), and clears it at the
    /// first reading that finds the lock held. A source an owner holds (a
    /// deploy restarting the integrator) and a halted mission are no work
    /// waiting: nothing fires for them, however long.
    #[test]
    fn the_integrator_down_alert_fires_once_and_clears_when_it_runs_again() {
        let (base, source, scenarios, _) = world("integrator-down", "exit 0\n");
        let db = Db::new("integrator_down");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let (mut settings, _) = crate::alerts::Settings::from_env();
        settings.check_ms = 1;
        settings.integrator_down_ms = 300;
        settings.memory_low_mb = 0;
        settings.tmpfs_high_mb = 0;
        settings.test_log = None;
        settings.live_log = None;
        let mut monitor = crate::alerts::Monitor::with(settings);
        let mut check = || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            monitor.check(&db.store).expect("a check");
        };
        let past_the_bound = || std::thread::sleep(std::time::Duration::from_millis(400));

        // An owner holds the source: no work waits, however long.
        db.admin()
            .execute(
                "insert into mission_source_holds (source, hold, main, by, reason, held_at_ms,
                                                   expires_ms)
                 values ($1, 1, $2, 'deploy', 'restart', spawn_now_ms(),
                         spawn_now_ms() + 600000)",
                &[&source, &"a".repeat(40)],
            )
            .expect("a hold");
        check();
        past_the_bound();
        check();
        assert_eq!(integrator_alerts(&db), [], "a held source");
        db.admin()
            .execute("delete from mission_source_holds", &[])
            .expect("released");
        // A halted mission: the same.
        db.record(&json!({"type": "mission.halted", "id": mission, "reason": "a test"}));
        check();
        past_the_bound();
        check();
        assert_eq!(integrator_alerts(&db), [], "a halted mission");
        db.store.mission_resume(mission).expect("resume");

        check();
        assert_eq!(integrator_alerts(&db), [], "not yet: the bound starts");
        past_the_bound();
        check();
        let fired = integrator_alerts(&db);
        assert_eq!(fired.len(), 1, "{fired:?}");
        assert_eq!(fired[0].1, "open");
        let row = db
            .store
            .alerts(0)
            .expect("the alerts")
            .as_array()
            .expect("alerts")
            .iter()
            .find(|row| row["kind"] == "integrator_down")
            .cloned()
            .expect("the alert");
        assert_eq!(row["subject"], "spawn-integrate");
        assert_eq!(
            row["evidence"]["waiting"],
            json!([{"mission": mission, "task": task}])
        );
        let text = row["text"].as_str().unwrap_or_default();
        assert!(
            text.starts_with("spawn-integrate is not running: no integrator has held its lock")
                && text.contains(&format!(
                    "1 reported task waits for a gate (task #{task} of mission #{mission})"
                )),
            "{text}"
        );
        past_the_bound();
        check();
        assert_eq!(integrator_alerts(&db), fired, "once while it is open");

        // It runs again: the lock is held, the alert clears.
        assert!(db.store.integrator_try_lock().expect("the lock"));
        check();
        let cleared = integrator_alerts(&db);
        assert_eq!(cleared, [(fired[0].0.clone(), "cleared".to_string())]);
        past_the_bound();
        check();
        assert_eq!(integrator_alerts(&db).len(), 1, "held: nothing fires");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Batch 11 (2), on the store: the host fault's backoff ("the gate's
    /// last run on task N was a host fault … judged again in Ns") is said
    /// once, when the task starts waiting, not on every pass it goes on
    /// waiting; once the task has had a gate, its next wait is said again.
    #[test]
    fn a_host_faults_backoff_is_said_once_per_wait() {
        let (base, source, scenarios, integrator) = world("host-fault-said", "exit 1\n");
        let db = Db::new("host_fault_said");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        db.record(&host_fault_record(task, 0, 1));
        let waiting = gated(&db, mission);
        let said = || integrator.backs_off(&waiting, &waiting.tasks[0]);
        assert!(
            super::gate_wait(&waiting, &waiting.tasks[0])
                .is_some_and(|(_, line)| line.contains("was a host fault (1 in a row)")),
            "it waits out the recorded due time"
        );
        assert_eq!(said(), Some(super::Said::Now), "it starts waiting: said");
        assert_eq!(said(), Some(super::Said::Before), "the next pass: quiet");
        assert_eq!(said(), Some(super::Said::Before));
        // Another task's wait is its own.
        assert_eq!(integrator.waiting.borrow().len(), 1);
        // It has its gate, which faults again: the new wait is said.
        integrator.wait_over(task);
        db.record(&host_fault_record(task, 0, 2));
        let again = gated(&db, mission);
        assert_eq!(
            integrator.backs_off(&again, &again.tasks[0]),
            Some(super::Said::Now)
        );
        assert_eq!(
            integrator.backs_off(&again, &again.tasks[0]),
            Some(super::Said::Before)
        );
        let _ = std::fs::remove_dir_all(&base);
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

    /// Objective 29, end to end: its task scenario exits 6 (the gate cannot
    /// judge it here: no systemd user manager in the sandbox) and the gate
    /// states its `cannot_judge` fact. The task is gated `host_fault` with
    /// the evidence `cannot_judge`: never rejected, never aborted, nothing
    /// back to the worker, not judged again before its `due_ms`, escalating
    /// at the second in a row as any host fault — and never capped into an
    /// abort, however many in a row (`SPAWN_GATE_HOST_FAULT_CAP` counts no
    /// `cannot_judge`).
    #[test]
    fn a_scenario_the_host_cannot_judge_is_a_host_fault_never_a_rejection() {
        let dir = std::env::temp_dir().join(format!("spawn-db-cannot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let runs = dir.join("runs");
        let runs_path = runs.display();
        let gate = format!(
            "n=$(cat '{runs_path}' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >'{runs_path}'\n\
             echo '| == candidate: 029.sh must pass'\n\
             echo '029: cannot be judged here: no systemd user manager is reachable' >&2\n\
             printf '{{\"cannot_judge\":true,\"scenario\":\"029.sh\",\"runs\":\"6\"}}\n' \
             >\"$SPAWN_GATE_VERDICT\"\n\
             exit 6\n"
        );
        let (base, source, scenarios, mut integrator) = world("cannot-judge", &gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_millis(1),
            cap: 2,
        };
        let db = Db::new("cannot_judge");
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

        pass();
        assert_eq!(count(), 1);
        let (record, _, at) = last_verdict(&db);
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["evidence"], "cannot_judge");
        assert_eq!(record["scenario"], "029.sh");
        assert_eq!(record["host_faults"], 1);
        assert_eq!(record["escalate"], false);
        assert_eq!(record["backoff_ms"], 1);
        assert_eq!(record["due_ms"], at + 1, "its own time plus the backoff");
        assert!(record["candidate"].is_string());
        assert!(
            record["reason"].as_str().is_some_and(|reason| {
                reason.starts_with("host fault: this host cannot judge 029.sh")
                    && reason.contains(
                        "it says: 029: cannot be judged here: no systemd user manager is reachable",
                    )
            }),
            "the scenario's own line, quoted: {record}"
        );
        assert_eq!(
            record["cause"],
            "029: cannot be judged here: no systemd user manager is reachable"
        );
        assert!(
            record["log"]
                .as_str()
                .is_some_and(|log| log.contains("029: cannot be judged here")),
            "the gate's log is kept"
        );
        let after = gated(&db, mission);
        assert_eq!(
            (after.gate_host_faults, after.gate_host_task),
            (1, Some(task))
        );
        assert_eq!((after.gate_rejections, after.gate_aborts), (0, 0));
        assert_eq!(
            (after.tasks[0].state, after.tasks[0].revisions),
            (spawn_core::mission::TaskState::Reported, 0),
            "nothing went back to the worker"
        );
        assert!(after.halted.is_none());

        // Judged again after its due time (this test's backoff is 1 ms; the
        // hold itself is every host fault's, tested above): the second in a
        // row escalates, as any.
        past_due(&db);
        pass();
        assert_eq!(count(), 2);
        let record = last_verdict(&db).0;
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["evidence"], "cannot_judge");
        assert_eq!(record["host_faults"], 2);
        assert_eq!(record["escalate"], true);

        // The cap (2 here) never reaches it: four faults in a row stay host
        // faults, and `gate_aborts` never moves.
        for _ in 0..2 {
            past_due(&db);
            pass();
        }
        assert_eq!(count(), 4);
        let record = last_verdict(&db).0;
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["host_faults"], 4);
        let after = gated(&db, mission);
        assert_eq!((after.gate_aborts, after.gate_rejections), (0, 0));
        assert!(after.halted.is_none(), "no halt counts a host fault");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Wait until the database's clock is past the latest record's `due_ms`:
    /// a pass judges a task again only then, and the tests' backoffs of a few
    /// milliseconds otherwise race a pass started at once after the last
    /// (one short in the count, now and then).
    fn past_due(db: &Db) {
        let due = last_verdict(db).0["due_ms"].as_i64().expect("a due time");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while db
            .admin()
            .query_one("select spawn_now_ms()", &[])
            .expect("the database's clock")
            .get::<_, i64>(0)
            <= due
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the clock never passed {due}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
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
            cap: super::HOST_FAULT_CAP,
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

    /// A sandbox that never started its command is recorded with its own
    /// evidence, `sandbox_never_started`, beside the gate's fact, so a
    /// record names the fault without its text being read.
    #[test]
    fn a_sandbox_that_never_started_has_its_own_evidence() {
        let failure = crate::gate::GateFailure::HostFault {
            scenario: "sh build".to_string(),
            on: "sandbox".to_string(),
            runs: "never started".to_string(),
            reason: "sandbox: sh build in /w/candidate never started (bwrap exited 1: bwrap: \
                     Can't chdir to /w/candidate: Permission denied)"
                .to_string(),
            tree: Some(crate::gate::SandboxTree::Candidate),
            log: String::new(),
        };
        let record = super::gate_host_fault_fields(&failure, 1, std::time::Duration::from_mins(1));
        assert_eq!(record["evidence"], crate::gate::SANDBOX_NEVER_STARTED);
        assert_eq!(record["fact"]["on"], "sandbox");
        assert_eq!(record["fact"]["tree"], "candidate");
        let reason = record["reason"].as_str().expect("a reason");
        assert!(
            reason.starts_with("host fault: the gate's sandbox never started sh build (sandbox: ")
                && reason.contains("Can't chdir"),
            "{reason}"
        );
    }

    /// The gate's own host faults (exit 6 with its fact, gate.rs
    /// `HOST_FAULT_EXIT`): a scenario's harness error on the base (012.sh
    /// exited 1, then 0: Supply PA's gate aborted twice on that, 2026-11-08)
    /// and a check failing on the candidate and on the base (task 127's
    /// e2e) are each `host_fault` by the gate's fact: `gate_aborts` does not
    /// move, the task stays reported and is held until its due time, the
    /// second in a row escalates with its backoff doubled, and the next run
    /// that accepts lands it.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn the_gates_own_host_faults_are_held_and_never_counted() {
        let dir = std::env::temp_dir().join(format!("spawn-db-gate-fault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let runs = dir.join("runs");
        let runs_path = runs.display();
        let gate = format!(
            "n=$(cat '{runs_path}' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >'{runs_path}'\n\
             if [ \"$n\" -eq 1 ]; then\n\
               echo '| == base: 012.sh'\n\
               echo '| 012.sh exited 1, then 0 on the base: a harness error, not a verdict on the candidate'\n\
               printf '{{\"host_fault\":true,\"scenario\":\"012.sh\",\"on\":\"base\",\"runs\":\"1, then 0\",\"reason\":\"012.sh exited 1, then 0 on the base\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               exit 6\n\
             elif [ \"$n\" -eq 2 ]; then\n\
               echo '| == regression: check-e2e.sh must still pass'\n\
               printf '{{\"host_fault\":true,\"scenario\":\"check-e2e.sh\",\"on\":\"candidate and base\",\"runs\":\"1, then 1 on the candidate; 1 on the base\",\"reason\":\"check-e2e.sh failed twice on the candidate and on the base\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               exit 6\n\
             fi\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n"
        );
        let (base, source, scenarios, mut integrator) = world("gate-fault", &gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_secs(2),
            cap: super::HOST_FAULT_CAP,
        };
        let db = Db::new("gate_host_fault");
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

        // A scenario's harness error on the base.
        pass();
        assert_eq!(count(), 1);
        let (record, event, at) = last_verdict(&db);
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["evidence"], "gate");
        assert_eq!(
            record["fact"],
            json!({"scenario": "012.sh", "on": "base", "runs": "1, then 0",
                   "reason": "012.sh exited 1, then 0 on the base"})
        );
        assert!(
            record["reason"].as_str().is_some_and(
                |reason| reason.starts_with("host fault: 012.sh exited 1, then 0 on the base")
            ),
            "{record}"
        );
        assert!(
            record["log"]
                .as_str()
                .is_some_and(|log| log.contains("| == base: 012.sh"))
        );
        assert_eq!(
            (
                &record["host_faults"],
                &record["escalate"],
                &record["backoff_ms"]
            ),
            (&json!(1), &json!(false), &json!(2000))
        );
        assert_eq!(record["due_ms"], at + 2000);
        assert!(record["candidate"].is_string());
        let after = gated(&db, mission);
        assert_eq!(
            (
                after.gate_host_faults,
                after.gate_host_task,
                after.gate_aborts,
                after.gate_rejections
            ),
            (1, Some(task), 0, 0),
            "a host fault, not an abort"
        );
        assert_eq!(
            (after.tasks[0].state, after.tasks[0].revisions),
            (spawn_core::mission::TaskState::Reported, 0)
        );
        pass();
        assert_eq!(count(), 1, "not judged again before due_ms");
        assert_eq!(last_verdict(&db).1, event);
        wait_until(at + 2000);

        // A check failing on the candidate and on the base.
        pass();
        assert_eq!(count(), 2);
        let (record, event, at) = last_verdict(&db);
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["fact"]["scenario"], "check-e2e.sh");
        assert_eq!(record["fact"]["on"], "candidate and base");
        assert_eq!(
            (
                &record["host_faults"],
                &record["escalate"],
                &record["backoff_ms"]
            ),
            (&json!(2), &json!(true), &json!(4000))
        );
        let escalations = db.store.gate_host_fault_escalations().expect("escalations");
        assert_eq!(escalations[0]["event_id"], event, "{escalations}");
        let after = gated(&db, mission);
        assert_eq!((after.gate_host_faults, after.gate_aborts), (2, 0));
        assert_eq!(super::halt_reason(&after, 0), None);

        pass();
        assert_eq!(count(), 2, "held again, for the doubled backoff");
        wait_until(at + 4000);
        pass();
        assert_eq!(count(), 3);
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
        assert_eq!(gated(&db, mission).gate_aborts, 0);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sandbox that never started its command, recorded through the
    /// store for every tree the gate names (rev-e on 81dd42f9: the store's
    /// host_fault check refused the evidence, and no test wrote one): each
    /// is `host_fault` with evidence `sandbox_never_started` and its tree.
    /// The base's and the gate's own come before any candidate code ran, as
    /// a fault on the base does: neither counts toward the host-fault cap
    /// nor is ever capped. With the cap at 1, the candidate's first is a
    /// host fault, a base one after it still is, and the candidate's second
    /// is capped, recorded `aborted`.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_sandbox_that_never_started_is_recorded_for_each_tree_and_capped_only_on_the_candidate() {
        let dir =
            std::env::temp_dir().join(format!("spawn-db-never-started-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let runs = dir.join("runs");
        let runs_path = runs.display();
        let fact = |scenario: &str, tree: &str| {
            format!(
                "printf '{{\"host_fault\":true,\"scenario\":\"{scenario}\",\"on\":\"sandbox\",\
                 \"runs\":\"never started\",\"reason\":\"sandbox: {scenario} in /w/{tree} never \
                 started (bwrap exited 1: bwrap: cannot mount tmpfs on /run/x)\",\
                 \"evidence\":\"sandbox_never_started\",\"tree\":\"{tree}\"}}\\n' \
                 >\"$SPAWN_GATE_VERDICT\"\nexit 6\n"
            )
        };
        let gate = format!(
            "n=$(cat '{runs_path}' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >'{runs_path}'\n\
             case \"$n\" in\n\
             1) {}\n;;\n\
             2) {}\n;;\n\
             3) {}\n;;\n\
             4) {}\n;;\n\
             5) {}\n;;\n\
             esac\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n",
            fact("sh build", "base"),
            fact("socket-scan", "gate"),
            fact("sh build", "candidate"),
            fact("sh 012.sh", "base"),
            fact("sh check-e2e.sh", "candidate"),
        );
        let (base, source, scenarios, mut integrator) = world("never-started", &gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_millis(300),
            cap: 1,
        };
        let db = Db::new("gate_never_started");
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
        // One judging: wait out the last record's due time, then pass.
        let judged = |n: u32| -> Value {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while count() < n {
                assert!(std::time::Instant::now() < deadline, "run {n} never came");
                integrator.pass_on(&db.store).expect("a pass");
                integrator.settle();
                if count() < n {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            last_verdict(&db).0
        };
        for (n, tree) in [(1, "base"), (2, "gate"), (3, "candidate"), (4, "base")] {
            let record = judged(n);
            assert_eq!(record["verdict"], "host_fault", "run {n}: {record}");
            assert_eq!(
                record["evidence"],
                crate::gate::SANDBOX_NEVER_STARTED,
                "{record}"
            );
            assert_eq!(record["fact"]["on"], "sandbox", "{record}");
            assert_eq!(record["fact"]["tree"], tree, "{record}");
            assert_eq!(record["fact"]["runs"], "never started", "{record}");
            assert!(
                record["reason"].as_str().is_some_and(
                    |reason| reason.starts_with("host fault: the gate's sandbox never started")
                ),
                "{record}"
            );
        }
        // Only the candidate's counted: one, at the cap, so the next
        // candidate fault is capped.
        assert_eq!(
            db.store
                .task_host_faults(task, 0, &source_main(&source))
                .expect("the count"),
            1,
            "the base's and the gate's own do not count"
        );
        let record = judged(5);
        assert_eq!(record["verdict"], "aborted", "{record}");
        assert_eq!(
            record["host_fault"]["evidence"],
            crate::gate::SANDBOX_NEVER_STARTED,
            "{record}"
        );
        assert_eq!(
            record["host_fault"]["fact"]["tree"], "candidate",
            "{record}"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The gate's own test server that did not start (gate.sh
    /// `start_server`), recorded through the store for both starts: the
    /// first (`gate`) and the candidate phase's fresh one (`candidate`).
    /// Each is `host_fault` with evidence `test_server_never_started` and
    /// its start. Neither is the candidate's doing: with the cap at 1, both
    /// are recorded as host faults, neither counts toward the task, and the
    /// next judging lands.
    #[test]
    fn a_test_server_that_never_started_is_recorded_for_both_starts_and_never_counted() {
        let dir = std::env::temp_dir().join(format!(
            "spawn-db-server-never-started-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let runs = dir.join("runs");
        let runs_path = runs.display();
        let fact = |tree: &str, what: &str| {
            format!(
                "printf '{{\"host_fault\":true,\"scenario\":\"test server\",\"on\":\"test server\",\
                 \"runs\":\"never started\",\"reason\":\"{what} did not start: FATAL:  could not \
                 create shared memory segment: a host fault, not a verdict on the candidate\",\
                 \"evidence\":\"test_server_never_started\",\"tree\":\"{tree}\"}}\\n' \
                 >\"$SPAWN_GATE_VERDICT\"\nexit 6\n"
            )
        };
        let gate = format!(
            "n=$(cat '{runs_path}' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >'{runs_path}'\n\
             case \"$n\" in\n\
             1) {}\n;;\n\
             2) {}\n;;\n\
             esac\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n",
            // No apostrophe: the fact is in a single-quoted printf.
            fact("gate", "the first test server"),
            fact("candidate", "the fresh test server of the candidate phase"),
        );
        let (base, source, scenarios, mut integrator) = world("server-never-started", &gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_millis(300),
            cap: 1,
        };
        let db = Db::new("gate_server_never_started");
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
        let judged = |n: u32| -> Value {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while count() < n {
                assert!(std::time::Instant::now() < deadline, "run {n} never came");
                integrator.pass_on(&db.store).expect("a pass");
                integrator.settle();
                if count() < n {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            last_verdict(&db).0
        };
        for (n, tree) in [(1, "gate"), (2, "candidate")] {
            let record = judged(n);
            assert_eq!(record["verdict"], "host_fault", "run {n}: {record}");
            assert_eq!(
                record["evidence"],
                crate::gate::TEST_SERVER_NEVER_STARTED,
                "{record}"
            );
            assert_eq!(record["fact"]["on"], "test server", "{record}");
            assert_eq!(record["fact"]["tree"], tree, "{record}");
            assert_eq!(record["fact"]["runs"], "never started", "{record}");
            assert!(
                record["reason"].as_str().is_some_and(|reason| reason
                    .starts_with("host fault: the gate's own test server never started")),
                "{record}"
            );
        }
        assert_eq!(
            db.store
                .task_host_faults(task, 0, &source_main(&source))
                .expect("the count"),
            0,
            "no server start counts toward the task"
        );
        let record = judged(3);
        assert_eq!(record["verdict"], "accepted", "{record}");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The host-fault cap (rev-e on gate-check-flakes): one report revision
    /// of a task has at most `SPAWN_GATE_HOST_FAULT_CAP` uncounted host
    /// faults. With the cap at 2, a gate that reports the same host fault on
    /// every run is `host_fault` twice (no count), and the third run is
    /// recorded `aborted` with a reason that says why, the fault kept under
    /// `host_fault`: it counts toward the abort halt, as the next one does.
    #[test]
    fn host_faults_past_the_cap_on_one_revision_count_as_aborts() {
        // A host fault the cap counts: check-tests.sh's failures, every one
        // a connection error to the test database (evidence `failures`).
        let gate = "echo '| == regression: check-tests.sh must still pass'\n\
                    echo '| FAILED api::tests::one: error connecting to server: Connection refused (os error 111)'\n\
                    echo '| stage gate rejected 5ms regression: check-tests.sh must still pass'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
        let (base, source, scenarios, mut integrator) = world("gate-fault-cap", gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_secs(1),
            cap: 2,
        };
        let db = Db::new("gate_host_fault_cap");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        push_work(&base, &source, &agent, "work.txt");
        // One judging: a pass, its gate settled, after the last record's
        // due time (a host fault's backoff) has come.
        let judge_once = |due: Option<i64>| {
            if let Some(due) = due {
                let left = due - super::now_ms();
                if left > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(
                        u64::try_from(left).unwrap_or(0) + 50,
                    ));
                }
            }
            integrator.pass_on(&db.store).expect("a pass");
            integrator.settle();
            last_verdict(&db).0
        };

        let first = judge_once(None);
        assert_eq!(first["verdict"], "host_fault", "{first}");
        let second = judge_once(first["due_ms"].as_i64());
        assert_eq!(second["verdict"], "host_fault", "the cap is 2: {second}");
        assert_eq!(gated(&db, mission).gate_aborts, 0, "no count yet");

        let third = judge_once(second["due_ms"].as_i64());
        assert_eq!(third["verdict"], "aborted", "past the cap: {third}");
        let reason = third["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains(&format!(
                "task {task} has had 2 at report revision 0 on main {}",
                &source_main(&source)[..12]
            )) && reason.contains("SPAWN_GATE_HOST_FAULT_CAP allows is 2")
                && reason.contains("counts as an abort")
                && reason.contains("is a connection error to the test database")
                && !reason.contains("judged again"),
            "{reason}"
        );
        assert_eq!(third["host_fault"]["evidence"], "failures", "{third}");
        assert_eq!(
            third["host_fault"]["failures"][0]["test"],
            "api::tests::one"
        );
        assert!(third["log"].is_string(), "{third}");
        let after = gated(&db, mission);
        assert_eq!(
            (after.gate_aborts, after.gate_host_faults),
            (1, 0),
            "an abort, which ends the run of host faults"
        );
        assert_eq!(
            after.tasks[0].state,
            spawn_core::mission::TaskState::Reported
        );

        // The same revision's next host fault counts too.
        let fourth = judge_once(None);
        assert_eq!(fourth["verdict"], "aborted", "{fourth}");
        assert_eq!(gated(&db, mission).gate_aborts, 2);
        assert_eq!(
            db.store
                .task_host_faults(task, 0, &source_main(&source))
                .expect("count"),
            2
        );

        assert_eq!(super::host_fault_cap(None), 3);
        assert_eq!(super::host_fault_cap(Some(" 5 ")), 5);
        assert_eq!(super::host_fault_cap(Some("0")), 3);
        assert_eq!(super::host_fault_cap(Some("x")), 3);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A gate that notes its candidate in `<marks>/started`, holds until
    /// `<marks>/release` exists, then writes `verdict` and exits `exit`:
    /// main moves while it holds.
    fn gate_holding_then(marks: &Path, lines: &str, verdict: &str, exit: i32) -> String {
        let marks = marks.to_string_lossy();
        format!(
            "echo \"$3\" >>'{marks}/started'\n\
             while [ ! -e '{marks}/release' ]; do sleep 0.05; done\n\
             {lines}\
             printf '%s\\n' '{verdict}' >\"$SPAWN_GATE_VERDICT\"\n\
             exit {exit}\n"
        )
    }

    /// One judging whose gate holds while main moves on the source: the
    /// pass starts the gate on `m0`, main moves (the owner's push, recorded
    /// `source.moved` as `spawn mission moved` does, so no pass halts on a
    /// main no record explains), the gate is released and settled. Main's
    /// new commit.
    fn judged_while_main_moves(
        integrator: &Integrator,
        db: &Db,
        base: &Path,
        source: &str,
        marks: &Path,
        file: &str,
    ) -> String {
        let _ = std::fs::remove_file(marks.join("release"));
        let before = started(marks).len();
        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(
            wait_started(marks, before + 1).len(),
            before + 1,
            "the gate started"
        );
        let was = source_main(source);
        let now = main_moves(base, source, file);
        db.store.source_moved(source, &was, &now).expect("moved");
        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        now
    }

    /// Main moved while the gate ran (design-stale-on-main-move.md): every
    /// verdict but `accepted` (whose landing's compare-and-swap is its own
    /// guard) is `stale`, judged again on the new main, with what the gate
    /// found (`found`), main then (`base`) and now (`main_now`), and its
    /// reason; nothing is counted: no rejection, no abort, no run of host
    /// faults, no room count, and the task is ready again.
    #[test]
    fn a_verdict_on_a_main_that_moved_while_the_gate_ran_is_stale() {
        let cases = [
            (
                "host_fault",
                "echo '| == regression: check-tests.sh must still pass'\n\
                 echo '| FAILED api::tests::one: error connecting to server: Connection refused (os error 111)'\n",
                "{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}",
                0,
            ),
            (
                "rejected",
                "",
                "{\"verdict\":\"rejected\",\"reason\":\"regression: 012.sh must still pass\"}",
                0,
            ),
            (
                "no_room",
                "",
                "{\"no_room\":true,\"free_kb\":2048,\"min_gb\":10,\"tmpdir\":\"/data/tmp\"}",
                5,
            ),
            ("aborted", "echo 'gate: a harness error' >&2\n", "", 3),
        ];
        for (found, lines, verdict, exit) in cases {
            let marks = fresh_marks(&format!("spawn-stale-move-{found}"));
            let release = Release(marks.clone());
            let (base, source, scenarios, mut integrator) = world(
                &format!("stale-move-{found}"),
                &gate_holding_then(&marks, lines, verdict, exit),
            );
            integrator.judge.host = super::HostSettings {
                server_log: None,
                backoff: std::time::Duration::from_secs(1),
                cap: 3,
            };
            let db = Db::new(&format!("stale_move_{found}"));
            let m0 = source_main(&source);
            let mission = db.gated_mission(&source, &scenarios);
            let task = db.reported_task(mission, "w");
            let agent = agent_of(&db, mission, task);
            push_work(&base, &source, &agent, "work.txt");

            let m1 = judged_while_main_moves(&integrator, &db, &base, &source, &marks, "main.txt");
            let (record, _, _) = last_verdict(&db);
            assert_eq!(record["verdict"], "stale", "{found}: {record}");
            assert_eq!(record["found"], found, "{found}: {record}");
            assert_eq!(record["base"], m0.as_str(), "{found}");
            assert_eq!(record["main_now"], m1.as_str(), "{found}");
            let reason = record["reason"].as_str().unwrap_or_default();
            assert!(
                reason.starts_with(&format!(
                    "main moved from {} to {} while the gate ran: its {found}",
                    &m0[..12],
                    &m1[..12]
                )) && reason.ends_with("is no verdict on the new main; judged again"),
                "{found}: {reason}"
            );
            assert!(record.get("backoff_ms").is_none(), "{found}: {record}");
            let after = gated(&db, mission);
            assert_eq!(
                (
                    after.gate_rejections,
                    after.gate_aborts,
                    after.gate_host_faults
                ),
                (0, 0, 0),
                "{found}: nothing counted"
            );
            assert_eq!(
                after.tasks[0].state,
                spawn_core::mission::TaskState::Reported,
                "{found}: ready to be judged again"
            );
            assert!(after.halted.is_none(), "{found}");
            assert_eq!(
                db.store.task_host_faults(task, 0, &m0).expect("count"),
                0,
                "{found}: no host fault for the cap"
            );
            drop(release);
            let _ = std::fs::remove_dir_all(&base);
            let _ = std::fs::remove_dir_all(&marks);
        }
    }

    /// Supply PA's mission 6, 114516 (2026-11-09): two gate aborts in a row,
    /// a task at the host-fault cap, and a gate started on main A whose
    /// verdict lands, after main moved to B, as a host fault the cap would
    /// make the third abort and halt the mission. It is `stale` instead,
    /// found `host_fault`: `gate_aborts` stays 2, and no halt.
    #[test]
    fn a_capped_host_fault_on_a_main_that_moved_is_stale_and_halts_nothing() {
        let marks = fresh_marks("spawn-stale-114516");
        let release = Release(marks.clone());
        let gate = gate_holding_then(
            &marks,
            "echo '| == regression: check-tests.sh must still pass'\n\
             echo '| FAILED api::tests::one: error connecting to server: Connection refused (os error 111)'\n",
            "{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}",
            0,
        );
        let (base, source, scenarios, mut integrator) = world("stale-114516", &gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_millis(1),
            cap: 1,
        };
        let db = Db::new("stale_114516");
        let a = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        for _ in 0..2 {
            db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                              "verdict": "aborted", "base": a, "reason": "a harness error"}));
        }
        let agent = agent_of(&db, mission, task);
        push_work(&base, &source, &agent, "work.txt");
        // The task's first host fault, on A: the cap (1) is reached.
        std::fs::write(marks.join("release"), "").expect("release");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let first = last_verdict(&db).0;
        assert_eq!(first["verdict"], "host_fault", "{first}");
        assert_eq!(db.store.task_host_faults(task, 0, &a).expect("count"), 1);
        assert_eq!(gated(&db, mission).gate_aborts, 2);
        past_due(&db);

        let b = judged_while_main_moves(&integrator, &db, &base, &source, &marks, "main.txt");
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "stale", "never capped: {record}");
        assert_eq!(record["found"], "host_fault");
        assert_eq!(
            (record["base"].as_str(), record["main_now"].as_str()),
            (Some(a.as_str()), Some(b.as_str()))
        );
        assert!(record.get("host_fault").is_none(), "{record}");
        let after = gated(&db, mission);
        assert_eq!(after.gate_aborts, 2, "gate_aborts unchanged");
        assert_eq!(super::halt_reason(&after, 0), None, "no halt");
        assert!(after.halted.is_none());
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&marks);
    }

    /// An owner's landing requested while a gate of its source runs moves
    /// main only after that gate (design-owner-push-between-gates.md, D1):
    /// a pass during the gate applies nothing, the task's verdict is its
    /// own (`accepted`, landed on the main it was judged against, never
    /// `stale`), and the next pass lands the owner's commit on the task's
    /// landing. On a step 0 that applies every request at once, main moved
    /// under the gate and the verdict came back `stale` (Supply PA tasks
    /// 214, 218, 220, 222, 223, 226 and 227, 2026-11-09/10).
    #[test]
    fn an_owner_push_requested_while_a_gate_runs_moves_main_only_after_it() {
        let marks = fresh_marks("spawn-owner-between");
        let release = Release(marks.clone());
        let gate = gate_holding_then(&marks, "", "{\"verdict\":\"accepted\"}", 0);
        let (base, source, scenarios, integrator) = world("owner-between", &gate);
        let db = Db::new("owner_between");
        let m0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = agent_of(&db, mission, task);
        push_work(&base, &source, &agent, "work.txt");
        let _ = std::fs::remove_file(marks.join("release"));
        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(wait_started(&marks, 1).len(), 1, "the task's gate started");

        let commit = owner_commit(&base, &source, "CONTRACT.md", "v1\n", "owner: the contract");
        let request = owner_request(&db, &source, &commit);
        integrator
            .pass_on(&db.store)
            .expect("a pass while the gate runs");
        assert_eq!(
            source_main(&source),
            m0,
            "no owner's landing moves main under a gate of its source"
        );
        assert!(control_records(&db, "source.moved").is_empty());

        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        let (record, _, _) = last_verdict(&db);
        assert_eq!(
            record["verdict"], "accepted",
            "the task's own verdict: {record}"
        );
        let landed = control_records(&db, "task.landed");
        assert_eq!(landed.len(), 1, "{landed:?}");
        let landing = landed[0]["to"].as_str().expect("the landing").to_string();
        assert_eq!(landed[0]["from"], m0.as_str());
        assert_eq!(source_main(&source), landing);

        integrator
            .pass_on(&db.store)
            .expect("the pass after the gate");
        let moved = control_records(&db, "source.moved");
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(moved[0]["request"], json!(request));
        assert_eq!(moved[0]["from"], landing.as_str());
        let now = source_main(&source);
        assert_eq!(
            git(&["-C", &source, "rev-parse", &format!("{now}^1")]).expect("first parent"),
            landing,
            "the owner's landing is on the task's"
        );
        assert_eq!(
            db.mission(mission)["baseline"],
            now.as_str(),
            "the mission's baseline follows the owner's landing"
        );
        assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&marks);
    }

    /// The owner's commit with `files` (paths under the tree, directories
    /// made), put in the source at its owner's ref, no branch moved.
    fn owner_commit_files(
        base: &Path,
        source: &str,
        files: &[(&str, &str)],
        subject: &str,
    ) -> String {
        let clone = base.join(format!("owner-files-{}", subject.len()));
        let clone_path = clone.to_string_lossy().to_string();
        let _ = std::fs::remove_dir_all(&clone);
        git(&["clone", "-q", source, &clone_path]).expect("the owner's clone");
        for (file, text) in files {
            let path = clone.join(file);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("directories");
            std::fs::write(path, text).expect("write");
        }
        super::tests::commit_in(&clone_path, subject);
        let commit = git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("the commit");
        let reference = super::owner::owner_ref(&commit);
        git(&[
            "-C",
            &clone_path,
            "push",
            "-q",
            "origin",
            &format!("{commit}:{reference}"),
        ])
        .expect("into the source, no branch");
        commit
    }

    /// A gated owner's request, as `spawn mission push --gated` records it.
    fn gated_request(db: &Db, source: &str, commit: &str, reason: &str) -> i64 {
        db.store
            .source_push_request(
                source,
                commit,
                &super::owner::owner_ref(commit),
                &spawn_postgres::PushRequest {
                    gated: true,
                    reason: Some(reason.to_string()),
                    ..spawn_postgres::PushRequest::default()
                },
            )
            .expect("the owner's gated request")
    }

    /// A gated owner's push (design-owner-push-between-gates.md) is a
    /// judging of its source: requested while a task's gate runs, it starts
    /// only after that gate, and ahead of the source's next ready task. Its
    /// gate judges the owner's landing merge on the task's landing, with
    /// the candidate's own scenarios and an empty protected list (D3), and
    /// its acceptance moves main to that merge, `source.moved` naming the
    /// verdict. The next task is judged on the new main after it.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_gated_owner_push_is_judged_after_the_running_gate_and_before_the_next_task() {
        let marks = fresh_marks("spawn-owner-gated");
        let release = Release(marks.clone());
        let seen = marks.to_string_lossy().to_string();
        let gate = gate_holding_then(
            &marks,
            &format!(
                "echo \"$4\" >>'{seen}/scenarios-given'\n\
                 cat \"$4/protected\" >>'{seen}/protected-seen'\n"
            ),
            "{\"verdict\":\"accepted\"}",
            0,
        );
        let (base, source, scenarios, integrator) = world("owner-gated", &gate);
        let db = Db::new("owner_gated");
        let m0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = agent_of(&db, mission, task);
        push_work(&base, &source, &agent, "work.txt");
        let _ = std::fs::remove_file(marks.join("release"));
        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(wait_started(&marks, 1).len(), 1, "the task's gate started");

        let commit = owner_commit_files(
            &base,
            &source,
            &[
                ("scenarios/protected", "scenarios/\n"),
                ("scenarios/check-contract.sh", "exit 0\n"),
                ("CONTRACT.md", "v1\n"),
            ],
            "owner: the contract",
        );
        let request = gated_request(&db, &source, &commit, "the contract, v1");
        let second = db.reported_task(mission, "w2");
        let second_agent = agent_of(&db, mission, second);
        push_work(&base, &source, &second_agent, "second.txt");
        integrator
            .pass_on(&db.store)
            .expect("a pass while the gate runs");
        assert_eq!(
            source_main(&source),
            m0,
            "nothing moves under the task's gate"
        );
        assert_eq!(
            started(&marks).len(),
            1,
            "no other judging of the source starts"
        );

        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        let landed = control_records(&db, "task.landed");
        assert_eq!(landed.len(), 1, "{landed:?}");
        let landing = landed[0]["to"].as_str().expect("the landing").to_string();

        integrator
            .pass_on(&db.store)
            .expect("the pass after the gate");
        assert_eq!(
            integrator
                .running
                .borrow()
                .iter()
                .map(|gate| gate.owner)
                .collect::<Vec<_>>(),
            vec![Some(request)],
            "the owner's judging takes the source, not the second task's"
        );
        integrator.settle();
        let judged = started(&marks);
        assert_eq!(judged.len(), 2, "{judged:?}");
        let moved = control_records(&db, "source.moved");
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(moved[0]["request"], json!(request));
        assert_eq!(moved[0]["from"], landing.as_str());
        assert_eq!(moved[0]["verdict"], "accepted");
        let now = source_main(&source);
        assert_eq!(moved[0]["to"], now.as_str());
        assert_eq!(judged[1], now, "the owner's gate judged what landed");
        assert_eq!(
            git(&["--git-dir", &source, "rev-parse", &format!("{now}^1")]).expect("first parent"),
            landing,
            "the owner's landing merge is on the task's landing"
        );
        assert_eq!(
            git(&["--git-dir", &source, "rev-parse", &format!("{now}^2")]).expect("second parent"),
            commit
        );
        let given = std::fs::read_to_string(marks.join("scenarios-given")).expect("given");
        let given: Vec<&str> = given.lines().collect();
        assert_eq!(given.len(), 2, "{given:?}");
        assert_ne!(
            given[1],
            scenarios.to_string_lossy(),
            "the candidate's own scenarios"
        );
        assert!(Path::new(given[1]).ends_with("scenarios"), "{given:?}");
        assert_eq!(
            std::fs::read_to_string(marks.join("protected-seen")).expect("protected"),
            "",
            "the copy's protected list is empty"
        );
        assert!(
            !Path::new(given[1]).exists(),
            "the candidate's tree is removed after its gate"
        );
        assert!(verdicts(&db, second).is_empty(), "the second task waited");

        integrator.pass_on(&db.store).expect("the next pass");
        integrator.settle();
        assert_eq!(verdicts(&db, second), vec!["accepted"]);
        assert_eq!(started(&marks)[2].len(), 40);
        assert_eq!(
            git(&[
                "--git-dir",
                &source,
                "rev-parse",
                &format!("{}^1", source_main(&source))
            ])
            .expect("first parent"),
            now,
            "the second task landed on the owner's landing"
        );
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&marks);
    }

    /// A gated owner's push its gate rejects records `source.push_refused`
    /// with the verdict and the gate's log, and moves nothing; no mission
    /// counts it.
    #[test]
    fn a_rejected_owner_judging_is_refused_and_moves_nothing() {
        let gate = "echo '| == regression: 046.sh must still pass'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"regression: 046.sh fails on the candidate\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                    echo '{\"verdict\":\"rejected\",\"reason\":\"regression: 046.sh fails on the candidate\"}'\n";
        let (base, source, scenarios, integrator) = world("owner-rejected", gate);
        let db = Db::new("owner_rejected");
        let m0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let before = gated(&db, mission);
        let commit = owner_commit_files(
            &base,
            &source,
            &[("scenarios/protected", ""), ("BROKEN.md", "x\n")],
            "owner: breaks 046",
        );
        let request = gated_request(&db, &source, &commit, "a change");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        assert_eq!(source_main(&source), m0, "nothing moves");
        assert!(control_records(&db, "source.moved").is_empty());
        let refused = control_records(&db, "source.push_refused");
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert_eq!(refused[0]["request"], json!(request));
        assert_eq!(refused[0]["verdict"], "rejected");
        assert_eq!(
            refused[0]["reason"],
            "the owner's gate rejected it: regression: 046.sh fails on the candidate"
        );
        assert!(
            refused[0]["log"]
                .as_str()
                .is_some_and(|log| log.contains("046.sh must still pass")),
            "{}",
            refused[0]
        );
        let after = gated(&db, mission);
        assert_eq!(
            after.gate_rejections, before.gate_rejections,
            "no mission counts it"
        );
        assert_eq!(after.gate_aborts, before.gate_aborts);
        integrator.pass_on(&db.store).expect("the next pass");
        integrator.settle();
        assert_eq!(
            control_records(&db, "source.push_refused").len(),
            1,
            "judged once"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An owner judging whose gate still ends `main_red` (a gate.sh without
    /// SPAWN_GATE_RED_UNSOLVED, which otherwise judges the red check as
    /// unsolved on the base and goes on) gets no verdict, not a refusal
    /// (the lead, 2026-11-10): the push neither broke nor fixed main,
    /// nothing is recorded, nothing moves, and it is judged again after its
    /// backoff.
    #[test]
    fn an_owner_judging_on_a_red_main_it_does_not_fix_is_no_verdict() {
        let gate = "printf '%s\\n' '{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"main\",\"runs\":\"1, then 1 on the candidate; 1, then 1 on the base\",\"reason\":\"r\",\"failures\":[{\"test\":\"api::slice_11\",\"message\":\"expected 23 to be 21\"}]}' >\"$SPAWN_GATE_VERDICT\"\nexit 6\n";
        let (base, source, scenarios, mut integrator) = world("owner-red", gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_mins(10),
            cap: 3,
        };
        let db = Db::new("owner_red");
        let m0 = source_main(&source);
        db.gated_mission(&source, &scenarios);
        let commit = owner_commit_files(
            &base,
            &source,
            &[("scenarios/protected", ""), ("B.md", "b\n")],
            "owner: unrelated to the red check",
        );
        let request = gated_request(&db, &source, &commit, "b");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        assert_eq!(source_main(&source), m0, "nothing moves");
        assert!(
            control_records(&db, "source.push_refused").is_empty(),
            "not refused for a red it did not cause"
        );
        assert!(control_records(&db, "source.moved").is_empty());
        let backoff = Arc::clone(&integrator.owner.borrow().backoff);
        assert_eq!(
            backoff
                .lock()
                .expect("backoff")
                .get(&request)
                .map(|(faults, _)| *faults),
            Some(1),
            "judged again after its backoff"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An owner judging whose gate aborts (a non-zero exit with no verdict
    /// to believe: a harness error) records nothing and is judged again
    /// after its backoff, as the host's other failures are (the lead,
    /// 2026-11-10, rev-b's N1): never `source.push_refused`.
    #[test]
    fn an_aborted_owner_gate_is_no_verdict() {
        let (base, source, scenarios, integrator) =
            world("owner-aborted", "echo 'half a run'\nexit 1\n");
        let db = Db::new("owner_aborted");
        let m0 = source_main(&source);
        db.gated_mission(&source, &scenarios);
        let commit = owner_commit_files(
            &base,
            &source,
            &[("scenarios/protected", ""), ("C.md", "c\n")],
            "owner: c",
        );
        let request = gated_request(&db, &source, &commit, "c");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        assert_eq!(source_main(&source), m0, "nothing moves");
        assert!(
            control_records(&db, "source.push_refused").is_empty(),
            "an abort is no refusal"
        );
        assert!(control_records(&db, "source.moved").is_empty());
        let backoff = Arc::clone(&integrator.owner.borrow().backoff);
        assert_eq!(
            backoff
                .lock()
                .expect("backoff")
                .get(&request)
                .map(|(faults, _)| *faults),
            Some(1),
            "judged again after its backoff"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An owner's gate runs with SPAWN_GATE_RED_UNSOLVED=1 (the lead,
    /// 2026-11-10, rev-b's N2): a check main is red on is unsolved on the
    /// base, so the push is judged on the rest (test-gate.sh 5a'), never
    /// held or refused for a red it did not cause. A task's gate runs
    /// without it.
    #[test]
    fn an_owner_gate_judges_a_red_check_as_unsolved_on_the_base_and_a_task_gate_does_not() {
        let marks = fresh_marks("spawn-owner-red-unsolved");
        let seen = marks.to_string_lossy().to_string();
        let gate = format!(
            "echo \"${{SPAWN_GATE_RED_UNSOLVED:-unset}}\" >>'{seen}/red-unsolved'\n\
             printf '%s\\n' '{{\"verdict\":\"accepted\"}}' >\"$SPAWN_GATE_VERDICT\"\n"
        );
        let (base, source, scenarios, integrator) = world("owner-red-unsolved", &gate);
        let db = Db::new("owner_red_unsolved");
        let mission = db.gated_mission(&source, &scenarios);
        let commit = owner_commit_files(
            &base,
            &source,
            &[("scenarios/protected", ""), ("D.md", "d\n")],
            "owner: d",
        );
        gated_request(&db, &source, &commit, "d");
        integrator.pass_on(&db.store).expect("the owner's judging");
        integrator.settle();
        assert_eq!(control_records(&db, "source.moved").len(), 1);
        let task = db.reported_task(mission, "w");
        let agent = agent_of(&db, mission, task);
        push_work(&base, &source, &agent, "work.txt");
        integrator.pass_on(&db.store).expect("the task's judging");
        integrator.settle();
        assert_eq!(verdicts(&db, task), vec!["accepted"]);
        assert_eq!(
            std::fs::read_to_string(marks.join("red-unsolved")).expect("seen"),
            "1\nunset\n",
            "the owner's gate only"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&marks);
    }

    /// A gated owner's push whose candidate has no scenarios directory
    /// where the mission's is cannot be judged by it: refused, verdict
    /// `no_scenarios`, the owner's to fix; nothing moves.
    #[test]
    fn a_gated_owner_push_without_the_scenarios_directory_is_refused() {
        let (base, source, scenarios, integrator) = world("owner-no-scenarios", "exit 0\n");
        let db = Db::new("owner_no_scenarios");
        let m0 = source_main(&source);
        db.gated_mission(&source, &scenarios);
        let commit = owner_commit_files(&base, &source, &[("E.md", "e\n")], "owner: e");
        let request = gated_request(&db, &source, &commit, "e");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        assert_eq!(source_main(&source), m0);
        let refused = control_records(&db, "source.push_refused");
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert_eq!(refused[0]["request"], json!(request));
        assert_eq!(refused[0]["verdict"], "no_scenarios");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An owner judging's tree gets the installed controller's
    /// web/node_modules, a real directory made before its gate starts (the
    /// gate binds it only when it is there at its setup), so a check that
    /// builds the UI judges the owner's push instead of failing on both
    /// sides as a host fault (lead-mac, 2026-11-10). Only when the candidate
    /// names the dependencies it was installed from (web/pnpm-lock.yaml).
    #[test]
    fn an_owner_judgings_tree_gets_the_controllers_web_node_modules() {
        let (base, source, scenarios, integrator) = world("owner-modules", "");
        let seen = base.join("modules-seen");
        let gate = format!(
            "m=\"$4/../web/node_modules\"\n\
             if [ -d \"$m\" ] && [ ! -L \"$m\" ] && [ -f \"$m/pkg/index.js\" ]; then echo copied; \
             else echo none; fi >>'{seen}'\n\
             printf '%s\\n' '{{\"verdict\":\"accepted\"}}' >\"$SPAWN_GATE_VERDICT\"\n",
            seen = seen.display()
        );
        std::fs::write(base.join("gate.sh"), gate).expect("gate");
        let modules = base.join("web/node_modules");
        std::fs::create_dir_all(modules.join(".pnpm")).expect("modules");
        std::fs::create_dir_all(modules.join("pkg")).expect("modules");
        std::fs::write(modules.join(".pnpm/lock.yaml"), "lock v1\n").expect("lock");
        std::fs::write(modules.join("pkg/index.js"), "x\n").expect("pkg");
        let db = Db::new("owner_modules");
        db.gated_mission(&source, &scenarios);
        let seen_lines = || {
            std::fs::read_to_string(&seen)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        };

        let commit = owner_commit_files(
            &base,
            &source,
            &[
                ("scenarios/protected", ""),
                ("web/package.json", "{}\n"),
                ("web/pnpm-lock.yaml", "lock v1\n"),
            ],
            "owner: the ui",
        );
        gated_request(&db, &source, &commit, "the ui");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        assert_eq!(
            seen_lines(),
            vec!["copied"],
            "the gate found the copy at its start"
        );
        assert_eq!(
            control_records(&db, "source.moved").len(),
            1,
            "judged, and landed"
        );

        // Other dependencies than the installed ones: not given.
        let commit = owner_commit_files(
            &base,
            &source,
            &[("web/pnpm-lock.yaml", "lock v2, other dependencies\n")],
            "owner: new dependencies",
        );
        gated_request(&db, &source, &commit, "new dependencies");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        assert_eq!(seen_lines(), vec!["copied", "none"]);
        assert!(
            std::fs::symlink_metadata(modules.join("pkg/index.js")).is_ok(),
            "the controller's own is untouched"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An owner judging that finds no verdict (the host's fault) records
    /// nothing: the request stays pending, is not judged again before its
    /// backoff, and is judged again after it.
    #[test]
    fn an_owner_judging_with_no_verdict_stays_pending_until_its_backoff() {
        let gate = format!(
            "echo \"$3\" >>\"$(dirname \"$0\")/started\"\n\
             printf '%s\\n' '{}' >\"$SPAWN_GATE_VERDICT\"\nexit 6\n",
            "{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"base\",\"runs\":\"1, then 1 on the base\",\"reason\":\"r\"}"
        );
        let (base, source, scenarios, mut integrator) = world("owner-no-verdict", &gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_mins(10),
            cap: 3,
        };
        let db = Db::new("owner_no_verdict");
        let m0 = source_main(&source);
        db.gated_mission(&source, &scenarios);
        let commit = owner_commit_files(
            &base,
            &source,
            &[("scenarios/protected", ""), ("A.md", "a\n")],
            "owner: a",
        );
        let request = gated_request(&db, &source, &commit, "a");
        let runs = || {
            std::fs::read_to_string(base.join("started"))
                .unwrap_or_default()
                .lines()
                .count()
        };
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        assert_eq!(runs(), 1);
        assert_eq!(source_main(&source), m0);
        assert!(control_records(&db, "source.moved").is_empty());
        assert!(control_records(&db, "source.push_refused").is_empty());
        let backoff = Arc::clone(&integrator.owner.borrow().backoff);
        assert_eq!(
            backoff
                .lock()
                .expect("backoff")
                .get(&request)
                .map(|(faults, _)| *faults),
            Some(1)
        );
        integrator
            .pass_on(&db.store)
            .expect("a pass in its backoff");
        integrator.settle();
        assert_eq!(runs(), 1, "not judged again inside its backoff");
        backoff
            .lock()
            .expect("backoff")
            .insert(request, (1, std::time::Instant::now()));
        integrator
            .pass_on(&db.store)
            .expect("a pass after its backoff");
        integrator.settle();
        assert_eq!(runs(), 2, "judged again after it");
        assert_eq!(
            backoff
                .lock()
                .expect("backoff")
                .get(&request)
                .map(|(faults, _)| *faults),
            Some(2),
            "the backoff grows"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An urgent owner's request (`spawn mission push --urgent`,
    /// design-owner-push-between-gates.md, D2) stops the task's gate in
    /// flight on its source: the gate is killed, not waited for, and the
    /// task's judging records itself discarded by the request (`stale`,
    /// found `discarded`, the request and the owner's reason), counted
    /// nowhere. The next pass applies the request, and the task is judged
    /// again on the owner's main. A request that is not urgent waits for
    /// the gate (D1, above).
    #[test]
    fn an_urgent_owner_push_stops_the_gate_in_flight_and_discards_it() {
        let marks = fresh_marks("spawn-owner-urgent");
        let release = Release(marks.clone());
        let gate = gate_holding_then(&marks, "", "{\"verdict\":\"accepted\"}", 0);
        let (base, source, scenarios, integrator) = world("owner-urgent", &gate);
        let db = Db::new("owner_urgent");
        let m0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let before = gated(&db, mission);
        let task = db.reported_task(mission, "w");
        let agent = agent_of(&db, mission, task);
        push_work(&base, &source, &agent, "work.txt");
        let _ = std::fs::remove_file(marks.join("release"));
        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(wait_started(&marks, 1).len(), 1, "the task's gate started");

        let commit = owner_commit(&base, &source, "FIX.md", "fix\n", "owner: fix 046");
        let request = db
            .store
            .source_push_request(
                &source,
                &commit,
                &super::owner::owner_ref(&commit),
                &spawn_postgres::PushRequest {
                    urgent: true,
                    reason: Some("046 is red on main".to_string()),
                    ..spawn_postgres::PushRequest::default()
                },
            )
            .expect("the urgent request");
        // A gate that is not stopped is released after 300 s (a guard
        // against a hang only): it then lands `accepted`, and the test fails.
        let watchdog = marks.join("release");
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_mins(5));
            let _ = std::fs::write(watchdog, "");
        });
        integrator
            .pass_on(&db.store)
            .expect("the pass that stops it");
        assert_eq!(source_main(&source), m0, "nothing moves under the gate");
        integrator.settle();
        assert!(!marks.join("release").exists(), "never released");
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "stale", "{record}");
        assert_eq!(record["found"], "discarded");
        assert_eq!(record["request"], json!(request));
        assert_eq!(
            record["reason"],
            format!(
                "discarded by the owner's urgent landing (request {request}): 046 is red on main"
            )
        );
        let after = gated(&db, mission);
        assert_eq!(after.gate_aborts, before.gate_aborts, "counted nowhere");
        assert_eq!(after.gate_rejections, before.gate_rejections);
        assert!(control_records(&db, "source.moved").is_empty());

        std::fs::write(marks.join("release"), "").expect("release");
        integrator.pass_on(&db.store).expect("the pass after it");
        let moved = control_records(&db, "source.moved");
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(moved[0]["request"], json!(request));
        assert_eq!(moved[0]["from"], m0.as_str());
        let owner_main = source_main(&source);
        integrator
            .pass_on(&db.store)
            .expect("the pass that judges again");
        integrator.settle();
        assert_eq!(verdicts(&db, task), vec!["stale", "accepted"]);
        let judged = started(&marks);
        assert_eq!(judged.len(), 2, "{judged:?}");
        assert_eq!(
            git(&[
                "--git-dir",
                &source,
                "rev-parse",
                &format!("{}^1", source_main(&source))
            ])
            .expect("first parent"),
            owner_main,
            "judged again and landed on the owner's main"
        );
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&marks);
    }

    /// A red main (design-main-red.md) is a fact about the main the gate
    /// judged against: when main moved while the gate ran, no
    /// `source.main_red` is written for a main that is no longer main, and
    /// the task's verdict is `stale`, found `host_fault`.
    #[test]
    fn no_red_main_is_recorded_for_a_main_that_moved_while_the_gate_ran() {
        let marks = fresh_marks("spawn-stale-red");
        let release = Release(marks.clone());
        let gate = gate_holding_then(
            &marks,
            "echo '| == regression: check-tests.sh must still pass'\n",
            "{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"main\",\"runs\":\"1, then 1 on the candidate; 1, then 1 on the base\",\"reason\":\"r\",\"failures\":[{\"test\":\"api::slice_11\",\"message\":\"expected 23 to be 21\"}]}",
            6,
        );
        let (base, source, scenarios, mut integrator) = world("stale-red", &gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_secs(1),
            cap: 3,
        };
        let db = Db::new("stale_red");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = agent_of(&db, mission, task);
        push_work(&base, &source, &agent, "work.txt");
        judged_while_main_moves(&integrator, &db, &base, &source, &marks, "main.txt");
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "stale", "{record}");
        assert_eq!(record["found"], "host_fault");
        assert!(control_records(&db, "source.main_red").is_empty());
        assert!(
            db.mission(mission)["red"].is_null(),
            "{}",
            db.mission(mission)
        );
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&marks);
    }

    /// A judging that panics after main moved (judged_or_aborted): no abort
    /// counted toward the halt for a main that is no longer main, `stale`
    /// with found `aborted` and the panic's reason. Three of them halt
    /// nothing.
    #[test]
    fn a_judging_that_panics_after_main_moved_is_stale() {
        let (base, source, scenarios, integrator) = world("panic-moved", "exit 1\n");
        let db = Db::new("panic_moved");
        let m0 = source_main(&source);
        let mission = mission_at_baseline(&db, &source, &scenarios, &m0);
        let aborts = gated(&db, mission).gate_aborts;
        let task = db.reported_task(mission, "w");
        let m1 = main_moves(&base, &source, "main.txt");
        db.store.source_moved(&source, &m0, &m1).expect("moved");
        for _ in 0..3 {
            super::judged_or_aborted(&db.store, &ready_task(&db, task), &m0, mission, || {
                panic!("a bug in the judging")
            });
        }
        assert_eq!(verdicts(&db, task), vec!["stale"; 3]);
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["found"], "aborted", "{record}");
        assert_eq!(record["main_now"], m1.as_str());
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("a bug in the judging")),
            "{record}"
        );
        assert_eq!(gated(&db, mission).gate_aborts, aborts, "none counted");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let board = db.mission(mission);
        assert!(board["halted"].is_null(), "{board}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Main cannot be read when the gate ends (the source is gone for the
    /// moment): the verdict is recorded as found, as before the stale check,
    /// with the read's error as `main_read`, so the record says main was
    /// never checked. The same for a judging that panicked.
    #[test]
    fn a_verdict_whose_main_cannot_be_read_is_recorded_as_found_with_main_read() {
        let marks = fresh_marks("spawn-main-unread");
        let release = Release(marks.clone());
        let gate = gate_holding_then(
            &marks,
            "",
            "{\"verdict\":\"rejected\",\"reason\":\"regression: 012.sh must still pass\"}",
            0,
        );
        let (base, source, scenarios, integrator) = world("main-unread", &gate);
        let db = Db::new("main_unread");
        let m0 = source_main(&source);
        let mission = mission_at_baseline(&db, &source, &scenarios, &m0);
        let task = db.reported_task(mission, "w");
        let agent = agent_of(&db, mission, task);
        push_work(&base, &source, &agent, "work.txt");
        let away = base.join("source-away.git");
        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(wait_started(&marks, 1).len(), 1, "the gate started");
        std::fs::rename(&source, &away).expect("the source goes away");
        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "rejected", "as found: {record}");
        assert_eq!(record["base"], m0.as_str());
        assert!(
            record["main_read"]
                .as_str()
                .is_some_and(|error| !error.is_empty()),
            "{record}"
        );
        assert!(record.get("main_now").is_none(), "{record}");

        // A judging that panics while the source is away: aborted, as
        // before, and it says main was not read.
        let other = db.reported_task(mission, "x");
        super::judged_or_aborted(&db.store, &ready_task(&db, other), &m0, mission, || {
            panic!("a bug in the judging")
        });
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "aborted", "{record}");
        assert!(record["main_read"].is_string(), "{record}");
        std::fs::rename(&away, &source).expect("the source is back");
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&marks);
    }

    /// The cap counts per main: only the host faults judged against the main
    /// being judged now. Supply PA's task 207 (2026-11-09): its host fault of
    /// 01:45 on fcf5e1e (a red main) still counted at 08:32, on a main that
    /// had moved since. With the cap at 1, a host fault on A reaches it; main
    /// moves to B (the owner's push, recorded); the next host fault, on B, is
    /// `host_fault` again, the first on B, never capped into an abort.
    #[test]
    fn host_faults_on_an_older_main_count_nothing_toward_the_cap_on_this_one() {
        let gate = "echo '| == regression: check-tests.sh must still pass'\n\
                    echo '| FAILED api::tests::one: error connecting to server: Connection refused (os error 111)'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
        let (base, source, scenarios, mut integrator) = world("cap-per-main", gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_millis(1),
            cap: 1,
        };
        let db = Db::new("cap_per_main");
        let a = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = agent_of(&db, mission, task);
        push_work(&base, &source, &agent, "work.txt");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let first = last_verdict(&db).0;
        assert_eq!(first["verdict"], "host_fault", "{first}");
        assert_eq!(first["base"], a.as_str());
        assert_eq!(db.store.task_host_faults(task, 0, &a).expect("count"), 1);

        let b = main_moves(&base, &source, "main.txt");
        db.store.source_moved(&source, &a, &b).expect("moved");
        assert_eq!(
            db.store.task_host_faults(task, 0, &b).expect("count"),
            0,
            "none on B yet"
        );
        past_due(&db);
        integrator.pass_on(&db.store).expect("a pass on B");
        integrator.settle();
        let second = last_verdict(&db).0;
        assert_eq!(
            second["verdict"], "host_fault",
            "A's fault is not B's: {second}"
        );
        assert_eq!(second["base"], b.as_str());
        assert_eq!(db.store.task_host_faults(task, 0, &b).expect("count"), 1);
        assert_eq!(gated(&db, mission).gate_aborts, 0);

        // On B, the cap holds as before: the next one is an abort.
        past_due(&db);
        integrator.pass_on(&db.store).expect("a third pass");
        integrator.settle();
        let third = last_verdict(&db).0;
        assert_eq!(third["verdict"], "aborted", "{third}");
        assert!(
            third["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains(&format!(
                    "has had 1 at report revision 0 on main {}",
                    &b[..12]
                ))),
            "{third}"
        );
        assert_eq!(gated(&db, mission).gate_aborts, 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A gate verdict's `flakes` (checks that failed once and passed again)
    /// are on the task's `task.gated` record; a verdict without them leaves
    /// the record without the key, as every record before it.
    #[test]
    fn a_verdicts_flakes_are_on_its_record() {
        let gate = "printf '{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\",\"flakes\":[\"check-e2e.sh\"]}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
        let (base, source, scenarios, integrator) = world("gate-flakes", gate);
        let db = Db::new("gate_flakes");
        let mission = db.gated_mission(&source, &scenarios);
        let _task = db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        push_work(&base, &source, &agent, "work.txt");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["flakes"], json!(["check-e2e.sh"]), "{record}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A gate verdict's `main_red_scenarios` (scenarios solved on an
    /// ancestor of the base and unsolved on it) are on the task's
    /// `task.gated` record, whatever the verdict; a verdict without them
    /// leaves the record without the key.
    #[test]
    fn a_verdicts_red_scenarios_are_on_its_record() {
        let gate = "printf '{\"verdict\":\"accepted\",\"main_red_scenarios\":[\"046.sh\"]}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
        let (base, source, scenarios, integrator) = world("gate-red-scenarios", gate);
        let db = Db::new("gate_red_scenarios");
        let mission = db.gated_mission(&source, &scenarios);
        let _task = db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        push_work(&base, &source, &agent, "work.txt");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["main_red_scenarios"], json!(["046.sh"]), "{record}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A gate verdict without `main_red_scenarios` (every gate before it)
    /// leaves the record without the key.
    #[test]
    fn a_verdict_without_red_scenarios_has_none_on_its_record() {
        let gate =
            "printf '{\"verdict\":\"rejected\",\"reason\":\"r\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
        let (base, source, scenarios, integrator) = world("gate-no-red-scenarios", gate);
        let db = Db::new("gate_no_red_scenarios");
        let mission = db.gated_mission(&source, &scenarios);
        let _task = db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        push_work(&base, &source, &agent, "work.txt");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let (record, _, _) = last_verdict(&db);
        assert!(record.get("main_red_scenarios").is_none(), "{record}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A base-only host fault (the gate's fact `on` `base`, before any
    /// candidate code ran) counts toward no halt, so it never counts toward
    /// the cap and is never capped: with the cap at 2, a gate that keeps
    /// reporting a base harness error is `host_fault` every time, and the
    /// task never aborts.
    #[test]
    fn base_only_host_faults_never_reach_the_cap() {
        let gate = "echo '| == base: 012.sh'\n\
                    printf '{\"host_fault\":true,\"scenario\":\"012.sh\",\"on\":\"base\",\"runs\":\"1, then 0\",\"reason\":\"012.sh exited 1, then 0 on the base\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                    exit 6\n";
        let (base, source, scenarios, mut integrator) = world("gate-fault-base-cap", gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_secs(1),
            cap: 2,
        };
        let db = Db::new("gate_host_fault_base_cap");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        push_work(&base, &source, &agent, "work.txt");
        let mut due = None;
        for round in 0..4 {
            if let Some(due) = due {
                let left = due - super::now_ms();
                if left > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(
                        u64::try_from(left).unwrap_or(0) + 50,
                    ));
                }
            }
            integrator.pass_on(&db.store).expect("a pass");
            integrator.settle();
            let record = last_verdict(&db).0;
            assert_eq!(record["verdict"], "host_fault", "round {round}: {record}");
            assert_eq!(record["fact"]["on"], "base", "{record}");
            due = record["due_ms"].as_i64();
        }
        assert_eq!(
            db.store
                .task_host_faults(task, 0, &source_main(&source))
                .expect("count"),
            0,
            "base-only faults are not counted"
        );
        assert_eq!(gated(&db, mission).gate_aborts, 0);
        assert_eq!(gated(&db, mission).gate_host_faults, 4, "each one a run");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A check that failed on the candidate and on the base (the gate's fact
    /// `on` `candidate and base`) fails on main without the candidate, so it
    /// counts toward no halt, never counts toward the cap and is never
    /// capped: with the cap at 2, a gate that keeps reporting it (a red main:
    /// Supply PA's check-tests, 2026-11-09 00:48-03:42) is `host_fault` every
    /// time, and the task never aborts.
    #[test]
    fn candidate_and_base_host_faults_never_reach_the_cap() {
        let gate = "echo '| == regression: check-e2e.sh must still pass'\n\
                    printf '{\"host_fault\":true,\"scenario\":\"check-e2e.sh\",\"on\":\"candidate and base\",\"runs\":\"1, then 1 on the candidate; 1 on the base\",\"reason\":\"check-e2e.sh failed twice on the candidate and on the base\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                    exit 6\n";
        let (base, source, scenarios, mut integrator) = world("gate-fault-both-cap", gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_secs(1),
            cap: 2,
        };
        let db = Db::new("gate_host_fault_both_cap");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        push_work(&base, &source, &agent, "work.txt");
        let mut due = None;
        for round in 0..4 {
            if let Some(due) = due {
                let left = due - super::now_ms();
                if left > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(
                        u64::try_from(left).unwrap_or(0) + 50,
                    ));
                }
            }
            integrator.pass_on(&db.store).expect("a pass");
            integrator.settle();
            let record = last_verdict(&db).0;
            assert_eq!(record["verdict"], "host_fault", "round {round}: {record}");
            assert_eq!(record["fact"]["on"], "candidate and base", "{record}");
            due = record["due_ms"].as_i64();
        }
        assert_eq!(
            db.store
                .task_host_faults(task, 0, &source_main(&source))
                .expect("count"),
            0,
            "faults on the candidate and the base are not counted"
        );
        assert_eq!(gated(&db, mission).gate_aborts, 0);
        assert_eq!(gated(&db, mission).gate_host_faults, 4, "each one a run");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A gate that rejects a merge (`$3^2` exists) with check-tests.sh failing
    /// on it, and gives the head alone, judged against the base it was built
    /// on, the host fault `head_fault` (a verdict file's JSON) with exit 6.
    fn head_alone_gate(head_fault: &str) -> String {
        format!(
            "if git --git-dir=\"$1\" rev-parse -q --verify \"$3^2\" >/dev/null; then\n\
               echo ' FAIL  test/health.test.ts > shifts what the server stamps'\n\
               printf '{{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               exit 0\n\
             fi\n\
             echo '| == the head alone'\n\
             printf '%s\\n' '{head_fault}' >\"$SPAWN_GATE_VERDICT\"\n\
             exit 6\n"
        )
    }

    /// The task's work, made on `on` (an older main) and pushed as
    /// `spawn/<agent>`: one new file. The pushed commit.
    fn push_work_on(base: &Path, source: &str, agent: &str, on: &str, file: &str) -> String {
        let clone = base.join(format!("agent-on-{file}"));
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", source, &clone_path]).expect("clone");
        git(&["-C", &clone_path, "checkout", "-q", on]).expect("checkout the older main");
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

    /// Supply PA's task 207 (2026-11-09): a rejected merge's head, judged
    /// alone against the base it was built on, gave no verdict (a host
    /// fault there). The record says so through the store: its reason names
    /// the merge's rejection, the head's base and that the fault is the head's
    /// run there, never a fault of main; `head` and `head_base` are on it; and
    /// the supervisor's digest item, at the second in a row, says it was the
    /// work alone on that base.
    #[test]
    fn a_head_judged_alone_without_a_verdict_says_it_was_the_head_on_its_base() {
        let fault = "{\"host_fault\":true,\"scenario\":\"011.sh\",\"on\":\"base\",\"runs\":\"2\",\"reason\":\"a harness error\"}";
        let (base, source, scenarios, mut integrator) =
            world("head-alone-fault", &head_alone_gate(fault));
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_secs(1),
            cap: 3,
        };
        let db = Db::new("head_alone_fault");
        let m0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        let m1 = main_moves(&base, &source, "main.txt");
        let head = push_work_on(&base, &source, &agent, &m0, "work.txt");
        let mut due = None;
        for _ in 0..2 {
            if let Some(due) = due {
                let left = due - super::now_ms();
                if left > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(
                        u64::try_from(left).unwrap_or(0) + 50,
                    ));
                }
            }
            integrator.pass_on(&db.store).expect("a pass");
            integrator.settle();
            due = last_verdict(&db).0["due_ms"].as_i64();
        }
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["head"], head.as_str(), "{record}");
        assert_eq!(record["head_base"], m0.as_str(), "{record}");
        assert_eq!(record["base"], m1.as_str(), "the merge's base, as before");
        let reason = record["reason"].as_str().expect("a reason");
        for wanted in [
            &format!("the merge with main {} was rejected", &m1[..12]),
            "regression: check-tests.sh fails on the candidate",
            &format!(
                "judging the work alone against the base it was built on, {}, gave no verdict",
                &m0[..12]
            ),
            "011.sh exited 2 on the base",
        ] {
            assert!(reason.contains(wanted), "{wanted:?} in {reason}");
        }
        assert_eq!(record["escalate"], true, "the second in a row: {record}");
        let items = crate::dispatch::digest::host_fault_escalations(
            db.store
                .gate_host_fault_escalations()
                .expect("escalations")
                .as_array()
                .expect("records"),
        );
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(
            items[0].line.contains(&format!(
                "judging the work alone against the base it was built on ({}), after main \
                 rejected the merge",
                &m0[..12]
            )),
            "{}",
            items[0].line
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The other half of task 207: the head alone fails the check on its own
    /// base too (that base was a red main), so it cannot be judged there. The
    /// merge's rejection stands, through the store: `rejected`, counted, with
    /// the merge's reason and log (the failing test), `head_verdict`
    /// `unjudged` and the reason, and a note that says so; no host fault, no
    /// backoff, no run of host faults. Both shapes the gate gives a check
    /// failing on a base: `candidate and base`, and a red main.
    #[test]
    fn a_head_whose_own_base_fails_the_check_leaves_the_merges_rejection_standing() {
        for (tag, fault) in [
            (
                "both",
                "{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"candidate and base\",\"runs\":\"1, then 1 on the candidate; 1 on the base\",\"reason\":\"r\"}",
            ),
            (
                "red",
                "{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"main\",\"runs\":\"1, then 1 on the candidate; 1, then 1 on the base\",\"reason\":\"r\",\"failures\":[{\"test\":\"api::slice_11\",\"message\":\"expected 23 to be 21\"}]}",
            ),
        ] {
            let (base, source, scenarios, mut integrator) =
                world(&format!("head-unjudged-{tag}"), &head_alone_gate(fault));
            integrator.judge.host = super::HostSettings {
                server_log: None,
                backoff: std::time::Duration::from_secs(1),
                cap: 3,
            };
            let db = Db::new(&format!("head_unjudged_{tag}"));
            let m0 = source_main(&source);
            let mission = db.gated_mission(&source, &scenarios);
            db.reported_task(mission, "w");
            let agent = db.mission(mission)["tasks"][0]["agent_id"]
                .as_str()
                .expect("the task's agent")
                .to_string();
            main_moves(&base, &source, "main.txt");
            let head = push_work_on(&base, &source, &agent, &m0, "work.txt");
            integrator.pass_on(&db.store).expect("a pass");
            integrator.settle();
            let (record, _, _) = last_verdict(&db);
            assert_eq!(record["verdict"], "rejected", "{tag}: {record}");
            assert_eq!(
                record["reason"], "regression: check-tests.sh fails on the candidate",
                "{tag}: the merge's reason"
            );
            assert!(
                record["log"]
                    .as_str()
                    .is_some_and(|log| log.contains("shifts what the server stamps")),
                "{tag}: the merge's log, with its failing test: {record}"
            );
            assert_eq!(record["head"], head.as_str());
            assert_eq!(record["head_base"], m0.as_str());
            assert_eq!(record["head_verdict"], "unjudged", "{tag}: {record}");
            let why = record["head_reason"].as_str().expect("why");
            assert!(
                why.contains(&m0[..12]) && why.contains("check-tests.sh fails there too"),
                "{tag}: {why}"
            );
            let note = record["note"].as_str().expect("a note");
            assert!(
                note.starts_with("The gate rejected this work: regression: check-tests.sh")
                    && note.contains("shifts what the server stamps")
                    && note.contains("so the gate's rejection of the merge with main stands"),
                "{tag}: {note}"
            );
            assert!(
                record.get("backoff_ms").is_none(),
                "{tag}: no backoff: {record}"
            );
            let after = gated(&db, mission);
            assert_eq!(after.gate_host_faults, 0, "{tag}: no run of host faults");
            assert_eq!(after.gate_aborts, 0);
            assert_eq!(after.gate_rejections, 1, "{tag}: a rejection, counted");
            let board = db.mission(mission);
            assert_eq!(
                board["tasks"][0]["state"], "revise",
                "{tag}: sent back: {board}"
            );
            assert!(
                control_records(&db, "source.main_red").is_empty(),
                "{tag}: the head's old base is not the source's main"
            );
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// Main is red on a check (design-main-red.md), through the store. A gate
    /// that finds check-tests.sh failing twice on the base with the same test
    /// (an assertion: the work's class) gives the source one red,
    /// `source.main_red` with `alert`, and the task a `host_fault` with
    /// evidence `main_red` that starts no run and is never capped (the cap
    /// here is 1). The red main holds the source's gates: the next pass
    /// starts none. Once the probe interval has passed, one gate runs anyway,
    /// a probe (`source.main_probe`, the record's `probe`), with the red check
    /// given to the gate (SPAWN_GATE_BASE_CHECK); the store takes one probe
    /// per interval. When the red check passes on the base, the source is
    /// green again (`source.main_green`, its red's `since_ms`), the work is
    /// judged as usual, and the supervisor is told of the red once and of the
    /// green once.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_red_main_holds_its_sources_gates_until_a_probe_finds_it_green() {
        let dir = std::env::temp_dir().join(format!("spawn-db-main-red-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let (runs, green) = (dir.join("runs"), dir.join("green"));
        let (runs_path, green_path) = (runs.display(), green.display());
        let gate = format!(
            "echo \"base_check=${{SPAWN_GATE_BASE_CHECK-}}\" >>'{runs_path}'\n\
             if [ -e '{green_path}' ]; then\n\
               echo '| base check: check-tests.sh passes on the base: main is green on it again'\n\
               if grep -q fault '{green_path}'; then\n\
                 printf '{{\"host_fault\":true,\"scenario\":\"check-e2e.sh\",\"on\":\"candidate and base\",\"runs\":\"1, then 1 on the candidate; 1, then 0 on the base\",\"reason\":\"r\",\"base_check\":\"check-tests.sh\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                 exit 6\n\
               fi\n\
               printf '{{\"verdict\":\"accepted\",\"base_check\":\"check-tests.sh\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               exit 0\n\
             fi\n\
             echo '| == regression: check-tests.sh must still pass'\n\
             printf '{{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"main\",\"runs\":\"1, then 1 on the candidate; 1, then 1 on the base\",\"reason\":\"r\",\"failures\":[{{\"test\":\"api::tests::adds\",\"message\":\"assertion `left == right` failed\"}}]}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
             exit 6\n"
        );
        let (base, source, scenarios, mut integrator) = world("main-red", &gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_secs(1),
            cap: 1,
        };
        let db = Db::new("main_red");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        push_work(&base, &source, &agent, "work.txt");
        let main = source_main(&source);
        let gates = || {
            std::fs::read_to_string(&runs)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect::<Vec<String>>()
        };
        let pass = |integrator: &Integrator| {
            integrator.pass_on(&db.store).expect("a pass");
            integrator.settle();
        };

        // The first gate finds main red: one red for the source, the task's
        // host_fault with no run, no backoff, no count.
        pass(&integrator);
        assert_eq!(gates(), ["base_check="], "no red yet, so no base check");
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["evidence"], super::MAIN_RED, "{record}");
        assert_eq!(record["fact"]["on"], "main");
        assert_eq!(record["fact"]["main"], main.as_str());
        assert_eq!(record["backoff_ms"], 0);
        assert!(record.get("probe").is_none(), "{record}");
        let reds = control_records(&db, "source.main_red");
        assert_eq!(reds.len(), 1);
        assert_eq!(reds[0]["alert"], true, "the first of the red: {}", reds[0]);
        assert_eq!(reds[0]["scenario"], "check-tests.sh");
        assert_eq!(reds[0]["failures"][0]["test"], "api::tests::adds");
        assert_eq!(reds[0]["failures"][0]["class"], "work");
        let after = gated(&db, mission);
        let red = after.red.clone().expect("the source is red");
        assert_eq!(
            (red.main.as_str(), red.scenario.as_str()),
            (main.as_str(), "check-tests.sh")
        );
        assert_eq!(
            (after.gate_host_faults, after.gate_aborts),
            (0, 0),
            "no run, no count"
        );
        assert_eq!(
            db.store
                .task_host_faults(task, 0, &source_main(&source))
                .expect("count"),
            0
        );

        // Told once: the red's alert.
        let news = crate::dispatch::digest::main_red_news(
            db.store
                .main_red_news()
                .expect("news")
                .as_array()
                .expect("records"),
        );
        assert_eq!(news.len(), 1, "{news:?}");
        assert_eq!(news[0].why, crate::dispatch::Why::MainRed);
        assert!(
            news[0].line.contains("is red on check-tests.sh")
                && news[0].line.contains("api::tests::adds"),
            "{}",
            news[0].line
        );

        // The red main holds the source's gates: no gate, no record.
        let verdicts = control_records(&db, "task.gated").len();
        pass(&integrator);
        assert_eq!(gates().len(), 1, "held: no gate ran");
        assert_eq!(control_records(&db, "task.gated").len(), verdicts);

        // Past the probe interval: one gate anyway, a probe, given the red
        // check; still red, so a second red record, no alert, still no count.
        integrator.red_probe_ms = 1;
        std::thread::sleep(std::time::Duration::from_millis(5));
        // A probe whose gate does not start this pass (no CPU room: another
        // source's gate holds the cores) is not spent: no probe record, no
        // gate.
        let (release, waiting) = std::sync::mpsc::channel::<()>();
        integrator.running.borrow_mut().push(super::Running {
            mission: 0,
            source: "another source".to_string(),
            task: 0,
            shares_server: false,
            jobs: 2,
            owner: None,
            stop: std::sync::Arc::new(super::Stop::default()),
            halt: std::sync::Arc::new(crate::gate::Halt::default()),
            handle: std::thread::spawn(move || {
                let _ = waiting.recv();
            }),
        });
        integrator.cpu = Some(crate::gate::Cpu {
            cores: 2,
            reserved: 1,
            jobs: Some(1),
        });
        // No settle: it would wait for the holding thread.
        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(gates().len(), 1, "no CPU room: no probe gate");
        assert!(
            control_records(&db, "source.main_probe").is_empty(),
            "no probe spent"
        );
        integrator.cpu = None;
        drop(release);
        integrator.settle();
        pass(&integrator);
        assert_eq!(gates(), ["base_check=", "base_check=check-tests.sh"]);
        assert_eq!(control_records(&db, "source.main_probe").len(), 1);
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["probe"], true, "{record}");
        assert_eq!(
            record["verdict"], "host_fault",
            "the cap of 1 never caps it: {record}"
        );
        let reds = control_records(&db, "source.main_red");
        assert_eq!(reds.len(), 2);
        assert_eq!(reds[1]["alert"], false, "{}", reds[1]);
        assert_eq!(gated(&db, mission).gate_aborts, 0);
        // The store takes one probe per interval: none is due an hour after
        // the last.
        assert!(
            db.store
                .integrator_record(&json!({"type": "source.main_probe", "source": source,
                                           "main": main, "task": task, "probe_ms": 3_600_000}))
                .is_err()
        );
        // A red that names no failure is refused.
        assert!(
            db.store
                .integrator_record(&json!({"type": "source.main_red", "source": source,
                                           "main": main, "scenario": "check-tests.sh",
                                           "failures": [], "task": task}))
                .is_err()
        );

        // The red check passes on the base, and the gate then ends with a
        // host fault on another check: green again all the same (the gate
        // says it in any fact), and the fault recorded as usual.
        std::fs::write(&green, "fault").expect("green");
        std::thread::sleep(std::time::Duration::from_millis(5));
        pass(&integrator);
        assert_eq!(gates().len(), 3);
        let greens = control_records(&db, "source.main_green");
        assert_eq!(greens.len(), 1);
        assert_eq!(greens[0]["scenario"], "check-tests.sh");
        assert!(
            greens[0]["since_ms"].is_i64(),
            "the red's since: {}",
            greens[0]
        );
        assert!(gated(&db, mission).red.is_none(), "green: no red");
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["fact"]["scenario"], "check-e2e.sh", "{record}");
        assert_eq!(
            record["probe"], true,
            "the gate that found it green was a probe"
        );
        // Green, the source's gates run as usual: after the fault's backoff,
        // the work is judged and accepted, no probe and no base check.
        std::fs::write(&green, "").expect("green");
        if let Some(due) = record["due_ms"].as_i64() {
            let left = due - super::now_ms();
            if left > 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    u64::try_from(left).unwrap_or(0) + 50,
                ));
            }
        }
        pass(&integrator);
        assert_eq!(
            gates().last().map(String::as_str),
            Some("base_check="),
            "{:?}",
            gates()
        );
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "accepted", "{record}");
        assert!(record.get("probe").is_none(), "{record}");
        let news = crate::dispatch::digest::main_red_news(
            db.store
                .main_red_news()
                .expect("news")
                .as_array()
                .expect("records"),
        );
        assert!(
            news.iter()
                .any(|item| item.why == crate::dispatch::Why::MainGreen
                    && item.line.contains("passes check-tests.sh again")),
            "{news:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Objective 033's evidence beside the cap (this objective's): a base
    /// that disagreed with itself is a base-only fault, before any candidate
    /// code, so it counts toward no halt and never reaches
    /// `SPAWN_GATE_HOST_FAULT_CAP`: with the cap at 2, four disagreements in
    /// a row are `host_fault` every time, `base_disagrees` every time, and
    /// the task never aborts (the store's count leaves them out too).
    #[test]
    fn base_disagreements_never_reach_the_cap() {
        let gate = "echo '| == base: 011.sh'\n\
                    echo '| 011.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts'\n\
                    echo '| stage gate aborted 12003ms base: 011.sh again (unsolved needs two runs that agree)'\n\
                    exit 4\n";
        let (base, source, scenarios, mut integrator) = world("base-disagrees-cap", gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_secs(1),
            cap: 2,
        };
        let db = Db::new("base_disagrees_cap");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        push_work(&base, &source, &agent, "work.txt");
        let mut due = None;
        for round in 0..4 {
            if let Some(due) = due {
                let left = due - super::now_ms();
                if left > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(
                        u64::try_from(left).unwrap_or(0) + 50,
                    ));
                }
            }
            integrator.pass_on(&db.store).expect("a pass");
            integrator.settle();
            let record = last_verdict(&db).0;
            assert_eq!(record["verdict"], "host_fault", "round {round}: {record}");
            assert_eq!(record["evidence"], "base_disagrees", "{record}");
            due = record["due_ms"].as_i64();
        }
        assert_eq!(
            db.store
                .task_host_faults(task, 0, &source_main(&source))
                .expect("count"),
            0,
            "a base's disagreement is not counted"
        );
        let after = gated(&db, mission);
        assert_eq!(after.gate_aborts, 0);
        assert_eq!(after.gate_host_faults, 4, "each one a run");
        assert_eq!(after.gate_base_disagrees, 4, "each one a disagreement");
        let _ = std::fs::remove_dir_all(&base);
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

    /// Objective 33, end to end: a gate that exits 4 after its own pair of
    /// lines (a scenario of the base exiting 1 and then 0) is recorded
    /// `host_fault` with the evidence `base_disagrees`, the gate's own
    /// scenario (not a nested gate's 102.sh), both exits and its setup
    /// seconds; the task stays reported and is not judged again before the
    /// record's `due_ms`; three in a row double the backoff, share 020's run,
    /// never escalate and carry the alert at the third; a base run that
    /// exits 2, or `1, then 2`, is an abort and resets both runs; the
    /// accepted run lands and the mission is never halted.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_base_that_disagrees_with_itself_is_a_host_fault_judged_again() {
        let dir =
            std::env::temp_dir().join(format!("spawn-db-base-disagrees-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let runs = dir.join("runs");
        let runs_path = runs.display();
        let template = r#"
n=$(cat 'RUNS' 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" >'RUNS'
disagrees() {
  echo '| == base: 011.sh'
  echo '| stage gate exit 1 74210ms base: 011.sh'
  echo '| == base: 011.sh again (unsolved needs two runs that agree)'
  if [ "$2" = nested ]; then
    echo '| | 102.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts'
    echo '| | stage gate aborted 5ms base: 102.sh again (unsolved needs two runs that agree)'
  fi
  echo '| 011.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts'
  echo '| stage gate aborted 66871ms base: 011.sh again (unsolved needs two runs that agree)'
  echo "| gate time: base 141 s (2 scenario runs, 19 results from the cache), candidate 0 s, setup $1 s (exports, socket scan, static checks), total 385 s"
  exit 4
}
case "$n" in
1) disagrees 244 nested ;;
2) disagrees 31 ;;
3) disagrees 29 ;;
4)
  echo '| == base: 011.sh'
  echo '| 011.sh exited 2 on the base: a harness error, not a verdict; the gate aborts'
  echo '| stage gate aborted 5120ms base: 011.sh again (unsolved needs two runs that agree)'
  exit 4
  ;;
5)
  echo '| == base: 011.sh'
  echo '| 011.sh exited 1, then 2 on the base: a harness error, not a verdict; the gate aborts'
  echo '| stage gate aborted 12003ms base: 011.sh again (unsolved needs two runs that agree)'
  exit 4
  ;;
esac
printf '{"verdict":"accepted"}\n' >"$SPAWN_GATE_VERDICT"
"#;
        let gate = template.replace("RUNS", &runs_path.to_string());
        let (base, source, scenarios, mut integrator) = world("base-disagrees-pass", &gate);
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_secs(2),
            cap: super::HOST_FAULT_CAP,
        };
        let db = Db::new("base_disagrees_pass");
        let mission = db.gated_mission(&source, &scenarios);
        let _task = db.reported_task(mission, "w");
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
        let state = |db: &Db| {
            let after = gated(db, mission);
            (after.tasks[0].state, after.tasks[0].revisions)
        };

        // (1) Run 1: the base disagreed with itself.
        pass();
        assert_eq!(count(), 1);
        let (record, event, at) = last_verdict(&db);
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["evidence"], "base_disagrees");
        assert_eq!(
            record["scenario"], "011.sh",
            "the gate's own line, not 102.sh"
        );
        assert_eq!(record["exits"], json!([1, 0]));
        assert_eq!(record["setup_s"], 244);
        assert_eq!(record["host_faults"], 1);
        assert_eq!(record["base_disagrees"], 1);
        assert_eq!(record["escalate"], false);
        assert_eq!(record["alert"], false);
        assert_eq!(record["backoff_ms"], 2000);
        assert_eq!(record["due_ms"], at + 2000, "its own time plus the backoff");
        assert!(record["candidate"].is_string());
        assert!(
            record["log"].as_str().is_some_and(|log| {
                log.contains(
                    "| 011.sh exited 1, then 0 on the base: a harness error, not a verdict; the \
                     gate aborts",
                ) && log.contains("| | 102.sh exited 1, then 0")
            }),
            "{record}"
        );
        assert!(record["reason"].as_str().is_some_and(|reason| {
            reason
                .starts_with("host fault: the base disagrees with itself: 011.sh exited 1, then 0")
                && reason.contains("244 s")
                && reason.ends_with("not a verdict, judged again later")
        }));
        let after = gated(&db, mission);
        assert_eq!((after.gate_host_faults, after.gate_base_disagrees), (1, 1));
        assert_eq!((after.gate_rejections, after.gate_aborts), (0, 0));
        assert_eq!(
            state(&db),
            (spawn_core::mission::TaskState::Reported, 0),
            "nothing went back to the worker"
        );
        assert!(after.halted.is_none());
        assert_eq!(
            db.store
                .gate_base_disagrees_alerts()
                .expect("alerts")
                .as_array()
                .map(Vec::len),
            Some(0),
            "the first in a row is no alert"
        );

        // (2) Held until its due time, then judged again.
        pass();
        assert_eq!(count(), 1, "not judged again before due_ms");
        assert_eq!(last_verdict(&db).1, event);
        wait_until(at + 2000);

        // (3) Run 2: the second in a row, the backoff doubled, no escalation.
        pass();
        assert_eq!(count(), 2);
        let (record, _, at) = last_verdict(&db);
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["host_faults"], 2);
        assert_eq!(record["base_disagrees"], 2);
        assert_eq!(record["setup_s"], 31);
        assert_eq!(record["backoff_ms"], 4000);
        assert_eq!(record["due_ms"], at + 4000);
        assert_eq!(record["escalate"], false, "never, whatever the count");
        assert_eq!(record["alert"], false);
        assert_eq!(gated(&db, mission).gate_aborts, 0);
        assert_eq!(
            db.store
                .gate_host_fault_escalations()
                .expect("escalations")
                .as_array()
                .map(Vec::len),
            Some(0),
            "a base disagreement is no escalation"
        );

        // (4) Run 3: three in a row, the alert, and no halt.
        pass();
        assert_eq!(count(), 2, "held again, for the doubled backoff");
        wait_until(at + 4000);
        pass();
        let (record, alert_event, at) = last_verdict(&db);
        assert_eq!(record["verdict"], "host_fault", "{record}");
        assert_eq!(record["host_faults"], 3);
        assert_eq!(record["base_disagrees"], 3);
        assert_eq!(record["backoff_ms"], 8000);
        assert_eq!(record["escalate"], false);
        assert_eq!(record["alert"], true, "three in a row raise one alert");
        let after = gated(&db, mission);
        assert_eq!((after.gate_host_faults, after.gate_base_disagrees), (3, 3));
        assert!(after.halted.is_none(), "no halt counts a host fault");
        assert_eq!(after.gate_aborts, 0);
        let alerts = db.store.gate_base_disagrees_alerts().expect("alerts");
        assert_eq!(alerts.as_array().map(Vec::len), Some(1), "{alerts}");
        assert_eq!(alerts[0]["event_id"], alert_event);
        assert_eq!(alerts[0]["scenario"], "011.sh");

        // (5) Control: a real harness error on the base is an abort, counts,
        // and ends both runs.
        wait_until(at + 8000);
        pass();
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "aborted", "{record}");
        let after = gated(&db, mission);
        assert_eq!(
            (
                after.gate_aborts,
                after.gate_host_faults,
                after.gate_base_disagrees
            ),
            (1, 0, 0),
            "a base run that exited 2 is an abort and resets both runs"
        );
        pass();
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "aborted", "1, then 2 is an abort too");
        assert_eq!(gated(&db, mission).gate_aborts, 2);
        assert!(gated(&db, mission).halted.is_none());

        // Run 6 accepts and lands, ending both runs.
        pass();
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "accepted", "{record}");
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
        let after = gated(&db, mission);
        assert_eq!(
            (
                after.gate_host_faults,
                after.gate_base_disagrees,
                after.gate_aborts
            ),
            (0, 0, 0)
        );
        assert!(after.halted.is_none());
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&dir);
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

    // Objective 21: the owner's landings (`spawn mission push`), step 0.

    /// A gated mission on `source` whose baseline is `baseline` (its first
    /// verdict's base), with nothing left to judge.
    fn mission_at_baseline(db: &Db, source: &str, scenarios: &Path, baseline: &str) -> i64 {
        let mission = db.gated_mission(source, scenarios);
        let task = db.reported_task(mission, "w");
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "aborted", "base": baseline, "reason": "a gate that died"}));
        db.store.task_cancel(task).expect("nothing left to judge");
        assert_eq!(db.mission(mission)["baseline"], baseline);
        mission
    }

    /// The owner's commit on `source`'s main as it is now (`file` holding
    /// `text`, subject `subject`), put in the source at
    /// `refs/spawn-owner/<commit>` as `spawn mission push` does: no branch
    /// moves. The commit.
    fn owner_commit(base: &Path, source: &str, file: &str, text: &str, subject: &str) -> String {
        let clone = base.join(format!("owner-{file}-{}", text.len()));
        let clone_path = clone.to_string_lossy().to_string();
        let _ = std::fs::remove_dir_all(&clone);
        git(&["clone", "-q", source, &clone_path]).expect("the owner's clone");
        std::fs::write(clone.join(file), text).expect("write");
        super::tests::commit_in(&clone_path, subject);
        let commit = git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("the commit");
        let reference = super::owner::owner_ref(&commit);
        git(&[
            "-C",
            &clone_path,
            "push",
            "-q",
            "origin",
            &format!("{commit}:{reference}"),
        ])
        .expect("into the source, no branch");
        commit
    }

    /// The owner's request for `commit` on `source`: its event id.
    fn owner_request(db: &Db, source: &str, commit: &str) -> i64 {
        db.store
            .source_push_request(
                source,
                commit,
                &super::owner::owner_ref(commit),
                &spawn_postgres::PushRequest::default(),
            )
            .expect("the owner's request")
    }

    /// The control records of `kind`, oldest first.
    fn control_records(db: &Db, kind: &str) -> Vec<Value> {
        db.admin()
            .query(
                "select kind -> 'detail' -> 'record' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = $1
                  order by session_seq",
                &[&kind],
            )
            .expect("records")
            .iter()
            .map(|row| row.get(0))
            .collect()
    }

    fn halted(db: &Db, mission: i64) -> Value {
        db.mission(mission)["halted"].clone()
    }

    /// A pending request is applied before the baseline check: main moves
    /// from where the last landing left it to the owner's commit (a
    /// fast-forward), one `source.moved` names the request with `from` and
    /// `to`, the mission's baseline follows, and no pass halts it, although
    /// the integrator's mirror still holds the old main when the pass begins.
    #[test]
    fn an_owner_landing_is_applied_before_the_baseline_check() {
        let (base, source, scenarios, integrator) = world("owner-ff", "exit 1\n");
        let db = Db::new("owner_ff");
        let b0 = source_main(&source);
        let mirror = mirror_of(&integrator, &source);
        let mission = mission_at_baseline(&db, &source, &scenarios, &b0);
        let commit = owner_commit(&base, &source, "CONTRACT.md", "v1\n", "owner: the contract");
        assert_eq!(source_main(&source), b0, "the request moves no branch");
        let request = owner_request(&db, &source, &commit);

        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(source_main(&source), commit, "a fast-forward to the commit");
        let moved = control_records(&db, "source.moved");
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(moved[0]["request"], json!(request));
        assert_eq!(moved[0]["source"], source.as_str());
        assert_eq!(moved[0]["from"], b0.as_str());
        assert_eq!(moved[0]["to"], commit.as_str());
        assert_eq!(moved[0]["by"], "human");
        assert_eq!(moved[0]["missions"], json!([mission]));
        assert_eq!(db.mission(mission)["baseline"], commit.as_str());
        assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
        assert_eq!(
            git(&["--git-dir", &mirror, "rev-parse", "refs/heads/main"]).expect("mirror main"),
            commit,
            "the mission's step read the new main after it moved"
        );
        assert_eq!(
            db.store.source_pushes_pending().expect("pending"),
            json!([])
        );

        for _ in 0..2 {
            integrator.pass_on(&db.store).expect("another pass");
        }
        assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
        assert_eq!(
            control_records(&db, "source.moved").len(),
            1,
            "applied once"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Main moved (a recorded move) since the owner's commit's base: the
    /// landing is a merge commit, first parent main, second the commit, by
    /// the landing identity, its message naming the owner's landing, the
    /// request and the commit's subject; main moves to it, and the
    /// `source.moved` naming the request moves the baseline. Nothing halts.
    #[test]
    fn an_owner_landing_on_a_main_that_moved_is_a_merge() {
        let (base, source, scenarios, integrator) = world("owner-merge", "exit 1\n");
        let db = Db::new("owner_merge");
        let b0 = source_main(&source);
        let mission = mission_at_baseline(&db, &source, &scenarios, &b0);
        let commit = owner_commit(&base, &source, "CONTRACT.md", "v1\n", "owner: the contract");
        let b1 = main_moves(&base, &source, "landed.txt");
        assert_eq!(db.store.source_moved(&source, &b0, &b1).expect("moved"), 1);
        let request = owner_request(&db, &source, &commit);

        integrator.pass_on(&db.store).expect("a pass");
        let main = source_main(&source);
        assert_eq!(parent(&[&source], &main, 1), b1, "first parent: main");
        assert_eq!(parent(&[&source], &main, 2), commit, "second: the commit");
        for file in ["CONTRACT.md", "landed.txt", "README.md"] {
            git(&[
                "--git-dir",
                &source,
                "cat-file",
                "-e",
                &format!("{main}:{file}"),
            ])
            .unwrap_or_else(|error| panic!("{file} in the merge: {error}"));
        }
        let message = git(&["--git-dir", &source, "log", "-1", "--format=%B", &main]).expect("log");
        assert!(
            message.starts_with(&format!(
                "Owner landing (request {request}): owner: the contract"
            )),
            "{message}"
        );
        assert!(
            message.contains(&format!("Spawn-Owner-Request: {request}")),
            "{message}"
        );
        let who = git(&[
            "--git-dir",
            &source,
            "log",
            "-1",
            "--format=%an <%ae> %cn <%ce>",
            &main,
        ])
        .expect("log");
        assert_eq!(
            who,
            "spawn-integrate <integrate@localhost> spawn-integrate <integrate@localhost>"
        );
        let moved: Vec<Value> = control_records(&db, "source.moved")
            .into_iter()
            .filter(|record| record["request"] == json!(request))
            .collect();
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(moved[0]["from"], b1.as_str());
        assert_eq!(moved[0]["to"], main.as_str());
        assert_eq!(db.mission(mission)["baseline"], main.as_str());
        integrator.pass_on(&db.store).expect("another pass");
        assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A commit that conflicts with main is refused: `source.push_refused`
    /// names the request and the reason, main and the baseline stay, no
    /// `source.moved` names it, nothing halts, and the request is no longer
    /// pending. So is a commit the source does not have.
    #[test]
    fn a_conflicting_owner_landing_is_refused_and_moves_nothing() {
        let (base, source, scenarios, integrator) = world("owner-conflict", "exit 1\n");
        let db = Db::new("owner_conflict");
        let b0 = source_main(&source);
        let mission = mission_at_baseline(&db, &source, &scenarios, &b0);
        let commit = owner_commit(
            &base,
            &source,
            "README.md",
            "the owner's\n",
            "owner: readme",
        );
        let b1 = main_moves(&base, &source, "README.md");
        assert_eq!(db.store.source_moved(&source, &b0, &b1).expect("moved"), 1);
        let request = owner_request(&db, &source, &commit);
        let missing = "0123456789abcdef0123456789abcdef01234567";
        let absent = owner_request(&db, &source, missing);

        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(source_main(&source), b1, "main stays");
        let refused = control_records(&db, "source.push_refused");
        assert_eq!(refused.len(), 2, "{refused:?}");
        assert_eq!(refused[0]["request"], json!(request));
        assert!(
            refused[0]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("conflicts")),
            "{}",
            refused[0]
        );
        assert_eq!(refused[1]["request"], json!(absent));
        assert!(
            refused[1]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("not in the source")),
            "{}",
            refused[1]
        );
        assert!(
            control_records(&db, "source.moved")
                .iter()
                .all(|record| record["request"].is_null()),
            "no move names a refused request"
        );
        assert_eq!(db.mission(mission)["baseline"], b1.as_str());
        integrator.pass_on(&db.store).expect("another pass");
        assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
        assert_eq!(
            db.store.source_pushes_pending().expect("pending"),
            json!([])
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A pass that finds a request pending for a source (recorded after step
    /// 0 read the requests, or one whose landing failed after main moved)
    /// holds every halt by baseline for that source, and for no other:
    /// both mains moved behind Spawn's back, only B halts. Then the request
    /// is applied, and the move behind Spawn's back, which no record
    /// explains, still halts A.
    #[test]
    fn a_pending_owner_landing_holds_the_baseline_halt_of_its_source_only() {
        let (base_a, source_a, scenarios, integrator) = world("owner-hold-a", "exit 1\n");
        let (base_b, source_b, _, _) = world("owner-hold-b", "exit 1\n");
        let db = Db::new("owner_hold");
        let a = mission_at_baseline(&db, &source_a, &scenarios, &source_main(&source_a));
        let b = mission_at_baseline(&db, &source_b, &scenarios, &source_main(&source_b));
        main_moves(&base_a, &source_a, "behind-a.txt");
        main_moves(&base_b, &source_b, "behind-b.txt");
        let commit = owner_commit(&base_a, &source_a, "CONTRACT.md", "v1\n", "owner: contract");
        let request = owner_request(&db, &source_a, &commit);

        let held = super::owner::pending_by_source(&db.store).expect("pending");
        assert_eq!(held.get(&source_a), Some(&request));
        assert_eq!(held.len(), 1);
        integrator
            .steps(&db.store, &held, &[], Vec::new())
            .expect("the steps");
        assert!(halted(&db, a).is_null(), "held: {}", halted(&db, a));
        assert!(
            halted(&db, b)
                .as_str()
                .is_some_and(|reason| reason.contains("but the last landing left")),
            "another source is not held: {}",
            halted(&db, b)
        );

        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(source_main(&source_a), commit, "applied");
        let moved = control_records(&db, "source.moved");
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(moved[0]["missions"], json!([]), "A's baseline was not main");
        assert!(
            halted(&db, a)
                .as_str()
                .is_some_and(|reason| reason.contains("but the last landing left")),
            "a move no record explains still halts: {}",
            halted(&db, a)
        );
        for base in [base_a, base_b] {
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// Mission 4, 2026-11-08 09:23: task 182 (objective 34) was rejected
    /// three times — "the work changes nothing on main" twice, then "scenario
    /// already passes on the base" — because the owner batch had landed the
    /// behaviour already, and the third halted the mission. A gate that says
    /// the objective's scenario passes on the base is a fact about the
    /// objective: it is recorded solved there, the task ends as a cancel,
    /// no rejection is counted, and no task for it is assigned again.
    #[test]
    fn an_objective_solved_on_the_base_ends_its_task_without_a_rejection() {
        let gate = "echo '| == base: 023.sh'\n\
                    echo '| verdict rejected scenario already passes on the base'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"scenario already passes on the base\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
        let (base, source, scenarios, integrator) = world("solved-base", gate);
        let db = Db::new("solved_base");
        let mission = backlog_mission(&db, &base, "solved", &[23], 1, None);
        std::fs::write(scenarios.join("023.sh"), "exit 0\n").expect("the scenario");
        let none = std::collections::BTreeSet::new();
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("assign");
        let task = objective_task(&db, mission, 23);
        db.store.task_delivered(task.id, 1).expect("delivered");
        assert!(db.store.task_reported(task.id, "done").expect("reported"));
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        let base_commit = source_main(&source);
        push_work(&base, &source, &agent, "work.txt");

        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let solved = control_records(&db, "objective.solved");
        assert_eq!(solved.len(), 1, "{solved:?}");
        assert_eq!(solved[0]["objective"], json!(23));
        assert_eq!(solved[0]["task"], json!(task.id));
        assert_eq!(solved[0]["base"], json!(base_commit));
        assert!(
            solved[0]["scenario"]
                .as_str()
                .is_some_and(|scenario| scenario.ends_with("023.sh")),
            "{}",
            solved[0]
        );
        let cancelled = control_records(&db, "task.cancelled");
        assert_eq!(cancelled.len(), 1, "{cancelled:?}");
        assert_eq!(cancelled[0]["id"], json!(task.id));
        assert!(cancelled[0]["solved"].is_number(), "{}", cancelled[0]);
        assert!(
            control_records(&db, "task.gated").is_empty(),
            "no rejection is recorded"
        );
        let after = gated(&db, mission);
        assert_eq!((after.gate_rejections, after.gate_aborts), (0, 0));
        assert!(db.mission(mission)["halted"].is_null());
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "cancelled");
        // No task for it again: the objective is a landing.
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none)
            .expect("the next pass");
        assert!(
            flying(&db, mission).is_empty(),
            "no task for a solved objective"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The same for the other wording mission 4's task 182 hit: a candidate
    /// whose tree is main's is not rejected at once in an objective mission,
    /// but judged — the gate's base check answers "solved on the base"
    /// before any candidate build, so the objective is recorded solved
    /// instead of the task rejected ("the work changes nothing on main").
    #[test]
    fn an_objective_task_whose_work_changes_nothing_is_judged_not_rejected() {
        let gate = "echo '| verdict rejected scenario already passes on the base'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"scenario already passes on the base\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
        let (base, source, scenarios, integrator) = world("solved-base-empty", gate);
        let db = Db::new("solved_base_empty");
        let mission = backlog_mission(&db, &base, "solved", &[23], 1, None);
        std::fs::write(scenarios.join("023.sh"), "exit 0\n").expect("the scenario");
        let none = std::collections::BTreeSet::new();
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("assign");
        let task = objective_task(&db, mission, 23);
        db.store.task_delivered(task.id, 1).expect("delivered");
        assert!(db.store.task_reported(task.id, "done").expect("reported"));
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        // A branch with a commit that changes no file: main's tree.
        let clone = base.join("empty-work");
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", &source, &clone_path]).expect("clone");
        git(&[
            "-C",
            &clone_path,
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "nothing",
        ])
        .expect("empty");
        git(&[
            "-C",
            &clone_path,
            "push",
            "-q",
            "origin",
            &format!("HEAD:refs/heads/spawn/{agent}"),
        ])
        .expect("push the agent's branch");

        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        assert!(
            control_records(&db, "task.gated").is_empty(),
            "not rejected at once: {:?}",
            control_records(&db, "task.gated")
        );
        assert_eq!(control_records(&db, "objective.solved").len(), 1);
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "cancelled");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The owner's hold says the owner is moving main. Mission 6, 2026-11-08
    /// 06:36: the owner held the source, pushed main behind Spawn's back and
    /// recorded the move after; a pass between the push and the record
    /// halted the mission, and `spawn mission moved` then matched no mission
    /// (it skipped the halted ones), so the halt stood until a human resumed
    /// it. The real order: hold, push, a pass, then moved and release.
    #[test]
    fn the_owners_hold_keeps_a_moved_main_from_halting() {
        let (base, source, scenarios, integrator) = world("owner-hold-move", "exit 1\n");
        let db = Db::new("owner_hold_move");
        let b0 = source_main(&source);
        let mission = mission_at_baseline(&db, &source, &scenarios, &b0);
        let lock = db
            .store
            .source_hold_lock(&source)
            .expect("the landing lock");
        db.store
            .source_hold(&source, &b0, 3600, Some("the owner's deploy"))
            .expect("held");
        drop(lock);
        let moved = main_moves(&base, &source, "owner-push.txt");

        integrator.pass_on(&db.store).expect("a pass");
        assert!(
            halted(&db, mission).is_null(),
            "no halt while held: {}",
            halted(&db, mission)
        );
        let after = db.mission(mission);
        assert_eq!(
            after["baseline"],
            b0.as_str(),
            "nothing is judged against it"
        );

        // Then the move is recorded, as `spawn mission moved` does, and the
        // hold released: the mission takes the owner's main.
        assert_eq!(
            db.store
                .source_moved(&source, &b0, &moved)
                .expect("the owner's move"),
            1
        );
        db.store.source_release(&source).expect("released");
        let after = db.mission(mission);
        assert_eq!(after["baseline"], moved.as_str());
        assert!(after["halted"].is_null(), "halted: {}", after["halted"]);
        integrator.pass_on(&db.store).expect("another pass");
        assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A mission the integrator halted on a moved main is moved and cleared
    /// by the owner's record of that very move: same source, from and to.
    /// A halt for another reason at the same baseline keeps it.
    #[test]
    fn a_main_move_is_recorded_over_a_halt_that_says_so() {
        let (base, source, scenarios, integrator) = world("owner-move-halted", "exit 1\n");
        let db = Db::new("owner_move_halted");
        let b0 = source_main(&source);
        let mission = mission_at_baseline(&db, &source, &scenarios, &b0);
        let other = mission_at_baseline(&db, &source, &scenarios, &b0);
        db.record(&json!({"type": "mission.halted", "id": other,
                          "reason": "three gate rejections in a row"}));
        let moved = main_moves(&base, &source, "owner-push.txt");

        integrator.pass_on(&db.store).expect("a pass");
        assert!(
            halted(&db, mission)
                .as_str()
                .is_some_and(|reason| reason.contains("but the last landing left")),
            "{}",
            halted(&db, mission)
        );
        // The move the halt names: applied.
        assert_eq!(
            db.store
                .source_moved(&source, &b0, &moved)
                .expect("the owner's move"),
            1
        );
        let after = db.mission(mission);
        assert!(after["halted"].is_null(), "halted: {}", after["halted"]);
        assert_eq!(after["baseline"], moved.as_str());
        let kept = db.mission(other);
        assert_eq!(
            kept["halted"],
            json!("three gate rejections in a row"),
            "another halt keeps its baseline"
        );
        assert_eq!(kept["baseline"], b0.as_str());
        integrator.pass_on(&db.store).expect("another pass");
        assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The integrator stopped between an owner landing's move of main and
    /// its record: the request is still pending and main already holds the
    /// commit. The next pass records the move from where it began (the
    /// missions' baseline), so nothing halts; a commit main held already
    /// moves nothing.
    #[test]
    fn an_owner_landing_whose_record_was_lost_is_recorded_from_where_it_began() {
        let (base, source, scenarios, integrator) = world("owner-lost", "exit 1\n");
        let db = Db::new("owner_lost");
        let b0 = source_main(&source);
        let mission = mission_at_baseline(&db, &source, &scenarios, &b0);
        let commit = owner_commit(&base, &source, "CONTRACT.md", "v1\n", "owner: the contract");
        let request = owner_request(&db, &source, &commit);
        // The move went out; its record did not.
        git(&[
            "--git-dir",
            &source,
            "update-ref",
            "refs/heads/main",
            &commit,
            &b0,
        ])
        .expect("moved");

        integrator.pass_on(&db.store).expect("a pass");
        let moved = control_records(&db, "source.moved");
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(moved[0]["request"], json!(request));
        assert_eq!(moved[0]["from"], b0.as_str());
        assert_eq!(moved[0]["to"], commit.as_str());
        assert_eq!(db.mission(mission)["baseline"], commit.as_str());
        assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));

        // The same commit again: main holds it, nothing moves.
        let again = owner_request(&db, &source, &commit);
        integrator.pass_on(&db.store).expect("another pass");
        let moved = control_records(&db, "source.moved");
        assert_eq!(moved.len(), 2, "{moved:?}");
        assert_eq!(moved[1]["request"], json!(again));
        assert_eq!(moved[1]["from"], commit.as_str());
        assert_eq!(moved[1]["to"], commit.as_str());
        assert_eq!(source_main(&source), commit);
        assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Objective 036, through the store: an accepted verdict a gate run gave
    /// carries what it judged (`inputs`, `env`), lands, and is judged again
    /// beside it on the runner the controller's dispatcher names. The runner
    /// rejects it: the task stays landed, nothing is counted, the record
    /// says the two disagree on equal inputs, and the supervisor's digest
    /// has one item for it until a delivered digest tells it. A verdict on
    /// another scenario set is recorded without a comparison and is no news.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_shadow_judges_a_copy_beside_the_verdict_and_decides_nothing() {
        let gate = "echo '| == setup'\n\
                    echo '| base cache: toolchain primary-tc'\n\
                    printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
        let (base, source, _, integrator) = world("shadow", gate);
        let ctl = base.join("ctl");
        let scenarios = ctl.join("rsi").join("scenarios");
        std::fs::create_dir_all(&scenarios).expect("scenarios");
        std::fs::create_dir_all(ctl.join("deploy")).expect("deploy");
        std::fs::write(scenarios.join("check-ok.sh"), "exit 0\n").expect("a check");
        let marker = "0123456789abcdef0123456789abcdef01234567";
        std::fs::write(ctl.join(".controller"), format!("{marker}\n")).expect("marker");
        // The runner's dispatcher: it notes its arguments, places a
        // rejection naming the runner's toolchain, and exits 0.
        let notes = base.join("dispatched");
        std::fs::write(
            ctl.join("deploy/gate-runner.sh"),
            format!(
                "printf '%s\\n' \"$@\" >>'{notes}'\n\
                 job=\"$6/$(printf %s \"$4\" | cut -c1-12)-x\"\n\
                 mkdir -p \"$job\"\n\
                 printf '{{\"result\":\"verdict\",\"host\":\"runner-host\"}}\\n' >\"$job/record.json\"\n\
                 printf '{{\"verdict\":\"rejected\"}}\\n' >\"$job/verdict.json\"\n\
                 printf '| base cache: toolchain runner-tc\\n' >\"$job/gate.log\"\n",
                notes = notes.display()
            ),
        )
        .expect("dispatcher");
        let shadow_dir = base.join("shadow");
        let integrator = integrator.with_shadow(
            super::shadow::Settings {
                runner: "bb-runner".to_string(),
                every: 1,
            },
            shadow_dir.clone(),
        );
        let db = Db::new("shadow");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        push_work(&base, &source, &agent_of(&db, mission, task), "work.txt");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let (gated_record, _, _) = last_verdict(&db);
        assert_eq!(gated_record["verdict"], "accepted", "{gated_record}");
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
        let given = &gated_record["inputs"];
        assert_eq!(given["base"], gated_record["base"]);
        assert_eq!(given["candidate"], gated_record["candidate"]);
        assert_eq!(given["controller"], marker);
        assert_eq!(
            given["scenarios_sha256"],
            json!(super::inputs::scenarios_sha256(&scenarios))
        );
        assert_eq!(given["scenario"], Value::Null);
        assert_eq!(gated_record["env"]["toolchain"], "primary-tc");
        assert_eq!(gated_record["env"]["runner"], Value::Null);

        let shadows = || control_records(&db, "task.shadow_gated");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while shadows().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "no shadow record within 30 s"
            );
            integrator.pass_on(&db.store).expect("a pass");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let record = shadows()[0].clone();
        assert_eq!(record["id"], task);
        assert_eq!(record["runner"], "bb-runner");
        assert_eq!(record["result"], "verdict");
        assert_eq!(record["verdict"], "rejected");
        assert_eq!(record["primary"], "accepted");
        assert_eq!(record["exit"], 0);
        assert_eq!(record["inputs"], *given, "{record}");
        assert_eq!(record["unequal"], json!([]));
        assert_eq!(record["agrees"], json!(false));
        assert_eq!(record["env"]["toolchain"], "runner-tc");
        assert_eq!(record["env"]["host"], "runner-host");
        assert_eq!(record["primary_env"], gated_record["env"]);
        let dispatched = std::fs::read_to_string(&notes).expect("the dispatcher ran");
        let arguments: Vec<&str> = dispatched.lines().collect();
        assert_eq!(arguments[0], "bb-runner");
        assert_eq!(Some(arguments[2]), given["base"].as_str());
        assert_eq!(Some(arguments[3]), given["candidate"].as_str());
        // The shadow decided nothing, and its directory is gone.
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
        let after = gated(&db, mission);
        assert_eq!(after.gate_rejections, 0);
        assert_eq!(after.gate_aborts, 0);
        let left: Vec<_> = std::fs::read_dir(&shadow_dir)
            .expect("the shadow directory")
            .flatten()
            .collect();
        assert!(left.is_empty(), "{left:?}");
        // Told once: an item of its own until a digest carrying its tag is
        // delivered.
        let items = crate::dispatch::digest::shadow_disagreements(
            db.store
                .shadow_disagreements()
                .expect("disagreements")
                .as_array()
                .expect("a list"),
        );
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].why, crate::dispatch::Why::ShadowDisagrees);
        assert!(items[0].why.is_news());
        let line = &items[0].line;
        assert!(
            line.contains(&format!("#{task} "))
                && line.contains("shadow")
                && line.contains("bb-runner")
                && line.contains("accepted")
                && line.contains("rejected")
                && line.contains("primary-tc")
                && line.contains("runner-tc"),
            "{line}"
        );
        // More passes record nothing more.
        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(shadows().len(), 1);

        // A record that compared nothing is never news, and the store takes
        // no `agrees` on unequal inputs.
        let mut unequal = record.clone();
        unequal["unequal"] = json!(["scenarios_sha256"]);
        unequal["agrees"] = Value::Null;
        db.record(&unequal);
        unequal["agrees"] = json!(false);
        assert!(db.store.integrator_record(&unequal).is_err());
        assert_eq!(
            db.store
                .shadow_disagreements()
                .expect("disagreements")
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The types of the integrator's records after `after` (an event id),
    /// in log order, with the task each names (its `id`).
    fn record_types_after(db: &Db, after: i64) -> Vec<(String, Option<i64>)> {
        db.admin()
            .query(
                "select kind -> 'detail' -> 'record' ->> 'type',
                        (kind -> 'detail' -> 'record' ->> 'id')::bigint
                   from spawn_events
                  where session_id = 'session_control' and id > $1
                    and event_name = 'control.recorded'
                  order by session_seq",
                &[&after],
            )
            .expect("records")
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect()
    }

    fn last_event(db: &Db) -> i64 {
        db.admin()
            .query_one("select coalesce(max(id), 0) from spawn_events", &[])
            .expect("last event")
            .get(0)
    }

    /// A drain (objective 37): task A's gate runs when it is asked;
    /// `integrator.draining` names [A] and the bound. Task B, reported on
    /// another source meanwhile, gets no gate through any number of passes,
    /// although a gate is free, and the drain is not over while A's runs.
    /// Released, A is accepted and lands, and the pass after records
    /// `integrator.drained`, after the landing; B is still `reported`.
    #[test]
    fn a_drain_lets_the_running_gate_land_and_starts_none() {
        let marks = fresh_marks("spawn-db-drain-marks");
        let _release = Release(marks.clone());
        let (base_a, source_a, scenarios, mut integrator) = world("drain-a", &holding_gate(&marks));
        integrator.gates = 2;
        let (base_b, source_b, _, _) = world("drain-b", "exit 1\n");
        let db = Db::new("drain");
        let a = db.gated_mission(&source_a, &scenarios);
        let b = db.gated_mission(&source_b, &scenarios);
        let a_task = db.reported_task(a, "a1");
        push_work(&base_a, &source_a, &agent_of(&db, a, a_task), "a1.txt");
        integrator.pass_on(&db.store).expect("the first pass");
        assert_eq!(wait_started(&marks, 1).len(), 1, "A's gate runs");
        let mark = last_event(&db);

        integrator.begin_drain(Some(&db.store), 7200);
        assert!(integrator.draining());
        assert_eq!(
            control_records(&db, "integrator.draining"),
            vec![json!({"type": "integrator.draining", "gates": [a_task], "bound_s": 7200})]
        );
        let b_task = db.reported_task(b, "b1");
        push_work(&base_b, &source_b, &agent_of(&db, b, b_task), "b1.txt");
        for _ in 0..3 {
            integrator
                .pass_on(&db.store)
                .expect("a pass while draining");
            assert!(
                !integrator.drained(Some(&db.store), true),
                "not drained while A's gate runs"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert_eq!(started(&marks).len(), 1, "no gate starts while draining");

        std::fs::write(marks.join("release"), "").expect("release");
        let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
        loop {
            integrator.reap();
            integrator.pass_on(&db.store).expect("a pass");
            if integrator.drained(Some(&db.store), true) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never drained: {:?}",
                record_types_after(&db, mark)
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let records = record_types_after(&db, mark);
        let at = |kind: &str, task: Option<i64>| {
            records
                .iter()
                .position(|(seen, id)| seen == kind && *id == task)
                .unwrap_or_else(|| panic!("no {kind} in {records:?}"))
        };
        assert!(at("task.landed", Some(a_task)) < at("integrator.drained", None));
        assert!(
            !records
                .iter()
                .any(|(kind, id)| *id == Some(b_task) && kind.starts_with("task.gated")),
            "no verdict on B: {records:?}"
        );
        assert_eq!(db.mission(a)["tasks"][0]["state"], "landed");
        assert_eq!(db.mission(b)["tasks"][0]["state"], "reported");
        assert_eq!(started(&marks).len(), 1, "B never had a gate");
        assert_eq!(
            control_records(&db, "integrator.drained"),
            vec![json!({"type": "integrator.drained", "bound_s": 7200})]
        );
        for base in [base_a, base_b, marks] {
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// A drain at its bound: the gate still running is stopped (its process
    /// gone), `integrator.drain_expired` names its task and the bound, and
    /// nothing is recorded for the stopped gate: no verdict, the task still
    /// `reported` for the next integrator.
    #[test]
    fn a_drain_at_its_bound_stops_the_gate_and_records_nothing_for_it() {
        let marks = fresh_marks("spawn-db-drain-bound-marks");
        let _release = Release(marks.clone());
        let gate = format!(
            "echo $$ >'{}/pid'\n{}",
            marks.to_string_lossy(),
            holding_gate(&marks)
        );
        let (base, source, scenarios, integrator) = world("drain-bound", &gate);
        let db = Db::new("drain_bound");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w1");
        push_work(&base, &source, &agent_of(&db, mission, task), "w1.txt");
        integrator.pass_on(&db.store).expect("the first pass");
        assert_eq!(wait_started(&marks, 1).len(), 1, "the gate runs");
        let pid: u32 = std::fs::read_to_string(marks.join("pid"))
            .expect("the gate's pid")
            .trim()
            .parse()
            .expect("a pid");

        integrator.begin_drain(Some(&db.store), 1);
        let now = std::time::Instant::now();
        assert!(!integrator.drain_expired(now));
        assert!(integrator.drain_expired(now + std::time::Duration::from_secs(2)));
        integrator.expire_drain(Some(&db.store));
        assert_eq!(
            control_records(&db, "integrator.drain_expired"),
            vec![json!({"type": "integrator.drain_expired", "gates": [task], "bound_s": 1})]
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !super::warm::exited_unreaped(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(super::warm::exited_unreaped(pid), "the gate {pid} is gone");
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert!(control_records(&db, "task.gated").is_empty(), "no verdict");
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "reported");
        assert!(control_records(&db, "integrator.drained").is_empty());
        for path in [base, marks] {
            let _ = std::fs::remove_dir_all(&path);
        }
    }

    /// The store takes the integrator's drain records only as they are
    /// specified: a bound from 1 to 86400 s, and the gates (but drained's)
    /// as task numbers.
    #[test]
    fn the_store_takes_drain_records_as_specified() {
        let db = Db::new("drain_records");
        for good in [
            json!({"type": "integrator.draining", "gates": [], "bound_s": 7200}),
            json!({"type": "integrator.drained", "bound_s": 1}),
            json!({"type": "integrator.drain_expired", "gates": [4, 9], "bound_s": 86400}),
        ] {
            db.store.integrator_record(&good).expect("taken");
        }
        for bad in [
            json!({"type": "integrator.draining", "gates": ["4"], "bound_s": 7200}),
            json!({"type": "integrator.draining", "bound_s": 7200}),
            json!({"type": "integrator.drained", "bound_s": 0}),
            json!({"type": "integrator.drain_expired", "gates": [4], "bound_s": 86401}),
            json!({"type": "integrator.drain_expired", "gates": [4]}),
            json!({"type": "integrator.stopped", "gates": [], "bound_s": 5}),
        ] {
            assert!(db.store.integrator_record(&bad).is_err(), "refused: {bad}");
        }
    }
}
