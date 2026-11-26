#!/bin/sh
# Black box: an alert written from outside the binary goes through the
# store's mask (objective 172; builds on 170's mask). scripts/rsi/nightly.sh
# wrote its alerts by psql (spawn_alert_record directly), so a landing's
# subject, which is an agent's words, reached the alert and the home page as
# written. `spawn alert record` takes one alert record on stdin and records
# it through the store's alert_record: masked and shape-checked.
#
# The world: a store of its own.
#   (1) `spawn alert record` given an alert.fired whose text and evidence
#       (a landing's subject) hold a GitHub token and a PGPASSWORD= value
#       exits 0. The stored record holds neither value and holds
#       [redacted]; its key, kind and subject keep their bytes;
#       `spawn alerts --once --json` shows the alert open, its text masked.
#   (2) The same alert.fired again is refused (the store's second-fired
#       rule), and anything but an alert.fired or alert.cleared object
#       (an alert.judged, not JSON) is refused before the store, naming
#       what it takes.
#   (3) An alert.cleared for the key clears it.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "172: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od git sed grep curl cut; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_172_alert_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL
HOME=$WORK/home
SPAWN_CLAUDE_PROJECTS=$WORK/home/projects
export HOME SPAWN_CLAUDE_PROJECTS
mkdir -p "$SPAWN_CLAUDE_PROJECTS" || harness "mkdir $SPAWN_CLAUDE_PROJECTS failed"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
XDG_RUNTIME_DIR=$WORK/runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
: >"$WORK/pids"
cleanup() {
    while read -r pid; do
        kill -KILL "$pid" 2>/dev/null || true
    done <"$WORK/pids"
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    set +e
    if [ "$rc" -ne 0 ]; then
        echo "172: the gate runs:" >&2
        cat "$WORK/judged.log" >&2 2>/dev/null || true
        echo "172: the integrator's last lines:" >&2
        grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 20 >&2 || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "172: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

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

note() { echo "172: $*" >&2; }
q() { scalar "$1"; }

TOKEN=ghp_172madeup0123456789abcdefghijklmnopq
PASSWORD=hunter172hunter172
SECRET="curl -H 'Authorization: Bearer $TOKEN' and PGPASSWORD=$PASSWORD psql"
leaks() { case $1 in *"$TOKEN"* | *"$PASSWORD"*) return 0 ;; esac; return 1; }
masked_ok() { # <what> <text>
    if leaks "$2"; then check_failed "$1 holds a credential unmasked: [$(printf '%s' "$2" | head -c 200 | tr '\n' ' ')]"
    elif [ "${2#*\[redacted\]}" = "$2" ]; then check_failed "$1 holds no [redacted]: [$(printf '%s' "$2" | head -c 200 | tr '\n' ' ')]"
    fi
}
newest() { # <type> <key>
    q "select coalesce(kind -> 'detail' -> 'record' ->> '$2', '') from spawn_events
        where kind -> 'detail' -> 'record' ->> 'type' = '$1' order by id desc limit 1"
}
key="nightly_red:spawn:abc172"
fired=$(jq -cn --arg key "$key" --arg s "$SECRET" '{type: "alert.fired", key: $key, kind: "nightly_red",
    subject: "spawn", notice: false, text: ("main is red since abc172; " + $s),
    evidence: {main: "abc172", failed: ["023.sh"], landings: [("abc172 two: " + $s)]}}')
record() { printf '%s' "$1" | "$SPAWN_BIN" alert record >"$WORK/out.txt" 2>"$WORK/err.txt"; }
if record "$fired"; then
    stored=$(q "select (kind -> 'detail' -> 'record')::text from spawn_events
                 where kind -> 'detail' -> 'record' ->> 'type' = 'alert.fired' order by id desc limit 1")
    masked_ok "(1) the stored alert.fired" "$stored"
    [ "$(printf '%s' "$stored" | jq -r '"\(.key) \(.kind) \(.subject)"')" = "$key nightly_red spawn" ] ||
        check_failed "(1) the stored alert's key, kind or subject changed: $(printf '%s' "$stored" | jq -c '{key, kind, subject}')"
    capture "$SPAWN_BIN" alerts --once --json
    shown=$(jq -c --arg key "$key" '[.[] | select(.key == $key)][0] // null' "$WORK/out.txt")
    [ "$(printf '%s' "$shown" | jq -r '.state // ""')" = open ] ||
        check_failed "(1) spawn alerts does not show $key open: $shown"
    masked_ok "(1) the alert spawn alerts shows" "$(printf '%s' "$shown" | jq -r '.text // ""')"
else
    check_failed "(1) spawn alert record refused an alert.fired: $(cat "$WORK/err.txt" | tail -n 2 | tr '\n' ' ')"
fi
note "(1) checked: an alert recorded from outside is masked"

if record "$fired"; then check_failed "(2) the same alert.fired was recorded twice"; fi
for bad in '{"type": "alert.judged", "key": "k"}' 'not json'; do
    if record "$bad"; then
        check_failed "(2) spawn alert record took [$bad]"
    else
        grep -q 'alert.fired or alert.cleared' "$WORK/err.txt" ||
            check_failed "(2) the refusal of [$bad] does not name what it takes: $(tr '\n' ' ' <"$WORK/err.txt" | head -c 200)"
    fi
done
note "(2) checked: a second fired and anything else are refused"

if record "$(jq -cn --arg key "$key" '{type: "alert.cleared", key: $key, kind: "nightly_red", subject: "spawn", evidence: {}}')"; then
    [ "$(q "select state from spawn_alerts where key = '$key'")" = cleared ] ||
        check_failed "(3) $key is $(q "select state from spawn_alerts where key = '$key'"), not cleared"
else
    check_failed "(3) the alert.cleared was refused: $(tail -n 2 "$WORK/err.txt" | tr '\n' ' ')"
fi
note "(3) checked: a cleared clears it"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "172: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "172: an alert recorded from outside the binary is masked and shape-checked"
exit 0
