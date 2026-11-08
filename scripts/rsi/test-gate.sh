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
# shellcheck disable=SC2086 # pids
trap 'kill $background 2>/dev/null || true; rm -rf "$work" "$marker" "$masked"' EXIT

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
    'exit=6 {"host_fault":true,"scenario":"check-broken.sh","on":"candidate and base","runs":"1, then 1 on the candidate; 1 on the base"'
expect "the log says so" "$(cat "$work/gate.out")" \
    'check-broken.sh fails on the candidate and on the base: a host or harness fault, no verdict'
expect "with every base result cached the base is built for it" \
    "$(judge "$good" "" "$broken_scenarios")" 'exit=6 {"host_fault":true,"scenario":"check-broken.sh"'
expect "the cached run still judged it on the base" "$(cat "$work/gate.out")" \
    'check-broken.sh fails on the base too (1)'
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
trap 'kill $background 2>/dev/null || true; rm -f /tmp/.s.PGSQL.$$ /run/user/$(id -u)/test-gate-$$.sock /var/tmp/test-gate-$$.sock; rm -rf "$work" "$marker"' EXIT
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
    until [ -e "$hold.held" ] || [ "$held" -ge 600 ]; do sleep 0.05; held=$((held + 1)); done
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
    # Any other bwrap failure stays a failure: here the masked path is
    # still there, as a file where the directory was, so bwrap fails on it
    # and the gate does not start the sandbox again.
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
        until [ -e "$hold.held" ] || [ "$held" -ge 600 ]; do sleep 0.05; held=$((held + 1)); done
        kill "$vanish_listener"
        rm -rf "$vanish"
        : >"$vanish"
        rm -f "$hold"
    ) &
    vanish_watcher=$!
    replaced=$(
        export SPAWN_GATE_TEST_HOLD="$hold"
        unset TMPDIR
        judge "$good"
    )
    wait "$vanish_watcher" 2>/dev/null || true
    refuse "a mask path still there (a file now) is no race: no second start" \
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
        until [ -s "$hold.held" ] || [ "$held" -ge 600 ]; do sleep 0.05; held=$((held + 1)); done
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
    # run (the hold file says `base-run`), the mask path made a file, so
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
        until [ -s "$hold.held" ] || [ "$held" -ge 600 ]; do sleep 0.05; held=$((held + 1)); done
        kill "$vanish_listener"
        rm -rf "$vanish"
        : >"$vanish"
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
else
    echo "note: $vanish was not under a scanned directory (TMPDIR covers it): no mask, no hold"
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
expect "and kept as cannot_judge" "$(grep -l '^cannot_judge$' "$cache"/* | wc -l)" '1'
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
# gate takes for a harness fault (no verdict, exit 4).
expect "an unlisted directory under \$HOME is not visible" \
    "$(judge "$good" "" "$unlisted_scenarios")" 'exit=6 {"host_fault":true,"scenario":"check-owner-inputs.sh"'
expect "on the candidate or the base" "$(cat "$work/gate.out")" \
    'check-owner-inputs.sh fails on the base too (1)'
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
    sed -E -e 's/[0-9]+ms/Nms/g' -e 's/[0-9]+ s\b/N s/g' -e '/^\| (base|candidate) runs: /d' \
        -e '/^\| gate time: /d' -e '/^\| sockets hidden from the sandbox: /d' \
        -e '/^\| host scenario slots: /d' \
        -e '/^\| waited [0-9.]+ s for host scenario slot /d' \
        -e '/^\| [a-z]+\.[0-9]+: (still )?waiting for a host scenario slot /d' "$work/gate.out"
}
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
expect "the gate time line counts the runs" "$(cat "$work/gate.out")" '(4 scenario runs, 0 results from the cache)'
expect "a bad SPAWN_GATE_JOBS is no verdict" "$(with_jobs 0 bad "$good" "$par")" 'exit=3 '
expect "and says why" "$(cat "$work/gate.out")" 'SPAWN_GATE_JOBS is a whole number from 1'

# 16b. A failing scenario among passing ones: the first failure in the
# scenario order is the verdict, whichever ended first, and the runs after
# it stop. 062 fails slowly, 063 at once, 064 would run for 29 s.
pfail=$work/parallel-fail
mkdir -p "$pfail"
cp "$scenarios/protected" "$scenarios/build" "$pfail/"
on_candidate() { # <name> <the candidate's commands>: passes on the base
    printf '#!/bin/sh\n[ "$(cat value)" = one ] && exit 0\n%s\n' "$2" >"$pfail/$1"
}
on_candidate 061.sh 'sleep 2'
on_candidate 062.sh 'sleep 3; echo "062 failed"; exit 1'
on_candidate 063.sh 'echo "063 failed"; exit 1'
on_candidate 064.sh 'sleep 29.25'
started=$(date +%s)
expect "the first failure in order is the verdict, not the first to end" \
    "$(with_jobs 4 pfail "$good" "$pfail")" '"reason":"regression: 062.sh fails on the candidate"'
expect "the runs after it were stopped, not waited for" "$([ $(($(date +%s) - started)) -lt 25 ] && echo early)" early
refuse "and their blocks are not printed" "$(cat "$work/gate.out")" '== regression: 063.sh'
refuse "nor is anything after them" "$(cat "$work/gate.out")" '== regression: 064.sh'
expect "the blocks before it are" "$(cat "$work/gate.out")" '| == regression: 061.sh must still pass'
sleep 1
refuse "no run of the gate is left" "$(ps -eo args | grep -F 'sleep 29.25' | grep -v grep || true)" 'sleep'

# 16c. A scenario on the serial list runs alone: each scenario marks itself
# running in the tree, and one that sees a neighbour it must not have exits 2
# (a harness error on the base: no verdict).
alone=$work/alone
mkdir -p "$alone"
cp "$scenarios/protected" "$scenarios/build" "$alone/"
for name in 071.sh 072.sh 073.sh; do
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
touch running.$name
check
sleep 1
check
rm -f running.$name
EOF
done
printf '# run alone\n071.sh\n' >"$alone/serial"
expect "a scenario on the serial list runs alone, on the base and the candidate" \
    "$(with_jobs 4 alone "$good" "$alone")" 'exit=0 {"verdict":"accepted"}'
expect "the others ran side by side" "$([ "$(wall_of base)" -lt "$(own_of base)" ] && echo overlapped)" overlapped
expect "the same results again: all from the cache" \
    "$(with_jobs 4 alone "$good" "$alone" >/dev/null; cat "$work/gate.out")" \
    '| base results from the cache: 071.sh 072.sh 073.sh; run on the base: none'
printf '072.sh\n' >>"$alone/serial"
expect "one put on the list is judged again, alone" \
    "$(with_jobs 4 alone "$good" "$alone" >/dev/null; cat "$work/gate.out")" \
    '| base results from the cache: 071.sh 073.sh; run on the base: 072.sh'
rm "$alone/serial"
expect "the control: without the list, 071 meets its neighbours" \
    "$(with_jobs 4 alone-control "$good" "$alone")" 'exit=6 {"host_fault":true,'

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
printf '#!/bin/sh\nsleep 29.5\n' >"$abort/093.sh"
started=$(date +%s)
expect "a harness error among parallel base runs ends the gate: a host fault" \
    "$(with_jobs 4 abort "$good" "$abort")" 'exit=6 {"host_fault":true,'
expect "without waiting for the runs after it" "$([ $(($(date +%s) - started)) -lt 25 ] && echo early)" early
expect "after the blocks before it" "$(cat "$work/gate.out")" '| == base: 091.sh: solved'
expect "and says why" "$(cat "$work/gate.out")" '092.sh exited 3 on the base'
sleep 1
refuse "no run of the gate is left" "$(ps -eo args | grep -F 'sleep 29.5' | grep -v grep || true)" 'sleep'

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

# 17b. A gate that starts while a base-only run judges the same key waits
# for its result instead of judging it again (the key's lock).
slow=$work/slow
mkdir -p "$slow"
cp "$scenarios/protected" "$scenarios/build" "$slow/"
printf '#!/bin/sh\n[ "$(cat value)" = one ] && sleep 12\nexit 0\n' >"$slow/111.sh"
(
    export SPAWN_GATE_JOBS=2 XDG_CACHE_HOME="$work/caches/locked"
    sh "$gate" --base-only "$repo" "$base" "$slow" >"$work/warm.out" 2>&1
) &
warming=$!
waited=0
until ls "$work/caches/locked/spawn/gate-base/"*.lock >/dev/null 2>&1 || [ "$waited" -ge 600 ]; do
    sleep 0.1
    waited=$((waited + 1))
done
expect "a gate started meanwhile" "$(with_jobs 2 locked "$good" "$slow")" 'exit=0 {"verdict":"accepted"}'
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
    # A gate ended by a signal takes its server and its runs with it.
    printf '#!/bin/sh\n[ "$(cat value)" = one ] || sleep 29.75\n' >"$own/check-long.sh"
    rm -rf "$integrator"
    mkdir -m 700 "$integrator"
    (
        export "$pg_var=$pg_source" SPAWN_GATE_JOBS=4 XDG_CACHE_HOME="$work/caches/own-$kind" TMPDIR="$pgtmp"
        exec sh "$gate" "$repo" "$base" "$good" "$own" >"$work/gate.out" 2>&1
    ) &
    signalled=$!
    waited=0
    until ps -eo args | grep -F 'sleep 29.75' | grep -qv grep || [ "$waited" -ge 600 ]; do
        sleep 0.1
        waited=$((waited + 1))
    done
    kill -TERM "$signalled"
    wait "$signalled" || :
    sleep 1
    refuse "$kind: a gate ended by a signal leaves no run" "$(ps -eo args | grep -F 'sleep 29.75' | grep -v grep || true)" 'sleep'
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
cat >"$slots/check-slot.sh" <<'SLOT'
#!/bin/sh
echo "slot-start $(date +%s%3N)"
sleep 2
echo "slot-end $(date +%s%3N)"
exit 0
SLOT
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
slot_run a 1 1 >/dev/null & slot_a=$!
slot_run b 1 1 >/dev/null & slot_b=$!
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
slot_run f 2 1 >/dev/null & slot_f=$!
slot_run g 2 1 >/dev/null & slot_g=$!
wait "$slot_f" "$slot_g"
expect "a host_jobs file outranks SPAWN_GATE_HOST_JOBS" \
    "$(overlaps $(slot_window "$work/slots-f.log" 1) $(slot_window "$work/slots-g.log" 1))" apart
expect "and its number is the one stated" "$(cat "$work/slots-f.log")" \
    '| host scenario slots: 1 (every gate on this host takes one per scenario run)'
refuse "and no no-file caveat is said" "$(cat "$work/slots-f.log")" "host_jobs file"
rm -f "$HOME/.cache/spawn/gate-slots/host_jobs"
slot_run c 2 1 >/dev/null & slot_c=$!
slot_run d 2 1 >/dev/null & slot_d=$!
wait "$slot_c" "$slot_d"
expect "two host slots: the same two runs overlap" \
    "$(overlaps $(slot_window "$work/slots-c.log" 1) $(slot_window "$work/slots-d.log" 1))" overlap
slot_run e 2 2
expect "one gate with two jobs gets both slots (no deadlock)" \
    "$(overlaps $(slot_window "$work/slots-e.log" 1) $(slot_window "$work/slots-e.log" 2))" overlap
rm -f "$slots/check-slot-two.sh"
# A gate killed as a whole process group: its scenario dies, the kernel
# drops its slot, and the next gate takes it at once.
slowslots=$work/slots-slow
mkdir -p "$slowslots"
cp "$scenarios/protected" "$slowslots/"
printf '#!/bin/sh\nsleep 30\nexit 0\n' >"$slowslots/check-slow.sh"
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
    [ "$waited" -lt 100 ] || break
done
killed_gate=$(cat "$killed_pidfile" 2>/dev/null || true)
[ -n "$killed_gate" ] || { echo "FAIL the killed gate never started"; exit 1; }
waited=0
until ! flock -n "$slot_file" true 2>/dev/null; do
    sleep 0.1
    waited=$((waited + 1))
    [ "$waited" -lt 300 ] || break
done
[ "$waited" -lt 300 ] || { echo "FAIL the killed gate never took its slot"; exit 1; }
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
    [ "$waited" -lt 100 ] || break
done
expect "a killed gate leaves its slot free" "$([ "$waited" -lt 100 ] && echo free || echo held)" free
printf '#!/bin/sh\nsleep 1\nexit 0\n' >"$slowslots/check-slow.sh"
killed_run=0
started=$(date +%s)
XDG_CACHE_HOME="$work/caches/slots-kill" SPAWN_GATE_JOBS=1 SPAWN_GATE_HOST_JOBS=1 \
    sh "$gate" "$repo" "$base" "$good" "$slowslots" >"$work/slots-after-kill.log" 2>&1 || killed_run=$?
expect "and the next gate runs at once: exit 0" "$killed_run" 0
expect "without waiting for a slot" "$([ $(( $(date +%s) - started )) -lt 20 ] && echo quick)" quick
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
printf '#!/bin/sh\nn=0\nwhile [ ! -e %s/release ] && [ "$n" -lt 1200 ]; do sleep 0.1; n=$((n + 1)); done\nexit 0\n' \
    "$waitfold" >"$waitfold/check-hold.sh"
XDG_CACHE_HOME="$work/caches/slots-hold" SPAWN_GATE_JOBS=1 SPAWN_GATE_HOST_JOBS=1 \
    sh "$gate" "$repo" "$base" "$good" "$waitfold" >"$work/slots-hold.log" 2>&1 &
hold=$!
waited=0
until ! flock -n "$slot_file" true 2>/dev/null; do
    sleep 0.1
    waited=$((waited + 1))
    [ "$waited" -lt 300 ] || break
done
[ "$waited" -lt 300 ] || { echo "FAIL the holder never took its slot"; exit 1; }
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
# The holder holds the one slot long enough for both later gates to be
# waiting before it lets go (each needs its exports and static checks
# first).
priohold=$work/prio-hold
mkdir -p "$priohold"
cp "$scenarios/protected" "$priohold/"
printf '#!/bin/sh\nsleep 15\nexit 0\n' >"$priohold/check-hold.sh"
prio_hold() { # <tag>: a normal gate holding the one slot for 15 s, in the background
    XDG_CACHE_HOME="$work/caches/prio-$1" SPAWN_GATE_JOBS=1 SPAWN_GATE_HOST_JOBS=1 \
        sh "$gate" "$repo" "$base" "$good" "$priohold" >"$work/prio-$1.log" 2>&1 &
    prio_holder=$!
    prio_n=0
    until ! flock -n "$slot_file" true 2>/dev/null; do
        sleep 0.1
        prio_n=$((prio_n + 1))
        [ "$prio_n" -lt 300 ] || { echo "FAIL the holder ($1) never took its slot"; exit 1; }
    done
}
prio_gate() { # <tag> <priority>: a gate on the slot scenario, in the background
    XDG_CACHE_HOME="$work/caches/prio-$1" SPAWN_GATE_JOBS=1 SPAWN_GATE_HOST_JOBS=1 \
        SPAWN_GATE_SLOT_PRIORITY="$2" \
        sh "$gate" "$repo" "$base" "$good" "$slots" >"$work/prio-$1.log" 2>&1 &
}
prio_said() { # <tag> <text>: wait (up to 30 s) until the gate's log says it
    prio_n=0
    until grep -qF "$2" "$work/prio-$1.log" 2>/dev/null; do
        sleep 0.1
        prio_n=$((prio_n + 1))
        [ "$prio_n" -lt 300 ] || { echo "FAIL $1 never said: $2"; exit 1; }
    done
}
prio_hold h1
prio_gate normal normal
prio_normal=$!
prio_said normal "waiting for a host scenario slot (all 1 taken"
prio_gate low low
prio_low=$!
prio_said low "waiting for a host scenario slot (claimed last"
prio_status=0
wait "$prio_holder" "$prio_normal" "$prio_low" || prio_status=$?
expect "the three gates finish: exit 0" "$prio_status" 0
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
flock "$slot_file" sh -c 'until [ -e "$1" ]; do sleep 0.1; done' _ "$invert_release" &
invert_holder=$!
prio_n=0
until ! flock -n "$slot_file" true 2>/dev/null; do
    sleep 0.1
    prio_n=$((prio_n + 1))
    [ "$prio_n" -lt 300 ] || { echo "FAIL the inversion case's holder never took the slot"; exit 1; }
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
# rejected as such.
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
else
    echo "note: no PostgreSQL programs in $schema_pg: the schema upgrade case is not run"
fi

echo "gate: all cases pass"
