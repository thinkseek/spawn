#!/bin/sh
# Black box (objective 238) [recoverability]: one outage never skips later
# hourly event-log dumps. spawn-backup.service (deploy/systemd) is started
# by an hourly timer and retries a failed run (Restart=on-failure). Its start
# limit (StartLimitIntervalSec=2h, StartLimitBurst=3) counted the timer's
# starts with the retries. On 2026-11-19 the 17:05 run, then 18:03 and 18:19
# failing (curl: SSL connection timeout), used it up. systemd refused the
# 18:34 retry and the 19:02 timer run ("Start request repeated too quickly"),
# and R2 had no dump from 17:05 to 20:20.
#
# The unit's own settings, read from the tree under test (its working
# directory): the start limit (StartLimitIntervalSec, StartLimitBurst) as
# its [Unit] sets it, and Restart= and RestartPreventExitStatus= as its
# [Service] sets them (a key in another section is ignored, as systemd does). They are
# given to a transient service of the systemd user manager, started by a
# transient timer of its own every 10 s (the hour, scaled), with RestartSec
# 2 s (the 15 min, scaled). Its command is a stand-in dump that counts its
# runs. Only this run's units (guarded-bb238-<pid>-<n>, inside the owner's
# guarded-* rule, never a spawn-* unit) are made, stopped and reset, on every
# exit (a failure, a harness stop or a signal); nothing else in the manager is
# touched.
#
# Required:
#   (1) a run succeeds (the hour's copy), the next F fail (an outage), F
#       being the unit's own StartLimitBurst plus one (systemd's default 5
#       when the file sets none: 6), so any start limit the outage could use
#       up is used up, a window that never resets (StartLimitIntervalSec=
#       infinity) included (R2 rev-g's F1, R3 rev-f's): a later start, a
#       retry or the timer's, still runs the dump, within 30 s + 3 s x F.
#       A StartLimitBurst over 100 is refused (harness) to keep the run
#       bounded.
#   (2) the retry is still there: the third run comes 2-5 s after the second
#       (RestartSec), not at the timer's next tick.
#   (3) exit 3 (a suspect dump) and exit 4 (the R2 limit) are still never
#       retried: one run each, and none more in 3 x RestartSec.
#
# On live main 9e1a8d22 (2h / 3) this exits 1 at (1): the fourth start in
# the window is refused, and so is every timer start after it.
#
# rsi: task-only
# The gate cannot judge it: its sandbox has no systemd user manager, so this
# exits 6 there (as 029). It runs only as objective 238's own scenario, never
# on the base for another gate nor as a regression (scripts/rsi/gate.sh,
# deploy/systemd/README.md "Which objectives the gate cannot judge"); the
# readers and the owner judge it on the host (R2, rev-g's F2).
#
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure; 6 cannot be judged here (no systemd user manager:
# the gate's sandbox), judged on the host by the readers and the owner.
set -eu

harness() {
    echo "238: harness: $*" >&2
    exit 2
}
cannot_judge() {
    echo "238: cannot be judged here: $*. The start limit is the user manager's; this objective is judged on the host." >&2
    exit 6
}
failures=0
check_failed() {
    echo "238: FAILED: $*" >&2
    failures=$((failures + 1))
}

tree=$PWD
unit_file=$tree/deploy/systemd/spawn-backup.service
[ -f "$unit_file" ] || harness "no $unit_file (run from the tree under test)"
for tool in systemctl systemd-run awk sed grep od tr date sleep cat; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
[ -n "$(systemctl --user show -p Version --value 2>/dev/null || true)" ] ||
    cannot_judge "no systemd user manager is reachable"

stem="guarded-bb238-$$-$(od -An -N2 -tu2 /dev/urandom | tr -d ' ')"
case $stem in guarded-bb238-[0-9]*-[0-9]*) ;; *) harness "a unit name of this run's own: [$stem]" ;; esac
WORK=""
cleanup() {
    for unit in "$stem.timer" "$stem.service" "$stem-x3.service" "$stem-x4.service"; do
        systemctl --user stop "$unit" >/dev/null 2>&1 || true
        systemctl --user reset-failed "$unit" >/dev/null 2>&1 || true
    done
    [ -z "$WORK" ] || rm -rf "$WORK"
}
trap cleanup EXIT
WORK=$(mktemp -d) || harness "mktemp failed"
[ -n "$WORK" ] && [ -d "$WORK" ] || harness "mktemp gave no directory"
# A signal exits through the same cleanup (the EXIT trap runs on exit).
trap 'exit 2' HUP INT TERM

# prop <section> <key>: the unit file's own value of <key> in [<section>]
# (its last assignment there, blanks around "=" allowed), or nothing.
# systemd reads each key in its own section only: StartLimitIntervalSec
# in [Service] is "Unknown key name ..., ignoring" (R1, rev-f's F1).
prop() {
    awk -v sec="$1" -v key="$2" '
        /^[ \t]*\[/ { cur = $0; gsub(/^[ \t]*\[|\][ \t]*$/, "", cur); next }
        cur == sec && match($0, "^[ \t]*" key "[ \t]*=") {
            val = substr($0, RLENGTH + 1); gsub(/^[ \t]+|[ \t]+$/, "", val); found = 1
        }
        END { if (found) print val }' "$unit_file"
}
# The unit's settings, as positional parameters (a value may hold spaces:
# RestartPreventExitStatus=3 4). RestartSec is not read: it is scaled to 2 s.
set --
for pair in Unit:StartLimitIntervalSec Unit:StartLimitBurst Service:Restart Service:RestartPreventExitStatus; do
    value=$(prop "${pair%%:*}" "${pair#*:}")
    [ -z "$value" ] || set -- "$@" -p "${pair#*:}=$value"
done
echo "238: the unit's settings: $*" >&2
# The outage: the unit's own StartLimitBurst plus one failures (systemd's
# default burst, 5, when the file sets none), so no start limit survives it.
burst=$(prop Unit StartLimitBurst)
burst=${burst:-5}
case $burst in '' | *[!0-9]*) harness "StartLimitBurst [$burst] is no whole number" ;; esac
[ "$burst" -le 100 ] || harness "StartLimitBurst $burst is over 100: the outage would not fit a bounded run"
fails=$((burst + 1))
wait_s=$((30 + 3 * fails))

# The stand-in dump: run n appends "n <ms>" to <dir>/starts; run 1 succeeds,
# runs 2 to <fails>+1 fail (exit 1, as curl's timeout did; <dir>/fails holds
# the count), and a later run marks <dir>/ran-after and succeeds. With a
# code: every run exits with it.
cat >"$WORK/dump.sh" <<'SH'
dir=$1
n=$(($(wc -l <"$dir/starts" 2>/dev/null || echo 0) + 1))
echo "$n $(($(date +%s%N) / 1000000))" >>"$dir/starts"
[ -z "${2-}" ] || exit "$2"
[ "$n" -gt 1 ] || exit 0
[ "$n" -gt $(($(cat "$dir/fails") + 1)) ] || exit 1
: >"$dir/ran-after"
SH
mkdir -p "$WORK/out" "$WORK/x3" "$WORK/x4"
echo "$fails" >"$WORK/out/fails"

# --- (1) and (2): an outage, then the timer --------------------------------------------------
systemd-run --user --quiet --unit "$stem" --on-calendar='*:*:0/10' \
    --timer-property=AccuracySec=100ms -p Type=oneshot "$@" -p RestartSec=2s \
    -- sh "$WORK/dump.sh" "$WORK/out" >"$WORK/run.err" 2>&1 ||
    harness "systemd-run of the timer and service failed: $(head -c 300 "$WORK/run.err")"
i=0
while [ "$i" -lt "$wait_s" ] && [ ! -e "$WORK/out/ran-after" ]; do
    i=$((i + 1))
    sleep 1
done
runs=$(wc -l <"$WORK/out/starts" 2>/dev/null || echo 0)
[ "$runs" -ge 3 ] || harness "the timer started the stand-in $runs time(s) in $wait_s s (want the success and two failures at least)"
[ -e "$WORK/out/ran-after" ] ||
    check_failed "(1) after a run and $fails failures (StartLimitBurst $burst plus one), no later start ran the dump in $wait_s s: runs [$(tr '\n' ' ' <"$WORK/out/starts")], the service's Result [$(systemctl --user show -p Result --value "$stem.service" 2>/dev/null)]"
t2=$(sed -n 's/^2 //p' "$WORK/out/starts")
t3=$(sed -n 's/^3 //p' "$WORK/out/starts")
gap=$((t3 - t2))
[ "$gap" -ge 1500 ] && [ "$gap" -le 5000 ] ||
    check_failed "(2) the third run came ${gap} ms after the second (want 2-5 s: a retry, not the next tick)"
systemctl --user stop "$stem.timer" "$stem.service" >/dev/null 2>&1 || true

# --- (3) exit 3 and 4 are never retried -------------------------------------------------------
for code in 3 4; do
    systemd-run --user --quiet --unit "$stem-x$code" -p Type=oneshot "$@" -p RestartSec=2s \
        -- sh "$WORK/dump.sh" "$WORK/x$code" "$code" >"$WORK/run$code.err" 2>&1 || true
done
sleep 7
for code in 3 4; do
    got=$(wc -l <"$WORK/x$code/starts" 2>/dev/null || echo 0)
    [ "$got" -eq 1 ] || check_failed "(3) a run that exits $code ran $got time(s) in 7 s (want 1: never retried)"
done

if [ "$failures" -gt 0 ]; then
    echo "238: $failures check(s) failed" >&2
    exit 1
fi
echo "238: one outage never skips later hourly dumps: a start after $fails failures runs, the retry stays, exit 3 and 4 are not retried"
