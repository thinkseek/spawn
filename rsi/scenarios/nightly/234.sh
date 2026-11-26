#!/bin/sh
# Contract 234 [recoverability][observability]: each integrator start is
# named in the store. On 2026-11-18 14:57-15:03 a stray `spawn integrate`
# (pid 2375721) judged with the live database for at least an hour: it
# halted mission 9 and no record named it, its start or who started it
# (sync/incident-2375721-rev-b.md: "starter unknown"). Objective 221 refuses
# a second integrator; this names the one that holds the lock.
#
# Required: a start that takes the integrator's lock records, before
# anything else is written, one control record `integrator.started` holding
# exactly these fields:
#   pid, ppid: the integrator's process and its parent;
#   parent_comm: the parent's comm (/proc/<ppid>/comm), or null;
#   ancestors: [{pid, comm}] from the parent up to pid 1, at most 16;
#   cwd: its working directory, or null;
#   cgroup: its unified cgroup line (/proc/self/cgroup "0::<path>": the
#     unit or scope that holds it), or null;
#   gate: the gate script it runs, an absolute path; gate_from_env: whether
#     SPAWN_GATE named it (false: its default, read from cwd);
#   root: its root, an absolute path; once: whether it runs --once.
# Its stdout says "integrate: started as pid <pid> by <parent comm> (pid
# <ppid>) in <cwd>, cgroup <cgroup>; gate <gate> (<SPAWN_GATE's part>)".
# A start the lock refuses records nothing.
#
# Black box, on a database of its own (`spawn init-db`), no missions. Each
# integrator is started by a starter of its own: a copy of sh named
# "st) 1 (234" that runs it as a child (not exec), so the parent's pid and
# comm are known to the scenario. Its name holds ") 1 (": a stat read by
# the first ")" takes a wrong parent, which ends the ancestors there.
#   (1) A, started by hand: in $WORK/here, SPAWN_GATE unset (the default
#       scripts/rsi/gate.sh is a stub there), --root ws-a (relative). Within
#       30 s one integrator.started is recorded with exactly the fields
#       above: pid = A, ppid = A's starter, parent_comm "st) 1 (234",
#       ancestors [starter, this scenario's shell, ...] ending at pid 1 (or
#       16 long), cwd $WORK/here, cgroup this scenario's own unified line
#       (null when it has none), gate $WORK/here/scripts/rsi/gate.sh,
#       gate_from_env false, root $WORK/here/ws-a, once false. A's stdout
#       holds the started line with those values.
#   (2) B starts while A runs (SPAWN_GATE set): refused at 15 s (exit 1),
#       and still one integrator.started.
#   (3) A is stopped; C runs --once with SPAWN_GATE naming $WORK/gate.sh:
#       exit 0, and a second integrator.started: pid = C, gate $WORK/gate.sh,
#       gate_from_env true, once true.
#   (4) The store refuses integrator.started (the scenario's database wraps
#       spawn_integrator_record to raise on it): D, --once, still exits 0,
#       its stderr says "integrate: cannot record integrator.started: ...",
#       and no record is added.
#   (5, rev-k's R1 F1 and F2) E, --once, SPAWN_GATE set but empty, started
#       under 18 nested sh: its record (the third) has gate
#       $WORK/here/scripts/rsi/gate.sh and gate_from_env false (an empty
#       value names nothing), ancestors[0] = its ppid, and exactly 16
#       ancestors.
#
# On live main bde44342 it exits 1 at (1): no integrator.started is recorded.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "234: harness: $*" >&2
    exit 2
}
[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
controller=$(cd "$(dirname "$0")/../.." && pwd)
for tool in psql od timeout git; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
[ -n "$WORK" ] && [ -d "$WORK" ] || harness "mktemp gave no directory"
WORK=$(cd "$WORK" && pwd -P)
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -m 700 "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_234_start_$run"
database_made=""
pids=""
cleanup() {
    for pid in $pids; do
        kill "$pid" 2>/dev/null || true
    done
    for pid in $pids; do
        wait "$pid" 2>/dev/null || true
    done
    pids=""
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    set +e
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "234: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
export SPAWN_DATABASE_URL
tries=0
until env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db >"$WORK/init.out" 2>&1 </dev/null; do
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(tail -n 3 "$WORK/init.out")"
    sleep 1
done
for name in SPAWN_GATE SPAWN_GATE_ENV_FILE SPAWN_INTEGRATE_GATES SPAWN_GATE_SHADOW_RUNNER \
    SPAWN_TEST_DATABASE_LOG SPAWN_WORKSPACE_ROOT; do
    unset "$name" 2>/dev/null || true
done
SPAWN_INTEGRATE_WARM=0
export SPAWN_INTEGRATE_WARM
printf 'exit 0\n' >"$WORK/gate.sh"

sh_path=$(command -v sh) || harness "no sh"
cp "$sh_path" "$WORK/st) 1 (234" || harness "cannot copy sh"
mkdir -p "$WORK/here/scripts/rsi" || harness "cannot make the start directory"
printf 'exit 0\n' >"$WORK/here/scripts/rsi/gate.sh"

check_failed() {
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
q() {
    psql -d "$SPAWN_DB_NAME" -qtA -v ON_ERROR_STOP=1 -c "$1" || harness "psql: $1"
}
STARTED="from spawn_events e where e.session_id = 'session_control'
    and e.event_name = 'control.recorded'
    and e.kind -> 'detail' -> 'record' ->> 'type' = 'integrator.started'"
count_started() { q "select count(*) $STARTED"; }
# field <n> <path>: the n-th integrator.started record's field (jsonb #>> path), "null" if null
field() {
    q "select coalesce(r #>> '{$2}', 'null') from (select e.kind -> 'detail' -> 'record' r $STARTED
        order by e.id offset $(($1 - 1)) limit 1) x"
}
keys() {
    q "select string_agg(k, ',' order by k) from (select jsonb_object_keys(r) k from
        (select e.kind -> 'detail' -> 'record' r $STARTED order by e.id offset $(($1 - 1)) limit 1) x) y"
}
expect() { # <what> <expected> <got>
    [ "$3" = "$2" ] || check_failed "$1: expected [$2], got [$3]"
}
# start_by_hand <name> <dir> <args...>: in <dir>, "st) 1 (234" runs the
# integrator as its child; the starter's pid in $starter, the integrator's
# in $WORK/<name>.pid, its output in $WORK/<name>.out and .err.
start_by_hand() {
    n=$1 d=$2
    shift 2
    (cd "$d" && exec "$WORK/st) 1 (234" -c '
        out=$1 err=$2 pidf=$3
        shift 3
        "$@" >"$out" 2>"$err" </dev/null &
        echo "$!" >"$pidf.new" && mv "$pidf.new" "$pidf"
        wait "$!"' starter "$WORK/$n.out" "$WORK/$n.err" "$WORK/$n.pid" "$@") &
    starter=$!
    pids="$pids $starter"
    i=0
    until [ -s "$WORK/$n.pid" ]; do
        i=$((i + 1))
        [ "$i" -lt 100 ] || harness "$n's starter wrote no pid within 10 s"
        sleep 0.1
    done
}

# --- (1) A, started by hand, is named ----------------------------------------------
start_by_hand A "$WORK/here" env -u SPAWN_TEST_DATABASE_URL -u SPAWN_GATE "$SPAWN_BIN" integrate --root ws-a
a=$(cat "$WORK/A.pid")
pids="$pids $a"
i=0
while [ "$(count_started)" -lt 1 ]; do
    i=$((i + 1))
    if [ "$i" -ge 300 ] || ! kill -0 "$a" 2>/dev/null; then
        asserted=1
        check_failed "(1) no integrator.started recorded within 30 s of A's start (A alive: $(kill -0 "$a" 2>/dev/null && echo yes || echo no); A said: $(tail -n 2 "$WORK/A.out" | tr '\n' ' ') $(tail -n 2 "$WORK/A.err" | tr '\n' ' '))"
        exit 1
    fi
    sleep 0.1
done
[ -s "$WORK/A.out" ] || sleep 1
my_cgroup=$(sed -n 's/^0:://p' /proc/$$/cgroup 2>/dev/null | head -n 1)
[ -n "$my_cgroup" ] || my_cgroup=null
expect "(1) the record's fields" "ancestors,cgroup,cwd,gate,gate_from_env,once,parent_comm,pid,ppid,root,type" "$(keys 1)"
expect "(1) pid (A)" "$a" "$(field 1 pid)"
expect "(1) ppid (A's starter)" "$starter" "$(field 1 ppid)"
expect "(1) parent_comm" "st) 1 (234" "$(field 1 parent_comm)"
expect "(1) ancestors[0]" "$starter st) 1 (234" "$(field 1 ancestors,0,pid) $(field 1 ancestors,0,comm)"
expect "(1) ancestors[1] (this scenario's shell)" "$$" "$(field 1 ancestors,1,pid)"
n_anc=$(q "select jsonb_array_length(r -> 'ancestors') from (select e.kind -> 'detail' -> 'record' r $STARTED order by e.id limit 1) x")
last_anc=$(field 1 "ancestors,$((n_anc - 1)),pid")
[ "$last_anc" = 1 ] || [ "$n_anc" = 16 ] ||
    check_failed "(1) the ancestors end at pid $last_anc after $n_anc, neither pid 1 nor 16 long"
expect "(1) cwd" "$WORK/here" "$(field 1 cwd)"
expect "(1) cgroup (this scenario's own)" "$my_cgroup" "$(field 1 cgroup)"
expect "(1) gate (the default, read from cwd)" "$WORK/here/scripts/rsi/gate.sh" "$(field 1 gate)"
expect "(1) gate_from_env" false "$(field 1 gate_from_env)"
expect "(1) root (absolute)" "$WORK/here/ws-a" "$(field 1 root)"
expect "(1) once" false "$(field 1 once)"
want_line="integrate: started as pid $a by st) 1 (234 (pid $starter) in $WORK/here, cgroup $(
    [ "$my_cgroup" = null ] && echo unknown || echo "$my_cgroup"); gate $WORK/here/scripts/rsi/gate.sh (SPAWN_GATE unset: the default, read from the working directory)"
grep -qxF "$want_line" "$WORK/A.out" ||
    check_failed "(1) A's stdout has no line [$want_line]: [$(head -n 3 "$WORK/A.out" | tr '\n' '~')]"
echo "234: (1) checked: A's start named (pid, starter, ancestors, cwd, cgroup, default gate, root)" >&2

# --- (2) B, refused by the lock, records nothing --------------------------------------
code=0
env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/gate.sh" timeout 40 \
    "$SPAWN_BIN" integrate --root "$WORK/ib" >"$WORK/B.out" 2>"$WORK/B.err" </dev/null || code=$?
expect "(2) B's exit, started while A holds the lock" 1 "$code"
expect "(2) integrator.started records after B's refused start" 1 "$(count_started)"
kill -0 "$a" 2>/dev/null || check_failed "(2) A ended while B was refused"
echo "234: (2) checked: a refused start records nothing" >&2

# --- (3) C, --once with SPAWN_GATE ---------------------------------------------------
kill "$a" 2>/dev/null || true
wait "$starter" 2>/dev/null || true
i=0
while kill -0 "$a" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -lt 150 ] || harness "A still runs 15 s after TERM"
    sleep 0.1
done
start_by_hand C "$WORK" env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/gate.sh" \
    "$SPAWN_BIN" integrate --once --root "$WORK/ic"
c=$(cat "$WORK/C.pid")
code=0
wait "$starter" || code=$?
expect "(3) C's exit (--once)" 0 "$code"
expect "(3) integrator.started records after C" 2 "$(count_started)"
expect "(3) pid (C)" "$c" "$(field 2 pid)"
expect "(3) ppid (C's starter)" "$starter" "$(field 2 ppid)"
expect "(3) parent_comm" "st) 1 (234" "$(field 2 parent_comm)"
expect "(3) gate (SPAWN_GATE's)" "$WORK/gate.sh" "$(field 2 gate)"
expect "(3) gate_from_env" true "$(field 2 gate_from_env)"
expect "(3) once" true "$(field 2 once)"
expect "(3) root" "$WORK/ic" "$(field 2 root)"
expect "(3) cwd" "$WORK" "$(field 2 cwd)"
grep -qF "; gate $WORK/gate.sh (from SPAWN_GATE)" "$WORK/C.out" ||
    check_failed "(3) C's stdout does not end its started line with [; gate $WORK/gate.sh (from SPAWN_GATE)]: [$(head -n 2 "$WORK/C.out" | tr '\n' '~')]"
echo "234: (3) checked: a --once start with SPAWN_GATE named" >&2

# --- (5) rev-k's R1 F1/F2: E, --once, SPAWN_GATE set empty, started 18 shells deep ---
printf '%s\n' 'n=$1; shift' 'if [ "$n" -gt 0 ]; then sh "$0" $((n - 1)) "$@"; else "$@"; fi' >"$WORK/nest.sh"
start_by_hand E "$WORK/here" env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE= \
    sh "$WORK/nest.sh" 18 "$SPAWN_BIN" integrate --once --root "$WORK/ie"
code=0
wait "$starter" || code=$?
expect "(5) E's exit" 0 "$code"
expect "(5) integrator.started records after E" 3 "$(count_started)"
expect "(5) gate (empty SPAWN_GATE: the default)" "$WORK/here/scripts/rsi/gate.sh" "$(field 3 gate)"
expect "(5) gate_from_env (SPAWN_GATE set but empty names nothing)" false "$(field 3 gate_from_env)"
expect "(5) ancestors[0] is the parent" "$(field 3 ppid)" "$(field 3 ancestors,0,pid)"
n5=$(q "select jsonb_array_length(r -> 'ancestors') from (select e.kind -> 'detail' -> 'record' r $STARTED order by e.id offset 2 limit 1) x")
expect "(5) at most 16 ancestors, and 16 when there are more" 16 "$n5"
echo "234: (5) checked (rev-k variant): empty SPAWN_GATE, 16 ancestors of a deeper chain" >&2

# --- (4) a record the store refuses: the start goes on ----------------------------
q "alter function spawn_integrator_record(jsonb) rename to spawn_integrator_record_k" >/dev/null
q "create function spawn_integrator_record(p_record jsonb) returns bigint language plpgsql as \$f\$
begin
    if p_record ->> 'type' = 'integrator.started' then
        raise exception 'the scenario refuses integrator.started';
    end if;
    return spawn_integrator_record_k(p_record);
end \$f\$" >/dev/null
start_by_hand D "$WORK" env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/gate.sh" \
    "$SPAWN_BIN" integrate --once --root "$WORK/id"
code=0
wait "$starter" || code=$?
expect "(4) D's exit (--once, its record refused)" 0 "$code"
expect "(4) integrator.started records after D" 3 "$(count_started)"
grep -qF "integrate: cannot record integrator.started: " "$WORK/D.err" ||
    check_failed "(4) D's stderr does not say [integrate: cannot record integrator.started: ...]: [$(tail -n 3 "$WORK/D.err" | tr '\n' '~')]"
echo "234: (4) checked: a refused record is said, and the start goes on" >&2

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "234: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "234: each integrator start that takes the lock is named in the store: its process, its starter and ancestors, cwd, cgroup, gate, root and once"
exit 0
