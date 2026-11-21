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

## 2. The policy

### 2.1 Mandatory core, on every landing

- The static trust checks keep running: protected paths (the controller's
  `rsi/scenarios/protected`, never the candidate's copy, matched by the
  gate's string-prefix rule), no build-time code (`build.rs`, `.cargo/`, a
  `Cargo.toml` with `build =`, a new proc-macro), and the test count from
  the base's cached `cargo test` list (C15).
- A versioned evidence record is written: policy version and digest, base
  and candidate commits and tree, toolchain and environment probe, the
  selected jobs and the skipped jobs with a deterministic reason.
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
was), but they are not landing-judge inputs: the gate never reads them. They
must not escalate. The prototype keeps them in the review flag and out of
the escalation reason. If the owner rejects that split, every 243-type
change pays the full gate and this proposal has no narrow class.

### 2.4 Cache

- A key is a content hash of what a job reads, computed on both trees: the
  Rust tree (`crates/`, `Cargo.toml`, `Cargo.lock`), the web tree, the
  scripts tree, the scenario blob, the toolchain/environment probe, and the
  policy version.
- A result is reused only when the candidate's key equals the base's key and
  the base's accepting gate recorded the result. The accepting gate's own
  candidate runs seed the new main (C16; the gate already does this for
  solved objectives, `scripts/rsi/gate.sh:4814`).
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
reused. The difference is what the retirement buys for a code change; it
buys nothing for a 243-type change, whose results are all cache hits anyway.

Nothing is hidden outside the clock: section 7 lists exactly what the landing
does not prove and the nightly owner of each such fact.

### 2.6 The 180 s target

The target covers selection, the necessary build/listing, and every check the
policy requires for that change class. It does not apply to the full or
escalated gate and it does not apply to nightly. There is no universal
3-minute SLA: the numbers in section 4 show three classes that cannot meet
it under today's checks.

## 3. Change classes

| class | paths | landing jobs |
| --- | --- | --- |
| nightly-only / data | `rsi/backlog/`, `rsi/scenarios/nightly/` | static core + C1; the changed nightly scenario is proven by the nightly |
| docs | `docs/`, `README*` | static core + C1 |
| web | `web/src/`, `web/test/`, `web/index.html` | static core + `check-web.sh` + C1 + journeys whose key changed |
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
| 243 `5e416443..865cc4df` | proposed (historical nightly, unsolved carried) | 4 | **131.0 s** | WITHIN |
| 243 | cache-only + carry (no check retired) | 4 | **131.0 s** | WITHIN |
| 243 | strict cache, unsolved not carried | 5 | 286.9 s | OVER |
| 485 `865cc4df..33777df8` | proposed + task 241 | 29 | 479.1 s | OVER |
| 485 | today's checks + cache | 187 | 1221.2 s | OVER |
| web-only (unit fixture) | conservative journey keys | 13 | 257.6 s | OVER |
| shared `337c94b5..2e8781ca` | full escalation | 186 | 1973.8 s | OVER |
| unknown path (unit fixture) | fail-closed | all | 1973.8 s | OVER |
| `build.rs` (unit fixture) | static trust | - | - | reject, exit 1 |
| missing timing/policy/record | no verdict | - | - | exit 2 |

How the 131.0 s is made:

- setup 45 s (measured, includes the static checks);
- the four C1 journeys, longest-first on four slots: 85.993 s wall; their own
  time is 305.3 s, so the slots matter (run serially, 350 s);
- nothing else runs: 183 results are cache hits from the base's accepting
  gate - the build and test listing, the five CI jobs, the eight non-C1
  journeys, twelve non-journey checks and 158 historical objective results.
  Their Rust and web inputs are byte-identical and the base recorded a pass.

The table's 243 rows are the same landing under three policies. **No check is
retired in row 2**: all 186 skipped jobs are skipped because their inputs are
byte-identical and the base's accepting gate recorded a pass, not because a
policy removed them. Row 3 is the price of the one unsolved objective on
main (241): with no recorded pass and no carried `unsolved` fact, a strict
cache re-judges it on the base (two agreeing runs, 119.4 s measured) and on
the candidate (59.2 s). That is the difference between 131.0 s and 286.9 s.

The 485 rows show the floor for a Rust change: build 20.3 + listing 73.9
(serial), check-tests 187.8, clippy 30.4, every journey whose Rust input
changed, and task 241 at 39.9. Even with historical objectives nightly it is
479.1 s. Under today's checks it is 1221.2 s.

The web row is deliberately conservative: the prototype keys every journey
on rust + web + scripts, so a `web/src/` change re-runs the eight non-C1
journeys too. Narrowing that needs the per-journey `depends_on` proof (C14);
until then the selector over-selects, which is the safe direction.

Assumptions, all printed by the selector:

- the base record is the accepting gate's candidate passes for the base
  commit, seeded into the new main as the gate already does for solved
  objectives (C16); candidate runs of a later tree are never read;
- four scenario slots, longest-first (the gate's order); setup includes the
  static checks; no host noise;
- the timings are one measured run each (243 and 485) on this host; a
  different host or toolchain needs its own profile, and a missing time is
  no verdict.

## 5. The prototype

`rsi/gate-impact/` is bounded and read-only:

- `select.py` — the selector. Given `--repo`, `--base`, `--candidate`, it
  prints the policy version and digest, the changed paths and classes, the
  content keys (static, rust, web, scripts, per job), every selected and
  skipped job with a reason and a key, the approval digest, and the
  predicted wall clock against 180 s. Exit codes: 0 selection, 1 static
  reject, 2 no verdict, 3 `--require-narrow` not met.
- `policy.json` — policy version 1: the classes, jobs, C1 set, nightly notes
  and the non-goals.
- `timings.json` — the measured own times from the 243 and 485 gate logs, the
  serial build/listing times, the base-run times, and the measured full-gate
  totals.
- `records/base-5e416443.json`, `records/base-865cc4df.json` — the accepting
  gates' candidate passes (the seed), plus the `unsolved` fact for 241.
- `test_select.py` — 13 tests on a temporary repository: narrow nightly, web
  and rust classes, carry/run, full escalation, unknown path, build.rs,
  unknown policy version, missing timing, wrong base record, the approval
  digest, and a no-change diff.
- `replay.sh`, `evidence/` — the runs in section 4.

It runs no check, builds nothing, writes no cache, and is not wired into
`gate.sh` or `deploy/`.

## 6. What the dry run confirms and what it refutes

Confirmed:

- A 243-type change lands in **131.0 s** under the proposed policy, and in
  131.0 s under cache-only with the base's results intact. It is 186 skipped
  jobs, four C1 journeys, and no retired check in the cache-only row.
- The specific cause of the 1973 s 243 landing is the nightly file in the
  landing cache key, not the number of scenarios.
- Unknown impact and a missing dependency fail closed: full gate or no
  verdict, never a narrow accept.

Refuted:

- "Cache alone is enough." Strict cache-only with the one unsolved objective
  on main costs 286.9 s, over the budget. The 131 s needs either the
  approved retirement (row 1) or a carried `unsolved` result (row 2).
- "A Rust change fits in 180 s." It does not: 479.1 s under the proposed
  policy, 1221.2 s under today's checks. check-tests alone is 184.8 s.
- "A web change fits under conservative keys." It is 257.6 s until the
  per-journey keys are proven.
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

## 8. Approvals and work before a live switch

1. **Owner: ratify the class split** in section 2.3 (`rsi/backlog/` and
   `rsi/scenarios/nightly/` are protected but not judge inputs).
2. **Owner: approve the landing-key fix** that excludes
   `rsi/scenarios/nightly/` from the base-key material, with a test. This is
   a change to `scripts/rsi/gate.sh` (protected).
3. **Owner: decide the unsolved result.** Either install the approved
   design-gate-v2 Decision 1 retirement (row 1), or extend the accepting
   gate's seed to carry an `unsolved` result whose declared inputs are
   identical in base and candidate. The gate's existing reuse rule already
   refuses a scenario that declares nothing (`# reads:`), so 241 would need
   that declaration and its proof. Row 3 is the cost of doing neither.
4. **Two independent reviews** of the selector, the class split and every
   protected-file diff, then the owner's protected landing path.
5. **C14 proof per job key**, especially the journeys and the non-journey
   checks: run the job with the excluded paths removed and show the result
   does not change. Until then the selector over-selects.
6. **C15/C16/C19 wiring**: the base test list, the accepting gate's seed of
   the new main's journey passes, and a named key for every fact the
   integrator reads.
7. **Install behind a flag, measure on real landings, then switch.** No live
   `gate.sh` swap in task 490; no check retired; no scenario folded without
   C9's mutant proofs.
8. **If Rust changes are to meet 180 s**, select tests per crate with the
   C14/C15 proofs. That is not in this prototype and is not claimed.

## 9. Open questions

- Should the web class be 139 s (per-journey keys) or 257.6 s (conservative)?
  Only the C14 proof decides.
- Does carrying an `unsolved` result need the base's two-run proof only, or
  two candidate runs on the new main too? Proposal: the base's two-run
  `unsolved` plus identical declared inputs is enough; a changed input
  invalidates it.
- Which of the 12 non-journey `check-*` scripts are stage-1 self-tests, and
  which read the candidate binary? The prototype conservatively keys them on
  rust + web + scripts.
- The host timing profile must be refreshed when the toolchain or host
  changes; a stale profile is a wrong prediction, not a wrong verdict.
- The 4-slot assumption is this host's; a smaller host changes every wall
  clock in section 4.

## 10. Evidence

- 243 gate: `ws/gate-logs/01a1fe882ab31b22c5321e4b2fd8/spawn-gate-2401779-18ea16198a19e3c8-0`
- 485 gate: `ws/gate-logs/01a1fec0ab37747fd6452934d5c1/spawn-gate-2503213-18ea19776e34b1e2-1`
- the gate that accepted the 243 base: `ws/gate-logs/01a1f56e8ddb9e523d04842fc720/spawn-gate-3706967-18e98b3e7b4f564c-0`
- `sync/design-gate-v2.md` (approved 2026-11-10, conditions C1-C19)
- `scripts/rsi/gate.sh:3150` (the base cache), `:3211` (`scenarios_key`),
  `:4814` (seeds from the accepting gate), `:4839` (only `solved` is seeded)
