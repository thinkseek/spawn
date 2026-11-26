#!/bin/sh
# Black box: scratch has an owner, an objective and a lease (objective 140).
# `spawn scratch claim|renew|release` writes a folder's owner file; the
# storage line's scratch walk reads it, lists every claimed folder by owner
# with its age, bytes, build output and whether its objective landed, and
# counts a folder whose lease ended as nobody's ("lease ended"). Nothing is
# removed: the sweep of landed build output is a later objective (145).
#
# Required:
#   (1) claim: `spawn scratch claim <dir> --agent <name> --objective
#       <mission>/<objective> [--build <path>]... [--lease-hours <h>]`
#       writes <dir>/.spawn-owner, one JSON line {agent, objective,
#       created_ms, lease_until_ms, build}, the lease 24 h by default.
#       Refused, nothing written: a relative folder; a folder named through
#       a symlink; a build path through a symlink or not ending in target;
#       --lease-hours outside 1..168; a folder whose lease runs (renew or
#       release it). renew sets the lease from now; release ends it.
#   (2) GET /api/core-lines: `scratch.claimed` lists every claimed folder,
#       at a root's top level or one below it, by owner then oldest first,
#       each {path, agent, objective, created_ms, lease_until_ms, bytes,
#       build_bytes, landed}: bytes the folder's regular files (null while
#       its lease runs: objective 155 never walks it whole), build_bytes
#       its build paths' (never through a symlink), landed whether the
#       store has a task.landed of that mission and objective.
#   (3) a claimed folder whose lease ended is nobody's, reason "lease
#       ended", and its bytes count in storage.unowned_scratch_bytes; 094's
#       pid markers read as before.
#   (4) the page's words (web/src/coreLines.ts, as the Home card shows
#       them): "<agent> · <objective>" and "<size> (build <size>) · <age>
#       old · lease until <time>|lease ended · landed|not landed".
#   (5) read only: the answer changes nothing under the scratch root.
#
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure.

set -eu

asserted=""
harness() {
    echo "140: harness: $*" >&2
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
for tool in psql jq od curl sed grep sha1sum cut node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_140_scratch_$run"
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
    for pid in "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
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
        echo "140: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "140: harness: unexpected exit $rc" >&2
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
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL SPAWN_FLEET_TEST_DATABASE_URL; do
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



note() { echo "140: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

seed_now=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]" ;; esac
A=$(printf 'bb-140-a' | sha1sum | cut -c1-40)
B=$(printf 'bb-140-b' | sha1sum | cut -c1-40)
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
begin;
create temp sequence seed_seq start 100000;
create function pg_temp.rec(at bigint, r jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), at) \$\$;
create function pg_temp.objective(at bigint, task int, objective int) returns void language sql as \$\$
    select pg_temp.rec(at, jsonb_build_object('type', 'task.assigned', 'id', task,
        'mission_id', 1, 'kind', 'ephemeral', 'created_by', 'resident',
        'agent_id', 'agent_worker_' || task, 'session_id', 'session_worker_' || task,
        'objective', objective, 'brief', 'Objective ' || objective || '.')) \$\$;
create function pg_temp.landed(at bigint, task int) returns void language sql as \$\$
    select pg_temp.rec(at, jsonb_build_object('type', 'task.landed', 'id', task,
        'from', '$A', 'to', '$B')) \$\$;
select pg_temp.rec($seed_now - 7200000, '{"type":"mission.added","id":1,"text":"140 mission","origin":"human","sources":[]}');
select pg_temp.objective($seed_now - 7200000, 1, 7);
select pg_temp.landed($seed_now - 3600000, 1);
select pg_temp.objective($seed_now - 7200000, 2, 8);
select pg_temp.objective($seed_now - 7200000, 3, 9);
select pg_temp.landed($seed_now - 3600000, 3);
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(scalar "select string_agg(id || ':' || mission_id || '/' || objective || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:1/7:landed 2:1/8:assigned 3:1/9:landed" ] || harness "the seed folded to tasks [$tasks]"

S=$(cd "$WORK" && mkdir -p scratch && cd scratch && pwd -P)
fill() { # <file> <bytes>
    mkdir -p "$(dirname "$1")"
    head -c "$2" /dev/zero >"$1" || harness "cannot write $1"
}
size_of() { find "$1" -type f -printf '%s\n' | awk '{ s += $1 } END { print s + 0 }'; }
ticks() { awk '{ print $22 }' "/proc/$1/stat"; }
my_ticks=$(ticks $$)
case "$my_ticks" in '' | *[!0-9]*) harness "no start time for this shell: [$my_ticks]" ;; esac
sh -c 'exit 0' &
dead=$!
wait "$dead" || true
[ ! -e "/proc/$dead" ] || harness "the exited process $dead is still there"
owner() { jq -c . "$1/.spawn-owner" 2>/dev/null; }
scratch() { capture "$SPAWN_BIN" scratch "$@"; }

fill "$S/a/t/target/debug/x" 3000
fill "$S/a/src.rs" 100
fill "$S/b/y" 2000
fill "$S/sess/o/z" 500
fill "$S/sess/p/w" 700
fill "$S/c/t/target/x" 10
fill "$WORK/outside/target/secret" 9000
ln -s "$WORK/outside" "$S/c/link" || harness "ln -s failed"
ln -s "$S/a" "$S/alias" || harness "ln -s failed"
fill "$S/old/data" 100000
printf '%s %s\n' "$$" "$my_ticks" >"$S/old/.spawn-owner"
fill "$S/gone/data" 300000
printf '%s %s\n' "$dead" "$my_ticks" >"$S/gone/.spawn-owner"
find "$S/old" "$S/gone" -exec touch -h -d '2 days ago' {} + || harness "cannot age the pid-marked entries"

# --- (1) claim, renew, release ---------------------------------------------------------------
refused() { # <check> <what the message holds> <args...>
    check=$1
    holds=$2
    shift 2
    scratch "$@"
    if [ "$status" -eq 0 ]; then
        check_failed "$check spawn scratch $* was taken: $(out)"
    else
        case "$(err)" in *"$holds"*) ;; *) check_failed "$check spawn scratch $*: no [$holds] in [$(err)]" ;; esac
    fi
}
refused "(1)" "absolute" claim sess/o --agent rev-e --objective 1/9
refused "(1)" "a symlink" claim "$S/alias" --agent rev-e --objective 1/7
refused "(1)" "a symlink" claim "$S/c" --agent rev-e --objective 1/7 --build link/target
refused "(1)" "ending in target" claim "$S/c" --agent rev-e --objective 1/7 --build t
refused "(1)" "--lease-hours" claim "$S/c" --agent rev-e --objective 1/7 --lease-hours 0
refused "(1)" "--lease-hours" claim "$S/c" --agent rev-e --objective 1/7 --lease-hours 169
[ ! -e "$S/c/.spawn-owner" ] || check_failed "(1) a refused claim wrote an owner file: $(cat "$S/c/.spawn-owner")"

before_claim=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
scratch claim "$S/a" --agent rev-e --objective 1/7 --build t/target --build base/target
[ "$status" -eq 0 ] || check_failed "(1) claiming a: $(err)"
expect_eq "$(owner "$S/a" | jq -c '{agent, objective, build, day: (.lease_until_ms - .created_ms)}')" \
    '{"agent":"rev-e","objective":"1/007","build":["t/target","base/target"],"day":86400000}' \
    "(1) a's owner file: the agent, the objective, the build paths, a 24 h lease"
expect_eq "$(owner "$S/a" | jq -c 'keys')" '["agent","build","created_ms","lease_until_ms","objective"]' \
    "(1) the owner file holds exactly its five fields"
expect_eq "$(wc -l <"$S/a/.spawn-owner" | tr -d ' ')" 1 "(1) the owner file is one line"
# After the claim, a's base becomes a symlink to a folder holding target/ (9000 bytes):
# a build path reached through it is never counted.
ln -s "$WORK/outside" "$S/a/base" || check_failed "(2) planting a's base symlink failed"
expect_eq "$(out)" "$(cat "$S/a/.spawn-owner")" "(1) claim prints the owner file's line"
refused "(1)" "renew or release" claim "$S/a" --agent rev-b --objective 1/8
scratch claim "$S/b" --agent rev-b --objective 1/8 --lease-hours 2
[ "$status" -eq 0 ] || check_failed "(1) claiming b: $(err)"
scratch renew "$S/b" --lease-hours 5
[ "$status" -eq 0 ] || check_failed "(1) renewing b: $(err)"
renewed=$(owner "$S/b" | jq '.lease_until_ms')
now_after=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
awk -v v="$renewed" -v lo="$((before_claim + 5 * 3600000))" -v hi="$((now_after + 5 * 3600000))" 'BEGIN { exit !(v >= lo && v <= hi) }' ||
    check_failed "(1) renew --lease-hours 5 sets the lease 5 h from now: [$renewed], not between [$((before_claim + 5 * 3600000))] and [$((now_after + 5 * 3600000))]"
scratch release "$S/b"
[ "$status" -eq 0 ] || check_failed "(1) releasing b: $(err)"
scratch claim "$S/sess/o" --agent rev-e --objective 1/9
[ "$status" -eq 0 ] || check_failed "(1) claiming sess/o: $(err)"
refused "(1)" "no claim" renew "$S/sess/p"

snapshot() { find "$S" "$WORK/outside" -printf '%p %y %s %T@ %l\n' | sort; }
snapshot >"$WORK/before.txt"

cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<JSON
{"ranges": {}, "storage": {"scratch_roots": ["$S"]}}
JSON
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
asked=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/core-lines answered $code: $(head -c 300 "$WORK/api.json")"

# --- (2) every claimed folder, by owner ------------------------------------------------------
expect_eq "$(jq -c '[.scratch.claimed[] | {path, agent, objective, bytes, build_bytes, landed}]' "$WORK/api.json")" \
    "[{\"path\":\"$S/b\",\"agent\":\"rev-b\",\"objective\":\"1/008\",\"bytes\":$(size_of "$S/b"),\"build_bytes\":0,\"landed\":false},{\"path\":\"$S/a\",\"agent\":\"rev-e\",\"objective\":\"1/007\",\"bytes\":null,\"build_bytes\":3000,\"landed\":true},{\"path\":\"$S/sess/o\",\"agent\":\"rev-e\",\"objective\":\"1/009\",\"bytes\":null,\"build_bytes\":0,\"landed\":true}]" \
    "(2) the claimed folders by owner, oldest first: b (not landed), a (its t/target counted, base/target through a symlink not), sess/o one level down (landed); not c (refused), sess/p or the pid-marked ones; a and sess/o leased, so never walked whole (bytes null, objective 155)"
expect_eq "$(jq -c --argjson c "$(owner "$S/a")" '[.scratch.claimed[] | select(.agent == "rev-e" and .objective == "1/007") | {created_ms, lease_until_ms}] == [{created_ms: $c.created_ms, lease_until_ms: $c.lease_until_ms}]' "$WORK/api.json")" \
    true "(2) a's created and lease as its owner file says"

# --- (3) nobody's -------------------------------------------------------------------------------
expect_eq "$(jq -c '[.scratch.unowned[] | {path, bytes, reason}]' "$WORK/api.json")" \
    "[{\"path\":\"$S/gone\",\"bytes\":$(size_of "$S/gone"),\"reason\":\"gone\"},{\"path\":\"$S/b\",\"bytes\":$(size_of "$S/b"),\"reason\":\"lease ended\"}]" \
    "(3) nobody's: the exited owner's (094) and b, released (lease ended); not a or sess/o (leased), old (its pid runs), sess (changed now)"
# Objective 176: scratch nobody owns is a storage diagnostic.
fig() { jq -c "(.diagnostics // [])[] | select(.key == \"$1\") | .value" "$WORK/api.json"; }
expect_eq "$(fig storage.unowned_scratch_bytes)" "$(($(size_of "$S/gone") + $(size_of "$S/b")))" \
    "(3) scratch nobody owns counts the ended lease"

# --- (4) the page's words ------------------------------------------------------------------------
cat >"$WORK/view.test.mjs" <<'NODE'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
const { coreLinesView } = await import(`${process.env.BB_TREE}/web/src/coreLines.ts`);
test("the claimed folders in the card's words", () => {
  const answer = JSON.parse(readFileSync(`${process.env.BB_WORK}/api.json`, "utf8"));
  const view = coreLinesView(answer, Number(process.env.BB_ASKED));
  assert.deepEqual(view.scratch.map((row) => row.name), ["rev-b · 1/008", "rev-e · 1/007", "rev-e · 1/009"]);
  assert.match(view.scratch[0].text, /^\d+\.\d MB \(build 0\.0 MB\) · .+ old · lease ended · not landed$/);
  assert.match(view.scratch[1].text, /^build \d+\.\d MB \(not walked whole while leased\) · .+ old · lease until \d\d-\d\d \d\d:\d\d · landed$/);
  assert.match(view.scratch[2].text, / · lease until \d\d-\d\d \d\d:\d\d · landed$/);
  assert.equal(view.scratch[1].detail, `${process.env.BB_SCRATCH}/a`);
});
NODE
if ! BB_TREE=$tree BB_WORK=$WORK BB_ASKED=$asked BB_SCRATCH=$S node --test "$WORK/view.test.mjs" >"$WORK/view.out" 2>&1; then
    check_failed "(4) the card's words for the claimed folders: $(grep -m 8 -E 'Error|expected|actual|\+ |- |^not ok' "$WORK/view.out" | tr '\n' ' ' | head -c 700)"
fi

# --- (5) read only -------------------------------------------------------------------------------
snapshot >"$WORK/after.txt"
cmp -s "$WORK/before.txt" "$WORK/after.txt" ||
    check_failed "(5) the scratch changed: $(diff "$WORK/before.txt" "$WORK/after.txt" | head -n 6 | tr '\n' ' ' | head -c 400)"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "140: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "140: scratch has an owner, an objective and a lease; claimed folders are listed by owner, an ended lease is nobody's, nothing removed"
exit 0
