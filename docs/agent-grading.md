# Agent grading (phases 1 and 2)

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
| `engine_label`, `model_label` | columns / fields | the engine and model the preset stands for: the worker's engine when `engine_label` names a registered driver (`GradedPreset::driver_id()`), with `model_label` as that engine's model; else labels for people (`label_only`) | a `ModelProfile`: see "Phase 1 runs on `ModelProfile`" and "The engine a preset runs" |
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

## The engine a preset runs (Herdr PR 4, design 7.1)

`GradedPreset::driver_id()` (`spawn_core::grading::preset_driver`) gives the
engine a preset's worker runs: its `engine_label` when that names a
registered driver (`claude`, `agy`; driver ids equal Herdr's agent kinds),
else `claude` with `label_only: true`, its labels then for people only.
`/api/grading` names both (`driver`, `label_only`). Routing (`route_preset`)
stays pure: grade, then cost rank, then id. It never reads whether a host
can run the engine.

Delivery (`crates/spawn-cli/src/deliver.rs`) starts a task's worker on that
engine. For an engine told its model at launch (agy), the start carries the
preset's `model_label` (`CommandAction::Start.model_label`, agy's
`--model`); a Claude Code start carries none and is byte for byte what it
was. An ephemeral worker that ran on another engine, or on agy with
another model label than its task's preset names, is stopped
(`switch-engine-<task>-<revision>`), then started afresh on the task's; a
resident keeps the one engine it ran on. A resume (a revision, a restart)
runs the model label its session's last execution was launched on, as its
`execution.started` records it (the launch summary's `model`), so a
resumed worker stays on its preset's model and the preset is credited only
with what it ran. Only an execution that recorded none resumes on the
host's label (`SPAWN_AGY_MODEL`). Delivery checks a resume's label against
`agy models` as it checks a start's.

When the host cannot run the engine (not enabled in `SPAWN_ENGINES`, not
installed, its probe failed) or `agy models` does not list the label (the
capability probe, `GET /api/capabilities`), no command is made: the task
waits, and one `engine_unavailable` escalation names the engine and the
label. Never a silent fall back to Claude Code. Before the loop's first
probe, a task on another engine waits without an escalation.

## Top-Down 3-Tier Grading Stack

Agent grading uses a **top-down strong mainline** strategy instead of defaulting
routine tasks to weak models and waiting for failures:

| Tier | Grade | Role (`tier_role`) | Default preset (`spawn grade init-topdown`) | Engine / Model label | Execution profile | Cost rank | Initial lock / calibration |
|---|---|---|---|---|---|---|---|
| **T3** | `l3` | `top-decision` | `topdown-l3-fable` | `claude` / `claude-fable-5-2` | `fable-5-2` | `30` | Locked `l3` (`default`): architectural decisions, security/credentials/sandbox/migrations, and final escalation fallback from `l2` rework |
| **T2** | `l2` | `mainline` | `topdown-l2-neon` | `agy` / `gemini-pro-orchestrator-neon` | `neon-mainline` | `20` | Locked `l2` (`default`): supervisor & default strong mainline execution |
| **T1** | `l1` | `mechanical-offload` | `topdown-l1-mechanical` *(opt-in `--with-l1`)* | `claude` / `deepseek-v4` *(or `deepseek-v4.1-flash`, `kimi` via `--l1-model`)* | `mechanical-l1` | `10` | Onboarded `l1` (`cheap`), unlocked: strictly deterministic, zero-ambiguity mechanical tasks only; demoted/disabled cleanly on failure |

### Top-Down Guarantees

- **Idempotent seeding (`spawn grade init-topdown [--with-l1] [--l1-model <label>] [--json]`):**
  seeds `T3` (`topdown-l3-fable`) and `T2` (`topdown-l2-neon`) as active locked
  tiers, and optionally seeds and onboards `T1` (`topdown-l1-mechanical`) when
  `--with-l1` is given, leaving any existing catalog entries untouched.
- **Upward-only fallback (`resolve_routing_difficulty`):** when a task classifies
  as `l1` and no routable `l1` preset exists, routing falls back **upward** to
  `l2` (`topdown-l2-neon`). When a task classifies as `l3` and no routable `l3`
  preset exists, routing **never silently downgrades `l3` to `l2`**; it falls
  back to `--model default` with an explicit `routing_fallback` noting the refusal
  to downgrade `l3`.
- **Active over Demoted:** within the same `SkillGrade`, `route_preset` prefers
  `PresetState::Active` presets over `Demoted` ones before comparing `cost_rank`
  and `preset_id`.
- **Strict L1 offload gate (`can_offload_to_l1` + `brief-v3`):** only strictly
  deterministic `l1` work with an `Active` `l1` preset offloads to `T1`; the first
  quality rejection at `l1` immediately escalates rework to `l2` (`for_rework(1)`).
- **`model_label` / `engine_label` propagation:** `RoutingDecision` carries
  `engine_label` and `model_label`, and `spawn task assign` includes them in its
  JSON output alongside `execution_profile`.
- **Re-probe opt-in (off by default):** `SPAWN_GRADING_REPROBE_SECS` defaults to
  `0` (disabled / `i64::MAX` ms) so unflagged tasks do not unexpectedly shift to
  non-optimal starved presets unless an operator sets `SPAWN_GRADING_REPROBE_SECS > 0`.

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

A gate's `rejected` reroutes too (phase 2, "Gate step-up" below). In
phase 1 only a review did.

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
  or reassign carried them on (the successor counts instead). A budget
  overrun's continuation that carried the overrun's grading (same
  `first_preset_id`, its `quality_rejections` with it: "Known limits")
  replaces its cancelled predecessor the same way, so one piece of work is
  one sample; a continuation that carried nothing (`first_preset_id` null:
  no preset took its rework) leaves the predecessor counted. Left out: a
  cancel with no quality rejection (re-issue, reassign, over budget,
  aborted, mission closed, resident retired, a person's clean cancel).
  First pass: `landed` or `accepted` with no quality rejection.

## Commands

```
spawn grade init-topdown [--with-l1] [--l1-model <label>] [--json]
spawn grade register <preset_id> --engine-label <e> --model-label <m> --cost-rank <n> [--model-profile default|cheap]
spawn grade list [--json]
spawn grade exam --workspaces <dir> [--suite evals/onboarding]    # no store; prints results JSON
spawn grade onboard <preset_id> --results <json file>
spawn grade lock-grade <preset_id> --grade l1|l2|l3  |  unlock-grade <preset_id>
spawn grade enable|disable <preset_id>
spawn grade calibrate [--apply] [--json]
spawn grade upkeep [--apply] [--json]
spawn mission list [--limit N] [--json | --table]        # --json default; enriches tasks with grading & rework_step_up
spawn task assign ... (--brief <text|-> | --brief-file <path>) [--difficulty l1|l2|l3] [--preset <preset_id>]
spawn task reassign --id N [--reason <text|-> | --reason-file <path>]
spawn task reject --id N --note <text>        # reroutes stepped-up rework in the same statement
```

## Brief-file and Stdin Dispatch (`#119`)

- `spawn task assign` accepts `--brief-file <path>` and `--brief -` (read from
  stdin), and `spawn task reassign` accepts `--reason-file <path>` and
  `--reason -`. Passing both inline and `-file`, or empty/whitespace-only inputs,
  is refused.
- Supervisors write non-trivial or multi-line briefs to plain `.md`/`.txt` files
  and invoke `--brief-file <path>` (or pipe `--brief - <<'EOF'`) rather than
  passing multi-line text or `permissions.deny`-triggering shell command lists
  through `--brief "..."`. Spawn enforces credential and sandbox denials via
  `--settings`.

## Onboarding suite

`evals/onboarding/l<n>-<name>/`: `spec.md` (given to the preset),
`check.sh <workspace>` (exit 0 pass, 1 fail, 2 harness fault),
`reference/` (a passing workspace, tested in spawn-cli). Offline,
deterministic, POSIX `sh` and base tools. `spawn grade exam` bounds each
check at 30 s in Rust and treats exit 2, a signal or the time limit as a
harness fault that stops the exam. It is not sandboxed, and the workspace
must not see `evals/`: see `evals/onboarding/README.md`.

## Phase 2 (GitHub issue #99)

Every phase-1 call, record, answer and test is unchanged: each addition is
named only when it says something.

### Difficulty classifier (`classify_difficulty`, rules `brief-v3`)

An unflagged ephemeral `spawn task assign` (no `--difficulty`, `--preset`
or `--model`; never a resident) with a non-empty catalog has its
difficulty classified from the brief, before the catalog's coverage is
checked (see below). The rules are pure, integer-only
and deterministic, and every rule that matched is kept as evidence:

| rule | points | matches |
|---|---|---|
| `security` | +4 | English stems (`security`, `vulnerab*`, `exploit`, `cve-`, `authenticat*`, `authoriz*`, `credential`, `password`, `privilege`, `injection`, `csrf`, `xss`, `sandbox`, `secret`) and Chinese markers (`安全`, `凭据`, `密钥`, `权限`, `沙箱`, `鉴权`, `认证`, `提权`, `漏洞`, `注入`) |
| `migration` | +4 | `migrat*`, `schema`, `backfill`, `refold`, `"alter table"`, `"data loss"`, `"breaking change"`, and Chinese (`迁移`, `数据库表`, `不可逆`, `破坏性变更`) |
| `architecture` | +3 | `architecture`, `architectural`, `rfc`, `adr`, `protocol`, `consensus`, `"threat model"`, `"key rotation"`, and Chinese (`重大决策`, `架构设计`, `架构重构`, `核心架构`, `协议设计`, `分级重构`, `全局重构`, `顶层设计`, `共识`) |
| `concurrency` | +2 | `deadlock`, `concurren*`, `transaction`, `atomic`, `"race condition"`, `"advisory lock"`, and Chinese (`并发`, `死锁`, `竞态`, `锁顺序`, `原子性`) |
| `cross_cutting` | +2 | `refactor`, `redesign`, `architect*`, `rewrite`, `"across crates"`, `"every crate"`, `"backwards compat"`, and Chinese (`架构重构`, `全局重构`, `分级重构`, `跨模块`, `跨crate`, `全局改造`) |
| `multi_step` | +1/+2, +1 | 3 (6) or more enumerated steps; a multi-step marker (`phase`, `pipeline`, `end-to-end`, `multi-step`) |
| `file_scope` | +1/+2 | 3 (6) or more distinct paths |
| `crate_scope` | +1/+2 | 2 (3) or more distinct crates |
| `long_brief` | +1 | above 2000 characters |
| `low_risk` | -2 | mechanical, repetitive or tagging work only: `typo*`, `spelling*`, `formatting*`, `whitespace*`, `rustfmt*`, `mechanical*`, `repetitive*`, `"cargo fmt"`, `"registry entry"`, and Chinese mechanical markers (`拼写`, `错别字`, `格式化`, `排版`, `注释`, `重命名`) anywhere; `tag` or `label` only as the brief's first word followed by a determiner (`the`, `a`, `an`, `all`, `its`, `their`, `our`, `this`, `these`), `bulk-*` only as its first word; `regenerat*` only when its object, past a determiner, is a generated artifact (`fixture*`, `snapshot*`, `lockfile*`, `golden*`, `generated*`). Cancelled whenever any `security`, `migration`, `architecture`, or `concurrency` rule matched or any broadening stem/Chinese blocker is present (`every*`, `whatever*`, `caller*`, `implement*`, `support*`, `feature*`, authoring, code to build, defect/perf, `delete*`, `remove*`, `drop*`, `replace*`, `change*`, `redesign*`, `删除`, `移除`, `修复`, `实现`, `设计`, `重构`, `优化`, `修改`, `新增`, `逻辑`, `架构`). |

L1 means deterministic, repetitive or tagging work, never semantic
writing. `brief-v2` (replacing `brief-v1`) dropped `wording`, `readme`,
`docstring`, `one-line` and "docs only" / "doc only" from `low_risk`: a
README section, a docstring or wording is authored, and a one-line change
says nothing about determinism. A typo or spelling fix stays a mechanical
fix wherever it sits ("Fix the typo in README.md" is `l1`), while "Update
the README wording for the install section" and "Write the docstring for
X" are `l2`; "Tag the 40 open tasks with their capability" and "Add the
registry entry for the new file" are `l1`; "Implement label support" or
"Add tag filtering to the task list" are `l2`. A tagging word that is
not the brief's opening imperative names a thing the work is about, not
the work: "Fix the crash in tag matching", "Speed up the bulk insert
path", "Labels vanish after a restart", "Tag colour is too dark in the
sidebar", "Bulk import fails on empty rows"
and "Fix the scheduler" followed by a line "Tag the release afterwards"
are `l2`. "Regenerate the fixtures" is `l1`; "Regenerate the session
token and update the snapshot test" is `l2`. The rule keeps its name
`low_risk`, so recorded signals read the same, and records written under
`brief-v1` stay readable: nothing rejects a rules name it does not know.

Keywords are read per word and per path segment (`sql/schema.sql` names
the schema). The score is `l3` from 4; `l1` at -2 or below with no risk
rule, at most one path and at most 400 characters; `l2` otherwise. A brief
no rule matches is `l2`, exactly as phase 1 routed it. The evidence is the
rule's own vocabulary (a keyword of its list, a count), never text of the
brief.

The brief is classified before the catalog's coverage is checked, using
top-down upward-only fallback (`resolve_routing_difficulty`):
- If the catalog covers the classified difficulty, route there (with active
  presets preferred over demoted ones at equal grade).
- If classified `l1` and no routable `l1` preset exists, fall back upward to `l2`.
- If classified `l3` and no routable `l3` preset exists, **never downgrade `l3`
  to `l2`**; fall back to `--model default` with `routing_fallback` noting the
  missing `l3` coverage.

The `task.assigned` record carries the decision as `classification`
`{difficulty, score, rules, signals, rationale, routed_at (only when it ran at another difficulty)}`,
and the answer carries `classification` when a rule matched, beside `engine_label`,
`model_label`, and optional `execution_profile`. An empty catalog still means
`--model default` without a word.

### Gate step-up

A gate's `rejected` verdict (`is_quality_rejection`; never `conflict`,
`incompatible`, `host_fault`, `no_room`, `aborted` or `stale`) of a task
routed through the catalog reroutes its rework under `spawn task
reject`'s rule (`quality_reroute`, the same `for_rework` thresholds,
counting review and gate rejections together). It is recorded in the
verdict's own statement: `spawn_integrator_record` and
`spawn_task_reroute` in one SQL statement
(`PostgresStore::integrator_record_rerouted`, crate spawn-cli
`integrate::record_gated`), both or neither. A reroute the catalog cannot
make (nothing strong enough) or the store refuses never costs the
verdict: the integrator says so and records the verdict alone, as in
phase 1.

### Upkeep and the anti-starvation re-probe

- **Re-probe** (`reprobe_due`, `route_with_reprobe`): a routable preset
  whose grade is not locked, with no task first assigned to it at its
  calibrated grade's difficulty for `SPAWN_GRADING_REPROBE_SECS` (default
  `0`: off / opt-in; set positive whole seconds to enable), counted from the later of that task and its
  `window_start_ms`, is *due*. The next unflagged assignment routed at
  exactly that difficulty goes to it instead of the minimum-sufficient
  preset: the longest-starved first, then cost rank, then id. The answer
  says `reprobe: true`. This is bounded: once it has the task, the preset
  is not due again for an interval. It is idempotent: it is derived from
  `mission_tasks` (`PostgresStore::grading_activity`), not recorded.
  Explicit routing and rework never re-probe. A demoted preset now
  refills its window, and a dearer preset of the same grade is probed
  too.
- **Upkeep** (`spawn grade upkeep [--apply] [--json]`; the fleet loop runs
  it with `--apply` every `SPAWN_GRADING_UPKEEP_SECS`, default 21600,
  `0`: never, first at the loop's start): calibration as `spawn grade
  calibrate`, then the presets due a re-probe and whether `l2` is
  uncovered. Each run is one catalog read plus one window per routed
  preset, with at most one record per preset. A regraded preset's window
  starts afresh, so a second run holds. A failed run is said and tried
  again a minute later; it never fails a pass. The loop prints a line only
  when something was applied or is due.

### Named execution profiles

`grading_presets.execution_profile` (an added column, nullable) names the
profile a preset stands for: any `[a-z0-9][a-z0-9_-]{0,63}` but `default`
and `cheap`, which are `model_profile` itself. The worker still runs on
`model_profile` (`--model default|cheap`, `mission_tasks.model`): the
dispatcher knows two runtimes. The catalog, routing, `spawn grade` and
the API now tell apart as many profiles as presets name.

- `spawn grade register ... [--execution-profile <name>]` (the record's
  optional `execution_profile`).
- `spawn grade list`'s text table has a new `PROFILE` column, after
  `MODEL`: the preset's execution profile, else its model profile's name.
  The table is for people and its layout changed. `spawn grade list
  --json` stays phase-1 compatible: every phase-1 field is as it was, and
  `execution_profile` appears only on a preset that has one.
- `RoutingDecision.execution_profile`, the assign answer's
  `execution_profile`, and `task.assigned` / `task.rerouted` carry it when
  the preset has one.
- `GET /api/grading`: `{presets: [... every catalog field, routing_grade,
  profile, driver, label_only, tier_role, reprobe_due], profiles: [sorted names]}`,
  read-only, rendered by the web UI's `#/grading` view (`GradingView`).
- `spawn mission list [--limit N] [--json | --table]`: enriches each mission's
  tasks with `difficulty`, `routed_difficulty`, `preset_id`, `first_preset_id`,
  `tier_role`, `quality_rejections`, `engine_label`, `model_label`,
  `execution_profile`, and `rework_step_up` (`{quality_rejections,
  from_difficulty, routed_difficulty, stepped_up, first_preset_id, preset_id}`)
  from `grading_presets` and `mission_tasks` (via `PostgresStore::routed_task`),
  keeping `--json` as the default for existing callers and adding `--table` for
  terminal review.

## Known limits

- **Objective tasks are routed like an unflagged ephemeral task.** A
  task made from an objective (the integrator's `objectives::assign`, the
  SQL function `spawn_objective_assign`, which objective missions such as
  Mission #4 assign by) is classified from the objective's own title and
  prompt (never the brief's standing rules, earlier tasks' reports or gate
  history, whose words would move the classifier), routed through the
  catalog by the same rule as `spawn task assign` (its classified
  difficulty, else `l2`, with the anti-starvation re-probe:
  `missions::route_unflagged`), and recorded with its `model`,
  `difficulty`, `preset_id`, `execution_profile` and `classification`.
  Its gate rejections step its rework up as any routed task's do, within
  the task. Across tasks of one objective:
  - A budget overrun's continuation (`continues`, `p_continues`) keeps the
    continued task's grading, as a re-issue does: its difficulty,
    `first_preset_id` and `quality_rejections`, on its stepped-up
    `preset_id` (and that preset's `model` and execution profile) while
    that preset still takes its rework, else on the minimum-sufficient
    preset that does (`missions::continuation_route`). It is not
    classified again. When no routable preset takes its rework any more,
    it is assigned without the catalog. A continuation of a task assigned
    without the catalog is routed afresh by the integrator, and records
    exactly what it did before from the five-argument SQL call.
  - A fresh task after the objective **yields** (its attempt limit,
    `SPAWN_OBJECTIVE_ATTEMPTS`, 3 by default) is classified and routed
    afresh: nothing of the yielded task's step-up is carried, so at the
    default limit an `l1` objective never runs at `l3` across a yield.

  When the catalog is empty, covers neither its difficulty nor `l2`, or
  cannot be read, it is assigned as before (`default`, `l2`, no preset; an
  uncovered catalog or a read failure is logged). Grading never makes the
  integrator skip an objective, with one narrow race: a preset disabled
  (or regraded) between the integrator's catalog read and its
  `spawn_objective_assign` call is refused by the SQL's checks; the
  refusal is logged, nothing is recorded, and the next pass routes the
  objective on the catalog as it is then. `spawn_objective_assign` called
  with its five arguments (the gate's scenarios do) records exactly what
  it did before, unless it continues a routed task. Unlike
  `spawn task assign`, this holds for the mission's resident too: a
  resident whose next objective routes to another model profile is
  stopped and resumed on it (`deliver`'s model switch).
- A resident task of `spawn task assign` without `--difficulty` or
  `--preset` is not classified or routed: it runs on `--model default` as
  before.
- **Item 4 of issue #99 is still name-only at dispatch.** `execution_profile`
  is carried by the catalog, routing, the records, `spawn grade`,
  `spawn mission list`, and `/api/grading` (and shown in `#/grading`), but
  the dispatcher still runs every worker on `model_profile` (`default` or
  `cheap`). The web UI (`#/grading`, `MissionsView`, `TaskPanel`) now displays
  the `/api/grading` catalog by Top-Down tier and renders optional task grading
  badges when present, while the `MISSION_BOARD` SQL backing `GET /api/missions`
  does not yet project `mission_tasks` grading columns (`spawn mission list`
  supplies them via CLI enrichment; web task badges degrade gracefully when
  omitted). This change does not close issue #99 on its own.
- The classifier reads only the brief's words and structure. Under `brief-v3`,
  destructive verbs such as `delete`, `remove`, `drop`, `replace`, and `change`
  (along with Chinese equivalents `删除`, `修改`, `修复`, `设计`, etc.) cancel
  `low_risk`, so `"Run cargo fmt and delete the old dispatcher"` now classifies
  `l2`. Name `--difficulty` when phrasing still understates the change.
- A malformed `SPAWN_GRADING_REPROBE_SECS` is warned about once per
  process (on stderr), and re-probe stays off. The classifier does not
  read the code a task will touch.
- A re-probe gives a starved preset real work at its grade when enabled:
  work it is graded for, though possibly dearer than the cheapest choice.
  `SPAWN_GRADING_REPROBE_SECS` defaults to `0` (off / opt-in); set positive
  whole seconds to enable it.
- An execution profile is not run by the dispatcher. It is a name the
  catalog, routing and records carry, and the worker runs on its
  `model_profile`.

## Known limits of phase 1 (phase 2 addresses those marked)

- **Demoted presets can starve** (phase 2: the re-probe). Calibration reads only tasks at the
  difficulty of a preset's calibrated grade, and routing gives each
  difficulty to the minimum-sufficient grade, then the cheapest. A preset
  demoted `l2 → l1` competes for `l1` tasks with every `l1` preset; if one
  of those is cheaper it gets none, its window never fills, and it stays
  demoted. Bring it back by onboarding it again or by naming it
  (`--preset`) on tasks at its grade. Likewise a preset graded above every
  difficulty it is given (an `l3` preset taking only `l2` work) has an
  empty window and is never calibrated.
- Gate rejections do not reroute; only a review's `spawn task reject` does
  (phase 2: the gate step-up).
- Calibration runs when a person runs it; nothing schedules it (phase 2:
  the loop's upkeep).
- A re-issued or reassigned task keeps its preset without checking that it
  is still routable.
- The suite's default path is relative to the working directory.
- `#/grading` now displays the `/api/grading` catalog grouped by Top-Down tier
  (`spawn grade list` shows the same in the CLI); `spawn mission list` shows
  task difficulty, preset, `tier_role`, and `rework_step_up` history, while
  `GET /api/missions` still awaits SQL projection of task grading columns in
  `spawn-postgres` (the web `MissionsView` and `TaskPanel` render those badges
  whenever provided).
