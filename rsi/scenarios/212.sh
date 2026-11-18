#!/bin/sh
# Black box: the supervisor's exact duplicate brief in flight is not
# assigned; the owner's is, and says what it repeats (GitHub #63 part 2,
# objective 212).
#
# The problem (#63, mission 2): tasks 13 and 14 got the same judge-audit
# brief, 14 was cancelled and 17 was made with that brief again; two crawls
# ran side by side in different workspaces against the same host. Nothing
# compared a new brief with the work in flight.
#
# Required (through `spawn task assign` with the supervisor's own login, as
# its pane has it, and with the owner's, and `spawn mission list`):
#   (1) from the supervisor, a brief that, trimmed, is the brief of a task of
#       the same mission in flight is refused: exit status not 0, the error
#       naming that task, the mission and its state ("task <id> of mission
#       <m> has the same brief and is assigned"); no task is made.
#   (2) the same brief in another mission, and another brief in the same
#       mission, are assigned.
#   (3) `--again` assigns it anyway; `spawn mission list` shows both tasks.
#   (4) once every task with that brief is cancelled, it is assigned.
#   (5) from the owner (one who gives several workers one brief means it),
#       the duplicate is assigned, and its task.assigned record names the
#       task it repeats (duplicate_of); so does the record of (3)'s --again.
#   (6) a task accepted in a mission with no gate is done: its brief is
#       assigned again. In a gated mission a task is in flight while
#       delivered, reported, sent back (revise) and accepted (it still waits
#       for its gate): from the supervisor, its brief is refused, naming the
#       state.
#   (7) that task, once its gate accepts it (the integrator's task.gated:
#       landing), is still in flight: refused, naming it "landing"; once
#       landed (task.landed) it is done: its brief is assigned again.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "212: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od curl sed grep cut awk tr head; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_212_duplicate_$run"
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
serve_pid=""
cleanup() {
    if [ -n "$serve_pid" ]; then
        kill "$serve_pid" 2>/dev/null || true
        wait "$serve_pid" 2>/dev/null || true
    fi
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/out.txt" ]; then
        echo "212: the last output (first 60 lines):" >&2
        head -n 60 "$WORK/out.txt" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "212: harness: unexpected exit $rc" >&2
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
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_WORKSPACE_ROOT=$WORK/ws
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT
mkdir -p "$SPAWN_WORKSPACE_ROOT"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done


note() { echo "212: $*" >&2; }
expect_eq() { # <got> <want> <what>
    # A filter that does not compile or run is the harness's fault, not a red.
    case "$1" in *jq-failed*) harness "$3: a jq filter failed" ;; esac
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# The supervisor's own login, as its pane gets it: this database's
# supervisor login, with a password made up here and handed over in a
# passfile (never on a command line).
sup_role=$(scalar "select spawn_login_role('spawn_supervisor')")
[ -n "$sup_role" ] || harness "this database has no supervisor login"
sup_pw="bb212$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
printf 'alter role "%s" password '"'"'%s'"'"'\n' "$sup_role" "$sup_pw" |
    psql -d "$SPAWN_DB_NAME" -qtA -v ON_ERROR_STOP=1 -f - >/dev/null 2>"$WORK/role.err" ||
    harness "cannot set the supervisor login's password: $(head -c 300 "$WORK/role.err")"
escape() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/:/\\:/g'; }
printf '%s:%s:%s:%s:%s\n' "$(escape "$pg_url_host")" "$pg_url_port" "$SPAWN_DB_NAME" "$sup_role" "$sup_pw" \
    >"$WORK/sup.pass"
chmod 600 "$WORK/sup.pass"
sup_url="$pg_url_scheme://$sup_role@$(pg_url_encode_host "$pg_url_host"):$pg_url_port/$SPAWN_DB_NAME"

BRIEF="Audit the judge's verdicts of the last run."
# <mission> [extra flags...]: the supervisor's assign; the task id, or ""
# when refused; leaves out/err.
assign() {
    m=$1; shift
    capture env -u PGPASSWORD SPAWN_DATABASE_URL="$sup_url" PGPASSFILE="$WORK/sup.pass" \
        "$SPAWN_BIN" task assign --mission "$m" --kind ephemeral --worker judge --brief "$BRIEF" "$@"
    if [ "$status" -eq 0 ]; then out | jq -r '.task'; else echo ""; fi
}
mission_add() {
    capture "$SPAWN_BIN" mission add --text "$1"
    [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
    out | jq -r '.mission // .id'
}
count_tasks() { scalar "select count(*) from mission_tasks where mission_id = $1"; }
# <task>: what its task.assigned record says it repeats ("" for none)
repeats_of() {
    scalar "select coalesce(kind -> 'detail' -> 'record' ->> 'duplicate_of', '') from spawn_events
             where session_id = 'session_control'
               and kind -> 'detail' -> 'record' ->> 'type' = 'task.assigned'
               and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}

m1=$(mission_add "212 judge audit")
m2=$(mission_add "212 another mission")
[ -n "$m1" ] && [ "$m1" != null ] && [ -n "$m2" ] && [ "$m2" != null ] || harness "missions not made: [$m1] [$m2]"
first=$(assign "$m1")
[ -n "$first" ] && [ "$first" != null ] || harness "the supervisor's first assign failed: $(err)"

# --- (1) the same brief, trimmed, in flight in the same mission ---------------------------------

second=$(assign "$m1")
expect_eq "$second" "" "(1) the same brief while task $first is assigned is refused"
grep -q "task $first of mission $m1 has the same brief and is assigned" "$WORK/err.txt" ||
    check_failed "(1) the refusal names task $first, mission $m1 and its state: [$(head -c 300 "$WORK/err.txt")]"
BRIEF_SAVED=$BRIEF
BRIEF="  $BRIEF_SAVED
"
spaced=$(assign "$m1")
BRIEF=$BRIEF_SAVED
expect_eq "$spaced" "" "(1) the same brief with other leading and trailing space is refused"
expect_eq "$(count_tasks "$m1")" 1 "(1) no task is made by a refused assign"
note "(1) checked: refused, naming the task"

# --- (2) another mission, another brief ---------------------------------------------------------

elsewhere=$(assign "$m2")
[ -n "$elsewhere" ] || check_failed "(2) the same brief in another mission is assigned: $(head -c 300 "$WORK/err.txt")"
capture "$SPAWN_BIN" task assign --mission "$m1" --kind ephemeral --worker crawl --brief "Crawl arXiv."
[ "$status" -eq 0 ] || check_failed "(2) another brief in the same mission is assigned: $(err)"
note "(2) checked: no duplicate across missions or briefs"

# --- (3) --again -------------------------------------------------------------------------------

again=$(assign "$m1" --again)
[ -n "$again" ] || check_failed "(3) --again assigns the duplicate: $(head -c 300 "$WORK/err.txt")"
capture "$SPAWN_BIN" mission list
[ "$status" -eq 0 ] || harness "mission list failed: $(err)"
same=$(out | jq --argjson m "$m1" --arg b "$BRIEF" '[.. | objects | select(has("tasks")) | select(.id == $m) | .tasks[] | select(.brief == $b)] | length' 2>/dev/null || echo jq-failed)
expect_eq "$same" 2 "(3) mission $m1 lists two tasks with the brief after --again"
note "(3) checked: --again"

# --- (4) cancelled -------------------------------------------------------------------------------

for t in "$first" "$again"; do
    [ -n "$t" ] || continue
    capture "$SPAWN_BIN" task cancel --id "$t"
    [ "$status" -eq 0 ] || harness "task cancel $t failed: $(err)"
done
fresh=$(assign "$m1")
[ -n "$fresh" ] || check_failed "(4) with every task of the brief cancelled it is assigned: $(head -c 300 "$WORK/err.txt")"
note "(4) checked: cancelled is not in flight"

# --- (5) the owner's duplicate --------------------------------------------------------------

capture "$SPAWN_BIN" task assign --mission "$m1" --kind ephemeral --worker judge --brief "$BRIEF"
owners=""
if [ "$status" -eq 0 ]; then owners=$(out | jq -r '.task'); fi
if [ -z "$owners" ] || [ "$owners" = null ]; then
    check_failed "(5) the owner's duplicate of task $fresh is assigned: $(err)"
else
    expect_eq "$(repeats_of "$owners")" "$fresh" "(5) the owner's duplicate's record names the task it repeats"
fi
[ -z "$again" ] || expect_eq "$(repeats_of "$again")" "$first" "(5) the record of (3)'s --again names the task it repeats"
note "(5) checked: the owner's duplicate is assigned and names what it repeats"

# --- (6) accepted: done without a gate, in flight with one -------------------------------------

# <task>: delivered, reported and accepted (the owner's accept)
accept_task() {
    scalar "select spawn_task_delivered($1, 1)" >/dev/null
    scalar "select spawn_task_reported($1, 'done')" >/dev/null
    capture "$SPAWN_BIN" task accept --id "$1"
    [ "$status" -eq 0 ] || harness "task accept $1 failed: $(err)"
    [ "$(scalar "select state from mission_tasks where id = $1")" = accepted ] ||
        harness "task $1 is $(scalar "select state from mission_tasks where id = $1"), not accepted"
}
# Each mission here has a brief of its own, so (6) stands whatever (1)-(5) found.
BRIEF="$BRIEF_SAVED (ungated)"
m4=$(mission_add "212 ungated mission")
[ -n "$m4" ] && [ "$m4" != null ] || harness "the ungated mission was not made"
[ "$(scalar "select gate is null from mission_missions where id = $m4")" = t ] || harness "mission $m4 has a gate"
done_task=$(assign "$m4")
[ -n "$done_task" ] || harness "the supervisor's assign in mission $m4 failed: $(err)"
accept_task "$done_task"
done_again=$(assign "$m4")
[ -n "$done_again" ] ||
    check_failed "(6) the brief of task $done_task, accepted in mission $m4 with no gate, is refused: $(head -c 300 "$WORK/err.txt")"

BRIEF="$BRIEF_SAVED (gated)"
m3=$(mission_add "212 gated mission")
[ -n "$m3" ] && [ "$m3" != null ] || harness "the gated mission was not made"
scalar "select spawn_mission_configure($m3, '$WORK/scenarios', 'review', null, null)" >/dev/null
[ "$(scalar "select gate is not null from mission_missions where id = $m3")" = t ] ||
    harness "mission $m3 has no gate"
gated=$(assign "$m3")
[ -n "$gated" ] || harness "the supervisor's assign in the gated mission failed: $(err)"
# On its way: delivered, reported, sent back (revise), each in flight (R2).
# <state>: the supervisor's same brief is refused, naming task $gated and <state>
refused_while() {
    [ "$(scalar "select state from mission_tasks where id = $gated")" = "$1" ] ||
        harness "task $gated is $(scalar "select state from mission_tasks where id = $gated"), not $1"
    dup=$(assign "$m3")
    expect_eq "$dup" "" "(6) the same brief while task $gated is $1 is refused"
    grep -q "task $gated of mission $m3 has the same brief and is $1" "$WORK/err.txt" ||
        check_failed "(6) the refusal names task $gated, mission $m3 and $1: [$(head -c 300 "$WORK/err.txt")]"
}
scalar "select spawn_task_delivered($gated, 1)" >/dev/null
refused_while delivered
scalar "select spawn_task_reported($gated, 'done')" >/dev/null
refused_while reported
capture "$SPAWN_BIN" task reject --id "$gated" --note "made-up: again"
[ "$status" -eq 0 ] || harness "task reject $gated failed: $(err)"
refused_while revise
accept_task "$gated"
waiting=$(assign "$m3")
expect_eq "$waiting" "" "(6) the same brief while task $gated is accepted in a gated mission is refused"
grep -q "task $gated of mission $m3 has the same brief and is accepted" "$WORK/err.txt" ||
    check_failed "(6) the refusal names task $gated, mission $m3 and accepted: [$(head -c 300 "$WORK/err.txt")]"
note "(6) checked: accepted is done without a gate, in flight with one"

# --- (7) landing in flight, landed done (R2, rev-i's F1 and n1) ---------------------------------

# <record json>: as the integrator records it (the owner may)
integrator() { scalar "select spawn_integrator_record('$1'::jsonb)" >/dev/null; }
rev=$(scalar "select revisions from mission_tasks where id = $gated")
integrator "{\"type\": \"task.gated\", \"id\": $gated, \"revision\": ${rev:-0}, \"verdict\": \"accepted\", \"base\": \"1111111111111111111111111111111111111111\", \"candidate\": \"2222222222222222222222222222222222222222\", \"judging\": \"0193a2b4c5d6e7f80123456789abcdef\"}"
[ "$(scalar "select state from mission_tasks where id = $gated")" = landing ] ||
    harness "task $gated is $(scalar "select state from mission_tasks where id = $gated") after its gate accepted it, not landing"
landing_dup=$(assign "$m3")
expect_eq "$landing_dup" "" "(7) the same brief while task $gated is landing is refused"
grep -q "task $gated of mission $m3 has the same brief and is landing" "$WORK/err.txt" ||
    check_failed "(7) the refusal names task $gated, mission $m3 and landing: [$(head -c 300 "$WORK/err.txt")]"
integrator "{\"type\": \"task.landed\", \"id\": $gated, \"from\": \"1111111111111111111111111111111111111111\", \"to\": \"2222222222222222222222222222222222222222\", \"judging\": \"0193a2b4c5d6e7f80123456789abcdef\"}"
[ "$(scalar "select state from mission_tasks where id = $gated")" = landed ] ||
    harness "task $gated is $(scalar "select state from mission_tasks where id = $gated") after task.landed, not landed"
after_landed=$(assign "$m3")
[ -n "$after_landed" ] ||
    check_failed "(7) the brief of task $gated, landed, is refused: $(head -c 300 "$WORK/err.txt")"
BRIEF=$BRIEF_SAVED
note "(7) checked: landing is in flight, landed is done"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "212: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "212: the supervisor's exact duplicate brief in flight is refused, naming the task; --again and a cancelled task let it through; the owner's is assigned, naming what it repeats"
exit 0
