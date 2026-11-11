#!/bin/sh
# Journey j10, verdicts: what the integrator records when a gate's outcome is
# not a plain verdict on the work, and that every record says whether it is a
# statement about the code or about the infrastructure.
#
# It folds these Spawn scenarios (rsi/scenarios/NNN.sh), every numbered check
# of each unless named:
#   016 (1)-(5)  a candidate that fails only once main is merged is
#                `incompatible`, not counted toward the halt, sent back with the
#                gate's line and main's commits, and lands after the fix; one
#                whose own head fails is `rejected` and counted;
#   020 (1)-(5)  check-tests failures that are all connection errors (or a
#                crash in the test database's log during the gate) are
#                `host_fault`, judged again after a doubling backoff, two in a
#                row escalated, never halted; a real failure among them, or
#                database errors outside a crash window, stay `rejected`;
#   033 (1)-(5)  a base that fails then passes is `host_fault`
#                (`base_disagrees`), no abort, no escalation at two, an alert at
#                three; other base harness errors stay `aborted`;
#   018 (1)-(4)  short disk, memory or test database: `no_room` naming the
#                check, the measure and the threshold, the gate not run; with
#                room the gate runs and the task lands;
#   008 (b)      a merge git cannot run records no gate verdict (no `conflict`
#                sent to the worker) and lands with a working git. 008 (a), the
#                git-floor refusal, is not here: it moves to CI (step 3);
#   035 `class`  every `task.gated` carries `class`: `code` for accepted and
#                rejected, `infrastructure` for host_fault and aborted (035's
#                own four gates), with gate_rejections as 035 checks them. 035's
#                `inputs` and `env` field shapes are not here: they move to CI.
#                Added: at the end of each lane every `task.gated` this journey
#                recorded (all of the verdicts above) carries the class its
#                verdict names in 035's contract.
#
# Shape. Two lanes run side by side, each with its own database, tmux socket
# and loop (`spawn loop`: Bridge, pusher, dispatcher, supervisor on the fake
# engine, no model judge); each part has missions of its own, so no counter
# is shared. Every judging is a `spawn integrate --once` pass the journey
# runs with the part's own environment and recording gate (SPAWN_GATE), as
# the scenario did: a pass judges every gated mission in the database, so
# within a lane the parts run one after another and a part's missions get
# their gate (`mission configure --gate --accept gate`) only when the part
# starts and lose it (`--accept review --no-gate`) when it ends.
#   lane A: one loop, 13 residents, all tasks assigned at once and all
#           reported before any part judges; then 016, 020, 018, 035 in turn.
#   lane B: 008 (b) first (no loop: `spawn tick` drives its ephemeral worker,
#           as 008 did), then one loop for 033. 033 runs beside lane A
#           because its backoffs (4 + 8 + 16 s) are the journey's floor.
# Each step echoes `j10: [NNN (n)]` before the checks it carries. The
# traceability is /var/tmp/spawn/sync/gate-journeys-spawn-proof/trace/j10.tsv.
#
# On Spawn live main 13fc93a7 no `task.gated` carries `class` (objective 035
# is open there: its `inputs` and `env` landed with objective 036, its
# `class` did not), so there the journey exits 1 at its class steps, which
# come after every other check of their lane; 035.sh fails there at (1) too.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
# Named check-*: the gate runs a failing check again, and on the base if it fails twice (design-gate-v2.md C2)
#
# Runs inside the gate's sandbox (scripts/rsi/gate.sh), from the root of the
# tree under test: needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and
# drops its own databases, uses private tmux sockets, no network. Every wait
# is bounded in time.
set -eu

harness() {
    echo "j10: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep awk date df cut sha1sum; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
real_git=$(command -v git)
real_version=$("$real_git" --version | sed -n 's/^git version \([0-9]*\)\.\([0-9]*\).*/\1 \2/p')
[ -n "$real_version" ] || harness "cannot read the real git's version"
set -- $real_version
[ "$1" -gt 2 ] || { [ "$1" -eq 2 ] && [ "$2" -ge 43 ]; } ||
    harness "the real git is $1.$2; 008 (b) needs 2.43 or later"

TEST_URL=$SPAWN_TEST_DATABASE_URL
SPAWN_PSQL=psql
export SPAWN_PSQL
# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

# The loop-wide environment both lanes share; each lane adds its own.
for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_GATE_MIN_FREE_GB \
    SPAWN_GATE_MIN_FREE_MEMORY_MB SPAWN_INTEGRATE_GATES SPAWN_TEST_DATABASE_LOG \
    SPAWN_GATE_HOST_FAULT_BACKOFF_SECS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_INTEGRATE_WARM; do
    unset "$name" 2>/dev/null || true
done
# No base warm after a landing (035's setting, here for every part): a warm
# runs the part's recording gate with `--base-only` in a later pass, which
# would be noise in that part's judged.log. It decides nothing.
SPAWN_INTEGRATE_WARM=0
export SPAWN_INTEGRATE_WARM

# --- shared by both lanes ---------------------------------------------------------

# Set by each lane (in its own subshell): WORK, SPAWN_DB_NAME, SPAWN_TMUX_SOCKET,
# lane, loop_pid, database_made, asserted, P (the part running).
lane_cleanup() {
    if [ -n "${loop_pid-}" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
    fi
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
    if [ -n "${database_made-}" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
lane_finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ "$rc" -ne 130 ]; then
        echo "j10: lane $lane failed in part ${P##*/}" >&2
        if [ -s "$WORK/loop.log" ]; then
            echo "j10: lane $lane: the loop's last lines:" >&2
            tail -n 12 "$WORK/loop.log" >&2
        fi
        for log in integrate judged passes engine; do
            if [ -n "${P-}" ] && [ -s "$P/$log.log" ]; then
                echo "j10: lane $lane, ${P##*/}: $log.log:" >&2
                grep -v '^\s*$' "$P/$log.log" | grep -v '^| ' | tail -n 15 >&2
            fi
        done
        if [ -n "${database_made-}" ]; then
            echo "j10: lane $lane: the gate records (task verdict class reason):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select r ->> 'id', r ->> 'verdict', coalesce(r ->> 'class', '-'),
                       left(coalesce(r ->> 'reason', ''), 120)
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control' and r ->> 'type' = 'task.gated'
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    lane_cleanup
    if [ "$rc" -eq 0 ]; then
        code=0
    elif [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then
        code=1
    elif [ "$rc" -eq 130 ]; then
        code=130
    else
        [ "$rc" -eq 2 ] || echo "j10: harness: lane $lane: unexpected exit $rc" >&2
        code=2
    fi
    # The parent reads a lane's result here (an exited lane it has not
    # waited for still answers kill -0).
    echo "$code" >"$lanes/$lane.rc"
    exit "$code"
}

# lane_world <a|b>: the lane's scratch, database and tmux socket.
lane_world() {
    lane=$1
    asserted=""
    database_made=""
    loop_pid=""
    P=""
    WORK=$(mktemp -d) || harness "mktemp failed"
    # Not $$ alone: the gate's PID namespace makes it the same small number.
    run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
    SPAWN_DB_NAME="bb_j10${lane}_verdicts_$run"
    SPAWN_TMUX_SOCKET="bb-j10${lane}-$run"
    export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET
    trap lane_finish EXIT
    trap 'exit 130' INT TERM
    # Logins of this lane's databases that a run killed before its cleanup
    # left behind (scripts/blackbox/scenario-db.sh): this prefix only.
    sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
    psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
        harness "cannot create the lane $lane database"
    database_made=1

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
    # 033's and 035's helpers (033's BB_033_MEASURE is not carried: a check
    # always fails here, as it does in the gate).
    measured_fail() { fail "$*"; } #@ 033:214 #@ 033:216
    expect_eq() { # <got> <want> <what>
        [ "$1" = "$2" ] || measured_fail "$3: expected [$2], got [$1]" #@ 033:220 #@ 035:183
    }

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

    cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
    chmod 755 "$WORK/tmux"
}

# The loop's environment (020's, 033's: the supervisor on, every stall bound
# out of the way) and its engine.
lane_loop_env() {
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
    # Free disk is not what this journey is about: no warning, no floor.
    SPAWN_DISK_WARN_GB=0
    SPAWN_DISK_FLOOR_GB=0
    SPAWN_SUPERVISOR=1
    SPAWN_MISSION_FILE=$WORK/mission.md
    SPAWN_STALL_DELIVERY_SECS=1800
    SPAWN_STALL_ACTIVITY_SECS=1800
    SPAWN_STALL_REVIEW_SECS=1800
    SPAWN_STALL_GATE_SECS=1800
    SPAWN_STALL_LANDING_SECS=1800
    SPAWN_STALL_STOP_SECS=1800
    SPAWN_STALL_HALTED_SECS=1800
    export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
        SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
        SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_SUPERVISOR \
        SPAWN_MISSION_FILE SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
        SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
        SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
    mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"
    : >"$SPAWN_MISSION_FILE"

    # The fake engine: the scenarios' engines in one, by what the resident's
    # part file names ($BB_WORK/part-<name>):
    #   m016 (016's): BB:FIELDS <f...> struct.txt and every file under uses/
    #        name <f...>; BB:USE uses/<name>.txt names struct.txt's fields;
    #        BB:BREAK those and `z`; a revision waits, its turn open, until
    #        $BB_WORK/fix-<name> exists, then names struct.txt's fields;
    #   w035 (035's): BB:WRITE:<file> writes <file>.txt;
    #   write (020's, 018's, 033's): BB:WRITE writes <name>.txt;
    # anything else is a turn that changes nothing. Once the source
    # ($BB_WORK/src-<name>, else $BB_WORK/source.git) holds the file on
    # spawn/<agent> (or 30 s passed), a second turn writes the report. A
    # session that is not a resident (the supervisor) takes no job.
    cat >"$WORK/claude" <<'ENGINE'
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
case "$SPAWN_SESSION_ID" in
session_resident_*) part=$(cat "$BB_WORK/part-$name" 2>/dev/null || echo write) ;;
*) part=none ;;
esac
src=$(cat "$BB_WORK/src-$name" 2>/dev/null || echo "$BB_WORK/source.git")
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
# pushed <file>: the source's spawn/<agent> holds <file> as the workspace does.
pushed() {
  [ "$(git --git-dir="$src" show "refs/heads/spawn/$agent:$1" 2>/dev/null)" = "$(cat "$1")" ]
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
job=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$part" in
    m016)
      case "$typed" in
      *"BB:FIELDS "*) [ -n "$job" ] || job="fields ${typed#*BB:FIELDS }" ;;
      *"BB:USE"*) [ -n "$job" ] || job=use ;;
      *"BB:BREAK"*) [ -n "$job" ] || job=break ;;
      *"[Review of your report on task #"*) [ -n "$job" ] || job=revise ;;
      esac
      ;;
    w035)
      case "$typed" in
      *"BB:WRITE:"*)
        job=${typed#*BB:WRITE:}
        job=${job%%[!a-z]*}
        [ -z "$job" ] || job="file $job"
        ;;
      esac
      ;;
    write)
      case "$typed" in *"BB:WRITE"*) job=write ;; esac
      ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$job" ]; then
      mkdir -p uses
      file=uses/$name.txt
      case "$job" in
      fields\ *)
        fields=${job#fields }
        printf '%s\n' "$fields" >struct.txt
        for used in uses/*.txt; do printf '%s\n' "$fields" >"$used"; done
        file=struct.txt
        ;;
      use) cp struct.txt "$file" ;;
      break) printf '%s z\n' "$(cat struct.txt)" >"$file" ;;
      revise)
        printf 'revision %s %s\n' "$(date +%s)" "$name" >>"$BB_WORK/engine.log"
        until [ -e "$BB_WORK/fix-$name" ]; do sleep 0.5; done
        cp struct.txt "$file"
        ;;
      file\ *)
        file=${job#file }.txt
        printf '%s wrote %s\n' "$name" "${job#file }" >"$file"
        ;;
      write)
        file=$name.txt
        printf '%s was here\n' "$name" >"$file"
        ;;
      esac
      printf 'wrote %s %s %s %s\n' "$(date +%s)" "$name" "$file" "$(cat "$file")" >>"$BB_WORK/engine.log"
      turn_ends "wrote $file"
      start=$(date +%s)
      until pushed "$file"; do
        [ $(($(date +%s) - start)) -ge 30 ] && break
        sleep 0.2
      done
      line '"type":"user","message":{"role":"user","content":"report"}'
      sleep 2
      mkdir -p .spawn
      if [ "$part" = m016 ]; then
        printf '## Outcome\ndone\n## What changed\n%s now names %s\n## Open issues\nnone\n' \
          "$file" "$(cat "$file")" >.spawn/report.md
      else
        printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$file" >.spawn/report.md
      fi
      printf 'reported %s %s\n' "$(date +%s)" "$name" >>"$BB_WORK/engine.log"
      job=
    fi
    turn_ends idle
    pending=0
  fi
done
ENGINE
    chmod 755 "$WORK/claude"
    : >"$WORK/engine.log"
}

lane_init_db() {
    # The test server is shared: another run's init-db can collide on
    # cluster-wide objects ("tuple concurrently updated"). init-db is idempotent.
    tries=0
    while :; do
        capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
        [ "$status" -eq 0 ] && break
        tries=$((tries + 1))
        [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
        sleep 1
    done
}

now() { date +%s; }
now_ms() { echo $(($(date +%s%N) / 1000000)); }
q() { scalar "$1"; }
task_state() { q "select state from mission_tasks where id = $1"; }
revisions() { q "select revisions from mission_tasks where id = $1"; }
tasks_line() { q "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks"; }
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }

propose() { # <resident> <charter>
    capture "$SPAWN_BIN" resident propose --name "$1" --charter "$2"
    [ "$status" -eq 0 ] || harness "resident propose $1 failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$1"
    [ "$status" -eq 0 ] || harness "resident approve $1 failed: $(err)"
}
new_mission() { # <text> <source>: prints the id
    capture "$SPAWN_BIN" mission add --text "$1" --source "$2"
    [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
    id=$(jq -r .mission "$WORK/out.txt")
    [ -n "$id" ] && [ "$id" != null ] || harness "mission add printed no id: $(out)"
    echo "$id"
}
assign_to() { # <mission> <resident> <brief>: prints the id
    capture "$SPAWN_BIN" task assign --mission "$1" --kind resident --worker "$2" --brief "$3"
    [ "$status" -eq 0 ] || harness "task assign for $2 failed: $(err)"
    id=$(jq -r .task "$WORK/out.txt")
    [ -n "$id" ] && [ "$id" != null ] || harness "task assign for $2 printed no id"
    echo "$id"
}
# A part's missions get their gate when the part starts, and lose it when it
# ends: a pass judges every gated mission in the database.
gate_on() { # <mission> <gate dir>
    capture "$SPAWN_BIN" mission configure --id "$1" --gate "$2" --accept gate
    [ "$status" -eq 0 ] || harness "mission configure $1 exited $status: $(err)"
}
gate_off() { # <mission>...
    for id in "$@"; do
        capture "$SPAWN_BIN" mission configure --id "$id" --accept review --no-gate
        [ "$status" -eq 0 ] || harness "mission configure $id (no gate) exited $status: $(err)"
    done
}

# The session is judged waiting at its prompt, live.
waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$1'")" = WAITING ] &&
        [ "$(q "select count(*) from agent_executions
                where session_id = '$1' and ended_at_ms is null")" -ge 1 ]
}
# The supervisor gets a digest only while its latest judgement covers its
# latest fact: a line is typed into it now and then (not as `human`: no
# news), as objective 013's scenario does.
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
# The digests delivered to the supervisor whose tags match `$1` (SQL like).
DIGEST_TAGS="select c.id, tag from spawn_commands c,
                lateral jsonb_array_elements_text(
                    case when jsonb_typeof(c.action -> 'tags') = 'array'
                         then c.action -> 'tags' else '[]'::jsonb end) tag
              where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
                and c.action ->> 'type' = 'input' and c.state = 'finished' and c.succeeded"
digests_like() { q "select count(*) from ($DIGEST_TAGS) d where d.tag like '$1'"; }
digest_text_tagged() {
    sql "select action ->> 'text' from spawn_commands
          where id = (select min(d.id) from ($DIGEST_TAGS) d where d.tag = '$1')"
}

# [035 class] every `task.gated` of the lane carries the class its verdict
# names in 035's contract.
classes_hold() {
    echo "j10: [035 class] every task.gated of lane $lane carries its class"
    wrong=$(q "select coalesce(string_agg((r ->> 'id') || ':' || (r ->> 'verdict') || ':' ||
                                         coalesce(r ->> 'class', '(none)'), ' ' order by e.id), '')
                 from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control' and r ->> 'type' = 'task.gated'
                  and coalesce(r ->> 'class', '') <> case r ->> 'verdict'
                        when 'accepted' then 'code' when 'rejected' then 'code'
                        when 'conflict' then 'code' when 'incompatible' then 'code'
                        when 'aborted' then 'infrastructure' when 'host_fault' then 'infrastructure'
                        when 'no_room' then 'infrastructure' when 'stale' then 'superseded'
                        else '?' end")
    gated=$(q "select count(*) from spawn_events where session_id = 'session_control'
                 and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'")
    assert_eq "$wrong" "" "(035 class) the task.gated records (of $gated) whose class is not their verdict's (task:verdict:class)"
}

# =================================================================================
# Lane A: 016, 020, 018, 035 on one loop
# =================================================================================

lane_a() {
    lane_world a

    # One seed for every source of the lane (the workspaces are cloned from
    # repo.git and merge their mission's main at each delivery): 016's
    # struct.txt and uses/base.txt (`a b`), and a README.
    repo=$WORK/repo.git
    s016=$WORK/s016.git
    source=$WORK/source.git
    git init -q --initial-branch=main "$WORK/seed"
    printf 'base\n' >"$WORK/seed/README.md"
    printf 'a b\n' >"$WORK/seed/struct.txt"
    mkdir -p "$WORK/seed/uses"
    printf 'a b\n' >"$WORK/seed/uses/base.txt"
    git -C "$WORK/seed" add -A
    git -C "$WORK/seed" commit -qm "base: Facts {a, b}"
    for bare in "$repo" "$s016" "$source"; do
        git init -q --bare --initial-branch=main "$bare"
        git -C "$WORK/seed" push -q "$bare" main:main
    done
    lane_loop_env
    lane_init_db

    # The residents: their part (the engine's job) and their mission's source.
    for name in alpha beta gamma; do
        echo m016 >"$WORK/part-$name"
        echo "$s016" >"$WORK/src-$name"
        propose "$name" "Owns the 016 $name change."
    done
    for name in hostA mixed olddb hostB; do
        lower=$(printf '%s' "$name" | tr 'A-Z' 'a-z')
        echo write >"$WORK/part-$lower"
        propose "$lower" "Owns the 020 $name change."
    done
    for name in disk memory dbase room; do
        echo write >"$WORK/part-$name"
        propose "$name" "Owns the 018 $name change."
    done
    for name in binda bindb; do
        echo w035 >"$WORK/part-$name"
        propose "$name" "Owns the 035 change."
    done

    m016=$(new_mission "016 merged candidates" "$s016")
    m020=$(new_mission "020 host faults" "$source")
    m018=""
    for name in disk memory dbase room; do
        m018="$m018 $(new_mission "018 $name" "$source")"
    done
    m035a=$(new_mission "035 a verdict binds its inputs (A)" "$source")
    m035b=$(new_mission "035 a verdict binds its inputs (B)" "$source")

    "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!

    task_alpha=$(assign_to "$m016" alpha "Add field c to Facts. BB:FIELDS a b c")
    task_beta=$(assign_to "$m016" beta "Add a test that builds Facts. BB:USE")
    task_gamma=$(assign_to "$m016" gamma "Add a broken initializer. BB:BREAK")
    [ "$task_alpha" -lt "$task_beta" ] && [ "$task_beta" -lt "$task_gamma" ] ||
        harness "task ids are not in assignment order: $task_alpha $task_beta $task_gamma"
    t_hosta=$(assign_to "$m020" hosta "Write your file. BB:WRITE")
    t_mixed=$(assign_to "$m020" mixed "Write your file. BB:WRITE")
    t_olddb=$(assign_to "$m020" olddb "Write your file. BB:WRITE")
    t_hostb=$(assign_to "$m020" hostb "Write your file. BB:WRITE")
    [ "$t_hosta" -lt "$t_mixed" ] && [ "$t_mixed" -lt "$t_olddb" ] && [ "$t_olddb" -lt "$t_hostb" ] ||
        harness "task ids are not in assignment order"
    all_018=""
    set -- $m018
    for name in disk memory dbase room; do
        all_018="$all_018 $(assign_to "$1" "$name" "Write your file. BB:WRITE")"
        shift
    done
    task_a=$(assign_to "$m035a" binda "Write your file. BB:WRITE:one")
    task_b=$(assign_to "$m035b" bindb "Write your file. BB:WRITE:two")

    # Every task reported before any part judges (016, 020 and 018 each wait
    # for all of theirs before their first pass).
    all_tasks="$task_alpha $task_beta $task_gamma $t_hosta $t_mixed $t_olddb $t_hostb $all_018 $task_a $task_b"
    deadline=$(($(now) + 240))
    for id in $all_tasks; do
        until [ "$(task_state "$id")" = reported ]; do
            [ "$(now)" -lt "$deadline" ] ||
                harness "the lane's tasks were not all reported within 240 s (tasks: $(tasks_line))"
            sleep 1
        done
    done
    echo "j10: lane a: all $(echo $all_tasks | wc -w) tasks reported"

    part_016
    part_020
    part_018
    part_035
    classes_hold
    P=""
    echo "j10: lane a: 016, 020, 018 and 035 passed"
}

# --- 016: a candidate broken only by main's moves -----------------------------------

part_016() {
    P=$WORK/p016
    mkdir -p "$P"
    source=$s016
    mission=$m016
    base=$(git --git-dir="$source" rev-parse main)
    : >"$P/judged.log"
    : >"$P/passes.log"

    # The build rule, shared by the gate and the journey's own premises:
    # `build <git dir> <commit>` exits 0 when every file under uses/ names
    # exactly struct.txt's fields, and prints a compiler-like line per mistake.
    cat >"$P/build.sh" <<'EOF'
#!/bin/sh
dir=$1
commit=$2
fields=$(git --git-dir="$dir" show "$commit:struct.txt" 2>/dev/null) || {
    echo "error: struct.txt is missing"
    exit 1
}
status=0
for path in $(git --git-dir="$dir" ls-tree -r --name-only "$commit" uses/); do
    used=$(git --git-dir="$dir" show "$commit:$path")
    for field in $fields; do
        case " $used " in
        *" $field "*) ;;
        *)
            echo "error[E0063]: missing field \`$field\` in initializer of \`Facts\`"
            echo " --> $path:1"
            status=1
            ;;
        esac
    done
    for field in $used; do
        case " $fields " in
        *" $field "*) ;;
        *)
            echo "error[E0560]: struct \`Facts\` has no field named \`$field\`"
            echo " --> $path:1"
            status=1
            ;;
        esac
    done
done
exit $status
EOF
    chmod 755 "$P/build.sh"

    # The mission's scenarios directory: the recording gate below judges, this
    # only has to exist and hold a scenario (the integrator counts them).
    gate=$P/gate
    mkdir -p "$gate"
    printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
    printf '# nothing protected\n' >"$gate/protected"

    # The recording gate: builds the candidate by the rule above; notes every
    # pair it judged and its verdict.
    cat >"$P/fake-gate.sh" <<EOF
#!/bin/sh
echo "== candidate: build"
if out=\$('$P/build.sh' "\$1" "\$3" 2>&1); then
    printf '%s\n' "\$out"
    echo "| build: ok"
    printf '%s %s %s accepted\n' "\$(date +%s)" "\$2" "\$3" >>'$P/judged.log'
    printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
    echo '{"verdict":"accepted"}'
else
    printf '%s\n' "\$out" | sed 's/^/| tests: /'
    printf '%s %s %s rejected\n' "\$(date +%s)" "\$2" "\$3" >>'$P/judged.log'
    printf '{"verdict":"rejected","reason":"candidate tests do not build"}\n' >"\$SPAWN_GATE_VERDICT"
    echo '{"verdict":"rejected","reason":"candidate tests do not build"}'
fi
EOF
    chmod 755 "$P/fake-gate.sh"

    rejections() { q "select gate_rejections from mission_missions where id = $mission"; }
    halted() { q "select coalesce(halted, '') from mission_missions where id = $mission"; }
    mission_line() {
        q "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks
             where mission_id = $mission"
    }
    log_tail() { tail -n 6 "$P/integrate.log" 2>/dev/null | tr '\n' ' '; }
    GATED="select e.id || ' ' || (e.kind -> 'detail' -> 'record' ->> 'id') || ' ' ||
                  (e.kind -> 'detail' -> 'record' ->> 'verdict')
             from spawn_events e
            where e.session_id = 'session_control'
              and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
              and e.kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale'"
    # One integrator pass, accounted: the verdicts it recorded and the
    # mission's gate_rejections before and after it, one line in passes.log.
    integrate_once() {
        before_count=$(rejections)
        before_event=$(last_event)
        capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$P/fake-gate.sh" \
            SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
            "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
        cat "$WORK/out.txt" "$WORK/err.txt" >>"$P/integrate.log"
        [ "$status" -eq 0 ] || fail "integrate exited $status: $(err)" #@ 016:436
        verdicts=$(sql "$GATED and e.id > $before_event order by e.id" | tr '\n' ';')
        printf '%s %s -> %s %s\n' "$before_event" "$before_count" "$(rejections)" "$verdicts" \
            >>"$P/passes.log"
        return 0
    }
    record_of() { # <event id> <field>
        q "select kind -> 'detail' -> 'record' ->> '$2' from spawn_events where id = $1"
    }
    landed_to() {
        q "select kind -> 'detail' -> 'record' ->> 'to' from spawn_events
            where session_id = 'session_control'
              and kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
              and kind -> 'detail' -> 'record' ->> 'id' = '$1'
            order by id desc limit 1"
    }
    first_verdict() {
        q "select e.id || ' ' || (e.kind -> 'detail' -> 'record' ->> 'verdict') from spawn_events e
            where e.session_id = 'session_control'
              and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
              and e.kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale'
              and e.kind -> 'detail' -> 'record' ->> 'id' = '$1' and e.id > $2
            order by e.id limit 1"
    }
    pass_of() { grep -E "[ ;]$1 [0-9]+ [a-z]+(;|\$)" "$P/passes.log" | head -1 || true; }
    counted() {
        pass=$(pass_of "$1")
        [ -n "$pass" ] || harness "no pass recorded event $1"
        set -- $pass
        verdicts=$(printf '%s\n' "$pass" | cut -d' ' -f5- | tr ';' '\n' | grep -c . || true)
        [ "$verdicts" -eq 1 ] || harness "the pass that recorded event $1 recorded $verdicts verdicts: $pass"
        echo "$2 $4"
    }
    holder() {
        for dir in "$source" "$WORK"/integrator/.sources/*.git; do
            if git --git-dir="$dir" cat-file -e "$1^{commit}" 2>/dev/null; then
                echo "$dir"
                return 0
            fi
        done
        echo "$source"
    }
    judged() { # <base> <candidate> <verdict>
        grep -q "^[0-9]* $1 $2 $3\$" "$P/judged.log"
    }

    [ "$(git --git-dir="$source" rev-parse main)" = "$base" ] || harness "main moved before any pass"
    for name in alpha beta gamma; do
        git --git-dir="$source" merge-base --is-ancestor "$base" "refs/heads/spawn/agent_resident_$name" ||
            harness "$name's pushed branch does not hold M0"
    done
    gate_on "$mission" "$gate"

    # alpha lands
    deadline=$(($(now) + 120))
    until [ "$(task_state "$task_alpha")" = landed ]; do
        [ "$(now)" -lt "$deadline" ] ||
            harness "alpha's task did not land within 120 s (tasks: $(mission_line); integrate: $(log_tail))"
        integrate_once
        sleep 0.5
    done
    alpha_landing=$(landed_to "$task_alpha")
    [ -n "$alpha_landing" ] || harness "alpha's task.landed names no commit"
    main1=$(git --git-dir="$source" rev-parse main)
    [ "$(git --git-dir="$source" show "$main1:struct.txt")" = "a b c" ] ||
        harness "main after alpha's landing does not hold field c"
    landed_at=$(q "select max(id) from spawn_events where session_id = 'session_control'
                   and kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
                   and kind -> 'detail' -> 'record' ->> 'id' = '$task_alpha'")
    [ "$(rejections)" = 0 ] || harness "gate_rejections is $(rejections) after alpha's landing"

    # The premises, by the gate's own rule: beta's head builds alone, and does
    # not once merged with M1; gamma's head does not build alone.
    beta_head=$(git --git-dir="$source" rev-parse "refs/heads/spawn/agent_resident_beta")
    gamma_head=$(git --git-dir="$source" rev-parse "refs/heads/spawn/agent_resident_gamma")
    "$P/build.sh" "$source" "$beta_head" >/dev/null || harness "beta's head does not build alone"
    ! "$P/build.sh" "$source" "$gamma_head" >/dev/null || harness "gamma's head builds alone"
    git --git-dir="$source" merge-base --is-ancestor "$main1" "$beta_head" &&
        harness "beta's head already holds main after alpha's landing"

    echo "j10: [016 (1)] beta: incompatible, not counted"
    deadline=$(($(now) + 120))
    until verdict=$(first_verdict "$task_beta" "$landed_at") && [ -n "$verdict" ]; do
        [ "$(now)" -lt "$deadline" ] ||
            harness "beta's task got no verdict within 120 s of alpha's landing (tasks: $(mission_line); integrate: $(log_tail))"
        integrate_once
        sleep 0.5
    done
    set -- $verdict
    beta_event=$1
    beta_verdict=$2
    set -- $(counted "$beta_event")
    if [ "$beta_verdict" != incompatible ]; then
        fail "(1) task #$task_beta, whose head passes the gate alone and fails only merged with main, was gated '$beta_verdict' ($(record_of "$beta_event" reason)), not 'incompatible'; gate_rejections $1 -> $2" #@ 016:585
    fi
    [ "$2" = "$1" ] ||
        fail "(1) the incompatible verdict on task #$task_beta moved gate_rejections from $1 to $2" #@ 016:588
    # Sent back: revise, or already delivered when the loop has typed the
    # send-back into beta's session by now (an input to a busy session waits
    # its turn, 015).
    case "$(task_state "$task_beta")" in
    revise | delivered) ;;
    *) fail "(1) task #$task_beta's state after its incompatible verdict: expected revise or delivered, got $(task_state "$task_beta")" ;; #@ 016:593
    esac
    assert_eq "$(halted)" "" "(1) the mission's halt after beta's verdict" #@ 016:595

    echo "j10: [016 (2)] the record says why"
    candidate=$(record_of "$beta_event" candidate)
    head=$(record_of "$beta_event" head)
    head_base=$(record_of "$beta_event" head_base)
    head_verdict=$(record_of "$beta_event" head_verdict)
    verdict_base=$(record_of "$beta_event" base)
    note=$(record_of "$beta_event" note)
    [ -n "$candidate" ] || fail "(2) beta's incompatible record names no candidate" #@ 016:605
    [ -n "$head" ] || fail "(2) beta's incompatible record names no head" #@ 016:606
    [ -n "$head_base" ] || fail "(2) beta's incompatible record names no head_base" #@ 016:607
    assert_eq "$verdict_base" "$main1" "(2) the incompatible record's base (main when it was judged)" #@ 016:608
    dir=$(holder "$candidate")
    assert_eq "$(git --git-dir="$dir" rev-parse --verify -q "$candidate^2" || echo none)" "$head" \
        "(2) the second parent of the rejected candidate $candidate against the record's head" #@ 016:610
    assert_eq "$(git --git-dir="$dir" rev-parse --verify -q "$candidate^1" || echo none)" "$main1" \
        "(2) the first parent of the rejected candidate $candidate" #@ 016:612
    git --git-dir="$source" merge-base --is-ancestor "$head" "refs/heads/spawn/agent_resident_beta" ||
        fail "(2) the record's head $head is not on beta's pushed branch" #@ 016:615
    assert_eq "$(git --git-dir="$dir" merge-base "$head" "$verdict_base")" "$head_base" \
        "(2) the record's head_base against merge-base(head, base)" #@ 016:616
    assert_eq "$head_verdict" accepted "(2) the record's head_verdict" #@ 016:618
    judged "$verdict_base" "$candidate" rejected ||
        fail "(2) the gate never rejected the merged candidate ($verdict_base $candidate)" #@ 016:620
    judged "$head_base" "$head" accepted ||
        fail "(2) the gate never judged beta's head alone against its merge-base ($head_base $head) and accepted it" #@ 016:622
    main_commits=$(q "select string_agg(c, ' ') from spawn_events,
        jsonb_array_elements_text(case when jsonb_typeof(kind -> 'detail' -> 'record' -> 'main_commits') = 'array'
                                       then kind -> 'detail' -> 'record' -> 'main_commits' else '[]'::jsonb end) c
        where id = $beta_event")
    assert_contains " $main_commits " " $alpha_landing " \
        "(2) the incompatible record's main_commits (alpha's landing)" #@ 016:627
    short=$(printf '%s' "$alpha_landing" | cut -c1-7)
    assert_contains "$note" "missing field \`c\` in initializer of \`Facts\`" \
        "(2) the incompatible record's note (the gate's compiler line)" #@ 016:630
    assert_contains "$note" "$short" "(2) the incompatible record's note (alpha's landing commit)" #@ 016:632

    echo "j10: [016 (4)] gamma: its own head fails, so it is a rejection"
    deadline=$(($(now) + 120))
    until verdict=$(first_verdict "$task_gamma" "$landed_at") && [ -n "$verdict" ]; do
        [ "$(now)" -lt "$deadline" ] ||
            harness "gamma's task got no verdict within 120 s (tasks: $(mission_line); integrate: $(log_tail))"
        integrate_once
        sleep 0.5
    done
    set -- $verdict
    gamma_event=$1
    assert_eq "$2" rejected "(4) the verdict on task #$task_gamma, whose own head fails the gate" #@ 016:645
    set -- $(counted "$gamma_event")
    assert_eq "$2" "$(($1 + 1))" "(4) gate_rejections after gamma's rejection (was $1)" #@ 016:647
    gamma_head_judged=$(grep -c " $gamma_head rejected\$" "$P/judged.log" || true)
    [ "$gamma_head_judged" -ge 1 ] ||
        fail "(4) the gate never judged gamma's head ($gamma_head) alone: the rejection did not rest on it" #@ 016:651

    echo "j10: [016 (3)] the note reaches beta"
    verdict_ms=$(q "select recorded_at_ms from spawn_events where id = $beta_event")
    revision() {
        q "select id from spawn_commands
            where session_id = 'session_resident_beta' and principal = 'spawn-dispatch'
              and action ->> 'type' = 'input' and state = 'finished' and succeeded
              and created_at_ms >= $verdict_ms and action ->> 'text' like '%Task #$task_beta%'
            order by id limit 1"
    }
    deadline=$(($(now) + 150))
    until [ -n "$(revision)" ]; do
        [ "$(now)" -lt "$deadline" ] ||
            fail "(3) nothing was delivered to beta within 150 s of its incompatible verdict (task $(task_state "$task_beta"))" #@ 016:666
        sleep 1
    done
    text=$(sql "select action ->> 'text' from spawn_commands where id = $(revision)")
    assert_contains "$text" "Task #$task_beta" "(3) the input typed into beta after its verdict" #@ 016:670
    assert_contains "$text" "missing field \`c\` in initializer of \`Facts\`" \
        "(3) the revision typed into beta (the gate's compiler line)" #@ 016:671
    assert_contains "$text" "$short" "(3) the revision typed into beta (alpha's landing commit)" #@ 016:673

    echo "j10: [016 (5)] the way back: beta adapts to main and lands"
    revised() { grep -q "^revision [0-9]* beta\$" "$WORK/engine.log"; }
    deadline=$(($(now) + 60))
    until revised; do
        [ "$(now)" -lt "$deadline" ] || harness "beta's engine never took its revision"
        sleep 0.5
    done
    : >"$WORK/fix-beta"
    deadline=$(($(now) + 240))
    until [ "$(task_state "$task_beta")" = landed ]; do
        [ "$(now)" -lt "$deadline" ] ||
            fail "(5) task #$task_beta, sent back and fixed against main, did not land within 240 s (tasks: $(mission_line); integrate: $(log_tail))" #@ 016:687
        [ -z "$(halted)" ] || fail "(5) the mission halted: $(halted)" #@ 016:688
        integrate_once
        sleep 0.5
    done
    assert_eq "$(halted)" "" "(5) the mission's halt after beta landed" #@ 016:692
    main2=$(git --git-dir="$source" rev-parse main)
    "$P/build.sh" "$source" "$main2" >/dev/null ||
        fail "(5) main does not build after beta landed: $("$P/build.sh" "$source" "$main2" | tr '\n' ' ')" #@ 016:695
    assert_eq "$(git --git-dir="$source" show "$main2:uses/beta.txt")" "a b c" "(5) uses/beta.txt on main" #@ 016:696

    gate_off "$mission"
    echo "j10: 016: a candidate broken only by main's moves was sent back with the gate's output and main's commits, not counted; one broken alone was rejected and counted; the sent-back task adapted and landed"
}

# --- 020: host faults from check-tests' output and the test database's log ------------

part_020() {
    P=$WORK/p020
    mkdir -p "$P"
    source=$WORK/source.git
    mission=$m020
    : >"$P/judged.log"
    : >"$P/integrate.log"
    : >"$P/passes.log"
    gate=$P/gate
    mkdir -p "$gate"
    printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
    printf '# nothing protected\n' >"$gate/protected"

    # The test database's log, as postgres writes it (`%m [%p] `, UTC). It
    # holds a crash from an hour before: outside every gate's window.
    pglog=$P/postgres.log
    old=$(date -u -d '-1 hour' '+%Y-%m-%d %H:%M:%S.000 UTC' 2>/dev/null) ||
        old=$(date -u -r $(($(date +%s) - 3600)) '+%Y-%m-%d %H:%M:%S.000 UTC')
    {
        printf '%s [1] LOG:  server process (PID 311) was terminated by signal 11: Segmentation fault\n' "$old"
        printf '%s [1] LOG:  terminating any other active server processes\n' "$old"
        printf '%s [1] LOG:  all server processes terminated; reinitializing\n' "$old"
        printf '%s [1] LOG:  database system is ready to accept connections\n' "$old"
    } >"$pglog"

    # The recording gate, by what the candidate holds (020's header): the
    # residents' files are hosta.txt and hostb.txt (020 renamed them in its
    # gate after the reports; here the gate names them so from the start).
    cat >"$P/fake-gate.sh" <<EOF
#!/bin/sh
has() { git --git-dir="\$1" cat-file -e "\$3:\$4" 2>/dev/null; }
reject() {
    printf '%s %s %s\n' "\$(date +%s)" "\$1" "\$cand" >>'$P/judged.log'
    printf '{"verdict":"rejected","reason":"regression: check-tests.sh fails on the candidate"}\n' >"\$SPAWN_GATE_VERDICT"
    echo "| stage gate rejected 1200ms regression: check-tests.sh must still pass"
    echo '{"verdict":"rejected","reason":"regression: check-tests.sh fails on the candidate"}'
    exit 0
}
fails() { # <test> <message line>...
    test=\$1
    shift
    echo "| ---- \$test stdout ----"
    echo "| thread '\$test' panicked at crates/spawn-postgres/tests/common/mod.rs:31:10:"
    for text in "\$@"; do echo "| \$text"; done
    echo "| note: run with \\\`RUST_BACKTRACE=1\\\` environment variable to display a backtrace"
    echo "| --"
}
refused() { fails "\$1" 'connect to the test database: error connecting to server: Connection refused (os error 111)'; }
crashed() { fails "\$1" 'called \`Result::unwrap()\` on an \`Err\` value: Error { kind: Db, cause: Some(DbError { severity: "FATAL", parsed_severity: Some(Fatal), code: SqlState(E57P02), message: "terminating connection because of crash of another server process" }) }'; }
recovering() { fails "\$1" 'db error: FATAL: the database system is in recovery mode'; }
lost_oid() { fails "\$1" 'called \`Result::unwrap()\` on an \`Err\` value: Database("db error: ERROR: could not open relation with OID 16421")'; }
lost_cache() { fails "\$1" 'db error: ERROR: cache lookup failed for relation 16388'; }
# failed <test> <first message line>: the owner's one line per failing test.
failed() { echo "| FAILED \$1: \$2"; }
header() {
    echo "== candidate: build"
    echo "| build: ok"
    echo "| stage gate ok 900ms candidate: build"
    echo "| == regression: check-tests.sh must still pass"
}
cand=\$3
REFUSED='connect to the test database: error connecting to server: Connection refused (os error 111)'
CRASHED='called \`Result::unwrap()\` on an \`Err\` value: Error { kind: Db, cause: Some(DbError { severity: "FATAL", parsed_severity: Some(Fatal), code: SqlState(E57P02), message: "terminating connection because of crash of another server process" }) }'
RECOVERING='db error: FATAL: the database system is in recovery mode'
LOST_OID='called \`Result::unwrap()\` on an \`Err\` value: Database("db error: ERROR: could not open relation with OID 16421")'
LOST_CACHE='db error: ERROR: cache lookup failed for relation 16388'
if has "\$1" "\$2" "\$3" hostb.txt; then
    n=\$(cat '$P/hostB-runs' 2>/dev/null || echo 0)
    n=\$((n + 1))
    echo "\$n" >'$P/hostB-runs'
    if [ "\$n" -eq 1 ]; then
        # Today's form: the excerpt alone.
        header
        refused sources::tests::lists_every_source
        recovering stalls::delivery_is_measured_from_its_record
        echo "| test result: FAILED. 41 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.02s"
        reject hostB-\$n
    elif [ "\$n" -eq 2 ]; then
        sleep 1
        stamp=\$(date -u '+%Y-%m-%d %H:%M:%S.%3N UTC')
        {
            printf '%s [1] LOG:  server process (PID 4242) was terminated by signal 11: Segmentation fault\n' "\$stamp"
            printf '%s [1] DETAIL:  Failed process was running: select * from spawn_claim(\$1)\n' "\$stamp"
            printf '%s [1] LOG:  terminating any other active server processes\n' "\$stamp"
            printf '%s [1] LOG:  all server processes terminated; reinitializing\n' "\$stamp"
        } >>'$pglog'
        sleep 1
        header
        failed merges::tests::a_conflict_is_recorded "\$LOST_OID"
        failed sources::tests::lists_every_source "\$LOST_CACHE"
        lost_oid merges::tests::a_conflict_is_recorded
        lost_cache sources::tests::lists_every_source
        echo "| test result: FAILED. 41 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.40s"
        reject hostB-\$n
    fi
    printf '%s %s %s\n' "\$(date +%s)" hostB-\$n "\$cand" >>'$P/judged.log'
    echo "== candidate: build"
    echo "| build: ok"
    printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
    echo '{"verdict":"accepted"}'
    exit 0
fi
if has "\$1" "\$2" "\$3" olddb.txt; then
    # Today's form.
    header
    lost_oid merges::tests::a_conflict_is_recorded
    lost_cache sources::tests::lists_every_source
    echo "| test result: FAILED. 41 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.40s"
    reject olddb
fi
if has "\$1" "\$2" "\$3" mixed.txt; then
    # The owner's form; the excerpt is cut before the assertion.
    header
    failed api::tests::claims_in_order "\$REFUSED"
    failed control::tests::the_fold_replays "\$CRASHED"
    failed integrate::tests::a_landing_moves_main_once 'assertion \`left == right\` failed: the landing moved main'
    refused api::tests::claims_in_order
    crashed control::tests::the_fold_replays
    reject mixed
fi
if has "\$1" "\$2" "\$3" hosta.txt && [ ! -e '$P/fixed-hostA' ]; then
    # The owner's form; the excerpt is cut after the first failure.
    header
    failed api::tests::claims_in_order "\$REFUSED"
    failed control::tests::the_fold_replays "\$CRASHED"
    failed stalls::tests::delivery_is_measured_from_its_record "\$RECOVERING"
    refused api::tests::claims_in_order
    reject hostA
fi
printf '%s %s %s\n' "\$(date +%s)" accept "\$cand" >>'$P/judged.log'
echo "== candidate: build"
echo "| build: ok"
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
    chmod 755 "$P/fake-gate.sh"

    # The first host fault's backoff (it doubles with each in a row).
    BACKOFF_SECS=5
    rejections() { q "select gate_rejections from mission_missions where id = $mission"; }
    aborts() { q "select gate_aborts from mission_missions where id = $mission"; }
    halted() { q "select coalesce(halted, '') from mission_missions where id = $mission"; }
    log_tail() { tail -n 6 "$P/integrate.log" 2>/dev/null | tr '\n' ' '; }
    field() { q "select kind -> 'detail' -> 'record' ->> '$2' from spawn_events where id = $1"; }
    runs_of() { grep -c " $1 " "$P/judged.log" || true; }
    host_faults() { q "select gate_host_faults from mission_missions where id = $mission"; }
    recorded_ms() { q "select recorded_at_ms from spawn_events where id = $1"; }
    run_after() { awk -v name="$1" -v t="$2" '$2 == name && $1 >= t { print $1; exit }' "$P/judged.log"; }

    # One pass, with 018's checks met: the real test database, no disk or
    # memory floor to speak of; the test database's log.
    integrate_once() {
        before_count=$(rejections)
        before_event=$(last_event)
        capture env SPAWN_GATE="$P/fake-gate.sh" SPAWN_TEST_DATABASE_URL="$TEST_URL" \
            SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
            SPAWN_TEST_DATABASE_LOG="$pglog" SPAWN_GATE_HOST_FAULT_BACKOFF_SECS="$BACKOFF_SECS" \
            "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
        cat "$WORK/out.txt" "$WORK/err.txt" >>"$P/integrate.log"
        [ "$status" -eq 0 ] || fail "integrate exited $status: $(err | tail -n 3)" #@ 020:530
        printf '%s %s -> %s %s\n' "$before_event" "$before_count" "$(rejections)" \
            "$(q "select coalesce(string_agg((kind -> 'detail' -> 'record' ->> 'id') || ':' ||
                                             (kind -> 'detail' -> 'record' ->> 'verdict'), ';' order by id), '')
                   from spawn_events where session_id = 'session_control' and id > $before_event
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'")" >>"$P/passes.log"
    }
    judge_next() {
        deadline=$(($(now) + 90))
        while :; do
            integrate_once
            found=$(q "select e.id || ' ' || (e.kind -> 'detail' -> 'record' ->> 'verdict') from spawn_events e
                        where e.session_id = 'session_control'
                          and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                          and e.kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale'
                          and e.kind -> 'detail' -> 'record' ->> 'id' = '$1' and e.id > $2
                        order by e.id limit 1")
            if [ -n "$found" ]; then
                set -- $found
                event=$1
                verdict=$2
                before=$before_count
                after=$(rejections)
                return 0
            fi
            [ "$(now)" -lt "$deadline" ] ||
                harness "task #$1 got no verdict within 90 s of passes (tasks: $(tasks_line); integrate: $(log_tail))"
            sleep 0.5
        done
    }

    for name in hostA mixed olddb hostB; do
        lower=$(printf '%s' "$name" | tr 'A-Z' 'a-z')
        git --git-dir="$source" cat-file -e "refs/heads/spawn/agent_resident_$lower:$lower.txt" 2>/dev/null ||
            harness "$name's pushed branch does not hold $lower.txt"
    done
    gate_on "$mission" "$gate"

    echo "j10: [020 (1)] hostA: every failure a connection error -> host_fault"
    start=$(last_event)
    revisions_before=$(revisions "$t_hosta")
    aborts_before=$(aborts)
    judge_next "$t_hosta" "$start"
    if [ "$verdict" != host_fault ]; then
        fail "(1) task #$t_hosta, whose every check-tests failure is a connection error to the test database, was gated '$verdict' ($(field "$event" reason)), not 'host_fault'; gate_rejections $before -> $after" #@ 020:627
    fi
    assert_eq "$after" "$before" "(1) gate_rejections after the host fault (was $before)" #@ 020:629
    assert_eq "$(aborts)" "$aborts_before" "(1) gate_aborts after the host fault" #@ 020:630
    assert_eq "$(task_state "$t_hosta")" reported "(1) task #$t_hosta's state after its host fault" #@ 020:631
    assert_eq "$(revisions "$t_hosta")" "$revisions_before" "(1) task #$t_hosta's revisions after its host fault (nothing went back)" #@ 020:632
    assert_eq "$(halted)" "" "(1) the mission's halt after a host fault" #@ 020:633
    assert_eq "$(field "$event" evidence)" failures "(1) the host fault's evidence" #@ 020:634
    assert_eq "$(q "select string_agg(f ->> 'test', ' ' order by f ->> 'test') from spawn_events,
                     jsonb_array_elements(kind -> 'detail' -> 'record' -> 'failures') f where id = $event")" \
        "api::tests::claims_in_order control::tests::the_fold_replays stalls::tests::delivery_is_measured_from_its_record" \
        "(1) the failing tests the host fault names (all three FAILED lines; the excerpt shows one)" #@ 020:635
    assert_eq "$(field "$event" host_faults)" 1 "(1) the host fault's count in a row" #@ 020:639
    assert_eq "$(host_faults)" 1 "(1) the mission's gate_host_faults" #@ 020:640
    assert_eq "$(field "$event" escalate)" false "(1) the first host fault's escalate" #@ 020:641
    assert_eq "$(field "$event" backoff_ms)" $((BACKOFF_SECS * 1000)) "(1) the first host fault's backoff" #@ 020:642
    due=$(field "$event" due_ms)
    assert_eq "$due" $(($(recorded_ms "$event") + BACKOFF_SECS * 1000)) "(1) the first host fault's due time" #@ 020:644

    echo "j10: [020 (2)] judged again after the backoff, and the work lands"
    : >"$P/fixed-hostA"
    judge_next "$t_hosta" "$event"
    assert_eq "$verdict" accepted "(2) task #$t_hosta's verdict after its backoff, the host fixed" #@ 020:650
    ran=$(run_after accept 0)
    [ -n "$ran" ] && [ "$ran" -ge $((due / 1000)) ] ||
        fail "(2) task #$t_hosta was judged again at $ran, before its host fault's due time $((due / 1000)) (s)" #@ 020:653
    assert_eq "$(runs_of accept)" 1 "(2) accepted gate runs" #@ 020:654
    deadline=$(($(now) + 60))
    until [ "$(task_state "$t_hosta")" = landed ]; do
        [ "$(now)" -lt "$deadline" ] || fail "(2) task #$t_hosta was accepted but did not land within 60 s" #@ 020:657
        integrate_once
        sleep 0.5
    done
    assert_eq "$(host_faults)" 0 "(2) the mission's gate_host_faults after the landing" #@ 020:661

    echo "j10: [020 (3)] mixed: a real failure among connection errors -> rejected"
    judge_next "$t_mixed" "$start"
    [ "$verdict" = rejected ] ||
        fail "(3) task #$t_mixed, with a real assertion failure among connection errors (named by its FAILED line, cut from the excerpt), was gated '$verdict' ($(field "$event" reason)), not 'rejected'" #@ 020:667
    assert_eq "$after" "$((before + 1))" "(3) gate_rejections after the real failure (was $before)" #@ 020:668

    echo "j10: [020 (4)] olddb: database errors, a crash outside the window -> rejected"
    judge_next "$t_olddb" "$start"
    [ "$verdict" = rejected ] ||
        fail "(4) task #$t_olddb, whose failures are database errors that are not connection errors, with the test database's only crash an hour before the gate, was gated '$verdict' ($(field "$event" reason)), not 'rejected'" #@ 020:674
    assert_eq "$after" "$((before + 1))" "(4) gate_rejections after olddb's rejection (was $before)" #@ 020:675
    assert_eq "$(host_faults)" 0 "(4) the mission's gate_host_faults after real verdicts" #@ 020:676

    echo "j10: [020 (5)] hostB: two host faults in a row are escalated, never halted"
    deadline=$(($(now) + 120))
    until waiting session_supervisor; do
        [ "$(now)" -lt "$deadline" ] || harness "the supervisor was never judged waiting"
        sleep 0.5
    done
    judge_next "$t_hostb" "$start"
    [ "$verdict" = host_fault ] ||
        fail "(5) task #$t_hostb's first verdict, its failures all connection errors (today's 40-line output), was '$verdict' ($(field "$event" reason)), not 'host_fault'" #@ 020:690
    assert_eq "$after" "$before" "(5) gate_rejections after hostB's first host fault" #@ 020:691
    assert_eq "$(field "$event" host_faults)" 1 "(5) hostB's first host fault's count in a row" #@ 020:692
    assert_eq "$(field "$event" backoff_ms)" $((BACKOFF_SECS * 1000)) "(5) hostB's first host fault's backoff" #@ 020:693
    due=$(field "$event" due_ms)
    first=$event
    judge_next "$t_hostb" "$event"
    [ "$verdict" = host_fault ] ||
        fail "(5) task #$t_hostb's second verdict, database errors while the test database's log shows a crash during the gate, was '$verdict' ($(field "$event" reason)), not 'host_fault'" #@ 020:698
    ran=$(run_after hostB-2 0)
    [ -n "$ran" ] && [ "$ran" -ge $((due / 1000)) ] ||
        fail "(5) task #$t_hostb was judged a second time at $ran, before its first host fault's due time $((due / 1000)) (s)" #@ 020:701
    assert_eq "$after" "$before" "(5) gate_rejections after hostB's second host fault" #@ 020:702
    assert_eq "$(field "$event" evidence)" server_log "(5) the second host fault's evidence" #@ 020:703
    assert_contains "$(field "$event" crash)" "was terminated by signal 11" "(5) the second host fault's crash lines" #@ 020:704
    assert_eq "$(field "$event" host_faults)" 2 "(5) the second host fault's count in a row" #@ 020:705
    assert_eq "$(host_faults)" 2 "(5) the mission's gate_host_faults after two in a row" #@ 020:706
    assert_eq "$(field "$event" backoff_ms)" $((2 * BACKOFF_SECS * 1000)) "(5) the second host fault's backoff (doubled)" #@ 020:707
    due=$(field "$event" due_ms)
    assert_eq "$due" $(($(recorded_ms "$event") + 2 * BACKOFF_SECS * 1000)) "(5) the second host fault's due time" #@ 020:709
    assert_eq "$(field "$event" escalate)" true "(5) the second host fault's escalate" #@ 020:710
    assert_eq "$(halted)" "" "(5) the mission's halt after two host faults in a row" #@ 020:711
    tag="host gate-host-fault-$event"
    deadline=$(($(now) + 60))
    until [ "$(digests_like "$tag")" -ge 1 ]; do
        [ "$(now)" -lt "$deadline" ] ||
            fail "(5) two host faults in a row on task #$t_hostb were not escalated to the supervisor within 60 s (no digest tagged '$tag')" #@ 020:716
        ping_supervisor
        sleep 0.5
    done
    line=$(digest_text_tagged "$tag" | grep -F "task #$t_hostb" | head -n 1)
    assert_contains "$line" "task #$t_hostb" "(5) the escalation of two host faults in a row" #@ 020:721
    assert_contains "$line" "was terminated by signal 11" "(5) the escalation's host evidence (the crash line)" #@ 020:722
    judge_next "$t_hostb" "$event"
    assert_eq "$verdict" accepted "(5) task #$t_hostb's third verdict, after its backoff, the host recovered" #@ 020:724
    ran=$(run_after hostB-3 0)
    [ -n "$ran" ] && [ "$ran" -ge $((due / 1000)) ] ||
        fail "(5) task #$t_hostb was judged a third time at $ran, before its second host fault's due time $((due / 1000)) (s)" #@ 020:727
    deadline=$(($(now) + 60))
    until [ "$(task_state "$t_hostb")" = landed ]; do
        [ "$(now)" -lt "$deadline" ] || fail "(5) task #$t_hostb was accepted but did not land within 60 s" #@ 020:730
        integrate_once
        sleep 0.5
    done
    assert_eq "$(host_faults)" 0 "(5) the mission's gate_host_faults after hostB landed" #@ 020:734
    assert_eq "$(halted)" "" "(5) the mission's halt at the end" #@ 020:735
    assert_eq "$(digests_like 'host gate-host-fault-%')" 1 "(5) host-fault escalations delivered" #@ 020:736
    assert_eq "$(digests_like "host gate-host-fault-$first")" 0 "(5) escalations of the first host fault alone" #@ 020:737

    gate_off "$mission"
    echo "j10: 020: connection errors and a crash in the test database's log were recorded as host faults, not counted, and judged again after a doubling backoff; real failures stayed rejections; two in a row were escalated to the supervisor and nothing halted"
}

# --- 018: no room on the host: no_room, not a verdict ---------------------------------

part_018() {
    P=$WORK/p018
    mkdir -p "$P"
    : >"$P/judged.log"
    : >"$P/integrate.log"
    gate=$P/gate
    mkdir -p "$gate"
    printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
    printf '# nothing protected\n' >"$gate/protected"

    # The recording gate: accepts every candidate, notes every run.
    cat >"$P/fake-gate.sh" <<EOF
#!/bin/sh
printf '%s %s %s\n' "\$(date +%s)" "\$2" "\$3" >>'$P/judged.log'
echo "== candidate: build"
echo "| build: ok"
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
    chmod 755 "$P/fake-gate.sh"

    mission_of() { q "select mission_id from mission_tasks where id = $1"; }
    mission_field() { q "select $2 from mission_missions where id = $1"; }
    log_tail() { tail -n 6 "$P/integrate.log" 2>/dev/null | tr '\n' ' '; }
    runs() { grep -c . "$P/judged.log" || true; }
    next_gated() {
        q "select e.id || ' ' || (e.kind -> 'detail' -> 'record' ->> 'id') || ' ' ||
                  (e.kind -> 'detail' -> 'record' ->> 'verdict')
             from spawn_events e
            where e.session_id = 'session_control'
              and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
              and e.kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale' and e.id > $1
            order by e.id limit 1"
    }
    field() { q "select kind -> 'detail' -> 'record' ->> '$2' from spawn_events where id = $1"; }
    record_text() { q "select (kind -> 'detail' -> 'record')::text from spawn_events where id = $1"; }

    # One integrator pass with the phase's environment (`env` arguments).
    integrate_once() {
        capture env -u SPAWN_TEST_DATABASE_URL -u SPAWN_GATE_MIN_FREE_GB -u SPAWN_GATE_MIN_FREE_MEMORY_MB \
            SPAWN_GATE="$P/fake-gate.sh" "$@" \
            "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
        cat "$WORK/out.txt" "$WORK/err.txt" >>"$P/integrate.log"
        [ "$status" -eq 0 ] || fail "integrate exited $status: $(err | tail -n 3)" #@ 018:332
    }
    # The phase: passes with the environment `$@` until a gate record
    # appears; sets `event`, `task`, `verdict`, and `runs_before`.
    phase() {
        what=$1
        shift
        runs_before=$(runs)
        from=$(last_event)
        deadline=$(($(now) + 90))
        while :; do
            integrate_once "$@"
            found=$(next_gated "$from")
            [ -z "$found" ] || break
            [ "$(now)" -lt "$deadline" ] ||
                harness "($what) no gate record within 90 s of passes (tasks: $(tasks_line); integrate: $(log_tail))"
            sleep 0.5
        done
        set -- $found
        event=$1
        task=$2
        verdict=$3
        # One gate slot: one judging per pass, so one record. The checks
        # belong to the judging that would run a gate, not to every ready task.
        records=$(q "select count(*) from spawn_events
                      where session_id = 'session_control' and id > $from
                        and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                        and kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale'")
        [ "$records" -eq 1 ] ||
            fail "($what) one pass with one gate slot recorded $records gate outcomes; the checks run for the judging that would run the gate, one task" #@ 018:362
    }
    # The common checks of a phase whose check fell short.
    held_back() { # <label> <check> <phase description>
        label=$1
        if [ "$verdict" != no_room ]; then
            fail "($label) $3, yet the gate ran on task #$task's candidate and it was gated '$verdict' ($(field "$event" reason)); gate runs $runs_before -> $(runs)" #@ 018:369
        fi
        assert_eq "$(runs)" "$runs_before" "($label) gate runs while $3" #@ 018:371
        assert_eq "$(field "$event" check)" "$2" "($label) the no_room record's check" #@ 018:372
        assert_eq "$(task_state "$task")" reported "($label) task #$task's state after no_room" #@ 018:373
        m=$(mission_of "$task")
        assert_eq "$(mission_field "$m" gate_rejections)" 0 "($label) mission $m's gate_rejections" #@ 018:375
        assert_eq "$(mission_field "$m" gate_aborts)" 0 "($label) mission $m's gate_aborts" #@ 018:376
        assert_eq "$(mission_field "$m" "coalesce(gate_room_task, 0)")" "$task" \
            "($label) mission $m's no-room run (gate_room_task): judged again later" #@ 018:377
        assert_eq "$(mission_field "$m" gate_room_waits)" 1 "($label) mission $m's gate_room_waits" #@ 018:379
    }

    for id in $m018; do
        gate_on "$id" "$gate"
    done

    # The phases' environments. The real test database passes the database
    # check (it runs parallel queries); 0 GiB and 1 MiB pass the others.
    ok_disk=SPAWN_GATE_MIN_FREE_GB=0
    ok_memory=SPAWN_GATE_MIN_FREE_MEMORY_MB=1
    ok_db="SPAWN_TEST_DATABASE_URL=$TEST_URL"
    tmpdir=${TMPDIR:-/tmp}

    echo "j10: [018 (1)] disk"
    phase 1 SPAWN_GATE_MIN_FREE_GB=1000000 "$ok_memory" "$ok_db"
    held_back 1 disk "the gate's TMPDIR had less than SPAWN_GATE_MIN_FREE_GB=1000000 free"
    measured=$(field "$event" measured)
    threshold=$(field "$event" threshold)
    df_kb=$(df -Pk "$tmpdir" | awk 'NR == 2 { print $4 }')
    case "$measured" in '' | *[!0-9]*) fail "(1) the disk record's measured is not a whole number of KiB: [$measured]" ;; esac #@ 018:435
    assert_eq "$threshold" "$((1000000 * 1048576))" "(1) the disk record's threshold (KiB)" #@ 018:436
    assert_eq "$(field "$event" free_kb)" "$measured" "(1) the disk record's free_kb against its measured" #@ 018:437
    assert_eq "$(field "$event" min_gb)" 1000000 "(1) the disk record's min_gb" #@ 018:438
    diff_kb=$((measured > df_kb ? measured - df_kb : df_kb - measured))
    [ "$diff_kb" -le 1048576 ] || [ "$diff_kb" -le $((df_kb / 20)) ] ||
        fail "(1) the disk record measured $measured KiB free; df reads $df_kb KiB in $tmpdir" #@ 018:441
    assert_contains "$(field "$event" reason)" "SPAWN_GATE_MIN_FREE_GB=1000000" "(1) the disk record's reason" #@ 018:442
    assert_contains "$(field "$event" reason)" "not a verdict" "(1) the disk record's reason" #@ 018:443

    echo "j10: [018 (2)] memory"
    phase 2 "$ok_disk" SPAWN_GATE_MIN_FREE_MEMORY_MB=100000000 "$ok_db"
    held_back 2 memory "the host had less than SPAWN_GATE_MIN_FREE_MEMORY_MB=100000000 available"
    measured=$(field "$event" measured)
    case "$measured" in '' | *[!0-9]*) fail "(2) the memory record's measured is not a whole number of MiB: [$measured]" ;; esac #@ 018:450
    assert_eq "$(field "$event" threshold)" 100000000 "(2) the memory record's threshold (MiB)" #@ 018:451
    if [ -r /proc/meminfo ]; then
        available=$(awk '/^MemAvailable:/ { print int($2 / 1024) }' /proc/meminfo)
        if [ -n "$available" ] && [ "$available" -gt 0 ]; then
            [ "$measured" -le $((available * 2)) ] && [ "$measured" -ge $((available / 2)) ] ||
                fail "(2) the memory record measured $measured MiB available; /proc/meminfo reads $available MiB" #@ 018:456
        fi
    fi
    assert_contains "$(field "$event" reason)" "$measured" "(2) the memory record's reason (the measured MiB)" #@ 018:459
    assert_contains "$(field "$event" reason)" "SPAWN_GATE_MIN_FREE_MEMORY_MB=100000000" "(2) the memory record's reason" #@ 018:460

    echo "j10: [018 (3)] database"
    mkdir -p "$P/nodb"
    nodb=$(printf '%s' "$P/nodb" | sed 's#/#%2F#g')
    phase 3 "$ok_disk" "$ok_memory" "SPAWN_TEST_DATABASE_URL=postgres://bb:bb018secret@$nodb/bb_none"
    held_back 3 database "the test database could not be reached"
    [ -n "$(field "$event" measured)" ] || fail "(3) the database record's measured is empty" #@ 018:468
    [ -n "$(field "$event" threshold)" ] || fail "(3) the database record's threshold is empty" #@ 018:469
    assert_contains "$(field "$event" reason)" "test database" "(3) the database record's reason" #@ 018:470
    assert_not_contains "$(record_text "$event")" "bb018secret" "(3) the database record (the URL's password)" #@ 018:471

    echo "j10: [018 (4)] room: the gate runs"
    phase 4 "$ok_disk" "$ok_memory" "$ok_db"
    [ "$verdict" = accepted ] ||
        fail "(4) with every threshold met, task #$task was gated '$verdict' ($(field "$event" reason)), not accepted" #@ 018:477
    assert_eq "$(runs)" "$((runs_before + 1))" "(4) gate runs with every threshold met" #@ 018:478
    deadline=$(($(now) + 60))
    until [ "$(task_state "$task")" = landed ]; do
        [ "$(now)" -lt "$deadline" ] || fail "(4) task #$task was accepted but did not land within 60 s" #@ 018:481
        integrate_once "$ok_disk" "$ok_memory" "$ok_db"
        sleep 0.5
    done

    gate_off $m018
    echo "j10: 018: the gate was not run while disk, memory or the test database fell short; each was recorded no_room with its check, measure and threshold, and the gate ran once the host had room"
}

# --- 035: every verdict names its class --------------------------------------------------

part_035() {
    P=$WORK/p035
    mkdir -p "$P"
    : >"$P/judged.log"
    : >"$P/integrate.log"
    # A controller tree: its `.controller` names a commit, its rsi/scenarios
    # is the missions' gate directory (as 035 has it; only `class` is read).
    ctl=$P/ctl
    gate=$ctl/rsi/scenarios
    mkdir -p "$gate/sub"
    controller_commit=$(printf 'bb-035-controller' | sha1sum | cut -c1-40)
    printf '%s\n' "$controller_commit" >"$ctl/.controller"
    printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
    printf '# nothing protected\n' >"$gate/protected"
    printf 'a file in a subdirectory: part of the set\n' >"$gate/sub/notes"
    toolchain="rustc 1.95.0-bb cargo 1.95.0-bb psql 16.bb server 160099"

    # The recording gate (035's): task A (one.txt) by run number: a host
    # fault on the base after its setup (exit 6, the gate's fact), a setup
    # failure before any toolchain line (exit 3), then accepted; task B
    # (two.txt): rejected.
    cat >"$P/fake-gate.sh" <<EOF
#!/bin/sh
has() { git --git-dir="\$1" cat-file -e "\$2:\$3" 2>/dev/null; }
repo=\$1
base=\$2
cand=\$3
note() {
    # note <name> <the gate's arguments>: \$2 repo, \$3 base, \$4 candidate, \$5 scenarios, \$6 scenario
    printf '%s %s %s %s %s %s\n' "\$(date +%s%3N)" "\$1" "\$3" "\$4" "\$5" "\${6:--}" >>'$P/judged.log'
}
setup() {
    echo "| == setup"
    echo "| base cache: toolchain $toolchain"
    echo "| == base: build"
    echo "| build: ok"
    echo "| stage gate ok 10512ms base: build"
}
verdict() { # <accepted|rejected> [reason]
    if [ "\$1" = rejected ]; then
        json="{\"verdict\":\"rejected\",\"reason\":\"\$2\"}"
    else
        json='{"verdict":"accepted"}'
    fi
    echo "| == candidate: build"
    echo "| stage gate ok 900ms candidate: build"
    printf '%s\n' "\$json" >"\$SPAWN_GATE_VERDICT"
    echo "\$json"
    exit 0
}
if has "\$repo" "\$cand" two.txt; then
    note task-b "\$@"
    setup
    echo "| check-ok.sh failed on the candidate"
    verdict rejected "scenario fails on the candidate"
fi
if has "\$repo" "\$cand" one.txt; then
    n=\$(cat '$P/runs-a' 2>/dev/null || echo 0)
    n=\$((n + 1))
    echo "\$n" >'$P/runs-a'
    case "\$n" in
    1)
        note a-1 "\$@"
        setup
        echo "| == base: 011.sh"
        echo "| 011.sh exited 2, then 1 on the base: the host could not judge it"
        json="{\"host_fault\":true,\"scenario\":\"011.sh\",\"on\":\"base\",\"runs\":\"2, then 1\",\"reason\":\"011.sh exited 2, then 1 on the base\",\"base\":\"\$base\",\"candidate\":\"\$cand\"}"
        printf '%s\n' "\$json" >"\$SPAWN_GATE_VERDICT"
        echo "\$json"
        exit 6
        ;;
    2)
        note a-2 "\$@"
        echo "gate: SPAWN_GATE_MIN_FREE_GB is a whole number, not 'x'" >&2
        exit 3
        ;;
    *)
        note "a-\$n" "\$@"
        setup
        verdict accepted
        ;;
    esac
fi
note other "\$@"
verdict accepted
EOF
    chmod 755 "$P/fake-gate.sh"

    BACKOFF_SECS=2
    rejections_of() { q "select gate_rejections from mission_missions where id = $1"; }
    log_tail() { grep -v '^| ' "$P/integrate.log" 2>/dev/null | tail -n 6 | tr '\n' ' '; }
    field() { q "select coalesce(kind -> 'detail' -> 'record' ->> '$2', '') from spawn_events where id = $1"; }
    path() {
        q "select case when kind -> 'detail' -> 'record' #> '{$2}' is null then 'absent'
                       when jsonb_typeof(kind -> 'detail' -> 'record' #> '{$2}') = 'null' then 'null'
                       else kind -> 'detail' -> 'record' #>> '{$2}' end
             from spawn_events where id = $1"
    }
    # 035's integrator environment, one pass at a time (035 ran one long
    # `spawn integrate`: that was for its `inputs`, read when the gate runs,
    # which are not carried here).
    integrate_once() {
        capture env SPAWN_GATE="$P/fake-gate.sh" SPAWN_TEST_DATABASE_URL="$TEST_URL" \
            SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
            SPAWN_GATE_HOST_FAULT_BACKOFF_SECS="$BACKOFF_SECS" \
            "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
        cat "$WORK/out.txt" "$WORK/err.txt" >>"$P/integrate.log"
        [ "$status" -eq 0 ] || harness "integrate exited $status: $(err | tail -n 3)"
    }
    # The next gate verdict (not `stale`) on task `$1` after event `$2`,
    # passes every half second for at most 90 s: sets `event` and `verdict`.
    judge_next() {
        deadline=$(($(now) + 90))
        while :; do
            integrate_once
            found=$(q "select e.id || ' ' || (e.kind -> 'detail' -> 'record' ->> 'verdict') from spawn_events e
                        where e.session_id = 'session_control'
                          and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                          and e.kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale'
                          and e.kind -> 'detail' -> 'record' ->> 'id' = '$1' and e.id > $2
                        order by e.id limit 1")
            if [ -n "$found" ]; then
                set -- $found
                event=$1
                verdict=$2
                return 0
            fi
            [ "$(now)" -lt "$deadline" ] ||
                harness "task #$1 got no verdict within 90 s (tasks: $(tasks_line); integrate: $(log_tail))"
            sleep 0.5
        done
    }
    # 035's check_binding, its `class` line only (inputs and env: CI).
    check_binding() {
        e=$1 label=$2
        expect_eq "$(path "$e" class)" "$3" "$label the verdict's class ($(field "$e" verdict))" #@ 035:501
    }

    for id in "$task_a" "$task_b"; do
        [ "$(task_state "$id")" = reported ] || harness "task #$id is $(task_state "$id"), not reported"
    done
    git --git-dir="$WORK/source.git" cat-file -e "refs/heads/spawn/agent_resident_binda:one.txt" 2>/dev/null ||
        harness "binda's pushed branch does not hold one.txt"
    git --git-dir="$WORK/source.git" cat-file -e "refs/heads/spawn/agent_resident_bindb:two.txt" 2>/dev/null ||
        harness "bindb's pushed branch does not hold two.txt"
    start=$(last_event)
    rejections_before=$(rejections_of "$m035a")
    rejections_b=$(rejections_of "$m035b")
    gate_on "$m035a" "$gate"
    gate_on "$m035b" "$gate"

    echo "j10: [035 (1)] a host fault: class infrastructure"
    judge_next "$task_a" "$start"
    e1=$event
    [ "$verdict" = host_fault ] ||
        harness "task #$task_a's first gate (a host fault on the base, exit 6 with the gate's fact) was gated '$verdict' ($(field "$e1" reason))"
    check_binding "$e1" "(1)" infrastructure a-1 "$controller_commit" "" "$toolchain"
    expect_eq "$(rejections_of "$m035a")" "$rejections_before" "(1) gate_rejections after a host fault" #@ 035:575

    echo "j10: [035 (2)] an abort before the setup: class infrastructure"
    judge_next "$task_a" "$e1"
    e2=$event
    [ "$verdict" = aborted ] ||
        harness "task #$task_a's second gate (exit 3 before its setup) was gated '$verdict' ($(field "$e2" reason))"
    check_binding "$e2" "(2)" infrastructure a-2 "$controller_commit" "" null
    expect_eq "$(rejections_of "$m035a")" "$rejections_before" "(2) gate_rejections after an abort" #@ 035:584

    echo "j10: [035 (3)] the acceptance: class code"
    judge_next "$task_a" "$e2"
    e3=$event
    [ "$verdict" = accepted ] ||
        harness "task #$task_a's third gate (accepted) was gated '$verdict' ($(field "$e3" reason))"
    check_binding "$e3" "(3)" code a-3 "$controller_commit" "" "$toolchain"
    deadline=$(($(now) + 60))
    until [ "$(task_state "$task_a")" = landed ]; do
        [ "$(now)" -lt "$deadline" ] || harness "task #$task_a was accepted but did not land within 60 s"
        integrate_once
        sleep 0.3
    done

    echo "j10: [035 (4)] a rejection of the code: class code"
    judge_next "$task_b" "$start"
    e4=$event
    [ "$verdict" = rejected ] ||
        harness "task #$task_b's gate (rejected) was gated '$verdict' ($(field "$e4" reason))"
    check_binding "$e4" "(4)" code task-b null "" "$toolchain"
    expect_eq "$(rejections_of "$m035b")" "$((rejections_b + 1))" "(4) gate_rejections after a rejection of the code" #@ 035:618

    gate_off "$m035a" "$m035b"
    echo "j10: 035: host faults and aborts were infrastructure and not counted as rejections; acceptances and rejections were code"
}

# =================================================================================
# Lane B: 008 (b) without a loop, then 033 on one loop
# =================================================================================

lane_b() {
    lane_world b
    part_008b
    part_033
    classes_hold
    P=""
    echo "j10: lane b: 008 (b) and 033 passed"
}

# --- 008 (b): a merge git cannot run is no conflict ---------------------------------------

part_008b() {
    P=$WORK/p008
    mkdir -p "$P"
    # A git without a working `--attr-source` for merge-tree, as git 2.40 is:
    # exit 129 and its usage error; everything else goes to the real git.
    mkdir -p "$P/git-no-attr-source"
    cat >"$P/git-no-attr-source/git" <<EOF
#!/bin/sh
attr=""
merge=""
for argument in "\$@"; do
  case "\$argument" in
  --attr-source=*) attr=\$argument ;;
  merge-tree) merge=1 ;;
  esac
done
if [ -n "\$attr" ] && [ -n "\$merge" ]; then
  echo "unknown option: \$attr" >&2
  echo "usage: git [-v | --version] [-h | --help] [-C <path>] [-c <name>=<value>]" >&2
  exit 129
fi
exec '$real_git' "\$@"
EOF
    chmod 755 "$P/git-no-attr-source/git"

    source=$P/source.git
    default=$P/default.git
    git init -q --bare --initial-branch=main "$source"
    git init -q --bare --initial-branch=main "$default"
    git init -q --initial-branch=main "$P/seed"
    printf 'base\n' >"$P/seed/README.md"
    git -C "$P/seed" add -A
    git -C "$P/seed" commit -qm base
    git -C "$P/seed" push -q "$source" main:main
    git -C "$P/seed" push -q "$default" main:main
    mkdir -p "$P/gate"
    : >"$P/judged.log"

    # The recording gate: accepts every candidate and notes what it judged.
    cat >"$P/fake-gate.sh" <<EOF
#!/bin/sh
printf '%s %s\n' "\$2" "\$3" >>'$P/judged.log'
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
    chmod 755 "$P/fake-gate.sh"

    # The fake engine: a typed `put <file> <content> exit` writes the file
    # (logging it outside the workspace) and exits.
    cat >"$P/claude" <<EOF
#!/bin/sh
printf 'Welcome to Claude Code\n\n'
printf '❯ '
while IFS= read -r line; do
  case "\$line" in
  put\ *)
    set -- \${line#put }
    printf '%s\n' "\$2" >"\$1" || exit 3
    printf '%s %s\n' "\$1" "\$2" >>'$P/engine.log'
    [ "\$3" = exit ] && exit 0
    printf '\nwrote %s\n❯ ' "\$1" ;;
  *)
    printf '\n%s\n❯ ' "echo: \$line" ;;
  esac
done
EOF
    chmod 755 "$P/claude"

    # 008's environment: no loop; `spawn tick` is the host.
    SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
    SPAWN_GIT_REPO=$default
    SPAWN_CLAUDE_PROGRAM=$P/claude
    SPAWN_CLAUDE_PROJECTS=$P/projects
    SPAWN_TMUX_PROGRAM=$WORK/tmux
    SPAWN_WORKER=worker-bb
    export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_CLAUDE_PROGRAM \
        SPAWN_CLAUDE_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER
    host=$P/host
    integrator=$WORK/integrator
    mkdir -p "$host" "$integrator"
    lane_init_db

    with_git() {
        directory=$1
        shift
        capture env PATH="$directory:$PATH" "$@"
    }
    tick_on() {
        capture env SPAWN_WORKSPACE_ROOT="$1" "$SPAWN_BIN" tick
        [ "$status" -eq 0 ] || fail "tick on $1 exited $status: $(err)" #@ 008:256
        return 0
    }
    ended() {
        q "select count(*) from agent_executions where session_id = '$1' and ended_at_ms is not null"
    }
    src_ref() { git --git-dir="$source" rev-parse --verify --quiet "$1" || echo none; }
    main_file() { git --git-dir="$source" show "main:$1" 2>/dev/null || echo missing; }
    task_state008() { q "select state from mission_tasks where id = $task"; }
    gated_records() {
        q "select count(*) from spawn_events
            where session_id = 'session_control' and event_name = 'control.recorded'
              and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
              and kind -> 'detail' -> 'record' ->> 'id' = '$task'"
    }

    capture "$SPAWN_BIN" mission add --text "008 landing" --source "$source" \
        --gate "$P/gate" --accept gate
    [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
    mission=$(jq -r .mission "$WORK/out.txt")
    [ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker x \
        --brief "work of x"
    [ "$status" -eq 0 ] || harness "task assign failed: $(err)"
    task=$(jq -r .task "$WORK/out.txt")
    agent=$(q "select agent_id from mission_tasks where id = $task")
    session=$(q "select session_id from mission_tasks where id = $task")

    capture "$SPAWN_BIN" submit --agent "$agent" --session "$session" --key kx1 \
        --action '{"type":"start","engine":"claude","prompt":"put X.txt from-x exit"}'
    [ "$status" -eq 0 ] || harness "submit failed: $(err)"
    deadline=$(($(now) + 180))
    while [ "$(ended "$session")" -lt 1 ]; do
        tick_on "$host"
        [ "$(now)" -lt "$deadline" ] || harness "the worker's execution did not end within 180 s"
        sleep 0.25
    done
    grep -qx "X.txt from-x" "$P/engine.log" 2>/dev/null || harness "the engine did not write X.txt"
    [ "$(src_ref "refs/heads/spawn/$agent")" != none ] || harness "the pusher did not publish spawn/$agent"

    # The source's main moves on (a file the worker never touched): landing
    # the task needs a merge.
    git -C "$P/seed" pull -q "$source" main
    printf 'other\n' >"$P/seed/OTHER.txt"
    git -C "$P/seed" add -A
    git -C "$P/seed" commit -qm "main moves"
    git -C "$P/seed" push -q "$source" main:main
    moved=$(src_ref refs/heads/main)

    command=$(q "select id from spawn_commands where idempotency_key = 'kx1'")
    [ "$(q "select spawn_task_delivered($task, $command)")" = t ] || harness "task $task not delivered"
    [ "$(q "select spawn_task_reported($task, 'done')")" = t ] || harness "task $task not reported"

    echo "j10: [008 (b)] a merge git cannot run is no conflict"
    with_git "$P/git-no-attr-source" env -u SPAWN_TEST_DATABASE_URL \
        SPAWN_GATE="$P/fake-gate.sh" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        "$SPAWN_BIN" integrate --once --root "$integrator"
    output="$(out) $(err)"
    assert_eq "$(gated_records)" 0 \
        "(b) gate verdicts recorded for task $task when git could not merge (integrate: $output)" #@ 008:339
    assert_eq "$(task_state008)" reported "(b) task $task's state after a merge git could not run" #@ 008:341
    assert_eq "$(q "select revisions from mission_tasks where id = $task")" 0 \
        "(b) revisions of task $task after a merge git could not run" #@ 008:342
    assert_contains "$output" "unknown option" "(b) what the integrator showed of git's error" #@ 008:344
    assert_eq "$(src_ref refs/heads/main)" "$moved" "(b) the source's main after a failed merge" #@ 008:345

    capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$P/fake-gate.sh" \
        SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        "$SPAWN_BIN" integrate --once --root "$integrator"
    assert_eq "$(task_state008)" landed \
        "(b) task $task once git can merge again (integrate: $(out) $(err))" #@ 008:350
    assert_eq "$(main_file X.txt)" from-x "(b) X.txt on the source's main" #@ 008:352
    assert_eq "$(main_file OTHER.txt)" other "(b) main's own change kept by the landing" #@ 008:353
    echo "j10: 008 (b): a merge git could not run was an error, not a conflict, and landed once git could"
}

# --- 033: a base that disagrees with itself ---------------------------------------------

part_033() {
    P=$WORK/p033
    mkdir -p "$P"
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
    gate=$P/gate
    mkdir -p "$gate"
    printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
    printf '# nothing protected\n' >"$gate/protected"
    : >"$P/judged.log"
    : >"$P/integrate.log"
    : >"$P/passes.log"

    # The recording gate, by the run's number for flaky's candidate (033's
    # header): runs 1-3 the base disagrees with itself (exit 4), run 4 a
    # first base run that exits 2, run 5 `1, then 2`, run 6 on accepted.
    cat >"$P/fake-gate.sh" <<EOF
#!/bin/sh
has() { git --git-dir="\$1" cat-file -e "\$3:\$4" 2>/dev/null; }
cand=\$3
note() { printf '%s %s %s\n' "\$(date +%s%3N)" "\$1" "\$cand" >>'$P/judged.log'; }
accept() {
    note "\$1"
    echo "| == candidate: build"
    echo "| stage gate ok 900ms candidate: build"
    printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
    echo '{"verdict":"accepted"}'
    exit 0
}
gate_time() { # <setup s>
    echo "| gate time: base 141 s (2 scenario runs, 19 results from the cache), candidate 0 s, setup \$1 s (exports, socket scan, static checks), total \$((\$1 + 141)) s"
}
build() {
    echo "| == base: build"
    echo "| build: ok"
    echo "| stage gate ok 10512ms base: build"
}
disagrees() { # <setup s> [nested]
    build
    echo "| == base: 011.sh"
    echo "| blackbox: (1) coverage: the latest observation of session_resident_core is based on seq 41, its latest fact is seq 44"
    echo "| stage gate exit 1 74210ms base: 011.sh"
    echo "| == base: 011.sh again (unsolved needs two runs that agree)"
    if [ -n "\${2-}" ]; then
        # A nested gate's output, as 024.sh's toy gates print it.
        echo "| | 102.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts"
    fi
    echo "| 011: covered within 10 s, delivered within 30 s, no churn"
    echo "| 011.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts"
    echo "| stage gate aborted 66871ms base: 011.sh again (unsolved needs two runs that agree)"
    gate_time "\$1"
    exit 4
}
if has "\$1" "\$2" "\$3" flaky.txt; then
    n=\$(cat '$P/runs' 2>/dev/null || echo 0)
    n=\$((n + 1))
    echo "\$n" >'$P/runs'
    case "\$n" in
    1) note run-1; disagrees 244 nested ;;
    2) note run-2; disagrees 31 ;;
    3) note run-3; disagrees 29 ;;
    4)
        note run-4
        build
        echo "| == base: 011.sh"
        echo "| 011: harness: init-db failed 5 times: could not connect to the test database"
        echo "| 011.sh exited 2 on the base: a harness error, not a verdict; the gate aborts"
        echo "| stage gate aborted 5120ms base: 011.sh"
        gate_time 30
        exit 4
        ;;
    5)
        note run-5
        build
        echo "| == base: 011.sh"
        echo "| stage gate exit 1 70112ms base: 011.sh"
        echo "| == base: 011.sh again (unsolved needs two runs that agree)"
        echo "| 011: harness: the database is unreachable (while checking: (1) coverage)"
        echo "| 011.sh exited 1, then 2 on the base: a harness error, not a verdict; the gate aborts"
        echo "| stage gate aborted 12003ms base: 011.sh again (unsolved needs two runs that agree)"
        gate_time 28
        exit 4
        ;;
    *) accept "run-\$n" ;;
    esac
fi
accept other
EOF
    chmod 755 "$P/fake-gate.sh"

    lane_loop_env
    echo write >"$WORK/part-flaky"
    # The first host fault's backoff (it doubles with each in a row).
    BACKOFF_SECS=4

    rejections() { q "select gate_rejections from mission_missions where id = $mission"; }
    aborts() { q "select gate_aborts from mission_missions where id = $mission"; }
    halted() { q "select coalesce(halted, '') from mission_missions where id = $mission"; }
    host_faults() {
        q "select coalesce((to_jsonb(m) ->> 'gate_host_faults'), '') from mission_missions m where id = $mission"
    }
    base_disagrees() {
        q "select coalesce((to_jsonb(m) ->> 'gate_base_disagrees'), '') from mission_missions m where id = $mission"
    }
    log_tail() { grep -v '^| ' "$P/integrate.log" 2>/dev/null | tail -n 6 | tr '\n' ' '; }
    field() { q "select coalesce(kind -> 'detail' -> 'record' ->> '$2', '') from spawn_events where id = $1"; }
    recorded_ms() { q "select recorded_at_ms from spawn_events where id = $1"; }
    run_at() { awk -v name="$1" '$2 == name { print $1; exit }' "$P/judged.log"; }

    # One pass, with 018's checks met: the real test database, no disk or
    # memory floor to speak of.
    integrate_once() {
        before_count=$(aborts)
        before_event=$(last_event)
        capture env SPAWN_GATE="$P/fake-gate.sh" SPAWN_TEST_DATABASE_URL="$TEST_URL" \
            SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
            SPAWN_GATE_HOST_FAULT_BACKOFF_SECS="$BACKOFF_SECS" \
            "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
        cat "$WORK/out.txt" "$WORK/err.txt" >>"$P/integrate.log"
        [ "$status" -eq 0 ] || harness "integrate exited $status: $(err | tail -n 3)"
        printf '%s %s -> %s %s\n' "$before_event" "$before_count" "$(aborts)" \
            "$(q "select coalesce(string_agg((kind -> 'detail' -> 'record' ->> 'id') || ':' ||
                                             (kind -> 'detail' -> 'record' ->> 'verdict'), ';' order by id), '')
                   from spawn_events where session_id = 'session_control' and id > $before_event
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'")" >>"$P/passes.log"
    }
    # The next gate verdict (not `stale`) on task `$1` after event `$2`,
    # passes every half second until it comes: sets `event`, `verdict`, and
    # `before` / `after` (gate_aborts around the pass that recorded it).
    # 1 when the mission halted first (nothing is judged then).
    judge_next() {
        deadline=$(($(now) + 90))
        while :; do
            integrate_once
            found=$(q "select e.id || ' ' || (e.kind -> 'detail' -> 'record' ->> 'verdict') from spawn_events e
                        where e.session_id = 'session_control'
                          and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                          and e.kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale'
                          and e.kind -> 'detail' -> 'record' ->> 'id' = '$1' and e.id > $2
                        order by e.id limit 1")
            if [ -n "$found" ]; then
                set -- $found
                event=$1
                verdict=$2
                before=$before_count
                after=$(aborts)
                return 0
            fi
            [ -z "$(halted)" ] || return 1
            [ "$(now)" -lt "$deadline" ] ||
                harness "task #$1 got no verdict within 90 s of passes (tasks: $(tasks_line); integrate: $(log_tail))"
            sleep 0.5
        done
    }
    # A judging that the halt stopped is the failure (033's measure mode,
    # which resumed the mission, is not carried).
    judge_or_halt() { # <task> <after event> <check>
        if judge_next "$1" "$2"; then return 0; fi
        measured_fail "$3: the mission halted before task #$1 was judged again: [$(halted)]" #@ 033:572
    }

    capture "$SPAWN_BIN" mission add --text "033 a base that disagrees with itself" --source "$source"
    [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
    mission=$(jq -r .mission "$WORK/out.txt")
    [ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
    propose flaky "Owns the 033 change."
    started=$(now)
    "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!
    gate_on "$mission" "$gate"
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker flaky \
        --brief "Write your file. BB:WRITE"
    [ "$status" -eq 0 ] || harness "task assign failed: $(err)"
    task=$(jq -r .task "$WORK/out.txt")
    [ -n "$task" ] && [ "$task" != null ] || harness "task assign printed no id: $(out)"
    assigned=$(now_ms)

    deadline=$(($(now) + 120))
    until [ "$(task_state "$task")" = reported ]; do
        [ "$(now)" -lt "$deadline" ] ||
            harness "task #$task was not reported within 120 s (tasks: $(tasks_line))"
        sleep 0.5
    done
    echo "j10: 033: task #$task reported $(($(now_ms) - assigned)) ms after its assignment"
    git --git-dir="$source" cat-file -e "refs/heads/spawn/agent_resident_flaky:flaky.txt" 2>/dev/null ||
        harness "flaky's pushed branch does not hold flaky.txt"
    deadline=$(($(now) + 120))
    until waiting session_supervisor; do
        [ "$(now)" -lt "$deadline" ] || harness "the supervisor was never judged waiting"
        sleep 0.5
    done

    echo "j10: [033 (1)] a base that disagrees with itself -> host_fault"
    start=$(last_event)
    revisions_before=$(revisions "$task")
    rejections_before=$(rejections)
    judge_next "$task" "$start" || harness "the mission halted before task #$task was judged"
    e1=$event
    t1=$(recorded_ms "$e1")
    if [ "$verdict" != host_fault ]; then
        measured_fail "(1) task #$task, whose gate found 011.sh exiting 1 and then 0 on the base (a base that disagrees with itself, before any candidate code ran), was gated '$verdict' ($(field "$e1" reason)), not 'host_fault'; gate_aborts $before -> $after, toward the halt after three" #@ 033:623
    fi
    expect_eq "$after" "$before" "(1) gate_aborts after the disagreeing base (was $before)" #@ 033:625
    expect_eq "$(rejections)" "$rejections_before" "(1) gate_rejections after the disagreeing base" #@ 033:626
    expect_eq "$(task_state "$task")" reported "(1) task #$task's state after its disagreeing base" #@ 033:627
    expect_eq "$(revisions "$task")" "$revisions_before" "(1) task #$task's revisions (nothing went back to the worker)" #@ 033:628
    expect_eq "$(halted)" "" "(1) the mission's halt after one disagreeing base" #@ 033:629
    expect_eq "$(field "$e1" evidence)" base_disagrees "(1) the host fault's evidence" #@ 033:630
    expect_eq "$(field "$e1" scenario)" 011.sh "(1) the scenario the host fault names (the gate's own line, not a nested gate's)" #@ 033:631
    expect_eq "$(q "select coalesce((kind -> 'detail' -> 'record' -> 'exits')::text, '') from spawn_events where id = $e1")" \
        "[1, 0]" "(1) the exits the host fault names" #@ 033:632
    expect_eq "$(field "$e1" setup_s)" 244 "(1) the gate's setup time the host fault names (its gate time line)" #@ 033:634
    expect_eq "$(field "$e1" host_faults)" 1 "(1) the host fault's count in a row" #@ 033:635
    expect_eq "$(host_faults)" 1 "(1) the mission's gate_host_faults" #@ 033:636
    expect_eq "$(field "$e1" escalate)" false "(1) the first host fault's escalate" #@ 033:637
    expect_eq "$(field "$e1" base_disagrees)" 1 "(1) the base disagreements in a row the first host fault names" #@ 033:638
    expect_eq "$(base_disagrees)" 1 "(1) the mission's gate_base_disagrees" #@ 033:639
    expect_eq "$(field "$e1" alert)" false "(1) the first host fault's alert" #@ 033:640
    expect_eq "$(field "$e1" backoff_ms)" $((BACKOFF_SECS * 1000)) "(1) the first host fault's backoff" #@ 033:641
    due1=$(field "$e1" due_ms)
    expect_eq "$due1" $((t1 + BACKOFF_SECS * 1000)) "(1) the first host fault's due time" #@ 033:643

    echo "j10: [033 (2)] judged again only after the backoff"
    judge_or_halt "$task" "$e1" "(2)"
    e2=$event
    t2=$(recorded_ms "$e2")
    ran=$(run_at run-2)
    echo "j10: 033: run 2 started $((ran - t1)) ms after run 1's record ($(field "$e1" verdict)), gated '$verdict' (gate_aborts $before -> $after)"
    case "$due1" in
    '' | *[!0-9]*)
        measured_fail "(2) task #$task's gate ran again $((ran - t1)) ms after its first disagreeing base, with no due time to wait for (the record names no due_ms)" #@ 033:654
        ;;
    *)
        [ "$ran" -ge "$due1" ] ||
            measured_fail "(2) task #$task's gate ran again at $ran ms, before its host fault's due time $due1 ms" #@ 033:658
        ;;
    esac

    echo "j10: [033 (3)] the second in a row: no escalation at two"
    if [ "$verdict" != host_fault ]; then
        measured_fail "(3) task #$task's second disagreeing base in a row was gated '$verdict' ($(field "$e2" reason)), not 'host_fault'; gate_aborts $before -> $after" #@ 033:665
    fi
    expect_eq "$after" "$before" "(3) gate_aborts after the second disagreeing base" #@ 033:667
    expect_eq "$(field "$e2" evidence)" base_disagrees "(3) the second host fault's evidence" #@ 033:668
    expect_eq "$(field "$e2" host_faults)" 2 "(3) the second host fault's count in a row" #@ 033:669
    expect_eq "$(host_faults)" 2 "(3) the mission's gate_host_faults after two in a row" #@ 033:670
    expect_eq "$(field "$e2" backoff_ms)" $((2 * BACKOFF_SECS * 1000)) "(3) the second host fault's backoff (doubled)" #@ 033:671
    due2=$(field "$e2" due_ms)
    expect_eq "$due2" $((t2 + 2 * BACKOFF_SECS * 1000)) "(3) the second host fault's due time" #@ 033:673
    expect_eq "$(field "$e2" escalate)" false "(3) the second host fault's escalate (no escalation at two: the owner's ruling)" #@ 033:674
    expect_eq "$(field "$e2" base_disagrees)" 2 "(3) the base disagreements in a row the second host fault names" #@ 033:675
    expect_eq "$(base_disagrees)" 2 "(3) the mission's gate_base_disagrees after two in a row" #@ 033:676
    expect_eq "$(field "$e2" alert)" false "(3) the second host fault's alert" #@ 033:677
    expect_eq "$(halted)" "" "(3) the mission's halt after two disagreeing bases in a row" #@ 033:678

    echo "j10: [033 (4)] three in a row raise an alert and never halt"
    judge_or_halt "$task" "$e2" "(4)"
    e3=$event
    t3=$(recorded_ms "$e3")
    ran=$(run_at run-3)
    echo "j10: 033: run 3 started $((ran - t2)) ms after run 2's record, gated '$verdict' (gate_aborts $before -> $after)"
    case "$due2" in
    '' | *[!0-9]*) measured_fail "(4) task #$task's gate ran a third time $((ran - t2)) ms after its second disagreeing base, with no due time to wait for" ;; #@ 033:688
    *) [ "$ran" -ge "$due2" ] || measured_fail "(4) task #$task's gate ran a third time at $ran ms, before its second host fault's due time $due2 ms" ;; #@ 033:689
    esac
    if [ "$verdict" != host_fault ]; then
        measured_fail "(4) task #$task's third disagreeing base in a row was gated '$verdict' ($(field "$e3" reason)), not 'host_fault'; gate_aborts $before -> $after" #@ 033:692
    fi
    expect_eq "$(field "$e3" host_faults)" 3 "(4) the third host fault's count in a row" #@ 033:694
    expect_eq "$(field "$e3" backoff_ms)" $((4 * BACKOFF_SECS * 1000)) "(4) the third host fault's backoff (doubled again)" #@ 033:695
    expect_eq "$(field "$e3" escalate)" false "(4) the third host fault's escalate" #@ 033:696
    expect_eq "$(field "$e3" base_disagrees)" 3 "(4) the base disagreements in a row the third host fault names" #@ 033:697
    expect_eq "$(base_disagrees)" 3 "(4) the mission's gate_base_disagrees after three in a row" #@ 033:698
    expect_eq "$(field "$e3" alert)" true "(4) the third host fault's alert (three in a row raise one, recorded in that record)" #@ 033:699
    due3=$(field "$e3" due_ms)
    # Before objective 030 exists the record is the alert, and the digest
    # takes it to the supervisor once.
    tag="host gate-base-disagrees-$e3"
    if [ "$verdict" = host_fault ]; then
        deadline=$(($(now) + 60))
        until [ "$(digests_like "$tag")" -ge 1 ]; do
            [ "$(now)" -lt "$deadline" ] || {
                measured_fail "(4) three disagreeing bases in a row on task #$task did not reach the supervisor within 60 s (no digest tagged '$tag')" #@ 033:708
                break
            }
            ping_supervisor
            sleep 0.5
        done
        if [ "$(digests_like "$tag")" -ge 1 ]; then
            line=$(digest_text_tagged "$tag" | grep -F "task #$task" | head -n 1)
            case "$line" in
            *"task #$task"*) ;;
            *) line="" ;;
            esac
            case "$line" in
            *011.sh*) ;;
            *) line="" ;;
            esac
            case "$line" in
            *"1, then 0"*) ;;
            *) line="" ;;
            esac
            case "$line" in
            *"3 in a row"*) ;;
            *) line="" ;;
            esac
            [ -n "$line" ] ||
                measured_fail "(4) the alert of three disagreeing bases does not name task #$task, 011.sh, its exits (1, then 0) and \`3 in a row\`: [$(digest_text_tagged "$tag" | head -n 3 | tr '\n' ' ')]" #@ 033:733
        fi
    else
        measured_fail "(4) the supervisor was not told of three disagreeing bases in a row (no host fault was recorded)" #@ 033:736
    fi
    # Two more passes: a halt rule fires at the start of a pass.
    integrate_once
    sleep 0.3
    integrate_once
    reason=$(halted)
    if [ -n "$reason" ]; then
        measured_fail "(4) three disagreeing bases in a row halted the mission ($reason), $(($(q "select max(recorded_at_ms) from spawn_events where session_id = 'session_control' and kind -> 'detail' -> 'record' ->> 'type' = 'mission.halted'") - t1)) ms after the first; a host fault never halts" #@ 033:744
    fi
    expect_eq "$(aborts)" 0 "(4) gate_aborts after three disagreeing bases" #@ 033:747

    echo "j10: [033 (5)] control: a real harness error on the base is still an abort"
    judge_or_halt "$task" "$e3" "(5)"
    e4=$event
    ran=$(run_at run-4)
    case "$due3" in
    '' | *[!0-9]*) ;;
    *) [ "$ran" -ge "$due3" ] || measured_fail "(5) task #$task's gate ran a fourth time at $ran ms, before its third host fault's due time $due3 ms" ;; #@ 033:756
    esac
    [ "$verdict" = aborted ] ||
        fail "(5) task #$task, whose base run of 011.sh exited 2 (a harness error, not a disagreement), was gated '$verdict' ($(field "$e4" reason)), not 'aborted'" #@ 033:759
    assert_eq "$after" "$((before + 1))" "(5) gate_aborts after a base that exited 2 (was $before)" #@ 033:760
    expect_eq "$(host_faults)" 0 "(5) the mission's gate_host_faults after a real abort" #@ 033:761
    expect_eq "$(base_disagrees)" 0 "(5) the mission's gate_base_disagrees after a real abort" #@ 033:762
    judge_or_halt "$task" "$e4" "(5)"
    e5=$event
    [ "$verdict" = aborted ] ||
        fail "(5) task #$task, whose base run of 011.sh exited 1 and then 2 (a harness error, not a disagreement), was gated '$verdict' ($(field "$e5" reason)), not 'aborted'" #@ 033:766
    assert_eq "$after" "$((before + 1))" "(5) gate_aborts after a base that exited 1, then 2 (was $before)" #@ 033:767
    judge_or_halt "$task" "$e5" "(5)"
    assert_eq "$verdict" accepted "(5) task #$task's verdict on its sixth gate run" #@ 033:769
    deadline=$(($(now) + 60))
    until [ "$(task_state "$task")" = landed ]; do
        [ "$(now)" -lt "$deadline" ] || fail "(5) task #$task was accepted but did not land within 60 s" #@ 033:772
        integrate_once
        sleep 0.5
    done
    assert_eq "$(aborts)" 0 "(5) gate_aborts after the landing" #@ 033:776
    expect_eq "$(halted)" "" "(5) the mission's halt at the end" #@ 033:777
    expect_eq "$(digests_like 'host gate-base-disagrees-%')" 1 "(5) base-disagreement alerts delivered" #@ 033:778
    expect_eq "$(digests_like 'host gate-host-fault-%')" 0 "(5) host-fault escalations delivered (none at two for a base that disagrees with itself)" #@ 033:779
    expect_eq "$(q "select count(*) from spawn_events where session_id = 'session_control'
                      and kind -> 'detail' -> 'record' ->> 'type' = 'mission.halted'
                      and kind -> 'detail' -> 'record' ->> 'id' = '$mission'")" 0 \
        "(5) halts of the mission" #@ 033:780
    echo "j10: 033: the run took $(($(now) - started)) s"
    echo "j10: 033: three bases that disagreed with themselves were recorded as host faults, not counted, judged again after a doubling backoff, never escalated at two, alerted once at three in a row, and never halted the mission; real harness errors on the base stayed aborts"
}

# =================================================================================
# The two lanes, side by side
# =================================================================================

started=$(date +%s)
pid_a=""
pid_b=""
lanes=$(mktemp -d) || harness "mktemp failed"
stop_lanes() {
    for pid in $pid_a $pid_b; do
        kill "$pid" 2>/dev/null || true
    done
    for pid in $pid_a $pid_b; do
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$lanes"
}
trap 'trap - EXIT; stop_lanes' EXIT
trap 'exit 130' INT TERM

(lane_a) &
pid_a=$!
(lane_b) &
pid_b=$!

# Both lanes run to their end, a failing one too: each part's checks are
# told even when another part's fail.
rc_a=""
rc_b=""
# A lane that ends without its result file died outside its trap: 2.
lane_result() { # <a|b> <pid>
    if [ -s "$lanes/$1.rc" ]; then
        wait "$2" 2>/dev/null || true
        cat "$lanes/$1.rc"
    elif ! kill -0 "$2" 2>/dev/null; then
        echo 2
    fi
}
while [ -z "$rc_a" ] || [ -z "$rc_b" ]; do
    if [ -z "$rc_a" ]; then
        rc_a=$(lane_result a "$pid_a")
        [ -z "$rc_a" ] || pid_a=""
    fi
    if [ -z "$rc_b" ]; then
        rc_b=$(lane_result b "$pid_b")
        [ -z "$rc_b" ] || pid_b=""
    fi
    [ -n "$rc_a" ] && [ -n "$rc_b" ] || sleep 1
done
trap - EXIT
rm -rf "$lanes"

if [ "$rc_a" -eq 1 ] || [ "$rc_b" -eq 1 ]; then
    echo "j10: an assertion failed (lane a exit $rc_a, lane b exit $rc_b)" >&2
    exit 1
fi
if [ "$rc_a" -ne 0 ] || [ "$rc_b" -ne 0 ]; then
    echo "j10: harness: lane a exit $rc_a, lane b exit $rc_b" >&2
    exit 2
fi
echo "j10: the verdicts journey passed in $(($(date +%s) - started)) s"
exit 0
