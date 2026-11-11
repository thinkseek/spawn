#!/bin/sh
# Journey j11, push-drain: the integrator is never stopped under a gate. An
# owner's push to a mission source is recorded and applied by the running
# integrator between its own steps (never under a gate on the same source).
#
# Folds today Spawn scenario 021 whole (design-gate-journeys-spawn.md §2,
# row check-j11-push-drain: 021, 037), every numbered check of it:
#   021 setup  beta's first task lands on SB, so mission B has a baseline.
#   021 (1) another source's gate survives: alpha's gate on SA runs; the
#           owner pushes to SB with `spawn mission push`; within 30 s, while
#           alpha's gate runs, SB's main is the owner's commit, one
#           `source.moved` names the request, from and to, B's baseline is
#           the new main, nothing halts (after two more passes too), alpha's
#           gate is alive and has no record, the integrator is the same
#           process; released, alpha's gate ends `accepted`, once, and lands.
#   021 (2) the same source (D1): beta2's gate is held on SB; the owner's
#           second push waits for it, said once; SB's main stays the main
#           beta2's gate judged; released, beta2 is `accepted` once (never
#           `stale`) and lands on that main; within 30 s the push is applied
#           on beta2's landing (a merge: first parent beta2's landing,
#           second the owner's commit); `spawn mission pushes` says
#           `pending`, then `applied` with its source.moved; B never halts.
#
# 037 (the drain: `spawn integrate --drain`, SIGUSR1,
# SPAWN_INTEGRATE_DRAIN_SECS) joins when its objective lands. It is unsolved
# on 13fc93a7 (`spawn integrate --drain` is refused by the usage), and a
# journey folds only solved scenarios (037.sh stays its task's acceptance
# test). Its section, all seven checks, is
# /data/tmp/ccd/rev-g-tmp/sj/j11/section-037.patch (`patch -p1` from the
# tree root). It runs after 021's, on a loop, tmux socket and database of
# its own, because the two conflict loop-wide: 037's loop runs the
# supervisor (021's none), with SPAWN_MISSION_FILE and every
# SPAWN_STALL_*_SECS at 1800; 021 needs one integrator never restarted,
# while 037 drains, refuses, expires and TERMs five on one --root and
# asserts no gate for its task B while one drains; 037's gate directory is
# `no-test-database`, and its integrators get SPAWN_TEST_DATABASE_URL,
# SPAWN_GATE_JOBS=1, SPAWN_GATE_WORKER_CORES=0 and the base warm.
#
# Today: one work directory, one database, one tmux socket, one `spawn
# loop`, one long-running `spawn integrate`. Every wait and bound is 021's.
# The control records (spawn_events) and mission_* tables are read where
# 021 reads them, with its assertions.
#
# Each step echoes `j11: [NNN (n)]` before the checks it carries.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
# Named check-*: the gate runs a failing check again, and on the base if it fails twice (design-gate-v2.md C2)
#
# Needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own
# databases, uses private tmux sockets, no network. Every wait is bounded in
# time.
set -eu

asserted=""
harness() {
    echo "j11: harness: $*" >&2
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

ROOT=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$ROOT"' EXIT
# A scratch HOME, and the tmux server's socket, in $ROOT: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$ROOT/home
TMUX_TMPDIR=$ROOT/tmux-sock
mkdir -p "$HOME" "$TMUX_TMPDIR"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME TMUX_TMPDIR
W021=$ROOT/s021
mkdir -p "$W021"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
DB021="bb_j11_push_$run"
SOCKET021="bb-j11p-$run"
SPAWN_PSQL=psql
export SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

. "$controller/scripts/blackbox/assertions.sh"

# An assertion exits 1, unless the database is gone: then nothing the
# journey read can be trusted, and that is the harness's failure.
fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}

made021=""
loop_pid=""
integrate_pid=""
section=""

# Section 021's processes and database (021's cleanup).
teardown_021() {
    # Released, so a held gate ends on its own, and stopped by the pid it
    # wrote: $W021 goes below before its next poll could see the release,
    # and killing the integrator leaves its gate. Only a gate still held
    # has a pid file: it removes it once released.
    : >"$W021/release-alpha" 2>/dev/null || true
    : >"$W021/release-beta2" 2>/dev/null || true
    for held in "$W021"/gate-alpha.pid "$W021"/gate-beta2.pid; do
        [ -s "$held" ] || continue
        held_pid=$(cat "$held")
        # Only this run's fake gate, never a pid since reused.
        tr '\0' ' ' <"/proc/$held_pid/cmdline" 2>/dev/null | grep -qF "$W021/fake-gate.sh" &&
            kill "$held_pid" 2>/dev/null || true
    done
    for pid in "$integrate_pid" "$loop_pid"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    integrate_pid=""
    loop_pid=""
    tmux -L "$SOCKET021" kill-server >/dev/null 2>&1 || true
    if [ -n "$made021" ]; then
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$DB021" >/dev/null 2>&1 || true
        made021=""
    fi
}
cleanup() {
    teardown_021
    chmod -R u+rwX "$ROOT" 2>/dev/null || true
    rm -rf "$ROOT"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        echo "j11: ended in section $section" >&2
        if [ "$section" = 021 ]; then
            if [ -s "$W021/loop.log" ]; then
                echo "j11: [021] the loop's last lines:" >&2
                tail -n 10 "$W021/loop.log" >&2
            fi
            if [ -s "$W021/integrate.log" ]; then
                echo "j11: [021] the integrator's last lines:" >&2
                tail -n 15 "$W021/integrate.log" >&2
            fi
            if [ -s "$W021/judged.log" ]; then
                echo "j11: [021] the gate runs (time event name base candidate):" >&2
                cat "$W021/judged.log" >&2
            fi
            if [ -n "$made021" ]; then
                echo "j11: [021] the control records after setup (id type task verdict reason):" >&2
                psql -d "$DB021" -tA -c "select e.id, r ->> 'type', coalesce(r ->> 'id', ''),
                           coalesce(r ->> 'verdict', ''), left(coalesce(r ->> 'reason', ''), 120)
                    from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                    where e.session_id = 'session_control'
                      and r ->> 'type' in ('task.gated', 'task.landed', 'mission.halted',
                                           'source.moved', 'source.push_requested', 'source.push_refused')
                    order by e.session_seq" >&2 2>/dev/null || true
            fi
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "j11: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

# Logins of this journey's databases that a run killed before its cleanup
# left behind (scripts/blackbox/scenario-db.sh): this prefix only.
sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${DB021%"$run"}" >/dev/null 2>&1 || true

journey_started=$(date +%s)

# =============================================================================
# Section 021: the owner's push, applied by the one running integrator.
# =============================================================================

section=021
WORK=$W021
SPAWN_DB_NAME=$DB021
SPAWN_TMUX_SOCKET=$SOCKET021
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the database of section 021"
made021=1

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
# `<time> start|end <name> <base> <candidate>`, and its pid while held
# (written before its start line).
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
name=other
for held in beta2 alpha; do
    if git --git-dir="\$1" cat-file -e "\$3:\$held.txt" 2>/dev/null; then name=\$held; break; fi
done
# The pid first, whole (written aside, then moved in): the scenario reads it
# as soon as the start line below is there, and found it missing or empty
# when the start line came first.
if [ "\$name" != other ]; then
    echo \$\$ >'$WORK/gate-'\$name.pid.\$\$ && mv -f '$WORK/gate-'\$name.pid.\$\$ '$WORK/gate-'\$name.pid
fi
printf '%s start %s %s %s\n' "\$(date +%s)" "\$name" "\$2" "\$3" >>'$WORK/judged.log'
echo "== candidate: build"
if [ "\$name" != other ]; then
    waited=0
    until [ -e '$WORK/release-'\$name ] || [ "\$waited" -ge 1200 ]; do
        sleep 0.5
        waited=\$((waited + 1))
    done
    # Released: no longer held, so the cleanup has no pid of it to kill (a
    # pid kept past its gate could be any process by then).
    rm -f '$WORK/gate-'\$name.pid
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

# `spawn mission pushes --source <SB> --commit <commit>`, as the owner checks
# a push: sets `pushed` to the state of request $request in it (one row of
# it, newest first), `pushed_outcome` to its outcome's event id.
owner_pushes() {
    capture "$SPAWN_BIN" mission pushes --source "$sb" --commit "$commit"
    [ "$status" -eq 0 ] ||
        fail "$1 \`spawn mission pushes --source <SB> --commit $commit\` exited $status: $(err | head -c 300)"
    pushed=$(jq -r --argjson r "$request" \
        'if length == 1 and .[0].request == $r then .[0].state else "rows \(map(.request))" end' \
        "$WORK/out.txt" 2>/dev/null) || fail "$1 \`spawn mission pushes\` printed no list: $(out | head -c 300)"
    pushed_outcome=$(jq -r '.[0].outcome.event_id // empty' "$WORK/out.txt")
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
echo "j11: [021 setup] beta's first task lands on SB; mission B has a baseline"
t_beta1=$(assign "$mission_b" beta beta1.txt)
wait_state "$t_beta1" landed 240 "setup"
[ "$(baseline "$mission_b")" = "$(main_of "$sb")" ] ||
    harness "mission B's baseline $(baseline "$mission_b") is not SB's main $(main_of "$sb") after beta1 landed"
git clone -q "$sb" "$WORK/owner"

# --- (1) an owner push to SB while A's gate runs on SA ---------------------------------
echo "j11: [021 (1)] another source's gate survives an owner push to SB"

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

# --- (2) an owner push to SB while beta's gate runs on SB (D1) --------------------------
echo "j11: [021 (2)] the same source (D1): the owner's push waits for beta2's gate, then lands on its landing"

t_beta2=$(assign "$mission_b" beta beta2.txt)
wait_gate beta2 "(2)"
first_base=$(grep " start beta2 " "$WORK/judged.log" | head -n 1 | cut -d' ' -f4)
owner_push "the supply contract, amended" "(2)"
# A pass sees the request and leaves it waiting: its line, once (300 s: a
# guard against a hang only).
waits="(request $request) waits for the judging in flight on its source"
deadline=$(($(now) + 300))
until grep -qF "$waits" "$WORK/integrate.log"; do
    if [ "$(applied "$request")" -gt 0 ]; then
        fail "(2) the owner's push (request $request) was applied while beta2's gate ran on SB: SB's main is $(main_of "$sb"), the main beta2's gate judged $first_base"
    fi
    [ "$(now)" -lt "$deadline" ] ||
        harness "(2) no pass said the owner's push (request $request) waits within 300 s (integrate: $(log_tail))"
    integrator_alive || fail "(2) the integrator exited: $(log_tail)"
    no_halt "(2) while the owner's push waited"
    sleep 0.5
done
assert_eq "$(applied "$request")" 0 "(2) a source.moved for the owner's push while beta2's gate runs"
owner_pushes "(2) while beta2's gate runs"
assert_eq "$pushed" pending "(2) the owner's push in \`spawn mission pushes\` while it waits"
assert_eq "$pushed_outcome" "" "(2) the waiting push's outcome in \`spawn mission pushes\`"
assert_eq "$(main_of "$sb")" "$first_base" "(2) SB's main while beta2's gate runs (the main it judged)"
assert_eq "$(gate_records "$t_beta2")" "" "(2) beta2's gate records while its gate runs"
no_halt "(2) while beta2's gate runs"
integrator_alive || fail "(2) the integrator is not running after the owner's push: $(log_tail)"
: >"$WORK/release-beta2"
wait_state "$t_beta2" landed 120 "(2)"
assert_eq "$(gate_records "$t_beta2")" accepted \
    "(2) beta2's gate records (its own verdict, on the main it judged: never stale)"
assert_eq "$(grep -c " start beta2 " "$WORK/judged.log" || true)" 1 "(2) beta2's gate runs"
landing=$(q "select kind -> 'detail' -> 'record' ->> 'to' from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
                and kind -> 'detail' -> 'record' ->> 'id' = '$t_beta2'")
[ -n "$landing" ] || fail "(2) beta2 is landed but no task.landed names it"
assert_eq "$(q "select kind -> 'detail' -> 'record' ->> 'from' from spawn_events
               where session_id = 'session_control'
                 and kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
                 and kind -> 'detail' -> 'record' ->> 'id' = '$t_beta2'")" \
    "$first_base" "(2) beta2 landed on the main its gate judged"
deadline=$(($(now) + 30))
until [ "$(applied "$request")" -gt 0 ]; do
    if [ "$(now)" -ge "$deadline" ]; then
        fail "(2) the owner's push (request $request) was not applied within 30 s of beta2's landing: SB's main is $(main_of "$sb") (integrate: $(log_tail))"
    fi
    no_halt "(2) while the owner's push waited for its pass"
    sleep 0.5
done
moved2=$(applied "$request")
owner_pushes "(2) after the owner's landing"
assert_eq "$pushed" applied "(2) the owner's push in \`spawn mission pushes\` once landed"
assert_eq "$pushed_outcome" "$(q "select id from spawn_events
                                  where session_id = 'session_control'
                                    and kind -> 'detail' -> 'record' ->> 'type' = 'source.moved'
                                    and kind -> 'detail' -> 'record' ->> 'request' = '$request'")" \
    "(2) the landed push's outcome in \`spawn mission pushes\` (its source.moved)"
assert_eq "$(field "$moved2" from)" "$landing" "(2) the source.moved record's from (beta2's landing)"
assert_eq "$(field "$moved2" to)" "$(main_of "$sb")" "(2) the source.moved record's to (SB's main)"
assert_eq "$(git --git-dir="$sb" rev-parse main^1)" "$landing" \
    "(2) the first parent of SB's main (beta2's landing)"
assert_eq "$(git --git-dir="$sb" rev-parse main^2)" "$commit" \
    "(2) the second parent of SB's main (the owner's second commit)"
git --git-dir="$sb" cat-file -e main:beta2.txt || fail "(2) SB's main does not hold beta2.txt"
assert_eq "$(baseline "$mission_b")" "$(main_of "$sb")" "(2) mission B's baseline after the owner's landing"
assert_eq "$(grep -cF "$waits" "$WORK/integrate.log" || true)" 1 "(2) the owner's push said to wait"
# Two more passes (the integrator sleeps 5 s between them) see the new main.
sleep 12
no_halt "(2) after the owner's landing"
integrator_alive || fail "(2) the integrator is not running at the end: $(log_tail)"


echo "j11: [021] the owner's pushes were recorded and applied by the running integrator: another source's gate survived, the same source's push waited for its gate and landed after it, nothing halted ($(($(date +%s) - journey_started)) s)"
echo "j11: the journey took $(($(now) - journey_started)) s"
exit 0
