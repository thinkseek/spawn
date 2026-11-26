#!/bin/sh
# Objective 182 [storage][sustainability]: the owner's core metric set's
# demoted figures, as the lead ruled them on 2026-11-16: free disk is a
# diagnostic with no range (its alert is disk_low's, objective 204; its
# fall is how a sweep's effect is read), leaked test schemas "stays visible
# until it has read 0 for 7 days, then an alert only", and dollars per
# landing is shown under sustainability "as a plain reading, no range".
#
# Required (through `GET /api/core-lines`; the ranges file gives free disk
# {"min": 1e18}, dollars per landing {"max": 1, "target": 0.5}, leaked test
# schemas {"max": 0}):
#   (1) storage.free_bytes is a diagnostic: valued, no range, state
#       no_range, no line (it raises nothing); it is not an alert figure;
#       its file entry is in ignored_ranges with the reason "an alert of its
#       own (disk_low, objective 204)".
#   (2) performance.usd_per_landing is the sustainability line's one
#       reading (`readings`), with no range, no target, and the range_note
#       "no range until use with no price is under 10%"; the other lines
#       have no reading; it is not a diagnostic; its file entry is in
#       ignored_ranges with the reason "shown with no range until use with
#       no price is under 10% (the owner's core metric set)".
#   (3) storage.leaked_test_schemas (the serve's test server is a database
#       of the scenario's own, so it reads 0):
#       (a) with no host.core_lines sample, a diagnostic of line storage;
#       (b) with hourly samples carrying 0 over the whole last week, an
#           alert figure of line storage (the last) and no diagnostic;
#           and a real `spawn loop` started then writes its start-up
#           sample with storage.leaked_test_schemas the number 0 (an alert
#           figure stays sampled, so the week does not end an hour later);
#       (c) one of those samples, three days ago, reading 2: a diagnostic;
#       (d) three hours of samples missing four days ago: a diagnostic
#           (a missing sample is not a zero);
#       (e) every sample 0, but none in the last three hours (the loop
#           stopped): a diagnostic (the week must reach now);
#       (g) every sample 0, but the first only 7 d - 3 h ago (the week
#           begins three hours late): a diagnostic;
#       (h) hourly samples over the week that do not carry the figure,
#           while it reads 0 now: a diagnostic (a sample without it is not a
#           zero);
#       (f) every sample 0 again, and a leaked schema planted (made two
#           days ago, no owner): it reads 1, a diagnostic.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on live main (73cbdd8e, and 6f8ceb8c): free disk is an alert figure, dollars
# per landing a diagnostic, and leaked test schemas never leaves the
# diagnostics.

set -eu

asserted=""
harness() {
    echo "182: harness: $*" >&2
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

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od sed grep curl head git awk; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
[ -d "$WORK" ] || harness "mktemp gave no directory"
trap 'rm -rf "$WORK"' EXIT
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
hc_id=$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')
SPAWN_DB_NAME="bb_182_lines_$run"
SPAWN_HERDR_SESSION="bb-182-$hc_id"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_HERDR_SESSION SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
test_made=""
serve_pid=""
loop_pid=""
# (3b)'s loop, and the private tmux server it would use, are stopped and
# waited on: kill-server returns before its processes end.
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
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
        loop_pid=""
    fi
    stop_herdr
}
stop_serve() {
    [ -n "$serve_pid" ] || return 0
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
}
cleanup() {
    stop_serve
    stop_loop
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
    set +e
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "182: the last answer's lists:" >&2
        jq -c '{readings: [.lines[] | {line, readings: [.readings[]? | .key]}], alert_figures: [.alert_figures[]? | .key], diagnostics: [.diagnostics[]? | {key, line, value}], ignored_ranges}' "$WORK/api.json" >&2 2>/dev/null || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "182: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1
# The serve's test server: a database of the scenario's own, so leaked test
# schemas reads 0 until a schema is planted there.
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
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_CORE_LINES_FILE=$WORK/core-lines.json
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROJECTS SPAWN_CORE_LINES_FILE
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$SPAWN_CLAUDE_PROJECTS" "$WORK/web"
printf '<!doctype html><title>spawn</title>\n' >"$WORK/web/index.html"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

q() { scalar "$1"; }
note() { echo "182: $*" >&2; }
KIB=1024
MIB=1048576
now_ms=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$now_ms" in '' | *[!0-9]*) harness "no time from the database: [$now_ms]" ;; esac

seed() { # <record JSON> <recorded_at_ms>: a control record after every other, as the loop's
    cat >"$WORK/seed.sql" <<'SQL'
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
select 'agent_control', 'session_control', null, 'control', null,
       jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', :'record'::jsonb)),
       coalesce(max(session_seq), 0) + 1, :at
  from spawn_events where session_id = 'session_control';
SQL
    capture psql -d "$SPAWN_DB_NAME" -q -v ON_ERROR_STOP=1 -v record="$1" -v at="$2" -f "$WORK/seed.sql"
    [ "$status" -eq 0 ] || harness "seeding $1 failed: $(err)"
}
answer() { # <what>: GET /api/core-lines into WORK/api.json
    # Emptied here, in the scenario's shell: a redirect in the background
    # serve truncates only in the child process, so the port read below
    # could still see the last serve's port (the 155.sh pattern).
    : >"$WORK/serve.log" || harness "cannot empty serve.log"
    SPAWN_TEST_DATABASE_URL="$server_url/$TEST_DB_NAME" \
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
    [ -n "$port" ] || harness "serve did not start ($1): $(head -c 400 "$WORK/serve.log")"
    code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/core-lines answered $code ($1): $(head -c 200 "$WORK/api.json")"
    stop_serve
}
api() { jq -c "$1" "$WORK/api.json"; }
keys() { api "[.$1[]? | .key]"; }
leaked_where() { # where leaked test schemas is: "diagnostic <line> <value>", "alert <line> <value> last|not-last", or "nowhere"
    api '. as $a
        | ([$a.diagnostics[]? | select(.key == "storage.leaked_test_schemas")] | first) as $d
        | ([$a.alert_figures[]? | select(.key == "storage.leaked_test_schemas")] | first) as $f
        | if $d != null and $f == null then "diagnostic \($d.line) \($d.value)"
          elif $f != null and $d == null then
            "alert \($f.line) \($f.value) \(if ($a.alert_figures | last | .key) == "storage.leaked_test_schemas" then "last" else "not-last" end)"
          elif $d == null and $f == null then "nowhere" else "both" end' | tr -d '"'
}

cat >"$SPAWN_CORE_LINES_FILE" <<'JSON'
{"ranges": {
   "storage.free_bytes": {"min": 1e18},
   "performance.usd_per_landing": {"max": 1, "target": 0.5},
   "storage.leaked_test_schemas": {"max": 0}}}
JSON
HOUR=3600000
DAY=86400000

# --- (1) free disk, a diagnostic --------------------------------------------------------

answer "the first answer"
expect_eq "$(api '[.diagnostics[]? | select(.key == "storage.free_bytes") | {valued: (.value | type == "number"), range, state, lined: has("line")}]')" \
    '[{"valued":true,"range":null,"state":"no_range","lined":false}]' \
    "(1) free disk among the diagnostics: valued, no range, no line"
expect_eq "$(api '[.alert_figures[]? | select(.key == "storage.free_bytes")] | length')" 0 "(1) free disk is not an alert figure"
expect_eq "$(api '[.ignored_ranges[] | select(.key == "storage.free_bytes") | .reason]')" \
    '["an alert of its own (disk_low, objective 204)"]' "(1) free disk's range is ignored, said why"
note "(1) checked: free disk"

# --- (2) dollars per landing, a reading of the sustainability line -------------------------

expect_eq "$(api '[.lines[] | {line, readings: [.readings[]? | .key]}]')" \
    '[{"line":"storage","readings":[]},{"line":"performance","readings":[]},{"line":"sustainability","readings":["performance.usd_per_landing"]}]' \
    "(2) the lines' readings"
expect_eq "$(api '[.lines[] | .readings[]? | {range, target: (.target // null), range_note}]')" \
    '[{"range":null,"target":null,"range_note":"no range until use with no price is under 10%"}]' \
    "(2) dollars per landing: no range, no target, its note"
expect_eq "$(api '[.diagnostics[]?, .alert_figures[]? | select(.key == "performance.usd_per_landing")] | length')" 0 \
    "(2) dollars per landing is neither a diagnostic nor an alert figure"
expect_eq "$(api '[.ignored_ranges[] | select(.key == "performance.usd_per_landing") | .reason]')" \
    "[\"shown with no range until use with no price is under 10% (the owner's core metric set)\"]" \
    "(2) dollars per landing's range is ignored, said why"
note "(2) checked: dollars per landing"

# --- (3) leaked test schemas: visible until a week at 0 ---------------------------------------

expect_eq "$(leaked_where)" "diagnostic storage 0" "(3a) leaked test schemas with no sample"

samples() { # hourly host.core_lines samples, leaked test schemas 0 (or the values <4th>), from <first ms ago> down to now, but those in [<gap from> <gap to>] ms ago
    cat >"$WORK/samples.sql" <<'SQL'
delete from spawn_events
 where session_id = 'session_control' and kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines';
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
select 'agent_control', 'session_control', null, 'control', null,
       jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record',
         jsonb_build_object('type', 'host.core_lines', 'values', :'values'::jsonb))),
       (select coalesce(max(session_seq), 0) from spawn_events where session_id = 'session_control') + row_number() over (order by g),
       :now - g
  from generate_series(:first::bigint, 0, -:hour::bigint) g
 where not (g between :gap_from::bigint and :gap_to::bigint);
SQL
    values=${4-}
    [ -n "$values" ] || values='{"storage.leaked_test_schemas": 0}'
    capture psql -d "$SPAWN_DB_NAME" -q -v ON_ERROR_STOP=1 -v now="$now_ms" -v first="$1" -v hour="$HOUR" \
        -v gap_from="$2" -v gap_to="$3" -v values="$values" -f "$WORK/samples.sql"
    [ "$status" -eq 0 ] || harness "seeding the samples failed: $(err)"
}
# A week and a bit of hourly samples at 0, none missing (the gap is in the future).
week=$((7 * DAY + 2 * HOUR))
samples "$week" -2 -1
[ "$(q "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'")" -ge 170 ] ||
    harness "fewer than 170 samples were seeded"
answer "a week of samples at 0"
expect_eq "$(leaked_where)" "alert storage 0 last" "(3b) leaked test schemas after a week at 0"

# The hourly sample the loop writes must keep carrying it once it is an
# alert figure: a real `spawn loop` writes one at its start. Its world: a
# repository, an engine program it never starts (no mission), a private
# tmux server, the serve's test database.
git init -q --bare "$WORK/repo.git" || harness "git init failed"
git init -q "$WORK/seed" && printf 'base\n' >"$WORK/seed/README.md" &&
    git -C "$WORK/seed" add -A &&
    git -C "$WORK/seed" -c user.name=bb -c user.email=bb@localhost -c commit.gpgsign=false commit -qm base &&
    git -C "$WORK/seed" push -q "$WORK/repo.git" HEAD:main || harness "seeding the repository failed"
printf '#!/bin/sh\nexit 1\n' >"$WORK/claude"
chmod 755 "$WORK/claude"
before=$(q "select coalesce(max(id), 0) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'")
env -u SPAWN_SUPERVISOR -u SPAWN_DISPATCH -u SPAWN_GATE -u SPAWN_MODEL_RELAY -u SPAWN_JUDGE_URL \
    -u SPAWN_JUDGE_MODEL -u SPAWN_PROGRAM -u CLAUDE_CONFIG_DIR -u SPAWN_TERMINAL_BACKEND \
    -u SPAWN_HERDR_MODE -u SPAWN_HERDR_SHARDS -u SPAWN_HERDR_PROGRAM -u SPAWN_TMUX_SOCKET \
    -u SPAWN_TMUX_PROGRAM -u TMUX -u TMUX_PANE -u TMUX_TMPDIR \
    BB_PATH="$PATH" SPAWN_GIT_REPO="$WORK/repo.git" SPAWN_CLAUDE_PROGRAM="$WORK/claude" \
    SPAWN_HERDR_CORE_PROGRAM="$SPAWN_HC" SPAWN_WORKER=worker-bb SPAWN_LOOP_INTERVAL_MS=300 \
    SPAWN_MISSION_FILE="$WORK/mission.md" SPAWN_DISK_WARN_GB=0 SPAWN_DISK_FLOOR_GB=0 \
    SPAWN_TEST_DATABASE_URL="$server_url/$TEST_DB_NAME" \
    "$SPAWN_BIN" loop --repo "$WORK/repo.git" --root "$SPAWN_WORKSPACE_ROOT" >"$WORK/loop.log" 2>&1 &
loop_pid=$!
i=0
until grep -q 'core lines: sampled' "$WORK/loop.log" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -lt 240 ] || harness "the loop wrote no core lines sample within 60 s: $(tail -n 5 "$WORK/loop.log" | tr '\n' ' ' | cut -c1-400)"
    kill -0 "$loop_pid" 2>/dev/null || harness "the loop ended: $(tail -n 5 "$WORK/loop.log" | tr '\n' ' ' | cut -c1-400)"
    sleep 0.25
done
stop_loop
expect_eq "$(q "select coalesce(jsonb_typeof(v) || ' ' || v::text, 'absent')
    from (select kind -> 'detail' -> 'record' -> 'values' -> 'storage.leaked_test_schemas' v from spawn_events
           where kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines' and id > $before
           order by id desc limit 1) s")" "number 0" \
    "(3b) storage.leaked_test_schemas in the sample the loop wrote while it is an alert figure"

q "update spawn_events set kind = jsonb_set(kind, '{detail,record,values,storage.leaked_test_schemas}', '2')
    where id = (select id from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'
                order by abs(recorded_at_ms - ($now_ms - 3 * $DAY)) limit 1) returning id" >/dev/null ||
    harness "cannot set a sample to 2"
answer "one sample three days ago at 2"
expect_eq "$(leaked_where)" "diagnostic storage 0" "(3c) leaked test schemas with one sample in the week at 2"

samples "$week" $((4 * DAY)) $((4 * DAY + 2 * HOUR))
answer "three hours missing four days ago"
expect_eq "$(leaked_where)" "diagnostic storage 0" "(3d) leaked test schemas with three hours of samples missing"

samples "$week" 0 $((3 * HOUR))
answer "no sample in the last three hours"
expect_eq "$(leaked_where)" "diagnostic storage 0" "(3e) leaked test schemas with no sample in the last three hours"

samples $((7 * DAY - 3 * HOUR)) -2 -1
answer "the week's samples begin three hours late"
expect_eq "$(leaked_where)" "diagnostic storage 0" "(3g) leaked test schemas with the week's first sample three hours late"

samples "$week" -2 -1 '{"storage.free_bytes": 1}'
answer "a week of samples without the figure"
expect_eq "$(leaked_where)" "diagnostic storage 0" "(3h) leaked test schemas with a week of samples that do not carry it"

samples "$week" -2 -1
psql -d "$TEST_DB_NAME" -q -v ON_ERROR_STOP=1 -c "create schema bb182_leftover" \
    -c "comment on schema bb182_leftover is '{\"made_at_ms\": $((now_ms - 2 * DAY))}'" >/dev/null 2>&1 ||
    harness "cannot plant a leaked schema"
answer "a leaked schema planted"
expect_eq "$(leaked_where)" "diagnostic storage 1" "(3f) leaked test schemas reading 1 after a week at 0"
note "(3) checked: leaked test schemas"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "182: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "182: free disk is a diagnostic, dollars per landing a plain reading of sustainability, leaked test schemas an alert only after a week at 0"
exit 0
