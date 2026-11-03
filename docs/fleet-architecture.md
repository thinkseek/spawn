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
 │               Deterministic controllers (RSI evolve) as workloads     │
 ├──────────────────────────────────────────────────────────────────────┤
 │ 3 Observation Rules: decide every state the facts settle              │
 │               Jev: decides only what the rules leave ambiguous        │
 │               → observation events (state, who decided, why)          │
 ├──────────────────────────────────────────────────────────────────────┤
 │ 2 Event log   PostgreSQL: spawn_events (truth), spawn_commands        │
 │               (queue), grants, views; rsi_* controller tables         │
 ├──────────────────────────────────────────────────────────────────────┤
 │ 1 Runtime     Bridge: start / input / resume / stop                   │
 │               engines (Claude Code, Codex) in tmux + git workspaces   │
 │               hooks, JSON lines, pane tail → typed events             │
 └──────────────────────────────────────────────────────────────────────┘
```

Commands flow down (4 → queue → 1). Facts flow up (1 → log → 3 → 4/5).
No layer calls another layer's process directly; they meet in the log.

| Layer | Exists today | Changes |
| --- | --- | --- |
| 1 Runtime | Bridge, tmux, git, hooks | none |
| 2 Event log | events, commands, grants, views | observation payload grows (additive); supervisor role |
| 3 Observation | Observer with one judge per deployment | two tiers: rules first, Jev on ambiguity |
| 4 Control | RSI controller | supervisor session, notifier |
| 5 Interface | session timeline, RSI page | fleet board, attention queue, agent detail |

## 2. Observation: rules first, Jev on ambiguity

The Observer folds facts per session (hooks, executions, screen readings) as
today. Each time a session's facts change, and every 30 s while an execution
runs, it classifies:

| Facts | Decided by | State |
| --- | --- | --- |
| permission dialog on screen or permission notification | rules | BLOCKED (permission) |
| a tool call is open, or the pane is busy | rules | WORKING |
| execution ended by Stop | rules | COMPLETED |
| no execution yet | rules | WAITING |
| turn stopped, prompt idle: finished, asking a question, or waiting? | **Jev** | WAITING / BLOCKED / COMPLETED |
| execution live, no open tool, pane quiet for long: thinking or hung? | **Jev** | WORKING / BLOCKED |
| repeated tool failures or engine errors: progressing or stuck? | **Jev** | WORKING / BLOCKED |
| process lost, screen layout unknown | **Jev** | any |

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
`reason = JudgeUnavailable`, and the session is retried. It is never marked dead.

Jev's distribution doubles as the attention signal. An observation asks for
attention when the probability on states that need someone (BLOCKED,
UNKNOWN), among the states the facts allow, reaches 0.5, or when
`needs_human` or `stuck` does. Uncertainty between harmless states (WAITING
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
  Neither can write observations, runtime events or `rsi_*`.
- **Deterministic controllers** (the RSI `evolve`) own their sessions and
  appear in the fleet as members.

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

## 4. Experience and the RSI loop

Nothing new is stored outside the log. Three derived views make it usable:

| View | Rows | Labels from |
| --- | --- | --- |
| `decisions` | every observation and every dispatcher/supervisor command, with tier, evidence, confidence | the decision itself |
| `outcomes` | what happened next for the same session within a window | facts: kept working, needed another action, a human stepped in, task passed |
| `interventions` | commands by principal `human`, with the situation before them | principal on the command |

`spawn corpus export` writes episodes (evidence + decision + outcome) as
JSONL. The RSI controller snapshots a corpus per objective round into its own
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
| `rsi_*` tables are mutable rows, not events | Storage, owned by one controller; forced by the closed `EventKind` |

- The event log is the only state; every process is restartable.
- `EventKind` stays closed; observation fields are additive.
- Facts > rules > Jev; observation never acts; Jev alone triggers only reversible, bounded actions.
- Only the Bridge writes runtime events and only the Observer writes observations, enforced by DB roles.
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

## 8. Build order

1. Observation tiers: rules first, Jev on ambiguity, extended observation payload with tier, evidence and escalation.
2. Read API and fleet UI: attention queue, fleet board, agent detail with the Jev panel, ladder health.
3. Dispatcher with rules + Jev playbook; roles and grants per principal.
4. Supervisor session with `spawn fleet` / `spawn tail`; digests from the dispatcher.
5. Experience views, `spawn corpus export`, replay commands.
6. RSI objectives over the corpus (rules coverage first), replacing the ingest-speed demo objective.

## 9. Open questions

1. Where do missions come from: a file the supervisor reads, CLI input, or a UI box?
2. How much authority does the supervisor get: max fleet size, may it stop or start RSI, which repos?
3. Which repositories do workers work on first?
