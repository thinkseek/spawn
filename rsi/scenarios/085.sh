#!/bin/sh
# Black box: how long the disk lasts is measured from every grower that
# persists, not from the transcript archive alone.
#
# The problem (owner via lead-mac and rev-e, 2026-11-12 13:50;
# sync/roadmap-effects.md, Storage, "Days until the disk is full";
# sync/notes-guardrails-rev-e.md, item 7): the storage north star is the
# days until the disk is full, at the measured growth of what persists. The
# page computes it from the transcript archive alone (objective 065), but
# Claude Code's transcripts (never deleted) and the live database grow too,
# so the figure says the disk lasts longer than it does. Workspaces are left
# out: they are checkouts that come and go with tasks, and the card shows
# their total on its own row.
#
# Required (through `spawn loop`'s sample, `GET /api/fleet`, and
# web/src/resources.ts under node):
#   (1) the loop's `host.storage` sample also records `database_bytes`: the
#       live database's size (pg_database_size), as health.storage.database
#       reads it.
#   (2) `health.storage` keeps `growth_bytes_per_day` as the archive's alone,
#       and adds
#       - `growth_parts` {archive, projects, database}: bytes a day, each
#         measured as 065 measures the archive's (its size in this answer
#         less its size in the oldest sample of the last 7 days that
#         carries it, over the days between, rounded down; null without such
#         a sample a day old): the archive's from `archive_bytes`, Claude
#         Code's transcripts' from `projects_bytes`, the database's from
#         `database_bytes` against this answer's database bytes;
#       - `persistent_growth_bytes_per_day`: their sum, null while any part
#         is null;
#       - `days_left`: free_bytes over the persistent growth, rounded down;
#         null without one above 0.
#   (3) resources.ts `storageView`: `lasts` reads "about <days> days at the
#       growth of the archive, Claude Code's transcripts and the database";
#       the archive's row still says the archive's own growth.
#
# The world: archive 12 MiB, Claude Code's projects 1 MiB, workspaces 2 MiB
# (as 065). Samples recorded before the loop starts: 3 days ago in the old
# shape (no database_bytes): archive 9 MiB, projects 0.25 MiB; 2 days ago:
# archive 10 MiB, projects 0.5 MiB, and database_bytes 8 MiB below the
# database's size when seeded. So the archive and the projects are measured
# from 3 days ago, the database from 2 days ago.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "085: harness: $*" >&2
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
SPAWN_DB_NAME="bb_085_growth_$run"
SPAWN_TMUX_SOCKET="bb-085-$run"
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
            echo "085: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "085: the host.storage records (recorded, record):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select recorded_at_ms, kind -> 'detail' -> 'record' from spawn_events
                where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage' order by id" >&2 2>/dev/null || true
        fi
        if [ -s "$WORK/api.json" ]; then
            echo "085: the answer's storage:" >&2
            jq -c '.health.storage' "$WORK/api.json" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "085: harness: unexpected exit $rc" >&2
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
note() { echo "085: $*" >&2; }
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

# --- the disk: archive, Claude Code's projects, workspaces ------------------------------

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
                                           'database_bytes', $db_then))) v(ago_ms, r)
 order by ago_ms desc;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the samples failed: $(err)"
at3=$(q "select min(recorded_at_ms) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage'")
at2=$(q "select max(recorded_at_ms) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage'")
seeded_max=$(q "select max(id) from spawn_events")

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
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "$base/api/fleet" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/fleet answered $code: $(head -c 300 "$WORK/api.json")"
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
persistent=$(storage '.persistent_growth_bytes_per_day')
if [ -n "$archive" ] && [ "$archive" != null ] && [ "$projects" != null ] && [ "$database" != null ] && [ -n "$projects" ] && [ -n "$database" ]; then
    expect_eq "$persistent" "$((archive + projects + database))" "(2) persistent_growth_bytes_per_day: the three parts' sum"
fi
free=$(storage '.free_bytes')
if [ "$free" != null ] && [ "$persistent" != null ] && [ "$persistent" -gt 0 ] 2>/dev/null; then
    expect_eq "$(storage '.days_left')" "$((free / persistent))" "(2) days_left: free_bytes over the persistent growth, rounded down"
else
    check_failed "(2) no persistent growth above 0 to check days_left with: [$persistent]"
fi
note "(2) checked: the answer"

# --- (3) the card's words --------------------------------------------------------------------

cat >"$WORK/storage.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { storageView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/resources.ts`).href);
const storage = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8")).health.storage;
const mb = (bytes) => `${(bytes / 1024 ** 2).toFixed(1)} MB`;

test("(3) how long the disk lasts, and from what", () => {
  const view = storageView(storage);
  assert.equal(typeof storage.days_left, "number", "the answer has days_left");
  assert.equal(view.lasts,
    `about ${storage.days_left.toLocaleString("en-US")} days at the growth of the archive, Claude Code's transcripts and the database`);
  const archive = view.rows.find((row) => row.name === "Transcript archive");
  assert.equal(archive.note, `grows about ${mb(storage.growth_bytes_per_day)} a day`, "the archive's row: its own growth");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/storage.test.mjs" >"$WORK/storage.out" 2>&1; then
    check_failed "(3) web/src/resources.ts storageView over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/storage.out" | head -n 10 | tr '\n' ' ' | head -c 900)"
fi
note "(3) checked: the card's words"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "085: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "085: how long the disk lasts is measured from the archive, Claude Code's transcripts and the database"
exit 0
