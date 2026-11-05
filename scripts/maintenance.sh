#!/bin/sh
# Routine cleanup of what Spawn itself writes, run daily by
# spawn-maintenance.timer (deploy/systemd). Nothing Spawn did not create is
# touched:
#
#   1. scratch  the gate's scratch directories, `$TMPDIR/spawn-gate.XXXXXX`
#               (scripts/disk.sh names them), owned by this user and not
#               modified for SPAWN_MAINTENANCE_SCRATCH_AGE_MIN (default 1440,
#               a day). The gate removes its own scratch on exit and the
#               integrator kills it after 45 minutes, so one that old was
#               left by a gate killed before its trap ran.
#   2. dumps    deploy.sh's pre-deploy dumps in SPAWN_BACKUP_DIR (default
#               ~/.local/share/spawn-rsi/backups), named exactly
#               `spawn-YYYYMMDD-HHMMSS.dump`: the newest
#               SPAWN_MAINTENANCE_KEEP_DUMPS (default 5) stay. pg-backup.sh's
#               `spawn-auto-*.dump` are its own to rotate (it keeps the
#               newest); every other file there is left alone.
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

# 1. the gate's scratch
tmp=${TMPDIR:-/tmp}
age=${SPAWN_MAINTENANCE_SCRATCH_AGE_MIN:-1440}
if whole SPAWN_MAINTENANCE_SCRATCH_AGE_MIN "$age"; then
    removed=0
    # -type d without -L: a symlink named like scratch is not followed.
    for dir in $(find "$tmp" -mindepth 1 -maxdepth 1 -type d -name "$GATE_SCRATCH_PREFIX.??????" \
        -user "$(id -u)" -mmin +"$age" | sort); do
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
    say "scratch: removed $removed gate scratch director$([ "$removed" = 1 ] && echo y || echo ies) from $tmp$note"
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

exit "$failed"
