#!/bin/sh
# Tests of the gate (scripts/rsi/gate.sh) on throwaway repositories: no
# cargo, no database. Prints one line per case and exits non-zero on the
# first wrong verdict. Needs bwrap, as the gate does.
#
# usage: scripts/rsi/test-gate.sh
set -eu

gate=$(cd "$(dirname "$0")" && pwd)/gate.sh
work=$(mktemp -d)
# A file in the host's /tmp, which the sandbox must not show.
marker=$(mktemp /tmp/test-gate-marker.XXXXXX)
trap 'rm -rf "$work" "$marker"' EXIT

# The owner's home, with secrets the candidate must not read.
export HOME="$work/home"
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
# print `exit=<status> <verdict file>`. The gate's output is in gate.out.
integrator=$work/integrator
judge() {
    rm -rf "$integrator"
    mkdir -m 700 "$integrator"
    : >"$integrator/stdout"
    status=0
    SPAWN_GATE_VERDICT=$integrator/verdict sh "$gate" "$repo" "$base" "$1" "${3:-$scenarios}" ${2:+"$2"} \
        >"$work/gate.out" 2>&1 || status=$?
    echo "exit=$status $(cat "$integrator/verdict" 2>/dev/null || true)"
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
expect "the verdict file and stdout agree" "$(tail -n 1 "$work/gate.out")" '{"verdict":"accepted"}'
expect "no task scenario: regression only" "$(judge "$good")" 'exit=0 {"verdict":"accepted"}'

expect "a task scenario already green on the base" "$(judge "$good" "$scenarios/check-built.sh")" 'already passes'

branch protected
printf 'two\n' >"$seed/value"
printf 'weaker\n' >"$seed/judge/rule"
touched=$(commit protected)
expect "protected path refused" "$(judge "$touched" "$scenarios/007.sh")" 'protected'

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
[ "${SPAWN_TEST_DATABASE_URL-}" = postgres://test ] || { echo "no test database"; exit 1; }
[ -z "${OWNER_TOKEN-}" ] || { echo "owner token leaked"; exit 1; }
EOF
env_candidate=$(commit env)
export SPAWN_DATABASE_URL=postgres://owner SPAWN_TEST_DATABASE_URL=postgres://test OWNER_TOKEN=secret
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

# 3c. No network but loopback: a host listener and the live database port
# are unreachable; the sandbox's own loopback works.
listener=$work/listener.port
python3 -c '
import socket, sys, time
s = socket.socket(); s.bind(("127.0.0.1", 0)); s.listen()
print(s.getsockname()[1], flush=True)
time.sleep(600)' >"$listener" &
listener_pid=$!
trap 'kill "$listener_pid" 2>/dev/null; rm -rf "$work" "$marker"' EXIT
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

# 4. A build script by another name: `build = ` in any Cargo.toml, however
# spelled. `[build-dependencies]` and `build-override` are not build keys.
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
printf '[package]\nname = "x"\n[build-dependencies]\ncc = "1"\n[profile.dev.build-override]\nopt-level = 1\n' \
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

# 7. The candidate may not weaken its own tests: the count of test
# attributes minus ignores may not fall. A moved test is not a drop.
branch tests-dropped
printf 'two\n' >"$seed/value"
sed -i '/#\[tokio::test\]/,/async fn two/d' "$lib"
dropped=$(commit tests-dropped)
expect "a dropped test is refused" "$(judge "$dropped")" 'drops or ignores tests (2 -> 1)'
branch tests-ignored
printf 'two\n' >"$seed/value"
sed -i 's/^    #\[test\]$/    #[test]\n    #[ignore = "flaky"]/' "$lib"
ignored=$(commit tests-ignored)
expect "an ignored test is refused" "$(judge "$ignored")" 'drops or ignores tests (2 -> 1)'
branch tests-cfg-ignored
printf 'two\n' >"$seed/value"
sed -i 's/^    #\[tokio::test\]$/    #[tokio::test] #[cfg_attr(all(), ignore)]/' "$lib"
cfg_ignored=$(commit tests-cfg-ignored)
expect "a test ignored through cfg_attr is refused" "$(judge "$cfg_ignored")" 'drops or ignores tests'
branch tests-moved
printf 'two\n' >"$seed/value"
sed -i '/#\[tokio::test\]/,/async fn two/d' "$lib"
mkdir -p "$seed/crates/lib/tests"
printf '#[tokio::test]\nasync fn two() {}\n' >"$seed/crates/lib/tests/two.rs"
moved=$(commit tests-moved)
expect "a moved test is not a drop" "$(judge "$moved")" 'exit=0 {"verdict":"accepted"}'

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

# 6. A reason with a quote stays valid JSON.
quoted_scenarios=$work/quoted
cp -r "$scenarios" "$quoted_scenarios"
printf '#!/bin/sh\nexit 1\n' >"$quoted_scenarios/check-\"q\".sh"
expect "a quote in a reason is escaped" "$(judge "$good" "" "$quoted_scenarios")" \
    '{"verdict":"rejected","reason":"regression: check-\"q\".sh fails on the candidate"}'

echo "gate: all cases pass"
