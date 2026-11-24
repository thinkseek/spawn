# Onboarding exams

The offline, deterministic exam suite `spawn grade exam` runs before a
graded preset is routed any task (docs/agent-grading.md). One directory per
case, named `l<n>-<name>`: `l<n>` (`l1` to `l3`) is the grade the case
examines.

Each case holds:

- `spec.md`: the task, as given to the preset under exam. It names the one
  file the preset must write into its workspace.
- `check.sh`: the machine check. `sh check.sh <workspace>` exits 0 when the
  workspace passes, 1 when it fails, 2 on a harness fault. It runs under
  `LC_ALL=C` and touches no network.
- `reference/`: a passing workspace, so the suite tests its own checks
  (spawn-cli `grading::tests`).

## Host tools

`spawn grade exam` runs each check as `sh check.sh <workspace>` with an
empty environment but `PATH=/usr/local/bin:/usr/bin:/bin` and `LC_ALL=C`.
The checks use a POSIX `sh` and its builtins (`printf`, `cd`, `pwd`, `[`,
`command`, `trap`) and `mktemp`, `rm`, `cat`, `cmp`, `diff`, `head`,
`basename` and `dirname`; `timeout` when the host has it (GNU coreutils:
most Linux hosts, not macOS). The candidate scripts may use what their
`spec.md` names: `awk`, `sed`, `sort` and `tr`. The references use only
those.

Every check is bounded in Rust as well: at most 30 s each, in a process
group of its own that is killed whole at the end, on a host with `timeout`
or without. A check that exits 2, dies by a signal or runs past 30 s is a
harness fault, never the preset's failure: the exam stops with an error and
prints no results, so nothing is onboarded on it.

## Phase 1 caveats

- **Not sandboxed.** A check runs the candidate's script on the host, as
  the user running `spawn grade exam`, with that user's files and network.
  Run it as a user and on a host where that is acceptable, on workspaces
  you have read.
- **The exam must not see the answers.** The expected outputs live in this
  directory (the `check.sh` heredocs, `l3-toposort/cases/*.out`) and the
  references in `reference/`. The workspace the preset writes its answer
  in must not be able to read `evals/`: give it a fresh directory outside
  the Spawn tree, holding only the case's `spec.md`.
- One short exercise per grade is thin evidence, and fixed public cases can
  be memorised: the onboarded grade is a floor for routing, and calibration
  moves it on real outcomes.

## How a preset is examined

1. For each case, give the preset `spec.md` in a fresh workspace
   `<dir>/<case_id>/` outside the Spawn tree, and let it write its answer
   there.
2. From the root of a Spawn tree (the suite's default path,
   `evals/onboarding`, is relative), or with `--suite <dir>`:
   `spawn grade exam --workspaces <dir> > results.json`
3. `spawn grade onboard <preset_id> --results results.json`

The preset is graded the highest grade every case of which, and of every
grade below it, passed; without `l1` it is disabled.
