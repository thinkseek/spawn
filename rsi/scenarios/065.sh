#!/bin/sh
# Black box: the page says what is using the disk, how fast the transcript
# archive grows, how long the disk lasts at that rate, and when Claude
# Code's own transcripts fall.
#
# The problem, on the live host (sync/roadmap-effects.md, "Core tracks:
# storage": "the owner can see on the page what is using the disk and how
# long it lasts"; the owner's 01:00 order that one of 064-066 be a storage
# objective about showing growth). The transcript archive is the only
# steady grower (about 0.07 GB a day; the owner has ruled that no
# transcript is deleted), the live database had bloated to 2.7 GB for about
# 17 MB of data before it was compacted by hand, and on 11-12 between 00:34
# and 00:37 every native Claude Code transcript on the host (~/.claude/
# projects, 274 project directories) was removed by a test run, found by a
# person. Each was found with `du`, psql or ls. The page shows none of it:
# `/api/fleet`'s health says only the free space of the workspace root and
# TMPDIR (crates/spawn-cli/src/disk.rs `report`), and Home's host section
# the same (web/src/fleet.ts `disk`, web/src/resources.ts).
#
# Required (read through the store, `GET /api/fleet` and web/src/resources.ts
# under node over it):
#   (1) the loop records a `host.storage` sample at its start, and then
#       whenever its latest one is an hour old: {archive_bytes,
#       archive_files, projects_bytes, projects_files}, the regular files
#       (apparent sizes summed) under <root>/.transcripts and under Claude
#       Code's projects directory (as the Bridge resolves it:
#       SPAWN_CLAUDE_PROJECTS, else $CLAUDE_CONFIG_DIR/projects, else
#       $HOME/.claude/projects).
#   (2) `health.storage`: `free_bytes` (the workspace root's file system)
#       read at the request; the sizes read at most every 10 minutes and
#       kept between (the page polls /api/fleet every 3 s, and a walk of
#       the live root takes about 0.3 s), `read_at_ms` saying when:
#       - `archive` {path, bytes, files}; `projects` {path, bytes, files,
#         before}: `before` the `projects_files` of the newest sample at
#         least an hour older than `read_at_ms` (null without one);
#       - `workspaces` {path, bytes}: the regular files under the root
#         outside .transcripts and .transcripts-progress;
#       - `database` {bytes, tables}: pg_database_size, and the five largest
#         tables by pg_total_relation_size, [{name, bytes}], largest first;
#       - `growth_bytes_per_day`: what was appended to the archive: its
#         bytes now less the `archive_bytes` of the oldest sample of the
#         last 7 days, over the days between them; null without a sample
#         at least a day old;
#       - `days_left`: free_bytes over growth_bytes_per_day, rounded down;
#         null when the growth is null or not above zero.
#   (3) resources.ts `storageView(storage)`: `rows` [{name, size, note}]
#       holding, in this order (other rows may sit among them), rows for
#       "Transcript archive" (note "grows about <size> a day", or "not
#       measured yet" without a growth), "Claude Code transcripts" (note
#       "<files> files", and ", down from <before>" when `before` is more),
#       "Workspaces" and "Database" (note "largest: <table> <size>",
#       anything another objective adds after a comma);
#       `lasts`: "about <days> days at the archive's growth" (days grouped
#       en-US), null when days_left is null. A size reads "<n> MB" to one
#       decimal under 1024 MB (1 MB = 1024 * 1024 bytes), "<n> GB" above.
#
# A real `spawn loop` and `spawn serve` run on a scratch database with a
# scratch workspace root and projects directory (HOME is a scratch
# directory too: nothing here reads or writes the real one):
#   archive: agent_a/a1.jsonl 3 MiB, agent_a/a2.jsonl 4 MiB,
#     agent_b/b1.jsonl 5 MiB: 12 MiB in 3 files;
#   projects: 3 files of 1 MiB in all; workspaces: agent_c/big.bin 2 MiB;
#   samples recorded before the loop starts: 6.5 days ago archive 5 MiB
#     (so 7 MiB appended in 6.5 days), projects 40 files; 2 hours ago
#     archive 11 MiB, projects 40 files (so the 3 now are down from 40).
# The loop's own sample is the barrier for the reads. The free space and
# the database are compared with `df` and psql read at the same time,
# within 1% (or 64 MiB of free space). Each check is reported; the run
# fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "065: harness: $*" >&2
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
SPAWN_DB_NAME="bb_065_runs_$run"
SPAWN_TMUX_SOCKET="bb-065-$run"
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
            echo "065: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "065: the host.storage records (recorded, record):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select recorded_at_ms, kind -> 'detail' -> 'record' from spawn_events
                where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage' order by id" >&2 2>/dev/null || true
        fi
        if [ -s "$WORK/api.json" ]; then
            echo "065: the answer's storage:" >&2
            jq -c '.health.storage' "$WORK/api.json" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "065: harness: unexpected exit $rc" >&2
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
cat >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
printf 'Welcome to Claude Code\n\n❯ '
while :; do
  if IFS= read -r -t 1 typed; then printf 'you: %s\n\nidle\n❯ ' "$typed"; fi
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
note() { echo "065: $*" >&2; }
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

# Two samples from before, as the loop records them (control records),
# 6.5 days and 2 hours ago.
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
create temp sequence seed_seq start 100000;
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
select 'agent_control', 'session_control', null, 'control', null,
       jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
       nextval('seed_seq'), (floor(extract(epoch from clock_timestamp()) * 1000))::bigint - ago_ms
  from (values
    (561600000::bigint, jsonb_build_object('type', 'host.storage', 'archive_bytes', 5 * $MIB, 'archive_files', 1,
                                           'projects_bytes', 9 * $MIB, 'projects_files', 40)),
    (7200000::bigint, jsonb_build_object('type', 'host.storage', 'archive_bytes', 11 * $MIB, 'archive_files', 3,
                                         'projects_bytes', 9 * $MIB, 'projects_files', 40))) v(ago_ms, r)
 order by ago_ms desc;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the samples failed: $(err)"
oldest_ms=$(q "select min(recorded_at_ms) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage'")
seeded_max=$(q "select max(id) from spawn_events")

# --- (1) the loop's sample ---------------------------------------------------------------

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
sampled() {
    [ -n "$(q "select id from spawn_events where id > $seeded_max
                 and kind -> 'detail' -> 'record' ->> 'type' = 'host.storage' limit 1")" ]
}
if wait_until 60 sampled; then
    expect_eq "$(q "select (kind -> 'detail' -> 'record') - 'type' from spawn_events where id > $seeded_max
                      and kind -> 'detail' -> 'record' ->> 'type' = 'host.storage' order by id limit 1" | jq -cS '{archive_bytes, archive_files, projects_bytes, projects_files}')" \
        "{\"archive_bytes\":$((12 * MIB)),\"archive_files\":3,\"projects_bytes\":$MIB,\"projects_files\":3}" \
        "(1) the loop's host.storage sample at its start"
else
    check_failed "(1) the loop recorded no host.storage sample within 60 s of its start (its latest was 2 hours old)"
fi
note "(1) checked: the loop's sample"

# --- (2) the answer ------------------------------------------------------------------------

# The loop keeps writing to the database, so its size is read on both sides
# of the answer (before serve starts, which may read it then, and after the
# GET): the answer's size must lie between the two.
db_before=$(scalar "select pg_database_size(current_database())")
# Every table's pg_total_relation_size, {name: bytes}: read on both sides of
# the answer too, as the tables grow while the loop writes.
table_sizes() {
    scalar "select coalesce(json_object_agg(c.relname, pg_total_relation_size(c.oid)), '{}')
              from pg_class c join pg_namespace n on n.oid = c.relnamespace
             where c.relkind in ('r', 'p') and n.nspname = 'public'" | jq -c . ||
        harness "psql's table sizes are not JSON"
}
tables_before=$(table_sizes)
start_serve
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "$base/api/fleet" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/fleet answered $code: $(head -c 300 "$WORK/api.json")"
df_free=$(df -B1 --output=avail "$root" | tail -n 1 | tr -d ' ')
db_bytes=$(scalar "select pg_database_size(current_database())")
tables_after=$(table_sizes)
# table_sizes runs in a subshell: its harness exit leaves the value empty.
[ -n "$tables_before" ] && [ -n "$tables_after" ] || harness "psql read no table sizes: [$tables_before] [$tables_after]"
for value in "$df_free" "$db_before" "$db_bytes"; do
    case "$value" in '' | *[!0-9]*) harness "df or psql read nothing: [$df_free] [$db_before] [$db_bytes]" ;; esac
done
storage() { jq -c ".health.storage | $1" "$WORK/api.json"; }

expect_eq "$(storage '.archive | {path, bytes, files}')" \
    "{\"path\":\"$root/.transcripts\",\"bytes\":$((12 * MIB)),\"files\":3}" "(2) the archive: 12 MiB in 3 files"
expect_eq "$(storage '.projects | {path, bytes, files, before}')" \
    "{\"path\":\"$projects\",\"bytes\":$MIB,\"files\":3,\"before\":40}" \
    "(2) Claude Code's projects: 1 MiB in 3 files, down from the 40 of the sample 2 hours ago"
expect_eq "$(storage '.workspaces | {path, bytes}')" "{\"path\":\"$root\",\"bytes\":$((2 * MIB))}" \
    "(2) the workspaces: the 2 MiB under the root outside the archive"
read_at=$(storage '.read_at_ms')
asked_ms=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
near "$read_at" "$asked_ms" 600000 || check_failed "(2) read_at_ms [$read_at]: when the sizes were read, within 10 minutes of [$asked_ms]"
growth=$(storage '.growth_bytes_per_day')
if [ "$read_at" != null ] && [ -n "$read_at" ]; then
    want_growth=$(awk -v b=$((7 * MIB)) -v ms=$((read_at - oldest_ms)) 'BEGIN { printf "%d", b * 86400000 / ms }')
    near "$growth" "$want_growth" "$((want_growth / 100))" ||
        check_failed "(2) growth_bytes_per_day [$growth]: 7 MiB appended since the sample 6.5 days ago, per day ([$want_growth])"
fi
free=$(storage '.free_bytes')
slack=$((df_free / 100 > 64 * MIB ? df_free / 100 : 64 * MIB))
near "$free" "$df_free" "$slack" || check_failed "(2) free_bytes [$free], against df's [$df_free] (slack $slack)"
if [ "$free" != null ] && [ "$growth" != null ] && [ "$growth" -gt 0 ] 2>/dev/null; then
    expect_eq "$(storage '.days_left')" "$((free / growth))" "(2) days_left: the answer's free_bytes over its growth, rounded down"
fi
got_db=$(storage '.database.bytes')
# Between the two reads, with 1% slack on each side for a page or two.
awk -v g="$got_db" -v lo="$((db_before - db_before / 100))" -v hi="$((db_bytes + db_bytes / 100))" \
    'BEGIN { exit !(g ~ /^[0-9]+$/ && g >= lo && g <= hi) }' ||
    check_failed "(2) database.bytes [$got_db], against pg_database_size read before [$db_before] and after [$db_bytes] the answer (1% slack each side)"
# The five largest tables, largest first, judged against the two readings:
# each listed table's bytes lie between its own before and after (with the
# same 1% slack, at least a page); the list is largest first by its own
# bytes (one reading of the answer's); and no table left out was larger,
# before the answer, than the smallest listed. An exact order of sizes read
# at another moment would fail on tables that grow past each other.
table_problems=$(jq -r --argjson b "$tables_before" --argjson a "$tables_after" '
    def slack(x): ([x / 100, 8192] | max);
    (.health.storage.database.tables // null) as $t
    | if ($t | type) != "array" then "the tables are not a list: \($t | tojson)"
      else
        ([($b | length), 5] | min) as $want
        | ([$t[].name]) as $names
        | ($t | map(.bytes) | min) as $low
        | (if ($t | length) != $want then "\($t | length) tables listed, not \($want)" else empty end),
          ($t[] | . as $r
            | if $b[$r.name] == null or $a[$r.name] == null then "\($r.name | tojson) is no table of the database"
              elif ($r.bytes | type) != "number"
                   or $r.bytes < $b[$r.name] - slack($b[$r.name])
                   or $r.bytes > $a[$r.name] + slack($a[$r.name])
              then "\($r.name) at \($r.bytes | tojson) bytes, outside its \($b[$r.name]) before and \($a[$r.name]) after"
              else empty end),
          (range(1; $t | length) as $i
            | select(($t[$i - 1].bytes | type) == "number" and ($t[$i].bytes | type) == "number"
                     and $t[$i - 1].bytes < $t[$i].bytes)
            | "\($t[$i - 1].name) (\($t[$i - 1].bytes)) is listed before the larger \($t[$i].name) (\($t[$i].bytes))"),
          ($b | to_entries[]
            | select(.key as $k | $names | any(. == $k) | not)
            | select(($low | type) == "number" and .value > $low + slack($low))
            | "\(.key) (\(.value) bytes before the answer) is left out, larger than the smallest listed (\($low))")
      end' "$WORK/api.json" 2>&1) || harness "jq could not read the answer's tables: $table_problems"
[ -z "$table_problems" ] ||
    check_failed "(2) the database's five largest tables, largest first: $(printf '%s' "$table_problems" | tr '\n' ';' | head -c 700)"
note "(2) checked: the answer"

# --- (3) the card's words --------------------------------------------------------------------

cat >"$WORK/storage.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { storageView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/resources.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const storage = answer.health.storage;
const mb = (bytes) => `${(bytes / 1024 ** 2).toFixed(1)} MB`;

// The four rows this objective names, found by name: each once, in this
// order. Rows another objective adds (074's "Gate test database") may sit
// among them and are no failure here.
const NAMED = ["Transcript archive", "Claude Code transcripts", "Workspaces", "Database"];
const named = (view, name) => {
  const found = view.rows.filter((row) => row.name === name);
  assert.equal(found.length, 1, `one "${name}" row, among ${JSON.stringify(view.rows.map((row) => row.name))}`);
  return found[0];
};

test("(3) the rows: what uses the disk", () => {
  const view = storageView(storage);
  const names = view.rows.map((row) => row.name).filter((name) => NAMED.includes(name));
  assert.deepEqual(names, NAMED, "the four named rows, each once, in this order");
  assert.deepEqual([named(view, NAMED[0]).size, named(view, NAMED[0]).note], ["12.0 MB", `grows about ${mb(storage.growth_bytes_per_day)} a day`]);
  assert.deepEqual([named(view, NAMED[1]).size, named(view, NAMED[1]).note], ["1.0 MB", "3 files, down from 40"]);
  assert.equal(named(view, NAMED[2]).size, "2.0 MB");
  // Its own words first; another objective may add after a comma (074's ", log 0.0 MB").
  assert.match(named(view, NAMED[3]).note, new RegExp(`^largest: ${storage.database.tables[0].name} \\d+\\.\\d (MB|GB)(, |$)`));
});

test("(3) without a fall, without a growth, and in GB", () => {
  const steady = storageView({ ...storage, projects: { ...storage.projects, before: 3 } });
  assert.equal(named(steady, NAMED[1]).note, "3 files");
  const young = storageView({ ...storage, growth_bytes_per_day: null, days_left: null });
  assert.deepEqual([named(young, NAMED[0]).note, young.lasts], ["not measured yet", null]);
  assert.equal(named(storageView({ ...storage, archive: { ...storage.archive, bytes: 3 * 1024 ** 3 } }), NAMED[0]).size, "3.0 GB");
});

test("(3) how long the disk lasts", () => {
  assert.equal(storageView(storage).lasts, `about ${storage.days_left.toLocaleString("en-US")} days at the archive's growth`);
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/storage.test.mjs" >"$WORK/storage.out" 2>&1; then
    check_failed "(3) web/src/resources.ts storageView over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/storage.out" | head -n 10 | tr '\n' ' ' | head -c 900)"
fi
note "(3) checked: the card's words"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "065: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "065: the page says what uses the disk, what the archive grows by, how long the disk lasts, and when Claude Code's transcripts fall"
exit 0
