#!/bin/sh
# Objective 180 [storage]: the real bytes on disk of each storage source,
# read daily outside the owner's holds (the reading the storage north star,
# "Bytes a landing leaves behind", and S1, "Rebuildable bytes on disk", are
# measured from: objective 185).
#
# The problem (the owner's core metric set, 2026-11-14 18:42; objective 176
# shows both as "not measured yet"): the storage line steers by what each
# landing leaves on the disk, and by how much of the disk is rebuildable
# (scratch, build cache, test databases). Apparent sizes say nothing on
# btrfs, where a reflinked copy shares its data: the lead's storage study
# counts each physical extent once. So the loop reads, once a day and only
# outside every owner hold, the real bytes of each source, each extent
# counted once across the sources (in the order transcripts, build cache,
# scratch), and records them.
#
# Required (through `spawn loop`'s records and its log):
#   (1) while the quiet-hold file the Core lines file names
#       (storage.quiet_hold_file) does not say ENDED, the loop takes no
#       reading, and says so in its log ("storage bytes: not measured
#       (owner gate)").
#   (2) with it ENDED and no reading a day old (the newest, seeded, is two
#       days old), the loop records one
#       `host.storage_bytes` reading: scratch counts a file and its plain
#       copy (8 MiB), never its reflinked copy, nor the excluded directory,
#       nor a reflink of a build cache file (counted in the build cache, 1
#       MiB); transcripts are the archive and Claude Code's projects (512
#       KiB).
#       The test databases: none configured, 0; the session backups: no
#       status file, not measured.
#   (3) a loop started again within the day takes no second reading.
#
# Needs a file system with reflinks (btrfs, xfs) under TMPDIR; cannot_judge
# (6) otherwise.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure; 6 cannot_judge
# (host lacks reflinks under TMPDIR). Any unexpected exit is reported as 2.
#
# Fails with 1 on the base (176, 01ee71ef): "(1) the loop did not say,
# within 60 s, that a hold put the reading off"; "(2) the loop recorded no
# host.storage_bytes reading within 120 s of its start".

set -eu

asserted=""
harness() {
    echo "180: harness: $*" >&2
    exit 2
}
cannot_judge() {
    echo "180: cannot_judge: $*" >&2
    exit 6
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
# scripts/storage-extents.py is what the loop would run.
tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od sed grep head cp sync python3 nice; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
extents=$tree/scripts/storage-extents.py
[ -f "$extents" ] || extents=$controller/scripts/storage-extents.py

WORK=$(mktemp -d) || harness "mktemp failed"
[ -d "$WORK" ] || harness "mktemp gave no directory"
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME, and isolated XDG dirs, in WORK: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
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
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
hc_id=$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')
SPAWN_DB_NAME="bb_180_storage_bytes_$run"
SPAWN_HERDR_SESSION="bb-180-$hc_id"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_HERDR_SESSION SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
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
stop_loop() {
    [ -n "$loop_pid" ] || return 0
    kill "$loop_pid" 2>/dev/null || true
    wait "$loop_pid" 2>/dev/null || true
    loop_pid=""
}
cleanup() {
    stop_loop
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
    if [ "$rc" -ne 0 ] && [ "$rc" -ne 6 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "180: the loop's last storage lines:" >&2
            grep -i "storage" "$WORK/loop.log" | tail -n 8 >&2 || true
        fi
        if [ -n "$database_made" ]; then
            echo "180: the host.storage_bytes records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select recorded_at_ms, kind -> 'detail' -> 'record' -> 'sources' from spawn_events
                where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage_bytes' order by id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 6 ]; then exit 6; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "180: harness: unexpected exit $rc" >&2
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

bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
printf 'Welcome to Claude Code\n\n❯ '
while :; do
  if bb_read_line typed; then printf 'you: %s\n\nidle\n❯ ' "$typed"; fi
done
ENGINE
chmod 755 "$WORK/claude"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN SPAWN_SESSION_BACKUP_STATUS SPAWN_FLEET_TEST_DATABASE_URL \
    SPAWN_STORAGE_EXTENTS SPAWN_QUIET_HOLD_FILE XDG_CACHE_HOME \
    SPAWN_TERMINAL_BACKEND SPAWN_HERDR_MODE SPAWN_HERDR_SHARDS SPAWN_HERDR_PROGRAM \
    SPAWN_TMUX_SOCKET SPAWN_TMUX_PROGRAM TMUX TMUX_PANE TMUX_TMPDIR \
    HERDR_SOCKET_PATH HERDR_SESSION HERDR_PANE_ID HC_SOCKET HC_SESSION \
    http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done

SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_HERDR_CORE_PROGRAM=$SPAWN_HC
SPAWN_WORKER=worker-bb
SPAWN_DISPATCH=0
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_MISSION_FILE=$WORK/mission.md
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
SPAWN_CORE_LINES_FILE=$WORK/core-lines.json
quiet=$WORK/owner-gate.state
SPAWN_STORAGE_EXTENTS=$extents
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS SPAWN_HERDR_CORE_PROGRAM SPAWN_WORKER SPAWN_DISPATCH SPAWN_LOOP_INTERVAL_MS \
    SPAWN_MISSION_FILE SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_CORE_LINES_FILE \
    SPAWN_STORAGE_EXTENTS
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$SPAWN_MISSION_FILE"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

now() { date +%s; }
q() { scalar "$1"; }
note() { echo "180: $*" >&2; }
wait_until() { # <seconds> <command...>
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
start_loop() {
    env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!
}
readings() { q "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage_bytes'"; }

# --- the disk -------------------------------------------------------------------

KIB=1024
MIB=1048576
scratch=$WORK/scratch
cache=$HOME/.cache
mkdir -p "$scratch/pg" "$cache" "$SPAWN_WORKSPACE_ROOT/.transcripts/agent_a" "$SPAWN_CLAUDE_PROJECTS/-bb"
blob() { head -c "$2" /dev/urandom >"$1"; } # <path> <bytes>
blob "$scratch/a.bin" $((4 * MIB))
blob "$scratch/pg/held.bin" $((2 * MIB))
blob "$cache/c.bin" $((1 * MIB))
blob "$SPAWN_WORKSPACE_ROOT/.transcripts/agent_a/t.jsonl" $((256 * KIB))
blob "$SPAWN_CLAUDE_PROJECTS/-bb/s.jsonl" $((256 * KIB))
sync "$scratch/a.bin" "$cache/c.bin"
cp --reflink=always "$scratch/a.bin" "$scratch/a-reflink.bin" 2>/dev/null ||
    cannot_judge "TMPDIR's file system makes no reflinks (cp --reflink=always failed under $WORK)"
cp --reflink=always "$cache/c.bin" "$scratch/c-reflink.bin" ||
    harness "the build cache's file could not be reflinked into scratch"
cp --reflink=never "$scratch/a.bin" "$scratch/a-plain.bin"
sync
cat >"$SPAWN_CORE_LINES_FILE" <<EOF
{"storage": {"scratch_roots": ["$scratch"], "scratch_exclude": ["$scratch/pg"],
             "quiet_hold_file": "$quiet"}}
EOF

# Two days ago: a reading, as the loop records them (a control record).
now_ms=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$now_ms" in '' | *[!0-9]*) harness "no time from the database: [$now_ms]" ;; esac
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
select 'agent_control', 'session_control', null, 'control', null,
       jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record',
           '{"type": "host.storage_bytes", "sources": {"scratch": {"bytes": 1}}}'::jsonb)),
       coalesce(max(session_seq), 0) + 1, $((now_ms - 2 * 86400000))
  from spawn_events where session_id = 'session_control';
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the reading failed: $(err)"
expect_eq "$(readings)" 1 "the seeded reading"

# --- (1) an owner hold: no reading ------------------------------------------------

printf 'IN PROGRESS fd-bb at 10:00\n' >"$quiet"
start_loop
held() { grep -q "storage bytes: not measured (owner gate)" "$WORK/loop.log"; }
if wait_until 60 held; then
    sleep 2
    expect_eq "$(readings)" 1 "(1) readings while the quiet-hold file says IN PROGRESS"
else
    check_failed "(1) the loop did not say, within 60 s, that a hold put the reading off"
fi
stop_loop
note "(1) checked: no reading during a hold"

# --- (2) the reading ---------------------------------------------------------------

printf 'ENDED fd-bb at 10:05\n' >"$quiet"
start_loop
read_once() { [ "$(readings)" -ge 2 ]; }
if wait_until 120 read_once; then
    sources=$(q "select kind -> 'detail' -> 'record' -> 'sources' from spawn_events
                  where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage_bytes' order by id desc limit 1")
    expect_eq "$(printf '%s' "$sources" | jq -c '.scratch.bytes')" $((8 * MIB)) \
        "(2) scratch: a file and its plain copy, never its reflink, the excluded directory or the build cache's reflink"
    expect_eq "$(printf '%s' "$sources" | jq -c '.build_cache.bytes')" $((1 * MIB)) "(2) the build cache"
    expect_eq "$(printf '%s' "$sources" | jq -c '.transcripts.bytes')" $((512 * KIB)) \
        "(2) transcripts: the archive and Claude Code's projects"
    expect_eq "$(printf '%s' "$sources" | jq -c '.test_databases.bytes')" 0 "(2) test databases (none configured)"
    expect_eq "$(printf '%s' "$sources" | jq -c '.session_backups | has("not_measured")')" true \
        "(2) session backups without a status file: not measured"
else
    check_failed "(2) the loop recorded no host.storage_bytes reading within 120 s of its start"
fi
stop_loop
note "(2) checked: the reading"

# --- (3) a restart within the day ---------------------------------------------------

before=$(readings)
start_loop
sleep 8
expect_eq "$(readings)" "$before" "(3) readings after a restart within the day"
stop_loop
note "(3) checked: no second reading within the day"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "180: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "180: the loop reads the real bytes on disk daily, each extent once, outside the owner's holds"
exit 0
