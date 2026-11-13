#!/bin/sh
# Black box: every engine is refused its passfile, by name or by its
# variable, and the two process-listing globs of 108 that refused ordinary
# commands are anchored on a command position (objective 113, after 108;
# live before 110 hands any engine a PGPASSFILE).
#
# Why. 110 hands engines their database URLs without passwords, with
# PGPASSFILE naming a 0600 passfile under ~/.config/spawn/. The path is
# refused by name (`*.config/spawn*`), but `cat $PGPASSFILE` names only the
# variable: it must be refused before any engine holds one. And rev-e's N3
# on 108: `*ps x*` refused `ls groups x`, `cargo test -- steps x` and
# `jq '.ps x'`, and `*top *-c*` refused `systemctl --user stop x; sh -c true`
# (a refusal costs a fleet turn and teaches agents to route around the list).
#
# The harness is 108's: a real `spawn loop` runs a mission with a resident
# on a fake engine that keeps the --settings it is handed; one objective is
# assigned and delivered.
#   (1) Every engine's deny list holds each of the 14 entries in
#       $WORK/want, exactly: `ps x*` and `top *-c*` each at the start, after
#       `; `, `&& `, `| ` and `$(`; `Bash(*PGPASSFILE*)`; `Bash(*.pgpass*)`;
#       `Read(~/.pgpass)` and `Edit(~/.pgpass)` (rev-e's F1: the file tools,
#       which (2)'s matcher does not cover). It no longer holds
#       `Bash(*ps x*)` or `Bash(*top *-c*)`.
#   (2) Matched as Claude Code documents its Bash rules (`*` matches any
#       text; a compound command is also checked one part at a time), the
#       list refuses each of 16 commands on its whole text (the passfile by
#       variable, by ~/.pgpass and by path; `ps x` and `top -c` at each
#       command position), and none of 9 others (rev-e's four false
#       refusals, `pgrep -f`, `ps -p`, `top -b -n 1`, psql naming
#       $SPAWN_TEST_DATABASE_URL, `cargo test`), neither whole nor in any
#       part. The commands are text: none is run. (2)'s matcher is Claude
#       Code's Bash rules only; the Read and Edit rules are checked by (1)
#       as entries.
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
    echo "113: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep awk curl; do
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
SPAWN_DB_NAME="bb_113_runs_$run"
SPAWN_TMUX_SOCKET="bb-113-$run"
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
cleanup() {
    for pid in "$loop_pid" "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
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
            echo "113: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engines.log" ]; then
            echo "113: the engines' starts (ms|resident|execution|start|pid|native):" >&2
            cat "$WORK/engines.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "113: harness: unexpected exit $rc" >&2
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
  --resume) native=$2 ;;
  --session-id) native=$2 ;;
  --settings) settings=$2 ;;
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
cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

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
SPAWN_TMUX_PROGRAM=$WORK/tmux
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
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
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
note() { echo "113: $*" >&2; }
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
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
}
# objective_assign <mission> <objective>: the integrator's own path; prints
# the task's id, or nothing (the refusal in $WORK/assign.err).
objective_assign() {
    psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 \
        -c "select spawn_objective_assign($1, $2, '$WORK/ctl/scenarios/$2.sh', 'Objective $2 of 113 mission $1', null)" \
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

capture "$SPAWN_BIN" resident propose --name bbone --charter "Owns the 113 work."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name bbone
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
record '{"type":"mission.added","id":1,"text":"113 mission 1","origin":"human","sources":[]}' >/dev/null
record "{\"type\":\"mission.configured\",\"id\":1,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\",\"objectives\":{\"backlog\":\"$WORK/ctl/backlog\",\"resident\":\"bbone\",\"workers\":1}}" >/dev/null
t1=$(objective_assign 1 1)
[ -n "$t1" ] || harness "spawn_objective_assign failed: $(head -c 300 "$WORK/assign.err")"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
wait_until 90 delivered "$t1" || harness "task #$t1 was not delivered within 90 s (it is $(task_state "$t1"))"
worker_key=""
wait_until 30 worker_typed || {
    for f in "$WORK"/typed-*.log "$WORK"/settings-*.json; do [ -f "$f" ] && echo "113: seen ${f#"$WORK"/}: $(head -c 160 "$f" | tr '\n' ' ')" >&2; done
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
Bash(ps x*)
Bash(*; ps x*)
Bash(*&& ps x*)
Bash(*| ps x*)
Bash(*$(ps x*)
Bash(top *-c*)
Bash(*; top *-c*)
Bash(*&& top *-c*)
Bash(*| top *-c*)
Bash(*$(top *-c*)
Bash(*PGPASSFILE*)
Bash(*.pgpass*)
Read(~/.pgpass)
Edit(~/.pgpass)
ENTRIES
for key in $keys; do
    jq -r '.permissions.deny[]' "$WORK/settings-$key.json" >"$WORK/have-$key"
    missing=$(grep -vxFf "$WORK/have-$key" "$WORK/want" || true)
    [ -z "$missing" ] || check_failed "(1) the deny list handed to the engine of $key lacks $(printf '%s\n' "$missing" | grep -c .) of the $(grep -c . "$WORK/want") entries: $(printf '%s' "$missing" | head -n 8 | tr '\n' ' ')"
done
for key in $keys; do
    for gone in 'Bash(*ps x*)' 'Bash(*top *-c*)'; do
        ! grep -qxF "$gone" "$WORK/have-$key" || check_failed "(1) the deny list handed to the engine of $key still holds $gone"
    done
done
note "(1) checked: $(wc -l <"$WORK/want" | tr -d ' ') entries in each engine's deny list"

# --- (2) the list refuses the prints and the URLs, and lets the rest run --------------

cat >"$WORK/refused" <<'CMDS'
cat $PGPASSFILE
cat "$(printenv PGPASSFILE)"
head -c 100 "${PGPASSFILE}"
cat ~/.pgpass
cp ~/.pgpass /tmp/x
grep . /home/bb/.config/spawn/engines.pgpass
ps x
ps xww
cd /tmp && ps x
cd /tmp; ps x | grep cargo
echo $(ps x)
uptime | ps x
top -b -n 1 -c
top -c
cd /tmp && top -b -n 1 -c
cd /tmp; top -c | head
CMDS
cat >"$WORK/allowed" <<'CMDS'
ls groups x
cargo test -- steps x
jq '.ps x' data.json
systemctl --user stop x; sh -c true
pgrep -f cargo
ps -p 123
top -b -n 1
psql -d "$SPAWN_TEST_DATABASE_URL" -c 'select 1'
cargo test --workspace
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

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "113: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "113: every engine is refused its passfile by name or variable and the process listings at a command position, and none of the four false refusals of 108"
exit 0
