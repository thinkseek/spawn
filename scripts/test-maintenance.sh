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

# The gate's base cache: entries used lately stay, one untouched for 20
# days goes, and so does an unfinished write that old; a file not named
# like an entry, and a directory named like one, stay.
# Under a dot directory, as the default ~/.cache is: a match on the whole
# path once took every server template there for a half-made one.
cache=$work/.cache/spawn/gate-base
mkdir -p "$cache"
hex=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
stale() { touch -h -d '20 days ago' "$@"; }
echo solved >"$cache/a$(printf '%s' "$hex" | cut -c2-)"   # used lately
echo unsolved >"$cache/b$(printf '%s' "$hex" | cut -c2-)" # untouched for 20 days
stale "$cache/b$(printf '%s' "$hex" | cut -c2-)"
echo solved >"$cache/c$(printf '%s' "$hex" | cut -c2-).4242"
stale "$cache/c$(printf '%s' "$hex" | cut -c2-).4242"
echo owner >"$cache/notes"
stale "$cache/notes"
mkdir "$cache/d$(printf '%s' "$hex" | cut -c2-)"
stale "$cache/d$(printf '%s' "$hex" | cut -c2-)"

# The private test servers' templates: one a gate started from lately
# stays, one no gate started from for 20 days goes with its lock, one a gate
# is making (no `ready`) stays; a half-made one older than a day goes, a
# fresh one stays.
servers=$work/.cache/spawn/gate-pg
mkdir -p "$servers"
for t in a b c; do
    mkdir "$servers/$t$(printf '%s' "$hex" | cut -c2-)"
    touch "$servers/$t$(printf '%s' "$hex" | cut -c2-).lock"
done
touch "$servers/a$(printf '%s' "$hex" | cut -c2-)/ready"
touch "$servers/b$(printf '%s' "$hex" | cut -c2-)/ready"
stale "$servers/b$(printf '%s' "$hex" | cut -c2-)/ready" "$servers/b$(printf '%s' "$hex" | cut -c2-)"
stale "$servers/a$(printf '%s' "$hex" | cut -c2-)" "$servers/c$(printf '%s' "$hex" | cut -c2-)"
mkdir "$servers/.d$(printf '%s' "$hex" | cut -c2-).Ab12Cd" "$servers/.e$(printf '%s' "$hex" | cut -c2-).Ef34Gh"
stale "$servers/.d$(printf '%s' "$hex" | cut -c2-).Ab12Cd"

run() {
    env TMPDIR="$tmp" SPAWN_BACKUP_DIR="$backups" XDG_CACHE_HOME="$work/.cache" "$@" \
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
before_cache=$(listing "$cache")
before_servers=$(listing "$servers")
run SPAWN_MAINTENANCE_DRY_RUN=1
expect "a dry run deletes nothing in TMPDIR" "$(listing "$tmp")" "$before_tmp"
expect "nor in the backups" "$(listing "$backups")" "$before_backups"
expect "nor in the base cache" "$(listing "$cache")" "$before_cache"
expect "nor in the servers' templates" "$(listing "$servers")" "$before_servers"
expect "it says what would go" "$(grep -c 'would remove' "$work/out")" "7"

status=0
run || status=$?
expect "a run succeeds" "$status" "0"
expect "only the old gate scratch directory goes" "$(listing "$tmp")" \
    "other-project spawn-gate-other spawn-gate.def456 spawn-gate.fil123 spawn-gate.lnk123 spawn-gate.toolong1 "
expect "a symlink's target is untouched" "$(listing "$elsewhere")" "keep "
expect "the newest five deploy dumps stay, and every other file" "$(listing "$backups")" \
    "missions-20261104T1126.sql pg-backup.state spawn-20261101-000000.dump spawn-20261101-010101.dump.gpg spawn-20261103-010101.dump spawn-20261104-010101.dump spawn-20261104T1401 spawn-20261105-010101.dump spawn-20261106-010101.dump spawn-20261107-010101.dump spawn-auto-20261101-000000.dump spawn-auto-20261102-000000.dump spawn-full-20261104T1216.dump "

expect "base cache entries no gate used for 14 days go, and nothing else" "$(listing "$cache")" \
    "a$(printf '%s' "$hex" | cut -c2-) d$(printf '%s' "$hex" | cut -c2-) notes "
expect "a recently used entry is intact" "$(cat "$cache/a$(printf '%s' "$hex" | cut -c2-)")" "solved"
expect "a template a gate started from lately stays, under a dot directory too" "$(listing "$servers")" \
    ".e$(printf '%s' "$hex" | cut -c2-).Ef34Gh a$(printf '%s' "$hex" | cut -c2-) a$(printf '%s' "$hex" | cut -c2-).lock c$(printf '%s' "$hex" | cut -c2-) c$(printf '%s' "$hex" | cut -c2-).lock "
expect "with its ready" "$(listing "$servers/a$(printf '%s' "$hex" | cut -c2-)")" "ready "

after_backups=$(listing "$backups")
run || status=$?
expect "a second run removes nothing more" "$(listing "$backups")" "$after_backups"

status=0
run SPAWN_MAINTENANCE_SCRATCH_AGE_MIN=old SPAWN_MAINTENANCE_KEEP_DUMPS=2 || status=$?
expect "a failed step fails the run" "$status" "1"
expect "after the other steps ran" "$(listing "$backups" | grep -o 'spawn-2026110[0-9]-010101.dump ' | tr -d '\n')" \
    "spawn-20261106-010101.dump spawn-20261107-010101.dump "

status=0
run SPAWN_MAINTENANCE_KEEP_DUMPS=all || status=$?
expect "a count that is not a number fails, deleting nothing" "$status:$(listing "$backups" | grep -c 'spawn-2026110' || true)" "1:1"

echo "maintenance: all cases pass"
