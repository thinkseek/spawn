#!/bin/sh
# Black box: the storage page names the scratch that landed objectives
# left behind, by agent and objective (objective 232). The scratch walk
# (objectives 094, 140, 155) already lists every claimed folder with
# whether its objective landed; this adds up the landed ones, so the
# storage north star's gain (bytes a landing leaves behind) names who left
# it. Monitoring only: no range, no alert, nothing removed, no new store
# read.
#
# Required (GET /api/core-lines, its scratch walk's claimed folders):
#   (1) diagnostics hold storage.landed_scratch_bytes, "Scratch of landed
#       objectives", unit bytes, after the six of objective 176 (last), no
#       range, state "no_range", on no line, never an alert figure: its
#       value is the sum, over the claimed folders whose objective landed,
#       of each folder's bytes, or its build_bytes while its lease runs
#       (objective 155 never walks a leased folder whole). A folder of an
#       objective not landed, and an unclaimed folder whatever its name, add
#       nothing.
#   (2) its by_owner: one row per agent and objective, {agent, objective,
#       bytes, folders, leased} (leased: the folders counted by their build
#       output only), largest bytes first, then agent, then objective.
#   (3) the page's words (web/src/coreLines.ts, coreLinesView): the
#       diagnostic's own mark, and view.landedScratch, one row per by_owner
#       row: name "<agent> · <objective>", text "<size> · <n> folder(s)",
#       with " · <k> leased, build only" when k > 0.
#   (4) with no folder claimed: 0, by_owner [].
#   (5) read only: the answer changes nothing under the scratch root.
#   (6) (R2, rev-b's F1, rev-h's F1) no complete walk, as when
#       storage.unowned_scratch_bytes is null: the first walk past
#       storage.scratch_bound, no roots, the one root refused (an exclude
#       holds it; a symlink): value null with a note, never 0.
#   (7) (R2, rev-b's F2, rev-h's F2) a tie on bytes within one agent:
#       its rows by objective, whatever order the claims came in.
#
# On live main aaa1b740 this exits 1: no such diagnostic.
#
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure.

set -eu

asserted=""
harness() {
    echo "232: harness: $*" >&2
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
SPAWN_DB_NAME="bb_232_landed_$run"
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
        echo "232: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "232: harness: unexpected exit $rc" >&2
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



note() { echo "232: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

seed_now=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]" ;; esac
A=$(printf 'bb-232-a' | sha1sum | cut -c1-40)
B=$(printf 'bb-232-b' | sha1sum | cut -c1-40)
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
select pg_temp.rec($seed_now - 7200000, '{"type":"mission.added","id":1,"text":"232 mission","origin":"human","sources":[]}');
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
scratch() { capture "$SPAWN_BIN" scratch "$@"; }
fill "$S/a/t/target/debug/x" 50000
fill "$S/a/src.rs" 1000
fill "$S/b/y" 90000
fill "$S/d/z" 20000
fill "$S/sess/o/t/target/x" 3000
fill "$S/sess/o/src.rs" 400000
fill "$S/sess/q/w" 7000
fill "$S/t007/target/debug/big" 80000

# The claims: a and d of landed 1/7 (rev-e and rev-i), released, walked
# whole; b of 1/8 (not landed), released; sess/o of landed 1/9, leased, its
# build t/target only; sess/q of 1/9, released. t007 is claimed by nobody.
claim() { # <dir> <agent> <objective> [args...]
    d=$1 a=$2 o=$3
    shift 3
    scratch claim "$d" --agent "$a" --objective "$o" "$@"
    [ "$status" -eq 0 ] || harness "claiming $d: $(err)"
}
release() {
    scratch release "$1"
    [ "$status" -eq 0 ] || harness "releasing $1: $(err)"
}
claim "$S/a" rev-e 1/7 --build t/target
release "$S/a"
claim "$S/b" rev-b 1/8
release "$S/b"
claim "$S/d" rev-i 1/7
release "$S/d"
claim "$S/sess/o" rev-e 1/9 --build t/target
claim "$S/sess/q" rev-e 1/9
release "$S/sess/q"
A_BYTES=$(size_of "$S/a")
D_BYTES=$(size_of "$S/d")
Q_BYTES=$(size_of "$S/sess/q")

snapshot() { find "$S" -printf '%p %y %s %T@ %l\n' | sort; }
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

fig() { jq -c "[(.diagnostics // [])[] | select(.key == \"storage.landed_scratch_bytes\")] | .[0] // null | $1" "$WORK/api.json"; }

# --- (1) the figure ---------------------------------------------------------------------------
expect_eq "$(jq -c '.scratch.claimed | length' "$WORK/api.json")" 5 "(1) the walk lists the five claimed folders"
expect_eq "$(fig '{name, unit, range, state, line}')" \
    '{"name":"Scratch of landed objectives","unit":"bytes","range":null,"state":"no_range","line":null}' \
    "(1) the diagnostic: its name, bytes, no range, on no line"
expect_eq "$(jq -c '[(.diagnostics // [])[].key] | .[-1]' "$WORK/api.json")" '"storage.landed_scratch_bytes"' \
    "(1) last of the diagnostics"
expect_eq "$(jq -c '[(.alert_figures // [])[].key, (.lines[] | ([.north_star] + .guardrails)[].key)] | map(select(. == "storage.landed_scratch_bytes")) | length' "$WORK/api.json")" 0 \
    "(1) never an alert figure, north star or guardrail"
want=$((A_BYTES + D_BYTES + Q_BYTES + 3000))
expect_eq "$(fig '.value')" "$want" \
    "(1) a ($A_BYTES) + d ($D_BYTES) + sess/q ($Q_BYTES) + sess/o's build (3000), not b (1/8 not landed), not sess/o's sources, not t007 (unclaimed)"

# --- (2) by agent and objective ---------------------------------------------------------------
# a is the largest, then d, then 1/9's two folders (sess/q + sess/o's build).
rows="[{\"agent\":\"rev-e\",\"objective\":\"1/007\",\"bytes\":$A_BYTES,\"folders\":1,\"leased\":0}"
rows="$rows,{\"agent\":\"rev-i\",\"objective\":\"1/007\",\"bytes\":$D_BYTES,\"folders\":1,\"leased\":0}"
rows="$rows,{\"agent\":\"rev-e\",\"objective\":\"1/009\",\"bytes\":$((Q_BYTES + 3000)),\"folders\":2,\"leased\":1}]"
[ "$A_BYTES" -gt "$D_BYTES" ] && [ "$D_BYTES" -gt $((Q_BYTES + 3000)) ] || harness "the folders' sizes do not order a > d > 1/9"
expect_eq "$(fig '[.by_owner[] | {agent, objective, bytes, folders, leased}]')" "$rows" "(2) by_owner, largest first"
expect_eq "$(fig '[.by_owner[] | keys | join(",")] | unique')" '["agent,bytes,folders,leased,objective"]' \
    "(2) each row holds exactly its five fields"

# --- (3) the page's words ---------------------------------------------------------------------
cat >"$WORK/view.test.mjs" <<'NODE'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
const { coreLinesView } = await import(`${process.env.BB_TREE}/web/src/coreLines.ts`);
test("landed objectives' scratch in the card's words", () => {
  const answer = JSON.parse(readFileSync(`${process.env.BB_WORK}/api.json`, "utf8"));
  const view = coreLinesView(answer, Number(process.env.BB_ASKED));
  const mark = view.diagnostics[view.diagnostics.length - 1];
  assert.equal(mark.name, "Scratch of landed objectives");
  assert.match(mark.value, /^\d+\.\d MB$/);
  assert.deepEqual(view.landedScratch.map((row) => row.name), ["rev-e · 1/007", "rev-i · 1/007", "rev-e · 1/009"]);
  assert.match(view.landedScratch[0].text, /^\d+\.\d MB · 1 folder$/);
  assert.match(view.landedScratch[2].text, /^\d+\.\d MB · 2 folders · 1 leased, build only$/);
});
NODE
if ! BB_TREE=$tree BB_WORK=$WORK BB_ASKED=$asked node --test "$WORK/view.test.mjs" >"$WORK/view.out" 2>&1; then
    check_failed "(3) the card's words: $(grep -m 8 -E 'Error|expected|actual|\+ |- |^not ok' "$WORK/view.out" | tr '\n' ' ' | head -c 700)"
fi

# --- (5) read only ----------------------------------------------------------------------------
snapshot >"$WORK/after.txt"
cmp -s "$WORK/before.txt" "$WORK/after.txt" ||
    check_failed "(5) the scratch changed: $(diff "$WORK/before.txt" "$WORK/after.txt" | head -n 6 | tr '\n' ' ' | head -c 400)"

# --- (4) nothing claimed ----------------------------------------------------------------------
kill "$serve_pid" 2>/dev/null || true
wait "$serve_pid" 2>/dev/null || true
serve_pid=""
E=$(cd "$WORK" && mkdir -p empty/x && cd empty && pwd -P)
fill "$E/x/f" 100
cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<JSON
{"ranges": {}, "storage": {"scratch_roots": ["$E"]}}
JSON
: >"$WORK/serve.log"
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
[ -n "$port" ] || harness "the second serve did not start: $(head -c 400 "$WORK/serve.log")"
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "the second GET /api/core-lines answered $code"
expect_eq "$(fig '{value, by_owner}')" '{"value":0,"by_owner":[]}' "(4) nothing claimed: 0 and no rows"

# --- (6) no complete walk: null with a note, never 0 (R2: rev-b's F1 P1-P3, rev-h's F1) ------
reserve() { # <core-lines json>: serve again with this Core lines file, and ask once
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
    printf '%s\n' "$1" >"$SPAWN_WORKSPACE_ROOT/core-lines.json"
    : >"$WORK/serve.log"
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
    [ -n "$port" ] || harness "serve did not start again: $(head -c 400 "$WORK/serve.log")"
    code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/core-lines answered $code after a restart"
}
unmeasured() { # <check>
    expect_eq "$(fig '{value, noted: (.note | type == "string" and length > 0)}')" '{"value":null,"noted":true}' \
        "$1: null with a note, never 0"
}
reserve "{\"ranges\": {}, \"storage\": {\"scratch_roots\": [\"$S\"], \"scratch_bound\": {\"entries\": 1, \"ms\": 1}}}"
unmeasured "(6) the first walk past storage.scratch_bound"
reserve '{"ranges": {}}'
unmeasured "(6) no scratch roots"
reserve "{\"ranges\": {}, \"storage\": {\"scratch_roots\": [\"$E\"], \"scratch_exclude\": [\"$WORK\"]}}"
[ "$(jq -c '.scratch.refused | length' "$WORK/api.json")" -ge 1 ] || harness "the root was not refused: $(jq -c '.scratch.refused' "$WORK/api.json")"
unmeasured "(6) the one root refused (an exclude holds it)"
ln -s "$S" "$WORK/link" || harness "ln -s failed"
reserve "{\"ranges\": {}, \"storage\": {\"scratch_roots\": [\"$WORK/link\"]}}"
unmeasured "(6) the one root refused (a symlink)"

# --- (7) a tie on bytes within one agent: by objective (R2: rev-b's F2 P4, rev-h's F2) ---------
T=$(cd "$WORK" && mkdir -p tie && cd tie && pwd -P)
fill "$T/m/f" 5000
fill "$T/n/f" 5000
claim "$T/m" rev-e 1/9
release "$T/m"
claim "$T/n" rev-e 1/7
release "$T/n"
[ "$(size_of "$T/m")" -eq "$(size_of "$T/n")" ] || harness "the tied folders differ in size"
reserve "{\"ranges\": {}, \"storage\": {\"scratch_roots\": [\"$T\"]}}"
expect_eq "$(fig '[.by_owner[] | .objective]')" '["1/007","1/009"]' \
    "(7) rev-e's two landed objectives, equal bytes, 1/9 claimed first: 1/007 then 1/009"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "232: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "232: the storage page names landed objectives' scratch by agent and objective; read only"
exit 0
