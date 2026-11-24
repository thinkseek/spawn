#!/bin/sh
# Objective 166 [observability] (after 151): the notice fold where the CLI
# reads it. 151 folded notices in /api/alerts only, so `spawn alerts --once
# --json` no longer answered the same array (no `superseded_by`), and
# `spawn alerts --once` in text printed every state of every task. The fold
# now lives in the window both read; the text shows each subject's newest
# notice with "· N earlier", as the Alerts view does.
#
# Black box, through `spawn alerts --once [--json]` and `spawn serve`'s GET
# /api/alerts. 151's world: notices and a need fired through
# spawn_alert_record, the writer the loop's alert step uses: task #7 moves
# through 4 states (events 10 to 13, fired out of order), task #8 has 1,
# mission #4 has 2; one need (memory_low:1) is open.
#   (1) `spawn alerts --once --json` holds all 8 rows, each notice's
#       superseded_by its subject's next one (task #7's 10, 11, 12 say 11,
#       12, 13; mission #4's 20 says 21; the rest none), and is the array
#       GET /api/alerts answers, row for row.
#   (2) `spawn alerts --once` prints 4 lines, in the answer's order: the
#       open need as before; task #7's 13 ending "task #7 landed · 3
#       earlier"; task #8's ending "task #8 assigned"; mission #4's 21
#       ending "mission #4 halted · 1 earlier"; no superseded notice's
#       line.
#   (3) the store holds the same 8 rows, 7 of them notices: nothing
#       recorded or removed by the reads.
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure.

set -eu

asserted=""
harness() {
    echo "166: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep awk curl df head; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_166_cli_fold_$run"
SPAWN_TMUX_SOCKET="bb-166-$run"
# The tmux server's socket lives in WORK, so the cleanup removes nothing
# outside it (cc-rules (a), 2026-11-12).
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$TMUX_TMPDIR"
export TMUX_TMPDIR
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
serve_pid=""
cleanup() {
    for pid in "$loop_pid" "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "166: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "166: the alerts (key, state):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, state from spawn_alerts order by key" >&2 2>/dev/null || true
        fi
        if [ -s "$WORK/alerts.json" ]; then
            echo "166: the answer's rows (key, state, superseded_by):" >&2
            jq -c '.[] | [.key, .state, .superseded_by]' "$WORK/alerts.json" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "166: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# --- the world ----------------------------------------------------------------

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
repo=$WORK/repo.git
git init -q --bare --initial-branch=main "$repo"
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git -C "$WORK/seed" push -q "$repo" main:main

# A quiet engine: the loop may deliver the tasks below to a worker; this
# one shows a prompt, takes what is typed and does nothing with it.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
printf 'Welcome to Claude Code\n\n❯ '
while :; do
  if bb_read_line typed; then printf 'you: %s\n\nidle\n❯ ' "$typed"; fi
done
ENGINE
chmod 755 "$WORK/claude"
cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_ALERT_CHECK_SECS=2
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
SPAWN_MISSION_FILE=$WORK/mission.md
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
    SPAWN_ALERT_CHECK_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_MISSION_FILE \
    SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/ctl/scenarios" "$WORK/web"
: >"$SPAWN_MISSION_FILE"
printf '<!doctype html><title>spawn</title>\n' >"$WORK/web/index.html"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- helpers ----------------------------------------------------------------

now() { date +%s; }
q() { scalar "$1"; }
note() { echo "166: $*" >&2; }
wait_until() { # <seconds> <command...>
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
start_serve() {
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$WORK/web" >"$WORK/serve.log" 2>&1 &
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
    base="http://127.0.0.1:$port"
}
near() { # <got> <want> <slack>: |got - want| <= slack
    [ -n "$1" ] && [ "$1" != null ] && awk -v g="$1" -v w="$2" -v s="$3" 'BEGIN { d = g - w; if (d < 0) d = -d; exit !(d <= s) }'
}

# A scratch HOME: nothing here reads or writes the real one.
real_home=$HOME
HOME=$WORK/home
export HOME
mkdir -p "$HOME"
[ "$HOME" != "$real_home" ] && [ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
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

# --- the notices and a need ----------------------------------------------------------------

record() { # <json>: through the alert step's own writer
    capture psql -d "$SPAWN_DB_NAME" -v ON_ERROR_STOP=1 -tA -c "select spawn_alert_record('$1'::jsonb)"
    [ "$status" -eq 0 ] || harness "spawn_alert_record refused $1: $(err)"
}
notice() { # <kind> <subject id> <event> <word>
    if [ "$1" = task_state ]; then subject="task #$2"; else subject="mission #$2"; fi
    record "{\"type\": \"alert.fired\", \"key\": \"$1:$2:$3\", \"kind\": \"$1\", \"subject\": \"$subject\", \"evidence\": {\"event_id\": $3}, \"text\": \"$subject $4\", \"notice\": true}"
}
# Out of order: the fold reads the event in the key, not the firing order.
notice task_state 7 10 assigned
notice task_state 7 12 reported
notice task_state 7 11 delivered
notice task_state 8 15 assigned
notice mission_state 4 20 active
notice task_state 7 13 landed
notice mission_state 4 21 halted
record '{"type": "alert.fired", "key": "memory_low:1", "kind": "memory_low", "subject": "host", "evidence": {"measured_mb": 1000, "threshold_mb": 4096}, "text": "MemAvailable is 1000 MiB, under 4096", "notice": false}'
rows_before=$(q "select count(*) || ' ' || count(*) filter (where state = 'notice') from spawn_alerts")
[ "$rows_before" = "8 7" ] || harness "the store holds [$rows_before] rows and notices, not [8 7]"
start_serve

# --- (1) the CLI's array is the page's --------------------------------------------------------

code=$(curl -s --noproxy '*' -o "$WORK/alerts.json" -w '%{http_code}' "$base/api/alerts" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/alerts answered $code: $(head -c 200 "$WORK/alerts.json")"
capture "$SPAWN_BIN" alerts --once --json
[ "$status" -eq 0 ] || harness "spawn alerts --once --json exited $status: $(err)"
cp "$WORK/out.txt" "$WORK/once.json"
jq -e 'type == "array"' "$WORK/once.json" >/dev/null 2>&1 || harness "spawn alerts --once --json printed no JSON array: $(head -c 200 "$WORK/once.json")"
got=$(jq -c '[.[] | {key, state, superseded_by}] | sort_by(.key)' "$WORK/once.json")
want='[{"key":"memory_low:1","state":"open","superseded_by":null},{"key":"mission_state:4:20","state":"notice","superseded_by":"mission_state:4:21"},{"key":"mission_state:4:21","state":"notice","superseded_by":null},{"key":"task_state:7:10","state":"notice","superseded_by":"task_state:7:11"},{"key":"task_state:7:11","state":"notice","superseded_by":"task_state:7:12"},{"key":"task_state:7:12","state":"notice","superseded_by":"task_state:7:13"},{"key":"task_state:7:13","state":"notice","superseded_by":null},{"key":"task_state:8:15","state":"notice","superseded_by":null}]'
expect_eq "$got" "$want" "(1) spawn alerts --once --json: every row kept, each notice superseded by its subject's next one"
expect_eq "$(jq -cS . "$WORK/once.json")" "$(jq -cS . "$WORK/alerts.json")" \
    "(1) spawn alerts --once --json against GET /api/alerts"
note "(1) checked: the CLI's array"

# --- (2) the text ------------------------------------------------------------------------------

capture "$SPAWN_BIN" alerts --once
[ "$status" -eq 0 ] || harness "spawn alerts --once exited $status: $(err)"
cp "$WORK/out.txt" "$WORK/once.txt"
# Each line with its time as <time>, in the answer's order (by firing
# time, then key): the rows no later notice supersedes.
got=$(sed -E 's/ [0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2} / <time> /' "$WORK/once.txt")
want=""
for key in $(jq -r '.[] | select(.superseded_by == null) | .key' "$WORK/alerts.json"); do
    case $key in
        memory_low:1) line="open memory_low host <time> MemAvailable is 1000 MiB, under 4096" ;;
        task_state:7:13) line="notice task_state task #7 <time> task #7 landed · 3 earlier" ;;
        task_state:8:15) line="notice task_state task #8 <time> task #8 assigned" ;;
        mission_state:4:21) line="notice mission_state mission #4 <time> mission #4 halted · 1 earlier" ;;
        *) line="unexpected $key" ;;
    esac
    want="$want${want:+
}$line"
done
expect_eq "$got" "$want" "(2) spawn alerts --once: each subject's newest notice with how many came before"
for gone in "task #7 assigned" "task #7 delivered" "task #7 reported" "mission #4 active"; do
    if grep -qF "$gone" "$WORK/once.txt"; then
        check_failed "(2) spawn alerts --once printed a superseded notice: $(grep -F "$gone" "$WORK/once.txt")"
    fi
done
note "(2) checked: the text"

# --- (3) the store -------------------------------------------------------------------------

expect_eq "$(q "select count(*) || ' ' || count(*) filter (where state = 'notice') from spawn_alerts")" "8 7" \
    "(3) the store's rows and notices after the reads"
note "(3) checked: the store"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "166: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "166: spawn alerts --once answers the page's folded array, and its text each subject's newest notice"
exit 0
