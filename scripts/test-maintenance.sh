#!/bin/sh
# Tests of scripts/maintenance.sh on a throwaway TMPDIR and backups
# directory: it removes only what Spawn made, and only what is old enough;
# of the gate's scratch, only what its gate has left: a scratch whose gate
# runs stays however old, one whose gate is gone goes however fresh (the
# gate's record in it: pid, start time, boot), and one a process still has
# open stays. Prints one line per case and exits non-zero on the first
# failure.
#
# usage: scripts/test-maintenance.sh
set -eu

maintenance=$(cd "$(dirname "$0")" && pwd)/maintenance.sh
# shellcheck source=scripts/disk.sh
. "$(dirname "$0")/disk.sh"
work=$(mktemp -d)
holders=""
# shellcheck disable=SC2086 # the holders' pids, split
trap 'kill $holders 2>/dev/null || :; chmod -R u+w "$work" 2>/dev/null; rm -rf "$work"' EXIT

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

# The gate's own record in its scratch (disk.sh, gate_scratch): the scratch
# of a gate that runs stays, two days old as it is (a gate may run longer
# than any age: issue #16, item 4); the scratch of a gate that is gone goes,
# made a moment ago; so does one whose pid now belongs to another process
# (another start time), and one from before a reboot. A record that does not
# read as one, or from another pid namespace, proves nothing: by age, as a
# scratch without one. A scratch whose gate is gone but in which a process
# still has a file open stays.
identity=$(proc_identity)
sleep 600 &
gate_pid=$!
holders="$holders $gate_pid"
record() { # <scratch> <pid> <start time> [<boot id and pid namespace>]
    mkdir -p "$tmp/$1"
    printf '%s %s %s\n' "$2" "$3" "${4:-$identity}" >"$tmp/$1/$GATE_SCRATCH_OWNER"
}
( : ) &
dead_pid=$!
wait "$dead_pid"
record spawn-gate.live01 "$gate_pid" "$(proc_start "$gate_pid")"     # its gate runs
old "$tmp/spawn-gate.live01"
record spawn-gate.dead01 "$dead_pid" 12345                          # its gate is gone
record spawn-gate.reuse1 "$gate_pid" "$(($(proc_start "$gate_pid") - 1))" # the pid is another process's now
record spawn-gate.boot01 "$gate_pid" "$(proc_start "$gate_pid")" "00000000-0000-0000-0000-000000000000 ${identity#* }"
record spawn-gate.junk01 "not a" record                             # no record: fresh, so it stays
record spawn-gate.junk02 "not a" record                             # no record: old, so it goes
old "$tmp/spawn-gate.junk02"
record spawn-gate.ns0001 "$dead_pid" 12345 "${identity% *} pid:[1]" # another pid namespace: fresh, stays
record spawn-gate.held01 "$dead_pid" 12345                          # gone, but a process has a file open in it
echo log >"$tmp/spawn-gate.held01/log"
( exec 3<"$tmp/spawn-gate.held01/log" && exec sleep 600 ) &
holders="$holders $!"
own=$(TMPDIR="$tmp" gate_scratch)                                   # this shell's, as a gate makes it
old "$own"
sleep 0.3

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
expect "it says what would go" "$(grep -c 'would remove' "$work/out")" "11"
expect "gate_scratch records the shell that asked" "$(cat "$own/$GATE_SCRATCH_OWNER")" "$$ $(proc_start $$) $identity"
expect "and that record proves it runs" "$(gate_scratch_owner "$own")" "alive"

status=0
run || status=$?
expect "a run succeeds" "$status" "0"
expect "only the old gate scratch directory goes, and those whose gate is gone" "$(listing "$tmp")" \
    "other-project spawn-gate-other ${own##*/} spawn-gate.def456 spawn-gate.fil123 spawn-gate.held01 spawn-gate.junk01 spawn-gate.live01 spawn-gate.lnk123 spawn-gate.ns0001 spawn-gate.toolong1 "
expect "it counts the scratch whose gate runs" "$(grep -c 'kept 2 whose gate runs' "$work/out")" "1"
expect "and names the one a process still uses" "$(grep -c "kept $tmp/spawn-gate.held01: its gate is gone, but a process still uses it" "$work/out")" "1"
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

# The age is for a scratch without a record only, and never under the
# gate's wall clock: at the unit's old 180 minutes the live gate's scratch,
# two days old, still stays; a scratch without a record that is 4 hours old
# goes at 180, and stays when a gate may run 300.
mkdir "$tmp/spawn-gate.four01"
touch -d '4 hours ago' "$tmp/spawn-gate.four01"
run SPAWN_MAINTENANCE_SCRATCH_AGE_MIN=180 SPAWN_GATE_WALL_CLOCK_MINUTES=300
expect "a scratch without a record stays for as long as a gate may run" \
    "$(listing "$tmp" | tr ' ' '\n' | grep -c -e 'spawn-gate.four01' -e 'spawn-gate.live01')" "2"
run SPAWN_MAINTENANCE_SCRATCH_AGE_MIN=180
expect "past the age it goes, and a running gate's scratch stays at any age" \
    "$(listing "$tmp" | tr ' ' '\n' | grep -e 'spawn-gate.four01' -e 'spawn-gate.live01' | tr '\n' ' ')" "spawn-gate.live01 "
# shellcheck disable=SC2086
kill $holders 2>/dev/null || true
# shellcheck disable=SC2086
wait $holders 2>/dev/null || true
holders=""
run
expect "once the gate and the holder are gone, their scratch goes" \
    "$(listing "$tmp" | tr ' ' '\n' | grep -c -e 'spawn-gate.live01' -e 'spawn-gate.held01' || true)" "0"
expect "this shell's own scratch stays: it runs" "$(ls -d "$own")" "$own"

status=0
run SPAWN_MAINTENANCE_SCRATCH_AGE_MIN=old SPAWN_MAINTENANCE_KEEP_DUMPS=2 || status=$?
expect "a failed step fails the run" "$status" "1"
expect "after the other steps ran" "$(listing "$backups" | grep -o 'spawn-2026110[0-9]-010101.dump ' | tr -d '\n')" \
    "spawn-20261106-010101.dump spawn-20261107-010101.dump "

status=0
run SPAWN_MAINTENANCE_KEEP_DUMPS=all || status=$?
expect "a count that is not a number fails, deleting nothing" "$status:$(listing "$backups" | grep -c 'spawn-2026110' || true)" "1:1"

echo "maintenance: all cases pass"
