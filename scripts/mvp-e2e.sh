#!/usr/bin/env bash
# MVP end to end: the whole decision ladder with a scripted engine and a stub
# Jev, on real PostgreSQL, git and tmux (docs/fleet-architecture.md §7).
#
# It asserts every hop:
#   1. a rule decides WORKING while a tool runs (no model call);
#   2. Jev decides an idle prompt, and a finished worker reaches the
#      supervisor as a digest;
#   3. the guarded nudge restarts a worker that stopped mid-task, once;
#   4. a HUMAN: question reaches attention even when Jev is confident it is
#      fine, and a human command resolves it;
#   5. with Jev down, a stalled worker is escalated on rules alone, not nudged;
#   6. killing the supervisor's engine, then its tmux server, then the loop,
#      each ends with session_supervisor live again, resumed natively;
#   7. the corpus exports and replay reports a tier per episode;
#   8. missions: a human mission reaches the supervisor; a resident is
#      proposed by the supervisor and runs only after a human approves it;
#      its tasks are delivered one at a time, reported, sent back once and
#      accepted; an ephemeral worker reports and is stopped once accepted; a
#      human message to a worker reaches the supervisor; a resident killed is
#      resumed, a resident stopped by the supervisor stays down.
#
# Usage: SPAWN_DATABASE_URL=postgres://... scripts/mvp-e2e.sh
# Runs in its own schema (dropped afterwards) and its own tmux socket.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
bin=${SPAWN_BIN:-}
if [ -z "$bin" ]; then
  (cd "$root" && cargo build --quiet -p spawn-cli)
  bin=${CARGO_TARGET_DIR:-$root/target}/debug/spawn
fi
base_url=${SPAWN_DATABASE_URL:?SPAWN_DATABASE_URL is required}
tmux=${SPAWN_TMUX_PROGRAM:-tmux}
work=$(mktemp -d)
socket="spawn-mvp-$$"
schema="mvp_e2e_$$"
port=$((20000 + $$ % 20000))
loop_pid=""
jev_pid=""

case "$base_url" in
*\?*) url="$base_url&options=-csearch_path%3D$schema" ;;
*) url="$base_url?options=-csearch_path%3D$schema" ;;
esac

# Reads go through the supervisor's login: the fleet readers are filtered by
# grants, and the owner holds none. Its password is derived from the owner's
# as spawn-postgres does it (role_password): SHA-256 of
# "spawn-login-v1\0<owner password>\0<role>", in hex.
owner_password=$(printf '%s' "$url" | sed -E 's#^[a-z]+://[^:@]*:([^@]*)@.*#\1#')
sup_password=$(printf 'spawn-login-v1\0%s\0spawn_supervisor' "$owner_password" | sha256sum | cut -d' ' -f1)
sup_url=$(printf '%s' "$url" | sed -E "s#://[^@]*@#://spawn_supervisor:$sup_password@#")

cleanup() {
  [ -n "$loop_pid" ] && kill "$loop_pid" 2>/dev/null || true
  [ -n "$jev_pid" ] && kill "$jev_pid" 2>/dev/null || true
  "$tmux" -L "$socket" kill-server 2>/dev/null || true
  rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$socket"
  if [ "${KEEP:-0}" != 1 ]; then rm -rf "$work"; else echo "mvp-e2e: kept $work" >&2; fi
}
trap cleanup EXIT

fail() {
  echo "mvp-e2e: FAIL: $*" >&2
  echo "--- loop log (tail)" >&2
  tail -n 40 "$work/loop.log" >&2 || true
  exit 1
}
pass() { echo "mvp-e2e: ok: $*"; }

# wait_for <seconds> <description> <command...>
wait_for() {
  local limit=$1 what=$2
  shift 2
  local deadline=$((SECONDS + limit))
  until "$@" >/dev/null 2>&1; do
    [ "$SECONDS" -lt "$deadline" ] || fail "timed out waiting for $what"
    sleep 0.5
  done
}

# --- repository and workspaces -------------------------------------------
repo=$work/repo.git
git init -q --bare --initial-branch=main "$repo"
seed=$work/seed
git init -q --initial-branch=main "$seed"
printf 'base\n' >"$seed/README.md"
git -C "$seed" add -A
git -C "$seed" -c user.name=e2e -c user.email=e2e@localhost commit -qm base
git -C "$seed" push -q "$repo" main:main
mkdir -p "$work/ws"

# --- scripted engine ------------------------------------------------------
# Draws a prompt, writes a Claude Code session transcript where the real
# engine does (the Bridge reads it; nothing reports to Spawn), and acts on
# tokens in typed lines:
#   E2E:WORK   one tool call, then "DONE", then stop
#   E2E:STALL  stop partway ("STALLED"); a later "continue" finishes it
#   E2E:ASK    stop with a HUMAN: question
#   E2E:REPORT write .spawn/report.md, then stop; with E2E:WRITE=<file>,
#              write that file into the workspace first
# Other lines are read silently; one second of quiet ends the turn.
cat >"$work/claude" <<'ENGINE'
#!/usr/bin/env bash
log=$E2E_WORK/argv.log
printf '%s %s\n' "$SPAWN_SESSION_ID" "$*" >>"$log"
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=$2 ;; esac
  shift
done
dir=$SPAWN_CLAUDE_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
c=0
line() {
  c=$((c + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$c" "$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)" "$native" "$1" >>"$transcript"
}
prompt() { line '"type":"user","message":{"role":"user","content":"prompt"}'; }
n=0
tool() {
  n=$((n + 1))
  id=t$n-$SPAWN_EXECUTION_ID
  line "\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}]}"
  sleep "${1:-0.3}"
  line "\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
}
stop() { line '"type":"system","subtype":"turn_duration","durationMs":1'; printf '\n❯ '; }
printf 'Welcome to Claude Code\n\n❯ '
pending=0
while :; do
  if IFS= read -r -t 1 line; then
    # Screen-reader mode shows each message typed in as a `you:` line.
    printf 'you: %s\n' "$line"
    case "$line" in
    *E2E:WORK*)
      prompt
      printf '\nworking...\n'
      tool "${E2E_TOOL_SECONDS:-0.3}"
      printf 'DONE: work finished\n'
      stop; pending=0 ;;
    *E2E:STALL*)
      prompt
      tool
      printf 'STALLED: halfway through, stopping here\n'
      stop; pending=0 ;;
    continue)
      prompt
      tool
      printf 'DONE: finished after continue\n'
      stop; pending=0 ;;
    *E2E:REPORT*)
      prompt
      tool
      mkdir -p .spawn
      case "$line" in
      *E2E:WRITE=*) written=${line#*E2E:WRITE=}; printf 'written\n' >"${written%% *}" ;;
      esac
      printf '## Outcome\ndone: %s\n## What changed\nfiles: %s\n## Open issues\nnone\n' "$line" "$(ls | tr '\n' ' ')" >.spawn/report.md
      printf 'DONE: reported\n'
      stop; pending=0 ;;
    *E2E:ASK*)
      prompt
      printf 'HUMAN: which region should I deploy to?\n'
      stop; pending=0 ;;
    *)
      if [ "$pending" = 0 ]; then
        prompt
      fi
      pending=1 ;;
    esac
  elif [ "$pending" = 1 ]; then
    printf 'idle\n'
    stop; pending=0
  fi
done
ENGINE
chmod 755 "$work/claude"

# --- stub Jev ---------------------------------------------------------------
# Answers from what the evidence shows. For a HUMAN: question it answers a
# confident, harmless WAITING on purpose: the rule must page anyway.
cat >"$work/jev.py" <<'JEV'
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

def answer(state, conf, needs_human, stuck):
    probs = {s: 0.0 for s in ["WORKING", "WAITING", "BLOCKED", "COMPLETED", "UNKNOWN"]}
    probs[state] = conf
    rest = [s for s in probs if s != state]
    for s in rest:
        probs[s] = round((1 - conf) / len(rest), 4)
    return {"model": "stub", "answers": {
        "semantic_state": {"type": "choice", "choice": state, "confidence": conf, "probabilities": probs},
        "needs_human": {"type": "noul", "noul": needs_human},
        "stuck": {"type": "noul", "noul": stuck}}}

class Stub(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0))).decode()
        if "DONE:" in body.split("STALLED:")[-1] or "finished after continue" in body:
            reply = answer("COMPLETED", 0.85, 0.05, 0.05)
        elif "STALLED:" in body:
            reply = answer("WAITING", 0.9, 0.05, 0.1)
        elif "HUMAN:" in body:
            reply = answer("WAITING", 0.95, 0.05, 0.05)
        else:
            reply = answer("WAITING", 0.8, 0.1, 0.05)
        data = json.dumps(reply).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_):
        pass

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Stub).serve_forever()
JEV

start_jev() {
  python3 -I "$work/jev.py" "$port" &
  jev_pid=$!
  wait_for 10 "the stub Jev" bash -c "exec 3<>/dev/tcp/127.0.0.1/$port"
}

# --- environment ------------------------------------------------------------
export E2E_WORK=$work E2E_SPAWN=$bin SPAWN_CLAUDE_PROJECTS=$work/claude-projects
export SPAWN_DATABASE_URL=$url
export SPAWN_GIT_REPO=$repo SPAWN_WORKSPACE_ROOT=$work/ws
export SPAWN_CLAUDE_PROGRAM=$work/claude SPAWN_PROGRAM=$bin
export SPAWN_TMUX_PROGRAM=$tmux SPAWN_TMUX_SOCKET=$socket
export SPAWN_WORKER=worker-mvp SPAWN_LOOP_INTERVAL_MS=300
export SPAWN_JUDGE_URL=http://127.0.0.1:$port SPAWN_JUDGE_MODEL=stub
export SPAWN_SUPERVISOR=1 SPAWN_MISSION_FILE=$work/mission.md
: >"$work/mission.md"

"$bin" init-db >/dev/null
start_jev

start_loop() {
  "$bin" loop --repo "$repo" --root "$work/ws" >>"$work/loop.log" 2>&1 &
  loop_pid=$!
}
start_loop

# Small SQL helpers through the owner login, via the fleet JSON.
fleet() { SPAWN_DATABASE_URL=$sup_url "$bin" fleet; }
tail_of() { SPAWN_DATABASE_URL=$sup_url "$bin" tail --session "$1"; }
observation() { # session -> latest observation JSON
  tail_of "$1" | jq -c '.judgement // {}'
}
pane() { # session -> captured pane text of its latest terminal
  local terminal
  terminal=$(fleet | jq -r --arg s "$1" '.[] | select(.session_id == $s) | "spawn-\(.execution_id // "")"' 2>/dev/null)
  "$tmux" -L "$socket" capture-pane -p -t "=$terminal:" -S -200 2>/dev/null || true
}
live() { fleet | jq -e --arg s "$1" 'any(.[]; .session_id == $s and .live)' >/dev/null; }
execution_of() { fleet | jq -r --arg s "$1" '.[] | select(.session_id == $s) | .execution_id // ""'; }

submit() { # principal session text-or-start
  local principal=$1 session=$2 action=$3
  local agent=${session/session_/agent_}
  "$bin" submit --principal "$principal" --agent "$agent" --session "$session" \
    --key "e2e-$(date +%s%N)" --action "$action" >/dev/null
}
start_worker() { submit spawn_supervisor "$1" "{\"type\":\"start\",\"engine\":\"claude\",\"prompt\":\"$2\"}"; }

# --- 0. the supervisor comes up -------------------------------------------
wait_for 60 "the supervisor to start" live session_supervisor
wait_for 30 "the supervisor's native session" grep -q '^session_supervisor' "$work/argv.log"
pass "supervisor started by the liveness rule"

# --- 1+2. rules decide WORKING; Jev decides the idle prompt; digest ---------
export E2E_TOOL_SECONDS=3
start_worker session_worker_a "E2E:WORK build the thing"
judged_working_by_rules() {
  tail_of session_worker_a | jq -e '
    any(.judgements[]?; .state == "WORKING" and .judge == "rules")' >/dev/null
}
wait_for 40 "a rules WORKING judgement for worker a" judged_working_by_rules
pass "rule decided WORKING while the tool ran"
unset E2E_TOOL_SECONDS
completed_by_jev() { observation session_worker_a | jq -e '.state == "COMPLETED" and .judge == "jev"'; }
wait_for 60 "Jev to judge worker a COMPLETED" completed_by_jev
digest_has() { pane session_supervisor | grep -q "\[spawn:esc $1 "; }
wait_for 60 "a digest about worker a in the supervisor's pane" digest_has session_worker_a
pass "Jev decided the idle prompt; the supervisor got the digest"

# --- 3. the guarded nudge, once --------------------------------------------
start_worker session_worker_b "E2E:STALL do half"
nudged_once() {
  tail_of session_worker_b | jq -e '
    [.commands[]? | select(.principal == "spawn-dispatch")] | length == 1' >/dev/null
}
wait_for 60 "one nudge into worker b" nudged_once
finished_after_nudge() { pane session_worker_b | grep -q 'finished after continue'; }
wait_for 30 "worker b to finish after the nudge" finished_after_nudge
sleep 3
nudged_once || fail "worker b was nudged more than once"
pass "the guarded nudge restarted a stalled worker, once"

# --- 4. HUMAN: question pages despite a confident Jev; human resolves ------
start_worker session_worker_c "E2E:ASK deploy it"
asks() { observation session_worker_c | jq -e '.attention == true and .attention_reason == "human_question"'; }
wait_for 60 "attention on worker c's question" asks
observation session_worker_c | jq -e '.state == "WAITING" and .judge == "jev"' >/dev/null ||
  fail "the stub should have called worker c a harmless WAITING"
wait_for 60 "a digest about worker c" digest_has session_worker_c
submit human session_worker_c '{"type":"input","text":"E2E:WORK eu-west"}'
resolved() { observation session_worker_c | jq -e '(.attention // false) == false'; }
wait_for 60 "worker c's attention to clear after the human answer" resolved
pass "a HUMAN: question reached attention and a human command resolved it"

# --- 5. Jev down: rules and escalation only ---------------------------------
kill "$jev_pid"; wait "$jev_pid" 2>/dev/null || true; jev_pid=""
start_worker session_worker_d "E2E:STALL do half"
unavailable() { observation session_worker_d | jq -e '.attention_reason == "judge_unavailable"'; }
wait_for 60 "worker d judged with Jev unreachable" unavailable
wait_for 60 "a digest about worker d" digest_has session_worker_d
tail_of session_worker_d | jq -e '[.commands[]? | select(.principal == "spawn-dispatch")] | length == 0' >/dev/null ||
  fail "worker d was nudged without Jev"
pass "with Jev down the stalled worker was escalated, not nudged"
start_jev

# --- 8. missions, residents, tasks, reports --------------------------------
sup() { SPAWN_DATABASE_URL=$sup_url "$bin" "$@"; }
board() { sup mission list; }
task_state() { board | jq -r --argjson id "$1" '.missions[].tasks[] | select(.id == $id) | .state'; }
state_is() { [ "$(task_state "$1")" = "$2" ]; }

mission=$("$bin" mission add --text "E2E mission: prove the mission layer" | jq -r .mission)
wait_for 60 "the new mission in the supervisor's digest" digest_has missions

sup resident propose --name core --charter "Owns the e2e core concern." >/dev/null
if sup task assign --mission "$mission" --kind resident --worker core --brief "E2E:REPORT one" >/dev/null 2>&1; then
  fail "an unapproved resident got a task"
fi
if sup resident approve --name core >/dev/null 2>&1; then
  fail "the supervisor approved its own resident"
fi
"$bin" resident approve --name core >/dev/null
wait_for 60 "the approval in the supervisor's digest" digest_has session_resident_core
pass "a resident runs only after a human approves the supervisor's proposal"

t1=$(sup task assign --mission "$mission" --kind resident --worker core --brief "E2E:REPORT first" | jq -r .task)
t2=$(sup task assign --mission "$mission" --kind resident --worker core --brief "E2E:REPORT second" | jq -r .task)
wait_for 60 "task $t1 to be delivered" state_is "$t1" delivered
[ "$(task_state "$t2")" = assigned ] || fail "the resident got two tasks at once"
wait_for 60 "task $t1's report" state_is "$t1" reported
wait_for 60 "the report in the supervisor's digest" digest_has session_resident_core
board | jq -e --argjson id "$t1" '.missions[].tasks[] | select(.id == $id) | .report | test("done: .*E2E:REPORT first")' >/dev/null ||
  fail "task $t1's report was not stored"
sup task reject --id "$t1" --note "E2E:REPORT redo with more care" >/dev/null
wait_for 60 "task $t1 to come back after the note" state_is "$t1" reported
board | jq -e --argjson id "$t1" '.missions[].tasks[] | select(.id == $id) | .revisions == 1 and (.report | test("redo"))' >/dev/null ||
  fail "the revision did not carry the note"
sup task accept --id "$t1" >/dev/null
wait_for 60 "task $t2 to be delivered after $t1" state_is "$t2" delivered
wait_for 60 "task $t2's report" state_is "$t2" reported
sup task accept --id "$t2" >/dev/null
pass "resident tasks: one at a time, reported, sent back once, accepted"

board | jq -e --argjson id "$t2" '.missions[].tasks[] | select(.id == $id) | .report | test("README") | not' >/dev/null ||
  fail "a mission without sources saw the default repository"
pass "a mission without sources works in a scratch workspace"

# A mission sourced from another repository, whose default branch is not main.
other=$work/other
git init -q --initial-branch=trunk "$other"
printf 'x\n' >"$other/OTHER_MARKER.txt"
git -C "$other" add -A
git -C "$other" -c user.name=e2e -c user.email=e2e@localhost commit -qm other
sourced=$("$bin" mission add --text "E2E sourced mission" --source "$other" | jq -r .mission)
t3=$(sup task assign --mission "$sourced" --kind ephemeral --worker probe --brief "E2E:REPORT probe" | jq -r .task)
probe=$(board | jq -r --argjson id "$t3" '.missions[].tasks[] | select(.id == $id) | .session_id')
wait_for 60 "the ephemeral report" state_is "$t3" reported
board | jq -e --argjson id "$t3" '.missions[].tasks[] | select(.id == $id) | .report | test("OTHER_MARKER")' >/dev/null ||
  fail "the sourced mission's worker did not get its repository"
sup task accept --id "$t3" >/dev/null
gone() { ! live "$probe"; }
wait_for 60 "the ephemeral worker to be stopped" gone
pass "an ephemeral worker got its mission's repository, reported, and was stopped once accepted"

# The source moves on (a merge lands); the next new worker starts from it.
printf 'y\n' >"$other/NEWER_MARKER.txt"
git -C "$other" add -A
git -C "$other" -c user.name=e2e -c user.email=e2e@localhost commit -qm newer
t4=$(sup task assign --mission "$sourced" --kind ephemeral --worker probe2 --brief "E2E:REPORT probe2" | jq -r .task)
wait_for 60 "the second ephemeral report" state_is "$t4" reported
board | jq -e --argjson id "$t4" '.missions[].tasks[] | select(.id == $id) | .report | test("NEWER_MARKER")' >/dev/null ||
  fail "a new worker started from a stale copy of its mission's source"
sup task accept --id "$t4" >/dev/null
pass "a new worker starts from the source as it is now, not as it was first mirrored"

# --- landing: one gate, one landing path (docs/fleet-architecture.md §10.3) ---
gated=$work/gated.git
git init -q --bare --initial-branch=main "$gated"
git -C "$seed" push -q "$gated" main:main
gate=$work/gate
mkdir -p "$gate"
printf '#!/bin/sh\n# Check: no file named BAD.\n[ ! -e BAD ]\n' >"$gate/check-no-bad.sh"
gm=$("$bin" mission add --text "E2E gated mission" --source "$gated" --gate "$gate" | jq -r .mission)
if sup mission configure --id "$gm" --gate "$gate" --accept gate >/dev/null 2>&1; then
  fail "the supervisor set a mission's gate"
fi
integrate() { SPAWN_GATE=$root/scripts/rsi/gate.sh "$bin" integrate --once --root "$work/ws" >>"$work/integrate.log" 2>&1; }
on_main() { git --git-dir="$gated" cat-file -e "main:$1" 2>/dev/null; }
# Two workers start from the same main, so the second lands as a merge.
t5=$(sup task assign --mission "$gm" --kind ephemeral --worker lander --brief "E2E:REPORT E2E:WRITE=FIRST_MARKER.txt" | jq -r .task)
t6=$(sup task assign --mission "$gm" --kind ephemeral --worker second --brief "E2E:REPORT E2E:WRITE=SECOND_MARKER.txt" | jq -r .task)
wait_for 60 "the first gated report" state_is "$t5" reported
wait_for 60 "the second gated report" state_is "$t6" reported
sup task accept --id "$t5" >/dev/null
sup task accept --id "$t6" >/dev/null
[ "$(task_state "$t5")" = accepted ] || fail "accepted gated work closed before landing"
integrate
state_is "$t5" landed || fail "accepted work did not land: $(task_state "$t5"); $(tail -n 5 "$work/integrate.log")"
on_main FIRST_MARKER.txt || fail "the landing is not on the source's main"
integrate
state_is "$t6" landed || fail "the second task did not land: $(task_state "$t6"); $(tail -n 5 "$work/integrate.log")"
on_main SECOND_MARKER.txt && on_main FIRST_MARKER.txt || fail "the merge lost a landing"
[ "$(git --git-dir="$gated" rev-list --parents -n 1 main | wc -w)" -eq 3 ] || fail "the second landing is not a merge"
lander=$(board | jq -r --argjson id "$t5" '.missions[].tasks[] | select(.id == $id) | .session_id')
gone_lander() { ! live "$lander"; }
wait_for 60 "the landed worker to be stopped" gone_lander
pass "accepted work passed the gate and landed on its source, the second as a merge"

t7=$(sup task assign --mission "$gm" --kind ephemeral --worker breaker --brief "E2E:REPORT E2E:WRITE=BAD" | jq -r .task)
wait_for 60 "the breaking report" state_is "$t7" reported
sup task accept --id "$t7" >/dev/null
integrate
state_is "$t7" revise || fail "rejected work was not sent back: $(task_state "$t7")"
board | jq -e --argjson id "$t7" '.missions[].tasks[] | select(.id == $id) | .review_note | test("check-no-bad")' >/dev/null ||
  fail "the note does not carry the gate's output"
! on_main BAD || fail "rejected work landed"
"$bin" task cancel --id "$t7" >/dev/null
pass "work the gate rejects goes back to its worker with the gate's output"

# Someone pushes to the source outside the gate: the mission halts.
outside=$work/outside
git clone -q "$gated" "$outside"
printf 'x\n' >"$outside/OUTSIDE.txt"
git -C "$outside" add -A
git -C "$outside" -c user.name=e2e -c user.email=e2e@localhost commit -qm outside
git -C "$outside" push -q origin HEAD:main
integrate
board | jq -e --argjson id "$gm" '.missions[] | select(.id == $id) | .halted | test("last landing")' >/dev/null ||
  fail "an outside push did not halt the mission"
if sup mission resume --id "$gm" >/dev/null 2>&1; then
  fail "the supervisor resumed a halted mission"
fi
"$bin" mission resume --id "$gm" >/dev/null
board | jq -e --argjson id "$gm" '.missions[] | select(.id == $id) | .halted == null' >/dev/null || fail "resume did not clear the halt"
pass "a push outside the gate halts the mission until a human resumes it"

# RSI as a mission (§10.4): the integrator turns the backlog into tasks for
# a resident, and the gate alone accepts them.
improved=$work/improved.git
git init -q --bare --initial-branch=main "$improved"
git -C "$seed" push -q "$improved" main:main
rsi_gate=$work/rsi-gate
rsi_backlog=$work/rsi-backlog
mkdir -p "$rsi_gate" "$rsi_backlog"
printf '#!/bin/sh\n# Objective 1: the file exists.\n[ -e OBJECTIVE_DONE ]\n' >"$rsi_gate/001.sh"
printf '#!/bin/sh\n# Check: no file named BAD.\n[ ! -e BAD ]\n' >"$rsi_gate/check-no-bad.sh"
printf '{"id": 1, "title": "Mark it done", "prompt": "E2E:REPORT E2E:WRITE=OBJECTIVE_DONE"}\n' >"$rsi_backlog/001.json"
sup resident propose --name improver --charter "Pursues the backlog's objectives" >/dev/null
"$bin" resident approve --name improver >/dev/null
om=$("$bin" mission add --text "E2E objectives" --source "$improved" --gate "$rsi_gate" --accept gate \
  --backlog "$rsi_backlog" --resident improver | jq -r .mission)
if sup mission configure --id "$om" --gate "$rsi_gate" --accept gate --backlog "$rsi_backlog" --resident improver >/dev/null 2>&1; then
  fail "the supervisor configured objectives"
fi
SPAWN_GATE=$root/scripts/rsi/gate.sh "$bin" integrate --once --root "$work/ws" >>"$work/integrate.log" 2>&1
objective_task() { board | jq -r --argjson id "$om" '[.missions[] | select(.id == $id) | .tasks[] | select(.objective == 1)] | first | .id // empty'; }
t8=$(objective_task)
[ -n "$t8" ] || fail "the integrator assigned no objective task: $(tail -n 5 "$work/integrate.log")"
board | jq -e --argjson id "$t8" '.missions[].tasks[] | select(.id == $id) | .created_by == "spawn-integrate" and (.brief | test("Objective 1"))' >/dev/null ||
  fail "the objective task does not come from the integrator"
wait_for 60 "the objective's report" state_is "$t8" reported
SPAWN_GATE=$root/scripts/rsi/gate.sh "$bin" integrate --once --root "$work/ws" >>"$work/integrate.log" 2>&1
state_is "$t8" landed || fail "the gate did not land the objective: $(task_state "$t8"); $(tail -n 5 "$work/integrate.log")"
git --git-dir="$improved" cat-file -e main:OBJECTIVE_DONE || fail "the objective's work is not on main"
SPAWN_GATE=$root/scripts/rsi/gate.sh "$bin" integrate --once --root "$work/ws" >>"$work/integrate.log" 2>&1
[ "$(board | jq --argjson id "$om" '[.missions[] | select(.id == $id) | .tasks[]] | length')" -eq 1 ] ||
  fail "a solved objective was assigned again"
pass "an objective becomes a resident's task, the gate alone lands it, and a solved one is not reassigned"

"$bin" submit --principal human --agent agent_resident_core --session session_resident_core \
  --key "human-$(date +%s%N)" --action '{"type":"input","text":"hello from a human"}' >/dev/null
human_noted() { pane session_supervisor | grep -q '\[spawn:esc session_resident_core human-'; }
wait_for 60 "the human message in the supervisor's digest" human_noted
pass "a human message to a worker reached the supervisor"

before=$(execution_of session_resident_core)
runner=$("$tmux" -L "$socket" list-panes -t "=spawn-$before:" -F '#{pane_pid}')
kill -KILL "$(pgrep -P "$runner" | head -1)"
resident_back() { live session_resident_core && [ "$(execution_of session_resident_core)" != "$before" ]; }
wait_for 60 "the resident back after its engine was killed" resident_back
grep -q -- "^session_resident_core .*--resume" "$work/argv.log" || fail "the resident was not resumed natively"
SPAWN_DATABASE_URL=$sup_url SPAWN_GATEWAY=1 "$bin" submit --agent agent_resident_core --session session_resident_core \
  --key "sup-stop-$(date +%s%N)" --action '{"type":"stop"}' >/dev/null
down() { ! live session_resident_core; }
wait_for 30 "the resident to stop" down
stopped_at=$(execution_of session_resident_core)
sleep 6
[ "$(execution_of session_resident_core)" = "$stopped_at" ] && ! live session_resident_core ||
  fail "a resident stopped by the supervisor was revived"
pass "a killed resident is resumed; one the supervisor stopped stays down"

# --- 6. the supervisor never ends -----------------------------------------
resumed_count() { grep -c -- "^session_supervisor .*--resume" "$work/argv.log" || true; }

before=$(execution_of session_supervisor)
runner=$("$tmux" -L "$socket" list-panes -t "=spawn-$before:" -F '#{pane_pid}')
engine=$(pgrep -P "$runner" | head -1)
resumes=$(resumed_count)
kill -KILL "$engine"
new_live() { live session_supervisor && [ "$(execution_of session_supervisor)" != "$before" ]; }
wait_for 60 "the supervisor back after its engine was killed" new_live
[ "$(resumed_count)" -gt "$resumes" ] || fail "the supervisor was not resumed natively"
pass "engine killed -> supervisor resumed"

before=$(execution_of session_supervisor)
resumes=$(resumed_count)
"$tmux" -L "$socket" kill-server
wait_for 90 "the supervisor back after its tmux server was killed" new_live
[ "$(resumed_count)" -gt "$resumes" ] || fail "the supervisor was not resumed after the tmux kill"
pass "tmux server killed -> supervisor resumed"

before=$(execution_of session_supervisor)
kill -KILL "$loop_pid"; wait "$loop_pid" 2>/dev/null || true
start_loop
sleep 5
live session_supervisor || fail "the supervisor died with the loop"
[ "$(execution_of session_supervisor)" = "$before" ] || fail "the loop restart replaced the supervisor's execution"
pass "loop killed -> the same supervisor execution kept running"

# --- 7. experience --------------------------------------------------------
"$bin" corpus export >"$work/corpus.jsonl"
records=$(wc -l <"$work/corpus.jsonl")
[ "$records" -gt 0 ] || fail "the corpus is empty"
"$bin" judge --replay "$work/corpus.jsonl" >"$work/replay.json"
jq -e '.total > 0 and (.by_tier | length) > 0' "$work/replay.json" >/dev/null || fail "replay has no tiers"
pass "corpus exported ($records records); replay: $(jq -c '{coverage, by_tier}' "$work/replay.json")"

echo "mvp-e2e: PASS"
