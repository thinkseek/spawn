#!/bin/sh
# The gate: one black-box judge for every landing, RSI or fleet
# (docs/fleet-architecture.md §10.2).
#
# usage: gate.sh <bare-repo> <base-commit> <candidate-commit> <scenarios-dir> [<scenario>]
#        gate.sh --base-only <bare-repo> <base-commit> <scenarios-dir>
#
# Exports both commits to temporary directories and judges the candidate
# with the controller's scenarios, never the trees' own: a candidate cannot
# weaken the test that judges it.
#
# - The candidate may not change a path listed in `<scenarios-dir>/protected`
#   (one prefix per line, `#` comments, `!<prefix>` for an exception): the
#   controller's tasks, scenarios, gate and black-box tests, and the UI's
#   build configuration (all of `web/` but `web/src/` and `web/index.html`). Paths are compared as git stores them (no
#   rename detection, no quoting); a path with a newline is refused, and so
#   is every candidate when the list is missing (an empty list protects
#   nothing on purpose).
# - It may not change anything under a `node_modules/` or `target/`
#   directory.
# - It may not add build-time code: a changed `build.rs` or `.cargo/`, a
#   `build =` key in any Cargo.toml of the tree (cargo would run the file it
#   names as a build script), a workspace member with a build script or a
#   proc-macro target, or a local crate outside the workspace with either
#   that is new or whose directory the diff touches.
# - `<scenarios-dir>/build`, when present, builds each tree (run inside it).
# - With a task scenario: it must fail on the base (exit exactly 1) and pass
#   on the candidate.
# - Every other scenario in the directory must pass on the candidate, except
#   ratchet scenarios (a line `# rsi: ratchet`), which compare against the
#   base and so fail on any tree that did not improve, and objective
#   scenarios (`<digits>.sh`) the base fails too: an objective nobody has
#   solved yet is not a regression, and must not block unrelated work.
#   `check-*` scenarios are invariants and always must pass. A check that
#   fails on the candidate is run again there; one that fails twice is run
#   on the base: passing there, it is a regression (rejected); failing there
#   too, it is no verdict on the candidate, and is run on the base once
#   more: two base runs that exit 1 with the same failing tests (the
#   `FAILED <test>:` lines check-tests.sh prints) say main is red on the
#   check (main_red: exit 6 with `on` `main` and the failures); anything
#   else is a host or harness fault (exit 6, `on` `candidate and base`,
#   below). One that passes the second time is
#   a flake, said in the log, and the gate goes on ("Checks run twice"
#   below).
#
# A scenario that cannot be judged here. Some objectives judge the host
# itself (029: every execution and gate in a systemd user scope with memory
# and CPU limits), and the gate's sandbox has no systemd user manager and no
# delegated cgroup (the owner's ruling, 2026-11-06). Such a scenario exits 6
# and says why (029's "cannot be judged here: ..."). Exit 6 from the
# scenario is not the harness error above: it says the host cannot judge that
# scenario at all, the same on every run, and the gate reads it as a fact
# about the host, never as a result:
# - on the base, the result is `cannot_judge`, recorded in the base cache
#   like solved and unsolved (one run: the host says the same every time); a
#   gate on a tree for another objective is therefore not stopped by it
#   (029 is a regression in every Spawn gate's set);
# - on the candidate, a regression whose scenario exits 6 is skipped
#   ("| <name> cannot be judged here: not a regression"), and a *task*
#   scenario that exits 6 makes the gate write {"cannot_judge":true,
#   "scenario":"<name>"} to SPAWN_GATE_VERDICT and exit 6: no verdict. The
#   integrator records `host_fault` with the evidence `cannot_judge` (beside
#   the gate's own `host_fault` fact), retried after the backoff and counted
#   toward no halt: nothing is rejected or aborted. Which objectives the
#   gate cannot judge: deploy/systemd/README.md "Which objectives the gate
#   cannot judge" lists them, 029 and why.
#
# Exit-code contract for objective scenarios and the task scenario: 0 =
# pass, 1 = an assertion failed, 6 = the host cannot judge it here (below: "A
# scenario that cannot be judged here"), anything else = a harness error (a
# missing tool, an unreachable database, a timeout kill). Each is judged on
# the base BEFORE any candidate code runs, since both share the test server
# and a candidate could otherwise sabotage the base's run: 0 = solved, 1
# (twice, from two runs that agree) = unsolved, 6 = cannot_judge (cached: the
# host's answer does not change), anything else is a host fault (exit 6 with
# its fact in the verdict file, no verdict: the integrator records
# `host_fault`, retried after a backoff and counted toward no halt, never a
# skip). The one other shape is objective 033's: a first run that exits 1
# and a second that exits 0 (a base that disagrees with itself) keeps the
# gate's own pair of lines and exit 4, which the integrator records as
# `base_disagrees`, also a host fault counted toward no halt; the pair and
# exit 4 are 033's statement of it, one run is one record, never the host
# fact too. Apart from that pair, exit 4 is left for a run the gate itself
# lost (ended without a result). The base's
# runs all end before the candidate builds; within each phase, scenarios run
# SPAWN_GATE_JOBS at a time, the log and the verdict still the serial
# order's ("Parallel runs" below). Results are
# cached in ${XDG_CACHE_HOME:-~/.cache}/spawn/gate-base, keyed by everything
# that decides them (the base commit, the scenario's blob, this file's, the
# toolchain, and the files the scenario names: "The base cache" below),
# written outside the sandbox and hidden from it (a tmpfs over it, and the
# gate refuses to run when the directory lies under anything the sandbox
# binds back); the base moves only on landings, so most runs build and run
# nothing on the base. The base is built only on a cache miss.
#
# The cache's limit is the same-uid limit: candidate code in the sandbox
# can neither see nor write it, but any process outside the sandbox that
# runs as this uid can (on a host without containers, every agent). One that
# writes "unsolved" for an objective under the current key lets a candidate
# that breaks that objective land. Closing that needs the gate under a uid
# agents do not have.
#
# A tree that is not Spawn names its owner-installed inputs (dependencies the
# sandbox cannot download) in `<scenarios-dir>/bind`: absolute directories
# under $HOME, bound read-only at the same path.
#
# Scenarios run inside the tree with SPAWN_BIN (the tree's `spawn`, when the
# build made one) and SPAWN_BASELINE_TREE set. Their output is printed with a
# `| ` prefix. The verdict is one JSON line, last, on stdout; when the
# candidate's task run printed exactly one `METRIC` line on its stdout, the
# verdict carries it as "metric" (the controller reads the metric from the
# verdict only).
#
# The sandbox. Everything the candidate controls runs inside bubblewrap
# (`bwrap`, required): the build hook, the scenarios and the checks, and
# `cargo metadata` on the candidate's manifests. Inside it the root is
# read-only, /tmp is private, the PID namespace is new (the gate and the
# integrator are not in /proc), the tree under test is the only writable
# path, the base tree is read-only, and /run (the user's runtime directory,
# the docker socket, the system bus), /var/tmp and the host's TMPDIR are
# empty: no Unix socket of the host is reachable but the test database's
# (see "Unix sockets" at `sandbox` below). $HOME is an empty tmpfs
# with an allow-list bound back read-only (see `home_binds` below): the
# Rust toolchain, the install root of node/npm/npx when it lives there,
# and the controller's `scripts` and `web/node_modules` (the controller is
# the directory two levels above the scenarios). The network namespace is
# new: loopback only, so no host service (the live database, serve, the Jev
# relay) is reachable. The checks reach the test database through one Unix
# socket: its own socket file when SPAWN_TEST_DATABASE_URL names a socket
# directory, or, for a TCP URL (127.0.0.1, localhost, [::1], a name), a
# socket the gate forwards to that host and port; inside, the URL names
# that socket. The environment is cleared down to the list below. Cargo
# gets a private CARGO_HOME whose registry and git caches are the real
# ones, read-only, and runs offline: a candidate cannot fetch a new crate or
# rewrite a cached one.
# Git reads the gate's own config (an identity, no signing), not the
# owner's.
#
# The candidate's own tests and lints judge it too (check-tests,
# check-clippy), so it may not weaken them:
# - Tests: the number of tests cargo runs may not fall from base to
#   candidate. After the candidate's build, inside the sandbox, each tree
#   runs `cargo test --workspace -- --list --format terse`, and the same
#   with `--ignored`; runnable = listed minus ignored. A test deleted,
#   ignored, commented out, in a file no `mod` includes, or behind
#   `#[cfg(any())]` is not listed. Every runnable test name of the base must
#   be runnable on the candidate too: a renamed test, or one moved to
#   another module path, is refused even when the count holds. A test the
#   diff adds or edits may not have an empty body (`{}`, or only comments,
#   `;` and `()`). The listing comes from the trees' own test binaries, so
#   code that forges libtest's output at start-up could inflate it, and a
#   body that asserts nothing real (`assert!(true)`) is not empty text: this
#   stops weakening, not a forger.
#   A test the owner retired with its feature is named in
#   `<scenarios-dir>/retired-tests` and may go.
# - Lint configuration (text, before anything runs): a change to a
#   `[lints]` / `[workspace.lints]` table (or `lints.` keys) in any
#   Cargo.toml, any change to clippy.toml or .clippy.toml, a new crate-level
#   `#![allow(..)]` / `#![expect(..)]` (or one in `cfg_attr`), or a removed
#   `#![warn|deny|forbid(..)]` is refused. An item-level `#[allow]` is a
#   reviewer's job. A crate new in the candidate (a Cargo.toml with a
#   [package] table the base does not have) must inherit the workspace's
#   lints, `[lints]` with `workspace = true` and nothing else (or the dotted
#   `lints.workspace = true`, or `lints = { workspace = true }`): one without
#   it would be linted by nothing.
#
# Environment (the integrator passes these and nothing else, gate.rs):
#   PATH, HOME, LANG, TMPDIR   as usual
#   CARGO_HOME, RUSTUP_HOME    when the toolchain is not under $HOME
#   SPAWN_TEST_DATABASE_URL    the throwaway database the checks may use: a
#                              socket URL (postgres://u:p@%2Fdir/db, or
#                              ?host=/dir) or a TCP one, read by
#                              scripts/pg-url.sh
#   SPAWN_GATE_JOBS            scenarios run at once; spawn integrate always
#                              sets it (its share of the cores, gate.rs Cpu);
#                              by hand: default min(8, half the cores), 1 on
#                              the shared test server
#   SPAWN_GATE_HOST_JOBS       the host's scenario budget, all gates together
#                              ("Host scenario slots" below); default: the
#                              value every gate is given (SPAWN_GATE_JOBS)
#                              when it is set, else the same default one
#                              gate takes (min(8, half the cores))
#   SPAWN_GATE_SLOT_WAIT_SECS  how long one scenario run may wait for a slot
#                              before the gate says it had no room and exits 5
#                              (whole seconds from 1; default 1800, 30 min).
#                              A seam for the tests, which cannot wait 30
#                              minutes: spawn integrate does not pass it, and
#                              an owner who sets it is changing the bound
#   SPAWN_GATE_TEST_HOLD       the suite's seam: while this file exists, the
#                              build hook's first sandbox with socket masks
#                              waits between its mask check and bwrap (it creates
#                              <file>.held first); spawn integrate never
#                              passes it
#   SPAWN_GATE_SLOT_PRIORITY   normal or low: who claims a slot first ("Host
#                              scenario slots"); default low for --base-only
#                              (a warm), normal for every other gate
#   SPAWN_GATE_BUILD_JOBS      CARGO_BUILD_JOBS for the build hook and the
#                              test listing, inside the sandbox (whole number
#                              from 1); unset by default (cargo's own, every
#                              core), and 1 for --base-only, whatever is set
#   SPAWN_GATE_BASE_CHECK      a check's file name (check-<name>.sh, one of
#                              the scenarios): run on the base before the
#                              candidate, the main a source was red on
#                              ("main is red", the integrator sets it): its
#                              verdict says `base_check` when it passes, and
#                              two runs that fail alike are main_red (exit 6)
#   SPAWN_GATE_RED_UNSOLVED    1: a check main is red on (it fails twice on
#                              the base with the same failing tests) is
#                              unsolved on the base, not a regression and no
#                              stop, while the candidate's runs fail only
#                              tests main's red runs fail: the gate says so
#                              and judges the rest. A test the candidate
#                              newly fails in it, or a run naming no failed
#                              test, is a regression (new_failures).
#                              The integrator sets it for an owner's gated
#                              push only (design-owner-push-between-gates.md):
#                              the regression rule holds for what the base
#                              solves, and an owner's push during a red main
#                              must not go ungated for it
#   SPAWN_GATE_PG_IMAGE        the test server's docker image: the gate runs a
#                              server of its own from it (GitHub issue #6),
#                              and SPAWN_TEST_DATABASE_URL is not used
#   SPAWN_GATE_PG_BIN          or, on a host without docker, the directory of
#                              the test server's programs (postgres, initdb;
#                              /usr/lib/postgresql/16/bin): the same, from
#                              those
#   SPAWN_GATE_VERDICT         a file in a directory the integrator owns; the
#                              verdict is written there too, and the
#                              integrator believes that file only, and only
#                              when the gate exits 0. The directory is hidden
#                              from the sandbox.
#
# Base only (`--base-only`): the base phase alone, for the base cache: the
# results the next gates on that base take from it. deploy/deploy.sh starts
# one in the background once it moved main, and spawn integrate after each
# landing and each owner's landing (`source.moved`). It is a warm: one job,
# CARGO_BUILD_JOBS=1, and the last to claim a host scenario slot, so its
# footprint is at most one slot no gate is waiting for and one compiler job. Exit 0 with no verdict (none is
# written, whatever SPAWN_GATE_VERDICT says), 4 on a harness error, 3 as
# below.
#
# Exit status: 0 with a verdict. 5 means no room to judge now: TMPDIR has
# less than SPAWN_GATE_MIN_FREE_GB free (scripts/disk.sh gate_room). The
# gate exits before it makes any scratch or runs any candidate code, after
# it writes one JSON line to SPAWN_GATE_VERDICT:
#   {"no_room":true,"free_kb":<n>,"min_gb":<n>,"tmpdir":"<dir>"}
# When that write fails (no room at all), it still exits 5. The integrator
# records `no_room` from that exit status with that file, or, with no
# readable file, from its own measure of TMPDIR under the floor (gate.rs). It is no verdict and does not count toward a mission's halt.
# Any other status is an infrastructure failure (no bwrap, a git error, a
# missing bind directory, a bad SPAWN_GATE_MIN_FREE_GB, a killed gate), which
# the integrator records as an abort that counts. A gate whose own tree is
# under /tmp, /run or /var/tmp — itself or below it — or under the host's
# TMPDIR when that is not /tmp refuses up front (exit 3, below): the sandbox
# empties those, so it could not read its own scripts there and would
# compute base keys no other host matches — silently.

set -eu

# A gate whose own tree the sandbox empties cannot judge: toolchain() reads
# scripts/pg-url.sh through `$(dirname "$0")/..` inside the sandbox, and from
# a root the sandbox empties there it finds nothing. Those roots are /tmp,
# /run and /var/tmp, and the host's TMPDIR when that is not /tmp
# (`sandbox()` below masks it whole). A tree that *is* such a root is in it
# as much as one under it. The toolchain block then says "server:
# unreadable", the base cache key is like no other host's, and every
# scenario runs again in every gate, silently (measured 2026-11-08: the same
# base from /var/tmp gave 30 of 30 different keys, every tool version
# identical; deploy/gate-runner/setup.sh refuses a runner root the same
# way). Refuse up front, exit 3, naming the root and the reason — and no
# advice about where to run it instead: where a gate may run from is the
# caller's to know.
gate_root=$(cd "$(dirname "$0")/../.." && pwd) || {
    echo "gate: cannot resolve the directory of $0" >&2
    exit 3
}
masked_root() {
    printf 'gate: refusing to run from %s: %s; the gate could not read its own scripts there (scripts/pg-url.sh) inside the sandbox, so its toolchain block would say "server: unreadable" and every base key it computed would be one no other host matches\n' "$gate_root" "$1" >&2
    exit 3
}
case "$gate_root" in
/tmp | /tmp/* | /run | /run/* | /var/run | /var/run/* | /var/tmp | /var/tmp/*)
    masked_root "it is under /tmp, /run or /var/tmp, roots the sandbox empties"
    ;;
esac
# The host's TMPDIR is emptied too when it is /tmp's job to be the gateway's
# scratch: `sandbox()` masks it whole unless it is /tmp (or /).
case "${TMPDIR-}" in
'' | / | /tmp | /tmp/) ;;
*)
    tmpdir_root=$(cd "$TMPDIR" 2>/dev/null && pwd) || tmpdir_root=$TMPDIR
    case "$gate_root" in
    "$tmpdir_root" | "$tmpdir_root"/*)
        masked_root "it is under the host's TMPDIR ($tmpdir_root), which the sandbox empties too (TMPDIR is not /tmp)"
        ;;
    esac
    ;;
esac

# Base-only (`gate.sh --base-only <bare-repo> <base-commit> <scenarios-dir>`):
# the base phase alone, for the base cache (see "Base only" below). The
# candidate is the base itself, so every check of the diff passes on an
# empty one, and no verdict is written.
base_only=""
if [ "${1-}" = --base-only ]; then
    shift
    if [ $# -ne 3 ]; then
        echo "usage: gate.sh --base-only <bare-repo> <base-commit> <scenarios-dir>" >&2
        exit 3
    fi
    base_only=1
    set -- "$1" "$2" "$2" "$3"
    unset SPAWN_GATE_VERDICT
fi

repo=$1
baseline=$2
candidate=$3
scenarios=$4
scenario=${5-}

if ! command -v bwrap >/dev/null 2>&1; then
    echo "gate: bwrap is required to run candidate code" >&2
    exit 3
fi

. "$(dirname "$0")/../disk.sh"
room=0
gate_room || room=$?
case $room in
0) ;;
2)
    # No room to judge now: a fact, written where only the integrator reads
    # it (no candidate code has run), and a status of its own. A disk with
    # no room at all may refuse the write: the status stays 5, and the
    # integrator measures the free space itself.
    no_room_json=$(printf '{"no_room":true,"free_kb":%s,"min_gb":%s,"tmpdir":"%s"}' \
        "$disk_free" "$disk_min_gb" \
        "$(printf '%s' "$disk_dir" | tr -d '\000-\037' | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')")
    if [ -n "${SPAWN_GATE_VERDICT-}" ]; then
        printf '%s\n' "$no_room_json" >"$SPAWN_GATE_VERDICT" 2>/dev/null || :
    fi
    # Printed too: a full disk may refuse the file, and the integrator then
    # reads the last line here before measuring TMPDIR itself.
    printf '%s\n' "$no_room_json"
    exit 5
    ;;
*) exit 3 ;;
esac
work=$(gate_scratch)
# Stage timing in the format every suite shares (scripts/stage.sh): one
# `| stage gate <result> <ms>ms <name>` line as each section ends, and a
# total with the verdict.
STAGE_PREFIX="| "
# shellcheck source=scripts/stage.sh
. "$(dirname "$0")/../stage.sh"
gate_started=$(stage_now)
# Where the time goes, said once at the end (`gate_times`): the base's
# stages (its build and scenario runs, all skipped on a cache hit), the
# candidate's (its build, the test listing of both trees, its scenarios),
# and the setup: the time before the first stage or phase (exports, the
# socket scan, and the static checks, which all come before it: the
# build-time code check, lint_config, crate-level lints). Each is its own
# measure, so the three need not add up to the total. Time between stages
# that belongs to neither phase (the base results read from the cache,
# for one) is in the total only.
# stage_emit is stage.sh's, with the sum kept.
base_ms=0
candidate_ms=0
base_runs=0
stage_emit() { # <suite> <result> <ms> <name>
    case "$4" in
    total) ;;
    "base: build") base_ms=$((base_ms + $3)) ;;
    base:*) base_ms=$((base_ms + $3)) base_runs=$((base_runs + 1)) ;;
    *) candidate_ms=$((candidate_ms + $3)) ;;
    esac
    printf '%sstage %s %s %sms %s\n' "${STAGE_PREFIX-}" "$1" "$2" "$3" "$4"
    if [ -n "${SPAWN_STAGE_LOG-}" ]; then
        printf '{"suite":"%s","stage":"%s","result":"%s","ms":%s}\n' \
            "$(stage_json "$1")" "$(stage_json "$4")" "$(stage_json "$2")" "$3" >>"$SPAWN_STAGE_LOG"
    fi
}
times_said=""
gate_times() {
    [ -z "$times_said" ] || return 0
    times_said=1
    # A phase still running (the gate ends inside it: a kill, an exit from
    # a job's printer) is its wall clock so far, never the sum of its
    # overlapping runs' own times.
    if [ "${phase_started:-0}" -gt 0 ]; then phase_time "$phase_open"; fi
    total_ms=$(($(stage_now) - gate_started))
    # Setup is its own measure: the time before the gate's first stage, never
    # what is left of the total once the phases are taken off (they overlap
    # it and each other, which made it negative: "setup -416 s").
    [ -n "${setup_ms-}" ] || setup_ms=$total_ms
    echo "| gate time: base $((base_ms / 1000)) s ($base_runs scenario runs," \
        "$(printf '%s' "${from_cache-}" | wc -w | tr -d ' ') results from the cache)," \
        "candidate $((candidate_ms / 1000)) s, setup $((setup_ms / 1000)) s" \
        "(exports, socket scan, static checks), total $((total_ms / 1000)) s"
    # A sandbox started again after a masked path went away (`sandbox`):
    # said in the gate's own output whatever the command's log shows.
    if [ -s "${work-}/sandbox-retries" ]; then
        sed 's/^/| /' "$work/sandbox-retries"
    fi
}
# section <name>: a gate stage starts (and the one before it ends as ok).
section() {
    # The first stage ends the setup (exports, socket scan, static checks).
    [ -n "${setup_ms-}" ] || setup_ms=$(($(stage_now) - gate_started))
    stage_open gate "$1"
    echo "| == $1"
}
trap 'stage_close aborted; gate_times; rm -rf "$work"' EXIT
# A signal ends the gate through its EXIT trap, so the work directory goes
# too, whatever the shell (bash runs EXIT on a fatal signal, dash does not).
# SIGKILL leaves the directory: spawn-maintenance.timer removes it.
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
# Sandboxed git reads this, never the owner's ~/.gitconfig.
cat >"$work/gitconfig" <<'EOF'
[user]
	name = spawn-gate
	email = gate@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
[tag]
	gpgsign = false
EOF

metric=""
flakes=""

# JSON string body: backslash, quote and tab escaped, newlines as \n, other
# control characters dropped.
json_string() {
    printf '%s' "$1" | tr -d '\000-\010\013-\037' |
        sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e "s/$(printf '\t')/\\\\t/g" |
        awk 'NR > 1 { printf "%s", "\\n" } { printf "%s", $0 }'
}

# verdict <accepted|rejected> [reason]: the verdict names the commits it
# judged, resolved, so a reader can check what it is about (deploy.sh does).
verdict() {
    # A step whose sandbox never started is evidence the gate does not
    # have: no verdict, accepted or rejected, stands on it.
    if [ -z "${sandbox_checked-}" ] && [ -s "$work/sandbox-never-started" ]; then
        sandbox_fault "$work/sandbox-never-started"
    fi
    # The open section passed unless the verdict is a rejection of it.
    if [ "$1" = accepted ]; then stage_close ok; else stage_close "$1"; fi
    gate_times
    stage_emit gate "$1" "$(($(stage_now) - gate_started))" total
    json="{\"verdict\":\"$1\""
    [ -n "${2-}" ] && json="$json,\"reason\":\"$(json_string "$2")\""
    [ -n "$metric" ] && json="$json,\"metric\":\"$(json_string "$metric")\""
    # The checks that failed once and passed again on the candidate (a
    # flake, not a regression), in the log's order.
    [ -n "${flakes-}" ] && json="$json,\"flakes\":[$flakes]"
    # The check main was red on (SPAWN_GATE_BASE_CHECK), when it passed on
    # this base before any candidate code ran: main is green on it again,
    # whatever this gate ends with (gate.rs reads it from any fact).
    [ -n "${base_check_passed-}" ] && json="$json,\"base_check\":\"$base_check_passed\""
    json="$json,\"base\":\"$base_commit\",\"candidate\":\"$candidate_commit\"}"
    if [ -n "${SPAWN_GATE_VERDICT-}" ]; then
        printf '%s\n' "$json" >"$SPAWN_GATE_VERDICT"
    fi
    echo "$json"
}

# host_fault <scenario> <on> <runs> <reason> [<evidence> [<tree>]]: no verdict on the candidate,
# a fact about the host or the harness instead (exit 6), written where a
# verdict would be: `{"host_fault":true,"scenario","on","runs","reason",
# "base","candidate"}`. `on` is where it failed: `base` (a scenario's
# harness error on the base, before any candidate code ran: its runs did
# not agree, or it exited neither 0 nor 1), `candidate and base` (a check
# that failed twice on the candidate and once on the base), `sandbox` (a
# sandbox that never started its command, with the evidence
# `sandbox_never_started`: "A sandbox that never started its command") or
# `test server` (the gate's own server did not start, with the evidence
# `test_server_never_started` and the start as its tree: start_server). The
# integrator
# believes the fact only from its own verdict file with exit 6 (the sandbox
# never sees that file) and records `host_fault`: retried after a backoff,
# counted toward no halt (gate.rs `HOST_FAULT_EXIT`).
host_fault() {
    # Base only judges no candidate and writes no verdict file: no
    # verdict, as before (exit 4).
    if [ -n "$base_only" ]; then
        echo "| $4"
        exit 4
    fi
    stage_close host_fault
    gate_times
    stage_emit gate host_fault "$(($(stage_now) - gate_started))" total
    json="{\"host_fault\":true,\"scenario\":\"$(json_string "$1")\",\"on\":\"$2\""
    json="$json,\"runs\":\"$(json_string "$3")\",\"reason\":\"$(json_string "$4")\""
    [ -z "${5-}" ] || json="$json,\"evidence\":\"$5\""
    [ -z "${6-}" ] || json="$json,\"tree\":\"$6\""
    # The check main was red on (SPAWN_GATE_BASE_CHECK), when it passed on
    # this base before any candidate code ran: main is green on it again,
    # whatever this gate ends with (gate.rs reads it from any fact).
    [ -n "${base_check_passed-}" ] && json="$json,\"base_check\":\"$base_check_passed\""
    json="$json,\"base\":\"$base_commit\",\"candidate\":\"$candidate_commit\"}"
    if [ -n "${SPAWN_GATE_VERDICT-}" ]; then
        printf '%s\n' "$json" >"$SPAWN_GATE_VERDICT"
    fi
    echo "$json"
    exit 6
}

# main_red <scenario> <runs> <failures file>: main is red on a check, a
# fact about the base, not a verdict on the candidate (exit 6), written
# where a verdict would be: `{"host_fault":true,"scenario","on":"main",
# "runs","reason","failures":[{"test","message"}],"base","candidate"}`. The
# check failed twice on the base and the two runs share failing tests
# (shared_failed); <failures file> holds those, `FAILED <test>: <message>`,
# at most 20 of them in the fact. The integrator tells a red main from a
# host's fault by the failures' classes and the test server's log, and
# records `source.main_red` (gate.rs `GateFailure::MainRed`).
main_red() {
    if [ -n "$base_only" ]; then
        echo "| $1 fails on the base twice ($2): main is red on it"
        exit 4
    fi
    stage_close host_fault
    gate_times
    stage_emit gate host_fault "$(($(stage_now) - gate_started))" total
    main_red_list=""
    while IFS= read -r main_red_line; do
        main_red_rest=${main_red_line#FAILED }
        main_red_test=${main_red_rest%%: *}
        main_red_message=${main_red_rest#*: }
        [ "$main_red_message" != "$main_red_rest" ] || main_red_message=""
        main_red_list="$main_red_list${main_red_list:+,}{\"test\":\"$(json_string "$main_red_test")\",\"message\":\"$(json_string "$main_red_message")\"}"
    done <<EOF_MAIN_RED
$(head -n 20 "$3")
EOF_MAIN_RED
    json="{\"host_fault\":true,\"scenario\":\"$(json_string "$1")\",\"on\":\"main\""
    json="$json,\"runs\":\"$(json_string "$2")\""
    json="$json,\"reason\":\"$(json_string "$1 fails on the base twice ($2), with the same failing tests: main is red on it, not a verdict on the candidate")\""
    json="$json,\"failures\":[$main_red_list]"
    # The check main was red on (SPAWN_GATE_BASE_CHECK), when it passed on
    # this base before any candidate code ran: main is green on it again,
    # whatever this gate ends with (gate.rs reads it from any fact).
    [ -n "${base_check_passed-}" ] && json="$json,\"base_check\":\"$base_check_passed\""
    json="$json,\"base\":\"$base_commit\",\"candidate\":\"$candidate_commit\"}"
    if [ -n "${SPAWN_GATE_VERDICT-}" ]; then
        printf '%s\n' "$json" >"$SPAWN_GATE_VERDICT"
    fi
    echo "$json"
    exit 6
}

# shared_failed <out> <out> <dest>: the `FAILED <test>: …` lines of the first
# run whose test the second run failed too (check-tests.sh prints one per
# failing test); <dest> empty when they share none. 0 when they share one.
# new_failures <main-red> <out>...: for an owner's push
# (SPAWN_GATE_RED_UNSOLVED), the tests the candidate's runs fail (`FAILED
# <test>:`) that main's red runs do not, one a line, first seen first; a
# candidate run with no FAILED line names nothing it failed, so it says so
# (`(no FAILED line)`). Nothing when every candidate failure is main's own
# (rev-b F1 on bfbda5cb: the red check was skipped whatever the candidate
# broke inside it).
new_failures() {
    new_red=$1
    shift
    for new_out in "$@"; do
        if ! grep -q '^FAILED ' "$new_out" 2>/dev/null; then
            echo "(no FAILED line)"
            continue
        fi
        awk '
            FNR == NR { if ($1 == "FAILED") { t = $2; sub(/:$/, "", t); red[t] = 1 } next }
            $1 == "FAILED" { t = $2; sub(/:$/, "", t); if (!(t in red)) print t }
        ' "$new_red" "$new_out"
    done | awk '!seen[$0]++'
}
shared_failed() {
    awk '
        FNR == NR { if ($1 == "FAILED") { t = $2; sub(/:$/, "", t); seen[t] = 1 } next }
        $1 == "FAILED" { t = $2; sub(/:$/, "", t); if (t in seen && !(t in said)) { said[t] = 1; print } }
    ' "$2" "$1" >"$3" 2>/dev/null
    [ -s "$3" ]
}

# cannot_judge <scenario> <runs>: the scenario says this host cannot judge it
# here (029: no systemd user manager in the sandbox), so the gate states that
# as the fact and exits 6, no verdict: `{"cannot_judge":true,"scenario",
# "runs","base","candidate"}`. Exit 6 is the host-fault exit too; the fact's
# key says which one this is (gate.rs `cannot_judge`, `host_fault`), and the
# integrator records the evidence `cannot_judge` for it.
cannot_judge() {
    stage_close cannot_judge
    gate_times
    stage_emit gate cannot_judge "$(($(stage_now) - gate_started))" total
    json="{\"cannot_judge\":true,\"scenario\":\"$(json_string "$1")\""
    json="$json,\"runs\":\"$(json_string "$2")\""
    # The check main was red on (SPAWN_GATE_BASE_CHECK), when it passed on
    # this base before any candidate code ran: main is green on it again,
    # whatever this gate ends with (gate.rs reads it from any fact).
    [ -n "${base_check_passed-}" ] && json="$json,\"base_check\":\"$base_check_passed\""
    json="$json,\"base\":\"$base_commit\",\"candidate\":\"$candidate_commit\"}"
    if [ -n "${SPAWN_GATE_VERDICT-}" ]; then
        printf '%s\n' "$json" >"$SPAWN_GATE_VERDICT"
    fi
    echo "$json"
    exit 6
}

# What the verdict is about: both commits, resolved once.
base_commit=$(git --git-dir="$repo" rev-parse --verify --quiet "$baseline^{commit}") || {
    echo "gate: no commit $baseline in $repo" >&2
    exit 3
}
candidate_commit=$(git --git-dir="$repo" rev-parse --verify --quiet "$candidate^{commit}") || {
    echo "gate: no commit $candidate in $repo" >&2
    exit 3
}

# The controller resolves these paths against its own checkout. A relative
# path would resolve inside the tree under test.
for path in "$scenarios" ${scenario:+"$scenario"}; do
    case "$path" in
    /*) ;;
    *)
        verdict rejected "scenario path is not absolute"
        exit 0
        ;;
    esac
done
if [ ! -d "$scenarios" ]; then
    verdict rejected "scenarios directory is missing"
    exit 0
fi
if [ -n "$scenario" ] && [ ! -f "$scenario" ]; then
    verdict rejected "scenario is missing"
    exit 0
fi

cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
# The base's objective results (see "The base's result" below): written
# here, outside the sandbox, which never sees this directory.
base_cache=${XDG_CACHE_HOME:-$HOME/.cache}/spawn/gate-base
mkdir -p "$base_cache"
chmod 700 "$base_cache"
base_cache=$(cd "$base_cache" && pwd -P)
# The controller's checkout, as the checks locate it (two levels above the
# scenarios directory).
controller=$(cd "$scenarios/../.." && pwd)
# The gate's own helpers (scripts/pg-url.sh, which `toolchain` sources
# inside the sandbox): beside this script, which for a project that is not
# Spawn (Supply PA's gate/scenarios) is another checkout than the
# controller's. Bound back like the controller's scripts, or the probe of
# the test server reads nothing ("server: unreadable", 2026-11-09).
gate_scripts=$(cd "$(dirname "$0")/.." && pwd)

# $HOME in the sandbox is an empty tmpfs. Bound back read-only, one path per
# line: the Rust toolchain (rustup's proxies and toolchains), the install
# root of each tool in `home_tools` that lives under $HOME (with a symlink
# where PATH found it), and the controller's `scripts` and
# `web/node_modules`, which the checks use. Nothing else of $HOME is
# visible: not ~/.gitconfig, not ~/.claude.json.backup, not shell history.
home_tools="node npm npx"
home_binds=""
home_links=""
under_home() {
    case "$1" in "$HOME"/*) return 0 ;; esac
    return 1
}
bind_home() {
    [ -e "$1" ] || return 0
    home_binds="$home_binds$1
"
}
bound() {
    old_ifs=$IFS
    IFS='
'
    for bound_root in $home_binds; do
        case "$1" in "$bound_root" | "$bound_root"/*)
            IFS=$old_ifs
            return 0
            ;;
        esac
    done
    IFS=$old_ifs
    return 1
}
under_home "$cargo_home/bin" && bind_home "$cargo_home/bin"
under_home "$rustup_home" && bind_home "$rustup_home"
for tool in $home_tools; do
    found=$(command -v "$tool" 2>/dev/null) || continue
    real=$(readlink -f "$found")
    under_home "$real" || continue
    root=$(dirname "$real")
    [ "$(basename "$root")" = bin ] && root=$(dirname "$root")
    # A plain file in ~/bin or ~/.local/bin: that file, not the directory
    # above it.
    case "$root" in "$HOME" | "$HOME/.local") root=$real ;; esac
    bound "$real" || bind_home "$root"
    if [ "$found" != "$real" ] && under_home "$found" && ! bound "$found"; then
        home_links="$home_links$real|$found
"
    fi
done
for path in "$controller/scripts" "$controller/web/node_modules"; do
    under_home "$path" && bind_home "$path"
done
if [ "$gate_scripts" != "$controller/scripts" ] && under_home "$gate_scripts"; then
    bind_home "$gate_scripts"
fi
# The owner's own read-only inputs for a tree that is not Spawn (a project's
# installed dependencies, which the sandbox cannot download): one absolute
# directory per line in `<scenarios-dir>/bind`, `#` comments. Only under
# $HOME (anything else outside the emptied directories is visible already),
# and a missing one stops the gate (exit 3) rather than judging without it.
if [ -f "$scenarios/bind" ]; then
    while IFS= read -r path || [ -n "$path" ]; do
        case "$path" in '' | '#'*) continue ;; esac
        case "$path" in
        /*) ;;
        *)
            echo "gate: $scenarios/bind: not an absolute path: $path" >&2
            exit 3
            ;;
        esac
        if [ ! -d "$path" ]; then
            echo "gate: $scenarios/bind: no directory $path" >&2
            exit 3
        fi
        under_home "$path" && ! bound "$path" && bind_home "$path"
    done <"$scenarios/bind"
fi

# The base cache must stay hidden: the sandbox puts a tmpfs over it, and a
# bind of a directory above it, made after that tmpfs, would show it again.
# Those binds: the scenarios, the task scenario, the allow-listed $HOME
# paths and the test database's socket directory (below). The tree under
# test and the base tree are fresh directories under $work.
cache_exposed() {
    for exposed in "$@"; do
        [ -n "$exposed" ] && [ -e "$exposed" ] || continue
        exposed=$(cd "$(dirname "$exposed")" && pwd -P)/$(basename "$exposed")
        case "$base_cache/" in "$exposed"/*)
            echo "gate: the base cache $base_cache lies under $exposed, which the sandbox shows" >&2
            return 0
            ;;
        esac
    done
    return 1
}

# The test database, reached over one Unix socket file: the sandbox has no
# network but its own loopback, and sees no other socket of the host (see
# "Unix sockets" below). The URL is read as libpq reads it
# (scripts/pg-url.sh, percent-decoded):
# - a socket directory (postgres://u:p@%2Fdir:5432/db, or ?host=/dir): the
#   one file <dir>/.s.PGSQL.<port> is bound in, not the directory, so a
#   neighbour in it (another cluster's socket) stays out;
# - a TCP host (127.0.0.1, localhost, [::1], any name): the gate forwards a
#   socket of its own, $work/test-db/.s.PGSQL.<port>, to that host and port,
#   outside the sandbox (python3), and the sandbox reaches that server and
#   nothing else of the network. The forwarded connection is plain: a
#   server that requires TLS (hostssl) refuses it.
# Inside, SPAWN_TEST_DATABASE_URL names that socket
# (postgres://u:p@%2Fdir:<port>/db?<other parameters>), whatever form the
# owner gave. An unreadable URL or a missing socket is an infrastructure
# error (exit 3), not a verdict.
#
# A server of the gate's own (GitHub issue #6): with SPAWN_GATE_PG_IMAGE set
# (the test server's docker image, postgres:16-alpine), or SPAWN_GATE_PG_BIN
# (the directory of the host's programs of the test server's version,
# /usr/lib/postgresql/16/bin, on a host without docker), the gate starts a
# disposable PostgreSQL of those programs under its scratch and hands the
# sandbox its socket instead (scripts/rsi/gate-pg.sh), whatever
# SPAWN_TEST_DATABASE_URL says: postgres://postgres:<password>@%2F<dir>:5432/
# spawn_test, a superuser over a socket only the sandbox and the gate reach.
# Base results are taken on a clean server, and what candidate code leaves on
# it dies with the gate. It is sized for the scenarios that run at once
# (max_connections 100 + 50 per job). One that does not start is an
# infrastructure error (exit 3).
#
# Scenarios at once (see "Parallel runs" below): SPAWN_GATE_JOBS, a whole
# number from 1. `spawn integrate` always sets it, sharing the host's cores
# among the gates that can run at once and starting a gate only within
# them (crates/spawn-cli/src/gate.rs, Cpu; SPAWN_GATE_WORKER_CORES keeps
# cores for the workers). Run by hand without it: min(8, half the cores),
# or 1 on the shared test server (SPAWN_TEST_DATABASE_URL without a server
# of the gate's own), whose connections every agent's tests share.
# shellcheck source=scripts/pg-url.sh
. "$(dirname "$0")/../pg-url.sh"
# shellcheck source=scripts/rsi/gate-pg.sh
. "$(dirname "$0")/gate-pg.sh"
gate_jobs=${SPAWN_GATE_JOBS-}
if [ -z "$gate_jobs" ]; then
    if [ -n "${SPAWN_TEST_DATABASE_URL-}" ] && [ -z "$gate_pg" ]; then
        gate_jobs=1
    else
        gate_jobs=$(($(nproc 2>/dev/null || echo 2) / 2))
        [ "$gate_jobs" -le 8 ] || gate_jobs=8
        [ "$gate_jobs" -ge 1 ] || gate_jobs=1
    fi
fi
case "$gate_jobs" in
'' | *[!0-9]* | 0*)
    echo "gate: SPAWN_GATE_JOBS is a whole number from 1, not '$gate_jobs'" >&2
    exit 3
    ;;
esac
# A base-only run is a warm (the deploy's, the integrator's after a
# landing): one job, whatever SPAWN_GATE_JOBS says, so that every warm has
# the same footprint — at most one host scenario slot, claimed last ("Host
# scenario slots"), and one compiler job (below).
[ -z "$base_only" ] || gate_jobs=1
# The gate's build share: CARGO_BUILD_JOBS inside the sandbox for the
# build hook (`<scenarios-dir>/build`) and the test listing. Only a warm
# (`--base-only`) is pinned, to 1, so its build takes one core beside the
# gates it yields to; any other gate builds as before, with cargo's own
# default, unless the owner sets SPAWN_GATE_BUILD_JOBS (a gate's
# SPAWN_GATE_JOBS sizes its scenario runs, not its compiler: a lone gate
# building at that would build slower than it did). Unset, the variable is
# not passed at all, never passed empty. It is not in the base cache key: a
# -j changes how fast a tree builds, never what a scenario on it returns.
gate_build_jobs=${SPAWN_GATE_BUILD_JOBS-}
if [ -n "$gate_build_jobs" ]; then
    case "$gate_build_jobs" in
    *[!0-9]* | 0*)
        echo "gate: SPAWN_GATE_BUILD_JOBS is a whole number from 1, not '$gate_build_jobs'" >&2
        exit 3
        ;;
    esac
fi
[ -z "$base_only" ] || gate_build_jobs=1
# The host's scenario budget (see "Host scenario slots"): SPAWN_GATE_HOST_JOBS,
# else the value every gate is given (SPAWN_GATE_JOBS) when the owner set
# it, else the same default one gate takes. Without flock, or a writable
# slot directory, it becomes 0 — no budget, said below.
# The host's budget comes, in this order, from a file the host states
# (`$HOME/.cache/spawn/gate-slots/host_jobs`: the deploy writes it from the
# integrator's gate environment, the owner may edit it), then
# SPAWN_GATE_HOST_JOBS, then the value every gate is given (SPAWN_GATE_JOBS),
# then the hand-run default. The file wins precisely so a hand-run shell
# cannot raise (or lower) the host's cap by carrying another value; the gate
# only reads it, so no gate that runs first can decide it for the others.
slot_dir=$HOME/.cache/spawn/gate-slots
slot_stated=""
if [ -f "$slot_dir/host_jobs" ]; then
    slot_stated=$(tr -d ' \t\n' <"$slot_dir/host_jobs" 2>/dev/null || true)
    case "$slot_stated" in
    '' | *[!0-9]* | 0*)
        echo "gate: $slot_dir/host_jobs is not a whole number from 1: '$slot_stated'" >&2
        exit 3
        ;;
    esac
fi
if [ -n "$slot_stated" ]; then
    gate_slots=$slot_stated
else
    gate_slots=${SPAWN_GATE_HOST_JOBS-}
    if [ -z "$gate_slots" ]; then
        if [ -n "${SPAWN_GATE_JOBS-}" ]; then
            gate_slots=$gate_jobs
        else
            gate_slots=$(($(nproc 2>/dev/null || echo 2) / 2))
            [ "$gate_slots" -le 8 ] || gate_slots=8
            [ "$gate_slots" -ge 1 ] || gate_slots=1
        fi
    fi
fi
case "$gate_slots" in
'' | *[!0-9]* | 0*)
    echo "gate: SPAWN_GATE_HOST_JOBS is a whole number from 1, not '$gate_slots'" >&2
    exit 3
    ;;
esac
# How long one run may wait for a slot before the gate says it has no room
# (exit 5): SPAWN_GATE_SLOT_WAIT_SECS, default 30 minutes. Below the
# integrator's gate wall clock (SPAWN_GATE_WALL_CLOCK_MINUTES, default 120),
# which the gate cannot read, and said in the output when it is reached.
slot_wait=${SPAWN_GATE_SLOT_WAIT_SECS-}
if [ -z "$slot_wait" ]; then
    slot_wait=1800
else
    case "$slot_wait" in
    '' | *[!0-9]* | 0*)
        echo "gate: SPAWN_GATE_SLOT_WAIT_SECS is a whole number of seconds from 1, not '$slot_wait'" >&2
        exit 3
        ;;
    esac
fi
# Who claims first (see "Host scenario slots"): a warm (`--base-only`) is
# `low`, every other gate `normal`; SPAWN_GATE_SLOT_PRIORITY says otherwise
# (the suite, an owner's hand-run warm that should not wait).
slot_priority=${SPAWN_GATE_SLOT_PRIORITY-}
if [ -z "$slot_priority" ]; then
    if [ -n "$base_only" ]; then slot_priority=low; else slot_priority=normal; fi
fi
case "$slot_priority" in
normal | low) ;;
*)
    echo "gate: SPAWN_GATE_SLOT_PRIORITY is normal or low, not '$slot_priority'" >&2
    exit 3
    ;;
esac
# The slots themselves: files this host's gates lock one at a time, under
# $HOME (0700), so the integrator's gate, an owner's hand-run gate and the
# deploy's warm-up all find the same ones — with no variable to set, and
# whatever XDG_CACHE_HOME a hand-run shell carries. (test-gate.sh gives its
# runs a HOME of their own, so its gates stay out of the host's budget.)
# Outside the sandbox: no scenario can see or take a lock, and the kernel
# drops a held one when its fd closes, a kill included.
slot_held="" slot_kept=""
if command -v flock >/dev/null 2>&1; then
    if mkdir -p "$slot_dir" 2>/dev/null && chmod 700 "$slot_dir" 2>/dev/null; then
        slot_i=1
        while [ "$slot_i" -le "$gate_slots" ]; do
            if ! (umask 077 && : >>"$slot_dir/slot.$slot_i") 2>/dev/null; then
                gate_slots=0
                break
            fi
            slot_i=$((slot_i + 1))
        done
        # The demand lock beside the slots (see "Host scenario slots"):
        # without it no priority, every claimant as before.
        if [ "$gate_slots" -gt 0 ] && ! (umask 077 && : >>"$slot_dir/demand") 2>/dev/null; then
            slot_priority=normal
        fi
    else
        gate_slots=0
    fi
else
    gate_slots=0
fi
if [ "$gate_slots" = 0 ]; then
    echo "| host scenario slots: none (no flock, or $slot_dir cannot be written): each gate runs its own SPAWN_GATE_JOBS"
else
    if [ -z "$slot_stated" ]; then
        echo "| host scenario slots: no $slot_dir/host_jobs file: $gate_slots from SPAWN_GATE_HOST_JOBS or the default, so a gate started with a larger value can exceed this cap"
    fi
    echo "| host scenario slots: $gate_slots (every gate on this host takes one per scenario run)"
    [ "$slot_priority" = normal ] ||
        echo "| host scenario slots: claimed last (a warm: a slot only when no gate is waiting for one)"
fi
# Why a run waits, as its lines say it.
if [ "$slot_priority" = normal ]; then
    slot_busy="all $gate_slots taken, shared by every gate on this host"
else
    slot_busy="claimed last: all $gate_slots taken, or a gate is waiting for one"
fi
# Whatever the gate started dies with it: its scenario runs, the private
# server, the forwarder (below). A SIGKILL leaves none either: each runs
# under bubblewrap's --die-with-parent or the parent-death signal.
jobs_dir=$work/jobs
jobs_stop_all() {
    for stop_job in "$jobs_dir"/*/; do
        if [ -d "$stop_job" ]; then job_stop "${stop_job%/}"; fi
    done
}
# job_stop <job>: a scenario run of the gate's that is no longer needed. Its
# subshell first (so it starts nothing more and writes no cache entry), then
# the subshell bubblewrap is the child of, which ends the sandbox.
job_stop() {
    [ ! -e "$1/result" ] || return 0
    for stop_pid in job.pid pid; do
        [ ! -s "$1/$stop_pid" ] || kill -KILL "$(cat "$1/$stop_pid" 2>/dev/null)" 2>/dev/null || :
    done
    : >"$1/stopped" 2>/dev/null || :
}
db_forwarder=""
# In the EXIT trap, under `set -e`: nothing here may fail, or the gate ends
# with that status instead of its own.
gate_stop_children() {
    if [ -d "$jobs_dir" ]; then jobs_stop_all || :; fi
    if [ -n "$db_forwarder" ]; then kill "$db_forwarder" 2>/dev/null || :; fi
    gate_pg_stop || :
}
# The scratch, last. A run stopped early may still be ending, its /tmp (a
# directory under $work, see sandbox) not yet still: one more try a second
# later, and what is left is spawn-maintenance's (scratch older than a day).
# Never a failure here, which would replace the gate's status.
gate_remove_work() {
    chmod -R u+w "$work" 2>/dev/null || :
    rm -rf "$work" 2>/dev/null && return 0
    sleep 1
    chmod -R u+w "$work" 2>/dev/null || :
    rm -rf "$work" 2>/dev/null || :
}
trap 'stage_close aborted; gate_times; gate_stop_children; gate_remove_work' EXIT
db_socket=""
db_forward=""
sandbox_db_url=""
# start_server <dir> <tree>: the gate's own server, its files in <dir>;
# <tree> is which start: `gate`, the first, before anything of either tree,
# or `candidate`, the candidate phase's fresh one. Programs or a template
# that cannot be prepared are the owner's configuration: exit 3, as before.
# A server that does not come up is the host's: no verdict, the fact
# `test_server_never_started` (exit 6, `host_fault`), with the server's
# first FATAL line or the gate's own word, retried after a backoff and
# never counted toward the task. The suite's seam,
# SPAWN_GATE_TEST_PG_FAIL=<tree>, gives that start one setting no server
# knows, so it fails as a real server does.
start_server() {
    start_option=""
    [ "${SPAWN_GATE_TEST_PG_FAIL-}" != "$2" ] || start_option=spawn_gate_test_no_such_setting=1
    start_rc=0
    gate_pg_start "$1" "$gate_jobs" "$start_option" || start_rc=$?
    [ "$start_rc" -eq 0 ] && return 0
    [ "$start_rc" -eq 2 ] || exit 3
    start_said=$(sed -n -E 's/^.*(FATAL|PANIC):/\1:/p' "$1/server.log" 2>/dev/null | head -n 1 | cut -c1-300) ||
        start_said=""
    # Or what kept it from starting at all (gate-pg.sh: its copy of the
    # template).
    [ -n "$start_said" ] || start_said=$(head -n 1 "$1/start.said" 2>/dev/null | cut -c1-300) || start_said=""
    case "$2" in
    gate) start_what="the gate's test server" ;;
    *) start_what="the candidate phase's fresh test server" ;;
    esac
    host_fault "test server" "test server" "never started" \
        "$start_what did not start${start_said:+: $start_said}: a host fault, not a verdict on the candidate" \
        test_server_never_started "$2"
}
if [ -n "$gate_pg" ]; then
    start_server "$work/pg" gate
    # A link of the gate's to the socket the server made at start: the
    # server cannot change what it names (scripts/rsi/gate-pg.sh).
    mkdir -m 700 "$work/test-db"
    db_socket=$work/test-db/.s.PGSQL.5432
    if ! ln -P "$gate_pg_socket" "$db_socket" || [ ! -S "$db_socket" ]; then
        echo "gate: cannot link the private test server's socket into $work/test-db" >&2
        exit 3
    fi
    sandbox_db_url="postgres://postgres:$gate_pg_password@$(pg_url_encode_host "$work/test-db"):5432/spawn_test"
    echo "| test database: a server of the gate's own, $gate_pg" \
        "($(printf '%s' "$gate_pg_id" | cut -c1-12)), up in $gate_pg_ms ms," \
        "max_connections $((100 + 50 * gate_jobs)); scenarios at once: $gate_jobs"
    (umask 077 && printf '%s\0' --setenv SPAWN_TEST_DATABASE_URL "$sandbox_db_url" \
        >"$work/sandbox-db-args")
elif [ -n "${SPAWN_TEST_DATABASE_URL-}" ]; then
    if ! pg_url_parse "$SPAWN_TEST_DATABASE_URL"; then
        echo "gate: SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL" >&2
        exit 3
    fi
    case "$pg_url_host" in
    /*)
        db_socket=$pg_url_host/.s.PGSQL.$pg_url_port
        if [ ! -S "$db_socket" ]; then
            echo "gate: no test database socket at $db_socket" >&2
            exit 3
        fi
        db_socket=$(cd "$pg_url_host" && pwd -P)/.s.PGSQL.$pg_url_port
        ;;
    "")
        echo "gate: SPAWN_TEST_DATABASE_URL names no host; name the socket directory or the address" >&2
        exit 3
        ;;
    *)
        command -v python3 >/dev/null 2>&1 || {
            echo "gate: a TCP test database URL needs python3 to forward it into the sandbox" >&2
            exit 3
        }
        mkdir -m 700 "$work/test-db"
        db_socket=$work/test-db/.s.PGSQL.$pg_url_port
        db_forward="$pg_url_host $pg_url_port"
        ;;
    esac
    sandbox_db_url="$pg_url_scheme://${pg_url_userinfo:+$pg_url_userinfo@}$(pg_url_encode_host \
        "$(dirname "$db_socket")"):$pg_url_port/$(pg_url_encode_component "$pg_url_database")"
    [ -z "$pg_url_options" ] || sandbox_db_url="$sandbox_db_url?$pg_url_options"
    # The URL holds the test database's password: bwrap reads it from a file
    # only this uid can read (--args), never from its command line, which
    # every local user sees in ps.
    (umask 077 && printf '%s\0' --setenv SPAWN_TEST_DATABASE_URL "$sandbox_db_url" \
        >"$work/sandbox-db-args")
fi

# The forwarder of a TCP test database: a Unix socket in $work/test-db that
# relays each connection to the test server's host and port. It dies with
# the gate (the EXIT trap, and the parent-death signal if the gate is
# killed).
if [ -n "$db_forward" ]; then
    cat >"$work/test-db-forward.py" <<'EOF'
import ctypes, os, socket, sys, threading
ctypes.CDLL(None).prctl(1, 15)  # PR_SET_PDEATHSIG: SIGTERM when the gate dies
path, host, port = sys.argv[1], sys.argv[2], int(sys.argv[3])
listener = socket.socket(socket.AF_UNIX)
listener.bind(path)
listener.listen(256)

def pump(source, sink):
    try:
        while True:
            data = source.recv(65536)
            if not data:
                break
            sink.sendall(data)
    except OSError:
        pass
    for end in (source, sink):
        try:
            end.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

def relay(client):
    try:
        upstream = socket.create_connection((host, port), 10)
    except OSError:
        client.close()
        return
    upstream.settimeout(None)
    back = threading.Thread(target=pump, args=(upstream, client), daemon=True)
    back.start()
    pump(client, upstream)
    back.join()
    client.close()
    upstream.close()

while True:
    client, _ = listener.accept()
    threading.Thread(target=relay, args=(client,), daemon=True).start()
EOF
    # shellcheck disable=SC2086 # host and port
    python3 "$work/test-db-forward.py" "$db_socket" $db_forward </dev/null >/dev/null 2>"$work/test-db-forward.log" &
    db_forwarder=$!
    waited=0
    while [ ! -S "$db_socket" ]; do
        if ! kill -0 "$db_forwarder" 2>/dev/null || [ "$waited" -ge 100 ]; then
            echo "gate: the test database forwarder did not start:" >&2
            cat "$work/test-db-forward.log" >&2
            exit 3
        fi
        sleep 0.1
        waited=$((waited + 1))
    done
fi

old_ifs=$IFS
IFS='
'
# shellcheck disable=SC2086 # one path per line
if cache_exposed "$scenarios" "$scenario" "$db_socket" $home_binds; then
    IFS=$old_ifs
    exit 3
fi
IFS=$old_ifs

# PATH directories outside $HOME under the directories the sandbox empties
# (a toolchain in /run/current-system): bound back read-only, one per line.
hidden_path_dirs=""
old_ifs=$IFS
IFS=:
for path in $PATH; do
    [ -d "$path" ] || continue
    # $HOME has its own allow-list (home_binds).
    under_home "$path" && continue
    case "$path" in
    /tmp/* | /run/* | /var/run/* | /var/tmp/*) ;;
    *)
        case "${TMPDIR:-/tmp}" in / | /tmp | /tmp/) continue ;; esac
        case "$path" in "${TMPDIR%/}"/*) ;; *) continue ;; esac
        ;;
    esac
    hidden_path_dirs="$hidden_path_dirs$path
"
done
IFS=$old_ifs
socket_masks=""

# sandbox <tree> <command...>: run a command inside the tree, in bwrap.
#
# Unix sockets. A read-only bind does not stop connect() on a socket, so
# every directory where the host keeps them is an empty tmpfs inside: /tmp,
# /run (with /var/run, the user's runtime directory /run/user/<uid>, the
# docker, podman and containerd sockets, the system bus, /run/postgresql,
# ssh and gpg agents), /var/tmp, the host's TMPDIR and $HOME. /dev is a
# fresh one (no /dev/log), and the network namespace is new, which also
# hides the host's abstract sockets (@/tmp/.X11-unix/X0 and the like). Bound
# back: the PATH directories under those, the allow-listed $HOME paths, and
# the test database's one socket file. Whatever other socket the host still
# shows is found by a scan inside the sandbox before any candidate code
# runs (`socket_masks` below), and hidden too.
#
# /tmp is a fresh, empty directory on the gate's own disk (under $work), not
# a tmpfs. A tmpfs is sized by the host's memory (half of it), so a scenario
# that measures the room in its TMPDIR, a nested integrator's floor
# (SPAWN_GATE_MIN_FREE_GB, default 10), was judged by how much memory the
# host has: on a 17 GB host it saw 8.8 GiB and scenario 014 aborted every
# gate. What the tests write there took the host's memory besides. The
# directory is removed when the command ends, and with $work at the gate's
# exit.
sandbox_once() {
    tree=$1
    shift
    count=$#
    sandbox_tmp=$(mktemp -d "$work/sandbox-tmp.XXXXXX")
    sandbox_masked=""
    set -- "$@" bwrap --ro-bind / / --dev /dev --proc /proc --bind "$sandbox_tmp" /tmp --tmpfs /run \
        --unshare-pid --unshare-net --die-with-parent --new-session
    if [ -d /var/run ] && [ ! -L /var/run ]; then set -- "$@" --tmpfs /var/run; fi
    if [ -d /var/tmp ] && [ ! -L /var/tmp ]; then set -- "$@" --tmpfs /var/tmp; fi
    for name in credentials credentials.toml; do
        [ -e "$cargo_home/$name" ] && set -- "$@" --ro-bind /dev/null "$cargo_home/$name"
    done
    # The host's TMPDIR when it is not /tmp, the gate's own files and the
    # integrator's verdict directory, wherever TMPDIR put them.
    case "${TMPDIR:-/tmp}" in
    / | /tmp | /tmp/) ;;
    *) [ -d "$TMPDIR" ] && set -- "$@" --tmpfs "$TMPDIR" ;;
    esac
    set -- "$@" --tmpfs "$work" --tmpfs "$base_cache"
    if [ -n "${SPAWN_GATE_VERDICT-}" ]; then
        set -- "$@" --tmpfs "$(dirname "$SPAWN_GATE_VERDICT")"
    fi
    set -- "$@" --tmpfs "$HOME"
    old_ifs=$IFS
    IFS='
'
    for path in $hidden_path_dirs; do
        set -- "$@" --ro-bind "$path" "$path"
    done
    for path in $home_binds; do
        set -- "$@" --ro-bind "$path" "$path"
    done
    for link in $home_links; do
        set -- "$@" --symlink "${link%%|*}" "${link#*|}"
    done
    IFS=$old_ifs
    set -- "$@" --ro-bind "$scenarios" "$scenarios"
    [ -n "$scenario" ] && set -- "$@" --ro-bind "$scenario" "$scenario"
    [ -n "$db_socket" ] && set -- "$@" --ro-bind "$db_socket" "$db_socket"
    if [ "$tree" != "$work/baseline" ] && [ -d "$work/baseline" ]; then
        set -- "$@" --ro-bind "$work/baseline" "$work/baseline"
    fi
    set -- "$@" --bind "$tree" "$tree"
    # Git: the gate's config, not the owner's (signing keys, credential
    # helpers).
    set -- "$@" --ro-bind "$work/gitconfig" /tmp/gitconfig
    # Cargo: a private home over the real caches, read-only, offline.
    set -- "$@" --dir /tmp/cargo-home
    for name in registry git config config.toml; do
        [ -e "$cargo_home/$name" ] && set -- "$@" --ro-bind "$cargo_home/$name" "/tmp/cargo-home/$name"
    done
    # Last, so no bind above shows them again: the sockets the scan found.
    # One whose directory or file is gone since (a host process's temporary
    # directory) is skipped: bwrap cannot mount over what is not there, and
    # failing every later command would misjudge the candidate.
    old_ifs=$IFS
    IFS='
'
    for mask in $socket_masks; do
        case "$mask" in
        "directory "*)
            if [ -d "${mask#directory }" ]; then
                set -- "$@" --tmpfs "${mask#directory }"
                sandbox_masked="$sandbox_masked${mask#directory }
"
            fi
            ;;
        "file "*)
            if [ -e "${mask#file }" ]; then
                set -- "$@" --ro-bind /dev/null "${mask#file }"
                sandbox_masked="$sandbox_masked${mask#file }
"
            fi
            ;;
        esac
    done
    IFS=$old_ifs
    set -- "$@" --clearenv --setenv HOME "$HOME" --setenv PATH "$PATH" --setenv TMPDIR /tmp \
        --setenv CARGO_HOME /tmp/cargo-home --setenv CARGO_NET_OFFLINE true \
        --setenv npm_config_cache /tmp/npm-cache \
        --setenv GIT_CONFIG_GLOBAL /tmp/gitconfig
    # LANG and RUSTUP_HOME as they are; a tree's deps store (SUPPLY_PA_DEPS)
    # as the integrator names it, so `build` and the scenarios read the tree
    # the owner installed rather than the default under $HOME.
    for name in LANG RUSTUP_HOME SUPPLY_PA_DEPS; do
        eval "value=\${$name-}"
        [ -n "$value" ] && set -- "$@" --setenv "$name" "$value"
    done
    [ -n "$sandbox_db_url" ] && set -- "$@" --args 9
    set -- "$@" --chdir "$tree" --
    # Inside, before the command: one byte on fd 4 says it started (bwrap's
    # setup is done), and the caller's stderr comes back from fd 3, so the
    # command writes where it always did and bwrap's own messages stay apart
    # (see `sandbox` below). The command sees neither descriptor.
    # shellcheck disable=SC2016 # expanded inside the sandbox
    set -- "$@" sh -c 'printf s >&4; exec 4>&- 2>&3 3>&-; exec "$@"' sandbox-start
    # The command was given first: move it after the options.
    while [ "$count" -gt 0 ]; do
        set -- "$@" "$1"
        shift
        count=$((count - 1))
    done
    sandbox_status=0
    sandbox_started=""
    sandbox_said=""
    : >"$sandbox_tmp.started"
    # The suite's seam (SPAWN_GATE_TEST_HOLD): the build hook's first
    # sandbox with masks waits here, between the check above and bwrap's
    # mount, until the suite has removed what it wanted gone.
    if [ -n "${SPAWN_GATE_TEST_HOLD-}" ] && [ -e "$SPAWN_GATE_TEST_HOLD" ] &&
        [ -n "$sandbox_masked" ] && [ -n "${sandbox_hold_here-}" ]; then
        printf '%s\n' "$tree" >"$SPAWN_GATE_TEST_HOLD.held"
        sandbox_held=0
        while [ -e "$SPAWN_GATE_TEST_HOLD" ] && [ "$sandbox_held" -lt 600 ]; do
            sleep 0.1
            sandbox_held=$((sandbox_held + 1))
        done
    fi
    if [ -n "$sandbox_db_url" ]; then
        "$@" </dev/null 9<"$work/sandbox-db-args" 3>&2 2>"$sandbox_tmp.bwrap" 4>"$sandbox_tmp.started" ||
            sandbox_status=$?
    else
        "$@" </dev/null 3>&2 2>"$sandbox_tmp.bwrap" 4>"$sandbox_tmp.started" || sandbox_status=$?
    fi
    [ ! -s "$sandbox_tmp.started" ] || sandbox_started=1
    if [ -s "$sandbox_tmp.bwrap" ]; then
        sandbox_said=$(cat "$sandbox_tmp.bwrap")
        # Nothing bwrap said is lost: the caller's stderr gets it, as before.
        cat "$sandbox_tmp.bwrap" >&2
    fi
    rm -f "$sandbox_tmp.started" "$sandbox_tmp.bwrap"
    chmod -R u+w "$sandbox_tmp" 2>/dev/null || true
    rm -rf "$sandbox_tmp"
    return "$sandbox_status"
}

# sandbox <tree> <command...>: `sandbox_once`, started once more when the
# first one never ran the command because a path it was to mask changed
# between the check and bwrap's mount (a host process's temporary socket
# directory, removed in those milliseconds, or replaced by a regular file):
# bwrap's own message names that path, and the path is gone now or is a
# regular file. A regular file holds no socket, so leaving it unmasked shows
# nothing that a path gone would not; and candidate code can neither remove
# a host path nor put a file there (the host is read-only in the sandbox).
# Only then: any other failure of bwrap's (the path still a directory, a
# symlink, a socket, a FIFO), or of the command, is the result as it is.
# The second start checks the masks again, so it skips the path. The retry
# is said on stderr (the command's log) and in the gate's own output
# (`gate_times`), so a host race is never silent, nor a verdict on the
# candidate. A gate's masks come from a scan of every socket the host shows
# it, other gates' and suites' scratch under /data/tmp too, so this race
# is with unrelated host activity (2026-11-09: rev-b's suite case 3d' put a
# file where its socket directory was, and a gate of rev-f's suite that had
# scanned it failed to start its sandbox).
sandbox() {
    sandbox_rc=0
    sandbox_once "$@" || sandbox_rc=$?
    [ "$sandbox_rc" -ne 0 ] && [ -z "$sandbox_started" ] || return "$sandbox_rc"
    if sandbox_retry "$@"; then sandbox_rc=0; else sandbox_rc=$?; fi
    # Still never started: bwrap's status is no answer of the command's,
    # whatever the caller would make of it. The fact goes to the note
    # (sandbox_never), which the gate reads before any verdict or cache
    # entry ("A sandbox that never started its command").
    [ -n "$sandbox_started" ] || sandbox_never "$@"
    return "$sandbox_rc"
}
# sandbox_retry <tree> <command...>: the F-race's one more start (above);
# returns the first run's status when it does not apply.
sandbox_retry() {
    sandbox_gone=""
    sandbox_ifs=$IFS
    IFS='
'
    for sandbox_path in $sandbox_masked; do
        case "$sandbox_said" in
        *"$sandbox_path"*)
            if [ ! -e "$sandbox_path" ] && [ ! -L "$sandbox_path" ]; then
                sandbox_gone=$sandbox_path sandbox_became="went away"
                break
            fi
            if [ -f "$sandbox_path" ] && [ ! -L "$sandbox_path" ]; then
                sandbox_gone=$sandbox_path sandbox_became="became a regular file"
                break
            fi
            ;;
        esac
    done
    IFS=$sandbox_ifs
    [ -n "$sandbox_gone" ] || return "$sandbox_rc"
    sandbox_line="sandbox: $sandbox_gone $sandbox_became between the mask check and bwrap's mount (bwrap: $(printf '%s\n' "$sandbox_said" | head -n 1)); started again without it"
    echo "$sandbox_line" >&2
    printf '%s\n' "$sandbox_line" >>"$work/sandbox-retries" 2>/dev/null || :
    sandbox_once "$@"
}

# A sandbox that never started its command. bwrap failing before the
# command ran (its start byte never came: sandbox_once) is no answer of the
# command's, yet its status reads like one: 1 is "unsolved" on the base (and,
# twice, a cached `unsolved` every later gate on that base believes), "does
# not build" or "manifests do not parse" on the candidate, a regression of a
# check; and the socket scan's empty list masks nothing. So every such start
# is noted (sandbox_never: `<tree>|<what>|<line>` in $sandbox_note, the
# gate's own note, or a job's: job_run), and the gate reads the note before
# it states anything: verdict() and the job printers turn it into the fact
# (sandbox_fault), a never-started run writes no cache entry (base_job),
# and the socket scan and the toolchain probe stop the gate at once.
#
# The fact is a host fault (exit 6, `on` `sandbox`, evidence
# `sandbox_never_started`), not `cannot_judge`: that is a scenario's own
# statement, cached; a bwrap failure is the harness's and usually passes.
# One exception: a candidate's tree bwrap will not start in while the
# base's starts (a symlink where the sandbox binds, a path its mounts trip
# on) is the candidate's doing, so that is a rejection quoting bwrap, else
# a candidate could make itself unjudgeable.
sandbox_note=""
# sandbox_never <tree> <command...>: the note of a start that never ran the
# command: the tree, what was to run (the command's first words, past `env`
# and its assignments: never a value of those), bwrap's first line.
sandbox_never() {
    sandbox_never_tree=$1
    shift
    if [ "${1-}" = env ]; then
        shift
        while [ $# -gt 0 ]; do
            case "$1" in *=*) shift ;; *) break ;; esac
        done
    fi
    sandbox_never_what=$(printf '%s %s' "${1-}" "${2-}" | sed 's/ *$//')
    sandbox_never_said=$(printf '%s\n' "$sandbox_said" | sed -n '/./{p;q;}')
    sandbox_never_line="sandbox: $sandbox_never_what in $sandbox_never_tree never started (bwrap exited $sandbox_rc: ${sandbox_never_said:-it said nothing})"
    echo "$sandbox_never_line" >&2
    printf '%s|%s|%s\n' "$sandbox_never_tree" "$sandbox_never_what" "$sandbox_never_line" \
        >>"${sandbox_note:-$work/sandbox-never-started}" 2>/dev/null || :
}
# sandbox_fault <note> [<scenario>]: the fact of a note (above); exits.
# <scenario> names the run in the fact (a job's scenario), else what the
# sandbox was to run.
sandbox_fault() {
    sandbox_checked=1
    sandbox_fault_entry=$(awk -F'|' -v c="$work/candidate" '$1 == c { print; exit }' "$1" 2>/dev/null)
    if [ -n "$sandbox_fault_entry" ] && [ -z "$base_only" ]; then
        # The candidate's tree. Its doing only when bwrap still will not
        # start there and does start in the base's: a start that failed once
        # and starts now was the host's, as is one that starts in neither.
        # sandbox_once, never sandbox: a probe notes nothing.
        sandbox_once "$work/candidate" true >/dev/null 2>&1 || :
        sandbox_fault_candidate=$sandbox_started
        sandbox_once "$work/baseline" true >/dev/null 2>&1 || :
        sandbox_fault_base=$sandbox_started
        # Once more in the candidate's, after the base's: a start that a
        # host race failed once (as the F-race does) does not make a
        # rejection.
        if [ -z "$sandbox_fault_candidate" ] && [ -n "$sandbox_fault_base" ]; then
            sandbox_once "$work/candidate" true >/dev/null 2>&1 || :
            sandbox_fault_candidate=$sandbox_started
        fi
        if [ -z "$sandbox_fault_candidate" ] && [ -n "$sandbox_fault_base" ]; then
            verdict rejected "the sandbox cannot start in the candidate's tree, where it starts in the base's: ${sandbox_fault_entry#*|*|}"
            exit 0
        fi
    else
        sandbox_fault_entry=$(sed -n '1p' "$1" 2>/dev/null)
    fi
    sandbox_fault_what=${sandbox_fault_entry#*|}
    sandbox_fault_what=${sandbox_fault_what%%|*}
    # Which tree it was, in the fact: the candidate's counts toward the
    # integrator's host-fault cap; the base's and the gate's own (the
    # socket scan) come before any candidate code ran, as a fault on the
    # base does, and do not (integrate.rs base_only_fault).
    case "${sandbox_fault_entry%%|*}" in
    "$work/candidate") sandbox_fault_tree=candidate ;;
    "$work/baseline") sandbox_fault_tree=base ;;
    *) sandbox_fault_tree=gate ;;
    esac
    host_fault "${2:-${sandbox_fault_what:-sandbox}}" sandbox "never started" \
        "${sandbox_fault_entry#*|*|}: a host fault, not a verdict on the candidate" \
        sandbox_never_started "$sandbox_fault_tree"
}

extract() {
    mkdir -p "$2"
    git --git-dir="$repo" archive "$1" | tar -x -C "$2"
}

# The socket scan: every socket the sandbox still shows that its uid may
# connect to (`find -writable`: connect() needs write permission, and a
# read-only mount does not take it away), other than the test database's,
# found inside the sandbox with the host's own find before any candidate
# code runs. Each is hidden from then on: its directory under an empty
# tmpfs when nothing the sandbox needs is in, above or below that directory
# (a socket in /data/tmp/chrome.X when TMPDIR did not name /data/tmp), else
# the socket file alone under /dev/null. The limit: a socket created after
# the scan, in a directory no tmpfs covers, is reachable.
mkdir "$work/socket-scan"
sandbox "$work/socket-scan" find / \( -path /proc -o -path /sys -o -path /dev \) -prune -o \
    -type s -writable -print >"$work/sockets" 2>/dev/null || true
# A scan that never ran found nothing to mask: the gate stops rather than
# run scenarios beside sockets it did not hide.
if [ -s "$work/sandbox-never-started" ]; then sandbox_fault "$work/sandbox-never-started" socket-scan; fi
# needed_by_sandbox <directory>: 0 when hiding the directory would hide
# something the sandbox uses (a system directory, PATH, a bound path), or
# the directory is one of those or above one.
sandbox_paths=$(printf '%s\n' /bin /etc /lib /lib32 /lib64 /libx32 /nix /gnu /opt /sbin /usr \
    "$work" "$scenarios" "$HOME" "$cargo_home" "$rustup_home" "$controller/scripts" "$gate_scripts" \
    "$controller/web/node_modules" "$scenario" "$db_socket" "$PATH" | tr ':' '\n'
printf '%s%s' "$home_binds" "$hidden_path_dirs")
needed_by_sandbox() {
    [ "$1" = / ] && return 0
    printf '%s\n' "$sandbox_paths" | awk -v dir="${1%/}" '
        $0 == "" || $0 == "/" { next }
        { sub(/\/$/, "") }
        index(dir "/", $0 "/") == 1 || index($0 "/", dir "/") == 1 { found = 1; exit }
        END { exit !found }'
}
while IFS= read -r found_socket; do
    [ "$found_socket" = "$db_socket" ] && continue
    found_dir=$(dirname "$found_socket")
    if needed_by_sandbox "$found_dir"; then
        mask="file $found_socket"
    else
        mask="directory $found_dir"
    fi
    case "
$socket_masks" in *"
$mask
"*) continue ;; esac
    socket_masks="$socket_masks$mask
"
done <"$work/sockets"
[ -z "$socket_masks" ] ||
    echo "| sockets hidden from the sandbox: $(printf '%s' "$socket_masks" | grep -c .)"

# 0 = built (or nothing to build), 2 = did not build.
build() {
    [ -f "$scenarios/build" ] || return 0
    # The suite's seam (SPAWN_GATE_TEST_HOLD) holds this sandbox: the build
    # is where a vanished mask was a false "does not build". A hold file
    # that says `base-run` holds a base job's run instead (job_run).
    sandbox_hold_here=1
    [ "$(sandbox_hold_at)" != base-run ] || sandbox_hold_here=""
    build_status=0
    sandbox "$1" env ${gate_build_jobs:+"CARGO_BUILD_JOBS=$gate_build_jobs"} sh "$scenarios/build" >"$work/build.log" 2>&1 ||
        build_status=$?
    sandbox_hold_here=""
    [ "$build_status" -eq 0 ] || {
        sed 's/^/| build: /' "$work/build.log" | tail -n 30
        return 2
    }
}

# Host scenario slots. SPAWN_GATE_JOBS is one gate's own share, and every
# gate on a host once ran that many: an owner gate beside the integrator's
# (or the deploy's base-cache warm-up, which is a gate run too) doubled the
# host's scenario load, and on 2026-11-08 two gates' heavy scenarios at
# once starved a test past its wait and a task was rejected wrongly. So the
# host has one budget: every scenario run, from every gate, takes one of
# SPAWN_GATE_HOST_JOBS slots first, and a run waits when they are all
# taken.
#
# The claim sits in this file, in `job_run` (through `slot_acquire`), never
# in the integrator around it: whoever starts a gate — `spawn integrate`, an
# owner's hand-run, the deploy's warm-up — takes the same budget, with no
# configuration. A gate running two scenarios holds two slots (one per run's
# own subshell, one fd each, so the same process starting a second scenario
# claims a second slot and never re-counts the first). The number is the
# host's own statement, not each gate's: $HOME/.cache/spawn/gate-slots/host_jobs
# when it exists (the deploy writes it from the integrator's gate
# environment; the owner may edit it), else SPAWN_GATE_HOST_JOBS, else the
# value every gate is given (SPAWN_GATE_JOBS), else the hand-run default. The
# file wins so a hand-run shell cannot raise (or lower) the host's cap by
# carrying another value, and a gate started with a larger value cannot
# exceed it; the gate only reads the file, so no gate that runs first can
# decide it for the others. Without one, the gate says so in its output.
# The slots are files under $HOME/.cache/spawn/gate-slots (not under
# XDG_CACHE_HOME: the budget
# is the user's, and a hand-run gate with any XDG_CACHE_HOME still finds it;
# the gate makes it 0700), held by flock on an fd of the run's subshell: the
# kernel drops a lock when the fd closes, so a killed gate loses no slot and
# a run that never started takes none. A wait of more than one round is
# said while it is going (as a line beside the run's, so a waiting gate and
# a stalled one never look alike), and every ten minutes again while it
# lasts; the run's own block then records how long it waited and the slot
# it took. A wait is bounded: after SPAWN_GATE_SLOT_WAIT_SECS (default 1800,
# 30 min, well inside the integrator's gate wall clock) the run gives up,
# the gate says so, writes the no-room fact with `check` `host scenario
# slot` (its measure the wait, its threshold the budget) and exits 5 — the
# same class as a TMPDIR under its floor ("no room to judge now"): not a
# verdict, the task waits and is judged again after the room backoff, and
# the record says which check fell short. A hung holder therefore costs one
# judging's wait, not the host; the gate never holds a slot while giving up.
# The build hook is not a scenario and takes none (one per gate, and gates
# are few); everything that runs a scenario does — the base's runs and the
# cache warm-up's, the candidate's checks and the task scenario. Gates on
# different hosts have different budgets; the slots are per host, not per
# fleet.
#
# A warm claims last. A `--base-only` run (the deploy's warm-up, spawn
# integrate's warm after a landing) is `low`: it takes a slot only when no
# gate is waiting for one, through a demand lock beside the slots
# ($slot_dir/demand, fd 5; `slot_acquire` says how). So a warm runs in the
# slots the gates leave idle — during their builds, their test listings,
# between them — and a gate never waits behind a warm for more than the one
# run the warm already had when the gate arrived: there is no preemption,
# and a warm runs one job. When a warm and a gate share a base, they share
# its results too: each scenario's entry is judged under its key's lock
# (`base_job`), so whichever starts one first runs it and the other reads
# it.
#
# Parallel runs. The base's scenario runs, and then the candidate's, run
# SPAWN_GATE_JOBS at a time (above), the base's all done before any
# candidate code runs. Each run is a sandbox of its own: its own /tmp (so its
# own scratch, TMPDIR and tmux sockets), network namespace (its own ports:
# `spawn serve`, the fake engines) and PID namespace; a scenario names its
# databases bb_<n>_<name>_$$_<random>, and with a server of the gate's own
# (SPAWN_GATE_PG_IMAGE or SPAWN_GATE_PG_BIN) the server is the gate's alone.
# What runs of one tree share is the tree: cargo locks its target directory,
# and `spawn` is a copy no cargo command writes (see snapshot_bin). A scenario named in `<scenarios-dir>/serial` (one file name
# per line, `#` comments; read only by the gate, like `protected`) runs
# alone: nothing else of the gate runs beside it. That is for a scenario that
# takes a resource of the host every run shares; it is in that scenario's
# base cache key.
#
# The log and the verdict are the serial gate's, whatever finishes first:
# each run's block (its header, its output, its stage lines) is printed
# whole, in the scenario order, once it and every block before it are done,
# and the first failure in that order decides. Once one is known, the runs
# after it are stopped and the rest never start. An objective's second base
# run (an unsolved result needs two runs that agree) follows its first in the
# same slot: runs at other times, beside other neighbours, are better
# evidence of "unsolved" than two runs side by side, and a solved objective
# (most of them) costs one run, not two. A phase's time in the gate time line
# is its wall clock, and each phase says how many ran at once and the sum of
# its runs' own times.
#
# A job: one objective on the base (its runs) or one scenario on the
# candidate, in $jobs_dir/<phase>.<n>/: `scenario`, its runs' output
# (`log.<run>`, stdout and stderr; `out.<run>`, stdout alone: the metric is
# read from there, never from stderr; `status.<run>`), its block (`events`:
# `text <file>` and `stage <ms> <result>|<name>`, replayed by job_print so
# the stage lines go through stage_emit in order), and `result`, written
# last: the job is done. `job.pid` is its subshell; `pid`, while a run is
# going, the subshell bubblewrap is the child of (job_stop above).

# slot_acquire <job>: one of the host's scenario slots (see "Host scenario
# slots"), waiting while every one is taken; sets slot_held (the slot's
# path, empty when there is no budget), slot_n (its number) and
# slot_wait_ms. Taken in the run's own subshell, on fd 6, so the lock goes
# when the run's processes do. A wait past the first round leaves <job>/waiting
# for the gate's own loop to print, so a waiting gate is visible while it
# waits; the run's block then says how long it waited and the slot it took.
#
# Priority, on the demand lock ($slot_dir/demand, fd 5). A normal claimant
# holds it shared from just before its first scan until it holds a slot (or
# gives up): exactly while a gate wants a slot it does not have. A low one
# (a warm) scans only under the lock held exclusively, taken without
# waiting; when it cannot have it, a gate is waiting or scanning, so it
# takes no slot this round. No gate can start or be mid-scan while a warm
# scans, so a warm never takes the slot a waiting gate would have had. What
# is left is no preemption: a gate that arrives while a warm holds a slot
# waits out that one job of the warm (at most one: a warm runs one job, two
# runs at most, and keeps its slot between them: base_job).
slot_acquire() {
    slot_held="" slot_n="" slot_wait_ms=0 slot_said="" slot_gave_up=""
    [ "$gate_slots" -gt 0 ] || return 0
    rm -f "$1/waiting" "$1/waiting.said" "$1/waiting.said.n" 2>/dev/null || :
    slot_from=$(stage_now)
    if [ "$slot_priority" = normal ] && [ -e "$slot_dir/demand" ] &&
        { exec 5>>"$slot_dir/demand"; } 2>/dev/null; then
        flock -s 5 2>/dev/null || exec 5>&-
    fi
    while [ -z "$slot_held" ]; do
        slot_scan=1
        if [ "$slot_priority" = low ]; then
            slot_scan=""
            if [ ! -e "$slot_dir/demand" ]; then
                if [ ! -e "$slot_dir/slot.1" ]; then
                    # The slot files went away with it (a cache root cleaned
                    # under us): judge as before, as the scan does below.
                    gate_slots=0
                    return 0
                fi
                # The demand file alone went away: made again, so the warm
                # does not spin out its whole wait unable to scan. A normal
                # claimant that opened the old one before it went still holds
                # that one: until it has its slot, a warm may scan beside it.
                (umask 077 && : >>"$slot_dir/demand") 2>/dev/null || :
            fi
            if [ -e "$slot_dir/demand" ] && { exec 5>>"$slot_dir/demand"; } 2>/dev/null; then
                flock -n -x 5 2>/dev/null && slot_scan=1
            else
                # No demand lock to be had: no priority, as at the start
                # when it could not be made.
                slot_scan=1
            fi
        fi
        slot_i=1
        while [ -n "$slot_scan" ] && [ "$slot_i" -le "$gate_slots" ]; do
            if [ ! -e "$slot_dir/slot.$slot_i" ]; then
                # The slot files went away (a cache root cleaned under us):
                # judge as before rather than wait forever.
                exec 5>&-
                gate_slots=0
                return 0
            fi
            exec 6>>"$slot_dir/slot.$slot_i"
            if flock -n 6 2>/dev/null; then
                slot_held=$slot_dir/slot.$slot_i
                slot_n=$slot_i
                break
            fi
            exec 6>&-
            slot_i=$((slot_i + 1))
        done
        # A warm holds the demand lock only for its scan.
        [ "$slot_priority" != low ] || exec 5>&-
        if [ -z "$slot_held" ]; then
            slot_wait_ms=$(( $(stage_now) - slot_from ))
            if [ "$slot_wait_ms" -ge $((slot_wait * 1000)) ]; then
                # The bound: no room to judge now (see "Host scenario
                # slots"). Closing fd 5 drops the demand: giving up holds
                # nothing.
                exec 5>&-
                rm -f "$1/waiting" 2>/dev/null || :
                slot_gave_up=1
                return 0
            fi
            [ -n "$slot_said" ] || : >"$1/waiting" 2>/dev/null || :
            slot_said=1
            sleep 0.2
        fi
    done
    exec 5>&-
    [ -z "$slot_said" ] || rm -f "$1/waiting" 2>/dev/null || :
    slot_wait_ms=$(( $(stage_now) - slot_from ))
}
# spoken_wait <ms>: a wait in one line, as a person reads it ("2.1 s",
# "30.0 min").
spoken_wait() {
    if [ "$1" -ge 60000 ]; then
        printf '%s.%s min' "$(($1 / 60000))" "$((($1 % 60000) / 6000))"
    else
        printf '%s.%s s' "$(($1 / 1000))" "$((($1 % 1000) / 100))"
    fi
}
# slot_no_room <job>: the run gave up waiting for a slot: a fact about the
# host, written where only the integrator reads it and printed (the integrator
# reads the file, and its last line here when the write failed, so the slot
# keeps its shape either way), then exit 5 — no verdict, the task waits and
# is judged again. Nothing here holds a slot.
slot_no_room() {
    slot_gave=$(cat "$1/slot-waited" 2>/dev/null || echo 0)
    slot_name=$(cat "$1/scenario" 2>/dev/null || true)
    stage_close no_room
    gate_times
    stage_emit gate no_room "$(($(stage_now) - gate_started))" total
    echo "| $(basename "$slot_name"): gave up waiting for a host scenario slot after $(spoken_wait "$slot_gave") ($slot_busy)"
    json="{\"no_room\":true,\"check\":\"host scenario slot\",\"waited_ms\":$slot_gave,\"budget\":$gate_slots"
    json="$json,\"scenario\":\"$(json_string "$(basename "$slot_name")")\""
    # The check main was red on (SPAWN_GATE_BASE_CHECK), when it passed on
    # this base before any candidate code ran: main is green on it again,
    # whatever this gate ends with (gate.rs reads it from any fact).
    [ -n "${base_check_passed-}" ] && json="$json,\"base_check\":\"$base_check_passed\""
    json="$json,\"base\":\"$base_commit\",\"candidate\":\"$candidate_commit\"}"
    if [ -n "${SPAWN_GATE_VERDICT-}" ]; then
        printf '%s\n' "$json" >"$SPAWN_GATE_VERDICT" 2>/dev/null || :
    fi
    printf '%s\n' "$json"
    exit 5
}
slot_release() {
    [ -n "$slot_held" ] || return 0
    exec 6>&-
    slot_held=""
}

# slot_claim <job>: slot_acquire, and what the run's block says of it;
# fails when it gave up waiting, with the job done (abort, <job>/slot-waited:
# the printed handler states it, exit 5, rather than judging into a starved
# host).
slot_claim() {
    slot_acquire "$1"
    if [ -n "$slot_gave_up" ]; then
        job_say "| gave up waiting for a host scenario slot after $(spoken_wait "$slot_wait_ms") ($slot_busy)"
        printf 'waited %s for a host scenario slot (all %s taken, shared by every gate on this host)\n' \
            "$(spoken_wait "$slot_wait_ms")" "$gate_slots" >"$1/runs"
        printf '%s\n' "$slot_wait_ms" >"$1/slot-waited"
        job_done abort
        return 1
    fi
    if [ "$slot_wait_ms" -ge 200 ]; then
        job_say "| waited $((slot_wait_ms / 1000)).$((slot_wait_ms % 1000 / 100)) s for host scenario slot $slot_n of $gate_slots (shared by every gate on this host)"
    fi
}

# sandbox_hold_at: what the suite's hold file (SPAWN_GATE_TEST_HOLD) asks to
# hold: `base-run`, or the build (anything else).
sandbox_hold_at() {
    [ -n "${SPAWN_GATE_TEST_HOLD-}" ] && sed -n '1p' "$SPAWN_GATE_TEST_HOLD" 2>/dev/null || :
}

# job_run <job> <run> <tree> <scenario>: one run of the scenario in the
# built tree, in the sandbox, holding one of the host's scenario slots;
# returns its exit status. With slot_kept set, the caller already holds
# the slot (a low claimant's base job: below) and keeps it after the run.
job_run() {
    # This job's note of a sandbox that never started (sandbox_never): its
    # printer reads it, whatever the run's status says.
    sandbox_note=$1/never-started
    sandbox_hold_here=""
    if [ "$3" = "$work/baseline" ] && [ "$(sandbox_hold_at)" = base-run ]; then
        sandbox_hold_here=1
    fi
    if [ -z "$slot_kept" ]; then
        slot_claim "$1" || return 125
    fi
    {
        {
            run_status=0
            sandbox "$3" env SPAWN_BIN="$(spawn_bin "$3")" SPAWN_BASELINE_TREE="$work/baseline" \
                CARGO_TARGET_DIR="$3/target" sh "$4" 6>&- 8>&- &
            printf '%s\n' "$!" >"$1/pid"
            wait "$!" || run_status=$?
            rm -f "$1/pid"
            echo "$run_status" >"$1/status.$2"
        } | tee "$1/out.$2"
    } >"$1/log.$2" 2>&1
    [ -n "$slot_kept" ] || slot_release
    return "$(cat "$1/status.$2" 2>/dev/null || echo 1)"
}
# The job's block, written by the job ($job, $job_part): text, and a stage
# line between two texts.
job_say() { printf '%s\n' "$*" >>"$job/text.$job_part"; }
job_output() { sed 's/^/| /' "$job/log.$1" | tail -n 60 >>"$job/text.$job_part"; }
job_stage() { # <ms> <result> <name>
    printf 'text %s\nstage %s %s|%s\n' "$job/text.$job_part" "$1" "$2" "$3" >>"$job/events"
    job_part=$((job_part + 1))
}
job_done() { # <result...>
    printf 'text %s\n' "$job/text.$job_part" >>"$job/events"
    printf '%s\n' "$*" >"$job/result.new" && mv -f "$job/result.new" "$job/result"
}
# job_print <job>: its block, as the serial gate printed it. The time of its
# runs is added up (phase_runs_ms, phase_runs) for the phase's line.
job_print() {
    while IFS= read -r event; do
        case "$event" in
        "text "*) [ ! -f "${event#text }" ] || cat -- "${event#text }" ;;
        "stage "*)
            event=${event#stage }
            run_stage "${event#* }" "${event%% *}"
            ;;
        esac
    done <"$1/events"
}
# run_stage <result>|<name> <ms>: the stage line of a run, counted.
run_stage() {
    phase_runs_ms=$((phase_runs_ms + $2))
    phase_runs=$((phase_runs + 1))
    stage_emit gate "${1%%|*}" "$2" "${1#*|}"
}

# snapshot_bin <tree>: the binary the scenarios run (SPAWN_BIN), copied
# to target/gate/spawn, where no cargo command writes, as soon as the build
# hook has built the tree, and before anything else of the gate runs cargo
# in it. `cargo test` links target/debug/spawn again with the test build's
# features: the dev-dependencies turn on spawn-core's and spawn-host's
# `testing`. That covers check-tests while other scenarios run, and also the
# candidate's test listing, which runs before its scenarios. Until
# 2026-11-09 the candidate's copy was taken after that listing, so the
# candidate's scenarios ran a testing-featured binary and the base's ran
# the build hook's (rev-f, design-gate-time.md 3a). Both trees now run the
# build hook's binary. Copied inside the sandbox: the tree is the
# candidate's. A tree with no such binary keeps the path its build would
# have made.
snapshot_bin() {
    sandbox "$1" sh -c '[ -f target/debug/spawn ] || exit 0
        mkdir -p target/gate && cp target/debug/spawn target/gate/spawn.new &&
        mv -f target/gate/spawn.new target/gate/spawn' >"$work/snapshot.log" 2>&1 || {
        sed 's/^/| snapshot: /' "$work/snapshot.log" | tail -n 5
        return 1
    }
}
spawn_bin() {
    if [ -f "$1/target/gate/spawn" ]; then
        printf '%s' "$1/target/gate/spawn"
    else
        printf '%s' "$1/target/debug/spawn"
    fi
}

serial_list=""
if [ -f "$scenarios/serial" ]; then
    serial_list=$(grep -v '^#' "$scenarios/serial" | grep . || true)
fi
# runs_alone <scenario>: 0 when the serial list names it.
runs_alone() {
    [ -n "$serial_list" ] && printf '%s\n' "$serial_list" | grep -qxF -- "$(basename "$1")"
}

# The order runs start in (design-gate-time.md, cut B). The log and the
# verdict keep the scenario order (above); only the start order changes,
# and only with more than one run at a time: the runs on the serial list
# first, one by one, while nothing else runs (together, not scattered
# between the others, where each would wait for the runs before it to end
# and leave slots idle); then the job's own task and check-tests (`head`:
# the candidate's runs that decide most often); then the rest, longest
# first by the time the scenario's last completed run took in that phase,
# a scenario with no time yet first among them. The longest runs no longer
# start last and leave one slot busy alone at the end: on Spawn's measured
# phases about 80 s each (sim2.py). The times are the host's, under
# ${XDG_CACHE_HOME:-~/.cache}/spawn/gate-times/<the repository's root
# commit>/<phase>.<scenario>: one number, milliseconds, written after a run
# that completed (exit 0, solved or unsolved), read by the next gate of the
# same repository; never in a base cache key: the order changes no result.
# What it costs: a failure late in the scenario order may be found later,
# and a run's neighbours change (what `serial` is for).
gate_times_dir=""
# job_times_dir: the repository's directory of times, made once.
job_times_dir() {
    if [ -z "$gate_times_dir" ]; then
        jt_root=$(git --git-dir="$repo" rev-list --max-parents=0 "$base_commit" 2>/dev/null | LC_ALL=C sort | head -n 1)
        gate_times_dir=${XDG_CACHE_HOME:-$HOME/.cache}/spawn/gate-times/${jt_root:-unknown}
        mkdir -p "$gate_times_dir" 2>/dev/null || :
    fi
    printf '%s' "$gate_times_dir"
}
# job_time <phase> <scenario name>: its last completed run's milliseconds,
# or nothing.
job_time() {
    jt_held=$(cat "$(job_times_dir)/$1.$2" 2>/dev/null) || jt_held=""
    case "$jt_held" in '' | *[!0-9]*) ;; *) printf '%s' "$jt_held" ;; esac
}
# job_time_note <phase> <job>: the time of a job that completed, for the
# next gate's order. Its runs' stage times added (an objective's two base
# runs are one job, run in one slot), and a candidate run's last (`ms`, its
# stage line said when it is printed).
job_time_note() {
    case "$(cat "$2/result" 2>/dev/null)" in 0 | solved | unsolved) ;; *) return 0 ;; esac
    [ ! -e "$2/slot-waited" ] && [ ! -s "$2/never-started" ] || return 0
    jn_ms=$(awk '$1 == "stage" { ms += $2 } END { print ms + 0 }' "$2/events" 2>/dev/null) || return 0
    jn_last=$(cat "$2/ms" 2>/dev/null) || jn_last=0
    case "$jn_last" in '' | *[!0-9]*) jn_last=0 ;; esac
    jn_ms=$((jn_ms + jn_last))
    [ "${jn_ms:-0}" -gt 0 ] 2>/dev/null || return 0
    jn_file=$(job_times_dir)/$1.$(basename "$(cat "$2/scenario")")
    printf '%s\n' "$jn_ms" >"$jn_file.$$" 2>/dev/null && mv -f "$jn_file.$$" "$jn_file" 2>/dev/null ||
        rm -f "$jn_file.$$" 2>/dev/null || :
}
# job_order <phase> <count>: the start order of $jobs_dir/<phase>.1 ..
# <count> (above), their numbers in jo_order; said in the log when it is
# not the scenario order.
job_order() {
    jo_i=1 jo_known=0 jo_rest=0
    : >"$work/order.$1"
    while [ "$jo_i" -le "$2" ]; do
        jo_job=$jobs_dir/$1.$jo_i
        if [ -e "$jo_job/alone" ]; then
            jo_group=0 jo_ms=0
        elif [ -e "$jo_job/head" ]; then
            jo_group=1 jo_ms=0
        else
            jo_group=2
            jo_ms=$(job_time "$1" "$(basename "$(cat "$jo_job/scenario")")")
            # A result already (a cache hit) never starts: not counted.
            if [ ! -e "$jo_job/result" ]; then
                jo_rest=$((jo_rest + 1))
                [ -z "$jo_ms" ] || jo_known=$((jo_known + 1))
            fi
            [ -n "$jo_ms" ] || jo_ms=999999999999
        fi
        [ "$gate_jobs" -gt 1 ] || jo_group=0 jo_ms=0
        printf '%s %s %s\n' "$jo_group" "$jo_ms" "$jo_i" >>"$work/order.$1"
        jo_i=$((jo_i + 1))
    done
    jo_order=$(sort -k1,1n -k2,2nr -k3,3n "$work/order.$1" | awk '{ printf "%s%s", sep, $3; sep = " " }')
    if [ "$jo_order" != "$(seq -s ' ' 1 "$2")" ] && [ "$2" -gt 0 ]; then
        jo_names=""
        for jo_i in $jo_order; do
            [ ! -e "$jobs_dir/$1.$jo_i/result" ] || continue
            jo_names="$jo_names $(basename "$(cat "$jobs_dir/$1.$jo_i/scenario")")"
        done
        [ -z "$jo_names" ] ||
            echo "| $1 start order (serial list first, then the task and check-tests, then longest first by the last runs' times, $jo_known of $jo_rest known):$jo_names"
    fi
}

# run_jobs <phase> <count> <start> <finished> <printed>: the jobs
# $jobs_dir/<phase>.1 .. <count>, $gate_jobs at a time, in order. A job with
# a result already (a cache hit) is not started. <start> <job> runs one, in
# a background subshell; <finished> <job> <n> hears of each started job as
# it ends and may lower jobs_stop (no job after it is needed); <printed>
# <job> <n> prints each in order, and may end the gate.
run_jobs() {
    rj_phase=$1 rj_count=$2 rj_start=$3 rj_finished=$4 rj_printed=$5
    jobs_stop=$rj_count
    rj_print=1 rj_running="" rj_busy=0 rj_alone=""
    job_order "$rj_phase" "$rj_count"
    rj_pending=$jo_order
    while [ "$rj_print" -le "$jobs_stop" ]; do
        rj_changed=""
        rj_left="" rj_blocked=""
        for rj_next in $rj_pending; do
            # Past the first decisive run in the scenario order: never needed.
            [ "$rj_next" -le "$jobs_stop" ] || continue
            rj_job=$jobs_dir/$rj_phase.$rj_next
            [ ! -e "$rj_job/result" ] || continue
            # In order: one that cannot start yet holds back every one after it.
            if [ -n "$rj_blocked" ] || [ -n "$rj_alone" ]; then
                rj_left="$rj_left $rj_next"
                continue
            fi
            if [ -e "$rj_job/alone" ]; then
                if [ "$rj_busy" -ne 0 ]; then
                    rj_blocked=1 rj_left="$rj_left $rj_next"
                    continue
                fi
                rj_alone=$rj_next
            elif [ "$rj_busy" -ge "$gate_jobs" ]; then
                rj_blocked=1 rj_left="$rj_left $rj_next"
                continue
            fi
            # The gate's traps are not the job's; a failure inside it is
            # its result (`error`), never a silent end.
            (
                trap - EXIT HUP INT TERM
                "$rj_start" "$rj_job" || :
                [ -e "$rj_job/result" ] || {
                    job=$rj_job
                    job_part=error
                    job_say "| gate: the run of $(basename "$(cat "$rj_job/scenario")") ended without a result"
                    job_done error
                }
            ) </dev/null >>"$rj_job/errors" 2>&1 &
            printf '%s\n' "$!" >"$rj_job/job.pid"
            phase_started=$((phase_started + 1))
            rj_running="$rj_running $rj_next"
            rj_busy=$((rj_busy + 1))
            rj_changed=1
        done
        rj_pending=$rj_left
        rj_still=""
        for rj_i in $rj_running; do
            rj_job=$jobs_dir/$rj_phase.$rj_i
            # A run waiting for a host scenario slot says so while it waits,
            # and every ten minutes again: a stalled gate and a waiting one
            # must never look alike, and a holder that never ends (a hung
            # gate without a timeout) must stay visible.
            if [ -e "$rj_job/waiting" ]; then
                rj_wait_name=$(basename "$rj_job")
                if [ ! -e "$rj_job/waiting.said" ]; then
                    echo "| $rj_wait_name: waiting for a host scenario slot ($slot_busy)"
                    : >"$rj_job/waiting.said" 2>/dev/null || :
                    echo 0 >"$rj_job/waiting.said.n" 2>/dev/null || :
                else
                    rj_wait_from=$(date -r "$rj_job/waiting" +%s 2>/dev/null || echo 0)
                    rj_wait_due=$((($(date +%s) - rj_wait_from) / 600))
                    rj_wait_said=$(cat "$rj_job/waiting.said.n" 2>/dev/null || echo 0)
                    if [ "$rj_wait_due" -gt "$rj_wait_said" ] && [ "$rj_wait_due" -gt 0 ]; then
                        echo "| $rj_wait_name: still waiting for a host scenario slot ($((rj_wait_due * 10)) min; $slot_busy)"
                        echo "$rj_wait_due" >"$rj_job/waiting.said.n" 2>/dev/null || :
                    fi
                fi
            fi
            # A subshell gone without a result (killed by someone else) is
            # an error, not a wait forever.
            if [ ! -e "$rj_job/result" ] && ! kill -0 "$(cat "$rj_job/job.pid")" 2>/dev/null &&
                [ ! -e "$rj_job/result" ]; then
                { echo "gate: the run of $(basename "$(cat "$rj_job/scenario")") was killed"; cat "$rj_job/errors"; } |
                    sed 's/^/| /' >"$rj_job/text.killed"
                printf 'text %s\n' "$rj_job/text.killed" >>"$rj_job/events"
                echo error >"$rj_job/result"
            fi
            if [ -e "$rj_job/result" ]; then
                "$rj_finished" "$rj_job" "$rj_i"
                job_time_note "$rj_phase" "$rj_job"
                [ "$rj_alone" != "$rj_i" ] || rj_alone=""
                rj_busy=$((rj_busy - 1))
                rj_changed=1
            else
                rj_still="$rj_still $rj_i"
            fi
        done
        rj_running=""
        for rj_i in $rj_still; do
            if [ "$rj_i" -gt "$jobs_stop" ]; then
                job_stop "$jobs_dir/$rj_phase.$rj_i"
                [ "$rj_alone" != "$rj_i" ] || rj_alone=""
                rj_busy=$((rj_busy - 1))
                rj_changed=1
            else
                rj_running="$rj_running $rj_i"
            fi
        done
        while [ "$rj_print" -le "$jobs_stop" ] && [ -e "$jobs_dir/$rj_phase.$rj_print/result" ]; do
            "$rj_printed" "$jobs_dir/$rj_phase.$rj_print" "$rj_print"
            rj_print=$((rj_print + 1))
            rj_changed=1
        done
        [ -n "$rj_changed" ] || sleep 0.2
    done
    for rj_i in $rj_running; do
        job_stop "$jobs_dir/$rj_phase.$rj_i"
    done
}

# phase_start: the clock of a phase's runs starts.
phase_start() { # <base|candidate>
    # A phase that starts before any stage (a base read whole from the
    # cache) ends the setup too.
    [ -n "${setup_ms-}" ] || setup_ms=$(($(stage_now) - gate_started))
    phase_open=$1
    phase_at=$(stage_now)
    phase_base_ms=$base_ms
    phase_candidate_ms=$candidate_ms
    phase_runs_ms=0
    phase_runs=0
    phase_started=0
}
# phase_time <base|candidate>: the phase's time is its wall clock, not the
# sum of its runs' (the stage lines added those up); said once.
phase_time() {
    [ "$phase_started" -gt 0 ] || return 0
    phase_started=0
    phase_wall=$(($(stage_now) - phase_at))
    case "$1" in
    base) base_ms=$((phase_base_ms + phase_wall)) ;;
    candidate) candidate_ms=$((phase_candidate_ms + phase_wall)) ;;
    esac
    [ "$phase_runs" -gt 0 ] || return 0
    echo "| $1 runs: $phase_runs in $((phase_wall / 1000)) s, $gate_jobs at a time" \
        "(their own time: $((phase_runs_ms / 1000)) s)"
}

# The changed paths, as they are: no rename detection (a move lists the old
# path too, so a protected file cannot be moved away), no quoting
# (core.quotePath would print "scripts/\303\251.sh", quotes included, and a
# quoted path matches no prefix), NUL-separated. A path holding a newline is
# refused: one line per path is what the checks below read.
git -c core.quotePath=false --git-dir="$repo" diff --no-renames --no-ext-diff --name-only -z \
    "$base_commit" "$candidate_commit" | tr '\n\000' '\001\n' >"$work/changed"
if grep -q "$(printf '\001')" "$work/changed"; then
    echo "| a changed path holds a newline: $(grep "$(printf '\001')" "$work/changed" | tr '\001' '?' | head -n 3 | tr '\n' ' ')"
    verdict rejected "a changed path holds a newline"
    exit 0
fi
changed=$(cat "$work/changed")

# Without a protected list nothing is protected, which is never meant: a
# controller that lost its list (a candidate that moved it away once) judges
# nothing. An owner who means "nothing protected" gives an empty list.
if [ ! -f "$scenarios/protected" ]; then
    verdict rejected "the scenarios directory has no protected list"
    exit 0
fi
# A line `!<prefix>` excepts paths under it from the prefixes above it, so
# the owner can protect a directory but its sources (`web/`, `!web/src/`).
touched=$(printf '%s\n' "$changed" | awk -v list="$scenarios/protected" '
    BEGIN { while ((getline line < list) > 0) if (line != "" && line !~ /^#/) {
        if (substr(line, 1, 1) == "!") allowed[++a] = substr(line, 2); else prefix[++n] = line } }
    { for (i = 1; i <= a; i++) if (index($0, allowed[i]) == 1) next
      for (i = 1; i <= n; i++) if (index($0, prefix[i]) == 1) { print; break } }')
if [ -n "$touched" ]; then
    echo "| candidate touches protected paths:$(echo "$touched" | tr '\n' ' ')"
    verdict rejected "candidate touches protected paths"
    exit 0
fi

# Build scripts and cargo configuration run code at build time, before any
# scenario judges the candidate. No task needs them; refuse them.
buildtime=$(printf '%s\n' "$changed" | grep -E '(^|/)build\.rs$|(^|/)\.cargo/' || true)
if [ -n "$buildtime" ]; then
    echo "| candidate adds build-time code: $(echo "$buildtime" | tr '\n' ' ')"
    verdict rejected "candidate adds build-time code (build.rs or .cargo/)"
    exit 0
fi

# Nothing under a node_modules/ or target/ directory: both are gitignored,
# and git overwrites an ignored file with a committed one when it checks out
# or merges, so a committed web/node_modules/.bin/tsc or target/ file would
# replace the owner's build tools (deploy.sh runs them, and the sandbox
# binds the controller's web/node_modules).
planted=$(printf '%s\n' "$changed" | grep -E '(^|/)(node_modules|target)/' || true)
if [ -n "$planted" ]; then
    echo "| candidate commits build output or installed packages: $(echo "$planted" | head -n 5 | tr '\n' ' ')"
    verdict rejected "candidate changes a path under node_modules/ or target/"
    exit 0
fi

extract "$baseline" "$work/baseline"
extract "$candidate" "$work/candidate"

# `[package] build = "tools/gen.rs"` names a build script by another name.
# Any `build` key in any Cargo.toml is refused, quoted or dotted, in a
# comment or not: a false refusal costs a human a look, a miss runs code.
buildkey=$(cd "$work/candidate" && find . -name Cargo.toml -type f -exec \
    grep -lE '(^|[^A-Za-z0-9_-])["'\'']?build["'\'']?[[:space:]]*=' {} + || true)
if [ -n "$buildkey" ]; then
    echo "| candidate names a build script in: $(echo "$buildkey" | sed 's|^\./||' | tr '\n' ' ')"
    verdict rejected "candidate adds build-time code (a build key in Cargo.toml)"
    exit 0
fi
# What the text check cannot see (an escaped key, a `build.rs` the base
# already had), cargo's own reading of the manifests does: no workspace
# member may have a build script. `cargo metadata --no-deps` reads manifests
# and runs nothing; it runs sandboxed all the same.
if [ -f "$work/candidate/Cargo.toml" ] && command -v cargo >/dev/null 2>&1; then
    if ! sandbox "$work/candidate" cargo metadata --no-deps --offline --format-version 1 \
        >"$work/metadata.json" 2>"$work/metadata.log"; then
        sed 's/^/| metadata: /' "$work/metadata.log" | tail -n 20
        verdict rejected "candidate's manifests do not parse"
        exit 0
    fi
    if grep -q '"custom-build"' "$work/metadata.json"; then
        echo "| a workspace member of the candidate has a build script"
        verdict rejected "candidate adds build-time code (a build script target)"
        exit 0
    fi
    # A proc-macro crate of the workspace runs at compile time too, and the
    # deploy compiles outside any sandbox.
    if grep -q '"proc-macro"' "$work/metadata.json"; then
        echo "| a workspace member of the candidate is a proc-macro crate"
        verdict rejected "candidate adds build-time code (a proc-macro crate)"
        exit 0
    fi
    # A local crate outside the workspace (a path dependency the workspace
    # excludes, or does not include) is compiled too, and `--no-deps` does
    # not list it. The whole dependency graph, offline, in the sandbox: a
    # package with no source (a local path) and a proc-macro or build-script
    # target is refused when the base has none at that path, or when the
    # diff touches its directory. Registry crates are the lockfile's, which
    # a reviewer reads; they are not this check's.
    if ! command -v jq >/dev/null 2>&1; then
        echo "gate: jq is required to read cargo metadata" >&2
        exit 3
    fi
    # local_build_time <writable dir> <tree>: `<directory>\t<name>` of each
    # local package with a build-time target, the directory relative to the
    # tree.
    # The host's platform only: the graph of every platform names crates
    # (wasm's js-sys) this host never fetched, and offline cannot resolve
    # them; this host's build compiles only its own platform's.
    host_triple=$(cd "$work" && rustc -vV 2>/dev/null | sed -n 's/^host: //p')
    local_build_time() {
        sandbox "$1" cargo metadata --offline --format-version 1 --manifest-path "$2/Cargo.toml" \
            ${host_triple:+--filter-platform "$host_triple"} \
            >"$work/metadata-full.json" 2>>"$work/metadata.log" || return 1
        jq -r --arg tree "$2/" '
            .packages[] | select(.source == null)
            | select([.targets[].kind[]] | any(. == "proc-macro" or . == "custom-build"))
            | "\(.manifest_path | ltrimstr($tree) | sub("/?Cargo.toml$"; ""))\t\(.name)"' \
            "$work/metadata-full.json" | LC_ALL=C sort
    }
    : >"$work/build-time.base"
    if ! local_build_time "$work/candidate" "$work/candidate" >"$work/build-time.candidate"; then
        sed 's/^/| metadata: /' "$work/metadata.log" | tail -n 20
        verdict rejected "candidate's dependency graph does not resolve offline"
        exit 0
    fi
    # The base's: from its read-only tree, cargo writing nowhere but a
    # directory of its own. A base cargo cannot read counts as having none.
    if [ -s "$work/build-time.candidate" ] && [ -f "$work/baseline/Cargo.toml" ]; then
        mkdir -p "$work/base-metadata"
        local_build_time "$work/base-metadata" "$work/baseline" >"$work/build-time.base" ||
            : >"$work/build-time.base"
    fi
    new_build_time=$(LC_ALL=C comm -23 "$work/build-time.candidate" "$work/build-time.base" | cut -f2)
    # One the base had, at the same path, is new to the candidate when the
    # diff touches its directory.
    edited_build_time=$(LC_ALL=C comm -12 "$work/build-time.candidate" "$work/build-time.base" |
        while IFS='	' read -r crate_dir crate_name; do
            printf '%s\n' "$changed" | awk -v dir="$crate_dir/" 'dir == "/" || index($0, dir) == 1 { found = 1 } END { exit !found }' &&
                printf '%s\n' "$crate_name"
        done || true)
    new_build_time=$(printf '%s\n%s\n' "$new_build_time" "$edited_build_time" | grep . || true)
    if [ -n "$new_build_time" ]; then
        echo "| local crates with build-time code, new or changed: $(echo "$new_build_time" | tr '\n' ' ')"
        verdict rejected "candidate adds build-time code (a local proc-macro or build script outside the workspace: $(echo "$new_build_time" | tr '\n' ' ' | sed 's/ $//'))"
        exit 0
    fi
fi

# The candidate's own lints judge it (check-clippy): it may not relax them
# (the rules are in the header). Text only, on the exported trees, before
# anything is built. The test ratchet runs after the build, below.

# lint_config <tree>: every [lints]/[workspace.lints] line (and `lints.`
# key) of every Cargo.toml, and every clippy.toml, one fact per line.
lint_config() {
    (
        cd "$1" || exit 1
        find . -name Cargo.toml -type f | sort | while IFS= read -r manifest; do
            awk -v file="$manifest" '
                { sub(/#.*/, "") }
                /^[[:space:]]*\[/ { inlints = ($0 ~ /^[[:space:]]*\[(workspace\.)?lints([].])/) }
                inlints || /^[[:space:]]*(workspace\.)?lints[[:space:]]*[.=]/ {
                    gsub(/[[:space:]]+/, "")
                    if ($0 != "") print file ": " $0
                }' "$manifest"
        done
        find . \( -name clippy.toml -o -name .clippy.toml \) -type f | sort | while IFS= read -r file; do
            sed "s|^|$file: |" "$file"
        done
    )
}
# A crate new in the candidate that only inherits the workspace's lints
# relaxes nothing: its inheritance, in any of the forms Cargo reads the same
# (`[lints]` with `workspace = true` under it, the dotted
# `lints.workspace = true`, the inline `lints = { workspace = true }`, any
# spacing: lint_config drops whitespace), is left out of the comparison for
# a manifest the base does not have.
# Without this a new crate that inherits the lints was refused while one with
# no [lints] table at all (lint-free) passed (spawn-kdf, 2026-11-09).
lint_config_inherits_new() { # <base tree> <candidate tree>
    (cd "$1" && find . -name Cargo.toml -type f | sort) >"$work/lint-base-manifests"
    lint_config "$2" | awk -v known="$work/lint-base-manifests" '
        BEGIN { while ((getline line < known) > 0) base[line] = 1 }
        { split($0, part, ": "); file = part[1]; rest = substr($0, length(file) + 3) }
        !(file in base) && (rest == "[lints]" || rest == "workspace=true" ||
                            rest == "lints.workspace=true" || rest == "lints={workspace=true}") { next }
        { print }'
}
if [ "$(lint_config "$work/baseline")" != "$(lint_config_inherits_new "$work/baseline" "$work/candidate")" ]; then
    echo "| candidate changes lint configuration ([lints] tables or clippy.toml)"
    verdict rejected "candidate changes lint configuration"
    exit 0
fi
# lint_free_new <base tree> <candidate tree>: every crate new in the
# candidate (a Cargo.toml with a [package] table the base does not have)
# that does not inherit the workspace's lints (`[lints]` with
# `workspace = true`, or the dotted or inline form of it), one manifest per
# line. A crate with no [lints] table
# gets no lints at all: neither the workspace's nor any of its own, so every
# check-clippy rule passes over it unread. Inheriting is required, not only
# allowed (77fc38ae allowed it).
lint_free_new() {
    new_lints=$(lint_config "$2")
    (
        cd "$2" || exit 1
        find . -name Cargo.toml -type f | sort | while IFS= read -r manifest; do
            [ -f "$1/$manifest" ] && continue
            awk '{ sub(/#.*/, "") } /^[[:space:]]*\[package\][[:space:]]*$/ { found = 1 }
                 END { exit !found }' "$manifest" || continue
            if printf '%s\n' "$new_lints" | grep -qxF -- "$manifest: [lints]" &&
                printf '%s\n' "$new_lints" | grep -qxF -- "$manifest: workspace=true"; then
                continue
            fi
            if printf '%s\n' "$new_lints" | grep -qxF -e "$manifest: lints.workspace=true" \
                -e "$manifest: lints={workspace=true}"; then
                continue
            fi
            echo "$manifest"
        done
    )
}
lint_free=$(lint_free_new "$work/baseline" "$work/candidate")
if [ -n "$lint_free" ]; then
    echo "| crates new in the candidate without [lints] workspace = true: $(echo "$lint_free" | tr '\n' ' ')"
    # The reason says the fix: it is where the agent meets the rule.
    reason=$(echo "$lint_free" | sed 's|^\./||; s|^Cargo\.toml$|.|; s|/Cargo\.toml$||' | while IFS= read -r crate; do
        printf '%s; ' "crate ${crate:-.} is new and lacks \`[lints] workspace = true\` in its Cargo.toml; add it"
    done | sed 's/; $//')
    verdict rejected "$reason (a crate new in the candidate must inherit the workspace's lints)"
    exit 0
fi

# crate_lints <tree>: crate-level lint attributes, `file|attribute`, with
# whitespace removed and a multi-line attribute joined.
crate_lints() {
    (
        cd "$1" || exit 1
        find . -name '*.rs' -type f -exec awk '
            FNR == 1 { open = 0 }
            {
                line = $0
                if (!open && match(line, /#![[:space:]]*\[/)) { open = 1; text = ""; line = substr(line, RSTART) }
                if (open) {
                    text = text line
                    if (index(line, "]")) {
                        open = 0
                        gsub(/[[:space:]]+/, "", text)
                        text = substr(text, 1, index(text, "]"))
                        if (text ~ /^#!\[(allow|expect|warn|deny|forbid|cfg_attr)\(/) print FILENAME "|" text
                    }
                }
            }' {} + | sort -u
    )
}
crate_lints "$work/baseline" >"$work/lints.base"
crate_lints "$work/candidate" >"$work/lints.candidate"
relaxed=$( {
    comm -13 "$work/lints.base" "$work/lints.candidate" | grep -E '\|#!\[(allow|expect|cfg_attr)\(' || true
    comm -23 "$work/lints.base" "$work/lints.candidate" | grep -E '\|#!\[(warn|deny|forbid|cfg_attr)\(' || true
})
if [ -n "$relaxed" ]; then
    echo "| candidate relaxes crate-level lints: $(echo "$relaxed" | sed 's|^\./||' | tr '\n' ' ')"
    verdict rejected "candidate relaxes crate-level lints"
    exit 0
fi

# A test function the diff adds or edits may not have an empty body: `{}`,
# or nothing but comments, `;` and `()`. Text only, on the candidate's
# files the diff touches, with comments and string literals blanked first
# (a `"#[test] fn x() {}"` string is not a test). A test is any function
# under an attribute whose last path segment is `test` (#[test],
# #[tokio::test(..)], #[sqlx::test]). What it cannot see: a body that does
# something and asserts nothing, `assert!(true)`, a test that returns
# before its assertions; the base's own empty tests, untouched.
#
# rust_code <file>: the file with comments and the contents of string and
# char literals replaced by spaces, line for line.
rust_code() {
    LC_ALL=C awk '
        {
            line = $0; out = ""; n = length(line); i = 1
            while (i <= n) {
                c = substr(line, i, 1)
                if (depth > 0) {
                    two = substr(line, i, 2)
                    if (two == "/*") { depth++; out = out "  "; i += 2 }
                    else if (two == "*/") { depth--; out = out "  "; i += 2 }
                    else { out = out " "; i++ }
                    continue
                }
                if (quote != "") {
                    if (raw < 0 && c == "\\") { out = out "  "; i += 2; continue }
                    if (c == "\"" && (raw < 0 || substr(line, i + 1, raw) == hashes)) {
                        out = out "\"" hashes; i += 1 + (raw < 0 ? 0 : raw); quote = ""; continue
                    }
                    out = out " "; i++
                    continue
                }
                two = substr(line, i, 2)
                if (two == "//") break
                if (two == "/*") { depth = 1; out = out "  "; i += 2; continue }
                # A raw string starts at `r` (or `br`) that ends no identifier.
                prev = i > 1 ? substr(line, i - 1, 1) : ""
                if (prev == "b") prev = i > 2 ? substr(line, i - 2, 1) : ""
                if (c == "r" && prev !~ /[A-Za-z0-9_]/ && match(substr(line, i), /^r#*"/)) {
                    raw = RLENGTH - 2; hashes = substr(line, i + 1, raw)
                    out = out substr(line, i, RLENGTH); i += RLENGTH; quote = "\""
                    continue
                }
                if (c == "\"") { raw = -1; hashes = ""; quote = "\""; out = out c; i++; continue }
                # A char literal (an escaped one, or one byte); else a lifetime.
                if (c == "'\''") {
                    if (substr(line, i + 1, 1) == "\\" && match(substr(line, i + 3), /'\''/)) {
                        out = out "'\'' '\''"; i += 3 + RSTART; continue
                    }
                    if (substr(line, i + 2, 1) == "'\''") { out = out "'\'' '\''"; i += 3; continue }
                }
                out = out c; i++
            }
            print out
        }' "$1"
}
# empty_tests <file> <touched ranges>: `<line> <name>` of each test with an
# empty body whose lines (attribute to closing brace) meet a touched range
# (`first-last`, space-separated).
empty_tests() {
    rust_code "$1" | LC_ALL=C awk -v ranges="$2" '
        { code = code $0 "\n" }
        END {
            count = split(ranges, range, " ")
            rest = code; offset = 0
            attribute = "#[[:space:]]*\\[[[:space:]]*([A-Za-z_][A-Za-z0-9_]*[[:space:]]*::[[:space:]]*)*test[[:space:]]*[](]"
            while (match(rest, attribute)) {
                start = offset + RSTART
                after = start + RLENGTH - 1
                tail = substr(code, after + 1)
                offset = after; rest = tail
                if (!match(tail, /(^|[^A-Za-z0-9_])fn[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/)) continue
                name = substr(tail, RSTART, RLENGTH); sub(/^.*fn[[:space:]]+/, "", name)
                fn_end = RSTART + RLENGTH - 1
                open = index(substr(tail, fn_end + 1), "{")
                if (!open) continue
                body = substr(tail, fn_end + open + 1)
                if (!match(body, /[^[:space:];()]/) || substr(body, RSTART, 1) != "}") continue
                first = lines(substr(code, 1, start))
                last = lines(substr(code, 1, after + fn_end + open + RSTART))
                for (r = 1; r <= count; r++) {
                    split(range[r], bound, "-")
                    if (bound[1] <= last && bound[2] >= first) { print first " " name; break }
                }
            }
        }
        function lines(text) { return gsub(/\n/, "", text) + 1 }'
}
# The candidate's added and changed lines per .rs file, from the diff (a
# deletion touches the lines on both sides of it).
git -c core.quotePath=false --git-dir="$repo" diff -U0 --no-renames --no-color --no-ext-diff --no-textconv --src-prefix=a/ --dst-prefix=b/ \
    "$base_commit" "$candidate_commit" -- '*.rs' | awk '
    /^\+\+\+ / { file = ($0 == "+++ /dev/null") ? "" : substr($0, 7); next }
    /^@@ / && file != "" {
        split($3, added, ",")
        first = substr(added[1], 2) + 0
        size = (2 in added) ? added[2] + 0 : 1
        last = size == 0 ? first + 1 : first + size - 1
        touched[file] = touched[file] " " first "-" last
    }
    END { for (file in touched) print file "\t" substr(touched[file], 2) }' >"$work/touched-rust"
empty=""
while IFS='	' read -r file ranges; do
    [ -f "$work/candidate/$file" ] || continue
    found=$(empty_tests "$work/candidate/$file" "$ranges" | sed "s|^|$file:|")
    [ -n "$found" ] && empty="$empty$found
"
done <"$work/touched-rust"
if [ -n "$empty" ]; then
    echo "| tests with an empty body the candidate adds or edits: $(printf '%s' "$empty" | tr '\n' ' ')"
    verdict rejected "candidate adds or edits a test with an empty body ($(printf '%s' "$empty" | head -n 1))"
    exit 0
fi

# The base's result for each objective scenario and for the task scenario,
# decided before any candidate code runs: the base shares the test server
# with the candidate, which could otherwise sabotage the base's run and turn
# a solved objective into "unsolved". Exit 0 = solved, 1 = unsolved; any
# other exit is a harness error and aborts the gate (no verdict). An
# unsolved result needs two runs that agree.
#
# The base cache. A result is cached in $base_cache (which the sandbox
# cannot see, let alone write), keyed by everything that decides it:
# - the base commit;
# - the scenario's blob (`git hash-object`);
# - the gate: this file's blob and that of the URL parser it sources (it
#   shapes the sandbox's database URL);
# - the scenarios directory's other files, but its scenarios and the lists
#   only the gate reads (`protected`, `retired-tests`, `serial`): the build hook, the
#   `bind` list's text, helpers a scenario runs from beside it;
# - each directory the `bind` list names, by a fingerprint of what is
#   installed there: the blobs of its `.modules.yaml` and `.pnpm/lock.yaml`
#   (pnpm's record of an install) when either is present, and otherwise a
#   listing of everything under it, symlinks not followed (path, type, size,
#   modification time, link target). A `pnpm install` rewrites those, so the
#   next gate judges the base again; a file edited inside a directory that
#   has the markers, without an install, is not seen;
# - the files the scenario names on its code lines, and those they name in
#   turn: a path `scripts/...` is the controller's (the scenarios source
#   `$controller/scripts/blackbox/assertions.sh` and
#   `$controller/scripts/pg-url.sh`), a directory is all of its files, a
#   `<name>.sh` is a sibling scenario; one that does not exist is keyed as
#   missing. A controller file no scenario names (the black-box runner,
#   the e2e) changes no objective's key. A code line that reaches into
#   scripts/ indirectly (`scripts/$name`, `cd "$controller/scripts"` and a
#   relative path after it) keys all of the controller's scripts/;
# - the tools the scenarios run, asked in the sandbox inside the base tree
#   (whose rust-toolchain.toml picks the Rust toolchain): `rustc -vV`,
#   `cargo -V`, `node --version`, `psql -V`, `git --version`, `tmux -V`,
#   `python3 -V`, `jq --version`, and the test server's
#   `server_version_num`, asked over its socket.
# An entry is written only here, after a completed base run, and only
# "solved" or "unsolved"; an entry that cannot be read, or holds anything
# else, is a miss, judged again and rewritten. Candidate runs are never
# cached. A hit touches its entry: spawn-maintenance removes entries no
# gate used for 14 days (scripts/maintenance.sh). The log says which
# results came from the cache.
#
# - whether the scenario runs alone (the `serial` list): a scenario put on
#   it because it collides with its neighbours is judged again, alone;
# - with a server of the gate's own, the id of its programs (the image's,
#   or a hash of SPAWN_GATE_PG_BIN's), with the server's version: a result taken on a clean server is not one
#   taken on the shared server.
# A lock per key (flock on `<key>.lock` beside the entry, hidden with it)
# makes a gate that misses wait while another gate of this host (the
# deploy's base-only run) judges the same key, and then read its entry,
# instead of judging it twice.
#
# Not keyed: the state candidate code leaves on the shared test server
# (roles, databases, settings), when the gate has no server of its own
# (SPAWN_GATE_PG_IMAGE or SPAWN_GATE_PG_BIN); integrate.rs runs one such
# gate at a time.
blob() { git hash-object --no-filters -- "$1"; }
gate_key="gate $(blob "$0")"
[ -f "$(dirname "$0")/../pg-url.sh" ] && gate_key="$gate_key $(blob "$(dirname "$0")/../pg-url.sh")"
[ -f "$(dirname "$0")/gate-pg.sh" ] && gate_key="$gate_key $(blob "$(dirname "$0")/gate-pg.sh")"
scenarios_key=$(cd "$scenarios" && find . -type f ! -path './protected' ! -path './retired-tests' ! -path './serial' |
    grep -vE '^\./[^/]*\.sh$' | LC_ALL=C sort |
    while IFS= read -r file; do printf '%s %s\n' "$(blob "$file")" "$file"; done |
    sha256sum | cut -d' ' -f1)
# bind_fingerprint <dir>: what is installed in a bound directory (above).
bind_fingerprint() {
    bind_marked=""
    for bind_marker in .modules.yaml .pnpm/lock.yaml; do
        if [ -f "$1/$bind_marker" ]; then
            printf '%s %s\n' "$bind_marker" "$(blob "$1/$bind_marker")"
            bind_marked=1
        fi
    done
    [ -n "$bind_marked" ] ||
        printf 'listing %s\n' "$(find "$1" -printf '%P %y %s %T@ %l\n' 2>/dev/null | LC_ALL=C sort |
            sha256sum | cut -d' ' -f1)"
}
binds_key=none
if [ -f "$scenarios/bind" ]; then
    binds_key=$(while IFS= read -r path || [ -n "$path" ]; do
        case "$path" in '' | '#'*) continue ;; esac
        printf '%s\n%s\n' "$path" "$(bind_fingerprint "$path")"
    done <"$scenarios/bind" | sha256sum | cut -d' ' -f1)
fi
# named_by <file>: the paths a file names on its code lines, one per line
# (see above). Comments do not count: this file's own comments name
# scripts/blackbox, and a scenario that mentions the gate would otherwise
# key every black-box suite.
named_by() {
    sed -e 's/^[[:space:]]*#.*//' -e 's/[[:space:]]#.*//' "$1" 2>/dev/null >"$work/deps.code" || :
    grep -oE 'scripts/[A-Za-z0-9_./-]*[A-Za-z0-9_-]' "$work/deps.code" | sed "s|^|$controller/|" || true
    grep -oE '[A-Za-z0-9_-]+\.sh' "$work/deps.code" | while IFS= read -r name; do
        [ ! -f "$scenarios/$name" ] || printf '%s\n' "$scenarios/$name"
    done
    # scripts/ as a bare directory, or a scripts/ path that goes on in a
    # variable: what it reaches cannot be read off the text.
    if grep -qE 'scripts/?([^A-Za-z0-9_./-]|$)|scripts/[A-Za-z0-9_./-]*[$`{]' "$work/deps.code"; then
        printf '%s\n' "$controller/scripts"
    fi
}
# depends_on <scenario>: `<blob> <path>` (or `missing <path>`) of the
# scenario and everything it names, transitively, sorted.
depends_on() {
    printf '%s\n' "$1" >"$work/deps.files"
    while :; do
        while IFS= read -r file; do
            [ -f "$file" ] && named_by "$file"
        done <"$work/deps.files" | while IFS= read -r path; do
            if [ -d "$path" ]; then find "$path" -type f; else printf '%s\n' "$path"; fi
        done | cat "$work/deps.files" - | LC_ALL=C sort -u >"$work/deps.next"
        cmp -s "$work/deps.next" "$work/deps.files" && break
        mv "$work/deps.next" "$work/deps.files"
    done
    # Named relative to the scenarios directory or the controller: content
    # decides, not where the checkout lies.
    while IFS= read -r file; do
        name=${file#"$scenarios"/}
        [ "$name" != "$file" ] || name=${file#"$controller"/}
        if [ -f "$file" ]; then printf '%s %s\n' "$(blob "$file")" "$name"; else printf 'missing %s\n' "$name"; fi
    done <"$work/deps.files" | LC_ALL=C sort
}
# toolchain_probe <tree> <file>: the tools the scenarios run, asked in the
# sandbox inside <tree> (its rust-toolchain.toml picks the Rust toolchain),
# with the server's version, into <file>; prints the file's sha256.
toolchain_probe() {
    # The server's version through libpq's variables (pg-url.sh), never a
    # URL on psql's command line: it holds the password.
    sandbox "$1" sh -c '
        for tool in "rustc -vV" "cargo -V" "node --version" "psql -V" "git --version" \
            "tmux -V" "python3 -V" "jq --version"; do
            $tool 2>&1
            echo "${tool%% *}: $?"
        done
        if [ -z "${SPAWN_TEST_DATABASE_URL-}" ]; then
            echo "server: none"
        elif [ -f "$1" ] && . "$1" && pg_url_export "$SPAWN_TEST_DATABASE_URL"; then
            PGDATABASE=$pg_url_database PGCONNECT_TIMEOUT=10 psql -XtAc "show server_version_num" 2>&1
            echo "server: $?"
        else
            echo "server: unreadable"
        fi' toolchain "$gate_scripts/pg-url.sh" >"$2" 2>&1 || true
    # A probe that never ran would key the base cache by bwrap's message.
    if [ -s "$work/sandbox-never-started" ]; then sandbox_fault "$work/sandbox-never-started" toolchain; fi
    [ -z "$gate_pg_id" ] || echo "server: the gate's own, $gate_pg_kind $gate_pg_id" >>"$2"
    sha256sum <"$2" | cut -d' ' -f1
}
# The base's toolchain, asked once, when the first objective needs a key.
toolchain_key=""
toolchain() {
    [ -z "$toolchain_key" ] || return 0
    toolchain_key=$(toolchain_probe "$work/baseline" "$work/toolchain")
    echo "| base cache: toolchain $(sed -n 's/^release: /rustc /p' "$work/toolchain" | head -n 1)" \
        "$(sed -n 's/^\(cargo [^ ]*\).*/\1/p' "$work/toolchain" | head -n 1)" \
        "$(sed -n 's/^\(psql\) (PostgreSQL) \([^ ]*\).*/\1 \2/p' "$work/toolchain" | head -n 1)" \
        "$(toolchain_server)"
}
# toolchain_server: what the probe saw of the test server, as one phrase:
# its version, "none" only when the gate had no test database, "unreadable"
# when the gate's own pg-url.sh was not visible in the sandbox, or
# "unreachable" with psql's status and first line — never "none" for a
# server the gate runs (2026-11-09: Supply PA's gate said "server none"
# beside its own server, because the probe could not read pg-url.sh).
toolchain_server() {
    tc_version=$(sed -n 's/^\([0-9]\{5,6\}\)$/\1/p' "$work/toolchain" | head -n 1)
    tc_status=$(sed -n -E 's/^server: (none|unreadable|[0-9]+)$/\1/p' "$work/toolchain" | head -n 1)
    if [ -n "$tc_version" ]; then
        printf 'server %s' "$tc_version"
    elif [ "$tc_status" = none ]; then
        printf 'server none'
    elif [ "$tc_status" = unreadable ]; then
        printf "server unreadable (the gate's pg-url.sh is not visible in the sandbox)"
    else
        tc_said=$(awk '/^jq: [0-9]+$/ { after = 1; next } after && !/^server: / { print; exit }' \
            "$work/toolchain" | cut -c1-160)
        printf 'server unreachable: psql exit %s%s' "${tc_status:-unknown}" "${tc_said:+ ($tc_said)}"
    fi
}
# entry_result <entry>: solved, unsolved or cannot_judge (see "A scenario
# that cannot be judged here"), as the entry holds it; nothing for an entry
# that is missing, unreadable, or holds anything else. A seeded entry
# ("Seeds from the accepting gate", below) is `solved` and a second line,
# `seeded-by-gate <candidate commit> at <UTC time>`, and nothing else.
entry_result() {
    entry_held=""
    if [ -e "$1" ] || [ -L "$1" ]; then
        entry_held=$(cat -- "$1" 2>/dev/null) || entry_held=""
    fi
    case "$entry_held" in
    solved | unsolved | cannot_judge) printf '%s' "$entry_held" ;;
    "solved
seeded-by-gate "*)
        [ "$(printf '%s\n' "$entry_held" | wc -l)" -eq 2 ] &&
            printf '%s\n' "$entry_held" | sed -n 2p |
            grep -qE '^seeded-by-gate [0-9a-f]{40} at [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$' &&
            printf solved
        ;;
    esac
}
# entry_origin <entry>: the seed line of a seeded entry, or nothing.
entry_origin() {
    sed -n '2s/^seeded-by-gate \([0-9a-f]\{40\}\) at \(.*\)$/seeded by the gate of \1 at \2/p' -- "$1" 2>/dev/null || :
}
# base_job <job>: the base's result for one objective, in the background,
# under its key's lock (above); <job>/entry is its cache entry. A completed
# run, and only one, writes the entry. One that cannot be written costs the
# next gate a run, not this one its verdict.
base_job() {
    job=$1 job_part=0
    objective=$(cat "$job/scenario")
    entry=$(cat "$job/entry")
    name=$(basename "$objective")
    slot_kept="" result=""
    if command -v flock >/dev/null 2>&1 &&
        { command exec 8>>"$entry.lock"; } 2>/dev/null; then
        if [ "$slot_priority" = low ] && [ "$gate_slots" -gt 0 ]; then
            # A low claimant takes its slot before the key's lock, and the
            # lock only without waiting, then keeps both for its runs: it
            # never waits for a slot while holding a key a gate may be
            # waiting on (that gate would wait out the warm's whole slot
            # wait, silently). A gate holding the key while it waits for a
            # slot is no inversion: no warm scans while a gate waits.
            while :; do
                slot_claim "$job" || return 0
                if flock -n 8; then
                    slot_kept=1
                    break
                fi
                slot_release
                if [ -n "$base_only" ]; then
                    # Another gate judges this key and writes its entry: a
                    # warm has nothing to add.
                    job_say "| == base: $name: skipped (another gate is judging it; that gate writes the entry)"
                    job_done skipped
                    return 0
                fi
                # A whole gate needs the result: it waits for the key
                # holding no slot, and reads the entry the other gate left.
                flock 8
                result=$(entry_result "$entry")
                [ -z "$result" ] || break
                flock -u 8
            done
        else
            flock 8
        fi
        [ -n "$result" ] || result=$(entry_result "$entry")
        if [ -n "$result" ]; then
            base_job_release
            touch -c -- "$entry" 2>/dev/null || :
            job_say "| == base: $name: $result (cached: another gate judged it meanwhile)"
            job_done cached "$result"
            return 0
        fi
    fi
    run_name="base: $name"
    job_say "| == $run_name"
    run_at=$(stage_now)
    first=0
    job_run "$job" 1 "$work/baseline" "$objective" || first=$?
    [ ! -e "$job/slot-waited" ] || return 0
    base_never_started && return 0
    job_output 1
    result=""
    case "$first" in
    0) result=solved ;;
    1)
        job_stage "$(($(stage_now) - run_at))" "exit 1" "$run_name"
        run_name="base: $name again (unsolved needs two runs that agree)"
        job_say "| == $run_name"
        run_at=$(stage_now)
        again=0
        job_run "$job" 2 "$work/baseline" "$objective" || again=$?
        [ ! -e "$job/slot-waited" ] || return 0
        base_never_started && return 0
        job_output 2
        [ "$again" -eq 1 ] && result=unsolved
        first="1, then $again"
        ;;
    6)
        # The scenario says the host cannot judge it here (see "A scenario
        # that cannot be judged here"): a fact about the host, the same on
        # every run, so it is not run again. It is not unsolved either: the
        # candidate's own run of it is not a regression (below).
        job_stage "$(($(stage_now) - run_at))" "cannot judge" "$run_name"
        result=cannot_judge
        ;;
    esac
    if [ -z "$result" ]; then
        printf '%s\n' "$first" >"$job/runs"
                # A base that disagrees with itself (1, then 0) keeps objective 033's
        # statement and exit 4: base_printed exits with that pair below, and
        # gate.rs records it `host_fault` with evidence `base_disagrees`.
        # Any other harness error on the base is exit 6 with its fact
        # (`host_fault` in base_printed): a base-only host fault that counts
        # toward no halt.
        if [ "$first" = "1, then 0" ]; then
            job_say "| $name exited $first on the base: a harness error, not a verdict; the gate aborts"
        else
            job_say "| $name exited $first on the base: a harness error, not a verdict on the candidate"
        fi
        job_stage "$(($(stage_now) - run_at))" aborted "$run_name"
        job_done abort
        base_job_release
        return 0
    fi
    if [ -d "$entry" ] && [ ! -L "$entry" ]; then rm -rf -- "$entry"; fi
    if ! { printf '%s\n' "$result" >"$entry.$$" && mv -fT -- "$entry.$$" "$entry"; } 2>/dev/null; then
        rm -f -- "$entry.$$"
        job_say "| base cache: could not write the entry of $name"
    fi
    job_say "| == base: $name: $result"
    job_stage "$(($(stage_now) - run_at))" "$result" "$run_name"
    job_done "$result"
    base_job_release
}
# base_never_started: the run's sandbox never started its command
# (sandbox_never): no result, so no cache entry; base_printed states the
# fact. 0 when so.
base_never_started() {
    [ -s "$job/never-started" ] || return 1
    job_say "| == base: $name: its sandbox never started it; no result, nothing cached"
    job_done abort
    base_job_release
    return 0
}
# base_job_release: the slot a low claimant kept for its runs (above), let go
# when the job is done. The job's subshell exiting would drop it too; this
# says so where it happens.
base_job_release() {
    [ -z "$slot_kept" ] || { slot_kept=""; slot_release; }
}
# base_finished <job> <n>: an abort needs no run after it.
base_finished() {
    case "$(cat "$1/result")" in abort | error) [ "$2" -ge "$jobs_stop" ] || jobs_stop=$2 ;; esac
}
# base_printed <job> <n>: its block, and its result on record.
base_printed() {
    job_print "$1"
    printed_name=$(basename "$(cat "$1/scenario")")
    # shellcheck disable=SC2046 # the result's words
    set -- "$1" $(cat "$1/result")
    if [ -e "$1/slot-waited" ]; then slot_no_room "$1"; fi
    if [ -s "$1/never-started" ]; then sandbox_fault "$1/never-started" "$printed_name"; fi
    case "$2" in
    cached)
        from_cache="$from_cache $printed_name"
        result=$3
        ;;
    solved | unsolved | cannot_judge)
        run_on_base="$run_on_base $printed_name"
        result=$2
        ;;
    skipped)
        # A warm's key another gate was judging (base_job): that gate
        # writes the entry; this run has no result of it.
        skipped_on_base="$skipped_on_base $printed_name"
        return 0
        ;;
    abort)
        phase_time base
        # Objective 033: a base that disagrees with itself is stated by the
        # gate's own pair and exit 4 (gate.rs `base_disagrees`), never by the
        # host fact too: one run is one record.
        if [ "$(cat "$1/runs" 2>/dev/null)" = "1, then 0" ]; then
            exit 4
        fi
        host_fault "$printed_name" base "$(cat "$1/runs" 2>/dev/null)" \
            "$printed_name exited $(cat "$1/runs" 2>/dev/null) on the base, before any candidate code ran: a host or harness fault, not a verdict on the candidate"
        ;;
    *)
        phase_time base
        echo "| $printed_name: the gate could not run it on the base; the gate aborts"
        exit 4
        ;;
    esac
    printf '%s|%s\n' "$result" "$(cat "$1/scenario")" >>"$work/base-results"
}

# objective_key <commit> <toolchain key> <objective>: the key of the
# objective's result on <commit> as a base ("The base cache" above). Not to
# be run in parallel: depends_on writes $work/deps.files.
objective_key() {
    {
        printf 'base %s\nscenario %s\n%s\ntoolchain %s\nscenarios %s\nbinds %s\n' "$1" \
            "$(blob "$3")" "$gate_key" "$2" "$scenarios_key" "$binds_key"
        if runs_alone "$3"; then printf 'runs alone\n'; fi
        depends_on "$3"
    } | sha256sum | cut -d' ' -f1
}

# The base phase: every objective's key (and its result, when the cache
# has it), then one build of the base for those it has not, then their runs.
: >"$work/base-results"
from_cache=""
run_on_base=""
skipped_on_base=""
mkdir -p "$jobs_dir"
base_count=0
base_misses=0
for objective in "$scenarios"/*.sh ${scenario:+"$scenario"}; do
    [ -f "$objective" ] || continue
    if [ "$objective" != "$scenario" ]; then
        case "$(basename "$objective")" in *[!0-9].sh | .sh) continue ;; esac
        grep -q '^# rsi: ratchet' "$objective" && continue
    fi
    grep -qxF "$objective" "$work/base-results.seen" 2>/dev/null && continue
    printf '%s\n' "$objective" >>"$work/base-results.seen"
    objective_name=$(basename "$objective")
    toolchain
    entry=$base_cache/$(objective_key "$base_commit" "$toolchain_key" "$objective")
    base_count=$((base_count + 1))
    job=$jobs_dir/base.$base_count
    mkdir "$job"
    printf '%s\n' "$objective" >"$job/scenario"
    printf '%s\n' "$entry" >"$job/entry"
    if runs_alone "$objective"; then : >"$job/alone"; fi
    result=$(entry_result "$entry")
    if [ -z "$result" ] && { [ -e "$entry" ] || [ -L "$entry" ]; }; then
        echo "| == base: $objective_name: its cache entry cannot be read; judged again" >"$job/text.0"
    fi
    if [ -n "$result" ]; then
        # A used entry stays: maintenance removes the ones untouched.
        touch -c -- "$entry" 2>/dev/null || :
        origin=$(entry_origin "$entry")
        echo "| == base: $objective_name: $result (cached${origin:+, $origin})" >"$job/text.0"
        printf 'text %s\n' "$job/text.0" >"$job/events"
        echo "cached $result" >"$job/result"
    else
        base_misses=$((base_misses + 1))
    fi
done
if [ "$base_misses" -gt 0 ]; then
    section "base: build"
    if ! build "$work/baseline"; then
        verdict rejected "base does not build"
        exit 0
    fi
    if ! snapshot_bin "$work/baseline"; then
        echo "gate: cannot copy the base's spawn for its scenarios" >&2
        exit 3
    fi
    : >"$work/base-built"
    stage_close ok
fi
phase_start base
run_jobs base "$base_count" base_job base_finished base_printed
phase_time base
if [ -n "$from_cache$run_on_base" ]; then
    echo "| base results from the cache:${from_cache:- none}; run on the base:${run_on_base:- none}"
fi
# base_result <scenario>: solved, unsolved, or none (not judged on the base).
base_result() {
    awk -F'|' -v want="$1" '
        $2 == want { print $1; found = 1; exit }
        END { if (!found) print "none" }' "$work/base-results"
}

# Base only: the base's results are in the cache, for the gates that judge
# a candidate on this base next (the deploy starts one after it moves main:
# deploy/deploy.sh). No candidate, no verdict.
if [ -n "$base_only" ]; then
    echo "| base only: $(grep -c . "$work/base-results" || true) results for the base $base_commit" \
        "are in the base cache $base_cache"
    [ -z "$skipped_on_base" ] ||
        echo "| base only: skipped, another gate was judging them:$skipped_on_base"
    exit 0
fi

if [ -n "$scenario" ] && [ "$(base_result "$scenario")" = solved ]; then
    verdict rejected "scenario already passes on the base"
    exit 0
fi

# base_ready: the base built and its spawn copied, once for the gate (a
# check's base run may be the first the gate needs, from several jobs at
# once: one builds while the others wait on its lock, which a killed job
# releases; without flock, as for the base cache's lock, they do not wait).
# 0 = ready.
base_ready() {
    if command -v flock >/dev/null 2>&1 && { command exec 7>>"$work/base-build.lock"; } 2>/dev/null; then
        flock 7
    fi
    if [ ! -e "$work/base-built" ]; then
        if build "$work/baseline" >"$work/base-build.out" 2>&1 &&
            snapshot_bin "$work/baseline" >>"$work/base-build.out" 2>&1; then
            : >"$work/base-built"
        else
            sed 's/^/| /' "$work/base-build.out" | tail -n 20 >>"$job/text.$job_part"
        fi
    fi
    exec 7>&-
    [ -e "$work/base-built" ]
}

# The check main was red on (SPAWN_GATE_BASE_CHECK: the integrator passes
# it while a source's main is red on a check, design-main-red.md), run on
# the base before any candidate code: passing, main is green on it again and
# the verdict says so (`base_check`); failing twice with the same failing
# tests, main is still red (main_red, exit 6), and no candidate is judged on
# it; anything else (once and then not, different tests, no room) is
# neither, said, and the gate judges the candidate as usual.
base_check_passed=""
if [ -n "${SPAWN_GATE_BASE_CHECK-}" ]; then
    base_check_name=$SPAWN_GATE_BASE_CHECK
    case "$base_check_name" in
    check-*.sh) base_check_file=$scenarios/$base_check_name ;;
    *) base_check_file="" ;;
    esac
    case "$base_check_name" in */*) base_check_file="" ;; esac
    if [ -z "$base_check_file" ] || [ ! -f "$base_check_file" ]; then
        echo "| base check: $base_check_name is no check of this gate's: not run"
    else
        section "base: $base_check_name (main was red on it)"
        job=$jobs_dir/base-check
        mkdir -p "$job"
        job_part=0
        : >"$job/text.0"
        if ! base_ready; then
            cat "$job/text.0"
            echo "| base check: the base cannot be built to run $base_check_name: neither red nor green"
        else
            base_check_first=0
            job_run "$job" 1 "$work/baseline" "$base_check_file" || base_check_first=$?
            sed 's/^/| /' "$job/log.1" | tail -n 20
            # Outside run_jobs, so no printer reads the job's note of a
            # sandbox that never started (sandbox_never): read here, before
            # the status, which then says nothing. The base's tree: a host
            # fault before any candidate code ran (sandbox_fault).
            if [ -s "$job/never-started" ]; then sandbox_fault "$job/never-started" "$base_check_name"; fi
            if [ -e "$job/slot-waited" ]; then
                echo "| base check: no scenario slot for $base_check_name: neither red nor green"
            elif [ "$base_check_first" -eq 0 ]; then
                echo "| base check: $base_check_name passes on the base: main is green on it again"
                base_check_passed=$base_check_name
                # Said at once, in the integrator's file, so an exit with no
                # fact of its own (exit 3, a kill) still says it; every fact
                # written after keeps it.
                if [ -n "${SPAWN_GATE_VERDICT-}" ]; then
                    printf '{"base_check":"%s","base":"%s","candidate":"%s"}\n' \
                        "$base_check_passed" "$base_commit" "$candidate_commit" >"$SPAWN_GATE_VERDICT"
                fi
            else
                base_check_again=0
                job_run "$job" 2 "$work/baseline" "$base_check_file" || base_check_again=$?
                sed 's/^/| /' "$job/log.2" | tail -n 20
                if [ -s "$job/never-started" ]; then sandbox_fault "$job/never-started" "$base_check_name"; fi
                if [ ! -e "$job/slot-waited" ] && [ "$base_check_first" -eq 1 ] &&
                    [ "$base_check_again" -eq 1 ] &&
                    shared_failed "$job/out.1" "$job/out.2" "$job/main-red"; then
                    main_red "$base_check_name" "$base_check_first, then $base_check_again on the base, before the candidate" "$job/main-red"
                fi
                echo "| base check: $base_check_name exited $base_check_first, then $base_check_again on the base: neither red nor green"
            fi
        fi
        stage_close ok
        # job_run made this job's note the gate's: the gate's own again, so
        # what the sandbox fails to start from here on is noted where
        # verdict() reads it.
        sandbox_note=""
    fi
fi

section "candidate: build"
if ! build "$work/candidate"; then
    verdict rejected "candidate does not build"
    exit 0
fi
if ! snapshot_bin "$work/candidate"; then
    echo "gate: cannot copy the candidate's spawn for its scenarios" >&2
    exit 3
fi

# runnable_tests <writable dir> <tree> <target dir> <names file>: how many
# tests cargo runs in the tree, from inside the sandbox: libtest's
# `<name>: test` lines of `--list`, minus those of `--list --ignored`. Their
# names go to <names file>, sorted, one line per test (a name two test
# binaries share is there twice), a doctest's ` (line N)` dropped so that
# an edit above it does not rename it.
runnable_tests() {
    list_dir=$1
    list_tree=$2
    list_target=$3
    list_names=$4
    for list_mode in all ignored; do
        set -- --list --format terse
        [ "$list_mode" = ignored ] && set -- "$@" --ignored
        sandbox "$list_dir" env CARGO_TARGET_DIR="$list_target" ${gate_build_jobs:+"CARGO_BUILD_JOBS=$gate_build_jobs"} \
            cargo test --quiet --workspace \
            --manifest-path "$list_tree/Cargo.toml" -- "$@" \
            >"$work/tests.$list_mode" 2>>"$work/tests.log" || return 1
        sed -n 's/: test$//p' "$work/tests.$list_mode" | sed -E 's/ \(line [0-9]+\)$//' |
            LC_ALL=C sort >"$work/names.$list_mode"
    done
    LC_ALL=C comm -23 "$work/names.all" "$work/names.ignored" >"$list_names"
    grep -c . "$list_names" || true
}

# The candidate may not run fewer tests than the base, nor stop running one
# of the base's by name. Read from what cargo runs, so a test deleted,
# ignored, commented out, left in a file no `mod` includes, or behind
# `#[cfg(any())]` is gone; one renamed or moved to another module path is
# gone too (its name is its module path; a move that keeps the path, the
# same `mod tests` in another file, keeps the name). The base tree stays
# read-only: its cargo writes to a target directory of its own. The
# candidate's target directory is the one check-tests reuses. What names
# cannot see: a test kept by name whose body no longer tests anything (the
# empty-body check above catches only an empty one), and one of two
# same-named tests in different crates traded for a third.
if [ -f "$work/baseline/Cargo.toml" ] && [ -f "$work/candidate/Cargo.toml" ]; then
    section "tests: what cargo runs, base and candidate"
    listing_started=$(date +%s)
    mkdir -p "$work/base-target"
    : >"$work/tests.log"
    if ! base_tests=$(runnable_tests "$work/base-target" "$work/baseline" "$work/base-target" \
        "$work/tests.base-names"); then
        sed 's/^/| tests: /' "$work/tests.log" | tail -n 30
        verdict rejected "base tests do not build"
        exit 0
    fi
    : >"$work/tests.log"
    if ! candidate_tests=$(runnable_tests "$work/candidate" "$work/candidate" "$work/candidate/target" \
        "$work/tests.candidate-names"); then
        sed 's/^/| tests: /' "$work/tests.log" | tail -n 30
        verdict rejected "candidate tests do not build"
        exit 0
    fi
    echo "| tests cargo runs: $base_tests on the base, $candidate_tests on the candidate" \
        "(listed in $(($(date +%s) - listing_started)) s)"
    if [ "$candidate_tests" -lt "$base_tests" ]; then
        verdict rejected "candidate runs fewer tests ($base_tests -> $candidate_tests)"
        exit 0
    fi
    dropped=$(LC_ALL=C comm -23 "$work/tests.base-names" "$work/tests.candidate-names")
    # The owner retires tests with the features they removed: names listed in
    # `<scenarios-dir>/retired-tests` (one per line, `#` comments) may go.
    if [ -n "$dropped" ] && [ -f "$scenarios/retired-tests" ]; then
        grep -v '^#' "$scenarios/retired-tests" | grep . | LC_ALL=C sort -u >"$work/tests.retired"
        dropped=$(printf '%s\n' "$dropped" | LC_ALL=C comm -23 - "$work/tests.retired")
    fi
    if [ -n "$dropped" ]; then
        echo "| base tests the candidate does not run by name: $(printf '%s\n' "$dropped" | head -n 20 | tr '\n' ' ')"
        verdict rejected "candidate drops or renames base tests ($(printf '%s\n' "$dropped" | grep -c .): $(printf '%s\n' "$dropped" | head -n 3 | tr '\n' ' ' | sed 's/ $//'))"
        exit 0
    fi
fi

# The candidate's scenarios, N at a time: the task scenario first, then
# check-tests.sh (the regression most often decisive, so a failure there
# stops the rest early), then every other but the ratchets, in order. The
# first failure in that order decides, as in the serial gate.
#
# Checks run twice. A `check-*` scenario holds for every tree, so one failing
# run is evidence of the candidate or of the host (a starved loop, a slow
# disk: task 127's 85-minute gate was rejected by one e2e run that the same
# tree passes). A failing check is run again on the candidate, in the same
# slot; failing twice, it is run once on the base, built for it if the base
# phase did not (every base result cached). The base run comes after
# candidate code ran on the shared test server, so it can only spare the
# candidate a rejection by aborting the gate, never accept it:
# - passes the second time: a flake, not a regression (said in the log);
# - fails twice, passes on the base: a regression, rejected as before;
# - fails twice and on the base: no verdict on the candidate, a host or
#   harness fault: exit 6 with its fact (`host_fault` above), as for a
#   harness error on the base, and the integrator judges the task again
#   after a backoff.
# The first run's block keeps its header, so the integrator reads
# check-tests' failures from the first run (integrate/host.rs `section`).
# candidate_job <job>: one scenario on the candidate (its runs), in the
# background.
candidate_job() {
    job=$1 job_part=0
    run_name=$(cat "$job/section")
    job_scenario=$(cat "$job/scenario")
    job_say "| == $run_name"
    run_at=$(stage_now)
    run_status=0
    job_run "$job" 1 "$work/candidate" "$job_scenario" || run_status=$?
    [ ! -e "$job/slot-waited" ] || return 0
    job_output 1
    if [ "$run_status" -eq 0 ] && [ -s "$job/seed-entry" ] && [ ! -s "$job/never-started" ]; then
        seed
    fi
    is_check=""
    case "$(basename "$job_scenario")" in check-*) is_check=1 ;; esac
    if [ -n "$is_check" ] && [ "$run_status" -ne 0 ]; then
        job_stage "$(($(stage_now) - run_at))" "exit $run_status" "$run_name"
        job_say "| == $run_name (again: a failing check runs twice)"
        run_at=$(stage_now)
        again=0
        job_run "$job" 2 "$work/candidate" "$job_scenario" || again=$?
        [ ! -e "$job/slot-waited" ] || return 0
        job_output 2
        if [ "$again" -eq 0 ]; then
            job_say "| $(basename "$job_scenario") failed once ($run_status) and passed again on the candidate: a flake, not a regression"
            basename "$job_scenario" >"$job/flake"
            run_status=0
        else
            again_ms=$(($(stage_now) - run_at))
            job_stage "$again_ms" "exit $again" "$run_name (again)"
            run_at=$(stage_now)
            base_name="base: $(basename "$job_scenario") (it failed twice on the candidate)"
            job_say "| == $base_name"
            if ! base_ready; then
                job_say "| the base cannot be built to run it: no verdict on the candidate"
                printf '%s\n' "$run_status, then $again on the candidate; the base does not build" >"$job/runs"
                run_status=abort
            else
                on_base=0
                job_run "$job" 3 "$work/baseline" "$job_scenario" || on_base=$?
                [ ! -e "$job/slot-waited" ] || return 0
                job_output 3
                job_stage "$(($(stage_now) - run_at))" "exit $on_base" "$base_name"
                if [ "$on_base" -eq 0 ]; then
                    job_say "| $(basename "$job_scenario") passes on the base and failed twice on the candidate: a regression"
                    run_status=$again
                else
                    # Once more on the base: two runs that fail with the same
                    # failing tests say main is red on the check (main_red),
                    # not this candidate's doing and not the host's for one
                    # run; anything else stays the host's or the harness's.
                    job_say "| == $base_name (again: is main red on it?)"
                    run_at=$(stage_now)
                    on_base2=0
                    job_run "$job" 4 "$work/baseline" "$job_scenario" || on_base2=$?
                    [ ! -e "$job/slot-waited" ] || return 0
                    job_output 4
                    job_stage "$(($(stage_now) - run_at))" "exit $on_base2" "$base_name (again)"
                    printf '%s\n' "$run_status, then $again on the candidate; $on_base, then $on_base2 on the base" >"$job/runs"
                    if [ "$on_base" -eq 1 ] && [ "$on_base2" -eq 1 ] &&
                        shared_failed "$job/out.3" "$job/out.4" "$job/main-red"; then
                        job_say "| $(basename "$job_scenario") fails on the base twice with the same failing tests: main is red on it, no verdict on the candidate"
                        run_status=main_red
                        # An owner's push: unsolved on the base only for the
                        # tests main fails; one more the candidate fails is
                        # its own regression.
                        if [ "${SPAWN_GATE_RED_UNSOLVED-}" = 1 ]; then
                            red_new=$(new_failures "$job/main-red" "$job/out.1" "$job/out.2")
                            if [ -n "$red_new" ]; then
                                job_say "| $(basename "$job_scenario"): the candidate fails what main's red runs do not ($(printf '%s' "$red_new" | tr '\n' ' ' | sed 's/ $//')): a regression"
                                run_status=$again
                            fi
                        fi
                    else
                        rm -f "$job/main-red"
                        job_say "| $(basename "$job_scenario") fails on the base too ($on_base, then $on_base2): a host or harness fault, no verdict on the candidate"
                        run_status=abort
                    fi
                fi
                run_name=$base_name
            fi
        fi
    fi
    # The last run's time; a check that failed twice has a stage line for
    # each of its runs already, so its verdict's line adds none.
    if [ -n "${again_ms-}" ]; then
        echo 0 >"$job/ms"
    else
        echo "$(($(stage_now) - run_at))" >"$job/ms"
    fi
    echo "$run_name" >"$job/last"
    job_done "$run_status"
}
# decisive <job>: 0 when its failure is the verdict: the task scenario's, or
# a regression's (an objective the base did not leave unsolved).
decisive() {
    [ "$(cat "$1/result")" != 0 ] || return 1
    # A scenario the host cannot judge is no verdict either way (below).
    [ "$(cat "$1/result")" != 6 ] || return 1
    # A check main is red on, for an owner's push (SPAWN_GATE_RED_UNSOLVED):
    # unsolved on the base, the rest still judged.
    if [ "$(cat "$1/result")" = main_red ] && [ "${SPAWN_GATE_RED_UNSOLVED-}" = 1 ]; then
        return 1
    fi
    [ -e "$1/task" ] || [ "$(cat "$1/result")" = error ] ||
        [ "$(base_result "$(cat "$1/scenario")")" != unsolved ]
}
candidate_finished() {
    if decisive "$1"; then [ "$2" -ge "$jobs_stop" ] || jobs_stop=$2; fi
}
# candidate_printed <job> <n>: its block, then what it means.
candidate_printed() {
    job_print "$1"
    # A check that failed once and passed again: named in the verdict.
    if [ -s "$1/flake" ]; then
        flakes="${flakes:+$flakes,}\"$(json_string "$(cat "$1/flake")")\""
    fi
    printed_name=$(cat "$1/section")
    printed_ms=$(cat "$1/ms" 2>/dev/null || echo 0)
    printed_status=$(cat "$1/result")
    printed_scenario=$(cat "$1/scenario")
    # No room to judge now (a run gave up waiting for a slot): that is the
    # answer whatever else the job's status would have meant — never a
    # rejection, and never the "failed twice" story below, which would be
    # untrue of a run that never started.
    if [ -e "$1/slot-waited" ]; then slot_no_room "$1"; fi
    if [ -s "$1/never-started" ]; then
        sandbox_fault "$1/never-started" "$(basename "$printed_scenario")"
    fi
    if [ "$printed_status" = error ]; then
        phase_time candidate
        echo "gate: the gate could not run $(basename "$printed_scenario") on the candidate" >&2
        exit 3
    fi
    if [ "$printed_status" = 6 ]; then
        # The scenario says the host cannot judge it (see "A scenario that
        # cannot be judged here"): not a rejection, whatever it is. A
        # regression is skipped; the task itself cannot be judged, so the
        # gate answers the fact, not a verdict.
        if [ -e "$1/task" ]; then
            phase_time candidate
            run_stage "cannot_judge|$printed_name" "$printed_ms"
            cannot_judge "$(basename "$printed_scenario")" "$printed_status"
        fi
        echo "| $(basename "$printed_scenario") cannot be judged here: not a regression"
        run_stage "skipped|$printed_name" "$printed_ms"
        return 0
    fi
    if [ "$printed_status" = main_red ] && [ "${SPAWN_GATE_RED_UNSOLVED-}" = 1 ]; then
        echo "| $(basename "$printed_scenario") fails on the base twice with the same failing tests: main is red on it, so it is unsolved on the base (SPAWN_GATE_RED_UNSOLVED): not a regression"
        run_stage "skipped|$printed_name" "$printed_ms"
        return 0
    fi
    if [ "$printed_status" = main_red ]; then
        run_stage "host_fault|$(cat "$1/last")" "$printed_ms"
        phase_time candidate
        echo "| $(basename "$printed_scenario") fails on the base twice with the same failing tests: main is red on it, no verdict"
        main_red "$(basename "$printed_scenario")" "$(cat "$1/runs" 2>/dev/null)" "$1/main-red"
    fi
    if [ "$printed_status" = abort ]; then
        run_stage "host_fault|$(cat "$1/last")" "$printed_ms"
        phase_time candidate
        echo "| $(basename "$printed_scenario") fails on the candidate and on the base: a host or harness fault, no verdict"
        host_fault "$(basename "$printed_scenario")" "candidate and base" "$(cat "$1/runs" 2>/dev/null)" \
            "$(basename "$printed_scenario") failed twice on the candidate and on the base: a host or harness fault, not a verdict on the candidate"
    fi
    if [ -e "$1/task" ]; then
        # The metric is the scenario's: one line on its stdout. Stderr is
        # never read, and two lines mean candidate code printed one too, so
        # neither counts.
        metrics=$(grep -E '^METRIC [A-Za-z0-9_]+( [a-z]+=[0-9.]+)*$' "$1/out.1" 2>/dev/null || true)
        case "$(printf '%s' "$metrics" | grep -c . || true)" in
        0) ;;
        1) metric=$metrics ;;
        *) echo "| two or more METRIC lines on the scenario's stdout: no metric" ;;
        esac
        if [ "$printed_status" -ne 0 ]; then
            run_stage "rejected|$printed_name" "$printed_ms"
            phase_time candidate
            verdict rejected "scenario fails on the candidate"
            exit 0
        fi
    elif [ "$printed_status" -ne 0 ]; then
        # An objective scenario is a regression only if the base solved it,
        # as judged before any candidate code ran.
        if [ "$(base_result "$printed_scenario")" = unsolved ]; then
            echo "| $(basename "$printed_scenario") fails on the base too: an unsolved objective, not a regression"
            run_stage "unsolved|$printed_name" "$printed_ms"
            return 0
        fi
        run_stage "rejected|$printed_name" "$printed_ms"
        phase_time candidate
        verdict rejected "regression: $(basename "$printed_scenario") fails on the candidate"
        exit 0
    fi
    run_stage "ok|$printed_name" "$printed_ms"
}
# Seeds from the accepting gate (design-gate-time.md §2). The commit a gate
# accepts is the commit the integrator lands: the next gate's base is this
# gate's candidate, the same commit. So a candidate run of an objective
# scenario that completes with exit 0 is that commit's `solved`, and the
# gate writes it into the base cache under the key a base run of the
# candidate commit would use (objective_key, with the candidate's own
# toolchain probe). A key no later gate computes is never read. A seed is
# taken only where a candidate run sees what a base run sees:
# - a fresh server: the candidate phase gets one of its own, started after
#   the base phase ("A fresh server for the candidate's runs", below), as
#   the next gate's base phase does. A gate with no test database at all
#   seeds too; a gate on the shared test server never does;
# - an objective `<digits>.sh` or the task scenario, never a check, a
#   ratchet, or a scenario that reads SPAWN_BASELINE_TREE (a candidate run
#   sees the old base there, a base run itself) or runs cargo or anything
#   under target/ (the candidate tree's target/debug is the test listing's
#   build, linked again by check-tests while the candidate's runs go on; a
#   base run's is the build hook's), in it or in any file it names
#   (depends_on; for cargo and target/, its whole text). Scenarios run SPAWN_BIN, the build hook's
#   binary in both phases;
# - neighbours: the candidate phase runs the checks (check-tests among
#   them) beside the objectives, the base phase objectives only. The
#   load-sensitive scenarios run alone in both (`serial`); more load beside
#   a run makes it pass no more easily, the same flake risk a base run has;
# - the first run, completed, exit 0 (never a failure, a run stopped or cut
#   short, a sandbox that never started, a run that waited out its slot):
#   only `solved` is ever seeded, never `unsolved` or `cannot_judge`;
# - an entry that holds no result yet, under the key's lock without waiting.
# The entry says where it came from (`seeded-by-gate <candidate> at <UTC
# time>`, entry_result), and the gate that reads it says so.
seeding=""
seed_toolchain_key=""
if [ -z "$base_only" ]; then
    if [ -n "$gate_pg" ] || { [ -z "$db_socket" ] && [ -z "$sandbox_db_url" ]; }; then
        seeding=1
    else
        echo "| base cache: no seeds from this gate's candidate runs: they share the test server, not a fresh one"
    fi
fi
candidate_count=0
add_candidate_job() { # <scenario> <section>
    candidate_count=$((candidate_count + 1))
    job=$jobs_dir/candidate.$candidate_count
    mkdir "$job"
    printf '%s\n' "$1" >"$job/scenario"
    printf '%s\n' "$2" >"$job/section"
    if runs_alone "$1"; then : >"$job/alone"; fi
    seed_plan "$1"
}
# seed_plan <scenario>: $job/seed-entry, the entry a passing first run of
# the scenario seeds, when it may (above).
seed_plan() {
    [ -n "$seeding" ] || return 0
    if [ "$1" != "$scenario" ]; then
        case "$(basename "$1")" in *[!0-9].sh | .sh) return 0 ;; esac
    fi
    grep -q '^# rsi: ratchet' "$1" && return 0
    if [ -z "$seed_toolchain_key" ]; then
        seed_toolchain_key=$(toolchain_probe "$work/candidate" "$work/toolchain.candidate")
    fi
    seed_key=$(objective_key "$candidate_commit" "$seed_toolchain_key" "$1")
    # depends_on (in objective_key) left the scenario and every file it names.
    while IFS= read -r seed_file; do
        [ -f "$seed_file" ] || continue
        if grep -q 'SPAWN_BASELINE_TREE' -- "$seed_file"; then
            echo "| base cache: $(basename "$1") is not seeded: ${seed_file#"$scenarios"/} reads SPAWN_BASELINE_TREE"
            return 0
        fi
        # The whole text, comments too: a word read wrongly can only cost a
        # seed, never make a wrong one.
        if grep -qE '(^|[^A-Za-z0-9_])cargo([^A-Za-z0-9_-]|$)|target/' -- "$seed_file"; then
            echo "| base cache: $(basename "$1") is not seeded: ${seed_file#"$scenarios"/} runs cargo or reads target/"
            return 0
        fi
    done <"$work/deps.files"
    printf '%s\n' "$base_cache/$seed_key" >"$job/seed-entry"
}
# seed <job>: its first run passed; the entry it may seed gets `solved`.
seed() {
    seed_entry=$(cat "$job/seed-entry")
    [ -z "$(entry_result "$seed_entry")" ] || return 0
    command -v flock >/dev/null 2>&1 || return 0
    { command exec 8>>"$seed_entry.lock"; } 2>/dev/null || return 0
    if flock -n 8 && [ -z "$(entry_result "$seed_entry")" ] &&
        printf 'solved\nseeded-by-gate %s at %s\n' "$candidate_commit" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
            >"$seed_entry.seed.$$" 2>/dev/null &&
        mv -f "$seed_entry.seed.$$" "$seed_entry" 2>/dev/null; then
        job_say "| $(basename "$job_scenario"): solved on the candidate, seeded into the base cache for $(printf '%s' "$candidate_commit" | cut -c1-12) as a base"
    fi
    rm -f "$seed_entry.seed.$$" 2>/dev/null || :
    exec 8>&-
}
if [ -n "$scenario" ]; then
    add_candidate_job "$scenario" "candidate: $(basename "$scenario") must pass"
    : >"$job/task"
    : >"$job/head"
fi
for other in "$scenarios"/check-tests.sh "$scenarios"/*.sh; do
    [ -f "$other" ] || continue
    [ "$other" = "$scenario" ] && continue
    [ "$other" = "$scenarios/check-tests.sh" ] && [ -n "${tests_queued-}" ] && continue
    [ "$other" = "$scenarios/check-tests.sh" ] && tests_queued=1
    grep -q '^# rsi: ratchet' "$other" && continue
    add_candidate_job "$other" "regression: $(basename "$other") must still pass"
    [ "$other" != "$scenarios/check-tests.sh" ] || : >"$job/head"
done
stage_close ok
# A fresh server for the candidate's runs: the base phase's runs leave the
# server as they leave it, and a candidate run is to see what the next
# gate's base run of this commit will see, a server only this tree's runs
# have used (the seeds above). The same programs, the same password (the
# data of the gate's own copy of the template, gate-pg.sh), the same socket
# path the sandbox is given.
if [ -n "$gate_pg" ] && [ "$candidate_count" -gt 0 ]; then
    gate_pg_stop
    start_server "$work/pg.candidate" candidate
    rm -f "$db_socket"
    if ! ln -P "$gate_pg_socket" "$db_socket" || [ ! -S "$db_socket" ]; then
        echo "gate: cannot link the candidate's fresh test server's socket into $work/test-db" >&2
        exit 3
    fi
    echo "| test database: a fresh server for the candidate's runs, up in $gate_pg_ms ms"
fi
phase_start candidate
run_jobs candidate "$candidate_count" candidate_job candidate_finished candidate_printed
phase_time candidate
verdict accepted
