#!/bin/sh
# Tests of scripts/maintenance.sh on a throwaway TMPDIR and backups
# directory: it removes only what Spawn made, and only what is old enough.
# Prints one line per case and exits non-zero on the first failure.
#
# usage: scripts/test-maintenance.sh
set -eu

maintenance=$(cd "$(dirname "$0")" && pwd)/maintenance.sh
work=$(mktemp -d)
trap 'chmod -R u+w "$work" 2>/dev/null; rm -rf "$work"' EXIT

tmp=$work/tmp
backups=$work/backups
elsewhere=$work/elsewhere
mkdir -p "$tmp" "$backups" "$elsewhere/keep"
old() { touch -h -d '2 days ago' "$@"; }

# TMPDIR: the gate's scratch, old and fresh, and everyone else's.
mkdir -p "$tmp/spawn-gate.abc123/tree/.git/objects/aa"
echo x >"$tmp/spawn-gate.abc123/tree/.git/objects/aa/bb"
chmod a-w "$tmp/spawn-gate.abc123/tree/.git/objects/aa"
old "$tmp/spawn-gate.abc123"
mkdir "$tmp/spawn-gate.def456"                       # fresh: a gate running now
mkdir "$tmp/spawn-gate-other" "$tmp/spawn-gate.toolong1" "$tmp/other-project"
old "$tmp/spawn-gate-other" "$tmp/spawn-gate.toolong1" "$tmp/other-project"
echo y >"$tmp/spawn-gate.fil123"                     # a file, not a directory
old "$tmp/spawn-gate.fil123"
ln -s "$elsewhere" "$tmp/spawn-gate.lnk123"          # a symlink is not followed
old "$tmp/spawn-gate.lnk123"

# Backups: seven deploy dumps, pg-backup's own, and the owner's files.
for ts in 20261101-010101 20261102-010101 20261103-010101 20261104-010101 \
    20261105-010101 20261106-010101 20261107-010101; do
    echo dump >"$backups/spawn-$ts.dump"
done
for name in spawn-auto-20261101-000000.dump spawn-auto-20261102-000000.dump \
    spawn-full-20261104T1216.dump missions-20261104T1126.sql spawn-20261104T1401 \
    pg-backup.state spawn-20261101-010101.dump.gpg; do
    echo owner >"$backups/$name"
done
mkdir "$backups/spawn-20261101-000000.dump"          # a directory, not a dump

# A `spawn` that records how it was called.
fake=$work/spawn
cat >"$fake" <<EOF
#!/bin/sh
echo "\$*" >"$work/spawn-args"
exit "\${FAKE_EXIT:-0}"
EOF
chmod +x "$fake"

run() {
    env TMPDIR="$tmp" SPAWN_BACKUP_DIR="$backups" SPAWN_PROGRAM="$fake" "$@" \
        sh "$maintenance" >"$work/out" 2>&1
}
listing() { (cd "$1" && ls -A | LC_ALL=C sort | tr '\n' ' '); }
expect() {
    if [ "$2" = "$3" ]; then
        echo "ok   $1"
    else
        echo "FAIL $1"
        echo "     expected: $3"
        echo "     got:      $2"
        sed 's/^/     | /' "$work/out"
        exit 1
    fi
}

before_tmp=$(listing "$tmp")
before_backups=$(listing "$backups")
run SPAWN_MAINTENANCE_DRY_RUN=1
expect "a dry run deletes nothing in TMPDIR" "$(listing "$tmp")" "$before_tmp"
expect "nor in the backups" "$(listing "$backups")" "$before_backups"
expect "nor collects chunks" "$(cat "$work/spawn-args" 2>/dev/null || echo none)" "none"
expect "it says what would go" "$(grep -c 'would remove' "$work/out")" "3"

status=0
run || status=$?
expect "a run succeeds" "$status" "0"
expect "chunks are collected with a grace period" "$(cat "$work/spawn-args")" "chunks gc --grace 120"
expect "only the old gate scratch directory goes" "$(listing "$tmp")" \
    "other-project spawn-gate-other spawn-gate.def456 spawn-gate.fil123 spawn-gate.lnk123 spawn-gate.toolong1 "
expect "a symlink's target is untouched" "$(listing "$elsewhere")" "keep "
expect "the newest five deploy dumps stay, and every other file" "$(listing "$backups")" \
    "missions-20261104T1126.sql pg-backup.state spawn-20261101-000000.dump spawn-20261101-010101.dump.gpg spawn-20261103-010101.dump spawn-20261104-010101.dump spawn-20261104T1401 spawn-20261105-010101.dump spawn-20261106-010101.dump spawn-20261107-010101.dump spawn-auto-20261101-000000.dump spawn-auto-20261102-000000.dump spawn-full-20261104T1216.dump "

after_backups=$(listing "$backups")
run || status=$?
expect "a second run removes nothing more" "$(listing "$backups")" "$after_backups"

status=0
run FAKE_EXIT=1 SPAWN_MAINTENANCE_KEEP_DUMPS=2 || status=$?
expect "a failed collection fails the run" "$status" "1"
expect "after the other steps ran" "$(listing "$backups" | grep -o 'spawn-2026110[0-9]-010101.dump ' | tr -d '\n')" \
    "spawn-20261106-010101.dump spawn-20261107-010101.dump "

status=0
run SPAWN_MAINTENANCE_KEEP_DUMPS=all || status=$?
expect "a count that is not a number fails, deleting nothing" "$status:$(listing "$backups" | grep -c 'spawn-2026110' || true)" "1:1"

echo "maintenance: all cases pass"
