#!/bin/sh
# Black box: 145.sh judges only a quiet round of its sweep (objective 236).
# An unreadable process of this user can start and end at any time: gpg-agent
# makes itself non-dumpable, and every backup scenario starts one for its run
# (sync/flaky/145.md). 145.sh (3) used to judge its third round whatever it
# saw, so a busy host failed it (rc 1) with nothing wrong. Now it retries a
# round that was not quiet, 2 s apart, for SPAWN_145_QUIET_SECS (120) in
# all, and with no quiet round by then exits 2 ("run again"), never 1.
#
# This runs the tree's own 145.sh with SPAWN_BIN a wrapper around the
# binary under test. The wrapper changes no argument and no output. From the
# call after 145.sh's (4) refused excuse ("--excuse <pid>:1") on, before
# each of the next N `scratch sweep` calls, it kills the gpg-agent it started
# last and starts a fresh one (gpg-connect-agent in a scratch GNUPGHOME of
# its own). That is real churn of unreadable processes of this user.
#
# Required:
#   (0) the agents the wrapper starts are unreadable here (their /proc cwd
#       cannot be read), so the churn is real; else a harness exit 2.
#   (1) churn over the next 6 sweep calls, then none: 145.sh exits 0, three
#       runs in a row. A run where 145.sh itself says "run again" (another
#       session's unreadable process during its --apply) makes this run a
#       harness exit 2 too, never 1.
#   (2) churn on every sweep call, SPAWN_145_QUIET_SECS=10: 145.sh exits 2,
#       never 1, says "no quiet round" and "run again", and takes under 90 s
#       (the bound is SPAWN_145_QUIET_SECS, not the 120 s default).
#   (3) (R2, rev-b's F2) an agent killed and started only before each of the
#       first 3 --apply calls: the list changed (round 1), then an excused
#       process ended (rounds 2, 3); 145.sh exits 0.
#   (4) (R2, rev-b's F2) an agent added, none killed, before each of the
#       first 3 reviewed lists: "an unreadable process started since the
#       list" (round 1); 145.sh exits 0.
# On main 2e145e90, (1) exits 1: (3)'s reviewed list fails on an excused
# agent that ended, and its third round is judged.
#
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure.

set -eu

asserted=""
harness() {
    echo "236: harness: $*" >&2
    exit 2
}
[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
tree=$(pwd)
# 145.sh: the gate's (rsi/scenarios/145.sh) or, since it left the gate, the nightly's
# (rsi/scenarios/nightly/145.sh). 145 takes its controller two levels above itself, the
# tree as it ran here; a nightly copy runs from $WORK/c145/rsi/scenarios/ beside a link to
# the tree's scripts/ (below, once $WORK exists), so that stays the tree.
if [ -f "$tree/rsi/scenarios/145.sh" ]; then
    s145=rsi/scenarios/145.sh
elif [ -f "$tree/rsi/scenarios/nightly/145.sh" ]; then
    s145=rsi/scenarios/nightly/145.sh
else
    harness "run from the tree under test: no rsi/scenarios/145.sh or rsi/scenarios/nightly/145.sh under $tree"
fi
for tool in gpg-connect-agent gpgconf readlink; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
if [ "$s145" != rsi/scenarios/145.sh ]; then
    mkdir -p "$WORK/c145/rsi/scenarios" && cp "$tree/$s145" "$WORK/c145/rsi/scenarios/145.sh" &&
        ln -s "$tree/scripts" "$WORK/c145/scripts" || harness "cannot stage $s145 under $WORK/c145"
    s145=$WORK/c145/rsi/scenarios/145.sh
fi
REAL=$SPAWN_BIN
cleanup() {
    # Every agent the wrapper started, by its own home; then the homes.
    for h in "$WORK"/g/*; do
        [ -d "$h" ] || continue
        gpgconf --homedir "$h" --kill gpg-agent >/dev/null 2>&1 || true
        gpgconf --homedir "$h" --remove-socketdir >/dev/null 2>&1 || true
    done
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    set +e
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "236: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM
mkdir -p "$WORK/bin" "$WORK/g"
: >"$WORK/failures"
check_failed() {
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
note() { echo "236: $*" >&2; }

# The wrapper. State: $WORK/armed (the (4) refused excuse was seen), $WORK/left
# (churned calls left; "all" for every call), $WORK/last (the last agent's home).
cat >"$WORK/bin/spawn" <<'WRAP'
#!/bin/sh
W=${BB236_WORK:?}
start() { # a fresh agent in a home of its own
    h=$(mktemp -d "$W/g/h.XXXXXX") && chmod 700 "$h"
    gpg-connect-agent --homedir "$h" /bye >/dev/null 2>&1 || true
    printf '%s\n' "$h" >"$W/last"
    echo started >>"$W/started"
}
fresh() { # the last agent killed, a fresh one started
    last=$(cat "$W/last" 2>/dev/null || true)
    if [ -n "$last" ]; then
        gpgconf --homedir "$last" --kill gpg-agent >/dev/null 2>&1 || true
    fi
    start
}
if [ "${1-}" = scratch ] && [ "${2-}" = sweep ]; then
    if [ -f "$W/armed" ]; then
        apply="" excuse=""
        for a in "$@"; do
            case "$a" in --apply) apply=1 ;; --excuse) excuse=1 ;; esac
        done
        # Which calls this mode touches: every sweep call (churn), only
        # --apply calls (apply: kill and start), or only the reviewed list,
        # an excusing call without --apply (add: start, no kill).
        hit=""
        case "${BB236_MODE:-churn}" in
        churn) hit=1 ;;
        apply) [ -z "$apply" ] || hit=1 ;;
        add) [ -n "$apply" ] || [ -z "$excuse" ] || hit=1 ;;
        esac
        left=$(cat "$W/left")
        if [ -n "$hit" ] && { [ "$left" = all ] || [ "$left" -gt 0 ]; }; then
            if [ "${BB236_MODE:-churn}" = add ]; then start; else fresh; fi
            [ "$left" = all ] || echo $((left - 1)) >"$W/left"
        fi
    fi
    for a in "$@"; do
        case "$a" in *:1) touch "$W/armed" ;; esac
    done
fi
exec "$BB236_REAL" "$@"
WRAP
chmod +x "$WORK/bin/spawn"

# --- (0) the agents are unreadable here ------------------------------------------------------
agents() { # this user's gpg-agent pids, sorted
    for p in /proc/[0-9]*; do
        [ "$(cat "$p/comm" 2>/dev/null)" = gpg-agent ] || continue
        [ "$(awk '/^Uid/ { print $2 }' "$p/status" 2>/dev/null)" = "$(id -u)" ] || continue
        echo "${p#/proc/}"
    done | sort
}
agents >"$WORK/agents.before"
h0=$(mktemp -d "$WORK/g/h.XXXXXX") && chmod 700 "$h0"
gpg-connect-agent --homedir "$h0" /bye >/dev/null 2>&1 || harness "gpg-connect-agent could not start an agent"
agents >"$WORK/agents.after"
new=$(comm -13 "$WORK/agents.before" "$WORK/agents.after")
[ -n "$new" ] || harness "gpg-connect-agent started no gpg-agent of this user"
for pid in $new; do
    ! readlink "/proc/$pid/cwd" >/dev/null 2>&1 ||
        harness "the gpg-agent just started ($pid) is readable here: the churn would not be what 145.sh meets"
done
gpgconf --homedir "$h0" --kill gpg-agent >/dev/null 2>&1 || true
note "(0) the gpg-agent started here is unreadable: $(echo $new)"

run145() { # <label> <quiet secs> <left> [<mode>]: 145.sh with the wrapper; its rc in $rc, its stderr in $WORK/<label>.err, its seconds in $took
    rm -f "$WORK/armed" "$WORK/last" "$WORK/started"
    echo "$3" >"$WORK/left"
    t0=$(date +%s)
    (cd "$tree" && BB236_WORK=$WORK BB236_REAL=$REAL BB236_MODE=${4:-churn} SPAWN_BIN=$WORK/bin/spawn SPAWN_145_QUIET_SECS=$2 \
        sh "$s145" >"$WORK/$1.out" 2>"$WORK/$1.err") && rc=0 || rc=$?
    took=$(($(date +%s) - t0))
    started=$(wc -l <"$WORK/started" 2>/dev/null | tr -d ' ')
    note "$1: 145.sh rc=$rc in ${took} s, agents started ${started:-0}, rounds: $(grep -o '(3) round [0-9]*: [^;]*' "$WORK/$1.err" | tr '\n' '|')"
}

pass_through() { # <label>: 145.sh's own "run again" (exit 2) is this run's too, never a failure
    if [ "$rc" -eq 2 ] && grep -q 'run again' "$WORK/$1.err"; then
        harness "$1: 145.sh could not judge on this host ($(grep -m 1 'harness' "$WORK/$1.err" | head -c 300)): run again"
    fi
}

# --- (1) churn over 6 calls, then quiet: rc 0, three runs ---------------------------------------
for i in 1 2 3; do
    run145 "churn$i" 120 6
    pass_through "churn$i"
    [ "$rc" -ne 0 ] || [ "${started:-0}" -ge 6 ] || harness "(1) run $i: 145.sh passed, but the wrapper started ${started:-0} agents, not 6 (145.sh's (4) refused excuse not seen?)"
    [ "$rc" -eq 0 ] || check_failed "(1) run $i: 145.sh beside 6 calls of gpg-agent churn exited $rc: $(grep -m 4 -E '^blackbox|harness' "$WORK/churn$i.err" | tr '\n' ' ' | head -c 600)"
done
grep -q 'round 1: ' "$WORK/churn1.err" || check_failed "(1) run 1 judged its first round: the churn did not reach (3) ($(grep -m 3 '(3)' "$WORK/churn1.err" | tr '\n' ' '))"

# --- (2) never quiet: rc 2, run again ------------------------------------------------------------
run145 never 10 all
[ "$rc" -eq 2 ] || check_failed "(2) 145.sh with churn on every call exited $rc, not 2: $(grep -m 4 -E '^blackbox|harness' "$WORK/never.err" | tr '\n' ' ' | head -c 600)"
grep -q 'no quiet round' "$WORK/never.err" && grep -q 'run again' "$WORK/never.err" ||
    check_failed "(2) no \"no quiet round ... run again\" in: $(grep -m 2 harness "$WORK/never.err" | tr '\n' ' ' | head -c 400)"
# 145.sh's other steps take a few seconds; the 120 s default would show.
[ "$took" -lt 90 ] || check_failed "(2) with SPAWN_145_QUIET_SECS=10, 145.sh took ${took} s: the bound was not its"

# --- (3) churn only before --apply, three times: the list changed, then excuses ended ------------
run145 apply 120 3 apply
pass_through apply
[ "$rc" -eq 0 ] || check_failed "(3) 145.sh with an agent killed and started before each of 3 --apply calls exited $rc: $(grep -m 4 -E '^blackbox|harness' "$WORK/apply.err" | tr '\n' ' ' | head -c 600)"
grep -q 'round 1: the list changed before --apply' "$WORK/apply.err" && grep -q 'an excused process ended before --apply' "$WORK/apply.err" ||
    check_failed "(3) the rounds were not the list changed, then an excused process ended: $(grep -o '(3) round [0-9]*: [^;]*' "$WORK/apply.err" | tr '\n' '|')"

# --- (4) an agent added (none killed) before the reviewed list, three times ----------------------
run145 add 120 3 add
pass_through add
[ "$rc" -eq 0 ] || check_failed "(4) 145.sh with an agent added before each of 3 reviewed lists exited $rc: $(grep -m 4 -E '^blackbox|harness' "$WORK/add.err" | tr '\n' ' ' | head -c 600)"
grep -q 'round 1: an unreadable process started since the list' "$WORK/add.err" ||
    check_failed "(4) round 1 was not \"an unreadable process started since the list\": $(grep -o '(3) round [0-9]*: [^;]*' "$WORK/add.err" | tr '\n' '|')"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "236: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "236: 145.sh judges only a quiet round: real gpg-agent churn passes, endless churn is no verdict (exit 2)"
exit 0
