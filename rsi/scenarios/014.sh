#!/bin/sh
# Black box: a landing is one commit per task on main's first-parent line,
# whose message says what landed; the worker's checkpoints stay reachable
# from main, off that line.
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, no
# model judge, no supervisor) runs the resident of one gated objective
# mission (accept = gate, one worker) with a fake engine, and
# `spawn integrate --once` passes, run by the scenario, land its work
# through a recording gate (SPAWN_GATE) that accepts every candidate. The
# backlog holds two objectives (931, 932), each naming one file and a word
# (`BB:TASK <file> <word>`). The engine writes the file in one turn and,
# once its checkpoint is pushed, the report in a second turn; the report's
# `## What changed` says `summary: <file> now holds <word>`, a sentence no
# brief contains. Once both tasks landed:
#   (1) main's first-parent line from the base commit is exactly two
#       commits, the newer one landing the later task, and the base is
#       their first-parent ancestor.
#   (2) Each one's message: its first line holds the objective's title and
#       is not a checkpoint's (`spawn checkpoint ...`); the message names
#       the task (`task <id>`) and the objective (`objective <id>`, zero
#       padding allowed) and holds the report's summary sentence after the
#       first line; it holds no other task's id or summary.
#   (3) Each one's tree is the tree of the candidate in its task's
#       `task.gated` accepted record.
#   (4) Each task's `task.landed` record names that commit in `to`.
#   (5) Every checkpoint the resident recorded before the second task was
#       gated (its `artifact.committed` commits) is reachable from main: the
#       history is kept, off the first-parent line.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) or (2) on an integrator that lands a worker's pushed
# head itself (main then reads `spawn checkpoint exec_...`, one commit per
# checkpoint). Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs
# SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own
# database, uses a private tmux socket, no network. Every wait is bounded in
# time.
set -eu

asserted=""
harness() {
    echo "014: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_014_landing_$run"
SPAWN_TMUX_SOCKET="bb-014-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
cleanup() {
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
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
            echo "014: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "014: the integrator's last lines:" >&2
            tail -n 15 "$WORK/integrate.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "014: main's first-parent line:" >&2
            git --git-dir="$WORK/source.git" log --first-parent --format='%h %p %s' main >&2 2>/dev/null || true
            echo "014: the gate verdicts:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select r ->> 'id', r ->> 'verdict', left(coalesce(r ->> 'reason', ''), 200)
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control' and r ->> 'type' = 'task.gated'
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "014: harness: unexpected exit $rc" >&2
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

# The mission's scenarios directory: the recording gate below judges, this
# only has to exist and hold a scenario (the integrator counts them).
gate=$WORK/gate
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
: >"$WORK/judged.log"

# The recording gate: accepts every candidate and notes what it judged.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
printf '%s %s %s\n' "\$(date +%s)" "\$2" "\$3" >>'$WORK/judged.log'
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
chmod 755 "$WORK/fake-gate.sh"

# The backlog: two objectives, each a different file, with titles of their
# own.
objective() {
    printf '{"id": %s, "title": "%s", "prompt": "Write the file %s.\\nBB:TASK %s %s\\nNothing else."}\n' \
        "$2" "$3" "$4" "$4" "$5" >"$1/$2.json"
}
mkdir -p "$WORK/backlog"
objective "$WORK/backlog" 931 "Greet the harbour with a kestrel" HARBOUR.txt kestrel
objective "$WORK/backlog" 932 "Mark the lighthouse with an osprey" LIGHT.txt osprey

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet. It writes its session transcript where Claude Code does,
# so the Bridge and the Observer see turns as they see a real engine's. A
# turn whose text carried `BB:TASK <file> <word>` writes the file and ends;
# once the source's spawn/<agent> holds it (or 30 s passed), a second turn
# writes the report.
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
# turn_ends <text>: the screen settles at the prompt first, then the
# transcript ends the turn (the order objective 011's scenario needs on a
# base without the Observer's coverage rule).
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1
  line '"type":"system","subtype":"turn_duration","durationMs":1'
}
pushed() {
  git --git-dir="$BB_WORK/source.git" cat-file -e "refs/heads/spawn/$1:$2" 2>/dev/null
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
task=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:TASK "*) [ -n "$task" ] || task=${typed#*BB:TASK } ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$task" ]; then
      set -- $task
      file=$1
      word=$2
      printf '%s\n' "$word" >"$file"
      printf 'wrote %s %s %s\n' "$(date +%s)" "$SPAWN_SESSION_ID" "$file" >>"$BB_WORK/engine.log"
      turn_ends "wrote $file"
      agent=agent_${SPAWN_SESSION_ID#session_}
      start=$(date +%s)
      until pushed "$agent" "$file"; do
        [ $(($(date +%s) - start)) -ge 30 ] && break
        sleep 0.2
      done
      line '"type":"user","message":{"role":"user","content":"report"}'
      sleep 2
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\nsummary: %s now holds %s\n## Open issues\nnone\n' \
        "$file" "$word" >.spawn/report.md
      printf 'reported %s %s %s\n' "$(date +%s)" "$SPAWN_SESSION_ID" "$file" >>"$BB_WORK/engine.log"
      task=
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
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR; do
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
# Free disk is not what this scenario is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"

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

integrate_once() {
    capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || fail "integrate exited $status: $(err)"
    return 0
}

# A field of the task's latest control record of a type (and verdict).
record_field() {
    q "select kind -> 'detail' -> 'record' ->> '$3' from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = '$2'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'
          and ('$4' = '' or kind -> 'detail' -> 'record' ->> 'verdict' = '$4')
        order by id desc limit 1"
}
landed() {
    q "select count(*) from mission_tasks where mission_id = $1 and state = 'landed'"
}
mission_line() {
    q "select string_agg(id || ':' || state || ':' || coalesce(objective::text, '-'), ' ' order by id)
         from mission_tasks where mission_id = $1"
}
log_tail() { tail -n 6 "$WORK/integrate.log" 2>/dev/null | tr '\n' ' '; }
tree_of() {
    git --git-dir="$source" rev-parse --verify -q "$1^{tree}" 2>/dev/null && return 0
    for mirror in "$WORK"/integrator/.sources/*.git; do
        git --git-dir="$mirror" rev-parse --verify -q "$1^{tree}" 2>/dev/null && return 0
    done
    echo "missing:$1"
}
# The message names id <n> after <word>: `task 12`, `Task #12`, `objective 0931`.
names() {
    printf '%s\n' "$1" | grep -Eiq "$2[^0-9a-z]{0,3}0*$3([^0-9]|\$)"
}

capture "$SPAWN_BIN" mission add --text "014 landing messages" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" resident propose --name scribe --charter "Pursues the 014 objectives."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name scribe
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate \
    --backlog "$WORK/backlog" --resident scribe
[ "$status" -eq 0 ] || harness "mission configure exited $status: $(err)"

# --- both objectives land --------------------------------------------------------

deadline=$(($(now) + 240))
until [ "$(landed "$mission")" -ge 2 ]; do
    if [ "$(now)" -ge "$deadline" ]; then
        harness "the 2 objectives did not both land within 240 s (tasks: $(mission_line "$mission"); integrate: $(log_tail))"
    fi
    integrate_once
    sleep 0.5
done
for _ in 1 2; do integrate_once; done

tasks=$(q "select string_agg(id::text, ' ' order by id) from mission_tasks
            where mission_id = $mission and state = 'landed'")
set -- $tasks
[ "$#" -eq 2 ] || harness "expected 2 landed tasks, got: $(mission_line "$mission")"
first_task=$1
second_task=$2
[ "$(q "select objective from mission_tasks where id = $first_task")" = 931 ] &&
    [ "$(q "select objective from mission_tasks where id = $second_task")" = 932 ] ||
    harness "the tasks are not objectives 931 then 932: $(mission_line "$mission")"
main=$(git --git-dir="$source" rev-parse main)

# --- (1) one commit per task on main's first-parent line ------------------------------

line=$(git --git-dir="$source" rev-list --first-parent "$base..$main" | tr '\n' ' ')
set -- $line
[ "$#" -eq 2 ] ||
    fail "(1) main's first-parent line holds $# commits above the base after two landings, not 2: $(git --git-dir="$source" log --first-parent --format='%h %s' "$base..$main" | head -8 | tr '\n' ';')"
second_landing=$1
first_landing=$2
assert_eq "$(git --git-dir="$source" rev-parse "$first_landing^1")" "$base" \
    "(1) the first landing's first parent (the base)"

# --- (2)-(4) each landing's message, tree and record -------------------------------

check() { # <commit> <task> <objective> <title> <summary> <other task> <other summary>
    message=$(git --git-dir="$source" show -s --format=%B "$1")
    subject=$(printf '%s\n' "$message" | head -1)
    body=$(printf '%s\n' "$message" | tail -n +2)
    case "$subject" in
    "spawn checkpoint"*) fail "(2) the landing of task $2 reads like a checkpoint: [$subject]" ;;
    esac
    assert_contains "$subject" "$4" "(2) the first line of task $2's landing $1"
    names "$message" task "$2" ||
        fail "(2) the landing of task $2 does not name task $2: [$(printf '%s' "$message" | tr '\n' '|')]"
    names "$message" objective "$3" ||
        fail "(2) the landing of task $2 does not name objective $3: [$(printf '%s' "$message" | tr '\n' '|')]"
    assert_contains "$body" "$5" "(2) the body of task $2's landing (after its first line)"
    assert_not_contains "$message" "$7" "(2) the landing of task $2 (the other task's summary)"
    if names "$message" task "$6"; then
        fail "(2) the landing of task $2 names task $6 too: [$(printf '%s' "$message" | tr '\n' '|')]"
    fi
    candidate=$(record_field "$2" task.gated candidate accepted)
    [ -n "$candidate" ] || fail "(3) task $2 has no accepted task.gated record naming a candidate"
    assert_eq "$(tree_of "$1")" "$(tree_of "$candidate")" \
        "(3) the tree of task $2's landing $1 against its gated candidate $candidate"
    assert_eq "$(record_field "$2" task.landed to '')" "$1" "(4) task $2's task.landed 'to'"
}
check "$first_landing" "$first_task" 931 "Greet the harbour with a kestrel" \
    "summary: HARBOUR.txt now holds kestrel" "$second_task" "summary: LIGHT.txt now holds osprey"
check "$second_landing" "$second_task" 932 "Mark the lighthouse with an osprey" \
    "summary: LIGHT.txt now holds osprey" "$first_task" "summary: HARBOUR.txt now holds kestrel"
assert_eq "$(git --git-dir="$source" show "$main:HARBOUR.txt")" kestrel "HARBOUR.txt on main"
assert_eq "$(git --git-dir="$source" show "$main:LIGHT.txt")" osprey "LIGHT.txt on main"

# --- (5) the checkpoints stay reachable ---------------------------------------------

gated_at=$(q "select min(id) from spawn_events
    where session_id = 'session_control'
      and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
      and kind -> 'detail' -> 'record' ->> 'verdict' = 'accepted'
      and kind -> 'detail' -> 'record' ->> 'id' = '$second_task'")
checkpoints=$(sql "select kind -> 'detail' -> 'artifact' ->> 'commit' from spawn_events
    where agent_id = 'agent_resident_scribe' and event_name = 'artifact.committed'
      and id < $gated_at order by id")
[ -n "$checkpoints" ] || harness "the resident recorded no checkpoint"
for commit in $checkpoints; do
    git --git-dir="$source" merge-base --is-ancestor "$commit" "$main" 2>/dev/null ||
        fail "(5) the resident's checkpoint $commit is not reachable from main"
done

echo "014: two landings, two commits on main's first-parent line that say what landed; the checkpoints stay reachable"
exit 0
