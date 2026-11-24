#!/bin/sh
# Black box: a halted mission is resumed from Needs you. The page's item
# says why it halted (for "the scenarios fell", which scenarios are gone)
# and resumes that halt: `POST /api/mission/<id>/resume` records the human's
# `mission.resumed` of exactly that halt, once, and refuses with the reason
# when the mission is not halted, or is halted on another halt.
#
# Today Needs you shows "Mission #N ... is halted" with the reason and a
# command to copy (`spawn mission resume --id N`); the owner has to go to a
# shell. "The scenarios fell from 111 to 32" (mission #6, 2026-11-10) did not
# say which 79 scenarios were gone, so telling an intended replacement (the
# journeys replaced 001-090) from a candidate deleting its judges took a
# look at the tree; the halt sat about 7 h.
#
# Required (the objective's prompt says each rule):
#   /api/missions: each mission carries `halt`: null when it is not halted,
#     else {`event`: the id of its mission.halted record, `reason`, `at_ms`,
#     `gone`}; `gone` is, for a halt on the scenario floor, the scenario file
#     names (the gate directory's *.sh) the directory held at the gating
#     that set the floor and does not hold at the halt, sorted; null for any
#     other halt.
#   web/src/home.ts `needsYou`: a halted mission's item carries `resume`
#     {mission, halt: the halt's event}; its detail is the reason and, when
#     scenarios are gone, at most five of their names and "and N more" for
#     the rest; `full` names them all.
#   `POST /api/mission/<id>/resume` {"halt": <event>, "key": <idempotency
#     key>}: the guards of a person's input (the read guard, POST only, the
#     page's Origin, JSON, a bounded body; `{"probe": true}` writes nothing)
#     as the `spawn_human` login through one new right; 202 {"event": <the
#     mission.resumed record>} for the mission's current halt, the record
#     {"type": "mission.resumed", "id", "by": "human", "halt"} folded as the
#     owner's resume is; the same key again is the same resume (202, the same
#     event, nothing new); 409 with the reason ("error") when the mission is
#     not halted or is halted on another halt; 404 for no such mission; 400
#     without a key or a halt.
#
# A real loop (the Bridge, its pusher, the dispatcher, the supervisor) runs
# one resident, `bind`, of one gated mission (accept = gate) on a fake
# engine; `spawn serve` serves the API. The mission's gate directory holds
# a.sh to e.sh. One `spawn integrate`, with a gate that accepts, judges task
# A (the resident writes one.txt): the floor is 5, and it lands. Then c.sh,
# d.sh and e.sh are removed: the integrator halts the mission ("the
# scenarios fell from 5 to 2") and its mission_halted alert opens.
# Checked:
#   (1) /api/missions: the mission's halt names its mission.halted record,
#       the reason, and gone ["c.sh", "d.sh", "e.sh"].
#   (2) needsYou under node alone, over that answer: the item's resume is
#       {mission, halt}, its detail the reason and the three names; fixed
#       cases: twelve gone (five named, "and 7 more", all in full), a halt
#       of another reason (its reason, gone null, still resumable), a
#       mission not halted (no item).
#   (3) refused, nothing recorded, still halted: text/plain 415; another
#       site's Origin 403; a probe 200; no key 400; no halt 400; no such
#       mission 404; a halt that is not the mission's 409 with an error.
#   (4) the resume: 202 with its event, a mission.resumed of the mission by
#       human for that halt, one such record; the mission no longer halted
#       in the store and in /api/missions (halt null); the halt's alert
#       cleared on that record within 15 s.
#   (5) the same POST again: 202, the same event, still one record; a new
#       key: 409, "not halted", still one record.
#   (6) task B (two.txt) lands, setting the floor to 2; b.sh is removed and
#       the mission halts again ("the scenarios fell from 2 to 1"): halt
#       names the new record and gone ["b.sh"]; a resume naming the first
#       halt (a page not refreshed) is 409 and records nothing; one naming
#       the second resumes it.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (ea3a2e9f): /api/missions has no
# `halt`. It passes once the halt is named and resumed from the page as
# above.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network but loopback. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "051: harness: $*" >&2
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
for tool in psql tmux git jq od bash sed grep awk date sort curl node sha1sum; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME, and the tmux server's socket, in WORK: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$HOME" "$TMUX_TMPDIR"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME TMUX_TMPDIR
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
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_051_resume_$run"
SPAWN_TMUX_SOCKET="bb-051-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL
TEST_URL=$SPAWN_TEST_DATABASE_URL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
integrator_pid=""
serve_pid=""
cleanup() {
    if [ -n "$serve_pid" ]; then
        kill "$serve_pid" 2>/dev/null || true
        wait "$serve_pid" 2>/dev/null || true
    fi
    if [ -n "$integrator_pid" ]; then
        kill "$integrator_pid" 2>/dev/null || true
        wait "$integrator_pid" 2>/dev/null || true
    fi
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
    fi
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
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
            echo "051: the loop's last lines:" >&2
            tail -n 12 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "051: the integrator's last lines:" >&2
            grep -v '^\s*$' "$WORK/integrate.log" | grep -v '^| ' | tail -n 12 >&2
        fi
        if [ -s "$WORK/judged.log" ]; then
            echo "051: the gate runs (time_ms base candidate):" >&2
            cat "$WORK/judged.log" >&2
        fi
        if [ -s "$WORK/serve.log" ]; then
            echo "051: serve's last lines:" >&2
            tail -n 5 "$WORK/serve.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "051: harness: unexpected exit $rc" >&2
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

# An assertion exits 1, unless the database is gone: then nothing the
# scenario read can be trusted, and that is the harness's failure.
fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || fail "$3: expected [$2], got [$1]"
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
source=$WORK/source.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "base"
for bare in "$repo" "$source"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

# A controller tree: its `.controller` names a commit (deploy/controller.sh writes
# the deployed commit there), its rsi/scenarios is the mission's gate
# directory.
ctl=$WORK/ctl
gate=$ctl/rsi/scenarios
mkdir -p "$gate"
controller_commit=$(printf 'bb-051-controller' | sha1sum | cut -c1-40)
printf '%s\n' "$controller_commit" >"$ctl/.controller"
for name in a b c d e; do
    printf '#!/bin/sh\nexit 0\n' >"$gate/$name.sh"
done
: >"$WORK/judged.log"

# The gate: it notes each run and accepts, printing what scripts/rsi/gate.sh
# prints for an acceptance.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
printf '%s %s %s\n' "\$(date +%s%3N)" "\$2" "\$3" >>'$WORK/judged.log'
echo "| == setup"
echo "| base cache: toolchain rustc 1.95.0-bb cargo 1.95.0-bb psql 16.bb server 160099"
echo "| == candidate: build"
echo "| stage gate ok 900ms candidate: build"
printf '%s\n' '{"verdict":"accepted"}' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
exit 0
EOF
chmod 755 "$WORK/fake-gate.sh"

# The fake engine: a text naming `BB:WRITE:<file>` writes `<file>.txt`, waits
# for the pusher to publish it (at most 30 s), then writes the report.
# Anything else is a turn that changes nothing.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=$2 ;; esac
  shift
done
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
name=${SPAWN_SESSION_ID#session_resident_}
agent=agent_resident_$name
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1
  line '"type":"system","subtype":"turn_duration","durationMs":1'
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
job=
while :; do
  if bb_read_line typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:WRITE:"*)
      job=${typed#*BB:WRITE:}
      job=${job%%[!a-z]*}
      ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$job" ]; then
      file=$job.txt
      printf '%s wrote %s\n' "$name" "$job" >"$file"
      turn_ends "wrote $file"
      start=$(date +%s)
      until [ "$(git --git-dir="$BB_WORK/source.git" show "refs/heads/spawn/$agent:$file" 2>/dev/null)" = "$(cat "$file")" ]; do
        [ $(($(date +%s) - start)) -ge 30 ] && break
        sleep 0.2
      done
      line '"type":"user","message":{"role":"user","content":"report"}'
      sleep 2
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$file" >.spawn/report.md
      job=
    fi
    turn_ends idle
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_GATE_MIN_FREE_GB \
    SPAWN_GATE_MIN_FREE_MEMORY_MB SPAWN_INTEGRATE_GATES SPAWN_TEST_DATABASE_LOG \
    SPAWN_GATE_HOST_FAULT_BACKOFF_SECS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_INTEGRATE_WARM; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
SPAWN_SUPERVISOR=1
SPAWN_MISSION_FILE=$WORK/mission.md
# The stall bounds out of the way: nothing here is about stalls.
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
# No base warm after the landing: its gate runs would be noise in judged.log.
SPAWN_INTEGRATE_WARM=0
BACKOFF_SECS=2
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_SUPERVISOR \
    SPAWN_MISSION_FILE SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_INTEGRATE_WARM
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/integrate.log"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- helpers ----------------------------------------------------------------

now() { date +%s; }
q() { scalar "$1"; }
task_state() { q "select state from mission_tasks where id = $1"; }
halted() { q "select coalesce(halted, '') from mission_missions where id = $mission"; }
tasks_line() { q "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks"; }
log_tail() { grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 6 | tr '\n' ' '; }
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }
# A field of event $1's record, as text ('' when absent; JSON null is '').
field() { q "select coalesce(kind -> 'detail' -> 'record' ->> '$2', '') from spawn_events where id = $1"; }
# A path into event $1's record (jsonb #>> '{a,b}'), as text; `null` for JSON
# null, `absent` when the path does not exist.
path() {
    q "select case when kind -> 'detail' -> 'record' #> '{$2}' is null then 'absent'
                   when jsonb_typeof(kind -> 'detail' -> 'record' #> '{$2}') = 'null' then 'null'
                   else kind -> 'detail' -> 'record' #>> '{$2}' end
         from spawn_events where id = $1"
}
main_of() { git --git-dir="$source" rev-parse main; }

waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$1'")" = WAITING ] &&
        [ "$(q "select count(*) from agent_executions
                where session_id = '$1' and ended_at_ms is null")" -ge 1 ]
}

# The integrator, started once (see the header).
start_integrator() {
    env SPAWN_GATE="$WORK/fake-gate.sh" SPAWN_TEST_DATABASE_URL="$TEST_URL" \
        SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        SPAWN_GATE_HOST_FAULT_BACKOFF_SECS="$BACKOFF_SECS" \
        "$SPAWN_BIN" integrate --root "$WORK/integrator" >>"$WORK/integrate.log" 2>&1 &
    integrator_pid=$!
}
integrator_alive() { kill -0 "$integrator_pid" 2>/dev/null; }


note() { echo "051: $*" >&2; }

web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
start_serve() {
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
    base="http://127.0.0.1:$port"
}
# resume_post <mission> <body> [content type] [origin]: the status code; the
# answer in $WORK/resume.json. As the page posts: JSON, the page's own Origin.
resume_post() {
    bb_http "$WORK/resume.json" --noproxy '*' -X POST \
        -H "Content-Type: ${3:-application/json}" -H "Origin: ${4:-$base}" \
        --data "$2" "$base/api/mission/$1/resume"
}
answer() { head -c 300 "$WORK/resume.json" 2>/dev/null; }
missions_json() {
    code=$(curl -s --noproxy '*' -o "$WORK/missions.json" -w '%{http_code}' "$base/api/missions" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/missions answered $code: $(head -c 300 "$WORK/missions.json" 2>/dev/null)"
}
# halt_of: the mission's `halt` in /api/missions, compact JSON (`absent` when
# the mission has no such field).
halt_of() {
    missions_json
    jq -c --argjson id "$mission" '([.missions[]? | select(.id == $id)][0] // {}) | if has("halt") then .halt else "absent" end' "$WORK/missions.json"
}
resumes() {
    q "select count(*) from spawn_events where session_id = 'session_control'
         and kind -> 'detail' -> 'record' ->> 'type' = 'mission.resumed'
         and kind -> 'detail' -> 'record' ->> 'id' = '$mission'"
}
alert_state() { q "select coalesce((select state from spawn_alerts where key = '$1'), 'none')"; }
alert_is() { [ "$(alert_state "$1")" = "$2" ]; }
# wait_until <seconds> <command...>: true once the command is, within the bound.
wait_until() {
    bound=$(($(now) + $1))
    shift
    until "$@"; do
        [ "$(now)" -lt "$bound" ] || return 1
        sleep 0.5
    done
}
cleared_by() { q "select coalesce((select cleared_evidence ->> 'event_id' from spawn_alerts where key = '$1'), '')"; }

# --- one mission, one resident, task A gated with five scenarios --------------------

capture "$SPAWN_BIN" mission add --text "051 resume from Needs you" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" resident propose --name bind --charter "Owns the 051 change."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name bind
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
started=$(now)
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission configure exited $status: $(err)"

# gated_since <task> <event>: the task has a `task.gated` record after event.
gated_since() {
    [ "$(q "select count(*) from spawn_events where session_id = 'session_control'
              and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
              and kind -> 'detail' -> 'record' ->> 'id' = '$1' and id > $2")" -ge 1 ]
}
# assign_and_report <file name>: sets `task` and `assigned_at` (the last event
# before the assignment: every record about the task comes after it). A running
# integrator may judge the report the moment it comes and send the task back,
# so the wait ends at the report or at its first verdict, whichever is seen.
assign_and_report() {
    assigned_at=$(last_event)
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker bind \
        --brief "Write your file. BB:WRITE:$1"
    [ "$status" -eq 0 ] || harness "task assign failed: $(err)"
    task=$(jq -r .task "$WORK/out.txt")
    [ -n "$task" ] && [ "$task" != null ] || harness "task assign printed no id: $(out)"
    deadline=$(($(now) + 120))
    until [ "$(task_state "$task")" = reported ] || gated_since "$task" "$assigned_at"; do
        [ "$(now)" -lt "$deadline" ] ||
            harness "task #$task was not reported within 120 s (tasks: $(tasks_line))"
        sleep 0.3
    done
    git --git-dir="$source" cat-file -e "refs/heads/spawn/agent_resident_bind:$1.txt" 2>/dev/null ||
        harness "bind's pushed branch does not hold $1.txt"
}

assign_and_report one
task_a=$task
deadline=$(($(now) + 120))
until waiting session_supervisor; do
    [ "$(now)" -lt "$deadline" ] || harness "the supervisor was never judged waiting"
    sleep 0.5
done
start_serve
start_integrator
deadline=$(($(now) + 120))
until [ "$(task_state "$task_a")" = landed ]; do
    [ "$(now)" -lt "$deadline" ] || harness "task #$task_a did not land within 120 s (tasks: $(tasks_line); integrate: $(log_tail))"
    integrator_alive || harness "the integrator exited before task #$task_a landed: $(log_tail)"
    [ -z "$(halted)" ] || harness "the mission halted before task #$task_a landed: $(halted)"
    sleep 0.3
done
floor=$(q "select scenario_floor from mission_missions where id = $mission")
[ "$floor" = 5 ] || harness "task #$task_a's gating set the scenario floor to '$floor', not the 5 scenarios of the gate directory"

# --- the halt: three scenarios gone ----------------------------------------------------

before_halt=$(last_event)
rm -f "$gate/c.sh" "$gate/d.sh" "$gate/e.sh"
deadline=$(($(now) + 60))
until [ -n "$(halted)" ]; do
    [ "$(now)" -lt "$deadline" ] || harness "the mission did not halt within 60 s of losing three of its five scenarios (integrate: $(log_tail))"
    integrator_alive || harness "the integrator exited before the halt: $(log_tail)"
    sleep 0.3
done
reason=$(halted)
[ "$reason" = "the scenarios fell from 5 to 2" ] || harness "the mission halted for '$reason', not for its scenarios"
halt=$(q "select max(id) from spawn_events where session_id = 'session_control' and id > $before_halt
            and kind -> 'detail' -> 'record' ->> 'type' = 'mission.halted'
            and kind -> 'detail' -> 'record' ->> 'id' = '$mission'")
[ -n "$halt" ] || harness "no mission.halted record after event $before_halt"
alert="mission_halted:$mission:$halt"
wait_until 30 alert_is "$alert" open ||
    harness "the halt's alert $alert is not open 30 s after the halt (it reads '$(alert_state "$alert")')"

# --- (1) the missions read names the halt and what is gone ------------------------------

got=$(halt_of)
expect_eq "$(printf '%s' "$got" | jq -c '.event? // "none"' 2>/dev/null || echo "not an object")" "$halt" \
    "(1) /api/missions: the halted mission's halt.event (its mission.halted record; halt is $got)"
expect_eq "$(printf '%s' "$got" | jq -r '.reason? // "none"' 2>/dev/null)" "$reason" "(1) /api/missions: halt.reason"
expect_eq "$(printf '%s' "$got" | jq -c '.gone? // "none"' 2>/dev/null)" '["c.sh","d.sh","e.sh"]' \
    "(1) /api/missions: halt.gone, the scenario files the gate directory held when task #$task_a set the floor and no longer holds"
note "(1) /api/missions names the halt $halt, its reason and the three scenarios gone"

# --- (2) Needs you: the item resumes this halt and lists what is gone ---------------------

cp "$WORK/missions.json" "$WORK/missions-halted.json"
cat >"$WORK/needs.test.mjs" <<'EOF'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { needsYou } = await import(pathToFileURL(`${env.BB_TREE}/web/src/home.ts`).href);
const now = Date.now();
const input = (missions) => ({
  attention: null, missions, alerts: null, agents: null, health: null,
  missingAlwaysOn: 0, now,
});
const item = (missions, id) => needsYou(input(missions)).find((i) => i.key === `halted:${id}`);
const text = (i) => `${i.detail ?? ""}\n${i.full ?? ""}`;

test("the live answer: resume names the halt, the detail names what is gone", () => {
  const mission = Number(env.BB_MISSION);
  const got = item(JSON.parse(readFileSync(env.BB_MISSIONS, "utf8")), mission);
  assert.ok(got, "no Needs you item for the halted mission");
  assert.deepEqual(got.resume, { mission, halt: Number(env.BB_HALT) });
  for (const name of ["c.sh", "d.sh", "e.sh"]) assert.ok(got.detail.includes(name), `detail lacks ${name}: ${got.detail}`);
  assert.ok(got.detail.includes(env.BB_REASON), `detail lacks the reason: ${got.detail}`);
});

const halted = (id, halt) => ({
  id, mission_name: `m${id}`, halted: halt.reason, updated_at_ms: now - 60000, halt,
  state: "open", tasks: [],
});
test("twelve gone: five named, the rest counted, all in full", () => {
  const gone = Array.from({ length: 12 }, (_, n) => `${String(n + 1).padStart(3, "0")}.sh`);
  const got = item({ missions: [halted(7, { event: 70, reason: "the scenarios fell from 40 to 28", at_ms: now - 60000, gone })], holds: [] }, 7);
  assert.deepEqual(got.resume, { mission: 7, halt: 70 });
  for (const name of gone.slice(0, 5)) assert.ok(got.detail.includes(name), `detail lacks ${name}: ${got.detail}`);
  assert.ok(!got.detail.includes(gone[5]), `detail names more than five: ${got.detail}`);
  assert.ok(got.detail.includes("and 7 more"), `detail does not count the rest: ${got.detail}`);
  for (const name of gone) assert.ok((got.full ?? "").includes(name), `full lacks ${name}: ${got.full}`);
});
test("another halt: its reason, no list, still resumable", () => {
  const got = item({ missions: [halted(8, { event: 80, reason: "3 gate rejections in a row", at_ms: now - 60000, gone: null })], holds: [] }, 8);
  assert.deepEqual(got.resume, { mission: 8, halt: 80 });
  assert.equal(got.detail, "3 gate rejections in a row");
});
test("a mission that is not halted: no item", () => {
  assert.equal(item({ missions: [{ id: 9, mission_name: "m9", halted: null, halt: null, updated_at_ms: now, state: "open", tasks: [] }], holds: [] }, 9), undefined);
});
EOF
if ! BB_TREE=$tree BB_MISSIONS=$WORK/missions-halted.json BB_MISSION=$mission BB_HALT=$halt BB_REASON=$reason \
    node --test "$WORK/needs.test.mjs" >"$WORK/needs.out" 2>&1; then
    fail "(2) web/src/home.ts's needsYou, under node alone: $(grep -E 'not ok|AssertionError|Error|expected|actual|lacks|names|count' "$WORK/needs.out" | head -n 8 | tr '\n' ' ' | head -c 900)"
fi
note "(2) Needs you's item resumes halt $halt and names c.sh, d.sh and e.sh"

# --- (3) refusals write nothing -------------------------------------------------------------

[ "$(resumes)" = 0 ] || harness "a mission.resumed record exists before any resume"
good="{\"halt\":$halt,\"key\":\"bb-051-1\"}"
code=$(resume_post "$mission" "$good" text/plain)
expect_eq "$code" 415 "(3) a resume posted as text/plain (no JSON content type): $(answer)"
code=$(resume_post "$mission" "$good" application/json http://elsewhere.example)
expect_eq "$code" 403 "(3) a resume posted from another site's page (a cross-origin Origin): $(answer)"
code=$(resume_post "$mission" '{"probe":true}')
expect_eq "$code" 200 "(3) a probe of the resume: $(answer)"
code=$(resume_post "$mission" "{\"halt\":$halt}")
expect_eq "$code" 400 "(3) a resume with no idempotency key: $(answer)"
code=$(resume_post "$mission" '{"key":"bb-051-0"}')
expect_eq "$code" 400 "(3) a resume that names no halt: $(answer)"
code=$(resume_post 999999 "{\"halt\":$halt,\"key\":\"bb-051-0\"}")
expect_eq "$code" 404 "(3) a resume of a mission that does not exist: $(answer)"
other=$((halt - 1))
code=$(resume_post "$mission" "{\"halt\":$other,\"key\":\"bb-051-0\"}")
expect_eq "$code" 409 "(3) a resume naming event $other, not the mission's halt $halt (a page showing an older halt): $(answer)"
[ -n "$(jq -r '.error // empty' "$WORK/resume.json" 2>/dev/null)" ] ||
    fail "(3) the 409 for a halt that is not the mission's carries no reason (\"error\"): $(answer)"
expect_eq "$(resumes)" 0 "(3) mission.resumed records after the refusals"
expect_eq "$(halted)" "$reason" "(3) the mission's halt after the refusals"
note "(3) wrong type 415, another site 403, probe 200, no key or no halt 400, no mission 404, another halt 409: nothing recorded"

# --- (4) the resume: the human's mission.resumed, the halt over --------------------------------

code=$(resume_post "$mission" "$good")
expect_eq "$code" 202 "(4) the resume of halt $halt: $(answer)"
event=$(jq -r '.event // empty' "$WORK/resume.json" 2>/dev/null)
case "$event" in '' | *[!0-9]*) fail "(4) the 202 names no event (\"event\", the mission.resumed record): $(answer)" ;; esac
expect_eq "$(field "$event" type)" mission.resumed "(4) the type of the record event $event"
expect_eq "$(field "$event" id)" "$mission" "(4) the mission the record resumes"
expect_eq "$(field "$event" by)" human "(4) who the record says resumed it (by)"
expect_eq "$(field "$event" halt)" "$halt" "(4) the halt the record resumes (halt)"
expect_eq "$(resumes)" 1 "(4) mission.resumed records after the resume"
expect_eq "$(halted)" "" "(4) the mission's halt after the resume"
expect_eq "$(halt_of)" null "(4) /api/missions: the resumed mission's halt"
wait_until 15 alert_is "$alert" cleared ||
    fail "(4) the halt's alert $alert reads '$(alert_state "$alert")' 15 s after the resume, not cleared"
expect_eq "$(cleared_by "$alert")" "$event" "(4) the record the halt's alert cleared on"
note "(4) 202: mission.resumed $event by human for halt $halt; the mission runs and its alert cleared on it"

# --- (5) a retried POST is the same resume; a later one is refused ------------------------------

code=$(resume_post "$mission" "$good")
expect_eq "$code" 202 "(5) the same POST again (the same key: a retry): $(answer)"
expect_eq "$(jq -r '.event // empty' "$WORK/resume.json" 2>/dev/null)" "$event" "(5) the retry's event (the first resume's record)"
expect_eq "$(resumes)" 1 "(5) mission.resumed records after the retry"
code=$(resume_post "$mission" "{\"halt\":$halt,\"key\":\"bb-051-2\"}")
expect_eq "$code" 409 "(5) a new resume of the mission, no longer halted: $(answer)"
case "$(jq -r '.error // empty' "$WORK/resume.json" 2>/dev/null)" in
*"not halted"*) ;;
*) fail "(5) the 409 for a mission that is not halted does not say so (\"not halted\" in its error): $(answer)" ;;
esac
expect_eq "$(resumes)" 1 "(5) mission.resumed records after the refused resume"
integrator_alive || harness "the integrator exited during the scenario: $(log_tail)"
note "(5) the retry is the same resume $event; a new resume is refused: not halted"

# --- (6) halted again: the page's older halt is refused, the new one resumes -------------------

assign_and_report two
task_b=$task
deadline=$(($(now) + 120))
until [ "$(task_state "$task_b")" = landed ]; do
    [ "$(now)" -lt "$deadline" ] || harness "task #$task_b did not land within 120 s (tasks: $(tasks_line); integrate: $(log_tail))"
    integrator_alive || harness "the integrator exited before task #$task_b landed: $(log_tail)"
    [ -z "$(halted)" ] || harness "the mission halted before task #$task_b landed: $(halted)"
    sleep 0.3
done
[ "$(q "select scenario_floor from mission_missions where id = $mission")" = 2 ] ||
    harness "task #$task_b's gating after the resume did not set the floor to the 2 scenarios left"
before_halt=$(last_event)
rm -f "$gate/b.sh"
deadline=$(($(now) + 60))
until [ -n "$(halted)" ]; do
    [ "$(now)" -lt "$deadline" ] || harness "the mission did not halt again within 60 s of losing b.sh (integrate: $(log_tail))"
    sleep 0.3
done
[ "$(halted)" = "the scenarios fell from 2 to 1" ] || harness "the mission halted again for '$(halted)'"
halt2=$(q "select max(id) from spawn_events where session_id = 'session_control' and id > $before_halt
             and kind -> 'detail' -> 'record' ->> 'type' = 'mission.halted'
             and kind -> 'detail' -> 'record' ->> 'id' = '$mission'")
got=$(halt_of)
expect_eq "$(printf '%s' "$got" | jq -c '[.event?, .gone?]' 2>/dev/null)" "[$halt2,[\"b.sh\"]]" \
    "(6) /api/missions after the second halt: [halt.event, halt.gone] (the latest halt; halt is $got)"
code=$(resume_post "$mission" "{\"halt\":$halt,\"key\":\"bb-051-3\"}")
expect_eq "$code" 409 "(6) a resume naming the first halt $halt (a page not yet refreshed) while the mission is halted on $halt2: $(answer)"
expect_eq "$(resumes)" 1 "(6) mission.resumed records after the stale resume"
code=$(resume_post "$mission" "{\"halt\":$halt2,\"key\":\"bb-051-4\"}")
expect_eq "$code" 202 "(6) the resume of the second halt $halt2: $(answer)"
event2=$(jq -r '.event // empty' "$WORK/resume.json" 2>/dev/null)
expect_eq "$(field "$event2" halt)" "$halt2" "(6) the halt the second resume's record names"
expect_eq "$(resumes)" 2 "(6) mission.resumed records after the second resume"
expect_eq "$(halted)" "" "(6) the mission's halt after the second resume"
note "(6) halted again on $halt2 (b.sh gone): the first halt's resume is refused, the second's is $event2"

echo "051: a halted mission resumes from Needs you: the item names its halt and the scenarios gone, the POST records the human's mission.resumed of exactly that halt, once, and refuses with its reason otherwise"
exit 0
