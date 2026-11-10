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
# A dependency set the owner reviewed is named by the sha256 of package.json
# and the lockfile together, on the first line of web-dependencies-reviewed
# beside this script; only that exact set passes.
reviewed="$(dirname "$0")/web-dependencies-reviewed"
set_sha=$(cat web/package.json web/pnpm-lock.yaml | sha256sum | cut -d' ' -f1)
if [ -f "$reviewed" ] && [ "$(grep -v '^#' "$reviewed" | head -n 1)" = "$set_sha" ]; then
    echo "web dependencies: the owner reviewed this set"
else
    for file in web/package.json web/pnpm-lock.yaml; do
        if ! cmp -s "$file" "$SPAWN_BASELINE_TREE/$file"; then
            echo "web dependencies changed ($file): a human reviews new packages"
            exit 1
        fi
    done
fi
[ -d "$controller/web/node_modules" ] || { echo "no node_modules in $controller/web"; exit 1; }
rm -rf web/node_modules
ln -s "$controller/web/node_modules" web/node_modules
# The controller's node_modules is read-only in the gate's sandbox: Vite
# loads its config in memory instead of writing it to node_modules/.vite-temp.
cd web
# The UI's pure modules have tests of their own (web/test), run by node
# itself (it strips the types); no package is added for them.
node --test test/
npm run build -- --configLoader runner
