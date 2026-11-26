#!/bin/sh
# Check (objective 240) [observability]: the tree's deploy/on-landing.sh
# (#54, objective 235) passes its own test, deploy/test-on-landing.sh, run
# from the tree as the gate holds it, on the gate's test server. Without it
# the landed test is a checker no gate calls: a change to on-landing.sh that
# breaks it lands unseen (rev-k's 235 n2).
#
# The tree under test is the working directory. The test takes its
# repository from `git rev-parse --show-toplevel` and the gate's tree is an
# export, not a clone, so this copies the tree's deploy/ into a scratch git
# repository of its own and runs the test there, with HOME and TMPDIR in
# this run's scratch (the test refuses the real home as well), SPAWN_BIN the
# binary under test, and SPAWN_DEPLOY_TEST_URL_FILE a file (mode 600, in the
# scratch) naming the server of SPAWN_TEST_DATABASE_URL. The test makes a
# database of its own on that server, drops it at the end, and never runs a
# live unit: its fake live directory and bare missions repo are its scratch,
# and its deploy.sh is a stand-in.
#
# It must exit 0, with its last line "on-landing: all cases pass", no line
# starting "FAIL", and at least 87 lines starting "ok " (235's count), so a
# test emptied of its checks does not pass. A tree without
# deploy/test-on-landing.sh has nothing to check (exit 0, said): deploy/ is
# protected, so no candidate removes it; a tree that has the test but not
# deploy/on-landing.sh or deploy/backup/lib.sh fails.
#
# Exit status: 0 the test passed (or there is none); 1 it failed; 2 a
# harness or infrastructure failure (no psql, no server, the test's home
# guard refused). Any unexpected exit is reported as 2.

set -eu
# Only when the tree differs from the base (SPAWN_BASELINE_TREE) in what this
# check runs or reads: deploy/on-landing.sh, its test and deploy/backup/lib.sh,
# and the spawn commands the test drives (spawn mission landing, hold, release,
# deployed, push: spawn-cli's main.rs, missions.rs and integrate, and
# spawn-postgres's sources and schema). Otherwise it passes at once, as
# check-test-gate.sh does, so a tree that leaves them alone pays nothing (it
# never caught a regression in the gate ROI audit of 2026-11-24). Its own file
# is on the list, so a change to the check runs it.
base=${SPAWN_BASELINE_TREE-}
if [ -n "$base" ] && [ -d "$base" ]; then
    same=1
    for path in deploy/on-landing.sh deploy/test-on-landing.sh deploy/backup/lib.sh \
        crates/spawn-cli/src/main.rs crates/spawn-cli/src/missions.rs crates/spawn-cli/src/integrate.rs \
        crates/spawn-cli/src/integrate crates/spawn-postgres/src crates/spawn-postgres/sql \
        rsi/scenarios/check-on-landing.sh; do
        if [ -e "$path" ] || [ -e "$base/$path" ]; then
            diff -rq -- "$base/$path" "$path" >/dev/null 2>&1 || {
                same=""
                echo "changed against the base: $path"
            }
        fi
    done
    if [ -n "$same" ]; then
        echo "check-on-landing: on-landing, its test and the commands it drives are the base's: not run"
        exit 0
    fi
fi

asserted=""
harness() {
    echo "check-on-landing: harness: $*" >&2
    exit 2
}
check_failed() {
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}

for tool in bash git psql jq flock mktemp getent; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is not set"
tree=$(pwd)
[ -d "$tree/deploy" ] || harness "run from the tree under test: no deploy/ under $tree"
controller=$(cd "$(dirname "$0")/../.." && pwd) || harness "no controller above $0"
[ -f "$controller/scripts/pg-url.sh" ] || harness "no scripts/pg-url.sh in the controller $controller"
# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL") || harness "SPAWN_TEST_DATABASE_URL does not parse"

WORK=$(mktemp -d) || harness "mktemp failed"
[ -n "$WORK" ] && [ -d "$WORK" ] || harness "mktemp gave no directory"
finish() {
    rc=$?
    trap - EXIT
    set +e
    rm -rf "$WORK"
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "check-on-landing: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
[ -n "$real_home" ] || harness "no home for uid $(id -u)"
case $WORK/ in "$real_home"/*) harness "the scratch $WORK is in the real home $real_home" ;; esac
mkdir -m 700 "$WORK/home" "$WORK/tmp" "$WORK/repo" || harness "cannot make the scratch HOME, TMPDIR and repo"

if [ ! -f "$tree/deploy/test-on-landing.sh" ]; then
    echo "check-on-landing: the tree has no deploy/test-on-landing.sh: nothing to check"
    exit 0
fi
for f in deploy/on-landing.sh deploy/backup/lib.sh; do
    [ -f "$tree/$f" ] || check_failed "the tree has no $f (deploy/test-on-landing.sh needs it)"
done

cp -R "$tree/deploy" "$WORK/repo/deploy" || harness "cannot copy the tree's deploy/"
(
    cd "$WORK/repo" &&
        HOME=$WORK/home git init -q -b main &&
        HOME=$WORK/home git add -A &&
        HOME=$WORK/home git -c user.name=240 -c user.email=240@spawn.invalid commit -q -m "the tree's deploy/"
) >/dev/null 2>&1 || harness "cannot make the scratch repository"
(umask 077 && printf '%s/postgres\n' "$server_url" >"$WORK/test-url") || harness "cannot write the test URL file"

status=0
(
    cd "$WORK/repo" &&
        HOME=$WORK/home TMPDIR=$WORK/tmp SPAWN_BIN=$SPAWN_BIN \
            SPAWN_DEPLOY_TEST_URL_FILE=$WORK/test-url bash deploy/test-on-landing.sh
) >"$WORK/test.out" 2>&1 </dev/null || status=$?
last=$(tail -n 1 "$WORK/test.out")
failing=$(grep -m 5 '^FAIL' "$WORK/test.out" | cut -c1-200 | tr '\n' '|' || true)
[ "$status" -ne 2 ] || harness "deploy/test-on-landing.sh exited 2 (its home guard refused, or a tool failed: no server?); its last line: $last"
[ "$status" -eq 0 ] || check_failed "deploy/test-on-landing.sh exited $status; its last line: $last; $failing"
[ "$last" = "on-landing: all cases pass" ] ||
    check_failed "deploy/test-on-landing.sh's last line is not 'on-landing: all cases pass': $last"
[ -z "$failing" ] || check_failed "deploy/test-on-landing.sh printed failures: $failing"
passed=$(grep -c '^ok ' "$WORK/test.out" || true)
[ "$passed" -ge 87 ] || check_failed "deploy/test-on-landing.sh ran $passed checks, fewer than 235's 87"
echo "check-on-landing: the tree's landing tick passes its test: $passed checks, $last"
exit 0
