#!/bin/sh
# Check: scripts/storage-extents.py (objective 180's reading of real bytes)
# keeps its ordering rule: an extent counts in the first source that holds
# it, and a source after one that is over its bound is not measured, never a
# count that depends on how far the earlier walk got (cc-astra A82). Runs
# scripts/test-storage-extents.py: made-up extents and a made-up clock, real
# files only in its own scratch folder; nothing else is read. About 1 s.
# Exit 0 when every case passes; 1 otherwise, or when the files are missing.
set -eu
here=$(pwd)
for f in scripts/storage-extents.py scripts/test-storage-extents.py; do
    [ -f "$here/$f" ] || { echo "check-storage-extents: no $f in the tree"; exit 1; }
done
out=$(mktemp)
trap 'rm -f "$out"' EXIT
rc=0
python3 -I -B "$here/scripts/test-storage-extents.py" >"$out" 2>&1 || rc=$?
tail -n 3 "$out"
if [ "$rc" -eq 0 ] && grep -q '^OK$' "$out"; then
    echo "check-storage-extents: ok"
else
    echo "check-storage-extents: FAIL (exit $rc)"
    exit 1
fi
