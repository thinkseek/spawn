#!/bin/sh
# Black box: every alert ends with an outcome, the owner can judge one that
# should not have been raised, and the Core lines card says how often Needs
# you was right.
#
# The problem (owner, 2026-11-12 13:30; sync/roadmap-effects.md,
# Observability, north star "Needs you is right"): the observability line's
# north star is the share of the items Needs you showed that were true and
# really needed the owner. The record cannot tell: an alert is open or
# cleared, and a cleared alert looks the same whether its condition went
# away (it was right) or it was false, stale or a duplicate of another
# (it was wrong). By hand on 11-12: four false "message not taken" alerts
# in one day. Monitoring only: no gate, integrator or deploy reads any of it.
#
# Required (through `spawn alert judge`, `GET /api/alerts` and
# `GET /api/core-lines`):
#   (1) `spawn alert judge <key> false|stale|duplicate` (the owner's)
#       records `alert.judged` {key, verdict} through spawn_alert_record and
#       says so; an open alert it judges is cleared by it, its cleared
#       evidence {judged: <verdict>}. It refuses (exit not 0, nothing
#       written) another verdict, a key no alert has, a notice, and an alert
#       already judged.
#   (2) `GET /api/alerts`' rows carry `outcome`: null while open (and for a
#       notice), "cleared" when its condition cleared it, else its verdict.
#   (3) `GET /api/core-lines`' `observability.needs_you_right`: not
#       collected (value null, state not_collected) until A22/A23 land
#       (owner, 2026-11-12, sync/astra/findings.md A23): the share of ended
#       alerts their condition cleared is not whether they were right, so it
#       is not shown. The share itself stays a unit test of core_lines.rs.
#   (4) a running loop learns of a judgement: an open alert the owner judges
#       is cleared in the loop's view by its next pass, so the loop retries
#       no clear the store refuses, and a later occurrence of the same kind
#       and subject fires a new alert (objective 089's guardrail alert here).
#
# The history: alerts as the loop records them (spawn_alert_record): a1 and
# a2 fired and cleared, a3 and a4 fired and open, a5 a notice. Then the
# owner judges a2 false and a3 a duplicate. Then a real `spawn loop` runs
# with a ranges file that puts free disk out of range (089, raised and
# cleared after 2 s); the owner judges its alert false; free disk goes back
# in range for a while, and out again.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "092: harness: $*" >&2
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
for tool in psql tmux git jq od bash sed grep awk curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_092_outcomes_$run"
SPAWN_TMUX_SOCKET="bb-092-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL
# The tmux server's socket lives in WORK, so the cleanup removes nothing
# outside it (cc-rules (a), 2026-11-12).
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$TMUX_TMPDIR"
export TMUX_TMPDIR

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
            echo "092: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "092: the database_size alerts (key, subject, state, evidence, cleared evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, subject, state, evidence::text, coalesce(cleared_evidence::text, '')
                from spawn_alerts where kind = 'database_size' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "092: harness: unexpected exit $rc" >&2
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
# A scratch HOME: nothing here reads or writes the real one.
real_home=$HOME
HOME=$WORK/home
mkdir -p "$HOME" "$SPAWN_CLAUDE_PROJECTS"
[ "$HOME" != "$real_home" ] && [ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# The alert step reads every 2 s (default 30 s).
SPAWN_ALERT_CHECK_SECS=2
# The guardrails under test: read every second, raised after 6 s out of
# range, cleared after 2 s back in (defaults 300 s, 2 h and 1 h).
SPAWN_ALERT_GUARDRAIL_SECS=1
SPAWN_GUARDRAIL_RAISE_SECS=2
SPAWN_GUARDRAIL_CLEAR_SECS=2
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
export HOME SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
    SPAWN_ALERT_CHECK_SECS SPAWN_ALERT_GUARDRAIL_SECS SPAWN_GUARDRAIL_RAISE_SECS SPAWN_GUARDRAIL_CLEAR_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
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
note() { echo "092: $*" >&2; }
wait_until() { # <seconds> <command...>
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}

record() { # <JSON>: written as the integrator writes it; prints the event id
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_record('$1'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
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
alerts_json() {
    code=$(curl -s --noproxy '*' -o "$WORK/alerts.json" -w '%{http_code}' "$base/api/alerts" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/alerts answered $code: $(head -c 300 "$WORK/alerts.json" 2>/dev/null)"
}



# --- the alerts, the owner's verb -----------------------------------------------------------

unset SPAWN_TEST_DATABASE_URL SPAWN_FLEET_TEST_DATABASE_URL 2>/dev/null || true
cat >"$WORK/seed.sql" <<'SQL'
\set ON_ERROR_STOP 1
select spawn_alert_record(jsonb_build_object('type', 'alert.fired', 'key', 'bb:a' || k, 'kind', 'bb_kind',
       'subject', 'bb subject ' || k, 'text', 'alert ' || k, 'evidence', '{}'::jsonb, 'notice', k = 5))
  from generate_series(1, 5) k;
select spawn_alert_record(jsonb_build_object('type', 'alert.cleared', 'key', 'bb:a' || k, 'kind', 'bb_kind',
       'subject', 'bb subject ' || k, 'evidence', '{"why": "the condition went away"}'::jsonb))
  from generate_series(1, 2) k;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the alerts failed: $(err)"
[ "$(scalar "select string_agg(key || ':' || state, ' ' order by key) from spawn_alerts")" = "bb:a1:cleared bb:a2:cleared bb:a3:open bb:a4:open bb:a5:notice" ] ||
    harness "the seed folded to [$(scalar "select string_agg(key || ':' || state, ' ' order by key) from spawn_alerts")]"

# --- (1) the owner's verb ----------------------------------------------------------------

judge() { # <key> <verdict>: the verb's exit status
    "$SPAWN_BIN" alert judge "$1" "$2" >"$WORK/judge.out" 2>&1
}
judged_count() { scalar "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'alert.judged'"; }
if judge bb:a2 false; then :; else check_failed "(1) spawn alert judge bb:a2 false failed: $(head -c 300 "$WORK/judge.out")"; fi
if judge bb:a3 duplicate; then :; else check_failed "(1) spawn alert judge bb:a3 duplicate failed: $(head -c 300 "$WORK/judge.out")"; fi
expect_eq "$(judged_count)" 2 "(1) the two judgements are recorded (alert.judged)"
for refused in "bb:a1 wrong" "bb:none false" "bb:a2 stale" "bb:a5 false"; do
    # shellcheck disable=SC2086
    if judge $refused; then
        check_failed "(1) spawn alert judge $refused was taken: $(head -c 300 "$WORK/judge.out")"
    fi
done
expect_eq "$(judged_count)" 2 "(1) the refused judgements (another verdict, no such alert, judged already, a notice) wrote nothing"
note "(1) checked: the verb"

start_serve
get() { # <path> <file>: the answer's code
    bb_http "$2" --noproxy '*' "$base$1"
}

# --- (2) the outcomes ----------------------------------------------------------------------

code=$(get /api/alerts "$WORK/api.json")
[ "$code" = 200 ] || harness "GET /api/alerts answered $code"
expect_eq "$(jq -c '[.[] | select(.kind == "bb_kind") | {key, state, outcome}] | sort_by(.key)' "$WORK/api.json")" \
    '[{"key":"bb:a1","state":"cleared","outcome":"cleared"},{"key":"bb:a2","state":"cleared","outcome":"false"},{"key":"bb:a3","state":"cleared","outcome":"duplicate"},{"key":"bb:a4","state":"open","outcome":null},{"key":"bb:a5","state":"notice","outcome":null}]' \
    "(2) each alert's outcome: cleared by its condition, judged false, judged a duplicate (and cleared by it), open, a notice"
expect_eq "$(jq -c '[.[] | select(.key == "bb:a3") | .cleared_evidence.judged]' "$WORK/api.json")" '["duplicate"]' \
    "(2) the open alert judged a duplicate: cleared by the judgement, its cleared evidence the verdict"
expect_eq "$(jq -c '[.[] | select(.key == "bb:a2") | .cleared_evidence.why]' "$WORK/api.json")" '["the condition went away"]' \
    "(2) the cleared alert judged false keeps the evidence it was cleared with"
note "(2) checked: the outcomes"

# --- (3) the north star --------------------------------------------------------------------

code=$(get /api/core-lines "$WORK/lines.json")
if [ "$code" != 200 ]; then
    check_failed "(3) GET /api/core-lines answered $code"
else
    got=$(jq -c '.lines[] | select(.line == "observability") | .north_star | {key, unit, value, state}' "$WORK/lines.json")
    expect_eq "$got" '{"key":"observability.needs_you_right","unit":"share","value":null,"state":"not_collected"}' \
        "(3) the observability north star is not collected until A22/A23 land (a condition clearing is not an alert being right)"
    note "(3) checked: the north star"
fi

# --- (4) a running loop learns of a judgement ---------------------------------------------

ranges=$SPAWN_WORKSPACE_ROOT/core-lines.json
out_range() { printf '{"ranges": {"storage.free_bytes": {"min": 1000000000000000000}}}\n' >"$ranges"; }
in_range() { printf '{"ranges": {"storage.free_bytes": {"min": 1}}}\n' >"$ranges"; }
out_range
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
guardrail() { # free disk's guardrail alerts in /api/alerts, oldest first
    alerts_json
    jq -c '[.[] | select(.kind == "guardrail_out_of_range" and .subject == "guardrail storage.free_bytes")] | sort_by(.fired_at_ms)' "$WORK/alerts.json"
}
one_open() { [ "$(guardrail | jq '[.[] | select(.state == "open")] | length')" = 1 ]; }
if wait_until 60 one_open; then
    first=$(guardrail | jq -r '[.[] | select(.state == "open")][0].key')
    if judge "$first" false; then :; else check_failed "(4) spawn alert judge $first false failed: $(head -c 300 "$WORK/judge.out")"; fi
    # Back in range for longer than the clear hold, then out again.
    in_range
    sleep 8
    out_range
    second_open() { [ "$(guardrail | jq --arg k "$first" '[.[] | select(.state == "open" and .key != $k)] | length')" = 1 ]; }
    if wait_until 60 second_open; then
        expect_eq "$(guardrail | jq -c --arg k "$first" '[.[] | select(.key == $k) | {state, outcome}]')" '[{"state":"cleared","outcome":"false"}]' \
            "(4) the judged alert stays cleared, judged false"
    else
        check_failed "(4) free disk went back in range and out again after its alert was judged, and no new alert opened within 60 s: $(guardrail | head -c 500)"
    fi
    refused=$(grep -c "alerts: $first: " "$WORK/loop.log" || true)
    [ "${refused:-0}" -le 1 ] || check_failed "(4) the loop retried a clear of the judged alert the store refused, $refused times: $(grep "alerts: $first: " "$WORK/loop.log" | head -n 2 | tr '\n' ' ' | head -c 300)"
else
    check_failed "(4) free disk (out of its range) has no open guardrail_out_of_range alert 60 s after the loop started: $(guardrail | head -c 300)"
fi
note "(4) checked: the running loop"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "092: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "092: every alert ends with an outcome, the owner judges a wrong one, a running loop learns of it, and the card says how often Needs you was right"
exit 0
