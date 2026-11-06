#!/bin/sh
# Black box: the owner holds landings on one mission source, as a record,
# and only landings on that source stop. An owner deploy gates a candidate
# against the live main for 25-80 minutes, then deploy/deploy.sh refuses
# unless the verdict's base is still the live main; a task landing in
# between makes the verdict stale (2026-11-06 21:45: task 85 landed during
# the owner's gate, the owner's merge conflicted, the gate ran again). The
# only way to prevent it today is to stop spawn-integrate for the whole
# owner gate, which also stops every other mission's gates and objective
# assignment (`objectives::assign_next` runs only in the integrator: Supply
# PA's six objectives pushed at 21:45 waited for the deploy). Required:
# `spawn mission hold --source <s>` and `spawn mission release --source <s>`
# (owner login only) record `source.held` and `source.released`; while a
# source is held no task lands on it, its gates still run and record their
# verdicts, an accepted task waits (`task.held`, once per task and hold) and
# lands after the release by the existing rules, and assignment, the gates
# of other sources and owner pushes go on. A hold is bounded
# (SPAWN_SOURCE_HOLD_SECS, read by the hold command, default 10800 s: the
# record's `expires_ms` is its own time plus it); past it the integrator
# records `source.hold_expired` (and only then lands), and the dispatcher's
# digest tells the supervisor once (tag `missions source-hold-expired-<id>`).
# The hold and the tasks waiting behind it are visible in `spawn mission
# list` / GET /api/missions (`holds`) and in GET /api/attention (`holds`),
# never as a landing stall.
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, the
# supervisor, no model judge) runs two residents on a fake engine: `alpha`
# in objective mission A on source SA (accept = gate, workers 2, a backlog
# the scenario writes), `beta` in mission B on source SB (accept = gate,
# tasks assigned by the scenario); the supervisor runs on the same fake
# engine. One long-running `spawn integrate` (not `--once`; started once,
# never restarted) judges through a recording gate (SPAWN_GATE) that
# accepts every candidate and notes each run; a candidate holding beta2.txt
# is held, its gate running, until the scenario releases it
# ($WORK/release-beta2). `spawn serve` answers GET /api/attention. Stall
# bounds are 1800 s except SPAWN_STALL_LANDING_SECS=10.
#   (1) the owner holds SA while the integrator runs: the command prints the
#       hold (`hold`: its `source.held` event id), whose record names
#       `source` SA, `main` SA's main, `by` human and `expires_ms` = its own
#       time + 10800000 (the default bound). Then mission A gets its backlog
#       (objective 1: alpha writes alpha.txt) and beta's task beta1 is
#       assigned on SB. Gates on SA still run: alpha's task is gated
#       `accepted`; beta1 lands on SB while SA is held. Over several passes
#       (12 s) alpha's task stays `landing`, SA's main is the hold's `main`,
#       no `task.landed` names it, exactly one `task.held` names it and the
#       hold, and no mission halts. `spawn mission list` shows the hold in
#       `holds` (source, hold, alpha's task in `waiting`) and as mission A's
#       `hold` (mission B's is null); GET /api/attention lists it in `holds`
#       with alpha's task waiting, and no landing stall for that task
#       (`stall-<task>-landing-*`) although it has waited past the 10 s bound.
#   (2) assignment goes on: objective 2 is added to mission A's backlog; within
#       30 s mission A has a task for it, while SA is still held (alpha's
#       task still `landing`, SA's main unchanged).
#   (3) the release: `spawn mission release --source SA` prints `released`
#       (the hold), its `source.released` record names the hold; within 30 s
#       alpha's task lands (after the release's record) and SA's main holds
#       alpha.txt; the hold is gone from `spawn mission list`. A second
#       release records nothing (`released` null, exit 0); a hold on a
#       source no mission names is refused and records nothing.
#   (4) the bound: beta's task beta2 is judged on SB, its gate held; the owner
#       holds SB with SPAWN_SOURCE_HOLD_SECS=20 (the record's `expires_ms` is
#       its time + 20000); released, the gate records `accepted` and beta2
#       waits (`task.held` naming that hold); until `expires_ms` (on the
#       database's clock) beta2 does not land and SB's main is the hold's.
#       Within 30 s of `expires_ms` a `source.hold_expired` names the hold,
#       its source and beta2 in `waiting`, recorded at or after
#       `expires_ms`; beta2 lands after that record. Within 60 s a digest
#       tagged `missions source-hold-expired-<that record's id>` reaches the
#       supervisor, its line naming SB and task #<beta2>; in the end exactly
#       one such digest was delivered (the released hold made none), no
#       mission ever halted, and the integrator is the same process.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (a3543c2) and on the deployed binary
# (built from ffc9fe7): `spawn mission hold` is not a command ("spawn:
# usage: ..."); today the owner stops spawn-integrate for the whole owner
# gate instead. It passes once the owner's hold and release are records the
# running integrator acts on: no task lands on a held source (its gates
# still run, its accepted tasks wait, once recorded each), everything else
# goes on (other sources' landings, assignment), the release lets the
# waiting tasks land by the existing rules, a hold past its recorded
# `expires_ms` is ended by a `source.hold_expired` record and told to the
# supervisor once, and the hold is visible in the mission board and the
# attention API without a false landing stall.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket and a loopback port, no network. Every wait is bounded
# in time.
#
# Today (2026-11-06, the deployed binary): `spawn mission hold --source
# <SA>` exits 1 with the usage of `spawn mission` ("spawn: usage: ...");
# the scenario fails at (1) 0.8-1.0 s after it starts.
set -eu

asserted=""
harness() {
    echo "034: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep curl; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_034_hold_$run"
SPAWN_TMUX_SOCKET="bb-034-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
integrate_pid=""
serve_pid=""
started=$(date +%s)
cleanup() {
    # Released, so a held gate ends on its own.
    : >"$WORK/release-beta2" 2>/dev/null || true
    for pid in "$serve_pid" "$integrate_pid" "$loop_pid"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
    if [ -n "$database_made" ]; then
        psql -d postgres -tA -c "drop database if exists $SPAWN_DB_NAME with (force)" \
            >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        echo "034: ended after $(($(date +%s) - started)) s" >&2
        if [ -s "$WORK/loop.log" ]; then
            echo "034: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "034: the integrator's last lines:" >&2
            tail -n 15 "$WORK/integrate.log" >&2
        fi
        if [ -s "$WORK/judged.log" ]; then
            echo "034: the gate runs (time event name base candidate):" >&2
            cat "$WORK/judged.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "034: the control records (id type task/source verdict hold):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, r ->> 'type',
                       coalesce(r ->> 'id', r ->> 'source', ''),
                       coalesce(r ->> 'verdict', ''), coalesce(r ->> 'hold', '')
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control'
                  and (r ->> 'type' in ('task.gated', 'task.landed', 'task.held', 'task.assigned',
                                        'mission.halted')
                       or r ->> 'type' like 'source.%')
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "034: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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
sa=$WORK/sa.git
sb=$WORK/sb.git
# A repository no mission names: a hold on it is refused.
sc=$WORK/sc.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "base"
for bare in "$repo" "$sa" "$sb" "$sc"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

gate=$WORK/gate
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
: >"$WORK/judged.log"

# Mission A's backlog: objective 1 now (alpha writes alpha.txt); objective
# 2 is added in (2), a task that writes nothing.
backlog=$WORK/backlog-a
mkdir -p "$backlog"
printf '{"id": 1, "title": "Write alpha.txt", "prompt": "Write alpha.txt.\\nBB:WRITE alpha.txt\\nNothing else."}\n' \
    >"$WORK/objective-1.json"
printf '{"id": 2, "title": "Think it over", "prompt": "Read the README and think it over. Change nothing."}\n' \
    >"$WORK/objective-2.json"

# The recording gate: accepts every candidate; one holding beta2.txt runs
# until $WORK/release-beta2 exists (at most 600 s). Notes
# `<time> start|end <name> <base> <candidate>`.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
name=other
for file in beta2 beta1 alpha; do
    if git --git-dir="\$1" cat-file -e "\$3:\$file.txt" 2>/dev/null; then name=\$file; break; fi
done
printf '%s start %s %s %s\n' "\$(date +%s)" "\$name" "\$2" "\$3" >>'$WORK/judged.log'
echo "== candidate: build"
if [ "\$name" = beta2 ]; then
    waited=0
    until [ -e '$WORK/release-beta2' ] || [ "\$waited" -ge 1200 ]; do
        sleep 0.5
        waited=\$((waited + 1))
    done
fi
echo "| build: ok"
printf '%s end %s %s %s\n' "\$(date +%s)" "\$name" "\$2" "\$3" >>'$WORK/judged.log'
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
chmod 755 "$WORK/fake-gate.sh"

# The fake engine: a resident's text naming `BB:WRITE <file>` opens a turn
# that writes the file, waits for the pusher to publish it (at most 30 s),
# then writes the report. Any other text (a digest, a per-task worker's
# brief) is a turn that changes nothing.
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
agent=agent_${SPAWN_SESSION_ID#session_}
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
file=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$SPAWN_SESSION_ID:$typed" in
    session_resident_*"BB:WRITE "*) [ -n "$file" ] || file=${typed#*BB:WRITE }; file=${file%% *} ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$file" ] && [ -n "${BB_SOURCE-}" ]; then
      printf '%s was here\n' "$agent" >"$file"
      turn_ends "wrote $file"
      start=$(date +%s)
      until [ "$(git --git-dir="$BB_SOURCE" show "refs/heads/spawn/$agent:$file" 2>/dev/null)" = "$(cat "$file")" ]; do
        [ $(($(date +%s) - start)) -ge 30 ] && break
        sleep 0.2
      done
      line '"type":"user","message":{"role":"user","content":"report"}'
      sleep 2
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$file" >.spawn/report.md
    fi
    file=
    turn_ends idle
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"
# The engine waits on its own mission's source: alpha's is SA, beta's SB.
cat >"$WORK/claude-wrapper" <<EOF
#!/bin/sh
case "\$SPAWN_SESSION_ID" in
session_resident_alpha) BB_SOURCE='$sa' ;;
session_resident_beta) BB_SOURCE='$sb' ;;
*) BB_SOURCE= ;;
esac
export BB_SOURCE
exec '$WORK/claude' "\$@"
EOF
chmod 755 "$WORK/claude-wrapper"

cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_GATE_MIN_FREE_GB \
    SPAWN_GATE_MIN_FREE_MEMORY_MB SPAWN_INTEGRATE_GATES SPAWN_TEST_DATABASE_LOG \
    SPAWN_SOURCE_HOLD_SECS SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL SPAWN_SERVE_LOCAL_OPEN \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS \
    SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS \
    SPAWN_STALL_HALTED_SECS; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude-wrapper
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
# The stall bounds out of the way, except the landing's: a task waiting
# behind a hold is past it in (1), and must not read as a landing stall.
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=10
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
db_now_ms() { q "select spawn_now_ms()"; }
task_state() { q "select coalesce((select state from mission_tasks where id = $1), '')"; }
halted() { q "select coalesce(halted, '') from mission_missions where id = $1"; }
tasks_line() { q "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks"; }
log_tail() { tail -n 6 "$WORK/integrate.log" 2>/dev/null | tr '\n' ' '; }
main_of() { git --git-dir="$1" rev-parse main; }
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }
integrator_alive() { [ -n "$integrate_pid" ] && kill -0 "$integrate_pid" 2>/dev/null; }
field() { q "select coalesce(kind -> 'detail' -> 'record' ->> '$2', '') from spawn_events where id = $1"; }
recorded_ms() { q "select recorded_at_ms from spawn_events where id = $1"; }
# The control records of type `$1` whose field `$2` is `$3`, after event
# `$4` (0: any): how many, and the first's event id.
CONTROL="from spawn_events e where e.session_id = 'session_control'
           and e.event_name = 'control.recorded'"
records() { # <type> <field> <value> <after>
    q "select count(*) $CONTROL and e.kind -> 'detail' -> 'record' ->> 'type' = '$1'
         and e.kind -> 'detail' -> 'record' ->> '$2' = '$3' and e.id > $4"
}
first_record() { # <type> <field> <value> <after>
    q "select coalesce(min(e.id), 0) $CONTROL and e.kind -> 'detail' -> 'record' ->> 'type' = '$1'
         and e.kind -> 'detail' -> 'record' ->> '$2' = '$3' and e.id > $4"
}
accepted_record() { # <task>: the event id of its first accepted verdict, 0 if none
    q "select coalesce(min(e.id), 0) $CONTROL
         and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
         and e.kind -> 'detail' -> 'record' ->> 'verdict' = 'accepted'
         and e.kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
# Exactly the `task.held` records naming task `$1` and hold `$2`.
held_records() {
    q "select count(*) $CONTROL and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.held'
         and e.kind -> 'detail' -> 'record' ->> 'id' = '$1'
         and e.kind -> 'detail' -> 'record' ->> 'hold' = '$2'"
}
no_halt() { # <what>
    for m in "$mission_a" "$mission_b"; do
        [ -z "$(halted "$m")" ] || fail "$1: mission $m halted: $(halted "$m")"
    done
}
wait_state() { # <task> <state> <seconds> <what>
    deadline=$(($(now) + $3))
    until [ "$(task_state "$1")" = "$2" ]; do
        [ "$(now)" -lt "$deadline" ] ||
            harness "$4: task #$1 did not reach $2 within $3 s (tasks: $(tasks_line); integrate: $(log_tail))"
        integrator_alive || fail "$4: the integrator exited: $(log_tail)"
        sleep 1
    done
}
gate_started() { grep -q " start $1 " "$WORK/judged.log"; }
wait_gate() { # <name> <what>
    deadline=$(($(now) + 180))
    until gate_started "$1"; do
        [ "$(now)" -lt "$deadline" ] ||
            harness "$2: $1's gate did not start within 180 s (tasks: $(tasks_line); integrate: $(log_tail))"
        sleep 0.5
    done
}
# hold <source> <label> <what> [<bound s>]: the owner holds a source; sets
# `hold` (the event id it printed, a `source.held` record).
hold() {
    status=0
    if [ -n "${4-}" ]; then
        env SPAWN_SOURCE_HOLD_SECS="$4" "$SPAWN_BIN" mission hold --source "$1" \
            >"$WORK/out.txt" 2>"$WORK/err.txt" || status=$?
    else
        "$SPAWN_BIN" mission hold --source "$1" >"$WORK/out.txt" 2>"$WORK/err.txt" || status=$?
    fi
    [ "$status" -eq 0 ] ||
        fail "$3 \`spawn mission hold --source <$2>\` exited $status: $(err | head -c 300)"
    hold=$(jq -r '.hold // empty' "$WORK/out.txt" 2>/dev/null || true)
    case "$hold" in
    '' | *[!0-9]*) fail "$3 \`spawn mission hold --source <$2>\` printed no hold (its record's event id): $(out | head -c 300)" ;;
    esac
    assert_eq "$(field "$hold" type)" source.held "$3 the record of hold $hold"
    assert_eq "$(field "$hold" source)" "$1" "$3 the source hold $hold names"
    assert_eq "$(field "$hold" by)" human "$3 who hold $hold names"
}
# `spawn mission list` into $WORK/board.json.
board() {
    capture "$SPAWN_BIN" mission list
    [ "$status" -eq 0 ] || fail "\`spawn mission list\` exited $status: $(err | head -c 300)"
    jq -e . "$WORK/out.txt" >/dev/null 2>&1 || fail "\`spawn mission list\` printed no JSON: $(out | head -c 300)"
    cp "$WORK/out.txt" "$WORK/board.json"
}
# The hold on source `$1` in the `holds` of a JSON answer `$2`:
# `<hold> <waiting task ids, comma-separated>`, empty when none.
hold_in() {
    jq -r --arg s "$1" '[(.holds // [])[] | select(.source == $s)] | first
        | if . == null then "" else "\(.hold) \((.waiting // []) | map(tostring) | join(","))" end' \
        "$2" 2>/dev/null || true
}
board_mission_hold() { # <mission>: the mission's `hold` on the board, "null" if none
    jq -r --argjson m "$1" '[.missions[] | select(.id == $m)] | first | .hold // null
        | if type == "object" then .hold else . end | tostring' "$WORK/board.json" 2>/dev/null || true
}
# A record's numeric field `$2` minus its own time (recorded_at_ms); empty
# when the field is missing or not a number.
after_own_time() {
    q "select case when jsonb_typeof(kind -> 'detail' -> 'record' -> '$2') = 'number'
                   then ((kind -> 'detail' -> 'record' ->> '$2')::numeric - recorded_at_ms)::bigint::text
                   else '' end
         from spawn_events where id = $1"
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

# --- two missions, two sources, the dashboard -------------------------------------

for name in alpha beta; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 034 $name change."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$name"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
done
new_mission() { # <text> <source>
    capture "$SPAWN_BIN" mission add --text "$1" --source "$2"
    [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
    m=$(jq -r .mission "$WORK/out.txt")
    [ -n "$m" ] && [ "$m" != null ] || harness "mission add printed no id: $(out)"
    echo "$m"
}
mission_a=$(new_mission "034 A" "$sa")
mission_b=$(new_mission "034 B" "$sb")
capture "$SPAWN_BIN" mission configure --id "$mission_b" --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission configure B exited $status: $(err)"
assign() { # <mission> <resident> <file>
    capture "$SPAWN_BIN" task assign --mission "$1" --kind resident --worker "$2" \
        --brief "Write $3. BB:WRITE $3"
    [ "$status" -eq 0 ] || harness "task assign for $2 failed: $(err)"
    jq -r .task "$WORK/out.txt"
}

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
attention() { # GET /api/attention into $WORK/attention.json
    code=""
    i=0
    while [ "$i" -lt 50 ]; do
        i=$((i + 1))
        code=$(curl -s --noproxy '*' -o "$WORK/attention.json" -w '%{http_code}' \
            "http://127.0.0.1:$port/api/attention" 2>/dev/null) || code=""
        [ -n "$code" ] && [ "$code" != 000 ] && break
        sleep 0.1
    done
    [ "$code" = 200 ] || fail "GET /api/attention answered $code: $(head -c 300 "$WORK/attention.json" 2>/dev/null)"
}

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" SPAWN_GATE_MIN_FREE_GB=0 \
    SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
    "$SPAWN_BIN" integrate --root "$WORK/integrator" >>"$WORK/integrate.log" 2>&1 &
integrate_pid=$!

# --- (1) the owner holds SA; its gates run, its accepted task waits ---------------------

sa_main=$(main_of "$sa")
hold "$sa" SA "(1)"
hold_a=$hold
assert_eq "$(field "$hold_a" main)" "$sa_main" "(1) the main hold $hold_a names (SA's main)"
assert_eq "$(after_own_time "$hold_a" expires_ms)" 10800000 \
    "(1) hold $hold_a's expires_ms minus its own time (the default bound, 3 h)"
assert_eq "$(jq -r '.main // empty' "$WORK/out.txt")" "$sa_main" "(1) the main \`spawn mission hold\` printed"

# Mission A gets its backlog now, so its first objective is assigned while
# SA is held; beta's first task is assigned on SB.
cp "$WORK/objective-1.json" "$backlog/1.json"
capture "$SPAWN_BIN" mission configure --id "$mission_a" --gate "$gate" --accept gate \
    --backlog "$backlog" --resident alpha --workers 2
[ "$status" -eq 0 ] || harness "mission configure A exited $status: $(err)"
t_beta1=$(assign "$mission_b" beta beta1.txt)

deadline=$(($(now) + 120))
t_alpha=""
until [ -n "$t_alpha" ]; do
    t_alpha=$(q "select coalesce(min(id)::text, '') from mission_tasks
                  where mission_id = $mission_a and objective = 1")
    [ -n "$t_alpha" ] && break
    [ "$(now)" -lt "$deadline" ] ||
        fail "(1) mission A's objective 1 was not assigned within 120 s while SA was held (tasks: $(tasks_line); integrate: $(log_tail))"
    integrator_alive || fail "(1) the integrator exited: $(log_tail)"
    sleep 1
done
# The other source lands while SA is held.
deadline=$(($(now) + 240))
until [ "$(accepted_record "$t_beta1")" -gt 0 ]; do
    [ "$(now)" -lt "$deadline" ] ||
        harness "(1) beta1 on SB was not accepted within 240 s (tasks: $(tasks_line); integrate: $(log_tail))"
    integrator_alive || fail "(1) the integrator exited: $(log_tail)"
    sleep 1
done
deadline=$(($(now) + 30))
until [ "$(task_state "$t_beta1")" = landed ]; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(1) task #$t_beta1, accepted on SB (not held), did not land within 30 s while SA was held (state $(task_state "$t_beta1"); integrate: $(log_tail))"
    integrator_alive || fail "(1) the integrator exited: $(log_tail)"
    sleep 1
done
[ "$(task_state "$t_alpha")" != landed ] ||
    fail "(1) task #$t_alpha landed on SA while SA was held (hold $hold_a): SA's main moved $sa_main -> $(main_of "$sa")"

# Gates on a held source still run and record their verdicts: alpha's
# task is reported (the world works), then gated.
deadline=$(($(now) + 180))
until [ "$(accepted_record "$t_alpha")" -gt 0 ]; do
    case "$(task_state "$t_alpha")" in
    landed) fail "(1) task #$t_alpha landed on SA while SA was held (hold $hold_a)" ;;
    reported | accepted | landing) break ;;
    esac
    [ "$(now)" -lt "$deadline" ] ||
        harness "(1) alpha's task #$t_alpha was not reported within 180 s (tasks: $(tasks_line))"
    integrator_alive || fail "(1) the integrator exited: $(log_tail)"
    sleep 1
done
deadline=$(($(now) + 60))
until [ "$(accepted_record "$t_alpha")" -gt 0 ]; do
    [ "$(task_state "$t_alpha")" != landed ] ||
        fail "(1) task #$t_alpha landed on SA while SA was held (hold $hold_a)"
    [ "$(now)" -lt "$deadline" ] ||
        fail "(1) alpha's reported task #$t_alpha on held SA was not gated within 60 s: gates on a held source still run (tasks: $(tasks_line); integrate: $(log_tail))"
    integrator_alive || fail "(1) the integrator exited: $(log_tail)"
    sleep 1
done
accepted_alpha=$(accepted_record "$t_alpha")

# Over several passes (the integrator sleeps 5 s between them) the task
# waits: nothing lands on SA.
waited_until=$(($(now) + 12))
while [ "$(now)" -lt "$waited_until" ]; do
    state=$(task_state "$t_alpha")
    [ "$state" = landing ] ||
        fail "(1) task #$t_alpha, accepted on held SA, is '$state', not 'landing' (waiting behind hold $hold_a)"
    assert_eq "$(main_of "$sa")" "$sa_main" "(1) SA's main while held (hold $hold_a)"
    assert_eq "$(records task.landed id "$t_alpha" 0)" 0 "(1) task.landed records of task #$t_alpha while SA is held"
    no_halt "(1) while SA is held"
    integrator_alive || fail "(1) the integrator exited: $(log_tail)"
    sleep 1
done
assert_eq "$(held_records "$t_alpha" "$hold_a")" 1 \
    "(1) task.held records naming task #$t_alpha and hold $hold_a after several passes (one per task and hold)"
[ "$(first_record task.held id "$t_alpha" "$accepted_alpha")" -gt 0 ] ||
    fail "(1) task #$t_alpha's task.held does not follow its accepted verdict (event $accepted_alpha)"

# The hold and its waiting task are visible, and the wait is not a stall.
board
seen=$(hold_in "$sa" "$WORK/board.json")
case "$seen" in
"$hold_a "*) ;;
*) fail "(1) \`spawn mission list\` shows no hold $hold_a on SA in \`holds\`: [$seen]" ;;
esac
case ",${seen#* }," in
*",$t_alpha,"*) ;;
*) fail "(1) \`spawn mission list\` does not show task #$t_alpha waiting behind hold $hold_a: [$seen]" ;;
esac
assert_eq "$(board_mission_hold "$mission_a")" "$hold_a" "(1) mission A's \`hold\` on the board"
assert_eq "$(board_mission_hold "$mission_b")" null "(1) mission B's \`hold\` on the board (SB is not held)"
attention
listed=$(hold_in "$sa" "$WORK/attention.json")
case "$listed" in
"$hold_a "*) ;;
*) fail "(1) GET /api/attention lists no hold $hold_a on SA in \`holds\` while task #$t_alpha waits behind it: [$listed]" ;;
esac
case ",${listed#* }," in
*",$t_alpha,"*) ;;
*) fail "(1) GET /api/attention does not show task #$t_alpha waiting behind hold $hold_a: [$listed]" ;;
esac
stalls=$(jq -r --arg t "stall-$t_alpha-landing-" '[(.pending // [])[], (.told // [])[]]
    | map(.tag // "" | select(contains($t))) | join(" ")' "$WORK/attention.json")
assert_eq "$stalls" "" "(1) landing stalls of task #$t_alpha, waiting behind hold $hold_a past SPAWN_STALL_LANDING_SECS"

# --- (2) assignment goes on while SA is held ------------------------------------------

cp "$WORK/objective-2.json" "$backlog/2.json"
deadline=$(($(now) + 30))
until [ "$(q "select count(*) from mission_tasks where mission_id = $mission_a and objective = 2")" -ge 1 ]; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(2) objective 2, added to mission A's backlog while SA is held, was not assigned within 30 s (tasks: $(tasks_line); integrate: $(log_tail))"
    integrator_alive || fail "(2) the integrator exited: $(log_tail)"
    sleep 1
done
assert_eq "$(task_state "$t_alpha")" landing "(2) task #$t_alpha's state after objective 2 was assigned (SA still held)"
assert_eq "$(main_of "$sa")" "$sa_main" "(2) SA's main after objective 2 was assigned (SA still held)"
no_halt "(2)"

# --- (3) the release ------------------------------------------------------------------

capture "$SPAWN_BIN" mission release --source "$sa"
[ "$status" -eq 0 ] || fail "(3) \`spawn mission release --source <SA>\` exited $status: $(err | head -c 300)"
assert_eq "$(jq -r '.released // empty' "$WORK/out.txt")" "$hold_a" "(3) the hold \`spawn mission release\` printed"
released_a=$(first_record source.released hold "$hold_a" 0)
[ "$released_a" -gt 0 ] || fail "(3) no source.released record names hold $hold_a"
assert_eq "$(field "$released_a" source)" "$sa" "(3) the source the release names"
deadline=$(($(now) + 30))
until [ "$(task_state "$t_alpha")" = landed ]; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(3) task #$t_alpha, waiting behind hold $hold_a, did not land within 30 s of the release (state $(task_state "$t_alpha"); integrate: $(log_tail))"
    integrator_alive || fail "(3) the integrator exited: $(log_tail)"
    sleep 1
done
[ "$(first_record task.landed id "$t_alpha" "$released_a")" -gt 0 ] ||
    fail "(3) task #$t_alpha's task.landed does not follow the release (event $released_a)"
git --git-dir="$sa" cat-file -e main:alpha.txt 2>/dev/null || fail "(3) SA's main does not hold alpha.txt"
board
assert_eq "$(hold_in "$sa" "$WORK/board.json")" "" "(3) the board's hold on SA after the release"
no_halt "(3) after the release"
# A release with no hold records nothing; a hold on a source no mission
# names is refused.
before=$(last_event)
capture "$SPAWN_BIN" mission release --source "$sa"
[ "$status" -eq 0 ] || fail "(3) a second \`spawn mission release --source <SA>\` exited $status: $(err | head -c 300)"
assert_eq "$(jq -r '.released' "$WORK/out.txt" 2>/dev/null)" null "(3) the hold a second release printed"
assert_eq "$(q "select count(*) $CONTROL and e.id > $before
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'source.%'")" 0 \
    "(3) source records after a release with no hold"
capture "$SPAWN_BIN" mission hold --source "$sc"
[ "$status" -ne 0 ] || fail "(3) \`spawn mission hold\` on a source no mission names exited 0: $(out | head -c 300)"
assert_eq "$(records source.held source "$sc" 0)" 0 "(3) holds recorded on a source no mission names"

# --- (4) a hold past its bound expires by record, and the supervisor is told once -------

deadline=$(($(now) + 120))
until waiting session_supervisor; do
    [ "$(now)" -lt "$deadline" ] || harness "the supervisor was never judged waiting"
    sleep 0.5
done
t_beta2=$(assign "$mission_b" beta beta2.txt)
wait_gate beta2 "(4)"
sb_main=$(main_of "$sb")
hold "$sb" SB "(4)" 20
hold_b=$hold
assert_eq "$(after_own_time "$hold_b" expires_ms)" 20000 \
    "(4) hold $hold_b's expires_ms minus its own time (SPAWN_SOURCE_HOLD_SECS=20)"
expires=$(field "$hold_b" expires_ms)
: >"$WORK/release-beta2"
deadline=$(($(now) + 15))
until [ "$(accepted_record "$t_beta2")" -gt 0 ] && [ "$(held_records "$t_beta2" "$hold_b")" -ge 1 ]; do
    [ "$(task_state "$t_beta2")" != landed ] ||
        fail "(4) task #$t_beta2 landed on SB while SB was held (hold $hold_b, expires $expires, now $(db_now_ms))"
    if [ "$(now)" -ge "$deadline" ]; then
        if [ "$(accepted_record "$t_beta2")" -gt 0 ]; then
            fail "(4) task #$t_beta2, accepted on held SB, has no task.held naming hold $hold_b within 15 s"
        fi
        harness "(4) beta2's released gate recorded no verdict within 15 s (tasks: $(tasks_line); integrate: $(log_tail))"
    fi
    sleep 0.5
done
[ "$(recorded_ms "$(accepted_record "$t_beta2")")" -lt "$expires" ] ||
    harness "(4) beta2's verdict came after hold $hold_b's expires_ms: the host is too slow for a 20 s bound"
while [ "$(db_now_ms)" -lt "$expires" ]; do
    expired=$(first_record source.hold_expired hold "$hold_b" 0)
    [ "$expired" -eq 0 ] ||
        fail "(4) hold $hold_b's expiry was recorded at $(recorded_ms "$expired") ms, before its expires_ms $expires"
    state=$(task_state "$t_beta2")
    [ "$state" = landing ] ||
        fail "(4) task #$t_beta2, accepted on held SB, is '$state' before hold $hold_b's expires_ms, not 'landing'"
    assert_eq "$(main_of "$sb")" "$sb_main" "(4) SB's main while held (hold $hold_b)"
    no_halt "(4) while SB is held"
    sleep 1
done
deadline=$(($(now) + 30))
until [ "$(first_record source.hold_expired hold "$hold_b" 0)" -gt 0 ]; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(4) hold $hold_b on SB was not recorded expired within 30 s of its expires_ms $expires (task #$t_beta2 is $(task_state "$t_beta2"); integrate: $(log_tail))"
    integrator_alive || fail "(4) the integrator exited: $(log_tail)"
    sleep 1
done
expired=$(first_record source.hold_expired hold "$hold_b" 0)
assert_eq "$(field "$expired" source)" "$sb" "(4) the source the expiry names"
[ "$(recorded_ms "$expired")" -ge "$expires" ] ||
    fail "(4) hold $hold_b's expiry was recorded at $(recorded_ms "$expired") ms, before its expires_ms $expires"
assert_eq "$(q "select (kind -> 'detail' -> 'record' -> 'waiting') @> '[$t_beta2]'::jsonb
                  from spawn_events where id = $expired")" t "(4) task #$t_beta2 among the expiry's \`waiting\`"
deadline=$(($(now) + 30))
until [ "$(task_state "$t_beta2")" = landed ]; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(4) task #$t_beta2 did not land within 30 s of hold $hold_b's expiry record (state $(task_state "$t_beta2"); integrate: $(log_tail))"
    integrator_alive || fail "(4) the integrator exited: $(log_tail)"
    sleep 1
done
[ "$(first_record task.landed id "$t_beta2" "$expired")" -gt 0 ] ||
    fail "(4) task #$t_beta2's task.landed does not follow the expiry record (event $expired)"
git --git-dir="$sb" cat-file -e main:beta2.txt 2>/dev/null || fail "(4) SB's main does not hold beta2.txt"
tag="missions source-hold-expired-$expired"
deadline=$(($(now) + 60))
until [ "$(digests_like "$tag")" -ge 1 ]; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(4) the expiry of hold $hold_b on SB did not reach the supervisor within 60 s (no digest tagged '$tag')"
    ping_supervisor
    sleep 0.5
done
line=$(digest_text_tagged "$tag" | grep -F "task #$t_beta2" | grep -F "$sb" | head -n 1)
[ -n "$line" ] ||
    fail "(4) the supervisor's digest about hold $hold_b's expiry does not name SB ($sb) and task #$t_beta2 on one line: [$(digest_text_tagged "$tag" | head -n 4 | tr '\n' ' ')]"
# Told once: more digests come (pings keep the supervisor current), none
# about the expiry again, and none about the released hold.
settle=$(($(now) + 15))
while [ "$(now)" -lt "$settle" ]; do
    ping_supervisor
    sleep 0.5
done
assert_eq "$(digests_like "$tag")" 1 "(4) digests about hold $hold_b's expiry (told once)"
assert_eq "$(digests_like 'missions source-hold-expired-%')" 1 "(4) digests about expired holds (the released one is none)"
assert_eq "$(q "select count(*) $CONTROL and e.kind -> 'detail' -> 'record' ->> 'type' = 'mission.halted'")" 0 \
    "(4) halts of any mission"
integrator_alive || fail "(4) the integrator is not running at the end: $(log_tail)"

echo "034: the owner's holds were records the running integrator acted on: no task landed on a held source while its gates ran and another source landed, assignment went on, the release and the recorded expiry let the waiting tasks land, and the supervisor was told of the expiry once ($(($(now) - started)) s)"
exit 0
