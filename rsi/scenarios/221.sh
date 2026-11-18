#!/bin/sh
# Contract 221 [recoverability]: one `spawn integrate` per database, and a
# gate script that is not there refuses the start. The incident of
# 2026-11-18 15:03: a second integrator, started by hand in
# ~/.local/share/spawn-rsi with SPAWN_GATE unset, read the default
# scripts/rsi/gate.sh against that directory (no such file). It took the
# integrator's lock at every pass the unit's integrator left idle (the lock
# was taken per pass and let go when no gate ran), so it judged mission 9:
# every gate run was `sh <missing file>`, an abort, and three aborts halted
# the mission. FD 060's owner landing stalled 25 minutes.
#
# Required:
#   - `spawn integrate` takes the lock before it writes anything and keeps
#     it for its whole life, idle or not; while held, the lock's connection
#     is named "spawn integrate pid=<pid>" (the server keeps 63 bytes of a
#     name: no root).
#   - A second start on the same database waits up to 15 s for the lock (a
#     deploy's restart: the old integrator just stopped), and takes it if it
#     is freed within that; otherwise it exits 1 and says, on stderr:
#     "spawn: integrate: another integrator holds this database's lock
#     (<the holder's name>, connected at <YYYY-MM-DD HH:MM:SS> UTC); this one
#     does not start. Stop that one first, or drain it with `spawn integrate
#     --drain --root <its root>`." It records nothing and makes no root.
#   - A start whose gate script is not a readable file exits 1 before the
#     database is touched: "spawn: integrate: the gate script <path> does
#     not exist (SPAWN_GATE is unset: its default scripts/rsi/gate.sh is
#     read from the working directory <cwd>); this integrator does not
#     start", or "(from SPAWN_GATE)" when SPAWN_GATE named it.
# Under the unit (deploy/systemd/spawn-integrate.service: Restart=always,
# RestartSec=10) a refused start is started again 10 s after it exits, so
# the unit's integrator takes the lock at the first start after the holder
# ended; each refused start takes 15 s, so systemd's start limit (5 starts
# in 10 s by default) is never reached.
#
# Black box, on a database of its own (`spawn init-db`), no missions: the
# integrators have nothing to judge, so only their starts are under test.
# Every integrator gets a stub gate (SPAWN_GATE, `exit 0`) unless the check
# says otherwise, and a root of its own.
#   (1) A starts and passes; read from pg_locks five times over 2.5 s while
#       A is idle: the integrator's lock is held each time, by the
#       connection named "spawn integrate pid=<A>".
#   (2) B starts while A runs: it exits 1 at 15 s or later (bounded at
#       40 s), with the refusal naming A on stderr; its root was not made;
#       the log holds the same number of records as before B.
#   (3) A deploy's restart: C starts while A runs, A is stopped (TERM) 3 s
#       later; C says it waits, takes the lock within its 15 s and runs on:
#       pg_locks names C within 20 s of its start.
#   (4) D starts in a directory with no scripts/rsi/gate.sh, SPAWN_GATE
#       unset, while C runs: it exits 1 within 10 s with the gate refusal
#       (not the lock's, since the gate is checked first) naming the
#       resolved path and the directory; E, with SPAWN_GATE naming a file
#       that is not there, exits 1 with "(from SPAWN_GATE)". C still holds
#       the lock; the log holds the same number of records as before D.
#   (5) C's lock is lost: its connection is ended on the server
#       (pg_terminate_backend) and F starts, taking the lock within 20 s.
#       C stops within 30 s: exit 1, its last line on stderr "spawn:
#       integrate: this integrator's connection to the database ended and
#       another integrator holds the lock now (spawn integrate pid=<F>,
#       connected at <time> UTC); this one stops".
#   (4b, rev-k's R1 F1) H, SPAWN_GATE naming a directory: exit 1 within 10 s,
#       "... is not a readable file (from SPAWN_GATE) ...", no root made.
#   (4c, rev-i's R2 F1) I, SPAWN_GATE naming a regular file of mode 000: exit 1
#       within 10 s, "... is not a readable file (from SPAWN_GATE) ...", no root
#       made. Under a user who can still read a mode-000 file (root) the
#       scenario cannot judge it: a harness failure (2).
#   (6, rev-k's R1 F2) F's passes fail on a live connection (spawn_now_ms
#       renamed in the scenario's database): F keeps its connection and the
#       lock, so G, started after F's failed pass, is refused at 15 s naming
#       F; spawn_now_ms is renamed back and F runs on.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "221: harness: $*" >&2
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
SPAWN_DB_NAME="bb_221_one_$run"
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
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "221: harness: unexpected exit $rc" >&2
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

check_failed() {
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
# The integrator's lock on this database: "<granted pid's application_name>"
# per holder, one line each (none when free).
holders() {
    psql -d "$SPAWN_DB_NAME" -qtA -v ON_ERROR_STOP=1 -c "select coalesce(a.application_name, '')
        from pg_locks l join pg_stat_activity a on a.pid = l.pid
       where l.locktype = 'advisory' and l.granted and l.classid = 1399957336 and l.objsubid = 2
         and l.database = (select oid from pg_database where datname = current_database())" ||
        harness "cannot read pg_locks"
}
records() {
    psql -d "$SPAWN_DB_NAME" -qtA -v ON_ERROR_STOP=1 -c "select count(*) from spawn_events" ||
        harness "cannot count the log's records"
}
# start <name> <root> [env...]: an integrator in the background, its pid in
# $started, its output in $WORK/<name>.out and .err.
start() {
    n=$1 r=$2
    shift 2
    env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/gate.sh" "$@" \
        "$SPAWN_BIN" integrate --root "$r" >"$WORK/$n.out" 2>"$WORK/$n.err" </dev/null &
    started=$!
    pids="$pids $started"
}
alive() { kill -0 "$1" 2>/dev/null; }
# refusal <holder pid>: the line a refused start says
refusal() {
    printf '%s' "spawn: integrate: another integrator holds this database's lock (spawn integrate pid=$1, connected at "
}
refusal_end=' UTC); this one does not start. Stop that one first, or drain it with `spawn integrate --drain --root <its root>`.'

# --- (1) A holds the lock while idle -------------------------------------------
start A "$WORK/ia"
a=$started
i=0
until [ -s "$WORK/ia/integrate.pid" ]; do
    alive "$a" || harness "A ended at start: $(tail -n 3 "$WORK/A.err" | tr '\n' ' ')"
    i=$((i + 1))
    [ "$i" -lt 300 ] || harness "A wrote no pid file within 30 s: $(tail -n 3 "$WORK/A.err" | tr '\n' ' ')"
    sleep 0.1
done
sleep 1
want_a="spawn integrate pid=$a"
for k in 1 2 3 4 5; do
    got=$(holders)
    [ "$got" = "$want_a" ] || {
        check_failed "(1) reading $k of the idle A's lock: expected one holder [$want_a], got [$(printf '%s' "$got" | tr '\n' '~')]"
        break
    }
    sleep 0.5
done
echo "221: (1) checked: A holds the lock while idle, by name" >&2

# --- (2) B is refused after 15 s, naming A ---------------------------------------
before=$(records)
t0=$(date +%s)
code=0
env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/gate.sh" timeout 40 \
    "$SPAWN_BIN" integrate --root "$WORK/ib" >"$WORK/B.out" 2>"$WORK/B.err" </dev/null || code=$?
waited=$(($(date +%s) - t0))
if [ "$code" -ne 1 ]; then
    check_failed "(2) B, started while A runs, exited $code after ${waited} s (124: still running at 40 s), not 1: $(tail -n 2 "$WORK/B.err" | tr '\n' ' ')"
else
    [ "$waited" -ge 15 ] || check_failed "(2) B was refused after ${waited} s, before its 15 s wait"
    line=$(tail -n 1 "$WORK/B.err")
    case $line in
    "$(refusal "$a")"*"$refusal_end") ;;
    *) check_failed "(2) B's last line on stderr is not [$(refusal "$a")...$refusal_end]: [$line]" ;;
    esac
fi
[ ! -e "$WORK/ib" ] || check_failed "(2) the refused B made its root $WORK/ib"
after=$(records)
[ "$after" = "$before" ] || check_failed "(2) the log held $before records before B and $after after it"
alive "$a" || check_failed "(2) A ended while B was refused"
echo "221: (2) checked: B refused, naming A" >&2

# --- (3) a deploy's restart: C takes the lock A frees within C's wait ------------
start C "$WORK/ic"
c=$started
t0=$(date +%s)
sleep 3
alive "$c" || check_failed "(3) C ended within 3 s while A runs: $(tail -n 2 "$WORK/C.err" | tr '\n' ' ')"
kill "$a" 2>/dev/null || true
wait "$a" 2>/dev/null || true
want_c="spawn integrate pid=$c"
got=""
while [ $(($(date +%s) - t0)) -lt 20 ]; do
    got=$(holders)
    [ "$got" = "$want_c" ] && break
    alive "$c" || break
    sleep 0.5
done
if [ "$got" != "$want_c" ]; then
    check_failed "(3) 20 s after C's start (A stopped at 3 s), the lock's holders are [$(printf '%s' "$got" | tr '\n' '~')], not [$want_c]; C $(alive "$c" && echo runs || echo ended): $(tail -n 2 "$WORK/C.err" | tr '\n' ' ')"
fi
grep -qF "integrate: another integrator holds this database's lock (spawn integrate pid=$a, connected at " "$WORK/C.out" ||
    check_failed "(3) C did not say that it waits for A's lock: $(head -n 3 "$WORK/C.out" | tr '\n' ' ')"
echo "221: (3) checked: C takes the lock A freed within its wait" >&2

# --- (4) a gate script that is not there --------------------------------------------
mkdir -p "$WORK/cwd"
cwd=$(cd "$WORK/cwd" && pwd -P)
before=$(records)
code=0
(cd "$cwd" && env -u SPAWN_TEST_DATABASE_URL -u SPAWN_GATE timeout 10 \
    "$SPAWN_BIN" integrate --root "$WORK/id" >"$WORK/D.out" 2>"$WORK/D.err" </dev/null) || code=$?
want="spawn: integrate: the gate script $cwd/scripts/rsi/gate.sh does not exist (SPAWN_GATE is unset: its default scripts/rsi/gate.sh is read from the working directory $cwd); this integrator does not start"
if [ "$code" -ne 1 ]; then
    check_failed "(4) D, with no gate script, exited $code (124: still running at 10 s), not 1: $(tail -n 2 "$WORK/D.err" | tr '\n' ' ')"
else
    [ "$(tail -n 1 "$WORK/D.err")" = "$want" ] ||
        check_failed "(4) D's last line on stderr is not [$want]: [$(tail -n 1 "$WORK/D.err")]"
fi
code=0
env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/nope/gate.sh" timeout 10 \
    "$SPAWN_BIN" integrate --root "$WORK/ie" >"$WORK/E.out" 2>"$WORK/E.err" </dev/null || code=$?
want="spawn: integrate: the gate script $WORK/nope/gate.sh does not exist (from SPAWN_GATE); this integrator does not start"
if [ "$code" -ne 1 ]; then
    check_failed "(4) E, SPAWN_GATE naming no file, exited $code (124: still running at 10 s), not 1: $(tail -n 2 "$WORK/E.err" | tr '\n' ' ')"
else
    [ "$(tail -n 1 "$WORK/E.err")" = "$want" ] ||
        check_failed "(4) E's last line on stderr is not [$want]: [$(tail -n 1 "$WORK/E.err")]"
fi
for root in id ie; do
    [ ! -e "$WORK/$root" ] || check_failed "(4) a refused start made its root $WORK/$root"
done
[ "$(holders)" = "$want_c" ] || check_failed "(4) after D and E, the lock's holders are [$(holders | tr '\n' '~')], not [$want_c]"
after=$(records)
[ "$after" = "$before" ] || check_failed "(4) the log held $before records before D and E and $after after them"
echo "221: (4) checked: a missing gate script refuses the start" >&2

# --- (5) a lock lost to another integrator stops the one that lost it ------------
psql -d "$SPAWN_DB_NAME" -qtA -v ON_ERROR_STOP=1 -c "select pg_terminate_backend(l.pid)
    from pg_locks l
   where l.locktype = 'advisory' and l.granted and l.classid = 1399957336 and l.objsubid = 2
     and l.database = (select oid from pg_database where datname = current_database())" \
    >/dev/null || harness "cannot end C's connection"
start F "$WORK/if"
f=$started
want_f="spawn integrate pid=$f"
t0=$(date +%s)
got=""
while [ $(($(date +%s) - t0)) -lt 20 ]; do
    got=$(holders)
    [ "$got" = "$want_f" ] && break
    sleep 0.5
done
[ "$got" = "$want_f" ] ||
    check_failed "(5) 20 s after C's connection ended, the lock's holders are [$(printf '%s' "$got" | tr '\n' '~')], not F's [$want_f]"
while [ $(($(date +%s) - t0)) -lt 30 ] && alive "$c"; do
    sleep 0.5
done
if alive "$c"; then
    check_failed "(5) C runs on 30 s after its lock went to F: $(tail -n 2 "$WORK/C.err" | tr '\n' ' ')"
else
    code=0
    wait "$c" || code=$?
    [ "$code" -eq 1 ] || check_failed "(5) C stopped with exit $code, not 1"
    line=$(tail -n 1 "$WORK/C.err")
    case $line in
    "spawn: integrate: this integrator's connection to the database ended and another integrator holds the lock now ($want_f, connected at "*" UTC); this one stops") ;;
    *) check_failed "(5) C's last line on stderr is not the lost lock's, naming F: [$line]" ;;
    esac
fi
echo "221: (5) checked: a lock lost to another integrator stops the one that lost it" >&2

# --- (4b, rev-k) a directory named as the gate script -----------------------------------
code=0
env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/cwd" timeout 10 \
    "$SPAWN_BIN" integrate --root "$WORK/ih" >"$WORK/H.out" 2>"$WORK/H.err" </dev/null || code=$?
want="spawn: integrate: the gate script $WORK/cwd is not a readable file (from SPAWN_GATE); this integrator does not start"
if [ "$code" -ne 1 ]; then
    check_failed "(4b) H, SPAWN_GATE naming a directory, exited $code (124: still running at 10 s), not 1: $(tail -n 2 "$WORK/H.err" | tr '\n' ' ')"
else
    [ "$(tail -n 1 "$WORK/H.err")" = "$want" ] ||
        check_failed "(4b) H's last line on stderr is not [$want]: [$(tail -n 1 "$WORK/H.err")]"
fi
[ ! -e "$WORK/ih" ] || check_failed "(4b) a refused start made its root $WORK/ih"
echo "221: (4b) checked: a directory as the gate script refuses the start" >&2

# --- (4c, rev-i) a regular file this process cannot read, named as the gate script ------
printf 'exit 0\n' >"$WORK/unreadable-gate.sh" || harness "cannot write $WORK/unreadable-gate.sh"
chmod 000 "$WORK/unreadable-gate.sh" || harness "cannot chmod 000 $WORK/unreadable-gate.sh"
if cat "$WORK/unreadable-gate.sh" >/dev/null 2>&1; then
    harness "(4c) a mode-000 file is still readable by uid $(id -u) (root?): this check cannot judge here"
fi
code=0
env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/unreadable-gate.sh" timeout 10 \
    "$SPAWN_BIN" integrate --root "$WORK/ii" >"$WORK/I.out" 2>"$WORK/I.err" </dev/null || code=$?
want="spawn: integrate: the gate script $WORK/unreadable-gate.sh is not a readable file (from SPAWN_GATE); this integrator does not start"
if [ "$code" -ne 1 ]; then
    check_failed "(4c) I, SPAWN_GATE naming a mode-000 file, exited $code (124: still running at 10 s), not 1: $(tail -n 2 "$WORK/I.err" | tr '\n' ' ')"
else
    [ "$(tail -n 1 "$WORK/I.err")" = "$want" ] ||
        check_failed "(4c) I's last line on stderr is not [$want]: [$(tail -n 1 "$WORK/I.err")]"
fi
[ ! -e "$WORK/ii" ] || check_failed "(4c) a refused start made its root $WORK/ii"
echo "221: (4c) checked: an unreadable gate script refuses the start" >&2

# --- (6, rev-k) a pass that fails on a live connection keeps the lock -------------------
# F's passes fail (a function they read is renamed) while its connection lives:
# it keeps the lock, so G, started then, waits its 15 s and is refused naming F.
errs=$(wc -l <"$WORK/F.err")
psql -d "$SPAWN_DB_NAME" -qtA -v ON_ERROR_STOP=1 -c "alter function spawn_now_ms() rename to spawn_now_ms_k" >/dev/null ||
    harness "cannot rename spawn_now_ms"
t0=$(date +%s)
while [ $(($(date +%s) - t0)) -lt 20 ] && [ "$(wc -l <"$WORK/F.err")" -le "$errs" ]; do
    sleep 0.5
done
[ "$(wc -l <"$WORK/F.err")" -gt "$errs" ] || harness "(6) F's pass did not fail within 20 s of the rename"
note6=$(tail -n 1 "$WORK/F.err" | cut -c1-160)
code=0
t1=$(date +%s)
env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/gate.sh" timeout 40 \
    "$SPAWN_BIN" integrate --root "$WORK/ig" >"$WORK/G.out" 2>"$WORK/G.err" </dev/null || code=$?
waited=$(($(date +%s) - t1))
if [ "$code" -ne 1 ]; then
    check_failed "(6) G, started while F's passes fail, exited $code after ${waited} s (124: still running at 40 s, so it took the lock), not 1; F's pass said [$note6]"
else
    line=$(tail -n 1 "$WORK/G.err")
    case $line in
    "$(refusal "$f")"*"$refusal_end") ;;
    *) check_failed "(6) G's last line on stderr is not the refusal naming F: [$line]" ;;
    esac
fi
psql -d "$SPAWN_DB_NAME" -qtA -v ON_ERROR_STOP=1 -c "alter function spawn_now_ms_k() rename to spawn_now_ms" >/dev/null ||
    harness "cannot rename spawn_now_ms back"
alive "$f" || check_failed "(6) F ended while its passes failed: $(tail -n 2 "$WORK/F.err" | tr '\n' ' ')"
echo "221: (6) checked: a pass failing on a live connection keeps the lock (F's pass: $note6)" >&2

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "221: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "221: one integrator per database for its life; a missing gate script refuses the start"
exit 0
