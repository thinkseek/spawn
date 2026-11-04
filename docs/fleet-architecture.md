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
 │               engines (Claude Code, Codex) in tmux + git workspaces   │
 │               transcripts, JSON lines, screen, exit files → events    │
 └──────────────────────────────────────────────────────────────────────┘
```

Commands flow down (4 → queue → 1). Facts flow up (1 → log → 3 → 4/5).
No layer calls another layer's process directly; they meet in the log.

| Layer | Parts |
| --- | --- |
| 1 Runtime | Bridge, tmux, git workspaces and mission mirrors, the chunk store, transcripts and exit files |
| 2 Event log | `spawn_events` (with the control stream), `spawn_commands`, grants, projections, views |
| 3 Observation | Observer: rules first, then the model judge on ambiguity (Jev, or the local heuristic judge when no Jev URL is set) |
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
| execution live, no open tool, no fact for 2 min (or a tool open and silent for 10 min): thinking or hung? | **Jev** | WORKING / WAITING / BLOCKED |
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

Without a Jev URL (`SPAWN_JUDGE_URL` or `--judge-url`), the heuristic judge
(`HeuristicJudge`, `judge: "heuristic"`) answers the ambiguous cases with
fixed rules over the same input. It keeps the system running without a model
and is what the tests use.

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
  episode (the latest non-dispatcher command) and three per agent-hour; a
  worker idle at its prompt and not nudged, a nudge that did not help, a
  failed resume, attention, and a finished execution nobody stopped all
  escalate. Escalations coalesce into one digest typed into the supervisor
  while it is idle. The dispatcher keeps no state: idempotency keys mark its
  nudges and resumes, and a `[spawn:esc <session> <id>]` marker per item in
  each delivered digest marks what the supervisor was told.
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
| The engine process exits or crashes | A **liveness rule** (control layer, facts only, no model): no live execution means `resume` with the recorded native session id, with backoff |
| The machine reboots | tmux is gone, so the execution is `Lost`; the same rule resumes it from the native transcript |
| The native session cannot be resumed (transcript lost, resume fails three times) | `start` a fresh native session inside the same Spawn session, with a handoff prompt rebuilt from the log: mission, open worker tasks, recent escalations and decisions |
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
| Delivery | dispatcher (rules) | delivers assigned tasks, one at a time per worker; resumes, nudges, collects reports, wakes the supervisor with digests | choose who does what |
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
--brief <text>` (the supervisor; checked like the gateway), `spawn task
accept` /
`reject`, `spawn resident propose` / `approve` / `retire`, `spawn mission
close`. Every one is a checked SQL function that appends to the control
stream (§10.1).

**Storage.** Missions, tasks and the resident roster are the `mission_*`
tables, folded from the control stream (§10.1): a mission has its text,
origin, sources, state and landing settings; a task has its mission, brief,
kind, assignee, state and report.

**Mission sources.** A mission names its source: any git repository (a path
or a URL), mirrored into `$SPAWN_WORKSPACE_ROOT/.sources/`, or none (an empty
scratch repository). Each worker's workspace is a branch of that mirror.
`SPAWN_GIT_REPO` is the repository of agents no mission owns.

**Limits.** None fixed. The supervisor decides fleet size; humans stop what
overreaches, and the UI shows the live workers.

## 4. Experience

Nothing is stored outside the log. `spawn corpus export` derives episodes
from it and writes them as JSONL: an episode is one model judgement with its
evidence and an outcome label. The outcome is measured from the evidence the
judgement covered (not from when it was written) and labelled `continued`
(the agent made a tool call before anyone acted), `needed_decision` (a human
or the supervisor typed next), `needed_input`, `nudged`, `stopped`, `ended`,
`restarted` (an outage; says nothing about the judged state) or `open`.

`spawn judge --replay <corpus>` runs the current rules over a corpus and
reports which tier would decide each episode, the coverage, and the rules'
agreement with the model and with the outcomes. That is the measure of
moving decisions down the ladder: episodes move from Jev to the rules
without losing agreement.

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
| The supervisor's native transcript lives on the host (`~/.claude`) | A cache; the handoff rebuilt from the log is the authoritative recovery path (§3.1) |
| Code lives in git | Artifact storage; every commit is referenced from an event |

- The event log is the only state; every process is restartable.
- `EventKind` stays closed; observation fields are additive.
- Facts > rules > Jev; observation never acts; Jev alone triggers only reversible, bounded actions.
- Worker panes hold no database login. The Bridge reads what an engine does from files it writes anyway and from its screen: the Claude transcript (the Bridge names the session with `--session-id`, so it knows the path before the engine starts), the runner's copy of Codex output, the pane read every tick, and the exit file the runner writes. The supervisor's pane gets `spawn_supervisor`, which reads the fleet and submits commands through the gateway but appends no events of its own. The Bridge, the Observer and the controllers run as the owner. `spawn serve` reads as `spawn_reader`, which can write nothing; it binds loopback, and on any other address needs `SPAWN_SERVE_TOKEN`. The restricted logins' passwords are derived from the owner's (SHA-256 over the owner's password and the role name, `spawn_postgres::role_url`) and set by every migration, so nothing stores them and only a holder of the owner's credentials can form them. The owner's password is percent-decoded first, so every spelling of it agrees. An owner URL without a password derives nothing: `role_url` refuses, and such a migration leaves the logins as they are (created `nologin`; enabled only by a migration that has the owner's password), never with a password derived from the empty string.
- Every landing goes through the black-box gate, a host-side commit, a compare-and-swap move of the baseline, and the halt rules (§10).

## 7. The closed loop

```text
 worker acts ──► observe (rules → Jev) ──► dispatch (rules → Jev nudge → supervisor → human)
      ▲                                              │
      └──────────── command ◄────────────────────────┘
                                                     │ every decision + its outcome
                                                     ▼
                    corpus export ──► replay: which tier decides each episode
```

The supervisor is kept alive by the liveness rule (resume, then handoff),
receives escalation digests and submits to workers. Workers are Claude Code
sessions started for the supervisor's tasks. Observation runs rules first and
Jev on ambiguity, with the full payload. The dispatcher's policy decides
fixed cases by rules, allows one guarded Jev-gated nudge (`continue`), and
escalates everything else to the supervisor, which may escalate to a human.
A human acts from the attention queue with `spawn submit --principal human`.
Every decision is exported with its outcome, and replay measures how many
the rules could decide.

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
10. The corpus exports, and replay reports a tier per episode.

## 8. Known limit: one uid

Without container mode (`SPAWN_EXEC_IMAGE`) every engine runs as the owner's
Unix user, and all panes share one tmux server. The database boundary holds
(no pane can write the log), but the file boundary does not: an agent can
read another process's environment through `/proc` or tmux (including the
supervisor pane's login), write the files the Bridge reads for another
session (a transcript, an exit file), which the Bridge then reads as that
session's facts, or simply `tmux send-keys` into another pane. The Bridge
reads those files without following symlinks, refuses anything but a regular
file and caps each read, so a planted FIFO or an endless file cannot stall
or exhaust it. Holding no login stops a worker from forging facts through
the database; it does not isolate agents from each other. Isolation between
agents needs container mode or one uid per agent. The gate runs candidate
code in a bubblewrap sandbox under the same uid (see docs/rsi.md §4).

## 9. Not built

- **Experience views in SQL** (`decisions`, `outcomes`, `interventions`).
  Outcomes are derived per export by `spawn corpus export`.
- **RSI objectives over the corpus**, judged by black-box replay through the
  candidate's `spawn` binary on a corpus snapshot the candidate cannot edit:
  rules coverage (share of episodes decided by rules up, agreement with the
  labelled outcome kept), Jev filtering (escalations to the supervisor down,
  outcome accuracy kept), playbook reach (interventions in the corpus the
  playbook would have resolved, up), and human interventions per agent-hour
  on live runs. A `spawn dispatch --replay` for the playbook does not exist.
- **A write endpoint in the UI** (an input box to the supervisor). Humans
  act with `spawn submit --principal human`.
- **Codex workers for mission tasks.** Delivery starts every worker with
  Claude Code; Codex runs only through `spawn submit`.
- **More dispatcher rules:** stopping a COMPLETED one-shot task and
  retrying after a transient API error (both escalate), and the actions
  `restart` and `reassign`.
- **Ladder health over long periods.** The UI's figures cover the last 24 h.
- **The integrator judging an agent's work from its pushed branch**, and the
  backlog, scenarios and gate versioned with the source, so work done on
  another host lands too (docs/storage.md).

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
  `resident.approved`, `resident.retired`, `legacy.imported`.
  `legacy.imported` carries one row of the mission tables as it stood
  before the control stream existed: a database with mission rows and no
  records imports them once, on migration.
- **Who writes it:** only the checked mission functions, through
  `spawn_control_record`. The integrator's records (`task.gated`,
  `task.landed`, `task.aborted`, `mission.halted`) and `source.moved` need
  the owner's login. The restricted logins write no events at all:
  `spawn_append` refuses them.
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
  §4). The verdict is one JSON line; the integrator believes only the
  verdict file it owns.

`scripts/rsi/test-gate.sh` tests the gate itself.

### 10.3 Landing

- **Mission settings** (`spawn mission configure`, owner only): a gate (a
  scenarios directory in the controller's checkout), how tasks are accepted,
  and an optional budget:
  - `review`: the supervisor accepts, then the gate runs;
  - `gate`: the gate alone decides;
  - budget: `--minutes` and `--tool-calls`. A delivered task over budget is
    aborted (`task.aborted`) and its worker stopped; an abort counts toward
    the abort halt, not as a rejection.
- **The integrator:** `spawn integrate` (unit `spawn-integrate`). For each
  task that is ready, it:
  1. commits the workspace;
  2. builds the candidate: the branch itself, or a merge of it with `main`
     when `main` has moved. A conflict sends the task back. A branch with a
     pending merge of `main` or with conflict markers is sent back as
     `conflict` before it is judged.
  3. runs the gate, with a 45-minute wall clock;
  4. records `task.gated` (verdict, log, metric) before git moves;
  5. moves `main` on the source with a compare-and-swap push
     (`--force-with-lease` on the base); if `main` moved meanwhile the
     verdict is `stale` and the task is judged again;
  6. records `task.landed`.
- **On rejection:** the task goes back to the worker (`revise`), with the
  gate log as the note.
- **Halt rules,** per mission and read from the log:
  - three gate rejections in a row;
  - three gate aborts in a row;
  - the scenario count below its floor (the most scenarios a gate run of
    the mission has counted since it was configured or last resumed);
  - the source's `main` not where the last landing left it.

  A halted mission lands nothing until a human runs `spawn mission resume`.
- **A shared source has one baseline.** A landing moves the baseline of
  every mission on that source, so RSI and fleet work on the same repository
  don't halt each other. A deploy that moves `main` forward records
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
- **Tasks come from the backlog** (`rsi/backlog/*.json`, in the controller's
  checkout). The integrator gives the lowest open objective to the resident,
  one task at a time.
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
- **One gate at a time.** Integrator passes hold an advisory lock, so two
  gates never run at once.
