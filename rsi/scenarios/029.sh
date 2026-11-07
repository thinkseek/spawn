#!/bin/sh
# Black box: each execution and each gate runs in its own resource envelope.
# Every engine execution runs in a systemd user scope of its own,
# `<slice stem>-<execution id>.scope` (spawn-exec_12.scope in the default
# spawn.slice), inside one slice of Spawn's (SPAWN_SLICE) whose MemoryHigh,
# MemoryMax and CPUWeight are settings; the engine's children (builds, tests,
# tools) inherit the scope. An execution's MemoryHigh, MemoryMax and
# MemorySwapMax (SPAWN_EXECUTION_MEMORY_HIGH, _MAX, _SWAP_MAX: by the owner's
# ruling of 2026-11-06 10G, 12G and 4G by default) bound it, so a
# memory-limit kill ends that execution only: objective 028 records it
# (`execution.interrupted`, cause `memory_limit`, path `resume`) and resumes it
# with its work, while a sibling execution goes on untouched. An execution or
# a gate starts only when its envelope fits under the slice's limit and the
# host's available memory; otherwise it waits, recorded with the measure and
# the threshold (`admission.waiting`), and starts once it fits
# (`admission.admitted`). A gate runs in a scope of its own,
# `<slice stem>-gate-<task id>-<n>.scope`, with SPAWN_GATE_MEMORY_HIGH, _MAX
# and _SWAP_MAX (10G, 12G, 4G by default), and a gate its envelope kills is a
# host fault (objective 020's `host_fault`), judged again after 020's
# backoff, never an abort and never toward a halt (the owner's ruling).
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, no
# supervisor, no model judge) runs three residents of one gated mission on a
# fake engine, each started by a task of the mission, in a slice of this run
# alone (bb029_<run>.slice, SPAWN_SLICE_MEMORY_HIGH=128M,
# SPAWN_SLICE_MEMORY_MAX=144M, CPUWeight left at its default), executions
# in 92M/96M/16M (SPAWN_EXECUTION_MEMORY_HIGH, _MAX, _SWAP_MAX) and gates in
# 30M/32M/8M (SPAWN_GATE_MEMORY_HIGH, _MAX, _SWAP_MAX): every envelope is set,
# since the defaults do not fit under a 144M slice, and each MemoryHigh sits
# close below its MemoryMax, because a process that grows page by page past
# MemoryHigh is throttled, not killed (on this host 80M/96M/16M stalled at
# 96 MiB for 60 s; 92M/96M/16M was killed in 2.7 s, 30M/32M/8M in 7.8 s).
# SPAWN_GATE_HOST_FAULT_BACKOFF_SECS=3. Each engine notes, when it starts, its
# pid, its cgroup, that cgroup's memory.max, memory.swap.max and memory.high,
# a child's cgroup and whether work.txt is in its workspace. `hog` writes work.txt (checkpointed),
# then, told to, runs a tool (python3) that allocates past its envelope in
# 8 MiB steps up to 256 MiB, only when its own memory.max caps it at 256 MiB
# or less (uncapped, the host is not put at risk: it says so and allocates
# nothing). `calm` writes calm.txt and reports, then idles, and, told to,
# holds 56 MiB in a child until told to let go. `late` gets its task while
# calm holds it. Checked:
#   (1) hog's and calm's engines, and a child of each, run in
#       bb029_<run>-<their execution id>.scope directly inside the run's
#       slice, with memory.high 92 MiB, memory.max 96 MiB and memory.swap.max
#       16 MiB; the slice is active with memory.high 128 MiB, memory.max 144
#       MiB and cpu.weight 50; the loop's `host.envelope` record (state
#       `systemd`) names the slice and each of these settings in bytes.
#   (2) hog's allocation is killed by its envelope: its execution ends
#       within 20 s (the engine's pid gone), the slice counts the OOM kill
#       (memory.events oom_kill > 0), and calm runs on: the same execution,
#       its engine's pid alive, its scope's oom_kill 0.
#   (3) within 20 s of the end, an `execution.interrupted` record for hog's
#       execution: cause `memory_limit`, path `resume`, evidence naming the
#       scope, memory_max 100663296 and oom_kills > 0; within 30 s of it hog
#       is resumed (a new execution of its session, `--resume`) in a scope of
#       its own in the slice, its workspace holding work.txt byte for byte;
#       the killed execution's scope leaves no unit behind.
#   (4) admission: calm holds 56 MiB, so the slice's memory (anonymous and
#       shared, page cache not counted) is above 144 - 96 = 48 MiB, and late,
#       given its first task now, does not start: within 20 s an `admission.waiting`
#       record for agent_resident_late (kind `execution`, envelope_bytes
#       100663296, measure `slice`, threshold_bytes 50331648, measured_bytes
#       above it), and no engine of late starts while calm holds; calm lets
#       go, and within 30 s late's engine starts in its own scope in the
#       slice, after an `admission.admitted` record (waited_ms at least the
#       hold it waited through).
#   (5) a gate: calm's reported task is judged by `spawn integrate --once`
#       passes through a recording gate (SPAWN_GATE), which runs in
#       bb029_<run>-gate-<task id>-<n>.scope inside the slice, with
#       memory.high 30 MiB, memory.max 32 MiB and memory.swap.max 8 MiB, and
#       on its first run allocates past that envelope (hog's tool).
#   (6) that gate, killed by its memory limit, is a host fault: within 30 s
#       a `task.gated` with verdict `host_fault`, evidence `memory_limit`
#       naming its scope, memory_max 33554432 and oom_kills > 0, backoff_ms
#       3000 and due_ms its time + 3000; gate_aborts and gate_rejections
#       unchanged, the task still `reported`, the mission not halted; no gate
#       runs for it before due_ms; then it is judged again in a scope of a
#       larger n, and accepted.
#
# It needs a systemd user manager that delegates the memory controller, and
# the host's cgroup v2 tree: it is judged outside the gate's sandbox (whose
# /run is empty, so no user manager is reachable), by the owner (mission 7):
# the gate gets no delegated cgroup (the owner's ruling, 2026-11-06), and 032's
# real memory-limit kill is judged the same way. Without one it exits 1
# with "029: cannot be judged here: ...", never 2: an objective scenario that
# exits 2 on the base aborts every gate. The units it makes (the slice, the
# scopes in it, set-property drop-ins under $XDG_RUNTIME_DIR) are named after
# its run and removed at its end; it never touches another unit.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong, or it cannot be judged on this host); 2 a harness or
# infrastructure failure. Any unexpected error exits 2, never 1.
# BB_029_MEASURE=1 (only to show today's behaviour) reports every failed
# check and goes on, then exits 1 if any failed; with no envelope to kill
# hog's tool it kills hog's engine itself (SIGKILL to the pid the engine
# noted) to show how a killed execution is recorded and revived today.
#
# Fails with 1 at (1) on the live main (b950e25; the deployed binary is built
# from it): the engines run in the scopes tmux makes for its panes,
# app.slice/tmux-spawn-<uuid>.scope, with memory.max and memory.swap.max
# `max`, and the run's slice does not exist. It passes once every execution
# and every gate runs in its own scope in Spawn's slice with the configured
# limits, a memory-limit kill is recorded and resumed by objective 028's
# rules, and admission holds an execution whose envelope does not fit.
# Every wait is bounded in time.
#
# Today (four plain runs and three with BB_029_MEASURE=1, 2026-11-06, live
# main b950e25): hog's and calm's engines run in tmux's pane scopes,
# app.slice/tmux-spawn-<uuid>.scope, memory.max and memory.swap.max `max`,
# and the run's slice is not active; uncapped, the tool allocates nothing,
# so the measure kills hog's engine itself: the execution ends 1.38-1.42 s
# after the tool was told to run (the engine's 1 s poll included), recorded
# `execution.ended` cause `exited` with no exit code (no cause, no memory
# evidence, no `execution.interrupted`), and the residents' liveness rule
# resumes it 384-396 ms after its end, its own session, work.txt intact,
# again in a tmux scope; late starts 446-843 ms after its task's assignment
# with no admission record; the gate runs in the cgroup of the process that
# ran `spawn integrate`, with no limit; uncapped, its tool allocates nothing,
# so the measure's gate kills itself (SIGKILL), which today is recorded
# `aborted` ("the gate failed: the gate ended with signal: 9 (SIGKILL)";
# gate_aborts 0 -> 1, toward the halt) and judged again 0.7 s later, at the
# next pass, with no due time (one measure run, 38.9 s, 18:21 CST). It fails at (1) in 2-17 s; a measure run takes
# 31-92 s.
set -eu

asserted=""
harness() {
    echo "029: harness: $*" >&2
    exit 2
}
# Not judgeable on this host: an assertion (1), never a harness failure (2).
cannot_judge() {
    asserted=1
    echo "029: cannot be judged here: $*. Resource envelopes are systemd user scopes; this objective is judged outside the gate's sandbox, by the owner (mission 7)." >&2
    exit 1
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
MEASURE=${BB_029_MEASURE-}

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed awk sha256sum python3 systemctl; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

# --- the host: a user manager, cgroup v2, memory delegated ---------------------

[ "$(stat -f -c %T /sys/fs/cgroup 2>/dev/null || true)" = cgroup2fs ] ||
    cannot_judge "/sys/fs/cgroup is not a cgroup v2 tree"
manager_version=$(systemctl --user show -p Version --value 2>/dev/null || true)
[ -n "$manager_version" ] ||
    cannot_judge "no systemd user manager is reachable (systemctl --user: $(systemctl --user show -p Version --value 2>&1 | head -n 1))"
manager_cg=$(systemctl --user show -p ControlGroup --value 2>/dev/null || true)
[ -n "$manager_cg" ] && [ -d "/sys/fs/cgroup$manager_cg" ] ||
    cannot_judge "the user manager's cgroup is not visible ([$manager_cg])"
grep -qw memory "/sys/fs/cgroup$manager_cg/cgroup.subtree_control" 2>/dev/null ||
    cannot_judge "the user manager does not delegate the memory controller ($(cat "/sys/fs/cgroup$manager_cg/cgroup.subtree_control" 2>/dev/null))"

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
# Digits and `_` only: a `-` in a slice's name nests it in another slice.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_029_envelope_$run"
SPAWN_TMUX_SOCKET="bb-029-$run"
SPAWN_PSQL=psql
stem="bb029_$run"
slice="$stem.slice"
slice_dir="/sys/fs/cgroup$manager_cg/$slice"
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
cleanup() {
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
    fi
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
    # This run's units only: its slice (and every scope left in it), failed
    # scopes of its stem, and the slice's runtime drop-ins.
    systemctl --user stop "$slice" >/dev/null 2>&1 || true
    systemctl --user reset-failed "$stem-*" >/dev/null 2>&1 || true
    if [ -n "${XDG_RUNTIME_DIR-}" ] && [ -d "$XDG_RUNTIME_DIR/systemd/user.control/$slice.d" ]; then
        rm -rf "$XDG_RUNTIME_DIR/systemd/user.control/$slice.d"
    fi
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
            echo "029: the loop's last lines:" >&2
            tail -n 12 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engines.log" ]; then
            echo "029: the engines (ms|name|execution|start|pid|cgroup|memory.max|memory.swap.max|child cgroup|work.txt|memory.high):" >&2
            cat "$WORK/engines.log" >&2
        fi
        if [ -s "$WORK/gate.log" ]; then
            echo "029: the gate runs (ms|cgroup|memory.max|memory.swap.max|memory.high|run):" >&2
            cat "$WORK/gate.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "029: the executions and the envelope records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select session_id, execution_id, coalesce(cause, 'live'), coalesce(exit_code::text, '-')
                from agent_executions order by started_at_ms" >&2 2>/dev/null || true
            psql -d "$SPAWN_DB_NAME" -tA -c "select left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e
                where e.session_id = 'session_control'
                  and e.kind -> 'detail' -> 'record' ->> 'type' similar to '(execution|admission|host|task\.gated)%'
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "029: harness: unexpected exit $rc" >&2
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
# A check that BB_029_MEASURE=1 only reports.
failed_any=""
measured_fail() {
    if [ -n "$MEASURE" ]; then
        failed_any=1
        echo "029: (measure) would fail: $*"
    else
        fail "$*"
    fi
}

MIB=1048576
EXEC_HIGH=$((92 * MIB))
EXEC_MAX=$((96 * MIB))
EXEC_SWAP=$((16 * MIB))
GATE_HIGH=$((30 * MIB))
GATE_MAX=$((32 * MIB))
GATE_SWAP=$((8 * MIB))
BACKOFF_SECS=3
SLICE_HIGH=$((128 * MIB))
SLICE_MAX=$((144 * MIB))
SLICE_WEIGHT=50
THRESHOLD=$((SLICE_MAX - EXEC_MAX))

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
source=$WORK/source.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
for bare in "$repo" "$source"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

gate=$WORK/gate
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
# The recording gate: where it runs and under which limits (run <n>), then
# on its first run hog's tool, which allocates past a capped envelope (the
# envelope's kill ends this gate: no verdict); later runs are accepted.
# Uncapped (today), the tool allocates nothing: under BB_029_MEASURE=1 the
# gate then kills itself, to show how a killed gate is recorded today;
# otherwise it ends with no verdict. It runs with the gate's cleared
# environment, so its paths and the measure flag are written in.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
n=\$(( \$(cat '$WORK/gate-runs' 2>/dev/null || echo 0) + 1 ))
echo "\$n" >'$WORK/gate-runs'
cg=\$(sed -n 's/^0:://p' /proc/self/cgroup | head -n 1)
printf '%s|%s|%s|%s|%s|%s\n' "\$(date +%s%3N)" "\$cg" \
    "\$(cat "/sys/fs/cgroup\$cg/memory.max" 2>/dev/null || echo none)" \
    "\$(cat "/sys/fs/cgroup\$cg/memory.swap.max" 2>/dev/null || echo none)" \
    "\$(cat "/sys/fs/cgroup\$cg/memory.high" 2>/dev/null || echo none)" "\$n" >>'$WORK/gate.log'
if [ "\$n" = 1 ]; then
    rc=0
    out=\$(python3 -I '$WORK/alloc.py' '$WORK' gate 2>&1) || rc=\$?
    printf '%s|gate|%s|%s\n' "\$(date +%s%3N)" "\$rc" "\$out" >>'$WORK/gate-alloc.log'
    echo "gate: the tool ended with \$rc: \$out"
    if [ "\$rc" = 3 ] && [ -n '$MEASURE' ]; then
        kill -9 \$\$
    fi
    exit 3
fi
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
chmod 755 "$WORK/fake-gate.sh"

# hog's tool: allocates (pages touched) in 8 MiB steps up to 256 MiB, only
# under a memory.max of 256 MiB or less; exits 3 uncapped, 0 if it survives.
cat >"$WORK/alloc.py" <<'EOF'
import os, sys, time
work = sys.argv[1]
tag = sys.argv[2] if len(sys.argv) > 2 else "hog"
cgroup = ""
for line in open("/proc/self/cgroup"):
    if line.startswith("0::"):
        cgroup = line[3:].strip()
try:
    limit = open("/sys/fs/cgroup" + cgroup + "/memory.max").read().strip()
except OSError:
    limit = "none"
cap = 256 << 20
if not limit.isdigit() or int(limit) > cap:
    print("no cap: memory.max " + limit + ", nothing allocated")
    sys.exit(3)
with open(work + "/alloc-start-" + tag, "w") as out:
    out.write("%d\n" % int(time.time() * 1000))
chunks = []
while len(chunks) * (8 << 20) < cap:
    chunks.append(b"\x01" * (8 << 20))
    time.sleep(0.01)
print("survived: %d MiB allocated under memory.max %s" % (len(chunks) * 8, limit))
EOF
# calm's holder: 56 MiB (pages touched) until release-calm, at most 300 s.
cat >"$WORK/hold.py" <<'EOF'
import os, sys, time
work = sys.argv[1]
held = b"\x02" * (56 << 20)
with open(work + "/holding-calm.part", "w") as out:
    out.write("%d %d\n" % (os.getpid(), len(held)))
os.rename(work + "/holding-calm.part", work + "/holding-calm")
start = time.time()
while not os.path.exists(work + "/release-calm") and time.time() - start < 300:
    time.sleep(0.2)
EOF

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet, the transcript's end of the turn first, then the prompt.
# It writes its session transcript where Claude Code does. When it starts it
# notes (engines.log) its pid, its cgroup, that cgroup's memory.max and
# memory.swap.max, a child's cgroup, whether work.txt is in its workspace and
# the cgroup's memory.high.
# hog: its first turn writes work.txt (a tool call: checkpointed); once
# go-hog exists it runs alloc.py once (hogged marks it). calm: once
# hold-calm exists it runs hold.py in the background. A turn whose text
# names BB:WRITE writes <name>.txt, waits for the pusher to publish it (at
# most 30 s), then writes the report.
cat >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
start=new
while [ $# -gt 0 ]; do
  case "$1" in
  --resume) native=$2 start=resume ;;
  --session-id) native=$2 ;;
  esac
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
tool() { # <id> <command...>: a tool call around a command
  id="toolu_${SPAWN_EXECUTION_ID}_$1"
  shift
  line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
  "$@"
  line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
}
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1
  line '"type":"system","subtype":"turn_duration","durationMs":1'
}
cgroup=$(sed -n 's/^0:://p' /proc/self/cgroup | head -n 1)
child=$(sh -c "sed -n 's/^0:://p' /proc/self/cgroup | head -n 1")
work=absent
[ -f work.txt ] && work="found $(sha256sum <work.txt | cut -d' ' -f1)"
printf '%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s\n' "$(date +%s%3N)" "$name" "$SPAWN_EXECUTION_ID" "$start" "$$" \
  "$cgroup" "$(cat "/sys/fs/cgroup$cgroup/memory.max" 2>/dev/null || echo none)" \
  "$(cat "/sys/fs/cgroup$cgroup/memory.swap.max" 2>/dev/null || echo none)" "$child" "$work" \
  "$(cat "/sys/fs/cgroup$cgroup/memory.high" 2>/dev/null || echo none)" \
  >>"$BB_WORK/engines.log"
write_work() { printf "hog's work, written by %s\n" "$SPAWN_EXECUTION_ID" >work.txt; }
allocate() {
  rc=0
  out=$(python3 -I "$BB_WORK/alloc.py" "$BB_WORK" 2>&1) || rc=$?
  printf '%s|%s|%s|%s\n' "$(date +%s%3N)" "$SPAWN_EXECUTION_ID" "$rc" "$out" >>"$BB_WORK/alloc.log"
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
job=
holder=
while :; do
  if [ "$name" = hog ] && [ -e "$BB_WORK/go-hog" ] && [ ! -e "$BB_WORK/hogged" ]; then
    : >"$BB_WORK/hogged"
    tool alloc allocate
  fi
  if [ "$name" = calm ] && [ -z "$holder" ] && [ -e "$BB_WORK/hold-calm" ]; then
    python3 -I "$BB_WORK/hold.py" "$BB_WORK" &
    holder=$!
  fi
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in *"BB:WRITE"*) job=write ;; esac
  elif [ "$pending" = 1 ]; then
    if [ "$name" = hog ] && [ ! -f work.txt ]; then
      tool work write_work
    fi
    if [ -n "$job" ]; then
      file=$name.txt
      printf '%s was here\n' "$name" >"$file"
      turn_ends "wrote $file"
      begun=$(date +%s)
      until [ "$(git --git-dir="$BB_WORK/source.git" show "refs/heads/spawn/$agent:$file" 2>/dev/null)" = "$(cat "$file")" ]; do
        [ $(($(date +%s) - begun)) -ge 30 ] && break
        sleep 0.2
      done
      line '"type":"user","message":{"role":"user","content":"report"}'
      sleep 2
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$file" >.spawn/report.md
      job=
    fi
    turn_ends idle
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
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_LOOP_INTERVAL_MS \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS \
    SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS \
    SPAWN_STALL_HALTED_SECS SPAWN_ENVELOPE SPAWN_SLICE SPAWN_SLICE_MEMORY_HIGH \
    SPAWN_SLICE_MEMORY_MAX SPAWN_SLICE_CPU_WEIGHT SPAWN_EXECUTION_MEMORY_HIGH \
    SPAWN_EXECUTION_MEMORY_MAX SPAWN_EXECUTION_MEMORY_SWAP_MAX SPAWN_GATE_MEMORY_HIGH \
    SPAWN_GATE_MEMORY_MAX SPAWN_GATE_MEMORY_SWAP_MAX SPAWN_GATE_HOST_FAULT_BACKOFF_SECS \
    SPAWN_RECOVER_BACKOFF_SECS SPAWN_GATE_MIN_FREE_GB SPAWN_GATE_MIN_FREE_MEMORY_MB; do
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
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
# The envelopes under test: a slice of this run alone, small enough that
# two idle executions fit and a third does not while calm holds 56 MiB.
# CPUWeight is left at its default (50). Every envelope is set: the defaults
# (10G, 12G, 4G) do not fit under this slice, and a MemoryHigh well below
# its MemoryMax throttles a page-by-page allocation instead of killing it
# (the header's measures), so each MemoryHigh sits close below its MemoryMax.
SPAWN_ENVELOPE=systemd
SPAWN_SLICE=$slice
SPAWN_SLICE_MEMORY_HIGH=128M
SPAWN_SLICE_MEMORY_MAX=144M
SPAWN_EXECUTION_MEMORY_HIGH=92M
SPAWN_EXECUTION_MEMORY_MAX=96M
SPAWN_EXECUTION_MEMORY_SWAP_MAX=16M
SPAWN_GATE_MEMORY_HIGH=30M
SPAWN_GATE_MEMORY_MAX=32M
SPAWN_GATE_MEMORY_SWAP_MAX=8M
# Objective 028's resume backoff, short: the time to resume is measured.
SPAWN_RECOVER_BACKOFF_SECS=1
# No stall is under test.
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_ENVELOPE \
    SPAWN_SLICE SPAWN_SLICE_MEMORY_HIGH SPAWN_SLICE_MEMORY_MAX SPAWN_EXECUTION_MEMORY_HIGH \
    SPAWN_EXECUTION_MEMORY_MAX SPAWN_EXECUTION_MEMORY_SWAP_MAX SPAWN_GATE_MEMORY_HIGH \
    SPAWN_GATE_MEMORY_MAX SPAWN_GATE_MEMORY_SWAP_MAX SPAWN_RECOVER_BACKOFF_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS \
    SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"
: >"$WORK/engines.log"
: >"$WORK/alloc.log"
: >"$WORK/gate.log"
: >"$WORK/gate-alloc.log"

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
now_ms() { echo $(($(date +%s%N) / 1000000)); }
q() { scalar "$1"; }
mib() { awk -v b="$1" 'BEGIN { if (b ~ /^[0-9]+$/) printf "%.1f MiB", b / 1048576; else print b }'; }

# The latest engines.log line of a resident: ms|name|execution|start|pid|
# cgroup|memory.max|memory.swap.max|child cgroup|work.txt|memory.high
engine_line() { awk -F'|' -v n="$1" '$2 == n' "$WORK/engines.log" | tail -n 1; }
engines_of() { awk -F'|' -v n="$1" '$2 == n' "$WORK/engines.log" | wc -l | tr -d ' '; }
field() { printf '%s' "$1" | cut -d'|' -f"$2"; }
alive() { [ -n "$1" ] && kill -0 "$1" 2>/dev/null; }
events_count() { # <cgroup> <counter>: memory.events' counter, or `none`
    if [ -r "/sys/fs/cgroup$1/memory.events" ]; then
        awk -v k="$2" '$1 == k { print $2; found = 1 } END { if (!found) print "none" }' \
            "/sys/fs/cgroup$1/memory.events"
    else
        echo none
    fi
}
# A control record of a type whose field equals a value: `<at_ms>|<record>`,
# oldest first.
records() {
    sql "select e.recorded_at_ms || '|' || (e.kind -> 'detail' -> 'record')::text
           from spawn_events e
          where e.session_id = 'session_control'
            and e.kind -> 'detail' -> 'record' ->> 'type' = '$1'
            and e.kind -> 'detail' -> 'record' ->> '$2' = '$3'
          order by e.session_seq"
}
record_of() { records "$@" | head -n 1 | cut -d'|' -f2-; }
record_at() { records "$@" | head -n 1 | cut -d'|' -f1; }
live_execution() {
    q "select execution_id from agent_executions where session_id = 'session_resident_$1'
        and ended_at_ms is null order by started_at_ms desc limit 1"
}
ended_at() { q "select ended_at_ms from agent_executions where execution_id = '$1'"; }
# within <seconds> <shell condition>: polls every 0.3 s; 1 when time ran out.
within() {
    limit=$(($(now) + $1))
    shift
    while :; do
        if eval "$*"; then return 0; fi
        [ "$(now)" -lt "$limit" ] || return 1
        sleep 0.3
    done
}
integrate_once() {
    capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        SPAWN_GATE_HOST_FAULT_BACKOFF_SECS="$BACKOFF_SECS" \
        SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || harness "integrate exited $status: $(err | tail -n 3)"
    return 0
}
slice_active() { [ "$(systemctl --user show -p ActiveState --value "$slice" 2>/dev/null)" = active ]; }
slice_file() { cat "$slice_dir/$1" 2>/dev/null || echo none; }
# The slice's memory that cannot be given back to the page cache: anonymous
# and shared (what is in swap is zram, the host measure's to see); page
# cache does not count.
slice_held() {
    if [ -r "$slice_dir/memory.stat" ]; then
        awk '$1 == "anon" || $1 == "shmem" { sum += $2; found = 1 } END { if (found) print sum; else print "none" }' \
            "$slice_dir/memory.stat"
    else
        echo none
    fi
}
# check_envelope <name> <engine line>: (1)'s checks of one engine.
check_envelope() {
    exec_id=$(field "$2" 3)
    cg=$(field "$2" 6)
    want="$manager_cg/$slice/$stem-$exec_id.scope"
    [ "$cg" = "$want" ] ||
        measured_fail "(1) $1's engine (execution $exec_id, pid $(field "$2" 5)) runs in $cg (memory.max $(field "$2" 7), memory.swap.max $(field "$2" 8)), not in its own scope $stem-$exec_id.scope inside $slice: an execution has no envelope, and one allocation can take the host's memory"
    [ "$(field "$2" 9)" = "$cg" ] ||
        measured_fail "(1) a child of $1's engine runs in $(field "$2" 9), not in its engine's cgroup $cg: children do not inherit the envelope"
    [ "$(field "$2" 11)" = "$EXEC_HIGH" ] && [ "$(field "$2" 7)" = "$EXEC_MAX" ] && [ "$(field "$2" 8)" = "$EXEC_SWAP" ] ||
        measured_fail "(1) $1's execution $exec_id runs with memory.high $(field "$2" 11), memory.max $(field "$2" 7) and memory.swap.max $(field "$2" 8), not $EXEC_HIGH, $EXEC_MAX and $EXEC_SWAP (SPAWN_EXECUTION_MEMORY_HIGH=92M, _MAX=96M, _SWAP_MAX=16M): an envelope is its MemoryHigh, its MemoryMax and its swap cap (this host's swap is zram, RAM itself)"
}

# --- (1) every execution in its own scope, in the slice ---------------------------

# A resident runs once it has a task: one gated mission, a task each.
for name in hog calm late; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 029 $name work."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$name"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
done
capture "$SPAWN_BIN" mission add --text "029 envelopes" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
assign() { # <resident> <brief>: prints the task's id
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker "$1" --brief "$2"
    [ "$status" -eq 0 ] || harness "task assign to $1 failed: $(err)"
    id=$(jq -r .task "$WORK/out.txt")
    [ -n "$id" ] && [ "$id" != null ] || harness "task assign to $1 printed no id"
    echo "$id"
}
t0=$(now_ms)
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission configure failed: $(err)"
hog_task=$(assign hog "Write your work and keep at it. BB:HOG")
task=$(assign calm "Write your file. BB:WRITE")

within 60 '[ -n "$(engine_line hog)" ] && [ -n "$(engine_line calm)" ]' ||
    harness "the residents' engines did not start within 60 s (engines: $(cat "$WORK/engines.log" | tr '\n' ' '))"
hog1=$(engine_line hog)
calm1=$(engine_line calm)
hog_exec=$(field "$hog1" 3)
hog_pid=$(field "$hog1" 5)
calm_exec=$(field "$calm1" 3)
calm_pid=$(field "$calm1" 5)
echo "029: engines up $(($(field "$hog1" 1) - t0)) ms (hog) and $(($(field "$calm1" 1) - t0)) ms (calm) after the loop started"
echo "029: hog's engine ($hog_exec) in $(field "$hog1" 6), memory.max $(field "$hog1" 7), memory.swap.max $(field "$hog1" 8)"
echo "029: calm's engine ($calm_exec) in $(field "$calm1" 6), memory.max $(field "$calm1" 7), memory.swap.max $(field "$calm1" 8)"
check_envelope hog "$hog1"
check_envelope calm "$calm1"
if slice_active; then
    [ "$(slice_file memory.high)" = "$SLICE_HIGH" ] && [ "$(slice_file memory.max)" = "$SLICE_MAX" ] &&
        [ "$(slice_file cpu.weight)" = "$SLICE_WEIGHT" ] ||
        measured_fail "(1) $slice has memory.high $(slice_file memory.high), memory.max $(slice_file memory.max), cpu.weight $(slice_file cpu.weight); wanted $SLICE_HIGH (128M), $SLICE_MAX (144M) and $SLICE_WEIGHT (the default)"
else
    measured_fail "(1) the slice $slice (SPAWN_SLICE) is not active ($(systemctl --user show -p ActiveState --value "$slice" 2>&1)): Spawn's executions have no slice to share and to yield from"
fi
envelope=$(record_of host.envelope state systemd)
want_envelope="{\"slice\":\"$slice\",\"memory_high\":$SLICE_HIGH,\"memory_max\":$SLICE_MAX,\"cpu_weight\":$SLICE_WEIGHT,\"execution_memory_high\":$EXEC_HIGH,\"execution_memory_max\":$EXEC_MAX,\"execution_memory_swap_max\":$EXEC_SWAP,\"gate_memory_high\":$GATE_HIGH,\"gate_memory_max\":$GATE_MAX,\"gate_memory_swap_max\":$GATE_SWAP}"
if [ -z "$envelope" ]; then
    measured_fail "(1) the loop recorded no host.envelope with state systemd ($(records host.envelope type host.envelope | cut -d'|' -f2- | head -c 300)): the slice and the envelopes it runs under are in no record"
else
    got_envelope=$(printf '%s' "$envelope" | jq -c '{slice, memory_high, memory_max, cpu_weight, execution_memory_high, execution_memory_max, execution_memory_swap_max, gate_memory_high, gate_memory_max, gate_memory_swap_max}')
    [ "$got_envelope" = "$want_envelope" ] ||
        measured_fail "(1) the host.envelope record names $got_envelope, not the run's settings in bytes $want_envelope"
fi

# --- (2) a memory-limit kill ends that execution only -----------------------------

# hog's work is checkpointed first: it is what the resumed execution must find.
within 30 '[ -n "$(git --git-dir="$source" show "refs/heads/spawn/agent_resident_hog:work.txt" 2>/dev/null)" ]' ||
    harness "hog's work.txt did not reach spawn/agent_resident_hog within 30 s"
want_sha=$(git --git-dir="$source" show "refs/heads/spawn/agent_resident_hog:work.txt" | sha256sum | cut -d' ' -f1)
oom_before=$(events_count "$manager_cg/$slice" oom_kill)
calm_cg=$(field "$calm1" 6)
t_go=$(now_ms)
: >"$WORK/go-hog"
within 30 '[ -s "$WORK/alloc.log" ] || ! alive "$hog_pid" || [ -n "$(ended_at "$hog_exec")" ]' ||
    harness "hog's tool neither finished nor ended its execution within 30 s of being told to run"
sleep 1
allocated=$(cat "$WORK/alloc.log")
killed_by=envelope
case "$(field "$allocated" 4)" in
"no cap"*)
    measured_fail "(2) hog's tool found no memory cap ($(field "$allocated" 4)): nothing bounds what an execution and its children allocate"
    killed_by=""
    ;;
survived*)
    measured_fail "(2) hog's tool allocated past its envelope and was not killed ($(field "$allocated" 4)): the envelope does not hold"
    killed_by=""
    ;;
esac
if [ -z "$killed_by" ] && [ -n "$MEASURE" ] && alive "$hog_pid"; then
    # Only to show today's handling of a killed execution: the pid the
    # engine noted, checked to still be that engine.
    case "$(tr '\0' ' ' <"/proc/$hog_pid/cmdline" 2>/dev/null)" in
    *"$WORK/claude"*)
        kill -9 "$hog_pid"
        killed_by=measure
        echo "029: (measure) no envelope to kill hog's tool: hog's engine (pid $hog_pid) killed with SIGKILL instead"
        ;;
    esac
fi
[ -n "$killed_by" ] || fail "(2) hog's execution was not killed; nothing more to judge"
within 20 '! alive "$hog_pid" && [ -n "$(ended_at "$hog_exec")" ]' ||
    measured_fail "(2) hog's execution $hog_exec did not end within 20 s of its memory-limit kill (engine pid $hog_pid $(alive "$hog_pid" && echo alive || echo gone); alloc: $(field "$allocated" 3) $(field "$allocated" 4)): the kill must end the whole execution"
hog_end=$(ended_at "$hog_exec")
[ -n "$hog_end" ] || hog_end=$(now_ms)
echo "029: hog's execution $hog_exec ended $((hog_end - t_go)) ms after its tool was told to run ($(q "select coalesce(cause, '-') || ', exit code ' || coalesce(exit_code::text, 'none') from agent_executions where execution_id = '$hog_exec'"))"
if [ "$killed_by" = envelope ]; then
    oom_after=$(events_count "$manager_cg/$slice" oom_kill)
    [ "$oom_before" != none ] || oom_before=0
    [ "$oom_after" != none ] && [ "$oom_after" -gt "$oom_before" ] ||
        measured_fail "(2) the slice counts no OOM kill (memory.events oom_kill $oom_before -> $oom_after) though hog's execution ended"
fi
alive "$calm_pid" ||
    measured_fail "(2) calm's engine (pid $calm_pid) died with hog's execution: the kill was not confined to hog's envelope"
[ "$(live_execution calm)" = "$calm_exec" ] ||
    measured_fail "(2) calm's execution is now [$(live_execution calm)], not $calm_exec: the sibling was disturbed"
calm_ooms=$(events_count "$calm_cg" oom_kill)
if [ "$killed_by" = envelope ]; then
    [ "$calm_ooms" = 0 ] ||
        measured_fail "(2) calm's scope ($calm_cg) counts oom_kill [$calm_ooms], not 0: the kill reached the sibling, or calm has no scope of its own"
fi

# --- (3) recorded as a memory-limit kill, resumed with its work -------------------

interrupted=""
within 20 'interrupted=$(record_of execution.interrupted execution_id "$hog_exec") && [ -n "$interrupted" ]' || true
if [ -z "$interrupted" ]; then
    measured_fail "(3) no execution.interrupted record for hog's execution $hog_exec within 20 s of its end (its end today: $(q "select 'execution.ended cause ' || coalesce(cause, '-') || ', exit code ' || coalesce(exit_code::text, 'none') from agent_executions where execution_id = '$hog_exec'")): a memory-limit kill is not recorded as one"
else
    echo "029: $interrupted"
    [ "$(printf '%s' "$interrupted" | jq -r .cause)" = memory_limit ] &&
        [ "$(printf '%s' "$interrupted" | jq -r .path)" = resume ] ||
        measured_fail "(3) hog's interruption is recorded cause [$(printf '%s' "$interrupted" | jq -r .cause)] path [$(printf '%s' "$interrupted" | jq -r .path)], not memory_limit and resume"
    [ "$(printf '%s' "$interrupted" | jq -r '.evidence.scope // empty')" = "$stem-$hog_exec.scope" ] &&
        [ "$(printf '%s' "$interrupted" | jq -r '.evidence.memory_max // empty')" = "$EXEC_MAX" ] &&
        [ "$(printf '%s' "$interrupted" | jq -r '(.evidence.oom_kills // 0) > 0')" = true ] ||
        measured_fail "(3) hog's interruption's evidence does not name its scope ($stem-$hog_exec.scope), memory_max $EXEC_MAX and oom_kills > 0: $(printf '%s' "$interrupted" | jq -c '.evidence')"
fi
from_ms=$(record_at execution.interrupted execution_id "$hog_exec")
[ -n "$from_ms" ] || from_ms=$hog_end
within 30 '[ "$(engines_of hog)" -ge 2 ]' ||
    measured_fail "(3) hog was not resumed within 30 s of its interruption (executions of its session: $(q "select string_agg(execution_id || ':' || coalesce(cause, 'live'), ' ' order by started_at_ms) from agent_executions where session_id = 'session_resident_hog'"))"
if [ "$(engines_of hog)" -ge 2 ]; then
    hog2=$(engine_line hog)
    hog2_exec=$(field "$hog2" 3)
    echo "029: hog resumed as $hog2_exec ($(field "$hog2" 4)), its engine up $(($(field "$hog2" 1) - from_ms)) ms after the interruption's record (or end) and $(($(field "$hog2" 1) - hog_end)) ms after the end; in $(field "$hog2" 6); work.txt [$(field "$hog2" 10)]"
    [ "$(field "$hog2" 4)" = resume ] ||
        measured_fail "(3) hog's execution after the kill ($hog2_exec) is a new native session, not its session resumed: the path of a memory-limit kill is resume"
    [ "$(field "$hog2" 10)" = "found $want_sha" ] ||
        measured_fail "(3) hog's resumed workspace does not hold its work: work.txt [$(field "$hog2" 10)], wanted sha256 $want_sha"
    [ "$(field "$hog2" 6)" = "$manager_cg/$slice/$stem-$hog2_exec.scope" ] ||
        measured_fail "(3) hog's resumed execution $hog2_exec runs in $(field "$hog2" 6), not in its own scope $stem-$hog2_exec.scope inside $slice"
fi
if slice_active; then
    within 30 '[ "$(systemctl --user show -p LoadState --value "$stem-$hog_exec.scope" 2>/dev/null)" = not-found ]' ||
        measured_fail "(3) the killed execution's scope $stem-$hog_exec.scope is still loaded ($(systemctl --user show -p ActiveState -p Result "$stem-$hog_exec.scope" 2>&1 | tr '\n' ' ')): every ended execution would leave a unit behind"
fi

# --- (4) admission: an envelope that does not fit waits ----------------------------

: >"$WORK/hold-calm"
# calm's first turn (its report) may still run: its engine looks between turns.
within 45 '[ -s "$WORK/holding-calm" ]' || harness "calm's holder did not start within 45 s"
sleep 1
if slice_active; then
    held=$(slice_held)
    [ "$held" != none ] && [ "$held" -gt "$THRESHOLD" ] ||
        harness "the slice holds $(mib "$held") of anonymous and shared memory while calm holds 56 MiB; the scenario needs more than $(mib "$THRESHOLD")"
    echo "029: the slice holds $(mib "$held") (anonymous and shared) while calm holds 56 MiB; an execution fits under $(mib "$THRESHOLD")"
fi
t_assign=$(now_ms)
late_task=$(assign late "Wait for room. BB:LATE")
waiting=""
within 20 'waiting=$(record_of admission.waiting agent_id agent_resident_late) && [ -n "$waiting" ] || [ -n "$(engine_line late)" ]' || true
if [ -n "$(engine_line late)" ] && [ -z "$waiting" ]; then
    measured_fail "(4) late's engine started $(($(field "$(engine_line late)" 1) - t_assign)) ms after its task was assigned, while calm held 56 MiB of a slice whose limit leaves room for no 96 MiB envelope; no admission.waiting record"
elif [ -z "$waiting" ]; then
    measured_fail "(4) late neither started nor recorded admission.waiting within 20 s of its task's assignment"
else
    echo "029: $waiting"
    [ "$(printf '%s' "$waiting" | jq -r .kind)" = execution ] &&
        [ "$(printf '%s' "$waiting" | jq -r .measure)" = slice ] &&
        [ "$(printf '%s' "$waiting" | jq -r .envelope_bytes)" = "$EXEC_MAX" ] &&
        [ "$(printf '%s' "$waiting" | jq -r .threshold_bytes)" = "$THRESHOLD" ] &&
        [ "$(printf '%s' "$waiting" | jq -r '.measured_bytes > .threshold_bytes')" = true ] ||
        measured_fail "(4) late's admission.waiting does not name kind execution, measure slice, envelope_bytes $EXEC_MAX, threshold_bytes $THRESHOLD and measured_bytes above it: $waiting"
    sleep 3
    [ -z "$(engine_line late)" ] ||
        measured_fail "(4) late's engine started while calm still held its memory, after its admission.waiting"
fi
t_release=$(now_ms)
: >"$WORK/release-calm"
within 30 '[ -n "$(engine_line late)" ]' ||
    measured_fail "(4) late did not start within 30 s of calm letting go of its memory"
if [ -n "$(engine_line late)" ]; then
    late1=$(engine_line late)
    late_exec=$(field "$late1" 3)
    echo "029: late's engine ($late_exec) up $(($(field "$late1" 1) - t_assign)) ms after its task was assigned ($(($(field "$late1" 1) - t_release)) ms from calm letting go); in $(field "$late1" 6)"
    [ "$(field "$late1" 6)" = "$manager_cg/$slice/$stem-$late_exec.scope" ] ||
        measured_fail "(4) late's engine ($late_exec) runs in $(field "$late1" 6), not in its own scope $stem-$late_exec.scope inside $slice"
    if [ -n "$waiting" ]; then
        admitted=$(record_of admission.admitted agent_id agent_resident_late)
        [ -n "$admitted" ] &&
            [ "$(printf '%s' "$admitted" | jq -r --argjson w $((t_release - $(record_at admission.waiting agent_id agent_resident_late))) '(.waited_ms // -1) >= $w')" = true ] ||
            measured_fail "(4) late started with no admission.admitted record whose waited_ms covers its wait: [$admitted]"
    fi
fi

# --- (5) a gate in its own scope, killed by its envelope ------------------------------

# The first task.gated record (not `stale`) of task $1 after event $2:
# `<event id>|<at ms>|<record>`.
gated_after() {
    sql "select e.id || '|' || e.recorded_at_ms || '|' || (e.kind -> 'detail' -> 'record')::text
           from spawn_events e
          where e.session_id = 'session_control'
            and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
            and e.kind -> 'detail' -> 'record' ->> 'id' = '$1'
            and coalesce(e.kind -> 'detail' -> 'record' ->> 'verdict', '') <> 'stale'
            and e.id > $2
          order by e.id limit 1"
}
mission_col() { q "select coalesce($1::text, '') from mission_missions where id = $mission"; }
# gate_scope_n <cgroup>: the n of $stem-gate-$task-<n>.scope directly in the
# slice, or nothing.
gate_scope_n() {
    case "$1" in
    "$manager_cg/$slice/$stem-gate-$task-"*.scope)
        n=${1#*"$stem-gate-$task-"}
        n=${n%%.scope*}
        case "$n" in '' | *[!0-9]*) ;; *) echo "$n" ;; esac
        ;;
    esac
}

within 60 '[ "$(q "select state from mission_tasks where id = $task")" = reported ]' ||
    harness "calm's task #$task was not reported within 60 s (state $(q "select state from mission_tasks where id = $task"))"
before_gate=$(q "select coalesce(max(id), 0) from spawn_events")
aborts_before=$(mission_col gate_aborts)
rejections_before=$(mission_col gate_rejections)
revisions_before=$(q "select revisions from mission_tasks where id = $task")
deadline=$(($(now) + 90))
while [ ! -s "$WORK/gate.log" ]; do
    [ "$(now)" -lt "$deadline" ] ||
        harness "task #$task was not gated within 90 s (state $(q "select state from mission_tasks where id = $task"))"
    integrate_once
    sleep 0.5
done
judged=$(head -n 1 "$WORK/gate.log")
scope1=$(basename "$(field "$judged" 2)")
n1=$(gate_scope_n "$(field "$judged" 2)")
echo "029: task #$task's gate (run 1) ran in $(field "$judged" 2), memory.high $(field "$judged" 5), memory.max $(field "$judged" 3), memory.swap.max $(field "$judged" 4)"
[ -n "$n1" ] ||
    measured_fail "(5) task #$task's gate runs in $(field "$judged" 2), not in its own scope $stem-gate-$task-<n>.scope inside $slice"
[ "$(field "$judged" 5)" = "$GATE_HIGH" ] && [ "$(field "$judged" 3)" = "$GATE_MAX" ] && [ "$(field "$judged" 4)" = "$GATE_SWAP" ] ||
    measured_fail "(5) task #$task's gate runs with memory.high $(field "$judged" 5), memory.max $(field "$judged" 3) and memory.swap.max $(field "$judged" 4), not $GATE_HIGH, $GATE_MAX and $GATE_SWAP (SPAWN_GATE_MEMORY_HIGH=30M, _MAX=32M, _SWAP_MAX=8M)"

# --- (6) a gate its envelope killed is a host fault, judged again ---------------------

# Passes every half second until the judging is recorded (the gate's tool
# needs a few seconds to reach its MemoryMax through its MemoryHigh).
gated=""
deadline=$(($(now) + 30))
while :; do
    gated=$(gated_after "$task" "$before_gate")
    [ -n "$gated" ] && break
    [ "$(now)" -lt "$deadline" ] || break
    sleep 0.5
    integrate_once
done
[ -n "$gated" ] ||
    fail "(6) task #$task's first gate run recorded no verdict within 30 s (its tool: $(cut -d'|' -f3- "$WORK/gate-alloc.log" | tr '\n' ' '); integrate: $(tail -n 3 "$WORK/integrate.log" | tr '\n' ' '))"
g1_id=$(printf '%s' "$gated" | cut -d'|' -f1)
g1_at=$(printf '%s' "$gated" | cut -d'|' -f2)
g1=$(printf '%s' "$gated" | cut -d'|' -f3-)
g1_verdict=$(printf '%s' "$g1" | jq -r '.verdict // empty')
echo "029: run 1's tool: $(cut -d'|' -f3- "$WORK/gate-alloc.log" | tr '\n' ' '); gated '$g1_verdict' ($(printf '%s' "$g1" | jq -r '.reason // empty' | head -c 200)); gate_aborts $aborts_before -> $(mission_col gate_aborts)"
if [ "$g1_verdict" != host_fault ]; then
    measured_fail "(6) task #$task's gate, killed by its memory limit, was gated '$g1_verdict' ($(printf '%s' "$g1" | jq -r '.reason // empty' | head -c 200)), not 'host_fault'; gate_aborts $aborts_before -> $(mission_col gate_aborts): a gate its envelope kills is the host's fault, never an abort"
else
    [ "$(printf '%s' "$g1" | jq -r '.evidence // empty')" = memory_limit ] &&
        [ "$(printf '%s' "$g1" | jq -r '.scope // empty')" = "$scope1" ] &&
        [ "$(printf '%s' "$g1" | jq -r '.memory_max // empty')" = "$GATE_MAX" ] &&
        [ "$(printf '%s' "$g1" | jq -r '(.oom_kills // 0) > 0')" = true ] ||
        measured_fail "(6) the host fault does not name evidence memory_limit, the gate's scope $scope1, memory_max $GATE_MAX and oom_kills > 0: $(printf '%s' "$g1" | jq -c '{evidence, scope, memory_high, memory_max, oom_kills, memory_peak}')"
    [ "$(printf '%s' "$g1" | jq -r '.backoff_ms // empty')" = $((BACKOFF_SECS * 1000)) ] &&
        [ "$(printf '%s' "$g1" | jq -r '.due_ms // empty')" = $((g1_at + BACKOFF_SECS * 1000)) ] ||
        measured_fail "(6) the host fault's backoff_ms [$(printf '%s' "$g1" | jq -r '.backoff_ms // empty')] and due_ms [$(printf '%s' "$g1" | jq -r '.due_ms // empty')] are not $((BACKOFF_SECS * 1000)) and its time + $((BACKOFF_SECS * 1000)) ($((g1_at + BACKOFF_SECS * 1000)))"
fi
[ "$(mission_col gate_aborts)" = "$aborts_before" ] && [ "$(mission_col gate_rejections)" = "$rejections_before" ] ||
    measured_fail "(6) gate_aborts $aborts_before -> $(mission_col gate_aborts), gate_rejections $rejections_before -> $(mission_col gate_rejections) after a gate its envelope killed: it must count toward no halt"
[ "$(q "select state from mission_tasks where id = $task")" = reported ] &&
    [ "$(q "select revisions from mission_tasks where id = $task")" = "$revisions_before" ] ||
    measured_fail "(6) task #$task is $(q "select state || ', revisions ' || revisions from mission_tasks where id = $task") after its gate was killed by its memory limit; wanted reported, revisions $revisions_before"
[ -z "$(mission_col halted)" ] ||
    measured_fail "(6) the mission halted after a gate its envelope killed: [$(mission_col halted)]"
due=$(printf '%s' "$g1" | jq -r '.due_ms // empty')
deadline=$(($(now) + 60))
while [ "$(wc -l <"$WORK/gate.log" | tr -d ' ')" -lt 2 ]; do
    [ "$(now)" -lt "$deadline" ] ||
        harness "task #$task's gate did not run again within 60 s of its first judging (state $(q "select state from mission_tasks where id = $task"))"
    integrate_once
    sleep 0.5
done
judged2=$(sed -n 2p "$WORK/gate.log")
ran2=$(field "$judged2" 1)
n2=$(gate_scope_n "$(field "$judged2" 2)")
echo "029: task #$task's gate ran again $((ran2 - g1_at)) ms after its first judging was recorded, in $(field "$judged2" 2)"
case "$due" in
'' | *[!0-9]*) measured_fail "(6) task #$task's gate ran again $((ran2 - g1_at)) ms after its first judging, with no due time to wait for" ;;
*) [ "$ran2" -ge "$due" ] || measured_fail "(6) task #$task's gate ran again at $ran2 ms, before its host fault's due time $due ms" ;;
esac
if [ -n "$n1" ]; then
    [ -n "$n2" ] && [ "$n2" -gt "$n1" ] ||
        measured_fail "(6) task #$task's second gate runs in $(field "$judged2" 2), not in a scope $stem-gate-$task-<n>.scope of a larger n than the first ($n1)"
fi
gated2=""
within 30 'gated2=$(gated_after "$task" "$g1_id") && [ -n "$gated2" ]' || true
[ "$(printf '%s' "$gated2" | cut -d'|' -f3- | jq -r '.verdict // empty' 2>/dev/null)" = accepted ] ||
    measured_fail "(6) task #$task's second gate run was gated [$(printf '%s' "$gated2" | cut -d'|' -f3- | jq -r '.verdict // empty' 2>/dev/null)], not accepted"

if [ -n "$failed_any" ]; then
    asserted=1
    exit 1
fi
echo "029: every execution and the gate ran in their own scopes in $slice; the memory-limit kill ended hog's execution only, was recorded and resumed with its work; late waited for room and started once it fit; the gate its envelope killed was a host fault, judged again after its backoff"
exit 0
