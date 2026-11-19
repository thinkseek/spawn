#!/bin/sh
# Objective 230 [observability]: when free disk gains, Core lines says so.
# The Fleet page's storage diagnostics (web/src/coreLines.ts, read by the
# Core lines card and the health strip) said "Free disk lost a day" beside
# "-2662.8 MB a day" when storage.disk_fall is below zero, and "Store that
# grew most: unattributed" beside a negative size when every store shrank.
#
# Required (a real `spawn serve` on its own database, hourly host.core_lines
# samples planted as a loop records them against the free disk F the serve
# reads, as 222.sh and 225.sh plant them; each answer of GET /api/core-lines
# read through web/src/coreLines.ts's coreLinesView under node, as the page
# does):
#   (1) a week gaining 5 GB a day up to F - 20 GB a day ago, then gaining 20 GB
#       through the last day (the answer: disk_fall below zero, basis "fit";
#       grew_most below zero, unattributed): "Free disk gained a day (the
#       week's fit)" with the gain as a positive size (the value's own words
#       for |value|, no minus); "Store that shrank least: unattributed" with
#       |value|;
#   (2) a week gaining 20 GB a day up to F - 5 GB a day ago, then gaining 5 GB
#       (disk_fall below zero, basis "day"): "Free disk gained a day (over
#       the last day)", |value|; the store that shrank least, |value|;
#   (3) a week falling 10 GB a day, then a day losing 10 GB (both figures
#       above zero): "Free disk lost a day (the week's fit)" or "(the last
#       day's fall)" by the answer's basis, and "Store that grew most:
#       unattributed", each value in the words it had before 230;
#   (4) in each, no other figure says "gained" or "shrank least".
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on live main (ca482ad5): (1) and (2) say "lost" with a
# negative size, and "grew most" with a negative size.

set -eu

asserted=""
harness() {
    echo "230: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od sed grep curl head; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
[ -d "$WORK" ] || harness "mktemp gave no directory"
trap 'rm -rf "$WORK"' EXIT
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_230_gained_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
test_made=""
serve_pid=""
stop_serve() {
    [ -n "$serve_pid" ] || return 0
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
}
cleanup() {
    stop_serve
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    if [ -n "$test_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$TEST_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "230: the last answer's disk figures:" >&2
        jq -c '[.diagnostics[]? | select(.key == "storage.disk_fall" or .key == "storage.free_bytes")]' "$WORK/api.json" >&2 2>/dev/null || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "230: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1
# The serve's test server: a database of the scenario's own.
TEST_DB_NAME="${SPAWN_DB_NAME}_t"
psql -d postgres -tA -c "create database $TEST_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario's test database"
test_made=1

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

for name in SPAWN_SERVE_DATABASE_URL SPAWN_SESSION_BACKUP_STATUS SPAWN_FLEET_TEST_DATABASE_URL \
    SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
export HOME
mkdir -p "$HOME"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_CORE_LINES_FILE=$WORK/core-lines.json
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROJECTS SPAWN_CORE_LINES_FILE
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$SPAWN_CLAUDE_PROJECTS" "$WORK/web"
printf '<!doctype html><title>spawn</title>\n' >"$WORK/web/index.html"
printf '{"ranges": {}}\n' >"$SPAWN_CORE_LINES_FILE"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

q() { scalar "$1"; }
note() { echo "230: $*" >&2; }
HOUR=3600000
DAY=$((24 * HOUR))
GB=1000000000

answer() { # <what>: GET /api/core-lines into WORK/api.json
    # The last serve's log goes first: the redirection below truncates it in
    # the child, so a read right after the fork could see the old port.
    rm -f "$WORK/serve.log" "$WORK/api.json"
    SPAWN_TEST_DATABASE_URL="$server_url/$TEST_DB_NAME" \
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
    [ -n "$port" ] || harness "serve did not start ($1): $(head -c 400 "$WORK/serve.log")"
    code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/core-lines answered $code ($1): $(head -c 200 "$WORK/api.json")"
    stop_serve
}
# The disk_fall diagnostic: "<value> <basis>", "none" for a missing field.
fall_said() {
    jq -r '[.diagnostics[]? | select(.key == "storage.disk_fall")] | first
        | if . == null then "missing" else "\(.value // "none") \(.basis // "none")" end' "$WORK/api.json"
}
free_now() {
    jq -r '[.diagnostics[]? | select(.key == "storage.free_bytes")] | first | .value // empty' "$WORK/api.json"
}

# samples <F> <a> <b> <newest ms ago> [<gap from> <gap to>]: hourly host.core_lines samples from 7 d
# + 2 h ago down to <newest ms ago>, the free disk of one g ms ago being
# F + a * g / 24 h through the last day (g < 24 h) and F + a + b * (g - 24 h)
# / 24 h before it (bytes; a and b in GB); none in [gap from, gap to] ms ago.
samples() {
    cat >"$WORK/samples.sql" <<'SQL'
delete from spawn_events
 where session_id = 'session_control' and kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines';
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
select 'agent_control', 'session_control', null, 'control', null,
       jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record',
         jsonb_build_object('type', 'host.core_lines', 'values', jsonb_build_object('storage.free_bytes',
           case when g < :day::bigint
                then :free::numeric + :a::numeric * :gb::numeric * g / :day::numeric
                else :free::numeric + :a::numeric * :gb::numeric
                     + :b::numeric * :gb::numeric * (g - :day::bigint) / :day::numeric end)))),
       (select coalesce(max(session_seq), 0) from spawn_events where session_id = 'session_control') + row_number() over (order by g),
       :now::bigint - g
  from generate_series((7 * :day::bigint + 2 * :hour::bigint), :newest::bigint, -:hour::bigint) g
 where not (g between :gap_from::bigint and :gap_to::bigint);
SQL
    now_ms=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
    case "$now_ms" in '' | *[!0-9]*) harness "no time from the database: [$now_ms]" ;; esac
    capture psql -d "$SPAWN_DB_NAME" -q -v ON_ERROR_STOP=1 -v now="$now_ms" -v free="$1" -v a="$2" -v b="$3" \
        -v newest="$4" -v gap_from="${5:--2}" -v gap_to="${6:--1}" -v day="$DAY" -v hour="$HOUR" -v gb="$GB" -f "$WORK/samples.sql"
    [ "$status" -eq 0 ] || harness "seeding the samples failed: $(err)"
}

tree=$PWD
[ -f "$tree/web/src/coreLines.ts" ] || harness "no web/src/coreLines.ts under $tree (run from the tree under test)"
command -v node >/dev/null 2>&1 || harness "node is not installed"
# said <key>: the answer's diagnostic "<value> <basis>", "none" for a missing field
said() {
    jq -r --arg k "$1" '[.diagnostics[]? | select(.key == $k)] | first
        | if . == null then "missing" else "\(.value // "none") \(.basis // "none")" end' "$WORK/api.json"
}
# world <name> <want disk_fall sign: neg|pos> <want basis|any>: the answer's
# figures as the world plants them (222's rules), else a harness fault
world() {
    set -- "$1" "$2" "$3" $(said storage.disk_fall) $(said storage.grew_most)
    case "$2:$4" in neg:-[0-9]* | pos:[1-9]*) ;; *) harness "($1) storage.disk_fall is [$4], not $2 (222)" ;; esac
    case "$2:$6" in neg:-[0-9]* | pos:[1-9]*) ;; *) harness "($1) storage.grew_most is [$6], not $2 (222 R2: the fit)" ;; esac
    [ "$3" = any ] || [ "$5" = "$3" ] || harness "($1) storage.disk_fall's basis is [$5], not $3 (222)"
    cp "$WORK/api.json" "$WORK/$1.json"
}

answer "no sample"
F=$(free_now)
case "$F" in '' | *[!0-9]*) harness "the serve reads no free disk: [$F]" ;; esac

# --- (1) a week gaining 5 GB a day, a day gaining 20 GB: the fit's, below zero ----------------
samples "$F" -20 -5 "$HOUR"
answer "a week gaining, a day gaining more"
world fitgain neg fit

# --- (2) a week gaining 20 GB a day, a day gaining 5 GB: the day's, below zero ----------------
F=$(free_now)
samples "$F" -5 -20 "$HOUR"
answer "a week gaining, a day gaining less"
world daygain neg day

# --- (3) a week and a day falling 10 GB a day: above zero, as before ---------------------------
F=$(free_now)
samples "$F" 10 10 "$HOUR"
answer "a week and a day falling"
world lost pos any

# --- the page's words, as coreLinesView says them ------------------------------------------------
cat >"$WORK/gained.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
const { coreLinesView, unitWords } = await import(`${process.env.BB_TREE}/web/src/coreLines.ts`);
const answer = (name) => JSON.parse(readFileSync(`${process.env.BB_WORK}/${name}.json`, "utf8"));
const raw = (a, key) => a.diagnostics.find((d) => d.key === key);
const marks = (v) => [...v.rows.flatMap((row) => [row.north_star, ...row.guardrails, ...(row.readings ?? [])]), ...v.diagnostics];
// The diagnostic's mark: the one at the raw figure's place in the diagnostics.
const mark = (a, key) => coreLinesView(a).diagnostics[a.diagnostics.findIndex((d) => d.key === key)];
const words = (v) => unitWords("bytes_per_day", v);

for (const [name, basisWords] of [["fitgain", " (the week's fit)"], ["daygain", " (over the last day)"]]) {
  test(`(${name === "fitgain" ? 1 : 2}) a disk that gained: gained, the size positive`, () => {
    const a = answer(name);
    const fall = raw(a, "storage.disk_fall");
    const m = mark(a, "storage.disk_fall");
    assert.equal(m.name, `Free disk gained a day${basisWords}`);
    assert.equal(m.value, words(-fall.value));
    assert.doesNotMatch(m.value, /-/);
    const grew = raw(a, "storage.grew_most");
    const g = mark(a, "storage.grew_most");
    assert.equal(g.name, `Store that shrank least: ${grew.store}`);
    assert.equal(g.value, words(-grew.value));
    assert.doesNotMatch(g.value, /-/);
  });
}
test("(3) a disk that lost: the words as before 230", () => {
  const a = answer("lost");
  const fall = raw(a, "storage.disk_fall");
  const m = mark(a, "storage.disk_fall");
  assert.equal(m.name, `Free disk lost a day${fall.basis === "fit" ? " (the week's fit)" : " (the last day's fall)"}`);
  assert.equal(m.value, words(fall.value));
  const grew = raw(a, "storage.grew_most");
  const g = mark(a, "storage.grew_most");
  assert.equal(g.name, `Store that grew most: ${grew.store}`);
  assert.equal(g.value, words(grew.value));
});
// Each raw figure with its mark, as coreLinesView maps them: each line's
// north star, guardrails and readings to its row, the diagnostics in order.
const pairs = (a) => {
  const v = coreLinesView(a);
  const out = [];
  (a.lines ?? []).forEach((line, i) => {
    const row = v.rows[i];
    out.push([line.north_star, row.north_star]);
    (line.guardrails ?? []).forEach((g, j) => out.push([g, row.guardrails[j]]));
    (line.readings ?? []).forEach((r, j) => out.push([r, row.readings[j]]));
  });
  (a.diagnostics ?? []).forEach((d, i) => out.push([d, v.diagnostics[i]]));
  return out;
};
test("(4) no other figure says gained or shrank least", () => {
  for (const name of ["fitgain", "daygain", "lost"]) {
    const said = pairs(answer(name))
      .filter(([f, m]) => !(["storage.disk_fall", "storage.grew_most"].includes(f?.key) && typeof f?.value === "number" && f.value < 0))
      .filter(([, m]) => /gained|shrank least/.test(`${m.name} ${m.value}`))
      .map(([f, m]) => `${f?.key}: ${m.name}`);
    assert.deepEqual(said, [], name);
  }
});
TEST
if BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/gained.test.mjs" >"$WORK/gained.out" 2>&1; then
    note "(1)-(4) checked: the page's words"
else
    # node's reporter: "ℹ fail <n>" (spec) or "# fail <n>" (tap); neither means node did not run them
    grep -q '^ℹ fail [1-9]\|^# fail [1-9]' "$WORK/gained.out" || harness "node did not run the checks: $(head -c 600 "$WORK/gained.out")"
    grep -E '^not ok|^✖ \(|AssertionError|expected|actual' "$WORK/gained.out" | sort -u | head -n 16 | while IFS= read -r line; do
        check_failed "web/src/coreLines.ts: $line"
    done
    [ -s "$WORK/failures" ] || check_failed "web/src/coreLines.ts: $(tail -c 600 "$WORK/gained.out")"
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "230: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "230: Core lines says gained and shrank least when the disk gains, the size positive"
exit 0
