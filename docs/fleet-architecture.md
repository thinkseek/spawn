# Spawn fleet architecture

## 0. Goal

**Scale agents with as little human intervention as possible.**

The north-star metric is human interventions per agent-hour, at a given
throughput (tasks completed, scenarios passed). Every part below exists to
push that number down, and to do it with the system itself as the improver.

Three ideas carry the design:

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

## 1. Layers

```text
 ┌──────────────────────────────────────────────────────────────────────┐
 │ 5 Interface   web UI (fleet board, attention queue, agent detail)    │
 │               read-only HTTP API · spawn CLI                          │
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
| 5 Interface | `spawn serve` (read-only API and web UI), the `spawn` CLI |

## 2. Observation: rules first, Jev on ambiguity

The Observer folds facts per session (transcript events, executions, screen
readings). Each time a session's facts change, and every 30 s while an
execution runs, it classifies:

| Facts | Decided by | State |
| --- | --- | --- |
| permission dialog on screen or permission notification | rules | BLOCKED (permission) |
| a tool call is open, or the pane read busy less than 2 min ago | rules | WORKING |
| execution ended by Stop | rules | COMPLETED |
| no execution yet | rules | WAITING |
| turn stopped, prompt idle: finished, asking a question, or waiting? | **Jev** | WAITING / BLOCKED / COMPLETED |
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

## 3. Control: dispatcher, supervisor, human

Observation says what state an agent is in. Control decides what to do about
it, on the same ladder.

- **Dispatcher** (inside `spawn loop`) reacts to attention observations
  (BLOCKED, `needs_human`, COMPLETED, `stuck`, execution lost):
  a deterministic **policy** that maps (facts, observation) to an action.
  Jev never chooses the action; its judgement is one input, and only within
  the limits of §2.1:
  1. *Facts and rules* decide fixed cases: a lost execution is resumed
     once.
  2. *A guarded nudge*: when the agent stopped without asking anything
     (Jev: WAITING, `needs_human` low, confidence high) and its task is not
     done, type `continue`, once per episode, rate-limited per agent.
  3. *Everything else escalates*: BLOCKED, `needs_human` high, `stuck` high,
     low confidence, Jev unavailable, or a nudge that did not help. The
     escalation goes to the supervisor when there is one, else to a human.

  The code is `crates/spawn-cli/src/dispatch.rs`, principal `spawn-dispatch`.
  It acts only on a judgement that covers the session's latest fact; a lost
  execution is resumed once; the nudge needs Jev's WAITING at an idle prompt
  with confidence ≥ 0.7, `needs_human` ≤ 0.2 and `stuck` < 0.5, once per
  episode (the latest non-dispatcher command) and three per agent-hour. Only
  a nudge that reached the worker counts: one refused before typing (a pane
  not ready) is sent again under `<key>-retry-<n>` on a judgement made after
  it, twice, then escalated as never typed. A worker idle at its prompt and
  not nudged, a nudge that did not help, a
  failed resume (or one whose engine was lost again within a minute),
  attention, and a finished execution nobody stopped (its recorded end
  cause is not `stopped`) all escalate. A Stop or Discard that failed,
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

  **Stalls: no task waits silently.** A stall is the absence of an event,
  so the dispatcher also measures, for every open task, the step it waits
  on and since which record, from the log alone (`dispatch/stall.rs`, the
  query `stall_facts`), and escalates a step that has not moved within its
  bound, once per stall:

  | Step | The task | Measured from | Bound (default) |
  | --- | --- | --- | --- |
  | `delivery` | assigned or sent back, and its session's turn (a revision first, then the oldest new task; no earlier open task of the session holding it) | `task.assigned`, or the review or gate verdict that sent it back | `SPAWN_STALL_DELIVERY_SECS` (600) |
  | `activity` | delivered, waiting for its worker (a worker idle at its prompt with no report collected included) | the later of `task.delivered` and the session's latest fact (a record of the session that is not the Observer's nor storage's) | `SPAWN_STALL_ACTIVITY_SECS` (900) |
  | `judgement` | reported, under review | `task.reported` | `SPAWN_STALL_REVIEW_SECS` (1800) |
  | `judgement` | in a gated mission, the task the integrator judges next (`Mission::ready`'s order) | the later of the record that made it ready and the mission's latest verdict that is not `stale` | `SPAWN_STALL_GATE_SECS` (3600) |
  | `landing` | landing | its `task.gated` accepted | `SPAWN_STALL_LANDING_SECS` (600) |
  | `stop` | an ephemeral task closed (accepted without a gate, landed, cancelled) whose worker still runs | the record that closed it | `SPAWN_STALL_STOP_SECS` (300) |
  | `halted` | a halted mission with open tasks | its `mission.halted` | `SPAWN_STALL_HALTED_SECS` (0) |

  A task queued behind another of its session, or behind the task its
  mission's gate judges next, is not escalated: the one ahead of it is. The
  tasks of a halted mission that wait for the gate or a landing are the
  halt's, escalated once per halt about session `missions`
  (`stall-mission-<mission>-halted-<record>`), with the reason and the open
  tasks. A failed command (a delivery attempt, a model switch's or a
  retirement's Stop, a resume) is the evidence of the step it blocks, not a
  step of its own. A delivery whose retries are spent was escalated as
  `delivery failed`, which is its stall's escalation; one that may have
  typed the task is not retried, and its stall is escalated at its bound.
  A worker at its prompt whose screen says background shells still run
  (Claude Code's `· done … N shells still running`, read from the line the
  engine draws right above its prompt) is treated as active: its `activity`
  step is not escalated while that is its latest screen reading, since the
  engine resumes the agent when the shells finish.

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
  new record and a new tag. A failed retry, a stale verdict, a nudge or a
  judgement moves no step. Every age is on the database's clock
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
  `spawn submit`. The dispatcher wakes it by typing a digest into its pane
  while it is idle; digests coalesce while it works. Its memory is its native
  session (restored by resume) plus the log.
- **Human**: the attention queue in the UI lists only what the supervisor
  escalated or could not resolve. Each human action is a command with
  principal `human`; it is the event the whole system tries to make rarer.
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
| Decision | supervisor | accepts a mission, splits it into tasks, routes each task to a resident or a fresh ephemeral worker, accepts or rejects reports, closes the mission | type into panes itself |
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
   task and digests it. The supervisor reads reports and judgements, and
   opens a transcript (`tail`) only when it must. Its own context stays
   about the mission.
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
allows only file, https and ssh (`GIT_ALLOW_PROTOCOL`).

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
| Attention queue (top of the board) | What reached a human, since when, the question on the screen, which tiers already tried |
| Fleet board | One card per agent: role, engine, state and for how long, decided by rules / Jev / supervisor, task, current tool |
| Agent detail | Live terminal; the Jev panel (distribution, needs_human, stuck, evidence, rules-vs-Jev); state timeline; event timeline |
| Ladder health | Decisions per tier, escalations, human decisions per agent-hour, Jev failures and disagreements with the rules, engine restarts |
| Missions | Missions with their landing settings; tasks with brief, report and review note; residents waiting for a human's approval |
| RSI | Objective missions: the backlog's objectives, tasks, gate verdicts and logs, metrics, the resident's terminal |

The API is read-only. Humans talk to the supervisor through
`spawn submit --session session_supervisor --action '{"type":"input",...}'`.

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
| Code lives in git | Artifact storage; every commit is referenced from an event |

- The event log is the only state; every process is restartable.
- `EventKind` stays closed; observation fields are additive.
- Facts > rules > Jev; observation never acts; Jev alone triggers only reversible, bounded actions.
- Worker panes hold no database login. The Bridge reads what an engine does from files it writes anyway and from its screen: the Claude transcript (the Bridge names the session with `--session-id`, so it knows the path before the engine starts), the pane read every tick, and the exit file the runner writes. The supervisor's pane gets `spawn_supervisor`, which reads the fleet and submits commands through the gateway but appends no events of its own. The Bridge, the Observer and the controllers run as the owner. `spawn serve` reads as `spawn_reader`, which can write nothing; it binds loopback, and on any other address needs `SPAWN_SERVE_TOKEN`. Roles belong to the whole cluster, so each database has its own pair, named after it (`spawn_supervisor_<db>`, `spawn_reader_<db>`, SQL `spawn_login_role`); grants, commands and records still name the principals `spawn_supervisor` and `spawn_reader` (`spawn_principal`). Two installs on one cluster thus share no privileges and set none of each other's passwords. The cluster-wide `spawn_supervisor`, `spawn_reader` and `spawn_terminal` of earlier installs lose every privilege in the schema and act as nobody; a migration warns while one can still log in, for a human to drop it. A login's password is PBKDF2-HMAC-SHA256 of the owner's password (600,000 rounds), salted with a random salt kept in the login's role comment and its name (`spawn_postgres::role_url`), and set by every migration: nothing secret is stored, only a holder of the owner's password can form it, and a login that knows its own password and salt pays the rounds for every guess at the owner's. The owner's password is percent-decoded first, so every spelling of it agrees. An owner URL without a password derives nothing: `role_url` refuses, and such a migration turns the logins off (no login, no password, with a warning) until a migration that has the owner's password.
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
A human acts from the attention queue with `spawn submit --principal human`.
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
  `mission.halted`, `mission.resumed`, `source.moved`, `task.assigned`,
  `task.delivered`, `task.reported`, `task.reviewed`, `task.cancelled`,
  `task.gated`, `task.landed`, `task.aborted`, `resident.proposed`,
  `resident.approved`, `resident.retired`, `legacy.imported`, `host.disk`.
  `legacy.imported` carries one row of the mission tables as it stood
  before the control stream existed: a database with mission rows and no
  records imports them once, on migration.
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
  a timeout or a signal.

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
    reports (`mission_news`) and `spawn_task_review` refuses them: a review
    could send back the revision the gate is judging;
  - budget: `--minutes` and `--tool-calls`. A delivered task over budget is
    aborted (`task.aborted`) and its worker stopped (the Stop is submitted
    before the record, so a failed record is retried with it; a Stop that
    fails is pursued and escalated by the dispatcher); an abort counts
    toward the abort halt, not as a rejection.

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
  45-minute gate run on revision 0 never lands, or sends back, revision 1.
- **The integrator:** `spawn integrate` (unit `spawn-integrate`). A mission
  whose source it cannot fetch this pass is skipped: the mirror's last copy
  of `main` is no reading of the source's. A mission whose source has
  another mission's task in `landing` waits for it (the push may have moved
  `main` before its `task.landed`). Every pass visits every mission (its
  halts, budget, landing and objective assignment never wait for another
  mission's gate); only a landing makes the rest of its source wait for the
  next pass. A free gate goes to the ready task that has waited longest,
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
  3. runs the gate on C, with a 45-minute wall clock;
  4. records `task.gated` (verdict, log, metric, C as `candidate`) before
     git moves. A gate
     that had no room (exit 5 with its fact, §10.2) is gated `no_room`. This
     is no verdict on the work. The task stays where it was, and the record
     names the reason, `free_kb`, `min_gb` and `tmpdir`. It moves neither
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
     record to the supervisor, with the free space and `TMPDIR` ("no room
     for the gate on this host"). It stays pending until a delivered digest
     carries its tag (`host gate-room-<event>`), however long that takes, so
     it is told once.
  5. moves `main` on the source with a compare-and-swap push
     (`--force-with-lease` on the base); if `main` moved meanwhile the
     verdict is `stale` and the task is judged again, unless the candidate
     is already in `main`'s history (an earlier push of this landing whose
     record was lost), which is a landing;
  6. records `task.landed` (`from`, `to` = C).
- **On rejection:** the task goes back to the worker (`revise`), with the
  gate log as the note.
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
  - three gate rejections in a row;
  - three gate aborts in a row (`no_room` is not an abort);
  - the scenario count below its floor (the most scenarios a gate run of
    the mission has counted since it was configured or last resumed);
  - the source's `main` not where the last landing left it.

  A halted mission lands nothing until a human runs `spawn mission resume`.
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
  - Each integrator pass fills the free slots with the lowest open
    objectives that have no task in flight: an objective never has two
    tasks in flight. `spawn_objective_assign` checks both rules again under
    the append lock, so a second integrator or a stale read cannot
    oversubscribe.
  - Who gets which task: the resident takes a task whenever it has none
    open in the mission; every other task goes to a new per-task worker
    (kind `ephemeral`, agent `agent_worker_objective_<task>`, session
    `session_worker_objective_<task>`). It needs no approval, starts from
    the source's main as it is then, keeps no context, and is stopped by the
    dispatcher once its task lands or is cancelled (`retire-task-<id>`). Its
    report reaches the supervisor's digest like any report. With N = 1 this
    is one task at a time on the resident, as before.
  - Landing stays one at a time per mission: one candidate judged at a
    time, the lowest ready task first, main moved by compare-and-swap.
    Across missions the gate goes to the longest wait (§10.3), so a
    mission that is always ready does not starve another's. A task whose base is
    no longer main lands as the merge by main's rules; two in-flight
    objectives on the same lines make the second a `conflict`.
  - Budgets apply per task, from its delivery; the halt rules stay per
    mission (three rejections in a row from any of its tasks halt it), and
    a halted mission gets no new task and no judging until a human resumes
    it.
  - The brief is the objective, the working rules, and the gate verdicts of
    its recent attempts, read from the log.
  - The task's scenario is `rsi/scenarios/<id>.sh`.
  - A one-shot objective is solved once a task for it lands. A recurring one
    (`recurring`, `metric`, `target`) stays open until the metric of its
    latest landing meets the target; the metric comes from the gate's
    verdict, never from scenario output.
- **The RSI page** reads the objective missions: `/api/rsi` lists each open
  objective mission with its backlog's objectives (status, landed metric
  values), its tasks with their `task.gated` verdicts, the halt reason and
  the baseline (the shape is documented in `crates/spawn-cli/src/serve.rs`);
  `/api/rsi/task/<id>` answers one task; `/api/rsi/pane` shows the
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
  gate. Each judging records its own verdict and landing, which the next
  pass reads; finished judgings are reaped before a pass reads the
  missions. A judging that errs or panics before any record of its own is
  recorded `aborted` with the reason, so the three-aborts halt bounds it.
  A restart mid-gate kills the gate with the process (no record), and the
  next integrator judges the task again. One integrator per database: its
  advisory lock belongs to the connection, kept while any gate runs.
