# rsi/gate-impact

The impact-based landing selector: the bounded task-490 prototype for
`docs/gate-impact-policy.md`.

**Dry run only.** `scripts/rsi/gate.sh` does not read this directory, nothing
here is installed, no check is retired, and no cache is written. The live
landing path still runs the full historical suite.

## Run

```sh
python3 rsi/gate-impact/select.py \
  --repo /path/to/missions/spawn.git \
  --base 5e416443d48c5639456ecb377ae4f214ec05c11c \
  --candidate 865cc4df9f88b1c594a78a2aff3a853604930eb0 \
  --profile 243 \
  --base-record rsi/gate-impact/records/base-5e416443.json
```

`--format json` emits the same facts as one object. The selector prints:
policy version and digest, base/candidate/tree, the approval digest, the
changed paths and classes, the content keys (static, rust, web, scripts and
per job), every selected and skipped job with its reason and key, and the
predicted wall clock against the 180 s budget.

Exit codes: `0` a selection; `1` static trust refused the candidate (for
example a `build.rs`); `2` no verdict (unreadable policy, timings, record or
tree, a record for another base, or a selected job with no measured time);
`3` `--require-narrow` was given and the decision is not narrow.

Useful switches:

- `--historical-mode nightly` (default): the proposed design-gate-v2
  Decision 1 retirement of the historical objectives and the 192/209 holds.
- `--historical-mode cache-only`: today's checks with content-addressed
  results only. This is the conservative replay.
- `--unsolved-mode carry` (default) / `run`: whether an unchanged input
  carries the base's recorded `unsolved` result to the new main, or the base's
  two agreeing runs are charged again.
- `--task-scenario 241.sh --task-base-cached [--task-runs 3]`: the task's own
  scenario, its cached base result, and the live P25 run count.

## Files

| file | what it is |
| --- | --- |
| `select.py` | the selector |
| `policy.json` | policy version 1: classes, jobs, C1 set, nightly notes |
| `timings.json` | own times from the 243 and 485 gate logs, serial times, base runs, measured full-gate totals |
| `records/base-*.json` | the accepting gates' candidate passes (the seed), plus the `unsolved` fact for 241 |
| `test_select.py` | 13 self-contained tests on a temporary repository |
| `replay.sh` | the dry-run replay against the local mission mirror |
| `evidence/` | the replay and test output of 2026-11-22 |

## Tests

```sh
python3 rsi/gate-impact/test_select.py
```

They build a temporary repository with the real path shapes, use the real
policy and timings, and check: a narrow nightly change (131.0 s), cache-only
with and without the unsolved carry (131.0 s vs 286.9 s), a Rust change with
its task scenario (over budget), a web change under conservative keys (over
budget), full escalation for a script change, unknown path fail-closed to the
full gate, `build.rs` rejection, unknown policy version, missing timing, a
base record for another commit, the approval digest bound to the candidate,
and an empty diff.

## What it does not do

- Run any job, build anything, or write a cache entry.
- Decide that a scenario may be retired or folded (C9).
- Narrow on model text; only git paths and content hashes select.
- Replace `gate.sh`, change the controller, or touch a running gate/nightly.
