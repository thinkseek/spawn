#!/bin/sh
# Journey j09, stall-cancel: no task waits silently, a cancelled task's work
# stays out of the next landing, and a task over its budget keeps its work.
#
# Folds Spawn scenarios 013 (2)-(4), 012 and 026 (design-gate-journeys-spawn.md
# §2, §3):
#   013 (2) the silent worker: exactly one escalation for its task, with step
#           `activity`, between the bound and the bound + 30 s after the
#           session's latest fact; its text names `task #<id>`, the step,
#           that fact's event name and the state of the session's latest
#           judgement;
#   013 (3) keyed by the stall: the silent worker makes one tool call and
#           goes silent again; that stall gets exactly one escalation of its
#           own, a new tag, and the first one is not sent again;
#   013 (4) none for healthy tasks: no stall tag for `busy` or `quick`,
#           which both report and are accepted.
#   013 (1), the 90 s delivery stall (the `stuck` resident whose pane takes
#           no text), is not here: it goes to the nightly (§3).
#   012 (0) a first task lands; (1) a task cancelled 3 s into a 60 s turn:
#           no tool call more than 15 s after the cancel, its turn never
#           finishes; (2) within 30 s the worker is told the task is
#           cancelled; (3) the cancelled task stays cancelled, no report, no
#           task.reported, no task.gated; (4) the next task starts without
#           one.txt, its gated candidate and main hold two.txt and zero.txt
#           and not one.txt; (5) a commit reachable from a ref of the source
#           holds one.txt.
#   026 (1) an overrun is `task.over_budget`, not a gate abort; (2) the
#           objective's next task continues it (`continues`, told the
#           branch, its commit and diff stat, its workspace holds the work);
#           (3) a second overrun is escalated to the supervisor once, the
#           objective is not issued again, 962 goes on, no halt.
#
# How the parts share setup:
#   Section L, one database, one tmux socket, one `spawn loop`, with 013's
#     loop-wide settings (supervisor on, stall bounds: activity 30 s,
#     delivery 90 s, the rest 600 s): 013's residents quiet, busy and quick
#     (013's mission, accept = review) and 012's resident core (012's gated
#     mission, its own source) run side by side; 012's part runs in a
#     subshell. 012 sets no SPAWN_STALL_* value (the defaults: activity 900,
#     review 1800, gate 3600, landing 600, stop 300, halted 0); 013's
#     shorter bounds make none of 012's tasks count as stalled (each is
#     delivered for seconds, with tool calls, then reported, judged and
#     landed, or cancelled), and 013 (4) checks that no task but the silent
#     one was escalated. 012 ran with no supervisor; here the supervisor
#     hears of 012's records too, and nothing 012 checks reads it. One
#     engine program serves both: the session picks 012's engine (core,
#     keys read one at a time) or 013's (every other session).
#     013's SPAWN_DELIVERY_NOT_READY_MS=15000 and its tmux wrapper that
#     refuses the stuck pane's text served only (1); they are not set here.
#   Section 026, side by side with L, in a database of its own with its own
#     loop: 026 sets every stall bound to 1800 s because keeper's task waits
#     idle by design, so under 013's 30 s activity bound keeper's task would
#     be escalated as a stall (013 (4) counts exactly that).
# The control records (spawn_events), spawn_commands and the mission_*
# tables are read where the folded scenarios read them, with their
# assertions.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
# Named check-*: the gate runs a failing check again, and on the base if it fails twice (design-gate-v2.md C2)
#
# Needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own
# databases, uses private tmux sockets, no network. Every wait is bounded in
# time.
set -eu

asserted=""
harness() {
    echo "j09: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
MEASURE=${BB_026_MEASURE-}

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed grep stty sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME in WORK: nothing here reads or writes the real home, nor
# is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
# The runtime directory too (backlog S20): what Spawn keeps there (a
# launch's environment file, its terminals' leaders) stays in WORK, never
# in the real one. systemd's user manager stays reachable by its bus.
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
. "$controller/scripts/blackbox/lib-herdr.sh"
herdr_setup "$WORK" "j09"
SPAWN_DB_NAME="bb_j09_stall_$run"
DB026="bb_j09_overrun_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

. "$controller/scripts/blackbox/assertions.sh"

# An assertion exits 1, unless the database is gone: then nothing the
# journey read can be trusted, and that is the harness's failure.
fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}

database_made=""
loop_pid=""
s012_pid=""
s026_pid=""
cleanup() {
    for pid in "$s012_pid" "$s026_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
    fi
    herdr_cleanup
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    sh "$controller/scripts/blackbox/scenario-db.sh" drop "$DB026" >/dev/null 2>&1 || true
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "j09: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "j09: the digests the supervisor received (id, tags):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, action -> 'tags' from spawn_commands
                where session_id = 'session_supervisor' and principal = 'spawn-dispatch'
                  and action ->> 'type' = 'input' and succeeded order by id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "j09: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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

# =============================================================================
# Section 026: a task over its budget keeps its work (scenario 026), in a
# database of its own with its own loop, run in a subshell beside section L.
# Its body is 026's.
# =============================================================================
section_026() {
    WORK=$WORK/s026
    mkdir -p "$WORK"
    herdr_setup "$WORK" "j09o"
    SPAWN_DB_NAME=$DB026
    export WORK SPAWN_DB_NAME
    asserted=""
    database_made=""
    loop_pid=""
    harness() {
        echo "j09: [026] harness: $*" >&2
        exit 2
    }
    s026_finish() {
        rc=$?
        trap - EXIT
        if [ "$rc" -ne 0 ]; then
            if [ -s "$WORK/loop.log" ]; then
                echo "j09: [026] the loop's last lines:" >&2
                tail -n 12 "$WORK/loop.log" >&2
            fi
            if [ -s "$WORK/integrate.log" ]; then
                echo "j09: [026] the integrator's last lines:" >&2
                grep -v '^\s*$' "$WORK/integrate.log" | tail -n 12 >&2
            fi
            if [ -n "$database_made" ]; then
                echo "j09: [026] the mission's task records:" >&2
                psql -d "$SPAWN_DB_NAME" -tA -c "select r ->> 'type', r ->> 'id', left(coalesce(r ->> 'reason', r ->> 'agent_id', ''), 120)
                    from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                    where e.session_id = 'session_control' and r ->> 'type' like 'task.%'
                      and r ->> 'type' not in ('task.delivered')
                    order by e.session_seq" >&2 2>/dev/null || true
            fi
        fi
        if [ -n "$loop_pid" ]; then
            kill "$loop_pid" 2>/dev/null || true
            wait "$loop_pid" 2>/dev/null || true
        fi
        herdr_cleanup
        if [ -n "$database_made" ]; then
            sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
        fi
        if [ "$rc" -eq 0 ]; then exit 0; fi
        if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
        [ "$rc" -eq 2 ] || echo "j09: [026] harness: unexpected exit $rc" >&2
        exit 2
    }
    trap s026_finish EXIT
    trap 'exit 130' INT TERM

    sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
    psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
        harness "cannot create the scenario database"
    database_made=1

    # A check that BB_026_MEASURE=1 only reports.
    failed_any=""
    measured_fail() {
        if [ -n "$MEASURE" ]; then
            failed_any=1
            echo "j09: [026] (measure) would fail: $*"
        else
            fail "$*"
        fi
    }

    repo=$WORK/repo.git
    source=$WORK/source.git
    git init -q --initial-branch=main "$WORK/seed"
    printf 'base\n' >"$WORK/seed/README.md"
    git -C "$WORK/seed" add -A
    git -C "$WORK/seed" commit -qm base
    for bare in "$repo" "$source"; do
        git init -q --bare --initial-branch=main "$bare"
        git -C "$WORK/seed" push -q "$bare" main:main
    done

    # The mission's scenarios directory: no gate runs here (nothing is
    # reported), it only has to exist and hold a scenario.
    gate=$WORK/gate
    mkdir -p "$gate"
    printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
    printf '# nothing protected\n' >"$gate/protected"
    cat >"$WORK/fake-gate.sh" <<'EOF'
#!/bin/sh
printf '{"verdict":"accepted"}\n' >"$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
    chmod 755 "$WORK/fake-gate.sh"

    backlog=$WORK/backlog
    mkdir -p "$backlog"
    printf '{"id": 961, "title": "Write the big file", "prompt": "Write big.txt.\\nBB:BIG\\nNothing else."}\n' \
        >"$backlog/961.json"
    printf '{"id": 962, "title": "Write the small file", "prompt": "Write small.txt.\\nBB:SMALL\\nNothing else."}\n' \
        >"$backlog/962.json"

    # 026's fake engine: a prompt; text typed in opens a turn, which ends
    # after a second of quiet, the transcript's end of the turn first, then
    # the prompt. `BB:BIG` notes whether big.txt was in the workspace (and
    # its sha256), writes it if not (120 lines), then shows a running turn
    # and makes a tool call every 0.2 s until it is stopped. `BB:SMALL`
    # writes small.txt and ends the turn.
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
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
calls() {
  printf '\n✻ Working… (esc to interrupt)\n\n❯ '
  n=0
  while [ "$n" -lt 3000 ]; do
    n=$((n + 1))
    id="toolu_${SPAWN_EXECUTION_ID}_$n"
    line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
    sleep 0.1
    line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
    printf '%s %s\n' "$(date +%s%3N)" "$n" >"$BB_WORK/calls-$SPAWN_SESSION_ID"
    sleep 0.1
  done
  while :; do sleep 3600; done
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
mode=
while :; do
  if bb_read_line typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:BIG"*) mode=big ;;
    *"BB:SMALL"*) mode=small ;;
    esac
  elif [ "$pending" = 1 ]; then
    case "$mode" in
    big)
      if [ -f big.txt ]; then
        found="found $(sha256sum <big.txt | cut -d' ' -f1)"
      else
        found=absent
      fi
      printf '%s %s %s\n' "$(date +%s%3N)" "$SPAWN_SESSION_ID" "$found" >>"$BB_WORK/found.log"
      if [ ! -f big.txt ]; then
        i=0
        while [ "$i" -lt 120 ]; do
          i=$((i + 1))
          printf 'the big work, line %s\n' "$i"
        done >big.txt
      fi
      calls
      ;;
    small) printf 'small\n' >small.txt ;;
    esac
    mode=
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    pending=0
  fi
done
ENGINE
    chmod 755 "$WORK/claude"

    for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
        SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_LOOP_INTERVAL_MS SPAWN_STALL_DELIVERY_SECS \
        SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS \
        SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS; do
        unset "$name" 2>/dev/null || true
    done
    SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
    SPAWN_GIT_REPO=$repo
    SPAWN_WORKSPACE_ROOT=$WORK/ws
    SPAWN_CLAUDE_PROGRAM=$WORK/claude
    SPAWN_CLAUDE_PROJECTS=$WORK/projects
    BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
    BB_WORK=$WORK
    SPAWN_WORKER=worker-bb
    SPAWN_SUPERVISOR=1
    SPAWN_MISSION_FILE=$WORK/mission.md
    SPAWN_LOOP_INTERVAL_MS=300
    SPAWN_DISK_WARN_GB=0
    SPAWN_DISK_FLOOR_GB=0
    # No stall is under test: keeper's task waits idle, by design.
    SPAWN_STALL_ACTIVITY_SECS=1800
    SPAWN_STALL_DELIVERY_SECS=1800
    SPAWN_STALL_REVIEW_SECS=1800
    SPAWN_STALL_GATE_SECS=1800
    SPAWN_STALL_LANDING_SECS=1800
    SPAWN_STALL_STOP_SECS=1800
    SPAWN_STALL_HALTED_SECS=1800
    export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
        SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_WORKER \
        SPAWN_SUPERVISOR SPAWN_MISSION_FILE SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB \
        SPAWN_DISK_FLOOR_GB SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
        SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
        SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
    mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"
    : >"$SPAWN_MISSION_FILE"
    : >"$WORK/found.log"

    tries=0
    while :; do
        capture "$SPAWN_BIN" init-db
        [ "$status" -eq 0 ] && break
        tries=$((tries + 1))
        [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
        sleep 1
    done

    now() { date +%s; }
    q() { scalar "$1"; }
    integrate_once() {
        capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
            SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
            "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
        cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
        [ "$status" -eq 0 ] || harness "integrate exited $status: $(err | tail -n 3)"
        return 0
    }
    # A task's control records of a type: `<at_ms>|<record>`, oldest first.
    records() {
        sql "select e.recorded_at_ms || '|' || (e.kind -> 'detail' -> 'record')::text
               from spawn_events e
              where e.session_id = 'session_control'
                and e.kind -> 'detail' -> 'record' ->> 'type' = '$2'
                and e.kind -> 'detail' -> 'record' ->> 'id' = '$1'
              order by e.session_seq"
    }
    record_at() { records "$1" "$2" | head -n 1 | cut -d'|' -f1; }
    record_of() { records "$1" "$2" | head -n 1 | cut -d'|' -f2-; }
    # The time of the overrun's record, whatever its type: `task.over_budget`
    # as required, `task.aborted` as before.
    overrun_at() {
        q "select min(e.recorded_at_ms) from spawn_events e
            where e.session_id = 'session_control'
              and e.kind -> 'detail' -> 'record' ->> 'type' in ('task.over_budget', 'task.aborted')
              and e.kind -> 'detail' -> 'record' ->> 'id' = '$1'"
    }
    objective_tasks() {
        sql "select id from mission_tasks where mission_id = $mission and objective = $1 order by id"
    }
    counter() { q "select $1 from mission_missions where id = $mission"; }
    calls_of() { # the tool calls the engine of a session has made
        if [ -f "$WORK/calls-$1" ]; then cut -d' ' -f2 "$WORK/calls-$1"; else echo 0; fi
    }
    mission_line() {
        q "select string_agg(id || ':' || coalesce(objective::text, '-') || ':' || state || ':' || agent_id, ' ' order by id)
             from mission_tasks where mission_id = $mission"
    }
    # wait_until <seconds> <shell condition>: integrator passes every 0.5 s
    # until the condition holds; 1 when the time ran out.
    wait_until() {
        limit=$(($(now) + $1))
        shift
        while :; do
            integrate_once
            if eval "$*"; then return 0; fi
            [ "$(now)" -lt "$limit" ] || return 1
            sleep 0.5
        done
    }
    # The supervisor's digests that name both tasks.
    told_both() {
        q "select count(*) from spawn_commands c
            where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
              and c.action ->> 'type' = 'input' and c.succeeded
              and c.action ->> 'text' ~ '#$1([^0-9]|\$)' and c.action ->> 'text' ~ '#$2([^0-9]|\$)'"
    }

    capture "$SPAWN_BIN" resident propose --name keeper --charter "Holds a task of the 026 mission."
    [ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name keeper
    [ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
    capture "$SPAWN_BIN" mission add --text "026 over budget" --source "$source"
    [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
    mission=$(jq -r .mission "$WORK/out.txt")
    [ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
    # keeper's own task of the mission: the resident is taken, so every
    # objective task goes to a per-task worker.
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker keeper \
        --brief "Hold this task open. BB:HOLD"
    [ "$status" -eq 0 ] || harness "task assign to keeper failed: $(err)"

    "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!

    capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate \
        --tool-calls 5 --backlog "$backlog" --resident keeper --workers 2
    [ "$status" -eq 0 ] || harness "mission configure failed: $(err)"

    # --- 026 (1) the first overrun ----------------------------------------------------

    echo "j09: [026] (1)"
    t1=""
    t1_delivered=""
    wait_until 90 '[ -n "$(objective_tasks 961 | head -n 1)" ] &&
        t1=$(objective_tasks 961 | head -n 1) &&
        t1_delivered=$(record_at "$t1" task.delivered) && [ -n "$t1_delivered" ]' ||
        harness "objective 961 was not assigned and delivered within 90 s (tasks: $(mission_line))"
    t1_agent=$(q "select agent_id from mission_tasks where id = $t1")
    t1_session=$(q "select session_id from mission_tasks where id = $t1")
    t1_kind=$(q "select kind from mission_tasks where id = $t1")
    [ "$t1_kind" = ephemeral ] ||
        harness "task #$t1 went to the resident ($t1_agent), not to a per-task worker"

    wait_until 45 '[ -n "$(overrun_at "$t1")" ]' ||
        fail "(1) task #$t1 made $(calls_of "$t1_session") tool calls on a budget of 5, and no record of its overrun came within 45 s of its delivery"
    t1_over=$(overrun_at "$t1")
    echo "j09: [026] task #$t1 delivered, its overrun recorded $((t1_over - t1_delivered)) ms later"
    aborted=$(record_of "$t1" task.aborted)
    if [ -n "$aborted" ]; then
        measured_fail "(1) task #$t1's budget overrun was recorded as task.aborted ($(printf '%s' "$aborted" | jq -r .reason)), a gate abort: the mission's gate_aborts went from 0 to $(counter gate_aborts), toward the halt after three; an overrun is a record of its own (task.over_budget) and counts toward no halt"
    fi
    over=$(record_of "$t1" task.over_budget)
    if [ -z "$over" ]; then
        measured_fail "(1) task #$t1 went over its budget and has no task.over_budget record"
    else
        [ -n "$(printf '%s' "$over" | jq -r '.reason // empty')" ] &&
            [ "$(printf '%s' "$over" | jq -r '(.tool_calls // 0) > 5')" = true ] &&
            [ "$(printf '%s' "$over" | jq -c '.budget')" = '{"tool_calls":5}' ] ||
            measured_fail "(1) task #$t1's task.over_budget record lacks its reason, its tool calls (more than 5) or its budget ({\"tool_calls\":5}): $over"
    fi
    [ "$(counter gate_aborts)" -eq 0 ] && [ "$(counter gate_rejections)" -eq 0 ] ||
        measured_fail "(1) after task #$t1's overrun the mission counts $(counter gate_aborts) gate aborts and $(counter gate_rejections) rejections; an overrun is neither"

    # --- 026 (2) the next task continues it -------------------------------------------

    echo "j09: [026] (2)"
    t2=""
    wait_until 60 't2=$(objective_tasks 961 | sed -n 2p) && [ -n "$t2" ]' ||
        fail "(2) objective 961 got no task after task #$t1's overrun within 60 s (tasks: $(mission_line))"
    t2_assigned=$(record_at "$t2" task.assigned)
    echo "j09: [026] task #$t2 assigned $((t2_assigned - t1_over)) ms after task #$t1's overrun"
    t2_agent=$(q "select agent_id from mission_tasks where id = $t2")
    t2_session=$(q "select session_id from mission_tasks where id = $t2")
    continues=$(record_of "$t2" task.assigned | jq -r '.continues // empty')
    [ "$continues" = "$t1" ] ||
        measured_fail "(2) task #$t2's task.assigned names continues=[$continues], not task #$t1, whose overrun it follows"
    t2_delivered=""
    wait_until 45 't2_delivered=$(record_at "$t2" task.delivered) && [ -n "$t2_delivered" ]' ||
        fail "(2) task #$t2 was not delivered within 45 s (tasks: $(mission_line))"
    # What T2's worker was told: every delivery command of the task.
    told=$(sql "select coalesce(action ->> 'text', action ->> 'prompt', '') from spawn_commands
                 where idempotency_key like 'deliver-$t2-%' order by id")
    printf '%s\n' "$told" >"$WORK/told-$t2.txt"
    branch=spawn/$t1_agent
    git --git-dir="$source" rev-parse -q --verify "refs/heads/$branch" >/dev/null ||
        measured_fail "(2) task #$t1's work never reached the source: it has no branch $branch"
    grep -qF -- "$branch" "$WORK/told-$t2.txt" ||
        measured_fail "(2) what task #$t2's worker was told does not name task #$t1's branch $branch ($(wc -c <"$WORK/told-$t2.txt") bytes, starting: $(head -c 160 "$WORK/told-$t2.txt" | tr '\n' ' '))"
    commit=""
    for hex in $(grep -oE '\b[0-9a-f]{7,40}\b' "$WORK/told-$t2.txt" | sort -u); do
        full=$(git --git-dir="$source" rev-parse -q --verify "$hex^{commit}" 2>/dev/null) || continue
        git --git-dir="$source" merge-base --is-ancestor "$full" "refs/heads/$branch" 2>/dev/null || continue
        git --git-dir="$source" cat-file -e "$full:big.txt" 2>/dev/null || continue
        commit=$full
        break
    done
    if [ -z "$commit" ]; then
        measured_fail "(2) what task #$t2's worker was told names no commit of $branch that holds task #$t1's big.txt"
    else
        stat=$(git --git-dir="$source" diff --shortstat main "$commit" | sed 's/^ *//')
        grep -qF -- "$stat" "$WORK/told-$t2.txt" && grep -qF big.txt "$WORK/told-$t2.txt" ||
            measured_fail "(2) what task #$t2's worker was told does not give the diff stat of ${commit%"${commit#???????}"} against main ('$stat', big.txt)"
    fi
    # The engine's first note on T2's session since T2's delivery (T2 may run
    # on T1's own agent, resumed: the same session).
    found_of_t2() {
        awk -v s="$t2_session" -v t="$t2_delivered" '$2 == s && $1 >= t { $1 = ""; $2 = ""; sub(/^  /, ""); print; exit }' \
            "$WORK/found.log"
    }
    wait_until 30 '[ -n "$(found_of_t2)" ]' ||
        fail "(2) task #$t2's engine never started the work within 30 s of its delivery"
    found=$(found_of_t2)
    want_sha=$(git --git-dir="$source" show "refs/heads/$branch:big.txt" 2>/dev/null | sha256sum | cut -d' ' -f1)
    [ -n "$commit" ] && want_sha=$(git --git-dir="$source" show "$commit:big.txt" | sha256sum | cut -d' ' -f1)
    echo "j09: [026] $branch against main once task #$t2 started: $(git --git-dir="$source" diff --shortstat main "refs/heads/$branch" 2>/dev/null | sed 's/^ *//')"
    [ "$found" = "found $want_sha" ] ||
        measured_fail "(2) task #$t2's workspace ($t2_agent) did not hold task #$t1's work: its engine found big.txt [$found], wanted the bytes of $branch's (sha256 $want_sha)"

    # --- 026 (3) the second overrun goes to the supervisor ----------------------------

    echo "j09: [026] (3)"
    wait_until 60 '[ -n "$(overrun_at "$t2")" ]' ||
        fail "(3) task #$t2 made $(calls_of "$t2_session") tool calls on a budget of 5, and no record of its overrun came within 60 s of its delivery"
    t2_over=$(overrun_at "$t2")
    echo "j09: [026] task #$t2 delivered, its overrun recorded $((t2_over - t2_delivered)) ms later"
    [ -z "$(record_of "$t2" task.aborted)" ] && [ -n "$(record_of "$t2" task.over_budget)" ] ||
        measured_fail "(3) task #$t2's overrun was not recorded as task.over_budget (task.aborted: $(record_of "$t2" task.aborted | jq -r '.reason // "none"'))"
    deadline=$(($(now) + 40))
    while :; do
        integrate_once
        if [ "$(told_both "$t1" "$t2")" -ge 1 ] && [ -n "$(objective_tasks 962)" ]; then break; fi
        if [ "$(now)" -ge "$deadline" ] || [ -n "$(q "select halted from mission_missions where id = $mission")" ]; then
            break
        fi
        sleep 0.5
    done
    for _ in 1 2 3 4 5; do integrate_once; sleep 0.3; done
    [ "$(told_both "$t1" "$t2")" -ge 1 ] ||
        measured_fail "(3) task #$t2, the second overrun on objective 961, was not escalated to the supervisor within 40 s: no digest it was told names #$t1 and #$t2"
    [ "$(told_both "$t1" "$t2")" -le 1 ] ||
        measured_fail "(3) the supervisor was told of the two overruns $(told_both "$t1" "$t2") times; once"
    tasks961=$(objective_tasks 961 | tr '\n' ' ')
    [ "$(objective_tasks 961 | wc -l)" -eq 2 ] ||
        measured_fail "(3) objective 961 was issued again after its second overrun: its tasks are now $tasks961(all tasks: $(mission_line))"
    [ -n "$(objective_tasks 962)" ] ||
        measured_fail "(3) objective 962 got no task while 961 was held after its second overrun (tasks: $(mission_line))"
    halted=$(q "select coalesce(halted, '') from mission_missions where id = $mission")
    [ -z "$halted" ] && [ "$(counter gate_aborts)" -eq 0 ] ||
        measured_fail "(3) after two overruns the mission counts $(counter gate_aborts) gate aborts and is halted: [$halted]"

    if [ -n "$failed_any" ]; then
        asserted=1
        exit 1
    fi
    echo "j09: [026] two overruns recorded as such, counted toward no halt; the second task continued the first's branch, told so; the second overrun went to the supervisor and 961 was not issued again"
    exit 0
}

( section_026 ) &
s026_pid=$!

# =============================================================================
# Section L: one database, one loop, 013's settings: 013 (2)-(4) in the
# foreground, 012 in a subshell beside it.
# =============================================================================

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

repo=$WORK/repo.git
git init -q --bare --initial-branch=main "$repo"
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git -C "$WORK/seed" push -q "$repo" main:main
# 012's source: seeded with the same base commit, as 012 seeds its repo and
# source.
mkdir -p "$WORK/p012"
git init -q --bare --initial-branch=main "$WORK/p012/source.git"
git -C "$WORK/seed" push -q "$WORK/p012/source.git" main:main

# 013's fake engine (every session but core): a prompt; text typed in opens
# a turn, which ends after a second of quiet, as Claude Code's does: the
# transcript's end of the turn first, then the prompt on the screen. It
# writes its transcript where Claude Code does. A turn whose text names
# `BB:WORK <file> <seconds>` is a task: a tool call every 0.5 s for that
# long, then the file and the report. One that names `BB:SILENT <name>`
# opens the turn and goes silent for good: nothing more in the transcript
# or on the screen, except one tool call each time the harness creates
# $BB_WORK/wake-<name>.
bb_engine >"$WORK/engine-013" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=$2 ;; esac
  shift
done
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
tool() {
  line "\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"$1\",\"name\":\"Bash\",\"input\":{}}]}"
  sleep 0.25
  line "\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$1\",\"content\":\"ok\"}]}"
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
work=
silent=
while :; do
  if bb_read_line typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:WORK "*) [ -n "$work" ] || work=${typed#*BB:WORK } ;;
    *"BB:SILENT "*) [ -n "$silent" ] || silent=${typed#*BB:SILENT } ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$silent" ]; then
      set -- $silent
      name=$1
      printf 'silent %s %s\n' "$(date +%s)" "$name" >>"$BB_WORK/engine.log"
      wakes=0
      while :; do
        if [ -e "$BB_WORK/wake-$name" ]; then
          rm -f "$BB_WORK/wake-$name"
          wakes=$((wakes + 1))
          tool "toolu_${name}_wake_$wakes"
          printf 'woke %s %s %s\n' "$(date +%s)" "$name" "$wakes" >>"$BB_WORK/engine.log"
        fi
        sleep 0.5
      done
    fi
    if [ -n "$work" ]; then
      set -- $work
      file=$1
      seconds=$2
      printf 'busy %s %s\n' "$(date +%s)" "$file" >>"$BB_WORK/engine.log"
      end=$(($(date +%s) + seconds))
      n=0
      while [ "$(date +%s)" -lt "$end" ]; do
        n=$((n + 1))
        tool "toolu_${file%%.*}_$n"
        printf 'tool %s\n' "$n"
        sleep 0.25
      done
      printf 'done\n' >"$file"
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$file" >.spawn/report.md
      printf 'wrote %s %s\n' "$(date +%s)" "$file" >>"$BB_WORK/engine.log"
      work=
    fi
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/engine-013"

# 012's fake engine (core's session). Keys are read one at a time, as
# Claude Code reads them: Enter ends a line, Escape (alone) or Ctrl-C
# interrupts a turn, text typed during a turn is held until it ends. Lines
# typed at the prompt make one turn, which starts after a second of quiet.
# Every turn ends the same way: its last tool call is still open when the
# prompt comes back on the screen, and a moment later the transcript
# records the tool's result and the turn's end together.
# engine.log lines (in $WORK/p012):
#   heard <time> <the turn's text on one line>
#   busy <time> <file> / sees <time> <file> <the *.txt in the workspace>
#   wrote <time> <file> / tool <time> <file> <n>
#   finished <time> <file> / interrupted <time> <file> / ended <time> <pid>
cat >"$WORK/p012/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=$2 ;; esac
  shift
done
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
count=0
# entry <fields>: one transcript line, into $entry.
entry() {
  count=$((count + 1))
  printf -v entry '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1"
}
line() {
  entry "$1"
  printf '%s' "$entry" >>"$transcript"
}
tool_use() {
  line "\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"$1\",\"name\":\"Bash\",\"input\":{}}]}"
}
tool_result() {
  entry "\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$1\",\"content\":\"$2\"$3}]}"
}
log() { printf '%s %s %s\n' "$1" "$(date +%s)" "$2" >>"$BB_WORK/engine.log"; }
trap 'log ended "$$"; exit 0' HUP TERM
stty -icanon -echo -isig min 1 2>/dev/null || true

text=      # the lines of the turn typed so far
held=      # lines typed during a turn, for the next one
current=   # the line being typed
stop=0     # an interrupt key came
# Ctrl-C is a key, as in Claude Code's raw mode: bash's `read` lets the
# terminal turn it into SIGINT, which is read back as the key.
sigint=0
trap 'sigint=1' INT
# key <timeout>: one key into $key. 0 a key, 1 the terminal is gone, 2 none.
key() {
  key=
  IFS= read -r -s -d '' -n 1 -t "$1" key
  status=$?
  if [ "$sigint" = 1 ]; then
    sigint=0
    key=$'\003'
    return 0
  fi
  [ "$status" -eq 0 ] && return 0
  [ "$status" -gt 128 ] && return 2
  return 1
}
# take <busy>: what the key in $key does.
take() {
  case "$key" in
  $'\e')
    # Escape alone interrupts; Escape followed at once by more is a sequence.
    if IFS= read -r -s -d '' -n 1 -t 0.05 next; then
      while IFS= read -r -s -d '' -n 1 -t 0.05 next; do
        case "$next" in [A-Za-z~]) break ;; esac
      done
    else
      [ "$1" = 1 ] && stop=1
    fi
    ;;
  $'\003') [ "$1" = 1 ] && stop=1 ;;
  $'\n' | $'\r')
    if [ -n "$current" ]; then
      printf 'you: %s\n' "$current"
      if [ "$1" = 1 ]; then
        held="$held$current"$'\n'
      else
        [ -n "$text" ] || line '"type":"user","message":{"role":"user","content":"prompt"}'
        text="$text$current"$'\n'
      fi
      current=
    fi
    ;;
  *) current=$current$key ;;
  esac
  return 0
}
# poll <ms>: keys for about that long, during a turn.
poll() {
  until_ms=$(($(date +%s%3N) + $1))
  while [ "$(date +%s%3N)" -lt "$until_ms" ]; do
    key 0.1
    case $? in
    0) take 1 ;;
    1) log ended "$$"; exit 0 ;;
    esac
    [ "$stop" = 1 ] && return 0
  done
}
# turn_ends <screen> <tool> <its result> <the turn's end>: the prompt comes
# back while the tool is still open, then the result and the end, at once.
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1.5
  tool_result "$2" "$3" "$4"
  result=$entry
  entry "$5"
  printf '%s%s' "$result" "$entry" >>"$transcript"
}
turn() {
  log heard "$(printf '%s' "$text" | tr '\n' ' ')"
  work=
  for word in $(printf '%s' "$text" | sed -n 's/.*BB:WORK \([A-Za-z0-9.]*\) \([0-9]*\).*/\1:\2/p'); do
    grep -qx "${word%%:*}" "$BB_WORK/started.list" 2>/dev/null && continue
    work=$word
    break
  done
  text=
  turn_end='"type":"system","subtype":"turn_duration","durationMs":1'
  if [ -z "$work" ]; then
    id="toolu_read_$count"
    tool_use "$id"
    printf 'tool read\n'
    sleep 0.5
    turn_ends idle "$id" ok "" "$turn_end"
    return 0
  fi
  file=${work%%:*}
  seconds=${work#*:}
  printf '%s\n' "$file" >>"$BB_WORK/started.list"
  log busy "$file"
  log sees "$file $(ls ./*.txt 2>/dev/null | tr '\n' ' ')"
  printf '%s\n' "${file%.txt}" >"$file"
  log wrote "$file"
  end=$(($(date +%s) + seconds))
  n=1
  id="toolu_${file%%.*}_$n"
  tool_use "$id"
  stop=0
  while :; do
    printf 'tool %s\n' "$n"
    log tool "$file $n"
    poll 400
    [ "$stop" = 1 ] && break
    [ "$(date +%s)" -lt "$end" ] || break
    tool_result "$id" ok ""
    printf '%s' "$entry" >>"$transcript"
    n=$((n + 1))
    id="toolu_${file%%.*}_$n"
    tool_use "$id"
  done
  if [ "$stop" = 1 ]; then
    stop=0
    log interrupted "$file"
    turn_ends 'Interrupted by user' "$id" '[Request interrupted by user for tool use]' \
      ',"is_error":true' \
      '"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}'
    return 0
  fi
  mkdir -p .spawn
  printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$file" >.spawn/report.md
  log finished "$file"
  turn_ends idle "$id" ok "" "$turn_end"
}

printf 'Welcome to Claude Code\n\n❯ '
while :; do
  if [ -n "$held" ] && [ -z "$text" ]; then
    line '"type":"user","message":{"role":"user","content":"prompt"}'
    text=$held
    held=
  fi
  key 1
  case $? in
  0) take 0 ;;
  1) log ended "$$"; exit 0 ;;
  2) [ -n "$text" ] && turn ;;
  esac
done
ENGINE
chmod 755 "$WORK/p012/claude"

# The loop's one engine program: 012's resident core runs 012's engine, with
# its own engine log; every other session runs 013's.
cat >"$WORK/claude" <<EOF
#!/bin/sh
case "\${SPAWN_SESSION_ID-}" in
session_resident_core)
  BB_WORK='$WORK/p012'
  export BB_WORK
  exec '$WORK/p012/claude' "\$@"
  ;;
esac
exec '$WORK/engine-013' "\$@"
EOF
chmod 755 "$WORK/claude"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_DELIVERY_RETRY_SECS \
    SPAWN_DELIVERY_NOT_READY_MS SPAWN_REISSUE_AFTER_SECS; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_SUPERVISOR=1
SPAWN_MISSION_FILE=$WORK/mission.md
# Free disk is not what this journey is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
# 013's stall bounds: short for the steps under test, long for the rest
# (the harness reviews 013's tasks at once; no halt, no cancel of them).
DELIVERY_BOUND=90
BOUND=30
SLACK=30
SPAWN_STALL_DELIVERY_SECS=$DELIVERY_BOUND
SPAWN_STALL_ACTIVITY_SECS=$BOUND
SPAWN_STALL_REVIEW_SECS=600
SPAWN_STALL_GATE_SECS=600
SPAWN_STALL_LANDING_SECS=600
SPAWN_STALL_STOP_SECS=600
SPAWN_STALL_HALTED_SECS=600
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_SUPERVISOR SPAWN_MISSION_FILE SPAWN_DISK_WARN_GB \
    SPAWN_DISK_FLOOR_GB SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/p012/integrator"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/engine.log"
: >"$WORK/p012/engine.log"
: >"$WORK/p012/started.list"

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

# --- 013's helpers ----------------------------------------------------------------

now() { date +%s; }
q() { scalar "$1"; }

# wait_for <seconds> <command...>: true once the command is, false past the
# limit.
wait_for() {
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        tend
        sleep 0.5
    done
}

# The session is judged waiting at its prompt, live.
waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$1'")" = WAITING ] &&
        [ "$(q "select count(*) from agent_executions
                where session_id = '$1' and ended_at_ms is null")" -ge 1 ]
}
task_state() { q "select state from mission_tasks where id = $1"; }

# The digests delivered to the supervisor that carry a stall tag for task
# `$1` (any step, or step `$2`): one row per (command, tag).
STALL_DIGESTS="select c.id, c.created_at_ms, tag
                 from spawn_commands c,
                      lateral jsonb_array_elements_text(
                          case when jsonb_typeof(c.action -> 'tags') = 'array'
                               then c.action -> 'tags' else '[]'::jsonb end) tag
                where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
                  and c.action ->> 'type' = 'input' and c.state = 'finished' and c.succeeded"
stall_digests() {
    session=$(q "select session_id from mission_tasks where id = $1")
    q "select count(*) from ($STALL_DIGESTS) d
        where d.tag like '$session stall-$1-${2:-}%'"
}
stall_tags() {
    session=$(q "select session_id from mission_tasks where id = $1")
    q "select count(distinct d.tag) from ($STALL_DIGESTS) d where d.tag like '$session stall-$1-%'"
}
escalated() { [ "$(stall_digests "$1" "$2")" -ge 1 ]; }
# Of the stall digests for task `$1`, the `$2`-th (1-based, oldest first):
# its command id.
nth_digest() {
    session=$(q "select session_id from mission_tasks where id = $1")
    q "select d.id from ($STALL_DIGESTS) d where d.tag like '$session stall-$1-%'
        order by d.id offset $(($2 - 1)) limit 1"
}
digest_at() { q "select created_at_ms from spawn_commands where id = $1"; }
digest_text() { sql "select action ->> 'text' from spawn_commands where id = $1"; }
# The line of digest `$1` about task `$2`: every line naming `task #<id>`.
digest_lines() { digest_text "$1" | grep -F "task #$2" || true; }
digest_tag() {
    session=$(q "select session_id from mission_tasks where id = $2")
    q "select tag from jsonb_array_elements_text((select action -> 'tags' from spawn_commands
                                                   where id = $1)) tag
        where tag like '$session stall-$2-%' limit 1"
}

# The latest fact of session `$1` recorded at or before `$2` (ms): a record
# that is not the Observer's, nor storage's. Prints `<recorded_at_ms> <event_name>`.
latest_fact_before() {
    q "select recorded_at_ms || ' ' || event_name from spawn_events
        where session_id = '$1' and source <> 'observer'
          and event_name not in ('artifact.committed', 'merge.conflicted')
          and recorded_at_ms <= $2
        order by session_seq desc limit 1"
}
# The state of the latest judgement of session `$1` recorded at or before `$2`.
judged_before() {
    q "select kind -> 'detail' -> 'observation' ->> 'state' from spawn_events
        where session_id = '$1' and source = 'observer' and event_name = 'observation.recorded'
          and recorded_at_ms <= $2
        order by session_seq desc limit 1"
}

# The supervisor gets a digest only while its latest judgement covers its
# latest fact. An Observer that writes a judgement only when it changes
# (objective 011 not solved) leaves an idle supervisor's stale at the first
# record after it, so a line is typed into it now and then (not as `human`:
# no news, and nothing the stall rules read). With 011 solved it is harmless.
pings=0
next_ping=0
ping_supervisor() {
    [ "$(now)" -ge "$next_ping" ] || return 0
    next_ping=$(($(now) + 8))
    pings=$((pings + 1))
    capture "$SPAWN_BIN" submit --principal bb-harness --agent agent_supervisor \
        --session session_supervisor --key "bb-ping-$pings" \
        --action '{"type":"input","text":"ping"}'
    [ "$status" -eq 0 ] || harness "submit of a ping to the supervisor failed: $(err)"
}

# The harness is 013's reviewer: a healthy task's report is accepted as soon
# as it is collected. Called while waiting, with the pings.
healthy=""
tend() {
    [ -z "$healthy" ] || ping_supervisor
    for id in $healthy; do
        if [ "$(task_state "$id")" = reported ]; then
            capture "$SPAWN_BIN" task accept --id "$id"
            [ "$status" -eq 0 ] || harness "task accept of #$id failed: $(err)"
        fi
    done
}

# --- the residents and the missions -----------------------------------------------

capture "$SPAWN_BIN" mission add --text "013 stalls"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"

for name in quiet busy quick; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 013 $name concern."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$name"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
    # Started with no prompt: tasks are typed in by the dispatcher. Not
    # submitted as `human`: a human's command is news for the supervisor.
    capture "$SPAWN_BIN" submit --principal bb-harness --agent "agent_resident_$name" \
        --session "session_resident_$name" --key "bb-start-$name" \
        --action '{"type":"start","engine":"claude","prompt":""}'
    [ "$status" -eq 0 ] || harness "submit of $name's start failed: $(err)"
done

# 012's gated mission and its resident core.
gate012=$WORK/p012/gate
mkdir -p "$gate012"
printf '#!/bin/sh\nexit 0\n' >"$gate012/check-ok.sh"
printf '# nothing protected\n' >"$gate012/protected"
capture "$SPAWN_BIN" mission add --text "012 cancelled work" --source "$WORK/p012/source.git" \
    --gate "$gate012" --accept gate
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission012=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission012" ] && [ "$mission012" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" resident propose --name core --charter "Owns the 012 concern."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name core
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

# =============================================================================
# Part 012: a cancelled task's work (scenario 012), in a subshell on section
# L's loop. Its body is 012's.
# =============================================================================
part_012() {
    WORK=$WORK/p012
    export WORK
    mission=$mission012
    source=$WORK/source.git
    asserted=""
    harness() {
        echo "j09: [012] harness: $*" >&2
        exit 2
    }
    p012_finish() {
        rc=$?
        trap - EXIT
        if [ "$rc" -ne 0 ]; then
            if [ -s "$WORK/integrate.log" ]; then
                echo "j09: [012] the integrator's last lines:" >&2
                tail -n 10 "$WORK/integrate.log" >&2
            fi
            if [ -s "$WORK/engine.log" ]; then
                echo "j09: [012] the engine's last lines:" >&2
                grep -v '^tool ' "$WORK/engine.log" | tail -n 15 >&2
            fi
        fi
        if [ "$rc" -eq 0 ]; then exit 0; fi
        if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
        [ "$rc" -eq 2 ] || echo "j09: [012] harness: unexpected exit $rc" >&2
        exit 2
    }
    trap p012_finish EXIT
    trap 'exit 130' INT TERM

    : >"$WORK/judged.log"
    # The recording gate: accepts every candidate and notes the candidate commit.
    cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
printf '%s %s %s\n' "\$(date +%s)" "\$2" "\$3" >>'$WORK/judged.log'
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
    chmod 755 "$WORK/fake-gate.sh"

    # wait_for <seconds> <command...>: true once the command is, false past
    # the limit.
    wait_for() {
        limit=$1
        shift
        deadline=$(($(now) + limit))
        until "$@"; do
            [ "$(now)" -lt "$deadline" ] || return 1
            sleep 0.25
        done
    }

    integrate_once() {
        capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
            SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
            "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
        cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
        [ "$status" -eq 0 ] || fail "integrate exited $status: $(err)"
        return 0
    }

    is_state() { [ "$(task_state "$1")" = "$2" ]; }
    records() {
        q "select count(*) from spawn_events
            where session_id = 'session_control'
              and kind -> 'detail' -> 'record' ->> 'type' = '$2'
              and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
    }
    rejections() {
        q "select count(*) from spawn_events
            where session_id = 'session_control'
              and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
              and kind -> 'detail' -> 'record' ->> 'verdict' = 'rejected'
              and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
    }
    main_has() { git --git-dir="$source" cat-file -e "main:$1" 2>/dev/null; }
    # The commit (in the source or the integrator's mirror) holds the file.
    holds() {
        git --git-dir="$source" cat-file -e "$1:$2" 2>/dev/null && return 0
        for mirror in "$WORK"/integrator/.sources/*.git; do
            git --git-dir="$mirror" cat-file -e "$1:$2" 2>/dev/null && return 0
        done
        return 1
    }
    engine_did() { grep -q "^$1 [0-9]* $2\$" "$WORK/engine.log"; }
    # The engine's last tool call for `$1`, as a time, or 0.
    last_tool() {
        sed -n "s/^tool \([0-9]*\) $1 [0-9]*\$/\1/p" "$WORK/engine.log" | tail -n 1 | grep . || echo 0
    }
    # A turn the engine heard after time `$1` names task `$2` and its cancel.
    told() {
        sed -n 's/^heard \([0-9]*\) \(.*\)$/\1 \2/p' "$WORK/engine.log" |
            while read -r at text; do
                [ "$at" -ge "$1" ] || continue
                printf '%s\n' "$text" | grep -Eiq "task[^0-9a-z]{0,3}$2([^0-9]|\$)" || continue
                printf '%s\n' "$text" | grep -iq 'cancel' || continue
                echo yes
                break
            done | grep -q yes
    }
    log_tail() { tail -n 6 "$WORK/integrate.log" 2>/dev/null | tr '\n' ' '; }

    # land <task> <what>: integrate as soon as the task is reported, until it
    # lands with no rejection.
    land() {
        deadline=$(($(now) + 150))
        until is_state "$1" landed; do
            [ "$(rejections "$1")" -eq 0 ] ||
                fail "$2: the gate rejected task $1 ($(q "select kind -> 'detail' -> 'record' ->> 'reason' from spawn_events
                    where kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                      and kind -> 'detail' -> 'record' ->> 'id' = '$1' order by id desc limit 1"))"
            [ "$(now)" -lt "$deadline" ] ||
                fail "$2: task $1 did not land within 150 s (state: $(task_state "$1"); integrate: $(log_tail))"
            case "$(task_state "$1")" in
            reported | accepted | landing) integrate_once ;;
            esac
            sleep 0.3
        done
    }

    assign() {
        capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker core \
            --brief "$1"
        [ "$status" -eq 0 ] || harness "task assign failed: $(err)"
        jq -r .task "$WORK/out.txt"
    }

    # --- 012 (0) the resident's landed work -------------------------------------------

    echo "j09: [012] (0)"
    zero=$(assign "Write the file.
BB:WORK zero.txt 2")
    [ -n "$zero" ] && [ "$zero" != null ] || harness "task assign printed no id"
    wait_for 90 engine_did wrote zero.txt ||
        harness "task $zero never reached the engine (state $(task_state "$zero"))"
    wait_for 60 is_state "$zero" reported ||
        harness "task $zero was not reported within 60 s (state $(task_state "$zero"))"
    land "$zero" "(0) the resident's first task"
    main_has zero.txt || harness "task $zero landed without zero.txt on main"

    # --- the cancel, in the middle of a turn ------------------------------------------

    one=$(assign "Write the file, then keep checking it for a minute.
BB:WORK one.txt 60")
    wait_for 90 engine_did wrote one.txt ||
        harness "task $one never reached the engine (state $(task_state "$one"))"
    sleep 3
    engine_did finished one.txt && harness "the turn of task $one ended before its cancel"
    is_state "$one" delivered || harness "task $one is $(task_state "$one"), not delivered, before its cancel"
    cancelled_at=$(now)
    capture "$SPAWN_BIN" task cancel --id "$one"
    [ "$status" -eq 0 ] || harness "task cancel failed: $(err)"

    # --- 012 (1) promptly stopped ------------------------------------------------------

    echo "j09: [012] (1)"
    sleep 20
    last=$(last_tool one.txt)
    [ "$last" -le $((cancelled_at + 15)) ] ||
        fail "(1) the worker was still working on cancelled task $one $((last - cancelled_at)) s after the cancel (its last tool call)"
    engine_did finished one.txt &&
        fail "(1) the worker finished the turn of cancelled task $one"

    # --- 012 (2) told -------------------------------------------------------------------

    echo "j09: [012] (2)"
    wait_for $((cancelled_at + 30 - $(now))) told "$cancelled_at" "$one" ||
        fail "(2) within 30 s of the cancel, the worker was told nothing that names task $one as cancelled"

    # --- the next task ------------------------------------------------------------------

    echo "j09: [012] (4)"
    two=$(assign "Write the file.
BB:WORK two.txt 2")
    wait_for 90 engine_did wrote two.txt ||
        fail "(4) the next task $two never reached the worker (state $(task_state "$two"))"
    wait_for 60 is_state "$two" reported ||
        fail "(4) task $two was not reported within 60 s (state $(task_state "$two"))"
    land "$two" "(4) the next task"

    # --- 012 (3) never collected --------------------------------------------------------

    echo "j09: [012] (3)"
    assert_eq "$(task_state "$one")" cancelled "(3) the state of cancelled task $one"
    assert_eq "$(sql "select coalesce(report, '') from mission_tasks where id = $one" | tr -d '\n')" "" \
        "(3) the report of cancelled task $one"
    assert_eq "$(records "$one" task.reported)" 0 "(3) task.reported records of cancelled task $one"
    assert_eq "$(records "$one" task.gated)" 0 "(3) task.gated records of cancelled task $one"
    engine_did finished one.txt &&
        fail "(1) the worker finished the turn of cancelled task $one after all"

    # --- 012 (4) not carried over -------------------------------------------------------

    echo "j09: [012] (4)"
    sees=$(sed -n 's/^sees [0-9]* two\.txt \(.*\)$/\1/p' "$WORK/engine.log" | tail -n 1)
    case " $sees " in
    *one.txt*) fail "(4) task $two started in a workspace that still holds cancelled task $one's one.txt ($sees)" ;;
    esac
    case " $sees " in
    *zero.txt*) ;;
    *) fail "(4) task $two started in a workspace without the landed zero.txt ($sees)" ;;
    esac
    assert_contains "$(sql "select coalesce(report, '') from mission_tasks where id = $two" | tr '\n' ' ')" two.txt \
        "(4) the report collected for task $two"
    assert_eq "$(rejections "$two")" 0 "(4) rejections of task $two"
    judged=$(tail -n 1 "$WORK/judged.log" | cut -d' ' -f3)
    [ -n "$judged" ] || harness "the gate judged nothing"
    holds "$judged" two.txt || fail "(4) the candidate the gate judged for task $two ($judged) lacks two.txt"
    holds "$judged" zero.txt || fail "(4) the candidate the gate judged for task $two ($judged) lacks zero.txt"
    holds "$judged" one.txt &&
        fail "(4) the candidate the gate judged for task $two ($judged) holds cancelled task $one's one.txt"
    main_has two.txt || fail "(4) main lacks two.txt after task $two landed"
    main_has zero.txt || fail "(4) main lost zero.txt"
    main_has one.txt && fail "(4) cancelled task $one's one.txt reached main with task $two"
    assert_eq "$(q "select gate_rejections from mission_missions where id = $mission")" 0 \
        "the mission's gate rejections"
    assert_eq "$(q "select coalesce(halted, '') from mission_missions where id = $mission")" "" \
        "the mission's halt"

    # --- 012 (5) not destroyed ----------------------------------------------------------

    echo "j09: [012] (5)"
    kept=$(git --git-dir="$source" rev-list --all | while read -r commit; do
        if [ "$(git --git-dir="$source" show "$commit:one.txt" 2>/dev/null)" = one ]; then
            echo "$commit"
            break
        fi
    done)
    [ -n "$kept" ] ||
        fail "(5) no commit reachable from a ref of the source holds cancelled task $one's one.txt: its work was destroyed"

    echo "j09: [012] the cancelled task's worker was stopped and told at once; its work stayed out of the next landing, and was kept"
    exit 0
}

( part_012 ) &
s012_pid=$!

# =============================================================================
# Part 013 (2)-(4), in the foreground on section L's loop.
# =============================================================================

for name in quiet busy quick; do
    wait_for 120 waiting "session_resident_$name" ||
        harness "resident $name was never judged waiting at its prompt"
done
wait_for 90 waiting session_supervisor || harness "the supervisor was never judged waiting"

assign() {
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker "$1" \
        --brief "$2"
    [ "$status" -eq 0 ] || harness "task assign for $1 failed: $(err)"
    jq -r .task "$WORK/out.txt"
}
task_quiet=$(assign quiet "BB:SILENT quiet")
task_busy=$(assign busy "BB:WORK busy.txt 60")
task_quick=$(assign quick "BB:WORK quick.txt 2")
for id in "$task_quiet" "$task_busy" "$task_quick"; do
    [ -n "$id" ] && [ "$id" != null ] || harness "task assign printed no id"
done
healthy="$task_busy $task_quick"

# The premises: the quiet worker took its task and went silent, the healthy
# ones were delivered.
went_silent() { grep -q "^silent [0-9]* quiet\$" "$WORK/engine.log"; }
wait_for 60 went_silent || harness "the quiet worker never took its task"
is_past_assigned() { [ "$(task_state "$1")" != assigned ]; }
for id in $healthy; do
    wait_for 60 is_past_assigned "$id" || harness "healthy task #$id was never delivered"
done

# --- 013 (2) the silent worker ----------------------------------------------------------

echo "j09: [013] (2)"
wait_for 120 escalated "$task_quiet" activity ||
    fail "(2) no escalation of task #$task_quiet stuck waiting for its worker's activity within 120 s (stall digests for it: $(stall_digests "$task_quiet"))"
digest=$(nth_digest "$task_quiet" 1)
at=$(digest_at "$digest")
set -- $(latest_fact_before session_resident_quiet "$at")
[ "$#" -eq 2 ] || harness "the quiet session has no fact before its escalation"
fact_at=$1
fact_name=$2
state=$(judged_before session_resident_quiet "$at")
[ $((at - fact_at)) -ge $((BOUND * 1000 - 1000)) ] ||
    fail "(2) task #$task_quiet was escalated $(((at - fact_at) / 1000)) s after its worker's latest fact ($fact_name), before the ${BOUND} s bound"
[ $((at - fact_at)) -le $(((BOUND + SLACK) * 1000)) ] ||
    fail "(2) task #$task_quiet was escalated $(((at - fact_at) / 1000)) s after its worker's latest fact (bound $BOUND s + $SLACK s)"
lines=$(digest_lines "$digest" "$task_quiet")
[ -n "$lines" ] || fail "(2) the escalation's text does not name task #$task_quiet: $(digest_text "$digest")"
for word in activity "$fact_name" "$state"; do
    printf '%s\n' "$lines" | grep -qF -- "$word" ||
        fail "(2) the escalation of task #$task_quiet does not name '$word' (the latest fact and judgement): $lines"
done
case "$(digest_tag "$digest" "$task_quiet")" in
"session_resident_quiet stall-$task_quiet-activity-"?*) ;;
*) fail "(2) the escalation's tag is not '<session> stall-<task>-activity-<n>': $(digest_tag "$digest" "$task_quiet")" ;;
esac
first_quiet_tag=$(digest_tag "$digest" "$task_quiet")

# --- 013 (3) it moved, and stalled anew --------------------------------------------------

echo "j09: [013] (3)"
# Hold a while first: the same stall is not escalated again.
hold_until=$(($(now) + 20))
while [ "$(now)" -lt "$hold_until" ]; do
    tend
    sleep 0.5
done
assert_eq "$(stall_digests "$task_quiet")" 1 "(3) escalations of task #$task_quiet before it moved"
: >"$WORK/wake-quiet"
woke() { grep -q "^woke [0-9]* quiet 1\$" "$WORK/engine.log"; }
wait_for 30 woke || harness "the quiet worker did not make its tool call"
two_tags() { [ "$(stall_tags "$task_quiet")" -ge 2 ]; }
wait_for $((BOUND + SLACK + 30)) two_tags ||
    fail "(3) task #$task_quiet moved (one tool call) and stalled again, but was not escalated again within $((BOUND + SLACK + 30)) s"
second=$(nth_digest "$task_quiet" 2)
second_tag=$(digest_tag "$second" "$task_quiet")
[ "$second_tag" != "$first_quiet_tag" ] ||
    fail "(3) the new stall of task #$task_quiet reused the first one's tag ($second_tag)"
case "$second_tag" in
"session_resident_quiet stall-$task_quiet-activity-"?*) ;;
*) fail "(3) the second escalation's tag is not '<session> stall-<task>-activity-<n>': $second_tag" ;;
esac
woke_at=$(q "select max(recorded_at_ms) from spawn_events
              where session_id = 'session_resident_quiet' and event_name = 'tool.completed'")
[ "$(digest_at "$second")" -ge $((woke_at + BOUND * 1000 - 1000)) ] ||
    fail "(3) the new stall of task #$task_quiet was escalated before the bound after its new fact"

# --- 013 (4) the window closes: exactly one per stall, none for the healthy -------------

echo "j09: [013] (3)(4)"
# 20 s past (3), so a second escalation of the stall, or a first of a
# healthy task, has time to show before the counts below.
hold_until=$(($(now) + 20))
while [ "$(now)" -lt "$hold_until" ]; do
    tend
    sleep 0.5
done
is_accepted() { [ "$(task_state "$1")" = accepted ]; }
for id in $healthy; do
    wait_for 60 is_accepted "$id" ||
        fail "(4) healthy task #$id was not reported and accepted (state $(task_state "$id"))"
done
assert_eq "$(stall_digests "$task_quiet")" 2 "(3) escalations of task #$task_quiet (two stalls)"
assert_eq "$(stall_tags "$task_quiet")" 2 "(3) stall tags for task #$task_quiet"
for id in $healthy; do
    assert_eq "$(stall_digests "$id")" 0 "(4) escalations of healthy task #$id"
done
# Every stall tag of the loop: 013's healthy tasks and 012's tasks (sharing
# the loop) get none.
others=$(q "select count(*) from ($STALL_DIGESTS) d
             where d.tag like '% stall-%'
               and d.tag not like 'session_resident_quiet stall-$task_quiet-%'")
assert_eq "$others" 0 "(4) stall escalations about anything but the stalled task"
[ "$(task_state "$task_quiet")" = delivered ] ||
    harness "the quiet task left 'delivered' ($(task_state "$task_quiet"))"
echo "j09: [013] the silent worker was escalated once per stall, within the bound, naming the task, the step and the evidence; the healthy tasks were not"

# --- the verdicts of the parts beside it --------------------------------------------------

rc012=0
wait "$s012_pid" || rc012=$?
s012_pid=""
rc026=0
wait "$s026_pid" || rc026=$?
s026_pid=""
# An assertion in either part decides before a harness failure in the other.
for part in 012 026; do
    eval "rc=\$rc$part"
    if [ "$rc" -eq 1 ]; then
        asserted=1
        echo "j09: [$part] an assertion failed (above)" >&2
        exit 1
    fi
done
[ "$rc012" -eq 0 ] || harness "part 012 exited $rc012"
[ "$rc026" -eq 0 ] || harness "part 026 exited $rc026"

echo "j09: a silent worker was escalated once per stall and healthy ones never; a cancelled task's worker was stopped and told, its work kept off main and on a ref; an overrun's work was continued, a second overrun escalated, no halt"
exit 0
