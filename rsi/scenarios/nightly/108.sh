#!/bin/sh
# Black box: every engine the Bridge starts is refused a command that holds
# a URL with a password, prints its whole environment, lists processes with
# their arguments, or reads a credential file by any reader; and its task
# says what to do instead.
#
# The record (rev-b, sync/url-leak-sources.md; the lead, 11-13). A database
# URL with its password reached 16 transcripts, every time through an
# agent's own Bash command:
# (2)+(3) an agent wrote the literal URL into its command line, which
#     then showed in other agents' `ps -o args` and `pgrep -af` output;
# (5) an agent printed its environment (`env | grep …`). Claude Code also
#     hands every tool shell the env block of ~/.claude/settings.json, which
#     holds the gateway key (lead-mac, 11-13 09:06), so a wholesale print
#     shows that key too.
# Engines' environments still hold SPAWN_TEST_DATABASE_URL, and the
# supervisor's holds SPAWN_DATABASE_URL. CREDENTIAL_DENY
# (crates/spawn-core/src/bridge.rs) refuses `*/environ*` and `ps …e`, but not
# `env`, `printenv`, `export -p`, `declare -x` or `set`, and nothing refuses a
# URL on a command line: CREDENTIALS_LINE only asks.
# This closes the accidental shell prints. `echo $X`, `env -u X` with no
# command, and a python or node one-liner that prints the environment are
# not refused (a glob cannot tell them from what must run): an engine's
# environment that holds no password (objective A1, next) closes the source.
#
# A real `spawn loop` (the Bridge, the dispatcher) runs a mission that takes
# its work from an objective backlog with a resident (bbone) on a fake
# engine, which keeps the --settings it is handed and every line typed into
# it. One objective is assigned and delivered (to the resident).
#   (1) The deny list handed to every engine started (the resident's, which
#       takes the objective's task, and any other) holds each of the 139 entries in $WORK/want below,
#       exactly. They are: `Bash(*://*:*@*)`; for each of `env`, `printenv`,
#       `export -p`, `declare -x`, `set`, `export`, `declare`, `declare -p`,
#       `typeset`, `typeset -p`, `typeset -x`, `env -0`, `env --null`,
#       `/usr/bin/env` and `command env` (P), `Bash(P)`, `Bash(P |*)`,
#       `Bash(P|*)`, `Bash(*; P)`, `Bash(*&& P)`, `Bash(*; P |*)` and
#       `Bash(*&& P |*)`; the process listings with arguments (`ps -ef`,
#       `ps aux`, `ps ax`, `ps -e -f`, `ps -A -f`, `ps -Af`, `ps -wwf`,
#       `ps … args/cmd/command`, `pgrep -a/-l` and their joined forms,
#       `pstree -a`, `*/cmdline*`, `Read(//proc/**)`; `ps x` and `top … -c`
#       are 113's, anchored on a command position); and Claude Code's settings
#       by any path (`*/.claude*`, `*cd .claude*`, `*settings.local.json*`).
#       An edit that drops one fails.
#   (2) Matched as Claude Code documents its Bash rules (`*` matches any
#       text; a compound command is also checked one part at a time), the
#       list refuses each of 72 commands on its whole text, so the
#       refusal does not depend on Claude Code splitting the command: the
#       prints alone and behind a pipe, `;` or `&&` (with the forms agents used
#       in the record: env | grep, env | sort, printenv | grep, set | grep,
#       export -p, /proc/self/environ, tr '\\0' '\\n' < /proc/<pid>/environ); URLs with a made-up
#       password; process listings with arguments; and the credential files
#       read by head, tail, less, more, grep, sed, awk, xxd, od, base64,
#       strings, cp, dd and python, perl and node one-liners. These include the old spawn.env copies
#       (spawn.env.bak-*) and ~/.claude/settings*.json. It refuses none of
#       27 others (`env VAR=x cargo test`, `printenv HOME`, `set -e`, a URL
#       without a password, `ps -p 123`, `pgrep -f cargo`, `declare -p FOO`,
#       `top -b -n 1`, `pstree -p`, …), neither
#       whole nor in any part. No command is run: they are text, and no
#       credential file is read.
#   (3) The task typed into the engine says what to do instead: "A command
#       that prints the whole environment, lists processes with their
#       arguments, reads a credential file or holds a URL with its password
#       is refused: the variable is already in your environment; name it
#       ($SPAWN_TEST_DATABASE_URL), never expand or print it; find a process
#       with `pgrep -f` (pids only) or `ps -p <pid>`, in a command of its
#       own."
# The limit of the check: it cannot run Claude Code (no print mode), so it
# reads the list the Bridge hands a fake engine and matches it as Claude
# Code documents; it does not watch a real engine refuse a command.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "108: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed grep awk curl; do
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
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home";; esac
export HOME
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_108_runs_$run"
. "$controller/scripts/blackbox/lib-herdr.sh"
herdr_setup "$WORK" "108"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
serve_pid=""
cleanup() {
    for pid in "$loop_pid" "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    herdr_cleanup
    if [ -n "$database_made" ]; then
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
            echo "108: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engines.log" ]; then
            echo "108: the engines' starts (ms|resident|execution|start|pid|native):" >&2
            cat "$WORK/engines.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "108: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

# A failed check is reported and the run goes on; the run fails at its end
# if any did. Unless the database is gone: then nothing read can be trusted.
: >"$WORK/failures"
check_failed() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
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


# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet, the transcript's end of the turn first, then the prompt.
# It writes its native transcript where Claude Code does, and logs each
# start: ms|resident|execution|new or resume|pid|native session.

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet. It keeps what an engine is handed: its --settings, as
# settings-<session>.json, and every line typed into it, in
# typed-<session>.log (the session's id with every character outside
# [A-Za-z0-9_] as _). It writes its native transcript where Claude Code
# does, so the Bridge sees its turns as a real engine's.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
settings=
while [ $# -gt 0 ]; do
  case "$1" in
  --resume) native=$2;;
  --session-id) native=$2;;
  --settings) settings=$2;;
  esac
  shift
done
key=$(printf '%s' "$SPAWN_SESSION_ID" | tr -c 'A-Za-z0-9_' '_')
printf '%s' "$settings" >"$BB_WORK/settings-$key.json"
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
while :; do
  if bb_read_line typed; then
    printf '%s\n' "$typed" >>"$BB_WORK/typed-$key.log"
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
  elif [ "$pending" = 1 ]; then
    printf '\nidle\n❯ '
    sleep 1
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN SPAWN_ENVELOPE SPAWN_SLICE http_proxy https_proxy \
    HTTP_PROXY HTTPS_PROXY; do
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
# The alert step reads every 2 s (default 30 s).
SPAWN_ALERT_CHECK_SECS=2
# A mission with no task in work this long is idle.
SPAWN_ALERT_MISSION_IDLE_SECS=8
SPAWN_RECOVER_BACKOFF_SECS=1
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
SPAWN_MISSION_FILE=$WORK/mission.md
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_ALERT_CHECK_SECS SPAWN_ALERT_MISSION_IDLE_SECS \
    SPAWN_RECOVER_BACKOFF_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_MISSION_FILE \
    SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/ctl/scenarios" "$WORK/ctl/backlog" "$WORK/web"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/engines.log"
printf '<!doctype html><title>spawn</title>\n' >"$WORK/web/index.html"

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
note() { echo "108: $*" >&2; }
wait_until() { # <seconds> <command...>
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
record() { # <JSON>: written as the integrator writes it; prints the event id
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_record('$1'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")";; esac
    echo "$id"
}
# objective_assign <mission> <objective>: the integrator's own path; prints
# the task's id, or nothing (the refusal in $WORK/assign.err).
objective_assign() {
    psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 \
        -c "select spawn_objective_assign($1, $2, '$WORK/ctl/scenarios/$2.sh', 'Objective $2 of 108 mission $1', null)" \
        2>"$WORK/assign.err" | head -n 1 || true
}
task_state() { q "select state from mission_tasks where id = $1"; }
delivered() { [ "$(task_state "$1")" = delivered ]; }
worker_typed() { # the engine that was typed the task (the resident does objective work)
    for f in "$WORK"/typed-*.log; do
        [ -f "$f" ] || continue
        grep -qi "task #$t1\b" "$f" && { worker_key=${f#"$WORK"/typed-}; worker_key=${worker_key%.log}; return 0; }
    done
    return 1
}

# --- the mission -------------------------------------------------------------------

capture "$SPAWN_BIN" resident propose --name bbone --charter "Owns the 108 work."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name bbone
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
record '{"type":"mission.added","id":1,"text":"108 mission 1","origin":"human","sources":[]}' >/dev/null
record "{\"type\":\"mission.configured\",\"id\":1,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\",\"objectives\":{\"backlog\":\"$WORK/ctl/backlog\",\"resident\":\"bbone\",\"workers\":1}}" >/dev/null
t1=$(objective_assign 1 1)
[ -n "$t1" ] || harness "spawn_objective_assign failed: $(head -c 300 "$WORK/assign.err")"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
wait_until 90 delivered "$t1" || harness "task #$t1 was not delivered within 90 s (it is $(task_state "$t1"))"
worker_key=""
wait_until 30 worker_typed || {
    for f in "$WORK"/typed-*.log "$WORK"/settings-*.json; do [ -f "$f" ] && echo "108: seen ${f#"$WORK"/}: $(head -c 160 "$f" | tr '\n' ' ')" >&2; done
    harness "no engine was typed task #$t1 within 30 s of its delivery"
}
resident_key=$(printf '%s' session_resident_bbone | tr -c 'A-Za-z0-9_' '_')
keys=$(printf '%s\n%s\n' "$worker_key" "$resident_key" | sort -u)
for key in $keys; do
    [ -s "$WORK/settings-$key.json" ] || harness "the engine of $key was handed no --settings"
    jq -e '.permissions.deny | type == "array"' "$WORK/settings-$key.json" >/dev/null 2>&1 ||
        harness "the engine of $key was handed settings with no permissions.deny list: $(head -c 300 "$WORK/settings-$key.json")"
done
note "engines: the task went to $worker_key; the resident is $resident_key"

# --- (1) every engine's deny list holds the new entries, each exactly -------------

cat >"$WORK/want" <<'ENTRIES'
Bash(*://*:*@*)
Bash(env)
Bash(env |*)
Bash(env|*)
Bash(*; env)
Bash(*&& env)
Bash(*; env |*)
Bash(*&& env |*)
Bash(printenv)
Bash(printenv |*)
Bash(printenv|*)
Bash(*; printenv)
Bash(*&& printenv)
Bash(*; printenv |*)
Bash(*&& printenv |*)
Bash(export -p)
Bash(export -p |*)
Bash(export -p|*)
Bash(*; export -p)
Bash(*&& export -p)
Bash(*; export -p |*)
Bash(*&& export -p |*)
Bash(declare -x)
Bash(declare -x |*)
Bash(declare -x|*)
Bash(*; declare -x)
Bash(*&& declare -x)
Bash(*; declare -x |*)
Bash(*&& declare -x |*)
Bash(set)
Bash(set |*)
Bash(set|*)
Bash(*; set)
Bash(*&& set)
Bash(*; set |*)
Bash(*&& set |*)
Bash(*ps -ef*)
Bash(*ps -fe*)
Bash(*ps -eF*)
Bash(*ps -f *)
Bash(*ps -fp*)
Bash(*ps aux*)
Bash(*ps axu*)
Bash(*ps -aux*)
Bash(*ps *args*)
Bash(*ps *cmd*)
Bash(*ps *command*)
Bash(*pgrep* -a*)
Bash(*pgrep* -fa*)
Bash(*pgrep* -af*)
Bash(*pgrep* -l*)
Bash(*pgrep* -fl*)
Bash(*pgrep* -lf*)
Bash(*pgrep* --list*)
Bash(*/cmdline*)
Read(//proc/**)
Bash(*/.claude*)
Bash(*cd .claude*)
Bash(*settings.local.json*)
Bash(export)
Bash(export |*)
Bash(export|*)
Bash(*; export)
Bash(*&& export)
Bash(*; export |*)
Bash(*&& export |*)
Bash(declare)
Bash(declare |*)
Bash(declare|*)
Bash(*; declare)
Bash(*&& declare)
Bash(*; declare |*)
Bash(*&& declare |*)
Bash(declare -p)
Bash(declare -p |*)
Bash(declare -p|*)
Bash(*; declare -p)
Bash(*&& declare -p)
Bash(*; declare -p |*)
Bash(*&& declare -p |*)
Bash(typeset)
Bash(typeset |*)
Bash(typeset|*)
Bash(*; typeset)
Bash(*&& typeset)
Bash(*; typeset |*)
Bash(*&& typeset |*)
Bash(typeset -p)
Bash(typeset -p |*)
Bash(typeset -p|*)
Bash(*; typeset -p)
Bash(*&& typeset -p)
Bash(*; typeset -p |*)
Bash(*&& typeset -p |*)
Bash(typeset -x)
Bash(typeset -x |*)
Bash(typeset -x|*)
Bash(*; typeset -x)
Bash(*&& typeset -x)
Bash(*; typeset -x |*)
Bash(*&& typeset -x |*)
Bash(env -0)
Bash(env -0 |*)
Bash(env -0|*)
Bash(*; env -0)
Bash(*&& env -0)
Bash(*; env -0 |*)
Bash(*&& env -0 |*)
Bash(env --null)
Bash(env --null |*)
Bash(env --null|*)
Bash(*; env --null)
Bash(*&& env --null)
Bash(*; env --null |*)
Bash(*&& env --null |*)
Bash(/usr/bin/env)
Bash(/usr/bin/env |*)
Bash(/usr/bin/env|*)
Bash(*; /usr/bin/env)
Bash(*&& /usr/bin/env)
Bash(*; /usr/bin/env |*)
Bash(*&& /usr/bin/env |*)
Bash(command env)
Bash(command env |*)
Bash(command env|*)
Bash(*; command env)
Bash(*&& command env)
Bash(*; command env |*)
Bash(*&& command env |*)
Bash(*ps ax*)
Bash(*ps -e -f*)
Bash(*ps -A -f*)
Bash(*ps -Af*)
Bash(*ps -wwf*)
Bash(*ps -wf*)
Bash(*pstree -a*)
Bash(*pstree -pa*)
Bash(*pstree -ap*)
Bash(*pstree * -a*)
ENTRIES
for key in $keys; do
    jq -r '.permissions.deny[]' "$WORK/settings-$key.json" >"$WORK/have-$key"
    missing=$(grep -vxFf "$WORK/have-$key" "$WORK/want" || true)
    [ -z "$missing" ] || check_failed "(1) the deny list handed to the engine of $key lacks $(printf '%s\n' "$missing" | grep -c .) of the $(grep -c . "$WORK/want") entries: $(printf '%s' "$missing" | head -n 8 | tr '\n' ' ')"
done
note "(1) checked: $(wc -l <"$WORK/want" | tr -d ' ') entries in each engine's deny list"

# --- (2) the list refuses the prints and the URLs, and lets the rest run --------------

cat >"$WORK/refused" <<'CMDS'
env
env | grep -i spawn
env|grep PG
cd /tmp && env
cd /tmp; env | sort
printenv
printenv | grep DATABASE
export -p
export -p | grep URL
declare -x
cd /tmp && declare -x | head
set
set | grep SPAWN
env | sort
cat /proc/self/environ
tr '\0' '\n' < /proc/4321/environ
psql 'postgres://bb:made-up-pw@%2Ftmp%2Fbb/bb' -c 'select 1'
curl -s https://bb:made-up-pw@example.invalid/
cd /tmp; SPAWN_TEST_DATABASE_URL=postgres://bb:made-up-pw@localhost/bb cargo test
ps -ef
ps -ef | grep cargo
ps aux | grep cargo
ps auxww
ps -eo pid,etimes,args
ps -o cmd= -p 1
ps -fp 1
pgrep -af cargo
pgrep -fa 'cargo test'
pgrep -fl spawn
pgrep -a bash
pgrep -u 501 -a sh
cat /proc/1/cmdline
tr '\0' ' ' </proc/123/cmdline
cat /proc/123/environ
head -c 200 ~/.config/spawn/test-database.url
tail -n 1 ~/.config/spawn/r2.env
grep -c . ~/.local/share/spawn-rsi/spawn.env.bak-relay
sed -n 1p ~/.local/share/spawn-rsi/spawn.env.bak-webdir
xxd ~/.local/share/spawn-rsi/spawn.env | head
base64 ~/.claude/settings.json
cd ~/.claude && grep TOKEN settings.json
cd; cd .claude; less settings.json
python3 -c "print(open('/home/bb/.claude/settings.local.json').read())"
cp ~/.config/spawn/backup.key /tmp/x
awk 1 ~/.local/share/spawn-rsi/env.sh
export
export | grep URL
declare
declare -p
typeset -p
typeset -x
env -0
env --null | tr '\0' '\n'
/usr/bin/env
command env
cd /tmp && export | sort
ps ax
ps x
ps axww
ps -e -f
ps -A -f
ps -Af
ps -wwf
top -b -n 1 -c
pstree -a
pstree -ap
od -c ~/.config/spawn/backup.key
strings ~/.local/share/spawn-rsi/spawn.env
dd if=~/.config/spawn/r2.env of=/tmp/x
more ~/.claude/settings.json
perl -ne print ~/.config/spawn/typesafe.env
node -e "require('fs').readFileSync('/home/bb/.claude/settings.json')"
CMDS
cat >"$WORK/allowed" <<'CMDS'
env VAR=x cargo test
env -u FOO sh -c true
env -i PATH=/usr/bin sh -c true
printenv HOME
set -e
set -euo pipefail; make
export FOO=bar
declare -a list
psql 'postgres://bb@%2Ftmp%2Fbb/bb' -c 'select 1'
curl -s http://127.0.0.1:8787/api/rsi
cargo test --workspace
grep -rn environment src
cd env && ls
git log --oneline -3
ps -o pid,etime -p 123
ps -p 123
pgrep -f cargo
pgrep -x tmux
kill 123
cat /proc/loadavg
ls ~/.config
cat README.md
cargo clippy --all-targets -- -D warnings
declare -p FOO
top -b -n 1
pstree -p
ps -p 1 -o pid,etime
CMDS
python3 -I - "$WORK/have-$worker_key" "$WORK/refused" "$WORK/allowed" >"$WORK/match.out" 2>&1 <<'PY' || true
import re, sys
# Claude Code's documented Bash rule: `*` matches any text; a compound
# command is also checked one part at a time.
rules = [l.rstrip("\n") for l in open(sys.argv[1]) if l.startswith("Bash(") and l.rstrip("\n").endswith(")")]
pats = [(r, re.compile("^" + re.escape(r[5:-1]).replace(r"\*", ".*") + "$", re.S)) for r in rules]
def hit(text):
    return [r for r, p in pats if p.match(text)]
parts = lambda c: [x.strip() for x in re.split(r"&&|\|\||;|\||\n", c) if x.strip()]
for c in open(sys.argv[2]).read().splitlines():
    if not hit(c):
        print("NOT REFUSED: " + c)
for c in open(sys.argv[3]).read().splitlines():
    h = hit(c) + [r for x in parts(c) for r in hit(x)]
    if h:
        print("REFUSED: " + c + "  by " + h[0])
PY
if [ -s "$WORK/match.out" ]; then
    check_failed "(2) the deny list, matched as Claude Code documents: $(head -n 12 "$WORK/match.out" | tr '\n' ' ')"
fi
note "(2) checked: $(wc -l <"$WORK/refused" | tr -d ' ') commands refused, $(wc -l <"$WORK/allowed" | tr -d ' ') run"

# --- (3) the task says what to do instead -------------------------------------------

sentence='A command that prints the whole environment, lists processes with their arguments, reads a credential file or holds a URL with its password is refused: the variable is already in your environment; name it ($SPAWN_TEST_DATABASE_URL), never expand or print it; find a process with `pgrep -f` (pids only) or `ps -p <pid>`, in a command of its own.'
grep -qF "$sentence" "$WORK/typed-$worker_key.log" ||
    check_failed "(3) the task typed into the engine does not say: $sentence"
note "(3) checked: the task's credentials line"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "108: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "108: every engine is refused a URL with a password, the wholesale environment prints, the process listings with arguments and the credential files by any reader, and its task says what to do instead"
exit 0
