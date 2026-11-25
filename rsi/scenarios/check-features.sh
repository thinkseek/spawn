#!/bin/sh
# Check (issue #98, PR-3): the tree under test keeps its feature map whole.
# `spawn features check` (the tree's own SPAWN_BIN, crates/spawn-cli/src/
# feature_inventory.rs) walks the tree and docs/feature-map/registry.json and
# must find no in-scope file that no feature names (wild), no registry path
# that is gone (stale), no feature with neither parent nor relation (orphans)
# and no relation that points nowhere, at itself or round a cycle
# (invalid_relations). A candidate that adds, renames or removes a file says
# so in the registry in the same commit, or this check refuses it.
#
# No database, no network, no HOME: the command runs under `env -i` with PATH
# alone, reads the tree and the registry, and writes nothing (`check`, never
# `associate`). The registry is the tree's, so a candidate may correct it;
# the rule (every file covered) is the controller's, here. What the gate
# writes into the tree while it judges is excluded by the registry's own
# boundary (target/, node_modules/, web/dist/, .spawn/, logs), so other
# checks running beside this one leave no wild file. About 0.1 s.
#
# Exit status: 0 every count is 0; 1 a count is not, or the command failed
# on the tree (the report says which file); 2 a harness fault (no SPAWN_BIN,
# not run from a tree with a registry).
set -eu
harness() {
    echo "check-features: harness: $*" >&2
    exit 2
}
[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
tree=$(pwd)
registry=$tree/docs/feature-map/registry.json
[ -f "$registry" ] || {
    echo "check-features: no docs/feature-map/registry.json in the tree under test"
    exit 1
}
status=0
report=$(env -i PATH="$PATH" "$SPAWN_BIN" features check --root "$tree" --registry "$registry" 2>&1) || status=$?
printf "%s\n" "$report"
summary=$(printf "%s\n" "$report" | grep "^feature coverage " | head -n 1) || true
[ -n "$summary" ] || {
    echo "check-features: FAIL: no summary line from spawn features check (exit $status)"
    exit 1
}
bad=""
for count in wild stale orphans invalid_relations; do
    case " $summary " in
    *" $count=0 "*) ;;
    *) bad="$bad $count" ;;
    esac
done
if [ "$status" -ne 0 ] || [ -n "$bad" ]; then
    echo "check-features: FAIL (exit $status; not 0:${bad:- none}): name each file above in a feature of docs/feature-map/registry.json, or drop the stale path"
    exit 1
fi
echo "check-features: ok"
