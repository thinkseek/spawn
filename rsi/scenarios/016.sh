#!/bin/sh
# Black box: a candidate that passes the gate alone but fails it only once
# main is merged into it is sent back like a conflict, with the gate's
# output and the main commits it must adapt to, and is not counted as a
# rejection toward the mission's halt. A candidate whose own head fails is
# still a rejection.
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, no
# model judge, no supervisor) runs three residents of one gated mission
# (accept = gate) on a fake engine, and `spawn integrate --once` passes, run
# by the scenario one at a time, judge their work through a recording gate
# (SPAWN_GATE). The gate "builds" a tree by one rule that stands for a
# struct and its initializers: every file under uses/ must name exactly the
# fields that struct.txt names, or the gate rejects the candidate with
# `candidate tests do not build` and a compiler-like line
# (`error[E0063]: missing field `c` in initializer of `Facts``). It records
# every (base, candidate) pair it judged and its verdict.
#
# The three tasks are delivered from the same main M0 (struct.txt `a b`,
# uses/base.txt `a b`) and all reported before the first pass:
#   - alpha adds field `c`: struct.txt and every file under uses/ become
#     `a b c`. It passes the gate and lands first (M1).
#   - beta adds uses/beta.txt naming `a b`: right on M0, so its own head
#     passes the gate; merged with M1 (a textually clean merge) it does not.
#     This is task 36 and task 48 of 2026-11-06.
#   - gamma adds uses/gamma.txt naming `a b z`: wrong on M0 already.
# Checked:
#   (1) beta's verdict, the first after alpha's landing: `incompatible`,
#       not `rejected`; the pass that records it leaves the mission's
#       gate_rejections where it was; the task goes back (`revise`) and the
#       mission is not halted.
#   (2) its record says why, from records and commits, not text: `candidate`
#       is the merged commit the gate rejected, `head` its second parent
#       (beta's pushed head), `head_base` the merge-base of `head` and the
#       verdict's `base` (main), `head_verdict` `accepted`; the gate really
#       judged both pairs (`base`, `candidate`) rejected and (`head_base`,
#       `head`) accepted; `main_commits` holds alpha's landing commit (its
#       `task.landed` `to`); the `note` holds the gate's compiler line and
#       names alpha's landing commit (at least its first 7 hex digits).
#   (3) the note reaches beta: the dispatcher's next input to beta that
#       succeeded is its revision, carrying the compiler line and the commit.
#   (4) gamma, whose own head fails too: verdict `rejected`, and its pass
#       adds exactly one to gate_rejections; the gate judged its head as
#       well (the head is what decides, not the reason's text).
#   (5) the way back works: beta, sent back, gets main merged into its
#       workspace at the delivery of the revision (as every delivery does),
#       fixes uses/beta.txt to `a b c` and reports again; that report is
#       accepted and lands, the mission is not halted, and main builds.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (115f078, and the deployed binary
# built from 3e2c08e): beta's merged candidate is gated `rejected`
# (`candidate tests do not build`) and the rejection is counted toward the
# halt (gate_rejections 0 -> 1), though its head passes the gate alone. It
# passes once the integrator, when the gate rejects a merged candidate whose
# head does not contain main, judges the head alone against its merge-base
# with main with the same gate, and records `incompatible` (sent back, not
# counted) when the head passes, `rejected` when it does not.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "016: harness: $*" >&2
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
SPAWN_DB_NAME="bb_016_merged_$run"
SPAWN_TMUX_SOCKET="bb-016-$run"
SPAWN_PSQL=psql
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
    if [ -n "$database_made" ]; then
        psql -d postgres -tA -c "drop database if exists $SPAWN_DB_NAME with (force)" \
            >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "016: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "016: the integrator's last lines:" >&2
            tail -n 15 "$WORK/integrate.log" >&2
        fi
        if [ -s "$WORK/judged.log" ]; then
            echo "016: what the gate judged (time base candidate verdict):" >&2
            cat "$WORK/judged.log" >&2
        fi
        if [ -s "$WORK/passes.log" ]; then
            echo "016: the passes (verdicts, gate_rejections before -> after):" >&2
            cat "$WORK/passes.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "016: the gate verdicts:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select r ->> 'id', r ->> 'verdict', left(coalesce(r ->> 'reason', ''), 120)
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control' and r ->> 'type' = 'task.gated'
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "016: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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
source=$WORK/source.git
git init -q --initial-branch=main "$WORK/seed"
printf 'a b\n' >"$WORK/seed/struct.txt"
mkdir -p "$WORK/seed/uses"
printf 'a b\n' >"$WORK/seed/uses/base.txt"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "base: Facts {a, b}"
for bare in "$repo" "$source"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done
base=$(git --git-dir="$source" rev-parse main)

# The build rule, shared by the gate and the scenario's own premises:
# `build <git dir> <commit>` exits 0 when every file under uses/ names
# exactly struct.txt's fields, and prints a compiler-like line per mistake.
cat >"$WORK/build.sh" <<'EOF'
#!/bin/sh
dir=$1
commit=$2
fields=$(git --git-dir="$dir" show "$commit:struct.txt" 2>/dev/null) || {
    echo "error: struct.txt is missing"
    exit 1
}
status=0
for path in $(git --git-dir="$dir" ls-tree -r --name-only "$commit" uses/); do
    used=$(git --git-dir="$dir" show "$commit:$path")
    for field in $fields; do
        case " $used " in
        *" $field "*) ;;
        *)
            echo "error[E0063]: missing field \`$field\` in initializer of \`Facts\`"
            echo " --> $path:1"
            status=1
            ;;
        esac
    done
    for field in $used; do
        case " $fields " in
        *" $field "*) ;;
        *)
            echo "error[E0560]: struct \`Facts\` has no field named \`$field\`"
            echo " --> $path:1"
            status=1
            ;;
        esac
    done
done
exit $status
EOF
chmod 755 "$WORK/build.sh"

# The mission's scenarios directory: the recording gate below judges, this
# only has to exist and hold a scenario (the integrator counts them).
gate=$WORK/gate
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
: >"$WORK/judged.log"

# The recording gate: `<gate> <repo> <base> <candidate> <scenarios>
# [<scenario>]`, as the integrator runs it. Builds the candidate by the rule
# above; notes every pair it judged and its verdict.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
echo "== candidate: build"
if out=\$('$WORK/build.sh' "\$1" "\$3" 2>&1); then
    printf '%s\n' "\$out"
    echo "| build: ok"
    printf '%s %s %s accepted\n' "\$(date +%s)" "\$2" "\$3" >>'$WORK/judged.log'
    printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
    echo '{"verdict":"accepted"}'
else
    printf '%s\n' "\$out" | sed 's/^/| tests: /'
    printf '%s %s %s rejected\n' "\$(date +%s)" "\$2" "\$3" >>'$WORK/judged.log'
    printf '{"verdict":"rejected","reason":"candidate tests do not build"}\n' >"\$SPAWN_GATE_VERDICT"
    echo '{"verdict":"rejected","reason":"candidate tests do not build"}'
fi
EOF
chmod 755 "$WORK/fake-gate.sh"

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet. It writes its session transcript where Claude Code does.
# The task's text names one job:
#   BB:FIELDS <f...>  struct.txt and every file under uses/ name <f...>;
#   BB:USE            uses/<name>.txt names struct.txt's fields;
#   BB:BREAK          uses/<name>.txt names struct.txt's fields and `z`;
# and a revision (`[Review of your report on task #`) waits, its turn open,
# until $BB_WORK/fix-<name> exists, then rewrites uses/<name>.txt to
# struct.txt's fields as the workspace has them. Once the source's
# spawn/<agent> holds what the turn wrote (or 30 s passed), a second turn
# writes the report.
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
# pushed <file>: the source's spawn/<agent> holds <file> as the workspace does.
pushed() {
  [ "$(git --git-dir="$BB_WORK/source.git" show "refs/heads/spawn/$agent:$1" 2>/dev/null)" = "$(cat "$1")" ]
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
    *"BB:FIELDS "*) [ -n "$job" ] || job="fields ${typed#*BB:FIELDS }" ;;
    *"BB:USE"*) [ -n "$job" ] || job=use ;;
    *"BB:BREAK"*) [ -n "$job" ] || job=break ;;
    *"[Review of your report on task #"*) [ -n "$job" ] || job=revise ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$job" ]; then
      mkdir -p uses
      file=uses/$name.txt
      case "$job" in
      fields\ *)
        fields=${job#fields }
        printf '%s\n' "$fields" >struct.txt
        for used in uses/*.txt; do printf '%s\n' "$fields" >"$used"; done
        file=struct.txt
        ;;
      use) cp struct.txt "$file" ;;
      break) printf '%s z\n' "$(cat struct.txt)" >"$file" ;;
      revise)
        printf 'revision %s %s\n' "$(date +%s)" "$name" >>"$BB_WORK/engine.log"
        until [ -e "$BB_WORK/fix-$name" ]; do sleep 0.5; done
        cp struct.txt "$file"
        ;;
      esac
      printf 'wrote %s %s %s %s\n' "$(date +%s)" "$name" "$file" "$(cat "$file")" >>"$BB_WORK/engine.log"
      turn_ends "wrote $file"
      start=$(date +%s)
      until pushed "$file"; do
        [ $(($(date +%s) - start)) -ge 30 ] && break
        sleep 0.2
      done
      line '"type":"user","message":{"role":"user","content":"report"}'
      sleep 2
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s now names %s\n## Open issues\nnone\n' \
        "$file" "$(cat "$file")" >.spawn/report.md
      printf 'reported %s %s\n' "$(date +%s)" "$name" >>"$BB_WORK/engine.log"
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
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR; do
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
# Free disk is not what this scenario is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"
: >"$WORK/engine.log"
: >"$WORK/passes.log"

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

# --- helpers ----------------------------------------------------------------

now() { date +%s; }
q() { scalar "$1"; }

task_state() { q "select state from mission_tasks where id = $1"; }
rejections() { q "select gate_rejections from mission_missions where id = $mission"; }
halted() { q "select coalesce(halted, '') from mission_missions where id = $mission"; }
mission_line() {
    q "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks
         where mission_id = $mission"
}
log_tail() { tail -n 6 "$WORK/integrate.log" 2>/dev/null | tr '\n' ' '; }

# The gate verdicts recorded after control event `$1` other than `stale`:
# one line `<event id> <task> <verdict>` each.
GATED="select e.id || ' ' || (e.kind -> 'detail' -> 'record' ->> 'id') || ' ' ||
              (e.kind -> 'detail' -> 'record' ->> 'verdict')
         from spawn_events e
        where e.session_id = 'session_control'
          and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
          and e.kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale'"
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }

# One integrator pass, accounted: the verdicts it recorded and the mission's
# gate_rejections before and after it, one line in passes.log.
integrate_once() {
    before_count=$(rejections)
    before_event=$(last_event)
    capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || fail "integrate exited $status: $(err)"
    verdicts=$(sql "$GATED and e.id > $before_event order by e.id" | tr '\n' ';')
    printf '%s %s -> %s %s\n' "$before_event" "$before_count" "$(rejections)" "$verdicts" \
        >>"$WORK/passes.log"
    return 0
}

# Field `$2` of the control record of event `$1`.
record_of() { # <event id> <field>
    q "select kind -> 'detail' -> 'record' ->> '$2' from spawn_events where id = $1"
}
landed_to() {
    q "select kind -> 'detail' -> 'record' ->> 'to' from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'
        order by id desc limit 1"
}
# The first verdict (not `stale`) on task `$1` after control event `$2`:
# `<event id> <verdict>`.
first_verdict() {
    q "select e.id || ' ' || (e.kind -> 'detail' -> 'record' ->> 'verdict') from spawn_events e
        where e.session_id = 'session_control'
          and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
          and e.kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale'
          and e.kind -> 'detail' -> 'record' ->> 'id' = '$1' and e.id > $2
        order by e.id limit 1"
}
# The pass line (passes.log) that recorded control event `$1`.
pass_of() { grep -E "[ ;]$1 [0-9]+ [a-z]+(;|\$)" "$WORK/passes.log" | head -1 || true; }
# The gate_rejections before and after the pass that recorded event `$1`,
# when that pass recorded exactly one verdict: `<before> <after>`.
counted() {
    pass=$(pass_of "$1")
    [ -n "$pass" ] || harness "no pass recorded event $1"
    set -- $pass
    # <before event> <before> -> <after> <verdicts...>
    verdicts=$(printf '%s\n' "$pass" | cut -d' ' -f5- | tr ';' '\n' | grep -c . || true)
    [ "$verdicts" -eq 1 ] || harness "the pass that recorded event $1 recorded $verdicts verdicts: $pass"
    echo "$2 $4"
}
# A git directory that holds commit `$1`: the source, or the integrator's
# mirror (a landing candidate that did not land is only there).
holder() {
    for dir in "$source" "$WORK"/integrator/.sources/*.git; do
        if git --git-dir="$dir" cat-file -e "$1^{commit}" 2>/dev/null; then
            echo "$dir"
            return 0
        fi
    done
    echo "$source"
}
judged() { # <base> <candidate> <verdict>
    grep -q "^[0-9]* $1 $2 $3\$" "$WORK/judged.log"
}

capture "$SPAWN_BIN" mission add --text "016 merged candidates" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
for name in alpha beta gamma; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 016 $name change."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$name"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
done

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission configure exited $status: $(err)"

assign() {
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker "$1" \
        --brief "$2"
    [ "$status" -eq 0 ] || harness "task assign for $1 failed: $(err)"
    jq -r .task "$WORK/out.txt"
}
task_alpha=$(assign alpha "Add field c to Facts. BB:FIELDS a b c")
task_beta=$(assign beta "Add a test that builds Facts. BB:USE")
task_gamma=$(assign gamma "Add a broken initializer. BB:BREAK")
for id in "$task_alpha" "$task_beta" "$task_gamma"; do
    [ -n "$id" ] && [ "$id" != null ] || harness "task assign printed no id"
done
[ "$task_alpha" -lt "$task_beta" ] && [ "$task_beta" -lt "$task_gamma" ] ||
    harness "task ids are not in assignment order: $task_alpha $task_beta $task_gamma"

# --- all three reported from M0, before any pass -------------------------------------

all_reported() {
    for id in "$task_alpha" "$task_beta" "$task_gamma"; do
        [ "$(task_state "$id")" = reported ] || return 1
    done
}
deadline=$(($(now) + 240))
until all_reported; do
    [ "$(now)" -lt "$deadline" ] ||
        harness "the three tasks were not all reported within 240 s (tasks: $(mission_line))"
    sleep 1
done
[ "$(git --git-dir="$source" rev-parse main)" = "$base" ] || harness "main moved before any pass"
for name in alpha beta gamma; do
    git --git-dir="$source" merge-base --is-ancestor "$base" "refs/heads/spawn/agent_resident_$name" ||
        harness "$name's pushed branch does not hold M0"
done

# --- alpha lands ---------------------------------------------------------------------

deadline=$(($(now) + 120))
until [ "$(task_state "$task_alpha")" = landed ]; do
    [ "$(now)" -lt "$deadline" ] ||
        harness "alpha's task did not land within 120 s (tasks: $(mission_line); integrate: $(log_tail))"
    integrate_once
    sleep 0.5
done
alpha_landing=$(landed_to "$task_alpha")
[ -n "$alpha_landing" ] || harness "alpha's task.landed names no commit"
main1=$(git --git-dir="$source" rev-parse main)
[ "$(git --git-dir="$source" show "$main1:struct.txt")" = "a b c" ] ||
    harness "main after alpha's landing does not hold field c"
landed_at=$(q "select max(id) from spawn_events where session_id = 'session_control'
               and kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
               and kind -> 'detail' -> 'record' ->> 'id' = '$task_alpha'")
[ "$(rejections)" = 0 ] || harness "gate_rejections is $(rejections) after alpha's landing"

# The premises, by the gate's own rule: beta's head builds alone, and does
# not once merged with M1; gamma's head does not build alone.
beta_head=$(git --git-dir="$source" rev-parse "refs/heads/spawn/agent_resident_beta")
gamma_head=$(git --git-dir="$source" rev-parse "refs/heads/spawn/agent_resident_gamma")
"$WORK/build.sh" "$source" "$beta_head" >/dev/null || harness "beta's head does not build alone"
! "$WORK/build.sh" "$source" "$gamma_head" >/dev/null || harness "gamma's head builds alone"
git --git-dir="$source" merge-base --is-ancestor "$main1" "$beta_head" &&
    harness "beta's head already holds main after alpha's landing"

# --- (1) beta: incompatible, not counted -----------------------------------------------

deadline=$(($(now) + 120))
until verdict=$(first_verdict "$task_beta" "$landed_at") && [ -n "$verdict" ]; do
    [ "$(now)" -lt "$deadline" ] ||
        harness "beta's task got no verdict within 120 s of alpha's landing (tasks: $(mission_line); integrate: $(log_tail))"
    integrate_once
    sleep 0.5
done
set -- $verdict
beta_event=$1
beta_verdict=$2
set -- $(counted "$beta_event")
if [ "$beta_verdict" != incompatible ]; then
    fail "(1) task #$task_beta, whose head passes the gate alone and fails only merged with main, was gated '$beta_verdict' ($(record_of "$beta_event" reason)), not 'incompatible'; gate_rejections $1 -> $2"
fi
[ "$2" = "$1" ] ||
    fail "(1) the incompatible verdict on task #$task_beta moved gate_rejections from $1 to $2"
# Sent back: revise, or already delivered when the loop has typed the send-back
# into beta's session by now (an input to a busy session waits its turn, 015).
case "$(task_state "$task_beta")" in
revise | delivered) ;;
*) fail "(1) task #$task_beta's state after its incompatible verdict: expected revise or delivered, got $(task_state "$task_beta")" ;;
esac
assert_eq "$(halted)" "" "(1) the mission's halt after beta's verdict"

# --- (2) the record says why ------------------------------------------------------------

candidate=$(record_of "$beta_event" candidate)
head=$(record_of "$beta_event" head)
head_base=$(record_of "$beta_event" head_base)
head_verdict=$(record_of "$beta_event" head_verdict)
verdict_base=$(record_of "$beta_event" base)
note=$(record_of "$beta_event" note)
[ -n "$candidate" ] || fail "(2) beta's incompatible record names no candidate"
[ -n "$head" ] || fail "(2) beta's incompatible record names no head"
[ -n "$head_base" ] || fail "(2) beta's incompatible record names no head_base"
assert_eq "$verdict_base" "$main1" "(2) the incompatible record's base (main when it was judged)"
dir=$(holder "$candidate")
assert_eq "$(git --git-dir="$dir" rev-parse --verify -q "$candidate^2" || echo none)" "$head" \
    "(2) the second parent of the rejected candidate $candidate against the record's head"
assert_eq "$(git --git-dir="$dir" rev-parse --verify -q "$candidate^1" || echo none)" "$main1" \
    "(2) the first parent of the rejected candidate $candidate"
git --git-dir="$source" merge-base --is-ancestor "$head" "refs/heads/spawn/agent_resident_beta" ||
    fail "(2) the record's head $head is not on beta's pushed branch"
assert_eq "$(git --git-dir="$dir" merge-base "$head" "$verdict_base")" "$head_base" \
    "(2) the record's head_base against merge-base(head, base)"
assert_eq "$head_verdict" accepted "(2) the record's head_verdict"
judged "$verdict_base" "$candidate" rejected ||
    fail "(2) the gate never rejected the merged candidate ($verdict_base $candidate)"
judged "$head_base" "$head" accepted ||
    fail "(2) the gate never judged beta's head alone against its merge-base ($head_base $head) and accepted it"
main_commits=$(q "select string_agg(c, ' ') from spawn_events,
    jsonb_array_elements_text(case when jsonb_typeof(kind -> 'detail' -> 'record' -> 'main_commits') = 'array'
                                   then kind -> 'detail' -> 'record' -> 'main_commits' else '[]'::jsonb end) c
    where id = $beta_event")
assert_contains " $main_commits " " $alpha_landing " \
    "(2) the incompatible record's main_commits (alpha's landing)"
short=$(printf '%s' "$alpha_landing" | cut -c1-7)
assert_contains "$note" "missing field \`c\` in initializer of \`Facts\`" \
    "(2) the incompatible record's note (the gate's compiler line)"
assert_contains "$note" "$short" "(2) the incompatible record's note (alpha's landing commit)"

# --- (4) gamma: its own head fails, so it is a rejection --------------------------------

deadline=$(($(now) + 120))
until verdict=$(first_verdict "$task_gamma" "$landed_at") && [ -n "$verdict" ]; do
    [ "$(now)" -lt "$deadline" ] ||
        harness "gamma's task got no verdict within 120 s (tasks: $(mission_line); integrate: $(log_tail))"
    integrate_once
    sleep 0.5
done
set -- $verdict
gamma_event=$1
assert_eq "$2" rejected "(4) the verdict on task #$task_gamma, whose own head fails the gate"
set -- $(counted "$gamma_event")
assert_eq "$2" "$(($1 + 1))" "(4) gate_rejections after gamma's rejection (was $1)"
gamma_candidate=$(record_of "$gamma_event" candidate)
gamma_head_judged=$(grep -c " $gamma_head rejected\$" "$WORK/judged.log" || true)
[ "$gamma_head_judged" -ge 1 ] ||
    fail "(4) the gate never judged gamma's head ($gamma_head) alone: the rejection did not rest on it"

# --- (3) the note reaches beta -----------------------------------------------------------

verdict_ms=$(q "select recorded_at_ms from spawn_events where id = $beta_event")
revision() {
    q "select id from spawn_commands
        where session_id = 'session_resident_beta' and principal = 'spawn-dispatch'
          and action ->> 'type' = 'input' and state = 'finished' and succeeded
          and created_at_ms >= $verdict_ms and action ->> 'text' like '%Task #$task_beta%'
        order by id limit 1"
}
deadline=$(($(now) + 150))
until [ -n "$(revision)" ]; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(3) nothing was delivered to beta within 150 s of its incompatible verdict (task $(task_state "$task_beta"))"
    sleep 1
done
text=$(sql "select action ->> 'text' from spawn_commands where id = $(revision)")
assert_contains "$text" "Task #$task_beta" "(3) the input typed into beta after its verdict"
assert_contains "$text" "missing field \`c\` in initializer of \`Facts\`" \
    "(3) the revision typed into beta (the gate's compiler line)"
assert_contains "$text" "$short" "(3) the revision typed into beta (alpha's landing commit)"

# --- (5) the way back: beta adapts to main and lands ------------------------------------

revised() { grep -q "^revision [0-9]* beta\$" "$WORK/engine.log"; }
deadline=$(($(now) + 60))
until revised; do
    [ "$(now)" -lt "$deadline" ] || harness "beta's engine never took its revision"
    sleep 0.5
done
: >"$WORK/fix-beta"
deadline=$(($(now) + 240))
until [ "$(task_state "$task_beta")" = landed ]; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(5) task #$task_beta, sent back and fixed against main, did not land within 240 s (tasks: $(mission_line); integrate: $(log_tail))"
    [ -z "$(halted)" ] || fail "(5) the mission halted: $(halted)"
    integrate_once
    sleep 0.5
done
assert_eq "$(halted)" "" "(5) the mission's halt after beta landed"
main2=$(git --git-dir="$source" rev-parse main)
"$WORK/build.sh" "$source" "$main2" >/dev/null ||
    fail "(5) main does not build after beta landed: $("$WORK/build.sh" "$source" "$main2" | tr '\n' ' ')"
assert_eq "$(git --git-dir="$source" show "$main2:uses/beta.txt")" "a b c" "(5) uses/beta.txt on main"

echo "016: a candidate broken only by main's moves was sent back with the gate's output and main's commits, not counted; one broken alone was rejected and counted; the sent-back task adapted and landed"
exit 0
