#!/bin/sh
# Tests of the gate (scripts/rsi/gate.sh) on throwaway repositories: no
# cargo, no database. Prints one line per case and exits non-zero on the
# first wrong verdict.
#
# usage: scripts/rsi/test-gate.sh
set -eu

gate=$(cd "$(dirname "$0")" && pwd)/gate.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

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
# a ratchet scenario that always fails, and a protected path.
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
cat >"$scenarios/009.sh" <<'EOF'
#!/bin/sh
# rsi: ratchet
exit 1
EOF
printf 'judge/\n# a comment\n\n' >"$scenarios/protected"

branch() {
    git -C "$seed" checkout -q -B "$1" "$base"
}
verdict() {
    sh "$gate" "$repo" "$base" "$1" "$scenarios" ${2:+"$2"} | tail -n 1
}
expect() {
    case "$2" in
    *"$3"*) echo "ok   $1" ;;
    *)
        echo "FAIL $1: expected $3 in $2"
        exit 1
        ;;
    esac
}

branch good
printf 'two\n' >"$seed/value"
good=$(commit good)
expect "red then green lands" "$(verdict "$good" "$scenarios/007.sh")" '"verdict":"accepted"'
expect "no task scenario: regression only" "$(verdict "$good")" '"verdict":"accepted"'

expect "a task scenario already green on the base" "$(verdict "$good" "$scenarios/check-built.sh")" 'already passes'

branch protected
printf 'two\n' >"$seed/value"
printf 'weaker\n' >"$seed/judge/rule"
touched=$(commit protected)
expect "protected path refused" "$(verdict "$touched" "$scenarios/007.sh")" 'protected'

branch buildtime
printf 'two\n' >"$seed/value"
printf 'fn main() {}\n' >"$seed/build.rs"
buildtime=$(commit buildtime)
expect "build-time code refused" "$(verdict "$buildtime")" 'build-time'

branch regress
printf 'two\n' >"$seed/value"
mkdir -p "$seed/x"
commit regress >/dev/null
# A regression: the build hook copies `value`, so break the candidate by
# making the check fail through a tree that has no value at all.
git -C "$seed" rm -q value
printf 'two\n' >"$seed/other"
regress=$(commit regress-2)
expect "a failing check rejects" "$(verdict "$regress")" '"verdict":"rejected"'

expect "relative scenarios refused" "$(sh "$gate" "$repo" "$base" "$good" scenarios | tail -n 1)" 'not absolute'
echo "gate: all cases pass"
