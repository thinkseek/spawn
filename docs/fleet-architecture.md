# Spawn fleet architecture

## 0. Goal

**Scale agents with as little human intervention as possible.**

The north-star metric is human interventions per agent-hour, at a given
throughput (tasks completed, scenarios passed). Every part below exists to
push that number down, and to do it with the system itself as the improver.

Four ideas carry the design:

1. **A decision ladder.** Every judgement and every action goes to the
   cheapest tier that can decide it with confidence:
   rules (free, exact) → Jev (System 1: fast, calibrated, cheap) →
   supervisor (System 2: a Claude session, slow, expensive) → human (scarcest).
   A tier that is not confident escalates; it never guesses. Tiers are
   strictly layered (see §2.1): a lower-cost tier never overrides a fact,
   and Jev is a filter, not an authority.
2. **Everything is experience.** Each decision is an event: tier, evidence,
   answer, confidence. Later facts label it: the agent kept working after
   "COMPLETED", a human had to step in after "fine", the playbook action
   unblocked the worker. The event log is the store; labels are derived, not
   hand-written.
3. **RSI consumes the experience.** The system improves its own code through
   gated missions (§10.4). The objectives that would move decisions down the
   ladder by replaying recorded episodes are not built (§9).
4. **Synchronization points.** Work runs in parallel and joins only at
   named points, where a record says exactly what it covers (§1.1).

## 1. Layers

```text
 ┌──────────────────────────────────────────────────────────────────────┐
 │ 5 Interface   web UI (fleet board, escalations, agent detail)        │
 │               HTTP API: reads, one write (a person's input) · CLI     │
 ├──────────────────────────────────────────────────────────────────────┤
 │ 4 Control     Supervisor: one perpetual Claude Code session           │
 │                 reads fleet state, submits commands to workers        │
 │               Dispatcher: delivers tasks, nudges, digests escalations │
 │               Integrator: gate + host-side commit + CAS baseline      │
 ├──────────────────────────────────────────────────────────────────────┤
 │ 3 Observation Rules: decide every state the facts settle              │
 │               Jev: decides only what the rules leave ambiguous        │
 │               → observation events (state, who decided, why)          │
 ├──────────────────────────────────────────────────────────────────────┤
 │ 2 Event log   PostgreSQL: spawn_events (truth), spawn_commands        │
 │               (queue), grants, views; the control stream (§10.1)      │
 ├──────────────────────────────────────────────────────────────────────┤
 │ 1 Runtime     Bridge: start / input / resume / stop                   │
 │               Claude Code engines in tmux + git workspaces            │
 │               transcripts, screen, exit files → events                │
 └──────────────────────────────────────────────────────────────────────┘
```

Commands flow down (4 → queue → 1). Facts flow up (1 → log → 3 → 4/5).
No layer calls another layer's process directly; they meet in the log.

| Layer | Parts |
| --- | --- |
| 1 Runtime | Bridge, tmux, git workspaces and mission mirrors, transcripts and exit files |
| 2 Event log | `spawn_events` (with the control stream), `spawn_commands`, grants, projections, views |
| 3 Observation | Observer: rules first, then the model judge (Jev) on ambiguity; without a Jev URL, the rules alone |
| 4 Control | dispatcher and liveness rule (in `spawn loop`), supervisor session, integrator (`spawn integrate`) |
| 5 Interface | `spawn serve` (API and web UI: reads, and one write, a person's input to a session), the `spawn` CLI |

### 1.1 Synchronization points

Work runs in parallel between synchronization points and joins only at
them. A synchronization point is where work changes hands: the points
are named in the table below, one row per hand-off. Between two points no one waits on another's
progress; all waiting happens at a point (a landing that waits for a hold to
end or for its turn, §10.3, waits at the Gate). At a point nothing moves on
until a record names exactly what it covers (the task, the report's
revision, the commit) and who decided. A loop that moves on from inferred
progress (a quiet pane, a status that looks done) instead of such a
record is a defect, wherever it is. A file becomes a record only through
the one who reads it: a worker's `.spawn/report.md` counts once the
dispatcher has stored it as `task.reported`.

Two terms used below. The *lead* is the session that decides for a
mission's owner: Spawn's supervisor, or the owner's own supervising
session. A *ccd* is a Claude Code session run interactively with
`claude --dangerously-skip-permissions` (never print mode).

| Point | Joins | Moves on when | Record | Decided by | Enforced today |
| --- | --- | --- | --- | --- | --- |
| Brief | asker → worker | the task names its goal, inputs and done criteria, and is delivered once its worker has reported its previous task (§3.2) | `task.assigned`, then `task.delivered` | supervisor, the integrator for objective tasks (§10.4), or the owner | the records, and a brief that is not empty; its content is not checked. The base is the agent's latest `artifact.committed` at delivery (`reason` `base_merged` when the delivery merged `main`), or `execution.started` `fork.commit` for a branch the start created; it is not in `task.assigned` |
| Review | worker → reviewer | a verdict names the task, the report's revision and the commit it read | review verdict | a ccd session that did not write the work | not in code: the lead runs it, outside the log (§9) |
| Accept | report → mission | in a review-mode mission, the accept or reject names the revision it judged | `task.reviewed` (§10.3) | supervisor, or the owner | the revision check when `--revision` is given, a note on every send-back, and a refusal in a gate-mode mission; citing a review verdict is not checked (§9) |
| Gate, fleet | branch → `main` | the gate's verdict names the candidate; the baseline moves only to that commit, by compare-and-swap | `task.gated`, `task.landed` (§10.2, §10.3) | integrator | yes |
| Gate, owner work | owner's checkout → candidate | the owner's gate names the candidate and the base it judged against (the missions' `main`) | the gate's verdict file | the owner's gate | only at Deploy: accepted owner work reaches the owner's checkout before any gate |
| Deploy | candidate → `main` and live | the verdict is `accepted`, its candidate is HEAD, its base is the missions' `main`, and the tree is clean | `source.moved` when `main` moves (`spawn mission moved`, §10.3) | `deploy/deploy.sh` | yes, by `deploy/deploy.sh` |
| Publish | owner's checkout → remote | the pushed commit passed its gate and a secret scan | the push | owner | by the owner's procedure, not by code |

**The review.** A report gets a review before it moves on: in a
review-mode mission before it is accepted, in a gate-mode mission before
its gate runs (the cheap check first, the gate after). The reviewer is a
ccd that did not write the work, and it starts from the brief, the diff and the report,
never from the author's conversation. It checks two things:

- *Completion quality.* Every done criterion in the brief is met and shown
  (a test, a command and its output), not only claimed. The diff touches
  only what the brief needs. Changed behaviour has tests. Nothing is left
  half done: no TODO, no skipped or weakened test, no disabled check. The
  report matches the diff.
- *Efficiency.* What the work cost (elapsed time, turns, deliveries, restarts,
  rejected attempts, model spend) against the size of the change. It also
  looks for loops (the same failing command again and again), wandering
  outside the brief, and work redone that already existed. An efficiency
  finding never rejects correct work: it goes into experience, and into the
  briefs and routing rules that caused it.

The verdict is `pass`, `fix` (concrete defects, each with `file:line` and how
it fails) or `reject`. It holds for the revision and commit it names and no
other: a report sent back and revised needs a new review. Several review
sessions may run at once. The lead works with them as peers, one subject per session. The
lead's own commits pass the same point: a session other than the lead
reviews them. Today the lead runs reviews for owner work (missions 7 and 8)
and for everything it merges, gates or deploys; for the fleet's other
reports the Review point is not built (§9).

## 2. Observation: rules first, Jev on ambiguity

The Observer folds facts per session (transcript events, executions, screen
readings). Each time a session's facts change, and every 30 s while an
execution runs, it classifies:

| Facts | Decided by | State |
| --- | --- | --- |
| permission dialog on screen or permission notification | rules | BLOCKED (permission) |
| a tool call is open, or the pane read busy less than 2 min ago | rules | WORKING |
| turn stopped at its prompt while a background job the execution started is open (§2.2) | rules | WORKING (`WAITING_FOR_BACKGROUND_JOB`) |
| execution ended by Stop | rules | COMPLETED |
| no execution yet | rules | WAITING |
| turn stopped, prompt idle, no background job open: finished, asking a question, or waiting? | **Jev** | WAITING / BLOCKED / COMPLETED |
| execution live, no open tool, no fact for 2 min (or a tool open for 10 min with no transcript progress, however busy the screen looks): thinking or hung? | **Jev** | WORKING / WAITING / BLOCKED |
| failed turn or an error on screen: progressing or stuck? | **Jev** | WORKING / WAITING / BLOCKED |
| process exited on its own (a non-zero exit code makes BLOCKED the rules' guess and always asks for attention) | **Jev** | COMPLETED / BLOCKED |
| process lost | **Jev** | UNKNOWN / BLOCKED / COMPLETED |

A failing tool call alone is not ambiguous: it only becomes the latest error
that Jev sees when another case asks it.

When rules decide, no model call is made. When they cannot, Jev gets the
evidence a human would use: the terminal tail, the task, and the recent
facts. It answers in one request:

- `semantic_state` (choice): distribution over the five states;
- `needs_human` (yes/no): probability that a human must answer or decide;
- `stuck` (yes/no): probability that the agent loops or makes no progress.

Without a Jev URL (`SPAWN_JUDGE_URL` or `--judge-url`), the Observer judges
by the rules alone. A case they cannot settle goes the way of a judge that is
down (below): the rules' guess with its ambiguity, `reason =
JudgeUnavailable`, `judge: "none"`, and attention, so the dispatcher
escalates it rather than nudging. Observations in the log that name
`judge: "heuristic"`, a judge since removed, read and show like any other.

Every observation records who decided and why:

```json
{ "state": "BLOCKED", "confidence": 0.82,
  "judge": "jev", "ambiguity": "turn_stopped_idle",
  "rules_state": "WAITING",
  "probabilities": {"WORKING":0.0,"WAITING":0.18,"BLOCKED":0.82,"COMPLETED":0.0,"UNKNOWN":0.0},
  "needs_human": 0.96, "stuck": 0.22, "latency_ms": 748,
  "evidence": {"screen_tail": "...", "task": "...", "open_tools": 0, "turn": "stopped"} }
```

Every field after `judge` is optional in the serde payload, so an observation
without them still parses. `EventKind` has no variant for them.

If Jev is down, the rules' best guess is written with
`reason = JudgeUnavailable`, and the session is retried after 30 s, doubling
to at most 10 min. While retries are pending they replace the 30 s recheck.
It is never marked dead.

The Observer writes an observation at once when the judgement changes
(state, reason, attention, ambiguity). It also keeps a session's latest
judgement covering its latest fact: when a later fact (any record of the
session not the Observer's: a screen reading, a Bridge step, a checkpoint, a
command) leaves the verdict as it was, the session is judged again and, once
its facts have settled (no newer fact for 2 s), that verdict is written with
`based_on_seq` at the latest fact. It is a judgement of those facts: the
rules run again, and a Jev answer stands for them only when the evidence it
was shown is the same (its fingerprint, which counts the engine's moves and
what the facts say, not every record), with no second call. A session with no
new fact gets no new observation; a burst of facts with an unchanged verdict
(a busy turn) gets one, once it settles. A restarted Observer checks every
session once, and covers one that lags with a single observation. The
dispatcher acts only on a judgement that covers the latest fact, so this is
what lets it act on a worker that ended its turn before the screen settled at
its prompt.

Every Jev answer is written, even when the state did not change, together
with the evidence and a stable fingerprint of it. That makes the log the full
record of what the model saw and said, and lets a restarted Observer (or a
one-shot `spawn tick`) skip evidence it already judged. The Observer restores
its state only from rows whose source is `observer`, and the projection only
takes observations from that source.

Jev's distribution doubles as the attention signal. An observation asks for
attention when the probability on states that need someone (BLOCKED,
UNKNOWN), among the states the facts allow, reaches 0.5, or when
`needs_human` or `stuck` does, or when more than half of Jev's mass lies on
states the facts rule out (Jev and the facts disagree). Uncertainty between harmless states (WAITING
0.56 against COMPLETED 0.40 for an idle agent) pages no one: a false alarm
costs exactly the human attention the system exists to save.

### 2.1 Trust boundaries

Jev is a model. It is useful exactly where facts run out, and it is wrong
some of the time. The layering keeps its errors cheap:

1. **Facts > rules > Jev.** Jev is consulted only for cases the rules mark
   ambiguous, and its answer can never override a fact: if a permission
   dialog is on screen, the state is BLOCKED whatever Jev says. A Jev answer
   is stored as an inference with its confidence and evidence, never as a
   fact.
2. **Observation never acts.** The observation layer writes judgements and
   nothing else. Deciding what to do is the control layer's job, under its
   own deterministic policy.
3. **Jev alone can only trigger reversible, bounded actions.** In control, a
   Jev judgement may at most cause a nudge that a human could undo by doing
   nothing (typing `continue` into an agent that stopped mid-task), at most
   once per episode and a few times per agent-hour. Stopping, restarting,
   answering an agent's question, landing code, or anything with side
   effects outside the agent's workspace needs rules over facts, the
   supervisor, or a human.
4. **Trust is measured, not assumed.** Every Jev judgement is later compared
   with what happened (§4). Its per-question agreement and calibration are
   shown in the UI. Thresholds change only through a gated landing, never at
   run time.
5. **Jev is optional.** If Jev is slow, down, or disagrees with itself, the
   system runs on rules alone and escalates what they cannot decide. Losing
   Jev costs more human attention, never correctness.

### 2.2 Waiting on its own job

Workers often end a turn while a job they started runs in the background: a
test run or a gate started with Bash `run_in_background` (or a command the
engine moved there when its timeout ran out), a Monitor watching a log, or
an Agent launched in the background. Claude Code wakes the agent when the
job reports. Such a worker is working, not idle, and that is a fact the
rules settle from the transcript:

- **The records.** The Bridge reads them from the transcript it already
  follows (`ingest::claude_line`), as source `claude`. `job.started`
  (`job_id`, `kind` `bash`, `monitor` or `agent`; when is the record's time)
  comes from the tool result that started the job: its
  `toolUseResult.backgroundTaskId` (Bash), `taskId` with `timeoutMs`
  (Monitor), or `agentId` with `isAsync` and `status` `async_launched`
  (Agent; also a Skill forked to the background, `status` `forked`,
  `background`). A `SendMessage` whose result says it resumed a finished
  Agent (`resumedAgentId`) starts that Agent again under its id: each run is
  a start of its own, keyed by the job's id and the call that started it.
  `job.ended` (`job_id`, `how`: `completed`, `failed`, `killed`, `stopped`,
  `expired`) comes from the engine's `<task-notification>` with a
  `<status>`, or a Monitor's `[Monitor expired …]` event, as the engine
  gave it to the agent (mid-turn as a `queued_command` attachment, or as the
  prompt of a turn of its own; the copies are the same bytes, and the end is
  keyed by the job's id and a digest of the notification, so it is stored
  once and a later run's end is another record), or from a `TaskStop` whose
  result says it stopped the job. The engine's queue (`queue-operation`) is
  not read: it also carries sub-agents' notifications (removed and handed to
  the sub-agent, whose jobs are not the session's), and it can hold a quick
  job's end before the line with the result that started it, so a start
  read after it would open a job already ended. A Monitor's other events end
  nothing; an Agent's notification that
  says it `is waiting on its own background work` ends nothing (it paused
  with work of its own and notifies again); a message only queued for a
  running Agent starts nothing; a stop that failed ends nothing.
- **Within its execution.** A job is open only in the execution that started
  it, while its latest start has no later end: the execution's end (and so a
  resume, a new execution) closes its open jobs, whatever the transcript
  says later; an end for an id no start of the execution names closes
  nothing.
- **The rule.** A turn that stopped at its prompt (a `turn.stopped`, or a
  ready screen) while at least one job of the running execution is open is
  WORKING with reason `WAITING_FOR_BACKGROUND_JOB`, decided by the rules: no
  Jev call, no attention, so no attention or idle escalation and no nudge,
  and its task's `activity` step is not stalled. A failed turn with a job
  open is still the judge's (`engine_error`), which sees `open_jobs`. A turn
  that stops with no job open is judged exactly as before.
- **The bound.** A job open for `SPAWN_STALL_JOB_SECS` (default 7200 s; a
  Monitor expires by itself within an hour) is escalated once, heading
  `background job past its bound`, line "a background job has run for
  <duration>" with its kind, id, record and execution
  (`dispatch/jobs.rs`, tag `<session> job-<execution>-<job>-<start record>`), its age on the
  database's clock. Told once, it stays told while the job is open, and it
  is the only item about the worker meanwhile: what to do (stop the job,
  stop the worker, wait) is the supervisor's decision.
- **Causality** (docs/design.md §14). Decided from these records alone. The
  screen's "N shells still running" or "N monitors still running" footer is
  evidence in Jev's view, never a rule.

The attention view (`GET /api/attention`, `on_own_jobs`) lists the workers
held this way with their jobs and each job's age, and the board shows such a
worker's "doing now" as its background job and its age, and never as quiet.

## 3. Control: dispatcher, supervisor, human

Observation says what state an agent is in. Control decides what to do about
it, on the same ladder.

- **Dispatcher** (inside `spawn loop`) reacts to attention observations
  (BLOCKED, `needs_human`, COMPLETED, `stuck`, execution lost):
  a deterministic **policy** that maps (facts, observation) to an action.
  Jev never chooses the action; its judgement is one input, and only within
  the limits of §2.1:
  1. *Facts and rules* decide fixed cases: a lost execution is resumed
     once (the one-resume rule, below).
  2. *A guarded nudge*: when the agent stopped without asking anything
     (Jev: WAITING, `needs_human` low, confidence high) and its task is not
     done, type `continue`, once per episode, rate-limited per agent.
  3. *Everything else escalates*: BLOCKED, `needs_human` high, `stuck` high,
     low confidence, Jev unavailable, or a nudge that did not help. The
     escalation goes to the supervisor when there is one, else to a human.

  The code is `crates/spawn-cli/src/dispatch.rs`, principal `spawn-dispatch`.
  It acts only on a judgement that covers the session's latest fact; a lost
  execution is resumed once (the one-resume rule); the nudge needs Jev's
  WAITING at an idle prompt with confidence ≥ 0.7, `needs_human` ≤ 0.2 and
  `stuck` < 0.5, once per
  episode (the latest non-dispatcher command) and three per agent-hour. Only
  a nudge that reached the worker counts: one refused before typing (a pane
  not ready) is sent again under `<key>-retry-<n>` on a judgement made after
  it, twice, then escalated as never typed. A worker idle at its prompt and
  not nudged, a nudge that did not help, a
  failed resume (the one-resume rule),
  attention, and a finished execution nobody stopped (its recorded end
  cause is not `stopped`) all escalate. A worker the rules judged waiting
  at its prompt on a background job of its own (§2.2) is not idle: it is
  not nudged nor escalated, and gets no new work unless its last task has
  closed (a revision, or a task after a closed one, is typed at its prompt
  as for any stopped turn); only its job past its bound is escalated, once.
  A Stop or Discard that failed,
  whoever sent it (a model switch, a closed task's worker, a retired
  resident, the integrator's budget abort, the supervisor, a person),
  leaves the worker running: the dispatcher reads the Stops of the live
  execution from the command queue, sends the action again under
  `stop-<execution>-retry-<n>` with a wait of a minute doubling, 3 times,
  without waiting for a new judgement (a failed Stop changed nothing), then
  escalates `stop failed` naming the task, and sends no more. Meanwhile
  nothing is typed into that worker and it is not nudged. Escalations
  coalesce into one digest typed into the supervisor
  while it is idle. The dispatcher keeps no state: idempotency keys and
  their commands' outcomes mark its nudges, resumes and stops, and the tags
  a delivered digest files on its command (`<session> <id>` per item, never
  in the text, which quotes workers' reports) mark what the supervisor was
  told. An approval or a human command (once finished, with its outcome: one
  that failed is reported as failed) is news until it was told, however
  long that takes.

  **The one-resume rule.** A lost worker execution is resumed once
  (`dispatch-resume-<execution>`); once that resume has failed, the session
  is escalated (`resume failed`) and never resumed again by rule, until one
  of its executions stays up. Failed is read from what the log records
  about the session since its last execution that stayed up (it neither
  died young, lost or exited non-zero within a minute of its start, nor
  ended lost before its engine became ready), never from the key of its
  latest execution: a resume that started an execution gave it a new id,
  and no resume is ever sent under that key. Whichever comes first trips
  it, about the dispatcher's own resume:

  - a resume whose execution ended lost before its engine became ready
    (the log has no Bridge `screen.observed` of it in state `ready`: its
    ready wait failed and the start was abandoned), whatever its command's
    outcome and however long its end took. The item carries the evidence,
    the execution's end (`its start failed: pane never became ready, last
    state <state>…`) or else the command's error, and is named by that
    execution (`resume-<execution>`): a later execution that fails the same
    way, a person's resume, is not told again;
  - a resume whose command succeeded and whose engine was lost again (or
    exited non-zero) within a minute;
  - a resume whose command failed before it started an execution.

  A resume whose engine became ready and stayed up did not fail: the
  session's next loss is resumed once again. (Task 132: after the host
  move of 2026-11-07, every resume of one session failed at its ready
  wait and ended lost, while the rule looked only at the young-death count
  (successful resumes) and the latest execution's key, so it was resumed
  every 20 s for ten minutes.) The facts are `failed_resume` and
  `resumed_young` in `dispatch_facts`; the rule is `resume_lost` in
  `dispatch/worker.rs`.

  **Stalls: no task waits silently.** A stall is the absence of an event,
  so the dispatcher also measures, for every open task, the step it waits
  on and since which record, from the log alone (`dispatch/stall.rs`, the
  query `stall_facts`), and escalates a step that has not moved within its
  bound, once per stall:

  | Step | The task | Measured from | Bound (default) |
  | --- | --- | --- | --- |
  | `delivery` | assigned or sent back, and its session's turn (a revision first, then the oldest new task; no earlier open task of the session holding it) | `task.assigned`, or the review or gate verdict that sent it back | `SPAWN_STALL_DELIVERY_SECS` (600) |
  | `activity` | delivered, waiting for its worker (a worker idle at its prompt with no report collected included, or with a report not taken as unfinished) | the later of `task.delivered` and the session's latest fact (a record of the session that is not the Observer's nor storage's) | `SPAWN_STALL_ACTIVITY_SECS` (900) |
  | `judgement` | reported, under review | `task.reported` | `SPAWN_STALL_REVIEW_SECS` (1800) |
  | `judgement` | in a gated mission, the task the integrator judges next (`Mission::ready`'s order) | the later of the record that made it ready and the mission's latest verdict that is not `stale` | `SPAWN_STALL_GATE_SECS` (3600) |
  | `landing` | landing, its source not held by the owner (§10.3, "Landing: the owner's hold") | the later of its `task.gated` accepted and the record that ended the latest hold on its source | `SPAWN_STALL_LANDING_SECS` (600) |
  | `stop` | an ephemeral task closed (accepted without a gate, landed, cancelled) whose worker still runs | the record that closed it | `SPAWN_STALL_STOP_SECS` (300) |
  | `halted` | a halted mission with open tasks | its `mission.halted` | `SPAWN_STALL_HALTED_SECS` (0) |

  A task queued behind another of its session, or behind the task its
  mission's gate judges next, is not escalated: the one ahead of it is. A
  landing on a source the owner holds waits behind the hold, not for the
  integrator: its `landing` step is not escalated while the hold lasts, and
  after a record ends it the step is measured from that record. The
  tasks of a halted mission that wait for the gate or a landing are the
  halt's, escalated once per halt about session `missions`
  (`stall-mission-<mission>-halted-<record>`), with the reason and the open
  tasks. A failed command (a delivery attempt, a model switch's or a
  retirement's Stop, a resume) is the evidence of the step it blocks, not a
  step of its own. A delivery whose retries are spent was escalated as
  `delivery failed`, which is its stall's escalation; one that may have
  typed the task is not retried, and its stall is escalated at its bound.
  A worker waiting on a background job of its own is active (§2.2): while
  its live execution has a job open (`job.started` without its `job.ended`)
  and its latest turn has ended, its `activity` step is not escalated, since
  the engine resumes the agent when the job reports. A job open past
  `SPAWN_STALL_JOB_SECS` (7200) is escalated once on its own, and that is the
  task's only item while the job runs. The screen's "N shells still running"
  is no rule.

  The item, heading `stalled`, is one line naming `task #<id>`, the step,
  how long it stood still, the evidence (the record it is measured from and
  its age, the latest judgement's state and whether it covers the latest
  fact, every failed attempt with its key and error, the latest stale
  verdict's reason) and what Spawn already tried and still will (each
  delivery attempt and its outcome, a retry due, or due and held and why;
  a nudge, a resume, the idle or report item already sent). Its tag is
  `<session> stall-<task>-<step>-<n>`, `n` naming the record the step is
  measured from (the control record's id; for `activity` the `session_seq`
  of the latest fact, or the delivery record's id when no fact came after
  it): `told` sends it once, and a step that moved and stalled anew has a
  new record and a new tag. A failed retry, a stale verdict, a nudge, an
  input waiting for its turn or a judgement moves no step. Every age is on the database's clock
  (`spawn_now_ms()` against the records' times); nothing is inferred from
  another loop's timing or absence; a restarted loop computes the same
  stalls and tags. The stall layer acts on nothing: Spawn's own recoveries
  (delivery retries, the resume of a lost ephemeral worker and the liveness
  rule for residents, the integrator judging a `stale` report again, the
  retire Stop) run on their own schedules, and every default bound is longer
  than their first attempt. Typing into a worker, delivering again after an
  attempt that may have typed, cancelling, reassigning, resuming a halted
  mission, stopping a resident or extending a budget each need a decision:
  the supervisor's.
- **Supervisor**: one perpetual Claude Code session (`agent_supervisor`).
  It plans missions into worker tasks and handles what the dispatcher
  escalates. Its tools are CLI commands over the log: `spawn fleet` (fleet
  state as JSON), `spawn tail <session>` (screen and recent events) and
  `spawn submit`. It never waits on a human (its charter,
  `supervise::charter`; the owner, 2026-11-06: "no human but can input").
  Whatever its grants allow it decides and does in the same turn (reviews,
  accept or send back, assigning, routing and cancelling tasks, nudging a
  stalled worker, a `continue` after a transient model API error, stopping a
  worker, proposing or retiring a resident), reporting each as a line
  "Decided (the owner may reverse): ...". What they do not allow (deploys,
  owner-protected files, money, deleting data, a `discard` included,
  changing grants or credentials: approving a resident, setting a
  mission's gate, resuming a halted mission) is one line "For the owner: ..." that it does
  not wait on; it moves on to other work. It never ends a turn on a question
  or a list of decisions for the owner. The dispatcher wakes it by typing a
  digest into its pane while it is idle; digests coalesce while it works.
  Idle is its turn stopped at its prompt, whatever Jev judged: a turn that
  ended on a question for the human (one of the old charter) is judged
  BLOCKED, and digests still reach it. A digest has one line per
  session and reason: repeated items of one kind about one worker (a human's
  notes that could not be delivered, retried) are counted on one line, the
  latest quoted, so they cannot fill the digest. Its memory is its native
  session (restored by resume) plus the log.

  **A changed charter reaches it by record.** The charter
  (`supervise::charter`) is the prompt of a new native session (the first,
  or a handoff), and that start records the charter's hash (the `Start`
  command's `charter`, FNV-1a of the text). A restarted `spawn loop`, a
  resume (after an engine exit, a crash or a reboot) and a compaction keep
  the native session, and the charter it holds. Which charter that is, is
  read from records only (`dispatch_facts`' `charter`,
  `dispatch/charter.rs`): the start of the execution that first named the
  current native session (the latest `session.native_started`'s id) gave the
  hash it recorded (none, for a start that recorded none: a person's, or
  one from before the record), and the newest charter input the dispatcher
  sent into the session since that start which reached it (an input tagged
  `charter <hash>` that succeeded, or failed after typing may have begun)
  replaced it. When that is not the hash of the charter the running build
  writes, the dispatcher types the deployed charter in at the supervisor's
  next idle prompt, through the digest's path and in that pass's digest's
  place (the digest waits for the next idle prompt, its items pending): one
  input that opens with a line saying the charter changed and that the text
  replaces, in full, the one given earlier, tagged `charter <hash>` and
  keyed `dispatch-charter-<hash>-over-<command>`, `<command>` being the
  record it replaces (the start, or the charter input before it), so a
  restarted loop submits the same key, and a charter reverted and deployed
  again is typed again over the one that replaced it. Typed, the input is
  the record of the delivery: nothing is sent again for that hash, whatever
  restarts, and a new native session started with the charter holds it from
  its start and gets nothing. An attempt refused before anything was typed
  (a pane not ready, an execution gone) told nothing: it goes again under
  `<key>-retry-<n>` at an idle prompt judged on a fact after it (its own
  `command.finished` is one), 3 attempts in all, then it is escalated (`new
  charter not typed`, a condition that holds while those records do: the
  owner types it, or a handoff gives it) and not sent again. One that may
  have typed is not typed again. No step reads the time or the pane's text.

  Residents need none of this. A resident's charter is not Spawn's code: it
  is the text the supervisor proposed and a person approved
  (`resident.proposed`), which no deploy changes and which cannot change
  once approved (a proposal for a resident that is not a proposal is
  refused). The code around it holds no rule that a running resident could
  miss: the role line (`deliver::resident_role`, given at a start) only
  names the resident and its charter, and the report contract
  (`deliver::report_contract`) is typed again with every new task. A rule
  added to the role line would need the same record.
- **Human**: the escalations queue in the UI ("Escalations", top of the
  Fleet page: what the dispatcher raised, told to the supervisor or not) is
  computed by the dispatcher's own code, not by the UI:
  `GET /api/attention` runs the plan of the dispatcher's pass for this moment
  (`dispatch::read_plan`, the function the pass itself calls: the same facts,
  mission rows, disk and gate-room records and stalls, then `decide_with`)
  and submits, marks and collects nothing. The queue shows, top to bottom:
  what the supervisor asks ("The supervisor asks"), when its turn ended at
  its prompt judged BLOCKED or WAITING with `needs_human` at least 0.5 (or
  with a `HUMAN:` line), in its own words from its transcript: its last
  `HUMAN:` line not answered since, or else its reply's last paragraph, in
  at most three lines, the whole reply on request; right under it, always
  there whether it asks or not, the box to type to the supervisor; then the
  escalations not yet told to the supervisor; then those its digests told
  it whose condition still holds (a worker's state, a stall, a path still
  low on disk, a gate still without room), with when they were told, folded
  to a one-line count while nothing is pending. News (a disk recovery, an
  approval, a human's message, host faults at the gate) is resolved by
  being told, in the plan itself (`Why::is_news`), so the digest and the
  queue agree. A managed worker between tasks is not in it, since the
  dispatcher does not escalate it. Each item carries its digest heading,
  its line, its agent and its age. This queue is the UI's only definition
  of what is escalated: the sidebar's count is its count, and an agent is
  marked escalated when it has an item in it (pending or told and
  unresolved) or is the supervisor asking a question
  (`web/src/attention.ts`, `needsHuman`). A judge's `attention` flag is its
  opinion, shown with the judgement in the agent detail.
  A "For the owner:" line is no question: a latest reply that has one
  asks nothing, whatever Jev judged, unless a `HUMAN:` question of an
  old-style turn is still open. Those lines are listed apart, below the
  queue ("For the owner", `supervisor.for_owner`: newest first, each once,
  at most 10, from the end of its transcript): items to read, counted in no
  queue; an owner's message clears none of them, since nothing waits on
  them. Nothing waits on a person: a person reads the queue when they
  choose. Also apart, and counted in no queue: the workers working on a
  background job of their own ("On their own background jobs",
  `on_own_jobs`, §2.2), each with its jobs and their ages, so a person sees
  why a worker at its prompt is in no queue. The owner can type to the
  supervisor at any time (the box at the top of the Fleet page, its pane, or
  `spawn submit`), and to any agent (the box in its detail drawer; both
  boxes are `POST /api/session/<id>/input`, §5); the supervisor reads that
  as an instruction, which may reverse one of its decisions, not as the
  answer to a question. Each human action is a command with principal
  `human`; it is the event the whole system tries to make rarer.
- **Inputs to a busy session wait for its turn.** A person's or the
  supervisor's input (`spawn submit` or the web page's input box, any
  principal but `spawn-dispatch`, `spawn-integrate` and `spawn-liveness`)
  that reaches a pane not ready is
  not refused: the Bridge records it `waiting` (`command.step`, once) and
  types it on the next Ready reading, the session's waiting inputs oldest
  first, each on a Ready reading of its own, at most once. It holds no stop,
  discard or resume behind it; if the execution ends first it fails, as
  stopped or with the end's cause, and a resume does not revive it. Spawn's
  own loops keep their contract: their inputs are refused at once while the
  pane is not ready (and behind a waiting input) and they retry on their
  own, as the nudge and delivery retries above describe. The wait has no
  timeout: a stall shows on the session's task like any other, and the
  `waiting` record is no activity (it moves no stall step and does not make
  a judgement look stale).
- **Authority** comes from grants and DB roles. The dispatcher and the
  supervisor submit as their own principals and only for worker sessions.
  Neither can write observations or runtime events.
- **The integrator** (`spawn integrate`) lands work: it judges ready tasks
  with the mission's gate and lands them host-side (§10.3). RSI is a mission
  like any other, not a separate controller.

### 3.1 The supervisor session never ends

The supervisor is one Spawn session, `session_supervisor`, for the life of
the system. Its continuity is a design requirement, not a best effort:

| Threat | What keeps the session |
| --- | --- |
| `spawn loop` restarts or is replaced | The engine runs in tmux outside the loop's lifetime (`KillMode=process`); the next loop recovers the execution from the log and keeps observing it |
| The engine process exits or crashes | A **liveness rule** (control layer, facts only, no model): no live execution means `resume` with the recorded native session id, with backoff; a revival whose engine is lost or exits non-zero within a minute counts as a failed one, and only an execution that stayed up starts the count over |
| The machine reboots | tmux is gone, so the execution is `Lost`; the same rule resumes it from the native transcript |
| A transcript Claude Code deleted | The resume puts it back from the transcript archive (`<root>/.transcripts`, copied at turn ends and execution ends; docs/storage.md, "The transcript archive"); one gone with no copy fails the resume instead of starting blank under the old id |
| The native session cannot be resumed (transcript lost with no copy, resume fails three times) | `start` a fresh native session inside the same Spawn session, with a handoff prompt rebuilt from the log: mission, open worker tasks, recent escalations and decisions |
| The context window fills | Claude Code compacts on its own; the handoff path above is the fallback when it cannot |
| A deploy changes its charter | Each new native session's start records the hash of the charter it gives; when the native session that runs holds another (after a loop restart, a resume or a compaction), the dispatcher types the new charter in at its next idle prompt, once, by record (§3, the supervisor) |
| The supervisor hangs or loops | Its own observations (rules, then Jev) flag `stuck`; that goes to a human, never to an automatic stop |

Rules for the session itself:

- Nothing automatic ever stops it. Only a human `stop` command does, and the
  liveness rule treats any other end as an outage to repair.
- Its durable memory is the log, not its context window. Anything it must
  not forget (mission, decisions, open escalations) is either a command it
  submitted or an event about it, so a handoff can always be rebuilt.
- Its uptime, restarts, resumes and handoffs are first-class facts in the UI.

### 3.2 Missions, tasks and workers

Spawn works for missions, not repositories. A repository is just one possible
input of a mission.

**Distribution is layered, and every routing decision is the supervisor's.**

| Layer | Who | Does | Never does |
| --- | --- | --- | --- |
| Intake | human (or a standing source) | puts a mission in the inbox: `spawn mission add`, typing to the supervisor, answering a worker | bypass the record: every human input is a stored command with principal `human` |
| Decision | supervisor | accepts a mission, splits it into tasks, routes each task to a resident or a fresh ephemeral worker, accepts or rejects reports, closes the mission | type into panes itself, but for a nudge or a `continue` after a transient model API error |
| Delivery | dispatcher (rules) | delivers assigned tasks, one at a time per worker (a task typed into a live worker is preceded by a merge of `main` into its idle workspace, and its text says what the merge did: [storage.md](storage.md), "Main into a workspace"; a task is delivered when the command that carries it succeeds; a failed one, which typed nothing, is retried under `<key>-retry-<n>` with a doubling wait, 3 times, then escalated; a refusal because the pane was not ready yet (`does not accept input`, refused before typing) is no failed attempt and goes again after 5 s, counted only past 10 of them; one that may have typed the task is escalated at once; a delivery that never reached the worker is not its last delivery, so its retry is not held back by itself); resumes, nudges, collects reports, wakes the supervisor with digests | choose who does what |
| Execution | workers | do one task, end with a report | take work from anywhere but their assignment |

**Two kinds of worker.**

| | Resident | Ephemeral |
| --- | --- | --- |
| Exists for | a lasting concern tied to the architecture (a domain or role with a charter) | one task |
| Started | proposed by the supervisor with a charter (`spawn resident propose`); runs only after a human approves it (`spawn resident approve`), then kept | by the supervisor, on demand, per task |
| Context | accumulates knowledge of its domain; one task at a time; handoff from the log when it grows stale; its charter is given at every start and handoff | a self-contained brief and nothing else |
| Liveness | resumed after outages like the supervisor; a stop by the supervisor or a human is respected (killable) | resumed once at most; ends when its report is accepted |
| Workspace | its own, long-lived | its own, per task |

**Context cohesion: the rules that decide routing and protect every context.**

1. One session, one concern. A task goes to a resident only when it falls
   inside that resident's charter; anything else gets an ephemeral worker.
2. Serial inside a session. A resident starts its next task only after it has
   reported the last one; its queue waits in storage, not in its context.
3. Briefs, not histories. A worker receives a self-contained brief (goal,
   inputs, done criteria, report format), never the supervisor's conversation.
4. Reports, not transcripts. A worker ends by writing `.spawn/report.md`
   (outcome, what changed, open issues); the dispatcher stores it with the
   task and digests it. The brief tells it to write the file once, at the
   end, and to draft anywhere else: a report that is still a draft (a
   template placeholder such as `PENDING_RESULTS`, a line that is only
   `(pending)`, `TBD` or `TODO`, an Outcome that says draft or in progress)
   is not taken. The task stays delivered, the worker is told once per
   content of the file which markers are on which lines, and the check is
   recorded (`task.report_unfinished`, §10.1). The supervisor reads reports
   and judgements, and opens a transcript (`tail`) only when it must. Its
   own context stays about the mission.
5. Isolated workspaces. Every worker has its own; combining results is a task
   the supervisor decides on, not a side effect.
6. Human input is context too. A human may type to a worker directly. The
   message is delivered and recorded, and the supervisor gets a one-line
   summary of it in its next digest.

**Commands.** `spawn mission add --text ... [--source <repo>]` (a human),
`spawn task assign --mission N --kind resident|ephemeral --worker <name>
--brief <text> [--model default|cheap]` (the supervisor; checked like the
gateway; the model profile is §3.3), `spawn task
accept` /
`reject`, `spawn resident propose` / `approve` / `retire`, `spawn mission
close`. Every one is a checked SQL function that appends to the control
stream (§10.1).

**When a task closes.** A task closes by landing (or, without a gate,
acceptance), or by a cancel: `spawn task cancel`, the supervisor's, the
integrator's abort over budget (`task.aborted`), a gate rejection the
closure rule turns into a cancel, or a mission closed or a resident retired
under it (§10). A cancelled task's live work ends with it, at the source:

- *Stopped.* When a delivered task is cancelled and it was its worker's
  latest delivery, the dispatcher stops the worker at once, in the middle of
  a turn or idle (a Stop takes a final checkpoint, which keeps what it did;
  its turn for the task is never left to finish). An ephemeral worker is
  stopped and nothing more; a retired resident is stopped and not told.
- *Set aside.* Before a resident gets anything else, the task's work leaves
  its branch's tree: a new commit on top of the branch head, whose tree is
  the checkpoint the task started from ([storage.md](storage.md), "Work set
  aside"). Nothing is rewound or lost, and the next task's candidate holds
  none of it, so nothing rides into another objective's landing.
- *Told.* The resident is then resumed with a notice that names the task,
  says it is cancelled, and says where its work is (the commit that holds
  it). Its next task waits until the notice reached it.
- *Never collected.* A cancelled task's report is not collected, recorded
  or judged; a report file it leaves is not the next task's (the delivery
  mark rule).

A task never delivered changes nothing for its worker, and a resident whose
cancelled task is not its latest delivery is left alone. The dispatcher
keeps no state for this either: `spawn_events`, `spawn_commands` and their
keys (`cancel-stop-<task>-<execution>`, `cancel-notice-<task>-<n>`,
`set-aside:<task>`) say what was done.

**Storage.** Missions, tasks and the resident roster are the `mission_*`
tables, folded from the control stream (§10.1): a mission has its text,
origin, sources, state and landing settings; a task has its mission, brief,
kind, assignee, model profile, state and report.

**Mission sources.** A mission names its source: a git repository, as an
absolute path, an `https://` or `ssh://` URL, or ssh's `user@host:path`,
mirrored into `$SPAWN_WORKSPACE_ROOT/.sources/`, or none (an empty scratch
repository). Each worker's workspace is a branch of that mirror.
`SPAWN_GIT_REPO` is the repository of agents no mission owns. A source is
git run as the owner, so `spawn_mission_add` refuses any other shape (an
`ext::` command, a relative path, an option), the supervisor may name only a
source a human already gave a mission, and every git the controller runs
allows only file, https and ssh (`GIT_ALLOW_PROTOCOL`), in the C locale
(docs/storage.md, "Branching and merging").

**Limits.** None fixed. The supervisor decides fleet size; humans stop what
overreaches, and the UI shows the live workers.

### 3.3 Engine models

A Claude Code engine reaches its models through the **model relay**
(`scripts/model-relay.py`, unit `spawn-model-relay`, `127.0.0.1:8791`). The
relay routes each call by the `model` in its body: `deepseek-*` goes to
DeepSeek's Anthropic-compatible endpoint with `x-api-key:
$DEEPSEEK_API_KEY` in place of the engine's credentials; anything else goes
unchanged to the gateway of Claude Code's own settings, with the engine's own
auth header. The DeepSeek key lives only in the relay's process (from
`~/.hermes/.env`); the gateway token only in `~/.claude/settings.json`.
Spawn's environment, the engine's environment, the event log and the panes
hold neither.

Every task runs on one of two **profiles**:

| Profile | Main model (and sonnet, opus slots) | Small model (haiku slot: titles, summaries, background work) | For |
| --- | --- | --- | --- |
| `default` | Claude Code's own (`claude-opus-5-5` on the gateway) | `deepseek-flash` | everything not listed below |
| `cheap` | `deepseek-flash` (`SPAWN_CHEAP_MODEL`) | `deepseek-flash` (`SPAWN_SMALL_MODEL`) | simple, well-specified work: docs, mechanical edits, tests for stated behaviour |

Design, concurrency, security, debugging and anything a gate judges as hard
stay on `default`; the supervisor's charter says so.

How the profile travels:

- `spawn task assign --model cheap` records it in `task.assigned` and
  `mission_tasks.model` (default `default`).
- Delivery puts it on the `Start` or `Resume` it sends. A `Start` or
  `Resume` that names no profile (liveness revivals, the dispatcher's resume
  of a lost engine, a human's `spawn submit`) keeps the profile the session's
  last execution started with, which `execution.started` records: a resumed
  cheap worker stays cheap, and a revived resident keeps its models.
- A running engine keeps the models it started with. A task for a live
  worker on another profile stops it at the moment typing would be safe
  (`switch-model-<task>-<revision>`); the next pass resumes it, context
  kept, on the task's profile. A switch Stop that fails is retried and then
  escalated like every failed Stop (§3, the dispatcher); the task stays
  `assigned` and is never marked delivered by it.
- The Bridge passes the relay and the slots in the inline `--settings`
  `env` (`ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`,
  `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`). Claude Code merges inline
  settings over the user's files, `env` name by name (checked with 2.1.318),
  so the gateway token still comes from the user's own settings.

**When the relay is down.** The Bridge connects to the relay before every
Claude start. Unreachable: a `default` engine starts on Claude Code's own
settings (the gateway, its own small model) and the tick reports a warning;
a `cheap` start fails, is retried, and the failed delivery reaches the
supervisor's digest once its retries are spent. An engine already running when the relay dies sees API errors until
systemd restarts it (`Restart=always`). Engines use the relay only where
`SPAWN_MODEL_RELAY` names it (`http://127.0.0.1:8791` in `spawn.env`):
unset or `off`, every engine runs on Claude Code's own settings, nothing
probes a relay, and a cheap start fails.

## 4. Experience

Nothing is stored outside the log. Every model judgement is an
`observation.recorded` event with the evidence the model saw, its answer and
the rules' guess (§2); what happened next (the agent's next tool call, the
next command and who sent it, the end of the execution) follows it in the
same log. Nothing exports these episodes or replays them through the rules
(§9).

The ladder metrics in the UI (decisions per tier, escalations, human
decisions per agent-hour over the last 24 h) are computed from the log by
`spawn serve`.

## 5. Interface

| View | Answers |
| --- | --- |
| Escalations (top of the board) | What the supervisor asks, in at most three lines (its `HUMAN:` line or its reply's last paragraph; the whole reply on request), and under it the box to type to the supervisor, always there; the dispatcher's escalations not yet told to the supervisor, and those told and unresolved (folded to a count while nothing is pending), each with its heading, line, agent and age (`/api/attention`, computed by the dispatcher's code); below it, the supervisor's "For the owner:" lines with their age, to read, not counted |
| Always-on agents (below the queue) | The supervisor, then the residents, pinned live or not: liveness, state and for how long, last activity, uptime, the task each holds |
| Health strip | In this order: liveness (always-on agents up, shortest uptime, liveness restarts in 24 h), resources (free disk per place against its warning and floor; memory available now against a 4 GiB warning line, of the total; each gated mission's waits for gate room: the run's task, how many `no_room` outcomes since when, whether it was escalated; a value the server could not read shows as unread, never guessed), judgements, the decision ladder and human decisions per agent-hour |
| Fleet board | The rest of the fleet in groups: on a task (escalated first, by the escalations queue, then the quietest), idle, ended (folded, newest end first); each row its state, the mission and task it holds, what it does now, its last event; the sidebar's session list uses the same groups |
| Agent detail | A box to type to its session, with what was sent there and whether it was typed or waits for the agent's turn; live terminal; the Jev panel (distribution, needs_human, stuck, the judge's attention flag and its reason, evidence, rules-vs-Jev); the commands into it (an input waiting for the turn says so); state timeline; event timeline |
| Missions | Missions with open tasks first, then idle active ones, then closed ones (folded); each mission's tasks in flight (oldest first), landed or accepted, sent back, cancelled (folded), an objective's retries nested under its latest attempt; residents waiting for a human's approval |
| RSI | Objective missions: the backlog's objectives, tasks, gate verdicts and logs, metrics, the resident's terminal |

The API reads, as `spawn_reader`, with GET only (any other method on a
read is 405): `/api/overview`, `/api/fleet` (with
`/api/fleet/<session>/judgements` and `/commands`, each command with
`waiting` once the Bridge holds it for the agent's turn), `/api/attention`,
`/api/missions`, `/api/events`, `/api/session/<id>`, `/api/pane/<id>`,
`/api/rsi` (with `/api/rsi/task/<id>` and `/api/rsi/pane`) and
`/api/scorecard` (`crates/spawn-cli/src/serve.rs`; the answer of
`/api/attention` is documented in `crates/spawn-cli/src/serve/attention.rs`).

It writes one thing: a person's input to a session, `POST
/api/session/<id>/input` with `{"text", "key"}` (the key, an idempotency
key, may come as an `Idempotency-Key` header). That is the command `spawn
submit --principal human --agent <the session's agent> --session <id> --key
<key> --action '{"type":"input","text":...}'` makes, with the same
idempotency rule (a retried POST types once), and like every person's input
it waits for the session's turn (§3). It answers 202 with the command, which
`/commands` then follows; `{"probe": true}` passes every guard and writes
nothing, so the page knows before it offers the box. It runs as the
`spawn_human` login, whose one right is `spawn_human_input`: an input, as
principal `human`, to the agent the session belongs to; no other action, no
read, no event. Its guards, in order: the read guard (the clients that may
read may write: the token, `SPAWN_SERVE_LOCAL_OPEN`'s owner client,
`SPAWN_SERVE_OPEN_NETWORKS`' clients naming the server by its address);
POST only (405); without the token, an `Origin` naming the server as the
`Host` does, so a page on another site cannot make the owner's browser type,
and, with no token set at all, a client socket the owner's uid holds;
`Content-Type: application/json` (415), which no form can send without a
preflight; a body of at most 16 KiB (413); a key and a text a person types:
not blank, at most 8 KiB, no control character but tab and line ends (400).

`/api/missions` and `/api/rsi` send what the lists show: a task's brief,
report and review note come as their first lines with their whole length
(`<field>_bytes`), and `/api/missions/task/<id>` answers the whole texts
when a task is opened; an objective's prompt comes as its first lines and
`/api/rsi/objective/<mission>/<id>` answers it whole when the objective is
pointed at. Every answer carries a weak `ETag` and a read that
sends it back (`If-None-Match`) gets 304 while nothing it shows changed; a
read that says `Cache-Control: max-age=1`, as the page's polls do, may be
given the answer computed for another reader up to a second ago, so one
computation serves every open page (`crates/spawn-cli/src/serve/answers.rs`).
The page polls only while it is visible and reads again the moment it shows.
Humans talk to the supervisor through that box (top of the Fleet page), its
pane, or `spawn submit --session session_supervisor --action
'{"type":"input",...}'`.

## 6. Invariants

**Storage and compute are separate.** PostgreSQL is the only source of
truth (event log, command queue, grants, the tables folded from the control
stream). Every other process is compute: it holds nothing that cannot be
rebuilt from storage, and everything people read (SQL views, API, UI,
metrics, corpora) is derived from storage. A model judgement becomes storage
the moment it is written, together with the evidence it saw.

Known edges, each with its rule:

| Edge | Rule |
| --- | --- |
| The UI's live terminal reads tmux | A compute peek, labelled as such; the stored copy is the screen tail in each model observation |
| The supervisor's native transcript lives on the host (`~/.claude`, copied to the host's transcript archive) | A cache; the handoff rebuilt from the log is the authoritative recovery path (§3.1) |
| The attention queue reads the supervisor's question and its "For the owner:" lines from its transcript | A compute peek at the transcript the log names (`session.native_started`), read by the Observer's own reply reader; where it cannot be read, the screen tail stored with the judgement stands in for the question, labelled as such, and no line for the owner is listed |
| Code lives in git | Artifact storage; every commit is referenced from an event |

- The event log is the only state; every process is restartable.
- `EventKind` stays closed; observation fields are additive.
- Facts > rules > Jev; observation never acts; Jev alone triggers only reversible, bounded actions.
- Worker panes hold no database login. The Bridge reads what an engine does from files it writes anyway and from its screen: the Claude transcript (the Bridge names the session with `--session-id`, so it knows the path before the engine starts), the pane read every tick, and the exit file the runner writes. The supervisor's pane gets `spawn_supervisor`, which reads the fleet and submits commands through the gateway but appends no events of its own. The Bridge, the Observer and the controllers run as the owner. `spawn serve` reads as `spawn_reader`, which can write nothing, and types a person's input as `spawn_human`, which can call `spawn_human_input` and nothing else (an input, principal `human`, to the session's own agent); it binds loopback, and on any other address needs `SPAWN_SERVE_TOKEN`. Roles belong to the whole cluster, so each database has its own set, named after it (`spawn_supervisor_<db>`, `spawn_reader_<db>`, `spawn_human_<db>`, SQL `spawn_login_role`); grants, commands and records still name the principals `spawn_supervisor`, `spawn_reader` and `human` (`spawn_principal`). Two installs on one cluster thus share no privileges and set none of each other's passwords. The cluster-wide `spawn_supervisor`, `spawn_reader` and `spawn_terminal` of earlier installs lose every privilege in the schema and act as nobody; a migration warns while one can still log in, for a human to drop it. A login's password is PBKDF2-HMAC-SHA256 of the owner's password (600,000 rounds), salted with a random salt kept in the login's role comment and its name (`spawn_postgres::role_url`), and set by every migration: nothing secret is stored, only a holder of the owner's password can form it, and a login that knows its own password and salt pays the rounds for every guess at the owner's. The owner's password is percent-decoded first, so every spelling of it agrees. An owner URL without a password derives nothing: `role_url` refuses, and such a migration turns the logins off (no login, no password, with a warning) until a migration that has the owner's password.
- No model key in Spawn: the DeepSeek key stays in the model relay's process, the gateway token in Claude Code's own settings (§3.3).
- Every landing goes through the black-box gate, a host-side commit, a compare-and-swap move of the baseline, and the halt rules (§10).

## 7. The closed loop

```text
 worker acts ──► observe (rules → Jev) ──► dispatch (rules → Jev nudge → supervisor → human)
      ▲                                              │
      └──────────── command ◄────────────────────────┘
```

The supervisor is kept alive by the liveness rule (resume, then handoff),
receives escalation digests and submits to workers. Workers are Claude Code
sessions started for the supervisor's tasks. Observation runs rules first and
Jev on ambiguity, with the full payload. The dispatcher's policy decides
fixed cases by rules, allows one guarded Jev-gated nudge (`continue`), and
escalates everything else to the supervisor, which may escalate to a human.
A human reads the escalations queue when they choose and types to any
session from the web page (`POST /api/session/<id>/input`) or with `spawn
submit --principal human`.
Every decision is in the log with its evidence and what followed it.

### 7.1 The end-to-end check

`scripts/e2e.sh` runs the whole loop with a scripted engine and a stub Jev,
on real PostgreSQL, git and tmux, in its own schema and tmux socket (about
20 s). The gate runs it as `rsi/scenarios/check-e2e.sh` on every landing. It
asserts:

1. The liveness rule starts the supervisor.
2. A rule decides WORKING while a tool runs; Jev decides an idle prompt, and
   the supervisor gets the digest.
3. The guarded nudge restarts a stalled worker, once.
4. A `HUMAN:` question reaches the attention queue even when Jev is
   confident it is fine, and a human command resolves it.
5. With Jev down, a stalled worker is escalated on rules alone, not nudged.
6. Missions: a resident runs only after a human approves the supervisor's
   proposal; its tasks are delivered one at a time, reported, sent back once
   and accepted; a mission without a source works in a scratch workspace; an
   ephemeral worker gets its mission's repository, reports, and is stopped
   once accepted; a new worker starts from the source as it is, not as it
   was first mirrored.
7. Landing: accepted work passes the gate and lands on its source, the
   second landing as a merge; work the gate rejects goes back to its worker
   with the gate's output; a push outside the gate halts the mission until a
   human resumes it; an objective becomes a resident's task, the gate alone
   lands it, and a solved objective is not assigned again.
8. A human message to a worker reaches the supervisor; a killed resident is
   resumed; one the supervisor stopped stays down.
9. Killing the supervisor's engine, then its tmux server, ends with the same
   `session_supervisor` resumed natively; killing the loop leaves the same
   supervisor execution running.

## 8. Known limit: one uid

Every engine runs as the owner's Unix user, and all panes share one tmux server. The database boundary holds
(no pane can write the log), but the file boundary does not: an agent can
read another process's environment through `/proc` or tmux (including the
supervisor pane's login), write the files the Bridge reads for another
session (a transcript, an exit file), which the Bridge then reads as that
session's facts, or simply `tmux send-keys` into another pane. The Bridge
reads those files without following symlinks, refuses anything but a regular
file and caps each read, so a planted FIFO or an endless file cannot stall
or exhaust it. Holding no login stops a worker from forging facts through
the database; it does not isolate agents from each other. Isolation between
agents needs one uid (or one sandbox) per agent. The gate runs candidate
code in a bubblewrap sandbox under the same uid (see docs/rsi.md §4).

## 9. Not built

- **Experience views in SQL** (`decisions`, `outcomes`, `interventions`)
  and outcome labels: the log holds what followed each judgement, but
  nothing derives, exports or replays the episodes.
- **RSI objectives over the corpus**, judged by black-box replay through the
  candidate's `spawn` binary on a corpus snapshot the candidate cannot edit:
  rules coverage (share of episodes decided by rules up, agreement with the
  labelled outcome kept), Jev filtering (escalations to the supervisor down,
  outcome accuracy kept), playbook reach (interventions in the corpus the
  playbook would have resolved, up), and human interventions per agent-hour
  on live runs. Nothing replays the playbook over recorded episodes.
- **A write endpoint in the UI** (an input box to the supervisor). Humans
  act with `spawn submit --principal human`.
- **More dispatcher rules:** stopping a COMPLETED one-shot task and
  retrying after a transient API error (both escalate), and the actions
  `restart` and `reassign`.
- **Ladder health over long periods.** The UI's figures cover the last 24 h.
- **The backlog, scenarios and gate versioned with the source**
  (docs/storage.md).
- **The Review point in code** (§1.1). Review sessions are run by the lead
  and their verdicts are kept outside the log. No record type holds a
  verdict, `spawn task accept` does not require one on the report's
  revision, and a gate-mode mission's integrator gates a report without
  one.

## 10. One log, one landing path

RSI is a mission whose tasks the gate accepts; there is no separate RSI
controller. Missions, tasks, residents and landings are events in
`spawn_events`, and the `mission_*` tables are a projection of them.

### 10.1 The control stream

- **Where it lives:** one stream with session `session_control`, agent
  `agent_control` and source `control`. Every record is the event
  `control.recorded`, whose detail is `{type, ...}`. A new record type adds
  no `EventKind` variant.
- **Record types:** `mission.added`, `mission.configured`, `mission.closed`,
  `mission.halted`, `mission.resumed`, `source.moved`, `source.held`,
  `source.released`, `source.hold_expired`, `task.assigned`,
  `task.delivered`, `task.report_unfinished`, `task.reported`,
  `task.reviewed`, `task.cancelled`, `task.gated`, `task.held`, `task.landed`,
  `task.aborted`, `resident.proposed`, `resident.approved`,
  `resident.retired`, `legacy.imported`, `host.disk`.
  `task.report_unfinished` is a delivered task's report the dispatcher read
  and did not take because it is still a draft: it names the task's
  `revision`, the content's `digest`, the `markers` (each with its `line`,
  `kind`, `marker` and `text`) and the note that told the worker (`note`,
  its idempotency key, and `command_id`); one per revision and content,
  folded into no table (the task stays delivered).
  `legacy.imported` carries one row of the mission tables as it stood
  before the control stream existed: a database with mission rows and no
  records imports them once, on migration. A `task.cancelled` names `by`,
  who cancelled: `human` (the owner's login, `spawn task cancel` and a
  person's close or retirement), `spawn_supervisor`, or the loop that did
  (`spawn-integrate`, `spawn-dispatch`, `spawn-migrate` for a migration's
  own records); a record from before it names none and counts as a
  person's. The fold does not read it; the scorecard does
  ([design.md](design.md) §15).
- **Who writes it:** only the checked mission functions, through
  `spawn_control_record`. The integrator's records (`task.gated`,
  `task.landed`, `task.aborted`, `mission.halted`), `source.moved` and the
  loop's `host.disk` (through `spawn_host_record`; folded into no table,
  [storage.md](storage.md#disk-room)) need the owner's login. The
  restricted logins write no events at all: `spawn_append` refuses them.
- **The tables are a projection.** `spawn_control_apply(record)` is the only
  writer of `mission_*`, and it runs in the same transaction as the append.
  `spawn_control_refold()` rebuilds the tables from the log, and a test
  checks that the rebuilt tables equal the live ones.
- **It is not an agent.** It is left out of the session projection, and the
  Bridge and the Observer skip it.

### 10.2 One gate

`scripts/rsi/gate.sh <repo> <base> <candidate> <scenarios> [<scenario>]`
judges every landing, RSI or fleet. It exports both commits to temporary
directories and judges the candidate with the controller's scenarios, never
the tree's own:

- it refuses changes to the paths in `<scenarios>/protected` (`rsi/`,
  `scripts/rsi/`, `scripts/blackbox/`, `scripts/e2e.sh`), any build-time code
  (`build.rs`, `.cargo/`, a `build` key in a Cargo.toml), any change to lint
  configuration, and fewer runnable tests than the base;
- `<scenarios>/build` builds each tree;
- when a task has a scenario, that scenario must fail on the base (exit 1)
  and pass on the candidate;
- every other scenario must pass on the candidate, except ratchet scenarios
  and objective scenarios (`<digits>.sh`) the base fails too. The checks are
  scenarios: `check-clippy`, `check-tests`, `check-e2e`, `check-blackbox`,
  `check-web`. The halt rule on the scenario count (§10.3) protects them.
- everything the candidate controls runs in a bubblewrap sandbox (docs/rsi.md
  §4). The verdict is one JSON line that names the base and candidate
  commits it judged; the integrator believes only the verdict file it owns.
- objective results on the base (solved, or unsolved after two runs that
  agree) are cached outside the sandbox, keyed by everything that decides
  them: the base commit, the blobs of the scenario and of the gate, the
  tools the scenarios run (`rustc -vV`, `cargo -V`, `node --version`,
  `psql -V`, `git --version`, `tmux -V`, `python3 -V`, `jq --version`, in
  the sandbox inside the base tree) and the test server's
  `server_version_num`, the scenarios directory's build hook and helpers
  (and the `bind` list's text), a fingerprint of each directory the `bind`
  list names (the blobs of pnpm's `.modules.yaml` and `.pnpm/lock.yaml`
  when present, else a listing of the directory: path, type, size, mtime,
  link target), and the files the scenario names on its code lines
  (`scripts/...` of the controller, transitively; a scenario that reaches
  into `scripts/` through a variable or a `cd` keys all of it). A
  controller file no scenario names (the black-box runner) changes no key.
  Not keyed: an edit inside a bound directory that has pnpm's install
  record but was not installed, and the state candidate code leaves on the
  shared test server (a disposable server per gate is GitHub issue #6;
  until then one gate runs at a time). Only the gate
  writes an entry, after a completed base run; an entry it cannot read is
  a miss; candidate runs are never cached; the log lists which base
  results came from the cache, and the time spent on the base, the
  candidate and the setup. A hit touches its entry; spawn-maintenance
  removes entries no gate used for 14 days.
  Candidate code cannot see or write the cache, but any process of the
  same uid outside the sandbox can (the same-uid limit).
- **Exit status.** The gate exits 0 with a verdict. It exits 5 when it has
  no room to judge now: `TMPDIR` has less than `SPAWN_GATE_MIN_FREE_GB`
  free (`scripts/disk.sh` `gate_room`). In that case it first writes the
  fact `{"no_room":true,"free_kb","min_gb","tmpdir"}` to the verdict file,
  before any scratch is made or any candidate code runs. On a disk with no
  room at all, that write may fail, and the gate still exits 5. The
  integrator reads `no_room` from exit 5 together with the fact (`gate.rs`).
  Without a readable fact, it measures the free space itself (`df`), and it
  records `no_room` with its own measurement only when that is under the
  floor. It does this in two cases: the gate exited 5 (it measures the
  gate's `TMPDIR`), or the integrator could not create or use its private
  directory for the gate (it measures that directory's parent). Any other
  status is an abort: exit 3 (no bwrap, a missing bind directory, an unreadable
  `TMPDIR`, a floor that is not whole GiB), exit 4 (the base's run errored),
  a timeout or a signal. The integrator measures the same floor, free
  memory and the test database itself before it starts the gate (§10.3,
  step 3), so a short host is found before a gate runs; the gate's own
  check and exit 5 stay, for a `TMPDIR` that fills in between.

`scripts/rsi/test-gate.sh` tests the gate itself.

The controller is the deployed commit's tree, never the owner's working
checkout: `~/.local/share/spawn-rsi/controllers/<name>`, a symlink to
`<name>-<commit>`, switched atomically by a deploy (`deploy/controller.sh`,
deploy/systemd/README.md "The controller"). The integrator's `SPAWN_GATE`,
each mission's scenarios and backlog, and the maintenance script are paths
under it, and the integrator resolves the symlink once per step, so a gate
judges with one commit's gate, scenarios and helpers. **An owner edit is
live only after its deploy**: a change to the gate, a scenario or the backlog
is judged like any other commit before anything runs it, and what runs is a
record of what was judged (§14 of design.md).

### 10.3 Landing

- **Mission settings** (`spawn mission configure`, owner only): a gate (a
  scenarios directory in the controller, the deployed tree), how tasks are accepted,
  and an optional budget:
  - `review`: the supervisor accepts, then the gate runs;
  - `gate`: the gate alone decides. The supervisor is not told of its
    reports (`mission_news`) and `spawn_task_review` refuses them, because
    a review could send back the revision the gate is judging. Both stand
    until the Review point is built (§1.1, §9);
  - budget: `--minutes` and `--tool-calls`. A delivered task over budget is
    aborted (`task.aborted`) and its worker stopped (the Stop is submitted
    before the record, so a failed record is retried with it; a Stop that
    fails is pursued and escalated by the dispatcher); an abort counts
    toward the abort halt, not as a rejection.
    The minutes are active time, never the wall clock (`integrate::active_ms`):
    from the delivery, through each event of the task's session, to now.
    Readings are no events here: the Bridge re-reads a busy pane every
    30 s (`screen.observed`, and the observer's `observation.recorded`),
    also while Claude Code only retries a model API that is down. A gap
    counts at most `SPAWN_BUDGET_GAP_SECS` (300 by default), and only if it
    ends in the model's progress: a tool call or background job starting
    or ending, a turn stopping. A gap that ends in a failed turn, a prompt
    arriving (`message.received`: a delivery, a nudge, a job's
    notification), a command (a delivery, a nudge, a resume) or the
    engine's own events counts nothing: the worker waited. The gap to now
    counts at most the cap. So a silence of any length or cause (the fleet
    stopped, the host frozen or rebooted, no live execution, the model API
    down) adds at most one cap, and no outage can by itself put a task over
    budget (2026-11-07 23:05: a five-hour stop aborted tasks 113 and 159 by
    the wall clock). One bound is not the cap: a subagent's job open across
    a model API outage retries out of the log's sight and, once it fails,
    ends; its gap counts the fleet's alive time, so up to Claude Code's
    retry window for a subagent, not the outage's length. The safe side: a
    model that thinks or streams longer than the cap without a tool call
    counts the cap.
    A gap inside a tool call or background job that later
    ended (a build, a gate in the worker) counts the time the whole fleet
    was alive in it: the log's gaps between those two events, each capped
    the same way, since an outage either kills the tool's engine or writes
    nothing anywhere.

  A gate needs a source that is a directory on the integrator's host:
  agents' branches reach no other (storage.md, "Landing"), so a gate on a
  URL is refused, and a mission configured with one before that is halted
  by the integrator with the reason.
- **Verdicts name what they judged.** A task's report has a revision (its
  `revisions`, one more each time it is sent back). `task.reviewed` and
  `task.gated` name the revision they judged (`spawn task accept|reject
  --revision R`, which the supervisor's digest gives; without it, a human's
  review takes the report as it is now). The checked functions refuse a
  verdict on a revision the task has left, and the fold applies none: a
  gate run on revision 0 never lands, or sends back, revision 1.
- **The integrator:** `spawn integrate` (unit `spawn-integrate`). A pass
  first records the owner's holds past their bound as expired (below,
  "Landing: the owner's hold"). A mission
  whose source it cannot fetch this pass is skipped: the mirror's last copy
  of `main` is no reading of the source's. A mission whose source has
  another mission's task in `landing` waits for it (the push may have moved
  `main` before its `task.landed`), unless that task waits behind the
  owner's hold on the source (its `task.held` names the active hold: it
  pushed nothing). Every pass visits every mission (its
  halts, budget, landing and objective assignment never wait for another
  mission's gate); only a landing makes the rest of its source wait for the
  next pass, and a task waiting behind a hold is no landing. A free gate goes to the ready task that has waited longest,
  across missions, counted from its report or from the end of its
  mission's previous judging, whichever is later; the mission id decides
  nothing. For a mission's lowest ready task (one per mission at a time:
  §10.4, "One gate at a time, for now"), on a thread of its own, it:
  1. fetches the agent's pushed branch, the source's `spawn/<agent>`, into
     its mirror's `refs/spawn-integrate/<agent>` (it never commits a
     workspace and never moves `refs/heads/<agent>`). A push that lags the
     agent's latest checkpoint is gated `stale` after a short wait: no gate
     run, no rejection, the task stays ready and is judged again later
     ([storage.md](storage.md), "Landing");
  2. builds the candidate, the landing commit C: one merge commit whose
     first parent is `main` (the base) and second the pushed head, with the
     head's tree when `main` is in it and the merge by `main`'s merge rules
     when `main` has moved, never a fast-forward to a worker's commit. Its
     message says what landed: the objective's title, the objective and the
     task on the first line (for a task without an objective, the brief's
     first line and the task), then the report's `## What changed` (the
     whole report without one, bounded, a line that could read as a
     `Spawn-` trailer indented), then `Spawn-Task`, `Spawn-Objective` and
     `Spawn-Head` trailers. So `git log --first-parent main` is one line
     per landed task, `git bisect start --first-parent` visits only gated
     landings, `git revert -m 1 <C>` undoes one task, and every checkpoint
     stays reachable through the second parents ([storage.md](storage.md),
     "Landing"). A conflict first merges `main` into the agent's
     idle workspace, with conflict markers, then sends the task back with a
     note that names the marked files ([storage.md](storage.md), "Main into
     a workspace"). A branch with a pending merge of `main` or with conflict
     markers is sent back as `conflict` before it is judged.
  3. measures the host before the gate starts: **the host's room is
     checked before a gate, not discovered by its crash.** For this
     judging's task only (one that got a gate slot; a task that waits is
     neither checked nor recorded), in this order, stopping at the first
     that falls short:
     - disk: the free KiB (`df`) of the gate's `TMPDIR` (`/tmp` when unset)
       against `SPAWN_GATE_MIN_FREE_GB` (default 10, the floor
       `scripts/disk.sh` reads too: one floor, one meaning);
     - memory: `MemAvailable` of `/proc/meminfo` in MiB against
       `SPAWN_GATE_MIN_FREE_MEMORY_MB` (default 2048; the integrator's
       only, not passed to the gate);
     - database: when `SPAWN_TEST_DATABASE_URL` is set in the integrator's
       environment (whatever the owner's-cluster check then decides for the
       gate), a connection within 10 s, then, in one transaction that
       changes nothing, `SET LOCAL max_parallel_workers_per_gather = 2`,
       `parallel_setup_cost = 0`, `parallel_tuple_cost = 0`,
       `debug_parallel_query = on` (`force_parallel_mode` before
       PostgreSQL 16) and `SELECT count(*) FROM generate_series(1, 100000)`
       within 10 s: a Gather with a worker, so the dynamic shared memory the
       tests need (`/dev/shm` in a container) is exercised. Any error fails
       it. Not for a mission whose scenarios hold `no-test-database` (§10.4):
       its gate never gets the URL, so a test server that is down does not
       hold back its work.

     A threshold of 0 disables its check; one that does not parse is an
     abort, as a bad `SPAWN_GATE_MIN_FREE_GB` is to the gate. A check that
     falls short runs no gate: the task is gated `no_room` (step 5) naming
     the check, its measure and its threshold. On 2026-11-06 a test server
     with a 64 MB `/dev/shm` failed "could not resize shared memory segment"
     under the tests' parallel queries and the gate read it, 25 minutes in,
     as a regression of the candidate; the probe finds it in seconds, before
     any candidate runs. The decision rests on measures taken at the
     judging, recorded with their thresholds (design.md §14);
  4. runs the gate on C, with a wall clock (`SPAWN_GATE_WALL_CLOCK_MINUTES`, 120 by default). When the gate
     rejects C and C is a real merge (the pushed head does not contain
     `main`, so C's tree is not the head's), it then judges the head alone
     before recording anything: the same gate, scenarios and task scenario,
     base `head_base` = `git merge-base <head> <main>` (the `main` the head
     last took in), candidate the pushed head. The head passes: the verdict
     is `incompatible` (below). The head fails too: `rejected`. The head's
     run gives no verdict: `aborted` (or `no_room`), naming the head. When
     the head contains `main`, a rejection of C is a rejection and no second
     run happens. The decision rests on two recorded verdicts on named
     commits, never on the text of a reason or on timing. The second run
     happens only for a rejected real merge and doubles that pass's gate
     time;
  5. records `task.gated` (verdict, log, metric, C as `candidate`) before
     git moves. A check before the gate that fell short (step 3), or a gate
     that had no room (exit 5 with its fact, §10.2), is gated `no_room`. This
     is no verdict on the work. The task stays where it was, and the record
     names `check` (`disk`, `memory` or `database`; the gate's own floor is
     `disk`), `measured` and `threshold` (free KiB and the floor in KiB, with
     `free_kb`, `min_gb` and `tmpdir`; available MiB and the floor in MiB;
     the error and the probe it failed, with `server` as
     host:port/database or the socket directory, never the URL's password),
     `log`, `escalate`, and a reason that names the check, the measure and
     the threshold, ending "not a verdict, judged again later". It moves neither
     `gate_aborts` nor `gate_rejections`, so a disk that another project
     filled halts no mission. The fold keeps the mission's current run of
     them, which belongs to one task (`gate_room_task`, `gate_room_since_ms`,
     `gate_room_last_ms`, `gate_room_waits`, `gate_room_escalated`). These
     end the run: a gate that judges, conflicts or aborts, the task leaving
     the gate (`task.cancelled`, `task.aborted`), or `mission.resumed`. A
     `stale` leaves it. A `no_room` on another task starts a new run, with
     no inherited age, backoff or escalation. The integrator tries that task
     again only after a backoff from the latest `no_room`: 5 minutes,
     doubled each time, at most 40. The first `no_room` more than 2 hours
     into a run carries `escalate`. The dispatcher's digest takes that
     record to the supervisor, naming the check, its measure and its
     threshold ("no room for the gate on this host"). It stays pending until a delivered digest
     carries its tag (`host gate-room-<event>`), however long that takes, so
     it is told once.

     **A host fault is not a verdict on the work either.** When the gate
     rejects C with `regression: check-tests.sh fails on the candidate`,
     the integrator reads the failures check-tests.sh printed, from the
     gate's recorded output, in the check-tests section (from `| ==
     regression: check-tests.sh must still pass` to the next `| == ` or `|
     stage gate ` line). In the owner's form (one `| FAILED <test>: <first
     message line>` per failing test, then the excerpt), the FAILED lines
     are the failures and the excerpt is not read; in today's form (the
     40-line excerpt alone) a failure is a `| thread '<test>' panicked at
     <location>:` line with its message (up to a `note:`, `---- `, `test
     result:`, `--` or blank line), or the `| Error: ...` line of a test
     that returned an error. A failure the excerpt cut off is not seen. A
     failure is the host's when its message is a connection error to the
     test database (`error connecting to server`, `Connection refused`,
     `connection closed`, `server closed the connection unexpectedly`,
     `terminating connection because of crash of another server process`,
     `the database system is in recovery mode` / `is starting up` / `is
     shutting down` / `is not yet accepting connections`, `could not resize
     shared memory segment`, `SqlState(E57P0`, `SqlState(E080`), or a
     database error of any kind (`db error`, `Error { kind: Db`) while the
     test database's log (`SPAWN_TEST_DATABASE_LOG`, optional, lines as
     PostgreSQL writes them with `log_line_prefix` `%m [%p] `, `UTC` or a
     numeric offset) holds a crash line (`was terminated by signal`,
     `terminating any other active server processes`, `all server
     processes terminated; reinitializing`) stamped within the gate's run as
     the integrator measured it, one second either side. Unset or
     unreadable, the log is no evidence (said once). At least one failure
     read and every one the host's: `host_fault`, not `rejected` (and the
     head is not judged alone). The task stays where it was (`reported`, or
     `accepted` under review), its revisions and note do not move, nothing
     goes back to the worker, and neither `gate_rejections` nor
     `gate_aborts` grows or resets. The record names `base`, `candidate`,
     `revision`, `scenarios`, `reason` (`host fault: ...`, saying which
     evidence), `log`, `evidence` (`failures` when every failure is a
     connection error, else `server_log`), `failures` (per failure its
     `test`, its message's first line, bounded, and `class`, `connection`
     or `database`), `crash` (the log lines, when used), `host_faults`,
     `backoff_ms`, `due_ms` and `escalate`; the store refuses one without
     its evidence and writes `due_ms` itself, the record's own
     `recorded_at_ms` plus `backoff_ms`. The fold keeps the mission's host
     faults in a row (`gate_host_faults`, with the latest one's task and
     `due_ms`): + 1 on `host_fault`, unchanged on `no_room` and `stale`, 0
     on every other verdict, on `task.landed` and on `mission.resumed`.
     After the k-th in a row the task waits
     `SPAWN_GATE_HOST_FAULT_BACKOFF_SECS` (300 by default) x 2^(k-1), at
     most an hour: no pass gates it before the recorded `due_ms` (the
     mission's other tasks wait behind it, as behind a `no_room`), and the
     next pass after it judges it again. The one that makes the run 2
     carries `escalate`: the dispatcher's digest takes it to the supervisor
     once, as `host gate-host-fault-<event>` ("host faults at the gate"),
     naming the task, the mission, the count, the evidence (the crash lines,
     or the first failure's test and message) and when the task is judged
     again. A real failure among them, or none read, keeps it `rejected`;
     any other rejection reason is judged as before. The decision rests on
     the failures and the crash lines named in the record, the wait on the
     recorded `due_ms`, never on the reason's text alone or on time without
     a record (design.md §14).
  6. moves `main` on the source with a compare-and-swap push
     (`--force-with-lease` on the base); if `main` moved meanwhile the
     verdict is `stale` and the task is judged again, unless the candidate
     is already in `main`'s history (an earlier push of this landing whose
     record was lost), which is a landing. Under the landing lock, before
     the push, it reads the owner's hold on the source from the store: on a
     held source it pushes nothing (below);
  7. records `task.landed` (`from`, `to` = C).
- **Landing: the owner's hold.** An owner deploy of Spawn gates a candidate
  against the live `main` for 25-80 minutes, then `deploy/deploy.sh`
  refuses unless the verdict's base is still the live `main`: a task landing
  in between makes the verdict stale (2026-11-06 21:45: task 85 landed
  during the owner's gate). Stopping `spawn-integrate` for the whole gate
  prevented that and stopped everything else too: every mission's gates
  (killed with no record) and objective assignment, which runs only in the
  integrator. The hold stops landings on one source and nothing else:
  - `spawn mission hold --source <source> [--reason <text>]` (owner only;
    the supervisor's login is refused). `<source>` is a mission source as
    the missions name it (the first source of at least one mission, whatever
    its state); a source no mission names, or a remote one, is refused and
    nothing is recorded. Under the integrator's landing lock (so a landing
    whose push has begun records its outcome first) it reads the source's
    `main` M and records `source.held` {`source`, `main`: M, `by`: `human`,
    `bound_s`, `reason` (at most 500 characters, when given), `renews`};
    the store writes `expires_ms`, the record's own time + `bound_s` x 1000.
    `bound_s` is `SPAWN_SOURCE_HOLD_SECS` in the command's environment, a
    whole number from 1 to 86400, 10800 (3 h) by default: longer than the
    slowest owner gate plus a deploy, and than the gate's default wall
    clock. Any other value is refused, naming the variable. It prints
    `{hold, source, main, expires_ms, renews}`: gate the candidate against
    that `main`. A hold on a source already held renews it: a new
    `source.held` naming `renews` (the active hold), which ends with it (no
    release or expiry is recorded for it). A source has at most one active
    hold.
  - `spawn mission release --source <source>` (owner only) records
    `source.released` {`source`, `hold`, `by`: `human`} and prints
    `{source, released, waiting}`. With no active hold it records nothing
    and prints `released` null, so a deploy may release on every exit path.
  - The fold keeps the active holds in `mission_source_holds` (by source:
    `hold`, `main`, `by`, `reason`, `held_at_ms`, `expires_ms`), set by
    `source.held` and removed by the `source.released` or
    `source.hold_expired` naming the hold; `spawn_control_refold()`
    rebuilds it, and each task's `held_by`.
  - While a source is held, no task lands on it. `land()` reads the hold
    from the store under the landing lock, before its push, never from
    memory, and pushes nothing. A candidate already in `main`'s history
    landed before the hold and is recorded `task.landed`; any other records
    `task.held` {`id`, `hold`, `source`} once per task and hold (the store
    records nothing for a second, and accepts one only for a task in
    `landing` and the active hold on its mission's source), and the task
    stays `landing` (`held_by` the hold). So a hold never splits a push from
    its record. The source's gates still run and record their verdicts (a
    judging thread's own `land()` records its `task.held`). A task waiting
    behind the hold ends nothing early: its mission's step goes on to the
    baseline check and objective assignment, its source is not counted as
    moved, and the mission's other ready tasks wait behind it (one task per
    mission between its gate and its landing); the other missions on the
    source are judged as usual. Assignment, the gates and landings of other
    sources, halts, budgets and `spawn mission moved` go on, and the halt
    for a `main` moved with no record stays.
  - After a record ends the hold, a waiting task lands at the next pass by
    the rules above: when `main` moved meanwhile (a deploy that recorded
    `source.moved`) its landing is `stale` and it is judged again against
    the new `main`, never discarded and never counted.
  - The bound. At the start of each pass, before any mission's step, the
    integrator records `source.hold_expired` {`source`, `hold`,
    `expires_ms`, `waiting`: the tasks waiting behind it, oldest first} for
    every active hold whose `expires_ms` the database's clock
    (`spawn_now_ms()`) has reached; the store refuses it before then and for
    a hold that is not the active one, so a release or renewal recorded first
    wins. Only that record ends the hold: past `expires_ms` without it
    nothing lands, and a hold that expired while the integrator was down is
    recorded expired at its first pass, before anything lands. The
    dispatcher's digest tells the supervisor once ("a source hold expired",
    session `missions`, tag `missions source-hold-expired-<event>`): the
    source, the hold (its event, reason, when it was taken and its bound),
    when it expired, each waiting `task #<id>` with its mission, and that
    landings resumed, so an owner verdict judged against the hold's `main`
    may be stale. A released hold is told to nobody.
  - Visible: `spawn mission list` and `GET /api/missions` carry `holds`
    (every active hold with its `waiting` tasks) and each mission's `hold`
    (the active hold on its source, or null); `GET /api/attention` carries
    `holds`, only those with a task waiting, beside `pending` and `told`
    (the supervisor is told of an expiry, never of a hold); `spawn task
    stages` shows `held`. A task waiting behind a hold is no landing stall
    (§3, "Stalls").
  - The owner's use (`deploy/hold.sh`, `deploy/deploy.sh`): hold the Spawn
    source, gate the candidate against the hold's `main`, deploy; the
    deploy releases the hold once `main` moved forward and `source.moved`
    is recorded, or on any failure. A hold forgotten or left by a crash
    expires by itself. Nothing in it stops or restarts `spawn-integrate`.
    Every decision rests on records read from the store under the landing
    lock (design.md §14); the expiry's only use of time is the recorded
    `expires_ms` against the database's clock, and what ends the hold is
    its record, not the clock.
- **On rejection:** the task goes back to the worker (`revise`), with the
  gate log as the note. A `rejected` decided after a run of the head alone
  names `head`, `head_base` and `head_verdict` (`rejected`), so the record
  shows why it counted.
- **On incompatible** (the head passes alone and fails only merged with
  `main`: a semantic conflict, like task 36 adding fields to a struct that
  task 48's new test builds): the task goes back (`revise`, one more
  revision), as on `conflict`, and it is not a rejection of the work. Its
  record names `base` (`main`), `revision`, `scenarios`, `candidate` (C),
  `head` (C's second parent), `head_base`, `head_verdict` (`accepted`),
  `main_commits` (`main`'s first-parent commits in `head_base..base`,
  oldest first, full hashes: what the work must adapt to), `reason` and
  `log` (C's) and `note`. The note says the work passes on its own base but
  not merged with `main`, quotes C's gate output (its tail), lists each of
  `main_commits` by its 12-digit prefix and subject, and says `main` is
  merged into the workspace with this revision: the dispatcher's delivery
  of the revision merges `main` into the idle workspace, as every delivery
  does.
- **Closure is one rule** (`spawn_task_may_continue` in `schema.sql`). Once a
  mission is closed, or a task's resident retired, work goes on only where
  it asks nothing new of a worker:
  - a done mission with a gate keeps its delivered, reported, accepted and
    landing tasks: reports still come in, are judged or reviewed, and land,
    and a delivered task is still held to the budget;
  - a retired resident's accepted and landing tasks still land;
  - a dropped mission keeps nothing.

  Every other open task is cancelled by the close or the retirement, for a
  done mission as for a dropped one. The fold of `mission.closed` or
  `resident.retired` cancels it in the tables, and the writer
  (`spawn_mission_close`, `spawn_resident_retire`) then appends one
  `task.cancelled` record for each task it cancelled. So `spawn task stages`
  ends with `cancelled` for every cancelled task, and no reader has to
  infer the stage. The fold never appends: rebuilding the tables
  (`spawn_control_refold()`) gives the same tables and adds no record. A
  database from before this rule gets the missing record once, on
  migration. This covers each cancelled task whose last `task.%` record is
  neither `task.cancelled` nor `task.aborted`. The
  integrator's selection uses the same rule, and no transition moves a task
  into a state the rule drops: a send-back is refused, a gate rejection
  cancels the task, and deliveries and reports are not recorded. A worker
  still working on a task the rule cancels is stopped, and a resident gets
  the task's work set aside and is told before anything else (§3.2, "When a
  task closes").
- **Halt rules,** per mission and read from the log:
  - three gate rejections in a row: `rejected` counts; `accepted` resets
    the count; `incompatible`, `conflict`, `stale`, `aborted`, `no_room`
    and `host_fault` leave it (three `incompatible` in a row halt nothing);
  - three gate aborts in a row (`no_room` and `host_fault` are not aborts,
    and leave the count);
  - the scenario count below its floor (the most scenarios a gate run of
    the mission has counted since it was configured or last resumed);
  - the source's `main` not where the last landing left it.

  A halted mission lands nothing until a human runs `spawn mission resume`.
  No rule counts host faults, however many in a row: a halt needs a human,
  and a host fault is the host's, so it is judged again after its backoff
  and escalated at two in a row instead.
- **A shared source has one baseline.** A landing moves the baseline of
  every mission on that source whose baseline was its `from` and that is not
  halted (the record names them), so RSI and fleet work on the same
  repository don't halt each other, and a move of `main` that no landing
  explains still halts the missions that saw `main` before it. A deploy that moves `main` forward records
  `source.moved` (`spawn mission moved`, run by `deploy/deploy.sh`), and the
  missions on that source take the new `main` as their baseline instead of
  halting.

### 10.4 RSI as a mission

- **The RSI mission** is an objective mission: one with a backlog and a
  resident (`spawn mission configure --backlog <dir> --resident <name>`). On
  the live host:
  - source: `missions/spawn.git`;
  - gate: `rsi/scenarios`;
  - acceptance: `gate`;
  - budget: 45 minutes and 60 tool calls;
  - worker: the resident `rsi`, approved by a human.
- **Tasks come from the backlog** (`rsi/backlog/*.json`, in the controller:
  an objective committed to the owner's checkout is assigned only once
  deployed). A task in flight is judged with its objective's scenario in the
  mission's scenarios directory as configured now, not the path recorded at
  assignment.
- **The worker limit** N (`--workers N`, 1 to 16, stored as `"workers": N`
  in the mission's objectives; default 1) is how many of the mission's
  tasks may be in flight at once. A task is in flight from `task.assigned`
  until it lands or is cancelled (assigned, delivered, revise, reported,
  accepted, landing).
  - Each integrator pass fills the free slots with the open objectives
    that have no task in flight (an objective never has two), fewest
    attempts first: fewest yields, then fewest failed attempts since the
    last yield, then the lowest id. With no failures this is the lowest open
    objective, as before. `spawn_objective_assign` checks both rules again
    under the append lock, so a second integrator or a stale read cannot
    oversubscribe.
  - **No objective holds a slot for ever.** A failed attempt is an outcome
    that says the work did not make it: the gate's `rejected`,
    `incompatible`, or `conflict` when the worker got conflicts to resolve
    (the record's `merge` is `marked`, `unmarked` or `pending`, or absent on
    records from before it was named; not `not_mounted`, `busy`, `failed`
    or a clean merge), or an abort over budget. The host's outcomes
    (`no_room`, `host_fault`, a gate `aborted`) count for nothing, as for
    the halt rules, and an `accepted` verdict ends the count. Once an
    objective has `SPAWN_OBJECTIVE_ATTEMPTS` (3, a whole number from 1)
    failed attempts since its last yield, its task yields the slot as soon
    as it is being worked again (assigned, delivered or `revise`; a
    reported task waits for its gate, whose verdict may land it): the
    integrator stops its worker (`integrate-yield-<task>`) and records
    `task.cancelled` with `yielded`, `objective`, `failures`, `limit` and a
    one-line `reason` (the store adds `by: spawn-integrate` and refuses
    any other cancel by the integrator). Its work is set aside like any
    cancel's, no halt counts it, and the supervisor's digest tells it once
    (`missions objective-yielded-<event id>`). The objective goes to the
    back of the queue. A task whose gate runs now, or that this pass
    aborted, does not yield; the slot an abort frees is filled from the
    next pass on, fewest attempts first, so the aborted objective is not
    re-issued at once. Before this, on 2026-11-07, three objectives whose
    tasks kept failing held mission 4's three slots for a day (an abort was
    followed by a new task for the same objective five seconds later), and
    objectives 025 to 034 were never assigned.
  - Who gets which task: the resident takes a task whenever it has none
    open in the mission; every other task goes to a new per-task worker
    (kind `ephemeral`, agent `agent_worker_objective_<task>`, session
    `session_worker_objective_<task>`). It needs no approval, starts from
    the source's main as it is then (or from its objective's earlier work,
    below), keeps no context, and is stopped by the
    dispatcher once its task lands or is cancelled (`retire-task-<id>`). Its
    report reaches the supervisor's digest only in a review-mode mission;
    a gate-mode mission's reports do not (§10.3). With N = 1 this
    is one task at a time on the resident, as before.
  - Landing stays one at a time per mission: one candidate judged at a
    time, the lowest ready task first, main moved by compare-and-swap.
    Across missions the gate goes to the longest wait (§10.3), so a
    mission that is always ready does not starve another's. A task whose base is
    no longer main lands as the merge by main's rules; two in-flight
    objectives on the same lines make the second a `conflict`.
  - Budgets apply per task, in active time from its delivery (§10.3); the halt rules stay per
    mission (three rejections in a row from any of its tasks halt it), and
    a halted mission gets no new task and no judging until a human resumes
    it.
  - The brief is the objective, the working rules, the work its earlier
    tasks left without landing, and the gate verdicts of its recent
    attempts, all read from the log at assignment. For each of the last
    three tasks of the objective that were cancelled, aborted or yielded:
    its id, its outcome and reason (its last gate verdict, abort or
    cancel), its branch and last checkpoint (the latest `artifact.committed`
    of its session while it ran, not a set-aside), and its report, with the
    instruction to start from that work and not redo it (task 161 rebuilt
    2264 lines that task 155 had mostly written).
  - **An ephemeral task starts from that work.** When the newest earlier
    task of its objective that ended without landing left work (a
    checkpoint of its own while it ran), the dispatcher, before the task's
    Start, creates the worker's branch at that task's last checkpoint that
    is not a set-aside, and merges main into it the way a worker's
    delivery does (storage.md, "Main into a workspace"): cleanly, as a
    `base_merged` checkpoint; or, on a conflict, in the workspace mounted
    for the merge, with markers where they show, committed (a `base_merged`
    checkpoint naming no file) so the branch carries them and the Start's
    mount keeps the pending merge, the workspace unmounted again. The
    Start's prompt says which task's work and commit it starts from and
    what the merge did (the files it changed, or the files with markers to
    resolve first). A fork that fails holds the Start to the next pass:
    the task never starts from main over work it should continue. With no
    such earlier work its branch forks from main at the Start, as before;
    a resident keeps its own branch. (A worker's workspace is an export
    without `.git`: it cannot read another branch, so the work has to be in
    its tree.)
  - The task's scenario is `rsi/scenarios/<id>.sh`.
  - A one-shot objective is solved once a task for it lands. A recurring one
    (`recurring`, `metric`, `target`) stays open until the metric of its
    latest landing meets the target; the metric comes from the gate's
    verdict, never from scenario output.
- **The RSI page** reads the objective missions: `/api/rsi` lists each open
  objective mission with its backlog's objectives (status, landed metric
  values), its tasks with their `task.gated` verdicts, the halt reason and
  the baseline (the shape is documented in `crates/spawn-cli/src/serve.rs`);
  `/api/rsi/task/<id>` answers one task, `/api/rsi/objective/<mission>/<id>`
  one objective's whole prompt; `/api/rsi/pane` shows the
  resident's terminal.
- **One gate at a time, for now.** A judging (fetch, gate, landing) runs
  on a thread of its own. A mission has at most one in flight, and so does
  a source (missions on one source share its mirror and its main); while
  it runs, the integrator's passes leave that source's mirror alone (no
  landing, fetch or judging on it), but every mission still gets its
  halts, budget and objective assignments. At
  most `--gates N` (`SPAWN_INTEGRATE_GATES`) run at once across missions,
  and **N is 1**: more than one gate is unsafe until each gate has a test
  database server of its own (GitHub issue #6, which also covers the
  server state candidate code leaves behind). Today every gate reaches the
  same test PostgreSQL server, and its gate login is a superuser, so one
  gate's candidate code can disturb another gate's base runs (an
  "unsolved" is cached, and a regression then lands) or flake another
  mission's candidate toward its halt. The integrator refuses N > 1 (up to
  8) unless `SPAWN_TEST_DATABASE_PER_GATE=1` states that the server is per
  gate. A mission whose scenarios directory holds the file
  `no-test-database` (its owner's statement that its gate needs no test
  database, as Supply PA's tests run on PGlite) gets no
  SPAWN_TEST_DATABASE_URL, so its candidate code cannot reach the shared
  server: its judgings do not count against N and run beside a Spawn gate,
  still one per mission and per source. Each judging records its own verdict and landing, which the next
  pass reads; finished judgings are reaped before a pass reads the
  missions. A judging that panics before any record of its own is
  recorded `aborted` with the reason, so the three-aborts halt bounds a
  bug. One that returns an error records nothing: the error is shown to
  the operator and a later pass judges again (a git that cannot run a
  merge is the host's problem, not the work's: objective 008).
  A restart mid-gate kills the gate with the process (no record), and the
  next integrator judges the task again. One integrator per database: its
  advisory lock belongs to the connection, kept while any gate runs.
