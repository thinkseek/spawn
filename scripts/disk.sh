# Disk room for Spawn's scripts (POSIX sh, sourced): the gate refuses to
# start without room, and names its scratch so routine cleanup
# (scripts/maintenance.sh) can tell it from everyone else's files in TMPDIR.
#
#   gate_room      0 when TMPDIR has at least SPAWN_GATE_MIN_FREE_GB
#                  (default 10, whole GiB) free; otherwise says why on stderr
#                  and returns 1. The gate then exits 3: an abort the
#                  integrator records and retries, never a verdict on the
#                  candidate.
#   gate_scratch   mktemp -d in TMPDIR named $GATE_SCRATCH_PREFIX.XXXXXX
#   disk_free_kb   free KiB on the filesystem holding a path (df -P)

GATE_SCRATCH_PREFIX=spawn-gate

disk_free_kb() { # <path>
    df -Pk -- "$1" 2>/dev/null | awk 'NR == 2 && $4 ~ /^[0-9]+$/ { print $4; found = 1 } END { exit !found }'
}

gate_room() {
    disk_dir=${TMPDIR:-/tmp}
    disk_min_gb=${SPAWN_GATE_MIN_FREE_GB:-10}
    case $disk_min_gb in
    '' | *[!0-9]*)
        echo "gate: SPAWN_GATE_MIN_FREE_GB is whole GiB, not '$disk_min_gb'" >&2
        return 1
        ;;
    esac
    if ! disk_free=$(disk_free_kb "$disk_dir"); then
        echo "gate: cannot read the free space of $disk_dir (TMPDIR)" >&2
        return 1
    fi
    if [ "$disk_free" -lt $((disk_min_gb * 1048576)) ]; then
        echo "gate: $(awk -v kb="$disk_free" 'BEGIN { printf "%.1f", kb / 1048576 }') GiB free in $disk_dir (TMPDIR), under SPAWN_GATE_MIN_FREE_GB=$disk_min_gb: not starting. An abort, not a verdict: the integrator judges the candidate again later." >&2
        return 1
    fi
}

gate_scratch() {
    mktemp -d "${TMPDIR:-/tmp}/$GATE_SCRATCH_PREFIX.XXXXXX"
}
