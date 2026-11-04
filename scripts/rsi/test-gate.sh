#!/bin/sh
# Tests of the gate (scripts/rsi/gate.sh) on throwaway repositories: no
# cargo, no database. Prints one line per case and exits non-zero on the
# first wrong verdict.
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

# 6. A reason with a quote stays valid JSON.
quoted_scenarios=$work/quoted
cp -r "$scenarios" "$quoted_scenarios"
printf '#!/bin/sh\nexit 1\n' >"$quoted_scenarios/check-\"q\".sh"
expect "a quote in a reason is escaped" "$(judge "$good" "" "$quoted_scenarios")" \
    '{"verdict":"rejected","reason":"regression: check-\"q\".sh fails on the candidate"}'

echo "gate: all cases pass"
