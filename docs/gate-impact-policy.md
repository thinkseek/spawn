# Impact-based landing gate: policy note and dry run (task 490)

Status: **proposal and prototype, dry run only.** Nothing here is installed.
`scripts/rsi/gate.sh` does not read `rsi/gate-impact/`; the live landing path
still runs the full historical suite. Two independent reviews, the owner's
decisions in section 8, and the protected-file landing path are required
before any live switch. This note does not retire a check, does not touch
C1-C19 of `sync/design-gate-v2.md`, and does not authorize a skip from model
text.

## 1. What 243 measured, and why it took 1973.790 s

The owner gate of 243 (`865cc4df`, base `5e416443`) is the evidence (gate log
`spawn-gate-2401779-18ea16198a19e3c8-0/stdout`):

| part | seconds |
| --- | ---: |
| setup (exports, socket scan, static checks) | 45 |
| base phase, 160 runs, **0 results from the cache** | 778 |
| candidate phase: build 18, cargo test listing 61, 186 runs | 1149 |
| **total** | **1973.790** |

Four scenario slots were nearly saturated. The two longest candidate runs
were 192 (301.9 s, 218.6 s of it explicit sleep) and 209 (303.3 s, 89.6 s
sleep); `check-tests.sh` was 184.8 s with no sleep. 160 base and 186
candidate runs are distinct invocations, not retries.

243 changed two files: `rsi/backlog/243.json` and
`rsi/scenarios/nightly/023.sh`. **The gate never runs `rsi/scenarios/nightly/`,
but its base-cache key hashes every file under `rsi/scenarios/` except the
top-level `*.sh` files and four lists** (`scenarios_key`,
`scripts/rsi/gate.sh:3211`). The nightly edit therefore changed every
objective's base key and forced the base phase to re-judge all 160 runs that
the accepting gate of the base had already earned. The 0 cache hits are a
keying defect, not a workload fact.

The other evidence point is 485 (`33777df8`, base `865cc4df`, three runs of
its own scenario 241): 1309.4 s total, setup 74, base 0 (159 cached),
candidate 1232, check-tests 187.8, build 20.3, listing 73.9, task scenario
241 39.9 s a run. A Rust change that runs today's whole check-tests cannot
meet 180 s even with a perfect base: the check alone is 184.8 s.

The part rows above are rounded phase boundaries, not an exhaustive
decomposition: 45 + 778 + 1149 = 1972 s against the measured 1973.790 s
total (1.79 s unassigned), and 74 + 0 + 1232 = 1306 s against 1309.4 s
(3.4 s unassigned). The measured totals are the authoritative ones.

## 2. The policy

### 2.1 Mandatory core, on every landing

- The static trust checks keep running: protected paths (the controller's
  `rsi/scenarios/protected`, never the candidate's copy, matched by the
  gate's string-prefix rule), no build-time code (`build.rs`, `.cargo/`, a
  `Cargo.toml` with `build =`, a new proc-macro), and the test count from
  the base's cached `cargo test` list (C15). **The prototype does not
  implement that count as an always-on check.** It computes a static key
  from the protected list and the core id, but no job compares a test
  listing, and the listing itself is a cache-skippable job keyed on the
  Rust tree; until C15's wiring exists the ratchet is `gate.sh`'s job
  (`scripts/rsi/gate.sh:3830-3925`), not a core check this selector models.
- A versioned evidence record is written: policy version and digest, base
  and candidate commits and tree, the environment key (today the constant
  recorded in `timings.json`, not a live probe: section 9), the selected
  jobs and the skipped jobs with a deterministic reason.
- **Fail closed.** Unknown path class, missing dependency, or a cache miss
  never narrows: an unknown class runs the full gate; an unreadable or
  malformed policy, timings profile, base record or tree, a base record
  that does not name exactly the base commit (a missing, empty or short
  commit binds to no base), a candidate that does not descend from its
  base, or a selected job with no measured time is **no verdict** (exit 2),
  and the caller keeps today's full gate. No job is skipped because a model
  said so.
- A new candidate hash invalidates any prior selection or approval: the
  approval digest in section 5 covers the candidate tree and every selected
  key.

### 2.2 Impacted checks

Selection uses only controller-side signals: git paths and content hashes of
what a job reads (the shape of C14), never model text. A job runs when the
hash of its inputs changed. A job whose key is unchanged but for which no
base result is recorded runs too (a cache miss is not a skip; a record that
does not name the base commit is no verdict, not a miss). The C1 data-loss
and control journeys run on every landing, cached or not:

- `check-e2e.sh` (what C1 assigns to check-e2e),
- `check-j03-human.sh` (015 and 038: human input and keys),
- `check-j06-recover-a.sh` and `check-j07-recover-b.sh` (028: no line lost).

The task's own scenario, when there is one, is always selected: it must fail
on the base and pass on the candidate. The prototype predicts one candidate
run (design-gate-v2 section 2); `--task-runs 3` models the live P25 rule.

### 2.3 Escalation

Shared, cross-cutting, protected-as-judge, or unknown input escalates to
today's full gate (and to the full v2 CI + journey set once that set is the
documented full set). It is never a narrow accept:

- `scripts/**`, `deploy/**`, the top-level `rsi/scenarios/**` and the gate's
  lists, `web/**` except `web/src/`, `web/test/`, `web/index.html`,
  `rust-toolchain*`, `.cargo/**`;
- any changed path no class claims.

**One interpretation needs the owner's ratification.** `rsi/backlog/**` and
`rsi/scenarios/nightly/**` are protected (the owner reviews them, as 243
was), but no landing scenario reads them, so they must not escalate the
whole gate. The backlog is not inert, though: `check-tests.sh` runs
`cargo test`, and `crates/spawn-cli/src/objectives.rs` loads the tree's real
`rsi/backlog` to assert its order, so the backlog is a named input of
`check-tests.sh` (and of nothing else). A backlog edit therefore selects
that one job, not the full gate. The prototype keeps both directories in
the review flag and out of the escalation reason. If the owner rejects that
split, every 243-type change pays the full gate.

### 2.4 Cache

- A key is a content hash of what a job reads, computed on both trees: the
  Rust tree (`crates/`, `Cargo.toml`, `Cargo.lock`), the web tree, the
  scripts tree, the backlog tree (`rsi/backlog/`, for `check-tests.sh`),
  `README.md` (for scenario 008), the scenario blob, the environment key
  (`env`, today the constant recorded in `timings.json`, not a live probe;
  section 9), and the policy version.
- A result is reused only when the candidate's key equals the base's key and
  the base's accepting gate recorded the result **for exactly this base
  commit**. The accepting gate's own candidate runs are meant to seed the
  new main (C16; the gate does this today only for solved objectives,
  `scripts/rsi/gate.sh:4814`), so a check, journey or CI hit depends on the
  pending C15/C16/C19 wiring and is not available on a live landing yet.
- **Candidate runs never certify a later candidate.** The selector reads
  results; it never writes a cache entry. A new candidate, a changed input, a
  different toolchain or environment, or a new policy version changes the
  key and the approval digest.
- **A base change is a new tree, not a stale hit.** The base commit is not a
  key field; the key is the content the job reads. When main moves, the
  accepting gate's candidate run seeds the result for the new main, so the
  next landing reads a result taken on exactly that tree. A base whose tree
  differs for what the job reads is a different key; one that differs only in
  nightly data changes no landing key.
- **The nightly directory is not in any landing key.** A change under
  `rsi/scenarios/nightly/` cannot invalidate a landing result. This is the
  first fix the 243 evidence demands; it needs a test that a nightly edit
  leaves `scenarios_key`-equivalent landing keys alone.

### 2.5 Real-time invariants and nightly work

The proposed policy keeps 192/209-style holds and the historical objective
scenarios off the ordinary landing path, per the owner's approved
design-gate-v2 Decision 1 (performance bounds and retired scenarios are
checked nightly). The `--historical-mode cache-only` replay models today's
gate instead: every check stays, and only content-addressed results are
reused. A scenario listed in `rsi/scenarios/serial` (read at the base, as
the controller does) is charged alone, outside the four-slot makespan.
Neither default is this tool's own: `--historical-mode` takes the policy's
`default_mode` (cache-only) and `--unsolved-mode` its `unsolved_default`
(run), so an operator who omits the flags gets the conservative replay, not
the unapproved retirement or the unratified carry.

Nothing is hidden outside the clock: section 7 lists exactly what the landing
does not prove and the nightly owner of each such fact.

### 2.6 The 180 s target

The target covers selection, the necessary build/listing, and every check the
policy requires for that change class. It does not apply to the full or
escalated gate and it does not apply to nightly. There is no universal
3-minute SLA: the numbers in section 4 show four classes that cannot meet
it under today's checks, including the measured 243-type change once
check-tests is keyed on the backlog.

## 3. Change classes

| class | paths | landing jobs |
| --- | --- | --- |
| nightly-only / data | `rsi/backlog/`, `rsi/scenarios/nightly/` | static core + C1; a backlog edit also selects `check-tests.sh` (its key names `rsi/backlog/`), and the changed nightly scenario is proven by the nightly |
| docs | `docs/`, `README*` | static core + C1; a `README.md` edit also selects `008.sh`, which greps the README floor |
| web | `web/src/`, `web/test/`, `web/index.html` | static core + `check-web.sh` + `check-tests.sh` (its `cost_page` test runs `web/src/cost.ts`) + C1 + journeys whose key changed |
| rust | `crates/`, `Cargo.toml`, `Cargo.lock` | static core + build + test listing + `check-tests.sh` + `check-clippy.sh` + C1 + journeys + the task scenario |
| judge / shared | `scripts/`, `deploy/`, `rsi/` (except the two data paths), `web/` config, toolchain | today's full gate (186 candidate jobs + base phase) |
| unknown | anything else | full gate, `fail_closed: true` |

Protected paths are reported separately (`owner_review_required: true`). The
selector does not decide the protected-path rule; `gate.sh` still refuses a
worker candidate that changes one.

## 4. Dry run (2026-11-22, host `spawn`, 4 slots)

Command: `sh rsi/gate-impact/replay.sh` (raw output in
`rsi/gate-impact/evidence/dry-run-2026-11-22.txt`; tests in
`rsi/gate-impact/evidence/test-2026-11-22.txt`).

| fixture | mode | selected | predicted | budget |
| --- | --- | ---: | ---: | --- |
| 243 `5e416443..865cc4df` | no mode flag: the policy's defaults (cache-only, unsolved run) | 6 | 349.2 s ¹ | OVER |
| 243 | proposed (historical nightly, unsolved carried) | 5 | 229.8 s ¹ | OVER |
| 243 | cache-only + carry (no check retired) | 5 | 229.8 s ¹ | OVER |
| 243 | strict cache, unsolved not carried | 6 | 349.2 s ¹ | OVER |
| nightly-only (unit fixture) | proposed (historical nightly) | 4 | **131.0 s** ¹ | WITHIN |
| nightly-only (unit fixture) | cache-only + carry (no check retired) | 4 | **131.0 s** ¹ | WITHIN |
| 485 `865cc4df..33777df8` | proposed + task 241 | 29 | 510.5 s | OVER |
| 485 | today's checks + cache | 187 | 1277.1 s | OVER |
| web-only (`web/src/cost.ts` on 865cc4df, real tree, profile 243) | no mode flag: the policy's defaults (cache-only, unsolved run) | 185 | 1101.4 s | OVER |
| web-only (same real-tree change) | proposed (historical nightly) | 26 | 380.3 s | OVER |
| web-only (unit fixture) | conservative journey keys | 14 | 330.9 s | OVER |
| shared `337c94b5..2e8781ca` | full escalation | 186 | 1973.8 s | OVER |
| unknown path (unit fixture) | fail-closed | all | 1973.8 s | OVER |
| `build.rs` (unit fixture) | static trust | - | - | reject, exit 1 |
| missing timing/policy/record | no verdict | - | - | exit 2 |

The two real-tree web rows are a probe, not a `replay.sh` row: the selector
was run against a throwaway clone of the mission history at 865cc4df with a
`web/src/cost.ts` edit (profile 243), because the replay repository holds no
such commit. The unit-fixture rows, including the 14-job web fixture, are the
tests' temporary repository.

¹ **Depends on pending C15/C16/C19 wiring.** The gate seeds only solved
objectives today (`scripts/rsi/gate.sh:4839`); the selector's hits on
checks, journeys and CI jobs are a prediction until the accepting gate
seeds those results. The 25 such jobs in a narrow run are the build, the
test listing, check-tests (when its key is unchanged), check-clippy,
check-web, the eight non-C1 journeys and the twelve non-journey checks. No
reader should take 131.0 s as an achieved landing yet.

The 243 pair is **not** 131.0 s, and the reviews of the first draft are why.
243 edited `rsi/backlog/243.json`, and `cargo test` reads the backlog, so
`check-tests.sh` is selected by its own key (184.8 s) in every mode. The
honest 243-type number under the policy's own default (cache-only, unsolved
run) is 349.2 s; the 229.8 s row needs either the proposed nightly retirement
or the unratified carried `unsolved` result, and neither is installed. Both
are over budget until test selection (C14, C15) is proven, and no key fix can
bring a backlog edit under 180 s while `check-tests` alone is 184.8 s.

How the 131.0 s is made (a change that touches no key but a nightly data
file, e.g. `rsi/scenarios/nightly/023.sh`):

- setup 45 s (measured, includes the static checks);
- the four C1 journeys, longest-first on four slots: 85.993 s wall; their own
  time is 305.3 s, so the slots matter (run serially, 350 s);
- nothing else runs: 186 jobs are skipped, with two different accounts that
  give the same 4 selected and 131.0 s. Under the proposed retirement: 25
  cache hits on check, journey and CI results, 159 historical objectives
  skipped as `nightly-scope` (158 solved ones plus the unsolved 241, which
  the retirement does not read), and the two task-only scenarios (`029.sh`,
  `238.sh`). Under cache-only + carry, where no check is retired: 183 cache
  hits from the base's accepting gate (the 158 solved objectives and the 25
  check/journey/CI results), the 241 result carried as `unsolved` (not a
  pass), and the same two task-only jobs. Neither account is a claim that
  every skipped job recorded a pass; the first draft's "all 186 skipped jobs
  recorded a pass" was wrong.

The table's 243 rows are the same landing under three policies, none under
budget. **No check is retired in the cache-only + carry row**: its 185
skipped jobs (182 cache hits, the carried 241, two task-only) are skipped
because their inputs are byte-identical and the base recorded a result, not
because a policy removed them. The strict-cache rows are the price of the
one unsolved objective on main (241): with no carried `unsolved` fact, a
strict cache re-judges it on the base (two agreeing runs, 119.4 s measured)
and on the candidate (59.2 s, inside the 184.8 s makespan). That is the
difference between 229.8 s and 349.2 s.

The 485 rows show the floor for a Rust change: build 20.3 + listing 73.9 and
the serial journey `check-j04-observe.sh` 44.3 are charged alone (`rsi/scenarios/serial`),
check-tests 187.8, clippy 30.4, every journey whose Rust input changed, the
twelve non-journey checks, and task 241 at 39.9 (one candidate run;
`--task-runs 3` models P25). Even with historical objectives nightly it is
510.5 s. Under today's checks it is 1277.1 s (`107.sh` also runs alone,
30.3 s).

The web class is deliberately conservative: the prototype keys every journey
on rust + web + scripts, so a `web/src/` change re-runs the eight non-C1
journeys too, and it runs check-tests (the `cost_page` test) and
`check-j04-observe.sh` alone. On the real tree the class also runs the twelve
non-journey `check-*` scenarios and, under the policy default, the 159
historical objectives, because every scenario's key includes the web tree:
26 jobs and 380.3 s under the nightly retirement, 185 jobs and 1101.4 s under
the default (profile 243). The 14-job unit fixture's 330.9 s is the same
policy shape over the test repository (12 journeys, check-web, check-tests),
not the class's cost. Narrowing
that needs the per-journey `depends_on` proof (C14); until then the selector
over-selects in this class, which is the safe direction.

Assumptions, all printed by the selector:

- the base record is the accepting gate's candidate passes, bound to exactly
  the base commit (a missing, empty or short commit is no verdict); the
  checks/journeys/CI seed needs the pending C16 wiring, which exists today
  only for solved objectives; candidate runs of a later tree are never read;
- four scenario slots, longest-first (the gate's order), except the
  scenarios `rsi/scenarios/serial` runs alone; setup includes the static
  checks; no host noise;
- the timings are one measured run each (243 and 485) on this host; a
  different host or toolchain needs its own profile, and a missing time is
  no verdict;
- `--task-base-cached` is the caller's word, not a verified fact: it drops
  the task's base run from the prediction and the selector does not check
  that the base recorded a cached result, so a base with no cached run is
  understated by one task run (39.9 s for 241);
- with no `--base-record`, every job that runs is a cache miss (the
  fail-closed direction); a job skipped by the nightly retirement is neither
  a hit nor a miss.

## 5. The prototype

`rsi/gate-impact/` is bounded and read-only:

- `gate_impact.py` — the selector (named so it cannot shadow the standard
  library's `select` module on sys.path[0]). Given `--repo`, `--base`,
  `--candidate`, it prints the policy version and digest, the changed paths
  and classes, the content keys (static, rust, web, scripts, backlog,
  readme, per job), every selected and skipped job with a reason and a key,
  the approval digest, and the predicted wall clock against 180 s. Exit
  codes: 0 selection, 1 static reject, 2 no verdict (including malformed
  policy, negative slots, a base the candidate does not descend from, and a
  record that does not name exactly the base), 3 `--require-narrow` not met.
- `policy.json` — policy version 1: the classes, jobs, C1 set, scenario
  inputs, serial list, mode defaults and the non-goals.
- `timings.json` — the measured own times from the 243 and 485 gate logs, the
  serial build/listing times, the base-run times, and the measured full-gate
  totals.
- `records/base-5e416443.json`, `records/base-865cc4df.json` — the accepting
  gates' candidate passes (the seed), plus the `unsolved` fact for 241.
- `test_gate_impact.py` — 26 tests on a temporary repository: narrow nightly,
  web and rust classes, carry/run, the policy defaults, README -> 008,
  backlog -> check-tests, web/src -> check-tests, the serial list, full
  escalation, unknown path, nested build.rs/.cargo, a proc-macro with a
  trailing comment, protected prefixes, a subdirectory `--repo`, ancestry,
  unknown policy version, malformed policy, negative slots, missing timing,
  missing/empty/short/wrong base record, a cache miss with no record, the
  approval digest bound to the candidate, and a no-change diff.
- `replay.sh`, `evidence/` — the runs in section 4.

It runs no check, builds nothing, writes no cache, and is not wired into
`gate.sh` or `deploy/`.

## 6. What the dry run confirms and what it refutes

Confirmed:

- A change that touches only a nightly data file predicts **131.0 s**: four
  C1 journeys and 186 skipped jobs. It is a prediction, not an achieved
  landing: the seed wiring in the footnote is not installed, and the 131.0 s
  figure is the same under the proposed nightly retirement and under
  cache-only + carry. The nightly row therefore depends on the retirement
  (the 159 historical objectives are skipped, not carried), and the carry
  row on the carried `unsolved` rule; neither is installed.
- The specific cause of the 1973 s 243 landing is the nightly file in the
  landing cache key, not the number of scenarios.
- Unknown impact and a missing dependency fail closed: full gate or no
  verdict, never a narrow accept.

Refuted:

- "A 243-type change lands in 131 s." It does not: 243 edited the backlog,
  `cargo test` reads the backlog, and check-tests is 184.8 s, so the honest
  243-type prediction is 349.2 s under the policy's own default (229.8 s only
  with the nightly retirement or the carried `unsolved`), over budget.
- "Cache alone is enough." Strict cache-only with the one unsolved objective
  on main costs 349.2 s, over the budget. The 229.8 s needs a carried
  `unsolved` result; the retirement alone does not remove check-tests.
- "A Rust change fits in 180 s." It does not: 510.5 s under the proposed
  policy, 1277.1 s under today's checks. check-tests alone is 184.8 s.
- "A web change fits under conservative keys." On the real tree it is
  380.3 s under the retirement and 1101.4 s under the policy default, not the
  14-job unit fixture's 330.9 s, and check-tests must run for `cost.ts`.
- "There is a universal 3-minute SLA." There is not. Runtime/shared changes
  correctly stay at the full 1973.8 s.

## 7. What is not proven before landing

Under the proposed policy the landing does not prove: the changed nightly
assertion (023/025/013/032/036), the retired objective scenarios, or the
real-time holds 192/209. Those are the nightly's job under the approved
Decision 1; a red nightly marks main red, bisects and holds landings (C11,
C18). The change's own nightly assertion is verified within a day, not before
its landing. That is the accepted trade-off, and it is not hidden: the
selector prints every such skip with `nightly-scope` and the install is
pending.

The reviews of the first draft also found the opposite error in this
section: three keys were unproven *downward* and one record rule failed
open. They are fixed here and each has a test: scenario 008's key now names
`README.md`, `check-tests.sh` names `rsi/backlog/` and the web tree, the
base record must name exactly the base commit, `--repo` may be a worktree
subdirectory without losing the scenario list, serial scenarios run alone,
and the mode defaults are the policy's. What remains unproven is what the
C14 proof must decide: that no *other* job reads a path its key excludes.
The design no longer claims the selector over-selects in every class; it
over-selects in the journey keys and it under-selected in these three,
which was the bug.

## 8. Approvals and work before a live switch

1. **Owner: ratify the class split** in section 2.3 (`rsi/backlog/` and
   `rsi/scenarios/nightly/` are protected, but the backlog is a named input
   of `check-tests.sh`, not a reason to escalate the whole gate).
2. **Owner: approve the landing-key fix** that excludes
   `rsi/scenarios/nightly/` from the base-key material, with a test. This is
   a change to `scripts/rsi/gate.sh` (protected).
3. **Owner: decide the unsolved result.** Either install the approved
   design-gate-v2 Decision 1 retirement, or extend the accepting gate's seed
   to carry an `unsolved` result whose declared inputs are identical in base
   and candidate. The gate's existing reuse rule already refuses a scenario
   that declares nothing (`# reads:`), so 241 would need that declaration and
   its proof. The strict-cache rows are the cost of doing neither.
4. **Two independent reviews** of the selector, the class split and every
   protected-file diff, then the owner's protected landing path. The first
   pair returned FIX; this revision answers both, and a fresh pair is
   required.
5. **C14 proof per job key**, everywhere: run each job with the excluded
   paths removed and show the result does not change. The three holes the
   reviews found (README, backlog, `web/src`) are closed; the rest of the
   map is still the prototype's word, in both directions.
6. **C15/C16/C19 wiring**: the base test list, the accepting gate's seed of
   the new main's journey passes, and a named key for every fact the
   integrator reads.
7. **Install behind a flag, measure on real landings, then switch.** No live
   `gate.sh` swap in task 490; no check retired; no scenario folded without
   C9's mutant proofs.
8. **If Rust changes are to meet 180 s**, select tests per crate with the
   C14/C15 proofs. That is not in this prototype and is not claimed.

## 9. Open questions

- Should the web class be the conservative journey keys or something narrower?
  The 139 s once quoted is not an option: it is setup 45.0 + C1 85.993 +
  check-web 7.928 = 138.9 s and leaves out check-tests (184.8 s), which the
  `cost_page` test requires for `cost.ts`. The conservative real-tree cost is
  380.3 s under the retirement and 1101.4 s under the default; how far below
  that the class can go is the C14 proof's decision.
- Does carrying an `unsolved` result need the base's two-run proof only, or
  two candidate runs on the new main too? Proposal: the base's two-run
  `unsolved` plus identical declared inputs is enough; a changed input
  invalidates it.
- Which of the 12 non-journey `check-*` scripts are stage-1 self-tests, and
  which read the candidate binary? The prototype conservatively keys them on
  rust + web + scripts.
- The host timing profile must be refreshed when the toolchain or host
  changes. `env` is part of every key, so a stale profile is a stale *hit*,
  not merely a wrong prediction; computing the probe at selection time is
  the C15 wiring's job, and until it exists the profile is an assumption,
  not a measurement.
- The 4-slot assumption is this host's; a smaller host changes every wall
  clock in section 4.

## 10. Evidence

- 243 gate: `ws/gate-logs/01a1fe882ab31b22c5321e4b2fd8/spawn-gate-2401779-18ea16198a19e3c8-0`
- 485 gate: `ws/gate-logs/01a1fec0ab37747fd6452934d5c1/spawn-gate-2503213-18ea19776e34b1e2-1`
- the gate that accepted the 243 base: `ws/gate-logs/01a1f56e8ddb9e523d04842fc720/spawn-gate-3706967-18e98b3e7b4f564c-0`
- `sync/design-gate-v2.md` (approved 2026-11-10, conditions C1-C19)
- `scripts/rsi/gate.sh:3150` (the base cache), `:3211` (`scenarios_key`),
  `:4814` (seeds from the accepting gate), `:4839` (only `solved` is seeded)
