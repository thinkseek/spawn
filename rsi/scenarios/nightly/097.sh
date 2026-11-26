#!/bin/sh
# Black box: how long the disk lasts counts the session backups' growth,
# measured reflink-aware from their status file, and says so when it cannot.
#
# The problem (lead-mac, 2026-11-12 17:20, A4): "days until full" (085)
# sums the growth of the transcript archive, Claude Code's transcripts and
# the live database, but not /var/backups/spawn-sessions: every account's
# sessions copied every 15 minutes in versions. On btrfs a version is a
# reflink copy that shares its data, so the directory's apparent size says
# nothing about the disk. The backup's own status file (0644; the tree is
# root's) already says what the kept versions really cost: `estimated_bytes`,
# the sum of each version's `new_bytes`. The forecast must count that
# growth, or say that it does not.
#
# Required (through `spawn loop`'s sample, `GET /api/fleet`, and
# web/src/resources.ts under node):
#   (1) with SPAWN_SESSION_BACKUP_STATUS naming the backups' status file,
#       the loop's `host.storage` sample also records
#       `session_backup_bytes`: the file's `estimated_bytes`. A sample whose
#       file cannot be read leaves the field out.
#   (2) `health.storage.growth_parts` gains `session_backups`, measured as
#       the other parts are (this answer's estimated_bytes less the oldest
#       sample of 7 days that carries session_backup_bytes, over the days
#       between, rounded down; null without such a sample a day old), and
#       the persistent growth and days_left count it. `growth_excluded`
#       lists the parts left out of the sum because their source cannot be
#       read now: [] here.
#   (3) with no readable status file (unset, or a file that is not there),
#       the session backups' part is null, left out of the sum (the three
#       other parts still give the persistent growth and days_left) and
#       `growth_excluded` is ["session_backups"].
#   (4) storageView: `lasts` reads "about <days> days at the growth of the
#       archive, Claude Code's transcripts, the database and the session
#       backups" when they are counted, and 085's words when not; then
#       `lasts_note` is "session backups are not counted" (null otherwise).
#
# The world: 085's (archive 12 MiB, projects 1 MiB, workspaces 2 MiB;
# samples 3 days ago in the old shape and 2 days ago with the database's
# size), the 2-day-old sample also carrying session_backup_bytes 36 MiB, and
# a status file whose estimated_bytes is 40 MiB.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "097: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
SPAWN_HC=${SPAWN_HERDR_CORE_PROGRAM-}
if [ -z "$SPAWN_HC" ]; then
    spawn_dir=$(dirname "$SPAWN_BIN")
    if [ -x "$spawn_dir/spawn-hc" ]; then
        SPAWN_HC=$spawn_dir/spawn-hc
    elif [ -x "$(dirname "$spawn_dir")/spawn-hc" ]; then
        SPAWN_HC=$(dirname "$spawn_dir")/spawn-hc
    elif command -v spawn-hc >/dev/null 2>&1; then
        SPAWN_HC=$(command -v spawn-hc)
    fi
fi
[ -n "$SPAWN_HC" ] && [ -x "$SPAWN_HC" ] ||
    harness "spawn-hc is not installed (set SPAWN_HERDR_CORE_PROGRAM or build sibling spawn-hc beside SPAWN_BIN)"

# The candidate's tree: the gate runs a scenario from inside it, and its
# web/src is what the page would run (the controller holds main's copy).
tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
[ -f "$tree/web/src/home.ts" ] || harness "no web/src/home.ts under $tree (run from the candidate's tree)"
for tool in psql git jq od bash sed grep awk curl node df head; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/r
XDG_STATE_HOME=$WORK/state
XDG_CONFIG_HOME=$WORK/config
mkdir -m 700 "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
export XDG_RUNTIME_DIR XDG_STATE_HOME XDG_CONFIG_HOME
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
hc_id=$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')
SPAWN_DB_NAME="bb_097_growth_$run"
SPAWN_HERDR_SESSION="bb-097-$hc_id"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_HERDR_SESSION SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
serve_pid=""
hc() {
    env -u HERDR_SOCKET_PATH -u HERDR_SESSION -u HERDR_PANE_ID \
        HC_SESSION="$SPAWN_HERDR_SESSION" \
        HC_SOCKET="$XDG_RUNTIME_DIR/herdr-core/$SPAWN_HERDR_SESSION.sock" \
        "$SPAWN_HC" "$@"
}
stop_herdr() {
    [ -n "${SPAWN_HERDR_SESSION-}" ] && [ -S "${XDG_RUNTIME_DIR-}/herdr-core/$SPAWN_HERDR_SESSION.sock" ] || return 0
    for addr in $(hc ls --json --label "spawn_owner=$SPAWN_HERDR_SESSION" 2>/dev/null |
        jq -r '.terminals[]? | select(.state == "live") | .address // empty' 2>/dev/null || true); do
        hc kill "$addr" >/dev/null 2>&1 || true
    done
    hc server stop >/dev/null 2>&1 || true
    hc_wait=0
    while [ "$hc_wait" -lt 20 ] && [ -S "${XDG_RUNTIME_DIR-}/herdr-core/$SPAWN_HERDR_SESSION.sock" ]; do
        hc_wait=$((hc_wait + 1))
        sleep 0.1
    done
}
cleanup() {
    for pid in "$loop_pid" "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    stop_herdr
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
            echo "097: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "097: the host.storage records (recorded, record):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select recorded_at_ms, kind -> 'detail' -> 'record' from spawn_events
                where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage' order by id" >&2 2>/dev/null || true
        fi
        if [ -s "$WORK/api.json" ]; then
            echo "097: the answer's storage:" >&2
            jq -c '.health.storage' "$WORK/api.json" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "097: harness: unexpected exit $rc" >&2
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
[ -n "${BB_PATH-}" ] && PATH=$BB_PATH && export PATH
printf 'Welcome to Claude Code\n\n❯ '
while :; do
  if bb_read_line typed; then printf 'you: %s\n\nidle\n❯ ' "$typed"; fi
done
ENGINE
chmod 755 "$WORK/claude"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY \
    SPAWN_SESSION_BACKUP_STATUS SPAWN_TERMINAL_BACKEND SPAWN_HERDR_MODE \
    SPAWN_HERDR_SHARDS SPAWN_HERDR_PROGRAM SPAWN_TMUX_SOCKET SPAWN_TMUX_PROGRAM TMUX \
    TMUX_PANE TMUX_TMPDIR; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PATH=$PATH
SPAWN_HERDR_CORE_PROGRAM=$SPAWN_HC
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
    SPAWN_CLAUDE_PROJECTS BB_PATH SPAWN_HERDR_CORE_PROGRAM SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
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
note() { echo "097: $*" >&2; }
wait_until() { # <seconds> <command...>
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
# The log is emptied here, by this shell, before the launch: with `>log &`
# the truncation ran in the child, after the fork, so at (3)'s restart this
# shell could read the stopped serve's port line first and send the request
# to a dead port (task 303's base run, "answered 000" over (2)'s body, which
# curl had left in place). As 046's start_serve.
start_serve() {
    : >"$WORK/serve.log"
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$WORK/web" >>"$WORK/serve.log" 2>&1 &
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

# --- the disk: archive, Claude Code's projects, workspaces ------------------------------

MIB=1048576
root=$SPAWN_WORKSPACE_ROOT
projects=$SPAWN_CLAUDE_PROJECTS
mkdir -p "$root/.transcripts/agent_a" "$root/.transcripts/agent_b" "$root/agent_c" "$projects/-bb-one" "$projects/-bb-two"
blob() { head -c "$2" /dev/zero >"$1"; } # <path> <bytes>
blob "$root/.transcripts/agent_a/a1.jsonl" $((3 * MIB))
blob "$root/.transcripts/agent_a/a2.jsonl" $((4 * MIB))
blob "$root/.transcripts/agent_b/b1.jsonl" $((5 * MIB))
blob "$root/agent_c/big.bin" $((2 * MIB))
blob "$projects/-bb-one/s1.jsonl" $((MIB / 2))
blob "$projects/-bb-one/s2.jsonl" $((MIB / 4))
blob "$projects/-bb-two/s3.jsonl" $((MIB / 4))

# Two samples from before, as the loop records them (control records): 3
# days ago in the old shape, 2 days ago with the database's size.
db_seed=$(q "select pg_database_size(current_database())")
case "$db_seed" in '' | *[!0-9]*) harness "no database size: [$db_seed]" ;; esac
db_then=$((db_seed - 8 * MIB))
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
create temp sequence seed_seq start 100000;
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
select 'agent_control', 'session_control', null, 'control', null,
       jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
       nextval('seed_seq'), now_ms - ago_ms
  from (select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint as now_ms) n,
       (values
    (259200000::bigint, jsonb_build_object('type', 'host.storage', 'archive_bytes', 9 * $MIB, 'archive_files', 2,
                                           'projects_bytes', $MIB / 4, 'projects_files', 1)),
    (172800000::bigint, jsonb_build_object('type', 'host.storage', 'archive_bytes', 10 * $MIB, 'archive_files', 2,
                                           'projects_bytes', $MIB / 2, 'projects_files', 2,
                                           'database_bytes', $db_then,
                                           'session_backup_bytes', 36 * $MIB))) v(ago_ms, r)
 order by ago_ms desc;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the samples failed: $(err)"
at3=$(q "select min(recorded_at_ms) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage'")
at2=$(q "select max(recorded_at_ms) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage'")
seeded_max=$(q "select max(id) from spawn_events")

# The session backups' status file, as deploy/session-backup.sh writes it.
mkdir -p "$WORK/backups"
printf '{"at":%s,"accounts":[],"estimated_bytes":%s,"free_bytes":1}\n' "$(now)" "$((40 * MIB))" >"$WORK/backups/status.json"
SPAWN_SESSION_BACKUP_STATUS=$WORK/backups/status.json
export SPAWN_SESSION_BACKUP_STATUS

# --- (1) the loop's sample ---------------------------------------------------------------

loop_db_before=$(q "select pg_database_size(current_database())")
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
sampled() {
    [ -n "$(q "select id from spawn_events where id > $seeded_max
                 and kind -> 'detail' -> 'record' ->> 'type' = 'host.storage' limit 1")" ]
}
if wait_until 60 sampled; then
    loop_db_after=$(q "select pg_database_size(current_database())")
    sample=$(q "select (kind -> 'detail' -> 'record') - 'type' from spawn_events where id > $seeded_max
                  and kind -> 'detail' -> 'record' ->> 'type' = 'host.storage' order by id limit 1")
    expect_eq "$(printf '%s' "$sample" | jq -cS '{archive_bytes, projects_bytes}')" \
        "{\"archive_bytes\":$((12 * MIB)),\"projects_bytes\":$MIB}" "(1) the loop's sample: the archive and the projects, as before"
    expect_eq "$(printf '%s' "$sample" | jq -c '.session_backup_bytes')" "$((40 * MIB))" \
        "(1) the loop's sample: session_backup_bytes, the status file's estimated_bytes"
    sample_db=$(printf '%s' "$sample" | jq -r '.database_bytes')
    awk -v g="$sample_db" -v lo="$((loop_db_before - loop_db_before / 100))" -v hi="$((loop_db_after + loop_db_after / 100))" \
        'BEGIN { exit !(g ~ /^[0-9]+$/ && g >= lo && g <= hi) }' ||
        check_failed "(1) the loop's sample: database_bytes [$sample_db], against pg_database_size before [$loop_db_before] and after [$loop_db_after] it (1% slack each side)"
else
    check_failed "(1) the loop recorded no host.storage sample within 60 s of its start"
fi
note "(1) checked: the loop's sample"

# --- (2) the answer ------------------------------------------------------------------------

start_serve
code=$(bb_http "$WORK/api.json" --noproxy '*' "$base/api/fleet")
[ "$code" = 200 ] || harness "GET /api/fleet answered $code$(bb_http_why "$WORK/api.json"): $(head -c 300 "$WORK/api.json")"
storage() { jq -c ".health.storage | $1" "$WORK/api.json"; }
read_at=$(storage '.read_at_ms')
db_now=$(storage '.database.bytes')
for value in "$read_at" "$db_now"; do
    case "$value" in '' | *[!0-9]*) harness "the answer has no read_at_ms or database bytes: [$read_at] [$db_now]" ;; esac
done
per_day() { # <bytes> <since_ms>: bytes a day over read_at - since, rounded down
    awk -v b="$1" -v ms="$((read_at - $2))" 'BEGIN { printf "%d", b * 86400000 / ms }'
}
want_archive=$(per_day $((3 * MIB)) "$at3")
want_projects=$(per_day $((MIB - MIB / 4)) "$at3")
want_database=$(per_day $((db_now - db_then)) "$at2")
archive=$(storage '.growth_parts.archive')
near "$(storage '.growth_bytes_per_day')" "$want_archive" "$((want_archive / 100))" ||
    check_failed "(2) growth_bytes_per_day [$(storage '.growth_bytes_per_day')]: the archive's alone, 3 MiB in 3 days ([$want_archive])"
expect_eq "$archive" "$(storage '.growth_bytes_per_day')" "(2) growth_parts.archive: the same as growth_bytes_per_day"
projects=$(storage '.growth_parts.projects')
near "$projects" "$want_projects" "$((want_projects / 100 + 1))" ||
    check_failed "(2) growth_parts.projects [$projects]: 0.75 MiB in 3 days, from the old-shape sample ([$want_projects])"
database=$(storage '.growth_parts.database')
near "$database" "$want_database" "$((want_database / 100 + 1024))" ||
    check_failed "(2) growth_parts.database [$database]: this answer's database bytes less the 2-day-old sample's, over 2 days ([$want_database]); the 3-day-old sample has no database_bytes"
want_backups=$(per_day $((4 * MIB)) "$at2")
backups=$(storage '.growth_parts.session_backups')
near "$backups" "$want_backups" "$((want_backups / 100 + 1))" ||
    check_failed "(2) growth_parts.session_backups [$backups]: 4 MiB of estimated_bytes in 2 days, from the 2-day-old sample ([$want_backups])"
expect_eq "$(storage '.growth_excluded')" '[]' "(2) growth_excluded: nothing left out"
persistent=$(storage '.persistent_growth_bytes_per_day')
if [ -n "$archive" ] && [ "$archive" != null ] && [ "$projects" != null ] && [ "$database" != null ] && [ -n "$projects" ] && [ -n "$database" ] && [ -n "$backups" ] && [ "$backups" != null ]; then
    expect_eq "$persistent" "$((archive + projects + database + backups))" "(2) persistent_growth_bytes_per_day: the four parts' sum, the session backups' among them"
fi
free=$(storage '.free_bytes')
if [ "$free" != null ] && [ "$persistent" != null ] && [ "$persistent" -gt 0 ] 2>/dev/null; then
    expect_eq "$(storage '.days_left')" "$((free / persistent))" "(2) days_left: free_bytes over the persistent growth, rounded down"
else
    check_failed "(2) no persistent growth above 0 to check days_left with: [$persistent]"
fi
note "(2) checked: the answer"

# --- (3) no readable status file: left out, and said ---------------------------------------

kill "$serve_pid" 2>/dev/null || true
wait "$serve_pid" 2>/dev/null || true
serve_pid=""
# (2)'s answer is moved away, not copied: curl leaves the file as it was
# when no answer comes, and a stale body must not read as this one.
mv "$WORK/api.json" "$WORK/counted.json"
SPAWN_SESSION_BACKUP_STATUS=$WORK/backups/none.json start_serve
code=$(bb_http "$WORK/api.json" --noproxy '*' "$base/api/fleet")
[ "$code" = 200 ] || harness "GET /api/fleet (no status file) answered $code$(bb_http_why "$WORK/api.json"): $(head -c 300 "$WORK/api.json" 2>/dev/null)"
cp "$WORK/api.json" "$WORK/excluded.json"
expect_eq "$(storage '{backups: .growth_parts.session_backups, excluded: .growth_excluded}')" \
    '{"backups":null,"excluded":["session_backups"]}' "(3) no status file: the session backups' part is null and listed as left out"
a=$(storage '.growth_parts.archive'); pr=$(storage '.growth_parts.projects'); db=$(storage '.growth_parts.database')
p3=$(storage '.persistent_growth_bytes_per_day')
if [ "$a" != null ] && [ "$pr" != null ] && [ "$db" != null ]; then
    expect_eq "$p3" "$((a + pr + db))" "(3) no status file: the persistent growth is the other three parts' sum, not null"
    free3=$(storage '.free_bytes')
    [ "$p3" -gt 0 ] 2>/dev/null && expect_eq "$(storage '.days_left')" "$((free3 / p3))" "(3) no status file: days_left over the three parts"
else
    check_failed "(3) no status file: a part of the other three is null: [$a] [$pr] [$db]"
fi
note "(3) checked: the answer without the status file"

# --- (4) the card's words --------------------------------------------------------------------

cat >"$WORK/storage.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { storageView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/resources.ts`).href);
const read = (name) => JSON.parse(readFileSync(`${env.BB_WORK}/${name}.json`, "utf8")).health.storage;

test("(4) how long the disk lasts, the session backups counted or said not to be", () => {
  const counted = read("counted");
  const view = storageView(counted);
  assert.equal(view.lasts,
    `about ${counted.days_left.toLocaleString("en-US")} days at the growth of the archive, Claude Code's transcripts, the database and the session backups`);
  assert.equal(view.lasts_note, null);
  const excluded = read("excluded");
  const without = storageView(excluded);
  assert.equal(without.lasts,
    `about ${excluded.days_left.toLocaleString("en-US")} days at the growth of the archive, Claude Code's transcripts and the database`);
  assert.equal(without.lasts_note, "session backups are not counted");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/storage.test.mjs" >"$WORK/storage.out" 2>&1; then
    check_failed "(4) web/src/resources.ts storageView: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/storage.out" | head -n 10 | tr '\n' ' ' | head -c 900)"
fi
note "(4) checked: the card's words"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "097: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "097: how long the disk lasts counts the session backups' reflink-aware growth, and says so when it cannot"
exit 0
