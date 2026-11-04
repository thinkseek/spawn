# Spawn fleet architecture (draft for review)

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
   unblocked the worker. The event log already is the store; labels are
   derived, not hand-written.
3. **RSI consumes the experience.** The loop's objectives are ratchets over
   that data: move decisions down the ladder without losing accuracy. A
   candidate is judged black-box by replaying recorded episodes.

## 1. Layers

```text
 ┌──────────────────────────────────────────────────────────────────────┐
 │ 5 Interface   web UI (fleet board, attention queue, agent detail)    │
 │               read-only HTTP API · spawn CLI                          │
 ├──────────────────────────────────────────────────────────────────────┤
 │ 4 Control     Supervisor: one perpetual Claude Code session           │
 │                 reads fleet state, submits commands to workers        │
 │               Notifier: wakes the supervisor on attention changes     │
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

| Layer | Exists today | Changes |
| --- | --- | --- |
| 1 Runtime | Bridge, tmux, git, transcripts | none |
| 2 Event log | events, commands, grants, views | observation payload grows (additive); supervisor role |
| 3 Observation | Observer with one judge per deployment | two tiers: rules first, Jev on ambiguity |
| 4 Control | RSI controller | supervisor session, notifier |
| 5 Interface | session timeline, RSI page | fleet board, attention queue, agent detail |

## 2. Observation: rules first, Jev on ambiguity

The Observer folds facts per session (transcript events, executions, screen readings) as
today. Each time a session's facts change, and every 30 s while an execution
runs, it classifies:

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

Every observation records who decided and why:

```json
{ "state": "BLOCKED", "confidence": 0.82,
  "decided_by": "jev", "ambiguity": "turn_stopped_idle",
  "rules_state": "WAITING",
  "probabilities": {"WORKING":0.0,"WAITING":0.18,"BLOCKED":0.82,"COMPLETED":0.0,"UNKNOWN":0.0},
  "needs_human": 0.96, "stuck": 0.22, "latency_ms": 748,
  "evidence": {"screen_tail": "...", "task": "...", "open_tools": 0, "turn": "stopped"} }
```

The new fields are optional in the serde payload, so older readers still
parse the log. `EventKind` gains no variant.

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
   shown in the UI. Thresholds change only through a gated RSI landing,
   never at run time.
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
  1. *Facts and rules* decide fixed cases: resume a lost execution once,
     stop a COMPLETED one-shot task, retry after a transient API error.
  2. *A guarded nudge*: when the agent stopped without asking anything
     (Jev: WAITING, `needs_human` low, confidence high) and its task is not
     done, type `continue`, once per episode, rate-limited per agent.
  3. *Everything else escalates*: BLOCKED, `needs_human` high, `stuck` high,
     low confidence, Jev unavailable, or a nudge that did not help. The
     escalation goes to the supervisor when there is one, else to a human.
  As built (`crates/spawn-cli/src/dispatch.rs`, principal `spawn-dispatch`):
  it acts only on a judgement that covers the session's latest fact; a lost
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
- **The integrator** (`spawn integrate`) is the only deterministic
  controller: it judges accepted tasks with the mission's gate and lands them
  host-side. RSI is a mission like any other, not a separate controller.

### 3.1 The supervisor session never ends

The supervisor is one Spawn session, `session_supervisor`, for the life of
the system. Its continuity is a design requirement, not a best effort:

| Threat | What keeps the session |
| --- | --- |
| `spawn loop` restarts or is upgraded | The engine runs in tmux outside the loop's lifetime (`KillMode=process`); the next loop recovers the execution from the log and keeps observing it |
| The engine process exits or crashes | A **liveness rule** (control layer, facts only, no model): no live execution means `resume` with the recorded native session id, with backoff |
| The machine reboots | tmux is gone, so the execution is `Lost`; the same rule resumes it from the native transcript |
| The native session cannot be resumed (transcript lost, resume fails three times) | `start` a new native session inside the same Spawn session, with a handoff prompt rebuilt from the log: mission, open worker tasks, recent escalations and decisions |
| The context window fills | Claude Code compacts on its own; the handoff path above is the fallback when it cannot |
| The supervisor hangs or loops | Its own observations (rules, then Jev) flag `stuck`; that goes to a human, never to an automatic stop |

Rules for the session itself:

- Nothing automatic ever stops it. Only a human `stop` command does, and the
  liveness rule treats any other end as an outage to repair.
- Its durable memory is the log, not its context window. Anything it must
  not forget (mission, decisions, open escalations) is either a command it
  submitted or an event about it, so a handoff can always be rebuilt.
- Its uptime, restarts, resumes and handoffs are first-class facts in the UI.

### 3.2 Missions, tasks and workers (agreed and built 2026-11-04)

Spawn works for missions, not repositories. A repository is just one possible
input of a mission.

**Distribution is layered, and every routing decision is the supervisor's.**

| Layer | Who | Does | Never does |
| --- | --- | --- | --- |
| Intake | human (or a standing source) | puts a mission in the inbox: `spawn mission add`, typing to the supervisor, answering a worker; later a UI box | bypass the record: every human input is a stored command with principal `human` |
| Decision | supervisor | accepts a mission, splits it into tasks, routes each task to a resident or a new ephemeral worker, accepts or rejects reports, closes the mission | type into panes itself |
| Delivery | dispatcher (rules) | delivers assigned tasks, one at a time per worker; resumes, nudges, collects reports, wakes the supervisor with digests | choose who does what |
| Execution | workers | do one task, end with a report | take work from anywhere but their assignment |

**Two kinds of worker.**

| | Resident | Ephemeral |
| --- | --- | --- |
| Exists for | a lasting concern tied to the architecture (a domain or role with a charter) | one task |
| Started | by the supervisor, on demand, then kept | by the supervisor, on demand, per task |
| Context | accumulates knowledge of its domain; one task at a time; handoff from the log when it grows stale | a fresh, self-contained brief and nothing else |
| Liveness | resumed after outages like the supervisor; a stop by the supervisor or a human is respected (killable) | resumed once at most; ends when its report is accepted |
| Workspace | its own, long-lived | its own, per task |

**Context cohesion: the rules that decide routing and protect every context.**

1. One session, one concern. A task goes to a resident only when it falls
   inside that resident's charter; anything else gets an ephemeral worker.
2. Serial inside a session. A resident starts its next task only after it has
   reported the last one; its queue waits in storage, not in its context.
3. Briefs, not histories. A worker receives a self-contained brief (goal,
   inputs, done criteria, report format), never the supervisor's conversation.
4. Reports, not transcripts. Workers end with a short structured report; the
   supervisor reads reports and judgements, and opens a transcript (`tail`)
   only when it must. Its own context stays about the mission.
5. Isolated workspaces. Every worker has its own; combining results is a task
   the supervisor decides on, not a side effect.
6. Human input is context too. A human message to a worker is recorded, and
   the supervisor gets a one-line summary of it.

**Storage.** Missions, tasks and the resident roster are controller tables
(like `rsi_*`): a mission has its text, origin, sources and state; a task has
its mission, brief, kind, assignee, state and report. The supervisor writes
tasks through a checked function, as it submits commands through the gateway.

**Mission sources.** A mission names its inputs (any git repository, files,
or nothing); each worker's workspace is materialized from them.
`SPAWN_GIT_REPO` becomes only the default source.

## 4. Experience and the RSI loop

Nothing new is stored outside the log. Three derived views make it usable:

| View | Rows | Labels from |
| --- | --- | --- |
| `decisions` | every observation and every dispatcher/supervisor command, with tier, evidence, confidence | the decision itself |
| `outcomes` | what happened next for the same session within a window | facts: kept working, needed another action, a human stepped in, task passed |
| `interventions` | commands by principal `human`, with the situation before them | principal on the command |

`spawn corpus export` writes episodes (evidence + decision + outcome) as
JSONL. As built, an episode is one model judgement; its outcome is measured
from the evidence it covered (not from when it was written) and labelled
`continued` (the agent made a tool call before anyone acted), `needed_decision`
(a human or the supervisor typed next), `needed_input`, `nudged`, `stopped`,
`ended`, `restarted` (an outage; says nothing about the judged state) or
`open`. `spawn judge --replay` runs the current rules over a corpus and
reports coverage and agreement with the model and the outcomes. The RSI controller snapshots a corpus per objective round into its own
directory, so a candidate cannot edit the data that judges it.

RSI objectives are ratchets over the corpus, judged by black-box replay
through the candidate's `spawn` binary (`spawn judge --replay`,
`spawn dispatch --replay`):

| Objective | Ratchet |
| --- | --- |
| Rules coverage | share of episodes decided by rules up, with agreement to the labelled outcome kept |
| Jev filtering | share of escalations to the supervisor down, with outcome accuracy kept |
| Playbook reach | interventions in the corpus the playbook would have resolved, up |
| Human interventions | per agent-hour, measured on live runs between rounds |

Each landing moves decisions one rung down the ladder; the next round's
corpus contains the new cases the fleet ran into, so the loop keeps finding
work as long as humans still have to step in.

## 5. Interface

| View | Answers |
| --- | --- |
| Attention queue (top of the board) | What reached a human, since when, the question on the screen, which tiers already tried |
| Fleet board | One card per agent: role, engine, state and for how long, decided by rules / Jev / supervisor, task, current tool |
| Agent detail | Live terminal; the Jev panel (distribution, needs_human, stuck, evidence, rules-vs-Jev); state timeline; event timeline |
| Ladder health | Decisions per tier per hour, escalation rates, human interventions per agent-hour, Jev latency, failures |
| RSI | Objectives over the corpus, iterations, gate logs, metric ratchets |

The API stays read-only. Humans talk to the supervisor through
`spawn submit --session session_supervisor --action '{"type":"input",...}'`.
A UI input box would be the first write endpoint and needs its own decision.

## 6. Invariants kept

**Storage and compute are separate.** PostgreSQL is the only source of
truth (event log, command queue, grants, controller tables). Every other
process is compute: it holds nothing that cannot be rebuilt from storage, and
everything people read (SQL views, API, UI, metrics, RSI corpora) is derived
from storage. A model judgement becomes storage the moment it is written,
together with the evidence it saw.

Known edges, each with its rule:

| Edge | Rule |
| --- | --- |
| The UI's live terminal reads tmux | A compute peek, labelled as such; the stored copy is the screen tail in each model observation |
| The supervisor's native transcript lives on the host (`~/.claude`) | A cache; the handoff rebuilt from the log is the authoritative recovery path (§3.1) |
| Code lives in git | Artifact storage; every commit is referenced from an event |

- The event log is the only state; every process is restartable.
- `EventKind` stays closed; observation fields are additive.
- Facts > rules > Jev; observation never acts; Jev alone triggers only reversible, bounded actions.
- Worker panes hold no database login. The Bridge reads what an engine does from files it writes anyway and from its screen: the Claude transcript (the Bridge names the session with `--session-id`, so it knows the path before the engine starts), the runner's copy of Codex output, the pane read every tick, and the exit file the runner writes. The supervisor's pane gets `spawn_supervisor`, which reads the fleet and submits commands through the gateway but appends no events of its own. `spawn_terminal` is retired. The Bridge, the Observer and the controllers run as the owner. `spawn serve` reads as `spawn_reader`, which can write nothing; it binds loopback, and on any other address needs `SPAWN_SERVE_TOKEN`. The restricted logins' passwords are derived from the owner's (SHA-256 over the owner's password and the role name, `spawn_postgres::role_url`) and set by every migration, so nothing stores them and only a holder of the owner's credentials can form them. The owner's password is percent-decoded first, so every spelling of it agrees. An owner URL without a password derives nothing: `role_url` refuses, and such a migration leaves the logins as they are (created `nologin`; enabled only by a migration that has the owner's password), never with a password derived from the empty string.

**The same-uid limit.** Without container mode (`SPAWN_EXEC_IMAGE`) every engine runs as the owner's Unix user. The database boundary holds (no pane can write the log), but the file boundary does not: an agent can read another process's environment through `/proc` or tmux (including the supervisor's login), and write another session's transcript or exit file, which the Bridge then reads as that session's facts. The Bridge reads those files without following symlinks, refuses anything but a regular file and caps each read, so a planted FIFO or an endless file cannot stall or exhaust it. Closing the rest takes a separate uid or container per agent.
- Known limit: all panes share one uid and one tmux server. A worker cannot write the log, but it can write the files the Bridge reads for another session (a transcript, an exit file), read the supervisor pane's login from its environment, or simply `tmux send-keys` into another pane. Holding no login stops a worker from forging facts through the database; it does not isolate agents from each other. Isolation between agents needs container mode (`SPAWN_EXEC_IMAGE`) or one uid per agent. The same holds for the RSI gate, which runs candidate code on the host (see docs/rsi.md §5).
- RSI keeps its black-box gate, host-side commit, baseline compare-and-swap and halt rules.

## 7. MVP: the minimal closed loop

One loop, end to end, before anything else:

```text
 worker acts ──► observe (rules → Jev) ──► dispatch (rules → Jev playbook → human)
      ▲                                              │
      └──────────── command ◄────────────────────────┘
                                                     │ every decision + its outcome
                                                     ▼
                    RSI: replay corpus ──► candidate rules decide more ──► land
                                                     │
                    fewer Jev calls, fewer escalations ◄┘
```

| In the MVP | Out (later) |
| --- | --- |
| Supervisor: one perpetual session kept alive by the liveness rule (resume, then handoff); receives escalation digests; can submit to workers | Mission planning beyond a mission file |
| Workers: Claude Code sessions started by the supervisor or a task file | Codex workers |
| Observation: rules first, Jev on ambiguity, full payload | Periodic re-judging beyond the 30 s live check |
| Dispatcher: deterministic policy; rules for fixed cases, one guarded Jev-gated nudge (`continue`), everything else escalates to the supervisor, which may escalate to a human | `restart`, `reassign` |
| Human: attention queue in the UI; acts with `spawn submit --principal human` | UI write box |
| Experience: SQL views of decisions, outcome labels, interventions; `spawn corpus export` | Rich outcome windows |
| RSI: one objective, rules coverage, judged by `spawn judge --replay` | Jev filtering and playbook objectives |
| UI: attention queue, fleet board, agent detail with Jev panel, ladder metrics | Health over long periods |

**Done means:**

1. `scripts/mvp-e2e.sh` runs the loop with a scripted fake engine and a stub
   Jev and asserts every hop. A rule decides WORKING. Jev decides an
   ambiguous idle prompt. The guarded nudge restarts an agent that stopped
   mid-task, once. A question to a human reaches the attention queue even
   when Jev is confident, and a human command resolves it. With the stub Jev
   down, the same run still completes on rules plus escalations. Killing the
   supervisor's engine, then its whole tmux server, then the loop, each ends
   with the same `session_supervisor` live again and its native session
   resumed. Outcomes are labelled, the corpus exports, and
   replay reports which tier would decide each episode. It runs in the gate
   on every change.
2. A live run with real Claude workers and the hosted Jev shows nonzero
   decisions at every tier and a human-interventions-per-agent-hour figure in
   the UI.
3. One RSI landing on the rules-coverage objective. It moves a class of
   episodes from Jev to rules with agreement kept, and the live share of Jev
   calls drops.

### 7.1 Status (2026-11-04)

| Done-criterion | State |
| --- | --- |
| 1. `scripts/mvp-e2e.sh` asserts every hop | **Passes** (~20 s). Not yet run by the RSI gate: a scenario can never be removed once added, so it joins when it has proven stable. |
| 2. Live run with real Claude workers and hosted Jev, nonzero decisions at every tier, interventions per agent-hour in the UI | Partly: a live supervisor → worker → Jev → digest → review → stop loop ran; rules, Jev, dispatcher, supervisor and liveness all decided; the UI shows the ladder and `human_per_agent_hour`. Needs sustained worker traffic. |
| 3. One RSI landing on the rules-coverage objective | Not started: the corpus has ~30 episodes; the objective needs a few hundred labelled ones to judge agreement without overfitting. |

Fleet work so far: missions 1 and 2 (four refactor branches and the Missions view, all by ephemeral workers, reviewed by the supervisor) passed `scripts/merge-gate.sh` one by one and merged together, and landed at fcff135. The corpus had 47 labelled episodes then.

Open items:
- A resident keeps one branch across tasks, so merges that land in its mission source after it started never reach it. New ephemeral workers are fine: the mirror is refreshed before each new branch. Two fixes are possible: rebase the resident's branch onto `main` when a task is delivered (conflicts would then need a policy), or give every resident task its own branch.
- Merging was a human step: `merge-gate.sh` (since folded into `scripts/rsi/gate.sh`, §10.2) ran per branch, then once on the merged tree. Automating it would mean a host-side integrator holding the same CAS discipline as the RSI landing.

## 8. Build order

1. Observation tiers: rules first, Jev on ambiguity, extended observation payload with tier, evidence and escalation.
2. Read API and fleet UI: attention queue, fleet board, agent detail with the Jev panel, ladder health.
3. Dispatcher with rules + Jev playbook; roles and grants per principal.
4. Supervisor session with `spawn fleet` / `spawn tail`; digests from the dispatcher.
5. Experience views, `spawn corpus export`, replay commands.
6. RSI objectives over the corpus (rules coverage first). The ingest-speed demo objective is retired: it measured the hook parser, which is gone.

## 9. Decisions on §3.2 (2026-11-04)

1. Residents: the supervisor proposes a resident and its charter; it starts
   only after a human approves. The approval is a stored command with
   principal `human`.
2. Reports: a worker ends by writing `.spawn/report.md` (outcome, what
   changed, open issues). The Bridge stores it as part of the task; the
   supervisor reads the report, not the transcript.
3. Human to worker: allowed directly. The message is delivered and recorded,
   and the supervisor gets a one-line summary in its next digest.
4. Limits: none fixed. The supervisor decides fleet size; humans stop what
   overreaches, and the UI shows live workers and spend.

### 9.1 Build order for §3.2 (all six steps built; `scripts/mvp-e2e.sh` covers them)

1. Mission inbox: `mission_*` tables, `spawn mission add` (principal human),
   a "new mission" digest to the supervisor, missions in `spawn fleet` and
   the UI.
2. Tasks: `spawn task assign --mission --kind resident|ephemeral --worker
   --brief` for the supervisor (checked like the gateway), creating the task
   record and its delivery atomically; the dispatcher delivers, keeps one
   task in flight per worker, collects `.spawn/report.md` and digests reports.
   The dispatcher's own judgement-free rules (resume, nudge, escalate) stay.
3. Worker lifecycles: generalize the supervisor's liveness rule to residents
   (resumed after outages, a stop respected); ephemeral workers end when the
   supervisor accepts their report.
4. Resident proposals: `spawn resident propose --name --charter` by the
   supervisor, `spawn resident approve` by a human; charters stored and given
   to the resident at every start and handoff.
5. Mission sources: per-worker workspaces from the mission's sources (any
   git repository, files, or none) instead of one `SPAWN_GIT_REPO`.
6. e2e: extend `scripts/mvp-e2e.sh` with a mission that the supervisor splits
   into a resident task and an ephemeral task, both reporting back.

## 10. One log, one landing path (decided 2026-11-04)

Occam's razor, applied without touching the invariants. Two decisions:

1. RSI is a mission whose tasks the gate accepts. The separate controller
   (`evolve` iterations, `rsi_*` tables, `spawn-rsi`, `repo.git`) retires.
2. Missions, tasks, residents and landings are events in `spawn_events`.
   The `mission_*` tables become a projection of them.

### 10.1 The control stream

- **Where it lives:** one stream with session `session_control`, agent
  `agent_control` and source `control`. Every record is the event
  `control.recorded`, whose detail is `{type, ...}`.
- **Record types:** `mission.added`, `mission.closed`, `mission.halted`,
  `mission.resumed`, `task.assigned`, `task.delivered`, `task.reported`,
  `task.reviewed`, `task.cancelled`, `task.gated`, `task.landed`,
  `resident.proposed`, `resident.approved`, `resident.retired`,
  `legacy.imported`.
- **Who writes it:** only the checked mission functions do. Engine logins
  cannot, because `spawn_append` refuses source `control`.
- **The tables are a projection.** `spawn_control_apply(record)` is the only
  writer of `mission_*`, and it runs in the same transaction as the append.
  `spawn_control_refold()` rebuilds the tables from the log, and a test
  checks that the rebuilt tables equal the live ones.
- **It is not an agent.** It is left out of the session projection, and the
  Bridge and the Observer skip it.

### 10.2 One gate

- **The script:** `scripts/gate.sh <repo> <base> <candidate> <scenarios>
  [<scenario>]` is the RSI judge with its rules unchanged:
  - it refuses changes to `rsi/`, `scripts/rsi/`, `build.rs` and `.cargo/`;
  - when a task has a scenario, that scenario must fail on the base and pass
    on the candidate;
  - every non-ratchet scenario must pass on the candidate.
- **merge-gate's checks become scenarios.** Clippy, the tests, the e2e and
  the web build each become one. The rule that the scenario count never
  falls then protects them too.

### 10.3 Landing

- **Mission settings:** a mission may name a gate (a scenarios directory in
  the controller's checkout) and how its tasks are accepted:
  - `review`: the supervisor accepts, then the gate runs;
  - `gate`: the gate alone decides.
- **The integrator:** `spawn integrate` (unit `spawn-integrate`) replaces
  `spawn-rsi`. For each task that is ready, it:
  1. commits the workspace;
  2. builds the candidate: the branch itself, or a merge of it with `main`
     when `main` has moved. A conflict sends the task back.
  3. runs the gate;
  4. records `task.gated` (verdict, log, metric) before git moves;
  5. moves `main` on the source with a compare-and-swap push;
  6. records `task.landed`.
- **On rejection:** the task goes back to the worker (`revise`), with the
  gate log as the note.
- **Halt rules,** per mission and read from the log:
  - three gate rejections in a row;
  - three gate aborts in a row;
  - the scenario count below its floor;
  - the source's `main` not where the last landing left it.

  A halted mission lands nothing until a human runs `spawn mission resume`.

### 10.4 RSI as a mission

- **The RSI mission:**
  - source: `missions/spawn.git`;
  - gate: `rsi/scenarios`;
  - acceptance: `gate`;
  - worker: the resident `rsi`, approved by a human.
- **Tasks come from the backlog** (`rsi/backlog`, in the controller's
  checkout). The integrator gives the lowest open objective one task at a
  time.
  - The prompt is the objective plus its attempt history, read from the log.
  - The task's scenario is `rsi/scenarios/<id>.sh`.
- **Budgets:** the 45-minute and 60-tool-call limits become mission fields.
  A task over budget is aborted, which counts as an abort, not a rejection.
- **Migration:** the `rsi_attempts` rows are imported as `legacy.imported`,
  so objective status and history carry over. After that, the `rsi_*`
  tables, the iteration machine, `spawn-rsi` and `repo.git` retire.

### 10.5 Order

1. The control stream and its projection.
2. One gate, with the check scenarios.
3. The integrator, and landing for missions with a gate.
4. The RSI mission, the import, and retiring the old controller.

Each step is tested red, then green, gated, and committed.

### 10.6 Status (2026-11-04): built and live

- **The control stream** is live. The live database's 12 rows were imported
  once (`legacy.imported`). A refold inside a rolled-back transaction
  reproduces the live tables exactly.
- **One gate:** `scripts/rsi/gate.sh` replaced `judge-candidate.sh` and
  `merge-gate.sh`.
  - `rsi/scenarios` holds the build hook, the protected list, and five
    check scenarios.
  - `scripts/rsi/test-gate.sh` tests the gate itself.
  - The gate run on the step-4 tree accepted it.
- **`spawn integrate`** runs as `spawn-integrate`. The e2e covers:
  - a fast-forward landing and a merge landing;
  - a rejection that goes back with the gate's output;
  - a halt from an outside push, which only a human can resume;
  - an objective mission that the gate alone lands.
- **RSI** is mission 4.
  - Source: `missions/spawn.git`; gate: `rsi/scenarios`; acceptance:
    `gate`; budget: 45 minutes and 60 tool calls; resident: `rsi`.
  - Both old objectives were imported as landed, with objective 2 at
    1021 ns against its 1,100 ns target, so the integrator is idle until the
    backlog gets a new objective.
  - Objective 2 is since retired with its scenario and bench: it measured
    the hook parser, which the transcript reader replaced.
  - `spawn-rsi` and `repo.git` are retired; their units and the repository
    were moved to `~/.local/share/spawn-rsi/backups/`.
  - The supervisor's default repository is now the scratch repository.
- **A shared source has one baseline.** A landing moves the baseline of
  every mission on that source, so RSI and fleet work on Spawn don't halt
  each other.
- **The RSI page reads the objective mission** (mission 5, the first fleet
  mission that lands through the gate):
  - `/api/rsi` lists the open objective missions: the backlog's objectives
    with their status and landed metric values, the tasks with their
    `task.gated` verdicts, the halt reason and the baseline. The shape is
    documented in `crates/spawn-cli/src/serve.rs`.
  - `/api/rsi/task/<id>` replaces `/api/rsi/iteration/<n>`; `/api/rsi/pane`
    shows the mission resident's terminal.
  - Nothing reads `rsi_*` any more, and the tables are dropped.
  - The integrator's advisory lock moved into the missions code, with the
    old controller's key, so the two could never run at once.
  - `spawn mission import-rsi` is gone: its one import ran.
