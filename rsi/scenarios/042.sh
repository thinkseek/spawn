#!/bin/sh
# Black box: a stalled worker is restarted or reassigned, by the supervisor
# or by a person from the agent page, without losing its work. Restart: the
# worker's running execution is stopped and a new one resumes its native
# session on the same workspace, its earlier commits present. Reassign: the
# task moves to a fresh worker whose branch starts from the old worker's
# work, and the old one ends with its cause recorded. The trace links old
# and new.
#
# Today (13fc93a7) nothing restarts or reassigns on demand: there is no
# `spawn task restart` or `spawn task reassign`, no page route, and the
# recovery revives only an execution that ended by itself. A stalled worker
# (busy, no fact for SPAWN_STALL_ACTIVITY_SECS) is escalated to the
# supervisor, who can only stop it (a Stop is final), cancel the task (its
# work set aside) or type into it. Required (docs/fleet-architecture.md §9,
# backlog S4):
#   - `spawn task restart --id N [--reason <text>]` and `spawn task reassign
#     --id N [--reason <text>]`, for the owner, a person and the supervisor
#     (in its charter), on a `delivered` task with a worker; any other task
#     is refused (non-zero, naming its state) and nothing is sent;
#   - `POST /api/session/<id>/restart` and `/reassign` (`{"reason"}`, as the
#     input POST: JSON, the page's Origin, the human login), answering 202
#     `{"task","session_id","agent_id","principal":"human"}` (the task and
#     the worker that now carry the work), 409 for a session that holds no
#     delivered task (nothing recorded);
#   - a restart records `task.restarted` {task, session_id, execution_id,
#     by, reason, n}; the dispatcher stops the running execution (its final
#     checkpoint kept), records its end `execution.interrupted` cause
#     `restarted` path `final` (the recovery never revives it), then resumes
#     the same native session (a `resume`, key `restart-<task>-<n>`) with a
#     prompt naming the restart and its reason;
#   - a reassign, as the dispatcher's re-issue does (spawn_task_reissue),
#     cancels the task and assigns it again in one transaction: the old
#     task's `task.cancelled` names `by`, `reason` and `reassigned_as` (the
#     new task), the new task's `task.assigned` names `reassign_of` and
#     copies the old one's mission, kind, brief, model, objective and
#     `created_by`, with a fresh ephemeral worker whose branch starts from
#     the old worker's branch head; the old worker is stopped (its final
#     checkpoint kept), its end recorded `execution.interrupted` cause
#     `reassigned` path `final` (evidence `to_session`), never revived;
#   - `GET /api/session/<id>`: each execution carries `restart_of` (the
#     execution it replaced, else null); the session carries
#     `reassigned_to` / `reassigned_from` `{"task","session_id"}` (the new
#     task and its worker's session; the old task and its worker's; else
#     null); the page's trace shows them.
#
# A real `spawn loop` (the Bridge, the dispatcher, the supervisor, the
# recovery; no model judge) and `spawn serve` run two ephemeral workers of
# one mission, each with a delivered task, and the supervisor, on the fake
# engine below. Each worker's first turn commits a file and then stalls (a
# turn that never ends). Checked:
#   (1) restart from the CLI: `spawn task restart` exits 0; the task is
#       reported by the worker's second execution, in the same session,
#       started by a `resume` keyed `restart-<task>-...`, in the same
#       native session and workspace, finding the file the first turn
#       wrote (checkpointed at the stop); the first
#       execution's end is recorded `restarted final` and nothing from the
#       recovery revives the worker; the API names the second execution's
#       `restart_of`: the first.
#   (2) reassign from the page: the POST answers 202 with a new task and
#       a new session; the old task is cancelled (`reassigned_as` the new
#       one, `by` human, the reason) and the new task (`reassign_of` the
#       old, the same brief) is reported by its fresh worker, in a
#       workspace of its own that holds the file the old worker wrote (its
#       branch starts from the old one's checkpoint); the old worker ran
#       one execution, ended `reassigned final` (evidence `to_session`: the
#       new one), never revived; the API links the old session and the new
#       both ways.
#   (3) refusals: restarting the task (1) once reported exits non-zero
#       naming `reported` and sends nothing; the page's restart of the
#       supervisor (no task) answers 409 and records nothing.
#   (4) the supervisor's charter names `task restart --id` and `task
#       reassign --id` among its tools.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (13fc93a7): `spawn task restart` is
# an unknown verb. It passes once a stalled worker can be restarted and
# reassigned, by the CLI and from the page, its work kept and linked.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.

set -eu

asserted=""
harness() {
    echo "042: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep awk curl; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_042_restart_$run"
SPAWN_TMUX_SOCKET="bb-042-$run"
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
stop_loop() {
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
        loop_pid=""
    fi
}
cleanup() {
    stop_loop
    if [ -n "$serve_pid" ]; then
        kill "$serve_pid" 2>/dev/null || true
        wait "$serve_pid" 2>/dev/null || true
    fi
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
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
            echo "042: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engine.log" ]; then
            echo "042: what the engines read (last lines):" >&2
            tail -n 20 "$WORK/engine.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "042: the workers' commands (id, session, principal, key, type, state, succeeded, error):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, session_id, principal, idempotency_key,
                    action ->> 'type', state, succeeded, left(coalesce(error, ''), 120)
                from spawn_commands where session_id <> 'session_supervisor' order by id" \
                >&2 2>/dev/null || true
            echo "042: the recovery's records (id, session, type, cause, path, attempt, due):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, kind -> 'detail' -> 'record' ->> 'session_id',
                    kind -> 'detail' -> 'record' ->> 'type', kind -> 'detail' -> 'record' ->> 'cause',
                    kind -> 'detail' -> 'record' ->> 'path', kind -> 'detail' -> 'record' ->> 'attempt',
                    kind -> 'detail' -> 'record' ->> 'due_ms'
                from spawn_events where event_name = 'control.recorded'
                 and kind -> 'detail' -> 'record' ->> 'type' like 'execution.%' order by id" \
                >&2 2>/dev/null || true
            if [ -s "$WORK/turns.log" ]; then
                echo "042: the engines' turns:" >&2
                cat "$WORK/turns.log" >&2
            fi
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "042: harness: unexpected exit $rc" >&2
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

# The fake engine. A prompt typed in (a paste, then Enter) opens a turn
# after a second of quiet. The supervisor (agent_supervisor) ends each turn
# at once. A worker with a plan ($BB_WORK/ctl/<agent>.plan, one step per
# line; the n-th turn of the agent across its executions takes the n-th
# line, the last line once they run out) acts it out; a worker without one
# (the fresh worker a reassign makes) takes $BB_WORK/ctl/default.plan. The
# steps: `commit <name>` writes <name>.txt into the workspace (the Bridge's
# checkpoint commits it to the worker's branch: the engine sees no git),
# then stalls: the turn never ends (a spinner redrawn in place, no
# transcript line, until the engine is stopped); `work` runs a tool call,
# writes the report and ends the turn (the task done). Each turn is logged
# to turns.log: time, agent, execution, n, step, the native session, the
# workspace's path and the .txt files it holds (comma-joined).
cat >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=${2-}; shift ;; esac
  shift
done
agent=$SPAWN_AGENT_ID
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
stalled=0
turn() {
  if [ "$agent" = agent_supervisor ]; then
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    return
  fi
  plan=$BB_WORK/ctl/$agent.plan
  [ -f "$plan" ] || plan=$BB_WORK/ctl/default.plan
  counter=$BB_WORK/ctl/$agent.n
  n=$(cat "$counter" 2>/dev/null || echo 0)
  n=$((n + 1))
  printf '%s\n' "$n" >"$counter"
  step=$(sed -n "${n}p" "$plan")
  [ -n "$step" ] || step=$(tail -n 1 "$plan")
  files=$(ls ./*.txt 2>/dev/null | sed 's#^\./##' | tr '\n' ',')
  printf '%s %s %s %s %s %s %s %s\n' "$(date +%s%3N)" "$agent" "$SPAWN_EXECUTION_ID" "$n" \
    "${step%% *}" "$native" "$PWD" "${files:--}" >>"$BB_WORK/turns.log"
  case "$step" in
  commit\ *)
    name=${step#commit }
    id="toolu_${SPAWN_EXECUTION_ID}_$count"
    line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
    printf '%s\n' "$name" >"$name.txt"
    # The stall: the tool's result never comes, the spinner is redrawn in
    # place (one status line), the turn never ends.
    stalled=1
    printf '\n✻ Running…'
    ;;
  *)
    id="toolu_${SPAWN_EXECUTION_ID}_$count"
    line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
    printf 'done\n' >result.txt
    sleep 0.3
    line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
    mkdir -p .spawn
    printf '## Outcome\ndone\n## What changed\nresult.txt\n## Open issues\nnone\n' >.spawn/report.md
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    ;;
  esac
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
ticks=0
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'got %s %s %s\n' "$(date +%s.%N)" "$agent" "$typed" >>"$BB_WORK/engine.log"
    [ "$stalled" = 1 ] && continue
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
  elif [ "$stalled" = 1 ]; then
    ticks=$((ticks + 1))
    printf '\r\033[2K✻ Running… (%ss · esc to interrupt)' "$ticks"
  elif [ "$pending" = 1 ]; then
    pending=0
    turn
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
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
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
# The supervisor and the dispatcher run, as on the host: the dispatcher's
# digest is how the supervisor is told.
SPAWN_SUPERVISOR=1
# The recovery's resume backoff, short (default 10 s): a stopped execution
# the recovery wrongly revived would show within the checks' waits.
SPAWN_RECOVER_BACKOFF_SECS=2
# The stall escalation is not under test: the restart and the reassign are
# asked for directly, as the supervisor or a person would after it.
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
SPAWN_MISSION_FILE=$WORK/mission.md
# Free disk is not what this scenario is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_SUPERVISOR \
    SPAWN_MISSION_FILE SPAWN_RECOVER_BACKOFF_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/engine.log"
: >"$WORK/turns.log"
mkdir -p "$WORK/ctl"

# The test server is shared: another run's init-db can collide on cluster-wide
# objects ("tuple concurrently updated"). init-db is idempotent.
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

start_loop() {
    "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!
}

# wait_until <seconds> <command...>: true once the command is, false past
# the limit.
wait_until() {
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}

# The records of the log, by type: `control.recorded` events' records.
RECORD="kind -> 'detail' -> 'record'"
INTERRUPTED="select $RECORD ->> 'cause' c, $RECORD ->> 'path' p, $RECORD ->> 'execution_id' e,
                    $RECORD -> 'evidence' ev, id
               from spawn_events
              where event_name = 'control.recorded' and $RECORD ->> 'type' = 'execution.interrupted'"
records_of() { # <session>: `<cause> <path> <execution>` lines, in order
    sql "select c || ' ' || p || ' ' || e from ($INTERRUPTED) r where r.e in
          (select execution_id from agent_executions where session_id = '$1') order by id"
}
task_record() { # <type> <task>: the latest record of that type naming the task (id or task)
    q "select $RECORD from spawn_events where event_name = 'control.recorded'
         and $RECORD ->> 'type' = '$1'
         and coalesce($RECORD ->> 'id', $RECORD ->> 'task') = '$2' order by id desc limit 1"
}
executions_of() { q "select count(*) from agent_executions where session_id = '$1'"; }
execution_n() { # <session> <n>: its n-th execution, from 1
    q "select execution_id from agent_executions where session_id = '$1'
        order by started_at_ms, execution_id offset $(($2 - 1)) limit 1"
}
ended() { [ -n "$(q "select ended_at_ms from agent_executions where execution_id = '$1' and ended_at_ms is not null")" ]; }
revival_count() { q "select count(*) from spawn_commands where session_id = '$1'
                      and principal = 'spawn-liveness' and idempotency_key like 'recover-%'
                      and action ->> 'type' in ('resume', 'start', 'input')"; }
commands_into() { q "select count(*) from spawn_commands where session_id = '$1'"; }
state_of_task() { q "select state from mission_tasks where id = $1"; }
reported() { [ "$(state_of_task "$1")" = reported ]; }
# turn_field <agent> <step> <field>: a field of the agent's latest turn of
# that step in turns.log (3 execution, 6 native, 7 workspace, 8 files).
turn_field() { awk -v a="$1" -v s="$2" -v f="$3" '$2 == a && $5 == s { v = $f } END { print v }' "$WORK/turns.log"; }
stalled() { [ -n "$(turn_field "$1" commit 3)" ]; }
has() { # <comma list> <item>
    case ",$1," in *",$2,"*) return 0 ;; *) return 1 ;; esac
}

# --- the world's workers and the page ---------------------------------------------

capture "$SPAWN_BIN" mission add --text "042 restart and reassign"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
for label in restarted moved; do
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$label" \
        --brief "Do the 042 task and report."
    [ "$status" -eq 0 ] || harness "task assign ($label) failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $mission")
    agent=$(q "select agent_id from mission_tasks where id = $task")
    session=$(q "select session_id from mission_tasks where id = $task")
    [ -n "$agent" ] && [ -n "$session" ] || harness "task $task ($label) names no agent or session"
    eval "task_$label=\$task agent_$label=\$agent session_$label=\$session"
done
printf 'commit first\nwork\n' >"$WORK/ctl/$agent_restarted.plan"
printf 'commit second\n' >"$WORK/ctl/$agent_moved.plan"
printf 'work\n' >"$WORK/ctl/default.plan"
start_loop

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
base="http://127.0.0.1:$port"
# page_post <session> <verb> <body>: the status code; the answer in
# $WORK/post.json. As the page posts: JSON, the page's own Origin.
page_post() {
    curl -s --noproxy '*' -o "$WORK/post.json" -w '%{http_code}' -X POST \
        -H 'Content-Type: application/json' -H "Origin: $base" \
        --data "$3" "$base/api/session/$1/$2" 2>/dev/null || echo 000
}
session_json() { # <session> <file>
    code=$(curl -s --noproxy '*' -o "$2" -w '%{http_code}' "$base/api/session/$1?after=0&limit=1000" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/session/$1 answered $code: $(head -c 300 "$2" 2>/dev/null)"
}

wait_until 120 stalled "$agent_restarted" || harness "the first worker's first turn never ran: $(tail -n 5 "$WORK/turns.log" | tr '\n' ' ')"
wait_until 120 stalled "$agent_moved" || harness "the second worker's first turn never ran: $(tail -n 5 "$WORK/turns.log" | tr '\n' ' ')"
sleep 3
first=$(execution_n "$session_restarted" 1)
[ -n "$first" ] && ! ended "$first" || harness "the first worker's execution is not running while it stalls"
moved_first=$(execution_n "$session_moved" 1)
[ -n "$moved_first" ] && ! ended "$moved_first" || harness "the second worker's execution is not running while it stalls"
[ -f "$(turn_field "$agent_restarted" commit 7)/first.txt" ] ||
    harness "the first worker did not write first.txt in its workspace"

# --- (1) restart from the CLI -------------------------------------------------------------

capture "$SPAWN_BIN" task restart --id "$task_restarted" --reason "stalled on a hung tool"
[ "$status" -eq 0 ] || fail "(1) spawn task restart --id $task_restarted exited $status: $(err | head -c 300)"
wait_until 120 reported "$task_restarted" ||
    fail "(1) the restarted task was not reported within 120 s (task $(state_of_task "$task_restarted"); executions $(executions_of "$session_restarted"); records: $(records_of "$session_restarted" | tr '\n' ','))"
assert_eq "$(executions_of "$session_restarted")" 2 "(1) executions of the restarted worker"
second=$(execution_n "$session_restarted" 2)
assert_eq "$(turn_field "$agent_restarted" work 3)" "$second" "(1) the execution the restarted worker's second turn ran in"
assert_eq "$(records_of "$session_restarted")" "restarted final $first" "(1) the restarted worker's interruptions"
assert_eq "$(revival_count "$session_restarted")" 0 "(1) the recovery's revivals of the restarted worker"
assert_eq "$(q "select (c.action ->> 'type') || ' ' || (c.idempotency_key like 'restart-$task_restarted-%')::text
                  from agent_executions e join spawn_commands c on c.id = e.command_id
                 where e.execution_id = '$second'")" "resume true" \
    "(1) the command that started the second execution: a resume keyed restart-$task_restarted-..."
assert_eq "$(turn_field "$agent_restarted" work 6)" "$(turn_field "$agent_restarted" commit 6)" \
    "(1) the native session of the second execution (the first one's, resumed)"
assert_eq "$(turn_field "$agent_restarted" work 7)" "$(turn_field "$agent_restarted" commit 7)" \
    "(1) the workspace of the second execution (the first one's)"
has "$(turn_field "$agent_restarted" work 8)" first.txt ||
    fail "(1) the second execution's workspace lacks first.txt, the first one's work: $(turn_field "$agent_restarted" work 8)"
restarted=$(task_record task.restarted "$task_restarted")
[ -n "$restarted" ] || fail "(1) no task.restarted record names task $task_restarted"
assert_eq "$(printf '%s' "$restarted" | jq -r '[.session_id, .execution_id, .reason] | join(" ")')" \
    "$session_restarted $first stalled on a hung tool" "(1) the task.restarted record's session, execution and reason"
session_json "$session_restarted" "$WORK/restarted.json"
assert_eq "$(jq -r --arg e "$second" '.executions[] | select(.execution_id == $e) | .restart_of' "$WORK/restarted.json")" "$first" \
    "(1) the API's restart_of of the second execution"
assert_eq "$(jq -r --arg e "$first" '.executions[] | select(.execution_id == $e) | .restart_of' "$WORK/restarted.json")" null \
    "(1) the API's restart_of of the first execution"

# --- (2) reassign from the page -------------------------------------------------------------

code=$(page_post "$session_moved" reassign '{"reason":"stuck on the tests"}')
[ "$code" = 202 ] || fail "(2) POST /api/session/$session_moved/reassign answered $code: $(head -c 300 "$WORK/post.json")"
new_task=$(jq -r '.task' "$WORK/post.json")
new_session=$(jq -r '.session_id' "$WORK/post.json")
new_agent=$(jq -r '.agent_id' "$WORK/post.json")
assert_eq "$(jq -r '.principal' "$WORK/post.json")" human "(2) the principal the page's reassign answers"
[ -n "$new_task" ] && [ "$new_task" != null ] && [ "$new_task" != "$task_moved" ] ||
    fail "(2) the reassign answered no new task: $(cat "$WORK/post.json")"
[ -n "$new_session" ] && [ "$new_session" != null ] && [ "$new_session" != "$session_moved" ] ||
    fail "(2) the reassign answered no new session: $(cat "$WORK/post.json")"
assert_eq "$(q "select session_id || ' ' || agent_id || ' ' || kind || ' ' || mission_id || ' ' || brief from mission_tasks where id = $new_task")" \
    "$new_session $new_agent ephemeral $mission Do the 042 task and report." "(2) the new task's worker, kind, mission and brief"
wait_until 120 reported "$new_task" ||
    fail "(2) the reassigned task $new_task was not reported within 120 s (task $(state_of_task "$new_task"); old task $(state_of_task "$task_moved"))"
assert_eq "$(state_of_task "$task_moved")" cancelled "(2) the old task's state"
cancelled=$(task_record task.cancelled "$task_moved")
assert_eq "$(printf '%s' "$cancelled" | jq -r '[.by, (.reassigned_as | tostring), .reason] | join(" ")')" \
    "human $new_task stuck on the tests" "(2) the old task's task.cancelled: by, reassigned_as, reason"
assert_eq "$(task_record task.assigned "$new_task" | jq -r '.reassign_of | tostring')" "$task_moved" \
    "(2) the new task's task.assigned: reassign_of"
has "$(turn_field "$new_agent" work 8)" second.txt ||
    fail "(2) the fresh worker's workspace lacks second.txt, the old worker's checkpointed work: $(turn_field "$new_agent" work 8)"
[ "$(turn_field "$new_agent" work 7)" != "$(turn_field "$agent_moved" commit 7)" ] ||
    fail "(2) the fresh worker runs in the old worker's workspace"
assert_eq "$(executions_of "$session_moved")" 1 "(2) executions of the old worker"
ended "$moved_first" || fail "(2) the old worker's execution did not end"
# The recovery records an end on its own pass: give it the time a resume's
# backoff would take.
old_recorded() { [ -n "$(records_of "$session_moved")" ]; }
wait_until 30 old_recorded || true
assert_eq "$(records_of "$session_moved")" "reassigned final $moved_first" "(2) the old worker's interruptions"
assert_eq "$(q "select ev ->> 'to_session' from ($INTERRUPTED) r where r.e = '$moved_first'")" "$new_session" \
    "(2) the old worker's interruption's evidence: to_session"
assert_eq "$(revival_count "$session_moved")" 0 "(2) the recovery's revivals of the old worker"
session_json "$session_moved" "$WORK/old.json"
session_json "$new_session" "$WORK/new.json"
assert_eq "$(jq -r '[.reassigned_to.task, .reassigned_to.session_id] | map(tostring) | join(" ")' "$WORK/old.json")" \
    "$new_task $new_session" "(2) the API's reassigned_to of the old worker"
assert_eq "$(jq -r '[.reassigned_from.task, .reassigned_from.session_id] | map(tostring) | join(" ")' "$WORK/new.json")" \
    "$task_moved $session_moved" "(2) the API's reassigned_from of the fresh worker"

# --- (3) refusals ---------------------------------------------------------------------------

before=$(commands_into "$session_restarted")
capture "$SPAWN_BIN" task restart --id "$task_restarted" --reason again
[ "$status" -ne 0 ] || fail "(3) spawn task restart of the reported task $task_restarted exited 0"
printf '%s %s' "$(out)" "$(err)" | grep -q reported ||
    fail "(3) spawn task restart of the reported task did not name its state: $(err | head -c 300)"
supervisor_before=$(q "select count(*) from spawn_commands where session_id = 'session_supervisor' and principal = 'human'")
records_before=$(q "select count(*) from spawn_events where event_name = 'control.recorded' and $RECORD ->> 'type' like 'task.%'")
code=$(page_post session_supervisor restart '{"reason":"no task"}')
assert_eq "$code" 409 "(3) the page's restart of the supervisor (no delivered task): $(head -c 200 "$WORK/post.json")"
sleep 5
assert_eq "$(commands_into "$session_restarted")" "$before" "(3) commands into the reported task's worker after the refused restart"
assert_eq "$(q "select count(*) from spawn_commands where session_id = 'session_supervisor' and principal = 'human'")" \
    "$supervisor_before" "(3) commands into the supervisor after the refused page restart"
assert_eq "$(q "select count(*) from spawn_events where event_name = 'control.recorded' and $RECORD ->> 'type' like 'task.%'")" \
    "$records_before" "(3) task records after the two refusals"

# --- (4) the supervisor's charter -----------------------------------------------------------

charter_id=$(q "select id from spawn_commands where session_id = 'session_supervisor'
                 and action ->> 'type' = 'start' and action ? 'prompt' order by id desc limit 1")
[ -n "$charter_id" ] || harness "the supervisor was never started with a charter"
# The charter names each tool by the binary's path: `<spawn> task restart`.
for verb in " task restart --id" " task reassign --id"; do
    assert_eq "$(q "select position('$verb' in action ->> 'prompt') > 0 from spawn_commands where id = $charter_id")" t \
        "(4) the supervisor's charter names \`$verb\`"
done

echo "042: a stalled worker restarted from the CLI resumed its session on its workspace with its work; one reassigned from the page went to a fresh worker on its branch, the old one ended reassigned; the trace links both; refusals send nothing; the charter names both verbs"
exit 0
