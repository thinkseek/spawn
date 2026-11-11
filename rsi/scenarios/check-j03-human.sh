#!/bin/sh
# Journey j03, the human: the owner's control paths (design-gate-v2 C1).
# A person's text to a busy session waits for its turn, is typed once, in
# order, and survives a loop restart; a stop lets it fail with the reason;
# the dispatcher's own deliveries keep their contract. A person or the
# supervisor sends a named key (Esc, an arrow, Enter, Y, N) as a recorded
# command of its own, through the same submit path and the page's API: it
# does not wait, overtakes a waiting input, and is refused when nothing runs.
#
# Folds Spawn scenarios 015 (whole: checks (1)-(4)) and 038 (whole: checks
# (1)-(5)). Every assertion of both is carried; each step line names it as
# `[015 (n)]` or `[038 (n)]`. Traceability:
# /var/tmp/spawn/sync/gate-journeys-spawn-proof/trace/j03.tsv
#
# One database, one tmux socket, one `spawn loop` (the Bridge, the Observer
# with no model judge, the dispatcher, no supervisor) with two residents:
#   - `echo` (015's): the line-mode fake engine of 015, unchanged. A text
#     naming `BB:BUSY <seconds> <tag>` is a long turn (spinner on screen, a
#     tool call every 2 s), `BB:NOTE <tag>` a 4 s turn; every text read is
#     logged, and any text read while a turn runs is flagged.
#   - `keys` (038's): the raw-mode fake engine of 038, unchanged. It logs
#     each key by name with the pane's state; Esc during a turn interrupts it.
# `$WORK/claude` picks the engine by SPAWN_SESSION_ID (the Bridge sets it).
# 015 runs in the main shell, with its 30 s and 120 s turns as they were.
# 038 runs whole in a background subshell, started right after 015's loop
# restart (so no key is in flight across the restart), inside 015's waits
# for (2)-(4); it has its own work directory, engine log and session, and
# its idempotency keys that collided with 015's (bb-start, bb-stop) are
# renamed. `spawn serve` (038 (3)) answers the page's API on a loopback port.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
# Named check-*: the gate runs a failing check again, and on the base if it fails twice (design-gate-v2.md C2)
# Needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own
# database, uses a private tmux socket and a loopback port, no network.
# Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "j03: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep curl awk; do
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
SPAWN_DB_NAME="bb_j03_human_$run"
SPAWN_TMUX_SOCKET="bb-j03-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL
# The 038 part's own directory: its engine log, captures and markers.
KWORK=$WORK/k
mkdir -p "$KWORK"

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
k_pid=""
stop_loop() {
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
        loop_pid=""
    fi
}
cleanup() {
    if [ -n "$k_pid" ]; then
        kill "$k_pid" 2>/dev/null || true
        wait "$k_pid" 2>/dev/null || true
        k_pid=""
    fi
    if [ -s "$KWORK/serve.pid" ]; then
        serve_pid=$(cat "$KWORK/serve.pid")
        kill "$serve_pid" 2>/dev/null || true
    fi
    stop_loop
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
            echo "j03: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engine.log" ]; then
            echo "j03: what the echo engine (015) read:" >&2
            cat "$WORK/engine.log" >&2
        fi
        if [ -s "$KWORK/engine.log" ]; then
            echo "j03: what the keys engine (038) read:" >&2
            cat "$KWORK/engine.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "j03: the residents' commands (id, session, principal, key, state, succeeded, error):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, session_id, principal, idempotency_key, state, succeeded,
                    left(coalesce(error, ''), 160)
                from spawn_commands where session_id in ('session_resident_echo', 'session_resident_keys')
               order by id" >&2 2>/dev/null || true
            echo "j03: the keys resident's last events (id, name, step or state, command):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, event_name,
                    coalesce(kind -> 'detail' ->> 'step', kind -> 'detail' ->> 'state', ''),
                    coalesce(kind -> 'detail' ->> 'command_id', ''),
                    left(coalesce(kind -> 'detail' ->> 'evidence', ''), 60)
                from spawn_events where session_id = 'session_resident_keys'
               order by id desc limit 25" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "j03: harness: unexpected exit $rc" >&2
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
    echo "blackbox: [015] $*" >&2
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

# 015's fake engine (the `echo` resident's), unchanged. Every line it reads
# is logged `got <time> <text>`; a line read while one of its turns runs is
# logged `during <time> <text>` (typed into a busy pane) and otherwise
# ignored. A turn opens with the first line typed at its prompt and starts
# after a second of quiet; it is `BB:BUSY <seconds> <tag>` (a long turn,
# `busy-start`/`busy-end <time> <tag>` in the log), `BB:NOTE <tag>` (4 s,
# `note-end <time> <tag>` at its end), or anything else (4 s, like a note).
cat >"$WORK/claude-echo" <<'ENGINE'
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
log() { printf '%s %s %s\n' "$1" "$(date +%s.%N)" "$2" >>"$BB_WORK/engine.log"; }
# readline <seconds>: a whole typed line in $typed (0), or none yet (1).
# bash's `read -t` keeps what it read of a line when it times out (a paste
# that straddles the timeout): that part is held in $partial and joined to
# the rest of the line on the next call, never dropped.
partial=
readline() {
  chunk=
  if IFS= read -r -t "$1" chunk; then
    typed=$partial$chunk
    partial=
    return 0
  fi
  partial=$partial$chunk
  return 1
}
tool() {
  line "\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"$1\",\"name\":\"Bash\",\"input\":{}}]}"
  line "\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$1\",\"content\":\"ok\"}]}"
}
# run_turn <seconds> <id>: Busy on the screen for that long, a tool call
# every 2 s; anything typed meanwhile is logged as typed into a busy pane.
calls=0
run_turn() {
  end=$(($(date +%s) + $1))
  next=0
  printf '\n✻ Churning… (esc to interrupt)\n❯ '
  while [ "$(date +%s)" -lt "$end" ]; do
    if [ "$(date +%s)" -ge "$next" ]; then
      calls=$((calls + 1))
      tool "toolu_${SPAWN_EXECUTION_ID}_$2_$calls"
      printf '\n✻ Churning… (esc to interrupt)\n❯ '
      next=$(($(date +%s) + 2))
    fi
    if readline 0.5; then
      log during "$typed"
    fi
  done
  line '"type":"system","subtype":"turn_duration","durationMs":1'
  printf '\n✻ Churned for %ss · done\n❯ ' "$1"
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
job=
turns=0
while :; do
  if readline 1; then
    log got "$typed"
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:BUSY "*) [ -n "$job" ] || job="busy ${typed#*BB:BUSY }" ;;
    *"BB:NOTE "*) [ -n "$job" ] || job="note ${typed#*BB:NOTE }" ;;
    esac
  elif [ "$pending" = 1 ] && [ -z "$partial" ]; then
    # Quiet, and no line half read: the turn starts.
    turns=$((turns + 1))
    set -- ${job:-note other}
    case "$1" in
    busy)
      log busy-start "$3"
      run_turn "$2" "t$turns"
      log busy-end "$3"
      ;;
    note)
      run_turn 4 "t$turns"
      log note-end "$2"
      ;;
    esac
    job=
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude-echo"

# 038's fake engine (the `keys` resident's), unchanged. Its terminal is raw,
# so a key arrives as its bytes: a lone ESC is Esc, ESC [ A..D an arrow, CR
# alone Enter, a lone y or n the key. Text arrives as a burst of characters
# and a CR: logged `got <time> <text>`, it opens a turn. A key is logged
# `key <time> <name> <idle|busy>`. Esc while a turn runs ends the turn at
# once (`interrupted <time> <tag>`); a turn that runs its length logs
# `busy-end` or `note-end <time> <tag>`. Text read while a turn runs is
# logged `during <time> <text>` and ignored.
cat >"$WORK/claude-keys" <<'ENGINE'
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
log() { printf '%s %s %s\n' "$1" "$(date +%s.%N)" "$2" >>"$BB_WORK/engine.log"; }
stty raw -echo 2>/dev/null
calls=0
turn_end=
tag=
kind=
next_tool=0
# The spinner is one status line, redrawn in place as Claude Code's is:
# a redraw clears the prompt line and the spinner line above it first.
drawn=
unspin() {
  if [ -n "$drawn" ]; then printf '\r\033[2K\033[1A\r\033[2K'; else printf '\r\n'; fi
  drawn=
}
spinner() { unspin; printf '✻ Churning… (esc to interrupt)\r\n❯ '; drawn=1; }
end_turn() {
  case "$1" in
  interrupted)
    # What Claude Code writes when Esc interrupts a turn.
    line '"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}'
    # Claude Code redraws: the spinner goes, the interruption shows above
    # the prompt.
    unspin
    printf '  ⎿  Interrupted · What should Claude do instead?\r\n\r\n❯ '
    ;;
  *)
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    unspin
    printf '✻ Churned for 4s · done\r\n❯ '
    ;;
  esac
  turn_end=
}
state() { if [ -n "$turn_end" ]; then echo busy; else echo idle; fi; }
key() {
  log key "$1 $(state)"
  if [ "$1" = escape ] && [ -n "$turn_end" ]; then
    log interrupted "$tag"
    end_turn interrupted
  fi
}
text() {
  if [ -n "$turn_end" ]; then
    log during "$1"
    return
  fi
  log got "$1"
  printf 'you: %s\r\n' "$1"
  line '"type":"user","message":{"role":"user","content":"prompt"}'
  case "$1" in
  *"BB:BUSY "*)
    set -- ${1#*BB:BUSY }
    turn_end=$(($(date +%s) + $1)) tag=$2 kind=busy
    log busy-start "$tag"
    ;;
  *) turn_end=$(($(date +%s) + 4)) tag=$1 kind=note ;;
  esac
  next_tool=0
  spinner
}
# A lone y or n waits for what follows: a CR makes it text, anything else
# (or quiet) makes it the key.
flush() {
  case "$buf" in y | n) key "$buf"; buf= ;; esac
}
printf 'Welcome to Claude Code\r\n\r\n❯ '
buf=
quiet=0
while :; do
  now=$(date +%s)
  if [ -n "$turn_end" ] && [ "$now" -ge "$turn_end" ]; then
    log "$kind-end" "$tag"
    end_turn done
  fi
  if [ -n "$turn_end" ] && [ "$now" -ge "$next_tool" ]; then
    calls=$((calls + 1))
    line "\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_${SPAWN_EXECUTION_ID}_$calls\",\"name\":\"Bash\",\"input\":{}}]}"
    line "\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_${SPAWN_EXECUTION_ID}_$calls\",\"content\":\"ok\"}]}"
    spinner
    next_tool=$((now + 2))
  fi
  c=
  # bash's `read -t` keeps a byte it read as the timeout fired: such a byte
  # is input like any other (an Esc dropped here never reached the engine).
  if IFS= read -r -s -n 1 -d '' -t 0.3 c || [ -n "$c" ]; then
    quiet=0
    case "$c" in
    $'\e')
      rest=
      IFS= read -r -s -n 2 -d '' -t 0.05 rest || true
      flush
      case "$rest" in
      '') key escape ;;
      '[A') key up ;;
      '[B') key down ;;
      '[C') key right ;;
      '[D') key left ;;
      *) key "other-$(printf '%s' "$rest" | od -An -tx1 | tr -d ' ')" ;;
      esac
      ;;
    $'\r' | $'\n')
      if [ -z "$buf" ]; then key enter; else text "$buf"; buf=; fi
      ;;
    *)
      flush
      buf=$buf$c
      ;;
    esac
  else
    quiet=$((quiet + 1))
    [ "$quiet" -lt 3 ] || flush
  fi
done
ENGINE
chmod 755 "$WORK/claude-keys"

# The engine the loop launches: 038's for the `keys` resident (logging into
# its own directory), 015's for every other session.
cat >"$WORK/claude" <<EOF
#!/bin/sh
case "\${SPAWN_SESSION_ID-}" in
session_resident_keys)
    BB_WORK='$KWORK'
    export BB_WORK
    exec '$WORK/claude-keys' "\$@"
    ;;
*) exec '$WORK/claude-echo' "\$@" ;;
esac
EOF
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
# Free disk is not what this journey is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$WORK/engine.log"
: >"$KWORK/engine.log"

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

# --- helpers (015's) ------------------------------------------------------------

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

SESSION=session_resident_echo
AGENT=agent_resident_echo
# send <principal> <key> <action json>: the command's id.
send() {
    capture "$SPAWN_BIN" submit --principal "$1" --agent "$AGENT" --session "$SESSION" \
        --key "$2" --action "$3"
    [ "$status" -eq 0 ] || harness "submit $2 failed: $(err)"
    id=$(sed -n 's/^command=//p' "$WORK/out.txt")
    [ -n "$id" ] || harness "submit $2 printed no command id: $(out)"
    echo "$id"
}
say() { send "$1" "$2" "{\"type\":\"input\",\"text\":\"$3\"}"; }

state_of() { q "select state || ':' || coalesce(succeeded::text, '-') from spawn_commands where id = $1"; }
error_of() { q "select coalesce(error, '') from spawn_commands where id = $1"; }
finished() { [ "$(q "select state from spawn_commands where id = $1")" = finished ]; }
succeeded() { [ "$(state_of "$1")" = finished:true ]; }
# The Bridge's `command.step` records of command `$1` with step `$2`.
steps() {
    q "select count(*) from spawn_events
        where event_name = 'command.step' and source = 'bridge'
          and (kind -> 'detail' ->> 'command_id')::bigint = $1
          and kind -> 'detail' ->> 'step' = '$2'"
}
is_waiting() { [ "$(steps "$1" waiting)" -ge 1 ]; }
# Lines of the engine's log of kind `$1` whose text holds `$2`.
read_count() { grep -c "^$1 [0-9.]* .*$2" "$WORK/engine.log" || true; }
# The time of the first such line, and its line number.
read_at() { grep "^$1 [0-9.]* .*$2" "$WORK/engine.log" | head -1 | cut -d' ' -f2; }
line_of() { grep -n "^$1 [0-9.]* .*$2" "$WORK/engine.log" | head -1 | cut -d: -f1; }
logged() { [ "$(read_count "$1" "$2")" -ge 1 ]; }
before() { awk -v a="$1" -v b="$2" 'BEGIN { exit !(a + 0 <= b + 0) }'; }
# The pane showed Busy (the Bridge's own reading) after event `$1`.
busy_seen() {
    [ "$(q "select count(*) from spawn_events
            where session_id = '$SESSION' and event_name = 'screen.observed'
              and kind -> 'detail' ->> 'state' = 'busy' and id > $1")" -ge 1 ]
}
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }
judged_waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$SESSION'")" = WAITING ]
}
no_typing_into_busy() {
    [ "$(read_count during '')" -eq 0 ] ||
        fail "$1: text was typed into a busy pane: $(grep '^during ' "$WORK/engine.log" | head -3 | tr '\n' ' ')"
}
# open_turn <key> <seconds> <tag>: a long turn, running and seen Busy.
open_turn() {
    mark=$(last_event)
    turn=$(say human "$1" "BB:BUSY $2 $3")
    wait_until 60 succeeded "$turn" ||
        harness "the input that opens turn $3 did not succeed at a ready pane ($(state_of "$turn") $(error_of "$turn"))"
    wait_until 30 logged busy-start "$3\$" || harness "turn $3 never started"
    wait_until 30 busy_seen "$mark" || harness "the Bridge never read the pane as Busy during turn $3"
}

# --- the 038 part: named keys, run whole in a subshell --------------------------
#
# Called as `( keys_part ) &`: everything it sets stays in the subshell. It
# has its own WORK (captures and the keys engine's log), session and agent,
# and the helpers that 038 wrote differently from 015's. An assertion leaves
# `asserted` in its WORK, a harness failure `harness`, so the main shell can
# tell them apart (and stop early).
keys_part() {
    WORK=$KWORK
    SESSION=session_resident_keys
    AGENT=agent_resident_keys
    harness() {
        : >"$WORK/harness"
        echo "j03: [038] harness: $*" >&2
        exit 2
    }
    fail() {
        if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
            harness "the database is unreachable (while checking: $*)"
        fi
        : >"$WORK/asserted"
        echo "blackbox: [038] $*" >&2
        exit 1
    }
    # submit <principal> <key> <action json>: status and output in $WORK.
    submit() {
        capture "$SPAWN_BIN" submit --principal "$1" --agent "$AGENT" --session "$SESSION" \
            --key "$2" --action "$3"
    }
    # send <principal> <key> <action json>: the command's id (harness on refusal).
    send() {
        submit "$@"
        [ "$status" -eq 0 ] || harness "submit $2 failed: $(err)"
        id=$(sed -n 's/^command=//p' "$WORK/out.txt")
        [ -n "$id" ] || harness "submit $2 printed no command id: $(out)"
        echo "$id"
    }
    say() { send "$1" "$2" "{\"type\":\"input\",\"text\":\"$3\"}"; }
    # press <principal> <key> <name>: sets $pressed to the key command's id. A
    # refused submit is the behaviour missing (an assertion, not the harness),
    # so it runs in this shell, not in a command substitution.
    press() {
        submit "$1" "$2" "{\"type\":\"key\",\"key\":\"$3\"}"
        [ "$status" -eq 0 ] || fail "spawn submit refused the key $3: $(err)"
        pressed=$(sed -n 's/^command=//p' "$WORK/out.txt")
        [ -n "$pressed" ] || harness "submit $2 printed no command id: $(out)"
    }
    finished_at() { q "select finished_at_ms from spawn_commands where id = $1"; }
    read_count() { grep -c "^$1 [0-9.]* $2" "$WORK/engine.log" || true; }
    line_of() { grep -n "^$1 [0-9.]* $2" "$WORK/engine.log" | head -1 | cut -d: -f1; }
    logged() { [ "$(read_count "$1" "$2")" -ge 1 ]; }
    # A key command's records: finished succeeded, one pressing and one pressed
    # step, no step of text.
    key_recorded() { # <id> <label>
        succeeded "$1" || fail "$2: key command $1 finished $(state_of "$1"): $(error_of "$1")"
        assert_eq "$(q "select action ->> 'type' from spawn_commands where id = $1")" key "$2: the action type of command $1"
        assert_eq "$(steps "$1" pressing)" 1 "$2: 'pressing' steps of key command $1"
        assert_eq "$(steps "$1" pressed)" 1 "$2: 'pressed' steps of key command $1"
        for step in typing typed waiting; do
            assert_eq "$(steps "$1" "$step")" 0 "$2: '$step' steps of key command $1"
        done
    }
    no_text_during() {
        [ "$(read_count during '')" -eq 0 ] ||
            fail "$1: text was typed into a busy pane: $(grep '^during ' "$WORK/engine.log" | head -3 | tr '\n' ' ')"
    }

    wait_until 120 judged_waiting || harness "the resident was never judged waiting at its prompt"

    # --- 038 (1) every named key, once each, in order ---

    echo "j03: [038 (1)] every named key arrives, once each, in order"
    ids=""
    for name in escape up down left right enter y n; do
        principal=human
        [ "$name" = down ] && principal=spawn_supervisor
        press "$principal" "bb-key-$name" "$name"
        ids="$ids $pressed"
    done
    for id in $ids; do
        wait_until 60 finished "$id" || fail "(1) key command $id did not finish within 60 s ($(state_of "$id"))"
    done
    for id in $ids; do key_recorded "$id" "(1)"; done
    wait_until 10 logged key "n " || fail "(1) the engine never read the n key: $(tr '\n' ' ' <"$WORK/engine.log")"
    sleep 2
    got=$(grep '^key ' "$WORK/engine.log" | cut -d' ' -f3 | tr '\n' ' ')
    assert_eq "$got" "escape up down left right enter y n " "(1) the keys the engine read, in order"
    assert_eq "$(read_count got '')" 0 "(1) lines of text the engine read"
    assert_eq "$(q "select principal from spawn_commands where idempotency_key = 'bb-key-down'")" spawn_supervisor \
        "(1) the principal recorded for the supervisor's key"
    assert_eq "$(q "select count(*) from spawn_commands where action ->> 'type' = 'key' and principal = 'human'")" 7 \
        "(1) key commands recorded as the person's"

    # --- 038 (2) a key does not wait, and overtakes a waiting input ---

    echo "j03: [038 (2)] a key does not wait, and overtakes a person's input that does"
    mark=$(last_event)
    turn=$(say human bb-busy "BB:BUSY 90 T2")
    wait_until 60 succeeded "$turn" || harness "the input that opens the long turn did not succeed ($(state_of "$turn") $(error_of "$turn"))"
    wait_until 30 logged busy-start T2 || harness "the long turn never started"
    wait_until 30 busy_seen "$mark" || harness "the Bridge never read the pane as Busy during the long turn"
    after=$(say human bb-after "BB:NOTE AFTER")
    wait_until 30 is_waiting "$after" || harness "the input sent during the turn was never recorded waiting ($(state_of "$after") $(error_of "$after"))"
    finished "$after" && harness "the input sent during the turn finished before the key: $(state_of "$after")"
    press human bb-esc escape
    esc=$pressed
    wait_until 30 finished "$esc" ||
        fail "(2) an Esc sent while a person's input waits did not finish within 30 s ($(state_of "$esc")): a key must not wait for the turn"
    key_recorded "$esc" "(2)"
    logged key "escape busy" ||
        fail "(2) the Esc did not reach the pane while its turn ran: $(grep '^key ' "$WORK/engine.log" | tail -2 | tr '\n' ' ')"
    logged interrupted T2 || fail "(2) the long turn was not interrupted by the Esc"
    # The interruption is the turn's end on the record (Claude Code's
    # "[Request interrupted by user]" line), so the session reads waiting.
    pressed_at=$(q "select max(id) from spawn_events where event_name = 'command.step'
                     and (kind -> 'detail' ->> 'command_id')::bigint = $esc
                     and kind -> 'detail' ->> 'step' = 'pressed'")
    turn_stopped_after() {
        [ "$(q "select count(*) from spawn_events where session_id = '$SESSION'
                 and event_name = 'turn.stopped' and id > $pressed_at")" -ge 1 ]
    }
    wait_until 30 turn_stopped_after || fail "(2) no turn.stopped was recorded after the Esc interrupted the turn"
    wait_until 60 finished "$after" || fail "(2) the waiting input did not finish within 60 s of the Esc ($(state_of "$after"))"
    succeeded "$after" || fail "(2) the waiting input finished $(state_of "$after"): $(error_of "$after")"
    [ "$(finished_at "$esc")" -le "$(finished_at "$after")" ] ||
        fail "(2) the Esc, sent after the waiting input, finished after it"
    [ "$after" -lt "$esc" ] || harness "the input's command id is not before the Esc's"
    wait_until 20 logged got "BB:NOTE AFTER" || fail "(2) the engine never read the waiting input"
    assert_eq "$(read_count got 'BB:NOTE AFTER')" 1 "(2) times the engine read the waiting input"
    [ "$(line_of key 'escape busy')" -lt "$(line_of got 'BB:NOTE AFTER')" ] ||
        fail "(2) the waiting input reached the engine before the Esc that overtook it"
    no_text_during "(2)"
    wait_until 30 logged note-end "BB:NOTE AFTER" || harness "the input's turn never ended"

    # --- 038 (3) the page's path ---

    echo "j03: [038 (3)] the page's path: POST /api/session/<id>/key"
    web=$WORK/web
    mkdir -p "$web"
    printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" >"$WORK/serve.log" 2>&1 &
    echo $! >"$WORK/serve.pid"
    port=""
    i=0
    while [ "$i" -lt 100 ]; do
        i=$((i + 1))
        port=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$WORK/serve.log" 2>/dev/null | head -n 1)
        [ -n "$port" ] && break
        sleep 0.1
    done
    [ -n "$port" ] || harness "serve did not start: $(head -c 400 "$WORK/serve.log")"
    # post_key <session> <body>: the status code; the answer in $WORK/key.json.
    post_key() {
        curl -s -o "$WORK/key.json" -w '%{http_code}' -X POST \
            -H 'Content-Type: application/json' -H "Origin: http://127.0.0.1:$port" \
            --data "$2" "http://127.0.0.1:$port/api/session/$1/key" 2>/dev/null || echo 000
    }
    before_keys=$(read_count key '')
    code=$(post_key "$SESSION" '{"name":"up","key":"bb-page-up"}')
    [ "$code" = 202 ] || fail "(3) POST /api/session/$SESSION/key answered $code: $(head -c 300 "$WORK/key.json")"
    page=$(jq -r '.command_id' "$WORK/key.json")
    [ -n "$page" ] && [ "$page" != null ] || fail "(3) the page's key answered no command id: $(cat "$WORK/key.json")"
    wait_until 30 finished "$page" || fail "(3) the page's key did not finish within 30 s ($(state_of "$page"))"
    key_recorded "$page" "(3)"
    assert_eq "$(q "select principal from spawn_commands where id = $page")" human "(3) the page's key's principal"
    code=$(post_key "$SESSION" '{"name":"up","key":"bb-page-up"}')
    [ "$code" = 202 ] || fail "(3) the same key again answered $code: $(head -c 300 "$WORK/key.json")"
    assert_eq "$(jq -r '.command_id' "$WORK/key.json")" "$page" "(3) the command the same key again returns"
    sleep 2
    assert_eq "$(read_count key '')" "$((before_keys + 1))" "(3) keys the engine read from the page"
    code=$(post_key "$SESSION" '{"name":"f1","key":"bb-page-f1"}')
    [ "$code" = 400 ] || fail "(3) a key outside the set answered $code, not 400: $(head -c 300 "$WORK/key.json")"
    code=$(post_key session_nobody '{"name":"up","key":"bb-page-nobody"}')
    [ "$code" = 404 ] || fail "(3) a key to a session no agent has used answered $code, not 404: $(head -c 300 "$WORK/key.json")"

    # --- 038 (4) refused when nothing runs ---

    echo "j03: [038 (4)] refused when nothing runs"
    stop=$(send human bb-k-stop '{"type":"stop"}')
    wait_until 120 succeeded "$stop" || harness "the stop did not succeed ($(state_of "$stop") $(error_of "$stop"))"
    live() { [ "$(q "select coalesce(live::text, 'false') from agent_status where session_id = '$SESSION'")" = true ]; }
    live && harness "the session still runs an execution after its stop"
    # The loop is shared with 015's resident, whose commands land meanwhile:
    # only this session's commands are counted.
    commands_before=$(q "select count(*) from spawn_commands where session_id = '$SESSION'")
    code=$(post_key "$SESSION" '{"name":"escape","key":"bb-page-stopped"}')
    [ "$code" = 409 ] || fail "(4) a key to a stopped session answered $code, not 409: $(head -c 300 "$WORK/key.json")"
    jq -r '.error' "$WORK/key.json" | grep -q 'runs no execution' ||
        fail "(4) the refusal does not say the session runs no execution: $(cat "$WORK/key.json")"
    assert_eq "$(q "select count(*) from spawn_commands where session_id = '$SESSION'")" "$commands_before" "(4) commands after the refused key"
    press spawn_supervisor bb-key-stopped escape
    late=$pressed
    wait_until 30 finished "$late" || fail "(4) the supervisor's key to the stopped session did not finish ($(state_of "$late"))"
    assert_eq "$(state_of "$late")" finished:false "(4) the supervisor's key to the stopped session"
    error_of "$late" | grep -q 'runs no execution' ||
        fail "(4) its error does not say the session runs no execution: $(error_of "$late")"
    assert_eq "$(steps "$late" pressing)" 0 "(4) 'pressing' steps of the key to the stopped session"

    # --- 038 (5) the set is closed ---

    echo "j03: [038 (5)] the set is closed: a key outside it is not a command action"
    commands_before=$(q "select count(*) from spawn_commands where session_id = '$SESSION'")
    submit human bb-key-f1 '{"type":"key","key":"f1"}'
    [ "$status" -ne 0 ] || fail "(5) spawn submit took a key outside the set: $(out)"
    assert_eq "$(q "select count(*) from spawn_commands where session_id = '$SESSION'")" "$commands_before" "(5) commands after the refused key"

    echo "j03: [038] every named key reached the pane once, in order, at once (overtaking a waiting input), recorded with its sender; the page's path submits it as the person; a session that runs nothing refuses it with the reason"
}

# keys_poll: the 038 part's outcome so far; exits as it did once it failed.
keys_poll() {
    if [ -f "$KWORK/asserted" ]; then
        asserted=1
        echo "j03: the 038 part failed an assertion" >&2
        exit 1
    fi
    [ ! -f "$KWORK/harness" ] || harness "the 038 part failed (see above)"
}

# --- the residents, live and idle ----------------------------------------------

capture "$SPAWN_BIN" mission add --text "015 inputs wait"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" mission add --text "038 named keys"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
capture "$SPAWN_BIN" resident propose --name echo --charter "Owns the 015 inputs."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name echo
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
capture "$SPAWN_BIN" resident propose --name keys --charter "Owns the 038 keys."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name keys
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
send bb-harness bb-start '{"type":"start","engine":"claude","prompt":""}' >/dev/null
capture "$SPAWN_BIN" submit --principal bb-harness --agent agent_resident_keys \
    --session session_resident_keys --key bb-k-start --action '{"type":"start","engine":"claude","prompt":""}'
[ "$status" -eq 0 ] || harness "submit bb-k-start failed: $(err)"
start_loop
wait_until 120 judged_waiting || harness "the resident was never judged waiting at its prompt"

# --- 015 (1) two inputs sent while a long turn runs wait --------------------------

echo "j03: [015 (1)] waiting, not refused: two inputs sent while a 30 s turn runs"
open_turn bb-busy-first 30 first
one=$(say human bb-note-one "BB:NOTE one")
sleep 1
two=$(say spawn_supervisor bb-note-two "BB:NOTE two")
sleep 12
logged busy-end "first\$" && harness "the first turn ended before the inputs could be checked"
for id in "$one" "$two"; do
    if finished "$id"; then
        fail "(1) input $id, sent while the session's turn ran, finished $(state_of "$id") ($(error_of "$id")) instead of waiting for the turn to end"
    fi
    is_waiting "$id" ||
        fail "(1) input $id waits unrecorded: no command.step 'waiting' from the Bridge"
    assert_eq "$(steps "$id" waiting)" 1 "(1) the 'waiting' records of input $id"
done
assert_eq "$(read_count got 'BB:NOTE')" 0 "(1) notes the engine read while its turn ran"
no_typing_into_busy "(1)"

# --- 015 (2) typed when the turn ends, in order, once; across a restart ------------

echo "j03: [015 (2)] typed when the turn ends, in order, at most once, across a loop restart"
# The waiting is a record, not the Bridge's memory: a restarted loop keeps it.
stop_loop
start_loop
# The 038 part runs from here, beside 015's (2)-(4), on the same loop.
( keys_part ) &
k_pid=$!
wait_until 60 logged busy-end "first\$" || harness "the first turn never ended"
both_finished() { finished "$one" && finished "$two"; }
wait_until 180 both_finished ||
    fail "(2) the waiting inputs did not finish within 180 s of the turn's end (one: $(state_of "$one"), two: $(state_of "$two"))"
for id in "$one" "$two"; do
    succeeded "$id" || fail "(2) waiting input $id finished $(state_of "$id"): $(error_of "$id")"
    assert_eq "$(steps "$id" typed)" 1 "(2) the 'typed' records of input $id"
done
wait_until 30 logged note-end "two\$" || harness "the second note's turn never ended"
assert_eq "$(read_count got 'BB:NOTE one')" 1 "(2) times the engine read the first input"
assert_eq "$(read_count got 'BB:NOTE two')" 1 "(2) times the engine read the second input"
[ "$(line_of got 'BB:NOTE one')" -lt "$(line_of got 'BB:NOTE two')" ] ||
    fail "(2) the second input reached the engine before the first"
before "$(read_at busy-end 'first$')" "$(read_at got 'BB:NOTE one')" ||
    fail "(2) the first input reached the engine before the long turn ended"
before "$(read_at note-end 'one$')" "$(read_at got 'BB:NOTE two')" ||
    fail "(2) the second input reached the engine before the first one's turn ended"
no_typing_into_busy "(2)"
keys_poll

# --- 015 (3) a stop first: the waiting input fails with the reason -----------------

echo "j03: [015 (3)] stopped first: an input waiting behind a 120 s turn, then a stop"
wait_until 60 judged_waiting || harness "the resident was not judged waiting after the notes"
open_turn bb-busy-third 120 third
three=$(say human bb-note-three "BB:NOTE three")
echo "j03: [015 (1)] an input sent during turn third is recorded waiting"
wait_until 15 is_waiting "$three" ||
    fail "(1) input $three, sent during turn third, was not recorded waiting ($(state_of "$three") $(error_of "$three"))"
stop=$(send human bb-stop '{"type":"stop"}')
wait_until 60 finished "$stop" ||
    fail "(3) the stop was not handled within 60 s while an input waited ($(state_of "$stop"))"
succeeded "$stop" || fail "(3) the stop finished $(state_of "$stop"): $(error_of "$stop")"
logged busy-end "third\$" && harness "turn third ended before the stop: the stop proves nothing"
wait_until 30 finished "$three" ||
    fail "(3) input $three still waits ($(state_of "$three")) after its session was stopped"
succeeded "$three" && fail "(3) input $three succeeded though its session was stopped before its turn"
error=$(error_of "$three")
printf '%s' "$error" | grep -qi stopped ||
    fail "(3) input $three failed without saying the session was stopped: [$error]"
assert_eq "$(steps "$three" typing)" 0 "(3) the 'typing' records of input $three"
assert_eq "$(read_count got 'BB:NOTE three')" 0 "(3) times the engine read input three"
no_typing_into_busy "(3)"
keys_poll

# The session again, for the dispatcher's part (resumed by the harness, not
# as `human`).
send bb-harness bb-resume '{"type":"resume"}' >/dev/null
wait_until 120 judged_waiting || harness "the resumed resident was never judged waiting at its prompt"

# --- 015 (4) the dispatcher's deliveries still land --------------------------------

echo "j03: [015 (4)] the dispatcher's contract holds"
open_turn bb-busy-second 12 second
four=$(say human bb-note-four "BB:NOTE four")
echo "j03: [015 (1)] an input sent during turn second is recorded waiting"
wait_until 15 is_waiting "$four" || fail "(1) input $four, sent during turn second, was not recorded waiting ($(state_of "$four") $(error_of "$four"))"
capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker echo \
    --brief "BB:NOTE task"
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(jq -r .task "$WORK/out.txt")
[ -n "$task" ] && [ "$task" != null ] || harness "task assign printed no id"
# The task must be assigned while turn second still runs, behind input four.
logged busy-end "second\$" && harness "turn second ended before the task could be assigned behind it"
delivered() { [ "$(q "select state from mission_tasks where id = $task")" = delivered ]; }
wait_until 60 logged busy-end "second\$" || harness "turn second never ended"
wait_until 240 delivered ||
    fail "(4) task #$task was not delivered within 240 s of the turn's end (state $(q "select state from mission_tasks where id = $task"); input four $(state_of "$four"))"
succeeded "$four" || fail "(4) waiting input $four finished $(state_of "$four"): $(error_of "$four")"
wait_until 30 logged note-end "task\$" || harness "the task's turn never ended"
assert_eq "$(read_count got 'BB:NOTE four')" 1 "(4) times the engine read input four"
assert_eq "$(read_count got 'BB:NOTE task')" 1 "(4) times the engine read the task"
[ "$(line_of got 'BB:NOTE four')" -lt "$(line_of got 'BB:NOTE task')" ] ||
    fail "(4) the task, sent after input four, reached the engine before it"
DELIVERIES="from spawn_commands where session_id = '$SESSION' and principal = 'spawn-dispatch'
              and action ->> 'type' = 'input' and idempotency_key like 'deliver-$task-%'"
assert_eq "$(q "select count(*) $DELIVERIES and state = 'finished' and succeeded")" 1 \
    "(4) deliveries of task #$task that succeeded"
refused=$(q "select count(*) $DELIVERIES and state = 'finished' and not succeeded
              and coalesce(error, '') not like '%does not accept input%'")
[ "$refused" -eq 0 ] ||
    fail "(4) a delivery attempt of task #$task failed otherwise than as refused at once: $(q "select string_agg(idempotency_key || ': ' || error, '; ') $DELIVERIES and not succeeded")"
dispatcher_waiting=$(q "select count(*) from spawn_events e join spawn_commands c
                         on c.id = (e.kind -> 'detail' ->> 'command_id')::bigint
                        where e.event_name = 'command.step' and e.kind -> 'detail' ->> 'step' = 'waiting'
                          and c.principal = 'spawn-dispatch'")
assert_eq "$dispatcher_waiting" 0 "(4) inputs of the dispatcher recorded waiting"
no_typing_into_busy "(4)"
echo "j03: [015] inputs sent to a busy session waited, were typed once each in order after the turn (across a restart), did not disturb the dispatcher's delivery, and failed with the reason when the session was stopped first"

# --- the 038 part's outcome ----------------------------------------------------

k_rc=0
wait "$k_pid" || k_rc=$?
k_pid=""
keys_poll
[ "$k_rc" -eq 0 ] || harness "the 038 part exited $k_rc"

echo "j03: the owner's control paths hold: a person's input to a busy session waits, is typed once in order across a restart, fails with the reason on a stop, beside the dispatcher's deliveries; named keys reach the pane at once, once each, from a person, the supervisor and the page"
exit 0
