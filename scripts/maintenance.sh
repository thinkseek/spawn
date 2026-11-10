#!/bin/sh
# Routine cleanup of what Spawn itself writes, run daily by
# spawn-maintenance.timer (deploy/systemd). Nothing Spawn did not create is
# touched:
#
#   1. scratch  the gate's scratch directories, `$TMPDIR/spawn-gate.XXXXXX`
#               (scripts/disk.sh names them), owned by this user, whose
#               gate is gone. The gate removes its own scratch on exit, so
#               one that is still there was left by a gate killed before
#               its trap ran, or belongs to a gate that runs. Which, the
#               scratch says itself: the gate's record in it (disk.sh,
#               gate_scratch: its pid, its start time, the boot). While
#               that process runs the scratch stays, however old: a gate
#               runs as long as SPAWN_GATE_WALL_CLOCK_MINUTES allows, an
#               owner's by hand as long as it takes, and a directory's age
#               says nothing of either (issue #16, item 4: a fixed 180
#               minutes removed the scratch of gates still running). Once
#               that process is gone the scratch goes, whatever its age. A
#               scratch without a record (a gate from a controller before
#               the record; a gate killed before it wrote one) cannot say:
#               it goes by its age alone, not modified for
#               SPAWN_MAINTENANCE_SCRATCH_AGE_MIN (default 1440, a day)
#               and for SPAWN_GATE_WALL_CLOCK_MINUTES when that is longer.
#               In every case a scratch in which some process still has
#               its working directory, its root or an open file stays
#               (what a killed gate left running), and so does every
#               scratch when /proc cannot tell.
#   2. dumps    deploy.sh's pre-deploy dumps in SPAWN_BACKUP_DIR (default
#               ~/.local/share/spawn-rsi/backups), named exactly
#               `spawn-YYYYMMDD-HHMMSS.dump`: the newest
#               SPAWN_MAINTENANCE_KEEP_DUMPS (default 5) stay. pg-backup.sh's
#               `spawn-auto-*.dump` are its own to rotate (it keeps the
#               newest); every other file there is left alone.
#   3. cache    the gate's base cache, ${XDG_CACHE_HOME:-~/.cache}/spawn/
#               gate-base (scripts/rsi/gate.sh): entries (files named by
#               their 64-hex key, `<key>.<pid>` a gate never finished
#               writing, and `<key>.lock`, the key's lock) no gate used for
#               SPAWN_MAINTENANCE_CACHE_AGE_DAYS (default 14). A gate
#               touches an entry on every hit, so only the results of bases
#               and keys nobody judges any more go.
#   4. servers  the templates (and exported images) of the gate's private
#               test servers, ${XDG_CACHE_HOME:-~/.cache}/spawn/gate-pg/<id>
#               (the image's id or the host programs' hash;
#               scripts/rsi/gate-pg.sh), that no gate started a server from
#               for SPAWN_MAINTENANCE_CACHE_AGE_DAYS (a gate touches `ready`
#               at each start), with their locks, and a half-made one
#               (`.<id>.XXXXXX`, a gate killed while it made it) older than
#               a day.
#
# Every step runs even when an earlier one failed; the exit status is 1
# when any did. SPAWN_MAINTENANCE_DRY_RUN=1 prints what would go and
# deletes nothing.
#
# usage: scripts/maintenance.sh
set -u
# Lists of paths are split on newlines only, and never globbed.
set -f
export LC_ALL=C
IFS='
'

# shellcheck source=scripts/disk.sh
. "$(dirname "$0")/disk.sh"

dry=${SPAWN_MAINTENANCE_DRY_RUN:-0}
note=
[ "$dry" != 1 ] || note=" (dry run)"
failed=0
say() { echo "maintenance: $*"; }
fail() {
    echo "maintenance: error: $*" >&2
    failed=1
}
whole() { # <name> <value>: the value, when it is a whole number
    case $2 in
    '' | *[!0-9]*)
        fail "$1 is a whole number, not '$2'"
        return 1
        ;;
    esac
}

# scratch_in_use <dir>: a process has its working directory, its root or an
# open file there (/proc names physical paths, so the directory's is what is
# compared), or that cannot be told: the directory does not resolve, or
# this process cannot read its own /proc entry.
scratch_in_use() {
    physical=$(readlink -f -- "$1" 2>/dev/null) || return 0
    [ -n "$physical" ] && [ -n "$(readlink "/proc/$$/cwd" 2>/dev/null)" ] || return 0
    # The one glob of this script: the processes' entries in /proc.
    set +f
    hit=$(find /proc/[0-9]*/cwd /proc/[0-9]*/root /proc/[0-9]*/fd -maxdepth 1 \
        \( -lname "$physical" -o -lname "$physical/*" \) -print -quit 2>/dev/null)
    set -f
    [ -n "$hit" ]
}

# 1. the gate's scratch
tmp=${TMPDIR:-/tmp}
age=${SPAWN_MAINTENANCE_SCRATCH_AGE_MIN:-1440}
if whole SPAWN_MAINTENANCE_SCRATCH_AGE_MIN "$age"; then
    # A scratch without its gate's record goes by age alone: never younger
    # than a gate may run.
    case ${SPAWN_GATE_WALL_CLOCK_MINUTES-} in
    '' | *[!0-9]*) ;;
    *) [ "$SPAWN_GATE_WALL_CLOCK_MINUTES" -le "$age" ] || age=$SPAWN_GATE_WALL_CLOCK_MINUTES ;;
    esac
    removed=0
    running=0
    # -type d without -L: a symlink named like scratch is not followed.
    for dir in $(find "$tmp" -mindepth 1 -maxdepth 1 -type d -name "$GATE_SCRATCH_PREFIX.??????" \
        -user "$(id -u)" | sort); do
        case $(gate_scratch_owner "$dir") in
        alive)
            running=$((running + 1))
            continue
            ;;
        gone) ;;
        *) [ -n "$(find "$dir" -maxdepth 0 -mmin +"$age")" ] || continue ;;
        esac
        if scratch_in_use "$dir"; then
            say "scratch: kept $dir: its gate is gone, but a process still uses it (or /proc cannot tell)"
            continue
        fi
        if [ "$dry" = 1 ]; then
            say "scratch: would remove $dir"
            continue
        fi
        # The gate's trees can hold read-only directories (git objects).
        chmod -R u+w -- "$dir" 2>/dev/null
        if rm -rf -- "$dir"; then
            removed=$((removed + 1))
        else
            fail "scratch: could not remove $dir"
        fi
    done
    say "scratch: removed $removed gate scratch director$([ "$removed" = 1 ] && echo y || echo ies) from $tmp, kept $running whose gate runs$note"
fi

# 2. deploy.sh's dumps
backups=${SPAWN_BACKUP_DIR:-$HOME/.local/share/spawn-rsi/backups}
keep=${SPAWN_MAINTENANCE_KEEP_DUMPS:-5}
if whole SPAWN_MAINTENANCE_KEEP_DUMPS "$keep" && [ -d "$backups" ]; then
    pruned=0
    # The names sort by time; newest last. Only regular files, exactly named.
    for dump in $(find "$backups" -mindepth 1 -maxdepth 1 -type f \
        -name 'spawn-[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]-[0-9][0-9][0-9][0-9][0-9][0-9].dump' |
        sort | head -n "-$keep"); do
        if [ "$dry" = 1 ]; then
            say "dumps: would remove $dump"
            continue
        fi
        if rm -f -- "$dump"; then
            pruned=$((pruned + 1))
        else
            fail "dumps: could not remove $dump"
        fi
    done
    say "dumps: removed $pruned, kept the newest $keep deploy dumps in $backups$note"
fi

# 3. the gate's base cache
cache=${XDG_CACHE_HOME:-$HOME/.cache}/spawn/gate-base
days=${SPAWN_MAINTENANCE_CACHE_AGE_DAYS:-14}
if whole SPAWN_MAINTENANCE_CACHE_AGE_DAYS "$days" && [ -d "$cache" ]; then
    expired=0
    for entry in $(find "$cache" -mindepth 1 -maxdepth 1 -type f -user "$(id -u)" \
        -regextype posix-extended -regex '.*/[0-9a-f]{64}(\.[0-9]+|\.lock)?' -mtime +"$days" | sort); do
        if [ "$dry" = 1 ]; then
            say "cache: would remove $entry"
            continue
        fi
        if rm -f -- "$entry"; then
            expired=$((expired + 1))
        else
            fail "cache: could not remove $entry"
        fi
    done
    say "cache: removed $expired base results no gate used for $days days from $cache$note"
fi

# 4. the private test servers' templates (and exported images)
servers=${XDG_CACHE_HOME:-$HOME/.cache}/spawn/gate-pg
if whole SPAWN_MAINTENANCE_CACHE_AGE_DAYS "$days" 2>/dev/null && [ -d "$servers" ]; then
    gone=0
    # -type d without -L: a symlink named like a root is not followed.
    for root in $(find "$servers" -mindepth 1 -maxdepth 1 -type d -user "$(id -u)" \
        -regextype posix-extended -regex '.*/[0-9a-f]{64}' | sort) \
        $(find "$servers" -mindepth 1 -maxdepth 1 -type d -user "$(id -u)" \
            -regextype posix-extended -regex '.*/\.[0-9a-f]{64}\.[A-Za-z0-9]{6}' -mmin +1440 | sort); do
        # By its own name: the path above it may hold a dot directory
        # (~/.cache), and a match on the whole path took every template
        # for a half-made one and removed it whatever its age.
        case "${root##*/}" in
        .*) ;;
        *)
            # In use within the age (`ready` touched at each start), or a
            # gate is making it: it stays.
            [ -f "$root/ready" ] && [ -z "$(find "$root/ready" -mtime +"$days")" ] && continue
            [ -f "$root/ready" ] || continue
            ;;
        esac
        if [ "$dry" = 1 ]; then
            say "servers: would remove $root"
            continue
        fi
        # An exported image holds read-only directories.
        chmod -R u+w -- "$root" 2>/dev/null
        if rm -rf -- "$root" && rm -f -- "$root.lock"; then
            gone=$((gone + 1))
        else
            fail "servers: could not remove $root"
        fi
    done
    say "servers: removed $gone private test server templates no gate used for $days days from $servers$note"
fi

exit "$failed"
