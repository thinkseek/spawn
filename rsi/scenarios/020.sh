#!/bin/sh
# Black box: a gate failure that is the host's is not a verdict on the work.
# When check-tests.sh fails on the candidate and every failure is a
# connection error to the test database (or the test database's own log
# shows a server crash during the gate), the integrator records a host fault,
# not a rejection: it does not count toward the halt, nothing goes back to
# the worker, the gate runs again (once objective 018's checks pass), and two
# host faults in a row halt the mission's landings for a human. A real test
# failure among them keeps it a rejection.
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, no
# model judge, no supervisor) runs four residents of one gated mission
# (accept = gate) on a fake engine that writes `<name>.txt` and reports.
# `spawn integrate --once` passes, run by the scenario one at a time, judge
# through a recording gate (SPAWN_GATE) that prints what scripts/rsi/gate.sh
# prints when check-tests.sh fails (its section header, check-tests.sh's
# lines: each failing test's `thread '<name>' panicked at <file>:` and its
# message, `test result: FAILED. ...`) and the verdict `rejected`, reason
# `regression: check-tests.sh fails on the candidate`, by what the candidate
# holds:
#   - hostA.txt (until the host is "fixed"): three failures, each a
#     connection error (`error connecting to server: Connection refused`,
#     SQLSTATE E57P02 `terminating connection because of crash of another
#     server process`, `the database system is in recovery mode`);
#   - mixed.txt: two connection errors and one real assertion failure;
#   - olddb.txt: two database errors that are not connection errors (`could
#     not open relation with OID`, `cache lookup failed`), with a crash in
#     the test database's log an hour before the gate ran;
#   - hostB.txt: on its first run, two connection errors; on its second, the
#     database errors of olddb.txt while the gate appends a crash to the test
#     database's log (`server process (PID n) was terminated by signal 11:
#     Segmentation fault`, `terminating any other active server processes`,
#     `all server processes terminated; reinitializing`, stamped now, UTC);
#   - anything else: accepted.
# The integrator reads that log from SPAWN_TEST_DATABASE_LOG. The four tasks
# are judged in id order (hostA, mixed, olddb, hostB), all reported first.
# Checked:
#   (1) hostA's first verdict is `host_fault`: the pass leaves
#       gate_rejections and gate_aborts where they were, the task stays
#       `reported` with its revisions unchanged (nothing goes back to the
#       worker), the mission is not halted; the record names `evidence`
#       `failures` and `failures`, one entry per failing test with its
#       `test` name.
#   (2) the gate runs again: the host fixed, the next pass judges hostA again
#       (no other wait), accepted, and it lands.
#   (3) mixed: `rejected`, gate_rejections + 1 (a real failure among
#       connection errors is the work's).
#   (4) olddb: `rejected`, gate_rejections + 1 (database errors that are not
#       connection errors, and the log's crash is outside the gate's window).
#   (5) hostB: `host_fault` (`host_faults` 1), then `host_fault` again, by
#       the log (`evidence` `server_log`, `crash` holding the signal line,
#       `host_faults` 2, `escalate` true); then the mission halts with a
#       reason naming the host faults, and the gate does not run a third
#       time on hostB.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (06cc205, the deployed binary built
# from it): hostA's candidate, whose every failure is a connection error to
# the test database, is gated `rejected` (regression: check-tests.sh fails
# on the candidate), counted toward the halt (gate_rejections 0 -> 1) and
# sent back to its worker. It passes once the integrator, when check-tests.sh
# fails on the candidate, reads the failures check-tests.sh printed and the
# test database's log over the gate's window and records `host_fault` when
# every failure is the host's, `rejected` otherwise, and halts the mission
# after two host faults in a row.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "020: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep date; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_020_host_$run"
SPAWN_TMUX_SOCKET="bb-020-$run"
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
            echo "020: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "020: the integrator's last lines:" >&2
            tail -n 15 "$WORK/integrate.log" >&2
        fi
        if [ -s "$WORK/judged.log" ]; then
            echo "020: the gate runs (time case candidate):" >&2
            cat "$WORK/judged.log" >&2
        fi
        if [ -s "$WORK/passes.log" ]; then
            echo "020: the passes (before event, gate_rejections before -> after, verdicts):" >&2
            cat "$WORK/passes.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "020: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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
source=$WORK/source.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "base"
for bare in "$repo" "$source"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

gate=$WORK/gate
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
: >"$WORK/judged.log"

# The test database's log, as postgres writes it (`%m [%p] `, UTC). It holds
# a crash from an hour before the scenario: outside every gate's window.
pglog=$WORK/postgres.log
old=$(date -u -d '-1 hour' '+%Y-%m-%d %H:%M:%S.000 UTC' 2>/dev/null) ||
    old=$(date -u -r $(($(date +%s) - 3600)) '+%Y-%m-%d %H:%M:%S.000 UTC')
{
    printf '%s [1] LOG:  server process (PID 311) was terminated by signal 11: Segmentation fault\n' "$old"
    printf '%s [1] LOG:  terminating any other active server processes\n' "$old"
    printf '%s [1] LOG:  all server processes terminated; reinitializing\n' "$old"
    printf '%s [1] LOG:  database system is ready to accept connections\n' "$old"
} >"$pglog"

# The recording gate, by what the candidate holds (see the header). It
# prints check-tests.sh's failure lines as gate.sh does (`| ` prefixed).
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
has() { git --git-dir="\$1" cat-file -e "\$3:\$4" 2>/dev/null; }
reject() {
    printf '%s %s %s\n' "\$(date +%s)" "\$1" "\$cand" >>'$WORK/judged.log'
    printf '{"verdict":"rejected","reason":"regression: check-tests.sh fails on the candidate"}\n' >"\$SPAWN_GATE_VERDICT"
    echo "| stage gate rejected 1200ms regression: check-tests.sh must still pass"
    echo '{"verdict":"rejected","reason":"regression: check-tests.sh fails on the candidate"}'
    exit 0
}
fails() { # <test> <message line>...
    test=\$1
    shift
    echo "| ---- \$test stdout ----"
    echo "| thread '\$test' panicked at crates/spawn-postgres/tests/common/mod.rs:31:10:"
    for text in "\$@"; do echo "| \$text"; done
    echo "| note: run with \\\`RUST_BACKTRACE=1\\\` environment variable to display a backtrace"
    echo "| --"
}
refused() { fails "\$1" 'connect to the test database: error connecting to server: Connection refused (os error 111)'; }
crashed() { fails "\$1" 'called \`Result::unwrap()\` on an \`Err\` value: Error { kind: Db, cause: Some(DbError { severity: "FATAL", parsed_severity: Some(Fatal), code: SqlState(E57P02), message: "terminating connection because of crash of another server process" }) }'; }
recovering() { fails "\$1" 'db error: FATAL: the database system is in recovery mode'; }
lost_oid() { fails "\$1" 'called \`Result::unwrap()\` on an \`Err\` value: Database("db error: ERROR: could not open relation with OID 16421")'; }
lost_cache() { fails "\$1" 'db error: ERROR: cache lookup failed for relation 16388'; }
asserted() {
    echo "| ---- \$1 stdout ----"
    echo "| thread '\$1' panicked at crates/spawn-cli/src/integrate.rs:2711:9:"
    echo "| assertion \\\`left == right\\\` failed: the landing moved main"
    echo "|   left: 1"
    echo "|  right: 2"
    echo "| --"
}
header() {
    echo "== candidate: build"
    echo "| build: ok"
    echo "| stage gate ok 900ms candidate: build"
    echo "| == regression: check-tests.sh must still pass"
}
cand=\$3
if has "\$1" "\$2" "\$3" hostB.txt; then
    n=\$(cat '$WORK/hostB-runs' 2>/dev/null || echo 0)
    n=\$((n + 1))
    echo "\$n" >'$WORK/hostB-runs'
    header
    if [ "\$n" -eq 1 ]; then
        refused sources::tests::lists_every_source
        recovering stalls::delivery_is_measured_from_its_record
        echo "| test result: FAILED. 41 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.02s"
    else
        sleep 1
        stamp=\$(date -u '+%Y-%m-%d %H:%M:%S.%3N UTC')
        {
            printf '%s [1] LOG:  server process (PID 4242) was terminated by signal 11: Segmentation fault\n' "\$stamp"
            printf '%s [1] DETAIL:  Failed process was running: select * from spawn_claim(\$1)\n' "\$stamp"
            printf '%s [1] LOG:  terminating any other active server processes\n' "\$stamp"
            printf '%s [1] LOG:  all server processes terminated; reinitializing\n' "\$stamp"
        } >>'$pglog'
        sleep 1
        lost_oid merges::tests::a_conflict_is_recorded
        lost_cache sources::tests::lists_every_source
        echo "| test result: FAILED. 41 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.40s"
    fi
    reject hostB-\$n
fi
if has "\$1" "\$2" "\$3" olddb.txt; then
    header
    lost_oid merges::tests::a_conflict_is_recorded
    lost_cache sources::tests::lists_every_source
    echo "| test result: FAILED. 41 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.40s"
    reject olddb
fi
if has "\$1" "\$2" "\$3" mixed.txt; then
    header
    refused api::tests::claims_in_order
    crashed control::tests::the_fold_replays
    echo "| test result: FAILED. 41 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.97s"
    asserted integrate::tests::a_landing_moves_main_once
    echo "| test result: FAILED. 230 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 9.10s"
    reject mixed
fi
if has "\$1" "\$2" "\$3" hostA.txt && [ ! -e '$WORK/fixed-hostA' ]; then
    header
    refused api::tests::claims_in_order
    crashed control::tests::the_fold_replays
    recovering stalls::tests::delivery_is_measured_from_its_record
    echo "| test result: FAILED. 40 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.85s"
    reject hostA
fi
printf '%s %s %s\n' "\$(date +%s)" accept "\$cand" >>'$WORK/judged.log'
echo "== candidate: build"
echo "| build: ok"
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
chmod 755 "$WORK/fake-gate.sh"

# The fake engine: a text naming `BB:WRITE` writes `<name>.txt`, waits for
# the pusher to publish it (at most 30 s), then writes the report. Anything
# else (a revision) is a turn that changes nothing.
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
    case "$typed" in *"BB:WRITE"*) job=write ;; esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$job" ]; then
      file=$name.txt
      printf '%s was here\n' "$name" >"$file"
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
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_GATE_MIN_FREE_GB \
    SPAWN_GATE_MIN_FREE_MEMORY_MB SPAWN_INTEGRATE_GATES SPAWN_TEST_DATABASE_LOG; do
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
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"
: >"$WORK/integrate.log"
: >"$WORK/passes.log"

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
revisions() { q "select revisions from mission_tasks where id = $1"; }
rejections() { q "select gate_rejections from mission_missions where id = $mission"; }
aborts() { q "select gate_aborts from mission_missions where id = $mission"; }
halted() { q "select coalesce(halted, '') from mission_missions where id = $mission"; }
tasks_line() { q "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks"; }
log_tail() { tail -n 6 "$WORK/integrate.log" 2>/dev/null | tr '\n' ' '; }
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }
field() { q "select kind -> 'detail' -> 'record' ->> '$2' from spawn_events where id = $1"; }
runs_of() { grep -c " $1 " "$WORK/judged.log" || true; }

# One pass, with 018's checks met if they exist: the real test database,
# no disk or memory floor to speak of; the test database's log.
integrate_once() {
    before_count=$(rejections)
    before_event=$(last_event)
    capture env SPAWN_GATE="$WORK/fake-gate.sh" SPAWN_TEST_DATABASE_URL="$TEST_URL" \
        SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        SPAWN_TEST_DATABASE_LOG="$pglog" \
        "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || fail "integrate exited $status: $(err | tail -n 3)"
    printf '%s %s -> %s %s\n' "$before_event" "$before_count" "$(rejections)" \
        "$(q "select coalesce(string_agg((kind -> 'detail' -> 'record' ->> 'id') || ':' ||
                                         (kind -> 'detail' -> 'record' ->> 'verdict'), ';' order by id), '')
               from spawn_events where session_id = 'session_control' and id > $before_event
                and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'")" >>"$WORK/passes.log"
}

# The next gate verdict (not `stale`) on task `$1` after event `$2`, passes
# run until it comes: sets `event`, `verdict`, and `before` / `after` (the
# mission's gate_rejections around the pass that recorded it).
judge_next() {
    deadline=$(($(now) + 90))
    while :; do
        integrate_once
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
            before=$before_count
            after=$(rejections)
            return 0
        fi
        [ "$(now)" -lt "$deadline" ] ||
            harness "task #$1 got no verdict within 90 s of passes (tasks: $(tasks_line); integrate: $(log_tail))"
        sleep 0.5
    done
}

# --- one mission, four residents, four reported tasks ---------------------------

capture "$SPAWN_BIN" mission add --text "020 host faults" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
names="hostA mixed olddb hostB"
for name in $names; do
    lower=$(printf '%s' "$name" | tr 'A-Z' 'a-z')
    capture "$SPAWN_BIN" resident propose --name "$lower" --charter "Owns the 020 $name change."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$lower"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
done
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission configure exited $status: $(err)"

assign() {
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker "$1" \
        --brief "Write your file. BB:WRITE"
    [ "$status" -eq 0 ] || harness "task assign for $1 failed: $(err)"
    jq -r .task "$WORK/out.txt"
}
# The engine names its file after the resident; the gate reads hostA.txt,
# so the residents' files are renamed by the case below.
t_hosta=$(assign hosta)
t_mixed=$(assign mixed)
t_olddb=$(assign olddb)
t_hostb=$(assign hostb)
for id in "$t_hosta" "$t_mixed" "$t_olddb" "$t_hostb"; do
    [ -n "$id" ] && [ "$id" != null ] || harness "task assign printed no id"
done
[ "$t_hosta" -lt "$t_mixed" ] && [ "$t_mixed" -lt "$t_olddb" ] && [ "$t_olddb" -lt "$t_hostb" ] ||
    harness "task ids are not in assignment order"
all_reported() {
    for id in "$t_hosta" "$t_mixed" "$t_olddb" "$t_hostb"; do
        [ "$(task_state "$id")" = reported ] || return 1
    done
}
deadline=$(($(now) + 240))
until all_reported; do
    [ "$(now)" -lt "$deadline" ] ||
        harness "the four tasks were not all reported within 240 s (tasks: $(tasks_line))"
    sleep 1
done
for name in $names; do
    lower=$(printf '%s' "$name" | tr 'A-Z' 'a-z')
    git --git-dir="$source" cat-file -e "refs/heads/spawn/agent_resident_$lower:$lower.txt" 2>/dev/null ||
        harness "$name's pushed branch does not hold $lower.txt"
done
# The gate's cases read the files by these names.
sed -i -e 's/hostA\.txt/hosta.txt/g' -e 's/hostB\.txt/hostb.txt/g' "$WORK/fake-gate.sh"

# --- (1) hostA: every failure a connection error -> host_fault --------------------------

start=$(last_event)
revisions_before=$(revisions "$t_hosta")
aborts_before=$(aborts)
judge_next "$t_hosta" "$start"
if [ "$verdict" != host_fault ]; then
    fail "(1) task #$t_hosta, whose every check-tests failure is a connection error to the test database, was gated '$verdict' ($(field "$event" reason)), not 'host_fault'; gate_rejections $before -> $after"
fi
assert_eq "$after" "$before" "(1) gate_rejections after the host fault (was $before)"
assert_eq "$(aborts)" "$aborts_before" "(1) gate_aborts after the host fault"
assert_eq "$(task_state "$t_hosta")" reported "(1) task #$t_hosta's state after its host fault"
assert_eq "$(revisions "$t_hosta")" "$revisions_before" "(1) task #$t_hosta's revisions after its host fault (nothing went back)"
assert_eq "$(halted)" "" "(1) the mission's halt after one host fault"
assert_eq "$(field "$event" evidence)" failures "(1) the host fault's evidence"
assert_eq "$(q "select jsonb_array_length(kind -> 'detail' -> 'record' -> 'failures') from spawn_events where id = $event")" 3 \
    "(1) the failures the host fault names"
assert_eq "$(q "select string_agg(f ->> 'test', ' ' order by f ->> 'test') from spawn_events,
                 jsonb_array_elements(kind -> 'detail' -> 'record' -> 'failures') f where id = $event")" \
    "api::tests::claims_in_order control::tests::the_fold_replays stalls::tests::delivery_is_measured_from_its_record" \
    "(1) the failing tests the host fault names"
assert_eq "$(field "$event" host_faults)" 1 "(1) the host fault's count in a row"

# --- (2) the gate runs again, and the work lands ------------------------------------------

: >"$WORK/fixed-hostA"
judge_next "$t_hosta" "$event"
assert_eq "$verdict" accepted "(2) task #$t_hosta's verdict on the next pass, the host fixed"
assert_eq "$(runs_of accept)" 1 "(2) accepted gate runs"
deadline=$(($(now) + 60))
until [ "$(task_state "$t_hosta")" = landed ]; do
    [ "$(now)" -lt "$deadline" ] || fail "(2) task #$t_hosta was accepted but did not land within 60 s"
    integrate_once
    sleep 0.5
done

# --- (3) mixed: a real failure among connection errors -> rejected -------------------------

judge_next "$t_mixed" "$start"
[ "$verdict" = rejected ] ||
    fail "(3) task #$t_mixed, with a real assertion failure among connection errors, was gated '$verdict' ($(field "$event" reason)), not 'rejected'"
assert_eq "$after" "$((before + 1))" "(3) gate_rejections after the real failure (was $before)"

# --- (4) olddb: database errors, a crash outside the window -> rejected ---------------------

judge_next "$t_olddb" "$start"
[ "$verdict" = rejected ] ||
    fail "(4) task #$t_olddb, whose failures are database errors that are not connection errors, with the test database's only crash an hour before the gate, was gated '$verdict' ($(field "$event" reason)), not 'rejected'"
assert_eq "$after" "$((before + 1))" "(4) gate_rejections after olddb's rejection (was $before)"

# --- (5) hostB: two host faults in a row halt the mission ------------------------------------

judge_next "$t_hostb" "$start"
[ "$verdict" = host_fault ] ||
    fail "(5) task #$t_hostb's first verdict, its failures all connection errors, was '$verdict' ($(field "$event" reason)), not 'host_fault'"
assert_eq "$after" "$before" "(5) gate_rejections after hostB's first host fault"
assert_eq "$(field "$event" host_faults)" 1 "(5) hostB's first host fault's count in a row"
assert_eq "$(halted)" "" "(5) the mission's halt after one host fault"
judge_next "$t_hostb" "$event"
[ "$verdict" = host_fault ] ||
    fail "(5) task #$t_hostb's second verdict, database errors while the test database's log shows a crash during the gate, was '$verdict' ($(field "$event" reason)), not 'host_fault'"
assert_eq "$after" "$before" "(5) gate_rejections after hostB's second host fault"
assert_eq "$(field "$event" evidence)" server_log "(5) the second host fault's evidence"
assert_contains "$(field "$event" crash)" "was terminated by signal 11" "(5) the second host fault's crash lines"
assert_eq "$(field "$event" host_faults)" 2 "(5) the second host fault's count in a row"
assert_eq "$(field "$event" escalate)" true "(5) the second host fault's escalate"
runs_b=$(grep -c ' hostB-' "$WORK/judged.log" || true)
for _ in 1 2 3; do
    integrate_once
    sleep 0.5
done
assert_contains "$(halted)" "host fault" "(5) the mission's halt after two host faults in a row"
assert_eq "$(grep -c ' hostB-' "$WORK/judged.log" || true)" "$runs_b" "(5) gate runs on hostB after the halt"

echo "020: connection errors and a crash in the test database's log were recorded as host faults, not counted, and judged again; real failures stayed rejections; two host faults in a row halted the mission"
exit 0
