#!/bin/sh
# Black box: a cancelled task's live worker is told at once, its report for
# the task is never collected, and the work done for it does not ride into
# the next task's landing.
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, no model
# judge, no supervisor) runs a resident `core` of a gated mission (accept =
# gate) with a fake engine; `spawn integrate --once` passes, run by the
# scenario as soon as a task is reported, judge its work with a recording gate
# (SPAWN_GATE) that accepts every candidate. The fake engine behaves as Claude
# Code does where it matters here: it reads keys one at a time; Escape or
# Ctrl-C during a turn interrupts it (the prompt comes back and the
# transcript records "[Request interrupted by user for tool use]");
# text typed during a turn is held until the turn ends; a turn that names
# `BB:WORK <file> <seconds>` writes the file at once, then calls a tool every
# 0.4 s for that long, then writes the report. It never works on the same
# file twice, so a notice that quotes a brief does not start the work again.
# Its turns end so that the Observer's coverage gap (objective 011) does not
# arise: this scenario does not depend on 011.
#   (0) Task Z writes zero.txt and lands: the resident has landed work.
#   Task A starts a 60 s turn that writes one.txt; 3 s in, `spawn task cancel`.
#   (1) Promptly stopped: the engine makes no tool call for A more than 15 s
#       after the cancel, and A's turn never finishes (interrupted, or its
#       execution ended).
#   (2) Told: within 30 s of the cancel the worker is given a message (typed
#       in, or the prompt of a start or resume) that names task A and says it
#       is cancelled.
#   (3) Never collected: A stays cancelled, with no report, no task.reported
#       and no task.gated record.
#   Then task B (writes two.txt), assigned to the same resident.
#   (4) Not carried over: B's turn starts in a workspace without one.txt;
#       the candidate the gate judges for B holds two.txt and zero.txt and not
#       one.txt; B lands with no rejection; main then holds zero.txt and
#       two.txt and not one.txt; B's collected report is B's.
#   (5) Not destroyed: a commit reachable from a ref of the source repository
#       holds one.txt as A's turn wrote it (the work is set aside, not lost).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on a dispatcher that leaves a resident working on a
# cancelled task. Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs
# SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own database,
# uses a private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "012: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed stty; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_012_cancel_$run"
SPAWN_TMUX_SOCKET="bb-012-$run"
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
            echo "012: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "012: the integrator's last lines:" >&2
            tail -n 10 "$WORK/integrate.log" >&2
        fi
        if [ -s "$WORK/engine.log" ]; then
            echo "012: the engine's last lines:" >&2
            grep -v '^tool ' "$WORK/engine.log" | tail -n 15 >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "012: harness: unexpected exit $rc" >&2
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

gate=$WORK/gate
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
: >"$WORK/judged.log"

# The recording gate: accepts every candidate and notes the candidate commit.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
printf '%s %s %s\n' "\$(date +%s)" "\$2" "\$3" >>'$WORK/judged.log'
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
chmod 755 "$WORK/fake-gate.sh"

# The fake engine. Keys are read one at a time, as Claude Code reads them:
# Enter ends a line, Escape (alone) or Ctrl-C interrupts a turn, text typed
# during a turn is held until it ends. Lines typed at the prompt make one
# turn, which starts after a second of quiet. Every turn ends the same way:
# its last tool call is still open when the prompt comes back on the screen,
# and a moment later the transcript records the tool's result and the turn's
# end together. So the screen reading comes before the turn's end, and no
# fact follows it: whether a fact that leaves the judgement as it was is
# judged again is objective 011's concern, not this scenario's.
# engine.log lines:
#   heard <time> <the turn's text on one line>
#   busy <time> <file> / sees <time> <file> <the *.txt in the workspace>
#   wrote <time> <file> / tool <time> <file> <n>
#   finished <time> <file> / interrupted <time> <file> / ended <time> <pid>
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
: >"$WORK/engine.log"
: >"$WORK/started.list"

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

# wait_for <seconds> <command...>: true once the command is, false past the
# limit.
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

task_state() { q "select state from mission_tasks where id = $1"; }
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

capture "$SPAWN_BIN" mission add --text "012 cancelled work" --source "$source" \
    --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" resident propose --name core --charter "Owns the 012 concern."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name core
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"

assign() {
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker core \
        --brief "$1"
    [ "$status" -eq 0 ] || harness "task assign failed: $(err)"
    jq -r .task "$WORK/out.txt"
}

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

# --- (0) the resident's landed work ------------------------------------------------

zero=$(assign "Write the file.
BB:WORK zero.txt 2")
[ -n "$zero" ] && [ "$zero" != null ] || harness "task assign printed no id"
wait_for 90 engine_did wrote zero.txt ||
    harness "task $zero never reached the engine (state $(task_state "$zero"))"
wait_for 60 is_state "$zero" reported ||
    harness "task $zero was not reported within 60 s (state $(task_state "$zero"))"
land "$zero" "(0) the resident's first task"
main_has zero.txt || harness "task $zero landed without zero.txt on main"

# --- the cancel, in the middle of a turn -------------------------------------------

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

# --- (1) promptly stopped ------------------------------------------------------------

sleep 20
last=$(last_tool one.txt)
[ "$last" -le $((cancelled_at + 15)) ] ||
    fail "(1) the worker was still working on cancelled task $one $((last - cancelled_at)) s after the cancel (its last tool call)"
engine_did finished one.txt &&
    fail "(1) the worker finished the turn of cancelled task $one"

# --- (2) told ---------------------------------------------------------------------------

wait_for $((cancelled_at + 30 - $(now))) told "$cancelled_at" "$one" ||
    fail "(2) within 30 s of the cancel, the worker was told nothing that names task $one as cancelled"

# --- the next task ----------------------------------------------------------------------

two=$(assign "Write the file.
BB:WORK two.txt 2")
wait_for 90 engine_did wrote two.txt ||
    fail "(4) the next task $two never reached the worker (state $(task_state "$two"))"
wait_for 60 is_state "$two" reported ||
    fail "(4) task $two was not reported within 60 s (state $(task_state "$two"))"
land "$two" "(4) the next task"

# --- (3) never collected ---------------------------------------------------------------

assert_eq "$(task_state "$one")" cancelled "(3) the state of cancelled task $one"
assert_eq "$(sql "select coalesce(report, '') from mission_tasks where id = $one" | tr -d '\n')" "" \
    "(3) the report of cancelled task $one"
assert_eq "$(records "$one" task.reported)" 0 "(3) task.reported records of cancelled task $one"
assert_eq "$(records "$one" task.gated)" 0 "(3) task.gated records of cancelled task $one"
engine_did finished one.txt &&
    fail "(1) the worker finished the turn of cancelled task $one after all"

# --- (4) not carried over ------------------------------------------------------------------

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

# --- (5) not destroyed ------------------------------------------------------------------

kept=$(git --git-dir="$source" rev-list --all | while read -r commit; do
    if [ "$(git --git-dir="$source" show "$commit:one.txt" 2>/dev/null)" = one ]; then
        echo "$commit"
        break
    fi
done)
[ -n "$kept" ] ||
    fail "(5) no commit reachable from a ref of the source holds cancelled task $one's one.txt: its work was destroyed"

echo "012: the cancelled task's worker was stopped and told at once; its work stayed out of the next landing, and was kept"
exit 0
