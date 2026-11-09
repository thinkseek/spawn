#!/bin/sh
# Black box: every gate verdict binds the exact inputs it judged and where it
# judged them, and says whether it is a statement about the code or about
# the infrastructure. A verdict today names the task, its revision and the
# base, nothing more: not the candidate commit the gate was given (only an
# accepted record names it), not the controller whose gate and scenarios
# ran, not the scenario set, not the host or its toolchain. On 2026-11-08 a
# gate runner produced two REJECTED verdicts that were its environment (no
# cargo registry: "candidate's dependency graph does not resolve offline";
# a cargo config naming a rustc-wrapper the machine lacked), and nothing in
# the record told them from a rejection of the work. Milestone M2 (a runner
# judging the same candidates as the primary, docs in
# /var/tmp/spawn/sync/design-code-state-v2.md) needs two verdicts that can
# be shown to have judged the same immutable inputs.
#
# Required: every `task.gated` record carries
#   `class`   `code` (accepted, rejected, conflict, incompatible),
#             `infrastructure` (aborted, host_fault, no_room) or `superseded`
#             (stale): a host's failure is never a statement about the code;
#   `inputs`  {`base`, `candidate`: the commits the gate was given (its
#             second and third arguments), `controller`: the first line of
#             `.controller` at the root of the controller whose scenarios the
#             mission judges with (the directory two levels above the
#             mission's gate directory), read when the gate runs, or null
#             when there is none or it is not 40 hex digits (never guessed),
#             `scenarios_sha256`: the sha256 of the lines `<sha256>  <path>`
#             (sha256sum's format) of every regular file under the mission's
#             gate directory, paths relative to it without "./", sorted
#             bytewise, read when the gate runs, `scenario`: the task's own
#             scenario file name given to the gate, or null};
#   `env`     {`host`: the judging host's name (uname -n), `toolchain`: the
#             text after `| base cache: toolchain ` on the first such line of
#             the gate's output, or null when it printed none (a gate that
#             ended before its setup), `runner`: null for a gate run on this
#             host}.
# A record no gate ran for (stale, a conflict found before the gate) has
# `env` null; when the head is judged too (incompatible, a rejected real
# merge) the record also carries `head_inputs`, the same object for that run.
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, the
# supervisor, no model judge) runs one resident, `bind`, of one gated mission
# (accept = gate) on a fake engine; the supervisor runs on the same fake
# engine. The mission's gate directory is <ctl>/rsi/scenarios of a controller
# tree <ctl> whose `.controller` names a known commit. One long-running
# `spawn integrate` (not `--once`: started once, never restarted, so nothing
# read at its start can pass for what a later gate ran with), with
# SPAWN_GATE_HOST_FAULT_BACKOFF_SECS=2, judges through a recording gate
# (SPAWN_GATE) that notes each run's arguments and prints what
# scripts/rsi/gate.sh prints:
#   task A (the candidate holds one.txt), by run number:
#     run 1: setup (its `| base cache: toolchain ...` line), then a host fault
#            on the base (exit 6, the gate's own fact);
#     run 2: a setup failure before any toolchain line (exit 3): an abort;
#     run 3: setup, accepted.
#   task B (the candidate holds two.txt): setup, rejected.
# Checked:
#   (1) task A's host fault: `class` infrastructure, `inputs` (base and
#       candidate equal to the gate's arguments, base equal to the source's
#       main, controller the known commit, scenarios_sha256 computed here
#       from the gate directory, scenario null), `env` (host uname -n,
#       toolchain the gate's line, runner null); gate_rejections unchanged.
#   (2) its abort: `class` infrastructure, `env.toolchain` null, the same
#       `inputs`; gate_rejections unchanged.
#   (3) its acceptance: `class` code, the same `inputs` and `env`; it lands.
#   (4) before task B is judged, `.controller` is removed and a scenario file
#       added; task B's rejection: `class` code, `inputs.controller` null (not
#       the previous commit), `inputs.scenarios_sha256` the new set's (not the
#       old), base equal to the source's main (task A's landing), candidate the
#       gate's argument; gate_rejections went up by one; the integrator is
#       the same process that judged task A.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main and the deployed binary (984d059):
# the host fault's record has no `class` (its fields are those of today's
# `task.gated`). It passes once every verdict binds its inputs and its
# environment as above and names its class.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "035: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep awk date sha256sum sort find uname; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_035_bind_$run"
SPAWN_TMUX_SOCKET="bb-035-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL
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
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "035: the loop's last lines:" >&2
            tail -n 12 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "035: the integrator's last lines:" >&2
            grep -v '^\s*$' "$WORK/integrate.log" | grep -v '^| ' | tail -n 12 >&2
        fi
        if [ -s "$WORK/judged.log" ]; then
            echo "035: the gate runs (time_ms name base candidate scenarios scenario):" >&2
            cat "$WORK/judged.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "035: harness: unexpected exit $rc" >&2
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
git -C "$WORK/seed" commit -qm "base"
for bare in "$repo" "$source"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

# A controller tree: its `.controller` names a commit (deploy/controller.sh writes
# the deployed commit there), its rsi/scenarios is the mission's gate
# directory.
ctl=$WORK/ctl
gate=$ctl/rsi/scenarios
mkdir -p "$gate/sub"
controller_commit=$(printf 'bb-035-controller' | sha1sum | cut -c1-40)
printf '%s\n' "$controller_commit" >"$ctl/.controller"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
printf 'a file in a subdirectory: part of the set\n' >"$gate/sub/notes"
: >"$WORK/judged.log"

# The scenario set's digest, as the contract defines it.
scenarios_digest() {
    (cd "$gate" && find . -type f | sed 's|^\./||' | LC_ALL=C sort | while IFS= read -r path; do
        sha256sum "$path"
    done) | sha256sum | cut -d' ' -f1
}

toolchain="rustc 1.95.0-bb cargo 1.95.0-bb psql 16.bb server 160099"

# The recording gate (see the header). It notes its arguments and prints
# what scripts/rsi/gate.sh prints for each case, its stage lines included.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
has() { git --git-dir="\$1" cat-file -e "\$2:\$3" 2>/dev/null; }
repo=\$1
base=\$2
cand=\$3
note() {
    # note <name> <the gate's arguments>: \$2 repo, \$3 base, \$4 candidate, \$5 scenarios, \$6 scenario
    printf '%s %s %s %s %s %s\n' "\$(date +%s%3N)" "\$1" "\$3" "\$4" "\$5" "\${6:--}" >>'$WORK/judged.log'
}
setup() {
    echo "| == setup"
    echo "| base cache: toolchain $toolchain"
    echo "| == base: build"
    echo "| build: ok"
    echo "| stage gate ok 10512ms base: build"
}
verdict() { # <accepted|rejected> [reason]
    if [ "\$1" = rejected ]; then
        json="{\"verdict\":\"rejected\",\"reason\":\"\$2\"}"
    else
        json='{"verdict":"accepted"}'
    fi
    echo "| == candidate: build"
    echo "| stage gate ok 900ms candidate: build"
    printf '%s\n' "\$json" >"\$SPAWN_GATE_VERDICT"
    echo "\$json"
    exit 0
}
if has "\$repo" "\$cand" two.txt; then
    note task-b "\$@"
    setup
    echo "| check-ok.sh failed on the candidate"
    verdict rejected "scenario fails on the candidate"
fi
if has "\$repo" "\$cand" one.txt; then
    n=\$(cat '$WORK/runs-a' 2>/dev/null || echo 0)
    n=\$((n + 1))
    echo "\$n" >'$WORK/runs-a'
    case "\$n" in
    1)
        note a-1 "\$@"
        setup
        echo "| == base: 011.sh"
        echo "| 011.sh exited 2, then 1 on the base: the host could not judge it"
        json="{\"host_fault\":true,\"scenario\":\"011.sh\",\"on\":\"base\",\"runs\":\"2, then 1\",\"reason\":\"011.sh exited 2, then 1 on the base\",\"base\":\"\$base\",\"candidate\":\"\$cand\"}"
        printf '%s\n' "\$json" >"\$SPAWN_GATE_VERDICT"
        echo "\$json"
        exit 6
        ;;
    2)
        note a-2 "\$@"
        echo "gate: SPAWN_GATE_MIN_FREE_GB is a whole number, not 'x'" >&2
        exit 3
        ;;
    *)
        note "a-\$n" "\$@"
        setup
        verdict accepted
        ;;
    esac
fi
note other "\$@"
verdict accepted
EOF
chmod 755 "$WORK/fake-gate.sh"

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
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_INTEGRATE_WARM; do
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
main_of() { git --git-dir="$source" rev-parse main; }
# The gate run named $1 (judged.log's second column): its base and candidate.
# The first run of that name: a second run (the head judged alone) never
# replaces the one the verdict is about.
run_base() { awk -v name="$1" '$2 == name { print $3; exit }' "$WORK/judged.log"; }
run_candidate() { awk -v name="$1" '$2 == name { print $4; exit }' "$WORK/judged.log"; }

waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$1'")" = WAITING ] &&
        [ "$(q "select count(*) from agent_executions
                where session_id = '$1' and ended_at_ms is null")" -ge 1 ]
}

# The integrator, started once (see the header).
start_integrator() {
    env SPAWN_GATE="$WORK/fake-gate.sh" SPAWN_TEST_DATABASE_URL="$TEST_URL" \
        SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        SPAWN_GATE_HOST_FAULT_BACKOFF_SECS="$BACKOFF_SECS" \
        "$SPAWN_BIN" integrate --root "$WORK/integrator" >>"$WORK/integrate.log" 2>&1 &
    integrator_pid=$!
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

# The binding a verdict must carry (see the header): event $1, the check's
# label $2, class $3, the gate run's name $4 (its arguments are the inputs),
# controller $5 (or `null`), the scenario set's digest $6, toolchain $7 (or
# `null`).
check_binding() {
    e=$1 label=$2
    expect_eq "$(path "$e" class)" "$3" "$label the verdict's class ($(field "$e" verdict))"
    expect_eq "$(path "$e" inputs,base)" "$(run_base "$4")" "$label inputs.base (the base the gate was given)"
    expect_eq "$(path "$e" inputs,candidate)" "$(run_candidate "$4")" "$label inputs.candidate (the candidate the gate was given)"
    expect_eq "$(path "$e" inputs,controller)" "$5" "$label inputs.controller (.controller when the gate ran)"
    expect_eq "$(path "$e" inputs,scenarios_sha256)" "$6" "$label inputs.scenarios_sha256 (the gate directory when the gate ran)"
    expect_eq "$(path "$e" inputs,scenario)" null "$label inputs.scenario (the task has no scenario of its own)"
    expect_eq "$(path "$e" env,host)" "$(uname -n)" "$label env.host"
    expect_eq "$(path "$e" env,toolchain)" "$7" "$label env.toolchain (the gate's toolchain line)"
    expect_eq "$(path "$e" env,runner)" null "$label env.runner (judged on this host)"
}

# --- one mission, one resident, task A reported ----------------------------------

capture "$SPAWN_BIN" mission add --text "035 a verdict binds its inputs" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" resident propose --name bind --charter "Owns the 035 change."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name bind
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
started=$(now)
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission configure exited $status: $(err)"

# gated_since <task> <event>: the task has a `task.gated` record after event.
gated_since() {
    [ "$(q "select count(*) from spawn_events where session_id = 'session_control'
              and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
              and kind -> 'detail' -> 'record' ->> 'id' = '$1' and id > $2")" -ge 1 ]
}
# assign_and_report <file name>: sets `task` and `assigned_at` (the last event
# before the assignment: every record about the task comes after it). A running
# integrator may judge the report the moment it comes and send the task back,
# so the wait ends at the report or at its first verdict, whichever is seen.
assign_and_report() {
    assigned_at=$(last_event)
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker bind \
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
    git --git-dir="$source" cat-file -e "refs/heads/spawn/agent_resident_bind:$1.txt" 2>/dev/null ||
        harness "bind's pushed branch does not hold $1.txt"
}

assign_and_report one
task_a=$task
deadline=$(($(now) + 120))
until waiting session_supervisor; do
    [ "$(now)" -lt "$deadline" ] || harness "the supervisor was never judged waiting"
    sleep 0.5
done
digest_a=$(scenarios_digest)
main_a=$(main_of)
start_integrator

# --- (1) a host fault: infrastructure, with its inputs and environment ---------------

rejections_before=$(rejections)
judge_next "$task_a" "$assigned_at"
e1=$event
[ "$verdict" = host_fault ] ||
    harness "task #$task_a's first gate (a host fault on the base, exit 6 with the gate's fact) was gated '$verdict' ($(field "$e1" reason))"
check_binding "$e1" "(1)" infrastructure a-1 "$controller_commit" "$digest_a" "$toolchain"
expect_eq "$(path "$e1" inputs,base)" "$main_a" "(1) inputs.base (the source's main)"
expect_eq "$(rejections)" "$rejections_before" "(1) gate_rejections after a host fault"

# --- (2) an abort before the setup: infrastructure, no toolchain ---------------------

judge_next "$task_a" "$e1"
e2=$event
[ "$verdict" = aborted ] ||
    harness "task #$task_a's second gate (exit 3 before its setup) was gated '$verdict' ($(field "$e2" reason))"
check_binding "$e2" "(2)" infrastructure a-2 "$controller_commit" "$digest_a" null
expect_eq "$(rejections)" "$rejections_before" "(2) gate_rejections after an abort"

# --- (3) the acceptance: code, the same inputs; it lands ----------------------------

judge_next "$task_a" "$e2"
e3=$event
[ "$verdict" = accepted ] ||
    harness "task #$task_a's third gate (accepted) was gated '$verdict' ($(field "$e3" reason))"
check_binding "$e3" "(3)" code a-3 "$controller_commit" "$digest_a" "$toolchain"
deadline=$(($(now) + 60))
until [ "$(task_state "$task_a")" = landed ]; do
    [ "$(now)" -lt "$deadline" ] || harness "task #$task_a was accepted but did not land within 60 s"
    integrator_alive || harness "the integrator exited before task #$task_a landed: $(log_tail)"
    sleep 0.3
done

# --- (4) read when the gate runs: no marker, a new scenario set ------------------------

rm -f "$ctl/.controller"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-two.sh"
digest_b=$(scenarios_digest)
[ "$digest_b" != "$digest_a" ] || harness "the scenario set's digest did not change with a new file"
# Counted before the report: a running integrator can judge it within 0.5 s.
rejections_before=$(rejections)
assign_and_report two
task_b=$task
main_b=$(main_of)
[ "$main_b" != "$main_a" ] || harness "task #$task_a's landing did not move the source's main"
judge_next "$task_b" "$assigned_at"
e4=$event
[ "$verdict" = rejected ] ||
    harness "task #$task_b's gate (rejected) was gated '$verdict' ($(field "$e4" reason))"
check_binding "$e4" "(4)" code task-b null "$digest_b" "$toolchain"
expect_eq "$(path "$e4" inputs,base)" "$main_b" "(4) inputs.base (the source's main after task #$task_a landed)"
expect_eq "$(rejections)" "$((rejections_before + 1))" "(4) gate_rejections after a rejection of the code"
integrator_alive || harness "the integrator is not the process that judged task #$task_a"

echo "035: the run took $(($(now) - started)) s"
echo "035: every verdict named its class, the exact commits, controller and scenario set it judged, read when its gate ran, and the host and toolchain that judged them; host faults and aborts were infrastructure, not counted as rejections"
exit 0
