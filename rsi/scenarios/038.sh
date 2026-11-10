#!/bin/sh
# Black box: a person (or the supervisor) sends an agent a named key — Esc,
# an arrow, Enter, Y or N — as a recorded command of its own. The key goes
# through the same submit path as text, as its own action type (`key`), and
# never through a text input. It reaches the agent's pane once, at once, in
# any pane state: it does not wait for the turn as a person's text does. It
# is recorded and shown with who sent it, and a key for a session that runs
# nothing is refused with the reason.
#
# A real `spawn loop` (the Bridge, the Observer with no model judge, the
# dispatcher, no supervisor) runs one resident, `echo`, on a fake engine
# whose terminal is in raw mode. The engine logs each key it reads, by name
# (`key <time> <name> <idle|busy>`), and each line of text (`got <time>
# <text>`). A text naming `BB:BUSY <seconds> <tag>` opens a long turn: the
# screen shows the spinner (`✻ Churning… (esc to interrupt)`, read as Busy)
# and the transcript a tool call every 2 s. Esc during a turn interrupts it,
# as Claude Code's does: the turn ends at once. Any other text is a 4 s turn.
# `spawn serve` answers the page's API on a loopback port.
# Checked:
#   (1) every named key arrives, once each, in order: escape, up, down, left,
#       right, enter, y and n, sent with `spawn submit` as `human` (down as
#       `spawn_supervisor`), each a command of type `key` that finishes
#       succeeded with one `pressing` and one `pressed` step from the Bridge
#       and no `typing`, `typed` or `waiting` step; the engine reads each key
#       exactly once, in the order sent, and no text; the commands are
#       recorded with their principals.
#   (2) a key does not wait, and overtakes a person's input that does: while
#       a long turn runs, a person's input waits (`waiting`); an Esc sent
#       after it reaches the busy pane at once and ends the turn (the
#       record has a `turn.stopped` after the Esc, from the engine's
#       "[Request interrupted by user]"); the Esc's command finishes before
#       the input's, and the input is typed after the Esc, once.
#   (3) the page's path: `POST /api/session/<id>/key` with a name and an
#       idempotency key answers 202 with the command, which reaches the
#       pane once; the same key again returns the same command; a name
#       outside the set answers 400, and a session no agent has used 404.
#   (4) refused when nothing runs: a key sent to a stopped session through
#       the page answers 409 with a reason that says it runs no execution,
#       and records no command; one sent with `spawn submit` (the
#       supervisor's path) is recorded and finishes failed, saying the same.
#   (5) the set is closed: `{"type":"key","key":"f1"}` is not a command
#       action (`spawn submit` refuses it, and records nothing).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (bf165ad7): `spawn submit` refuses
# `{"type":"key","key":"escape"}` as not a command action, and there is no
# `/api/session/<id>/key`. It passes once a `key` action exists (escape, up,
# down, left, right, enter, y, n), the Bridge sends it to the pane at once
# and at most once (its own `pressing`/`pressed` steps), and the page's path
# submits it as the person, refusing a session that runs nothing.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket and a loopback port, no network. Every wait is
# bounded in time.

set -eu

asserted=""
harness() {
    echo "038: harness: $*" >&2
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
SPAWN_DB_NAME="bb_038_keys_$run"
SPAWN_TMUX_SOCKET="bb-038-$run"
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
            echo "038: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engine.log" ]; then
            echo "038: what the engine read:" >&2
            cat "$WORK/engine.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "038: the resident's commands (id, principal, key, state, succeeded, error):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, principal, idempotency_key, state, succeeded,
                    left(coalesce(error, ''), 160)
                from spawn_commands where session_id = 'session_resident_echo' order by id" \
                >&2 2>/dev/null || true
            echo "038: the resident's last events (id, name, step or state, command):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, event_name,
                    coalesce(kind -> 'detail' ->> 'step', kind -> 'detail' ->> 'state', ''),
                    coalesce(kind -> 'detail' ->> 'command_id', ''),
                    left(coalesce(kind -> 'detail' ->> 'evidence', ''), 60)
                from spawn_events where session_id = 'session_resident_echo'
               order by id desc limit 25" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "038: harness: unexpected exit $rc" >&2
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

# The fake engine. Its terminal is raw, so a key arrives as its bytes: a
# lone ESC is Esc, ESC [ A..D an arrow, CR alone Enter, a lone y or n the
# key. Text arrives as a burst of characters and a CR (a paste, then
# Enter): logged `got <time> <text>`, it opens a turn. A key is logged
# `key <time> <name> <idle|busy>`. Esc while a turn runs ends the turn at
# once (`interrupted <time> <tag>`), as Claude Code's does; a turn that
# runs its length logs `busy-end` or `note-end <time> <tag>`. Text read
# while a turn runs is logged `during <time> <text>` and ignored.
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
  if IFS= read -r -s -n 1 -d '' -t 0.3 c; then
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
mkdir -p "$SPAWN_WORKSPACE_ROOT"
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

SESSION=session_resident_echo
AGENT=agent_resident_echo
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

state_of() { q "select state || ':' || coalesce(succeeded::text, '-') from spawn_commands where id = $1"; }
error_of() { q "select coalesce(error, '') from spawn_commands where id = $1"; }
finished() { [ "$(q "select state from spawn_commands where id = $1")" = finished ]; }
succeeded() { [ "$(state_of "$1")" = finished:true ]; }
finished_at() { q "select finished_at_ms from spawn_commands where id = $1"; }
# The Bridge's `command.step` records of command `$1` with step `$2`.
steps() {
    q "select count(*) from spawn_events
        where event_name = 'command.step' and source = 'bridge'
          and (kind -> 'detail' ->> 'command_id')::bigint = $1
          and kind -> 'detail' ->> 'step' = '$2'"
}
is_waiting() { [ "$(steps "$1" waiting)" -ge 1 ]; }
read_count() { grep -c "^$1 [0-9.]* $2" "$WORK/engine.log" || true; }
line_of() { grep -n "^$1 [0-9.]* $2" "$WORK/engine.log" | head -1 | cut -d: -f1; }
logged() { [ "$(read_count "$1" "$2")" -ge 1 ]; }
judged_waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$SESSION'")" = WAITING ]
}
busy_seen() {
    [ "$(q "select count(*) from spawn_events
            where session_id = '$SESSION' and event_name = 'screen.observed'
              and kind -> 'detail' ->> 'state' = 'busy' and id > $1")" -ge 1 ]
}
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }
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

# --- the resident, live and idle ------------------------------------------------------

capture "$SPAWN_BIN" mission add --text "038 named keys"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
capture "$SPAWN_BIN" resident propose --name echo --charter "Owns the 038 keys."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name echo
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
send bb-harness bb-start '{"type":"start","engine":"claude","prompt":""}' >/dev/null
start_loop
wait_until 120 judged_waiting || harness "the resident was never judged waiting at its prompt"

# --- (1) every named key, once each, in order ----------------------------------------

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

# --- (2) a key does not wait, and overtakes a waiting input ------------------------------

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

# --- (3) the page's path ------------------------------------------------------------------

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

# --- (4) refused when nothing runs ------------------------------------------------------

stop=$(send human bb-stop '{"type":"stop"}')
wait_until 120 succeeded "$stop" || harness "the stop did not succeed ($(state_of "$stop") $(error_of "$stop"))"
live() { [ "$(q "select coalesce(live::text, 'false') from agent_status where session_id = '$SESSION'")" = true ]; }
live && harness "the session still runs an execution after its stop"
commands_before=$(q "select count(*) from spawn_commands")
code=$(post_key "$SESSION" '{"name":"escape","key":"bb-page-stopped"}')
[ "$code" = 409 ] || fail "(4) a key to a stopped session answered $code, not 409: $(head -c 300 "$WORK/key.json")"
jq -r '.error' "$WORK/key.json" | grep -q 'runs no execution' ||
    fail "(4) the refusal does not say the session runs no execution: $(cat "$WORK/key.json")"
assert_eq "$(q "select count(*) from spawn_commands")" "$commands_before" "(4) commands after the refused key"
press spawn_supervisor bb-key-stopped escape
late=$pressed
wait_until 30 finished "$late" || fail "(4) the supervisor's key to the stopped session did not finish ($(state_of "$late"))"
assert_eq "$(state_of "$late")" finished:false "(4) the supervisor's key to the stopped session"
error_of "$late" | grep -q 'runs no execution' ||
    fail "(4) its error does not say the session runs no execution: $(error_of "$late")"
assert_eq "$(steps "$late" pressing)" 0 "(4) 'pressing' steps of the key to the stopped session"

# --- (5) the set is closed --------------------------------------------------------------

commands_before=$(q "select count(*) from spawn_commands")
submit human bb-key-f1 '{"type":"key","key":"f1"}'
[ "$status" -ne 0 ] || fail "(5) spawn submit took a key outside the set: $(out)"
assert_eq "$(q "select count(*) from spawn_commands")" "$commands_before" "(5) commands after the refused key"

echo "038: every named key reached the pane once, in order, at once (overtaking a waiting input), recorded with its sender; the page's path submits it as the person; a session that runs nothing refuses it with the reason"
exit 0
