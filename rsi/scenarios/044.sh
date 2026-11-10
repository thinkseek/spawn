#!/bin/sh
# Black box: after a host restart, every execution that was running is
# recorded as a host restart, whatever order its end is processed in.
#
# The recovery (crates/spawn-cli/src/recover.rs, objective 028) records each
# end as one `execution.interrupted` naming its cause. A terminal server
# (tmux) that is gone took every terminal with it: that is how a host restart
# is told from one engine that crashed, and its record is cause
# `host_restart`, evidence `terminal_server: gone`, path `resume`. Today the
# recovery asks whether a terminal server answers at the moment it processes
# the end (`interrupt_end`: `!self.terminals.server_alive()`). After a
# reboot, the first resume starts a new terminal server; an end processed
# after that reads the new server as "alive", and the execution is recorded
# `crashed`. Which executions get `host_restart` and which `crashed` depends
# on the order the ends are processed in: rev-g saw a resident recorded
# `crashed` one pass after five workers were recorded `host_restart`
# (028 (8), journey j07). Required: an execution's terminal server is gone
# when the server that ran it is gone: none answers, or the one that answers
# started after the execution did (not the same server). A server that
# answers and ran the execution is not gone.
#
# A real `spawn loop` (the Bridge, the dispatcher, the recovery; no model
# judge, no supervisor), run as systemd runs spawn-loop.service (started
# again 5 s after it exits), runs two ephemeral workers of one mission,
# `first` and `second`, each with a delivered task, on the fake engine below
# (it appends `work <n>` to work.txt in its workspace each second, and logs
# each turn it starts). Spawn calls tmux through SPAWN_TMUX_PROGRAM, a
# wrapper on the scenario's private socket. While `hold` exists the wrapper
# answers, for the terminal it names, what a server that cannot answer would:
# exit 3 for any call naming that terminal, and for `list-panes -a` (the
# Bridge then asks each terminal on its own). An error says nothing about a
# terminal, so the Bridge leaves that execution as it is until the hold is
# lifted: the order of the two ends is fixed, without timing. The hold
# changes nothing true of the terminal: it died with the server.
# SPAWN_RECOVER_BACKOFF_SECS=2. Checked:
#   (1) the host restarts: the loop is stopped as systemd stops it (SIGTERM,
#       its pass finished), the terminal server is killed with both engines
#       in it, the hold is put on `second`'s terminal, and the loop starts 2 s
#       later. `first`: an `execution.interrupted`, cause `host_restart`
#       (evidence: terminal_server `gone`), path `resume`; its resume by key
#       `recover-<execution>-<attempt>`; its engine works again in its own
#       native session within 20 s of the loop's start, with every line it
#       had written. Then a new terminal server answers (the resume started
#       it), and `second`'s execution has no recorded end and no
#       interruption (the harness checks the hold held: exit 2 otherwise).
#   (2) the second end: the hold is lifted. `second`: an
#       `execution.interrupted`, cause `host_restart` (evidence:
#       terminal_server `gone`), path `resume`, its resume by key, its engine
#       working again in its own native session within 20 s of the hold's
#       end, with every line.
#   (3) a crash is still a crash: with the new server running, `first`'s
#       resumed engine is killed (SIGKILL): its execution is recorded cause
#       `crashed`, path `resume`, never `host_restart` (the server that ran it
#       still answers), and its engine works again within 20 s, with every
#       line.
#   (4) no work lost: each worker's branch holds work.txt as `work 1..n`.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (2) on the live main (afbde256): `second`'s end, processed
# after `first`'s resume started a new terminal server, is recorded cause
# `crashed` (evidence: its terminal is gone, no exit status), not
# `host_restart`. It passes once whether an execution's terminal server is
# gone is told from the server that ran it (for example its pid and start
# time against the execution's start), not from whether some server answers
# when the end is processed.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN,
# SPAWN_TEST_DATABASE_URL, tmux and jq; creates and drops its own database,
# uses a private tmux socket, loopback only. Every wait is bounded in time.
# On a passing tree it takes about a minute.
set -eu

asserted=""
harness() {
    echo "044: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed awk; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
real_tmux=$(command -v tmux)

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_044_restart_$run"
SPAWN_TMUX_SOCKET="bb-044-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
wrapper_pid=""
stop_loop() {
    [ -n "$wrapper_pid" ] || return 0
    : >"$WORK/loop.stop"
    child=$(cat "$WORK/loop.pid" 2>/dev/null || true)
    if [ -n "$child" ]; then kill -TERM "$child" 2>/dev/null || true; fi
    i=0
    while kill -0 "$wrapper_pid" 2>/dev/null; do
        i=$((i + 1))
        if [ "$i" -eq 150 ] && [ -n "$child" ]; then kill -KILL "$child" 2>/dev/null || true; fi
        if [ "$i" -ge 200 ]; then
            kill -KILL "$wrapper_pid" 2>/dev/null || true
            break
        fi
        sleep 0.2
    done
    wait "$wrapper_pid" 2>/dev/null || true
    wrapper_pid=""
}
cleanup() {
    stop_loop
    "$real_tmux" -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
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
            echo "044: the loop's last lines:" >&2
            grep -v '^\s*$' "$WORK/loop.log" | tail -n 12 >&2
        fi
        if [ -s "$WORK/turns.log" ]; then
            echo "044: the engines' last turns (ms agent session execution native how lines valid):" >&2
            tail -n 6 "$WORK/turns.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "044: interruption records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select r ->> 'type', r ->> 'execution_id', r ->> 'cause', r ->> 'path'
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control' and r ->> 'type' like 'execution.%'
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "044: harness: unexpected exit $rc" >&2
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
note() { echo "044: $*"; }

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
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git init -q --bare --initial-branch=main "$repo"
git -C "$WORK/seed" push -q "$repo" main:main

mkdir -p "$WORK/claude-config" "$WORK/state"

# The fake engine: a prompt; text typed in opens a turn, which starts after
# a second of quiet. It writes its session transcript where Claude Code
# does. Once its agent was given work (a typed `BB:WORK`), every turn works:
# one step a second, each a tool call (assistant `tool_use`, then its
# `tool_result`) that appends the next line `work <n>` to work.txt in its
# workspace (n = the lines already there + 1). A turn logs, as it starts,
# the lines work.txt holds and whether they are exactly `work 1..n`
# (turns.log). Its PID is in pid-<agent>.
cat >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
mode=new
while [ $# -gt 0 ]; do
  case "$1" in
  --resume) native=${2-}; mode=resume; shift ;;
  --session-id) native=${2-}; mode=new; shift ;;
  esac
  shift
done
agent=$SPAWN_AGENT_ID
printf '%s\n' "$$" >"$BB_WORK/pid-$agent"
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
step() {
  n=0
  if [ -f work.txt ]; then n=$(wc -l <work.txt); fi
  n=$((n + 1))
  id="toolu_${SPAWN_EXECUTION_ID}_$count"
  line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
  printf 'work %s\n' "$n" >>work.txt
  printf '%s %s\n' "$(date +%s%3N)" "$n" >"$BB_WORK/last-$agent.$$" && mv -f "$BB_WORK/last-$agent.$$" "$BB_WORK/last-$agent"
  sleep 0.2
  line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
  sleep 0.8
}
turn() {
  lines=0
  valid=valid
  if [ -f work.txt ]; then
    lines=$(wc -l <work.txt)
    valid=$(awk 'BEGIN { ok = 1 } $0 != "work " NR { ok = 0 } END { print ok ? "valid" : "invalid" }' work.txt)
  fi
  printf '%s %s %s %s %s %s %s %s\n' "$(date +%s%3N)" "$agent" "$SPAWN_SESSION_ID" \
    "$SPAWN_EXECUTION_ID" "$native" "$mode" "$lines" "$valid" >>"$BB_WORK/turns.log"
  mode=input
  printf '\n✻ Working… (esc to interrupt)\n\n❯ '
  while :; do step; done
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in *BB:WORK*) : >"$BB_WORK/working-$agent" ;; esac
  elif [ "$pending" = 1 ]; then
    pending=0
    if [ -f "$BB_WORK/working-$agent" ]; then
      turn
    else
      line '"type":"system","subtype":"turn_duration","durationMs":1'
      printf '\nidle\n❯ '
    fi
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# Spawn's tmux, on the private socket. While `hold` names a terminal, a call
# that names it (as a target `=<terminal>:`, `=<terminal>`, or its paste
# buffer `spawn-<terminal>`) and `list-panes -a` exit 3, as a server that
# cannot answer: the Bridge leaves that execution as it is.
cat >"$WORK/tmux" <<EOF
#!/bin/sh
if [ -s '$WORK/hold' ]; then
    held=\$(cat '$WORK/hold')
    all=""
    panes=""
    for arg in "\$@"; do
        case "\$arg" in
        "=\$held:" | "=\$held" | "\$held" | "\$held:"* | "spawn-\$held")
            echo "bb hold: \$held does not answer" >&2
            exit 3
            ;;
        list-panes) panes=1 ;;
        -a) all=1 ;;
        esac
    done
    if [ -n "\$panes" ] && [ -n "\$all" ]; then
        echo "bb hold: list-panes -a does not answer" >&2
        exit 3
    fi
fi
exec '$real_tmux' -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

# `spawn loop` as systemd runs spawn-loop.service: started again 5 s after
# it exits (Restart=always, RestartSec=5). Each start and exit is logged.
cat >"$WORK/run-loop" <<'EOF'
#!/bin/sh
while [ ! -f "$WORK/loop.stop" ]; do
    "$SPAWN_BIN" loop --repo "$BB_REPO" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    child=$!
    printf '%s\n' "$child" >"$WORK/loop.pid"
    printf '%s start %s\n' "$(date +%s%3N)" "$child" >>"$WORK/loop-runs.log"
    status=0
    wait "$child" || status=$?
    printf '%s exit %s %s\n' "$(date +%s%3N)" "$child" "$status" >>"$WORK/loop-runs.log"
    [ ! -f "$WORK/loop.stop" ] || exit 0
    sleep 5
done
EOF

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_GATE SPAWN_DISPATCH \
    SPAWN_SUPERVISOR SPAWN_LOOP_INTERVAL_MS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_RELAY http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
BB_REPO=$repo
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
CLAUDE_CONFIG_DIR=$WORK/claude-config
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_RECOVER_BACKOFF_SECS=2
# No stall is under test.
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
export SPAWN_DATABASE_URL BB_REPO SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS CLAUDE_CONFIG_DIR BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_RECOVER_BACKOFF_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS \
    SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$WORK/turns.log"
: >"$WORK/loop-runs.log"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

start_loop() {
    rm -f "$WORK/loop.stop" "$WORK/loop.pid"
    sh "$WORK/run-loop" &
    wrapper_pid=$!
    i=0
    until [ -s "$WORK/loop.pid" ]; do
        i=$((i + 1))
        [ "$i" -lt 100 ] || harness "the loop did not start"
        sleep 0.1
    done
}

# --- helpers ----------------------------------------------------------------

now_ms() { date +%s%3N; }
q() { scalar "$1"; }
put() { printf '%s\n' "$3" >"$WORK/state/$1.$2"; }
get() { cat "$WORK/state/$1.$2" 2>/dev/null || true; }
secs() { awk -v ms="$1" 'BEGIN { printf "%.1f", ms / 1000 }'; }

# The live execution of a session, if any.
live_exec() {
    q "select execution_id from agent_executions where session_id = '$1' and ended_at_ms is null
        order by started_at_ms desc, execution_id desc limit 1"
}
ended_at() { q "select ended_at_ms from agent_executions where session_id = '$1' and execution_id = '$2'"; }
# The first `execution.interrupted` record of an execution: `<at_ms>|<record>`.
interrupted() {
    sql "select e.recorded_at_ms || '|' || (e.kind -> 'detail' -> 'record')::text
           from spawn_events e
          where e.session_id = 'session_control' and e.event_name = 'control.recorded'
            and e.kind -> 'detail' -> 'record' ->> 'type' = 'execution.interrupted'
            and e.kind -> 'detail' -> 'record' ->> 'execution_id' = '$1'
          order by e.session_seq limit 1"
}
revival_key() { # the command keyed `recover-<execution>-<attempt>`: `type|succeeded`
    q "select (action ->> 'type') || '|' || coalesce(succeeded::text, 'open')
         from spawn_commands where session_id = '$1' and idempotency_key = 'recover-$2-$3'"
}
# The engine's first turn of an agent that started after a time:
# `ms agent session execution native how lines valid`.
turn_after() { awk -v a="$1" -v t="$2" '$2 == a && $1 > t { print; exit }' "$WORK/turns.log"; }
last_line() { # the last line an agent's engine wrote: `ms n` ("0 0" before its first)
    _l=$(cat "$WORK/last-$1" 2>/dev/null) || _l=""
    case "$_l" in [0-9]*\ [0-9]*) echo "$_l" ;; *) echo "0 0" ;; esac
}
working() { # the agent's engine wrote a line within the last 3 s
    set -- "$(last_line "$1")"
    [ "$(($(now_ms) - ${1%% *}))" -lt 3000 ]
}
branch_show() {
    for git_dir in "$SPAWN_WORKSPACE_ROOT"/.sources/*.git "$repo"; do
        if git --git-dir="$git_dir" rev-parse -q --verify "refs/heads/$1" >/dev/null 2>&1; then
            git --git-dir="$git_dir" show "refs/heads/$1:work.txt" 2>/dev/null || true
            return 0
        fi
    done
}
branch_lines() { branch_show "$1" | wc -l | tr -d ' '; }
branch_valid() {
    branch_show "$1" |
        awk 'BEGIN { ok = 1 } $0 != "work " NR { ok = 0 } END { print ok ? "valid" : "invalid" }'
}
# Note an agent's execution and native session before a fault.
snapshot() {
    session=$(get "$1" session)
    put "$1" exec "$(live_exec "$session")"
    put "$1" native "$(q "select e.kind -> 'detail' ->> 'native_session_id' from spawn_events e
        where e.session_id = '$session' and e.event_name = 'session.native_started'
        order by e.id desc limit 1")"
    put "$1" fault "$(now_ms)"
}
server_answers() { "$real_tmux" -L "$SPAWN_TMUX_SOCKET" list-sessions >/dev/null 2>&1; }

# `<check> <label> <cause> <terminal_server gone: yes|no>`: the agent's
# execution (snapshot) was interrupted with that cause, path resume, its
# resume by key.
check_interrupted() {
    exec=$(get "$2" exec)
    session=$(get "$2" session)
    record=$(interrupted "$exec")
    [ -n "$record" ] ||
        fail "($1) $(get "$2" agent)'s execution $exec ended (Bridge: $(q "select coalesce(cause, '-') from agent_executions where execution_id = '$exec'")) and no execution.interrupted names it"
    json=${record#*|}
    cause=$(printf '%s' "$json" | jq -r .cause)
    path=$(printf '%s' "$json" | jq -r .path)
    server=$(printf '%s' "$json" | jq -r '.evidence.terminal_server // "-"')
    if [ "$4" = yes ]; then
        [ "$cause" = "$3" ] && [ "$path" = resume ] && [ "$server" = gone ] ||
            fail "($1) $(get "$2" agent)'s execution $exec died with the host's terminal server and is recorded cause $cause, path $path, terminal_server $server; it is host_restart, resume, gone: $json"
    else
        [ "$cause" = "$3" ] && [ "$path" = resume ] && [ "$server" != gone ] ||
            fail "($1) $(get "$2" agent)'s execution $exec was killed while the terminal server that ran it went on, and is recorded cause $cause, path $path, terminal_server $server; it is crashed, resume: $json"
    fi
    attempt=$(printf '%s' "$json" | jq -r '.attempt // empty')
    [ "$(revival_key "$session" "$exec" "$attempt" | cut -d'|' -f1)" = resume ] ||
        fail "($1) no resume recover-$exec-$attempt for $(get "$2" agent)"
}
# `<check> <label> <from ms> <bound ms>`: the agent's engine worked again in
# its own native session within the bound, with every line it had written.
check_worked() {
    agent=$(get "$2" agent)
    until [ -n "$(turn_after "$agent" "$(get "$2" fault)")" ] || [ "$(now_ms)" -ge $(($3 + $4)) ]; do
        sleep 0.5
    done
    turn=$(turn_after "$agent" "$(get "$2" fault)")
    [ -n "$turn" ] || fail "($1) $agent's engine never worked again (within $(secs "$4") s)"
    set -- "$@" $turn
    # $5 ms, $6 agent, $7 session, $8 execution, $9 native, ${10} how, ${11} lines, ${12} valid
    [ "$(($5 - $3))" -le "$4" ] ||
        fail "($1) $agent's engine worked again $(secs $(($5 - $3))) s after; within $(secs "$4") s"
    [ "$9" = "$(get "$2" native)" ] && [ "${10}" = resume ] ||
        fail "($1) $agent went on in native session $9 (${10}); it resumes its own, $(get "$2" native)"
    [ "${11}" -ge "$(get "$2" written)" ] && [ "${12}" = valid ] ||
        fail "($1) $agent's workspace held ${11} lines (${12}) when it went on; it had written $(get "$2" written): work was lost"
    put "$2" back "$5"
}

# --- the agents ----------------------------------------------------------------

capture "$SPAWN_BIN" mission add --text "044 host restart"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
labels="first second"
for label in $labels; do
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$label" \
        --brief "Work through the steps until stopped. BB:WORK"
    [ "$status" -eq 0 ] || harness "task assign ($label) failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $mission")
    put "$label" agent "$(q "select agent_id from mission_tasks where id = $task")"
    put "$label" session "$(q "select session_id from mission_tasks where id = $task")"
done

start_loop

deadline=$(($(date +%s) + 120))
for label in $labels; do
    agent=$(get "$label" agent)
    until [ "$(branch_lines "$agent")" -ge 3 ] && working "$agent"; do
        [ "$(date +%s)" -lt "$deadline" ] ||
            harness "$agent did not start working within 120 s (tasks: $(q "select string_agg(id || ':' || state || ':' || agent_id, ' ' order by id) from mission_tasks"))"
        sleep 0.5
    done
done
note "both work: $(for l in $labels; do printf '%s=%s ' "$l" "$(branch_lines "$(get "$l" agent)")"; done)"

# --- (1) the host restarts -----------------------------------------------------------

for label in $labels; do snapshot "$label"; done
[ -n "$(get first exec)" ] && [ -n "$(get second exec)" ] || harness "no live execution to restart"
restart=$(now_ms)
# As systemd stops the host: the loop gets SIGTERM and finishes its pass,
# then the terminal server dies with every engine in it.
stop_loop
printf 'spawn-%s\n' "$(get second exec)" >"$WORK/hold"
"$real_tmux" -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
sleep 2
server_answers && harness "the terminal server still answers after it was killed"
for label in $labels; do put "$label" written "$(last_line "$(get "$label" agent)" | cut -d' ' -f2)"; done
started=$(now_ms)
start_loop
check_worked 1 first "$started" 20000
check_interrupted 1 first host_restart yes
note "(1) first: host_restart, resumed, working again $(secs $(($(get first back) - started))) s after the loop's start"
server_answers || harness "no terminal server answers after first's resume"
[ -z "$(ended_at "$(get second session)" "$(get second exec)")" ] ||
    harness "second's end was recorded while its terminal was held: the order is not the one under test"
[ -z "$(interrupted "$(get second exec)")" ] ||
    harness "second's execution was interrupted while its terminal was held"

# --- (2) the second end, with a new server up ---------------------------------------------

released=$(now_ms)
rm -f "$WORK/hold"
check_worked 2 second "$released" 20000
check_interrupted 2 second host_restart yes
note "(2) second: host_restart, resumed, working again $(secs $(($(get second back) - released))) s after the hold's end"

# --- (3) a crash on the new server is a crash ---------------------------------------------

sleep 3
working "$(get first agent)" || harness "first does not work before the crash"
snapshot first
[ "$(get first exec)" != "" ] || harness "first has no live execution to crash"
put first written "$(last_line "$(get first agent)" | cut -d' ' -f2)"
crashed=$(now_ms)
kill -KILL "$(cat "$WORK/pid-$(get first agent)")"
check_worked 3 first "$crashed" 20000
check_interrupted 3 first crashed no
note "(3) first: crashed on the running server, resumed, working again $(secs $(($(get first back) - crashed))) s after"

# --- (4) no work lost -------------------------------------------------------------------

sleep 3
for label in $labels; do
    agent=$(get "$label" agent)
    [ "$(branch_valid "$agent")" = valid ] ||
        fail "(4) $agent's branch work.txt is not lines 1..n in order: work was lost or doubled"
done

note "both ends of the restart recorded host_restart, in either order; a crash on a live server stays crashed"
exit 0
