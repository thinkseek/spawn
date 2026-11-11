#!/bin/sh
# Journey j05, operator: the CLI and the HTTP API an operator drives by hand.
#
# It folds (every assertion of each, unless named):
#   blackbox 010  the command line contract: init-db is repeatable, the usage
#                 names every command, an unknown or retired subcommand
#                 fails, a missing or invalid flag fails before the store;
#   blackbox 020  `spawn submit`: one queue row and one command.accepted event
#                 each, the same id for a repeated (principal, key), a new row
#                 for a new key or principal, nothing written on a rejection;
#   blackbox 030  `spawn tick` and the workspace store: the base tree under
#                 the root without .git, the prompt typed into the pane, one
#                 execution for two ticks;
#   blackbox 060  `spawn run -- <program>`: exit code forwarded, exit file
#                 written whole, no store writes, output to the pane only;
#   blackbox 080  the command gateway (`agent_commands`) and grants; the CLI
#                 bypasses them;
#   blackbox 100  `spawn serve`: the read-only API and static files, no
#                 writes, the token off loopback;
#   001           `spawn version` prints one line starting with "spawn " (with
#                 blackbox 010);
#   027 (1)-(6)   `spawn mission configure` changes only what it is given;
#   007 (2)       `spawn task stages --id N` ends `cancelled` for each task
#                 the closure rule cancels (007 (1), (3)-(5) move to CI as a
#                 Rust test, step 3);
#   017 (4)       `/api/scorecard` answers the same JSON as `spawn scorecard
#                 --json` on 017's seeded history (017's numbers, (1)-(3),
#                 move to CI, step 3).
# Blackbox 040 (queue) and 070 (views) are not here: they move to CI (step 3).
#
# Shape. No loop. Four databases, run one after another, because the parts'
# assertions count whole tables (spawn_commands, spawn_events, the tmux
# sessions) from an empty store, and a tick runs every pending command:
#   A: 010, 060, 020 (none of 010/060 writes a row, so 020's counts hold
#      as written), then 027 and 007 (they read their own ids only);
#   B: 080 alone (its counts start at zero);
#   C: 030 alone (its tick must find exactly its own command);
#   D: 100 (its overview starts with no session), then 017's history is
#      seeded and 100's loopback server answers 017 (4). The history's
#      window is October 2026, so 100's rows (written now) fall outside it.
# One tmux socket (only 030 opens a terminal), one bare repository, one
# fake engine for the whole journey. Each step echoes `j05: [NNN ...]` before
# the checks it carries; blackbox steps are named [bbNNN <section>], after
# the scenario's own `# ---` sections. Traceability:
# /var/tmp/spawn/sync/gate-journeys-spawn-proof/trace/j05.tsv.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
# Named check-*: the gate runs a failing check again, and on the base if it fails twice (design-gate-v2.md C2)
#
# Runs inside the gate's sandbox (scripts/rsi/gate.sh), from the root of the
# tree under test: needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and
# drops its own databases, uses a private tmux socket, no network but
# loopback. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "j05: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od curl sed grep cmp diff timeout; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

J=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$J"' EXIT
# A scratch HOME, and the tmux server's socket, in $J: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$J/home
TMUX_TMPDIR=$J/tmux-sock
mkdir -p "$HOME" "$TMUX_TMPDIR"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME TMUX_TMPDIR
# The runtime directory too (backlog S20): what Spawn keeps there (a
# launch's environment file, its terminals' leaders) stays in J, never
# in the real one. systemd's user manager stays reachable by its bus.
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$J/runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_TMUX_SOCKET="bb-j05-$run"
SPAWN_PSQL=psql
export SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

databases=""
serve_pid=""
guarded_pid=""
part=""
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ "$rc" -ne 130 ]; then
        echo "j05: failed in part $part" >&2
        if [ "$part" = 027 ] && [ -n "${SPAWN_DB_NAME-}" ]; then
            echo "j05: the mission's configuration records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select kind -> 'detail' -> 'record' from spawn_events
                where kind -> 'detail' -> 'record' ->> 'type' = 'mission.configured' order by id" >&2 2>/dev/null || true
        fi
        if [ "$part" = 017 ] && [ -s "${WORK-}/card.json" ]; then
            echo "j05: the scorecard's answer (first 60 lines):" >&2
            head -n 60 "$WORK/card.json" >&2
        fi
    fi
    for pid in $serve_pid $guarded_pid; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    for db in $databases; do
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$db" >/dev/null 2>&1 || true
    done
    chmod -R u+rwX "$J" 2>/dev/null || true
    rm -rf "$J"
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || [ "$rc" -eq 130 ] || echo "j05: harness: unexpected exit $rc" >&2
    [ "$rc" -eq 130 ] && exit 130
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

# Logins of this journey's databases that a run killed before its cleanup
# left behind (scripts/blackbox/scenario-db.sh): this prefix only.
sh "$controller/scripts/blackbox/scenario-db.sh" sweep "bb_j05" >/dev/null 2>&1 || true

WORK=$J
export WORK
. "$controller/scripts/blackbox/assertions.sh"

# An assertion exits 1, unless the database is gone: then nothing the
# journey read can be trusted, and that is the harness's failure.
fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: [$part] $*" >&2
    exit 1
}

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL; do
    unset "$name" 2>/dev/null || true
done

# --- the world: what scripts/blackbox/run-scenarios.sh gave each scenario ------

cat >"$J/gitconfig" <<'EOF'
[user]
	name = bb
	email = bb@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
EOF
GIT_CONFIG_GLOBAL=$J/gitconfig
export GIT_CONFIG_GLOBAL
repo=$J/repo.git
git init -q --bare --initial-branch=main "$repo"
git init -q --initial-branch=main "$J/seed"
printf 'base\n' >"$J/seed/README.md"
git -C "$J/seed" add -A
git -C "$J/seed" commit -qm base
git -C "$J/seed" push -q "$repo" main:main

cat >"$J/claude" <<'FAKE'
#!/bin/sh
printf 'Welcome to Claude Code\n\n'
printf '❯ '
while IFS= read -r line; do
  printf '\n%s\n' "echo: $line"
  printf '❯ '
done
FAKE
chmod 755 "$J/claude"

# The local tmux host runs the program named by SPAWN_TMUX_PROGRAM and adds no
# socket of its own, while assertions.sh reads SPAWN_TMUX_SOCKET: one wrapper
# pins the journey's private socket (030's and 100's, shared).
cat >"$J/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$J/tmux"

SPAWN_GIT_REPO=$repo
SPAWN_CLAUDE_PROGRAM=$J/claude
SPAWN_TMUX_PROGRAM=$J/tmux
SPAWN_WORKER=worker-bb
export SPAWN_GIT_REPO SPAWN_CLAUDE_PROGRAM SPAWN_TMUX_PROGRAM SPAWN_WORKER

# database <word>: a new empty database, its scratch directory and workspace
# root, for the parts that follow.
database() {
    SPAWN_DB_NAME="bb_j05_$1_$run"
    psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
        harness "cannot create the database $SPAWN_DB_NAME"
    databases="$databases $SPAWN_DB_NAME"
    SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
    WORK=$J/$1
    SPAWN_WORKSPACE_ROOT=$WORK/ws
    mkdir -p "$SPAWN_WORKSPACE_ROOT"
    export SPAWN_DB_NAME SPAWN_DATABASE_URL WORK SPAWN_WORKSPACE_ROOT
}

# ================================================================================
# Database A: blackbox 010, 060, 020; 027; 007 (2)
# ================================================================================
database a

# --- blackbox 010: the command line contract -------------------------------------
part=bb010
echo "j05: [bb010 cli contract] init-db is repeatable"
capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"
assert_contains "$(out)" "schema installed" "init-db output"

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "second init-db exit"

echo "j05: [bb010 cli contract] the usage, an unknown command, retired commands"
capture "$SPAWN_BIN"
assert_eq "$status" 1 "no subcommand exit"
assert_contains "$(err)" \
    "usage: spawn <init-db|submit|tick|loop|mission|task|resident|integrate|serve|fleet|tail|run|version>" \
    "the usage names every command"

capture "$SPAWN_BIN" nonsense
assert_eq "$status" 1 "unknown subcommand exit"
assert_contains "$(err)" "unknown command nonsense" "unknown subcommand message"

for gone in status dispatch charter hook corpus judge; do
    capture "$SPAWN_BIN" "$gone"
    assert_eq "$status" 1 "$gone exit"
    assert_contains "$(err)" "unknown command $gone" "$gone is not a command"
done

# 001 whole: `spawn version` prints one line that starts with "spawn ".
echo "j05: [001] spawn version"
capture "$SPAWN_BIN" version
assert_eq "$status" 0 "001: spawn version exit"
assert_eq "$(out | grep -c '^spawn ')" 1 "001: lines of spawn version starting with 'spawn '"

echo "j05: [bb010 cli contract] a missing or invalid flag fails before the store"
capture "$SPAWN_BIN" submit --agent agent_x --session session_x
assert_eq "$status" 1 "missing --key exit"
assert_contains "$(err)" "--key" "missing --key message"

capture "$SPAWN_BIN" submit --agent agent_x --session session_x --key k1 --action 'nope'
assert_eq "$status" 1 "invalid action exit"
assert_contains "$(err)" "--action" "invalid action message"

assert_eq "$(scalar 'select count(*) from spawn_commands')" 0 "no command was written"

# --- blackbox 060: spawn run -------------------------------------------------------
part=bb060
echo "j05: [bb060 the program is required]"
capture "$SPAWN_BIN" run
assert_eq "$status" 1 "run without a program exit"
assert_contains "$(err)" "spawn: run needs a program after --" \
    "run without a program message"

capture "$SPAWN_BIN" run --
assert_eq "$status" 1 "run with an empty -- exit"
assert_contains "$(err)" "spawn: run needs a program after --" \
    "run with an empty -- message"

echo "j05: [bb060 no exit file: the program still runs and its code is forwarded]"
capture env SPAWN_EXIT_FILE= \
    "$SPAWN_BIN" run -- sh -c 'printf "engine-out\n"; exit 7'
assert_eq "$status" 7 "exit code without SPAWN_EXIT_FILE"
assert_eq "$(out)" "engine-out" "stdout forwarded without SPAWN_EXIT_FILE"
assert_eq "$(err)" "" "no error without SPAWN_EXIT_FILE"

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"

echo "j05: [bb060 the full path: run, forward stdout, write the real exit code]"
exits=$WORK/exits
capture env SPAWN_ENGINE=claude SPAWN_AGENT_ID=agent_bb SPAWN_SESSION_ID=session_bb \
    SPAWN_EXECUTION_ID=exec_bb SPAWN_EXIT_FILE="$exits/exec_bb" \
    "$SPAWN_BIN" run -- sh -c 'printf "engine-said-hi\n"; exit 7'
assert_eq "$status" 7 "exit code with an exit file"
assert_eq "$(out)" "engine-said-hi" "stdout forwarded with an exit file"
assert_eq "$(err)" "" "no error on the exit-file path"
assert_file "$exits/exec_bb"
assert_eq "$(cat "$exits/exec_bb")" 7 "the exit file holds the exit code"
assert_no_file "$exits/exec_bb.partial"
assert_eq "$(scalar 'select count(*) from spawn_events')" 0 \
    "the runner appends no event"

capture env SPAWN_EXIT_FILE="$exits/exec_bb" "$SPAWN_BIN" run -- sh -c 'exit 0'
assert_eq "$status" 0 "exit code of a clean run"
assert_eq "$(cat "$exits/exec_bb")" 0 "the exit file holds the new code"

echo "j05: [bb060 a program that cannot start writes no exit file]"
capture env SPAWN_EXIT_FILE="$exits/exec_missing" \
    "$SPAWN_BIN" run -- "$WORK/no-such-program"
assert_eq "$status" 1 "exit code for a program that cannot start"
assert_contains "$(err)" "spawn: start $WORK/no-such-program:" \
    "start failure names the program"
assert_no_file "$exits/exec_missing"

echo "j05: [bb060 a program killed by a signal has no exit code]"
capture env SPAWN_EXIT_FILE="$exits/exec_signal" \
    "$SPAWN_BIN" run -- sh -c 'kill -TERM $$'
assert_eq "$status" 1 "signal-killed program exits 1"
assert_file "$exits/exec_signal"
assert_eq "$(cat "$exits/exec_signal")" "" "a signal death leaves an empty exit file"

echo "j05: [bb060 the output goes to the pane only]"
capture env SPAWN_ENGINE=codex SPAWN_ENGINE_OUTPUT="$WORK/copy/out.jsonl" \
    SPAWN_EXIT_FILE="$exits/exec_lines" \
    "$SPAWN_BIN" run -- sh -c 'printf "line-1\nline-2\n"; exit 5'
assert_eq "$status" 5 "exit code with an output copy asked for"
assert_eq "$(out)" "line-1
line-2" "every output line reaches the pane"
assert_eq "$(err)" "" "no error with an output copy asked for"
assert_no_file "$WORK/copy/out.jsonl"
assert_eq "$(cat "$exits/exec_lines")" 5 "the exit file holds the exit code"

assert_eq "$(scalar 'select count(*) from spawn_events')" 0 \
    "no run appended an event"

# --- blackbox 020: spawn submit ------------------------------------------------------
part=bb020
echo "j05: [bb020 first submit]"
capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"

capture "$SPAWN_BIN" submit --agent agent_001 --session session_001 \
    --key key-1 --action '{"type":"start","engine":"claude","prompt":"hello"}'
assert_eq "$status" 0 "first submit exit"
first=$(out | sed -n 's/^command=//p')
assert_contains "$(out)" "command=" "submit output names the command"
case "$first" in
    '' | *[!0-9]*) fail "submit printed no numeric command id: [$(out)]" ;;
esac

assert_eq "$(scalar 'select count(*) from spawn_commands')" 1 "one command row"
assert_eq "$(scalar "select state from spawn_commands where id = $first")" \
    "pending" "new command state"
assert_eq "$(scalar "select principal from spawn_commands where id = $first")" \
    "spawn" "default principal"
assert_eq "$(scalar "select idempotency_key from spawn_commands where id = $first")" \
    "key-1" "stored idempotency key"
assert_eq "$(scalar "select agent_id from spawn_commands where id = $first")" \
    "agent_001" "stored agent"
assert_eq "$(scalar "select session_id from spawn_commands where id = $first")" \
    "session_001" "stored session"
assert_eq "$(scalar "select action ->> 'type' from spawn_commands where id = $first")" \
    "start" "stored action type"
assert_eq "$(scalar "select action ->> 'prompt' from spawn_commands where id = $first")" \
    "hello" "stored action prompt"

assert_eq "$(scalar 'select count(*) from spawn_events')" 1 "one accepted event"
assert_eq "$(scalar 'select event_name from spawn_events')" \
    "command.accepted" "event name"
assert_eq "$(scalar 'select source from spawn_events')" "gateway" "event source"
assert_eq "$(scalar "select source_event_id from spawn_events")" \
    "cmd_${first}:accepted" "event source key"
assert_eq "$(scalar "select kind -> 'detail' ->> 'command_id' from spawn_events")" \
    "$first" "event command id"
assert_eq "$(scalar 'select agent_id from spawn_events')" "agent_001" "event agent"
assert_eq "$(scalar 'select session_id from spawn_events')" "session_001" "event session"
assert_eq "$(scalar 'select session_seq from spawn_events')" 1 "event session sequence"

echo "j05: [bb020 same (principal, key) is the same command]"
capture "$SPAWN_BIN" submit --agent agent_001 --session session_001 \
    --key key-1 --action '{"type":"start","engine":"claude","prompt":"hello"}'
assert_eq "$status" 0 "repeat submit exit"
assert_eq "$(out)" "command=$first" "repeat returns the same id"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 1 \
    "repeat leaves one command row"
assert_eq "$(scalar 'select count(*) from spawn_events')" 1 \
    "repeat appends no second accepted event"

echo "j05: [bb020 a different key is a different command]"
capture "$SPAWN_BIN" submit --agent agent_001 --session session_001 \
    --key key-2 --action '{"type":"input","text":"go"}'
assert_eq "$status" 0 "second key exit"
second=$(out | sed -n 's/^command=//p')
case "$second" in
    '' | *[!0-9]*) fail "second submit printed no numeric command id: [$(out)]" ;;
esac
[ "$second" != "$first" ] || fail "new key returned the old id $first"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 2 \
    "new key adds one command row"
assert_eq "$(scalar "select state from spawn_commands where id = $second")" \
    "pending" "second command state"
assert_eq "$(scalar "select action ->> 'type' from spawn_commands where id = $second")" \
    "input" "second command action type"
assert_eq "$(scalar 'select count(*) from spawn_events')" 2 \
    "new key appends one more accepted event"
assert_eq "$(scalar "select source_event_id from spawn_events where source_event_id = 'cmd_${second}:accepted'")" \
    "cmd_${second}:accepted" "second event source key"
assert_eq "$(scalar "select session_seq from spawn_events where source_event_id = 'cmd_${second}:accepted'")" \
    2 "second event follows the first on the session"

echo "j05: [bb020 --principal is recorded, and keys are scoped per principal]"
capture "$SPAWN_BIN" submit --principal alice --agent agent_002 \
    --session session_002 --key key-1 --action '{"type":"stop"}'
assert_eq "$status" 0 "principal submit exit"
third=$(out | sed -n 's/^command=//p')
case "$third" in
    '' | *[!0-9]*) fail "principal submit printed no numeric command id: [$(out)]" ;;
esac
[ "$third" != "$first" ] || fail "same key under another principal returned $first"
assert_eq "$(scalar "select principal from spawn_commands where id = $third")" \
    "alice" "--principal is recorded"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 3 \
    "same key under another principal adds a row"
assert_eq "$(scalar 'select count(*) from spawn_events')" 3 \
    "same key under another principal adds an event"
assert_eq "$(scalar "select source_event_id from spawn_events where source_event_id = 'cmd_${third}:accepted'")" \
    "cmd_${third}:accepted" "principal event source key"

capture "$SPAWN_BIN" submit --principal alice --agent agent_002 \
    --session session_002 --key key-1 --action '{"type":"stop"}'
assert_eq "$status" 0 "principal repeat exit"
assert_eq "$(out)" "command=$third" "principal repeat returns the same id"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 3 \
    "principal repeat leaves three rows"

echo "j05: [bb020 rejections write nothing]"
capture "$SPAWN_BIN" submit --agent bad --session session_001 \
    --key key-3 --action '{"type":"stop"}'
assert_eq "$status" 1 "invalid agent id exit"
assert_contains "$(err)" "identifier must be" "invalid agent id message"

capture "$SPAWN_BIN" submit --principal '' --agent agent_001 \
    --session session_001 --key key-4 --action '{"type":"stop"}'
assert_eq "$status" 1 "empty principal exit"
assert_contains "$(err)" "label length 0" "empty principal message"

capture "$SPAWN_BIN" submit --agent agent_001 --session session_001 \
    --key key-5 --action '{"type":"nope"}'
assert_eq "$status" 1 "invalid action exit"
assert_contains "$(err)" "--action is not a command action" "invalid action message"

capture "$SPAWN_BIN" submit --agent agent_001 --session session_001 \
    --action '{"type":"stop"}'
assert_eq "$status" 1 "missing key exit"
assert_contains "$(err)" "--key is required" "missing key message"

assert_eq "$(scalar 'select count(*) from spawn_commands')" 3 \
    "rejections add no command row"
assert_eq "$(scalar 'select count(*) from spawn_events')" 3 \
    "rejections add no event"

# --- 027: spawn mission configure changes only what it is given ------------------------
# 027's setup, on database A (the journey's bare repository is its source:
# the same one `main` commit holding README.md "base").
part=027
source=$repo
gate=$WORK/gate
backlog=$WORK/backlog
mkdir -p "$gate" "$backlog"
printf '# nothing protected\n' >"$gate/protected"
gate=$(cd "$gate" && pwd)
backlog=$(cd "$backlog" && pwd)

q() { scalar "$1"; }
# The mission's configuration as its row holds it, one canonical JSON line.
row() {
    q "select jsonb_build_object('gate', gate, 'accept', accept, 'budget', budget,
                                 'objectives', objectives)::text
         from mission_missions where id = $mission"
}
want() { # <gate|null> <accept> <budget json|null> <objectives json|null>
    q "select jsonb_build_object('gate', $1, 'accept', '$2', 'budget', $3::jsonb,
                                 'objectives', $4::jsonb)::text"
}
configure() {
    capture "$SPAWN_BIN" mission configure --id "$mission" "$@"
}
OBJ="'{\"backlog\": \"$backlog\", \"resident\": \"rsi\", \"workers\": 3}'"

capture "$SPAWN_BIN" mission add --text "027 configure" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
configure --gate "$gate" --accept gate --minutes 45 --tool-calls 60 --backlog "$backlog" \
    --resident rsi --workers 3
[ "$status" -eq 0 ] || harness "the first, full configure exited $status: $(err)"
full=$(want "'$gate'" gate "'{\"minutes\": 45, \"tool_calls\": 60}'" "$OBJ")
[ "$(row)" = "$full" ] || harness "the full configure stored $(row), not $full"

echo "j05: [027 (1)] one number"
configure --tool-calls 200
[ "$status" -eq 0 ] || fail "(1) \`spawn mission configure --id $mission --tool-calls 200\` exited $status: $(err | head -n 2)"
printed=$(out)
expected=$(want "'$gate'" gate "'{\"minutes\": 45, \"tool_calls\": 200}'" "$OBJ")
got=$(row)
[ "$got" = "$expected" ] ||
    fail "(1) \`spawn mission configure --id $mission --tool-calls 200\` changed more than the tool-call budget: the mission is now $got, was $full (wanted $expected)"

echo "j05: [027 (2)] before and after"
side() { # <before|after>: the printed side, canonical
    printf '%s' "$printed" | jq -c --arg s "$1" '.[$s] | {gate, accept, budget, objectives}' 2>/dev/null |
        while IFS= read -r json; do
            q "select '$(printf '%s' "$json" | sed "s/'/''/g")'::jsonb::text"
        done
}
[ "$(side before)" = "$full" ] ||
    fail "(2) the output's \`before\` is [$(side before)], not the configuration before the call, $full (output: $(printf '%s' "$printed" | tr -d '\n' | head -c 300))"
[ "$(side after)" = "$expected" ] ||
    fail "(2) the output's \`after\` is [$(side after)], not the configuration after the call, $expected"

echo "j05: [027 (3)] the record holds the whole result"
record=$(q "select (kind -> 'detail' -> 'record') - 'type' - 'id' from spawn_events
             where kind -> 'detail' -> 'record' ->> 'type' = 'mission.configured'
               and kind -> 'detail' -> 'record' ->> 'id' = '$mission'
             order by id desc limit 1")
recorded=$(q "select jsonb_build_object('gate', r -> 'gate', 'accept', r -> 'accept',
                                        'budget', r -> 'budget', 'objectives', r -> 'objectives')::text
                from (select '$(printf '%s' "$record" | sed "s/'/''/g")'::jsonb as r) x")
[ "$recorded" = "$expected" ] ||
    fail "(3) the newest mission.configured record holds $recorded, not the whole resulting configuration $expected"

echo "j05: [027 (4)] one setting of a group"
configure --workers 2
[ "$status" -eq 0 ] || fail "(4) \`configure --workers 2\` exited $status: $(err | head -n 1)"
expected=$(want "'$gate'" gate "'{\"minutes\": 45, \"tool_calls\": 200}'" \
    "'{\"backlog\": \"$backlog\", \"resident\": \"rsi\", \"workers\": 2}'")
[ "$(row)" = "$expected" ] || fail "(4) \`configure --workers 2\` left the mission $(row), not $expected"
configure --minutes 90
[ "$status" -eq 0 ] || fail "(4) \`configure --minutes 90\` exited $status: $(err | head -n 1)"
expected=$(want "'$gate'" gate "'{\"minutes\": 90, \"tool_calls\": 200}'" \
    "'{\"backlog\": \"$backlog\", \"resident\": \"rsi\", \"workers\": 2}'")
[ "$(row)" = "$expected" ] || fail "(4) \`configure --minutes 90\` left the mission $(row), not $expected"

echo "j05: [027 (5)] clearing is explicit"
configure --no-budget
[ "$status" -eq 0 ] || fail "(5) \`configure --no-budget\` exited $status: $(err | head -n 1)"
expected=$(want "'$gate'" gate null "'{\"backlog\": \"$backlog\", \"resident\": \"rsi\", \"workers\": 2}'")
[ "$(row)" = "$expected" ] || fail "(5) \`configure --no-budget\` left the mission $(row), not $expected"
before=$(row)
configure --no-gate
[ "$status" -ne 0 ] ||
    fail "(5) \`configure --no-gate\` on a mission with objectives succeeded (objectives need a gate): the mission is now $(row)"
[ "$(row)" = "$before" ] || fail "(5) a refused \`configure --no-gate\` changed the mission to $(row)"
configure --no-objectives
[ "$status" -eq 0 ] || fail "(5) \`configure --no-objectives\` exited $status: $(err | head -n 1)"
expected=$(want "'$gate'" gate null null)
[ "$(row)" = "$expected" ] || fail "(5) \`configure --no-objectives\` left the mission $(row), not $expected"
configure --no-gate --accept review
[ "$status" -eq 0 ] || fail "(5) \`configure --no-gate --accept review\` exited $status: $(err | head -n 1)"
expected=$(want null review null null)
[ "$(row)" = "$expected" ] || fail "(5) \`configure --no-gate --accept review\` left the mission $(row), not $expected"

echo "j05: [027 (6)] nothing given, nothing changed"
configure --gate "$gate" --accept gate --tool-calls 7
[ "$status" -eq 0 ] || fail "(6) \`configure --gate <dir> --accept gate --tool-calls 7\` exited $status: $(err | head -n 1)"
before=$(row)
configure
[ "$(row)" = "$before" ] || fail "(6) \`configure --id $mission\` with no setting changed the mission from $before to $(row)"

# --- 007 (2): spawn task stages ends `cancelled` ----------------------------------------
# 007's setup whole, on database A; only its check (2) is carried. It reads
# mission_tasks only to find a task's session and agent, as 007 did.
part=007
spawn_ok() {
    capture "$SPAWN_BIN" "$@"
    [ "$status" -eq 0 ] || harness "spawn $*: exit $status: $(err)"
}
assign() {
    spawn_ok task assign --mission "$1" --kind "$2" --worker "$3" --brief "work of $3"
    task=$(jq -r .task "$WORK/out.txt")
    [ -n "$task" ] && [ "$task" != null ] || harness "task assign printed no id: $(out)"
    echo "$task"
}
deliver_and_report() {
    session=$(q "select session_id from mission_tasks where id = $1")
    agent=$(q "select agent_id from mission_tasks where id = $1")
    spawn_ok submit --principal bb-harness --agent "$agent" --session "$session" \
        --key "bb-deliver-$1" --action '{"type":"input","text":"the task"}'
    command=$(q "select id from spawn_commands where idempotency_key = 'bb-deliver-$1'")
    [ "$(q "select spawn_task_delivered($1, $command)")" = t ] || harness "task $1 not delivered"
    [ "$(q "select spawn_task_reported($1, 'done')")" = t ] || harness "task $1 not reported"
}
last_stage() {
    capture "$SPAWN_BIN" task stages --id "$1"
    [ "$status" -eq 0 ] || fail "spawn task stages --id $1 exited $status: $(err)"
    stage=$(jq -r 'if type == "array" and length > 0 then .[-1].stage else "none" end' \
        "$WORK/out.txt") || fail "spawn task stages --id $1 printed no JSON array: $(out)"
}

spawn_ok mission add --text "007 resident work"
resident_mission=$(jq -r .mission "$WORK/out.txt")
spawn_ok mission add --text "007 dropped mission"
dropped_mission=$(jq -r .mission "$WORK/out.txt")
spawn_ok resident propose --name core --charter "Owns the 007 concern."
spawn_ok resident approve --name core

resident_assigned=$(assign "$resident_mission" resident core)
resident_reported=$(assign "$resident_mission" resident core)
resident_accepted=$(assign "$resident_mission" resident core)
deliver_and_report "$resident_reported"
deliver_and_report "$resident_accepted"
spawn_ok task accept --id "$resident_accepted"

dropped_assigned=$(assign "$dropped_mission" ephemeral a)
dropped_reported=$(assign "$dropped_mission" ephemeral b)
deliver_and_report "$dropped_reported"

spawn_ok resident retire --name core
spawn_ok mission close --id "$dropped_mission" --state dropped

cancelled="$resident_assigned $resident_reported $dropped_assigned $dropped_reported"

echo "j05: [007 (2)] the last stage of each cancelled task"
for task in $cancelled; do
    last_stage "$task"
    assert_eq "$stage" cancelled "(2) the last stage of task $task after the retirement and the drop"
done

# ================================================================================
# Database B: blackbox 080
# ================================================================================
database b

# --- blackbox 080: the command gateway and grants ------------------------------------
part=bb080
echo "j05: [bb080 No grant at all]"
capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"
assert_contains "$(out)" "schema installed" "init-db output"

me=$(scalar 'select session_user')
[ -n "$me" ] || fail "could not read session_user"

assert_eq "$(scalar "select count(*) from spawn_grants where principal <> 'spawn_supervisor'")" 0 "spawn_grants starts with no grant but the supervisor's"

capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into agent_commands (agent_id, session_id, idempotency_key, action) values ('agent_x', 'session_x', 'gw_1', '{\"type\":\"stop\"}'::jsonb)"
[ "$status" -ne 0 ] || fail "an insert into agent_commands without a grant was accepted"
assert_contains "$(err)" "has no grant for agent agent_x" "the gateway names the agent it refused"
assert_contains "$(err)" "principal $me" "the gateway names the principal it refused"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 0 "a refused insert queues no command"
assert_eq "$(scalar 'select count(*) from spawn_events')" 0 "a refused insert records no event"

echo "j05: [bb080 The CLI path bypasses the gateway]"
capture "$SPAWN_BIN" submit --agent agent_cli --session session_cli --key cli_1 --action '{"type":"stop"}'
assert_eq "$status" 0 "submit succeeds with zero grant rows"
assert_contains "$(out)" "command=" "submit prints the command id"
cli_line=$(out)
cli_id=${cli_line#command=}
assert_eq "$cli_id" "$(scalar 'select max(id) from spawn_commands')" "submit returns the queued id"
assert_eq "$(scalar "select principal from spawn_commands where id = $cli_id")" "spawn" "submit uses the default principal"
assert_eq "$(scalar "select agent_id from spawn_commands where id = $cli_id")" "agent_cli" "submit queues the requested agent"
assert_eq "$(scalar "select session_id from spawn_commands where id = $cli_id")" "session_cli" "submit queues the requested session"
assert_eq "$(scalar "select count(*) from spawn_grants where principal <> 'spawn_supervisor'")" 0 "submit needed no grant row"
assert_eq "$(scalar "select count(*) from spawn_events where event_name = 'command.accepted'")" 1 "submit records the accepted event"

echo "j05: [bb080 A grant for one agent only]"
capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into spawn_grants (principal, agent_id) values (session_user, 'agent_x')"
assert_eq "$status" 0 "inserting a grant row succeeds"
assert_eq "$(scalar "select count(*) from spawn_grants where principal <> 'spawn_supervisor'")" 1 "one grant row exists"
assert_eq "$(scalar "select agent_id from spawn_grants where principal = '$me'")" "agent_x" "the grant names agent_x"

capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into agent_commands (agent_id, session_id, idempotency_key, action) values ('agent_y', 'session_y', 'gw_y', '{\"type\":\"input\",\"text\":\"hi\"}'::jsonb)"
[ "$status" -ne 0 ] || fail "a grant for agent_x authorized agent_y"
assert_contains "$(err)" "has no grant for agent agent_y" "the gateway refuses the unlisted agent"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 1 "the refused agent_y insert queues nothing"

capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into agent_commands (agent_id, session_id, idempotency_key, action) values ('agent_x', 'session_x', 'gw_2', '{\"type\":\"stop\"}'::jsonb) returning id"
assert_eq "$status" 0 "the granted insert is accepted"
gw_id=$(out | head -1)
[ -n "$gw_id" ] || fail "the granted insert returned no id"
assert_eq "$gw_id" "$(scalar 'select max(id) from spawn_commands')" "the trigger returns the queued command id"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 2 "the granted insert lands in spawn_commands"
assert_eq "$(scalar "select agent_id from spawn_commands where id = $gw_id")" "agent_x" "the queued row carries the agent"
assert_eq "$(scalar "select session_id from spawn_commands where id = $gw_id")" "session_x" "the queued row carries the session"
assert_eq "$(scalar "select idempotency_key from spawn_commands where id = $gw_id")" "gw_2" "the queued row carries the idempotency key"
assert_eq "$(scalar "select principal from spawn_commands where id = $gw_id")" "$me" "the trigger uses session_user as the principal"
assert_eq "$(scalar "select state from spawn_commands where id = $gw_id")" "pending" "the queued command starts pending"
assert_eq "$(scalar "select action ->> 'type' from spawn_commands where id = $gw_id")" "stop" "the queued row carries the action"
assert_eq "$(scalar "select count(*) from spawn_events where source_event_id = 'cmd_${gw_id}:accepted'")" 1 "the gateway records the accepted event"

capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into agent_commands (agent_id, session_id, idempotency_key, action) values ('agent_x', 'session_x', 'gw_2', '{\"type\":\"stop\"}'::jsonb) returning id"
assert_eq "$status" 0 "a repeated granted insert is accepted"
assert_eq "$(out | head -1)" "$gw_id" "a repeated key returns the first command id"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 2 "a repeated key queues no second command"

echo "j05: [bb080 Grants still do not gate the CLI]"
capture "$SPAWN_BIN" submit --agent agent_cli2 --session session_cli2 --key cli_2 --action '{"type":"stop"}'
assert_eq "$status" 0 "submit succeeds for an agent with no grant"
assert_eq "$(scalar "select count(*) from spawn_grants where principal = '$me' and agent_id = 'agent_cli2'")" 0 "no grant was added for the CLI agent"
assert_eq "$(scalar "select count(*) from spawn_commands where agent_id = 'agent_cli2'")" 1 "the CLI submit queued its command"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 3 "only the CLI command was added"

echo "j05: [bb080 A wildcard grant authorizes any agent]"
capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into spawn_grants (principal, agent_id) values (session_user, '*')"
assert_eq "$status" 0 "inserting a wildcard grant succeeds"
capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into agent_commands (agent_id, session_id, idempotency_key, action) values ('agent_z', 'session_z', 'gw_3', '{\"type\":\"stop\"}'::jsonb) returning id"
assert_eq "$status" 0 "a wildcard grant authorizes an agent with no exact grant"
star_id=$(out | head -1)
assert_eq "$star_id" "$(scalar 'select max(id) from spawn_commands')" "the wildcard insert returns the queued id"
assert_eq "$(scalar "select agent_id from spawn_commands where id = $star_id")" "agent_z" "the wildcard insert lands in spawn_commands"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 4 "the wildcard insert added one command"

# ================================================================================
# Database C: blackbox 030
# ================================================================================
database c

# --- blackbox 030: spawn tick and the workspace store ----------------------------------
part=bb030
echo "j05: [bb030 tick] init-db"
capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"
assert_contains "$(out)" "schema installed" "init-db output"

tick_count() {
    out | tr ' ' '\n' | sed -n "s/^$1=//p"
}

echo "j05: [bb030 tick] an unknown engine, codex, and a tick without a repository"
capture "$SPAWN_BIN" submit --agent agent_ws --session session_ws --key k_bad \
    --action '{"type":"start","engine":"nope","prompt":"x"}'
assert_eq "$status" 1 "unknown engine exit"
assert_contains "$(err)" "--action is not a command action" "unknown engine message"

capture "$SPAWN_BIN" submit --agent agent_ws --session session_ws --key k_codex \
    --action '{"type":"start","engine":"codex","prompt":"x"}'
assert_eq "$status" 1 "codex engine exit"
assert_contains "$(err)" "unknown engine codex" "codex engine message"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 0 "no command was written"

capture sh -c 'unset SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT; exec "$0" tick' "$SPAWN_BIN"
assert_eq "$status" 1 "tick without repo or root exit"
assert_contains "$(err)" "--repo or SPAWN_GIT_REPO is required" "tick missing repo message"

echo "j05: [bb030 tick] one start command, one tick: the workspace and the pane"
capture "$SPAWN_BIN" submit --agent agent_ws --session session_ws --key k1 \
    --action '{"type":"start","engine":"claude","prompt":"probe workspace 030"}'
assert_eq "$status" 0 "submit exit"
assert_contains "$(out)" "command=" "submit output"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 1 "one command was written"

workspace="$SPAWN_WORKSPACE_ROOT/agent_ws"

capture "$SPAWN_BIN" tick
assert_eq "$status" 0 "first tick exit"
assert_eq "$(tick_count commands)" 1 "first tick handles one command"
assert_eq "$(tick_count sessions)" 1 "first tick counts the submitted session"

assert_file "$workspace/README.md"
assert_eq "$(cat "$workspace/README.md")" "base" "workspace holds the base content"
assert_no_file "$workspace/.git"

pane_shows_prompt() {
    pane | grep -F "echo: probe workspace 030" >/dev/null
}
wait_for 60 pane_shows_prompt || fail "prompt text never reached the pane: $(pane 2>&1)"

assert_eq "$(scalar 'select count(*) from agent_executions')" 1 "one execution row"
assert_eq "$(scalar "select state from spawn_commands where idempotency_key = 'k1'")" \
    "finished" "command is finished"
assert_eq "$(scalar "select succeeded from spawn_commands where idempotency_key = 'k1'")" \
    "t" "command succeeded"

echo "j05: [bb030 tick] a second tick starts no second execution"
capture "$SPAWN_BIN" tick
assert_eq "$status" 0 "second tick exit"
assert_eq "$(tick_count commands)" 0 "second tick handles no command"
assert_eq "$(tick_count sessions)" 1 "second tick still counts the session"
assert_eq "$(scalar 'select count(*) from agent_executions')" 1 \
    "second tick starts no second execution"
assert_eq "$(tmux -L "$SPAWN_TMUX_SOCKET" list-sessions -F '#{session_name}' | wc -l | tr -d ' ')" \
    1 "one terminal for one execution"

assert_file "$workspace/README.md"
assert_eq "$(cat "$workspace/README.md")" "base" "workspace still holds the base content"
# 030's terminal is done with: 100's tick below shares the socket.
tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true

# ================================================================================
# Database D: blackbox 100, then 017 (4) on the same loopback server
# ================================================================================
database d

# --- blackbox 100: the read-only HTTP API -------------------------------------------
part=bb100
echo "j05: [bb100 serve] an empty store"
capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"

bound_port() { # <log>: the port from serve's first line, bounded wait
    i=0
    while [ "$i" -lt 50 ]; do
        i=$((i + 1))
        p=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$1" 2>/dev/null | head -n 1)
        [ -n "$p" ] && { echo "$p"; return 0; }
        sleep 0.1
    done
    return 1
}
web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"

"$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" \
    >"$WORK/serve.log" 2>&1 &
serve_pid=$!
port=$(bound_port "$WORK/serve.log") || fail "serve did not start: $(cat "$WORK/serve.log")"

ready=0
i=0
while [ "$i" -lt 50 ]; do
    i=$((i + 1))
    if curl -fsS "http://127.0.0.1:$port/api/overview" >"$WORK/overview.json" 2>/dev/null; then
        ready=1
        break
    fi
    sleep 0.1
done
[ "$ready" -eq 1 ] || fail "serve did not answer on port $port"

assert_contains "$(cat "$WORK/overview.json")" '"sessions":[]' \
    "overview lists no session on an empty store"

code=$(curl -s -o "$WORK/session.json" -w '%{http_code}' \
    "http://127.0.0.1:$port/api/session/session_absent")
assert_eq "$code" 200 "session endpoint status for an unknown session"
assert_contains "$(cat "$WORK/session.json")" '"session_id":"session_absent"' \
    "session endpoint echoes the requested id"
assert_contains "$(cat "$WORK/session.json")" '"timeline":[]' \
    "unknown session has an empty timeline"

echo "j05: [bb100 serve] one real event, then every read leaves the store alone"
capture "$SPAWN_BIN" submit --agent agent_serve --session session_serve \
    --key serve-1 --action '{"type":"stop"}'
assert_eq "$status" 0 "submit exit"
capture "$SPAWN_BIN" tick
assert_eq "$status" 0 "tick exit"

before=$(scalar 'select count(*) from spawn_events')

curl -fsS "http://127.0.0.1:$port/api/overview" >"$WORK/overview.json" 2>/dev/null
assert_contains "$(cat "$WORK/overview.json")" '"session_id":"session_serve"' \
    "overview shows the judged session"

curl -fsS "http://127.0.0.1:$port/api/session/session_serve" >"$WORK/session.json" 2>/dev/null
assert_contains "$(cat "$WORK/session.json")" '"event_name":"command.accepted"' \
    "session endpoint shows the timeline event"
assert_contains "$(cat "$WORK/session.json")" '"event_count":' \
    "session endpoint reports freshness"

code=$(curl -s -o "$WORK/index.html" -w '%{http_code}' "http://127.0.0.1:$port/")
assert_eq "$code" 200 "index status"
assert_contains "$(cat "$WORK/index.html")" "spawn" "index served from the web directory"

code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/../Cargo.toml")
[ "$code" = 404 ] || fail "path traversal returned $code, expected 404"

echo "j05: [bb100 off loopback: no token, no server]"
capture env -u SPAWN_SERVE_TOKEN SPAWN_SERVE_ADDRESS="0.0.0.0:0" \
    timeout 5 "$SPAWN_BIN" serve --web "$web"
assert_eq "$status" 1 "serve on 0.0.0.0 without a token exit"
assert_contains "$(err)" "without SPAWN_SERVE_TOKEN" "the refusal names the token"

echo "j05: [bb100 with a token, every API request carries it]"
token=bb-serve-token-$$
SPAWN_SERVE_TOKEN=$token "$SPAWN_BIN" serve --address "0.0.0.0:0" \
    --web "$web" >"$WORK/guarded.log" 2>&1 &
guarded_pid=$!
guarded_port=$(bound_port "$WORK/guarded.log") ||
    fail "guarded serve did not start: $(cat "$WORK/guarded.log")"
guarded="http://127.0.0.1:$guarded_port"
wait_for 40 curl -fsS -H "Authorization: Bearer $token" "$guarded/api/overview" ||
    fail "guarded serve did not answer on port $guarded_port: $(cat "$WORK/guarded.log")"

code=$(curl -s -o "$WORK/denied.json" -w '%{http_code}' "$guarded/api/overview")
assert_eq "$code" 401 "API request without a token"
assert_contains "$(cat "$WORK/denied.json")" '"error":"unauthorized"' "401 body"
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer wrong-$token" \
    "$guarded/api/overview")
assert_eq "$code" 401 "API request with a wrong bearer token"
code=$(curl -s -o /dev/null -w '%{http_code}' "$guarded/api/pane/session_serve?token=wrong")
assert_eq "$code" 401 "pane request with a wrong ?token="
code=$(curl -s -o /dev/null -w '%{http_code}' "$guarded/api/pane/session_serve")
assert_eq "$code" 401 "pane request without a token"

code=$(curl -s -o "$WORK/overview.json" -w '%{http_code}' \
    -H "Authorization: Bearer $token" "$guarded/api/overview")
assert_eq "$code" 200 "API request with the bearer token"
assert_contains "$(cat "$WORK/overview.json")" '"session_id":"session_serve"' \
    "the guarded overview shows the session"
code=$(curl -s -o "$WORK/session.json" -w '%{http_code}' \
    "$guarded/api/session/session_serve?after=0&token=$token")
assert_eq "$code" 200 "API request with ?token="
assert_contains "$(cat "$WORK/session.json")" '"event_name":"command.accepted"' \
    "?token= sits beside the endpoint's own parameters"

code=$(curl -s -o /dev/null -w '%{http_code}' "$guarded/")
assert_eq "$code" 200 "static index without a token"

assert_eq "$(scalar 'select count(*) from spawn_events')" "$before" \
    "serving appended no event"
kill "$guarded_pid" 2>/dev/null || true
wait "$guarded_pid" 2>/dev/null || true
guarded_pid=""

# --- 017 (4): /api/scorecard answers what spawn scorecard --json answers -----------------
# 017's history and transcripts, unchanged, on database D after 100 (whose
# rows are of today, outside the October window); 100's loopback server,
# started with this database and workspace root, answers the API.
part=017
cat >"$WORK/seed.sql" <<'SQL'
\set ON_ERROR_STOP 1
begin;
-- Hours from T0 = 2026-10-02T00:00:00Z, in ms.
create function pg_temp.ms(h numeric) returns bigint language sql as $$
    select (extract(epoch from timestamptz '2026-10-02 00:00:00+00') * 1000)::bigint
           + round(h * 3600000)::bigint $$;
create temp sequence seed_seq;
-- After any control record already here (database D's serving and grants may
-- have written some of today's, outside the October window).
select setval('seed_seq', coalesce(max(session_seq), 0) + 1, false)
  from spawn_events where session_id = 'session_control';
-- One control record at hour h, in the order written.
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as $$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), pg_temp.ms(h)) $$;
-- One finished command: (principal, key, agent, session, action, succeeded,
-- error, created at hour h, finished one second later).
create function pg_temp.cmd(p text, k text, a text, s text, act jsonb, ok boolean, e text,
                            h numeric) returns void language sql as $$
    insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action, state,
                                succeeded, error, created_at_ms, finished_at_ms)
    values (p, k, a, s, act, 'finished', ok, e, pg_temp.ms(h), pg_temp.ms(h) + 1000) $$;

create function pg_temp.assign(h numeric, id int, m int, kind text, agent text, session text,
                               objective int, by_ text) returns void language sql as $$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.assigned', 'id', id, 'mission_id', m,
        'kind', kind, 'agent_id', agent, 'session_id', session,
        'brief', coalesce('Objective ' || objective, 'Task ' || id), 'created_by', by_,
        'objective', objective, 'model', 'default')) $$;
create function pg_temp.gated(h numeric, id int, verdict text, gate_ms bigint) returns void
language sql as $$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.gated', 'id', id, 'verdict', verdict,
        'base', 'b' || id, 'candidate', 'c' || id, 'reason', 'r', 'note', 'fix it',
        'log', '| == candidate: build' || chr(10) || '| ok' || chr(10)
               || case when gate_ms is null then '! the gate gave no verdict'
                       else '| stage gate ' || verdict || ' ' || gate_ms || 'ms total' end)) $$;
create function pg_temp.simple(h numeric, type_ text, id int) returns void language sql as $$
    select pg_temp.rec(h, jsonb_build_object('type', type_, 'id', id)) $$;

select pg_temp.rec(-47, '{"type":"mission.added","id":1,"text":"017 gated","origin":"human","sources":[]}');
select pg_temp.rec(-47, '{"type":"mission.configured","id":1,"gate":"/nonexistent/017","accept":"gate","budget":null,"objectives":null}');
select pg_temp.rec(-47, '{"type":"mission.added","id":2,"text":"017 review","origin":"human","sources":[]}');
select pg_temp.rec(-47, '{"type":"resident.proposed","name":"r1","charter":"c","proposed_by":"human"}');
select pg_temp.rec(-47, '{"type":"resident.approved","name":"r1"}');

-- The previous window.
select pg_temp.assign(-40, 1, 1, 'resident', 'agent_resident_r1', 'session_resident_r1', 1, 'spawn-integrate');
select pg_temp.rec(-40, '{"type":"task.delivered","id":1,"command_id":1}');
select pg_temp.rec(-38, '{"type":"task.reported","id":1,"report":"done","commit":null}');
select pg_temp.gated(-37, 1, 'accepted', 900000);
select pg_temp.rec(-37, '{"type":"task.landed","id":1,"from":"m0","to":"m1"}');
select pg_temp.assign(-30, 2, 1, 'resident', 'agent_resident_r1', 'session_resident_r1', 2, 'spawn-integrate');
select pg_temp.rec(-30, '{"type":"task.delivered","id":2,"command_id":2}');
select pg_temp.rec(-29, '{"type":"task.reported","id":2,"report":"done","commit":null}');
select pg_temp.gated(-28, 2, 'rejected', 300000);
select pg_temp.rec(-27.5, '{"type":"task.delivered","id":2,"command_id":3}');
select pg_temp.rec(-27, '{"type":"task.reported","id":2,"report":"done","commit":null}');
select pg_temp.gated(-26, 2, 'accepted', 900000);
select pg_temp.rec(-26, '{"type":"task.landed","id":2,"from":"m1","to":"m2"}');
select pg_temp.assign(-20, 3, 1, 'resident', 'agent_resident_r1', 'session_resident_r1', 3, 'spawn-integrate');
select pg_temp.rec(-20, '{"type":"task.delivered","id":3,"command_id":4}');
select pg_temp.rec(-16, '{"type":"task.reported","id":3,"report":"done","commit":null}');
select pg_temp.gated(-15, 3, 'accepted', 1500000);
select pg_temp.rec(-15, '{"type":"task.landed","id":3,"from":"m2","to":"m3"}');

-- The window.
select pg_temp.assign(1, 4, 1, 'resident', 'agent_resident_r1', 'session_resident_r1', 11, 'spawn-integrate');
select pg_temp.rec(1, '{"type":"task.delivered","id":4,"command_id":5}');
select pg_temp.assign(2, 5, 1, 'ephemeral', 'agent_worker_b_5', 'session_worker_b_5', 12, 'spawn-integrate');
select pg_temp.rec(2, '{"type":"task.delivered","id":5,"command_id":6}');
select pg_temp.rec(3, '{"type":"task.reported","id":4,"report":"done","commit":null}');
select pg_temp.gated(4, 4, 'rejected', 300000);
select pg_temp.rec(4.5, '{"type":"task.delivered","id":4,"command_id":7}');
select pg_temp.rec(5, '{"type":"task.reported","id":5,"report":"done","commit":null}');
select pg_temp.rec(6, '{"type":"task.reported","id":4,"report":"done","commit":null}');
select pg_temp.gated(7, 4, 'accepted', 600000);
select pg_temp.rec(7, '{"type":"task.landed","id":4,"from":"m3","to":"m4"}');
select pg_temp.gated(8, 5, 'conflict', null);
select pg_temp.rec(8.5, '{"type":"task.delivered","id":5,"command_id":8}');
select pg_temp.rec(9, '{"type":"task.reported","id":5,"report":"done","commit":null}');
select pg_temp.gated(10, 5, 'accepted', 900000);
select pg_temp.rec(10, '{"type":"task.landed","id":5,"from":"m4","to":"m5"}');
select pg_temp.assign(12, 10, 2, 'ephemeral', 'agent_worker_g_10', 'session_worker_g_10', null, 'human');
select pg_temp.rec(12, '{"type":"task.delivered","id":10,"command_id":9}');
select pg_temp.rec(14, '{"type":"task.reported","id":10,"report":"done","commit":null}');
select pg_temp.rec(15, '{"type":"task.reviewed","id":10,"accepted":false,"note":"redo it"}');
select pg_temp.rec(15, '{"type":"task.delivered","id":10,"command_id":10}');
select pg_temp.rec(16, '{"type":"task.reported","id":10,"report":"done","commit":null}');
select pg_temp.rec(17, '{"type":"task.reviewed","id":10,"accepted":true,"note":null}');
select pg_temp.assign(18, 11, 2, 'ephemeral', 'agent_worker_h_11', 'session_worker_h_11', null, 'spawn_supervisor');
select pg_temp.rec(18, '{"type":"task.delivered","id":11,"command_id":11}');
select pg_temp.rec(19, '{"type":"task.reported","id":11,"report":"done","commit":null}');
select pg_temp.rec(20, '{"type":"task.reviewed","id":11,"accepted":true,"note":null}');
select pg_temp.assign(20, 6, 1, 'ephemeral', 'agent_worker_c_6', 'session_worker_c_6', 13, 'spawn-integrate');
select pg_temp.simple(24, 'task.cancelled', 6);
select pg_temp.assign(24, 7, 1, 'ephemeral', 'agent_worker_c_7', 'session_worker_c_7', 13, 'spawn-integrate');
select pg_temp.rec(24, '{"type":"task.delivered","id":7,"command_id":12}');
select pg_temp.rec(26, '{"type":"task.reported","id":7,"report":"done","commit":null}');
select pg_temp.gated(27, 7, 'accepted', 600000);
select pg_temp.rec(27, '{"type":"task.landed","id":7,"from":"m5","to":"m6"}');
select pg_temp.assign(30, 8, 1, 'resident', 'agent_resident_r1', 'session_resident_r1', 14, 'spawn-integrate');
select pg_temp.rec(30.01, '{"type":"task.delivered","id":8,"command_id":13}');
select pg_temp.rec(31, '{"type":"task.reported","id":8,"report":"done","commit":null}');
select pg_temp.gated(32, 8, 'aborted', null);
select pg_temp.rec(32.2, '{"type":"mission.halted","id":1,"reason":"3 gate aborts in a row"}');
select pg_temp.rec(32.5, '{"type":"mission.resumed","id":1}');
select pg_temp.rec(33, '{"type":"task.gated","id":8,"verdict":"no_room","free_kb":1,"min_gb":10,"tmpdir":"/t","reason":"no room to judge now","log":"","escalate":false}');
select pg_temp.gated(34, 8, 'accepted', 1200000);
select pg_temp.rec(34, '{"type":"task.landed","id":8,"from":"m6","to":"m7"}');
select pg_temp.assign(40, 9, 1, 'resident', 'agent_resident_r1', 'session_resident_r1', 15, 'spawn-integrate');
select pg_temp.rec(40, '{"type":"task.delivered","id":9,"command_id":14}');
select pg_temp.assign(46, 12, 2, 'ephemeral', 'agent_worker_i_12', 'session_worker_i_12', null, 'human');

select spawn_control_refold();

-- The commands.
select pg_temp.cmd('spawn-dispatch', 'deliver-1-0', 'agent_resident_r1', 'session_resident_r1',
                   '{"type":"input","text":"Task #1"}', true, null, -40);
select pg_temp.cmd('spawn-dispatch', 'deliver-4-0', 'agent_resident_r1', 'session_resident_r1',
                   '{"type":"input","text":"Task #4"}', true, null, 1);
select pg_temp.cmd('human', 'board-1', 'agent_resident_r1', 'session_resident_r1',
                   '{"type":"input","text":"a note from the board"}', true, null, 5);
select pg_temp.cmd('spawn-dispatch', 'deliver-6-0', 'agent_worker_c_6', 'session_worker_c_6',
                   '{"type":"start","engine":"claude","prompt":"Task #6"}', false,
                   'start terminal: new-session for spawn-exec_1 failed: command too long', 20 + 10 / 3600.0);
select pg_temp.cmd('spawn-dispatch', 'deliver-6-0-retry-1', 'agent_worker_c_6', 'session_worker_c_6',
                   '{"type":"start","engine":"claude","prompt":"Task #6"}', false,
                   'start terminal: new-session for spawn-exec_2 failed: command too long', 20 + 80 / 3600.0);
select pg_temp.cmd('spawn-dispatch', 'deliver-6-0-retry-2', 'agent_worker_c_6', 'session_worker_c_6',
                   '{"type":"start","engine":"claude","prompt":"Task #6"}', false,
                   'start terminal: new-session for spawn-exec_3 failed: command too long', 20 + 210 / 3600.0);
select pg_temp.cmd('spawn-dispatch', 'deliver-6-0-retry-3', 'agent_worker_c_6', 'session_worker_c_6',
                   '{"type":"start","engine":"claude","prompt":"Task #6"}', false,
                   'start terminal: new-session for spawn-exec_4 failed: command too long', 20 + 460 / 3600.0);
select pg_temp.cmd('spawn', 'cli-stop-1', 'agent_worker_c_6', 'session_worker_c_6',
                   '{"type":"stop"}', false, 'session_worker_c_6 runs no execution', 21);
select pg_temp.cmd('human', 'board-2', 'agent_supervisor', 'session_supervisor',
                   '{"type":"input","text":"how is mission 1 doing?"}', true, null, 22);
select pg_temp.cmd('spawn-dispatch', 'deliver-7-0', 'agent_worker_c_7', 'session_worker_c_7',
                   '{"type":"start","engine":"claude","prompt":"Task #7"}', true, null, 24);
select pg_temp.cmd('spawn-dispatch', 'deliver-8-0', 'agent_resident_r1', 'session_resident_r1',
                   '{"type":"input","text":"Task #8"}', false, 'pane state Busy does not accept input',
                   30 + 5 / 3600.0);
select pg_temp.cmd('spawn-dispatch', 'deliver-8-0-retry-1', 'agent_resident_r1', 'session_resident_r1',
                   '{"type":"input","text":"Task #8"}', true, null, 30 + 20 / 3600.0);
select pg_temp.cmd('spawn-dispatch', 'dispatch-digest-1-a', 'agent_supervisor', 'session_supervisor',
                   '{"type":"input","text":"stalled: task #9","tags":["session_resident_r1 stall-9-activity-1"]}',
                   true, null, 45);
select pg_temp.cmd('spawn-dispatch', 'dispatch-digest-2-b', 'agent_supervisor', 'session_supervisor',
                   '{"type":"input","text":"stalled: task #9","tags":["session_resident_r1 stall-9-activity-1","session_worker_i_12 report-12-0"]}',
                   true, null, 45.5);
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
[ "$(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")" = \
    "1:landed 2:landed 3:landed 4:landed 5:landed 6:cancelled 7:landed 8:landed 9:delivered 10:accepted 11:accepted 12:assigned" ] ||
    harness "the seeded history folds to unexpected tasks: $(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")"

tr_dir=$SPAWN_WORKSPACE_ROOT/.transcripts
msg() { # <iso time> <message id> <model> <input> <cache creation> <cache read> <output>
    printf '{"type":"assistant","timestamp":"%s","sessionId":"s","message":{"id":"%s","model":"%s","role":"assistant","usage":{"input_tokens":%s,"cache_creation_input_tokens":%s,"cache_read_input_tokens":%s,"output_tokens":%s}}}\n' \
        "$@"
}
user_line() { printf '{"type":"user","timestamp":"%s","sessionId":"s","message":{"role":"user","content":"go"}}\n' "$1"; }
mkdir -p "$tr_dir/agent_resident_r1" "$tr_dir/agent_worker_b_5" "$tr_dir/agent_worker_c_7" \
    "$tr_dir/agent_worker_g_10" "$tr_dir/agent_worker_h_11" "$tr_dir/agent_supervisor"
native=0a0a0a0a-0000-4000-8000-000000000001
{
    msg 2026-09-30T09:00:00.000Z msg_p1 claude-opus-5-5 0 0 0 3000
    user_line 2026-10-02T01:30:00.000Z
    msg 2026-10-02T02:00:00.000Z msg_a1 claude-opus-5-5 100 1000 5000 400
    msg 2026-10-02T02:00:01.000Z msg_a1 claude-opus-5-5 100 1000 5000 400
    msg 2026-10-03T07:00:00.000Z msg_e1 claude-sonnet-5-5 10 0 2000 90
    msg 2026-10-03T14:00:00.000Z msg_x1 claude-opus-5-5 0 0 0 1000
} >"$tr_dir/agent_resident_r1/$native.jsonl"
{
    msg 2026-10-03T07:00:00.000Z msg_e1 claude-sonnet-5-5 10 0 2000 90
    msg 2026-10-03T17:00:00.000Z msg_f1 claude-opus-5-5 50 0 1000 50
} >"$tr_dir/agent_resident_r1/$native.1.jsonl"
msg 2026-10-02T03:00:00.000Z msg_b1 claude-opus-5-5 200 0 3000 300 \
    >"$tr_dir/agent_worker_b_5/0a0a0a0a-0000-4000-8000-000000000002.jsonl"
msg 2026-10-03T01:00:00.000Z msg_d1 claude-sonnet-5-5 20 500 1000 80 \
    >"$tr_dir/agent_worker_c_7/0a0a0a0a-0000-4000-8000-000000000003.jsonl"
msg 2026-10-02T13:00:00.000Z msg_g1 claude-sonnet-5-5 30 0 0 70 \
    >"$tr_dir/agent_worker_g_10/0a0a0a0a-0000-4000-8000-000000000004.jsonl"
msg 2026-10-02T19:00:00.000Z msg_h1 claude-haiku-5-5 5 0 0 15 \
    >"$tr_dir/agent_worker_h_11/0a0a0a0a-0000-4000-8000-000000000005.jsonl"
{
    msg 2026-10-02T22:00:00.000Z msg_s1 claude-opus-5-5 10 0 0 90
    printf '{"type":"assistant","timestamp":"2026-10-02T22:00:02.000Z","sessionId":"s","message":{"id":"msg_s2","model":"<synthetic>","role":"assistant","content":[]}}\n'
    msg 2026-10-04T02:00:00.000Z msg_s3 claude-opus-5-5 0 0 0 99999
} >"$tr_dir/agent_supervisor/0a0a0a0a-0000-4000-8000-000000000006.jsonl"

echo "j05: [017 (1)] the scorecard answers (the CLI side of (4))"
SINCE=2026-10-02T00:00:00Z
UNTIL=2026-10-04T00:00:00Z
capture "$SPAWN_BIN" scorecard --since "$SINCE" --until "$UNTIL" --json
cp "$WORK/out.txt" "$WORK/card.json"
[ "$status" -eq 0 ] ||
    fail "(1) \`spawn scorecard --since $SINCE --until $UNTIL --json\` exited $status: $(err | head -c 400)"
jq -e . "$WORK/card.json" >/dev/null 2>&1 || fail "(1) the scorecard's answer is not JSON: $(head -c 400 "$WORK/card.json")"
card=$WORK/card.json

echo "j05: [017 (4)] the dashboard's Scorecard reads the same numbers"
code=""
i=0
while [ "$i" -lt 50 ]; do
    i=$((i + 1))
    code=$(curl -s -o "$WORK/api.json" -w '%{http_code}' \
        "http://127.0.0.1:$port/api/scorecard?since=$SINCE&until=$UNTIL" 2>/dev/null) || code=""
    [ -n "$code" ] && [ "$code" != 000 ] && break
    sleep 0.1
done
[ "$code" = 200 ] || fail "(4) GET /api/scorecard answered $code: $(head -c 300 "$WORK/api.json" 2>/dev/null)"
jq -S . "$WORK/api.json" >"$WORK/api.sorted" 2>/dev/null ||
    fail "(4) /api/scorecard is not JSON: $(head -c 300 "$WORK/api.json")"
jq -S . "$card" >"$WORK/card.sorted"
cmp -s "$WORK/api.sorted" "$WORK/card.sorted" ||
    fail "(4) /api/scorecard differs from \`spawn scorecard --json\` for the same window: $(diff "$WORK/card.sorted" "$WORK/api.sorted" | head -n 12 | tr '\n' ' ')"

echo "j05: the operator's CLI and API answered as blackbox 010, 020, 030, 060, 080, 100, 001, 027, 007 (2) and 017 (4) require"
exit 0
