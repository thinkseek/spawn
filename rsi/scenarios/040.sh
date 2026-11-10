#!/bin/sh
# Black box: a person talks to the supervisor from the page. The supervisor
# page's input box sends a message as `spawn submit --principal human` does:
# the same command, recorded as the person's; the supervisor's pane gets it
# once; the trace on that page shows it as `You → supervisor` and the
# supervisor's answer after it; and a stopped supervisor refuses it with the
# reason, recording nothing.
#
# A real `spawn loop` (the Bridge, the Observer with no model judge, the
# supervisor kept alive, no dispatcher) runs the supervisor on a fake engine
# that answers each prompt (`answer: <its first line>`) after a second of
# quiet. `spawn serve` answers the page's API on a loopback port; the
# scenario posts as the page does (its own Origin, the owner's own client,
# no token). Checked:
#   (1) the page's path: `POST /api/session/session_supervisor/input` with
#       a text and an idempotency key answers 202 with the command, principal
#       `human`; the command is the one `spawn submit --principal human`
#       makes (principal `human`, agent `agent_supervisor`, action exactly
#       `{"type":"input","text":<the text>}`, as a CLI-submitted one beside
#       it is); it finishes succeeded; the engine reads the text exactly
#       once and answers it; the same key again returns the same command and
#       types nothing more.
#   (2) the trace: the page's trace of the supervisor's session, derived
#       by the UI's own web/src/trace.ts (`deriveTrace`, given the session's
#       timeline and its commands as `GET /api/session/<id>` and
#       `GET /api/fleet/<id>/commands` serve them, and the agent's name,
#       `supervisor`), has a command span labelled `You → supervisor` for
#       each of the two inputs, whose summary is its text, and after each a
#       turn span (the supervisor's answer, in the same view). trace.ts
#       and its imports must load with node alone; one that does not fails
#       here.
#   (3) refused when stopped: once a person stops the supervisor, the
#       page's input answers 409 with a reason that says it runs no
#       execution, and records no command; one sent with `spawn submit
#       --principal human` (the CLI's path) is recorded and finishes failed,
#       saying the same; the supervisor is not started again.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (2) on the live main (a484b2c7): the trace labels a
# person's input `You → input` (web/src/commands.ts `commandHead` names the
# action, not the agent). It passes once `commandHead(principal, action,
# to)` names the agent `to` for an input and `deriveTrace(events, now,
# executions, commands, to)` passes the session's agent name to it, and the
# page's input to a session that runs no execution is refused 409 with the
# reason before any command is made.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh), from the tree under
# test (the trace check reads its web/src): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket and a loopback port, no network. Every wait is
# bounded in time.

set -eu


asserted=""
harness() {
    echo "040: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_040_talk_$run"
SPAWN_TMUX_SOCKET="bb-040-$run"
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
            echo "040: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engine.log" ]; then
            echo "040: what the engine read:" >&2
            tail -n 30 "$WORK/engine.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "040: the supervisor's commands (id, principal, key, state, succeeded, error):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, principal, idempotency_key, state, succeeded,
                    left(coalesce(error, ''), 160)
                from spawn_commands where session_id = 'session_supervisor' order by id" \
                >&2 2>/dev/null || true
            echo "040: the supervisor's last events (id, name, step or state, command):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, event_name,
                    coalesce(kind -> 'detail' ->> 'step', kind -> 'detail' ->> 'state', ''),
                    coalesce(kind -> 'detail' ->> 'command_id', '')
                from spawn_events where session_id = 'session_supervisor'
               order by id desc limit 25" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "040: harness: unexpected exit $rc" >&2
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

# The fake engine: a prompt; text typed in (a paste, then Enter) is logged
# `got <time> <line>`, one entry per line, and opens a turn, which ends after
# a second of quiet, as Claude Code's does: the person's prompt and the
# model's answer (`answer: <its first line>`) in the transcript, then the
# end of the turn, then the prompt on the screen. It writes its transcript
# where Claude Code does.
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
first=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    printf 'got %s %s\n' "$(date +%s.%N)" "$typed" >>"$BB_WORK/engine.log"
    if [ "$pending" = 0 ]; then
      first=$typed
      text=$(printf '%s' "$typed" | jq -Rs .)
      line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":$text}"
    fi
    pending=1
  elif [ "$pending" = 1 ]; then
    answer=$(printf 'answer: %s' "$first" | jq -Rs .)
    line "\"type\":\"assistant\",\"message\":{\"id\":\"msg_$count\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":$answer}],\"stop_reason\":\"end_turn\"}"
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf 'answer: %s\n\n❯ ' "$first"
    printf 'answered %s %s\n' "$(date +%s.%N)" "$first" >>"$BB_WORK/engine.log"
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
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_ADDRESS \
    http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
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
# The supervisor runs, as on the host; the dispatcher does not (no digest
# reaches the supervisor's pane but what the scenario sends).
SPAWN_SUPERVISOR=1
SPAWN_DISPATCH=0
SPAWN_MISSION_FILE=$WORK/mission.md
# Free disk is not what this scenario is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_SUPERVISOR \
    SPAWN_DISPATCH SPAWN_MISSION_FILE
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

tree=$(pwd)
[ -f "$tree/web/src/trace.ts" ] && [ -f "$tree/web/src/commands.ts" ] ||
    harness "run from the tree under test: no web/src/trace.ts under $tree"

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

SESSION=session_supervisor
AGENT=agent_supervisor
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
state_of() { q "select state || ':' || coalesce(succeeded::text, '-') from spawn_commands where id = $1"; }
error_of() { q "select coalesce(error, '') from spawn_commands where id = $1"; }
finished() { [ "$(q "select state from spawn_commands where id = $1")" = finished ]; }
succeeded() { [ "$(state_of "$1")" = finished:true ]; }
read_count() { grep -c "^$1 [0-9.]* $2\$" "$WORK/engine.log" || true; }
logged() { [ "$(read_count "$1" "$2")" -ge 1 ]; }
judged_waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$SESSION'")" = WAITING ]
}
live() { [ "$(q "select coalesce(live::text, 'false') from agent_status where session_id = '$SESSION'")" = true ]; }
executions() { q "select count(*) from agent_executions where session_id = '$SESSION'"; }

# --- the supervisor, live and idle ------------------------------------------------------

start_loop
wait_until 120 live || harness "the supervisor was never started"
wait_until 120 judged_waiting || harness "the supervisor was never judged waiting at its prompt"

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
base="http://127.0.0.1:$port"
# post_input <body>: the status code; the answer in $WORK/input.json. As the
# page posts it: JSON, the page's own Origin, from the owner's own client.
post_input() {
    curl -s --noproxy '*' -o "$WORK/input.json" -w '%{http_code}' -X POST \
        -H 'Content-Type: application/json' -H "Origin: $base" \
        --data "$1" "$base/api/session/$SESSION/input" 2>/dev/null || echo 000
}
get_json() { # <path> <file>
    code=$(curl -s --noproxy '*' -o "$2" -w '%{http_code}' "$base$1" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET $1 answered $code: $(head -c 300 "$2" 2>/dev/null)"
}

# --- (1) the page's path ----------------------------------------------------------------

page_text="BB:ASK 040 from the page, how is the fleet"
cli_text="BB:ASK 040 from the shell, and the gates"
code=$(post_input "{\"text\":\"$page_text\",\"key\":\"bb-page-1\"}")
[ "$code" = 202 ] || fail "(1) POST /api/session/$SESSION/input answered $code: $(head -c 300 "$WORK/input.json")"
page=$(jq -r '.command_id' "$WORK/input.json")
[ -n "$page" ] && [ "$page" != null ] || fail "(1) the page's input answered no command id: $(cat "$WORK/input.json")"
assert_eq "$(jq -r '.principal' "$WORK/input.json")" human "(1) the principal the page's input answers"
wait_until 60 finished "$page" || fail "(1) the page's input did not finish within 60 s ($(state_of "$page"))"
succeeded "$page" || fail "(1) the page's input finished $(state_of "$page"): $(error_of "$page")"
assert_eq "$(q "select principal || ' ' || agent_id || ' ' || session_id from spawn_commands where id = $page")" \
    "human $AGENT $SESSION" "(1) the page's command: principal, agent, session"
assert_eq "$(q "select action = jsonb_build_object('type', 'input', 'text', '$page_text') from spawn_commands where id = $page")" t \
    "(1) the page's command is the input as given ($(q "select action from spawn_commands where id = $page"))"
wait_until 30 logged answered "$page_text" || fail "(1) the supervisor's engine never answered the page's input: $(tail -n 5 "$WORK/engine.log" | tr '\n' ' ')"
cli=$(send human bb-cli-1 "{\"type\":\"input\",\"text\":\"$cli_text\"}")
wait_until 60 finished "$cli" || harness "the CLI's input did not finish within 60 s ($(state_of "$cli"))"
succeeded "$cli" || harness "the CLI's input finished $(state_of "$cli"): $(error_of "$cli")"
assert_eq "$(q "select (a.principal, a.agent_id, a.session_id, a.action - 'text') = (b.principal, b.agent_id, b.session_id, b.action - 'text')
                 from spawn_commands a, spawn_commands b where a.id = $page and b.id = $cli")" t \
    "(1) the page's command and the one spawn submit --principal human makes have the same shape"
wait_until 30 logged answered "$cli_text" || harness "the supervisor's engine never answered the CLI's input"
code=$(post_input "{\"text\":\"$page_text\",\"key\":\"bb-page-1\"}")
[ "$code" = 202 ] || fail "(1) the same input again answered $code: $(head -c 300 "$WORK/input.json")"
assert_eq "$(jq -r '.command_id' "$WORK/input.json")" "$page" "(1) the command the same key again returns"
sleep 3
assert_eq "$(read_count got "$page_text")" 1 "(1) times the engine read the page's input"
assert_eq "$(read_count got "$cli_text")" 1 "(1) times the engine read the CLI's input"

# --- (2) the trace, as the page derives it ------------------------------------------------

get_json "/api/session/$SESSION?after=0&limit=1000" "$WORK/session.json"
get_json "/api/fleet/$SESSION/commands?limit=100" "$WORK/commands.json"
cat >"$WORK/trace.test.mjs" <<'TEST'
// The supervisor page's trace, as web/src/trace.ts derives it from what the
// API serves (normalized as web/src/api.ts and web/src/fleet.ts do).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const { BB_TREE, BB_WORK, BB_PAGE_TEXT, BB_CLI_TEXT } = process.env;
const { deriveTrace } = await import(pathToFileURL(`${BB_TREE}/web/src/trace.ts`).href);
const session = JSON.parse(readFileSync(`${BB_WORK}/session.json`, "utf8"));
const raw = JSON.parse(readFileSync(`${BB_WORK}/commands.json`, "utf8"));
const events = (session.timeline ?? []).map((event) => ({
  session_seq: Number(event.session_seq),
  event_id: Number(event.event_id),
  execution_id: typeof event.execution_id === "string" ? event.execution_id : null,
  source: String(event.source ?? "unknown"),
  event_name: String(event.event_name ?? "unknown"),
  detail: event.detail,
  recorded_at_ms: Number(event.recorded_at_ms),
}));
const list = Array.isArray(raw) ? raw : (raw.commands ?? []);
const commands = new Map();
for (const record of list) {
  if (record.command_id === null || record.command_id === undefined) continue;
  commands.set(String(record.command_id), {
    principal: String(record.principal ?? "unknown"),
    action: String(record.action ?? "unknown"),
    text: typeof record.text === "string" ? record.text : null,
  });
}

test("a person's input to the supervisor reads `You → supervisor`, and its answer follows", () => {
  const trace = deriveTrace(events, 0, [], commands, "supervisor");
  for (const text of [BB_PAGE_TEXT, BB_CLI_TEXT]) {
    const span = trace.spans.find((one) => one.kind === "command" && one.summary === text);
    const seen = trace.spans.filter((one) => one.kind === "command").map((one) => `${one.label} · ${one.summary}`);
    assert.ok(span, `no command span whose summary is "${text}": ${JSON.stringify(seen)}`);
    assert.equal(span.label, "You → supervisor", `the label of the input "${text}"`);
    const answer = trace.spans.find((one) => one.kind === "turn" && one.start >= span.start);
    assert.ok(answer, `no turn span after the input "${text}" (the supervisor's answer)`);
  }
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK BB_PAGE_TEXT=$page_text BB_CLI_TEXT=$cli_text \
    node --test "$WORK/trace.test.mjs" >"$WORK/trace.out" 2>&1; then
    # web/src/trace.ts and what it imports must load with node alone (the
    # objective says so): one that does not is the candidate's, not the
    # harness's.
    if grep -q "ERR_MODULE_NOT_FOUND\|SyntaxError\|ERR_UNKNOWN_FILE_EXTENSION\|ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX" "$WORK/trace.out"; then
        fail "(2) web/src/trace.ts does not load with node alone (relative .ts imports, import type for types, no packages, erasable TypeScript only): $(grep -m 3 'Error' "$WORK/trace.out" | tr '\n' ' ')"
    fi
    fail "(2) the supervisor page's trace: $(grep -E 'AssertionError|no command span|label|no turn span|expected|actual' "$WORK/trace.out" | head -n 6 | tr '\n' ' ')"
fi

# --- (3) refused when the supervisor is stopped ------------------------------------------

stop=$(send human bb-stop '{"type":"stop"}')
wait_until 120 succeeded "$stop" || harness "the stop did not succeed ($(state_of "$stop") $(error_of "$stop"))"
not_live() { ! live; }
wait_until 30 not_live ||
    harness "the supervisor still runs an execution after its stop"
runs=$(executions)
commands_before=$(q "select count(*) from spawn_commands")
code=$(post_input '{"text":"BB:ASK 040 while stopped","key":"bb-page-stopped"}')
[ "$code" = 409 ] || fail "(3) the page's input to the stopped supervisor answered $code, not 409: $(head -c 300 "$WORK/input.json")"
jq -r '.error' "$WORK/input.json" | grep -q 'runs no execution' ||
    fail "(3) the refusal does not say the supervisor runs no execution: $(cat "$WORK/input.json")"
assert_eq "$(q "select count(*) from spawn_commands")" "$commands_before" "(3) commands after the refused input"
late=$(send human bb-cli-stopped '{"type":"input","text":"BB:ASK 040 by the shell while stopped"}')
wait_until 30 finished "$late" || fail "(3) the CLI's input to the stopped supervisor did not finish ($(state_of "$late"))"
assert_eq "$(state_of "$late")" finished:false "(3) the CLI's input to the stopped supervisor"
error_of "$late" | grep -q 'runs no execution' ||
    fail "(3) its error does not say the supervisor runs no execution: $(error_of "$late")"
sleep 3
assert_eq "$(executions)" "$runs" "(3) the supervisor's executions after its stop (none started again)"
assert_eq "$(read_count got 'BB:ASK 040 while stopped')" 0 "(3) times the engine read the refused input"

echo "040: the page's input reached the supervisor once as the person's command, the same one spawn submit makes; the trace shows it as 'You → supervisor' with the answer after it; a stopped supervisor refuses it with the reason and records nothing"
exit 0
