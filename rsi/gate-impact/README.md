# rsi/gate-impact

The impact-based landing selector: the bounded task-490 prototype for
`docs/gate-impact-policy.md`, revised to answer its two independent reviews.

**Dry run only.** `scripts/rsi/gate.sh` does not read this directory, nothing
here is installed, no check is retired, and no cache is written. The live
landing path still runs the full historical suite.

## Run

```sh
python3 rsi/gate-impact/gate_impact.py \
  --repo /path/to/missions/spawn.git \
  --base 5e416443d48c5639456ecb377ae4f214ec05c11c \
  --candidate 865cc4df9f88b1c594a78a2aff3a853604930eb0 \
  --profile 243 \
  --base-record rsi/gate-impact/records/base-5e416443.json
```

The module is `gate_impact.py`, not `select.py`: a file named `select.py`
first on `sys.path` shadows the standard library's `select` module on hosts
where that module is an extension (macOS), and every git call then dies
inside `subprocess`. Plain `python3` works as written; no `-I` is needed.

`--format json` emits the same facts as one object. The selector prints:
policy version and digest, base/candidate/tree, the approval digest, the
changed paths and classes, the content keys (static, rust, web, scripts,
backlog, readme and per job), every selected and skipped job with its reason
and key, and the predicted wall clock against the 180 s budget.

Exit codes: `0` a selection; `1` static trust refused the candidate (for
example a `build.rs`); `2` no verdict (unreadable or malformed policy,
timings, record or tree, a record that does not name exactly the base
commit, a base the candidate does not descend from, a negative slot count,
a missing record file, or a selected job with no measured time); `3`
`--require-narrow` was given and the decision is not narrow.

Useful switches:

- `--historical-mode cache-only` (the policy's default) / `nightly`: nightly
  is the design-gate-v2 Decision 1 retirement of the historical objectives
  and the 192/209 holds — approved but not installed — and is never chosen by
  default; it needs the owner's switch. Cache-only is today's checks with
  content-addressed results only.
- `--unsolved-mode run` (the policy's default) / `carry`: run charges the
  base's two agreeing runs for an unchanged, unsolved objective; carry
  reuses the base's recorded `unsolved` fact, which is a new reuse rule and
  needs the owner's decision.
- `--task-scenario 241.sh --task-base-cached [--task-runs 3]`: the task's own
  scenario, its cached base result, and the live P25 run count.
  `--task-base-cached` is the caller's word, not a verified fact: it drops
  the task's base run from the prediction and the selector does not check
  that the base recorded a cached result, so a base with no cached run is
  understated by one task run (39.9 s for 241).

## Files

| file | what it is |
| --- | --- |
| `gate_impact.py` | the selector |
| `policy.json` | policy version 1: classes, jobs, C1 set, scenario inputs, serial list, mode defaults, nightly notes |
| `timings.json` | own times from the 243 and 485 gate logs, serial times, base runs, measured full-gate totals |
| `records/base-*.json` | the accepting gates' candidate passes (the seed), plus the `unsolved` fact for 241 — transcribed by hand from the two gate logs, because the gate seeds only solved objectives today (`scripts/rsi/gate.sh:4839`), so the check, journey and CI entries are hand entries |
| `test_gate_impact.py` | 26 self-contained tests on a temporary repository |
| `replay.sh` | the dry-run replay against the local mission mirror |
| `evidence/` | the replay and test output of 2026-11-22 |

## Tests

```sh
python3 rsi/gate-impact/test_gate_impact.py
```

They build a temporary repository with the real path shapes, use the real
policy and timings, and check: a narrow nightly change (131.0 s under the
retirement/carry; 286.9 s, OVER, under the policy's own defaults), cache-only
with and without the unsolved carry, the policy's own defaults, a README
edit selecting scenario 008, a backlog edit selecting check-tests, a
`web/src` edit selecting check-tests, the serial scenario charged alone, a
Rust change with its task scenario (over budget), full escalation for a
script change, unknown path fail-closed to the full gate, nested
`build.rs`/`.cargo` and a proc-macro with a trailing comment rejected,
protected prefixes, a subdirectory `--repo`, a base that is not an ancestor,
unknown policy version, malformed policy, negative slots, missing timing,
missing/empty/short/wrong base record, a cache miss with no record (no
result, no skip), the approval digest bound to the candidate, and an empty
diff.

## What it does not do

- Run any job, build anything, or write a cache entry.
- Decide that a scenario may be retired or folded (C9).
- Narrow on model text; only git paths and content hashes select.
- Replace `gate.sh`, change the controller, or touch a running gate/nightly.

## What the numbers mean

The 131.0 s row is a change that touches only a nightly data file, and it is
a prediction, not an achieved landing: the gate seeds only solved objectives
today, so hits on checks, journeys and CI jobs depend on the pending
C15/C16/C19 wiring. It is also the retirement/carry number only: under the
policy's own default (cache-only, unsolved run) the same change predicts
**286.9 s and is OVER**, because the base re-judges the unsolved 241 and 241
runs on the candidate. The measured 243 change also edited
`rsi/backlog/243.json`, and `cargo test` reads the backlog, so
`check-tests.sh` (184.8 s) runs and the honest 243-type number is 349.2 s
under the policy's own default (cache-only, unsolved run), or 229.8 s only
with the approved-but-not-installed nightly retirement or the unratified
carried `unsolved`; both are over budget.
`docs/gate-impact-policy.md` section 4 carries the full table.
