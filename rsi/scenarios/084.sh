#!/bin/sh
# Black box: the Core lines frame. One answer and one Home card say, for each
# of the four core lines, its north star and its guardrails, each with its
# value, its healthy range from one file, and whether it is in that range.
#
# The problem (owner, 2026-11-12 13:30; sync/roadmap-effects.md,
# "North-star and guardrail metrics", "Showing and alerting"): the owner
# steers by four core lines (performance, storage, observability,
# recoverability), each with one north star and a few guardrails that must
# stay in a healthy range. The figures are scattered over the page, or not
# collected, and no range is written anywhere a program can read, so
# nothing can say "out of range". This objective builds the frame and fills
# it with the storage figures that exist today; later objectives add the
# other lines' figures, the out-of-range alert and the 7-day trends.
# Monitoring only: no gate, integrator or deploy reads any of it.
#
# Required (read through `GET /api/core-lines`, and web/src/coreLines.ts
# under node):
#   (1) `GET /api/core-lines` answers {lines, ranges_file, ranges_error,
#       ignored_ranges}. `lines` is the four lines in the order performance,
#       storage, observability, recoverability, each {line, north_star,
#       guardrails}; every figure is {key, name, unit, value, range, state}.
#       Storage: north star `storage.days_left`, guardrails in this order
#       `storage.free_bytes`, `storage.archive_growth`,
#       `storage.database_ratio`, `storage.test_database_bytes`,
#       `storage.test_database_schemas`, their values the same as
#       `/api/fleet`'s health.storage (days_left, free_bytes,
#       growth_bytes_per_day, database bytes over log_bytes, the largest
#       test database's bytes, the most schemas). The other lines' north
#       stars (`performance.report_to_live`, `observability.needs_you_right`,
#       `recoverability.restores_passed`) are not collected yet: value null,
#       state `not_collected`, no guardrails yet.
#   (2) Ranges come only from the ranges file, read on every request:
#       `SPAWN_CORE_LINES_FILE`, by default <workspace root>/core-lines.json,
#       {"ranges": {<key>: {"min"?: n, "max"?: n}}}. A figure's `range` is
#       its entry or null, and its `state` is `in` or `out` of it,
#       `no_range` without one, `not_collected` without a value. A key that
#       names no figure, or an entry without a number min or max, is left
#       out and listed in `ignored_ranges` [{key, reason}] by key. A missing
#       file is no ranges and no error; a file that is not such JSON is no
#       ranges and a `ranges_error` saying why.
#   (3) web/src/coreLines.ts `coreLinesView(answer)`: {rows, notice,
#       ignored}, a row per line {line, north_star, guardrails} whose marks
#       are {name, value, range, tone} in words (stated in the test below),
#       `notice` "ranges file unreadable: <ranges_error>" or null, and
#       `ignored` "<key>: <reason>" for each ignored range. The Home card
#       shows the rows, the notice and the ignored ranges (read off a
#       screenshot: no check here reads the page's source).
#
# The history is seeded as the store holds it: one `host.storage` sample
# two days before "now", over a 3 MiB archive, so the archive grows about
# 1 MiB a day. `spawn serve` answers.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "084: harness: $*" >&2
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
SPAWN_DB_NAME="bb_084_core_lines_$run"
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
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "084: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "084: harness: unexpected exit $rc" >&2
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


note() { echo "084: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}
near() { # <got> <want> <slack>: |got - want| <= slack
    [ -n "$1" ] && [ "$1" != null ] && awk -v g="$1" -v w="$2" -v s="$3" 'BEGIN { d = g - w; if (d < 0) d = -d; exit !(d <= s) }'
}


# --- the history -------------------------------------------------------------------------

seed_now=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]" ;; esac
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
begin;
create function pg_temp.ms(h numeric) returns bigint language sql as \$\$
    select $seed_now::bigint + round(h * 3600000)::bigint \$\$;
create temp sequence seed_seq start 100000;
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), pg_temp.ms(h)) \$\$;
-- A sample as the loop records it (storage.rs, spawn_host_record).
create function pg_temp.sample(h numeric, archive bigint) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'host.storage', 'archive_bytes', archive,
        'archive_files', 1, 'projects_bytes', 0, 'projects_files', 0)) \$\$;
select pg_temp.sample(-48, 1048576);
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
# The archive the sizes are read from: 3 MiB.
mkdir -p "$SPAWN_WORKSPACE_ROOT/.transcripts/agent_a"
head -c 3145728 /dev/zero >"$SPAWN_WORKSPACE_ROOT/.transcripts/agent_a/a1.jsonl"

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
get() { # <path> <file>: the answer's code
    curl -s --noproxy '*' -o "$2" -w '%{http_code}' "http://127.0.0.1:$port$1" 2>/dev/null || echo 000
}
ranges=$SPAWN_WORKSPACE_ROOT/core-lines.json
unset SPAWN_CORE_LINES_FILE 2>/dev/null || true

# --- (1) and (2) with a ranges file ---------------------------------------------------------

cat >"$ranges" <<'JSON'
{"ranges": {
  "storage.free_bytes": {"min": 1},
  "storage.archive_growth": {"max": 524288},
  "storage.database_ratio": {"min": 0, "max": 1000000000000},
  "storage.test_database_schemas": {"max": "many"},
  "storage.free_bites": {"min": 1}
}}
JSON
# The test databases are shared and move all the time, so /api/fleet is
# read on both sides of the answer, and their figures must lie between.
fleet_code=$(get /api/fleet "$WORK/api-before.json")
[ "$fleet_code" = 200 ] || harness "GET /api/fleet answered $fleet_code"
code=$(get /api/core-lines "$WORK/lines.json")
fleet_code=$(get /api/fleet "$WORK/api.json")
[ "$fleet_code" = 200 ] || harness "GET /api/fleet answered $fleet_code"
if [ "$code" != 200 ]; then
    check_failed "(1) GET /api/core-lines answered $code: $(head -c 200 "$WORK/lines.json")"
else
    cp "$WORK/lines.json" "$WORK/api-lines.json"
    lines() { jq -c "$1" "$WORK/lines.json"; }
    fleet() { jq -c ".health.storage | $1" "$WORK/api.json"; }
    figure() { lines ".lines[] | select(.line == \"storage\") | ([.north_star] + .guardrails)[] | select(.key == \"$1\")"; }
    expect_eq "$(lines '[.lines[].line]')" '["performance","storage","observability","recoverability"]' "(1) the four lines in order"
    # The other lines' figures are filled by later objectives (086-088): only
    # their north stars' keys are this frame's, and every figure without a
    # value is not collected.
    expect_eq "$(lines '[.lines[] | select(.line != "storage") | .north_star.key]')" \
        '["performance.report_to_live","observability.needs_you_right","recoverability.restores_passed"]' \
        "(1) the other lines' north stars"
    expect_eq "$(lines '[.lines[] | ([.north_star] + .guardrails)[] | select((.value == null) != (.state == "not_collected")) | .key]')" '[]' \
        "(1) a figure is not collected exactly when it has no value"
    expect_eq "$(lines '.lines[] | select(.line == "storage") | [.north_star.key] + [.guardrails[].key]')" \
        '["storage.days_left","storage.free_bytes","storage.archive_growth","storage.database_ratio","storage.test_database_bytes","storage.test_database_schemas"]' \
        "(1) the storage line's north star and guardrails, in order"
    growth=$(fleet '.growth_bytes_per_day')
    near "$growth" 1048576 10486 || check_failed "(1) the seed: /api/fleet's growth [$growth] is about 1 MiB a day"
    days=$(figure storage.days_left | jq -r '.value')
    want_days=$(fleet '.days_left')
    days_state=no_range
    [ "$want_days" != null ] || days_state=not_collected
    expect_eq "$(figure storage.days_left | jq -c '{unit, range, state}')" \
        "{\"unit\":\"days\",\"range\":null,\"state\":\"$days_state\"}" "(1)(2) days left: no range set"
    # Both are the free space read at each request over the same growth.
    if [ "$want_days" = null ]; then
        # A later growth may need more than this seed gives (objective 085's
        # persistent growers): then neither answer has days left.
        expect_eq "$days" null "(1) days left: null, as /api/fleet's"
    else
        near "$days" "$want_days" "$((want_days / 100 + 1))" 2>/dev/null ||
            check_failed "(1) days left [$days]: /api/fleet's days_left [$want_days], within 1%"
    fi
    expect_eq "$(figure storage.archive_growth | jq -c '{unit, value, range, state}')" \
        "{\"unit\":\"bytes_per_day\",\"value\":$growth,\"range\":{\"max\":524288},\"state\":\"out\"}" \
        "(1)(2) archive growth: about 1 MiB a day, out of its range of 512 KiB a day or less"
    expect_eq "$(figure storage.free_bytes | jq -c '{unit, range, state}')" \
        '{"unit":"bytes","range":{"min":1},"state":"in"}' "(1)(2) free disk: in its range of 1 byte or more"
    free=$(figure storage.free_bytes | jq -r '.value')
    near "$free" "$(fleet '.free_bytes')" 1073741824 || check_failed "(1) free disk [$free]: /api/fleet's free_bytes, within 1 GiB"
    ratio_want=$(fleet '.database | if .bytes != null and (.log_bytes // 0) > 0 then .bytes / .log_bytes else null end')
    ratio=$(figure storage.database_ratio | jq -c '.value')
    if [ "$ratio_want" = null ]; then
        expect_eq "$ratio" null "(1) the database against its log, with no log size"
    else
        near "$ratio" "$ratio_want" 0.01 || check_failed "(1) the database against its log [$ratio]: /api/fleet's bytes over log_bytes [$ratio_want]"
        expect_eq "$(figure storage.database_ratio | jq -c '{unit, state}')" '{"unit":"ratio","state":"in"}' "(1)(2) the database ratio: in its range"
    fi
    # <field> <unit> <what>: the figure is null when /api/fleet had none on
    # both sides, else a number between the two sides' largest.
    bracketed() {
        before=$(jq -c "[.health.storage.test_databases[].$1 | numbers] | max" "$WORK/api-before.json")
        after=$(jq -c "[.health.storage.test_databases[].$1 | numbers] | max" "$WORK/api.json")
        got=$(figure "storage.test_database_$1" | jq -c '{unit, value, range, state}')
        value=$(printf '%s' "$got" | jq -c '.value')
        if [ "$before" = null ] && [ "$after" = null ]; then
            expect_eq "$got" "{\"unit\":\"$2\",\"value\":null,\"range\":null,\"state\":\"not_collected\"}" "$3, with no readable test database"
        else
            expect_eq "$(printf '%s' "$got" | jq -c '{unit, range, state}')" "{\"unit\":\"$2\",\"range\":null,\"state\":\"no_range\"}" "$3: no range set"
            lo=$before hi=$after
            [ "$lo" != null ] || lo=$hi
            [ "$hi" != null ] || hi=$lo
            awk -v v="$value" -v a="$lo" -v b="$hi" 'BEGIN { lo = a < b ? a : b; hi = a < b ? b : a; exit !(v != "null" && v + 0 >= lo && v + 0 <= hi) }' ||
                check_failed "$3 [$value]: between /api/fleet's before [$before] and after [$after]"
        fi
    }
    bracketed bytes bytes "(1)(2) the largest test database"
    bracketed schemas count "(1)(2) the most test schemas (its entry has no number, so no range)"
    expect_eq "$(lines '{ranges_file, ranges_error, ignored_ranges}')" \
        "{\"ranges_file\":\"$ranges\",\"ranges_error\":null,\"ignored_ranges\":[{\"key\":\"storage.free_bites\",\"reason\":\"unknown figure\"},{\"key\":\"storage.test_database_schemas\",\"reason\":\"needs a number min or max\"}]}" \
        "(2) the file, no error, and the two ranges left out"
    note "(1) checked: the answer with a ranges file"

    # --- (2) a file that is not JSON, then no file ---------------------------------------
    printf '{"ranges": {"storage.free_bytes": ' >"$ranges"
    code=$(get /api/core-lines "$WORK/lines.json")
    if [ "$code" != 200 ]; then
        check_failed "(2) GET /api/core-lines with a broken file answered $code"
    else
        expect_eq "$(lines '(.ranges_error | type == "string" and length > 0)')" true "(2) a broken file: ranges_error says why"
        expect_eq "$(lines '[.lines[] | ([.north_star] + .guardrails)[] | select(.range != null or .state == "in" or .state == "out")] | length')" 0 \
            "(2) a broken file: no figure has a range"
        expect_eq "$(lines '.ignored_ranges')" '[]' "(2) a broken file: nothing listed as ignored"
    fi
    rm -f "$ranges"
    code=$(get /api/core-lines "$WORK/lines.json")
    if [ "$code" != 200 ]; then
        check_failed "(2) GET /api/core-lines with no file answered $code"
    else
        expect_eq "$(lines '{ranges_error, ignored_ranges, ranged: ([.lines[] | ([.north_star] + .guardrails)[] | select(.range != null)] | length)}')" \
            '{"ranges_error":null,"ignored_ranges":[],"ranged":0}' "(2) no file: no ranges and no error"
        expect_eq "$(figure storage.free_bytes | jq -r '.state')" no_range "(2) no file: free disk has no range set"
    fi
    note "(2) checked: a broken file and no file"
fi

# --- (3) the card's model ------------------------------------------------------------------

[ -s "$WORK/api-lines.json" ] || printf '{}' >"$WORK/api-lines.json"
cat >"$WORK/lines.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { coreLinesView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/coreLines.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api-lines.json`, "utf8"));
const GIB = 1024 ** 3;
const MIB = 1024 ** 2;
const fig = (key, name, unit, value, range, state) => ({ key, name, unit, value, range, state });
const line = (name, north_star, guardrails = []) => ({ line: name, north_star, guardrails });
const synthetic = {
  ranges_file: "/ws/core-lines.json",
  ranges_error: null,
  ignored_ranges: [{ key: "storage.free_bites", reason: "unknown figure" }],
  lines: [
    line("performance", fig("performance.report_to_live", "Time from report to live", "ms", null, null, "not_collected")),
    line("storage", fig("storage.days_left", "Days until the disk is full", "days", 1234, { min: 180 }, "in"), [
      fig("storage.free_bytes", "Free disk", "bytes", 104 * GIB, { min: 40 * GIB }, "in"),
      fig("storage.archive_growth", "Transcript archive growth", "bytes_per_day", MIB, { max: 512 * MIB }, "in"),
      fig("storage.database_ratio", "Database against its log", "ratio", 4.7123, { max: 10 }, "in"),
      fig("storage.test_database_bytes", "Largest test database", "bytes", 3 * GIB, { min: MIB, max: 500 * MIB }, "out"),
      fig("storage.test_database_schemas", "Most test schemas", "count", 1214, null, "no_range"),
    ]),
    line("observability", fig("observability.needs_you_right", "Needs you is right", "share", null, null, "not_collected")),
    line("recoverability", fig("recoverability.restores_passed", "Critical resources with a restore passed in 7 days", "count", null, { min: 6 }, "not_collected")),
  ],
};

test("(3) the rows, in words", () => {
  const view = coreLinesView(synthetic);
  assert.deepEqual(view.rows.map((row) => row.line), ["Performance", "Storage", "Observability", "Recoverability"]);
  assert.deepEqual(view.rows[0].north_star,
    { name: "Time from report to live", value: "not collected yet", range: "no range set", tone: "muted" });
  assert.deepEqual(view.rows[1].north_star, { name: "Days until the disk is full", value: "1,234 days", range: "180 days or more", tone: "plain" });
  assert.deepEqual(view.rows[1].guardrails, [
    { name: "Free disk", value: "104.0 GB", range: "40.0 GB or more", tone: "plain" },
    { name: "Transcript archive growth", value: "1.0 MB a day", range: "512.0 MB a day or less", tone: "plain" },
    { name: "Database against its log", value: "4.7×", range: "10.0× or less", tone: "plain" },
    { name: "Largest test database", value: "3.0 GB", range: "between 1.0 MB and 500.0 MB", tone: "warning" },
    { name: "Most test schemas", value: "1,214", range: "no range set", tone: "muted" },
  ]);
  assert.deepEqual(view.rows[3].north_star,
    { name: "Critical resources with a restore passed in 7 days", value: "not collected yet", range: "6 or more", tone: "muted" });
  assert.deepEqual([view.notice, view.ignored], [null, ["storage.free_bites: unknown figure"]]);
});

test("(3) a broken ranges file is said on the card", () => {
  const view = coreLinesView({ ...synthetic, ranges_error: "expected value at line 1 column 34", ignored_ranges: [] });
  assert.deepEqual([view.notice, view.ignored], ["ranges file unreadable: expected value at line 1 column 34", []]);
});

test("(3) over the answer: four rows, the storage line's marks", () => {
  assert.ok(Array.isArray(answer.lines), "GET /api/core-lines answered");
  const view = coreLinesView(answer);
  assert.equal(view.rows.length, 4);
  assert.equal(view.rows[1].guardrails.length, 5);
  assert.deepEqual(view.rows[1].guardrails.slice(0, 2).map((mark) => mark.tone), ["plain", "warning"], "free disk in, archive growth out");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/lines.test.mjs" >"$WORK/lines.out" 2>&1; then
    check_failed "(3) web/src/coreLines.ts: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/lines.out" | head -n 14 | tr '\n' ' ' | head -c 1200)"
fi
note "(3) checked: the card's model"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "084: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "084: the Core lines answer and card say each figure, its range and whether it is in it"
exit 0
