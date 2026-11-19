#!/bin/sh
# Objective 237 [recoverability][performance]: a scenario harness never drops
# a database WITH (FORCE) while a loop of its own run holds LISTEN, and
# every harness drop is bounded, so a hang fails instead of holding the test
# server (and suite.lock) for every run.
#
# Why: on 2026-11-19 check-j02-lands.sh hung the shared test server twice.
# Its section [004] dropped its database WITH (FORCE) while section L's
# `spawn loop` held LISTEN on the run's other database. A loop drains that
# connection only between passes, so its backend sat blocked sending to it
# (ClientWrite). Such a backend never absorbs the server's ProcSignalBarrier,
# which every DROP DATABASE ... WITH (FORCE) waits on: the drop, and every
# other run's drop behind it, waited about 19 minutes.
#
# Black box, on databases of its own on the test server:
#   (1) check-j02-lands.sh runs whole with a `psql` first on PATH that, at
#       each "drop database", counts the backends of the same run's other
#       databases whose statement is a LISTEN, then runs the real psql. j02
#       exits 0; at least two drops are seen (its two databases); every
#       count is 0.
#   (2) A listener on database a that reads nothing is flooded with
#       notifications until its backend waits on ClientWrite. Then
#       `scenario-db.sh drop b` with SPAWN_SCENARIO_DB_DROP_SECS=3 ends
#       within 15 s (a 20 s timeout around it is not reached), says
#       "scenario-db: could not drop database <b>: ...", and b is still
#       there. With the listener gone, the same drop removes b.
#   (3) SPAWN_SCENARIO_DB_DROP_SECS soon, 0 (statement_timeout 0 would be no
#       bound at all) and 3601 are each refused: exit 2, saying
#       "SPAWN_SCENARIO_DB_DROP_SECS is a whole number of seconds from 1 to
#       3600", and nothing is dropped (rev-i's R0 F1).
#   (4) The sweep is bounded too (rev-i's R0 F2): database c, a leftover
#       under SPAWN_SCENARIO_DB_SWEEP_MINUTES=0, and the listener of (2)
#       stuck in ClientWrite again. `scenario-db.sh sweep <c>` with
#       SPAWN_SCENARIO_DB_DROP_SECS=3 ends within 15 s (a 20 s timeout around
#       it is not reached), says "scenario-db: could not drop database <c>: ...",
#       and c is still there. With the listener gone, the same sweep drops c.
#   (5) The default bound is 60 s (rev-b's R0 F3; the gate and the runners
#       set no SPAWN_SCENARIO_DB_DROP_SECS, so the default is what guards the
#       shared server): with the setting unset, `scenario-db.sh drop d` and
#       `scenario-db.sh sweep d` run through a `psql` first on PATH that
#       records each call's statements. Every psql session they open sends
#       "set statement_timeout = '60s'" before anything else, and d is dropped.
#
# (2) holds the test server's DROP ... WITH (FORCE) for at most the 3 s
# bound (the 20 s timeout on a candidate without it).
#
# On live main 9e1a8d22 it exits 1: (1) a drop with the loop's LISTEN
# present, (2) and (4) not done within 20 s, (3) the settings not refused,
# (5) no bound sent.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -u

asserted=""
harness() {
    echo "237: harness: $*" >&2
    exit 2
}
[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
controller=$(cd "$(dirname "$0")/../.." && pwd)
for tool in psql od timeout mkfifo date; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
real_psql=$(command -v psql)

WORK=$(mktemp -d) || harness "mktemp failed"
[ -n "$WORK" ] && [ -d "$WORK" ] || harness "mktemp gave no directory"
WORK=$(cd "$WORK" && pwd -P)

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
DB_A="bb_237_a_$run"
DB_B="bb_237_b_$run"
DB_C="bb_237_c_$run"
DB_D="bb_237_d_$run"
made=""
reader=""
cleanup() {
    if [ -n "$reader" ]; then
        kill "$reader" 2>/dev/null || true
        wait "$reader" 2>/dev/null || true
        reader=""
    fi
    exec 7>&- 2>/dev/null || true
    if [ -n "$made" ]; then
        for db in "$DB_A" "$DB_B" "$DB_C" "$DB_D"; do
            sh "$controller/scripts/blackbox/scenario-db.sh" drop "$db" >/dev/null 2>&1 || true
        done
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "237: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

: >"$WORK/failures"
check_failed() {
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
q() { "$real_psql" -X -d postgres -tAq -c "$1" 2>/dev/null; }
now_ms() { echo $(($(date +%s%N) / 1000000)); }

# --- (1) j02 drops nothing while its loop's LISTEN is there -----------------------------
mkdir -p "$WORK/bin" || harness "cannot make the psql shim's directory"
cat >"$WORK/bin/psql" <<'SHIM' || harness "cannot write the psql shim"
#!/bin/sh
# 237's psql: at a "drop database", the run's other databases' LISTEN backends, then the real psql.
for arg in "$@"; do
    case "$arg" in
    "drop database if exists "*)
        target=$(printf '%s\n' "$arg" | sed -n 's/^drop database if exists \([A-Za-z0-9_]*\).*/\1/p')
        rest=${target#bb_j02_}
        run=${rest#*_}
        if [ -n "$target" ] && [ "$rest" != "$target" ] && [ -n "$run" ]; then
            n=$("$REAL_PSQL_237" -X -d postgres -tAq -c "select count(*) from pg_stat_activity
                  where datname like 'bb\_j02\_%' and datname <> '$target'
                    and right(datname, length('$run') + 1) = '_$run'
                    and backend_type = 'client backend' and query ilike 'listen%'" 2>/dev/null)
            printf '%s %s\n' "$target" "${n:-unknown}" >>"$DROPS_237"
        fi
        ;;
    esac
done
exec "$REAL_PSQL_237" "$@"
SHIM
chmod +x "$WORK/bin/psql" || harness "cannot make the psql shim executable"
code=0
env PATH="$WORK/bin:$PATH" REAL_PSQL_237="$real_psql" DROPS_237="$WORK/drops.log" \
    sh "$controller/rsi/scenarios/check-j02-lands.sh" >"$WORK/j02.out" 2>&1 </dev/null || code=$?
case "$code" in
0) ;;
1) check_failed "(1) check-j02-lands.sh failed (exit 1): $(grep -m 3 'blackbox:' "$WORK/j02.out" | tr '\n' ' ')" ;;
*) harness "check-j02-lands.sh exited $code: $(tail -n 3 "$WORK/j02.out" | tr '\n' ' ')" ;;
esac
drops=$(grep -c . "$WORK/drops.log" 2>/dev/null || true)
[ "${drops:-0}" -ge 2 ] || harness "(1) the psql shim saw ${drops:-0} drop(s) of j02's databases, not at least 2"
while read -r target n; do
    [ "$n" = 0 ] ||
        check_failed "(1) j02 dropped $target WITH (FORCE) while $n LISTEN backend(s) of its own run were connected (its loop)"
done <"$WORK/drops.log"
echo "237: (1) checked: j02 ran whole and dropped none of its databases while its loop's LISTEN was there ($drops drops)" >&2

# --- (2) a drop behind a backend stuck in ClientWrite is bounded -------------------------
q "create database $DB_A" >/dev/null || harness "cannot create $DB_A"
made=1
q "create database $DB_B" >/dev/null || harness "cannot create $DB_B"
# stuck_listener <case>: a listener on a that reads nothing after its LISTEN
# (psql blocks on its input, never on the socket), flooded with
# notifications until its backend waits on ClientWrite.
stuck_listener() {
    rm -f "$WORK/fifo"
    mkfifo "$WORK/fifo" || harness "($1) mkfifo failed"
    exec 7<>"$WORK/fifo"
    "$real_psql" -X -d "$DB_A" <"$WORK/fifo" >/dev/null 2>&1 &
    reader=$!
    echo "listen ch_237;" >&7
    stuck=""
    i=0
    while [ "$i" -lt 10 ] && [ -z "$stuck" ]; do
        i=$((i + 1))
        "$real_psql" -X -d "$DB_A" -tAq -c "select count(pg_notify('ch_237', repeat('x', 7900) || g)) from generate_series(1, 2000) g" \
            >/dev/null 2>&1 || harness "($1) cannot notify on $DB_A"
        sleep 0.5
        [ "$(q "select wait_event from pg_stat_activity where datname = '$DB_A' and query ilike 'listen%'")" = ClientWrite ] && stuck=1
    done
    [ -n "$stuck" ] || harness "($1) the listener's backend never waited on ClientWrite after $i batches of 2000 notifications"
}
# release_listener <case>: the listener ends, and so does its backend.
release_listener() {
    kill "$reader" 2>/dev/null || true
    wait "$reader" 2>/dev/null || true
    reader=""
    exec 7>&-
    i=0
    while [ -n "$(q "select 1 from pg_stat_activity where datname = '$DB_A'")" ]; do
        i=$((i + 1))
        [ "$i" -lt 100 ] || harness "($1) the listener's backend is still there 10 s after its client ended"
        sleep 0.1
    done
}
stuck_listener 2
t0=$(now_ms)
code=0
SPAWN_SCENARIO_DB_DROP_SECS=3 timeout 20 sh "$controller/scripts/blackbox/scenario-db.sh" drop "$DB_B" \
    >"$WORK/drop.out" 2>"$WORK/drop.err" </dev/null || code=$?
took=$(($(now_ms) - t0))
if [ "$code" -eq 124 ]; then
    check_failed "(2) scenario-db.sh drop $DB_B behind a backend in ClientWrite was not done within 20 s (no bound)"
else
    [ "$took" -lt 15000 ] || check_failed "(2) the bounded drop took $took ms, not under 15 s"
    grep -qF "scenario-db: could not drop database $DB_B: " "$WORK/drop.err" ||
        check_failed "(2) the bounded drop does not say [scenario-db: could not drop database $DB_B: ...]: [$(tr '\n' '~' <"$WORK/drop.err")]"
    [ "$(q "select count(*) from pg_database where datname = '$DB_B'")" = 1 ] ||
        check_failed "(2) $DB_B is gone after a drop that could not finish"
fi
release_listener 2
SPAWN_SCENARIO_DB_DROP_SECS=3 timeout 20 sh "$controller/scripts/blackbox/scenario-db.sh" drop "$DB_B" >/dev/null 2>&1 </dev/null || true
[ "$(q "select count(*) from pg_database where datname = '$DB_B'")" = 0 ] ||
    check_failed "(2) with the listener gone, scenario-db.sh drop did not remove $DB_B"
echo "237: (2) checked: a drop behind a backend in ClientWrite failed at its bound ($took ms) and left the database; with the listener gone it dropped" >&2

# --- (3) a bound that is not a whole number from 1 to 3600 is refused ------------------------
for setting in soon 0 3601; do
    code=0
    SPAWN_SCENARIO_DB_DROP_SECS=$setting sh "$controller/scripts/blackbox/scenario-db.sh" drop "$DB_A" \
        >"$WORK/refused.out" 2>"$WORK/refused.err" </dev/null || code=$?
    [ "$code" -eq 2 ] || check_failed "(3) SPAWN_SCENARIO_DB_DROP_SECS=$setting: exit $code, not 2"
    grep -qF "SPAWN_SCENARIO_DB_DROP_SECS is a whole number of seconds from 1 to 3600" "$WORK/refused.err" ||
        check_failed "(3) the refusal of $setting does not name the setting: [$(tr '\n' '~' <"$WORK/refused.err")]"
    [ "$(q "select count(*) from pg_database where datname = '$DB_A'")" = 1 ] || {
        check_failed "(3) $DB_A was dropped under SPAWN_SCENARIO_DB_DROP_SECS=$setting"
        q "create database $DB_A" >/dev/null || harness "(3) cannot make $DB_A again"
    }
done
echo "237: (3) checked: soon, 0 and 3601 are refused and drop nothing" >&2

# --- (4) the sweep's drop behind a backend stuck in ClientWrite is bounded -----------------
q "create database $DB_C" >/dev/null || harness "cannot create $DB_C"
stuck_listener 4
t0=$(now_ms)
code=0
SPAWN_SCENARIO_DB_SWEEP_MINUTES=0 SPAWN_SCENARIO_DB_DROP_SECS=3 timeout 20 \
    sh "$controller/scripts/blackbox/scenario-db.sh" sweep "$DB_C" >"$WORK/sweep.out" 2>"$WORK/sweep.err" </dev/null || code=$?
took=$(($(now_ms) - t0))
if [ "$code" -eq 124 ]; then
    check_failed "(4) scenario-db.sh sweep $DB_C behind a backend in ClientWrite was not done within 20 s (no bound)"
else
    [ "$took" -lt 15000 ] || check_failed "(4) the bounded sweep took $took ms, not under 15 s"
    grep -qF "scenario-db: could not drop database $DB_C: " "$WORK/sweep.err" ||
        check_failed "(4) the bounded sweep does not say [scenario-db: could not drop database $DB_C: ...]: [$(tr '\n' '~' <"$WORK/sweep.err")]"
    [ "$(q "select count(*) from pg_database where datname = '$DB_C'")" = 1 ] ||
        check_failed "(4) $DB_C is gone after a sweep drop that could not finish"
fi
release_listener 4
SPAWN_SCENARIO_DB_SWEEP_MINUTES=0 SPAWN_SCENARIO_DB_DROP_SECS=3 timeout 20 \
    sh "$controller/scripts/blackbox/scenario-db.sh" sweep "$DB_C" >/dev/null 2>&1 </dev/null || true
[ "$(q "select count(*) from pg_database where datname = '$DB_C'")" = 0 ] ||
    check_failed "(4) with the listener gone, the sweep did not drop $DB_C"
echo "237: (4) checked: a sweep drop behind a backend in ClientWrite failed at its bound ($took ms); with the listener gone it dropped" >&2

# --- (5) the default bound, 60 s, on every session ---------------------------------------
mkdir -p "$WORK/bin5" || harness "cannot make the second psql shim's directory"
cat >"$WORK/bin5/psql" <<'SHIM' || harness "cannot write the second psql shim"
#!/bin/sh
# 237 (5)'s psql: one line per call, its -c statements joined by " ;; ", then the real psql.
line=""
want=""
for arg in "$@"; do
    if [ -n "$want" ]; then
        line="$line$(printf '%s' "$arg" | tr '\n' ' ') ;; "
        want=""
    elif [ "$arg" = -c ]; then
        want=1
    fi
done
printf '%s\n' "$line" >>"$CALLS_237"
exec "$REAL_PSQL_237" "$@"
SHIM
chmod +x "$WORK/bin5/psql" || harness "cannot make the second psql shim executable"
q "create database $DB_D" >/dev/null || harness "cannot create $DB_D"
: >"$WORK/calls.log"
env -u SPAWN_SCENARIO_DB_DROP_SECS PATH="$WORK/bin5:$PATH" REAL_PSQL_237="$real_psql" CALLS_237="$WORK/calls.log" \
    sh "$controller/scripts/blackbox/scenario-db.sh" drop "$DB_D" >/dev/null 2>"$WORK/d.err" </dev/null ||
    check_failed "(5) scenario-db.sh drop $DB_D with the setting unset failed: [$(tr '\n' '~' <"$WORK/d.err")]"
q "create database $DB_D" >/dev/null || harness "cannot create $DB_D again"
env -u SPAWN_SCENARIO_DB_DROP_SECS SPAWN_SCENARIO_DB_SWEEP_MINUTES=0 PATH="$WORK/bin5:$PATH" REAL_PSQL_237="$real_psql" \
    CALLS_237="$WORK/calls.log" sh "$controller/scripts/blackbox/scenario-db.sh" sweep "$DB_D" >/dev/null 2>"$WORK/d.err" </dev/null ||
    check_failed "(5) scenario-db.sh sweep $DB_D with the setting unset failed: [$(tr '\n' '~' <"$WORK/d.err")]"
calls=$(grep -c . "$WORK/calls.log" || true)
[ "${calls:-0}" -ge 5 ] || harness "(5) the psql shim saw ${calls:-0} calls, not the drop's two and the sweep's three or more"
grep -q "drop database if exists $DB_D with (force)" "$WORK/calls.log" || harness "(5) the shim saw no forced drop of $DB_D"
grep -q "drop database if exists $DB_D ;;" "$WORK/calls.log" || harness "(5) the shim saw no sweep drop of $DB_D"
while IFS= read -r call; do
    case "$call" in
    "set statement_timeout = '60s' ;; "*) ;;
    *) check_failed "(5) a psql session of scenario-db.sh did not start with the 60 s bound: [$call]" ;;
    esac
done <"$WORK/calls.log"
[ "$(q "select count(*) from pg_database where datname = '$DB_D'")" = 0 ] ||
    check_failed "(5) $DB_D is still there after the drop and the sweep"
echo "237: (5) checked: with the setting unset, every session of a drop and a sweep sent the 60 s bound first ($calls calls)" >&2

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "237: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "237: no harness drop WITH (FORCE) while its own loop holds LISTEN; every drop bounded"
exit 0
