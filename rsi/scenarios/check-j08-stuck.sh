#!/bin/sh
# Journey j08, stuck: work that cannot reach its worker is never left
# stranded. A delivery whose retries are spent without ever typing is
# re-issued once, escalated only when the re-issue is spent too, and tried
# once more after a deploy; a model-switch Stop that fails is retried, then
# escalated, and its task is never stranded.
#
# Folds Spawn scenarios 019 and 006 (design-gate-journeys-spawn.md §2),
# every numbered check of each:
#   019 (1) spent: task T1 (frail1, broken) has its delivery's retries spent
#           within 75 s (019: 150 s): every attempt failed, none may have
#           typed, and the failures past the free refusals number more than
#           three.
#   019 (2) re-issued, not escalated: no sooner than 4.5 s and within 25 s
#           (019: 9 s and 50 s) after the last attempt, T1 is cancelled by
#           `spawn-dispatch` (`by`, `reissued_as` T2, `attempts` with keys);
#           T2's `task.assigned` names `reissue_of` T1 and copies T1's
#           mission, kind, worker, session, brief and model; the supervisor
#           never gets T1's delivery failure.
#   019 (3) the re-issue goes through: the host fixed, T2 is delivered
#           within 60 s; the only re-issue of T1, never escalated.
#   019 (4) escalated only when the re-issue is spent too: T3 (frail2,
#           broken for good) is re-issued as T4 within 100 s (019: 200 s), T4
#           is spent within 75 s (019: 150 s); then, within 60 s, the
#           supervisor gets T4's delivery failure naming task #T4 and #T3,
#           never T3's; T4 is not re-issued (watched 7.5 s, 019: 15 s).
#   019 (5) after a deploy, tried once more: `spawn mission moved` records
#           `source.moved`; within 60 s exactly one new attempt of T4's
#           delivery, keyed `deliver-T4-0-moved-<event id>`, succeeds.
#   006 (1) a failed switch Stop is retried under a new idempotency key;
#   006 (2) at most 5 Stops for the task, none after the escalation;
#   006 (3) a digest to the supervisor escalates it (tag
#           `session_resident_core <id>`, not the approval), naming
#           `task #<task>`;
#   006 (4) the task stays `assigned`: never delivered, never dropped.
#
# Shortened waits (design §3): 019's re-issue chain runs at half the values
# 019 sets: SPAWN_DELIVERY_RETRY_SECS 1 (019: 2), SPAWN_REISSUE_AFTER_SECS 5
# (019: 10), SPAWN_DELIVERY_NOT_READY_MS 250 (019: 500; the product's floor
# is 100 ms). Every bound derived from them is halved with them (above, and
# marked `rewritten` in the trace); the bounds that do not derive from them
# (the 60 s deliveries and escalation, the 120/90 s waits for a judged
# prompt) are 019's.
#
# How the parts share setup: one database, one tmux socket, one `spawn
# loop`, one supervisor. 019's residents frail1 and frail2 and 006's
# resident core, with 006's task, are all made before the loop starts (006
# needs its task and the resident's start pending at the loop's first
# pass). Then 006 runs side by side with 019, in a subshell, each with its
# own polling and its own bounds. One fake engine (019's, a superset of
# 006's) and one tmux wrapper carry both faults: 019's `load-buffer`
# refusal of texts naming `BB:FRAIL<n>` while $WORK/broken-<n> exists, and
# 006's refusal of every `kill-session` while $WORK/refuse-kill exists (019
# kills no session). The settings do not cross: 019's delivery waits are
# not read by the Stop pursuit (dispatch/stops.rs uses its own fixed
# backoff), and 019's long stall bounds (1800 s) are beyond 006's defaults'
# reach in this run (none fires within 600 s, and 006's task is resident).
# Each part counts only its own records (its session, its task ids).
# spawn_commands, spawn_events, spawn_session_state, agent_executions and
# mission_tasks are read where 019 and 006 read them, with their assertions.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
# Named check-*: the gate runs a failing check again, and on the base if it fails twice (design-gate-v2.md C2)
#
# Needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own
# database, uses a private tmux socket, no network. Every wait is bounded in
# time.
set -eu

asserted=""
harness() {
    echo "j08: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

# 019's 150 s, halved with its retry and not-ready waits (design §3).
SPENT_WITHIN=${BB_019_SPENT_WITHIN:-75}
case "$SPENT_WITHIN" in '' | *[!0-9]*) harness "BB_019_SPENT_WITHIN is a number of seconds" ;; esac

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
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_j08_stuck_$run"
SPAWN_TMUX_SOCKET="bb-j08-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
s006_pid=""
cleanup() {
    if [ -n "$s006_pid" ]; then
        kill "$s006_pid" 2>/dev/null || true
        wait "$s006_pid" 2>/dev/null || true
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
            echo "j08: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "j08: the tasks (id state):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, state, agent_id from mission_tasks order by id" >&2 2>/dev/null || true
            echo "j08: [019] the delivery attempts (key, ok, error):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select idempotency_key, coalesce(succeeded::text, 'open'),
                       left(coalesce(error, ''), 90)
                  from spawn_commands where idempotency_key like 'deliver-%' order by id" >&2 2>/dev/null || true
            echo "j08: [006] the Stops of the resident core (key, state, ok):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select idempotency_key, state, coalesce(succeeded::text, 'open')
                  from spawn_commands where session_id = 'session_resident_core'
                   and action ->> 'type' = 'stop' order by id" >&2 2>/dev/null || true
            echo "j08: the digests the supervisor received (id, tags):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, action -> 'tags' from spawn_commands
                where session_id = 'session_supervisor' and principal = 'spawn-dispatch'
                  and action ->> 'type' = 'input' and succeeded order by id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "j08: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

# Logins of this journey's databases that a run killed before its cleanup
# left behind (scripts/blackbox/scenario-db.sh): this prefix only.
sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the journey database"
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

# `capture` writes to $CAP (each part its own: they run side by side).
CAP=$WORK
capture() {
    status=0
    "$@" >"$CAP/out.txt" 2>"$CAP/err.txt" || status=$?
    return 0
}
out() { cat "$CAP/out.txt"; }
err() { cat "$CAP/err.txt"; }

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

# 019's two bare repositories (006's one repository is the same `repo`).
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
base=$(git --git-dir="$source" rev-parse main)

# The fake engine (019's; 006's is the same without `BB:WORK`): a prompt;
# text typed in opens a turn, which ends after a second of quiet, the
# transcript's end of the turn first, then the prompt. A turn whose text
# names `BB:WORK <file>` writes the file and the report.
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
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
work=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in *"BB:WORK "*) [ -n "$work" ] || work=${typed#*BB:WORK } ;; esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$work" ]; then
      set -- $work
      printf 'done\n' >"$1"
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$1" >.spawn/report.md
      printf 'wrote %s %s\n' "$(date +%s)" "$1" >>"$BB_WORK/engine.log"
      work=
    fi
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# tmux on the private socket, with both parts' faults.
# 006: while $WORK/refuse-kill exists, no session can be killed: every Stop
# of a live worker fails at its terminal.
# 019: while $WORK/broken-<n> exists, a text naming `BB:FRAIL<n>` cannot be
# staged: `load-buffer` (the Bridge stages every text from a file) fails as
# tmux fails a command over its message size. Nothing reaches the pane.
cat >"$WORK/tmux" <<EOF
#!/bin/sh
if [ -e '$WORK/refuse-kill' ]; then
  for argument in "\$@"; do
    if [ "\$argument" = kill-session ]; then
      echo "kill-session refused by the scenario" >&2
      exit 1
    fi
  done
fi
stage=
for argument in "\$@"; do
  [ "\$argument" = load-buffer ] && stage=1
  last=\$argument
done
if [ -n "\$stage" ] && [ -f "\$last" ]; then
  for n in 1 2; do
    if [ -e '$WORK'/broken-\$n ] && grep -q "BB:FRAIL\$n" "\$last" 2>/dev/null; then
      echo "command too long" >&2
      exit 1
    fi
  done
fi
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"
: >"$WORK/refuse-kill"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_DELIVERY_RETRY_SECS \
    SPAWN_REISSUE_AFTER_SECS SPAWN_DELIVERY_NOT_READY_MS; do
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
SPAWN_SUPERVISOR=1
SPAWN_MISSION_FILE=$WORK/mission.md
# Free disk is not what this journey is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
# 019's waits under test, at half 019's values (design §3); the stall bounds
# out of the way.
RETRY_SECS=1
REISSUE_AFTER=5
SPAWN_DELIVERY_RETRY_SECS=$RETRY_SECS
SPAWN_REISSUE_AFTER_SECS=$REISSUE_AFTER
SPAWN_DELIVERY_NOT_READY_MS=250
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_SUPERVISOR SPAWN_MISSION_FILE SPAWN_DISK_WARN_GB \
    SPAWN_DISK_FLOOR_GB SPAWN_DELIVERY_RETRY_SECS SPAWN_REISSUE_AFTER_SECS SPAWN_DELIVERY_NOT_READY_MS \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS \
    SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/engine.log"

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

# --- helpers both parts use ---------------------------------------------------------

now() { date +%s; }
q() { scalar "$1"; }
task_state() { q "select state from mission_tasks where id = $1"; }

# The session is judged waiting at its prompt, live.
waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$1'")" = WAITING ] &&
        [ "$(q "select count(*) from agent_executions
                where session_id = '$1' and ended_at_ms is null")" -ge 1 ]
}

# The supervisor gets a digest only while its latest judgement covers its
# latest fact: a line is typed into it now and then (not as `human`: no
# news). Each part pings on its own schedule, under its own keys.
pings=0
next_ping=0
PING_PART=019
ping_supervisor() {
    [ "$(now)" -ge "$next_ping" ] || return 0
    next_ping=$(($(now) + 8))
    pings=$((pings + 1))
    capture "$SPAWN_BIN" submit --principal bb-harness --agent agent_supervisor \
        --session session_supervisor --key "bb-ping-$PING_PART-$pings" \
        --action '{"type":"input","text":"ping"}'
    [ "$status" -eq 0 ] || harness "submit of a ping to the supervisor failed: $(err)"
}

# --- the residents and 006's task, before the loop ---------------------------------

capture "$SPAWN_BIN" mission add --text "019 re-issue" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$CAP/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"

for name in frail1 frail2; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 019 $name concern."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$name"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
    capture "$SPAWN_BIN" submit --principal bb-harness --agent "agent_resident_$name" \
        --session "session_resident_$name" --key "bb-start-$name" \
        --action '{"type":"start","engine":"claude","prompt":""}'
    [ "$status" -eq 0 ] || harness "submit of $name's start failed: $(err)"
done

# 006: a resident on the default profile, with a task on the cheap one.
session=session_resident_core
agent=agent_resident_core
capture "$SPAWN_BIN" mission add --text "006 model switch"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission006=$(jq -r .mission "$CAP/out.txt")
[ -n "$mission006" ] && [ "$mission006" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" resident propose --name core --charter "Owns the 006 concern."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name core
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
# The task first, so the resident's workspace comes from the mission from the
# start. No loop runs yet, and the resident's own start (below) is pending
# when the loop's first pass begins: the dispatcher never delivers the task
# with a start of its own.
capture "$SPAWN_BIN" task assign --mission "$mission006" --kind resident --worker core \
    --brief "work for the cheap profile" --model cheap
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(jq -r .task "$CAP/out.txt")
[ -n "$task" ] && [ "$task" != null ] || harness "task assign printed no id: $(out)"
# The resident runs on the default profile. Not submitted as `human`: a
# human's command is news for the supervisor.
capture "$SPAWN_BIN" submit --principal bb-harness --agent "$agent" --session "$session" \
    --key bb-start-core --action '{"type":"start","engine":"claude","prompt":""}'
[ "$status" -eq 0 ] || harness "submit of the resident's start failed: $(err)"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

# =============================================================================
# Part 006, side by side with 019 in a subshell: the switch Stop that fails.
# =============================================================================
part_006() {
    CAP=$WORK/cap-006
    mkdir -p "$CAP"
    PING_PART=006
    pings=0
    asserted=""
    harness() {
        echo "j08: [006] harness: $*" >&2
        exit 2
    }
    p006_finish() {
        rc=$?
        trap - EXIT
        if [ "$rc" -eq 0 ]; then exit 0; fi
        if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
        [ "$rc" -eq 2 ] || echo "j08: [006] harness: unexpected exit $rc" >&2
        exit 2
    }
    trap p006_finish EXIT
    trap 'exit 130' INT TERM

    # wait_or_harness <seconds> <what> <command...>: a harness error past the limit.
    wait_or_harness() {
        limit=$1
        what=$2
        shift 2
        deadline=$(($(now) + limit))
        until "$@"; do
            [ "$(now)" -lt "$deadline" ] || harness "$what did not happen within $limit s"
            sleep 0.25
        done
    }
    stops() {
        q "select count(*) from spawn_commands
            where session_id = '$session' and principal = 'spawn-dispatch'
              and action ->> 'type' = 'stop'"
    }
    stop_keys() {
        q "select count(distinct idempotency_key) from spawn_commands
            where session_id = '$session' and principal = 'spawn-dispatch'
              and action ->> 'type' = 'stop'"
    }
    stops_succeeded() {
        q "select count(*) from spawn_commands
            where session_id = '$session' and action ->> 'type' = 'stop' and succeeded"
    }
    stop_finished() {
        [ "$(q "select count(*) from spawn_commands
                where session_id = '$session' and action ->> 'type' = 'stop'
                  and state = 'finished'")" -ge 1 ]
    }
    stops_failed() {
        q "select count(*) from spawn_commands
            where session_id = '$session' and principal = 'spawn-dispatch'
              and action ->> 'type' = 'stop' and state = 'finished' and not succeeded"
    }
    # Every failed Stop of the session happened two hours earlier: any retry
    # wait of an hour or less is over.
    age_failed_stops() {
        sql "update spawn_commands
                set created_at_ms = created_at_ms - 7200000,
                    finished_at_ms = finished_at_ms - 7200000
              where session_id = '$session' and action ->> 'type' = 'stop'
                and state = 'finished' and not succeeded
                and finished_at_ms > (extract(epoch from clock_timestamp()) * 1000)::bigint - 3600000" \
            >/dev/null
    }
    # A digest to the supervisor that escalates this task: an item about the
    # resident's session other than its approval, in a text that names the task.
    escalated006() {
        q "select count(*) from spawn_commands c
            where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
              and c.action ->> 'type' = 'input'
              and position('task #$task' in c.action ->> 'text') > 0
              and exists (select 1 from jsonb_array_elements_text(
                              case when jsonb_typeof(c.action -> 'tags') = 'array'
                                   then c.action -> 'tags' else '[]'::jsonb end) tag
                           where tag like '$session %' and tag <> '$session approved-core')"
    }
    # The worker is left alone on purpose: a failed Stop changes nothing on it.
    ping006() {
        pings=$((pings + 1))
        capture "$SPAWN_BIN" submit --principal bb-harness --agent agent_supervisor \
            --session session_supervisor --key "bb-ping-006-$pings" \
            --action '{"type":"input","text":"ping"}'
        [ "$status" -eq 0 ] || harness "submit of a ping to the supervisor failed: $(err)"
    }

    echo "j08: [006 (1)] the switch Stop of a resident on the default profile, which fails"
    deadline=$(($(now) + 120))
    until [ "$(stops)" -ge 1 ]; do
        if [ "$(now)" -ge "$deadline" ]; then
            waiting "$session" ||
                harness "the resident was never judged waiting at its prompt (judged: $(q "select observation ->> 'state' from spawn_session_state where session_id = '$session'"))"
            fail "no Stop of the worker within 120 s of a cheap task for a worker on the default profile"
        fi
        sleep 0.25
    done
    # Read as the dispatcher reads it: no model named is the default profile.
    [ "$(q "select coalesce(kind -> 'detail' ->> 'model', 'default') from spawn_events
            where session_id = '$session' and event_name = 'execution.started'
            order by session_seq desc limit 1")" = default ] ||
        harness "the resident does not run on the default profile"
    wait_or_harness 90 "the supervisor judged waiting at its prompt" waiting session_supervisor
    wait_or_harness 60 "the first Stop finished" stop_finished
    [ "$(stops_succeeded)" -eq 0 ] || harness "a Stop succeeded although the terminal refuses to die"
    [ "$(stops_failed)" -ge 1 ] || harness "the first Stop did not fail"

    echo "j08: [006 (1)][006 (2)][006 (3)] retries under new keys, at most 5, then the escalation"
    deadline=$(($(now) + 240))
    next_ping=$(($(now) + 8))
    while [ "$(escalated006)" -eq 0 ]; do
        [ "$(stops_succeeded)" -eq 0 ] || harness "a Stop succeeded although the terminal refuses to die"
        if [ "$(now)" -ge "$deadline" ]; then
            [ "$(stop_keys)" -ge 2 ] ||
                fail "(1) the failed switch Stop was never retried under a new key (Stops: $(stops), task: $(task_state "$task"))"
            fail "(3) no digest escalated task #$task to the supervisor within 240 s ($(stops) Stops under $(stop_keys) keys, task: $(task_state "$task"))"
        fi
        [ "$(stops)" -le 5 ] || fail "(2) more than 5 Stops for one task without an escalation ($(stops))"
        age_failed_stops
        if [ "$(now)" -ge "$next_ping" ]; then
            ping006
            next_ping=$(($(now) + 8))
        fi
        sleep 0.5
    done
    [ "$(stop_keys)" -ge 2 ] || fail "(1) escalated, but the failed switch Stop was never retried under a new key"
    [ "$(stop_keys)" -eq "$(stops)" ] || fail "(1) a Stop key was used twice"

    echo "j08: [006 (2)] no Stop after the escalation"
    # After the escalation, no more Stops: a few passes, with failed Stops aged.
    settled=$(stops)
    [ "$settled" -le 5 ] || fail "(2) $settled Stops for one task (at most 5)"
    end=$(($(now) + 6))
    while [ "$(now)" -lt "$end" ]; do
        age_failed_stops
        sleep 0.5
    done
    assert_eq "$(stops)" "$settled" "(2) Stops of the worker after the escalation"
    echo "j08: [006 (4)] the task stays assigned, never delivered"
    assert_eq "$(task_state "$task")" assigned "(4) the task's state"
    assert_eq "$(q "select count(*) from spawn_events where session_id = 'session_control'
                      and kind -> 'detail' -> 'record' ->> 'type' = 'task.delivered'
                      and kind -> 'detail' -> 'record' ->> 'id' = '$task'")" 0 \
        "(4) task.delivered records of the task"

    echo "j08: [006] a failed switch Stop was retried under new keys ($settled Stops), then escalated to the supervisor; the task stayed assigned"
    exit 0
}

( part_006 ) &
s006_pid=$!

# =============================================================================
# Part 019, in the foreground: the delivery that never typed.
# =============================================================================

# wait_until <seconds> <command...>: true once the command is, false past
# the limit; pings the supervisor while it waits.
wait_until() {
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        ping_supervisor
        sleep 0.5
    done
}

# The attempts at task `$1`'s first delivery: its key and its retries.
ATTEMPTS="idempotency_key = 'deliver-\$t-0' or idempotency_key like 'deliver-\$t-0-retry-%'"
attempts_of() { # <task> <select list>
    t=$1
    q "select $2 from spawn_commands where $(printf '%s' "$ATTEMPTS" | sed "s/\\\$t/$t/g")"
}
# `spent` when the task's first delivery is spent as the dispatcher judges
# it (deliver.rs `attempts`): every attempt finished failed, none may have
# typed, and more than three failures past the ten free refusals for a pane
# not ready; otherwise a short account of where it stands.
spent_state() {
    line=$(attempts_of "$1" "count(*) || ' ' ||
        count(*) filter (where succeeded) || ' ' ||
        count(*) filter (where state <> 'finished') || ' ' ||
        count(*) filter (where error like '%does not accept input%') || ' ' ||
        count(*) filter (where error like '%may have been typed%')")
    set -- $line
    total=$1 ok=$2 open=$3 refusals=$4 typed=$5
    free=$((refusals < 10 ? refusals : 10))
    if [ "$ok" -eq 0 ] && [ "$open" -eq 0 ] && [ "$typed" -eq 0 ] && [ $((total - free)) -gt 3 ]; then
        echo spent
    else
        echo "attempts $total, succeeded $ok, open $open, refused as not ready $refusals, may have typed $typed"
    fi
}
is_spent() { [ "$(spent_state "$1")" = spent ]; }
spent_at() { attempts_of "$1" "max(finished_at_ms)"; }

control() { # <type> <task id>: event id of the latest such record
    q "select coalesce(max(id), 0) from spawn_events
        where session_id = 'session_control' and event_name = 'control.recorded'
          and kind -> 'detail' -> 'record' ->> 'type' = '$1'
          and kind -> 'detail' -> 'record' ->> 'id' = '$2'"
}
rec() { # <event id> <jq-ish field path in SQL>
    q "select kind -> 'detail' -> 'record' $2 from spawn_events where id = $1"
}
reissue_of() { # tasks whose task.assigned names reissue_of $1
    q "select coalesce(string_agg(kind -> 'detail' -> 'record' ->> 'id', ' '), '') from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.assigned'
          and kind -> 'detail' -> 'record' ->> 'reissue_of' = '$1'"
}
reissued() { [ -n "$(reissue_of "$1")" ]; }

# The digests delivered to the supervisor carrying tag `$1`.
digests_with() {
    q "select count(*) from spawn_commands c,
              lateral jsonb_array_elements_text(
                  case when jsonb_typeof(c.action -> 'tags') = 'array'
                       then c.action -> 'tags' else '[]'::jsonb end) tag
        where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
          and c.action ->> 'type' = 'input' and c.state = 'finished' and c.succeeded
          and tag = '$1'"
}
digest_text_with() {
    sql "select c.action ->> 'text' from spawn_commands c,
              lateral jsonb_array_elements_text(
                  case when jsonb_typeof(c.action -> 'tags') = 'array'
                       then c.action -> 'tags' else '[]'::jsonb end) tag
        where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
          and c.action ->> 'type' = 'input' and c.succeeded and tag = '$1'
        order by c.id limit 1"
}
escalated() { [ "$(digests_with "$1")" -ge 1 ]; }
delivered() { case "$(task_state "$1")" in delivered | reported | accepted) return 0 ;; esac; return 1; }

for name in frail1 frail2; do
    wait_until 120 waiting "session_resident_$name" ||
        harness "resident $name was never judged waiting at its prompt"
done
wait_until 90 waiting session_supervisor || harness "the supervisor was never judged waiting"

# --- the broken host, two tasks -------------------------------------------------------

: >"$WORK/broken-1"
: >"$WORK/broken-2"
assign() {
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker "$1" \
        --brief "$2"
    [ "$status" -eq 0 ] || harness "task assign for $1 failed: $(err)"
    jq -r .task "$CAP/out.txt"
}
t1=$(assign frail1 "Write f1. BB:FRAIL1 BB:WORK f1.txt")
t3=$(assign frail2 "Write f2. BB:FRAIL2 BB:WORK f2.txt")
for id in "$t1" "$t3"; do
    [ -n "$id" ] && [ "$id" != null ] || harness "task assign printed no id"
done

# --- (1) T1's delivery is spent -----------------------------------------------------------

echo "j08: [019 (1)] task #$t1's delivery, refused before typing, is spent within $SPENT_WITHIN s"
wait_until "$SPENT_WITHIN" is_spent "$t1" ||
    fail "(1) task #$t1's delivery, refused before typing at every attempt, was not spent within $SPENT_WITHIN s with SPAWN_DELIVERY_RETRY_SECS=$RETRY_SECS: $(spent_state "$t1")"
t1_spent=$(spent_at "$t1")
t1_attempts=$(attempts_of "$t1" "count(*)")

# --- (2) re-issued, not escalated ------------------------------------------------------------

echo "j08: [019 (2)] re-issued, not escalated, no sooner than $((REISSUE_AFTER * 1000 - 500)) ms and within $((REISSUE_AFTER + 20)) s of the last attempt"
# 019: REISSUE_AFTER + 40 at REISSUE_AFTER=10 (50 s); halved: 25 s.
deadline=$((t1_spent / 1000 + REISSUE_AFTER + 20))
until reissued "$t1"; do
    if [ "$(now)" -ge "$deadline" ]; then
        if escalated "session_resident_frail1 deliver-$t1-0"; then
            fail "(2) task #$t1's delivery was spent ($t1_attempts attempts, none typed) and escalated to the supervisor ('$(digest_text_with "session_resident_frail1 deliver-$t1-0" | grep -F "task #$t1" | head -n 1)'), not re-issued; task #$t1 is $(task_state "$t1")"
        fi
        fail "(2) task #$t1's delivery was spent ($t1_attempts attempts, none typed) and not re-issued within $((REISSUE_AFTER + 20)) s; task #$t1 is $(task_state "$t1")"
    fi
    ping_supervisor
    sleep 0.5
done
t2=$(reissue_of "$t1")
# The host is fixed as soon as the re-issue exists.
rm -f "$WORK/broken-1"
case "$t2" in *" "*) fail "(2) task #$t1 was re-issued more than once: tasks $t2" ;; esac
cancelled=$(control task.cancelled "$t1")
[ "$cancelled" -gt 0 ] || fail "(2) task #$t1 was re-issued as #$t2 but has no task.cancelled record"
assert_eq "$(task_state "$t1")" cancelled "(2) task #$t1's state after its re-issue"
assert_eq "$(rec "$cancelled" "->> 'by'")" spawn-dispatch "(2) the task.cancelled record's by"
assert_eq "$(rec "$cancelled" "->> 'reissued_as'")" "$t2" "(2) the task.cancelled record's reissued_as"
assert_eq "$(rec "$cancelled" "-> 'attempts' ->> 0 is not null")" t \
    "(2) the task.cancelled record names its attempts"
assert_eq "$(q "select jsonb_array_length(kind -> 'detail' -> 'record' -> 'attempts') from spawn_events where id = $cancelled")" \
    "$t1_attempts" "(2) the attempts named by the task.cancelled record"
assert_eq "$(q "select count(*) from spawn_events, jsonb_array_elements(kind -> 'detail' -> 'record' -> 'attempts') a
                 where id = $cancelled and a ->> 'key' like 'deliver-$t1-0%'")" "$t1_attempts" \
    "(2) the attempts of the task.cancelled record that name their key"
reissue_at=$(q "select recorded_at_ms from spawn_events where id = $cancelled")
# 019: REISSUE_AFTER * 1000 - 1000 at REISSUE_AFTER=10 (9000 ms); halved: 4500 ms.
[ $((reissue_at - t1_spent)) -ge $((REISSUE_AFTER * 1000 - 500)) ] ||
    fail "(2) task #$t1 was re-issued $(((reissue_at - t1_spent) / 1000)) s after its last attempt, before SPAWN_REISSUE_AFTER_SECS=$REISSUE_AFTER"
assigned=$(control task.assigned "$t2")
for field in mission_id kind agent_id session_id brief model; do
    assert_eq "$(q "select kind -> 'detail' -> 'record' ->> '$field' from spawn_events where id = $assigned")" \
        "$(q "select kind -> 'detail' -> 'record' ->> '$field' from spawn_events where id = $(control task.assigned "$t1")")" \
        "(2) the re-issue #$t2's $field against task #$t1's"
done
[ "$(digests_with "session_resident_frail1 deliver-$t1-0")" -eq 0 ] ||
    fail "(2) task #$t1's spent delivery, re-issued, was escalated to the supervisor too"

# --- (3) the re-issue goes through -------------------------------------------------------------

echo "j08: [019 (3)] the re-issue #$t2, the host fixed, is delivered within 60 s, the only one, never escalated"
wait_until 60 delivered "$t2" ||
    fail "(3) the re-issue #$t2, the host fixed, was not delivered within 60 s: $(spent_state "$t2")"
assert_eq "$(reissue_of "$t1")" "$t2" "(3) the re-issues of task #$t1"
[ "$(digests_with "session_resident_frail1 deliver-$t2-0")" -eq 0 ] ||
    fail "(3) the re-issue #$t2's delivery, which went through, was escalated"

# --- (4) a spent re-issue is escalated ----------------------------------------------------------

echo "j08: [019 (4)] task #$t3 re-issued, the re-issue spent, then escalated naming both; never re-issued again"
# 019: SPENT_WITHIN + REISSUE_AFTER + 40 (200 s); halved: 75 + 5 + 20 = 100 s.
wait_until $((SPENT_WITHIN + REISSUE_AFTER + 20)) reissued "$t3" ||
    fail "(4) task #$t3 was not re-issued within $((SPENT_WITHIN + REISSUE_AFTER + 20)) s: $(spent_state "$t3")"
t4=$(reissue_of "$t3")
case "$t4" in *" "*) fail "(4) task #$t3 was re-issued more than once: tasks $t4" ;; esac
wait_until "$SPENT_WITHIN" is_spent "$t4" ||
    fail "(4) the re-issue #$t4's delivery, refused at every attempt, was not spent within $SPENT_WITHIN s: $(spent_state "$t4")"
t4_tag="session_resident_frail2 deliver-$t4-0"
wait_until 60 escalated "$t4_tag" ||
    fail "(4) the re-issue #$t4, whose delivery is spent too, was not escalated to the supervisor within 60 s (no digest tagged '$t4_tag')"
line=$(digest_text_with "$t4_tag" | grep -F "task #$t4" | head -n 1)
assert_contains "$line" "task #$t4" "(4) the escalation of the spent re-issue"
assert_contains "$line" "#$t3" "(4) the escalation of the spent re-issue (the task it re-issued)"
[ "$(digests_with "session_resident_frail2 deliver-$t3-0")" -eq 0 ] ||
    fail "(4) task #$t3's spent delivery, re-issued, was escalated too"
# 019: REISSUE_AFTER + 5 at REISSUE_AFTER=10 (15 s); halved: 7.5 s.
sleep "$((REISSUE_AFTER + 2)).5"
assert_eq "$(reissue_of "$t4")" "" "(4) the re-issues of the spent re-issue #$t4"

# --- (5) after a deploy, tried once more ------------------------------------------------------------

echo "j08: [019 (5)] after a deploy (source.moved), exactly one more attempt of #$t4's delivery, which succeeds"
rm -f "$WORK/broken-2"
git -C "$WORK/seed" commit -q --allow-empty -m "deploy: the host is fixed"
git -C "$WORK/seed" push -q "$source" main:main
moved_to=$(git --git-dir="$source" rev-parse main)
before_moved=$(q "select coalesce(max(id), 0) from spawn_events")
capture "$SPAWN_BIN" mission moved --source "$source" --from "$base" --to "$moved_to"
[ "$status" -eq 0 ] || fail "(5) spawn mission moved exited $status: $(err)"
moved=$(q "select coalesce(min(id), 0) from spawn_events
            where session_id = 'session_control' and id > $before_moved
              and kind -> 'detail' -> 'record' ->> 'type' = 'source.moved'")
[ "$moved" -gt 0 ] ||
    fail "(5) \`spawn mission moved\` recorded no source.moved (it printed $(out | tr -d '\n ' | head -c 120)): the deploy is not a fact anything can act on"
moved_ms=$(q "select recorded_at_ms from spawn_events where id = $moved")
wait_until 60 delivered "$t4" ||
    fail "(5) the spent re-issue #$t4 was not delivered within 60 s of the deploy's source.moved: $(q "select count(*) from spawn_commands where idempotency_key like 'deliver-$t4-0%' and created_at_ms >= $moved_ms") attempts since"
after=$(q "select string_agg(idempotency_key || ':' || coalesce(succeeded::text, 'open'), ' ' order by id)
            from spawn_commands where idempotency_key like 'deliver-$t4-0%' and created_at_ms >= $moved_ms")
assert_eq "$after" "deliver-$t4-0-moved-$moved:true" "(5) the attempts of #$t4's delivery after the deploy"

echo "j08: [019] a delivery spent without typing was re-issued once and went through; a spent re-issue was escalated, naming both; after a deploy the spent delivery was tried once more and delivered"

# --- part 006's verdict ------------------------------------------------------------

rc006=0
wait "$s006_pid" || rc006=$?
s006_pid=""
case "$rc006" in
0) ;;
1)
    asserted=1
    echo "j08: [006] an assertion failed (above)" >&2
    exit 1
    ;;
*) harness "part 006 exited $rc006" ;;
esac

echo "j08: work that could not reach its worker was never stranded: a spent delivery re-issued once, a spent re-issue escalated, tried again after a deploy; a failed switch Stop retried, escalated, its task kept assigned"
exit 0
