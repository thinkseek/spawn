#!/bin/sh
# Black box: a base warm that an integrator restart cuts off is warmed
# again, for the source's main as it is then, by the integrator that
# starts next.
#
# The problem (rev-f, 2026-11-12 16:25; backlog S30): the integrator's
# landing-time base warm (`gate.sh --base-only`, integrate/warm.rs) runs as
# its child under `setpriv --pdeathsig TERM`, so every restart of
# spawn-integrate (a deploy restarts it with almost every landing group)
# kills the warm in flight. The next integrator reads the warm's pid file,
# says "the base warm of <commit> ... a previous integrator started is gone
# ...; nothing stopped", and drops the commit: nothing warms that main
# again. On 11-12 the 66dc88cf warm started 16:03:44 and died at the
# 16:05:36 deploy; the next gate on 66dc88cf listed the base's tests itself
# ("not in the cache (none): listed now", 77 s) and judged the base cold.
# The same at 15:21 for 47348208.
#
# Required: a warm's pid file also records its source and scenarios; a
# starting integrator that finds a warm a predecessor left (its pid file),
# whether gone or stopped by the take-over, warms the base of that warm's
# source's main as it is now, with that warm's scenarios, at its first
# passes: the lost warm's commit when main has not moved, main's new
# commit when it has. The warm runs as any warm does (one at a time,
# abandoned when main moves on, under the parent-death signal). A pid file
# that names no source (an older integrator's) re-warms nothing, and the
# integrator says so.
#
# The world: a gated mission on one source, judged by a recording gate
# (SPAWN_GATE). A candidate is `accepted`. A base warm (`--base-only`)
# notes its start and pid, holds until its release file, then writes its
# entries into the base cache ($XDG_CACHE_HOME/spawn/gate-base: a test
# listing entry and a scenario result, as gate.sh does) and notes its end.
# Integrators are stopped with TERM, as a restart stops them.
#   (1) the task is gated and lands (main L); the integrator starts the
#       warm of L.
#   (2) TERM: the integrator and its warm are gone within 10 s; the warm
#       never ended.
#   (3) a second integrator on the same root starts a warm of L within
#       30 s.
#   (4) while it holds, the source's main moves to M (an owner's commit)
#       and the second integrator gets TERM; a third integrator starts a
#       warm of M within 30 s, and none of L. Released, it ends, and M's
#       listing and result entries are in the base cache.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "096: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
# The tree under test: the gate runs a scenario from the candidate's tree,
# and $controller is the deployed controller's copy, which lacks the
# candidate's web/src until it lands.
tree=$(pwd)
[ -f "$tree/web/package.json" ] && [ -d "$tree/web/src" ] ||
    harness "run from the tree under test: no web/src under $tree"
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od git sed grep sha256sum cut setpriv; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_096_warm_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL
# A scratch HOME and Claude Code projects dir: nothing here reads or writes
# the real ones.
HOME=$WORK/home
SPAWN_CLAUDE_PROJECTS=$WORK/home/projects
export HOME SPAWN_CLAUDE_PROJECTS
mkdir -p "$SPAWN_CLAUDE_PROJECTS" || harness "mkdir $SPAWN_CLAUDE_PROJECTS failed"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
# The runtime directory too (backlog S20): what Spawn keeps there (a
# launch's environment file, its terminals' leaders) stays in WORK, never
# in the real one. systemd's user manager stays reachable by its bus.
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
# The integrators and warms this run started, by pid, one per line.
: >"$WORK/pids"
cleanup() {
    while read -r pid; do
        kill -KILL "$pid" 2>/dev/null || true
    done <"$WORK/pids"
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        echo "096: the gate runs:" >&2
        cat "$WORK/judged.log" >&2 2>/dev/null || true
        echo "096: the integrators' last lines:" >&2
        grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 20 >&2 || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "096: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

# Logins of this scenario's databases that a run killed before its cleanup
# left behind (scripts/blackbox/scenario-db.sh): this prefix only.
sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

# A failed check is reported and the run goes on; the run fails at its end
# if any did. Unless the database is gone: then nothing read can be trusted.
: >"$WORK/failures"
check_failed() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL \
    SPAWN_GATE_MIN_FREE_GB SPAWN_GATE_JOBS SPAWN_GATE_WORKER_CORES SPAWN_GATE_MIN_FREE_MEMORY_MB \
    SPAWN_INTEGRATE_GATES SPAWN_TEST_DATABASE_LOG SPAWN_INTEGRATE_DRAIN_SECS SPAWN_GATE_ENV_FILE \
    SPAWN_INTEGRATE_WARM SPAWN_INTEGRATE_SHADOW SPAWN_GATE_RUNNER; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_WORKSPACE_ROOT=$WORK/ws
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT
mkdir -p "$SPAWN_WORKSPACE_ROOT"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done


note() { echo "096: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the world -------------------------------------------------------------------------

cat >"$WORK/gitconfig" <<'EOF'
[user]
	name = bb
	email = bb@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
EOF
GIT_CONFIG_GLOBAL=$WORK/gitconfig
export GIT_CONFIG_GLOBAL
source_repo=$WORK/source.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "base"
git init -q --bare --initial-branch=main "$source_repo"
git -C "$WORK/seed" push -q "$source_repo" main:main
SPAWN_GIT_REPO=$source_repo
export SPAWN_GIT_REPO

gate=$WORK/ctl/rsi/scenarios
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
: >"$gate/no-test-database"
: >"$WORK/judged.log"
: >"$WORK/integrate.log"
cache=$WORK/cache
mkdir -p "$cache"

# The recording gate. A warm's arguments: --base-only <repo> <commit>
# <scenarios>. Each warm is run n (1, 2, ...): it notes `warm-start n
# <commit>`, writes its pid to warm-n.pid, holds until release-n (at most
# 150 s), writes its entries as gate.sh does, and notes `warm-end n
# <commit>`.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
note() { printf '%s %s\n' "\$(date +%s%3N)" "\$*" >>'$WORK/judged.log'; }
if [ "\$1" = --base-only ]; then
    commit=\$3
    n=\$(cat '$WORK/warms' 2>/dev/null || echo 0)
    n=\$((n + 1))
    echo "\$n" >'$WORK/warms'
    echo \$\$ >'$WORK'/warm-\$n.pid
    echo \$\$ >>'$WORK/pids'
    note warm-start "\$n" "\$commit"
    i=0
    while [ ! -e '$WORK'/release-\$n ] && [ "\$i" -lt 1500 ]; do
        sleep 0.1
        i=\$((i + 1))
    done
    mkdir -p '$cache/spawn/gate-base'
    key=\$(printf 'listing %s' "\$commit" | sha256sum | cut -c1-64)
    printf 'spawn gate tests listing v1 %s\nrunnable 1\nbb::a_test\n' "\$key" >'$cache/spawn/gate-base/'"\$key"
    key=\$(printf 'result %s check-ok' "\$commit" | sha256sum | cut -c1-64)
    printf 'solved\n' >'$cache/spawn/gate-base/'"\$key"
    note warm-end "\$n" "\$commit"
    echo "| base only: 1 results, the listing (1)"
    exit 0
fi
note gate "\$@"
echo "| == candidate: build"
echo "| stage gate ok 900ms candidate: build"
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
exit 0
EOF
chmod 755 "$WORK/fake-gate.sh"

# --- helpers ---------------------------------------------------------------------------------

now() { date +%s; }
q() { scalar "$1"; }
task_state() { q "select state from mission_tasks where id = $task"; }
warm_runs() { # <commit>: how many warms of it started
    awk -v c="$1" '$2 == "warm-start" && $4 == c { n++ } END { print n + 0 }' "$WORK/judged.log"
}
warm_ends() { awk -v c="$1" '$2 == "warm-end" && $4 == c { n++ } END { print n + 0 }' "$WORK/judged.log"; }
warms() { cat "$WORK/warms" 2>/dev/null || echo 0; }
gone() {
    [ ! -e "/proc/$1" ] || sed -n 's/^[0-9]* (.*) \([A-Z]\) .*/\1/p' "/proc/$1/stat" 2>/dev/null | grep -qx Z
}
wait_for() { # <seconds> <what> <command...>
    limit=$1
    what=$2
    shift 2
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.2
    done
}
# A new integrator on the same root, as a restart starts one: no systemd
# user manager (its gates are its children), the base warm on (the default)
# with its pid file and logs under the cache.
start_integrator() {
    rm -f "$WORK/integrate.pid"
    (
        env -u DBUS_SESSION_BUS_ADDRESS -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
            XDG_CACHE_HOME="$cache" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
            SPAWN_GATE_JOBS=1 SPAWN_GATE_WORKER_CORES=0 SPAWN_LOOP_INTERVAL_MS=300 \
            "$SPAWN_BIN" integrate --root "$WORK/integrator" >>"$WORK/integrate.log" 2>&1 &
        echo "$!" >"$WORK/integrate.pid.new"
        mv "$WORK/integrate.pid.new" "$WORK/integrate.pid"
    )
    integrator_pid=$(cat "$WORK/integrate.pid")
    echo "$integrator_pid" >>"$WORK/pids"
}
restart_stop() { # TERM, as systemd's restart; both gone within 10 s
    kill -TERM "$integrator_pid" 2>/dev/null || true
    wait_for 10 "the integrator" gone "$integrator_pid" ||
        harness "the integrator (pid $integrator_pid) outlived TERM by 10 s"
}

# --- (1) a landing and its warm ---------------------------------------------------------------

mkdir -p "$WORK/integrator"
capture "$SPAWN_BIN" mission add --text "096 warm" --source "$source_repo" --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker x --brief "work of x"
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(jq -r .task "$WORK/out.txt")
agent=$(q "select agent_id from mission_tasks where id = $task")
session=$(q "select session_id from mission_tasks where id = $task")
capture "$SPAWN_BIN" submit --agent "$agent" --session "$session" --key kw1 \
    --action '{"type":"start","engine":"claude","prompt":"write X.txt"}'
[ "$status" -eq 0 ] || harness "submit failed: $(err)"
# The worker's branch, as the pusher publishes it.
git -C "$WORK/seed" checkout -q -b work
printf 'from x\n' >"$WORK/seed/X.txt"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "x"
git -C "$WORK/seed" push -q "$source_repo" "work:refs/heads/spawn/$agent"
git -C "$WORK/seed" checkout -q main
command=$(q "select id from spawn_commands where idempotency_key = 'kw1'")
[ "$(q "select spawn_task_delivered($task, $command)")" = t ] || harness "task $task not delivered"
[ "$(q "select spawn_task_reported($task, 'done')")" = t ] || harness "task $task not reported"

start_integrator
landed() { [ "$(task_state)" = landed ]; }
wait_for 60 "the landing" landed || harness "task $task did not land within 60 s (state $(task_state))"
L=$(git --git-dir="$source_repo" rev-parse main)
first_warm() { [ "$(warm_runs "$L")" -ge 1 ]; }
if ! wait_for 30 "the warm of L" first_warm; then
    check_failed "(1) no warm of the landed main $L within 30 s of the landing"
    asserted=1
    echo "096: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
note "(1) checked: task $task landed, warm 1 of $L started"

# --- (2) a restart cuts the warm off ----------------------------------------------------------

warm1=$(cat "$WORK/warm-1.pid")
restart_stop
wait_for 10 "warm 1" gone "$warm1" || harness "warm 1 (pid $warm1) outlived its integrator by 10 s"
expect_eq "$(warm_ends "$L")" 0 "(2) the cut-off warm of L never ended"
note "(2) checked: the integrator and its warm are gone"

# --- (3) the next integrator warms L again ----------------------------------------------------

start_integrator
second_warm() { [ "$(warm_runs "$L")" -ge 2 ]; }
if wait_for 30 "a second warm of L" second_warm; then
    note "(3) checked: warm $(warms) of $L started by the next integrator"
else
    check_failed "(3) the next integrator started no warm of $L within 30 s (warm runs: $(warms); its words: $(grep -i 'warm' "$WORK/integrate.log" | grep -v '^| ' | tail -n 3 | tr '\n' ' ' | head -c 400))"
fi

# --- (4) main moves meanwhile: the warm is of main as it is ----------------------------------

git -C "$WORK/seed" pull -q "$source_repo" main
printf 'owner\n' >"$WORK/seed/OWNER.txt"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "the owner moves main"
git -C "$WORK/seed" push -q "$source_repo" main:main
M=$(git --git-dir="$source_repo" rev-parse main)
[ "$M" != "$L" ] || harness "main did not move"
before_l=$(warm_runs "$L")
held=$(warms)
restart_stop
[ "$held" -eq 0 ] || { pid=$(cat "$WORK/warm-$held.pid"); wait_for 10 "warm $held" gone "$pid" || harness "warm $held outlived its integrator"; }
start_integrator
warm_of_m() { [ "$(warm_runs "$M")" -ge 1 ]; }
if wait_for 30 "a warm of M" warm_of_m; then
    n=$(awk -v c="$M" '$2 == "warm-start" && $4 == c { print $3; exit }' "$WORK/judged.log")
    : >"$WORK/release-$n"
    ended_m() { [ "$(warm_ends "$M")" -ge 1 ]; }
    wait_for 30 "the warm of M to end" ended_m || check_failed "(4) the warm of $M did not end within 30 s of its release"
    listing=$(printf 'listing %s' "$M" | sha256sum | cut -c1-64)
    result=$(printf 'result %s check-ok' "$M" | sha256sum | cut -c1-64)
    [ -f "$cache/spawn/gate-base/$listing" ] && [ -f "$cache/spawn/gate-base/$result" ] ||
        check_failed "(4) M's listing and result entries are not in the base cache: $(ls "$cache/spawn/gate-base" 2>/dev/null | tr '\n' ' ')"
    expect_eq "$(warm_runs "$L")" "$before_l" "(4) the third integrator warms main as it is (M), not the lost warm's L"
    note "(4) checked: the warm of $M ran to its end, its entries in the cache"
else
    check_failed "(4) the third integrator started no warm of main $M within 30 s (warm runs: $(cat "$WORK/judged.log" | awk '$2 == "warm-start"' | tr '\n' ' '))"
fi
restart_stop

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "096: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "096: a base warm cut off by an integrator restart is warmed again, for main as it is, by the next integrator"
exit 0
