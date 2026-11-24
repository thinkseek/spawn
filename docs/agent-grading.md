# Agent grading (phase 1)

Which graded preset a task runs on: the cheapest one proven good enough for
the task, moved to a stronger one when its rework keeps failing on quality,
and graded up or down on what it actually delivers.

## Naming

Agent grading's words were chosen against the words Spawn already uses. The
table says what each one is, and what it is deliberately not.

| Word | Where | What it is | Not to be confused with |
|---|---|---|---|
| grading catalog | `grading_presets`, `PostgresStore::grading_catalog()` | every graded preset | the **resident roster** (`resident_roster()`, `RESIDENT_ROSTER`, the supervisor charter's "roster"): approved residents |
| `GradedPreset` / `preset_id` | a `grading_presets` row; `mission_tasks.preset_id`; `--preset` | one entry of the catalog, `[a-z0-9][a-z0-9_-]{0,63}` | `ModelProfile` (`--model default\|cheap`, `mission_tasks.model`): "profile" already names it, so the catalog never says "profile"; an `agent_id`, a resident, a node |
| `model_profile` | column / field | the `ModelProfile` a preset's worker starts on | a second model switch: it *is* `--model` |
| `engine_label`, `model_label` | columns / fields | labels for people: the engine and model the preset stands for | anything that runs: see "Phase 1 runs on `ModelProfile`" |
| `SkillGrade` (`l1`..`l3`) | `onboarded_grade`, `calibrated_grade`, `locked_grade` | what a preset has proven it can do | a Feature Map capability |
| `routing_grade()` | `GradedPreset` | the grade routing reads: `locked_grade`, else `calibrated_grade`, and none unless the preset is routable | the state `active` |
| `TaskDifficulty` (`l1`..`l3`, default `l2`) | `mission_tasks.difficulty` | what a task needs | a priority, an objective's size |
| `PresetState` | `state`: `unexamined`, `active`, `demoted`, `disabled` | where a preset stands; only `active` and `demoted` are routed. `state`, as on `mission_missions`, `mission_residents`, `mission_tasks` | a resident's `proposed/approved/retired`, an admission |
| lock / unlock a grade | `spawn grade lock-grade`, `locked_grade`, `preset.grade_locked` | a person's grade for a preset; calibration leaves it alone | **pin** (`spawn task assign --pin`, `mission_tasks.pinned`): a task kept with its worker |
| rework step-up / reroute | `TaskDifficulty::for_rework`, `ReworkReroute`, `RoutingDecision::stepped_up()`, `task.rerouted` | rework routed at a harder difficulty after quality rejections | **escalation** (`spawn_escalations`, the dispatcher's stop escalations) |
| `quality_rejections` | `mission_tasks` | sends-back for quality: a review's `revise`, a gate's `rejected` | `revisions`, which also counts a gate's `conflict` and `incompatible` |
| `window_start_ms` | `grading_presets` | where a preset's calibration window starts | when it was "graded" |
| onboarding exam | `evals/onboarding/`, `spawn grade exam` | the offline cases a preset passes before it is routed | the RSI gate's scenarios |
| calibration | `spawn grade calibrate`, `preset.regraded` | grade moves from recent outcomes | the scorecard; the Observer's calibrated probabilities |
| rank | `spawn_grade_rank()`, `SkillGrade::rank()` | a grade or difficulty stored as 1 to 3 | `cost_rank` (cheaper first) |

Grades and difficulties are one scale with two types. On every record, every
JSON answer and the command line they are `l1`, `l2`, `l3`; in tables they
are their rank, 1 to 3 (`smallint`). The folds read either spelling
(`spawn_grade_rank`), the checked writers only the wire name.

## Phase 1 runs on `ModelProfile`

What reaches a worker is the task's `model`, a `ModelProfile`: `default` or
`cheap`. A preset maps to one (`model_profile`); its `engine_label` and
`model_label` are not carried to the worker. So in phase 1 the catalog can
tell apart at most two runtimes. Two presets on the same `ModelProfile` are
examined, graded and calibrated apart, but run the same: a reroute between
them (`task.rerouted`) moves the task's preset and its calibration
attribution, and the worker runs as before. `spawn task reject` says so
(`same_model_profile: true` and a line on stderr).

## Rules (`spawn_core::grading`, pure, integer-only)

- **Rework step-up** (`TaskDifficulty::for_rework`): after `q` quality
  rejections, an `l1` task is routed at `l2` from `q >= 1` and at `l3` from
  `q >= 3`; `l2` at `l3` from `q >= 2`; `l3` stays. A gate's `conflict` or
  `incompatible` (fleet concurrency, not quality) never counts.
- **Routing** (`route_preset`): a named preset must be in the catalog,
  routable and graded at least what the routed difficulty requires, or the
  call fails. Otherwise, of the routable presets whose routing grade is at
  least the required grade, the least by `(grade, cost_rank, preset_id)`:
  the minimum-sufficient grade first, then the cheapest.
- **Onboarding** (`evaluate_onboarding`): the highest grade G such that
  every grade up to G has at least one case and every case at or below G
  passed. No `l1` means no grade and state `disabled`.
- **Calibration** (`calibrate_preset`): over the newest 10 outcomes, a
  preset with a locked grade or not routable holds; with `total >= 5` and
  `first_pass * 100 < total * 60` it is demoted one grade (`disabled` at
  `l1`); below its onboarded grade with `total >= 8` and
  `first_pass * 100 >= total * 90` it is restored one grade (back to
  `active` at its onboarded grade). Between 60 % and 90 % it holds.

## Assigning (`spawn task assign`)

- `--model`: exactly as before agent grading. Refused with `--difficulty`
  or `--preset`.
- `--difficulty` or `--preset`: through the catalog, strictly. A catalog
  that cannot take the task is an error (`no routable preset is graded …`,
  `preset … is graded …, below …`).
- Neither, `--kind resident`: as before, on `--model default`. A resident
  keeps one model profile; routing its tasks by the catalog would stop and
  resume it on another profile from one task to the next.
- Neither, an ephemeral task (the supervisor's ordinary assignment): routed
  at `l2` when some routable preset is graded `l2` or above. Otherwise (an
  empty catalog, a catalog with only `l1` presets, or one where `disable`,
  `unlock-grade` or `calibrate --apply` left `l2` uncovered) it is assigned
  on `--model default` as before. When the catalog holds presets, the
  answer says why (`routing_fallback`) and so does stderr; `spawn grade
  disable`, `unlock-grade`, `onboard` and `calibrate --apply` warn on stderr
  when they leave `l2` uncovered. An owner's catalog change never stops the
  fleet's assignments.

The supervisor's charter is unchanged: it names neither `--difficulty` nor
`--preset`, so its assignments take the default `l2` route or the
fallback.

## Rework (`spawn task reject`)

`spawn task reject` sends the task back and, when the stepped-up difficulty
needs more than the task's current preset has (or that preset is no longer
routable: a person disabled it), reroutes it to the minimum-sufficient
preset in the same statement (both or neither). When no routable preset is
strong enough, the task is still sent back and the answer carries
`reroute_error`.

Only a review reroutes. A gate's `rejected` counts as a quality rejection
(it steps up the next review's reroute and fails first pass) but does not
reroute by itself, so in a mission whose gate alone accepts (most of
Spawn's own RSI missions) a task stays on its preset.

## Storage (`crates/spawn-postgres/sql/schema.sql`)

- `grading_presets`: the catalog, the fold of `grading.recorded` events of
  the control session (agent `agent_control`, source `control`), the
  convention of the nodes' `node.recorded`. Every change is appended and
  folded in one transaction by the owner-only `spawn_grading_record`, which
  checks the record against the catalog first: its shape, its
  `from_state` (and `from_grade`) against the preset now, its `to_state`
  against the rule, one-step grade moves, no no-op. `spawn_grading_refold()`
  rebuilds the table from the log, from empty, and is idempotent.
  `spawn_control_refold` reads only `control.recorded`, so it neither reads
  nor clears the catalog, and an older schema's control refold never meets a
  grading record. `EventKind::GradingRecorded` decodes them for every reader
  of the whole log (the Bridge, the Observer), as control records that
  belong to no agent.
- Deploy order: every long-running Spawn process (the Bridge, the
  Observer, every other reader of the whole log) must run a build that decodes
  `grading.recorded` before the first `spawn grade register`, or an older
  reader stalls on the first catalog record. Landing and deploying this
  change satisfies that; register presets only after the deploy.
- Records:
  - `preset.registered` {preset_id, engine_label, model_label, model_profile, cost_rank}
  - `preset.onboarded` {preset_id, from_state, to_state, to_grade (null: failed `l1`), passed, total, failed_cases, suite}
  - `preset.regraded` {preset_id, decision, from_grade, to_grade, from_state, to_state, first_pass, total} (calibration)
  - `preset.grade_locked` {preset_id, from_grade, to_grade (null: unlocked), from_state, to_state}
  - `preset.state_changed` {preset_id, from_state, to_state}
- The checks that keep the catalog honest: a preset disabled while it has a
  grade (by a person or by calibration) is enabled before it is onboarded
  again; only a preset that never had a grade (it failed `l1`) is examined
  again while disabled. Unlocking the only grade of a routed preset makes
  it `unexamined`, never `active` on nothing.
- `mission_tasks.difficulty`, `preset_id`, `first_preset_id` and
  `quality_rejections`, appended after every other column by `alter table
  ... add column if not exists` only (a column added to the `create table`
  as well would put a fresh table's columns in another order than a
  migrated one's, which `check-schema-upgrade.sh` refuses).
  `first_preset_id` is set once by `task.assigned` and never moved by a
  reroute; `quality_rejections` moves on a review's `revise` and a gate's
  `rejected` only. A re-issue or reassign carries the difficulty, both
  presets and the rejections to the new task.
- `spawn_task_assign` takes two more parameters, both defaulted
  (`p_difficulty integer default 2`, `p_preset text default null`), so
  every earlier call still assigns as before. A named preset must be
  routable, graded at least `p_difficulty`, and `p_model` its
  `model_profile`: the SQL holds the supervisor's login to the same rule as
  the CLI. The `task.assigned` record names `difficulty` and `preset_id`
  only when a preset is named or the difficulty is not `l2`.
- `spawn_task_reroute` records `task.rerouted` {id, preset_id, model,
  from_preset_id, revision, routed_difficulty} for a task sent back, at
  exactly the difficulty its quality rejections step it up to, to another
  routable preset graded for it. `task.rerouted` is a control record, in
  the control fold (it moves `mission_tasks.preset_id` and `model`, which
  the dispatcher reads): a schema rolled back past agent grading would
  refuse to refold a log that holds one (`unknown control record`). That is
  deliberate: the task's model must follow the reroute in the same fold.
- Calibration's window is read from `mission_tasks` alone, through the
  index `mission_tasks_first_preset (first_preset_id, difficulty,
  updated_at_ms)`: the tasks first assigned to the preset, at the
  difficulty of its calibrated grade, assigned since `window_start_ms`,
  newest 10 by `updated_at_ms`. In it: tasks `landed` or `accepted`, and
  tasks `cancelled` after at least one quality rejection unless a re-issue
  or reassign carried them on (the successor counts instead). Left out: a
  cancel with no quality rejection (re-issue, reassign, over budget,
  aborted, mission closed, resident retired, a person's clean cancel).
  First pass: `landed` or `accepted` with no quality rejection.

## Commands

```
spawn grade register <preset_id> --engine-label <e> --model-label <m> --cost-rank <n> [--model-profile default|cheap]
spawn grade list [--json]
spawn grade exam --workspaces <dir> [--suite evals/onboarding]    # no store; prints results JSON
spawn grade onboard <preset_id> --results <json file>
spawn grade lock-grade <preset_id> --grade l1|l2|l3  |  unlock-grade <preset_id>
spawn grade enable|disable <preset_id>
spawn grade calibrate [--apply] [--json]
spawn task assign ... [--difficulty l1|l2|l3] [--preset <preset_id>]
spawn task reject --id N --note <text>        # reroutes stepped-up rework in the same statement
```

## Onboarding suite

`evals/onboarding/l<n>-<name>/`: `spec.md` (given to the preset),
`check.sh <workspace>` (exit 0 pass, 1 fail, 2 harness fault),
`reference/` (a passing workspace, tested in spawn-cli). Offline,
deterministic, POSIX `sh` and base tools. `spawn grade exam` bounds each
check at 30 s in Rust and treats exit 2, a signal or the time limit as a
harness fault that stops the exam. It is not sandboxed, and the workspace
must not see `evals/`: see `evals/onboarding/README.md`.

## Known limits of phase 1

- **Demoted presets can starve.** Calibration reads only tasks at the
  difficulty of a preset's calibrated grade, and routing gives each
  difficulty to the minimum-sufficient grade, then the cheapest. A preset
  demoted `l2 → l1` competes for `l1` tasks with every `l1` preset; if one
  of those is cheaper it gets none, its window never fills, and it stays
  demoted. Bring it back by onboarding it again or by naming it
  (`--preset`) on tasks at its grade. Likewise a preset graded above every
  difficulty it is given (an `l3` preset taking only `l2` work) has an
  empty window and is never calibrated.
- Gate rejections do not reroute; only a review's `spawn task reject` does.
- Calibration runs when a person runs it; nothing schedules it.
- A re-issued or reassigned task keeps its preset without checking that it
  is still routable.
- The suite's default path is relative to the working directory.
- No page shows the catalog; `spawn grade list` does. `spawn mission list`
  shows a task's `model`, not its difficulty or preset.
