#!/bin/sh
# Black box: the page says what is using the disk, how fast the transcript
# archive grows, and how long the disk lasts at that rate.
#
# The problem, on the live host (sync/roadmap-effects.md, "Core tracks:
# storage": "the owner can see on the page what is using the disk and how
# long it lasts"; the owner's 01:00 order that one of 064-066 be a storage
# objective about showing growth). The transcript archive is the only
# steady grower (about 0.12 GB a day, nothing prunes it, and the owner has
# ruled that nothing will), the live database had bloated to 2.7 GB for
# about 17 MB of data before it was compacted by hand, and each was found by
# a person with `du` and psql. The page shows none of it: `/api/fleet`'s
# health says only the free space of the workspace root and TMPDIR
# (crates/spawn-cli/src/disk.rs `report`), and Home's host section the same
# (web/src/fleet.ts `disk`, web/src/resources.ts).
#
# Required (read through `GET /api/fleet` and web/src/resources.ts under
# node over it):
#   (1) `health.storage`, read at the request as `disk` is:
#       - `free_bytes`: the free space of the workspace root's file system;
#       - `archive`: {path (<root>/.transcripts), bytes, files, week_bytes}:
#         the regular files under it, their apparent sizes summed, and the
#         sizes of those whose last change is under 7 days old;
#       - `workspaces`: {path (<root>), bytes}: the regular files under the
#         root outside .transcripts and .transcripts-progress;
#       - `database`: {bytes, tables}: pg_database_size of the store's
#         database, and its five largest tables by pg_total_relation_size,
#         [{name, bytes}], largest first;
#       - `growth_bytes_per_day`: the archive's `week_bytes` over 7;
#       - `days_left`: free_bytes over growth_bytes_per_day, rounded down;
#         null when the archive did not grow.
#   (2) resources.ts `storageView(storage)` gives the card's words: `rows`
#       [{name, size, note}] for "Transcript archive" (note "grows about
#       <size> a day"), "Workspaces" and "Database" (note "largest: <table>
#       <size>"); `lasts`: "about <days> days at the archive's growth"
#       (days grouped en-US), null when days_left is null. A size reads "<n>
#       MB" to one decimal under 1024 MB (1 MB = 1024 * 1024 bytes), "<n>
#       GB" above.
#
# `spawn serve` answers on a scratch database with a scratch workspace root:
#   archive: agent_a/a1.jsonl 3 MiB changed 1 day ago, agent_a/a2.jsonl
#     4 MiB 3 days ago, agent_b/b1.jsonl 5 MiB 10 days ago: 12 MiB, 7 MiB
#     in the last week, so 1 MiB a day;
#   workspaces: agent_c/big.bin 2 MiB.
# The free space and the database are compared with `df` and psql read at
# the same time, within 1% (or 64 MiB of free space). Each check is
# reported; the run fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, no tmux, no
# network but loopback. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "065: harness: $*" >&2
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
for tool in psql jq od curl sed grep cut node df touch head find; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_065_storage_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

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
        echo "065: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "065: harness: unexpected exit $rc" >&2
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


note() { echo "065: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# --- the workspace root and the archive -------------------------------------------------

MIB=1048576
root=$SPAWN_WORKSPACE_ROOT
mkdir -p "$root/.transcripts/agent_a" "$root/.transcripts/agent_b" "$root/agent_c"
blob() { # <path> <MiB> <age>: a file of that size whose last change was <age> ago
    head -c $(($2 * MIB)) /dev/zero >"$1"
    touch -d "$3 ago" "$1"
}
blob "$root/.transcripts/agent_a/a1.jsonl" 3 '1 day'
blob "$root/.transcripts/agent_a/a2.jsonl" 4 '3 days'
blob "$root/.transcripts/agent_b/b1.jsonl" 5 '10 days'
blob "$root/agent_c/big.bin" 2 '2 hours'

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
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/fleet" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/fleet answered $code: $(head -c 300 "$WORK/api.json")"
# What the host and the store say, read right after the answer.
df_free=$(df -B1 --output=avail "$root" | tail -n 1 | tr -d ' ')
db_bytes=$(scalar "select pg_database_size(current_database())")
db_tables=$(scalar "select json_agg(c.relname order by pg_total_relation_size(c.oid) desc, c.relname)
                      from (select c.oid, c.relname from pg_class c join pg_namespace n on n.oid = c.relnamespace
                             where c.relkind in ('r', 'p') and n.nspname = 'public'
                             order by pg_total_relation_size(c.oid) desc, c.relname limit 5) c")
db_tables=$(printf '%s' "$db_tables" | jq -c .) || harness "psql's table list is not JSON"
case "$df_free$db_bytes" in '' | *[!0-9]*) harness "df or psql read nothing: [$df_free] [$db_bytes]" ;; esac
storage() { jq -c ".health.storage | $1" "$WORK/api.json"; }
near() { # <got> <want> <slack>: |got - want| <= slack
    [ -n "$1" ] && [ "$1" != null ] && awk -v g="$1" -v w="$2" -v s="$3" 'BEGIN { d = g - w; if (d < 0) d = -d; exit !(d <= s) }'
}

# --- (1) the answer --------------------------------------------------------------------

expect_eq "$(storage '.archive | {path, bytes, files, week_bytes}')" \
    "{\"path\":\"$root/.transcripts\",\"bytes\":$((12 * MIB)),\"files\":3,\"week_bytes\":$((7 * MIB))}" \
    "(1) the archive: 12 MiB in 3 files, 7 MiB of them changed in the last 7 days"
expect_eq "$(storage '.workspaces | {path, bytes}')" "{\"path\":\"$root\",\"bytes\":$((2 * MIB))}" \
    "(1) the workspaces: the 2 MiB under the root outside the archive"
expect_eq "$(storage '.growth_bytes_per_day')" "$((7 * MIB / 7))" \
    "(1) the archive's growth: 7 MiB in the last week over 7 days"
free=$(storage '.free_bytes')
slack=$((df_free / 100 > 64 * MIB ? df_free / 100 : 64 * MIB))
near "$free" "$df_free" "$slack" || check_failed "(1) free_bytes [$free], against df's [$df_free] (slack $slack)"
days=$(storage '.days_left')
[ "$free" != null ] && expect_eq "$days" "$((free / MIB))" "(1) days_left: the answer's free_bytes over 1 MiB a day, rounded down"
got_db=$(storage '.database.bytes')
near "$got_db" "$db_bytes" "$((db_bytes / 100))" || check_failed "(1) database.bytes [$got_db], against pg_database_size [$db_bytes]"
expect_eq "$(storage '[.database.tables[]?.name]')" "$db_tables" \
    "(1) the database's five largest tables, largest first"
expect_eq "$(storage '[.database.tables[]?.bytes | type] | unique')" '["number"]' \
    "(1) each of the five carries its bytes"
note "(1) checked: the answer"

# --- (2) the card's words --------------------------------------------------------------------

cat >"$WORK/storage.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { storageView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/resources.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const storage = answer.health.storage;

test("(2) the rows: what uses the disk", () => {
  const { rows } = storageView(storage);
  assert.deepEqual(rows.map((row) => row.name), ["Transcript archive", "Workspaces", "Database"]);
  assert.deepEqual([rows[0].size, rows[0].note], ["12.0 MB", "grows about 1.0 MB a day"]);
  assert.equal(rows[1].size, "2.0 MB");
  assert.match(rows[2].size, /^\d+\.\d (MB|GB)$/);
  assert.match(rows[2].note, new RegExp(`^largest: ${storage.database.tables[0].name} \\d+\\.\\d (MB|GB)$`));
});

test("(2) how long the disk lasts", () => {
  const days = storage.days_left;
  assert.equal(storageView(storage).lasts, `about ${days.toLocaleString("en-US")} days at the archive's growth`);
  assert.equal(storageView({ ...storage, growth_bytes_per_day: 0, days_left: null }).lasts, null);
  assert.equal(storageView({ ...storage, archive: { ...storage.archive, bytes: 3 * 1024 ** 3 } }).rows[0].size, "3.0 GB");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/storage.test.mjs" >"$WORK/storage.out" 2>&1; then
    check_failed "(2) web/src/resources.ts storageView over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/storage.out" | head -n 10 | tr '\n' ' ' | head -c 900)"
fi
note "(2) checked: the card's words"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "065: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "065: the page says what uses the disk, how fast the archive grows and how long the disk lasts"
exit 0
