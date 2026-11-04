#!/bin/sh
# Check: the web UI builds. A change to its dependencies is refused: a new
# package is reviewed by a human. The build uses the controller's installed
# node_modules.
set -eu
[ -f web/package.json ] || exit 0
controller=$(cd "$(dirname "$0")/../.." && pwd)
for file in web/package.json web/pnpm-lock.yaml; do
    if ! cmp -s "$file" "$SPAWN_BASELINE_TREE/$file"; then
        echo "web dependencies changed ($file): a human reviews new packages"
        exit 1
    fi
done
[ -d "$controller/web/node_modules" ] || { echo "no node_modules in $controller/web"; exit 1; }
rm -rf web/node_modules
ln -s "$controller/web/node_modules" web/node_modules
cd web && npm run build
