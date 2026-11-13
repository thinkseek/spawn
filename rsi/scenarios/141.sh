#!/bin/sh
# Objective 141 [storage]: the storage line's north star, "Days until the
# disk is full", from the disk itself. Before, it was the free disk over the
# growth of the stores that persist (archive, Claude Code's transcripts, the
# database, session backups: 0.58 GB a day on live, "136 days"), while the
# free disk itself fell from 129 GB to 79 GB in a day (11-12 to 11-13, its
# own trend): builds and scratch are no store, so the figure said months
# where the disk had days.
#
# Black box, through `spawn serve`'s GET /api/core-lines and
# web/src/coreLines.ts under node. The readings are the hourly
# `host.core_lines` samples' storage.free_bytes (objective 090), seeded as
# the loop records them; the free disk now is the answer's own.
#   (1) no readings but now: days_left has no value and says when it is
#       first measured (measuring_since_ms, a day after the reading now);
#       storage.disk_fall and storage.grew_most have no value.
#   (2) readings 6, 3 and 1 days ago on a line falling 10 GiB a day to the
#       free disk now: storage.disk_fall is 10 GiB a day (within 1%),
#       days_left is the free disk over it, rounded down, and
#       storage.grew_most names "unattributed" (no store measured: all of
#       the fall) at the same bytes a day.
#   (3) the card names the store beside the figure: "Store that grew most:
#       unattributed".
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure.

set -eu

asserted=""
harness() {
    echo "141: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

# The candidate's tree: the gate runs a scenario from inside it, and its
# web/src is what the page would run (the controller holds main's copy).
tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
[ -f "$tree/web/src/home.ts" ] || harness "no web/src/home.ts under $tree (run from the candidate's tree)"
for tool in psql tmux git jq od bash sed grep awk curl node df head; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_141_disk_$run"
SPAWN_TMUX_SOCKET="bb-141-$run"
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
            echo "141: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "141: the host.core_lines samples (recorded, values):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select recorded_at_ms, kind -> 'detail' -> 'record' -> 'values' from spawn_events
                where kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines' order by id" >&2 2>/dev/null || true
        fi
        if [ -s "$WORK/api.json" ]; then
            echo "141: the answer's storage line:" >&2
            jq -c '.lines[] | select(.line == "storage")' "$WORK/api.json" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "141: harness: unexpected exit $rc" >&2
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
note() { echo "141: $*" >&2; }
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

# --- (1) no readings but now --------------------------------------------------------------

GIB=1073741824
D=86400000
H=3600000
get_answer() { # <file>
    code=$(curl -s --noproxy '*' -o "$1" -w '%{http_code}' "$base/api/core-lines" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/core-lines answered $code: $(head -c 200 "$1")"
}
fig() { # <file> <key>: the figure, as JSON
    jq -c --arg k "$2" '[.lines[] | ([.north_star] + .guardrails)[] | select(.key == $k)][0]' "$1"
}
start_serve
before_ms=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
get_answer "$WORK/api1.json"
after_ms=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
days=$(fig "$WORK/api1.json" storage.days_left)
since=$(printf '%s' "$days" | jq -r '.measuring_since_ms // "none"')
expect_eq "$(printf '%s' "$days" | jq -c '.value')" null "(1) with no reading but now, days_left's value"
if [ "$since" = none ] || [ "$since" -lt $((before_ms + D)) ] || [ "$since" -gt $((after_ms + D)) ]; then
    check_failed "(1) with no reading but now, days_left says when it is first measured, a day after now ([$((before_ms + D)), $((after_ms + D))]): got $since ($days)"
fi
expect_eq "$(fig "$WORK/api1.json" storage.disk_fall | jq -c '.value')" null "(1) storage.disk_fall's value with no reading but now"
expect_eq "$(fig "$WORK/api1.json" storage.grew_most | jq -c '.value')" null "(1) storage.grew_most's value with no reading but now"
note "(1) checked: no readings but now"

# --- (2) a week of readings falling 10 GiB a day --------------------------------------------

free=$(fig "$WORK/api1.json" storage.free_bytes | jq -r '.value')
case "$free" in '' | *[!0-9]*) harness "no free disk in the answer: [$free]" ;; esac
now_ms=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
midnight=$((now_ms - now_ms % D))
# On the line: free now plus 10 GiB for each day before now.
at_line() { awk -v f="$free" -v g="$GIB" -v n="$now_ms" -v a="$1" -v d="$D" 'BEGIN { printf "%.0f", f + 10 * g * (n - a) / d }'; }
a6=$((midnight - 6 * D + 2 * H))
a3=$((midnight - 3 * D + 2 * H))
a1=$((midnight - D + 2 * H))
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
create temp sequence seed_seq start 100000;
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
select 'agent_control', 'session_control', null, 'control', null,
       jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record',
           jsonb_build_object('type', 'host.core_lines', 'values', jsonb_build_object('storage.free_bytes', v)))),
       nextval('seed_seq'), at_ms
  from (values ($a6::bigint, $(at_line $a6)::bigint),
               ($a3::bigint, $(at_line $a3)::bigint),
               ($a1::bigint, $(at_line $a1)::bigint)) s(at_ms, v)
 order by at_ms;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the samples failed: $(err)"
get_answer "$WORK/api.json"
fall=$(fig "$WORK/api.json" storage.disk_fall | jq -r '.value')
free2=$(fig "$WORK/api.json" storage.free_bytes | jq -r '.value')
days=$(fig "$WORK/api.json" storage.days_left | jq -r '.value')
if ! near "$fall" $((10 * GIB)) $((GIB / 10)); then
    check_failed "(2) storage.disk_fall on readings falling 10 GiB a day: expected $((10 * GIB)) within 1%, got $fall"
fi
want_days=$(awk -v f="$free2" -v l="$fall" 'BEGIN { if (l + 0 > 0) printf "%d", f / l; else print "null" }')
expect_eq "$days" "$want_days" "(2) days_left: the free disk ($free2) over the fall ($fall), rounded down"
expect_eq "$(fig "$WORK/api.json" storage.days_left | jq -c '.measuring_since_ms // null')" null "(2) a measured days_left says no measuring start"
grew=$(fig "$WORK/api.json" storage.grew_most)
expect_eq "$(printf '%s' "$grew" | jq -c --argjson f "${fall:-0}" '{store, same: (.value == $f), unit}')" \
    '{"store":"unattributed","same":true,"unit":"bytes_per_day"}' \
    "(2) storage.grew_most with no store measured: unattributed, all of the fall"
note "(2) checked: free $free2, fall $fall a day, $days days"

# --- (3) the card's words -------------------------------------------------------------------

cat >"$WORK/card.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { coreLinesView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/coreLines.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));

test("(3) the store that grew most is named on the card", () => {
  const storage = coreLinesView(answer).rows.find((row) => row.line.toLowerCase().includes("storage"));
  assert.ok(storage, "a storage row");
  const names = storage.guardrails.map((mark) => mark.name);
  assert.ok(names.includes("Store that grew most: unattributed"), JSON.stringify(names));
  assert.ok(names.includes("Free disk lost a day"), JSON.stringify(names));
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/card.test.mjs" >"$WORK/card.out" 2>&1; then
    check_failed "(3) web/src/coreLines.ts: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/card.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
fi
note "(3) checked: the card's words"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "141: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "141: days until the disk is full come from the disk's own fall, and the store that grew most is named"
exit 0
