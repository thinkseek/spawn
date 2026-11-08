#!/usr/bin/env bash
# deploy/gate-runner.sh and deploy/gate-runner/ end to end against a fake
# runner: a local directory (deploy/gate-runner/setup.sh installs into it)
# and an `ssh` stub that runs the remote command here, as ssh's remote shell
# would. The live fleet is never touched: the gate's fixtures are a throwaway
# bare repository and scenario directories, with a PostgreSQL of the host's
# own (SPAWN_GATE_PG_BIN, as a real runner has).
#
#   deploy/test-gate-runner.sh
#
# Covered: the success path (the gate's verdict and log come back, the
# record names one attempt); a stage timeout (the runner's own `timeout`
# kills the gate); a missing result (a run that leaves no verdict); a
# truncated result (a collect stream cut short); SSH dying mid-ship and the
# retry succeeding; a retry that succeeds after the connection dies in the
# run; retry exhaustion (transport failure on every attempt); and --check:
# equal base-cache keys, a helper of another version, and a toolchain that
# computes different keys.
#
# The stub's knobs, read from the dispatcher's environment (it is spawned
# there): STUB_FAIL_SHIP, STUB_FAIL_RUN, STUB_FAIL_COLLECT (fail that many
# of the stage's calls with ssh's 255), STUB_DROP_VERDICT=1 (a run leaves no
# verdict), STUB_TRUNCATE_COLLECT=1 (the collect stream ends early). Counts
# are per case, in $STUB_COUNT_DIR.
set -euo pipefail

repo=$(git rev-parse --show-toplevel)
prog=test-gate-runner
failures=0
pass() { printf 'ok   %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; failures=$((failures + 1)); }
expect() { # <what> <got> <want>
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1: got '$2', want '$3'"; fi
}
contains() { # <what> <file> <text>
    if grep -qF -- "$3" "$2" 2>/dev/null; then pass "$1"; else fail "$1: '$3' is not in $2"; fi
}
lacks() { # <what> <file> <text>
    if grep -qF -- "$3" "$2" 2>/dev/null; then fail "$1: '$3' is in $2"; else pass "$1"; fi
}

pg_bin=$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -n 1)
[ -n "$pg_bin" ] && [ -x "$pg_bin/postgres" ] || {
    printf '%s: no PostgreSQL programs (/usr/lib/postgresql/*/bin); a runner needs one (or docker)\n' "$prog" >&2
    exit 1
}
command -v bwrap >/dev/null 2>&1 || {
    printf '%s: no bwrap; the gate needs it\n' "$prog" >&2
    exit 1
}

scratch=$(mktemp -d "${TMPDIR:-/tmp}/gate-runner-test.XXXXXX")
runner_home=""
# A long root moves a job's scratch to /tmp/gate-runner/<job> (the socket
# path limit); remember what was there, so only this test's jobs go.
short_before=$(ls -A /tmp/gate-runner 2>/dev/null || true)
cleanup() {
    chmod -R u+w "$scratch" "$runner_home" 2>/dev/null || true
    rm -rf "$scratch" "$runner_home"
    for job in $(ls -A /tmp/gate-runner 2>/dev/null || true); do
        case " $short_before " in
        *" $job "*) ;;
        *) rm -rf "/tmp/gate-runner/$job" ;;
        esac
    done
    rmdir /tmp/gate-runner 2>/dev/null || true
}
trap cleanup EXIT

# The gate's environment here and on the runner: the same toolchain, the
# same private server, the same floor (keys are compared in --check).
export SPAWN_GATE_MIN_FREE_GB=0
export SPAWN_GATE_JOBS=2
export SPAWN_GATE_PG_BIN=$pg_bin
export XDG_CACHE_HOME=$scratch/local-cache
mkdir -p "$XDG_CACHE_HOME"

# A throwaway repository: one base, one candidate that changes `value`.
repo_git=$scratch/repo.git
seed=$scratch/seed
git init -q --bare "$repo_git"
git init -q --initial-branch=main "$seed"
git -C "$seed" config user.name t
git -C "$seed" config user.email t@localhost
printf 'one\n' >"$seed/value"
mkdir -p "$seed/judge"
printf 'keep\n' >"$seed/judge/rule"
git -C "$seed" add -A
git -C "$seed" commit -qm base
git -C "$seed" push -q "$repo_git" HEAD:refs/heads/main
base=$(git -C "$seed" rev-parse HEAD)
printf 'two\n' >"$seed/value"
git -C "$seed" commit -qam candidate
git -C "$seed" push -q "$repo_git" HEAD:refs/heads/main
candidate=$(git -C "$seed" rev-parse HEAD)

# Scenarios: the gate needs a protected list; one check that passes, and a
# slow one for the timeout case.
scen=$scratch/scen
mkdir -p "$scen"
printf '# nothing protected: these are fixtures\n' >"$scen/protected"
printf '#!/bin/sh\nexit 0\n' >"$scen/check-ok.sh"
slow=$scratch/scen-slow
mkdir -p "$slow"
cp "$scen/protected" "$slow/"
printf '#!/bin/sh\nsleep 30\nexit 0\n' >"$slow/check-slow.sh"
# One objective, for --check: the base-cache key is per objective, so a
# scenario directory with none compares nothing (the keys would be equal and
# empty).
keysscen=$scratch/scen-keys
mkdir -p "$keysscen"
cp "$scen/protected" "$keysscen/"
printf '#!/bin/sh\nexit 1\n' >"$keysscen/001.sh"

# A root the gate's sandbox empties is refused, before anything else.
mkdir -p "$scratch/root-tmp"
refused=0
sh "$repo/deploy/gate-runner/setup.sh" --root "$scratch/root-tmp" --pg-bin "$pg_bin" \
    >"$scratch/refuse.out" 2>&1 || refused=$?
expect "setup.sh refuses a root the sandbox empties" "$refused" 1
contains "and says why" "$scratch/refuse.out" "the gate's sandbox empties those"

# The fake runner: setup.sh installs into a directory under $HOME (the
# gate's sandbox binds the controller's scripts back only under $HOME; it
# empties TMPDIR). The UI's dependencies are the controller's lockfile with
# a stand-in install, so --check's web comparison has both sides of it.
mkdir -p "$HOME/.cache"
runner_home=$(mktemp -d "$HOME/.cache/gate-runner-test.XXXXXX")
webdir=$runner_home/web-deps
mkdir -p "$webdir/node_modules"
cp "$repo/web/pnpm-lock.yaml" "$repo/web/package.json" "$webdir/"
printf 'stand-in\n' >"$webdir/node_modules/.keep"
GATE_RUNNER_ROOT=$runner_home sh "$repo/deploy/gate-runner/setup.sh" \
    --root "$runner_home" --pg-bin "$pg_bin" --jobs 2 --min-free-gb 0 --nice 10 --web "$webdir" \
    >"$scratch/setup.out" 2>"$scratch/setup.err" || {
    printf '%s: setup.sh failed:\n' "$prog" >&2
    cat "$scratch/setup.err" >&2
    exit 1
}
pass "setup.sh installed the runner ($(grep -c '=' "$scratch/setup.out") report lines)"

# The ssh stub: ssh [options] <host> <command string>; run the command here.
stub_dir=$scratch/bin
mkdir -p "$stub_dir"
export STUB_COUNT_DIR=$scratch/stub-count
mkdir -p "$STUB_COUNT_DIR"
export STUB_ROOT=$runner_home
cat >"$stub_dir/ssh" <<'EOF'
#!/bin/sh
# An ssh stand-in: it drops the options and the host, and runs the command
# string through a shell, as ssh's remote shell does. STUB_FAIL_* make the
# first N calls of a stage fail the way a dead connection does (exit 255);
# STUB_DROP_VERDICT and STUB_TRUNCATE_COLLECT tamper with a result.
while [ $# -gt 0 ]; do
    case $1 in
    -o) shift 2 ;;
    -*) shift ;;
    *) break ;;
    esac
done
[ $# -gt 1 ] || exit 255
shift # the host
cmd=$*
action=""
case $cmd in
*accept*) action=ship ;;
*" run "*) action=run ;;
*" keys "*) action=keys ;;
*collect*) action=collect ;;
*preflight* | *remove*) action=other ;;
esac
n=0
if [ -n "$action" ] && [ -f "$STUB_COUNT_DIR/$action" ]; then
    n=$(cat "$STUB_COUNT_DIR/$action")
fi
if [ -n "$action" ]; then
    n=$((n + 1))
    printf '%s\n' "$n" >"$STUB_COUNT_DIR/$action"
fi
fail_var="STUB_FAIL_$(printf '%s' "$action" | tr 'a-z' 'A-Z')"
eval "fail_until=\${$fail_var:-0}"
if [ "$n" -le "$fail_until" ]; then
    case "$action" in
    ship) head -c 400 >/dev/null 2>&1 || true ;; # part of the stream, then the connection dies
    esac
    exit 255
fi
if [ "$action" = collect ] && [ "${STUB_TRUNCATE_COLLECT:-0}" = 1 ]; then
    sh -c "$cmd" 2>/dev/null | head -c 300
    exit 0
fi
status=0
sh -c "$cmd" || status=$?
if [ "$action" = run ] && [ "${STUB_DROP_VERDICT:-0}" = 1 ]; then
    rm -f "$STUB_ROOT"/jobs/*/out/verdict 2>/dev/null || true
fi
exit "$status"
EOF
chmod 755 "$stub_dir/ssh"

export GATE_RUNNER_SSH=$stub_dir/ssh
export GATE_RUNNER_REMOTE_BIN=$runner_home/bin/remote

out=$scratch/out
dispatch() { # <out dir> [args...]
    status=0
    "$repo/deploy/gate-runner.sh" "$@" >"$scratch/dispatch.out" 2>"$scratch/dispatch.err" || status=$?
}
reset() { # <out dir>: a clean out directory and per-case stub counters
    rm -rf "$1"
    rm -f "$STUB_COUNT_DIR"/*
    unset STUB_FAIL_SHIP STUB_FAIL_RUN STUB_FAIL_COLLECT STUB_DROP_VERDICT STUB_TRUNCATE_COLLECT
}
job_of() { # <out dir>: the one job's directory
    find "$1" -mindepth 1 -maxdepth 1 -type d -not -name '.*' | head -n 1
}

# 1. The success path.
reset "$out"
dispatch "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a gate on the runner: the dispatcher exits with the gate's 0" "$status" 0
job=$(job_of "$out")
expect "the verdict comes back" "$(jq -r .verdict "$job/verdict.json")" accepted
expect "the verdict names the commits judged" \
    "$(jq -r '.base + " " + .candidate' "$job/verdict.json")" "$base $candidate"
expect "the record names one attempt" "$(jq -r '.attempts | length' "$job/record.json")" 1
expect "and its result" "$(jq -r '.result' "$job/record.json")" verdict
expect "the whole log comes back" \
    "$(jq -r '.attempts[0].ship + " " + .attempts[0].run + " " + .attempts[0].collect' "$job/record.json")" \
    "ok ok ok"
contains "the log is the gate's" "$job/gate.log" "| == candidate: build"
contains "and its verdict line" "$job/gate.log" '"verdict":"accepted"'
lacks "the runner's job is gone after the collection" "$scratch/dispatch.out" "kept the runner's job"
[ -d "$runner_home/jobs" ] && [ -z "$(ls -A "$runner_home/jobs")" ] &&
    pass "nothing left in the runner's jobs" || fail "the runner's job directory is not empty"

# 2. A stage timeout: the runner's own timeout kills the gate.
reset "$out"
dispatch --run-timeout 3 --retries 0 "$runner_home" "$repo_git" "$base" "$candidate" "$slow" "$out"
expect "a gate killed by the runner's timeout: no verdict, exit 2" "$status" 2
job=$(job_of "$out")
expect "the record says timeout" "$(jq -r '.result' "$job/record.json")" timeout
expect "the reason" "$(jq -r '.reason' "$job/record.json")" "a stage timed out"
[ ! -f "$job/verdict.json" ] && pass "no verdict was invented" || fail "a timeout produced a verdict.json"
contains "and the dispatcher says the gate was killed" "$scratch/dispatch.err" "killed by the runner's timeout"

# 3. A missing result: the run leaves no verdict.
reset "$out"
STUB_DROP_VERDICT=1 dispatch --retries 0 "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a run without a verdict: exit 2" "$status" 2
job=$(job_of "$out")
expect "the record says the result did not come back whole" "$(jq -r '.reason' "$job/record.json")" \
    "the result did not come back whole"
[ ! -f "$job/verdict.json" ] && pass "no verdict file" || fail "a missing verdict became a verdict.json"
contains "and says why" "$scratch/dispatch.err" "without a verdict"

# 4. A truncated result: the collect stream ends early.
reset "$out"
STUB_TRUNCATE_COLLECT=1 dispatch --retries 0 "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a truncated collect: exit 2" "$status" 2
job=$(job_of "$out")
expect "the record's collect failed" "$(jq -r '.attempts[0].collect' "$job/record.json")" "failed(2)"
[ ! -f "$job/verdict.json" ] && pass "no verdict from a truncated stream" || fail "a truncated stream produced a verdict.json"

# 5. SSH dies mid-ship, the retry succeeds.
reset "$out"
STUB_FAIL_SHIP=1 dispatch --retries 1 "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a ship that died and a retry that landed: exit 0" "$status" 0
job=$(job_of "$out")
expect "the record names two attempts" "$(jq -r '.attempts | length' "$job/record.json")" 2
expect "the first ship failed" "$(jq -r '.attempts[0].ship' "$job/record.json")" "failed(255)"
expect "the second landed" "$(jq -r '.attempts[1].ship + " " + .attempts[1].result' "$job/record.json")" \
    "ok verdict"
expect "the verdict is still the gate's" "$(jq -r .verdict "$job/verdict.json")" accepted

# 6. The connection dies in the run, the retry succeeds.
reset "$out"
STUB_FAIL_RUN=1 dispatch --retries 1 "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a run with no connection and a retry: exit 0" "$status" 0
job=$(job_of "$out")
expect "the first attempt's run lost the connection" "$(jq -r '.attempts[0].run' "$job/record.json")" "no-connection"
expect "the second attempt answered" "$(jq -r '.attempts[1].result' "$job/record.json")" verdict

# 7. Retry exhaustion: transport failure on every attempt.
reset "$out"
STUB_FAIL_SHIP=9 dispatch --retries 1 "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "every attempt's ship failed: exit 2" "$status" 2
job=$(job_of "$out")
expect "two attempts, both transport" \
    "$(jq -r '[.attempts[].result] | join(",")' "$job/record.json")" "transport,transport"
expect "the reason names the transport" "$(jq -r '.reason' "$job/record.json")" \
    "the transport failed on every attempt"
[ ! -f "$job/verdict.json" ] && pass "no verdict" || fail "a failed transport produced a verdict.json"

# 8. --check: the same host, the same keys.
reset "$out"
check_status=0
"$repo/deploy/gate-runner.sh" --check "$runner_home" "$repo_git" "$base" "$keysscen" \
    >"$scratch/check.out" 2>"$scratch/check.err" || check_status=$?
expect "--check on a compatible runner: exit 0" "$check_status" 0
contains "it reports the base keys equal" "$scratch/check.out" "equal (1 scenario(s)): a base result cached here is a hit there"
contains "and the server" "$scratch/check.out" "programs $pg_bin"
[ "$(find "$runner_home/jobs" -maxdepth 1 -name 'check-*' | wc -l)" = 0 ] &&
    pass "the check removed its job" || fail "the check left a job behind"

# 8b. --check under the integrator's gate environment file (the NUL-separated
# /proc/environ form deploy/gate-env.sh reads): the same keys, from exactly
# the environment the primary's gate gets.
envfile=$scratch/gate-env
{
    printf 'SPAWN_INTEGRATE_PID=%s\0' $$
    printf 'PATH=%s\0' "$PATH"
    printf 'HOME=%s\0' "$HOME"
    [ -z "${LANG-}" ] || printf 'LANG=%s\0' "$LANG"
    printf 'SPAWN_GATE_JOBS=2\0'
    printf 'SPAWN_GATE_MIN_FREE_GB=0\0'
    printf 'SPAWN_GATE_PG_BIN=%s\0' "$pg_bin"
} >"$envfile"
check_status=0
"$repo/deploy/gate-runner.sh" --check --env-file "$envfile" "$runner_home" "$repo_git" "$base" "$keysscen" \
    >"$scratch/check.out" 2>"$scratch/check.err" || check_status=$?
expect "--check under a gate environment file: exit 0" "$check_status" 0
contains "and the keys are equal" "$scratch/check.out" "a base result cached here is a hit there"

# 9. --check: a runner whose helper is another version.
cp "$runner_home/bin/remote" "$scratch/remote.orig"
printf '\n# edited\n' >>"$runner_home/bin/remote"
check_status=0
"$repo/deploy/gate-runner.sh" --check "$runner_home" "$repo_git" "$base" "$keysscen" \
    >"$scratch/check.out" 2>"$scratch/check.err" || check_status=$?
expect "--check with another helper version: exit 1" "$check_status" 1
contains "and says to rerun setup.sh" "$scratch/check.out" "rerun setup.sh there"
cp "$scratch/remote.orig" "$runner_home/bin/remote"

# 10. --check: a runner whose toolchain is not this host's (the key differs).
GATE_RUNNER_ROOT=$runner_home PATH=/usr/bin:/bin sh "$repo/deploy/gate-runner/setup.sh" \
    --root "$runner_home" --pg-bin "$pg_bin" --jobs 2 --min-free-gb 0 --nice 10 --force \
    >"$scratch/setup2.out" 2>"$scratch/setup2.err" || {
    printf '%s: setup.sh --force failed:\n' "$prog" >&2
    cat "$scratch/setup2.err" >&2
    exit 1
}
check_status=0
"$repo/deploy/gate-runner.sh" --check "$runner_home" "$repo_git" "$base" "$keysscen" \
    >"$scratch/check.out" 2>"$scratch/check.err" || check_status=$?
expect "--check with a different toolchain: exit 1" "$check_status" 1
contains "the keys differ" "$scratch/check.out" "differ (this host"

if [ "$failures" != 0 ]; then
    printf '%s: %s failure(s)\n' "$prog" "$failures" >&2
    exit 1
fi
printf '%s: all cases passed\n' "$prog"
