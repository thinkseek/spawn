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
# computes different keys; the registry (8m-8t): the sync, a path the primary
# did not list, two accepts at once, a reader that holds a crate open or
# opens it during an accept, the staging a killed accept left, a runner
# without flock, a registry on another file system than its cargo home, and
# --check sending the registry before it asks for keys.
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
badroot=""
otherfs=""
# A long root moves a job's scratch to the short directory (the socket path
# limit): this suite's own, so nothing of a real runner's is touched.
export GATE_RUNNER_SHORT="$HOME/.grs-test-$$"
short_before=""
cleanup() {
    chmod -R u+w "$scratch" "$runner_home" 2>/dev/null || true
    rm -rf "$scratch" "$runner_home"
    [ -z "$badroot" ] || rm -rf "$badroot"
    [ -z "$otherfs" ] || rm -rf "$otherfs"
    rm -rf "$GATE_RUNNER_SHORT"
}
trap cleanup EXIT

# The gate's environment here and on the runner: the same toolchain, the
# same private server, the same floor (keys are compared in --check).
export SPAWN_GATE_MIN_FREE_GB=0
export SPAWN_GATE_JOBS=2
export SPAWN_GATE_PG_BIN=$pg_bin
# The registry sync and the memory floor have cases of their own (8l-8n);
# the others do not depend on this host's cargo registry or its free memory.
export GATE_RUNNER_REGISTRY=""
export GATE_RUNNER_MIN_FREE_MEMORY_MB=0
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

# A root the gate's sandbox empties is refused, before anything else. The
# root is under /tmp explicitly: the suite's own scratch follows TMPDIR, and
# a root under any other TMPDIR is correct to accept (case 8g).
badroot=$(mktemp -d /tmp/gate-runner-refuse.XXXXXX)
refused=0
sh "$repo/deploy/gate-runner/setup.sh" --root "$badroot" --pg-bin "$pg_bin" \
    >"$scratch/refuse.out" 2>&1 || refused=$?
expect "setup.sh refuses a root the sandbox empties" "$refused" 1
contains "and says why" "$scratch/refuse.out" "the gate's sandbox empties those"
rm -rf "$badroot"

# The fake runner: setup.sh installs into a directory under $HOME (the
# gate's sandbox binds the controller's scripts back only under $HOME;
# elsewhere it shows the host's files, but it empties /tmp, /run and
# /var/tmp). The UI's dependencies are the controller's lockfile with a
# stand-in install, so --check's web comparison has both sides of it.
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
*registry-*) action=registry ;;
*accept*) action=ship ;;
*" run "*) action=run ;;
*" keys "*) action=keys ;;
*collect*) action=collect ;;
*remove*) action=remove ;;
*preflight*) action=other ;;
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
if [ "$action" = registry ] && [ -n "${STUB_REGISTRY_EXTRA:-}" ] && case $cmd in *registry-need*) true ;; *) false ;; esac; then
    # A compromised runner: it names a path of its choosing too.
    { sh -c "$cmd" || status=$?; printf '%s\n' "$STUB_REGISTRY_EXTRA"; }
    exit "$status"
fi
sh -c "$cmd" || status=$?
if [ "$action" = run ] && [ "${STUB_DROP_VERDICT:-0}" = 1 ]; then
    rm -f "$STUB_ROOT"/jobs/*/out/verdict 2>/dev/null || true
fi
if [ "$action" = run ] && [ "${STUB_TWO_LINE_VERDICT:-0}" = 1 ]; then
    for verdict in "$STUB_ROOT"/jobs/*/out/verdict; do
        [ ! -f "$verdict" ] || printf 'extra line\n' >>"$verdict"
    done
fi
if [ "$action" = run ] && [ "${STUB_FOREIGN_VERDICT:-0}" = 1 ]; then
    for verdict in "$STUB_ROOT"/jobs/*/out/verdict; do
        [ ! -f "$verdict" ] ||
            sed -i 's/"base":"[0-9a-f]*"/"base":"0000000000000000000000000000000000000000"/' "$verdict"
    done
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
    unset STUB_FAIL_SHIP STUB_FAIL_RUN STUB_FAIL_COLLECT STUB_FAIL_REMOVE STUB_DROP_VERDICT \
        STUB_TRUNCATE_COLLECT STUB_TWO_LINE_VERDICT STUB_FOREIGN_VERDICT
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
expect "the record names the machine that judged" "$(jq -r .host "$job/record.json")" "$(uname -n)"
expect "and its result" "$(jq -r '.result' "$job/record.json")" verdict
expect "the attempt's time is in seconds (a gate of exit 0 here takes well under 600)" \
    "$(jq '.attempts[0].seconds | (. >= 0 and . < 600 and . == floor)' "$job/record.json")" true
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

# 8c. A job whose run has not finished is not removed: its gate may still be
# running (a run whose connection died keeps going under the runner's own
# timeout), and deleting the directory under it would strand its server.
# A human may force it.
reset "$out"
dispatch --keep "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a kept job: exit 0" "$status" 0
job=$(job_of "$out")
kept_attempt=$(find "$runner_home/jobs" -maxdepth 1 -name '*.a1' | head -n 1)
[ -n "$kept_attempt" ] && pass "the runner's job is there" || fail "the kept job is missing"
rm -f "$kept_attempt/out/exit"
remove_status=0
"$runner_home/bin/remote" remove "$(basename "$kept_attempt")" >"$scratch/remove.out" 2>"$scratch/remove.err" ||
    remove_status=$?
expect "remove refuses a job with no result" "$remove_status" 4
contains "and says a gate may still be running there" "$scratch/remove.err" "a gate may still be running there"
[ -d "$kept_attempt" ] && pass "the job is still there" || fail "the job was removed anyway"
"$runner_home/bin/remote" remove "$(basename "$kept_attempt")" force
[ ! -e "$kept_attempt" ] && pass "remove force removes it" || fail "remove force left the job"

# 8h. A removal that fails is said and does not fail the dispatch: the gate
# answered, and what stayed on the runner is named. (The helper's own
# refusal of a job whose gate may still be running is case 8c.)
reset "$out"
STUB_FAIL_REMOVE=1 dispatch "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a removal that fails: the dispatch still exits 0" "$status" 0
contains "and the dispatcher says which job it left" "$scratch/dispatch.err" "left the runner's job"
job=$(job_of "$out")
expect "the verdict is the gate's" "$(jq -r .verdict "$job/verdict.json")" accepted
# Tidy: the stub refused every remove; take them by hand.
for attempt_job in "$runner_home"/jobs/*; do
    [ -e "$attempt_job" ] || continue
    "$runner_home/bin/remote" remove "$(basename "$attempt_job")" force
done

# 8i. A verdict that is not one line, and one that names other commits, are
# refused: exit 2 and no verdict file (the gate's answer must be one whole
# line about the commits asked for).
reset "$out"
STUB_TWO_LINE_VERDICT=1 dispatch --retries 0 "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a two-line verdict: exit 2" "$status" 2
job=$(job_of "$out")
[ ! -f "$job/verdict.json" ] && pass "no verdict file" || fail "a two-line verdict became a verdict.json"
contains "and says it is truncated" "$scratch/dispatch.err" "truncated"
reset "$out"
STUB_FOREIGN_VERDICT=1 dispatch --retries 0 "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a verdict that names other commits: exit 2" "$status" 2
job=$(job_of "$out")
[ ! -f "$job/verdict.json" ] && pass "no verdict file either" || fail "a foreign verdict became a verdict.json"
contains "and says so" "$scratch/dispatch.err" "names other commits"

# 8d. Old job directories are pruned on preflight (a gate's own timeout is
# hours, so nothing that old is running); fresh ones stay.
mkdir -p "$runner_home/jobs/old-$$" "$runner_home/jobs/new-$$"
mkdir -p "$GATE_RUNNER_SHORT/old-$$" "$GATE_RUNNER_SHORT/new-$$"
touch -d '10 days ago' "$runner_home/jobs/old-$$" "$GATE_RUNNER_SHORT/old-$$"
"$runner_home/bin/remote" preflight >"$scratch/preflight.out" 2>"$scratch/preflight.err"
[ ! -e "$runner_home/jobs/old-$$" ] && pass "a job older than the keep days is pruned" ||
    fail "an old job survived"
[ -d "$runner_home/jobs/new-$$" ] && pass "a fresh job stays" || fail "a fresh job was pruned"
[ ! -e "$GATE_RUNNER_SHORT/old-$$" ] && [ -d "$GATE_RUNNER_SHORT/new-$$" ] &&
    pass "and its short scratch, old and new" || fail "the short scratch did not follow"
contains "and says what it pruned" "$scratch/preflight.err" "older than 7 days"
rm -rf "$runner_home/jobs/new-$$" "$GATE_RUNNER_SHORT/new-$$"

# 8e. A dispatch to a runner rooted where the gate's sandbox empties its
# directories is refused, not run: its gate could not see the controller.
tmp_root="/tmp/gate-runner-root.$$"
cp -R "$runner_home" "$tmp_root"
status=0
GATE_RUNNER_REMOTE_BIN="$tmp_root/bin/remote" \
    "$repo/deploy/gate-runner.sh" "$tmp_root" "$repo_git" "$base" "$candidate" "$scen" "$out" \
    >"$scratch/dispatch.out" 2>"$scratch/dispatch.err" || status=$?
expect "a mis-rooted runner is refused: exit 3" "$status" 3
contains "and says why" "$scratch/dispatch.err" "the gate's sandbox empties"
rm -rf "$tmp_root"

# 8f. --check fails a missing toolchain tool and a scenarios directory with
# no objective to compare (equal and empty is no comparison).
mkdir -p "$scratch/broken"
printf '#!/bin/sh\nexit 1\n' >"$scratch/broken/jq"
chmod 755 "$scratch/broken/jq"
check_status=0
PATH="$scratch/broken:$PATH" "$repo/deploy/gate-runner.sh" --check "$runner_home" "$repo_git" "$base" "$keysscen" \
    >"$scratch/check.out" 2>"$scratch/check.err" || check_status=$?
expect "--check with a missing tool: exit 1" "$check_status" 1
contains "and the tool is a failure" "$scratch/check.out" "jq           FAIL"
nokeysscen=$scratch/scen-nokeys
mkdir -p "$nokeysscen"
cp "$scen/protected" "$nokeysscen/"
printf '#!/bin/sh\nexit 0\n' >"$nokeysscen/check-ok.sh"
check_status=0
"$repo/deploy/gate-runner.sh" --check "$runner_home" "$repo_git" "$base" "$nokeysscen" \
    >"$scratch/check.out" 2>"$scratch/check.err" || check_status=$?
expect "--check with no objective to compare: exit 1" "$check_status" 1
contains "and says nothing would be shared" "$scratch/check.out" "no objective scenario to compare"

# 8g. A root under this shell's TMPDIR is fine (the gate never runs with
# it): setup.sh accepts it.
tmpdir_root=$HOME/.cache/gate-runner-tmpdir.$$
TMPDIR=$HOME/.cache sh "$repo/deploy/gate-runner/setup.sh" --root "$tmpdir_root" --pg-bin "$pg_bin" \
    >"$scratch/setup3.out" 2>"$scratch/setup3.err" &&
    pass "a root under the setup shell's TMPDIR is accepted" ||
    fail "a root under the setup shell's TMPDIR was refused: $(head -n 1 "$scratch/setup3.err")"
rm -rf "$tmpdir_root"

# The fake runner's settings, kept: the cases below change them.
cp "$runner_home/config" "$scratch/config.orig"
nothing_ran() { # <what>: no job directory came back to $out
    if [ ! -d "$out" ] || [ -z "$(find "$out" -mindepth 1 -maxdepth 1 -type d -not -name '.*')" ]; then
        pass "$1"
    else
        fail "$1: a job came back to $out"
    fi
}

# 8j. What a gate gets is the cgroup's, not the VM's: cpu.max caps nproc
# (rounded up), memory.max less memory.current caps MemAvailable, and the
# default scenarios at once is half the capped cores.
fakecg=$scratch/cgroup
mkdir -p "$fakecg"
printf '150000 100000\n' >"$fakecg/cpu.max"
printf '%s\n' $((3 * 1073741824)) >"$fakecg/memory.max"
printf '%s\n' 1073741824 >"$fakecg/memory.current"
printf 'MemTotal:       16000000 kB\nMemAvailable:   12000000 kB\n' >"$scratch/meminfo"
sed -i 's/^GATE_RUNNER_JOBS=.*/GATE_RUNNER_JOBS=/' "$runner_home/config"
GATE_RUNNER_CGROUP=$fakecg GATE_RUNNER_MEMINFO=$scratch/meminfo \
    "$runner_home/bin/remote" preflight >"$scratch/preflight.out" 2>/dev/null
want_cpus=2
[ "$(nproc)" -ge 2 ] || want_cpus=$(nproc)
expect "cores: nproc capped by cpu.max, rounded up" "$(sed -n 's/^cpus=//p' "$scratch/preflight.out")" "$want_cpus"
expect "memory: what memory.max leaves above memory.current" \
    "$(sed -n 's/^mem_available_kb=//p' "$scratch/preflight.out")" 2097152
expect "the default scenarios at once: half the capped cores" "$(sed -n 's/^jobs=//p' "$scratch/preflight.out")" 1
printf 'max 100000\n' >"$fakecg/cpu.max"
printf 'max\n' >"$fakecg/memory.max"
GATE_RUNNER_CGROUP=$fakecg GATE_RUNNER_MEMINFO=$scratch/meminfo \
    "$runner_home/bin/remote" preflight >"$scratch/preflight.out" 2>/dev/null
expect "no quota: nproc" "$(sed -n 's/^cpus=//p' "$scratch/preflight.out")" "$(nproc)"
expect "no memory.max: MemAvailable" "$(sed -n 's/^mem_available_kb=//p' "$scratch/preflight.out")" 12000000
cp "$scratch/config.orig" "$runner_home/config"

# 8k. A runner that shows the host's files to the gate (an OrbStack machine
# that is not --isolated mounts the Mac at /mnt/mac) is refused before any
# ship; lxcfs and fusectl are the machine's own, not the host's.
printf 'mac /mnt/mac virtiofs rw 0 0\nlxcfs /proc/meminfo fuse.lxcfs rw 0 0\n' >"$scratch/mounts"
printf '/dev/vdb1 /mnt/machines/spawn btrfs rw 0 0\n' >>"$scratch/mounts"
reset "$out"
GATE_RUNNER_MOUNTS=$scratch/mounts dispatch "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a runner showing the host's files is refused: exit 3" "$status" 3
contains "and says why" "$scratch/dispatch.err" "an OrbStack machine must be --isolated"
contains "naming the shares, the host's and another machine's" "$scratch/dispatch.err" "(/mnt/mac,/mnt/machines/spawn)"
lacks "lxcfs is not a host share" "$scratch/dispatch.err" "/proc/meminfo"
nothing_ran "nothing was shipped or run"
check_status=0
GATE_RUNNER_MOUNTS=$scratch/mounts "$repo/deploy/gate-runner.sh" --check "$runner_home" "$repo_git" "$base" "$keysscen" \
    >"$scratch/check.out" 2>"$scratch/check.err" || check_status=$?
expect "--check on that runner: exit 1" "$check_status" 1
contains "with isolation a failure" "$scratch/check.out" "isolation    FAIL"
printf 'lxcfs /proc/meminfo fuse.lxcfs rw 0 0\nfusectl /sys/fs/fuse/connections fusectl rw 0 0\n' >"$scratch/mounts"
GATE_RUNNER_MOUNTS=$scratch/mounts "$runner_home/bin/remote" preflight >"$scratch/preflight.out" 2>/dev/null
expect "lxcfs and fusectl alone: no host share" "$(sed -n 's/^host_shares=//p' "$scratch/preflight.out")" none

# 8l. A cargo config naming a rustc-wrapper the gate's PATH lacks (the
# primary's kache config copied to a runner without kache) is refused: every
# build there would fail, and the gate would reject the work.
mkdir -p "$scratch/rcargo"
printf '[build]\nrustc-wrapper = "no-such-wrapper-%s"\n' "$$" >"$scratch/rcargo/config.toml"
printf "GATE_RUNNER_CARGO_HOME='%s'\n" "$scratch/rcargo" >>"$runner_home/config"
reset "$out"
dispatch "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a runner whose cargo wrapper is missing is refused: exit 3" "$status" 3
contains "and says which" "$scratch/dispatch.err" "names rustc-wrapper no-such-wrapper-$$"
nothing_ran "nothing was run on it"
cp "$scratch/config.orig" "$runner_home/config"

# 8m. The runner's registry gets what it lacks of this host's before the
# job: only the missing files and those of another size, nothing deleted
# there (a running gate binds it read-only), and nothing when in step.
regsrc=$scratch/reg-src
mkdir -p "$regsrc/index/x" "$regsrc/cache/y"
printf 'a\n' >"$regsrc/index/x/one"
printf 'bb\n' >"$regsrc/cache/y/two.crate"
mkdir -p "$scratch/rcargo2"
printf "GATE_RUNNER_CARGO_HOME='%s'\n" "$scratch/rcargo2" >>"$runner_home/config"
reset "$out"
GATE_RUNNER_REGISTRY=$regsrc dispatch "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a dispatch to a runner with no registry: the gate's 0" "$status" 0
[ "$status" = 0 ] || sed -n '1,5s/^/      /p' "$scratch/dispatch.err"
contains "the files it lacked are sent" "$scratch/dispatch.out" "sending 2 file(s) of the cargo registry"
diff -r "$regsrc" "$scratch/rcargo2/registry" >/dev/null && pass "the runner's registry is this host's" ||
    fail "the runner's registry differs from this host's"
printf 'ccc\n' >"$regsrc/cache/y/two.crate"
printf 'theirs\n' >"$scratch/rcargo2/registry/extra"
reset "$out"
GATE_RUNNER_REGISTRY=$regsrc dispatch "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
contains "a file of another size is sent again" "$scratch/dispatch.out" "sending 1 file(s) of the cargo registry"
cmp -s "$regsrc/cache/y/two.crate" "$scratch/rcargo2/registry/cache/y/two.crate" && pass "and replaced there" ||
    fail "the changed file was not replaced"
[ -f "$scratch/rcargo2/registry/extra" ] && pass "nothing of the runner's own is deleted" ||
    fail "the runner's own file was deleted"
reset "$out"
GATE_RUNNER_REGISTRY=$regsrc dispatch "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
lacks "in step, nothing is sent" "$scratch/dispatch.out" "sending"
check_status=0
GATE_RUNNER_REGISTRY=$regsrc "$repo/deploy/gate-runner.sh" --check "$runner_home" "$repo_git" "$base" "$keysscen" \
    >"$scratch/check.out" 2>"$scratch/check.err" || check_status=$?
contains "--check reports the registry" "$scratch/check.out" "0 file(s) of this host's to send"
cp "$scratch/config.orig" "$runner_home/config"

# 8m'. A runner that asks for a path this host's registry does not list
# (a compromised one asking for a secret) gets nothing: exit 3.
secret=$scratch/secret.env
printf 'TOKEN=do-not-send\n' >"$secret"
printf "GATE_RUNNER_CARGO_HOME='%s'\n" "$scratch/rcargo3" >>"$runner_home/config"
mkdir -p "$scratch/rcargo3/registry"
reset "$out"
STUB_REGISTRY_EXTRA="$secret" GATE_RUNNER_REGISTRY=$regsrc dispatch "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a runner asking for a path not in this host's registry: exit 3" "$status" 3
contains "and says so" "$scratch/dispatch.err" "does not list; nothing sent"
[ -z "$(find "$scratch/rcargo3" -name secret.env)" ] && [ ! -e "$scratch/rcargo3/registry/index" ] &&
    pass "and nothing reached the runner" || fail "files reached the runner: $(find "$scratch/rcargo3" -type f | head -3)"
cp "$scratch/config.orig" "$runner_home/config"

# 8m''. Two accepts at once leave every file whole, and no staging behind.
mkdir -p "$scratch/rcargo4/registry"
printf "GATE_RUNNER_CARGO_HOME='%s'\n" "$scratch/rcargo4" >>"$runner_home/config"
big=$scratch/reg-big
mkdir -p "$big/cache/z"
head -c 4000000 /dev/urandom >"$big/cache/z/big.crate"
( tar -cf - -C "$big" cache | sh "$runner_home/bin/remote" registry-accept ) &
a1=$!
( tar -cf - -C "$big" cache | sh "$runner_home/bin/remote" registry-accept ) &
a2=$!
wait "$a1"; s1=$?; wait "$a2"; s2=$?
expect "two accepts at once both succeed" "$s1 $s2" "0 0"
cmp -s "$big/cache/z/big.crate" "$scratch/rcargo4/registry/cache/z/big.crate" && pass "and the file is whole" ||
    fail "the file is not whole"
[ -z "$(find "$scratch/rcargo4" -maxdepth 1 -name '.gate-runner-registry.*' -type d)" ] && pass "and no staging is left" ||
    fail "staging left behind"
cp "$scratch/config.orig" "$runner_home/config"

# 8q. A reader and an accept. A gate that has a crate open reads the file it
# opened, whole; one that opens the crate while the accept is under way finds
# a whole file, the old one or the new, never part of one (the accept renames
# each file into place; unpacked under its final name, the crate is half
# there for as long as its bytes arrive). The accept here is held: its stream
# stops halfway until the readers have read. The staging a killed accept left
# is removed by it, and a second accept started meanwhile waits for the lock:
# it removes nothing of the first's, and both succeed.
mkdir -p "$scratch/rcargo5/registry/cache/z" "$scratch/rcargo5/.gate-runner-registry.killed/cache/z"
printf 'half\n' >"$scratch/rcargo5/.gate-runner-registry.killed/cache/z/half.crate"
printf "GATE_RUNNER_CARGO_HOME='%s'\n" "$scratch/rcargo5" >>"$runner_home/config"
newer=$scratch/reg-newer
mkdir -p "$newer/cache/z"
head -c 4000000 /dev/urandom >"$newer/cache/z/big.crate"
crate=$scratch/rcargo5/registry/cache/z/big.crate
cp "$big/cache/z/big.crate" "$crate"
sum_of() { sha256sum "$1" 2>/dev/null | cut -d' ' -f1; }
old_sum=$(sum_of "$big/cache/z/big.crate")
new_sum=$(sum_of "$newer/cache/z/big.crate")
tar -cf "$scratch/newer.tar" -C "$newer" cache
rm -f "$scratch/accept-done" "$scratch/accept-go"
( exec 3<"$crate" && while [ ! -e "$scratch/accept-done" ]; do sleep 0.1; done && sha256sum <&3 | cut -d' ' -f1 >"$scratch/held.sum" ) &
holder=$!
sleep 0.3
( { head -c 2000000 "$scratch/newer.tar"; while [ ! -e "$scratch/accept-go" ]; do sleep 0.1; done
    tail -c +2000001 "$scratch/newer.tar"; } |
    sh "$runner_home/bin/remote" registry-accept 2>"$scratch/accept-slow.err" ) &
a1=$!
# Until the accept holds the lock and has its own staging (the killed
# accept's is gone by then); 60 s at most.
staging_seen=0
for _ in $(seq 300); do
    staging_seen=$(find "$scratch/rcargo5" -maxdepth 1 -name '.gate-runner-registry.*' -type d | wc -l)
    [ "$staging_seen" != 1 ] || [ -e "$scratch/rcargo5/.gate-runner-registry.killed" ] || break
    sleep 0.2
done
[ ! -e "$scratch/rcargo5/.gate-runner-registry.killed" ] && killed_gone=yes || killed_gone=no
( tar -cf - -C "$regsrc" index | sh "$runner_home/bin/remote" registry-accept 2>"$scratch/accept-second.err" ) &
a2=$!
seen_old=0 seen_new=0 seen_part=0 reads=0
while kill -0 "$a1" 2>/dev/null; do
    case "$(sum_of "$crate")" in
    "$old_sum") seen_old=$((seen_old + 1)) ;;
    "$new_sum") seen_new=$((seen_new + 1)) ;;
    *) seen_part=$((seen_part + 1)) ;;
    esac
    reads=$((reads + 1))
    # Ten reads with half the stream in, then the rest may come.
    [ "$reads" -lt 10 ] || touch "$scratch/accept-go"
    sleep 0.2
done
touch "$scratch/accept-go"
s1=0; wait "$a1" || s1=$?
s2=0; wait "$a2" || s2=$?
touch "$scratch/accept-done"
wait "$holder" || true
expect "a slow accept, and a second one started during it, both succeed" "$s1 $s2" "0 0"
expect "the accept's staging was there while it ran, the killed accept's gone" "$staging_seen $killed_gone" "1 yes"
contains "and the accept says it removed the killed accept's staging" "$scratch/accept-slow.err" ".gate-runner-registry.killed, the staging of an accept that was killed"
expect "a reader opening the crate during the accept never read part of a file ($seen_old whole old, $seen_new whole new)" "$seen_part" 0
[ "$seen_old" -ge 1 ] && pass "and it did read during the accept" || fail "no read happened during the accept: the case proves nothing"
expect "a reader that held the crate open read the whole file it opened" "$(cat "$scratch/held.sum" 2>/dev/null)" "$old_sum"
expect "afterwards the crate is the new one, whole" "$(sum_of "$crate")" "$new_sum"
cmp -s "$regsrc/index/x/one" "$scratch/rcargo5/registry/index/x/one" && pass "and the second accept's file is in place" ||
    fail "the second accept's file is missing"
[ -z "$(find "$scratch/rcargo5" -maxdepth 1 -name '.gate-runner-registry.*' -type d)" ] && pass "and no staging is left" ||
    fail "staging left behind"
cp "$scratch/config.orig" "$runner_home/config"

# 8r. A runner without flock is refused at the preflight, with that reason:
# its registry-accept could not take its lock. A `flock` that does not work
# stands in for a missing one (the preflight reports no version either way).
mkdir -p "$scratch/noflock" "$scratch/rcargo6"
printf '#!/bin/sh\nexit 1\n' >"$scratch/noflock/flock"
chmod 755 "$scratch/noflock/flock"
PATH="$scratch/noflock:$PATH" "$runner_home/bin/remote" preflight >"$scratch/preflight.out" 2>/dev/null
expect "the preflight reports no flock" "$(sed -n 's/^flock=//p' "$scratch/preflight.out")" ""
contains "and reports the one this host has otherwise" "$scratch/setup.out" "flock=flock from util-linux"
reset "$out"
PATH="$scratch/noflock:$PATH" dispatch "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a runner without flock is refused: exit 3" "$status" 3
contains "and says why" "$scratch/dispatch.err" "the runner lacks flock (util-linux): its registry-accept takes a lock"
nothing_ran "nothing was run on it"
check_status=0
PATH="$scratch/noflock:$PATH" "$repo/deploy/gate-runner.sh" --check "$runner_home" "$repo_git" "$base" "$keysscen" \
    >"$scratch/check.out" 2>"$scratch/check.err" || check_status=$?
expect "--check on a runner without flock: exit 1" "$check_status" 1
contains "and flock is a failure" "$scratch/check.out" "flock        FAIL"
printf "GATE_RUNNER_CARGO_HOME='%s'\n" "$scratch/rcargo6" >>"$runner_home/config"
accept_status=0
tar -cf - -C "$regsrc" index | PATH="$scratch/noflock:$PATH" sh "$runner_home/bin/remote" registry-accept \
    2>"$scratch/accept.err" || accept_status=$?
expect "an accept that cannot lock accepts nothing: exit 2" "$accept_status" 2
contains "and says flock is required" "$scratch/accept.err" "flock, of util-linux, is required"
[ ! -e "$scratch/rcargo6/registry/index" ] && pass "and nothing reached the registry" || fail "files reached the registry without the lock"
cp "$scratch/config.orig" "$runner_home/config"

# 8s. A registry on another file system than its cargo home (here a symlink
# out of it): a rename into place would be a copy, so the preflight reports
# it, a dispatch that has a registry to send is refused, --check fails it,
# and an accept accepts nothing.
otherfs=""
for candidate_fs in /dev/shm /tmp; do
    [ -d "$candidate_fs" ] && [ -w "$candidate_fs" ] || continue
    [ "$(stat -c %d "$candidate_fs")" != "$(stat -c %d "$scratch")" ] || continue
    otherfs=$(mktemp -d "$candidate_fs/gate-runner-test-registry.XXXXXX")
    break
done
if [ -z "$otherfs" ]; then
    fail "no second file system (/dev/shm, /tmp) to put a registry on"
else
    mkdir -p "$scratch/rcargo7"
    ln -s "$otherfs" "$scratch/rcargo7/registry"
    printf "GATE_RUNNER_CARGO_HOME='%s'\n" "$scratch/rcargo7" >>"$runner_home/config"
    "$runner_home/bin/remote" preflight >"$scratch/preflight.out" 2>/dev/null
    expect "the preflight reports the registry on another file system" \
        "$(sed -n 's/^registry_fs=//p' "$scratch/preflight.out")" "other ($scratch/rcargo7/registry)"
    reset "$out"
    GATE_RUNNER_REGISTRY=$regsrc dispatch "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
    expect "a runner whose registry is on another file system is refused: exit 3" "$status" 3
    contains "and says why" "$scratch/dispatch.err" "is not on the file system of its cargo home $scratch/rcargo7"
    nothing_ran "nothing was run on it"
    check_status=0
    GATE_RUNNER_REGISTRY=$regsrc "$repo/deploy/gate-runner.sh" --check "$runner_home" "$repo_git" "$base" "$keysscen" \
        >"$scratch/check.out" 2>"$scratch/check.err" || check_status=$?
    expect "--check on it: exit 1" "$check_status" 1
    contains "with the registry a failure" "$scratch/check.out" "registry     FAIL not on the file system of its cargo home"
    accept_status=0
    tar -cf - -C "$regsrc" index | sh "$runner_home/bin/remote" registry-accept 2>"$scratch/accept.err" || accept_status=$?
    expect "an accept there accepts nothing: exit 2" "$accept_status" 2
    [ -z "$(ls -A "$otherfs")" ] && pass "and nothing was copied across" || fail "files were copied to the other file system"
    rm -rf "$otherfs"
    cp "$scratch/config.orig" "$runner_home/config"
fi
for home in rcargo2 rcargo-none-yet; do
    printf "GATE_RUNNER_CARGO_HOME='%s'\n" "$scratch/$home" >>"$runner_home/config"
    "$runner_home/bin/remote" preflight >"$scratch/preflight.out" 2>/dev/null
    expect "a registry in its cargo home, or none yet ($home): the same file system" \
        "$(sed -n 's/^registry_fs=//p' "$scratch/preflight.out")" same
    cp "$scratch/config.orig" "$runner_home/config"
done

# 8t. --check sends the registry before it asks for keys, as a dispatch does:
# a fresh runner's key run needs every crate (its static checks).
mkdir -p "$scratch/rcargo8"
printf "GATE_RUNNER_CARGO_HOME='%s'\n" "$scratch/rcargo8" >>"$runner_home/config"
reset "$out"
check_status=0
GATE_RUNNER_REGISTRY=$regsrc "$repo/deploy/gate-runner.sh" --check "$runner_home" "$repo_git" "$base" "$keysscen" \
    >"$scratch/check.out" 2>"$scratch/check.err" || check_status=$?
contains "--check reports what the fresh runner lacks" "$scratch/check.out" "2 file(s) of this host's to send"
contains "and sends it" "$scratch/check.out" "sending 2 file(s) of the cargo registry"
diff -r "$regsrc" "$scratch/rcargo8/registry" >/dev/null && pass "the runner's registry is this host's after the check" ||
    fail "the runner's registry differs from this host's after --check"
[ -f "$STUB_COUNT_DIR/keys" ] && pass "and then the keys were asked" ||
    fail "the keys were not asked after the send: $(tail -n 3 "$scratch/check.out" "$scratch/check.err" | tr '\n' ' ')"
cp "$scratch/config.orig" "$runner_home/config"

# 8n. A runner whose memory is under the floor is refused, before any ship.
reset "$out"
GATE_RUNNER_MIN_FREE_MEMORY_MB=99999999 dispatch "$runner_home" "$repo_git" "$base" "$candidate" "$scen" "$out"
expect "a runner under the memory floor is refused: exit 3" "$status" 3
contains "and says by how much" "$scratch/dispatch.err" "under the floor of 99999999 MiB"
nothing_ran "nothing was run on it"

# 8p. A read-only scenario directory (a deployed controller is read-only):
# the dispatch judges, and nothing is left behind here or on the runner.
roscen=$scratch/scen-readonly
cp -R "$scen" "$roscen"
chmod -R a-w "$roscen"
reset "$out"
dispatch "$runner_home" "$repo_git" "$base" "$candidate" "$roscen" "$out"
expect "a read-only scenario directory judges: the gate's 0" "$status" 0
job=$(job_of "$out")
attempt=$(jq -r '.attempts[-1].id' "$job/record.json")
[ -n "$attempt" ] && [ ! -e "$runner_home/jobs/$attempt" ] && pass "and the runner's job is removed" ||
    fail "the runner kept its job $attempt"
lacks "and nothing failed to be removed here" "$scratch/dispatch.err" "cannot remove"
chmod -R u+w "$roscen"

# 8o. A root too long for the gate's sockets puts the job's scratch in the
# short directory, on the disk, never in /tmp (a tmpfs on OrbStack machines:
# a gate's builds there are memory, and nested integrators find no room).
longroot=$HOME/.cache/gate-runner-a-root-whose-path-is-long-enough-to-pass-the-socket-limit-$$
GATE_RUNNER_ROOT=$longroot sh "$repo/deploy/gate-runner/setup.sh" --root "$longroot" --pg-bin "$pg_bin" \
    --jobs 2 --min-free-gb 0 --nice 10 >"$scratch/setup4.out" 2>"$scratch/setup4.err" ||
    { cat "$scratch/setup4.err" >&2; exit 1; }
reset "$out"
GATE_RUNNER_REMOTE_BIN=$longroot/bin/remote
dispatch --keep "$longroot" "$repo_git" "$base" "$candidate" "$scen" "$out"
GATE_RUNNER_REMOTE_BIN=$runner_home/bin/remote
expect "a runner with a long root judges: the gate's 0" "$status" 0
job=$(job_of "$out")
attempt=$(jq -r '.attempts[-1].id' "$job/record.json")
[ -n "$attempt" ] && [ -d "$GATE_RUNNER_SHORT/$attempt" ] && pass "its scratch was the short directory, on the disk" ||
    fail "no scratch in $GATE_RUNNER_SHORT for attempt $attempt: $(ls -A "$GATE_RUNNER_SHORT" 2>/dev/null)"
[ ! -e "/tmp/gate-runner/$attempt" ] && pass "and not /tmp/gate-runner" || fail "the scratch went to /tmp/gate-runner/$attempt"
"$longroot/bin/remote" remove "$attempt" >/dev/null 2>&1 || true
[ ! -e "$GATE_RUNNER_SHORT/$attempt" ] && pass "and removing the job removes its short scratch" ||
    fail "the short scratch outlived its job"
rm -rf "$longroot"

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
# Its cargo home is its own and names no rustc-wrapper: this host's may name
# one its PATH lacks (case 8l), and the toolchain is to be the only change.
mkdir -p "$scratch/rcargo-plain"
GATE_RUNNER_ROOT=$runner_home PATH=/usr/bin:/bin sh "$repo/deploy/gate-runner/setup.sh" \
    --root "$runner_home" --pg-bin "$pg_bin" --jobs 2 --min-free-gb 0 --nice 10 --force \
    --cargo-home "$scratch/rcargo-plain" \
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
