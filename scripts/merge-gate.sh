#!/usr/bin/env bash
# Gate one worker branch before a human merges it.
#
# usage: scripts/merge-gate.sh <bare-repo> <branch> [<base>]
#
# The same discipline as the RSI gate, applied to fleet work: the branch is
# exported (never checked out here), it may not touch the controller's files,
# and it must pass, on its own tree:
#   - cargo build and cargo clippy --workspace --all-targets with no warning;
#   - the web build, when the branch touches web/ (dependency changes are
#     refused: a new package is reviewed by hand);
#   - cargo test --workspace, database tests included (SPAWN_TEST_DATABASE_URL);
#   - scripts/mvp-e2e.sh (the whole decision ladder);
#   - every non-ratchet RSI scenario in the controller's rsi/scenarios.
# Prints PASS or FAIL with the failing step; exit status 0 only on PASS.
set -euo pipefail

repo=${1:?bare repository}
branch=${2:?branch}
base=${3:-main}
root=$(cd "$(dirname "$0")/.." && pwd)
test_url=${SPAWN_TEST_DATABASE_URL:?SPAWN_TEST_DATABASE_URL is required}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fail() { echo "merge-gate: FAIL ($branch): $*"; exit 1; }
step() { echo "merge-gate: $*"; }

git --git-dir="$repo" rev-parse --verify --quiet "refs/heads/$branch" >/dev/null || fail "no branch $branch"
touched=$(git --git-dir="$repo" diff --name-only "$base" "$branch" -- rsi scripts/rsi)
[ -z "$touched" ] || fail "touches controller files: $touched"
buildtime=$(git --git-dir="$repo" diff --name-only "$base" "$branch" | grep -E '(^|/)build\.rs$|(^|/)\.cargo/' || true)
[ -z "$buildtime" ] || fail "adds build-time code: $buildtime"

tree=$work/tree
mkdir -p "$tree"
git --git-dir="$repo" archive "$branch" | tar -x -C "$tree"
export CARGO_TARGET_DIR=$work/target

step "build and clippy"
(cd "$tree" && cargo build --quiet --workspace --all-targets) >"$work/build.log" 2>&1 || { tail -30 "$work/build.log"; fail "build"; }
(cd "$tree" && cargo clippy --quiet --workspace --all-targets) >"$work/clippy.log" 2>&1 || { tail -30 "$work/clippy.log"; fail "clippy errors"; }
if grep -qE '^(warning|error)' "$work/clippy.log"; then grep -E '^(warning|error)' -A4 "$work/clippy.log" | head -30; fail "clippy warnings"; fi

if git --git-dir="$repo" diff --name-only "$base" "$branch" -- web | grep -q .; then
  step "web build"
  webdeps=$(git --git-dir="$repo" diff --name-only "$base" "$branch" -- web/package.json web/pnpm-lock.yaml)
  [ -z "$webdeps" ] || fail "changes web dependencies (review by hand): $webdeps"
  [ -d "$root/web/node_modules" ] || fail "web build needs $root/web/node_modules (install once by hand)"
  ln -s "$root/web/node_modules" "$tree/web/node_modules"
  (cd "$tree/web" && npm run build) >"$work/web.log" 2>&1 || { tail -30 "$work/web.log"; fail "web build"; }
fi

step "tests"
(cd "$tree" && SPAWN_TEST_DATABASE_URL=$test_url cargo test --quiet --workspace) >"$work/test.log" 2>&1 || { grep -E 'FAILED|panicked' -A5 "$work/test.log" | head -40; fail "tests"; }

step "mvp e2e"
(cd "$tree" && SPAWN_BIN="$CARGO_TARGET_DIR/debug/spawn" SPAWN_DATABASE_URL=$test_url bash scripts/mvp-e2e.sh) >"$work/e2e.log" 2>&1 || { tail -20 "$work/e2e.log"; fail "mvp e2e"; }

step "rsi scenarios"
for scenario in "$root"/rsi/scenarios/*.sh; do
  grep -q '^# rsi: ratchet' "$scenario" && continue
  (cd "$tree" && SPAWN_BIN="$CARGO_TARGET_DIR/debug/spawn" sh "$scenario") >"$work/scenario.log" 2>&1 ||
    { tail -20 "$work/scenario.log"; fail "scenario $(basename "$scenario")"; }
done

echo "merge-gate: PASS ($branch, $(git --git-dir="$repo" diff --shortstat "$base" "$branch"))"
