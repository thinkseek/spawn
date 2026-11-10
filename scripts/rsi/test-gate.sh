#!/bin/sh
# Tests of the gate (scripts/rsi/gate.sh) on throwaway repositories: no
# database; cargo only for the test-count cases (a crate with no
# dependencies, offline). Prints one line per case and exits non-zero on
# the first wrong verdict. Needs bwrap, as the gate does.
#
# Run it with a private TMPDIR (e.g. TMPDIR=/data/tmp/<you>-suite), never the
# shared one: the suite's own scratch and sockets then stay out of /tmp, and
# case 20's killed gate — which no trap cleans up — leaves its scratch in
# that TMPDIR for the case to remove. (The cleanup removes only the killed
# gate's own scratch, found by its session, but a private TMPDIR keeps any
# lapse local.) The TMPDIR must not contain the checkout the gate is run
# from (so not /data/tmp with the tree at /data/tmp/ccd/...): the gate
# refuses a tree under the host's TMPDIR, because its sandbox empties that
# directory whole, and every case would stop at the refusal.
#
# usage: scripts/rsi/test-gate.sh
set -eu

# rustup finds its toolchains under the real $HOME, which is replaced below.
if [ -z "${RUSTUP_HOME-}" ] && [ -d "$HOME/.rustup" ]; then
    export RUSTUP_HOME="$HOME/.rustup"
fi

gate=$(cd "$(dirname "$0")" && pwd)/gate.sh
work=$(mktemp -d)
# The cases do not depend on this host's free disk; one case below raises
# the gate's floor to test the refusal.
export SPAWN_GATE_MIN_FREE_GB=0
# A file in the host's /tmp, which the sandbox must not show.
marker=$(mktemp /tmp/test-gate-marker.XXXXXX)
# Background helpers (listeners, fake servers) are killed on exit. One case
# copies the gate under /var/tmp (a root the sandbox empties); its scratch is
# removed on exit too, whatever happens in between.
background=""
masked=$work/no-masked-root
leftover=""
# Release files the suite's holders wait on (a lock held until the file
# exists): the exit creates them first, so a FAIL between a hold and its
# release ends the holder at once rather than at its bound.
releases=""
# The exit's cleanup. It never changes the suite's status: a helper killed
# just now (a server, a listener) may still be writing its last files while
# the scratch goes, and `rm -rf` then fails with "Directory not empty" (a
# suite whose every case passed exited 1 so inside a gate, check-test-gate,
# 2026-11-09). It retries, and says what it could not remove.
cleanup() {
    status=$?
    if [ -n "$releases" ]; then
        for release in $releases; do : >"$release" 2>/dev/null || true; done
        # Long enough for every poll of them (0.32 s at most) to see them
        # before the scratch, and the files with it, goes.
        sleep 1
    fi
    # shellcheck disable=SC2086 # pids
    kill $background 2>/dev/null || true
    # shellcheck disable=SC2086 # paths without spaces
    [ -z "$leftover" ] || rm -f $leftover
    for _ in 1 2 3 4 5; do
        rm -rf "$work" "$marker" "$masked" 2>/dev/null && break
        sleep 1
    done
    [ ! -e "$work" ] || echo "test-gate: could not remove $work" >&2
    exit "$status"
}
trap cleanup EXIT

# The owner's home, with secrets the candidate must not read. The gate's base
# cache goes under it too, never into the real one.
export HOME="$work/home"
export XDG_CACHE_HOME="$HOME/.cache"
cache=$XDG_CACHE_HOME/spawn/gate-base
mkdir -p "$HOME/.config/spawn" "$HOME/.ssh" "$HOME/.local/share/spawn-rsi"
echo owner-secret >"$HOME/.config/spawn/secret"
echo owner-key >"$HOME/.ssh/id_test"
echo owner-session >"$HOME/.claude.json"
# Files no deny-list named: the allow-list hides them too.
printf '[credential]\n\thelper = owner-helper\n' >"$HOME/.gitconfig"
echo owner-oauth >"$HOME/.claude.json.backup"
echo 'export TOKEN=owner' >"$HOME/.bash_history"
mkdir -p "$HOME/.hermes"
echo owner-hermes >"$HOME/.hermes/key"
echo SPAWN_DATABASE_URL=owner >"$HOME/.local/share/spawn-rsi/spawn.env"

repo=$work/repo.git
seed=$work/seed
git init -q --bare "$repo"
git init -q --initial-branch=main "$seed"
commit() {
    git -C "$seed" add -A
    git -C "$seed" -c user.name=t -c user.email=t@localhost commit -qm "$1"
    git -C "$seed" push -q -f "$repo" "HEAD:refs/heads/$1"
    git -C "$seed" rev-parse HEAD
}
printf 'one\n' >"$seed/value"
mkdir -p "$seed/judge"
printf 'keep\n' >"$seed/judge/rule"
# A crate with tests and lint configuration, for the ratchets.
mkdir -p "$seed/crates/lib/src"
printf '[package]\nname = "lib"\n\n[lints.clippy]\nall = "deny"\n' >"$seed/crates/lib/Cargo.toml"
cat >"$seed/crates/lib/src/lib.rs" <<'EOF'
#![deny(missing_docs)]
//! A library.

#[cfg(test)]
mod tests {
    #[test]
    fn one() {}

    #[tokio::test]
    async fn two() {}
}
EOF
lib=$seed/crates/lib/src/lib.rs
base=$(commit base)

# The controller's scenarios: a build hook that writes a "binary", a task
# scenario that wants value=two, a regression scenario that wants the binary,
# a check that runs the candidate's own `probe.sh` when it has one, a ratchet
# task scenario that prints a metric and then runs the candidate's
# `noise.sh`, a ratchet scenario that always fails, and a protected path.
scenarios=$work/scenarios
mkdir -p "$scenarios"
cat >"$scenarios/build" <<'EOF'
#!/bin/sh
cp value built
EOF
cat >"$scenarios/007.sh" <<'EOF'
#!/bin/sh
[ "$(cat value)" = two ]
EOF
cat >"$scenarios/check-built.sh" <<'EOF'
#!/bin/sh
[ -f built ] && [ "$(cat built)" = "$(cat value)" ]
EOF
cat >"$scenarios/check-probe.sh" <<'EOF'
#!/bin/sh
[ ! -f probe.sh ] || sh probe.sh
EOF
cat >"$scenarios/008.sh" <<'EOF'
#!/bin/sh
# rsi: ratchet
echo "METRIC speed baseline=10 candidate=5"
[ ! -f noise.sh ] || sh noise.sh
[ "$(cat value)" = two ]
EOF
cat >"$scenarios/009.sh" <<'EOF'
#!/bin/sh
# rsi: ratchet
exit 1
EOF
printf 'judge/\n# a comment\n\n' >"$scenarios/protected"

branch() {
    git -C "$seed" checkout -q -B "$1" "$base"
}
# judge <candidate> [<scenario>] [<scenarios-dir>]: run the gate as the
# integrator does, with SPAWN_GATE_VERDICT in a directory of its own, and
# print `exit=<status> <verdict file>`, without the commits the verdict names
# (checked once, below). The gate's output is in gate.out.
integrator=$work/integrator
judge() {
    rm -rf "$integrator"
    mkdir -m 700 "$integrator"
    : >"$integrator/stdout"
    status=0
    SPAWN_GATE_VERDICT=$integrator/verdict sh "$gate" "$repo" "$base" "$1" "${3:-$scenarios}" ${2:+"$2"} \
        >"$work/gate.out" 2>&1 || status=$?
    echo "exit=$status $(without_commits 2>/dev/null <"$integrator/verdict" || true)"
}
without_commits() {
    sed -E 's/,"base":"[0-9a-f]+","candidate":"[0-9a-f]+"\}$/}/'
}
expect() {
    case "$2" in
    *"$3"*) echo "ok   $1" ;;
    *)
        echo "FAIL $1: expected $3 in $2"
        sed 's/^/     /' "$work/gate.out" | tail -n 20
        exit 1
        ;;
    esac
}
refuse() {
    case "$2" in
    *"$3"*)
        echo "FAIL $1: did not expect $3 in $2"
        exit 1
        ;;
    *) echo "ok   $1" ;;
    esac
}

branch good
printf 'two\n' >"$seed/value"
good=$(commit good)
expect "red then green lands" "$(judge "$good" "$scenarios/007.sh")" 'exit=0 {"verdict":"accepted"}'
expect "the verdict file and stdout agree" "$(tail -n 1 "$work/gate.out" | without_commits)" '{"verdict":"accepted"}'
expect "the verdict names the commits it judged, resolved" "$(cat "$integrator/verdict")" \
    "\"base\":\"$base\",\"candidate\":\"$good\"}"
expect "a ref is resolved to its commit" \
    "$(SPAWN_GATE_VERDICT=$work/by-ref sh "$gate" "$repo" "$base" refs/heads/good "$scenarios" >/dev/null 2>&1; cat "$work/by-ref")" \
    "\"candidate\":\"$good\"}"
expect "a candidate that names no commit is no verdict" \
    "exit=$(sh "$gate" "$repo" "$base" no-such-ref "$scenarios" >/dev/null 2>&1 || echo $?)" 'exit=3'
expect "no task scenario: regression only" "$(judge "$good")" 'exit=0 {"verdict":"accepted"}'

# No room in TMPDIR: exit 5 and a no-room fact in the verdict file (no
# verdict), before any scratch is made. The integrator records it as
# `no_room`, which does not count toward a halt.
roomless=$work/roomless-tmp
mkdir "$roomless"
expect "a TMPDIR without room stops the gate with its own status" \
    "$(export TMPDIR="$roomless" SPAWN_GATE_MIN_FREE_GB=1000000000; judge "$good")" \
    'exit=5 {"no_room":true,"free_kb":'
expect "the reason names the floor and TMPDIR" "$(cat "$integrator/verdict")" \
    "\"min_gb\":1000000000,\"tmpdir\":\"$roomless\"}"
expect "and the free space, in KiB" \
    "$(grep -Ec '^\{"no_room":true,"free_kb":[0-9]+,"min_gb"' "$integrator/verdict")" 1
refuse "with no verdict" "$(cat "$integrator/verdict")" '"verdict"'
expect "a verdict file that cannot be written still exits 5" \
    "exit=$(export TMPDIR="$roomless" SPAWN_GATE_MIN_FREE_GB=1000000000
            SPAWN_GATE_VERDICT=$work/no-such-dir/verdict sh "$gate" "$repo" "$base" "$good" "$scenarios" \
                >/dev/null 2>&1 || echo $?)" 'exit=5'
expect "and says why" "$(cat "$work/gate.out")" 'under SPAWN_GATE_MIN_FREE_GB=1000000000: not starting'
refuse "and leaves no scratch" "$(ls -A "$roomless")" 'spawn-gate'
expect "a floor that is not whole GiB is an abort (exit 3), not short room" \
    "$(export SPAWN_GATE_MIN_FREE_GB=lots; judge "$good")" 'exit=3 '
refuse "and writes no no-room fact" "$(cat "$integrator/verdict" 2>/dev/null || true)" 'no_room'
expect "a TMPDIR whose free space cannot be read is an abort too" \
    "$(export TMPDIR="$work/no-such-tmp" SPAWN_GATE_MIN_FREE_GB=1; judge "$good")" 'exit=3 '
expect "the gate's scratch is named for cleanup" \
    "$(export TMPDIR="$roomless"; . "$(dirname "$gate")/../disk.sh"; basename "$(gate_scratch)")" 'spawn-gate.'
rm -rf "$roomless"

expect "a task scenario already green on the base" "$(judge "$good" "$scenarios/check-built.sh")" 'already passes'

branch protected
printf 'two\n' >"$seed/value"
printf 'weaker\n' >"$seed/judge/rule"
touched=$(commit protected)
expect "protected path refused" "$(judge "$touched" "$scenarios/007.sh")" 'protected'

# The controller's own protected list covers what picks or configures the
# toolchain that builds the candidate as the owner at deploy.
listed=$work/listed-scenarios
mkdir -p "$listed"
cp "$scenarios/007.sh" "$listed/"
cp "$(dirname "$gate")/../../rsi/scenarios/protected" "$listed/protected"
for path in rust-toolchain rust-toolchain.toml .cargo/config.toml; do
    branch "toolchain-$(echo "$path" | tr -c 'a-z\n' -)"
    printf 'two\n' >"$seed/value"
    mkdir -p "$seed/$(dirname "$path")"
    printf '[toolchain]\npath = "/opt/elsewhere"\n' >"$seed/$path"
    toolchain=$(commit "toolchain-$(echo "$path" | tr -c 'a-z\n' -)")
    expect "the protected list covers $path" \
        "$(judge "$toolchain" "$listed/007.sh" "$listed")" 'candidate touches protected paths'
done

branch buildtime
printf 'two\n' >"$seed/value"
printf 'fn main() {}\n' >"$seed/build.rs"
buildtime=$(commit buildtime)
expect "build-time code refused" "$(judge "$buildtime")" 'build-time'

branch regress
printf 'two\n' >"$seed/value"
mkdir -p "$seed/x"
commit regress >/dev/null
# A regression: the build hook copies `value`, so break the candidate by
# making the check fail through a tree that has no value at all.
git -C "$seed" rm -q value
printf 'two\n' >"$seed/other"
regress=$(commit regress-2)
expect "a failing check rejects" "$(judge "$regress")" '"verdict":"rejected"'

# 5a. Checks run twice (task 127: one e2e run that failed under load rejected
# an 85-minute gate). A check that fails twice on the candidate and passes on
# the base is a regression; one that fails once and then passes is a flake;
# one that fails on the candidate and on the base is no verdict (exit 4).
branch broken-probe
printf 'two\n' >"$seed/value"
printf 'echo "probe: broken"\nexit 1\n' >"$seed/probe.sh"
broken_probe=$(commit broken-probe)
expect "a check that fails twice and passes on the base is a regression" "$(judge "$broken_probe")" \
    'exit=0 {"verdict":"rejected","reason":"regression: check-probe.sh fails on the candidate"}'
expect "it ran again before the base" "$(cat "$work/gate.out")" \
    '| == regression: check-probe.sh must still pass (again: a failing check runs twice)'
expect "and the log says why" "$(cat "$work/gate.out")" \
    'check-probe.sh passes on the base and failed twice on the candidate: a regression'
branch flaky
printf 'two\n' >"$seed/value"
cat >"$seed/probe.sh" <<'EOF'
# Fails on its first run only: the tree is the candidate's, writable.
[ -f .probe-ran ] && exit 0
: >.probe-ran
echo "probe: a load flake"
exit 1
EOF
flaky=$(commit flaky)
expect "a check that fails once and then passes is a flake, not a regression, named in the verdict" \
    "$(judge "$flaky")" 'exit=0 {"verdict":"accepted","flakes":["check-probe.sh"]}'
expect "the log says it" "$(cat "$work/gate.out")" \
    'check-probe.sh failed once (1) and passed again on the candidate: a flake, not a regression'
broken_scenarios=$work/broken
cp -r "$scenarios" "$broken_scenarios"
printf '#!/bin/sh\necho "broken: fails on every tree"\nexit 1\n' >"$broken_scenarios/check-broken.sh"
printf '#!/bin/sh\nexit 0\n' >"$broken_scenarios/check-tests.sh"
expect "a check that fails on the candidate and on the base is no verdict" \
    "$(judge "$good" "" "$broken_scenarios")" \
    'exit=6 {"host_fault":true,"scenario":"check-broken.sh","on":"candidate and base","runs":"1, then 1 on the candidate; 1, then 1 on the base"'
expect "the log says so" "$(cat "$work/gate.out")" \
    'check-broken.sh fails on the candidate and on the base: a host or harness fault, no verdict'
expect "with every base result cached the base is built for it" \
    "$(judge "$good" "" "$broken_scenarios")" 'exit=6 {"host_fault":true,"scenario":"check-broken.sh"'
expect "the cached run still judged it on the base" "$(cat "$work/gate.out")" \
    'check-broken.sh fails on the base too (1, then 1)'
# 5a'. Main is red on a check (design-main-red.md): a check that fails twice
# on the candidate and twice on the base, both base runs failing the same
# tests (check-tests.sh's `FAILED <test>: <message>` lines), is main_red, a
# fact about main (exit 6, `on` `main`, the shared failures); one whose base
# runs fail once and then pass, or fail different tests, stays the host's
# (`candidate and base`). The red check named by SPAWN_GATE_BASE_CHECK runs
# on the base before the candidate: passing, the verdict says main is green
# on it again (`base_check`); failing alike, main is still red.
red_scenarios=$work/red
cp -r "$scenarios" "$red_scenarios"
printf '#!/bin/sh\nexit 0\n' >"$red_scenarios/check-tests.sh"
cat >"$red_scenarios/check-red.sh" <<'EOF'
#!/bin/sh
echo "FAILED lib::tests::adds: assertion failed: 1 + 1 == 3"
echo "FAILED lib::tests::other: boom"
exit 1
EOF
expect "a check failing twice on the base with the same tests is main red" \
    "$(judge "$good" "" "$red_scenarios")" \
    'exit=6 {"host_fault":true,"scenario":"check-red.sh","on":"main","runs":"1, then 1 on the candidate; 1, then 1 on the base"'
expect "naming the failing tests both base runs share" "$(cat "$integrator/verdict")" \
    '"failures":[{"test":"lib::tests::adds","message":"assertion failed: 1 + 1 == 3"},{"test":"lib::tests::other","message":"boom"}]'
expect "the log says main is red" "$(cat "$work/gate.out")" \
    'check-red.sh fails on the base twice with the same failing tests: main is red on it'
# For an owner's push (SPAWN_GATE_RED_UNSOLVED=1, design-owner-push-between-
# gates.md) the check main is red on is unsolved on the base, not a
# regression and no stop: the push is accepted when the rest passes, and a
# check the base solves still rejects it.
expect "an owner's push: the red check is unsolved on the base, accepted" \
    "$(export SPAWN_GATE_RED_UNSOLVED=1; judge "$good" "" "$red_scenarios")" 'exit=0 {"verdict":"accepted"'
expect "and says why" "$(cat "$work/gate.out")" \
    'check-red.sh fails on the base twice with the same failing tests: main is red on it, so it is unsolved on the base (SPAWN_GATE_RED_UNSOLVED): not a regression'
red_and_broken=$work/red-and-broken
cp -r "$red_scenarios" "$red_and_broken"
printf '#!/bin/sh\n# Passes on the base, fails on the candidate: a regression.\n[ "$PWD" = "$SPAWN_BASELINE_TREE" ]\n' >"$red_and_broken/check-zz-candidate.sh"
expect "an owner's push is still rejected for a check the base solves" \
    "$(export SPAWN_GATE_RED_UNSOLVED=1; judge "$good" "" "$red_and_broken")" \
    '"verdict":"rejected","reason":"regression: check-zz-candidate.sh fails on the candidate"'
# Only the tests main fails are unsolved (rev-b F1 on bfbda5cb): a test the
# candidate newly fails inside the red check is its regression, named, and
# a candidate run that fails with no FAILED line names nothing: rejected.
red_new=$work/red-new
cp -r "$red_scenarios" "$red_new"
cat >"$red_new/check-red.sh" <<'EOF'
#!/bin/sh
echo "FAILED lib::tests::adds: assertion failed: 1 + 1 == 3"
echo "FAILED lib::tests::other: boom"
if [ "$PWD" != "$SPAWN_BASELINE_TREE" ]; then echo "FAILED lib::tests::newly_broken: the push broke it"; fi
exit 1
EOF
expect "an owner's push that breaks a test inside the red check is rejected" \
    "$(export SPAWN_GATE_RED_UNSOLVED=1; judge "$good" "" "$red_new")" \
    '"verdict":"rejected","reason":"regression: check-red.sh fails on the candidate"'
expect "naming the test main does not fail" "$(cat "$work/gate.out")" \
    "check-red.sh: the candidate fails what main's red runs do not (lib::tests::newly_broken): a regression"
red_silent=$work/red-silent
cp -r "$red_scenarios" "$red_silent"
cat >"$red_silent/check-red.sh" <<'EOF'
#!/bin/sh
if [ "$PWD" != "$SPAWN_BASELINE_TREE" ]; then echo "it crashed before any test ran"; exit 1; fi
echo "FAILED lib::tests::adds: assertion failed: 1 + 1 == 3"
exit 1
EOF
expect "an owner's push whose run of the red check names no failed test is rejected" \
    "$(export SPAWN_GATE_RED_UNSOLVED=1; judge "$good" "" "$red_silent")" \
    '"verdict":"rejected","reason":"regression: check-red.sh fails on the candidate"'
expect "and says so" "$(cat "$work/gate.out")" \
    "check-red.sh: the candidate fails what main's red runs do not ((no FAILED line)): a regression"
# The control (F2): beside the red check, an objective the base solves and
# the candidate breaks is still a regression under SPAWN_GATE_RED_UNSOLVED.
red_objective=$work/red-objective
cp -r "$red_scenarios" "$red_objective"
printf '#!/bin/sh\n# Solved on the base, broken by the candidate.\n[ "$PWD" = "$SPAWN_BASELINE_TREE" ]\n' >"$red_objective/081.sh"
expect "an owner's push that breaks an objective the base solves is rejected beside the red check" \
    "$(export SPAWN_GATE_RED_UNSOLVED=1; judge "$good" "" "$red_objective")" \
    '"verdict":"rejected","reason":"regression: 081.sh fails on the candidate"'
flaky_red=$work/red-flaky
cp -r "$red_scenarios" "$flaky_red"
cat >"$flaky_red/check-red.sh" <<'EOF'
#!/bin/sh
# On the base, fails its first run only (the base tree is the run's, writable).
if [ "$PWD" = "$SPAWN_BASELINE_TREE" ]; then
    [ -f .red-once ] && exit 0
    : >.red-once
fi
echo "FAILED lib::tests::adds: assertion failed"
exit 1
EOF
expect "a base that fails once and then passes is no red main" \
    "$(judge "$good" "" "$flaky_red")" \
    'exit=6 {"host_fault":true,"scenario":"check-red.sh","on":"candidate and base","runs":"1, then 1 on the candidate; 1, then 0 on the base"'
apart_red=$work/red-apart
cp -r "$red_scenarios" "$apart_red"
cat >"$apart_red/check-red.sh" <<'EOF'
#!/bin/sh
# On the base, a different test fails on each run.
if [ "$PWD" = "$SPAWN_BASELINE_TREE" ]; then
    if [ -f .red-once ]; then echo "FAILED lib::tests::second: boom"; exit 1; fi
    : >.red-once
fi
echo "FAILED lib::tests::first: boom"
exit 1
EOF
expect "two base failures that share no test are no red main" \
    "$(judge "$good" "" "$apart_red")" \
    'exit=6 {"host_fault":true,"scenario":"check-red.sh","on":"candidate and base","runs":"1, then 1 on the candidate; 1, then 1 on the base"'
expect "still red on the base before the candidate: main_red, no candidate judged" \
    "$(SPAWN_GATE_BASE_CHECK=check-red.sh; export SPAWN_GATE_BASE_CHECK; judge "$good" "" "$red_scenarios")" \
    'exit=6 {"host_fault":true,"scenario":"check-red.sh","on":"main","runs":"1, then 1 on the base, before the candidate"'
refuse "the candidate was not built" "$(cat "$work/gate.out")" '| == candidate: build'
green_scenarios=$work/green
cp -r "$scenarios" "$green_scenarios"
printf '#!/bin/sh\nexit 0\n' >"$green_scenarios/check-tests.sh"
printf '#!/bin/sh\necho "lib: all pass"\nexit 0\n' >"$green_scenarios/check-red.sh"
expect "a later main that passes the red check is green on it, in the verdict" \
    "$(SPAWN_GATE_BASE_CHECK=check-red.sh; export SPAWN_GATE_BASE_CHECK; judge "$good" "" "$green_scenarios")" \
    'exit=0 {"verdict":"accepted","base_check":"check-red.sh"}'
expect "the log says so" "$(cat "$work/gate.out")" \
    'base check: check-red.sh passes on the base: main is green on it again'
# Green on the red check, then a host fault on another check: the fact
# still says main is green on it (`base_check`), as every fact the gate
# writes after the base check does.
green_fault=$work/green-fault
cp -r "$green_scenarios" "$green_fault"
printf '#!/bin/sh\nexit 1\n' >"$green_fault/check-down.sh"
expect "a host fault after a green base check still says it is green" \
    "$(SPAWN_GATE_BASE_CHECK=check-red.sh; export SPAWN_GATE_BASE_CHECK; judge "$good" "" "$green_fault")" \
    '"on":"candidate and base","runs":"1, then 1 on the candidate; 1, then 1 on the base"'
expect "with base_check in its fact" "$(cat "$integrator/verdict")" '"base_check":"check-red.sh"'
expect "a base check that is no check of the gate's is not run" \
    "$(SPAWN_GATE_BASE_CHECK=../check-red.sh; export SPAWN_GATE_BASE_CHECK; judge "$good" "" "$green_scenarios")" \
    'exit=0 {"verdict":"accepted"}'
expect "and says so" "$(cat "$work/gate.out")" "base check: ../check-red.sh is no check of this gate's: not run"
expect "check-tests.sh runs first among the regressions" \
    "$(grep -E '^\| == regression: (check-tests|007)\.sh' "$work/gate.out" | head -n 1)" 'check-tests.sh'

# 5b. An objective scenario the base fails too is unsolved, not a regression;
# one the base passes and the candidate breaks is a regression.
unsolved_scenarios=$work/unsolved
cp -r "$scenarios" "$unsolved_scenarios"
printf '#!/bin/sh\n[ -f solved ]\n' >"$unsolved_scenarios/010.sh"
expect "an unsolved objective does not block other work" \
    "$(judge "$good" "" "$unsolved_scenarios")" 'exit=0 {"verdict":"accepted"}'
expect "it is reported as unsolved" "$(cat "$work/gate.out")" 'an unsolved objective, not a regression'
printf '#!/bin/sh\n[ "$(cat value)" = one ]\n' >"$unsolved_scenarios/010.sh"
expect "breaking a solved objective is a regression" \
    "$(judge "$good" "" "$unsolved_scenarios")" 'regression: 010.sh fails on the candidate'

expect "relative scenarios refused" "$(sh "$gate" "$repo" "$base" "$good" scenarios | tail -n 1)" 'not absolute'

# 1. The reviewer's forgery: candidate code finds the gate in /proc, writes
# an accepted verdict to its stdout and to the integrator's verdict file,
# and kills it. In the sandbox it finds neither: the gate exits 0 with its
# own verdict, in the file.
branch forge
printf 'two\n' >"$seed/value"
cat >"$seed/probe.sh" <<EOF
#!/bin/sh
forged='{"verdict":"accepted"}'
echo "\$forged" >"$integrator/verdict" 2>/dev/null
for p in /proc/[0-9]*; do
    if tr '\\0' ' ' <"\$p/cmdline" 2>/dev/null | grep -q '[/ ]gate\\.sh '; then
        echo "\$forged" >"\$p/fd/1" 2>/dev/null
        kill -9 "\${p#/proc/}" 2>/dev/null
    fi
done
exit 1
EOF
forge=$(commit forge)
expect "a forged verdict and a killed gate do not land" "$(judge "$forge" "$scenarios/007.sh")" \
    'exit=0 {"verdict":"rejected","reason":"regression: check-probe.sh fails on the candidate"}'
refuse "the gate's stdout carries no forged verdict" "$(grep -v '^|' "$work/gate.out")" 'accepted'

# 2. The owner's database URL does not reach candidate code; the test
# database's does.
branch env
printf 'two\n' >"$seed/value"
cat >"$seed/probe.sh" <<'EOF'
#!/bin/sh
[ -z "${SPAWN_DATABASE_URL-}" ] || { echo "owner database: $SPAWN_DATABASE_URL"; exit 1; }
case "${SPAWN_TEST_DATABASE_URL-}" in
"postgres://t:p%40ss@%2F"*"%2Ftest-db:5433/spawn_test?sslmode=disable") ;;
*) echo "no test database: ${SPAWN_TEST_DATABASE_URL-}"; exit 1 ;;
esac
[ -z "${OWNER_TOKEN-}" ] || { echo "owner token leaked"; exit 1; }
EOF
env_candidate=$(commit env)
export SPAWN_DATABASE_URL=postgres://owner OWNER_TOKEN=secret \
    SPAWN_TEST_DATABASE_URL="postgres://t:p%40ss@test:5433/spawn_test?sslmode=disable"
expect "candidate code sees only the allowed environment" "$(judge "$env_candidate")" 'exit=0 {"verdict":"accepted"}'
unset SPAWN_DATABASE_URL SPAWN_TEST_DATABASE_URL OWNER_TOKEN

# 3. The sandbox: no owner secret, no gate in /proc, no host /tmp, nothing
# writable but the tree.
branch sandbox
printf 'two\n' >"$seed/value"
cat >"$seed/probe.sh" <<EOF
#!/bin/sh
fail=0
for f in "\$HOME/.config/spawn/secret" "\$HOME/.ssh/id_test" "\$HOME/.claude.json" \\
    "\$HOME/.local/share/spawn-rsi/spawn.env"; do
    if [ -s "\$f" ]; then echo "read \$f"; fail=1; fi
done
for p in /proc/[0-9]*; do
    if tr '\\0' ' ' <"\$p/cmdline" 2>/dev/null | grep -q '[/ ]gate\\.sh '; then echo "sees the gate: \$p"; fail=1; fi
done
[ ! -e "$marker" ] || { echo "sees the host's /tmp"; fail=1; }
[ ! -e "$integrator/stdout" ] || { echo "sees the integrator's directory"; fail=1; }
touch "$work/escaped" 2>/dev/null # lands in the private /tmp at most; checked below
! touch "$scenarios/escaped" 2>/dev/null || { echo "wrote into the scenarios"; fail=1; }
! touch "\$SPAWN_BASELINE_TREE/escaped" 2>/dev/null || { echo "wrote into the base tree"; fail=1; }
touch inside || { echo "cannot write its own tree"; fail=1; }
exit \$fail
EOF
sandboxed=$(commit sandbox)
expect "candidate code is confined" "$(judge "$sandboxed")" 'exit=0 {"verdict":"accepted"}'
[ ! -e "$work/escaped" ] || { echo "FAIL the sandbox wrote $work/escaped"; exit 1; }

# 3b. $HOME is an allow-list: a file nobody thought to mask is not there.
branch home
printf 'two\n' >"$seed/value"
cat >"$seed/probe.sh" <<'EOF'
#!/bin/sh
fail=0
for f in .gitconfig .claude.json.backup .bash_history .hermes/key; do
    if cat "$HOME/$f" 2>/dev/null | grep -q owner; then echo "read ~/$f"; fail=1; fi
done
exit $fail
EOF
home=$(commit home)
expect "a \$HOME file outside the allow-list is not readable" "$(judge "$home")" 'exit=0 {"verdict":"accepted"}'

# 3b'. A tool PATH finds under $HOME (node through a ~/.local/bin symlink)
# still runs; its neighbours in ~/.local/bin stay hidden, and so does a
# tool not on the allow-list (bun, though PATH finds it).
mkdir -p "$HOME/opt/node/bin" "$HOME/.local/bin" "$HOME/.cargo/bin" "$HOME/.bun/bin"
printf '#!/bin/sh\necho fake-node\n' >"$HOME/opt/node/bin/node"
chmod 755 "$HOME/opt/node/bin/node"
ln -s "$HOME/opt/node/bin/node" "$HOME/.local/bin/node"
printf '#!/bin/sh\n# token=owner\n' >"$HOME/.local/bin/owner-tool"
printf '#!/bin/sh\necho fake-bun\n' >"$HOME/.bun/bin/bun"
chmod 755 "$HOME/.bun/bin/bun"
branch tools
printf 'two\n' >"$seed/value"
cat >"$seed/probe.sh" <<'EOF'
#!/bin/sh
[ "$(node)" = fake-node ] || { echo "node does not run"; exit 1; }
[ ! -e "$HOME/.local/bin/owner-tool" ] || { echo "sees ~/.local/bin/owner-tool"; exit 1; }
[ ! -e "$HOME/.bun" ] || { echo "sees ~/.bun"; exit 1; }
EOF
tools=$(commit tools)
expect "a tool under \$HOME runs, its neighbours and ~/.bun stay hidden" \
    "$(PATH="$HOME/.local/bin:$HOME/.bun/bin:$PATH" judge "$tools")" 'exit=0 {"verdict":"accepted"}'

# 3c. No network but loopback: a host listener and the live database port
# are unreachable; the sandbox's own loopback works.
listener=$work/listener.port
python3 -c '
import socket, sys, time
s = socket.socket(); s.bind(("127.0.0.1", 0)); s.listen()
print(s.getsockname()[1], flush=True)
time.sleep(600)' >"$listener" &
listener_pid=$!
background="$background $listener_pid"
while [ ! -s "$listener" ]; do sleep 0.1; done
host_port=$(cat "$listener")
# The probe's control: the host listener answers from outside the sandbox.
python3 -c "import socket; socket.create_connection(('127.0.0.1', $host_port), 2)"
branch network
printf 'two\n' >"$seed/value"
cat >"$seed/probe.sh" <<EOF
#!/bin/sh
fail=0
for port in $host_port 15433; do
    if python3 -c "import socket; socket.create_connection(('127.0.0.1', \$port), 2)" 2>/dev/null; then
        echo "reached 127.0.0.1:\$port"; fail=1
    fi
done
python3 -c '
import socket
s = socket.socket(); s.bind(("127.0.0.1", 0)); s.listen()
socket.create_connection(s.getsockname(), 2)' || { echo "no loopback"; fail=1; }
exit \$fail
EOF
network=$(commit network)
expect "the host's listeners (and 127.0.0.1:15433) are unreachable" "$(judge "$network")" 'exit=0 {"verdict":"accepted"}'

# 3d. No Unix socket of the host but the test database's: candidate code
# tries connect() on every socket this uid may connect to outside the
# sandbox (`find / -type s -writable`, as the host has them now) and on the
# host's abstract sockets, and must reach only the test database. Planted
# beside them, where a host keeps its services: a PostgreSQL socket in
# /tmp, one in the user's runtime directory and in /var/tmp, a neighbour
# cluster in the test database's own socket directory, and one in a
# directory none of the sandbox's tmpfs covers (the scan finds it). The gate
# runs without TMPDIR, as the owner's `env -i` run does.
plant=$(mktemp -d "$work/plant.XXXXXX")
socket_db=$work/socket-db
mkdir -p "$socket_db"
planted="/tmp/.s.PGSQL.$$ $socket_db/.s.PGSQL.5433 $plant/service.sock"
for dir in "/run/user/$(id -u)" /var/tmp; do
    [ -d "$dir" ] && [ -w "$dir" ] && planted="$planted $dir/test-gate-$$.sock"
done
# shellcheck disable=SC2086 # paths
python3 -c '
import socket, sys, time
for path in sys.argv[1:]:
    s = socket.socket(socket.AF_UNIX); s.bind(path); s.listen(64)
    globals().setdefault("held", []).append(s)
time.sleep(900)' "$socket_db/.s.PGSQL.5432" $planted &
background="$background $!"
for path in "$socket_db/.s.PGSQL.5432" $planted; do
    while [ ! -S "$path" ]; do sleep 0.1; done
done
leftover="/tmp/.s.PGSQL.$$ /run/user/$(id -u)/test-gate-$$.sock /var/tmp/test-gate-$$.sock"
find / \( -path /proc -o -path /sys \) -prune -o -type s -writable -print 2>/dev/null |
    grep -vxF "$socket_db/.s.PGSQL.5432" >"$work/host-sockets" || true
for path in $planted; do
    grep -qxF "$path" "$work/host-sockets" || { echo "FAIL the scan outside misses $path"; exit 1; }
done
awk 'NR > 1 && $8 ~ /^@/ { print substr($8, 2) }' /proc/net/unix | sort -u >"$work/host-abstract"
# The probe's control: outside, every planted socket and the test database
# answer.
# shellcheck disable=SC2086 # paths
python3 -c '
import socket, sys
for path in sys.argv[1:]:
    socket.socket(socket.AF_UNIX).connect(path)' "$socket_db/.s.PGSQL.5432" $planted
branch sockets
printf 'two\n' >"$seed/value"
cp "$work/host-sockets" "$seed/host-sockets"
cp "$work/host-abstract" "$seed/host-abstract"
cat >"$seed/probe.sh" <<EOF
#!/bin/sh
exec python3 -c '
import socket, sys
reached = []
for path in open("host-sockets").read().split("\n"):
    if not path:
        continue
    s = socket.socket(socket.AF_UNIX); s.settimeout(1)
    try:
        s.connect(path); reached.append(path)
    except OSError:
        pass
for name in open("host-abstract").read().split("\n"):
    if not name:
        continue
    s = socket.socket(socket.AF_UNIX); s.settimeout(1)
    try:
        s.connect("\\0" + name); reached.append("@" + name)
    except OSError:
        pass
database = "$socket_db/.s.PGSQL.5432"
try:
    socket.socket(socket.AF_UNIX).connect(database)
except OSError as error:
    print("the test database is unreachable:", error); sys.exit(1)
for path in reached:
    print("reached", path)
print("tried", len(open("host-sockets").read().split()), "sockets")
sys.exit(1 if reached else 0)'
EOF
sockets=$(commit sockets)
expect "no host socket but the test database's is reachable" "$(
    SPAWN_TEST_DATABASE_URL="postgres://u:p%40ss@$(printf '%s' "$socket_db" | sed 's|/|%2F|g')/db"
    export SPAWN_TEST_DATABASE_URL
    unset TMPDIR
    judge "$sockets"
)" 'exit=0 {"verdict":"accepted"}'
case "$plant" in
/tmp/*) ;;
*) expect "the scan hid what no tmpfs covers" "$(cat "$work/gate.out")" 'sockets hidden from the sandbox' ;;
esac

# 3d'. A socket directory the scan found and that then goes away (a host
# process's temporary directory) does not break the sandbox: the gate does
# not mount over it, and the verdict is the candidate's. Deterministic: the
# gate is held (SPAWN_GATE_TEST_HOLD) after its build hook's sandbox checked
# the mask and before bwrap mounts it (the build is where this was a false
# "does not build"), the directory goes in that window, and
# the sandbox, whose bwrap names the path now gone, is started once more
# without it — and says so.
vanish=$(mktemp -d "$work/vanish.XXXXXX")
python3 -c '
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen()
time.sleep(900)' "$vanish/service.sock" &
vanish_listener=$!
background="$background $vanish_listener"
while [ ! -S "$vanish/service.sock" ]; do sleep 0.1; done
rm -f "$work/gate.out"
hold=$work/sandbox-hold
: >"$hold"
(
    held=0
    until [ -e "$hold.held" ] || [ "$held" -ge 6000 ]; do sleep 0.05; held=$((held + 1)); done
    kill "$vanish_listener"
    rm -rf "$vanish"
    rm -f "$hold"
) &
vanish_watcher=$!
expect "a hidden socket directory that goes away mid-gate changes no verdict" "$(
    export SPAWN_GATE_TEST_HOLD="$hold"
    unset TMPDIR
    judge "$good"
)" 'exit=0 {"verdict":"accepted"}'
kill "$vanish_watcher" 2>/dev/null || true
if [ -e "$hold.held" ]; then
    expect "the sandbox it broke was started again, and said so" "$(cat "$work/gate.out")" \
        "| sandbox: $vanish went away between the mask check and bwrap's mount"
    rm -f "$hold.held"
    # A regular file where the directory was holds no socket: as for a
    # directory gone, the sandbox starts again without the mask, and says
    # so (rev-b's 3d' did this beside a gate of another suite that had
    # scanned its directory, 2026-11-09).
    vanish=$(mktemp -d "$work/vanish.XXXXXX")
    python3 -c '
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen()
time.sleep(900)' "$vanish/service.sock" &
    vanish_listener=$!
    background="$background $vanish_listener"
    while [ ! -S "$vanish/service.sock" ]; do sleep 0.1; done
    : >"$hold"
    (
        held=0
        until [ -e "$hold.held" ] || [ "$held" -ge 6000 ]; do sleep 0.05; held=$((held + 1)); done
        kill "$vanish_listener"
        rm -rf "$vanish"
        : >"$vanish"
        rm -f "$hold"
    ) &
    vanish_watcher=$!
    as_file=$(
        export SPAWN_GATE_TEST_HOLD="$hold"
        unset TMPDIR
        judge "$good"
    )
    wait "$vanish_watcher" 2>/dev/null || true
    expect "a hidden socket directory replaced by a regular file mid-gate changes no verdict" "$as_file" \
        'exit=0 {"verdict":"accepted"}'
    expect "and the sandbox was started again, and said so" "$(cat "$work/gate.out")" \
        "| sandbox: $vanish became a regular file between the mask check and bwrap's mount"
    rm -f "$vanish" "$hold.held"
    # Any other bwrap failure stays a failure: here the masked path is
    # still there, as a FIFO where the directory was (neither gone nor a
    # regular file), so bwrap fails on it and the gate does not start the
    # sandbox again.
    vanish=$(mktemp -d "$work/vanish.XXXXXX")
    python3 -c '
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen()
time.sleep(900)' "$vanish/service.sock" &
    vanish_listener=$!
    background="$background $vanish_listener"
    while [ ! -S "$vanish/service.sock" ]; do sleep 0.1; done
    : >"$hold"
    (
        held=0
        until [ -e "$hold.held" ] || [ "$held" -ge 6000 ]; do sleep 0.05; held=$((held + 1)); done
        kill "$vanish_listener"
        rm -rf "$vanish"
        mkfifo "$vanish"
        rm -f "$hold"
    ) &
    vanish_watcher=$!
    replaced=$(
        export SPAWN_GATE_TEST_HOLD="$hold"
        unset TMPDIR
        judge "$good"
    )
    wait "$vanish_watcher" 2>/dev/null || true
    refuse "a mask path still there (a FIFO now) is no race: no second start" \
        "$(cat "$work/gate.out")" "started again without it"
    refuse "and the gate does not pass it off as accepted" "$replaced" 'exit=0 {"verdict":"accepted"}'
    # A sandbox that never started its command is no verdict either way: a
    # host fault, said as such (exit 6, on the sandbox, its own evidence),
    # never "does not build" (A sandbox that never started its command).
    expect "a sandbox that never started its command is a host fault: exit 6" "$replaced" 'exit=6 {"host_fault":true'
    expect "on the sandbox" "$replaced" '"on":"sandbox","runs":"never started"'
    expect "with its evidence" "$replaced" '"evidence":"sandbox_never_started"'
    expect "quoting bwrap" "$replaced" 'never started (bwrap exited'
    refuse "never a rejection" "$replaced" '"verdict":"rejected"'
    rm -f "$vanish" "$hold.held"
    # The candidate's own tree bwrap cannot start in, while it starts in the
    # base's: the candidate's doing, so a rejection quoting bwrap, never a
    # host fault it could repeat forever. Emulated at the same hold: the
    # held tree (the candidate's: the base's results are cached by now) is
    # made untraversable, so bwrap's --chdir fails there, and stays so for
    # the gate's probe of both trees.
    vanish=$(mktemp -d "$work/vanish.XXXXXX")
    python3 -c '
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen()
time.sleep(900)' "$vanish/service.sock" &
    vanish_listener=$!
    background="$background $vanish_listener"
    while [ ! -S "$vanish/service.sock" ]; do sleep 0.1; done
    : >"$hold"
    (
        held=0
        until [ -s "$hold.held" ] || [ "$held" -ge 6000 ]; do sleep 0.05; held=$((held + 1)); done
        held_tree=$(cat "$hold.held" 2>/dev/null)
        case "$held_tree" in */candidate) chmod 000 "$held_tree" ;; esac
        rm -f "$hold"
    ) &
    vanish_watcher=$!
    untraversable=$(
        export SPAWN_GATE_TEST_HOLD="$hold"
        unset TMPDIR
        judge "$good"
    )
    wait "$vanish_watcher" 2>/dev/null || true
    held_tree=$(cat "$hold.held" 2>/dev/null || true)
    case "$held_tree" in
    */candidate)
        # The gate's own exit could not remove a tree it cannot read.
        chmod 700 "$held_tree" 2>/dev/null || true
        rm -rf "$(dirname "$held_tree")"
        expect "a candidate's tree bwrap cannot start in is rejected" "$untraversable" \
            "exit=0 {\"verdict\":\"rejected\",\"reason\":\"the sandbox cannot start in the candidate's tree, where it starts in the base's: sandbox: "
        expect "quoting bwrap" "$untraversable" 'never started (bwrap exited'
        refuse "not a host fault" "$untraversable" 'host_fault'
        ;;
    *) echo "FAIL the hold held $held_tree, not the candidate's tree (the base's results were not cached)"; exit 1 ;;
    esac
    kill "$vanish_listener" 2>/dev/null || true
    rm -rf "$vanish" "$hold" "$hold.held"
    # A base run whose sandbox never started writes no cache entry, and the
    # gate says the fact (exit 6, the base's tree): held at the base job's
    # run (the hold file says `base-run`), the mask path made a FIFO, so
    # bwrap fails there. One objective, a cache of its own: the next gate
    # on it runs the objective again rather than read a false result.
    nsbase=$work/never-base
    mkdir -p "$nsbase"
    cp "$scenarios/protected" "$nsbase/"
    printf '#!/bin/sh\nexit 0\n' >"$nsbase/101.sh"
    vanish=$(mktemp -d "$work/vanish.XXXXXX")
    python3 -c '
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen()
time.sleep(900)' "$vanish/service.sock" &
    vanish_listener=$!
    background="$background $vanish_listener"
    while [ ! -S "$vanish/service.sock" ]; do sleep 0.1; done
    printf 'base-run\n' >"$hold"
    (
        held=0
        until [ -s "$hold.held" ] || [ "$held" -ge 6000 ]; do sleep 0.05; held=$((held + 1)); done
        kill "$vanish_listener"
        rm -rf "$vanish"
        mkfifo "$vanish"
        rm -f "$hold"
    ) &
    vanish_watcher=$!
    never_base=$(
        export SPAWN_GATE_TEST_HOLD="$hold" XDG_CACHE_HOME="$work/caches/never-base"
        unset TMPDIR
        judge "$good" "" "$nsbase"
    )
    wait "$vanish_watcher" 2>/dev/null || true
    case "$(cat "$hold.held" 2>/dev/null)" in
    */baseline) ;;
    *) echo "FAIL the base-run hold held $(cat "$hold.held" 2>/dev/null), not the base's tree"; exit 1 ;;
    esac
    expect "a base run whose sandbox never started is a host fault: exit 6" "$never_base" 'exit=6 {"host_fault":true'
    expect "in the base's tree" "$never_base" '"evidence":"sandbox_never_started","tree":"base"'
    expect "the base job says nothing was cached" "$(cat "$work/gate.out")" \
        'its sandbox never started it; no result, nothing cached'
    rm -f "$vanish" "$hold.held"
    never_again=$(
        export XDG_CACHE_HOME="$work/caches/never-base"
        unset TMPDIR
        judge "$good" "" "$nsbase"
    )
    expect "the next gate judges" "$never_again" 'exit=0 {"verdict":"accepted"}'
    expect "and runs the objective again, never a cached result" "$(cat "$work/gate.out")" \
        '| base results from the cache: none; run on the base: 101.sh'
    # The red check's base run (SPAWN_GATE_BASE_CHECK) whose sandbox never
    # started: outside the job printers, the block reads the note itself,
    # and it is the same fact in the base's tree, before the candidate is
    # built. No objective: the base check's run is the gate's first on the
    # base, where the hold holds it.
    nscheck=$work/never-check
    mkdir -p "$nscheck"
    cp "$scenarios/protected" "$nscheck/"
    printf '#!/bin/sh\nexit 0\n' >"$nscheck/check-red.sh"
    vanish=$(mktemp -d "$work/vanish.XXXXXX")
    python3 -c '
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen()
time.sleep(900)' "$vanish/service.sock" &
    vanish_listener=$!
    background="$background $vanish_listener"
    while [ ! -S "$vanish/service.sock" ]; do sleep 0.1; done
    printf 'base-run\n' >"$hold"
    (
        held=0
        until [ -s "$hold.held" ] || [ "$held" -ge 6000 ]; do sleep 0.05; held=$((held + 1)); done
        kill "$vanish_listener"
        rm -rf "$vanish"
        mkfifo "$vanish"
        rm -f "$hold"
    ) &
    vanish_watcher=$!
    never_check=$(
        export SPAWN_GATE_TEST_HOLD="$hold" XDG_CACHE_HOME="$work/caches/never-check" \
            SPAWN_GATE_BASE_CHECK=check-red.sh
        unset TMPDIR
        judge "$good" "" "$nscheck"
    )
    wait "$vanish_watcher" 2>/dev/null || true
    case "$(cat "$hold.held" 2>/dev/null)" in
    */baseline) ;;
    *) echo "FAIL the base-check hold held $(cat "$hold.held" 2>/dev/null), not the base's tree"; exit 1 ;;
    esac
    expect "a base check whose sandbox never started is a host fault: exit 6" "$never_check" \
        'exit=6 {"host_fault":true,"scenario":"check-red.sh","on":"sandbox","runs":"never started"'
    expect "in the base's tree" "$never_check" '"evidence":"sandbox_never_started","tree":"base"'
    refuse "and no word of green" "$never_check" '"base_check"'
    refuse "before the candidate is built" "$(cat "$work/gate.out")" '| == candidate: build'
    rm -f "$vanish" "$hold.held"
else
    # No hold is right only where the sandbox shows no socket to scan: under
    # the roots it covers whole (/tmp, which is the gate's TMPDIR here, /run
    # and /var/tmp). Anywhere else the gate should have masked the socket and
    # held, and every check above passed with nothing tested (rev-b on
    # 401d5e62: a gate too slow to reach the hold, as at load 45).
    case "$vanish" in
    /tmp/* | /run/* | /var/tmp/*)
        echo "note: $vanish was not under a scanned directory (TMPDIR covers it): no mask, no hold"
        ;;
    *)
        echo "FAIL the hold never held: $vanish is under a scanned directory, so the gate should have masked its socket and held there"
        exit 1
        ;;
    esac
fi

# 3e. A TCP test database URL works inside, through the gate's forwarder,
# for 127.0.0.1, localhost and [::1] (when the host has it): the sandbox
# reaches that server over a socket of the gate's, and still has no
# network. The fake server echoes what it is sent.
tcp_db=$work/tcp-db.ports
python3 -c '
import socket, sys, threading, time
def serve(listener):
    while True:
        client, _ = listener.accept()
        client.sendall(client.recv(64)); client.close()
for family, address in ((socket.AF_INET, "127.0.0.1"), (socket.AF_INET6, "::1")):
    try:
        listener = socket.socket(family); listener.bind((address, 0)); listener.listen(16)
    except OSError:
        print(address, "none", flush=True); continue
    print(address, listener.getsockname()[1], flush=True)
    threading.Thread(target=serve, args=(listener,), daemon=True).start()
time.sleep(900)' >"$tcp_db" &
background="$background $!"
while [ "$(grep -c . "$tcp_db" 2>/dev/null)" != 2 ]; do sleep 0.1; done
v4_port=$(awk '$1 == "127.0.0.1" { print $2 }' "$tcp_db")
v6_port=$(awk '$1 == "::1" { print $2 }' "$tcp_db")
branch tcp-db
printf 'two\n' >"$seed/value"
cat >"$seed/probe.sh" <<'EOF'
#!/bin/sh
exec python3 -c '
import os, socket, sys, urllib.parse
url = os.environ["SPAWN_TEST_DATABASE_URL"]
authority = url.split("://", 1)[1].split("/", 1)[0]
user, hostport = authority.rsplit("@", 1)
host, port = hostport.rsplit(":", 1)
directory = urllib.parse.unquote(host)
if user != "u:p%40ss" or not directory.startswith("/") or not url.endswith("/spawn_test?sslmode=disable"):
    print("not the forwarded socket URL:", url); sys.exit(1)
s = socket.socket(socket.AF_UNIX); s.settimeout(5)
s.connect(os.path.join(directory, ".s.PGSQL." + port)); s.sendall(b"ping")
if s.recv(64) != b"ping":
    print("the test server did not answer"); sys.exit(1)
try:
    socket.create_connection(("127.0.0.1", int(sys.argv[1])), 2)
    print("reached 127.0.0.1 over TCP"); sys.exit(1)
except OSError:
    pass' "$(cat tcp-port)"
EOF
printf '%s\n' "$v4_port" >"$seed/tcp-port"
tcp_candidate=$(commit tcp-db)
for host in 127.0.0.1 localhost; do
    expect "a TCP test URL to $host reaches the test server, through the forwarder only" \
        "$(SPAWN_TEST_DATABASE_URL="postgres://u:p%40ss@$host:$v4_port/spawn_test?sslmode=disable" \
            judge "$tcp_candidate")" 'exit=0 {"verdict":"accepted"}'
done
if [ "$v6_port" != none ]; then
    expect "a TCP test URL to [::1] reaches the test server" \
        "$(SPAWN_TEST_DATABASE_URL="postgres://u:p%40ss@[::1]:$v6_port/spawn_test?sslmode=disable" \
            judge "$tcp_candidate")" 'exit=0 {"verdict":"accepted"}'
fi
expect "a socket URL whose socket is missing is no verdict" \
    "$(SPAWN_TEST_DATABASE_URL="postgres://u:p@%2Fnonexistent%2Fdir/db" judge "$tcp_candidate")" 'exit=3 '
expect "and says why" "$(cat "$work/gate.out")" 'no test database socket at /nonexistent/dir/.s.PGSQL.5432'

# 4. A build script by another name: `build = ` in any Cargo.toml, however
# spelled. `[build-dependencies]` and `build-override` are not build keys
# (the new crate inherits the workspace's lints, as every new crate must).
branch buildkey
printf 'two\n' >"$seed/value"
mkdir -p "$seed/tools"
printf 'fn main() {}\n' >"$seed/tools/gen.rs"
printf '[package]\nname = "x"\nbuild = "tools/gen.rs"\n' >"$seed/Cargo.toml"
buildkey=$(commit buildkey)
expect "a build key in Cargo.toml refused" "$(judge "$buildkey")" 'build key'
branch buildkey-quoted
printf 'two\n' >"$seed/value"
mkdir -p "$seed/crates/x"
printf 'package = { name = "x", "build" = "gen.rs" }\n' >"$seed/crates/x/Cargo.toml"
quoted=$(commit buildkey-quoted)
expect "a quoted build key in a nested Cargo.toml refused" "$(judge "$quoted")" 'build key'
branch buildkey-none
printf 'two\n' >"$seed/value"
mkdir -p "$seed/crates/x"
printf '[package]\nname = "x"\n[build-dependencies]\ncc = "1"\n[profile.dev.build-override]\nopt-level = 1\n[lints]\nworkspace = true\n' \
    >"$seed/crates/x/Cargo.toml"
nobuildkey=$(commit buildkey-none)
expect "build-dependencies are not a build key" "$(judge "$nobuildkey")" 'exit=0 {"verdict":"accepted"}'

# 5. The metric is the scenario's one stdout line: a METRIC line on stderr
# is ignored, and a second one on stdout voids the metric.
branch metric-stderr
printf 'two\n' >"$seed/value"
printf '#!/bin/sh\necho "METRIC speed baseline=10 candidate=1" >&2\n' >"$seed/noise.sh"
noisy=$(commit metric-stderr)
expect "a METRIC line on stderr is not the metric" "$(judge "$noisy" "$scenarios/008.sh")" \
    '"metric":"METRIC speed baseline=10 candidate=5"'
branch metric-twice
printf 'two\n' >"$seed/value"
printf '#!/bin/sh\necho "METRIC speed baseline=10 candidate=1"\n' >"$seed/noise.sh"
twice=$(commit metric-twice)
expect "two METRIC lines on stdout: no metric" "$(judge "$twice" "$scenarios/008.sh")" 'exit=0 {"verdict":"accepted"}'
expect "two METRIC lines are reported" "$(cat "$work/gate.out")" 'two or more METRIC lines'

# 7. The candidate may not run fewer tests than the base, as cargo counts
# them, nor stop running one of the base's by name, nor add or edit a test
# into an empty body: a tiny crate on a base of its own (the only cases that
# run cargo, offline, no dependencies). Its base runs 4: tests::a, tests::b,
# extra and the doctest of `one` (tests::c is ignored).
branch base-rust
printf '[package]\nname = "t"\nversion = "0.1.0"\nedition = "2021"\n' >"$seed/Cargo.toml"
mkdir -p "$seed/src" "$seed/tests"
cat >"$seed/src/lib.rs" <<'EOF'
//! A crate with tests.

/// One.
///
/// ```
/// assert_eq!(t::one(), 1);
/// ```
pub fn one() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn a() {
        assert_eq!(super::one(), 1);
    }

    #[test]
    fn b() { assert_eq!(super::one() + 1, 2); }

    #[test]
    #[ignore]
    fn c() { assert_eq!(super::one(), 1); }
}
EOF
printf '#[test]\nfn extra() {\n    assert_eq!(t::one(), 1);\n}\n' >"$seed/tests/extra.rs"
(cd "$seed" && CARGO_NET_OFFLINE=true cargo generate-lockfile -q)
rust_base=$(commit base-rust)
rust_branch() {
    git -C "$seed" checkout -q -B "$1" "$rust_base"
    printf 'two\n' >"$seed/value"
}
judge_rust() {
    saved_base=$base
    base=$rust_base
    judge "$1"
    base=$saved_base
}
# drop_b: tests::b leaves src/lib.rs.
drop_b() { sed -i '/#\[test\]$/{N;/fn b() {/d}' "$seed/src/lib.rs"; }
rust_branch rust-same
same=$(commit rust-same)
expect "the same tests land" "$(judge_rust "$same")" 'exit=0 {"verdict":"accepted"}'
expect "the counts are reported" "$(cat "$work/gate.out")" 'tests cargo runs: 4 on the base, 4 on the candidate'
rust_branch rust-doc-shifted
sed -i '1a //!\n//! More about the crate.' "$seed/src/lib.rs"
shifted=$(commit rust-doc-shifted)
expect "a doctest whose line moved keeps its name" "$(judge_rust "$shifted")" 'exit=0 {"verdict":"accepted"}'
rust_branch rust-comment
drop_b
printf '\n// #[test]\n// fn b() {}\n/// A string, not a test.\npub const B: &str = "#[test] fn b() {}";\n' \
    >>"$seed/src/lib.rs"
comment=$(commit rust-comment)
expect "a test replaced by a comment and a string is refused" "$(judge_rust "$comment")" \
    'runs fewer tests (4 -> 3)'
rust_branch rust-orphan
drop_b
printf '#[test]\nfn b() {\n    assert_eq!(1 + 1, 2);\n}\n' >"$seed/src/orphan.rs"
orphan=$(commit rust-orphan)
expect "a test moved to a file no mod includes is refused" "$(judge_rust "$orphan")" \
    'runs fewer tests (4 -> 3)'
rust_branch rust-ignored
sed -i 's/^    fn b() {/    #[ignore]\n&/' "$seed/src/lib.rs"
ignored=$(commit rust-ignored)
expect "an ignored test is refused" "$(judge_rust "$ignored")" 'runs fewer tests (4 -> 3)'
rust_branch rust-cfg
sed -i 's/^#\[cfg(test)\]$/#[cfg(any())]/' "$seed/src/lib.rs"
uncompiled=$(commit rust-cfg)
expect "a test module compiled out is refused" "$(judge_rust "$uncompiled")" 'runs fewer tests (4 -> 2)'
rust_branch rust-renamed
sed -i 's/^    fn b() {/    fn b_renamed() {/' "$seed/src/lib.rs"
renamed=$(commit rust-renamed)
expect "a renamed test is refused though the count holds" "$(judge_rust "$renamed")" \
    'drops or renames base tests (1: tests::b)'
expect "the count did hold" "$(cat "$work/gate.out")" 'tests cargo runs: 4 on the base, 4 on the candidate'
retiring_scenarios=$work/retiring
cp -r "$scenarios" "$retiring_scenarios"
printf '# retired with their feature\ntests::b\n' >"$retiring_scenarios/retired-tests"
saved_base=$base
base=$rust_base
expect "a test the owner retired may go" "$(judge "$renamed" "" "$retiring_scenarios")" \
    'exit=0 {"verdict":"accepted"}'
# Retired by its last path component only (batch 16's bare line): nothing
# matches it, and the refusal says why and what to write.
printf '# retired by its last component only\nb\n' >"$retiring_scenarios/retired-tests"
expect "a test retired by its last component only is still a drop, and the reason says so" \
    "$(judge "$renamed" "" "$retiring_scenarios")" \
    'drops or renames base tests (1: tests::b); retired-tests names it by its last component only: write the whole name, as cargo lists it'
expect "the log names the line and the test" "$(cat "$work/gate.out")" \
    '| retired-tests: b names tests::b only by its last component: write the whole name, as cargo lists it'
printf '# retired with their feature\ntests::b\n' >"$retiring_scenarios/retired-tests"
base=$saved_base
rust_branch rust-moved-path
drop_b
printf '#[test]\nfn b() {\n    assert_eq!(t::one() + 1, 2);\n}\n' >>"$seed/tests/extra.rs"
moved_path=$(commit rust-moved-path)
expect "a test moved to another module path is a rename" "$(judge_rust "$moved_path")" \
    'drops or renames base tests (1: tests::b)'
rust_branch rust-moved
drop_b
printf 'mod tests {\n    #[test]\n    fn b() {\n        assert_eq!(t::one() + 1, 2);\n    }\n}\n' \
    >>"$seed/tests/extra.rs"
moved=$(commit rust-moved)
expect "a test moved to another file under its module path lands" "$(judge_rust "$moved")" \
    'exit=0 {"verdict":"accepted"}'
rust_branch rust-emptied
sed -i 's/^    fn b() { .*/    fn b() {}/' "$seed/src/lib.rs"
emptied=$(commit rust-emptied)
expect "a test whose body is emptied is refused" "$(judge_rust "$emptied")" \
    'adds or edits a test with an empty body (src/lib.rs:19 b)'
rust_branch rust-emptied-comment
sed -i 's|^    fn b() { .*|    fn b() {\n        // kept for later\n        ;\n    }|' "$seed/src/lib.rs"
emptied_comment=$(commit rust-emptied-comment)
expect "a body of a comment and a semicolon is empty" "$(judge_rust "$emptied_comment")" \
    'adds or edits a test with an empty body (src/lib.rs:19 b)'
rust_branch rust-new-empty
printf '\n#[tokio::test(flavor = "current_thread")]\nasync fn d() {}\n' >>"$seed/tests/extra.rs"
new_empty=$(commit rust-new-empty)
expect "a new test with an empty body is refused" "$(judge_rust "$new_empty")" \
    'adds or edits a test with an empty body (tests/extra.rs:6 d)'
# The limit, stated: a body that asserts nothing real is not empty text.
rust_branch rust-assert-true
sed -i 's/^    fn b() { .*/    fn b() { assert!(true); }/' "$seed/src/lib.rs"
assert_true=$(commit rust-assert-true)
expect "the limit: assert!(true) is not caught" "$(judge_rust "$assert_true")" 'exit=0 {"verdict":"accepted"}'
rust_branch rust-broken
printf '#[test]\nfn broken() { let x: u32 = "no"; }\n' >>"$seed/tests/extra.rs"
broken=$(commit rust-broken)
expect "tests that do not build are refused" "$(judge_rust "$broken")" 'candidate tests do not build'

# 7b. The candidate's scenarios run the binary its build hook built, as the
# base's do, never the one the test listing linked again. Here a
# dev-dependency turns on a feature, as spawn-core's `testing` does in
# Spawn. The `spawn` bin says whether it was built with it, and 095 wants
# it built without, on both trees.
git -C "$seed" checkout -q -B rust-bin-base "$rust_base"
printf '\n[features]\ntesting = []\n\n[dev-dependencies]\nt = { path = ".", features = ["testing"] }\n' \
    >>"$seed/Cargo.toml"
mkdir -p "$seed/src/bin"
printf 'fn main() {\n    println!("{}", if cfg!(feature = "testing") { "testing" } else { "plain" });\n}\n' \
    >"$seed/src/bin/spawn.rs"
(cd "$seed" && CARGO_NET_OFFLINE=true cargo generate-lockfile -q)
bin_base=$(commit rust-bin-base)
printf 'two\n' >"$seed/value"
bin_candidate=$(commit rust-bin-candidate)
feature_scenarios=$work/feature-bin
mkdir -p "$feature_scenarios"
cp "$scenarios/protected" "$feature_scenarios/"
printf '#!/bin/sh\nCARGO_TARGET_DIR="$PWD/target" cargo build -q --bin spawn\n' >"$feature_scenarios/build"
printf '#!/bin/sh\nout=$("$SPAWN_BIN")\n[ "$out" = plain ] || { echo "SPAWN_BIN was built $out"; exit 1; }\n' \
    >"$feature_scenarios/095.sh"
# The control, a check (it must pass on the candidate): by the time the
# candidate's scenarios run, the listing has linked target/debug/spawn
# again, with the feature.
printf '#!/bin/sh\nout=$(target/debug/spawn)\n[ "$out" = testing ] || { echo "target/debug/spawn was built $out"; exit 1; }\n' \
    >"$feature_scenarios/check-relinked.sh"
saved_base=$base
base=$bin_base
expect "the candidate's scenarios run its build hook's binary, not the listing's" \
    "$(judge "$bin_candidate" "" "$feature_scenarios")" 'exit=0 {"verdict":"accepted"}'
base=$saved_base

# 8. Nor relax its lints: [lints] tables, clippy.toml, crate-level allows,
# a removed crate-level deny.
branch lints-table
printf 'two\n' >"$seed/value"
sed -i 's/^all = "deny"$/all = "allow"/' "$seed/crates/lib/Cargo.toml"
table=$(commit lints-table)
expect "a relaxed [lints] table is refused" "$(judge "$table")" 'lint configuration'
branch lints-clippy-toml
printf 'two\n' >"$seed/value"
printf 'too-many-lines-threshold = 1000\n' >"$seed/clippy.toml"
clippy_toml=$(commit lints-clippy-toml)
expect "a new clippy.toml is refused" "$(judge "$clippy_toml")" 'lint configuration'
branch lints-allow
printf 'two\n' >"$seed/value"
sed -i '1i #![allow(\n    clippy::all,\n)]' "$lib"
allowed=$(commit lints-allow)
expect "a new crate-level allow (multi-line) is refused" "$(judge "$allowed")" 'relaxes crate-level lints'
branch lints-deny-removed
printf 'two\n' >"$seed/value"
sed -i '/#!\[deny(missing_docs)\]/d' "$lib"
undenied=$(commit lints-deny-removed)
expect "a removed crate-level deny is refused" "$(judge "$undenied")" 'relaxes crate-level lints'
branch lints-deny-added
printf 'two\n' >"$seed/value"
sed -i '1i #![forbid(unsafe_code)]' "$lib"
tightened=$(commit lints-deny-added)
expect "a new crate-level forbid is fine" "$(judge "$tightened")" 'exit=0 {"verdict":"accepted"}'
# A new crate that inherits the workspace's lints relaxes nothing, the way
# one with no [lints] table does not (spawn-kdf, batch 8, was refused for
# it); one that brings a [lints.clippy] table of its own, or more under its
# [lints] than `workspace = true`, is still refused.
branch lints-new-inherits
printf 'two\n' >"$seed/value"
mkdir -p "$seed/crates/kdf"
printf '[package]\nname = "kdf"\n\n[lints]\nworkspace = true # the strict choice\n' >"$seed/crates/kdf/Cargo.toml"
inherits=$(commit lints-new-inherits)
expect "a new crate inheriting the workspace's lints is fine" "$(judge "$inherits")" 'exit=0 {"verdict":"accepted"}'
branch lints-new-relaxed
printf 'two\n' >"$seed/value"
mkdir -p "$seed/crates/kdf"
printf '[package]\nname = "kdf"\n\n[lints]\nworkspace = true\n\n[lints.clippy]\nall = "allow"\n' >"$seed/crates/kdf/Cargo.toml"
relaxed=$(commit lints-new-relaxed)
expect "a new crate with a [lints.clippy] relaxation of its own is refused" "$(judge "$relaxed")" 'lint configuration'
branch lints-new-extra
printf 'two\n' >"$seed/value"
mkdir -p "$seed/crates/kdf"
printf '[package]\nname = "kdf"\n\n[lints]\nworkspace = true\nclippy = { all = "allow" }\n' >"$seed/crates/kdf/Cargo.toml"
extra=$(commit lints-new-extra)
expect "a new crate with more than workspace = true under [lints] is refused" "$(judge "$extra")" 'lint configuration'
# Inheriting is required: a new crate with no [lints] table gets no lints at
# all, and is refused naming its manifest (batch 12; 77fc38ae only allowed
# it). A manifest with no [package] (a workspace root) is no crate.
branch lints-new-none
printf 'two\n' >"$seed/value"
mkdir -p "$seed/crates/kdf"
printf '[package]\nname = "kdf"\n' >"$seed/crates/kdf/Cargo.toml"
lint_free=$(commit lints-new-none)
expect "a new crate without [lints] workspace = true is refused, naming it" "$(judge "$lint_free")" \
    "crate crates/kdf is new and lacks \`[lints] workspace = true\` in its Cargo.toml; add it (a crate new in the candidate must inherit the workspace's lints)"
expect "the gate's log says so" "$(cat "$work/gate.out")" \
    "| crates new in the candidate without [lints] workspace = true: ./crates/kdf/Cargo.toml"
# The dotted and inline forms, with any spacing, inherit the same.
branch lints-new-dotted
printf 'two\n' >"$seed/value"
mkdir -p "$seed/crates/kdf"
printf '[package]\nname = "kdf"\nversion = "0.1.0"\n\nlints.workspace=true\n' >"$seed/crates/kdf/Cargo.toml"
dotted=$(commit lints-new-dotted)
expect "a new crate with lints.workspace = true is fine" "$(judge "$dotted")" 'exit=0 {"verdict":"accepted"}'
branch lints-new-inline
printf 'two\n' >"$seed/value"
mkdir -p "$seed/crates/kdf"
printf '[package]\nname = "kdf"\n\nlints = {  workspace =true }\n' >"$seed/crates/kdf/Cargo.toml"
inline=$(commit lints-new-inline)
expect "a new crate with lints = { workspace = true } is fine" "$(judge "$inline")" 'exit=0 {"verdict":"accepted"}'
branch lints-new-spaced
printf 'two\n' >"$seed/value"
mkdir -p "$seed/crates/kdf"
printf '[package]\nname = "kdf"\n\n  [lints]\n\tworkspace=   true\n' >"$seed/crates/kdf/Cargo.toml"
spaced=$(commit lints-new-spaced)
expect "a new crate with [lints] workspace=true in other spacing is fine" "$(judge "$spaced")" 'exit=0 {"verdict":"accepted"}'
branch lints-new-other-table
printf 'two\n' >"$seed/value"
mkdir -p "$seed/crates/kdf"
printf '[package]\nname = "kdf"\n\n[dependencies]\nworkspace = true\n' >"$seed/crates/kdf/Cargo.toml"
other_table=$(commit lints-new-other-table)
expect "workspace = true under another table is no inheritance" "$(judge "$other_table")" "crate crates/kdf is new and lacks"

# 9. Objectives are judged on the base before any candidate code runs, and
# cached. A fake shared test server (a Unix socket the gate binds in, as it
# binds the test database's) that candidate code can break, as a superuser
# login could break the real one.
server_dir=$work/fakedb
mkdir -p "$server_dir"
python3 -c '
import socket, sys
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen()
broken, count = False, 0
while True:
    c, _ = s.accept()
    command = c.recv(64).decode().strip()
    if command == "break": broken = True
    elif command == "fix": broken = False
    elif command == "count": count += 1
    elif command == "reset": count = 0
    c.sendall((str(count) if command == "count" else ("broken" if broken else "ok")).encode())
    c.close()' "$server_dir/.s.PGSQL.5432" &
server_pid=$!
background="$background $server_pid"
while [ ! -S "$server_dir/.s.PGSQL.5432" ]; do sleep 0.1; done
# server <command>: ok, broken, or a count.
cat >"$work/server-client" <<EOF
#!/bin/sh
exec python3 -c '
import socket, sys
c = socket.socket(socket.AF_UNIX); c.connect(sys.argv[1]); c.sendall(sys.argv[2].encode())
print(c.recv(64).decode())' "$server_dir/.s.PGSQL.5432" "\$1"
EOF
server() { sh "$work/server-client" "$1"; }
export SPAWN_TEST_DATABASE_URL="postgres://u:p@$(printf '%s' "$server_dir" | sed 's|/|%2F|g')/db"
objectives=$work/objectives
cp -r "$scenarios" "$objectives"
cp "$work/server-client" "$objectives/server-client"
# The build runs the candidate's own build step, as cargo runs its code.
printf '#!/bin/sh\ncp value built\n[ ! -f sabotage.sh ] || sh sabotage.sh\n' >"$objectives/build"
# 011: solved on the base (value is not three); a broken server is a
# harness error, exit 2.
cat >"$objectives/011.sh" <<'EOF'
#!/bin/sh
[ "$(sh "$(dirname "$0")/server-client" status)" = ok ] || exit 2
[ "$(cat value)" != three ] || exit 1
EOF

branch sabotage
printf 'three\n' >"$seed/value"
cat >"$seed/sabotage.sh" <<EOF
#!/bin/sh
sh '$objectives/server-client' break >/dev/null
EOF
sabotage=$(commit sabotage)
server fix >/dev/null
expect "a candidate that breaks the server cannot make a solved objective unsolved" \
    "$(judge "$sabotage" "" "$objectives")" 'regression: 011.sh fails on the candidate'
expect "the base was judged before the candidate built" \
    "$(sed -n '/== candidate: build/q;p' "$work/gate.out")" '| == base: 011.sh: solved'

server fix >/dev/null
expect "a cached base result is used" "$(judge "$good" "" "$objectives")" 'exit=0 {"verdict":"accepted"}'
expect "the cache hit runs nothing on the base" "$(cat "$work/gate.out")" '| == base: 011.sh: solved (cached)'
refuse "no base build on a cache hit" "$(cat "$work/gate.out")" '== base: build'

printf '# a comment changes the scenario\n' >>"$objectives/011.sh"
expect "a changed scenario is judged again" "$(judge "$good" "" "$objectives")" 'exit=0 {"verdict":"accepted"}'
refuse "not from the cache" "$(cat "$work/gate.out")" '011.sh: solved (cached)'

# Candidate code can neither see nor write the base cache: it tries to turn
# every entry into "unsolved" and to plant one.
cached_before=$(cd "$cache" && for entry in *; do printf '%s=%s\n' "$entry" "$(cat "$entry")"; done)
[ -n "$cached_before" ] || { echo "FAIL no base cache entries to protect"; exit 1; }
branch poison
printf 'two\n' >"$seed/value"
cat >"$seed/probe.sh" <<EOF
#!/bin/sh
for entry in "$cache"/*; do echo unsolved >"\$entry" 2>/dev/null; done
mkdir -p "$cache" 2>/dev/null
echo unsolved >"$cache/planted" 2>/dev/null
exit 0
EOF
poison=$(commit poison)
expect "a candidate that writes at the base cache still gets its verdict" \
    "$(judge "$poison" "" "$objectives")" 'exit=0 {"verdict":"accepted"}'
expect "the base cache is unchanged" \
    "$(cd "$cache" && for entry in *; do printf '%s=%s\n' "$entry" "$(cat "$entry")"; done)" "$cached_before"
[ ! -e "$cache/planted" ] || { echo "FAIL the sandbox planted a base cache entry"; exit 1; }
echo "ok   nothing planted in the base cache"

# 9b. The base cache's key is everything that decides a base result, and
# nothing else. The scenarios run from a controller checkout (two levels
# above them) whose helper 011.sh sources; a controller file no scenario
# names is not in the key. The controller lies under $HOME, as the owner's
# does: the sandbox binds its scripts back from there.
controller=$HOME/controller
mkdir -p "$controller/rsi" "$controller/scripts/blackbox"
cp -r "$objectives" "$controller/rsi/scenarios"
printf '# helper\n' >"$controller/scripts/blackbox/lib.sh"
printf '# the runner of other suites\n' >"$controller/scripts/blackbox/runner.sh"
printf '. "$(dirname "$0")/../../scripts/blackbox/lib.sh"\n' >>"$controller/rsi/scenarios/011.sh"
printf '# like scripts/blackbox/runner.sh, which it does not run\n' >>"$controller/rsi/scenarios/011.sh"
keyed=$controller/rsi/scenarios
# from_cache: the base results the last run took from the cache.
from_cache() { sed -n 's/^| base results from the cache: \(.*\); run on the base:.*/\1/p' "$work/gate.out"; }
judge "$good" "" "$keyed" >/dev/null
expect "the key is not the scenario's path: 007.sh is a hit from another directory" \
    "$(cat "$work/gate.out")" '| base results from the cache: 007.sh; run on the base: 011.sh'
expect "a cache hit: the same key again" "$(judge "$good" "" "$keyed")" 'exit=0 {"verdict":"accepted"}'
expect "both results from the cache" "$(from_cache)" '007.sh 011.sh'
expect "each named so in the log" "$(cat "$work/gate.out")" '| == base: 007.sh: unsolved (cached)'
refuse "and nothing built on the base" "$(cat "$work/gate.out")" '== base: build'
expect "the log says where the time went" "$(cat "$work/gate.out")" \
    '| gate time: base 0 s (0 scenario runs, 2 results from the cache), candidate '
expect "the log names the toolchain" "$(cat "$work/gate.out")" '| base cache: toolchain rustc '

printf '# changed\n' >>"$controller/scripts/blackbox/runner.sh"
judge "$good" "" "$keyed" >/dev/null
expect "a controller file no scenario names, or names in a comment only, changes no key" \
    "$(from_cache)" '007.sh 011.sh'

printf '# changed\n' >>"$controller/scripts/blackbox/lib.sh"
judge "$good" "" "$keyed" >/dev/null
expect "a miss: a helper the scenario sources changed" "$(cat "$work/gate.out")" \
    'from the cache: 007.sh; run on the base: 011.sh'

printf '# changed\n' >>"$keyed/007.sh"
judge "$good" "" "$keyed" >/dev/null
expect "a miss: the scenario's blob changed" "$(cat "$work/gate.out")" \
    'from the cache: 011.sh; run on the base: 007.sh'

printf '# changed\n' >>"$keyed/build"
judge "$good" "" "$keyed" >/dev/null
expect "a miss: the build hook changed" "$(cat "$work/gate.out")" \
    'from the cache: none; run on the base: 007.sh 011.sh'

git -C "$seed" checkout -q -B other-base "$base"
printf 'unrelated\n' >"$seed/unrelated"
other_base=$(commit other-base)
saved_base=$base
base=$other_base
judge "$good" "" "$keyed" >/dev/null
base=$saved_base
expect "a miss: another base commit" "$(cat "$work/gate.out")" \
    'from the cache: none; run on the base: 007.sh 011.sh'

gate_copy=$work/gate-copy/scripts
# The gate refuses a tree under the host's TMPDIR (the sandbox empties that
# directory whole), and $work is under the suite's TMPDIR: the copy is run
# with a TMPDIR of its own, beside it, so the copy is not inside it.
gate_tmp=$work/gate-tmp
mkdir -p "$gate_copy/rsi" "$gate_tmp"
saved_tmpdir=${TMPDIR-}
TMPDIR=$gate_tmp
export TMPDIR
cp "$(dirname "$gate")"/../*.sh "$gate_copy/"
cp "$gate" "$(dirname "$gate")/gate-pg.sh" "$gate_copy/rsi/"
# The suite runs from a deployed controller too, a read-only tree whose
# modes cp carries over: the copy is the suite's own, so it is made writable.
chmod -R u+w "$gate_copy"
printf '# changed\n' >>"$gate_copy/rsi/gate.sh"
saved_gate=$gate
gate=$gate_copy/rsi/gate.sh
judge "$good" "" "$keyed" >/dev/null
gate=$saved_gate
expect "a miss: gate.sh's blob changed" "$(cat "$work/gate.out")" \
    'from the cache: none; run on the base: 007.sh 011.sh'
cp "$gate" "$gate_copy/rsi/gate.sh"
printf '# changed\n' >>"$gate_copy/rsi/gate-pg.sh"
gate=$gate_copy/rsi/gate.sh
judge "$good" "" "$keyed" >/dev/null
gate=$saved_gate
expect "a miss: the private server's script (gate-pg.sh) changed" "$(cat "$work/gate.out")" \
    'from the cache: none; run on the base: 007.sh 011.sh'
TMPDIR=$saved_tmpdir
export TMPDIR

# Another toolchain: a rustc that says another version, first on PATH.
mkdir -p "$work/other-rust"
printf '#!/bin/sh\necho "rustc 0.0.1-other (test)"\necho "release: 0.0.1-other"\n' >"$work/other-rust/rustc"
chmod +x "$work/other-rust/rustc"
saved_path=$PATH
PATH=$work/other-rust:$PATH
other_toolchain=$(judge "$good" "" "$keyed")
PATH=$saved_path
expect "a miss: another toolchain" "$other_toolchain" 'exit=0 {"verdict":"accepted"}'
expect "run on the base" "$(cat "$work/gate.out")" 'from the cache: none; run on the base: 007.sh 011.sh'
expect "and the log names it" "$(cat "$work/gate.out")" 'toolchain rustc 0.0.1-other'
judge "$good" "" "$keyed" >/dev/null
expect "the toolchain back: cached again" "$(from_cache)" '007.sh 011.sh'

# The test server's version and the other tools the scenarios run are in
# the key: a psql first on PATH that answers `-V` and the server's
# `server_version_num` (asked through libpq's variables, never a URL on its
# command line) from a file beside it.
mkdir -p "$work/fake-pg"
cat >"$work/fake-pg/psql" <<'EOF'
#!/bin/sh
here=$(dirname "$0")
case "$*" in
-V) cat "$here/client" ;;
*"server_version_num"*)
    [ -n "${PGHOST-}" ] && [ -n "${PGDATABASE-}" ] || { echo "no libpq variables" >&2; exit 2; }
    case "$*" in *postgres:*) echo "a URL on the command line" >&2; exit 2 ;; esac
    cat "$here/server" ;;
*) exit 2 ;;
esac
EOF
chmod +x "$work/fake-pg/psql"
echo 'psql (PostgreSQL) 17.0' >"$work/fake-pg/client"
echo 170000 >"$work/fake-pg/server"
with_fake_pg() {
    saved_path=$PATH
    PATH=$work/fake-pg:$PATH
    fake_pg_out=$(judge "$good" "" "${1:-$keyed}")
    PATH=$saved_path
}
with_fake_pg
expect "the log names the test server's version" "$(cat "$work/gate.out")" 'psql 17.0 server 170000'
with_fake_pg
expect "the same server: a hit" "$(from_cache)" '007.sh 011.sh'
echo 170004 >"$work/fake-pg/server"
with_fake_pg
expect "a miss: the test server's version changed" "$fake_pg_out" 'exit=0 {"verdict":"accepted"}'
expect "both run on the base" "$(cat "$work/gate.out")" 'from the cache: none; run on the base: 007.sh 011.sh'
echo 'psql (PostgreSQL) 17.1' >"$work/fake-pg/client"
with_fake_pg
expect "a miss: another psql" "$(cat "$work/gate.out")" 'from the cache: none; run on the base: 007.sh 011.sh'

# A hit touches its entry, so routine cleanup keeps the entries in use.
for entry in "$cache"/*; do touch -d '20 days ago' "$entry"; done
judge "$good" "" "$keyed" >/dev/null
expect "a hit" "$(from_cache)" '007.sh 011.sh'
expect "touches the two entries it used" "$(find "$cache" -type f -newermt '-1 hour' | grep -c .)" '2'

# A helper reached indirectly (a variable after scripts/, or a cd into
# scripts/ and a relative path) cannot be read off the text: all of the
# controller's scripts/ is in the key.
indirect=$HOME/indirect
mkdir -p "$indirect/rsi/scenarios" "$indirect/scripts/blackbox"
printf '# helper v1\n' >"$indirect/scripts/blackbox/lib.sh"
cp "$scenarios/protected" "$scenarios/build" "$indirect/rsi/scenarios/"
cat >"$indirect/rsi/scenarios/021.sh" <<'EOF'
#!/bin/sh
h=blackbox/lib.sh
. "$(dirname "$0")/../../scripts/$h"
[ "$(cat value)" != three ]
EOF
cat >"$indirect/rsi/scenarios/022.sh" <<'EOF'
#!/bin/sh
(cd "$(dirname "$0")/../../scripts" && . ./blackbox/lib.sh) || exit 2
[ "$(cat value)" != three ]
EOF
judge "$good" "" "$indirect/rsi/scenarios" >/dev/null
judge "$good" "" "$indirect/rsi/scenarios" >/dev/null
expect "indirect helpers: a hit while nothing changed" "$(from_cache)" '021.sh 022.sh'
printf '# helper v2\n' >"$indirect/scripts/blackbox/lib.sh"
judge "$good" "" "$indirect/rsi/scenarios" >/dev/null
expect "a miss: the helper reached through scripts/\$h or a cd changed" "$(cat "$work/gate.out")" \
    'from the cache: none; run on the base: 021.sh 022.sh'

# What is installed in a directory the `bind` list names decides base
# results (a project's node_modules), so it is in the key: a listing of the
# directory, or pnpm's install record when it has one.
bound=$HOME/bound-inputs
mkdir -p "$bound/lib" "$HOME/bound-ctl/rsi/scenarios"
echo v1 >"$bound/lib/answer"
cp "$scenarios/protected" "$scenarios/build" "$HOME/bound-ctl/rsi/scenarios/"
printf '%s\n' "$bound" >"$HOME/bound-ctl/rsi/scenarios/bind"
cat >"$HOME/bound-ctl/rsi/scenarios/031.sh" <<EOF
#!/bin/sh
[ -f '$bound/lib/answer' ] || exit 2
[ "\$(cat value)" != three ]
EOF
bound_scenarios=$HOME/bound-ctl/rsi/scenarios
judge "$good" "" "$bound_scenarios" >/dev/null
judge "$good" "" "$bound_scenarios" >/dev/null
expect "a bound directory unchanged: a hit" "$(from_cache)" '031.sh'
echo v2-longer >"$bound/lib/answer"
judge "$good" "" "$bound_scenarios" >/dev/null
expect "a miss: a file in a bound directory changed" "$(cat "$work/gate.out")" \
    'from the cache: none; run on the base: 031.sh'
mkdir -p "$bound/.pnpm"
printf 'lockfileVersion: 9\n' >"$bound/.pnpm/lock.yaml"
printf 'prunedAt: one\n' >"$bound/.modules.yaml"
judge "$good" "" "$bound_scenarios" >/dev/null
expect "a miss: the directory now has pnpm's install record" "$(cat "$work/gate.out")" \
    'from the cache: none; run on the base: 031.sh'
judge "$good" "" "$bound_scenarios" >/dev/null
expect "with the record: a hit" "$(from_cache)" '031.sh'
echo v3 >"$bound/lib/answer"
judge "$good" "" "$bound_scenarios" >/dev/null
expect "an edit without an install is not keyed (the record stands for it)" "$(from_cache)" '031.sh'
printf 'prunedAt: two\n' >"$bound/.modules.yaml"
judge "$good" "" "$bound_scenarios" >/dev/null
expect "a miss: an install rewrote the record" "$(cat "$work/gate.out")" \
    'from the cache: none; run on the base: 031.sh'
printf 'lockfileVersion: 9\npackages: {}\n' >"$bound/.pnpm/lock.yaml"
judge "$good" "" "$bound_scenarios" >/dev/null
expect "a miss: the installed lockfile changed" "$(cat "$work/gate.out")" \
    'from the cache: none; run on the base: 031.sh'

# A corrupt entry is a miss: whatever an entry holds but "solved" or
# "unsolved", an entry that cannot be read, or one that is a directory, is
# judged again, and rewritten.
for entry in "$cache"/*; do printf 'solved, trust me\n' >"$entry"; done
expect "corrupt entries: the verdict is the base's own" "$(judge "$good" "" "$keyed")" \
    'exit=0 {"verdict":"accepted"}'
expect "each judged again" "$(cat "$work/gate.out")" '| == base: 007.sh: its cache entry cannot be read; judged again'
expect "on the base" "$(cat "$work/gate.out")" 'from the cache: none; run on the base: 007.sh 011.sh'
judge "$good" "" "$keyed" >/dev/null
expect "and rewritten" "$(from_cache)" '007.sh 011.sh'
for entry in "$cache"/*; do chmod 000 "$entry"; done
judge "$good" "" "$keyed" >/dev/null
expect "unreadable entries are misses" "$(cat "$work/gate.out")" \
    'from the cache: none; run on the base: 007.sh 011.sh'
for entry in "$cache"/*; do
    chmod 600 "$entry" 2>/dev/null || true
    rm -rf "$entry"
    mkdir "$entry"
done
judge "$good" "" "$keyed" >/dev/null
expect "an entry that is a directory is a miss" "$(cat "$work/gate.out")" \
    'from the cache: none; run on the base: 007.sh 011.sh'
judge "$good" "" "$keyed" >/dev/null
expect "and is replaced" "$(from_cache)" '007.sh 011.sh'
chmod -R u+rwx "$cache"

# A base cache the sandbox would show again (under a directory it binds) is
# refused before anything runs.
exposed=$work/exposed
cp -r "$objectives" "$exposed"
expect "a base cache under the scenarios directory is refused" \
    "$(export XDG_CACHE_HOME="$exposed/cache"; judge "$good" "" "$exposed")" 'exit=3 '
expect "and says why" "$(cat "$work/gate.out")" 'which the sandbox shows'

# A harness error on the base aborts the gate: no verdict, never a skip.
server break >/dev/null
printf '# changed again, so not cached\n' >>"$objectives/011.sh"
expect "a harness exit on the base is a host fault, no verdict" "$(judge "$good" "" "$objectives")" \
    'exit=6 {"host_fault":true,"scenario":"011.sh","on":"base","runs":"2"'
refuse "with no verdict" "$(cat "$integrator/verdict" 2>/dev/null || true)" '"verdict"'
expect "and says why" "$(cat "$work/gate.out")" '011.sh exited 2 on the base: a harness error'
server fix >/dev/null

# The task scenario's red run: exactly 1 on the base, twice.
task_scenarios=$work/task-objectives
cp -r "$objectives" "$task_scenarios"
rm "$task_scenarios/011.sh"
printf '#!/bin/sh\n[ "$(cat value)" = two ] || exit 7\n' >"$task_scenarios/012.sh"
expect "a task scenario that exits 7 on the base is a host fault" \
    "$(judge "$good" "$task_scenarios/012.sh" "$task_scenarios")" 'exit=6 {"host_fault":true,"scenario":"012.sh","on":"base","runs":"7"'
cat >"$task_scenarios/012.sh" <<'EOF'
#!/bin/sh
# Flaky on the base: fails the first time, passes the second.
[ "$(cat value)" = two ] && exit 0
[ "$(sh "$(dirname "$0")/server-client" count)" -ge 2 ] && exit 0
exit 1
EOF
server reset >/dev/null
# Objective 033's disagreement keeps its own statement: exit 4 and the pair,
# never a host-fault fact (one run is one record; gate.rs tells them apart).
expect "an unsolved result needs two runs that agree" \
    "$(judge "$good" "$task_scenarios/012.sh" "$task_scenarios")" 'exit=4 '
refuse "and no host-fault fact is written" "$(cat "$integrator/verdict" 2>/dev/null || true)" 'host_fault'
expect "the base's disagreement is said as 033 says it" "$(cat "$work/gate.out")" \
    '012.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts'
expect "the flake is named" "$(cat "$work/gate.out")" 'exited 1, then 0 on the base'

# A scenario the host cannot judge here says so with exit 6 (029: no systemd
# user manager in the sandbox) and its own fact: a fact about the host, never
# a verdict (the host-fault fact's exit, the other key). On the base it is
# cached as `cannot_judge`, one run (the host's answer does not change); on
# the candidate a regression is skipped, and the task scenario makes the gate
# state the fact (exit 6, no verdict) rather than reject.
unjudgeable=$work/unjudgeable
mkdir -p "$unjudgeable"
printf 'judge/\n' >"$unjudgeable/protected"
cat >"$unjudgeable/010.sh" <<'JUDGE'
#!/bin/sh
echo "010: cannot be judged here: no systemd user manager is reachable" >&2
exit 6
JUDGE
expect "a regression that cannot be judged here is skipped" \
    "$(judge "$good" "" "$unjudgeable")" 'exit=0 {"verdict":"accepted"}'
expect "and says so, not a regression" "$(cat "$work/gate.out")" \
    '| 010.sh cannot be judged here: not a regression'
expect "the base records the fact" "$(cat "$work/gate.out")" '| == base: 010.sh: cannot_judge'
judge "$good" "" "$unjudgeable" >/dev/null
expect "the fact is cached: one run on the base" "$(from_cache)" '010.sh'
# -s: "an entry that is a directory is a miss" (above) left every entry and
# every entry's .lock a directory, and grep named each one on stderr: about
# 83 "Is a directory" lines in every suite log.
expect "and kept as cannot_judge" "$(grep -ls '^cannot_judge$' "$cache"/* | wc -l)" '1'
unjudged_task=$work/unjudged-task
mkdir -p "$unjudged_task"
printf 'judge/\n' >"$unjudged_task/protected"
cat >"$unjudged_task/012.sh" <<'JUDGE'
#!/bin/sh
echo "012: cannot be judged here: no systemd user manager is reachable" >&2
exit 6
JUDGE
expect "a task scenario that cannot be judged here is no verdict" \
    "$(judge "$good" "$unjudged_task/012.sh" "$unjudged_task")" 'exit=6 '
expect "the gate states the fact" "$(cat "$integrator/verdict")" \
    '{"cannot_judge":true,"scenario":"012.sh","runs":"6"'
refuse "never a verdict" "$(cat "$integrator/verdict")" 'verdict'
unset SPAWN_TEST_DATABASE_URL

# 10. The URL parser the gate and the checks share (scripts/pg-url.sh):
# `parsed <url>` prints what psql gets, `user|password|host|port|db|options`.
# shellcheck source=scripts/pg-url.sh
. "$(dirname "$gate")/../pg-url.sh"
parsed() {
    if pg_url_parse "$1"; then
        printf '%s|%s|%s|%s|%s|%s' "$pg_url_user" "$pg_url_password" "$pg_url_host" "$pg_url_port" \
            "$pg_url_database" "$pg_url_options"
    else
        printf 'not a URL'
    fi
}
expect "a socket URL, the password percent-decoded" "$(parsed 'postgres://u:p%40ss@%2Ftmp%2Fs/db')" \
    'u|p@ss|/tmp/s|5432|db|'
expect "a socket URL with a port" "$(parsed 'postgres://u:p%40ss@%2Ftmp%2Fs:5433/db')" 'u|p@ss|/tmp/s|5433|db|'
expect "localhost, the other parameters kept" \
    "$(parsed 'postgresql://u:pw@localhost:5434/spawn_test?sslmode=disable&application_name=t')" \
    'u|pw|localhost|5434|spawn_test|sslmode=disable&application_name=t'
expect "an IPv6 literal with a port" "$(parsed 'postgres://u:pw@[::1]:5435/x')" 'u|pw|::1|5435|x|'
expect "an IPv6 literal without one" "$(parsed 'postgres://u:pw@[::1]/x')" 'u|pw|::1|5432|x|'
expect "host and port as parameters" "$(parsed 'postgres://u:pw@/x?host=%2Frun%2Fpg&port=5436')" \
    'u|pw|/run/pg|5436|x|'
expect "a host parameter not encoded" "$(parsed 'postgres://u:pw@/x?host=/run/pg')" 'u|pw|/run/pg|5432|x|'
expect "no password" "$(parsed 'postgres://u@db.example:6543/x')" 'u||db.example|6543|x|'
expect "an encoded colon and percent" "$(parsed 'postgres://u:a%3Ab%25c@h/x')" 'u|a:b%c|h|5432|x|'
expect "a raw @ in the password: the host is after the last" "$(parsed 'postgres://u:p@ss@h/x')" 'u|p@ss|h|5432|x|'
expect "a list of hosts is refused" "$(parsed 'postgres://u@a,b/x')" 'not a URL'
expect "another scheme is refused" "$(parsed 'mysql://u@h/x')" 'not a URL'
expect "the server URL puts host and port in the authority" \
    "$(pg_url_server 'postgres://u:p%40ss@/x?host=/run/pg&port=5436')" 'postgres://u:p%40ss@%2Frun%2Fpg:5436'
expect "the server URL of an IPv6 host keeps the brackets" \
    "$(pg_url_server 'postgres://u:pw@[::1]:5435/x')" 'postgres://u:pw@[::1]:5435'
expect "psql gets the decoded password" \
    "$(pg_url_export 'postgres://u:p%40ss@%2Ftmp%2Fs/db' && printf '%s %s %s %s' "$PGUSER" "$PGPASSWORD" "$PGHOST" "$PGPORT")" \
    'u p@ss /tmp/s 5432'
refuse "no password, no PGPASSWORD" \
    "$(PGPASSWORD=stale; export PGPASSWORD; pg_url_export 'postgres://u@h/x'; printf '%s' "${PGPASSWORD-unset}")" 'stale'

# 11. The changed paths are read as they are (no rename detection, no
# quoting): a protected file moved away, a protected or build.rs path with a
# quote or a non-ASCII name, and a name with a newline are all refused, and
# a scenarios directory without a protected list judges nothing.
branch renamed-protected
printf 'two\n' >"$seed/value"
mkdir -p "$seed/elsewhere"
git -C "$seed" mv judge/rule elsewhere/rule
renamed=$(commit renamed-protected)
expect "a protected file moved away is refused" "$(judge "$renamed")" 'touches protected paths'
branch quoted-protected
printf 'two\n' >"$seed/value"
printf 'x\n' >"$seed/judge/é.sh"
printf 'x\n' >"$seed/judge/a\"b.sh"
quoted=$(commit quoted-protected)
expect "a protected path with a non-ASCII name or a quote is refused" "$(judge "$quoted")" 'touches protected paths'
branch quoted-buildrs
printf 'two\n' >"$seed/value"
mkdir -p "$seed/crates/é"
printf 'fn main() {}\n' >"$seed/crates/é/build.rs"
quoted_build=$(commit quoted-buildrs)
expect "a build.rs under a non-ASCII directory is refused" "$(judge "$quoted_build")" 'build-time'
branch newline-path
printf 'two\n' >"$seed/value"
printf 'x\n' >"$seed/new
line"
newline=$(commit newline-path)
expect "a path with a newline is refused" "$(judge "$newline")" 'a changed path holds a newline'
unprotected_scenarios=$work/unprotected
cp -r "$scenarios" "$unprotected_scenarios"
rm "$unprotected_scenarios/protected"
expect "a scenarios directory without a protected list judges nothing" \
    "$(judge "$good" "" "$unprotected_scenarios")" 'no protected list'

# 12. No committed node_modules/ or target/ anywhere: git overwrites an
# ignored file with a committed one, and the owner's build runs them.
for planted_path in web/node_modules/.bin/tsc target/debug/spawn crates/lib/target/x; do
    branch "planted-${planted_path%%/*}"
    printf 'two\n' >"$seed/value"
    mkdir -p "$seed/$(dirname "$planted_path")"
    printf '#!/bin/sh\n' >"$seed/$planted_path"
    planted=$(commit "planted-${planted_path%%/*}")
    expect "a committed $planted_path is refused" "$(judge "$planted")" 'node_modules/ or target/'
done

# 13. The controller's list protects web/ but web/src/ and web/index.html
# (an exception line, `!<prefix>`): a new
# vite.config.js (loaded before vite.config.ts) or postcss.config.js would
# run as the owner at deploy.
for web_path in web/vite.config.js web/postcss.config.js web/.postcssrc.json; do
    branch "web-${web_path#web/}"
    printf 'two\n' >"$seed/value"
    mkdir -p "$seed/web"
    printf 'export default {}\n' >"$seed/$web_path"
    web_config=$(commit "web-${web_path#web/}")
    expect "a new $web_path is refused" "$(judge "$web_config" "$listed/007.sh" "$listed")" 'candidate touches protected paths'
done
branch web-src
printf 'two\n' >"$seed/value"
mkdir -p "$seed/web/src"
printf 'export const x = 1;\n' >"$seed/web/src/app.ts"
printf '<!doctype html>\n' >"$seed/web/index.html"
web_src=$(commit web-src)
expect "web/src/ and web/index.html may change" "$(judge "$web_src" "$listed/007.sh" "$listed")" 'exit=0 {"verdict":"accepted"}'

# 14. A proc-macro crate outside the workspace (a path dependency the
# workspace does not include) runs at compile time too: refused when new or
# changed; one the base already had, untouched, is not.
macro_crate() {
    mkdir -p "$seed/evil/src"
    printf '[package]\nname = "evil"\nversion = "0.1.0"\nedition = "2021"\n\n[lib]\nproc-macro = true\n' \
        >"$seed/evil/Cargo.toml"
    printf 'extern crate proc_macro;\nuse proc_macro::TokenStream;\n\n/// Echo.\n#[proc_macro]\npub fn echo(input: TokenStream) -> TokenStream {\n    input\n}\n' \
        >"$seed/evil/src/lib.rs"
    printf '\n[dependencies]\nevil = { path = "evil" }\n' >>"$seed/Cargo.toml"
    (cd "$seed" && CARGO_NET_OFFLINE=true cargo generate-lockfile -q)
}
rust_branch rust-macro
macro_crate
macro=$(commit rust-macro)
expect "a new proc-macro crate outside the workspace is refused" "$(judge_rust "$macro")" \
    'candidate adds build-time code (a local proc-macro or build script outside the workspace: evil)'
saved_rust_base=$rust_base
rust_base=$macro
rust_branch rust-macro-kept
printf 'a change outside the macro crate\n' >"$seed/notes.txt"
macro_kept=$(commit rust-macro-kept)
expect "one the base had, untouched, is not" "$(judge_rust "$macro_kept")" 'exit=0 {"verdict":"accepted"}'
rust_branch rust-macro-edited
printf '\n/// Another.\n#[proc_macro]\npub fn other(input: TokenStream) -> TokenStream {\n    input\n}\n' \
    >>"$seed/evil/src/lib.rs"
macro_edited=$(commit rust-macro-edited)
expect "one the candidate edits is refused" "$(judge_rust "$macro_edited")" \
    'a local proc-macro or build script outside the workspace: evil'
rust_base=$saved_rust_base

# 14b. A dependency graph that does not resolve offline: a candidate's
# whose base resolves here is rejected; one whose base does not resolve
# either is the host's registry lacking a crate (a runner never sent it), a
# fact about the host (exit 6, `cannot_judge`), never a rejection of the
# work. The crate is one no registry here has; the lockfile is not updated.
rust_branch rust-unresolved
printf '\n[dependencies]\nno-such-crate-anywhere = "1"\n' >>"$seed/Cargo.toml"
unresolved=$(commit rust-unresolved)
expect "a candidate whose base resolves is rejected for its graph" "$(judge_rust "$unresolved")" \
    '{"verdict":"rejected","reason":"candidate'"'"'s dependency graph does not resolve offline"}'
saved_rust_base=$rust_base
rust_base=$unresolved
rust_branch rust-unresolved-base
printf 'a change on a base that does not resolve\n' >"$seed/notes.txt"
on_unresolved=$(commit rust-unresolved-base)
expect "one whose base does not resolve either cannot be judged here" "$(judge_rust "$on_unresolved")" \
    'exit=6 {"cannot_judge":true,"scenario":"dependencies","runs":"1"}'
refuse "and is no verdict" "$(cat "$integrator/verdict")" '"verdict"'
expect "the gate says why, for the integrator's record" "$(cat "$work/gate.out")" \
    '| dependencies: cannot judge: the base'"'"'s dependency graph does not resolve offline on this host either'
rust_base=$saved_rust_base

# 6. A reason with a quote stays valid JSON.
quoted_scenarios=$work/quoted
cp -r "$scenarios" "$quoted_scenarios"
# It fails on the candidate (value two) and passes on the base (one): a
# regression, whose reason holds the quote.
printf '#!/bin/sh\n[ "$(cat value)" = one ]\n' >"$quoted_scenarios/check-\"q\".sh"
expect "a quote in a reason is escaped" "$(judge "$good" "" "$quoted_scenarios")" \
    '{"verdict":"rejected","reason":"regression: check-\"q\".sh fails on the candidate"}'

# 15. `<scenarios-dir>/bind` names the owner's read-only inputs under $HOME
# (a project's installed dependencies): visible in the sandbox only when
# listed; a listed directory that is missing stops the gate (exit 3).
owner_inputs=$(mktemp -d "$HOME/.cache/spawn-gate-test-bind.XXXXXX")
printf 'here\n' >"$owner_inputs/marker"
unlisted_scenarios=$work/unlisted
cp -r "$scenarios" "$unlisted_scenarios"
printf '#!/bin/sh\n[ "$(cat "%s/marker" 2>/dev/null)" = here ]\n' "$owner_inputs" \
    >"$unlisted_scenarios/check-owner-inputs.sh"
# Unseen on the base as on the candidate: the check fails on both, which the
# gate takes for a harness fault (no verdict, exit 6; it prints no FAILED
# lines, so it is no red main).
expect "an unlisted directory under \$HOME is not visible" \
    "$(judge "$good" "" "$unlisted_scenarios")" 'exit=6 {"host_fault":true,"scenario":"check-owner-inputs.sh"'
expect "on the candidate or the base" "$(cat "$work/gate.out")" \
    'check-owner-inputs.sh fails on the base too (1, then 1)'
listed_inputs=$work/listed-inputs
cp -r "$unlisted_scenarios" "$listed_inputs"
printf '# owner inputs\n%s\n' "$owner_inputs" >"$listed_inputs/bind"
expect "a directory listed in bind is visible" "$(judge "$good" "" "$listed_inputs")" \
    'exit=0 {"verdict":"accepted"}'
printf '%s/gone\n' "$owner_inputs" >"$listed_inputs/bind"
expect "a listed directory that is missing stops the gate" \
    "$(judge "$good" "" "$listed_inputs")" 'exit=3'
rm -rf "$owner_inputs"

# 16. Parallel runs (SPAWN_GATE_JOBS): the base's runs, then the
# candidate's, N at a time, each block printed whole in the scenario order.
# with_jobs <n> <cache name> <candidate> <scenarios> [<scenario>]: judge with
# N at a time and a base cache of its own (so the base runs).
with_jobs() {
    (
        export SPAWN_GATE_JOBS="$1" XDG_CACHE_HOME="$work/caches/$2"
        judge "$3" "${5-}" "$4"
    )
}
# normalized: the gate's log without its times and its parallelism.
normalized() {
    # Times, and the sockets the host has open (other processes' too), and
    # the host's scenario budget: it is SPAWN_GATE_JOBS, which differs here
    # (1 against 4), and a wait for a slot is the host's, not the log's.
    # The lines are dropped before the times are masked: masking first made
    # "waited 0.4 s" into "waited 0.N s", which the drop no longer matched,
    # so a wait for a slot (another gate on the host) failed the comparison.
    # A wait of a minute or more is said in minutes (spoken_wait). normalized [<log>].
    sed -E -e '/^\| (base|candidate) runs: /d' \
        -e '/^\| gate time: /d' -e '/^\| sockets hidden from the sandbox: /d' \
        -e '/^\| host scenario slots: /d' -e '/^\| [a-z]+ start order /d' \
        -e '/^\| waited [0-9.]+ (s|min) for host scenario slot /d' \
        -e '/^\| [a-z]+\.[0-9]+: (still )?waiting for a host scenario slot /d' \
        -e 's/[0-9]+ms/Nms/g' -e 's/[0-9]+ s\b/N s/g' "${1:-$work/gate.out}"
}
printf '%s\n' '| waited 0.4 s for host scenario slot 4 of 4 (shared by every gate on this host)' \
    '| waited 2.1 min for host scenario slot 1 of 2 (shared by every gate on this host)' \
    '| == base: 051.sh' >"$work/waits.log"
expect "a wait for a host slot is not the log's, in seconds or in minutes" \
    "$(normalized "$work/waits.log" | tr '\n' '/' | sed 's%^| == base: 051.sh/$%dropped%')" dropped
# own_and_wall <phase>: the wall clock and the runs' own time the log names.
wall_of() { sed -n "s/^| $1 runs: [0-9]* in \([0-9]*\) s, .*/\1/p" "$work/gate.out"; }
own_of() { sed -n "s/^| $1 runs: .*their own time: \([0-9]*\) s)$/\1/p" "$work/gate.out"; }
par=$work/parallel
mkdir -p "$par"
cp "$scenarios/protected" "$scenarios/build" "$par/"
# 051 is the slowest and 054 the fastest: with four at a time they end in
# the reverse of their order.
for n in 1 2 3 4; do
    printf '#!/bin/sh\necho "05%s starts"\nsleep %s\necho "05%s ends"\n' "$n" "$((4 - n))" "$n" >"$par/05$n.sh"
done
expect "one at a time" "$(with_jobs 1 par1 "$good" "$par")" 'exit=0 {"verdict":"accepted"}'
normalized >"$work/serial.log"
expect "four at a time: the same verdict" "$(with_jobs 4 par4 "$good" "$par")" 'exit=0 {"verdict":"accepted"}'
normalized >"$work/parallel.log"
expect "and the same log, block for block, in the scenario order" \
    "$(diff "$work/serial.log" "$work/parallel.log" && echo same)" same
expect "each block whole: its header, its output, its result" \
    "$(grep -A3 '^| == base: 051.sh$' "$work/gate.out" | tr '\n' '/')" \
    '| == base: 051.sh/| 051 starts/| 051 ends/| == base: 051.sh: solved/'
expect "the runs overlapped: the base's wall clock is under their own time" \
    "$([ "$(wall_of base)" -lt "$(own_of base)" ] && echo overlapped)" overlapped
expect "and the candidate's" "$([ "$(wall_of candidate)" -lt "$(own_of candidate)" ] && echo overlapped)" overlapped
expect "the log says how many ran at once" "$(cat "$work/gate.out")" '| base runs: 4 in '
# The gate's time line: setup is its own measure (the time before the first
# stage), never the total less the phases, which overlap ("setup -416 s");
# and no phase is said longer than the gate. gate_time_ok [<log>].
gate_time_ok() {
    sed -n 's/^| gate time: base \([0-9]*\) s .*candidate \([0-9-]*\) s, setup \([0-9-]*\) s .*total \([0-9]*\) s$/\1 \2 \3 \4/p' \
        "${1:-$work/gate.out}" | {
        read -r gt_base gt_candidate gt_setup gt_total || {
            echo "no gate time line"
            return 0
        }
        if [ "$gt_setup" -ge 0 ] && [ $((gt_base + gt_candidate)) -le $((gt_total + 1)) ]; then
            echo fine
        else
            echo "base $gt_base, candidate $gt_candidate, setup $gt_setup, total $gt_total"
        fi
    }
}
expect "four at a time: setup is not negative, and base and candidate fit in the total" \
    "$(gate_time_ok)" fine
# A gate killed while its candidate runs overlap (two at a time, the slow
# one last, so five fast ones are printed first, 10 s of their own time in about 6 s): the open phase is its wall
# clock so far, not its runs' own time added up.
slow=$work/slow-par
mkdir -p "$slow"
cp "$scenarios/protected" "$scenarios/build" "$slow/"
for n in 1 2 3 4 5; do
    printf '#!/bin/sh\necho "06%s"\nsleep 2\n' "$n" >"$slow/06$n.sh"
done
printf '#!/bin/sh\necho "066"\nsleep 120\n' >"$slow/066.sh"
killed=$work/killed-gate.out
rm -rf "$integrator"
mkdir -m 700 "$integrator"
(
    export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$work/caches/slow-killed" SPAWN_GATE_VERDICT="$integrator/verdict"
    exec sh "$gate" "$repo" "$base" "$good" "$slow"
) >"$killed" 2>&1 &
killed_pid=$!
killed_wait=0
until grep -q '^| == regression: 065.sh' "$killed" 2>/dev/null; do
    killed_wait=$((killed_wait + 1))
    if [ "$killed_wait" -gt 3000 ]; then
        echo "FAIL the slow gate never printed its candidate's fifth run"
        kill -TERM "$killed_pid" 2>/dev/null || :
        exit 1
    fi
    sleep 0.1
done
kill -TERM "$killed_pid"
wait "$killed_pid" || :
expect "killed inside its candidate phase: the gate still says its time" "$(cat "$killed")" '| gate time: '
expect "with setup not negative, and base and candidate within the total" "$(gate_time_ok "$killed")" fine
expect "the open phase is said as it stood" "$(cat "$killed")" '| candidate runs: '
expect "the gate time line counts the runs" "$(cat "$work/gate.out")" '(4 scenario runs, 0 results from the cache)'
expect "a bad SPAWN_GATE_JOBS is no verdict" "$(with_jobs 0 bad "$good" "$par")" 'exit=3 '
expect "and says why" "$(cat "$work/gate.out")" 'SPAWN_GATE_JOBS is a whole number from 1'

# gone_within <seconds> <text>: the processes whose command line holds
# <text>, once none is left or <seconds> are up (a run the gate stopped
# can take a moment to go; one it left lives far longer than the poll).
gone_within() {
    gw_n=0
    while ps -eo args | grep -F "$2" | grep -qv grep && [ "$gw_n" -lt $(($1 * 10)) ]; do
        sleep 0.1
        gw_n=$((gw_n + 1))
    done
    ps -eo args | grep -F "$2" | grep -v grep || true
}
# 16b. A failing scenario among passing ones: the first failure in the
# scenario order is the verdict, whichever ended first, and the runs after
# it stop. 062 fails slowly, 063 at once, 064 would run for 120 s, far
# longer than any load stretches the rest (the bounds below are 100 s and
# 60 s: 29 s, the old length, left a load-45 gate 4 s of slack).
pfail=$work/parallel-fail
mkdir -p "$pfail"
cp "$scenarios/protected" "$scenarios/build" "$pfail/"
on_candidate() { # <name> <the candidate's commands>: passes on the base
    printf '#!/bin/sh\n[ "$(cat value)" = one ] && exit 0\n%s\n' "$2" >"$pfail/$1"
}
on_candidate 061.sh 'sleep 2'
on_candidate 062.sh 'sleep 3; echo "062 failed"; exit 1'
on_candidate 063.sh 'echo "063 failed"; exit 1'
on_candidate 064.sh 'sleep 120.25'
started=$(date +%s)
expect "the first failure in order is the verdict, not the first to end" \
    "$(with_jobs 4 pfail "$good" "$pfail")" '"reason":"regression: 062.sh fails on the candidate"'
outer=$(($(date +%s) - started))
# The candidate phase's own wall clock, not the gate's: its setup (the
# exports, the socket scan, the build) took 22 s of a 26 s gate at load 17
# inside another gate's sandbox (check-test-gate, 2026-11-09), while its
# runs took 3.
expect "the verdict came at the failure, not after the runs after it" \
    "$([ "$(wall_of candidate)" -lt 100 ] && echo early)" early
# That clock and the gate's total close at the verdict; the runs after it
# are stopped later, on the gate's exit. So the time from the verdict to
# the exit is the outer clock less the total: about 1 s when 064 is
# stopped, 117 s more when it is waited for (rev-b, F1).
gate_total=$(sed -n 's/^| gate time: .*total \([0-9]*\) s$/\1/p' "$work/gate.out")
expect "the runs after it were stopped, not waited for" \
    "$([ -n "$gate_total" ] && [ $((outer - gate_total)) -lt 60 ] && echo early) (outer $outer s, gate total ${gate_total:-none} s)" early
refuse "and their blocks are not printed" "$(cat "$work/gate.out")" '== regression: 063.sh'
refuse "nor is anything after them" "$(cat "$work/gate.out")" '== regression: 064.sh'
expect "the blocks before it are" "$(cat "$work/gate.out")" '| == regression: 061.sh must still pass'
refuse "no run of the gate is left" "$(gone_within 30 'sleep 120.25')" 'sleep'

# 16c. A scenario on the serial list runs alone: each scenario marks itself
# running in the tree, and one that sees a neighbour it must not have exits 2
# (a harness error on the base: no verdict). 072 and 073 meet: each leaves a
# file, met.<its number>, it never removes and waits, up to 60 s, for the
# other's, so
# two runs the gate starts side by side overlap whatever the host's load
# (rev-e: at load 12.8 on ce03a441 they did not), and two it runs one after
# the other do not: the first waits the bound out alone. The files are not
# named `<name>.sh`, which the base cache's key would follow ("the files
# the scenario names"): neither's key holds the other's blob.
alone=$work/alone
mkdir -p "$alone"
cp "$scenarios/protected" "$scenarios/build" "$alone/"
for name in 071.sh 072.sh 073.sh; do
    case $name in
    072.sh) meet=073.sh ;;
    073.sh) meet=072.sh ;;
    *) meet="" ;;
    esac
    meet_lines=""
    [ -z "$meet" ] || meet_lines="touch met.${name%.sh}
waited=0 # meet
while [ ! -e met.${meet%.sh} ] && [ \"\$waited\" -lt 600 ]; do sleep 0.1; waited=\$((waited + 1)); done # meet"
    cat >"$alone/$name" <<EOF
#!/bin/sh
check() {
    for other in running.*; do
        [ -e "\$other" ] && [ "\$other" != running.$name ] || continue
        case "$name \$other" in
        "071.sh "* | *running.071.sh) echo "$name beside \$other"; exit 2 ;;
        esac
    done
}
echo "$name started \$(date +%s%N)"
touch running.$name
check
$meet_lines
sleep 1
check
rm -f running.$name
echo "$name ended \$(date +%s%N)"
EOF
done
# stamp <name> <started|ended>: the time (ns) the base run of <name> printed,
# read from its own block in the log.
stamp() {
    sed -n "/^| == base: $1\$/,/^| == base: $1: /s/^| $1 $2 \([0-9]*\)\$/\1/p" "$work/gate.out"
}
# intervals <a> <b>: "overlapped" if each base run started before the other
# ended, "apart" if not, "unstamped" if a stamp is missing.
intervals() {
    a0=$(stamp "$1" started) a1=$(stamp "$1" ended) b0=$(stamp "$2" started) b1=$(stamp "$2" ended)
    if [ -z "$a0" ] || [ -z "$a1" ] || [ -z "$b0" ] || [ -z "$b1" ]; then
        echo unstamped
    elif [ "$a0" -lt "$b1" ] && [ "$b0" -lt "$a1" ]; then
        echo overlapped
    else
        echo apart
    fi
}
printf '# run alone\n071.sh\n' >"$alone/serial"
expect "a scenario on the serial list runs alone, on the base and the candidate" \
    "$(with_jobs 4 alone "$good" "$alone")" 'exit=0 {"verdict":"accepted"}'
# Side by side, read from the runs' own stamps, not from a bound on the wall
# clock (which failed once at load 12.8, on ce03a441), and certain by their
# meeting (above).
expect "the others ran side by side" "$(intervals 072.sh 073.sh)" overlapped
expect "and 071 beside neither" "$(intervals 071.sh 072.sh) $(intervals 071.sh 073.sh)" 'apart apart'
expect "the same results again: all from the cache" \
    "$(with_jobs 4 alone "$good" "$alone" >/dev/null; cat "$work/gate.out")" \
    '| base results from the cache: 071.sh 072.sh 073.sh; run on the base: none'
# On the list, 072 runs alone and would wait out its meeting: it leaves its
# file and no longer waits (its blob changes, and it is judged again in any
# case; 073's key does not hold it).
printf '072.sh\n' >>"$alone/serial"
sed -i '/ # meet$/d' "$alone/072.sh"
expect "one put on the list is judged again, alone" \
    "$(with_jobs 4 alone "$good" "$alone" >/dev/null; cat "$work/gate.out")" \
    '| base results from the cache: 071.sh 073.sh; run on the base: 072.sh'
rm "$alone/serial"
expect "the control: without the list, 071 meets its neighbours" \
    "$(with_jobs 4 alone-control "$good" "$alone")" 'exit=6 {"host_fault":true,'

# 16h. The start order (cut B): the serial list first, then the task and
# check-tests, then the rest longest first by the last completed runs'
# times; the log and the verdict keep the scenario order. 061 is the
# shortest and 062 the longest; 064 runs alone; 063 is the task.
longest=$work/longest
mkdir -p "$longest"
cp "$scenarios/protected" "$scenarios/build" "$longest/"
for run in 061:0.25 062:2.5 063:1.5 064:0.25; do
    cat >"$longest/${run%%:*}.sh" <<EOF
#!/bin/sh
echo "${run%%:*} started \$(date +%s%N)"
sleep ${run#*:}
[ "${run%%:*}" != 063 ] || [ "\$(cat value)" != one ]
EOF
done
printf '#!/bin/sh\nexit 0\n' >"$longest/check-tests.sh"
printf '064.sh\n' >"$longest/serial"
# started <name>: when the candidate run of <name> printed that it started.
started() { sed -n "s/^| $1 started \([0-9]*\)\$/\1/p" "$work/gate.out" | tail -n 1; }
expect "a first gate: no times yet" "$(with_jobs 2 longest "$good" "$longest" "$longest/063.sh")" \
    'exit=0 {"verdict":"accepted"}'
expect "its candidate runs start with the serial list, the task and check-tests, then in the scenario order" \
    "$(cat "$work/gate.out")" \
    "| candidate start order (serial list first, then the task and check-tests, then longest first by the last runs' times, 0 of 2 known): 064.sh 063.sh check-tests.sh 061.sh 062.sh"
expect "the times are kept for the next gate, per phase" \
    "$(cd "$work/caches/longest/spawn/gate-times/"* && ls | tr '\n' ' ')" \
    'base.061.sh base.062.sh base.063.sh base.064.sh candidate.061.sh candidate.062.sh candidate.063.sh candidate.064.sh candidate.check-tests.sh '
expect "the next gate starts the longest first" \
    "$(with_jobs 2 longest "$good" "$longest" "$longest/063.sh" >/dev/null; cat "$work/gate.out")" \
    "| candidate start order (serial list first, then the task and check-tests, then longest first by the last runs' times, 2 of 2 known): 064.sh 063.sh check-tests.sh 062.sh 061.sh"
expect "062 started before 061" "$([ "$(started 062)" -lt "$(started 061)" ] && echo before)" before
expect "and the log keeps the scenario order" \
    "$(grep -E '^\| == regression: 06[12]\.sh must still pass$' "$work/gate.out" | tr '\n' '/')" \
    '| == regression: 061.sh must still pass/| == regression: 062.sh must still pass/'
expect "one at a time, the scenario order and no line" \
    "$(with_jobs 1 longest "$good" "$longest" "$longest/063.sh" >/dev/null; grep -c 'start order' "$work/gate.out")" 0

# 16d. The base phase ends before any candidate code runs: each base run
# leaves a mark in the base tree, and each candidate run wants all of them.
order=$work/order
mkdir -p "$order"
cp "$scenarios/protected" "$scenarios/build" "$order/"
for n in 1 2 3 4 5; do
    cat >"$order/08$n.sh" <<EOF
#!/bin/sh
if [ "\$(cat value)" = one ]; then
    sleep $((n % 3))
    touch base-done.08$n
    [ $n -ne 5 ] || exit 1
    exit 0
fi
for n in 1 2 3 4 5; do
    [ -f "\$SPAWN_BASELINE_TREE/base-done.08\$n" ] || { echo "08\$n not done on the base"; exit 2; }
done
EOF
done
expect "every base run ended before the first candidate run" \
    "$(with_jobs 4 order "$good" "$order")" 'exit=0 {"verdict":"accepted"}'
expect "an unsolved objective still runs twice on the base, one run after the other" \
    "$(cat "$work/gate.out")" '| == base: 085.sh again (unsolved needs two runs that agree)'

# 16e. A harness error on the base aborts at once: the runs after it stop.
abort=$work/abort
mkdir -p "$abort"
cp "$scenarios/protected" "$scenarios/build" "$abort/"
printf '#!/bin/sh\nsleep 2\n' >"$abort/091.sh"
printf '#!/bin/sh\nexit 3\n' >"$abort/092.sh"
printf '#!/bin/sh\nsleep 120.5\n' >"$abort/093.sh"
started=$(date +%s)
expect "a harness error among parallel base runs ends the gate: a host fault" \
    "$(with_jobs 4 abort "$good" "$abort")" 'exit=6 {"host_fault":true,'
outer=$(($(date +%s) - started))
# As 16b: the gate's own total closes before the runs after it are
# stopped, so the outer clock less the total is that stop (a 25 s wall
# clock around the whole gate counted its setup, 45 s at load 45).
gate_total=$(sed -n 's/^| gate time: .*total \([0-9]*\) s$/\1/p' "$work/gate.out")
expect "without waiting for the runs after it" \
    "$([ -n "$gate_total" ] && [ "$(wall_of base)" -lt 100 ] && [ $((outer - gate_total)) -lt 60 ] && echo early) (base $(wall_of base) s, outer $outer s, gate total ${gate_total:-none} s)" early
expect "after the blocks before it" "$(cat "$work/gate.out")" '| == base: 091.sh: solved'
expect "and says why" "$(cat "$work/gate.out")" '092.sh exited 3 on the base'
refuse "no run of the gate is left" "$(gone_within 30 'sleep 120.5')" 'sleep'

# 16f. The scenarios run a copy of the built binary, which no cargo command
# writes: here one scenario links target/debug/spawn again, as check-tests'
# `cargo test` does, while another runs SPAWN_BIN.
bin_scenarios=$work/bin
mkdir -p "$bin_scenarios"
cp "$scenarios/protected" "$bin_scenarios/"
printf '#!/bin/sh\nmkdir -p target/debug\nprintf "#!/bin/sh\\necho built\\n" >target/debug/spawn\nchmod 755 target/debug/spawn\n' \
    >"$bin_scenarios/build"
cat >"$bin_scenarios/check-relink.sh" <<'EOF'
#!/bin/sh
printf '#!/bin/sh\necho relinked\n' >target/debug/spawn.new
chmod 755 target/debug/spawn.new
mv target/debug/spawn.new target/debug/spawn
EOF
printf '#!/bin/sh\nsleep 2\n[ "$("$SPAWN_BIN")" = built ] || { echo "ran $SPAWN_BIN: $("$SPAWN_BIN")"; exit 1; }\n' \
    >"$bin_scenarios/check-run.sh"
cp "$bin_scenarios/check-run.sh" "$bin_scenarios/095.sh"
expect "a binary linked again mid-gate is not the one the scenarios run" \
    "$(with_jobs 4 bin "$good" "$bin_scenarios")" 'exit=0 {"verdict":"accepted"}'

# 16g. Seeds from the accepting gate (design-gate-time.md §2): a candidate
# run of an objective that completes with exit 0 is the candidate commit's
# `solved` as a base. The next gate, on that commit as its base, reads it
# from the cache and says whose it is. Never seeded: a run that fails (202,
# unsolved on both), a check, an objective that reads SPAWN_BASELINE_TREE
# (203), a run cut short (204, below). A changed scenario is another key.
# These gates have no test database, so nothing is shared between runs.
seedd=$work/seed-scenarios
mkdir -p "$seedd"
cp "$scenarios/protected" "$seedd/"
printf '#!/bin/sh\nexit 0\n' >"$seedd/201.sh"
printf '#!/bin/sh\nexit 1\n' >"$seedd/202.sh"
printf '#!/bin/sh\n[ -n "$SPAWN_BASELINE_TREE" ]\n' >"$seedd/203.sh"
printf '#!/bin/sh\n[ ! -e target/debug/spawn ] || [ -x target/debug/spawn ]\n' >"$seedd/205.sh"
printf '#!/bin/sh\nexit 0\n' >"$seedd/check-ok.sh"
expect "a gate whose candidate passes an objective" "$(with_jobs 2 seeds "$good" "$seedd")" \
    'exit=0 {"verdict":"accepted"}'
expect "seeds it for its candidate commit as a base" "$(cat "$work/gate.out")" \
    "| 201.sh: solved on the candidate, seeded into the base cache for $(printf '%s' "$good" | cut -c1-12) as a base"
expect "and says why one that reads SPAWN_BASELINE_TREE is not seeded" "$(cat "$work/gate.out")" \
    '| base cache: 203.sh is not seeded: 203.sh reads SPAWN_BASELINE_TREE'
expect "nor one that reads target/ (the listing's build, on the candidate)" "$(cat "$work/gate.out")" \
    '| base cache: 205.sh is not seeded: 205.sh runs cargo or reads target/'
refuse "a failing run seeds nothing" "$(cat "$work/gate.out")" '202.sh: solved on the candidate'
refuse "nor does a check" "$(cat "$work/gate.out")" 'check-ok.sh: solved on the candidate'
seed_cache=$work/caches/seeds/spawn/gate-base
seeded_entry=$(grep -l "^seeded-by-gate $good at " "$seed_cache"/* 2>/dev/null | head -n 1)
expect "the entry says whose seed it is" "$(cat "$seeded_entry" 2>/dev/null)" "seeded-by-gate $good at "
# A result already in the entry (another gate's) is never overwritten.
printf 'unsolved\n' >"$seeded_entry"
with_jobs 2 seeds "$good" "$seedd" >/dev/null
expect "an entry that holds a result is not overwritten by a seed" \
    "$([ "$(cat "$seeded_entry")" = unsolved ] && echo kept)" kept
refuse "and the gate seeds nothing there" "$(cat "$work/gate.out")" '201.sh: solved on the candidate'
# A key whose lock another gate holds (it is judging that key) is not
# seeded: the seed does not wait, and does not write beside it.
rm -f "$seeded_entry"
sh -c 'exec 9>>"$1" && flock 9 && exec sleep 3600' holder "$seeded_entry.lock" &
seed_holder=$!
background="$background $seed_holder"
while flock -n "$seeded_entry.lock" true; do sleep 0.1; done
with_jobs 2 seeds "$good" "$seedd" >/dev/null
kill "$seed_holder" 2>/dev/null || true
expect "a key whose lock another gate holds is not seeded" \
    "$([ ! -e "$seeded_entry" ] && echo absent)" absent
refuse "and the gate says no seed there" "$(cat "$work/gate.out")" '201.sh: solved on the candidate'
expect "once the lock is free, the seed is written again" \
    "$(with_jobs 2 seeds "$good" "$seedd" >/dev/null; cat "$seeded_entry" 2>/dev/null)" "seeded-by-gate $good at "
saved_base=$base
base=$good
branch seeded-next
printf 'three\n' >"$seed/value"
seeded_next=$(commit seeded-next)
expect "the next gate, on that candidate as its base" "$(with_jobs 2 seeds "$seeded_next" "$seedd")" \
    'exit=0 {"verdict":"accepted"}'
expect "reads the seed, and says whose it is" "$(cat "$work/gate.out")" \
    "| == base: 201.sh: solved (cached, seeded by the gate of $good at "
expect "and runs the others on the base: no unsolved, no check, nothing that reads the old base or target/ was seeded" \
    "$(cat "$work/gate.out")" '| base results from the cache: 201.sh; run on the base: 202.sh 203.sh 205.sh'
seedd_changed=$work/seed-scenarios-changed
cp -r "$seedd" "$seedd_changed"
printf '#!/bin/sh\n# another blob\nexit 0\n' >"$seedd_changed/201.sh"
expect "a changed scenario is another key: its seed is not read" \
    "$(with_jobs 2 seeds "$seeded_next" "$seedd_changed" >/dev/null; cat "$work/gate.out")" \
    '| base results from the cache: 202.sh 203.sh 205.sh; run on the base: 201.sh'
# A run cut short: the task scenario fails at once on the candidate and
# decides, and 204, beside it, is stopped before it can end.
shortd=$work/seed-short
mkdir -p "$shortd"
cp "$scenarios/protected" "$shortd/"
# 204 runs until the suite lets it go, after the gate that decided early
# has ended: a 4 s run ended on its own when the task's failure took longer
# than that to decide (load), and was seeded.
shortd_release=$shortd/release
rm -f "$shortd_release"
releases="$releases $shortd_release"
printf '#!/bin/sh\nn=0\nwhile [ ! -e %s ] && [ "$n" -lt 6000 ]; do sleep 0.1; n=$((n + 1)); done\nexit 0\n' \
    "$shortd_release" >"$shortd/204.sh"
printf '#!/bin/sh\nexit 1\n' >"$work/seed-task.sh"
base=$saved_base
expect "a gate decided early" "$(with_jobs 2 seed-short "$good" "$shortd" "$work/seed-task.sh")" \
    '"reason":"scenario fails on the candidate"'
refuse "seeds nothing from the run it stopped" "$(cat "$work/gate.out")" '204.sh: solved on the candidate'
: >"$shortd_release"
base=$good
expect "and the next gate runs that objective on the base" \
    "$(with_jobs 2 seed-short "$seeded_next" "$shortd" >/dev/null; cat "$work/gate.out")" \
    '| base results from the cache: none; run on the base: 204.sh'
base=$saved_base

# 17. Base only (`--base-only`): the base phase alone, into the cache, with
# no verdict; the next gate on that base takes every result from the cache.
base_only() { # <scenarios>: the gate's exit status
    status=0
    SPAWN_GATE_VERDICT=$integrator/verdict sh "$gate" --base-only "$repo" "$base" "$1" \
        >"$work/gate.out" 2>&1 || status=$?
    echo "exit=$status"
}
rm -rf "$integrator"
mkdir -m 700 "$integrator"
expect "base only: exit 0" "$(export SPAWN_GATE_JOBS=4 XDG_CACHE_HOME="$work/caches/warm"; base_only "$par")" 'exit=0'
refuse "and no verdict, whatever SPAWN_GATE_VERDICT says" "$(ls -A "$integrator")" 'verdict'
expect "it says where the results are" "$(cat "$work/gate.out")" \
    "| base only: 4 results for the base $base are in the base cache $work/caches/warm/spawn/gate-base"
refuse "and judges no candidate" "$(cat "$work/gate.out")" '== candidate: build'
expect "the next gate on that base takes them all from the cache" \
    "$(with_jobs 4 warm "$good" "$par" >/dev/null; cat "$work/gate.out")" \
    '| base results from the cache: 051.sh 052.sh 053.sh 054.sh; run on the base: none'
expect "base only takes three arguments" \
    "exit=$(sh "$gate" --base-only "$repo" "$base" >/dev/null 2>&1 || echo $?)" 'exit=3'
expect "a harness error in base only is exit 4" \
    "$(export SPAWN_GATE_JOBS=4 XDG_CACHE_HOME="$work/caches/warm-abort"; base_only "$abort")" 'exit=4'

# 17c. Base keys (`--base-keys`): the key each objective's base result is
# cached under, printed with nothing built or run, and the same key the
# base-only run above wrote its entry as. deploy/gate-runner.sh --check
# compares two hosts' keys before shipping gates between them.
base_keys() { # <scenarios>: the gate's exit status
    status=0
    SPAWN_GATE_VERDICT=$integrator/verdict sh "$gate" --base-keys "$repo" "$base" "$1" \
        >"$work/base-keys.out" 2>&1 || status=$?
    echo "exit=$status"
}
# A directory of its own, as the judge above had: the case before this one
# judged a gate into $integrator after its own check.
rm -rf "$integrator"
mkdir -m 700 "$integrator"
expect "base keys: exit 0" "$(base_keys "$par")" 'exit=0'
expect "one key line per objective" \
    "$(grep -c '^key [0-9]*\.sh [0-9a-f]\{64\}$' "$work/base-keys.out")" 4
expect "the objectives the base phase judges" \
    "$(sed -n 's/^key \([0-9]*\.sh\) .*/\1/p' "$work/base-keys.out" | tr '\n' ' ')" \
    '051.sh 052.sh 053.sh 054.sh '
refuse "nothing built or run" "$(cat "$work/base-keys.out")" '| == '
refuse "and no verdict, whatever SPAWN_GATE_VERDICT says" "$(ls -A "$integrator")" 'verdict'
expect "the key is the entry the base-only run wrote" \
    "$(sed -n 's/^key 051\.sh \([0-9a-f]\{64\}\)$/\1/p' "$work/base-keys.out" |
        while IFS= read -r key; do [ -f "$work/caches/warm/spawn/gate-base/$key" ] && echo found; done)" \
    found
grep '^key ' "$work/base-keys.out" >"$work/keys.first"
XDG_CACHE_HOME="$work/caches/keys-again" sh "$gate" --base-keys "$repo" "$base" "$par" \
    >"$work/base-keys.again" 2>&1 || true
grep '^key ' "$work/base-keys.again" >"$work/keys.again"
expect "the same host computes the same keys again" \
    "$(cmp -s "$work/keys.first" "$work/keys.again" && echo same)" same
expect "base keys takes three arguments" \
    "exit=$(sh "$gate" --base-keys "$repo" "$base" >/dev/null 2>&1 || echo $?)" 'exit=3'

# 17b. A gate that starts while a base-only run judges the same key waits
# for its result instead of judging it again (the key's lock).
slow=$work/slow
mkdir -p "$slow"
cp "$scenarios/protected" "$scenarios/build" "$slow/"
# 111 runs on the base until the suite lets it go, and the suite does once
# the gate started meanwhile is blocked on the key's lock (its `flock 8`,
# a process under that gate), so the gate waits whatever its setup takes: a
# 12 s run ended before a gate that slow reached the key. Not /proc/locks:
# on this host a lock taken on an open descriptor (the gate's `exec 8>>`,
# then `flock -n 8`) is held but not listed there.
slow_release=$slow/release
rm -f "$slow_release"
releases="$releases $slow_release"
printf '#!/bin/sh\n[ "$(cat value)" = one ] || exit 0\nn=0\nwhile [ ! -e %s ] && [ "$n" -lt 6000 ]; do sleep 0.1; n=$((n + 1)); done\nexit 0\n' \
    "$slow_release" >"$slow/111.sh"
(
    export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$work/caches/locked"
    sh "$gate" --base-only "$repo" "$base" "$slow" >"$work/warm.out" 2>&1
) &
warming=$!
background="$background $warming"
# flock_under <pid>: whether a `flock 8` (a gate blocked on a key's lock)
# runs among <pid>'s descendants.
flock_under() {
    for fu_pid in $(pgrep -xf 'flock 8'); do
        fu_up=$fu_pid
        while [ -n "$fu_up" ] && [ "$fu_up" -gt 1 ]; do
            [ "$fu_up" = "$1" ] && return 0
            fu_up=$(ps -o ppid= -p "$fu_up" 2>/dev/null | tr -d ' ')
        done
    done
    return 1
}
# 111 going is the key held: the warm runs it only under the key's lock (a
# probe of the lock would take it for a moment, and a warm that found it
# taken then would skip the key).
waited=0
until ps -eo args | grep -F "$slow/111.sh" | grep -qv grep; do
    sleep 0.1
    waited=$((waited + 1))
    [ "$waited" -lt 3000 ] || { echo "FAIL the base-only run never ran 111 under its key"; exit 1; }
done
(with_jobs 2 locked "$good" "$slow" >"$work/locked.result") &
locked_gate=$!
background="$background $locked_gate"
waited=0
until flock_under "$locked_gate"; do
    sleep 0.1
    waited=$((waited + 1))
    [ "$waited" -lt 3000 ] || { echo "FAIL the gate started meanwhile never waited for the key"; exit 1; }
done
: >"$slow_release"
wait "$locked_gate" || true
expect "a gate started meanwhile" "$(cat "$work/locked.result")" 'exit=0 {"verdict":"accepted"}'
expect "waits for the other run's result" "$(cat "$work/gate.out")" \
    '| == base: 111.sh: solved (cached: another gate judged it meanwhile)'
expect "and counts it from the cache" "$(cat "$work/gate.out")" \
    '| base results from the cache: 111.sh; run on the base: none'
wait "$warming"
expect "the base-only run judged it" "$(cat "$work/warm.out")" '| == base: 111.sh: solved'

# 18. A server of the gate's own (GitHub issue #6), run by the gate under its
# scratch from the test server's programs: the host's (SPAWN_GATE_PG_BIN;
# TEST_GATE_PG_BIN, by default the newest /usr/lib/postgresql/<major>/bin)
# and a docker image's (SPAWN_GATE_PG_IMAGE; TEST_GATE_PG_IMAGE,
# postgres:16-alpine by default, major TEST_GATE_PG_IMAGE_MAJOR, 16), each
# when the host has it.
pg_url_sh=$(cd "$(dirname "$gate")/.." && pwd)/pg-url.sh
# q <sql>: one value, over the URL the sandbox got (libpq's variables).
q_function="q() { . '$pg_url_sh' && pg_url_export \"\$SPAWN_TEST_DATABASE_URL\" &&
    PGDATABASE=\$pg_url_database psql -XAtq -v ON_ERROR_STOP=1 -c \"\$1\"; }"
# A socket of the host's the server may try to put in place of its own.
python3 -c '
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen()
time.sleep(1800)' "$work/decoy.sock" &
background="$background $!"
while [ ! -S "$work/decoy.sock" ]; do sleep 0.1; done
# own_server_cases <variable> <value> <major> <kind>: the cases, with the
# server's programs from <variable>=<value>. Each kind has its own
# scenarios, caches and TMPDIR (where the gates' scratch, and so every
# process of theirs, is named).
own_server_cases() {
    pg_var=$1 pg_source=$2 pg_major=$3 kind=$4
    own=$work/own-pg-$kind
    pgtmp=$work/pgtmp-$kind
    mkdir -p "$own" "$pgtmp"
    cp "$scenarios/protected" "$scenarios/build" "$own/"
    cat >"$own/101.sh" <<EOF
#!/bin/sh
$q_function
[ "\$(q 'select 1')" = 1 ] || exit 2
[ "\$(q "select current_setting('max_connections')")" = 300 ] || exit 2
[ "\$(q "select current_setting('server_version_num')::int / 10000")" = $pg_major ] || exit 2
EOF
    # Candidate code leaves a table; the next gate's server has none.
    cat >"$own/check-leftover.sh" <<EOF
#!/bin/sh
$q_function
[ "\$(q "select to_regclass('public.leftover') is null")" = t ] || { echo "a table of an earlier gate"; exit 1; }
q 'create table leftover (x int)'
EOF
    # A superuser's COPY TO PROGRAM runs in the server's sandbox, which has
    # nothing of the host: not the owner's files, not their home.
    cat >"$own/check-escape.sh" <<EOF
#!/bin/sh
$q_function
q "copy (select 1) to program 'cat $HOME/.config/spawn/secret >/var/lib/postgresql/data/leak 2>/dev/null; touch $HOME/pwned 2>/dev/null; true'"
[ "\$(q "select pg_read_file('leak')")" = "" ] || { echo "the server read the owner's secret"; exit 1; }
EOF
    # It can replace the path of its own socket with a symlink to another
    # socket of the host; the sandbox's link still names the server's.
    cat >"$own/check-swap.sh" <<EOF
#!/bin/sh
$q_function
q "copy (select 1) to program 'rm -f /run/postgresql/.s.PGSQL.5432; ln -s $work/decoy.sock /run/postgresql/.s.PGSQL.5432'"
EOF
    cat >"$own/check-zswap.sh" <<EOF
#!/bin/sh
$q_function
[ "\$(PGCONNECT_TIMEOUT=5 q 'select 42')" = 42 ] || { echo "the sandbox's socket was swapped"; exit 1; }
EOF
    printf 'check-swap.sh\n' >"$own/serial"
    with_pg() { # <scenarios> [<candidate>]
        (
            export "$pg_var=$pg_source" SPAWN_GATE_JOBS=4 XDG_CACHE_HOME="$work/caches/own-$kind" TMPDIR="$pgtmp"
            judge "${2:-$good}" "" "$1"
        )
    }
    # What names this case's $pgtmp other than as a mask: another suite's
    # sandbox on this host masks the socket directory under $pgtmp
    # (`--tmpfs <dir>`, or `--ro-bind /dev/null <socket>`: gate.sh's socket
    # masks) and is no server of ours, so those two forms are dropped
    # before the match. A server cannot outlive the bwrap that names it
    # (gate-pg.sh: --unshare-all --die-with-parent, its own pid namespace).
    left() {
        ps -eo args | sed -e 's| --tmpfs [^ ]*||g' -e 's| --ro-bind /dev/null [^ ]*||g' |
            grep -F "$pgtmp/" | grep -v grep || true
    }
    expect "$kind: a gate on a server of its own" "$(with_pg "$own")" 'exit=0 {"verdict":"accepted"}'
    expect "$kind: says so, and how big" "$(cat "$work/gate.out")" "| test database: a server of the gate's own, $pg_source ("
    expect "$kind: with room for its scenarios" "$(cat "$work/gate.out")" 'max_connections 300; scenarios at once: 4'
    expect "$kind: its version is in the base cache's key" "$(cat "$work/gate.out")" " server $pg_major"
    [ ! -e "$HOME/pwned" ] || { echo "FAIL $kind: the server's program wrote $HOME/pwned"; exit 1; }
    echo "ok   $kind: the server's programs reach nothing of the host"
    expect "$kind: the next gate's server is a fresh one" "$(with_pg "$own")" 'exit=0 {"verdict":"accepted"}'
    expect "$kind: and its base results come from the cache" "$(cat "$work/gate.out")" \
        '| base results from the cache: 101.sh; run on the base: none'
    refuse "$kind: no server is left" "$(left)" 'bwrap'
    expect "$kind: a shared-server key is not a private one" \
        "$(rm -rf "$work/own-shared"; cp -r "$own" "$work/own-shared"; rm "$work/own-shared/serial" "$work/own-shared/check-"*
           printf '#!/bin/sh\nexit 0\n' >"$work/own-shared/101.sh"
           with_pg "$work/own-shared" >/dev/null
           (export XDG_CACHE_HOME="$work/caches/own-$kind" SPAWN_GATE_JOBS=1; judge "$good" "" "$work/own-shared") >/dev/null
           cat "$work/gate.out")" \
        '| base results from the cache: none; run on the base: 101.sh'
    # The candidate's runs get a fresh server, started after the base
    # phase: what a base run leaves on its server a candidate run does not
    # see, as the next gate's base run of the same commit will not
    # (design-gate-time.md §2). 102 leaves a table on the base.
    fresh=$work/own-fresh-$kind
    mkdir -p "$fresh"
    cp "$scenarios/protected" "$fresh/"
    cat >"$fresh/102.sh" <<EOF
#!/bin/sh
$q_function
if [ "\$(cat value)" = one ]; then q 'create table base_was_here (x int)'; exit 0; fi
[ "\$(q "select to_regclass('public.base_was_here') is null")" = t ] || { echo "the base phase's table, on the candidate's server"; exit 1; }
EOF
    expect "$kind: the candidate's runs get a fresh server" "$(with_pg "$fresh")" 'exit=0 {"verdict":"accepted"}'
    expect "$kind: the log says so" "$(cat "$work/gate.out")" "| test database: a fresh server for the candidate's runs, up in "
    expect "$kind: and a gate on its own server seeds" "$(cat "$work/gate.out")" \
        '| 102.sh: solved on the candidate, seeded into the base cache for '
    # The shared template removed while a gate runs (spawn-maintenance's
    # removal, batch 13, 2026-11-09) is remade by the next gate with a new
    # password; the candidate's fresh server still takes the password its
    # sandbox was given, from the gate's own copy. 103's base run waits while
    # the template goes, and the candidate's start remakes it.
    kept=$work/own-kept-$kind
    mkdir -p "$kept"
    cp "$scenarios/protected" "$kept/"
    # 103's base run lasts until the suite has removed the template (its
    # release file), not 3 s: at load the removal came after the base phase.
    kept_release=$kept/release
    rm -f "$kept_release"
    releases="$releases $kept_release"
    cat >"$kept/103.sh" <<EOF
#!/bin/sh
$q_function
if [ "\$(cat value)" = one ]; then
    n=0
    while [ ! -e $kept_release ] && [ "\$n" -lt 1000 ]; do sleep 0.3125; n=\$((n + 1)); done
    exit 0
fi
[ "\$(q 'select 42')" = 42 ] || { echo "the candidate's server refused the sandbox's password"; exit 1; }
EOF
    (
        export "$pg_var=$pg_source" SPAWN_GATE_JOBS=4 XDG_CACHE_HOME="$work/caches/own-kept-$kind" TMPDIR="$pgtmp"
        judge "$good" "" "$kept" >"$work/kept.result"
    ) &
    kept_gate=$!
    waited=0
    until ps -eo args | grep -F 'sleep 0.3125' | grep -qv grep || [ "$waited" -ge 3000 ]; do
        sleep 0.1
        waited=$((waited + 1))
    done
    [ "$waited" -lt 3000 ] || { echo "FAIL $kind: 103 never ran on the base"; exit 1; }
    templates=$work/caches/own-kept-$kind/spawn/gate-pg
    first_password=$(cat "$templates"/*/password)
    find "$templates" -mindepth 1 -maxdepth 1 -type d -regextype posix-extended -regex '.*/[0-9a-f]{64}' \
        -exec chmod -R u+w {} + -exec rm -rf {} +
    : >"$kept_release"
    wait "$kept_gate" || :
    expect "$kind: a template removed during the base phase" "$(cat "$work/kept.result")" 'exit=0 {"verdict":"accepted"}'
    refuse "$kind: was remade with a new password" "$(cat "$templates"/*/password)" "$first_password"
    expect "$kind: and the candidate's server took the sandbox's" "$(cat "$work/gate.out")" \
        '| 103.sh: solved on the candidate'
    # A server that does not start is the host's, not a verdict on the
    # candidate: exit 6, `test server`, with its evidence and which start,
    # quoting the server's FATAL line; either start (SPAWN_GATE_TEST_PG_FAIL
    # gives that start a setting no server knows).
    for start in gate candidate; do
        expect "$kind: a test server that does not start ($start) is a host fault, not a verdict" \
            "$(export SPAWN_GATE_TEST_PG_FAIL=$start; with_pg "$fresh")" \
            'exit=6 {"host_fault":true,"scenario":"test server","on":"test server","runs":"never started"'
        expect "$kind: with its evidence and its start ($start)" "$(cat "$integrator/verdict")" \
            "\"evidence\":\"test_server_never_started\",\"tree\":\"$start\""
        expect "$kind: quoting the server ($start)" "$(cat "$integrator/verdict")" \
            'did not start: FATAL:  unrecognized configuration parameter'
        refuse "$kind: and no server is left ($start)" "$(left)" 'bwrap'
    done
    # The gate's own copy of the template that cannot be taken (the shared
    # one remade under every copy; SPAWN_GATE_TEST_PG_KEEP_FAIL) is the
    # host's too, not an abort.
    expect "$kind: a template that cannot be copied is a host fault, not an abort" \
        "$(export SPAWN_GATE_TEST_PG_KEEP_FAIL=1; with_pg "$fresh")" \
        'exit=6 {"host_fault":true,"scenario":"test server","on":"test server","runs":"never started"'
    expect "$kind: saying why" "$(cat "$integrator/verdict")" \
        'did not start: its template in '
    # A gate ended by a signal takes its server and its runs with it.
    printf '#!/bin/sh\n[ "$(cat value)" = one ] || sleep 120.75\n' >"$own/check-long.sh"
    rm -rf "$integrator"
    mkdir -m 700 "$integrator"
    (
        export "$pg_var=$pg_source" SPAWN_GATE_JOBS=4 XDG_CACHE_HOME="$work/caches/own-$kind" TMPDIR="$pgtmp"
        exec sh "$gate" "$repo" "$base" "$good" "$own" >"$work/gate.out" 2>&1
    ) &
    signalled=$!
    waited=0
    until ps -eo args | grep -F 'sleep 120.75' | grep -qv grep || [ "$waited" -ge 3000 ]; do
        sleep 0.1
        waited=$((waited + 1))
    done
    [ "$waited" -lt 3000 ] || { echo "FAIL $kind: check-long never ran"; exit 1; }
    kill -TERM "$signalled"
    wait "$signalled" || :
    refuse "$kind: a gate ended by a signal leaves no run" "$(gone_within 30 'sleep 120.75')" 'sleep'
    refuse "$kind: and no server" "$(left)" 'bwrap'
    rm -f "$own/check-long.sh"
}
pg_bin=${TEST_GATE_PG_BIN-}
if [ -z "$pg_bin" ]; then
    pg_bin=$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -t/ -k5,5n | tail -n 1)
fi
if [ -n "$pg_bin" ] && [ -x "$pg_bin/postgres" ] && [ -x "$pg_bin/initdb" ]; then
    own_server_cases SPAWN_GATE_PG_BIN "$pg_bin" \
        "$("$pg_bin/postgres" --version | sed -n 's/^postgres (PostgreSQL) \([0-9]*\).*/\1/p')" programs
    expect "programs: a SPAWN_GATE_PG_BIN without postgres is no verdict" \
        "$(export SPAWN_GATE_PG_BIN="$work/no-such-bin"; judge "$good" "" "$par")" 'exit=3 '
    expect "programs: and says why" "$(cat "$work/gate.out")" 'has no postgres and initdb'
    expect "both a SPAWN_GATE_PG_IMAGE and a SPAWN_GATE_PG_BIN is no verdict" \
        "$(export SPAWN_GATE_PG_BIN="$pg_bin" SPAWN_GATE_PG_IMAGE=postgres:16-alpine; judge "$good" "" "$par")" 'exit=3 '
    expect "and says why" "$(cat "$work/gate.out")" 'are both set'
else
    echo "skip the gate's own test server from the host's programs: no postgres and initdb in '$pg_bin' (TEST_GATE_PG_BIN)"
fi
pg_image=${TEST_GATE_PG_IMAGE:-postgres:16-alpine}
if command -v docker >/dev/null 2>&1 && docker image inspect "$pg_image" >/dev/null 2>&1; then
    own_server_cases SPAWN_GATE_PG_IMAGE "$pg_image" "${TEST_GATE_PG_IMAGE_MAJOR:-16}" image
    expect "image: an image docker does not have is no verdict" \
        "$(export SPAWN_GATE_PG_IMAGE=spawn-no-such-image:0; judge "$good" "" "$par")" 'exit=3 '
    expect "image: and says why" "$(cat "$work/gate.out")" 'docker has no such image'
else
    echo "skip the gate's own test server from a docker image: docker has no image $pg_image (TEST_GATE_PG_IMAGE)"
fi

# 19. The gate passes a tree's deps store into the sandbox: SUPPLY_PA_DEPS,
# which a project's build hook and scenarios read, from the integrator's own
# environment (the sandbox clears it). Unset, the sandbox has none — the
# check below then fails on the candidate and on the base, which is how
# the case knows the value came through; a check failing on both is the
# host's, no verdict (exit 6, "Checks run twice"), never a rejection.
deps_scenarios=$work/supply-pa-deps-scenarios
mkdir -p "$deps_scenarios"
cp "$scenarios/protected" "$scenarios/build" "$deps_scenarios/"
# The store sits where the scenarios are, which the gate binds read-only (a
# project binds its own): so the check below can see the path it was given.
store=$deps_scenarios/store
mkdir -p "$store/node_modules"
printf '%s\n' "$store" >"$deps_scenarios/expected-deps"
cat >"$deps_scenarios/check-supply-deps.sh" <<'CHECK'
#!/bin/sh
# The store the integrator named reached the sandbox, and nothing else of the
# integrator's: the environment is cleared.
expected=$(cat "$(dirname "$0")/expected-deps")
[ "${SUPPLY_PA_DEPS-}" = "$expected" ] || {
    echo "SUPPLY_PA_DEPS is ${SUPPLY_PA_DEPS-<unset>}, not $expected"
    exit 1
}
[ -z "${SPAWN_DATABASE_URL-}" ] || {
    echo "the integrator's SPAWN_DATABASE_URL reached the sandbox"
    exit 1
}
[ -d "$SUPPLY_PA_DEPS" ] && echo "the store is visible through the path"
exit 0
CHECK
expect "the gate passes SUPPLY_PA_DEPS into the sandbox" \
    "$(export SUPPLY_PA_DEPS="$store"; judge "$good" "" "$deps_scenarios")" 'exit=0 {"verdict":"accepted"}'
expect "and the sandbox shows its path" "$(cat "$work/gate.out")" 'the store is visible through the path'
expect "unset, the sandbox has none: the check fails and the gate states no verdict" \
    "$(unset SUPPLY_PA_DEPS; judge "$good" "" "$deps_scenarios")" \
    'exit=6 {"host_fault":true,"scenario":"check-supply-deps.sh'
expect "and says what the sandbox saw" "$(cat "$work/gate.out")" 'SUPPLY_PA_DEPS is <unset>'

# 20. Host scenario slots: every scenario run takes one of the host's
# SPAWN_GATE_HOST_JOBS, across gates, so two gates do not each run their own
# SPAWN_GATE_JOBS. The scenarios note when they start and end (their own
# clock), which is what this case reads: with one slot the two gates' runs do
# not overlap, with two they do; a single gate with two jobs gets both slots;
# and a gate killed outright (its whole process group) leaves no slot held.
slots=$work/slots
mkdir -p "$slots"
cp "$scenarios/protected" "$slots/"
# Each run lasts until the suite lets it go (the release file), polling with
# a sleep of its own length (slot_token) so the suite can count the runs
# going: the suite releases two gates' runs once one gate says it waits
# (one slot) or once two runs go (two slots). A 2 s run asked the gates'
# setups to land within 2 s of each other, which load does not keep. Once
# released, a run lasts 1 s.
slot_release=$slots/release
releases="$releases $slot_release"
slot_token=0.2$$
cat >"$slots/check-slot.sh" <<SLOT
#!/bin/sh
echo "slot-start \$(date +%s%3N)"
n=0
while [ ! -e $slot_release ] && [ "\$n" -lt 1500 ]; do sleep $slot_token; n=\$((n + 1)); done
sleep 1
echo "slot-end \$(date +%s%3N)"
exit 0
SLOT
# slots_going <n>: wait until <n> slot runs go (up to 300 s).
slots_going() {
    sg_n=0
    until [ "$(ps -eo args | grep -F "sleep $slot_token" | grep -cv grep)" -ge "$1" ]; do
        sleep 0.1
        sg_n=$((sg_n + 1))
        [ "$sg_n" -lt 3000 ] || { echo "FAIL $1 slot runs never went at once"; exit 1; }
    done
}
# slots_said <text> <log>...: wait until one of the logs says it (up to 300 s).
slots_said() {
    ss_text=$1
    shift
    ss_n=0
    until cat "$@" 2>/dev/null | grep -qF "$ss_text"; do
        sleep 0.1
        ss_n=$((ss_n + 1))
        [ "$ss_n" -lt 3000 ] || { echo "FAIL no gate said: $ss_text"; exit 1; }
    done
}
cp "$slots/check-slot.sh" "$slots/check-slot-two.sh"
# slot_run <tag> <budget> [<jobs>]: one gate's exit status, its log in
# $work/slots-<tag>.log.
slot_run() { # <tag> <budget> [<jobs>]
    status=0
    # Its own cache root per gate: the budget must not depend on
    # XDG_CACHE_HOME (the slots are under $HOME), or a hand-run gate with
    # another cache root would miss it.
    XDG_CACHE_HOME="$work/caches/slots-$1" SPAWN_GATE_JOBS="${3:-2}" SPAWN_GATE_HOST_JOBS="$2" \
        sh "$gate" "$repo" "$base" "$good" "$slots" >"$work/slots-$1.log" 2>&1 || status=$?
    echo "exit=$status"
}
# slot_window <log> <n>: the n-th run's start and end, in ms, "start end".
slot_window() {
    awk -v want="$2" '
        /^\| slot-start / { n++; if (n == want) { start = $3; next } }
        /^\| slot-end / && start != "" { print start, $3; exit }' "$1"
}
# overlaps <a-start> <a-end> <b-start> <b-end>: "overlap" or "apart".
overlaps() {
    if [ "$1" -lt "$4" ] && [ "$3" -lt "$2" ]; then echo overlap; else echo apart; fi
}
rm -f "$slot_release"
slot_run a 1 1 >/dev/null & slot_a=$!
slot_run b 1 1 >/dev/null & slot_b=$!
slots_said "waiting for a host scenario slot (all 1 taken" "$work/slots-a.log" "$work/slots-b.log"
: >"$slot_release"
wait "$slot_a" "$slot_b"
# Both checks run (one scenario each gate), two gates, one slot in all.
wa=$(slot_window "$work/slots-a.log" 1)
wb=$(slot_window "$work/slots-b.log" 1)
[ -n "$wa" ] && [ -n "$wb" ] || { echo "FAIL no scenario run in a log"; exit 1; }
expect "one host slot: two gates' runs do not overlap" "$(overlaps $wa $wb)" apart
expect "the budget is in the log" "$(cat "$work/slots-a.log")" \
    '| host scenario slots: 1 (every gate on this host takes one per scenario run)'
expect "and the gate that waited says how long, and the slot it took" \
    "$(cat "$work/slots-a.log" "$work/slots-b.log")" \
    "for host scenario slot 1 of 1 (shared by every gate on this host)"
expect "and it was visible while it waited" \
    "$(cat "$work/slots-a.log" "$work/slots-b.log")" \
    "waiting for a host scenario slot (all 1 taken, shared by every gate on this host)"
# The host's own statement outranks the environment: with host_jobs set to
# 1, a gate given SPAWN_GATE_HOST_JOBS=2 still takes one slot, and the
# no-file caveat is not said.
mkdir -p "$HOME/.cache/spawn/gate-slots"
printf '1\n' >"$HOME/.cache/spawn/gate-slots/host_jobs"
rm -f "$slot_release"
slot_run f 2 1 >/dev/null & slot_f=$!
slot_run g 2 1 >/dev/null & slot_g=$!
slots_said "waiting for a host scenario slot (all 1 taken" "$work/slots-f.log" "$work/slots-g.log"
: >"$slot_release"
wait "$slot_f" "$slot_g"
expect "a host_jobs file outranks SPAWN_GATE_HOST_JOBS" \
    "$(overlaps $(slot_window "$work/slots-f.log" 1) $(slot_window "$work/slots-g.log" 1))" apart
expect "and its number is the one stated" "$(cat "$work/slots-f.log")" \
    '| host scenario slots: 1 (every gate on this host takes one per scenario run)'
refuse "and no no-file caveat is said" "$(cat "$work/slots-f.log")" "host_jobs file"
rm -f "$HOME/.cache/spawn/gate-slots/host_jobs"
rm -f "$slot_release"
slot_run c 2 1 >/dev/null & slot_c=$!
slot_run d 2 1 >/dev/null & slot_d=$!
slots_going 2
: >"$slot_release"
wait "$slot_c" "$slot_d"
expect "two host slots: the same two runs overlap" \
    "$(overlaps $(slot_window "$work/slots-c.log" 1) $(slot_window "$work/slots-d.log" 1))" overlap
rm -f "$slot_release"
slot_run e 2 2 >/dev/null & slot_e=$!
slots_going 2
: >"$slot_release"
wait "$slot_e"
expect "one gate with two jobs gets both slots (no deadlock)" \
    "$(overlaps $(slot_window "$work/slots-e.log" 1) $(slot_window "$work/slots-e.log" 2))" overlap
rm -f "$slots/check-slot-two.sh"
# A gate killed as a whole process group: its scenario dies, the kernel
# drops its slot, and the next gate takes it at once.
slowslots=$work/slots-slow
mkdir -p "$slowslots"
cp "$scenarios/protected" "$slowslots/"
printf '#!/bin/sh\nsleep 120\nexit 0\n' >"$slowslots/check-slow.sh"
killed_pidfile=$work/slots-killed.pid
rm -f "$killed_pidfile"
# The killed gate's scratch is its own (no trap ran): find it while its
# processes exist — the gate is a setsid leader, so every process of it has
# that session id — and remove only that directory afterwards, never another
# gate's scratch that a shared TMPDIR may hold.
bystander=${TMPDIR:-/tmp}/spawn-gate.bystander.$$
mkdir -p "$bystander"
XDG_CACHE_HOME="$work/caches/slots-kill" SPAWN_GATE_JOBS=1 SPAWN_GATE_HOST_JOBS=1 \
    setsid sh -c 'echo $$ >"$1"; shift; exec "$@"' _ "$killed_pidfile" \
    sh "$gate" "$repo" "$base" "$good" "$slowslots" >"$work/slots-killed.log" 2>&1 &
slot_file=$HOME/.cache/spawn/gate-slots/slot.1
waited=0
until [ -s "$killed_pidfile" ]; do
    sleep 0.1
    waited=$((waited + 1))
    [ "$waited" -lt 3000 ] || break
done
killed_gate=$(cat "$killed_pidfile" 2>/dev/null || true)
[ -n "$killed_gate" ] || { echo "FAIL the killed gate never started"; exit 1; }
waited=0
until ! flock -n "$slot_file" true 2>/dev/null; do
    sleep 0.1
    waited=$((waited + 1))
    [ "$waited" -lt 3000 ] || break
done
[ "$waited" -lt 3000 ] || { echo "FAIL the killed gate never took its slot"; exit 1; }
# No `--`: this file's sh is dash, whose kill reads "--" as a signal option
# and (-"$pid" never delivered) left the gate alive. A negative pid names the
# process group; the gate is its own session leader (setsid above).
killed_scratch=""
for killed_pid in $(ps -eo pid=,sess= | awk -v s="$killed_gate" '$2 == s { print $1 }'); do
    for killed_fd in /proc/"$killed_pid"/fd/*; do
        killed_path=$(readlink "$killed_fd" 2>/dev/null) || continue
        case "$killed_path" in
        "${TMPDIR:-/tmp}"/spawn-gate.*)
            killed_rest=${killed_path#"${TMPDIR:-/tmp}"/spawn-gate.}
            killed_scratch=${killed_rest%%/*}
            ;;
        esac
    done
done
kill -KILL "-$killed_gate" 2>/dev/null || { echo "FAIL the killed gate could not be killed"; exit 1; }
[ -z "$killed_scratch" ] || rm -rf "${TMPDIR:-/tmp}/spawn-gate.$killed_scratch"
expect "and it removes only the killed gate's scratch" \
    "$([ -e "$bystander" ] && echo kept || echo gone)" kept
rm -rf "$bystander"
waited=0
until flock -n "$slot_file" true 2>/dev/null; do
    sleep 0.1
    waited=$((waited + 1))
    [ "$waited" -lt 600 ] || break
done
expect "a killed gate leaves its slot free" "$([ "$waited" -lt 600 ] && echo free || echo held)" free
printf '#!/bin/sh\nsleep 1\nexit 0\n' >"$slowslots/check-slow.sh"
killed_run=0
XDG_CACHE_HOME="$work/caches/slots-kill" SPAWN_GATE_JOBS=1 SPAWN_GATE_HOST_JOBS=1 \
    sh "$gate" "$repo" "$base" "$good" "$slowslots" >"$work/slots-after-kill.log" 2>&1 || killed_run=$?
expect "and the next gate runs at once: exit 0" "$killed_run" 0
# No clock: the gate says a wait for its slot itself, "waiting for …"
# when it starts and "waited N s" for any wait from 200 ms (slot_claim). A
# 20 s wall clock around the gate counted its process start (load 45 inside
# another gate, check-test-gate on 2d0cab71), and its own total counts its
# setup, 45 s at that load.
refuse "without waiting for a slot" "$(cat "$work/slots-after-kill.log")" "waiting for a host scenario slot"
refuse "and its log says no wait" "$(cat "$work/slots-after-kill.log")" "waited"
# A run that cannot get a slot within its bound: no room to judge now (exit
# 5, the slot-shaped no-room fact), never a verdict; it starts nothing, and
# the holder keeps and then frees its slot. The bound is 30 min; the test's
# seam says 2 s, which is what SPAWN_GATE_SLOT_WAIT_SECS is for. The holder
# holds its slot until the suite lets it go (the release file, at most
# 120 s), not for a fixed time: the waiting gate's own setup comes before
# its claim, and at a load of 9 it outlasted a 5 s hold, so the waiter found
# the slot free and judged (exit 0).
waitfold=$work/slots-wait
mkdir -p "$waitfold"
cp "$scenarios/protected" "$waitfold/"
rm -f "$waitfold/release"
printf '#!/bin/sh\nn=0\nwhile [ ! -e %s/release ] && [ "$n" -lt 3000 ]; do sleep 0.1; n=$((n + 1)); done\nexit 0\n' \
    "$waitfold" >"$waitfold/check-hold.sh"
XDG_CACHE_HOME="$work/caches/slots-hold" SPAWN_GATE_JOBS=1 SPAWN_GATE_HOST_JOBS=1 \
    sh "$gate" "$repo" "$base" "$good" "$waitfold" >"$work/slots-hold.log" 2>&1 &
hold=$!
waited=0
until ! flock -n "$slot_file" true 2>/dev/null; do
    sleep 0.1
    waited=$((waited + 1))
    [ "$waited" -lt 3000 ] || break
done
[ "$waited" -lt 3000 ] || { echo "FAIL the holder never took its slot"; exit 1; }
wait_verdict=$work/slots-wait.verdict
wait_status=0
XDG_CACHE_HOME="$work/caches/slots-wait" SPAWN_GATE_JOBS=1 SPAWN_GATE_HOST_JOBS=1 \
    SPAWN_GATE_SLOT_WAIT_SECS=2 SPAWN_GATE_VERDICT="$wait_verdict" \
    sh "$gate" "$repo" "$base" "$good" "$waitfold" >"$work/slots-wait.log" 2>&1 || wait_status=$?
expect "a run that cannot get a slot in its bound: exit 5" "$wait_status" 5
expect "with the slot-shaped no-room fact" "$(cat "$wait_verdict")" '"check":"host scenario slot"'
expect "its measure the wait" "$(cat "$wait_verdict")" '"waited_ms":'
expect "and its threshold the budget" "$(cat "$wait_verdict")" '"budget":1'
expect "and the log says it gave up" "$(cat "$work/slots-wait.log")" \
    "gave up waiting for a host scenario slot after 2."
refuse "the scenario never ran in it" "$(cat "$work/slots-wait.log")" "slot-start"
: >"$waitfold/release"
hold_status=0
wait "$hold" || hold_status=$?
expect "the holder still finished: exit 0" "$hold_status" 0
expect "and the slot is free after" "$(flock -n "$slot_file" true && echo free)" free

# A gate whose own tree the sandbox empties (/tmp, /run, /var/tmp): it would
# read its scripts there as unreadable, compute base keys no other host
# matches, and say nothing (30 of 30 keys differed from /var/tmp, every tool
# version identical). Refused up front instead: exit 3, naming the root.
masked=$(mktemp -d /var/tmp/test-gate-masked.XXXXXX) || exit 1
mkdir -p "$masked/scripts/rsi"
cp "$gate" "$masked/scripts/rsi/gate.sh"
masked_status=0
masked_out=$(sh "$masked/scripts/rsi/gate.sh" "$repo" "$base" "$good" "$scenarios" 2>&1) || masked_status=$?
expect "a gate run from a root the sandbox empties: exit 3" "$masked_status" 3
expect "naming its own tree" "$masked_out" "refusing to run from $masked"
expect "and why" "$masked_out" "could not read its own scripts"
refuse "and nothing ran" "$masked_out" "=="
rm -rf "$masked"
masked=$work/no-masked-root
# The same gate under a root the sandbox shows gets past the refusal: it
# stops at the commit it cannot resolve, not before.
status=0
out=$(sh "$gate" /nonexistent no-such-commit no-such-commit "$scenarios" 2>&1) || status=$?
expect "a tree under no masked root is not refused" "exit=$status $out" "no commit no-such-commit"
refuse "and is not refused for its root" "exit=$status $out" "refusing to run from"

# A tree copied straight into a root the sandbox empties is in it as much as
# one under it: /var/tmp itself, not /var/tmp/<something>. Nothing is written
# into the shared root to test that: the copy stays in the suite's $work and
# a bwrap of the case's own binds it over /var/tmp for the gate under test,
# so the gate's root is exactly /var/tmp. bwrap is the suite's dependency
# already, and the refusal runs before the gate reaches bwrap itself.
bare_tree=$work/bare-root
mkdir -p "$bare_tree/scripts/rsi"
cp "$gate" "$bare_tree/scripts/rsi/gate.sh"
bare_status=0
bare_out=$(bwrap --ro-bind / / --bind "$bare_tree" /var/tmp \
    sh /var/tmp/scripts/rsi/gate.sh 2>&1) || bare_status=$?
expect "a tree copied to /var/tmp itself: exit 3" "$bare_status" 3
expect "naming the root itself" "$bare_out" "refusing to run from /var/tmp:"
expect "and why" "$bare_out" "roots the sandbox empties"
refuse "and nothing ran" "$bare_out" "=="

# The host's TMPDIR is emptied too when it is not /tmp (`sandbox()` masks it
# whole): a tree under it loses its own scripts the same way. The suite's
# own TMPDIR is the masked root here — the header requires it private, and
# under it the suite's scratch already lives.
case "${TMPDIR:-/tmp}" in
/tmp | /tmp/)
    echo "FAIL the host's TMPDIR case needs a private TMPDIR (the header says so)"
    exit 1
    ;;
esac
tmpdir_tree=$work/masked-tmpdir
mkdir -p "$tmpdir_tree/scripts/rsi"
cp "$gate" "$tmpdir_tree/scripts/rsi/gate.sh"
tmp_status=0
tmp_out=$(sh "$tmpdir_tree/scripts/rsi/gate.sh" 2>&1) || tmp_status=$?
expect "a tree under the host's TMPDIR: exit 3" "$tmp_status" 3
expect "naming the TMPDIR" "$tmp_out" "the host's TMPDIR ($TMPDIR)"
expect "and why" "$tmp_out" "the sandbox empties too"
refuse "and nothing ran" "$tmp_out" "=="

# 21. A warm claims last. A low claimant (SPAWN_GATE_SLOT_PRIORITY=low, the
# default of --base-only) takes a slot only when no gate is waiting for
# one: with one slot held, a normal gate already waiting gets it before a
# low one that arrived later; a low claimant alone takes an idle slot at
# once; and the demand lock goes with a waiting gate that is killed.
demand=$HOME/.cache/spawn/gate-slots/demand
# The holder holds the one slot (the slot file's lock, as in 21c) until the
# test lets go, once the later gates have said they wait: a holder that let
# go after a fixed 15 s was gone before a gate slow to start (each needs its
# exports and static checks first) was waiting, at load 19 with 12 gates
# (check-test-gate on f0c0d41c, 2026-11-10: "low never said").
prio_release=$work/prio.release
releases="$releases $prio_release"
prio_hold() { # <tag>: the one slot held until prio_let_go, in the background
    rm -f "$prio_release"
    flock "$slot_file" sh -c 'n=0; until [ -e "$1" ] || [ "$n" -ge 6000 ]; do sleep 0.1; n=$((n + 1)); done' \
        _ "$prio_release" &
    prio_holder=$!
    prio_n=0
    until ! flock -n "$slot_file" true 2>/dev/null; do
        sleep 0.1
        prio_n=$((prio_n + 1))
        [ "$prio_n" -lt 3000 ] || { echo "FAIL the holder ($1) never took its slot"; exit 1; }
    done
}
prio_let_go() { : >"$prio_release"; }
prio_gate() { # <tag> <priority>: a gate on the slot scenario, in the background
    XDG_CACHE_HOME="$work/caches/prio-$1" SPAWN_GATE_JOBS=1 SPAWN_GATE_HOST_JOBS=1 \
        SPAWN_GATE_SLOT_PRIORITY="$2" \
        sh "$gate" "$repo" "$base" "$good" "$slots" >"$work/prio-$1.log" 2>&1 &
}
prio_said() { # <tag> <text>: wait (up to 300 s: a gate's setup took 45 s at load 45) until the gate's log says it
    prio_n=0
    until grep -qF "$2" "$work/prio-$1.log" 2>/dev/null; do
        sleep 0.1
        prio_n=$((prio_n + 1))
        [ "$prio_n" -lt 3000 ] || { echo "FAIL $1 never said: $2"; exit 1; }
    done
}
prio_hold h1
prio_gate normal normal
prio_normal=$!
prio_said normal "waiting for a host scenario slot (all 1 taken"
prio_gate low low
prio_low=$!
prio_said low "waiting for a host scenario slot (claimed last"
prio_let_go
prio_status=0
wait "$prio_holder" "$prio_normal" "$prio_low" || prio_status=$?
expect "the holder and the two gates finish: exit 0" "$prio_status" 0
prio_wn=$(slot_window "$work/prio-normal.log" 1)
prio_wl=$(slot_window "$work/prio-low.log" 1)
[ -n "$prio_wn" ] && [ -n "$prio_wl" ] || { echo "FAIL no slot run in a priority log"; exit 1; }
expect "a waiting gate gets the slot before a warm that came later" \
    "$([ "${prio_wn%% *}" -lt "${prio_wl%% *}" ] && echo first)" first
expect "the low claimant says it claims last" "$(cat "$work/prio-low.log")" \
    '| host scenario slots: claimed last (a warm: a slot only when no gate is waiting for one)'
refuse "and a normal gate does not" "$(cat "$work/prio-normal.log")" 'claimed last'
expect "the demand lock is free after" "$(flock -n -x "$demand" true && echo free)" free
# Alone, a low claimant takes an idle slot at once.
prio_gate alone low
prio_alone=$!
prio_status=0
wait "$prio_alone" || prio_status=$?
expect "a low claimant alone: exit 0" "$prio_status" 0
refuse "without waiting" "$(cat "$work/prio-alone.log")" 'waited'
# A waiting gate that is killed takes its demand with it: the low claimant
# behind it gets the slot when the holder frees it.
prio_hold h2
XDG_CACHE_HOME="$work/caches/prio-killed" SPAWN_GATE_JOBS=1 SPAWN_GATE_HOST_JOBS=1 \
    setsid sh -c 'echo $$ >"$1"; shift; exec "$@"' _ "$work/prio-killed.pid" \
    sh "$gate" "$repo" "$base" "$good" "$slots" >"$work/prio-killed.log" 2>&1 &
prio_said killed "waiting for a host scenario slot (all 1 taken"
prio_gate behind low
prio_behind=$!
prio_said behind "waiting for a host scenario slot (claimed last"
prio_killed=$(cat "$work/prio-killed.pid")
# Its scratch, found by its session as in case 20, removed after the kill
# (no trap runs on KILL).
prio_scratch=""
for prio_pid in $(ps -eo pid=,sess= | awk -v s="$prio_killed" '$2 == s { print $1 }'); do
    for prio_fd in /proc/"$prio_pid"/fd/*; do
        prio_path=$(readlink "$prio_fd" 2>/dev/null) || continue
        case "$prio_path" in
        "${TMPDIR:-/tmp}"/spawn-gate.*)
            prio_rest=${prio_path#"${TMPDIR:-/tmp}"/spawn-gate.}
            prio_scratch=${prio_rest%%/*}
            ;;
        esac
    done
done
kill -KILL "-$prio_killed" 2>/dev/null || { echo "FAIL could not kill the waiting gate's group"; exit 1; }
[ -z "$prio_scratch" ] || rm -rf "${TMPDIR:-/tmp}/spawn-gate.$prio_scratch"
prio_let_go
prio_status=0
wait "$prio_behind" || prio_status=$?
expect "with the waiting gate killed, the warm behind it runs: exit 0" "$prio_status" 0
expect "and its scenario ran" "$(cat "$work/prio-behind.log")" '| slot-start '
wait "$prio_holder" || true
expect "and the demand lock is free after" "$(flock -n -x "$demand" true && echo free)" free
# --base-only is low by default, whatever the environment says otherwise.
prio_status=0
XDG_CACHE_HOME="$work/caches/prio-base" SPAWN_GATE_JOBS=4 SPAWN_GATE_HOST_JOBS=1 \
    sh "$gate" --base-only "$repo" "$base" "$slots" >"$work/prio-base.log" 2>&1 || prio_status=$?
expect "a base-only run: exit 0" "$prio_status" 0
expect "claims last by default" "$(cat "$work/prio-base.log")" 'claimed last'
expect "a bad SPAWN_GATE_SLOT_PRIORITY refuses to start: exit 3" \
    "exit=$(SPAWN_GATE_SLOT_PRIORITY=first sh "$gate" "$repo" "$base" "$good" "$slots" >/dev/null 2>&1 || echo $?)" \
    'exit=3'

# 21c. No inversion through a base entry. A warm (low) and a gate share a
# base cache and miss the same key, with the one slot held. The warm waits
# for its slot before it takes the key's lock, so the key stays free while
# it waits; the gate that comes after takes the key and the slot first (it
# is waiting, so the warm cannot scan), judges the objective itself, and
# the warm reads that entry (or skips the key, if the gate still holds it).
invert=$work/invert
mkdir -p "$invert"
cp "$scenarios/protected" "$invert/"
printf '#!/bin/sh\nsleep 1\nexit 0\n' >"$invert/302.sh"
invert_release=$work/invert.release
rm -f "$invert_release"
releases="$releases $invert_release"
flock "$slot_file" sh -c 'n=0; until [ -e "$1" ] || [ "$n" -ge 6000 ]; do sleep 0.1; n=$((n + 1)); done' _ "$invert_release" &
invert_holder=$!
prio_n=0
until ! flock -n "$slot_file" true 2>/dev/null; do
    sleep 0.1
    prio_n=$((prio_n + 1))
    [ "$prio_n" -lt 3000 ] || { echo "FAIL the inversion case's holder never took the slot"; exit 1; }
done
XDG_CACHE_HOME="$work/caches/invert" SPAWN_GATE_HOST_JOBS=1 \
    sh "$gate" --base-only "$repo" "$base" "$invert" >"$work/prio-invert-warm.log" 2>&1 &
invert_warm=$!
prio_said invert-warm "waiting for a host scenario slot (claimed last"
invert_lock=$(find "$work/caches/invert" -name '*.lock' | head -n 1)
[ -n "$invert_lock" ] || { echo "FAIL no base entry lock in the inversion case's cache"; exit 1; }
expect "a warm waiting for a slot holds no key's lock" \
    "$(flock -n "$invert_lock" true && echo free)" free
XDG_CACHE_HOME="$work/caches/invert" SPAWN_GATE_JOBS=1 SPAWN_GATE_HOST_JOBS=1 \
    sh "$gate" "$repo" "$base" "$good" "$invert" >"$work/prio-invert-gate.log" 2>&1 &
invert_gate=$!
prio_said invert-gate "waiting for a host scenario slot (all 1 taken"
: >"$invert_release"
prio_status=0
wait "$invert_holder" "$invert_warm" "$invert_gate" || prio_status=$?
expect "the warm and the gate finish: exit 0" "$prio_status" 0
expect "the gate judged the key itself" "$(cat "$work/prio-invert-gate.log")" '| == base: 302.sh: solved'
refuse "not from the warm's entry" "$(cat "$work/prio-invert-gate.log")" 'another gate judged it meanwhile'
invert_w=$(cat "$work/prio-invert-warm.log")
expect "the warm took the gate's entry or skipped the key" \
    "$(case "$invert_w" in *'302.sh: solved (cached: another gate judged it meanwhile)'* | *'302.sh: skipped (another gate is judging it'*) echo yes ;; esac)" yes
refuse "and never ran it" "$invert_w" '| == base: 302.sh
'

# 22. The build's share: CARGO_BUILD_JOBS inside the sandbox for the build
# hook is not set (nor set empty) unless SPAWN_GATE_BUILD_JOBS is, whatever
# SPAWN_GATE_JOBS says; a warm (--base-only) builds at 1 whatever is set;
# anything but a whole number from 1 refuses to start.
jobsdir=$work/build-jobs
mkdir -p "$jobsdir"
cp "$scenarios/protected" "$jobsdir/"
printf '#!/bin/sh\nprintf "%%s\\n" "${CARGO_BUILD_JOBS-unset}" >build-jobs\n' >"$jobsdir/build"
printf '#!/bin/sh\necho "build-jobs $(cat build-jobs)"\nexit 0\n' >"$jobsdir/check-jobs.sh"
jobs_run() { # <tag> [VAR=value ...]: the gate's exit status; its log in $work/jobs-<tag>.log
    jobs_tag=$1
    shift
    status=0
    env XDG_CACHE_HOME="$work/caches/jobs-$jobs_tag" SPAWN_GATE_HOST_JOBS=1 "$@" \
        sh "$gate" "$repo" "$base" "$good" "$jobsdir" >"$work/jobs-$jobs_tag.log" 2>&1 || status=$?
    echo "exit=$status"
}
expect "a gate's build share is cargo's own by default" "$(jobs_run share SPAWN_GATE_JOBS=3)" 'exit=0'
expect "CARGO_BUILD_JOBS is not passed (not even empty)" "$(cat "$work/jobs-share.log")" '| build-jobs unset'
expect "SPAWN_GATE_BUILD_JOBS sets it" \
    "$(jobs_run own SPAWN_GATE_JOBS=3 SPAWN_GATE_BUILD_JOBS=2)" 'exit=0'
expect "to its value" "$(cat "$work/jobs-own.log")" '| build-jobs 2'
expect "a bad SPAWN_GATE_BUILD_JOBS refuses to start: exit 3" \
    "$(jobs_run bad SPAWN_GATE_BUILD_JOBS=0)" 'exit=3'
expect "naming it" "$(cat "$work/jobs-bad.log")" 'SPAWN_GATE_BUILD_JOBS is a whole number from 1'
# A warm judges objectives only: one that says what its build ran at.
warmjobs=$work/build-jobs-warm
mkdir -p "$warmjobs"
cp "$jobsdir/protected" "$jobsdir/build" "$warmjobs/"
printf '#!/bin/sh\necho "build-jobs $(cat build-jobs)"\nexit 0\n' >"$warmjobs/301.sh"
status=0
env XDG_CACHE_HOME="$work/caches/jobs-warm" SPAWN_GATE_HOST_JOBS=1 SPAWN_GATE_BUILD_JOBS=4 \
    sh "$gate" --base-only "$repo" "$base" "$warmjobs" >"$work/jobs-warm.log" 2>&1 || status=$?
expect "a warm with SPAWN_GATE_BUILD_JOBS=4: exit 0" "$status" 0
expect "builds at 1 all the same" "$(cat "$work/jobs-warm.log")" 'build-jobs 1'

# The toolchain probe reads the test server through the gate's own
# scripts/pg-url.sh, inside the sandbox. For a project that is not Spawn the
# scenarios are another checkout's (Supply PA's gate/scenarios), so the
# gate's own scripts/ must be bound back under $HOME too: without it the
# probe read nothing and the log said "server none" beside a server the
# gate ran (2026-11-09). The gate here sits under $HOME, its scenarios in
# another tree, and the log names the server's version.
other_ctl=$HOME/other-ctl
other_project=$work/other-project/gate/scenarios
mkdir -p "$other_ctl" "$other_project" "$work/other-tmp"
cp -r "$(dirname "$gate")/.." "$other_ctl/scripts"
chmod -R u+w "$other_ctl"
cp "$scenarios/protected" "$other_project/"
printf '#!/bin/sh\nexit 0\n' >"$other_project/001.sh"
# With a server of the gate's own, as the live gate had (the programs of
# PostgreSQL 16 where this host has them; otherwise the case says so).
other_pg=${SPAWN_GATE_PG_BIN:-/usr/lib/postgresql/16/bin}
if [ -x "$other_pg/postgres" ] && [ -x "$other_pg/initdb" ]; then
    other_status=0
    other_out=$(unset SPAWN_TEST_DATABASE_URL; TMPDIR=$work/other-tmp XDG_CACHE_HOME="$work/caches/other" \
        SPAWN_GATE_PG_BIN=$other_pg \
        sh "$other_ctl/scripts/rsi/gate.sh" --base-only "$repo" "$base" "$other_project" 2>&1) ||
        other_status=$?
    expect "a gate under \$HOME judging another checkout's scenarios: exit 0" "$other_status" 0
    expect "with a server of its own" "$other_out" "| test database: a server of the gate's own"
    expect "its toolchain line names the server's version" \
        "$(printf '%s\n' "$other_out" | grep -E '^\| base cache: toolchain .* server [0-9]{5,6}$' || true)" \
        'server '
    refuse "never none while it runs one" "$other_out" "server none"
    refuse "nor unreadable" "$other_out" "server unreadable"
else
    echo "note: no PostgreSQL programs in $other_pg: the toolchain line's server case is not run"
fi

# 23. The candidate's schema must migrate the base's database
# (rsi/scenarios/check-schema-upgrade.sh, the real check, from a controller
# copy so it finds its scripts/). A base whose schema.sql makes a view
# v(a, b): a candidate that appends a column is accepted; one that puts it
# mid-list is rejected with PostgreSQL's own words (42P16, what refused
# 554a4bd4's deploy); one whose schema does not apply over itself is
# rejected as such. A candidate that adds a column to t in its create table
# alone (no `alter table ... add column if not exists`, which the base's
# database needs: its t exists) is rejected naming the missing column, and
# accepted with the alter.
schema_ctl=$work/schema-ctl
mkdir -p "$schema_ctl/rsi/scenarios"
cp -r "$(dirname "$gate")/.." "$schema_ctl/scripts"
chmod -R u+w "$schema_ctl"
cp "$(dirname "$gate")/../../rsi/scenarios/check-schema-upgrade.sh" "$schema_ctl/rsi/scenarios/"
cp "$scenarios/protected" "$schema_ctl/rsi/scenarios/"
git -C "$seed" checkout -q -B schema-base "$base"
mkdir -p "$seed/crates/spawn-postgres/sql"
printf 'create table if not exists t (a integer, b integer);\ncreate or replace view v as select a, b from t;\n' \
    >"$seed/crates/spawn-postgres/sql/schema.sql"
schema_base=$(commit schema-base)
schema_candidate() { # <name> <schema.sql>: a candidate on the schema base
    git -C "$seed" checkout -q -B "$1" "$schema_base"
    printf '%s\n' "$2" >"$seed/crates/spawn-postgres/sql/schema.sql"
    commit "$1"
}
schema_append=$(schema_candidate schema-append \
    'create table if not exists t (a integer, b integer);
create or replace view v as select a, b, a + b as c from t;')
schema_midlist=$(schema_candidate schema-midlist \
    'create table if not exists t (a integer, b integer);
create or replace view v as select a, a + b as c, b from t;')
schema_twice=$(schema_candidate schema-twice \
    'create table if not exists t (a integer, b integer);
create or replace view v as select a, b from t;
create table t2 (x integer);')
schema_column=$(schema_candidate schema-column \
    'create table if not exists t (a integer, b integer, c integer);
create or replace view v as select a, b from t;')
schema_altered=$(schema_candidate schema-altered \
    'create table if not exists t (a integer, b integer, c integer);
alter table t add column if not exists c integer;
create or replace view v as select a, b from t;')
# On a server of the gate's own (the suite has no shared test server), from
# the host's PostgreSQL programs; without them the case says so.
schema_pg=${SPAWN_GATE_PG_BIN:-/usr/lib/postgresql/16/bin}
schema_judge() { # <candidate>: judge it on the schema base, with a server of its own
    (
        export SPAWN_GATE_PG_BIN="$schema_pg" TMPDIR="$work/schema-tmp" XDG_CACHE_HOME="$work/caches/schema"
        base=$schema_base
        judge "$1" "" "$schema_ctl/rsi/scenarios"
    )
}
if [ -x "$schema_pg/postgres" ] && [ -x "$schema_pg/initdb" ]; then
    mkdir -p "$work/schema-tmp"
    expect "a schema that appends a view's column migrates the base's database" \
        "$(schema_judge "$schema_append")" 'exit=0 {"verdict":"accepted"}'
    expect "the check said so" "$(cat "$work/gate.out")" "migrates the base's database, and applies over itself"
    expect "one that puts it mid-list is rejected" "$(schema_judge "$schema_midlist")" '"verdict":"rejected"'
    expect "with PostgreSQL's words" "$(cat "$work/gate.out")" 'cannot change name of view column "b" to "c"'
    expect "naming what a deploy would do" "$(cat "$work/gate.out")" \
        "the candidate's schema.sql cannot migrate the base's database (a deploy would refuse it the same way)"
    expect "one that does not apply over itself is rejected" "$(schema_judge "$schema_twice")" '"verdict":"rejected"'
    expect "saying so" "$(cat "$work/gate.out")" "applied once but not over itself"
    expect "a column added without its alter is rejected" "$(schema_judge "$schema_column")" '"verdict":"rejected"'
    expect "naming the column a fresh database has" "$(cat "$work/gate.out")" "< t #3 c integer YES"
    expect "with its alter it is accepted" "$(schema_judge "$schema_altered")" 'exit=0 {"verdict":"accepted"}'
    expect "the columns compared" "$(cat "$work/gate.out")" "every table's columns are as a fresh database has them"
else
    echo "note: no PostgreSQL programs in $schema_pg: the schema upgrade case is not run"
fi

# 24. Main red on a scenario (046's lesson: Supply PA's 046 failed on main
# from 62516f9, and every gate read it as an objective not yet met). 071
# passes while `marker` says ok. Judged as a base on A (it passes), then on
# B, a descendant of A where it fails: main is red on 071, said and in the
# verdict, and the candidate's failure of it is not a verdict on the
# candidate. Not red: on a branch that never had A, and when the scenario
# itself changed since A (another blob: a stricter scenario, not a broken
# main).
red=$work/main-red
mkdir -p "$red"
cp "$scenarios/build" "$scenarios/protected" "$red/"
printf '#!/bin/sh\n[ "$(cat marker 2>/dev/null)" = ok ]\n' >"$red/071.sh"
git -C "$seed" checkout -q -B red-a "$base"
printf 'ok\n' >"$seed/marker"
red_a=$(commit red-a)
printf 'broken\n' >"$seed/marker"
red_b=$(commit red-b)
printf 'more\n' >"$seed/unrelated"
red_c=$(commit red-c)
git -C "$seed" checkout -q -B red-side "$base"
printf 'broken\n' >"$seed/marker"
red_side=$(commit red-side)
printf 'more\n' >"$seed/unrelated"
red_side_c=$(commit red-side-c)
red_cache=$work/caches/main-red
expect "071 is solved on A (base only)" \
    "$(export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$red_cache"; base=$red_a; base_only "$red"; cat "$work/gate.out")" \
    '| == base: 071.sh: solved'
red_out=$(export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$red_cache"; base=$red_b; judge "$red_c" "" "$red")
expect "solved on an ancestor, unsolved on the base: main red, in the verdict" "$red_out" \
    '"main_red_scenarios":["071.sh"]'
expect "and said, naming where it was solved" "$(cat "$work/gate.out")" \
    "| main red: 071.sh was solved on $(printf '%s' "$red_a" | cut -c1-12), an ancestor of this base"
expect "the candidate's failure of it is main's" "$(cat "$work/gate.out")" \
    '| 071.sh fails on the base too: main is red on it (above), not a verdict on the candidate'
refuse "never called an unsolved objective" "$(cat "$work/gate.out")" 'an unsolved objective, not a regression'
expect "and the candidate is judged as before: accepted" "$red_out" 'exit=0 {"verdict":"accepted"'
# The scenarios directory with a trailing slash, as a hand run may type it:
# the same directory, still red (before, "dir//071.sh" had dirname "dir",
# not "dir/", so no index key was written and red never fired).
red_out=$(export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$red_cache"; base=$red_b; judge "$red_c" "" "$red/")
expect "the scenarios directory given with a trailing slash: still main red" "$red_out" \
    '"main_red_scenarios":["071.sh"]'
red_out=$(export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$red_cache"; base=$red_side; judge "$red_side_c" "" "$red")
refuse "a branch that never had A: not red" "$red_out" 'main_red_scenarios'
expect "an unsolved objective there" "$(cat "$work/gate.out")" \
    '| 071.sh fails on the base too: an unsolved objective, not a regression'
printf '# stricter since A\n' >>"$red/071.sh"
red_out=$(export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$red_cache"; base=$red_b; judge "$red_c" "" "$red")
refuse "the scenario changed since A (another blob): not red" "$red_out" 'main_red_scenarios'
refuse "nor said" "$(cat "$work/gate.out")" '| main red:'
# Not red when anything else that decides the result changed since A (rev-f
# F1 on 517965e3): 074 compares marker with a data file beside it, solved on
# A; only that file changes, and B fails it: the scenarios changed, not
# main.
red4=$work/main-red-data
mkdir -p "$red4"
cp "$scenarios/build" "$scenarios/protected" "$red4/"
printf '#!/bin/sh\n[ "$(cat marker 2>/dev/null)" = "$(cat "$(dirname "$0")/074.want")" ]\n' >"$red4/074.sh"
printf 'ok\n' >"$red4/074.want"
expect "074 is solved on A" \
    "$(export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$red_cache"; base=$red_a; base_only "$red4"; cat "$work/gate.out")" \
    '| == base: 074.sh: solved'
printf 'nope\n' >"$red4/074.want"
red_out=$(export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$red_cache"; base=$red_b; judge "$red_c" "" "$red4")
refuse "a data file beside it changed since A (the same blob): not red" "$red_out" 'main_red_scenarios'
expect "an unsolved objective" "$(cat "$work/gate.out")" '| 074.sh fails on the base too: an unsolved objective, not a regression'
# Not red for a task scenario kept outside the scenarios directory (rev-f
# F2): no other task's gate runs it, so none could see it green again.
red5=$work/main-red-task
mkdir -p "$red5" "$work/elsewhere"
cp "$scenarios/build" "$scenarios/protected" "$red5/"
printf '#!/bin/sh\n[ "$(cat marker 2>/dev/null)" != broken ]\n' >"$work/elsewhere/075.sh"
expect "the task scenario passes on A" \
    "$(export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$red_cache"; base=$red_a; judge "$red_c" "$work/elsewhere/075.sh" "$red5")" \
    '"reason":"scenario already passes on the base"'
red_out=$(export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$red_cache"; base=$red_b; judge "$red_c" "$work/elsewhere/075.sh" "$red5")
refuse "and fails on B: no main red for it" "$red_out" 'main_red_scenarios'
refuse "nor said" "$(cat "$work/gate.out")" '| main red:'
# The same commit solved and then not (a helper beside the scenario changed:
# another base cache key, the same blob): a flake or the host, never main
# red, which needs an ancestor.
red2=$work/main-red-same
mkdir -p "$red2"
cp "$scenarios/build" "$scenarios/protected" "$red2/"
printf '#!/bin/sh\n[ "$(cat "$(dirname "$0")/072.want")" = "$(cat marker 2>/dev/null)" ]\n' >"$red2/072.sh"
printf 'ok\n' >"$red2/072.want"
expect "072 is solved on A" \
    "$(export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$red_cache"; base=$red_a; base_only "$red2"; cat "$work/gate.out")" \
    '| == base: 072.sh: solved'
printf 'other\n' >"$red2/072.want"
refuse "unsolved on that very commit: not main red" \
    "$(export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$red_cache"; base=$red_a; base_only "$red2"; cat "$work/gate.out")" \
    '| main red:'
expect "it was judged again there" "$(cat "$work/gate.out")" '| == base: 072.sh: unsolved'
# The index is hidden from the sandbox like the base cache (a tmpfs over it;
# this test's caches lie under a TMPDIR the sandbox does not empty): a
# scenario that looks into it sees nothing, so candidate code can neither
# read nor forge what main solved.
red3=$work/main-red-forge
mkdir -p "$red3"
cp "$scenarios/build" "$scenarios/protected" "$red3/"
printf '#!/bin/sh
echo "index holds: $(ls -A %s 2>/dev/null | tr "\\n" " ")."
' \
    "$red_cache/spawn/gate-solved/$(git -C "$seed" rev-list --max-parents=0 "$red_a" | LC_ALL=C sort | head -n 1)" >"$red3/073.sh"
expect "the index holds 071 and 072 on the host" \
    "$(ls -A "$red_cache/spawn/gate-solved/$(git -C "$seed" rev-list --max-parents=0 "$red_a" | LC_ALL=C sort | head -n 1)" | tr '\n' ' ')" '071.sh'
expect "a scenario that looks into the index: accepted" \
    "$(export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$red_cache"; base=$red_b; judge "$red_c" "" "$red3")" \
    'exit=0 {"verdict":"accepted"'
expect "and sees it empty" "$(cat "$work/gate.out")" '| index holds: .'
refuse "never what main solved" "$(cat "$work/gate.out")" 'index holds: 07'
git -C "$seed" checkout -q main 2>/dev/null || git -C "$seed" checkout -q -B main "$base"

# 25. A task-only scenario (`# rsi: task-only`, 029: a gate cannot judge it)
# is run only as its own task's scenario: 081 passes on the base and fails
# on the candidate, which would be a regression, yet it is neither run on
# the base for another gate nor as a regression, so the candidate is
# accepted; as the task's scenario, 082 is run as any task scenario (fails
# on the base, passes on the candidate).
only=$work/task-only
mkdir -p "$only"
cp "$scenarios/build" "$scenarios/protected" "$only/"
printf '#!/bin/sh\n# rsi: task-only\n[ "$PWD" = "$SPAWN_BASELINE_TREE" ]\n' >"$only/081.sh"
printf '#!/bin/sh\n# rsi: task-only\n[ "$(cat value)" = two ] || exit 1\n' >"$only/082.sh"
expect "a task-only scenario the candidate fails is no regression: accepted" \
    "$(judge "$good" "" "$only")" 'exit=0 {"verdict":"accepted"'
refuse "nor run on the base or the candidate" "$(cat "$work/gate.out")" '081.sh'
expect "as its own task's scenario it is judged: accepted" \
    "$(judge "$good" "$only/082.sh" "$only")" 'exit=0 {"verdict":"accepted"'
expect "run as the task's" "$(cat "$work/gate.out")" 'candidate: 082.sh must pass'

echo "gate: all cases pass"
