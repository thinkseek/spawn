#!/bin/sh
# Objective 156 [observability]: the owner's interventions through the
# queue count. observability.interventions_per_agent_hour read 0.0 on
# 11-14 over a day with 46 lead-mac posts in sync/queue.md (26 the owner's
# decisions, 20 lead-mac's steering): the figure counted only what the owner
# did through Spawn (commands, resumes, owner tasks, cancel-and-reassign,
# pauses), and nearly all of it went through the queue instead.
#
# Required (the objective's prompt): when the ranges file
# (SPAWN_CORE_LINES_FILE, by default <workspace root>/core-lines.json) names
# `observability.queue_file`, `/api/interventions` adds each `## lead-mac
# HH:MM — <text>` heading of that file in its window as one intervention:
# kind `owner_decision` when the text holds the word "owner" (any case),
# `steering` otherwise, with `source: "queue"`, the text with control
# characters dropped and cut at 120 characters (then "…"). Nobody else's
# heading counts: not the lead's ("Owner decision:", "owner gate PENDING"
# and "Owner question" included), not a reviewer's. A heading's day comes
# from the clock wrapping past midnight (a heading more than 12 h later than
# the one after it; minutes out of order are the same day), read back from
# the end of the file,
# in the local time `queue_utc_offset_minutes` east of UTC. The figure and
# Core lines' guardrail count them. A changed file is read again. Without
# `queue_file`, the answer is as before.
#
# Black box, through `spawn serve` on a seeded history: one execution from
# 10 h to 2 h ago (8 agent-hours in the day), a mission resumed 5 h ago, and
# a scratch queue file whose headings are stamped by the wall clock 8 h east
# of UTC (offset 480), oldest first, at hours before now:
#   -30  lead-mac  OWNER DECISIONS: older than the day
#   -29  lead      owner gate PENDING
#   -20  lead-mac  [recoverability] Leak check done
#   -12  rev-b     a reviewer's post
#   -6   lead      Owner question: an ask, not counted
#   -5   lead-mac  OWNER DECISIONS (by the owner): no second host
#   -4   lead      Owner decision: relayed, not counted
#   -3   lead-mac  F4 PASSES
#   -3h20 lead     a post stamped 20 minutes before the one above it, as
#                  live has (rev-f F1): the same day, not a midnight
#   -1   lead-mac  a hostile heading: an escape character, HTML, the word
#                  "owner", 200 more characters
# Checked:
#   (1) /api/interventions?window=24h: total 5, by_kind owner_decision 2,
#       steering 2, resume 1, the Spawn kinds 0, per_agent_hour 0.625 (5
#       over 8 agent-hours); its queue items, newest first, are lead-mac's
#       four of the day with their kinds and lines, principal lead-mac; all
#       items newest first.
#   (2) /api/interventions?window=7d: the post 30 h ago counts too
#       (owner_decision 3, total 6). A day ending 2 days ago holds none of
#       the posts and says the queue's kinds at 0.
#   (3) Core lines' observability.interventions_per_agent_hour is 0.625.
#   (4) The hostile heading's line is its text with the escape character
#       dropped, HTML as text, cut at 120 characters then "…".
#   (5) A post appended to the file counts at the next answer (steering 3).
#   (6) With the ranges file naming no queue, the answer is as before: total
#       1, by_kind exactly command, resume, owner_task, cancel_reassign.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 at (1), (2), (3), (4) and (5) on the base (148's throwaway on
# ede0d676): no queue post is counted (total 1, per_agent_hour 0.125).

set -eu

asserted=""
harness() {
    echo "156: harness: $*" >&2
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
for tool in psql jq od curl sed grep cut date; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_156_queue_$run"
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
    set +e
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "156: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "156: harness: unexpected exit $rc" >&2
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


note() { echo "156: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
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
-- Agent time as the Bridge records it (049, 100): 8 agent-hours in the day.
create function pg_temp.bridge(h numeric, name text, detail jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_worker_w_1', 'session_worker_w_1', 'exec_bb_156_w', 'bridge', null,
            jsonb_build_object('name', name, 'detail', detail), nextval('seed_seq'), pg_temp.ms(h)) \$\$;
select pg_temp.bridge(-10, 'execution.started', '{"engine":"claude","workspace":"/w/a"}');
select pg_temp.bridge(-2, 'execution.ended', '{"cause":"exited","exit_code":0}');
select pg_temp.rec(-5, '{"type":"mission.resumed","id":1}');
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"

# The queue, stamped as qpost stamps it, by the wall clock, here 8 h east of
# UTC.
offset=480
now_s=$(date +%s)
clock() { # <hours before now>: HH:MM in the queue's local time
    date -u -d "@$((now_s - $1 * 3600 + offset * 60))" +%H:%M
}
clockm() { # <minutes before now>: the same, to the minute
    date -u -d "@$((now_s - $1 * 60 + offset * 60))" +%H:%M
}
queue=$WORK/sync/queue.md
mkdir -p "$WORK/sync"
long=$(printf 'y%.0s' $(seq 1 200))
esc=$(printf '\033')
{
    printf '# The queue\n\nA line that is no heading.\n\n'
    printf '## lead-mac %s — OWNER DECISIONS: older than the day\n\nbody\n\n' "$(clock 30)"
    printf '## lead %s — owner gate PENDING fd0e3296\n\n' "$(clock 29)"
    printf '## lead-mac %s — [recoverability] Leak check done\n\n' "$(clock 20)"
    printf '## rev-b %s — a reviewer'"'"'s post\n\n' "$(clock 12)"
    printf '## lead %s — Owner question: an ask, not counted\n\n' "$(clock 6)"
    printf '## lead-mac %s — OWNER DECISIONS (by the owner): no second host\n\n' "$(clock 5)"
    printf '## lead %s — Owner decision: relayed, not counted\n\n' "$(clock 4)"
    printf '## lead-mac %s — F4 PASSES\n\n- 00:10 a list line, no heading\n\n' "$(clock 3)"
    printf '## lead %s — stamped 20 minutes before the post above\n\n' "$(clockm 200)"
    printf '## lead-mac %s — x%s[31m <b>Decisions</b> & "the owner" %s\n' "$(clock 1)" "$esc" "$long"
} >"$queue"
ranges_with_queue() {
    cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<JSON
{"ranges": {"observability.interventions_per_agent_hour": {"max": 0.05}},
 "observability": {"queue_file": "$queue", "queue_utc_offset_minutes": $offset}}
JSON
}
ranges_with_queue

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
    bb_http "$2" --noproxy '*' "http://127.0.0.1:$port$1"
}
answer() { # <window> <file>
    code=$(get "/api/interventions?window=$1" "$2")
    [ "$code" = 200 ] || harness "GET /api/interventions?window=$1 answered $code: $(head -c 200 "$2")"
    cp "$2" "$WORK/api.json"
}
kinds() { # <file>: by_kind as key=value, sorted
    jq -r '.by_kind | to_entries | sort_by(.key) | map("\(.key)=\(.value)") | join(",")' "$1"
}

# --- (1) the day --------------------------------------------------------------------------

answer 24h "$WORK/day.json"
expect_eq "$(jq -c '{total, per_agent_hour}' "$WORK/day.json")" '{"total":5,"per_agent_hour":0.625}' \
    "(1) the day's interventions: a resume and lead-mac's four posts over 8 agent-hours"
expect_eq "$(kinds "$WORK/day.json")" \
    "cancel_reassign=0,command=0,owner_decision=2,owner_task=0,resume=1,steering=2" \
    "(1) the day's interventions by kind"
hostile_line="$(printf 'x[31m <b>Decisions</b> & "the owner" %s' "$long" | cut -c1-120)…"
want=$(printf '%s\n' \
    "owner_decision|lead-mac|$hostile_line" \
    "steering|lead-mac|F4 PASSES" \
    "owner_decision|lead-mac|OWNER DECISIONS (by the owner): no second host" \
    "steering|lead-mac|[recoverability] Leak check done")
got=$(jq -r '.items[] | select(.source == "queue") | "\(.kind)|\(.principal)|\(.line)"' "$WORK/day.json")
[ "$got" = "$want" ] || check_failed "(1) the day's queue items (kind|principal|line), newest first: expected [$(echo "$want" | tr '\n' ';')], got [$(echo "$got" | tr '\n' ';')]"
expect_eq "$(jq -c '[.items[].at_ms] | . == (sort | reverse)' "$WORK/day.json")" true \
    "(1) the day's items, the store's and the queue's, newest first"
note "(1) done"

# --- (2) the week -------------------------------------------------------------------------

answer 7d "$WORK/week.json"
expect_eq "$(jq -c '{total, owner_decision: .by_kind.owner_decision, steering: .by_kind.steering}' "$WORK/week.json")" \
    '{"total":6,"owner_decision":3,"steering":2}' \
    "(2) the week's interventions: the post 30 h ago counts too"
past=$(date -u -d "@$((now_s - 2 * 86400))" +%Y-%m-%dT%H:%M:%SZ)
answer "24h&until=$past" "$WORK/past.json"
expect_eq "$(jq -r '.total' "$WORK/past.json")" 0 \
    "(2) a day ending 2 days ago: no post of the queue falls in it"
expect_eq "$(kinds "$WORK/past.json")" \
    "cancel_reassign=0,command=0,owner_decision=0,owner_task=0,resume=0,steering=0" \
    "(2) a day ending 2 days ago: the queue's kinds at 0 beside the Spawn kinds"
note "(2) done"

# --- (3) Core lines -----------------------------------------------------------------------

code=$(get /api/core-lines "$WORK/api.json")
if [ "$code" != 200 ]; then
    check_failed "(3) GET /api/core-lines answered $code: $(head -c 200 "$WORK/api.json")"
else
    got=$(jq -c '.lines[] | select(.line == "sustainability") | .north_star | select(.key == "observability.interventions_per_agent_hour") | {value, state}' "$WORK/api.json")
    expect_eq "$got" '{"value":0.625,"state":"out"}' \
        "(3) Core lines' interventions per agent-hour counts the queue's posts"
    note "(3) done"
fi

# --- (4) the hostile heading ----------------------------------------------------------------

got=$(jq -r '[.items[] | select(.source == "queue")][0].line // ""' "$WORK/day.json")
expect_eq "$got" "$hostile_line" \
    "(4) a hostile heading: the escape character dropped, HTML as text, cut at 120 then …"
expect_eq "$(jq -r '[.items[] | select(.source == "queue")][0].line // "" | length' "$WORK/day.json")" 121 \
    "(4) the hostile heading's line is 120 characters and …"
note "(4) done"

# --- (5) a changed file ---------------------------------------------------------------------

printf '\n## lead-mac %s — A post added since\n' "$(clock 0)" >>"$queue"
answer 24h "$WORK/again.json"
expect_eq "$(jq -r '.by_kind.steering' "$WORK/again.json")" 3 \
    "(5) a post appended to the queue counts at the next answer"
note "(5) done"

# --- (6) no queue named -----------------------------------------------------------------------

cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<'JSON'
{"ranges": {"observability.interventions_per_agent_hour": {"max": 0.05}}}
JSON
answer 24h "$WORK/none.json"
expect_eq "$(jq -c '{total, per_agent_hour}' "$WORK/none.json")" '{"total":1,"per_agent_hour":0.125}' \
    "(6) with no queue named, the answer is as before"
expect_eq "$(kinds "$WORK/none.json")" "cancel_reassign=0,command=0,owner_task=0,resume=1" \
    "(6) with no queue named, by_kind is as before"
note "(6) done"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "156: $(wc -l <"$WORK/failures") checks failed" >&2
    exit 1
fi
echo "156: lead-mac's posts in the queue count as the owner's interventions, by kind, in the figure and Core lines"
exit 0
