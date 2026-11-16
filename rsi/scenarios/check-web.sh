#!/bin/sh
# Check: the web UI builds. A change to its dependencies is refused unless
# the owner reviewed that exact set (web-dependencies-reviewed). The build uses the controller's installed
# node_modules.
#
# What it needs of the host, read by the gate before anything runs
# (scripts/rsi/gate.sh "Preflight"): without it the build below cannot start.
# needs: controller:web/node_modules/
set -eu
[ -f web/package.json ] || exit 0
controller=$(cd "$(dirname "$0")/../.." && pwd)
# A dependency set the owner reviewed is named by the sha256 of package.json,
# the lockfile and pnpm's configuration files in web/ (whichever are there,
# each after a line naming it), on the first line of
# web-dependencies-reviewed beside this script; only that exact set passes.
# The configuration is in the set because the owner's installs read it
# (scripts/web-install.sh --strict=web, and the owner gate's install of a
# candidate's web/): a change to it is a change to what installs.
reviewed="$(dirname "$0")/web-dependencies-reviewed"
config="web/pnpm-workspace.yaml web/.npmrc web/.pnpmfile.cjs web/.pnpmfile.mjs web/pnpmfile.cjs"
set_sha=$({
    cat web/package.json web/pnpm-lock.yaml
    for file in $config; do
        if [ -e "$file" ] || [ -L "$file" ]; then
            printf '== %s\n' "$file"
            cat "$file"
        fi
    done
} | sha256sum | cut -d' ' -f1)
if [ -f "$reviewed" ] && [ "$(grep -v '^#' "$reviewed" | head -n 1)" = "$set_sha" ]; then
    echo "web dependencies: the owner reviewed this set"
else
    for file in web/package.json web/pnpm-lock.yaml $config; do
        if { [ -e "$file" ] || [ -L "$file" ] || [ -e "$SPAWN_BASELINE_TREE/$file" ] || [ -L "$SPAWN_BASELINE_TREE/$file" ]; } &&
            ! cmp -s "$file" "$SPAWN_BASELINE_TREE/$file"; then
            echo "web dependencies changed ($file): a human reviews new packages"
            exit 1
        fi
    done
fi
# web/test/ is the fleet's to add to and change (rsi/scenarios/protected),
# but not to shrink: a test file the base has is still there. Removing one
# is the owner's change, as removing a cargo test is (gate.sh's ratchet).
if [ -d "$SPAWN_BASELINE_TREE/web/test" ]; then
    removed=$(cd "$SPAWN_BASELINE_TREE" && find web/test -type f | while IFS= read -r test_file; do
        [ -f "$OLDPWD/$test_file" ] || printf '%s ' "$test_file"
    done)
    if [ -n "$removed" ]; then
        echo "FAILED web-test-removed: $removed"
        exit 1
    fi
fi
[ -d "$controller/web/node_modules" ] || { echo "no node_modules in $controller/web"; exit 1; }
rm -rf web/node_modules
ln -s "$controller/web/node_modules" web/node_modules
# The controller's node_modules is read-only in the gate's sandbox: Vite
# loads its config in memory instead of writing it to node_modules/.vite-temp.
cd web
# The UI's pure modules have tests of their own (web/test), run by node
# itself (it strips the types); no package is added for them.
# Node 22.23+ no longer accepts a test directory as a positional argument;
# no-arg run auto-discovers the test/ directory (node 20 and 22).
node --test
npm run build -- --configLoader runner
