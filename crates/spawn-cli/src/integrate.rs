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
//!    that reaps it; an owner judging is never stopped for one, only by
//!    its own request's withdrawal (`spawn mission push --withdraw`,
//!    [`owner::withdraw`]), which discards what it judged. The integrator is
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
//!    room): nothing recorded, judged again after the host-fault backoff,
//!    and refused when its gate ends with none for the reason the request's
//!    judging before it recorded ([`owner::Judged`]);
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
//! once. With `SPAWN_GATE_SHADOW_NODE=1` the same verdicts are judged again
//! on spawn-node v1 too, recorded the same way (`runner` `node:<name>`;
//! [`remote`]), and each pass runs the nodes' level-triggered sweep
//! (`spawn_node_sweep`).
//!
//! Every `task.gated` also names its `class` ([`verdict_class`]: `code` for
//! a verdict on the work, `infrastructure` for one the host could not
//! judge, `superseded` for `stale`). A head judged alone too adds
//! `head_inputs`, read just before the head's own gate started. A record no
//! gate ran for (`stale`, a conflict or rejection found before the gate, a
//! `no_room` from the room check) has `env` null and the inputs it knows
//! ([`inputs::known`]). Nothing is decided from any of them.
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
//! lock allows one integrator per database (and schema); it belongs to the
//! connection.
//!
//! **One integrator per database, for its life** (objective 221: at 15:03 a
//! second integrator, started by hand where its default gate script was not,
//! took every pass the unit's integrator left idle and aborted every gate it
//! ran). At start, before the database is touched, the gate script must be
//! a file this process can read (the gate runs it with sh); otherwise the
//! start is refused, saying whether SPAWN_GATE named it or its default was
//! read from the working directory. Then the lock is taken on a connection
//! of its own, named "spawn integrate pid=<pid>" (`application_name`),
//! before anything is written, and kept for the process's life, idle or not
//! (`--once` too). A start that finds it held says so and asks every 0.5 s
//! for 15 s, so a deploy's restart takes it; still held, it exits 1 naming
//! the holder (its connection's name and start), nothing recorded and no
//! root made. Under the unit (Restart=always, RestartSec=10) a refused start
//! is started again 10 s later, each start waiting its 15 s, so systemd's
//! start limit is never reached. After a pass that failed it reads once
//! whether its connection still holds the lock, and keeps it if so; a
//! connection that ended is dropped, and the next pass asks for the lock on
//! a new one: refused by its own old connection it waits, refused by
//! another integrator it stops (exit 1, naming the holder). Nothing is read
//! on every pass.

use std::cell::RefCell;
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

use crate::commit_now::{CommitNow, commit_now};
use crate::config::{loop_interval, workspace_root};
use crate::gate::{self, GateFailure, RoomFailure};
use crate::{Flags, env_or, open_store};
use crate::{main_merge, objectives, sources};

mod host;
mod inputs;
pub(crate) mod judging;
mod landing;
pub(crate) mod owner;
mod remote;
mod retained;
mod shadow;
mod verdicts;
mod warm;

#[cfg(test)]
use landing::{LANDING_IDENTITY, SUMMARY_CHARS};
use landing::{
    Landing, Reading, TITLE_CHARS, commit_landing, cut, land, landing_candidate, landing_elsewhere,
    main_moved, merged_tree, read_main, short_commit, solved_on_base, stale_on_main_move,
    unlandable,
};
pub(crate) use landing::{SOLVED_ON_BASE, landing_message};

#[cfg(test)]
use verdicts::{
    CANNOT_JUDGE, MAIN_RED, NOTE_COMMITS, before_the_candidate, cannot_judge_cause,
    incompatible_note,
};
use verdicts::{
    LOAD_SENSITIVE, Rejection, aborted, base_disagrees_fields, base_only_fault,
    cannot_judge_fields, capped_host_fault, gate_host_fault_fields, host_fault_fields,
    load_sensitive_parked_fields, main_red_fields, markers_note, no_room, on_the_head, over_budget,
    red_as_host_fault, red_believed, rejected_fields, rejection, short_of_room,
};

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
/// regression phase, and three such aborts halt a mission. spawn-maintenance
/// keeps the scratch of a gate that runs, whatever this is (the gate's own
/// record in its scratch, scripts/disk.sh); a scratch without that record
/// goes by its age, which scripts/maintenance.sh never takes under this.
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

/// The usage of `spawn integrate`, the only copy (main's help reads it).
pub(crate) const USAGE: &str =
    "usage: spawn integrate [--once | --drain] [--root <dir>] [--gates <n>]";

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
    /// The owner has paused it (objective 099): no objective is assigned
    /// to it (`spawn_objective_assign` would refuse) until it is resumed.
    #[serde(default)]
    pub(crate) paused: bool,
    #[serde(default)]
    pub(crate) scenario_floor: u32,
    /// The scenario file names (`*.sh`) the gate directory held at the
    /// gating that set `scenario_floor` (the latest at the floor since the
    /// mission's last resume), folded by the store from that `task.gated`'s
    /// `scenario_names`; none before such a gating, or from a record without
    /// them. A halt on the floor names the ones gone since ([`floor_gone`]).
    #[serde(default)]
    pub(crate) scenario_names: Option<Vec<String>>,
    /// The gate directory's scenario file names as this pass read them, for
    /// the gating's record (`scenario_names`): never the store's.
    #[serde(skip)]
    pub(crate) scenarios_now: Option<Vec<String>>,
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
    /// The load-sensitive bound (sync/design-load-sensitive-bound.md): the
    /// pushed commit whose gate last found a regression load-sensitive, and
    /// every scenario found so across those judgings.
    #[serde(default)]
    pub(crate) gate_load_sensitive_pushed: Option<String>,
    #[serde(default)]
    pub(crate) gate_load_sensitive_scenarios: Option<Vec<String>>,
    /// The pushed commit the bound parked: not judged again until a new
    /// commit is pushed or a person releases it.
    #[serde(default)]
    pub(crate) gate_parked_pushed: Option<String>,
}

impl Mission {
    /// The task the gate judges next: one the supervisor accepted, or, when
    /// the gate alone accepts, also one just reported.
    fn ready(&self) -> Option<&Task> {
        self.tasks.iter().find(|task| {
            // A task the load-sensitive bound parked is not judged; the
            // mission's next one is ([`Integrator::parked`] says it once).
            task.gate_parked_pushed.is_none()
                && (task.state == TaskState::Accepted
                    || (self.accept == "gate" && task.state == TaskState::Reported))
        })
    }

    /// Its tasks the load-sensitive bound parked that would be judged
    /// otherwise.
    fn parked(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(|task| {
            task.gate_parked_pushed.is_some()
                && (task.state == TaskState::Accepted
                    || (self.accept == "gate" && task.state == TaskState::Reported))
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
    floor_reason(mission, scenarios)
}

/// The scenario floor's halt rule: a candidate that deletes the scenario
/// that judges it passes by removing the work, so the count may not fall.
fn floor_reason(mission: &Mission, scenarios: u32) -> Option<String> {
    (mission.scenario_floor > 0 && scenarios < mission.scenario_floor).then(|| {
        format!(
            "the scenarios fell from {} to {scenarios}",
            mission.scenario_floor
        )
    })
}

/// The scenarios a halt on the floor names as gone: the file names the
/// gating that set the floor saw (`scenario_names`) that `now` (the gate
/// directory's at the halt) no longer holds, sorted. None when that gating
/// named none (a record from before names were recorded).
pub(crate) fn floor_gone(mission: &Mission, now: &[String]) -> Option<Vec<String>> {
    let then = mission.scenario_names.as_ref()?;
    let mut gone: Vec<String> = then
        .iter()
        .filter(|name| !now.contains(name))
        .cloned()
        .collect();
    gone.sort();
    gone.dedup();
    Some(gone)
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

/// The file of the scenarios' root-fix owners, under the integrator's
/// root: one `<scenario> <owner>` a line (`#` begins a comment). Outside the
/// scenarios directory, so it moves no base-cache key.
pub(crate) const SCENARIO_OWNERS: &str = "scenario-owners";

/// Each of `scenarios` with its root-fix owner from [`SCENARIO_OWNERS`]
/// under `root`; None when the file names none (or there is no file).
pub(crate) fn scenario_owners(root: &Path, scenarios: &[String]) -> Vec<(String, Option<String>)> {
    let text = std::fs::read_to_string(root.join(SCENARIO_OWNERS)).unwrap_or_default();
    let named: Vec<(&str, &str)> = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter_map(|line| {
            let (scenario, owner) = line.split_once(char::is_whitespace)?;
            Some((scenario, owner.trim()))
        })
        .filter(|(_, owner)| !owner.is_empty())
        .collect();
    scenarios
        .iter()
        .map(|scenario| {
            let owner = named
                .iter()
                .find(|(name, _)| name == scenario)
                .map(|(_, owner)| (*owner).to_string());
            (scenario.clone(), owner)
        })
        .collect()
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

/// A stale verdict whose report's checkpoint, or else whose latest
/// checkpoint, is no commit of the mirror (`known`), says so (objective
/// 177): after a history rewrite it is neither in the commit map nor in the
/// source. The verdict is still stale; any other verdict is as it was.
pub(crate) fn name_unknown(
    verdict: Freshness,
    reported: Option<&str>,
    checkpoint: Option<&str>,
    known: impl Fn(&str) -> bool,
) -> Freshness {
    if !matches!(verdict, Freshness::Stale(_)) {
        return verdict;
    }
    let unknown = [("report's", reported), ("latest", checkpoint)]
        .into_iter()
        .find_map(|(which, commit)| commit.filter(|commit| !known(commit)).map(|c| (which, c)));
    match unknown {
        Some((which, commit)) => Freshness::Stale(format!(
            "the {which} checkpoint {commit} is an unknown commit (in neither the commit map nor \
             the source)"
        )),
        None => verdict,
    }
}

/// A stale verdict whose report's checkpoint, or else whose latest
/// checkpoint, the rewrite guard refuses (`why_old`, objective 194) says
/// so: it is built on the history before the rewrite, and never lands. An
/// unknown commit is [`name_unknown`]'s to say; any other verdict is as it
/// was.
pub(crate) fn name_old(
    verdict: Freshness,
    reported: Option<&str>,
    checkpoint: Option<&str>,
    why_old: impl Fn(&str) -> Option<String>,
) -> Freshness {
    if !matches!(verdict, Freshness::Stale(_)) {
        return verdict;
    }
    let old = [("report's", reported), ("latest", checkpoint)]
        .into_iter()
        .find_map(|(which, commit)| {
            commit
                .and_then(&why_old)
                .filter(|reason| !reason.contains("is an unknown commit"))
                .map(|reason| (which, reason))
        });
    match old {
        Some((which, reason)) => Freshness::Stale(format!(
            "the {which} checkpoint is from before the rewrite: {reason}"
        )),
        None => verdict,
    }
}

/// Whether this integrator has not yet recorded the stale verdict (task,
/// revision, reason) of a checkpoint from before a rewrite ([`name_old`]):
/// each is recorded once in its life (objective 194); a restart records it
/// once more.
fn first_stale(task: i64, revision: i32, reason: &str) -> bool {
    static SAID: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashSet<(i64, i32, String)>>,
    > = std::sync::OnceLock::new();
    SAID.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((task, revision, reason.to_string()))
}

/// The re-push of `task`'s agent's lost checkpoint `checkpoint` from this
/// host's mirror ([`sources::push_recorded`]), unless it is from before a
/// history rewrite ([`crate::rewrite_guard::why_old`], objective 177): then
/// nothing is pushed, and the refusal is recorded once (`push-again`).
fn push_again(store: &PostgresStore, mirror: &Path, source: &str, task: &Task, checkpoint: &str) {
    match crate::rewrite_guard::why_old(store, &mirror.to_string_lossy(), checkpoint) {
        Ok(None) => {}
        Ok(Some(reason)) => {
            println!(
                "integrate: task {}: {checkpoint} is not pushed again: {reason}",
                task.id
            );
            if let Err(error) =
                crate::rewrite_guard::refused(store, "push-again", task.id, checkpoint, &reason)
            {
                eprintln!("integrate: task {}: {error}", task.id);
            }
            return;
        }
        Err(error) => {
            eprintln!(
                "integrate: task {}: {checkpoint} is not pushed again: {error}",
                task.id
            );
            return;
        }
    }
    if let Some(again) = sources::push_recorded(mirror, source, &task.agent_id, checkpoint) {
        println!(
            "integrate: task {}: {checkpoint} pushed again: {again:?}",
            task.id
        );
    }
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
    let gate = gate_script()?;
    crate::config::require_git()?;
    // The integrator's lock, on a connection of its own, before anything is
    // written, kept for the process's life (objective 221).
    let lock = take_lock_at_start(&open_store, LOCK_WAIT)?;
    record_start(&lock.0, &gate, &root, once);
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
    // Gates in units of their own when a user manager answers (gate v2 step
    // 2: they outlive a restart); else as children, which a restart stops.
    let units = gate::units_available();
    if units {
        println!("integrate: {UNITS_LINE}");
    } else {
        println!(
            "integrate: no systemd user manager answers: gates run as this process's children, \
             and a restart stops them"
        );
    }
    let mut integrator = Integrator::new(root, gate, gate_env, host_room, gates)
        .with_cpu(cpu)
        .with_units(units)
        .with_lock(lock);
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
    // The shadow judging on spawn-node v1 (shadow.rs, remote.rs) beside the
    // runner's, and the candidates' gates on a node first (remote.rs).
    integrator = integrator.with_nodes_from_env();
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

/// How the integrator opens a connection to its store.
type Connect = dyn Fn() -> Result<PostgresStore, String>;

/// How long a start waits for a lock another integrator holds: a deploy's
/// restart (the old integrator just stopped) takes it within this.
const LOCK_WAIT: Duration = Duration::from_secs(15);

/// How often a waiting start asks for the lock again.
const LOCK_ASK: Duration = Duration::from_millis(500);

/// The name of the connection that holds the integrator's lock
/// (`application_name`): the pid alone, since the server keeps 63 bytes.
fn lock_name() -> String {
    format!("spawn integrate pid={}", std::process::id())
}

/// The lock's holder for a person: "<its connection's name>, connected at
/// <YYYY-MM-DD HH:MM:SS> UTC" (`integrator_lock_holder`'s answer).
pub(crate) fn holder_text(holder: &Value) -> String {
    let name = holder["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .map_or_else(
            || format!("an unnamed connection, server pid {}", holder["pid"]),
            str::to_string,
        );
    match holder["connected_ms"].as_i64() {
        Some(ms) => format!("{name}, connected at {} UTC", crate::alerts::time_text(ms)),
        None => name,
    }
}

/// The gate script, resolved against the working directory (the gate must
/// not resolve inside the tree it judges), or the refusal of the start when
/// it is not a readable file: checked before the database is touched
/// (objective 221: a second integrator started where scripts/rsi/gate.sh is
/// not aborted every gate it ran).
fn gate_script() -> Result<PathBuf, String> {
    let gate = std::path::absolute(env_or("SPAWN_GATE", "scripts/rsi/gate.sh"))
        .map_err(|error| format!("SPAWN_GATE: {error}"))?;
    let from_env = crate::config::env_value("SPAWN_GATE").is_some();
    let cwd = std::env::current_dir().unwrap_or_default();
    match gate_refusal(&gate, from_env, &cwd) {
        Some(refusal) => Err(refusal),
        None => Ok(gate),
    }
}

/// Why the gate script `gate` refuses the start, if it does: not there, or
/// not a file this process can read (the gate runs it with sh). `from_env`
/// says SPAWN_GATE named it; else its default was read from `cwd`.
pub(crate) fn gate_refusal(gate: &Path, from_env: bool, cwd: &Path) -> Option<String> {
    let what = match std::fs::metadata(gate) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "does not exist",
        Ok(meta) if meta.is_file() && std::fs::File::open(gate).is_ok() => return None,
        _ => "is not a readable file",
    };
    let named = if from_env {
        "(from SPAWN_GATE)".to_string()
    } else {
        format!(
            "(SPAWN_GATE is unset: its default scripts/rsi/gate.sh is read from the working \
             directory {})",
            cwd.display()
        )
    };
    Some(format!(
        "integrate: the gate script {} {what} {named}; this integrator does not start",
        gate.display()
    ))
}

/// Who this start is (objective 234), recorded on the lock's connection
/// `store` before anything else is written and said on stdout: a stray is
/// named from the store. A refused record is said on stderr and does not
/// stop the start (the lock keeps a second integrator out, not the record).
fn record_start(store: &PostgresStore, gate: &Path, root: &Path, once: bool) {
    let started = started_record(
        Path::new("/proc"),
        std::process::id(),
        std::os::unix::process::parent_id(),
        std::env::current_dir().ok().as_deref(),
        &Start {
            gate,
            gate_from_env: crate::config::env_value("SPAWN_GATE").is_some(),
            root: &std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf()),
            once,
        },
    );
    println!("{}", started_line(&started));
    if let Err(error) = store.integrator_record(&started) {
        eprintln!("integrate: cannot record integrator.started: {error}");
    }
}

/// At most this many ancestors are named in `integrator.started`.
const MAX_ANCESTORS: usize = 16;

/// The start's own fields of `integrator.started` (objective 234).
pub(crate) struct Start<'a> {
    pub(crate) gate: &'a Path,
    pub(crate) gate_from_env: bool,
    pub(crate) root: &'a Path,
    pub(crate) once: bool,
}

/// `<proc>/<pid>/comm`, its newline trimmed; `None` when it cannot be read.
fn proc_comm(proc: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(proc.join(pid.to_string()).join("comm")).ok()?;
    Some(text.strip_suffix('\n').unwrap_or(&text).to_string())
}

/// The parent pid in `<proc>/<pid>/stat`, read after the comm's last `)`
/// (a comm may hold `)` itself); `None` when it cannot be read.
fn proc_ppid(proc: &Path, pid: u32) -> Option<u32> {
    let text = std::fs::read_to_string(proc.join(pid.to_string()).join("stat")).ok()?;
    let (_, rest) = text.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The unified cgroup's path in `<proc>/<pid>/cgroup` (its `0::<path>`
/// line, after any v1 line); `None` without one or with an empty path.
fn proc_cgroup(proc: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(proc.join(pid.to_string()).join("cgroup")).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .filter(|path| !path.is_empty())
        .map(str::to_string)
}

/// `integrator.started` (objective 234): who this start is, read from
/// `proc` for `pid` (its parent pid from its stat, else `parent_id`): its
/// parent's comm, its ancestors [{pid, comm}] from the parent up to pid 1
/// (at most [`MAX_ANCESTORS`]; one whose comm or stat cannot be read ends
/// the list), its working directory, its unified cgroup, and `start`'s
/// fields. What cannot be read is null.
pub(crate) fn started_record(
    proc: &Path,
    pid: u32,
    parent_id: u32,
    cwd: Option<&Path>,
    start: &Start,
) -> Value {
    let parent = proc_ppid(proc, pid).unwrap_or(parent_id);
    let mut ancestors = Vec::new();
    let mut at = parent;
    while at != 0 && ancestors.len() < MAX_ANCESTORS {
        let Some(comm) = proc_comm(proc, at) else {
            break;
        };
        ancestors.push(json!({"pid": at, "comm": comm}));
        if at == 1 {
            break;
        }
        match proc_ppid(proc, at) {
            Some(next) => at = next,
            None => break,
        }
    }
    json!({
        "type": "integrator.started",
        "pid": pid,
        "ppid": parent,
        "parent_comm": proc_comm(proc, parent),
        "ancestors": ancestors,
        "cwd": cwd.map(|cwd| cwd.display().to_string()),
        "cgroup": proc_cgroup(proc, pid),
        "gate": start.gate.display().to_string(),
        "gate_from_env": start.gate_from_env,
        "root": start.root.display().to_string(),
        "once": start.once,
    })
}

/// The start's line on stdout (objective 234), from its
/// `integrator.started`: unknown values say "unknown".
pub(crate) fn started_line(record: &Value) -> String {
    let said = |field: &str| match &record[field] {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => "unknown".to_string(),
    };
    let from = if record["gate_from_env"] == Value::Bool(true) {
        "from SPAWN_GATE"
    } else {
        "SPAWN_GATE unset: the default, read from the working directory"
    };
    format!(
        "integrate: started as pid {} by {} (pid {}) in {}, cgroup {}; gate {} ({from})",
        said("pid"),
        said("parent_comm"),
        said("ppid"),
        said("cwd"),
        said("cgroup"),
        said("gate"),
    )
}

/// The integrator's lock taken at start, on a connection of its own named
/// by this process ([`lock_name`]): held by another integrator, the start
/// says so and asks every [`LOCK_ASK`] for `wait`, so a deploy's restart
/// takes it; still held then, it is refused, naming the holder. The store
/// and its server pid.
fn take_lock_at_start(
    connect: &dyn Fn() -> Result<PostgresStore, String>,
    wait: Duration,
) -> Result<(Arc<PostgresStore>, i32), String> {
    let store = connect()?;
    let name = lock_name();
    let deadline = std::time::Instant::now() + wait;
    let mut said = false;
    loop {
        if store
            .integrator_lock(&name)
            .map_err(|error| error.to_string())?
        {
            let pid = store.backend_pid().map_err(|error| error.to_string())?;
            return Ok((Arc::new(store), pid));
        }
        let holder = store
            .integrator_lock_holder()
            .map_err(|error| error.to_string())?;
        let named = holder
            .as_ref()
            .map_or_else(|| "it was let go as it was read".to_string(), holder_text);
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "integrate: another integrator holds this database's lock ({named}); this one \
                 does not start. Stop that one first, or drain it with `spawn integrate --drain \
                 --root <its root>`."
            ));
        }
        if !said {
            println!(
                "integrate: another integrator holds this database's lock ({named}); waiting up \
                 to {} s for it to end",
                wait.as_secs()
            );
            said = true;
        }
        std::thread::sleep(LOCK_ASK);
    }
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
        if let Some(stopped) = integrator.stopped.borrow_mut().take() {
            return Err(stopped);
        }
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

/// What the integrator says at start when its gates run in units of their
/// own. deploy/deploy.sh reads the installed binary for these words
/// (`units_supported`) to stop it at once, without a drain: keep them as
/// they are, or change both (a test reads deploy.sh for them).
const UNITS_LINE: &str =
    "gates run in systemd user units (spawn-gate-*): a restart does not stop them";

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
pub(crate) fn read_pid_file(text: &str) -> Option<(u32, u64)> {
    let mut words = text.split_whitespace();
    let pid = words.next()?.parse().ok()?;
    let ticks = words.next()?.parse().ok()?;
    words.next().is_none().then_some((pid, ticks))
}

/// Whether the process `pid` that started at `ticks` still runs: not gone,
/// not a zombie, and not another process under a pid used again.
pub(crate) fn still_runs(pid: u32, ticks: u64) -> bool {
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
    /// The store whose connection holds the integrator's lock, kept for the
    /// process's life, idle or not (objective 221); the judgings' threads
    /// record through it. Dropped only after a pass that failed when the
    /// connection no longer holds the lock.
    held: RefCell<Option<Arc<PostgresStore>>>,
    /// The server pid of the connection that last held the lock: a re-take
    /// refused by it is this integrator's own old connection (wait); one
    /// refused by any other is a lock lost to another integrator.
    lock_pid: std::cell::Cell<Option<i32>>,
    /// Why this integrator stops (its lock lost to another integrator).
    stopped: RefCell<Option<String>>,
    /// How a new connection to the store is made: `open_store`, the
    /// environment's (tests: a schema of their own).
    connect: Box<Connect>,
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
    /// The shadow judging of verdicts on spawn-node v1
    /// ([`shadow::NodeShadows`]); None unless `SPAWN_GATE_SHADOW_NODE=1`.
    node_shadows: RefCell<Option<shadow::NodeShadows>>,
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
    /// Whether the queued `gate` jobs a predecessor left were taken back
    /// (once, at the first pass, before any judging of this integrator
    /// starts; only with the primary path on).
    remote_reclaimed: std::cell::Cell<bool>,
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
    /// Gates run in units of their own (gate v2 step 2,
    /// [`gate::units_available`] at the start), else as this process's
    /// children.
    units: bool,
    /// The judging this clone takes over from an earlier integrator (D5,
    /// [`Integrator::adopt`]); None for one it starts.
    adopted: Option<Adopted>,
    /// The candidates' gates judged on spawn-node v1 first
    /// (`SPAWN_GATE_REMOTE=node`, [`remote::Primary`]); None (unset, empty
    /// or `local`) judges every gate on this host, as before.
    remote: Option<Arc<remote::Primary>>,
}

/// What the primary path gives the judging in place of a local run of the
/// candidate's gate ([`Judge::remote_primary`]): the outcome to judge, the
/// red check found passing on the base, and what was judged where.
struct RemoteJudged {
    judged: Result<gate::Verdict, GateFailure>,
    base_check: Option<String>,
    given: Value,
    env: Value,
}

/// What an adopted judging judges, as its `judging.json` says: the
/// candidate and branch its gate runs on, and when that gate started (the
/// test server's crash window opens there).
#[derive(Clone, Debug)]
struct Adopted {
    candidate: String,
    branch: String,
    started_ms: i64,
}

/// A left judging to adopt ([`Integrator::adoption`]): what its thread
/// judges, and its gate's private directory.
struct Adoption {
    mission: Mission,
    task: Task,
    source: String,
    main: String,
    count: u32,
    shares_server: bool,
    jobs: Option<usize>,
    gate: PathBuf,
    adopted: Adopted,
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
    /// Its stop: a task's, for an owner's urgent landing on its source
    /// ([`Stop`]); an owner's, for its request's withdrawal
    /// ([`Integrator::stop_withdrawn`]).
    stop: Arc<Stop>,
    /// Its halt, for a drain past its bound ([`gate::Halt`]).
    halt: Arc<gate::Halt>,
    /// Its judging's id (integrate/judging.rs): a judging directory no
    /// Running names is one an earlier integrator left ([`Integrator::adopt`]).
    judging: String,
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
                units: false,
                adopted: None,
                remote: None,
            },
            gates,
            cpu: None,
            running: RefCell::new(Vec::new()),
            held: RefCell::new(None),
            lock_pid: std::cell::Cell::new(None),
            stopped: RefCell::new(None),
            connect: Box::new(open_store),
            waiting: RefCell::new(std::collections::HashSet::new()),
            owner: RefCell::new(owner::Kept::default()),
            last_judged: RefCell::new(std::collections::HashMap::new()),
            warms: RefCell::new(None),
            shadows: RefCell::new(None),
            node_shadows: RefCell::new(None),
            red_probe_ms: main_red_probe_ms(
                crate::config::env_value("SPAWN_GATE_MAIN_RED_PROBE_MS").as_deref(),
            ),
            drain: RefCell::new(None),
            unlanded: std::cell::Cell::new(false),
            remote_reclaimed: std::cell::Cell::new(false),
        }
    }

    /// Its connections made by `connect` (tests: a schema of their own).
    #[cfg(test)]
    fn with_connect(mut self, connect: Box<Connect>) -> Self {
        self.connect = connect;
        self
    }

    /// The lock taken at start ([`take_lock_at_start`]), kept for life.
    fn with_lock(self, (store, pid): (Arc<PostgresStore>, i32)) -> Self {
        *self.held.borrow_mut() = Some(store);
        self.lock_pid.set(Some(pid));
        self
    }

    /// The store that holds the integrator's lock: the one held, else a new
    /// connection that takes it. `None` when another connection holds it:
    /// this integrator's own old one (asked again at the next pass), or,
    /// for one that never held it, another integrator. A lock this
    /// integrator held and lost to another integrator stops it
    /// ([`Self::stopped`], and an error).
    fn lock_store(&self) -> Result<Option<Arc<PostgresStore>>, String> {
        if let Some(store) = self.held.borrow().clone() {
            return Ok(Some(store));
        }
        let store = (self.connect)()?;
        if store
            .integrator_lock(&lock_name())
            .map_err(|error| error.to_string())?
        {
            let pid = store.backend_pid().map_err(|error| error.to_string())?;
            let store = Arc::new(store);
            *self.held.borrow_mut() = Some(Arc::clone(&store));
            self.lock_pid.set(Some(pid));
            return Ok(Some(store));
        }
        let Some(old) = self.lock_pid.get() else {
            return Ok(None);
        };
        let holder = store
            .integrator_lock_holder()
            .map_err(|error| error.to_string())?;
        match holder {
            None => Ok(None),
            Some(holder) if holder["pid"].as_i64() == Some(i64::from(old)) => Ok(None),
            Some(holder) => {
                let why = format!(
                    "integrate: this integrator's connection to the database ended and another \
                     integrator holds the lock now ({}); this one stops",
                    holder_text(&holder)
                );
                *self.stopped.borrow_mut() = Some(why.clone());
                Err(why)
            }
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
    /// names their tasks, and the caller exits. Gates in units of their own
    /// (gate v2 step 2) are no process of this one: a halt leaves them
    /// running, the record says so (`left_running`), and the integrator
    /// that starts next adopts them.
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
        let mut expired = json!({
            "type": "integrator.drain_expired", "gates": gates, "bound_s": bound_s,
        });
        if self.judge.units {
            expired["left_running"] = json!(true);
        }
        Self::record_own(store, &expired);
        println!(
            "integrate: the drain reached its bound of {bound_s} s: the gates of {} {}; exiting",
            if gates.is_empty() {
                "no task".to_string()
            } else {
                gates
                    .iter()
                    .map(|task| format!("task {task}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            if self.judge.units {
                "keep running in their units, for the integrator that starts next to adopt"
            } else {
                "stopped, nothing recorded for them"
            }
        );
    }

    /// Run gates in units of their own (gate v2 step 2): a restart of the
    /// integrator no longer stops them.
    fn with_units(mut self, units: bool) -> Self {
        self.judge.units = units;
        self
    }

    /// Whether `task` waits for the controller (P22), saying so when it
    /// starts to. Base and controller come from one commit: a gate on a
    /// main past an owner's landing the controller does not have judges it
    /// with the old controller's scenarios and gate (task 259, 2026-11-11:
    /// base d31d6dba, controller 983509fc, check-e2e failing on both). The
    /// window is the owner's landing (`source.moved`, which moves main)
    /// until its deploy (which switches the controller): the task waits for
    /// the deploy, without limit. Task landings need no deploy and hold
    /// nothing: only the owner's latest landing is compared. The alert
    /// `controller_behind` (alerts.rs) says when the wait is no window.
    fn controller_behind(
        &self,
        store: &PostgresStore,
        mission: &Mission,
        task: &Task,
        source: &str,
        mirror: &Path,
        scenarios: &Path,
    ) -> Result<bool, String> {
        if controller_commit(scenarios).is_none() {
            return Ok(false);
        }
        let Some(landed) = store
            .source_owner_landed(source)
            .map_err(|error| error.to_string())?
        else {
            return Ok(false);
        };
        let lag = controller_behind(mirror, scenarios, &landed, |commit| {
            commit_now(store, mirror, commit).map_err(|error| error.to_string())
        })?;
        let controller = match lag {
            ControllerLag::No => return Ok(false),
            ControllerLag::Behind(controller) => controller,
            ControllerLag::Unknown(why) => {
                if self
                    .waiting
                    .borrow_mut()
                    .insert((task.id, Wait::ControllerUnknown))
                {
                    println!(
                        "integrate: mission {}: task {}: {why}: it is not compared with the \
                         owner's landing {} on {source}, and is judged",
                        mission.id,
                        task.id,
                        short_commit(&landed)
                    );
                }
                return Ok(false);
            }
        };
        if self
            .waiting
            .borrow_mut()
            .insert((task.id, Wait::Controller))
        {
            println!(
                "integrate: mission {}: task {} waits: the owner's landing {} on {source} is \
                 newer than the controller its scenarios come from ({}): it waits for the \
                 deploy that switches the controller",
                mission.id,
                task.id,
                short_commit(&landed),
                short_commit(&controller)
            );
        }
        Ok(true)
    }

    /// Whether `task` is held by the nightly's red on its source (gate v2
    /// step 5b), saying so when it starts to be: while the nightly's fact
    /// has a red ([`nightly_red`]), no task of the source is judged, so no
    /// task lands on it (one already accepted when the red appeared lands:
    /// its gate judged before the red was known). The owner's pushes are
    /// judged and land as before (`owner.rs`): a fix or a revert is one. A
    /// green or flaky nightly, the timer's or one run by hand after the
    /// fix, ends the red and the hold.
    fn nightly_holds(&self, mission: &Mission, task: &Task, source: &str) -> bool {
        let Some(red) = nightly_red(&self.judge.root, source) else {
            return false;
        };
        if self
            .waiting
            .borrow_mut()
            .insert((task.id, Wait::NightlyRed))
        {
            println!(
                "integrate: mission {}: task {} held: main of {source} is red since {} on the \
                 nightly ({}): task landings wait for a green or flaky nightly; the owner's \
                 pushes still land",
                mission.id,
                task.id,
                short_commit(&red.since),
                if red.failed.is_empty() {
                    "no scenario named".to_string()
                } else {
                    red.failed.join(", ")
                }
            );
        }
        true
    }

    /// Each task of `mission` the load-sensitive bound parked
    /// ([`Self::parked_task`]). Its cost: per parked task, every pass, one
    /// read of the task's pushed head and its fetch into the mirror
    /// (`sources::fetch_pushed_into`), as a judged task's pass costs; a
    /// parked task stays so until a person or a new push releases it, so
    /// the number of them is the owner's to keep small (rev-e N2).
    fn parked(
        &self,
        store: &PostgresStore,
        mission: &Mission,
        source: &str,
        mirror: &Path,
    ) -> Result<(), String> {
        for task in mission.parked() {
            self.parked_task(store, mission, task, source, mirror)?;
        }
        Ok(())
    }

    /// A task the load-sensitive bound parked: a new commit pushed on its
    /// branch releases it (`task.released`, by the integrator, naming both
    /// commits), so its next pass judges the new one; else it stays parked,
    /// said once. Its mission's other tasks are judged meanwhile
    /// ([`Mission::ready`] skips it).
    fn parked_task(
        &self,
        store: &PostgresStore,
        mission: &Mission,
        task: &Task,
        source: &str,
        mirror: &Path,
    ) -> Result<(), String> {
        let Some(parked) = task.gate_parked_pushed.as_deref() else {
            return Ok(());
        };
        let pushed = sources::fetch_pushed_into(
            mirror,
            source,
            &task.agent_id,
            &integrator_ref(&task.agent_id),
        )
        .map_err(|error| error.to_string())?;
        if let Some(pushed) = pushed.as_deref().filter(|pushed| *pushed != parked) {
            record(
                store,
                &json!({"type": "task.released", "id": task.id, "by": "spawn-integrate",
                        "reason": format!("a new commit {pushed} was pushed (parked pushed commit: {parked})")}),
            )?;
            println!(
                "integrate: mission {}: task {} released: a new commit {} was pushed (parked pushed commit: {})",
                mission.id,
                task.id,
                short_commit(pushed),
                short_commit(parked)
            );
            self.wait_over(task.id);
            return Ok(());
        }
        if self.waiting.borrow_mut().insert((task.id, Wait::Parked)) {
            println!(
                "integrate: mission {}: task {} parked: its pushed commit {} was load-sensitive \
                 twice ({}); it is judged again on a new commit or a person's release (spawn \
                 task release --id {}); the mission's other tasks go on",
                mission.id,
                task.id,
                short_commit(parked),
                task.gate_load_sensitive_scenarios
                    .as_deref()
                    .unwrap_or_default()
                    .join(", "),
                task.id
            );
        }
        Ok(())
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
    /// first ([`warm::Warms::take_over`]), and the warm it cut off is warmed
    /// again for its source's main at the first passes ([`Self::poll_warm`]).
    fn with_warm(self, dir: PathBuf) -> Self {
        let mut warms = warm::Warms::new(self.judge.gate.clone(), self.judge.gate_env.clone(), dir);
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

    /// This integrator shadowing judged verdicts on spawn-node v1 too, when
    /// `SPAWN_GATE_SHADOW_NODE=1` ([`shadow::node_settings`]); its
    /// snapshots beside the mirrors.
    fn with_node_shadow_from_env(self) -> Self {
        match shadow::node_settings(crate::config::env_value) {
            Ok(Some(settings)) => {
                println!(
                    "integrate: judged verdicts are shadowed on spawn-node v1 (every {}-th)",
                    settings.every
                );
                let work = self.judge.root.join(".shadow-node");
                let gate_env = self.judge.gate_env.clone();
                self.with_node_shadow(settings, gate_env, work)
            }
            Ok(None) => self,
            Err(error) => {
                eprintln!("integrate: {error}; no shadow judging on nodes");
                self
            }
        }
    }

    /// What this integrator does with spawn-node v1, from the environment:
    /// its node shadows ([`Self::with_node_shadow_from_env`]) and the
    /// primary path ([`Self::with_remote_primary_from_env`]), each its own
    /// switch.
    fn with_nodes_from_env(self) -> Self {
        self.with_node_shadow_from_env()
            .with_remote_primary_from_env()
    }

    /// This integrator judging the candidates' gates on spawn-node v1 first
    /// when `SPAWN_GATE_REMOTE=node` ([`remote::primary_settings`]). Unset,
    /// empty or `local` changes nothing; any other value, or a setting it
    /// needs that is missing, is said and changes nothing either.
    fn with_remote_primary_from_env(self) -> Self {
        match remote::primary_settings(crate::config::env_value) {
            Ok(Some(settings)) => {
                println!(
                    "integrate: the candidates' gates are judged on spawn-node v1 first ({}), \
                     a lease awaited {} s",
                    if settings.believe_reject {
                        "M3b: a node's acceptance or rejection is believed"
                    } else {
                        "M3a: a node's acceptance is believed"
                    },
                    settings.queue_wait.as_secs()
                );
                let work = self.judge.root.join(".remote-node");
                self.with_remote_primary(settings, work)
            }
            Ok(None) => self,
            Err(error) => {
                eprintln!("integrate: {error}; every gate is judged on this host");
                self
            }
        }
    }

    /// This integrator judging the candidates' gates on spawn-node v1 first
    /// ([`remote::Primary`]), its snapshots in `work`.
    fn with_remote_primary(mut self, settings: remote::PrimarySettings, work: PathBuf) -> Self {
        shadow::remove_tree(&work);
        self.judge.remote = Some(Arc::new(remote::Primary::new(settings, work)));
        self
    }

    /// This integrator shadowing judged verdicts on spawn-node v1 too
    /// ([`shadow::NodeShadows`]), its snapshots in `work`.
    fn with_node_shadow(
        self,
        settings: shadow::NodeSettings,
        gate_env: Vec<(String, String)>,
        work: PathBuf,
    ) -> Self {
        shadow::remove_tree(&work);
        *self.node_shadows.borrow_mut() = Some(shadow::NodeShadows::new(settings, gate_env, work));
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
        let write = |shadowed: &Value| -> Result<(), String> {
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
        };
        // A drain starts no background judging: the verdicts recorded
        // meanwhile are not shadowed (one already running is recorded when
        // it ends, or ends with the integrator).
        let draining = self.draining();
        if let Some(shadows) = self.shadows.borrow_mut().as_mut() {
            shadows.record_ended(write);
            if !draining {
                for want in &wanted {
                    shadows.offer(want.clone());
                }
            }
        }
        // The node source, beside the runner: its jobs looked at, then the
        // same verdicts offered.
        if let Some(nodes) = self.node_shadows.borrow_mut().as_mut() {
            nodes.poll(store, &write);
            if !draining {
                for want in wanted {
                    nodes.offer(store, want);
                }
            }
        }
    }

    /// The nodes' level-triggered sweep (`spawn_node_sweep`: leases past
    /// their time wait for a result, then are fenced; silent nodes are
    /// recorded lost), once a pass; never an error of the pass.
    fn sweep_nodes(store: &PostgresStore) {
        match remote::sweep(store) {
            Ok(swept) if swept.waiting + swept.fenced + swept.lost > 0 => println!(
                "integrate: nodes swept: {} lease(s) waiting for a result, {} fenced, {} \
                 node(s) lost",
                swept.waiting, swept.fenced, swept.lost
            ),
            Ok(_) => {}
            Err(error) => eprintln!("integrate: the nodes' sweep: {error}"),
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
    ///
    /// A warm a predecessor's restart cut off ([`warm::Warms::want_lost`])
    /// is wanted again in a pass whose steps went through (`stepped`: its
    /// halts are applied), for its source's main as it is now, and only
    /// while a mission on that source is not halted: a source whose
    /// missions are all halted is warmed for no gate.
    fn poll_warm(&self, store: &PostgresStore, stepped: bool) {
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
        if let Some(source) = warms.lost_source().map(str::to_string)
            && stepped
        {
            match store
                .gated_missions()
                .map_err(|error| error.to_string())
                .and_then(|missions| {
                    serde_json::from_value::<Vec<Mission>>(missions)
                        .map_err(|error| format!("gated missions: {error}"))
                }) {
                Ok(missions) => {
                    let live = missions.iter().any(|mission| {
                        mission.halted.is_none() && mission.source.as_deref() == Some(&source)
                    });
                    warms.want_lost(live, &|source: &str| {
                        source_head(source).map(|(_, commit)| commit)
                    });
                }
                Err(error) => eprintln!(
                    "integrate: the base warm a previous integrator's restart cut off on \
                     {source} waits: {error}"
                ),
            }
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
        // The lock is kept for the process's life, idle or not (objective
        // 221): a second integrator never gets a pass this one leaves idle.
        let Some(store) = self.lock_store()? else {
            return Ok(());
        };
        let pass = self.pass_on(&store);
        // A pass that failed may have lost the connection: read once
        // whether it still holds the lock, and keep it if so; else the next
        // pass asks for the lock on a new connection.
        if pass.is_err() && !store.integrator_holds_lock().unwrap_or(false) {
            self.held.borrow_mut().take();
        }
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
        self.stop_withdrawn(store);
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
        // The primary path's jobs a predecessor queued and nobody waits for
        // any more, taken back before this integrator's first judging.
        if self.judge.remote.is_some() && !self.remote_reclaimed.replace(true) {
            remote::Primary::reclaim_orphans(store);
        }
        let steps = self.steps(store, &pending, &owner_moved, owner_judge);
        self.poll_warm(store, steps.is_ok());
        Self::sweep_nodes(store);
        self.poll_shadow(store);
        steps
    }

    /// Stop the owner judgings in flight whose request its owner withdrew
    /// (`spawn mission push --withdraw`, [`owner::withdraw`]), each once:
    /// its gate is killed, as a task's is for an urgent landing, and its
    /// thread finds the request ended and discards what it judged
    /// ([`owner::Judged::Ended`]); the pass that reaps it goes on with its
    /// source. One it could not ask about is asked again next pass: its
    /// gate runs on, and its result is discarded all the same.
    fn stop_withdrawn(&self, store: &PostgresStore) {
        for gate in self.running.borrow().iter() {
            let Some(request) = gate.owner else {
                continue;
            };
            if !gate.handle.is_finished()
                && owner::ended(store, request).ok().flatten().as_deref() == Some(owner::WITHDRAWN)
                && gate.stop.stop(request, None)
            {
                println!(
                    "integrate: the owner withdrew request {request} on {}: its owner judging is \
                     stopped",
                    gate.source
                );
            }
        }
    }

    /// Stop the task judgings in flight on the sources of `urgent` (the
    /// owner's urgent requests step 0 found waiting, D2), each once: its
    /// gate is killed, and it records itself discarded by the request
    /// ([`Stop`]). An owner judging is never stopped for one.
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
        // The judgings an earlier integrator left: their gates are taken
        // over before any mission's step, so none is judged again (D5).
        self.adopt(store, &missions);
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

    /// Take over the judgings an earlier integrator left (gate v2 step 2,
    /// design-gate-survives-restart.md D5): every judging directory no
    /// judging of this integrator owns. One whose gate has a runner
    /// (`runner.json`) is adopted: a judging thread of its own, which waits
    /// for that gate instead of starting one (still running, done, or gone
    /// without its status: abandoned), and records as any judging does; it
    /// holds its slot until it ends, whatever `--gates` says now. One with
    /// no gate (its integrator stopped before or after it), one whose task
    /// no longer waits for it at that revision, an owner's judging (judged
    /// again from its request), and one this integrator cannot read are
    /// abandoned: their units stopped, their directories removed.
    fn adopt(&self, store: &Arc<PostgresStore>, missions: &[Mission]) {
        let owned: std::collections::HashSet<String> = self
            .running
            .borrow()
            .iter()
            .map(|gate| gate.judging.clone())
            .collect();
        for left in judging::left(&self.judge.root, &owned) {
            let id = left.judging.id().to_string();
            match Self::adoption(missions, &left) {
                Ok(Some(adopted)) => {
                    if let Err(error) = self.start_adopted(store, left, adopted) {
                        eprintln!("integrate: adopt judging {id}: {error}");
                    }
                }
                Ok(None) => left.judging.abandon(),
                Err(why) => {
                    println!("integrate: judging {id} left by an earlier integrator: {why}");
                    left.judging.abandon();
                }
            }
        }
    }

    /// What adopting `left` takes, `None` when it has no gate to adopt (its
    /// task, untouched, is judged as any other), or why it is not adopted.
    fn adoption(missions: &[Mission], left: &judging::Left) -> Result<Option<Adoption>, String> {
        let Some(gate) = &left.gate else {
            return Ok(None);
        };
        let described = left
            .described
            .as_ref()
            .ok_or("its judging.json cannot be read; its gate is stopped")?;
        let version = described["version"].as_u64();
        if version != Some(judging::VERSION) {
            return Err(format!(
                "its judging.json is version {version:?}, this integrator reads {}; its gate is \
                 stopped",
                judging::VERSION
            ));
        }
        if described["kind"] != "task" {
            return Err(
                "an owner's judging is judged again from its request; its gate is stopped"
                    .to_string(),
            );
        }
        let field = |name: &str| {
            described[name].as_str().map(str::to_string).ok_or(format!(
                "its judging.json names no {name}; its gate is stopped"
            ))
        };
        let number = |name: &str| {
            described[name].as_i64().ok_or(format!(
                "its judging.json names no {name}; its gate is stopped"
            ))
        };
        let (mission_id, task_id) = (number("mission")?, number("task")?);
        let Some(mission) = missions.iter().find(|mission| mission.id == mission_id) else {
            return Err(format!("mission {mission_id} is gone; its gate is stopped"));
        };
        let revision = number("revision")?;
        let Some(task) = mission.tasks.iter().find(|task| {
            task.id == task_id
                && task.state == TaskState::Reported
                && i64::from(task.revisions) == revision
        }) else {
            return Err(format!(
                "task {task_id} no longer waits for this judging (revision {revision}); its gate \
                 is stopped"
            ));
        };
        let started_ms = gate::UnitRun::read(gate).map_or_else(now_ms, |run| run.started_ms);
        let mut gated = mission.clone();
        gated.gate = field("scenarios")?;
        gated.scenarios_now = serde_json::from_value(described["scenario_names"].clone())
            .ok()
            .flatten();
        Ok(Some(Adoption {
            mission: gated,
            task: task.clone(),
            source: field("source")?,
            main: field("base")?,
            count: u32::try_from(number("count")?).unwrap_or(0),
            shares_server: described["shares_server"].as_bool().unwrap_or(true),
            jobs: described["jobs"]
                .as_u64()
                .and_then(|jobs| usize::try_from(jobs).ok()),
            gate: gate.clone(),
            adopted: Adopted {
                candidate: field("candidate")?,
                branch: field("branch")?,
                started_ms,
            },
        }))
    }

    /// Start the judging thread of an adopted judging, as
    /// [`Self::start_judging`] does for a new one.
    fn start_adopted(
        &self,
        store: &Arc<PostgresStore>,
        left: judging::Left,
        adoption: Adoption,
    ) -> Result<(), String> {
        let Adoption {
            mission,
            task,
            source,
            main,
            count,
            shares_server,
            jobs,
            gate,
            adopted,
        } = adoption;
        let mirror = sources::ensure_mirror(&self.judge.root, Some(&source))
            .map_err(|error| error.to_string())?;
        let mut judge = self.judge.clone();
        let stop = Arc::new(Stop::default());
        judge.stop = Some(Arc::clone(&stop));
        judge.adopted = Some(adopted);
        let (task_id, mission_id) = (task.id, mission.id);
        let judging = left.judging;
        let judging_id = judging.id().to_string();
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
                judging.run_adopted(gate, || {
                    judged_or_aborted(&store, &task, &main, mission_id, || {
                        judge.judge(
                            &store,
                            &mirror,
                            &source_owned,
                            &main,
                            &mission,
                            count,
                            &task,
                            jobs,
                        )
                    });
                });
            })
            .map_err(|error| format!("start the adopted judging of task {task_id}: {error}"))?;
        println!(
            "integrate: mission {mission_id}: adopted the judging of task {task_id} ({judging_id}) \
             an earlier integrator left: its gate is not started again"
        );
        self.running.borrow_mut().push(Running {
            mission: mission_id,
            source,
            task: task_id,
            shares_server,
            jobs: jobs.unwrap_or(0),
            owner: None,
            stop,
            halt,
            judging: judging_id,
            handle,
        });
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
    #[allow(clippy::too_many_lines)]
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
        let judging = match judging::Judging::create(
            &self.judge.root,
            &json!({"kind": "owner", "mission": mission_id, "request": id, "source": source,
                    "shares_server": shares_server, "jobs": jobs,
                    "gate": pinned(&self.judge.gate), "scenarios": gated.gate}),
            self.judge.units,
        ) {
            Ok(judging) => judging,
            Err(error) => {
                eprintln!("integrate: start the owner judging of request {id}: {error}");
                return;
            }
        };
        let halt = Arc::new(gate::Halt::default());
        let thread_halt = Arc::clone(&halt);
        // Its stop, for its owner's withdrawal ([`Self::stop_withdrawn`]).
        let stop = Arc::new(Stop::default());
        let thread_stop = Arc::clone(&stop.flag);
        let judging_id = judging.id().to_string();
        let handle = std::thread::Builder::new()
            .name(format!("owner-gate-{id}"))
            .spawn(move || {
                gate::halt_with(thread_halt);
                gate::stop_with(thread_stop);
                judging.run(|| {
                    let judged = judge.judge_owner(&store, &mirror, &gated, &request, jobs);
                    owner::judged(&store, id, &said, judged, &backoff, judge.host.backoff);
                });
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
            stop,
            halt,
            judging: judging_id,
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
        let names = gate::scenario_names(&scenarios)?;
        let count = u32::try_from(names.len()).unwrap_or(u32::MAX);
        if let Some(reason) = halt_reason(mission, count) {
            // A halt on the floor names the scenarios gone since the gating
            // that set it, so a human tells a replacement from a deletion.
            let gone = (floor_reason(mission, count).as_ref() == Some(&reason))
                .then(|| floor_gone(mission, &names))
                .flatten();
            return halt_naming(store, mission, &reason, gone).map(|()| false);
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
        // its gate is doing; a closed mission gets none, nor one the owner
        // paused.
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
            && !mission.paused
            && let Err(error) = objectives::assign_next(store, mission, &settled)
        {
            eprintln!("integrate: mission {}: {error}", mission.id);
        }
        if held {
            return Ok(false);
        }
        if let Some((mirror, _)) = judgeable.as_ref() {
            self.parked(store, mission, source, mirror)?;
        }
        if let (Some((mirror, main)), Some(task)) = (judgeable, mission.ready()) {
            if self.backs_off(mission, task).is_some() {
                return Ok(false);
            }
            let red = self.red_holds(mission, source, &main, task);
            if red == RedHold::Held {
                return Ok(false);
            }
            if self.controller_behind(store, mission, task, source, &mirror, &scenarios)? {
                return Ok(false);
            }
            if self.nightly_holds(mission, task, source) {
                return Ok(false);
            }
            let mut gated = mission.clone();
            gated.gate = scenarios.to_string_lossy().into_owned();
            gated.scenarios_now = Some(names.clone());
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
        let judging = judging::Judging::create(
            &self.judge.root,
            &json!({"kind": "task", "mission": mission_id, "task": task_id,
                    "revision": task.revisions, "source": source, "base": main,
                    "count": count, "shares_server": shares_server, "jobs": jobs,
                    "gate": pinned(&self.judge.gate), "scenarios": gated.gate,
                    "scenario_names": gated.scenarios_now,
                    "probe": probe}),
            self.judge.units,
        )
        .map_err(|error| format!("start the judging of task {task_id}: {error}"))?;
        let judging_id = judging.id().to_string();
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
                judging.run(|| {
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
            judging: judging_id,
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
                // A checkpoint the mirror lacks is named so (objective 177).
                let named = name_unknown(
                    verdict.clone(),
                    reported.as_deref(),
                    checkpoint.as_deref(),
                    |commit| {
                        git(&[
                            "--git-dir",
                            &mirror_path,
                            "cat-file",
                            "-e",
                            &format!("{commit}^{{commit}}"),
                        ])
                        .is_ok()
                    },
                );
                if named != verdict {
                    return Ok(named);
                }
                // One built on the history before a rewrite too (objective
                // 194).
                return Ok(name_old(
                    verdict,
                    reported.as_deref(),
                    checkpoint.as_deref(),
                    |commit| {
                        crate::rewrite_guard::why_old(store, &mirror_path, commit).unwrap_or_else(
                            |error| {
                                eprintln!("integrate: task {}: {error}", task.id);
                                None
                            },
                        )
                    },
                ));
            }
            // A recorded checkpoint whose push was lost (issue #8) is pushed
            // again from this host's mirror, once a pass.
            if !pushed_again && let Some(checkpoint) = checkpoint.as_deref() {
                pushed_again = true;
                push_again(store, mirror, source, task, checkpoint);
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
        if gate::validate_revision("base", main).is_err() {
            // judge_pushed records the parameter abort before it reads a
            // branch. Do not let fetch/freshness turn bad trusted input into
            // an unrecorded retry or a stale verdict.
            return self
                .judge_pushed(
                    store, mirror, source, main, mission, count, task, None, jobs,
                )
                .map(|()| false);
        }
        // An adopted judging's branch was read when its gate started.
        if let Some(adopted) = &self.adopted {
            return self
                .judge_pushed(
                    store,
                    mirror,
                    source,
                    main,
                    mission,
                    count,
                    task,
                    Some(&adopted.branch),
                    jobs,
                )
                .map(|()| true);
        }
        let branch = match Self::pushed_candidate(store, mirror, source, task)? {
            Freshness::Fresh(branch) => Some(branch),
            Freshness::Nothing => None,
            Freshness::Stale(reason) => {
                // Said once in this integrator's life (objective 194).
                if reason.contains("checkpoint is from before the rewrite: ")
                    && !first_stale(task.id, task.revisions, &reason)
                {
                    return Ok(false);
                }
                // Not a verdict on the work: the pusher has not caught up.
                record(
                    store,
                    &json!({"type": "task.gated", "id": task.id, "verdict": "stale",
                            "revision": task.revisions, "reason": reason,
                            "inputs": inputs::known(Some(main), None), "env": null}),
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
    #[allow(clippy::too_many_arguments)]
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
        self.judge_pushed_using(
            store,
            mirror,
            source,
            main,
            mission,
            count,
            task,
            branch,
            jobs,
            landing_candidate,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn judge_pushed_using(
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
        landing: impl FnOnce(&str, &str, &str, &str) -> Result<Option<String>, String>,
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
        // `quarantined`: the regressions the owner's list kept from running
        // (gate.sh), named on the record so a gate that judged without them
        // says so.
        let quarantined: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        // `passed` and `reused`: the regressions that passed in the gate (run
        // on the candidate, or taken from the base's cache as solved), which
        // clear a validator alert (crate::alerts, `validator_defect`).
        let passed: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let reused: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
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
        // Until a gate runs, what is known (the base, the candidate once
        // there is one), and the head's `inputs` when the head is judged
        // alone too.
        let known: std::cell::RefCell<Value> =
            std::cell::RefCell::new(inputs::known(Some(main), None));
        let head_inputs: std::cell::RefCell<Option<Value>> = std::cell::RefCell::new(None);
        // A `stale` record judged nothing: what is known, no environment.
        let superseded = |record: &mut Value| {
            let candidate = known.borrow()["candidate"].as_str().map(str::to_string);
            record["inputs"] = inputs::known(Some(main), candidate.as_deref());
            record["env"] = Value::Null;
        };
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
                "base": main, "scenarios": count, "scenario_names": mission.scenarios_now,
                "request": request,
                "reason": format!(
                    "discarded by the owner's urgent landing (request {request}){}",
                    reason.map(|reason| format!(": {reason}")).unwrap_or_default()
                )});
                if let Some(log) = extra.get("log") {
                    discarded["log"] = log.clone();
                }
                superseded(&mut discarded);
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
            // its guard ([`land`]). A parameter error does not depend on main
            // and must keep its bounded abort classification. A reaped adopter
            // has no outcome of its own to supersede either: its abort must
            // reach the judging-id dedup even when the first outcome landed main.
            let reaped =
                verdict == "aborted" && extra["reason"] == aborted(&GateFailure::Reaped)["reason"];
            let now = if verdict == "accepted"
                || reaped
                || extra["evidence"].as_str() == Some("parameter_error")
            {
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
                stale["scenario_names"] = json!(mission.scenarios_now);
                if let Some(log) = extra.get("log") {
                    stale["log"] = log.clone();
                }
                superseded(&mut stale);
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
                                          "scenarios": count,
                                          "scenario_names": mission.scenarios_now});
            if let (Some(target), Some(extra)) = (record_value.as_object_mut(), extra.as_object()) {
                target.extend(extra.clone());
            }
            if !flakes.borrow().is_empty() {
                record_value["flakes"] = json!(*flakes.borrow());
            }
            if !quarantined.borrow().is_empty() {
                record_value["quarantined"] = json!(*quarantined.borrow());
            }
            if !passed.borrow().is_empty() {
                record_value["passed"] = json!(*passed.borrow());
            }
            if !reused.borrow().is_empty() {
                record_value["reused"] = json!(*reused.borrow());
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
            } else {
                record_value["inputs"] = known.borrow().clone();
                record_value["env"] = Value::Null;
            }
            if let Some(head) = head_inputs.borrow().clone() {
                record_value["head_inputs"] = head;
            }
            // A quality rejection steps the rework up in the verdict's own
            // statement (agent grading, [`record_gated`]).
            let rerouted = record_gated(store, &record_value)?;
            println!("integrate: task {} gated: {verdict}", task.id);
            if let Some(rerouted) = rerouted {
                println!(
                    "integrate: task {} rerouted to preset {} ({}) for its rework",
                    task.id, rerouted["preset_id"], rerouted["routed_difficulty"]
                );
            }
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

        // Invalid trusted inputs become bounded aborts before any git work.
        // Returning an error here would record nothing and retry forever.
        if let Err(error) = gate::validate_revision("base", main) {
            return gated("aborted", aborted(&GateFailure::Parameter(error)));
        }
        let mirror_path = mirror.to_string_lossy().to_string();
        let Some(branch) = branch.map(str::to_string) else {
            return rejected("the worker has no branch to land", "");
        };
        let scenario = task_scenario(scenarios, task.scenario.as_deref());
        let tree_of = |commit: &str| {
            git(&[
                "--git-dir",
                &mirror_path,
                "rev-parse",
                &format!("{commit}^{{tree}}"),
            ])
        };
        // An adopted judging (gate v2 step 2, D5) judges the candidate its
        // gate already runs on, as judging.json names it: the checks before
        // a gate were made when it started, and the candidate, a merge made
        // then, would not come out the same.
        let candidate = if let Some(adopted) = &self.adopted {
            if let Err(error) = gate::validate_revision("candidate", &adopted.candidate) {
                return gated("aborted", aborted(&GateFailure::Parameter(error)));
            }
            known.replace(inputs::known(Some(main), Some(&adopted.candidate)));
            adopted.candidate.clone()
        } else {
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
            let Some(candidate) = landing(&mirror_path, main, &branch, &message)? else {
                // The worker cannot merge main itself: merge it into the
                // workspace now, while the worker waits for this verdict, so
                // the note names what it has to resolve.
                let merge =
                    main_merge::merge_main(store, &self.root, &task.agent_id, &task.session_id);
                println!(
                    "integrate: task {}: main into its workspace: {merge:?}",
                    task.id
                );
                return gated(
                    "conflict",
                    json!({"note": main_merge::conflict_note(main, &merge), "merge": merge.kind()}),
                );
            };
            if let Err(error) = gate::validate_revision("candidate", &candidate) {
                return gated("aborted", aborted(&GateFailure::Parameter(error)));
            }
            known.replace(inputs::known(Some(main), Some(&candidate)));
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
            // What a later integrator needs to adopt this judging's gate,
            // written before it starts.
            judging::describe(&json!({"candidate": candidate, "branch": branch}))?;
            candidate
        };
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
            // The head's run is bound as the candidate's is: read just
            // before its gate starts.
            head_inputs.replace(Some(inputs::read(
                base,
                judged,
                scenarios,
                scenario.as_deref(),
            )));
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
        // database's log counts only inside this window (an adopted gate's
        // from when it started).
        let started = self
            .adopted
            .as_ref()
            .map_or_else(now_ms, |adopted| adopted.started_ms);
        let given = inputs::read(main, &candidate, scenarios, scenario.as_deref());
        bound.replace(Some((given.clone(), Value::Null)));
        // SPAWN_GATE_REMOTE=node: the candidate's gate on a node first, its
        // verdict believed by M3a or M3b; anything else is judged here, as
        // without it ([`Judge::remote_primary`]).
        let remote = self.remote_primary(
            store,
            mirror,
            main,
            &candidate,
            scenarios,
            scenario.as_deref(),
            &gate_env,
            task,
            test_database,
        );
        let (judged, base_check) = if let Some(remote) = remote {
            bound.replace(Some((remote.given, remote.env)));
            (remote.judged, remote.base_check)
        } else {
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
            bound.replace(Some((given, inputs::env_here(gate_log(&judged)))));
            (judged, base_check)
        };
        let ended = now_ms();
        // The candidate's gate has ended: any gate this judging runs after
        // it (a rejection's head re-judging) is not the candidate's, and a
        // later integrator must not adopt it as that (rev-e G1): it
        // abandons the judging instead ([`judging::left`]).
        if let Err(error) = judging::describe(&json!({"candidate_judged": true})) {
            eprintln!(
                "integrate: task {}: note its candidate judged: {error}",
                task.id
            );
        }
        if let Ok(verdict) = &judged {
            flakes.replace(verdict.flakes.clone());
            red_scenarios.replace(verdict.main_red_scenarios.clone());
            quarantined.replace(verdict.quarantined.clone());
            passed.replace(verdict.passed.clone());
            reused.replace(verdict.reused.clone());
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
            // recorded solved there and the task ends without a rejection,
            // whatever the backlog says (objective 210: it may no longer
            // hold the objective, or not be readable), unless it positively
            // reads the objective as recurring.
            Ok(verdict)
                if verdict.reason.trim() == SOLVED_ON_BASE
                    && task.objective.is_some_and(|objective| {
                        !crate::objectives::recurring(mission, objective)
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
                        load_sensitive,
                        head,
                    } => {
                        let mut extra = if load_sensitive.is_empty() {
                            self.cannot_judge(mission, task.id, &scenario, &log)
                        } else {
                            self.load_sensitive(
                                mission,
                                task,
                                &branch,
                                &scenario,
                                &log,
                                &load_sensitive,
                            )
                        };
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
            // A regression failing on the candidate and on the base, run
            // again now, is the validator's defect, never a rejection, and
            // no verdict for a task's candidate (gate.sh's "Three classes
            // of a failing regression"): `host_fault` with its own evidence,
            // so the record and its escalation say what it is.
            Err(
                failure @ (GateFailure::HostFault { .. } | GateFailure::ValidatorDefect { .. }),
            ) => {
                let mut extra = self.gate_host_fault(mission, task.id, &failure);
                extra["candidate"] = json!(candidate);
                gated("host_fault", extra)
            }
            // The host cannot judge this objective's scenario at all: the
            // objective is judged outside the gate, by the owner.
            // `host_fault`: retried after a backoff, never counted toward a
            // halt, never a rejection and never an abort.
            // A regression gate.sh found load-sensitive (failed at load,
            // passed alone as the base does): the same, with its own
            // evidence and its bound ([`Judge::load_sensitive`]).
            Err(GateFailure::CannotJudge {
                scenario,
                log,
                load_sensitive,
            }) => {
                let mut extra = if load_sensitive.is_empty() {
                    self.cannot_judge(mission, task.id, &scenario, &log)
                } else {
                    self.load_sensitive(mission, task, &branch, &scenario, &log, &load_sensitive)
                };
                extra["candidate"] = json!(candidate);
                gated("host_fault", extra)
            }
            Err(failure @ GateFailure::Reaped) => {
                // Normally the starting integrator's outcome already exists and the
                // store returns it without appending. If it left no outcome, this abort
                // bounds repeated no-record judgings just as Timeout does.
                gated("aborted", aborted(&failure))
            }
            Err(failure) => gated("aborted", aborted(&failure)),
        }
    }
}

impl Judge {
    /// The primary path for a task's candidate (`SPAWN_GATE_REMOTE=node`,
    /// design §12.2 S5): None, and the gate runs here as ever, when it is
    /// off, for an adopted judging (its gate already runs here), and
    /// whenever the node's run is not believed (fallback_local: no lease
    /// in time and the take-back won, the node's fault or skew, settled on
    /// the ledger alone, or a rejection M3a judges again). A believed
    /// verdict (M3a: an acceptance on the job's own inputs; M3b,
    /// `SPAWN_GATE_REMOTE_BELIEVE_REJECT=1`: a rejection too) is judged as a
    /// local run's is, except a rejection whose every failure reads as the
    /// node's test database, which is judged again here rather than become
    /// a host fault of the task. A judging stopped while it waits (an
    /// owner's urgent landing) ends as a stopped gate does: discarded.
    #[allow(clippy::too_many_arguments)]
    fn remote_primary(
        &self,
        store: &PostgresStore,
        mirror: &Path,
        main: &str,
        candidate: &str,
        scenarios: &Path,
        scenario: Option<&Path>,
        gate_env: &[(String, String)],
        task: &Task,
        test_database: bool,
    ) -> Option<RemoteJudged> {
        let primary = self.remote.as_ref()?;
        if self.adopted.is_some() {
            return None;
        }
        let request = remote::PrimaryRequest {
            task: task.id,
            judging_id: judging::current_id(),
            repo: mirror,
            base: main,
            candidate,
            scenarios,
            scenario,
            gate_env,
        };
        let stopped = || self.stop.as_ref().is_some_and(|stop| stop.by().is_some());
        match primary.judge(store, &request, &stopped) {
            remote::PrimaryOutcome::Believed(believed) => {
                let remote::Believed {
                    verdict,
                    given,
                    env,
                    base_check,
                    job_id,
                    belief,
                } = *believed;
                if !verdict.accepted
                    && test_database
                    && host::host_fault(&verdict.reason, &verdict.output, Vec::new).is_some()
                {
                    println!(
                        "integrate: task {}: the node's rejection on {job_id} reads as its test \
                         database's fault ({belief} not applied): judged on this host",
                        task.id
                    );
                    return None;
                }
                Some(RemoteJudged {
                    judged: Ok(verdict),
                    base_check,
                    given,
                    env,
                })
            }
            remote::PrimaryOutcome::Local { why } => {
                println!(
                    "integrate: task {}: judged on this host (fallback_local): {why}",
                    task.id
                );
                None
            }
            remote::PrimaryOutcome::Stopped => Some(RemoteJudged {
                judged: Err(GateFailure::Aborted {
                    reason: gate::STOPPED.to_string(),
                    log: String::new(),
                }),
                base_check: None,
                given: inputs::known(Some(main), Some(candidate)),
                env: Value::Null,
            }),
        }
    }

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

    /// The record of a gate that found regressions `named` load-sensitive
    /// on `task`'s pushed commit `pushed` (sync/design-load-sensitive-
    /// bound.md). The first time for that commit: a host fault with evidence
    /// [`LOAD_SENSITIVE`], the mission's next in a row with its backoff, as
    /// [`Self::cannot_judge`]. The second time for the same commit, whatever
    /// its scenarios: parked ([`LOAD_SENSITIVE_PARKED`]), naming every
    /// scenario found and each one's root-fix owner, with no backoff: the
    /// store ends the mission's run of host faults, so its other tasks are
    /// judged, and this one waits for a new commit or a person's release.
    fn load_sensitive(
        &self,
        mission: &Mission,
        task: &Task,
        pushed: &str,
        scenario: &str,
        log: &str,
        named: &[String],
    ) -> Value {
        let again = task.gate_load_sensitive_pushed.as_deref() == Some(pushed);
        let mut scenarios: Vec<String> = if again {
            task.gate_load_sensitive_scenarios
                .clone()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        for name in named {
            if !scenarios.contains(name) {
                scenarios.push(name.clone());
            }
        }
        if again {
            let owners = scenario_owners(&self.root, &scenarios);
            let extra = load_sensitive_parked_fields(pushed, &scenarios, &owners, log);
            println!(
                "integrate: task {}: {}",
                task.id,
                extra["reason"].as_str().unwrap_or_default()
            );
            return extra;
        }
        let faults = mission.gate_host_faults.saturating_add(1);
        let backoff = host_fault_backoff(self.host.backoff, faults);
        let mut extra = cannot_judge_fields(scenario, faults, backoff, log);
        extra["evidence"] = json!(LOAD_SENSITIVE);
        extra["pushed"] = json!(pushed);
        extra["sensitive"] = json!(scenarios);
        extra["reason"] = json!(format!(
            "host fault: {} load-sensitive on this task's pushed commit (failed at load, passed \
             alone as the base does); not a verdict on the work, judged again in {}; a second time \
             on the same pushed commit parks it",
            scenarios.join(", "),
            spoken(backoff)
        ));
        println!(
            "integrate: task {}: {}",
            task.id,
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
        // Reaped (gate v2 step 2): the integrator that started it read it;
        // this judging read nothing.
        Err(
            GateFailure::Timeout
            | GateFailure::Failed(_)
            | GateFailure::Parameter(_)
            | GateFailure::Reaped,
        ) => None,
        Err(
            GateFailure::Aborted { log, .. }
            | GateFailure::NoRoom { log, .. }
            | GateFailure::HostFault { log, .. }
            | GateFailure::MainRed { log, .. }
            | GateFailure::CannotJudge { log, .. }
            | GateFailure::BaseDisagrees { log, .. }
            | GateFailure::ValidatorDefect { log, .. },
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

/// The nightly's red on a source (gate v2 step 5b, sync/design-nightly-
/// min.md): its fact file `<root>/nightly/<name>.json`, which
/// scripts/rsi/nightly.sh writes for `$SPAWN_LIVE/missions/<name>.git`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NightlyRed {
    /// The main of the first red run of this red.
    pub(crate) since: String,
    /// The scenarios that failed.
    pub(crate) failed: Vec<String>,
}

/// The nightly's red on `source`, read from `<root>/nightly/<name>.json`
/// (`<name>`: the source's last path component without `.git`): the fact's
/// `red_since` while it is set. nightly.sh sets it at a red run, keeps it
/// through a skipped run (nothing ran that showed main fixed), and clears
/// it at a green or flaky run. None for a source with no fact, a fact that
/// cannot be read, or one with no red: a nightly that does not run holds
/// nothing (its own alert, nightly_stale, says so).
pub(crate) fn nightly_red(root: &Path, source: &str) -> Option<NightlyRed> {
    let name = Path::new(source).file_name()?.to_str()?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return None;
    }
    let text = std::fs::read_to_string(root.join("nightly").join(format!("{name}.json"))).ok()?;
    let fact: Value = serde_json::from_str(&text).ok()?;
    let since = fact["red_since"]
        .as_str()
        .filter(|since| !since.is_empty())?;
    let failed = fact["failed"]
        .as_array()
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some(NightlyRed {
        since: since.to_string(),
        failed,
    })
}

/// The commit the controller holding `scenarios` was built from: the
/// `<name>-<40 hex>` directory under a `controllers` directory on the
/// scenarios' physical path (deploy/controller.sh builds
/// `controllers/<name>-<commit>`; the mission's gate names it through the
/// `controllers/<name>` symlink). None for scenarios not in a controller.
pub(crate) fn controller_commit(scenarios: &Path) -> Option<String> {
    let physical = std::fs::canonicalize(scenarios).unwrap_or_else(|_| scenarios.to_path_buf());
    let parts: Vec<&std::ffi::OsStr> = physical.iter().collect();
    parts.windows(2).find_map(|pair| {
        if pair[0] != "controllers" {
            return None;
        }
        let name = pair[1].to_str()?;
        let (_, commit) = name.rsplit_once('-')?;
        (commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| commit.to_ascii_lowercase())
    })
}

/// Whether the controller holding `scenarios` lags the commit `landed`
/// (the owner's latest landing, already read through the commit map, in the
/// repository at `repository`: the mirror or the source itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControllerLag {
    /// Not behind: one commit, the scenarios in no controller, or the
    /// controller's commit not `landed`'s ancestor there.
    No,
    /// `landed` is newer than the controller's commit (as it is now, the
    /// commit map's new id for one built before a history rewrite).
    Behind(String),
    /// The controller's commit was dropped by the rewrite, or is in neither
    /// the commit map nor the repository: why (objective 178). Never behind.
    Unknown(String),
}

/// Whether the controller `scenarios` come from is behind the commit
/// `landed`: its commit (`controller_commit`) is read through `now` (the
/// commit map, then the repository: [`crate::commit_now::commit_now`])
/// before the ancestry check, so a controller built before a history
/// rewrite is compared by its new id. A dropped or unknown commit is
/// [`ControllerLag::Unknown`] with its sentence; an error of `now` (the
/// store's) is an error, not a verdict.
pub(crate) fn controller_behind(
    repository: &Path,
    scenarios: &Path,
    landed: &str,
    now: impl FnOnce(&str) -> Result<CommitNow, String>,
) -> Result<ControllerLag, String> {
    let Some(recorded) = controller_commit(scenarios) else {
        return Ok(ControllerLag::No);
    };
    let controller = match now(&recorded)?.id_or_why("the controller's commit", &recorded) {
        Ok(controller) => controller,
        Err(why) => return Ok(ControllerLag::Unknown(why)),
    };
    if landed.eq_ignore_ascii_case(&controller) {
        return Ok(ControllerLag::No);
    }
    let repository = repository.to_string_lossy();
    let behind = git(&[
        "-C",
        &repository,
        "merge-base",
        "--is-ancestor",
        &controller,
        landed,
    ])
    .is_ok();
    Ok(if behind {
        ControllerLag::Behind(controller)
    } else {
        ControllerLag::No
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
    /// The owner's latest landing on its source is newer than the
    /// controller its scenarios come from: its deploy has not switched the
    /// controller yet (P22).
    Controller,
    /// The controller's commit was dropped by a history rewrite or is an
    /// unknown commit (objective 178): said once, never a wait.
    ControllerUnknown,
    /// The nightly found main of its source red (gate v2 step 5b).
    NightlyRed,
    /// The load-sensitive bound parked its pushed commit.
    Parked,
}

/// Whether a wait was said this pass or before, for [`Integrator::backs_off`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Said {
    /// It starts waiting: the line was printed.
    Now,
    /// It went on waiting: said when it started.
    Before,
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

pub(crate) fn record(store: &PostgresStore, value: &Value) -> Result<(), String> {
    store
        .integrator_record(&judging::named(&bound(value)))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// [`record`] a gate's verdict and, when it is a quality rejection
/// (`rejected`; never a conflict, an incompatible head, a host fault, no
/// room, an abort or a stale judging) of a task routed through the grading
/// catalog whose stepped-up rework its preset no longer takes
/// (crate::grading `gate_reroute`), reroute the rework in the same
/// statement (`task.rerouted`, docs/agent-grading.md): the verdict and its
/// reroute are recorded both or neither, as a review's `spawn task
/// reject` records them. A reroute the catalog or the store refuses never
/// costs the verdict: it is said, and the verdict is recorded alone.
/// Returns the `task.rerouted` record when there is one.
pub(crate) fn record_gated(store: &PostgresStore, value: &Value) -> Result<Option<Value>, String> {
    let task = value["id"].as_i64();
    let verdict = value["verdict"].as_str().unwrap_or_default();
    let reroute = match task.map(|task| crate::grading::gate_reroute(store, task, verdict)) {
        Some(Ok(reroute)) => reroute,
        Some(Err(error)) => {
            eprintln!("integrate: task {task:?}: agent grading's step-up not read: {error}");
            None
        }
        None => None,
    };
    let bounded = bound(value);
    let named = judging::named(&bounded);
    // Why the step-up was not recorded with the verdict, said only once the
    // verdict itself is recorded: a verdict the integrator refuses (a stale
    // revision, one already recorded) is refused as before, alone.
    let unrouted = match reroute {
        Some(crate::missions::ReworkReroute {
            route: Ok(decision),
            revision,
            ..
        }) => match store.integrator_record_rerouted(
            &named,
            &decision.preset_id,
            revision,
            decision.routed_difficulty,
        ) {
            Ok((_, rerouted)) => return Ok(Some(rerouted)),
            Err(error) => Some(format!(
                "the gate's rework step-up to preset {} was refused ({error}): the verdict \
                 is recorded alone",
                decision.preset_id
            )),
        },
        Some(crate::missions::ReworkReroute {
            route: Err(error), ..
        }) => Some(format!(
            "the gate's rejection steps its rework up, but {error}: it stays on its preset"
        )),
        None => None,
    };
    store
        .integrator_record(&named)
        .map_err(|error| error.to_string())?;
    if let Some(why) = unrouted {
        eprintln!("integrate: task {task:?}: {why}");
    }
    Ok(None)
}

/// What a verdict is a statement about (objective 035): `code` for a verdict
/// on the work (`accepted`, `rejected`, `conflict`, `incompatible`),
/// `infrastructure` for one where the host could not judge it (`aborted`,
/// `host_fault` whatever its evidence, `no_room`), `superseded` for `stale`
/// (nothing was judged). A verdict added later must be given its class
/// here; `None` for one that has none, and no record carries any other.
pub(crate) fn verdict_class(verdict: &str) -> Option<&'static str> {
    match verdict {
        "accepted" | "rejected" | "conflict" | "incompatible" => Some("code"),
        "aborted" | "host_fault" | "no_room" => Some("infrastructure"),
        "stale" => Some("superseded"),
        _ => None,
    }
}

/// A record as the integrator writes it: every `task.gated` names its
/// verdict's class ([`verdict_class`]), and one that does not say what it
/// judged and where (a record no gate ran for) carries `inputs` with what it
/// knows (its `base`, its `candidate`, the rest null) and `env` null. Any
/// other record goes as it is.
fn bound(value: &Value) -> Value {
    let mut value = value.clone();
    if value["type"] != "task.gated" {
        return value;
    }
    if let Some(class) = verdict_class(value["verdict"].as_str().unwrap_or_default()) {
        value["class"] = json!(class);
    }
    if value.get("inputs").is_none() {
        value["inputs"] = inputs::known(value["base"].as_str(), value["candidate"].as_str());
    }
    if value.get("env").is_none() {
        value["env"] = Value::Null;
    }
    value
}

fn halt(store: &PostgresStore, mission: &Mission, reason: &str) -> Result<(), String> {
    halt_naming(store, mission, reason, None)
}

/// [`halt`], naming the scenarios `gone` for a halt on the scenario floor
/// ([`floor_gone`]): the record's `gone`, which the board shows.
fn halt_naming(
    store: &PostgresStore,
    mission: &Mission,
    reason: &str,
    gone: Option<Vec<String>>,
) -> Result<(), String> {
    let mut halted = json!({"type": "mission.halted", "id": mission.id, "reason": reason});
    if let Some(gone) = &gone {
        halted["gone"] = json!(gone);
    }
    record(store, &halted)?;
    match gone {
        Some(gone) if !gone.is_empty() => println!(
            "integrate: mission {} HALTED: {reason} (gone: {})",
            mission.id,
            gone.join(", ")
        ),
        _ => println!("integrate: mission {} HALTED: {reason}", mission.id),
    }
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
mod tests;

/// Objective 20: a gate failure that is the host's. The failures
/// check-tests.sh printed, in both of its forms; whose each one is; the test
/// database's log over the gate's window; the backoff, the hold and the
/// escalation; and no halt.
#[cfg(test)]
mod main_red_tests;

#[cfg(test)]
mod host_fault_tests;

/// Objective 33: a base that disagrees with itself (a scenario of it exited
/// 1 and then 0, before any candidate code ran) is objective 020's
/// `host_fault` with the evidence `base_disagrees`. Its record, its share of
/// 020's run and backoff, its own count in a row and the alert at three, and
/// no halt.
#[cfg(test)]
mod base_disagrees_tests;

/// A scenario the host cannot judge here (029's systemd user scopes): the
/// record says so, names the scenario, and is a host fault like any other.
#[cfg(test)]
mod cannot_judge_tests;

/// The integrator's passes against the test database
/// (`SPAWN_TEST_DATABASE_URL`; like the spawn-postgres tests, they do not
/// skip), each in a schema of its own, dropped afterwards, with real git
/// sources and a stub gate.
#[cfg(test)]
mod database_tests;

/// spawn-node v1 PR S5: the candidates' gates judged on a node first
/// (`SPAWN_GATE_REMOTE=node`), M3a and M3b, and the outcomes of section 6.2
/// on the ledger alone, against the test database.
#[cfg(test)]
mod remote_primary_tests;
