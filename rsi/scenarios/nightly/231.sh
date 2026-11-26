#!/bin/sh
# Objective 231 [observability]: the Core lines restores read a drill unit
# that only the user manager knows (a `--user` unit) from the user manager,
# so a drill that passes daily is not counted as never passed.
#
# The problem (rev-k's survey 2026-11-19 10:42; the lead's ask 11:11): the
# controllers' drill, spawn-backup-restore-drill-controllers.service, is a
# --user unit (deploy/backup/README.md), listed in the Core lines file as a
# plain "unit" row. serve/core_lines/drills.rs asked only the system
# manager (`systemctl show ... -- <unit>`), whose answer for a unit it does
# not know is LoadState=not-found, Result=success, ExecMainStatus=0 and no
# exit timestamp: no pass, no last run. So live read "restores passed in 7
# days: 3 of 6" with the controllers' drill passing every morning.
#
# Required (read through `GET /api/core-lines`, a stand-in SPAWN_SYSTEMCTL
# that answers as the system manager, or as the user manager when its first
# argument is --user; each call logged):
#   (1) a row whose unit the system manager does not know is read from the
#       user manager: usr-c (passed 10 h ago) has its passed_at_ms and last
#       from the --user answer; usr-f (failed 10 h ago) has no pass and last
#       {ok false, "Result=exit-code, ExecMainStatus=1"};
#   (2) a unit neither manager knows has no pass and no last (none-x);
#   (3) a unit the system manager knows is read from it alone, never from
#       the user manager: sys-a (loaded, passed 10 h ago; the user manager
#       would say another time) and legacy-l (an answer with no LoadState
#       line, as stand-ins and older systemd say) keep the system answer, and
#       --user is asked exactly for usr-c, usr-f and none-x, once each;
#       (3k, rev-k's R1 F1) sys-f (a system unit that failed; the user
#       manager would say passed) keeps its failure, and sys-e (the system
#       call exits 1) has no reading: neither is asked of the user manager;
#   (4) the north star restores_passed counts usr-c: 4 (sys-a, legacy-l,
#       masked-m, usr-c), where the system manager alone gives 3;
#   (5) a second reading within the 60 s keep asks systemctl nothing more;
#   (6, rev-j's R1 candidates) only LoadState=not-found falls back:
#       masked-m (a system unit the system manager says is masked, passed
#       10 h ago; the user manager would say another time) keeps the system
#       answer and is never asked with --user; and the serve's log says each
#       user call: "core-lines: <systemctl> show usr-c.service: not a system
#       unit (N ms); asking the user manager", then "core-lines: <systemctl>
#       --user show usr-c.service: N ms", and no --user line for sys-a.
# The stand-in prints only the properties -p names, as systemctl does: a
# LoadState line only when the call asks for it.
#
# Polled-read cost: one more systemctl call per user unit per minute (the
# 60 s keep), 1-2 ms on live; no database read.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "231: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od curl sed grep cut; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_231_units_$run"
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
    set +e
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "231: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "231: harness: unexpected exit $rc" >&2
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


note() { echo "231: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the stand-in managers ------------------------------------------------------------------

seed_now=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]" ;; esac
H=3600000
a_s=$(((seed_now - 10 * H) / 1000))
other_s=$(((seed_now - 2 * H) / 1000))
mkdir -p "$WORK/bin"
{
    echo '#!/bin/sh'
    echo '# A stand-in systemctl, named by SPAWN_SYSTEMCTL: each call logged; the unit is its last'
    echo '# argument; --user first answers as the user manager, otherwise as the system manager.'
    echo "printf '%s\\n' \"\$*\" >>\"$WORK/systemctl.calls\""
    echo '# As systemctl, it prints only the properties -p names: LoadState only when asked for.'
    echo 'props=; next=; for a; do [ -n "$next" ] && props=$a; next=; [ "$a" = -p ] && next=1; unit=$a; done'
    echo 'out() { case ",$props," in *,LoadState,*) printf "$1" ;; *) printf "$1" | grep -v "^LoadState=" ;; esac; }'
    echo 'if [ "$1" = --user ]; then'
    echo '  case "$unit" in'
    echo "  usr-c.service) out 'Result=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@$a_s\\nLoadState=loaded\\n' ;;"
    echo "  usr-f.service) out 'Result=exit-code\\nExecMainStatus=1\\nExecMainExitTimestamp=@$a_s\\nLoadState=loaded\\n' ;;"
    echo "  sys-a.service|legacy-l.service|masked-m.service|sys-f.service|sys-e.service) out 'Result=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@$other_s\\nLoadState=loaded\\n' ;;"
    echo "  *) out 'Result=success\\nExecMainStatus=0\\nExecMainExitTimestamp=\\nLoadState=not-found\\n' ;;"
    echo '  esac'
    echo 'else'
    echo '  case "$unit" in'
    echo "  sys-a.service) out 'Result=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@$a_s\\nLoadState=loaded\\n' ;;"
    echo "  masked-m.service) out 'Result=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@$a_s\\nLoadState=masked\\n' ;;"
    echo "  sys-f.service) out 'Result=exit-code\\nExecMainStatus=1\\nExecMainExitTimestamp=@$a_s\\nLoadState=loaded\\n' ;;"
    echo "  sys-e.service) echo 'Failed to connect to bus' >&2; exit 1 ;;"
    echo "  legacy-l.service) out 'Result=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@$a_s\\n' ;;"
    echo "  *) out 'Result=success\\nExecMainStatus=0\\nExecMainExitTimestamp=\\nLoadState=not-found\\n' ;;"
    echo '  esac'
    echo 'fi'
} >"$WORK/bin/systemctl"
chmod 755 "$WORK/bin/systemctl"
SPAWN_SYSTEMCTL=$WORK/bin/systemctl
export SPAWN_SYSTEMCTL
cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<'JSON'
{"ranges": {},
 "recoverability": {
   "drills": [
     {"name": "System drill", "unit": "sys-a.service"},
     {"name": "Legacy answer", "unit": "legacy-l.service"},
     {"name": "Controllers", "unit": "usr-c.service"},
     {"name": "User drill failed", "unit": "usr-f.service"},
     {"name": "Nowhere", "unit": "none-x.service"},
     {"name": "Masked", "unit": "masked-m.service"},
     {"name": "System drill failed", "unit": "sys-f.service"},
     {"name": "System manager error", "unit": "sys-e.service"}
   ]}}
JSON

rm -f "$WORK/serve.log"
web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
"$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" >"$WORK/serve.log" 2>&1 &
serve_pid=$!
port=""
i=0
while [ "$i" -lt 100 ]; do
    i=$((i + 1))
    port=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$WORK/serve.log" 2>/dev/null | head -n 1)
    [ -n "$port" ] && break
    sleep 0.1
done
[ -n "$port" ] || harness "serve did not start: $(head -c 400 "$WORK/serve.log")"
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/core-lines answered $code: $(head -c 200 "$WORK/api.json")"

a_ms=$((a_s * 1000))
row() { jq -c --arg n "$1" '.drills[] | select(.name == $n) | {passed_at_ms, last}' "$WORK/api.json"; }

# --- (1) a unit only the user manager knows ------------------------------------------------------
expect_eq "$(row Controllers)" "{\"passed_at_ms\":$a_ms,\"last\":{\"at_ms\":$a_ms,\"ok\":true,\"reason\":\"\"}}" \
    "(1) usr-c, a --user unit that passed, read from the user manager"
expect_eq "$(row 'User drill failed')" "{\"passed_at_ms\":null,\"last\":{\"at_ms\":$a_ms,\"ok\":false,\"reason\":\"Result=exit-code, ExecMainStatus=1\"}}" \
    "(1) usr-f, a --user unit that failed, read from the user manager"
note "(1) checked: user units"

# --- (2) a unit neither knows ------------------------------------------------------------------------
expect_eq "$(row Nowhere)" '{"passed_at_ms":null,"last":null}' "(2) none-x, known to neither manager: no pass, no last"
note "(2) checked: unknown unit"

# --- (3) a system unit stays the system manager's --------------------------------------------------
expect_eq "$(row 'System drill')" "{\"passed_at_ms\":$a_ms,\"last\":{\"at_ms\":$a_ms,\"ok\":true,\"reason\":\"\"}}" \
    "(3) sys-a, a system unit: the system manager's answer, not the user manager's"
expect_eq "$(row 'Legacy answer')" "{\"passed_at_ms\":$a_ms,\"last\":{\"at_ms\":$a_ms,\"ok\":true,\"reason\":\"\"}}" \
    "(3) legacy-l, an answer with no LoadState: the system manager's"
user_calls=$(grep '^--user ' "$WORK/systemctl.calls" 2>/dev/null | sed 's/.* //' | sort | tr '\n' ' ')
expect_eq "$user_calls" "none-x.service usr-c.service usr-f.service " "(3) the units asked of the user manager, once each"
# (3k, rev-k's R1 F1) a failed system unit, and a system call that fails, are not asked of the user manager
expect_eq "$(row "System drill failed")" "{\"passed_at_ms\":null,\"last\":{\"at_ms\":$a_ms,\"ok\":false,\"reason\":\"Result=exit-code, ExecMainStatus=1\"}}" \
    "(3k) sys-f, a system unit that failed (the user manager would say passed): the system manager's answer"
expect_eq "$(row "System manager error")" '{"passed_at_ms":null,"last":null}' \
    "(3k) sys-e, whose system call fails: no reading, never the user manager's"
note "(3) checked: system units"

# --- (4) the north star ----------------------------------------------------------------------------------
got=$(jq -c '.lines[] | select(.line == "sustainability") | .guardrails[] | select(.key == "recoverability.restores_passed") | .value' "$WORK/api.json")
expect_eq "$got" 4 "(4) restores passed in 7 days counts the user unit usr-c (sys-a, legacy-l, masked-m, usr-c)"
note "(4) checked: the north star"

# --- (5) the keep ------------------------------------------------------------------------------------------
before=$(grep -c . "$WORK/systemctl.calls" 2>/dev/null || true)
code=$(curl -s --noproxy '*' -o "$WORK/api2.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "the second GET /api/core-lines answered $code"
after=$(grep -c . "$WORK/systemctl.calls" 2>/dev/null || true)
expect_eq "$after" "$before" "(5) systemctl calls after a second reading within 60 s"
note "(5) checked: the keep"

# --- (6) only not-found falls back; the log says the user call -------------------------------------------
expect_eq "$(row Masked)" "{\"passed_at_ms\":$a_ms,\"last\":{\"at_ms\":$a_ms,\"ok\":true,\"reason\":\"\"}}" \
    "(6) masked-m, LoadState=masked: the system manager's answer, never the user manager's"
grep -q '^--user .* masked-m.service$' "$WORK/systemctl.calls" && check_failed "(6) masked-m was asked of the user manager"
sc=$SPAWN_SYSTEMCTL
grep -q "^core-lines: $sc show usr-c.service: not a system unit ([0-9]* ms); asking the user manager\$" "$WORK/serve.log" ||
    check_failed "(6) the serve log does not say usr-c is not a system unit: $(grep 'usr-c' "$WORK/serve.log" | head -n 3 | tr '\n' ' ')"
grep -q "^core-lines: $sc --user show usr-c.service: [0-9]* ms\$" "$WORK/serve.log" ||
    check_failed "(6) the serve log does not say the --user call for usr-c: $(grep 'usr-c' "$WORK/serve.log" | head -n 3 | tr '\n' ' ')"
grep -q "^core-lines: $sc show sys-a.service: [0-9]* ms\$" "$WORK/serve.log" ||
    check_failed "(6) the serve log does not say the system call for sys-a: $(grep 'sys-a' "$WORK/serve.log" | head -n 3 | tr '\n' ' ')"
grep -q -- "--user show sys-a.service" "$WORK/serve.log" && check_failed "(6) the serve log has a --user call for sys-a"
note "(6) checked: only not-found falls back; the log says each call"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "231: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "231: a drill unit only the user manager knows is read from it; a system unit stays the system manager's"
exit 0
