#!/bin/sh
# Black box: a runner judges a copy of what the primary judged, beside it,
# and decides nothing. Gates are the fleet's bottleneck (48 h to 2026-11-09:
# 112 gates, mean 17 min, max 101; a reported task waited about 5 h for its
# gate), and a second host can judge (deploy/gate-runner.sh ships a gate run
# to a runner and brings its verdict back), but before a runner's verdict may
# decide a landing it must be shown to agree with the primary's on the same
# immutable inputs (milestone M2, /var/tmp/spawn/sync/design-code-state-v2.md
# and design-trusted-change-integration.md). Objective 035 (every verdict
# binds its `inputs` and `env`) is a prerequisite: this scenario reads the
# primary's `inputs` from its `task.gated`. The dispatcher is the
# controller's own deploy/gate-runner.sh, as it is once the runner-env
# change is in the controller tree; this scenario copies it, with
# deploy/gate-runner/, into its controller tree and installs a fake runner
# with its setup.sh, as deploy/test-gate-runner.sh does.
#
# Required: with SPAWN_GATE_SHADOW_RUNNER set (the runner's name for the
# transport) and deploy/gate-runner.sh present in the controller (the
# directory two levels above the mission's gate directory), after the
# primary records an accepted or rejected verdict that a gate run gave, the
# integrator dispatches the same base and candidate with the scenario set
# to the runner in the background (`<controller>/deploy/gate-runner.sh
# <runner> <the gate's repository> <base> <candidate> <gate directory>
# <out>`, in the integrator's own environment), and records its end once:
# `task.shadow_gated` {`id`, `revision`, `base`, `candidate`, `runner`,
# `result` (the dispatcher's record.json `result`, or `refused` when it
# wrote none), `verdict` (accepted, rejected, or null), `exit` (the
# dispatcher's), `seconds`, `inputs` (035's object for what the runner was
# given: base, candidate, controller, scenarios_sha256 of the scenario set
# the runner received, scenario: null, the dispatcher ships no task
# scenario), `env` {host: null, toolchain: the `| base cache: toolchain`
# line of the runner's gate log, runner: the runner}, `primary` (the
# primary's verdict), `primary_env`, `unequal` (the names of the `inputs`
# fields whose values differ from the primary's, [] when none), `agrees`
# (true or false only when `unequal` is empty and both are verdicts, else
# null)}. It never changes a task's state, a landing, a halt or a count, and
# a landing never waits for it. A record with `agrees` false is told to the
# supervisor once (a digest item naming the task, both verdicts, the runner
# and both environments).
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, the
# supervisor, no model judge) runs one resident, `shadow`, of one gated
# mission (accept = gate) on a fake engine. The primary's gate (SPAWN_GATE)
# and the runner's (the controller tree's scripts/rsi/gate.sh, which the
# dispatcher ships) are recording fakes that print what scripts/rsi/gate.sh
# prints; each notes its arguments, and the runner's the digest of the
# scenario set it received. The runner is a directory under $HOME set up by
# the dispatcher's setup.sh and an `ssh` stub that runs the command here.
#   (0) task A (one.txt), the integrator without SPAWN_GATE_SHADOW_RUNNER:
#       accepted, lands; no `task.shadow_gated`, the runner never called.
#   Then one integrator with SPAWN_GATE_SHADOW_RUNNER, never restarted:
#   (1) task B (two.txt): accepted on both; the runner's gate holds until
#       released. B's shadow reaches the runner within 60 s of its verdict,
#       and B lands while the runner's gate is held and before any shadow
#       record (a landing never waits); released, the record: `inputs` what
#       the runner's gate was given and received, and equal to the
#       primary's, `unequal` [], `agrees` true, the runner's toolchain.
#   (2) task C (three.txt): accepted on the primary, rejected on the runner.
#       C lands all the same (the runner decides nothing), gate_rejections
#       unchanged; `agrees` false; the supervisor gets a digest naming #C,
#       the runner and `shadow` within 60 s.
#   (3) task D (four.txt): the primary's gate adds a file to the gate
#       directory while it runs, so the runner receives another scenario
#       set: `inputs` what the runner received; when its scenarios_sha256 is
#       not the primary's, `unequal` names it and `agrees` is null (never a
#       comparison of verdicts on different inputs); D lands.
#   (4) after 20 s more, the supervisor was told of C's disagreement once,
#       of B's and D's never.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (984d059) and on objective 035's
# reference implementation alone: no shadow reaches the runner. It passes
# once judged verdicts are shadowed as above.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "036: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
# The dispatcher as the controller ships it (the runner-env change).
for file in deploy/gate-runner.sh deploy/gate-runner/remote.sh deploy/gate-runner/setup.sh; do
    [ -f "$controller/$file" ] || harness "no $controller/$file: this controller has no gate runner to shadow on"
done
pg_bin=$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -n 1)
[ -n "$pg_bin" ] && [ -x "$pg_bin/postgres" ] ||
    harness "no PostgreSQL programs (/usr/lib/postgresql/*/bin): the runner's setup needs a private server named"
for tool in psql tmux git jq od bash sed grep awk date sha256sum sort find uname tar gzip timeout nice bwrap nproc; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_036_shadow_$run"
SPAWN_TMUX_SOCKET="bb-036-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL
# The runner's root: under $HOME, as setup.sh wants it (the gate's sandbox
# empties /tmp, /run and /var/tmp), short (its jobs' scratch is under it).
runner_root="$HOME/.bb036-$(od -An -N4 -tx4 /dev/urandom | tr -d ' ')"
TEST_URL=$SPAWN_TEST_DATABASE_URL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
integrator_pid=""
cleanup() {
    # The runner's gate, if one is held, ends on its release file.
    : >"$WORK/release-two" 2>/dev/null || true
    if [ -n "$integrator_pid" ]; then
        kill "$integrator_pid" 2>/dev/null || true
        wait "$integrator_pid" 2>/dev/null || true
    fi
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
    fi
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" "$runner_root" 2>/dev/null || true
    rm -rf "$WORK" "$runner_root"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "036: the loop's last lines:" >&2
            tail -n 12 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "036: the integrator's last lines:" >&2
            grep -v '^\s*$' "$WORK/integrate.log" | grep -v '^| ' | tail -n 12 >&2
        fi
        if [ -s "$WORK/judged.log" ]; then
            echo "036: the primary's gate runs (time_ms name base candidate):" >&2
            cat "$WORK/judged.log" >&2
        fi
        if [ -s "$WORK/runner.log" ]; then
            echo "036: the runner's gate runs (time_ms name base candidate scenarios_sha256):" >&2
            cat "$WORK/runner.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "036: harness: unexpected exit $rc" >&2
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
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || fail "$3: expected [$2], got [$1]"
}


# --- the world ----------------------------------------------------------------

cat >"$WORK/gitconfig" <<'GITCONFIG'
[user]
	name = bb
	email = bb@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
GITCONFIG
GIT_CONFIG_GLOBAL=$WORK/gitconfig
export GIT_CONFIG_GLOBAL

repo=$WORK/repo.git
source=$WORK/source.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "base"
for bare in "$repo" "$source"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

# A controller tree: its `.controller` names a commit, its rsi/scenarios is
# the mission's gate directory, its deploy/ the controller's dispatcher and
# its scripts/rsi/gate.sh the runner's gate (the dispatcher ships the
# scripts/ two levels above the gate directory).
ctl=$WORK/ctl
gate=$ctl/rsi/scenarios
mkdir -p "$gate/sub" "$ctl/deploy/gate-runner" "$ctl/scripts/rsi"
controller_commit=$(printf 'bb-036-controller' | sha1sum | cut -c1-40)
printf '%s\n' "$controller_commit" >"$ctl/.controller"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
printf 'a file in a subdirectory: part of the set\n' >"$gate/sub/notes"
cp "$controller/deploy/gate-runner.sh" "$ctl/deploy/gate-runner.sh"
cp "$controller/deploy/gate-runner/remote.sh" "$controller/deploy/gate-runner/setup.sh" "$ctl/deploy/gate-runner/"
: >"$WORK/judged.log"
: >"$WORK/runner.log"
: >"$WORK/ssh.log"

# The scenario set's digest, as objective 035 defines it, of directory $1.
digest_of() {
    (cd "$1" && find . -type f | sed 's|^\./||' | LC_ALL=C sort | while IFS= read -r path; do
        sha256sum "$path"
    done) | sha256sum | cut -d' ' -f1
}

toolchain="rustc 1.95.0-bb cargo 1.95.0-bb psql 16.bb server 160099"
runner_toolchain="rustc 1.95.0-bb-runner cargo 1.95.0-bb-runner psql 16.bb server 160098"

# The primary's recording gate. The newest file decides (the resident's
# branch keeps the earlier ones): four.txt adds a file to the gate directory
# while it runs (3), everything else is accepted.
cat >"$WORK/fake-gate.sh" <<GATE
#!/bin/sh
has() { git --git-dir="\$1" cat-file -e "\$2:\$3" 2>/dev/null; }
note() { printf '%s %s %s %s\n' "\$(date +%s%3N)" "\$1" "\$2" "\$3" >>'$WORK/judged.log'; }
echo "| == setup"
echo "| base cache: toolchain $toolchain"
echo "| stage gate ok 10512ms base: build"
if has "\$1" "\$3" four.txt; then
    note task-d "\$2" "\$3"
    printf '#!/bin/sh\nexit 0\n' >'$gate/check-four.sh'
elif has "\$1" "\$3" three.txt; then
    note task-c "\$2" "\$3"
elif has "\$1" "\$3" two.txt; then
    note task-b "\$2" "\$3"
elif has "\$1" "\$3" one.txt; then
    note task-a "\$2" "\$3"
else
    note other "\$2" "\$3"
fi
echo "| stage gate ok 900ms candidate: build"
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
exit 0
GATE
chmod 755 "$WORK/fake-gate.sh"

# The runner's recording gate, shipped in the job (scripts/rsi/gate.sh
# <repo> <base> <candidate> <scenarios>): it notes its arguments and the
# digest of the scenario set it received; two.txt holds until released (at
# most 150 s), three.txt is rejected, the rest accepted. The verdict names
# the commits, as the gate's does (the dispatcher checks them).
cat >"$ctl/scripts/rsi/gate.sh" <<GATE
#!/bin/sh
has() { git --git-dir="\$1" cat-file -e "\$2:\$3" 2>/dev/null; }
digest() {
    (cd "\$1" && find . -type f | sed 's|^\./||' | LC_ALL=C sort | while IFS= read -r path; do
        sha256sum "\$path"
    done) | sha256sum | cut -d' ' -f1
}
note() { printf '%s %s %s %s %s\n' "\$(date +%s%3N)" "\$1" "\$2" "\$3" "\$(digest "\$4")" >>'$WORK/runner.log'; }
echo "| == setup"
echo "| base cache: toolchain $runner_toolchain"
verdict=accepted
if has "\$1" "\$3" four.txt; then
    note task-d "\$2" "\$3" "\$4"
elif has "\$1" "\$3" three.txt; then
    note task-c "\$2" "\$3" "\$4"
    verdict=rejected
elif has "\$1" "\$3" two.txt; then
    note task-b "\$2" "\$3" "\$4"
    : >'$WORK/held-two'
    n=0
    until [ -e '$WORK/release-two' ] || [ "\$n" -ge 750 ]; do
        sleep 0.2
        n=\$((n + 1))
    done
else
    note other "\$2" "\$3" "\$4"
fi
json="{\"verdict\":\"\$verdict\",\"base\":\"\$2\",\"candidate\":\"\$3\"}"
printf '%s\n' "\$json" >"\$SPAWN_GATE_VERDICT"
echo "\$json"
exit 0
GATE
chmod 755 "$ctl/scripts/rsi/gate.sh"

# The transport: an ssh stand-in that drops the options and the host and
# runs the command string here, as ssh's remote shell would, noting it.
cat >"$WORK/ssh" <<SSH
#!/bin/sh
while [ \$# -gt 0 ]; do
    case \$1 in
    -o) shift 2 ;;
    -*) shift ;;
    *) break ;;
    esac
done
[ \$# -gt 1 ] || exit 255
shift
printf '%s %s\n' "\$(date +%s%3N)" "\$*" >>'$WORK/ssh.log'
exec sh -c "\$*"
SSH
chmod 755 "$WORK/ssh"

# The runner, installed by the controller's own setup.sh (no registry to
# sync, no memory floor, an empty cargo home: the fake gate builds nothing).
mkdir -p "$WORK/runner-cargo"
GATE_RUNNER_ROOT=$runner_root sh "$ctl/deploy/gate-runner/setup.sh" --root "$runner_root" \
    --pg-bin "$pg_bin" --jobs 1 --min-free-gb 0 --nice 0 --cargo-home "$WORK/runner-cargo" \
    >"$WORK/setup.out" 2>&1 || harness "the runner's setup.sh failed: $(tail -n 3 "$WORK/setup.out" | tr '\n' ' ')"

# The fake engine: a text naming `BB:WRITE:<file>` writes `<file>.txt`, waits
# for the pusher to publish it (at most 30 s), then writes the report.
# Anything else is a turn that changes nothing.
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
job=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:WRITE:"*)
      job=${typed#*BB:WRITE:}
      job=${job%%[!a-z]*}
      ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$job" ]; then
      file=$job.txt
      printf '%s wrote %s\n' "$name" "$job" >"$file"
      turn_ends "wrote $file"
      start=$(date +%s)
      until [ "$(git --git-dir="$BB_WORK/source.git" show "refs/heads/spawn/$agent:$file" 2>/dev/null)" = "$(cat "$file")" ]; do
        [ $(($(date +%s) - start)) -ge 30 ] && break
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
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_GATE_MIN_FREE_GB \
    SPAWN_GATE_MIN_FREE_MEMORY_MB SPAWN_INTEGRATE_GATES SPAWN_TEST_DATABASE_LOG \
    SPAWN_GATE_HOST_FAULT_BACKOFF_SECS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_INTEGRATE_WARM \
    SPAWN_GATE_SHADOW_RUNNER SPAWN_GATE_SHADOW_EVERY GATE_RUNNER_SSH GATE_RUNNER_REMOTE_BIN \
    GATE_RUNNER_ROOT GATE_RUNNER_SHORT GATE_RUNNER_REGISTRY GATE_RUNNER_RETRIES GATE_RUNNER_ENV_FILE \
    GATE_RUNNER_MIN_FREE_MEMORY_MB GATE_RUNNER_RUN_TIMEOUT GATE_RUNNER_TOTAL_TIMEOUT; do
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
SPAWN_SUPERVISOR=1
SPAWN_MISSION_FILE=$WORK/mission.md
# The stall bounds out of the way: nothing here is about stalls.
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
# No base warm after the landing: its gate runs would be noise in judged.log.
SPAWN_INTEGRATE_WARM=0
BACKOFF_SECS=2
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_SUPERVISOR \
    SPAWN_MISSION_FILE SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_INTEGRATE_WARM
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"
: >"$SPAWN_MISSION_FILE"
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
rejections() { q "select gate_rejections from mission_missions where id = $mission"; }
halted() { q "select coalesce(halted, '') from mission_missions where id = $mission"; }
tasks_line() { q "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks"; }
log_tail() { grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 6 | tr '\n' ' '; }
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }
# A field of event $1's record, as text ('' when absent; JSON null is '').
field() { q "select coalesce(kind -> 'detail' -> 'record' ->> '$2', '') from spawn_events where id = $1"; }
# A path into event $1's record (jsonb #>> '{a,b}'), as text; `null` for JSON
# null, `absent` when the path does not exist.
path() {
    q "select case when kind -> 'detail' -> 'record' #> '{$2}' is null then 'absent'
                   when jsonb_typeof(kind -> 'detail' -> 'record' #> '{$2}') = 'null' then 'null'
                   else kind -> 'detail' -> 'record' #>> '{$2}' end
         from spawn_events where id = $1"
}

waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$1'")" = WAITING ] &&
        [ "$(q "select count(*) from agent_executions
                where session_id = '$1' and ended_at_ms is null")" -ge 1 ]
}


# The primary's gate run named $1 (judged.log), or the runner's (runner.log):
# its base, candidate, and for the runner the scenario set's digest it got.
run_field() { awk -v name="$2" -v col="$3" '$2 == name { print $col; exit }' "$WORK/$1"; }

# The integrator: the dispatcher's environment (its transport, the runner's
# helper, no registry sync, no memory floor, no retry) whatever the case;
# SPAWN_GATE_SHADOW_RUNNER only when $1 is `shadow`.
start_integrator() {
    if [ "$1" = shadow ]; then set -- SPAWN_GATE_SHADOW_RUNNER=bb-runner; else set -- BB_036=none; fi
    env "$@" SPAWN_GATE="$WORK/fake-gate.sh" SPAWN_TEST_DATABASE_URL="$TEST_URL" \
        SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        GATE_RUNNER_SSH="$WORK/ssh" GATE_RUNNER_REMOTE_BIN="$runner_root/bin/remote" \
        GATE_RUNNER_SHORT="$runner_root/s" GATE_RUNNER_REGISTRY= GATE_RUNNER_RETRIES=0 \
        GATE_RUNNER_MIN_FREE_MEMORY_MB=0 GATE_RUNNER_RUN_TIMEOUT=300 GATE_RUNNER_TOTAL_TIMEOUT=600 \
        "$SPAWN_BIN" integrate --root "$WORK/integrator" >>"$WORK/integrate.log" 2>&1 &
    integrator_pid=$!
}
stop_integrator() {
    kill "$integrator_pid" 2>/dev/null || true
    wait "$integrator_pid" 2>/dev/null || true
    integrator_pid=""
}
integrator_alive() { kill -0 "$integrator_pid" 2>/dev/null; }

# The next gate verdict (not `stale`) on task `$1` after event `$2`, within
# 90 s: sets `event` and `verdict`.
judge_next() {
    deadline=$(($(now) + 90))
    while :; do
        found=$(q "select e.id || ' ' || (e.kind -> 'detail' -> 'record' ->> 'verdict') from spawn_events e
                    where e.session_id = 'session_control'
                      and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                      and e.kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale'
                      and e.kind -> 'detail' -> 'record' ->> 'id' = '$1' and e.id > $2
                    order by e.id limit 1")
        if [ -n "$found" ]; then
            set -- $found
            event=$1
            verdict=$2
            return 0
        fi
        integrator_alive || harness "the integrator exited while task #$1 waited for a verdict: $(log_tail)"
        [ -z "$(halted)" ] || harness "the mission halted before task #$1 was judged: $(halted)"
        [ "$(now)" -lt "$deadline" ] ||
            harness "task #$1 got no verdict within 90 s (tasks: $(tasks_line); integrate: $(log_tail))"
        sleep 0.3
    done
}

# The shadow records of task $1 (after event $2): their count, and the first.
shadows() {
    q "select count(*) from spawn_events where session_id = 'session_control'
         and kind -> 'detail' -> 'record' ->> 'type' = 'task.shadow_gated'
         and kind -> 'detail' -> 'record' ->> 'id' = '$1' and id > $2"
}
any_shadows() {
    q "select count(*) from spawn_events where session_id = 'session_control'
         and kind -> 'detail' -> 'record' ->> 'type' = 'task.shadow_gated'"
}
# shadow_of <task> <after> <seconds> <label>: sets `shadow` to the first
# shadow record's event, or fails.
shadow_of() {
    deadline=$(($(now) + $3))
    while [ "$(shadows "$1" "$2")" -lt 1 ]; do
        [ "$(now)" -lt "$deadline" ] ||
            fail "$4 no task.shadow_gated for task #$1 within $3 s of its verdict"
        integrator_alive || harness "the integrator exited while task #$1's shadow ran: $(log_tail)"
        sleep 0.3
    done
    shadow=$(q "select min(id) from spawn_events where session_id = 'session_control'
                 and kind -> 'detail' -> 'record' ->> 'type' = 'task.shadow_gated'
                 and kind -> 'detail' -> 'record' ->> 'id' = '$1' and id > $2")
}
until_landed() { # <task> <label>
    deadline=$(($(now) + 60))
    until [ "$(task_state "$1")" = landed ]; do
        [ "$(now)" -lt "$deadline" ] ||
            fail "$2 task #$1 was accepted by the primary but did not land within 60 s (state $(task_state "$1"))"
        integrator_alive || harness "the integrator exited before task #$1 landed: $(log_tail)"
        sleep 0.3
    done
}
# Digests delivered to the supervisor that tell of task $1's shadow: they
# name #<task>, the runner and the word shadow.
told_shadow() {
    q "select count(*) from spawn_commands c
        where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
          and c.action ->> 'type' = 'input' and c.succeeded
          and c.action ->> 'text' ~ '#$1([^0-9]|\$)' and c.action ->> 'text' ~* 'shadow'
          and c.action ->> 'text' ~ 'bb-runner'"
}

# check_shadow <shadow event> <primary event> <label> <runner run name>
# <runner verdict> <primary verdict>: the record's fields; `inputs` is what
# the runner's gate was given and received. Sets `unequal_want`, the input
# fields that differ from the primary's, as the record must name them.
check_shadow() {
    s=$1 p=$2 label=$3
    expect_eq "$(path "$s" id)" "$(path "$p" id)" "$label the shadow's id"
    expect_eq "$(path "$s" revision)" "$(path "$p" revision)" "$label the shadow's revision"
    expect_eq "$(path "$s" runner)" bb-runner "$label the shadow's runner"
    expect_eq "$(path "$s" result)" verdict "$label the shadow's result (the dispatcher's)"
    expect_eq "$(path "$s" exit)" 0 "$label the shadow's exit (the dispatcher's)"
    expect_eq "$(path "$s" verdict)" "$5" "$label the shadow's verdict (the runner's gate)"
    expect_eq "$(path "$s" primary)" "$6" "$label the shadow's primary verdict"
    case "$(path "$s" seconds)" in
    '' | absent | null | *[!0-9]*) fail "$label the shadow's seconds: expected a whole number, got [$(path "$s" seconds)]" ;;
    esac
    [ -n "$(run_field runner.log "$4" 3)" ] || fail "$label the runner's gate never ran for $4"
    expect_eq "$(path "$s" base)" "$(path "$p" inputs,base)" "$label the shadow's base (the primary's inputs.base)"
    expect_eq "$(path "$s" candidate)" "$(path "$p" inputs,candidate)" "$label the shadow's candidate (the primary's inputs.candidate)"
    expect_eq "$(path "$s" inputs,base)" "$(run_field runner.log "$4" 3)" "$label the shadow's inputs.base (what the runner's gate was given)"
    expect_eq "$(path "$s" inputs,candidate)" "$(run_field runner.log "$4" 4)" "$label the shadow's inputs.candidate (what the runner's gate was given)"
    expect_eq "$(path "$s" inputs,scenarios_sha256)" "$(run_field runner.log "$4" 5)" "$label the shadow's inputs.scenarios_sha256 (the set the runner received)"
    expect_eq "$(path "$s" inputs,controller)" "$controller_commit" "$label the shadow's inputs.controller"
    expect_eq "$(path "$s" inputs,scenario)" null "$label the shadow's inputs.scenario (the dispatcher ships no task scenario)"
    expect_eq "$(path "$s" env,runner)" bb-runner "$label the shadow's env.runner"
    expect_eq "$(path "$s" env,toolchain)" "$runner_toolchain" "$label the shadow's env.toolchain (the runner's gate log)"
    expect_eq "$(path "$s" primary_env,toolchain)" "$(path "$p" env,toolchain)" "$label the shadow's primary_env (the primary's env)"
    expect_eq "$(q "select coalesce(jsonb_typeof(kind -> 'detail' -> 'record' -> 'unequal'), 'absent')
                      from spawn_events where id = $s")" array "$label the shadow's unequal (a list)"
    unequal_want=$(q "select coalesce(jsonb_agg(f order by f), '[]'::jsonb)::text
                        from unnest(array['base','candidate','controller','scenarios_sha256','scenario']) f
                       where (select kind -> 'detail' -> 'record' -> 'inputs' -> f from spawn_events where id = $s)
                             is distinct from
                             (select kind -> 'detail' -> 'record' -> 'inputs' -> f from spawn_events where id = $p)")
    expect_eq "$(q "select (select coalesce(jsonb_agg(f order by f), '[]'::jsonb)
                              from jsonb_array_elements_text(kind -> 'detail' -> 'record' -> 'unequal') f)::text
                      from spawn_events where id = $s")" "$unequal_want" "$label the shadow's unequal (the inputs fields unlike the primary's)"
}

# --- one mission, one resident, the supervisor waiting ----------------------------

capture "$SPAWN_BIN" mission add --text "036 shadow judging" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" resident propose --name shadow --charter "Owns the 036 change."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name shadow
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
started=$(now)
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission configure exited $status: $(err)"

gated_since() {
    [ "$(q "select count(*) from spawn_events where session_id = 'session_control'
              and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
              and kind -> 'detail' -> 'record' ->> 'id' = '$1' and id > $2")" -ge 1 ]
}
# assign_and_report <file name>: sets `task` and `assigned_at` (the last
# event before the assignment). A running integrator may judge the report
# the moment it comes, so the wait ends at the report or its first verdict.
assign_and_report() {
    assigned_at=$(last_event)
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker shadow \
        --brief "Write your file. BB:WRITE:$1"
    [ "$status" -eq 0 ] || harness "task assign failed: $(err)"
    task=$(jq -r .task "$WORK/out.txt")
    [ -n "$task" ] && [ "$task" != null ] || harness "task assign printed no id: $(out)"
    deadline=$(($(now) + 120))
    until [ "$(task_state "$task")" = reported ] || gated_since "$task" "$assigned_at"; do
        [ "$(now)" -lt "$deadline" ] ||
            harness "task #$task was not reported within 120 s (tasks: $(tasks_line))"
        sleep 0.3
    done
    git --git-dir="$source" cat-file -e "refs/heads/spawn/agent_resident_shadow:$1.txt" 2>/dev/null ||
        harness "shadow's pushed branch does not hold $1.txt"
}

deadline=$(($(now) + 120))
until waiting session_supervisor; do
    [ "$(now)" -lt "$deadline" ] || harness "the supervisor was never judged waiting"
    sleep 0.5
done

# --- (0) no runner named: no shadow ------------------------------------------------

start_integrator plain
assign_and_report one
task_a=$task
judge_next "$task_a" "$assigned_at"
[ "$verdict" = accepted ] || harness "task #$task_a was gated '$verdict' ($(field "$event" reason))"
until_landed "$task_a" "(0)"
sleep 5
expect_eq "$(any_shadows)" 0 "(0) task.shadow_gated records without SPAWN_GATE_SHADOW_RUNNER"
[ ! -s "$WORK/ssh.log" ] || fail "(0) the runner was called without SPAWN_GATE_SHADOW_RUNNER: $(head -n 2 "$WORK/ssh.log")"
stop_integrator

# --- (1) agreement, and a landing that never waits for its shadow ------------------

start_integrator shadow
assign_and_report two
task_b=$task
judge_next "$task_b" "$assigned_at"
eb=$event
[ "$verdict" = accepted ] || harness "task #$task_b was gated '$verdict' ($(field "$eb" reason))"
deadline=$(($(now) + 60))
until [ -e "$WORK/held-two" ]; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(1) no shadow of task #$task_b's verdict reached the runner within 60 s (runner calls: $(wc -l <"$WORK/ssh.log"))"
    integrator_alive || harness "the integrator exited before task #$task_b's shadow: $(log_tail)"
    sleep 0.3
done
until_landed "$task_b" "(1) while its shadow was held on the runner,"
[ ! -e "$WORK/release-two" ] || harness "the runner's gate was released before task #$task_b landed"
expect_eq "$(shadows "$task_b" "$eb")" 0 "(1) shadow records of task #$task_b before its runner gate ended (it was held)"
: >"$WORK/release-two"
shadow_of "$task_b" "$eb" 60 "(1)"
sb=$shadow
check_shadow "$sb" "$eb" "(1)" task-b accepted accepted
expect_eq "$unequal_want" "[]" "(1) the shadow's inputs unlike the primary's (the same set)"
expect_eq "$(path "$sb" agrees)" true "(1) the shadow's agrees"

# --- (2) a disagreement: the runner decides nothing, the supervisor is told -----------

rejections_before=$(rejections)
assign_and_report three
task_c=$task
judge_next "$task_c" "$assigned_at"
ec=$event
[ "$verdict" = accepted ] || harness "task #$task_c was gated '$verdict' ($(field "$ec" reason))"
until_landed "$task_c" "(2) (the runner rejected it; it must decide nothing)"
shadow_of "$task_c" "$ec" 90 "(2)"
sc=$shadow
check_shadow "$sc" "$ec" "(2)" task-c rejected accepted
expect_eq "$unequal_want" "[]" "(2) the shadow's inputs unlike the primary's (the same set)"
expect_eq "$(path "$sc" agrees)" false "(2) the shadow's agrees"
expect_eq "$(task_state "$task_c")" landed "(2) task #$task_c's state after the runner's rejection"
expect_eq "$(rejections)" "$rejections_before" "(2) gate_rejections after the runner's rejection"
deadline=$(($(now) + 60))
until [ "$(told_shadow "$task_c")" -ge 1 ]; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(2) the supervisor got no digest naming #$task_c, the runner and the shadow within 60 s of the disagreement"
    sleep 0.5
done

# --- (3) another scenario set: no comparison of verdicts on unequal inputs ------------

digest_before=$(digest_of "$gate")
assign_and_report four
task_d=$task
judge_next "$task_d" "$assigned_at"
ed=$event
[ "$verdict" = accepted ] || harness "task #$task_d was gated '$verdict' ($(field "$ed" reason))"
[ -f "$gate/check-four.sh" ] || harness "the primary's gate did not add check-four.sh"
until_landed "$task_d" "(3)"
shadow_of "$task_d" "$ed" 90 "(3)"
sd=$shadow
check_shadow "$sd" "$ed" "(3)" task-d accepted accepted
expect_eq "$(run_field runner.log task-d 5)" "$(digest_of "$gate")" "(3) the set the runner received (the gate directory after the primary's gate added a file)"
if [ "$unequal_want" = "[]" ]; then
    expect_eq "$(path "$sd" agrees)" true "(3) the shadow's agrees (the same inputs)"
else
    case "$unequal_want" in *scenarios_sha256*) ;; *) fail "(3) the inputs differ, but not in scenarios_sha256: $unequal_want" ;; esac
    expect_eq "$(path "$sd" agrees)" null "(3) the shadow's agrees on unequal inputs ($unequal_want)"
fi
echo "036: task #$task_d: the primary judged the set $(path "$ed" inputs,scenarios_sha256 | cut -c1-12) (before $(printf '%s' "$digest_before" | cut -c1-12)), the runner $(path "$sd" inputs,scenarios_sha256 | cut -c1-12); unequal $unequal_want"

# --- (4) told once ----------------------------------------------------------------------

sleep 20
expect_eq "$(told_shadow "$task_c")" 1 "(4) digests that told the supervisor of task #$task_c's disagreement"
expect_eq "$(told_shadow "$task_b")" 0 "(4) digests that told the supervisor of task #$task_b's shadow (it agreed)"
expect_eq "$(told_shadow "$task_d")" 0 "(4) digests that told the supervisor of task #$task_d's shadow (no disagreement)"
integrator_alive || harness "the integrator is not the process that judged task #$task_b"

echo "036: the run took $(($(now) - started)) s"
echo "036: the runner judged a copy of each verdict on the inputs it names, beside the primary; the landings never waited and the runner decided nothing; the disagreement reached the supervisor once; unequal inputs were never compared"
exit 0
