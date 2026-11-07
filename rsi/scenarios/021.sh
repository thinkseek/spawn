#!/bin/sh
# Black box: an owner's push to a mission source is one command, recorded
# as an event and applied by the integrator between its own steps, for that
# source only. The integrator is never stopped: gates of other missions and
# other sources keep running, nothing halts, and a gate running on the same
# source finishes and is judged `stale` by the existing rule (judged again
# on the new main), never thrown away silently.
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, no
# model judge, no supervisor) runs two residents on a fake engine: `alpha`
# in mission A on source SA, `beta` in mission B on source SB, both gated
# (accept = gate). One long-running `spawn integrate` (not `--once`; started
# once, never restarted) judges through a recording gate (SPAWN_GATE) that
# accepts every candidate and notes each run's start and end; a candidate
# holding alpha.txt or beta2.txt is held, its gate running, until the
# scenario releases it ($WORK/release-<name>), as a 40-minute gate would be.
# The owner works in a clone of SB and lands with
# `spawn mission push --source SB --commit <commit>`.
#   setup: beta's first task (beta1.txt) lands on SB, so mission B has a
#     baseline (the main its last landing left): the rule that halts a
#     mission whose source's main moved without a landing applies to it.
#   (1) another source's gate survives: alpha's gate on SA is running; the
#       owner pushes a commit that holds SB's main. The command answers with
#       the request it recorded (`source.push_requested`). Within 30 s, while
#       alpha's gate still runs, the integrator applies it: SB's main is the
#       owner's commit, a `source.moved` record names `request` (the
#       request's event id), `from` the old main and `to` the new, mission
#       B's baseline is the new main, and neither mission is halted (after
#       two more passes too). Alpha's gate process is still alive, alpha has
#       no gate record yet, the integrator is the same process. Released,
#       alpha's gate ends `accepted`, once, and alpha lands on SA.
#   (2) the same source: beta's second task (beta2.txt) is judged on SB, its
#       gate held; the owner pushes a second commit to SB. Within 30 s, while
#       that gate runs, SB's main is the owner's second commit (a
#       `source.moved` naming the request), B not halted. Released, the gate
#       finishes: its verdict is `accepted`, then the landing finds main
#       moved and records `stale` (the existing rule), and beta2 is judged
#       again, against the owner's second commit, accepted and landed on top
#       of it. B is never halted; the integrator is the same process.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (06cc205, the deployed binary built
# from it): there is no `spawn mission push` (today the owner stops
# spawn-integrate, which kills every running gate, pushes, runs `spawn
# mission moved`, and starts it again; a push without the stop halts the
# mission: "the source's main is X, but the last landing left Y"). It passes
# once `spawn mission push` records the owner's landing as an event and the
# integrator applies it at its next step for that source, moving main and
# the baselines with one `source.moved`, without halting and without
# touching any running gate.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "021: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_021_push_$run"
SPAWN_TMUX_SOCKET="bb-021-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
integrate_pid=""
cleanup() {
    # Released, so a held gate ends on its own.
    : >"$WORK/release-alpha" 2>/dev/null || true
    : >"$WORK/release-beta2" 2>/dev/null || true
    for pid in "$integrate_pid" "$loop_pid"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
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
            echo "021: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "021: the integrator's last lines:" >&2
            tail -n 15 "$WORK/integrate.log" >&2
        fi
        if [ -s "$WORK/judged.log" ]; then
            echo "021: the gate runs (time event name base candidate):" >&2
            cat "$WORK/judged.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "021: the control records after setup (id type task verdict reason):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, r ->> 'type', coalesce(r ->> 'id', ''),
                       coalesce(r ->> 'verdict', ''), left(coalesce(r ->> 'reason', ''), 120)
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control'
                  and r ->> 'type' in ('task.gated', 'task.landed', 'mission.halted',
                                       'source.moved', 'source.push_requested', 'source.push_refused')
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "021: harness: unexpected exit $rc" >&2
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
sa=$WORK/sa.git
sb=$WORK/sb.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "base"
for bare in "$repo" "$sa" "$sb"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

gate=$WORK/gate
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
: >"$WORK/judged.log"

# The recording gate: accepts every candidate; one holding alpha.txt or
# beta2.txt runs until $WORK/release-<name> exists (at most 600 s). Notes
# `<time> start|end <name> <base> <candidate>`, and its pid while held.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
name=other
for held in beta2 alpha; do
    if git --git-dir="\$1" cat-file -e "\$3:\$held.txt" 2>/dev/null; then name=\$held; break; fi
done
printf '%s start %s %s %s\n' "\$(date +%s)" "\$name" "\$2" "\$3" >>'$WORK/judged.log'
echo "== candidate: build"
if [ "\$name" != other ]; then
    echo \$\$ >'$WORK/gate-'\$name.pid
    waited=0
    until [ -e '$WORK/release-'\$name ] || [ "\$waited" -ge 1200 ]; do
        sleep 0.5
        waited=\$((waited + 1))
    done
fi
echo "| build: ok"
printf '%s end %s %s %s\n' "\$(date +%s)" "\$name" "\$2" "\$3" >>'$WORK/judged.log'
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
chmod 755 "$WORK/fake-gate.sh"

# The fake engine: a text naming `BB:WRITE <file>` opens a turn that writes
# the file, waits for the pusher to publish it (at most 30 s), then writes
# the report.
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
name=${SPAWN_SESSION_ID#session_resident_}
agent=agent_resident_$name
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1
  line '"type":"system","subtype":"turn_duration","durationMs":1'
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
file=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in *"BB:WRITE "*) [ -n "$file" ] || file=${typed#*BB:WRITE }; file=${file%% *} ;; esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$file" ]; then
      printf '%s was here\n' "$name" >"$file"
      turn_ends "wrote $file"
      start=$(date +%s)
      until [ "$(git --git-dir="$BB_SOURCE" show "refs/heads/spawn/$agent:$file" 2>/dev/null)" = "$(cat "$file")" ]; do
        [ $(($(date +%s) - start)) -ge 30 ] && break
        sleep 0.2
      done
      line '"type":"user","message":{"role":"user","content":"report"}'
      sleep 2
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$file" >.spawn/report.md
      file=
    fi
    turn_ends idle
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"
# The engine waits on its own mission's source: alpha's is SA, beta's SB.
cat >"$WORK/claude-wrapper" <<EOF
#!/bin/sh
case "\$SPAWN_SESSION_ID" in
*alpha) BB_SOURCE='$sa' ;;
*) BB_SOURCE='$sb' ;;
esac
export BB_SOURCE
exec '$WORK/claude' "\$@"
EOF
chmod 755 "$WORK/claude-wrapper"

cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_GATE_MIN_FREE_GB \
    SPAWN_GATE_MIN_FREE_MEMORY_MB SPAWN_INTEGRATE_GATES SPAWN_TEST_DATABASE_LOG; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude-wrapper
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"
: >"$WORK/integrate.log"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- helpers ----------------------------------------------------------------

now() { date +%s; }
q() { scalar "$1"; }
task_state() { q "select state from mission_tasks where id = $1"; }
halted() { q "select coalesce(halted, '') from mission_missions where id = $1"; }
baseline() { q "select coalesce(baseline, '') from mission_missions where id = $1"; }
tasks_line() { q "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks"; }
log_tail() { tail -n 6 "$WORK/integrate.log" 2>/dev/null | tr '\n' ' '; }
main_of() { git --git-dir="$1" rev-parse main; }
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }
integrator_alive() { [ -n "$integrate_pid" ] && kill -0 "$integrate_pid" 2>/dev/null; }
# The `source.moved` that applied request `$1`: its event id.
applied() {
    q "select coalesce(min(id), 0) from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = 'source.moved'
          and kind -> 'detail' -> 'record' ->> 'request' = '$1'"
}
field() { q "select kind -> 'detail' -> 'record' ->> '$2' from spawn_events where id = $1"; }
gate_records() { # <task>: its gate verdicts, oldest first
    q "select coalesce(string_agg(kind -> 'detail' -> 'record' ->> 'verdict', ' ' order by id), '')
         from spawn_events where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
no_halt() { # <what>
    for m in "$mission_a" "$mission_b"; do
        [ -z "$(halted "$m")" ] || fail "$1: mission $m halted: $(halted "$m")"
    done
}
wait_state() { # <task> <state> <seconds> <what>
    deadline=$(($(now) + $3))
    until [ "$(task_state "$1")" = "$2" ]; do
        [ "$(now)" -lt "$deadline" ] ||
            harness "$4: task #$1 did not reach $2 within $3 s (tasks: $(tasks_line); integrate: $(log_tail))"
        integrator_alive || fail "$4: the integrator exited: $(log_tail)"
        sleep 1
    done
}
gate_started() { grep -q " start $1 " "$WORK/judged.log"; }
wait_gate() { # <name> <what>
    deadline=$(($(now) + 180))
    until gate_started "$1"; do
        [ "$(now)" -lt "$deadline" ] ||
            harness "$2: $1's gate did not start within 180 s (tasks: $(tasks_line); integrate: $(log_tail))"
        sleep 0.5
    done
}
# owner_push <message>: the owner commits in the clone of SB and lands it
# with `spawn mission push`; sets `commit` and `request`.
owner_push() {
    git -C "$WORK/owner" fetch -q origin main
    git -C "$WORK/owner" merge -q --ff-only origin/main
    printf '%s\n' "$1" >>"$WORK/owner/CONTRACT.md"
    git -C "$WORK/owner" add -A
    git -C "$WORK/owner" commit -qm "owner: $1"
    commit=$(git -C "$WORK/owner" rev-parse HEAD)
    status=0
    (cd "$WORK/owner" && "$SPAWN_BIN" mission push --source "$sb" --commit "$commit") \
        >"$WORK/out.txt" 2>"$WORK/err.txt" || status=$?
    [ "$status" -eq 0 ] ||
        fail "$2 \`spawn mission push --source <SB> --commit $commit\` exited $status: $(err | head -c 300)"
    request=$(jq -r '.request // empty' "$WORK/out.txt" 2>/dev/null || true)
    [ -n "$request" ] || fail "$2 \`spawn mission push\` printed no request: $(out | head -c 300)"
    assert_eq "$(q "select kind -> 'detail' -> 'record' ->> 'type' from spawn_events where id = $request")" \
        source.push_requested "$2 the record of request $request"
}

# --- two missions, two sources ------------------------------------------------------

for name in alpha beta; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 021 $name change."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$name"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
done
new_mission() { # <text> <source>
    capture "$SPAWN_BIN" mission add --text "$1" --source "$2"
    [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
    m=$(jq -r .mission "$WORK/out.txt")
    [ -n "$m" ] && [ "$m" != null ] || harness "mission add printed no id: $(out)"
    capture "$SPAWN_BIN" mission configure --id "$m" --gate "$gate" --accept gate
    [ "$status" -eq 0 ] || harness "mission configure exited $status: $(err)"
    echo "$m"
}
mission_a=$(new_mission "021 A" "$sa")
mission_b=$(new_mission "021 B" "$sb")
assign() { # <mission> <resident> <file>
    capture "$SPAWN_BIN" task assign --mission "$1" --kind resident --worker "$2" \
        --brief "Write $3. BB:WRITE $3"
    [ "$status" -eq 0 ] || harness "task assign for $2 failed: $(err)"
    jq -r .task "$WORK/out.txt"
}

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" SPAWN_GATE_MIN_FREE_GB=0 \
    SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
    "$SPAWN_BIN" integrate --root "$WORK/integrator" >>"$WORK/integrate.log" 2>&1 &
integrate_pid=$!

# Setup: beta's first task lands on SB; mission B has a baseline.
t_beta1=$(assign "$mission_b" beta beta1.txt)
wait_state "$t_beta1" landed 240 "setup"
[ "$(baseline "$mission_b")" = "$(main_of "$sb")" ] ||
    harness "mission B's baseline $(baseline "$mission_b") is not SB's main $(main_of "$sb") after beta1 landed"
git clone -q "$sb" "$WORK/owner"

# --- (1) an owner push to SB while A's gate runs on SA ---------------------------------

t_alpha=$(assign "$mission_a" alpha alpha.txt)
wait_gate alpha "(1)"
alpha_pid=$(cat "$WORK/gate-alpha.pid")
sb_before=$(main_of "$sb")
owner_push "the supply contract" "(1)"
deadline=$(($(now) + 30))
until [ "$(applied "$request")" -gt 0 ]; do
    if [ "$(now)" -ge "$deadline" ]; then
        fail "(1) the owner's push (request $request) was not applied within 30 s: SB's main is $(main_of "$sb"), the owner's commit $commit (integrate: $(log_tail))"
    fi
    no_halt "(1) while the owner's push waited"
    sleep 0.5
done
moved=$(applied "$request")
assert_eq "$(main_of "$sb")" "$commit" "(1) SB's main after the owner's landing"
assert_eq "$(field "$moved" source)" "$sb" "(1) the source.moved record's source"
assert_eq "$(field "$moved" from)" "$sb_before" "(1) the source.moved record's from"
assert_eq "$(field "$moved" to)" "$commit" "(1) the source.moved record's to"
assert_eq "$(baseline "$mission_b")" "$commit" "(1) mission B's baseline after the owner's landing"
kill -0 "$alpha_pid" 2>/dev/null ||
    fail "(1) alpha's gate on SA (pid $alpha_pid) did not survive the owner's push to SB"
assert_eq "$(gate_records "$t_alpha")" "" "(1) alpha's gate records while its gate runs"
integrator_alive || fail "(1) the integrator is not running after the owner's push: $(log_tail)"
# Two more passes (the integrator sleeps 5 s between them) see the new main.
sleep 12
no_halt "(1) after the owner's landing"
kill -0 "$alpha_pid" 2>/dev/null || fail "(1) alpha's gate died after the owner's push to SB"
: >"$WORK/release-alpha"
wait_state "$t_alpha" landed 60 "(1)"
assert_eq "$(gate_records "$t_alpha")" accepted "(1) alpha's gate records"
assert_eq "$(grep -c " start alpha " "$WORK/judged.log" || true)" 1 "(1) alpha's gate runs"
no_halt "(1) after alpha landed"

# --- (2) an owner push to SB while beta's gate runs on SB --------------------------------

t_beta2=$(assign "$mission_b" beta beta2.txt)
wait_gate beta2 "(2)"
first_base=$(grep " start beta2 " "$WORK/judged.log" | head -n 1 | cut -d' ' -f4)
owner_push "the supply contract, amended" "(2)"
deadline=$(($(now) + 30))
until [ "$(applied "$request")" -gt 0 ]; do
    if [ "$(now)" -ge "$deadline" ]; then
        fail "(2) the owner's push (request $request) was not applied within 30 s while beta2's gate ran on SB: SB's main is $(main_of "$sb") (integrate: $(log_tail))"
    fi
    no_halt "(2) while the owner's push waited"
    sleep 0.5
done
assert_eq "$(main_of "$sb")" "$commit" "(2) SB's main after the owner's second landing"
assert_eq "$(gate_records "$t_beta2")" "" "(2) beta2's gate records while its gate runs"
integrator_alive || fail "(2) the integrator is not running after the owner's push: $(log_tail)"
: >"$WORK/release-beta2"
wait_state "$t_beta2" landed 120 "(2)"
assert_eq "$(gate_records "$t_beta2")" "accepted stale accepted" \
    "(2) beta2's gate records (the held gate's verdict, the stale landing, the gate again on the new main)"
second_base=$(grep " start beta2 " "$WORK/judged.log" | sed -n 2p | cut -d' ' -f4)
assert_eq "$first_base" "$(field "$moved" to)" "(2) the base of beta2's first gate"
assert_eq "$second_base" "$commit" "(2) the base of beta2's second gate (the owner's second commit)"
git --git-dir="$sb" merge-base --is-ancestor "$commit" main ||
    fail "(2) SB's main after beta2 landed does not hold the owner's second commit"
git --git-dir="$sb" cat-file -e main:beta2.txt || fail "(2) SB's main does not hold beta2.txt"
no_halt "(2) after beta2 landed"
integrator_alive || fail "(2) the integrator is not running at the end: $(log_tail)"

echo "021: the owner's pushes were recorded and applied by the running integrator: another source's gate survived, the same source's gate finished and was judged again, nothing halted"
exit 0
