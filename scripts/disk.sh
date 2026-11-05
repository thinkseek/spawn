# Disk room for Spawn's scripts (POSIX sh, sourced): the gate refuses to
# start without room, and names its scratch so routine cleanup
# (scripts/maintenance.sh) can tell it from everyone else's files in TMPDIR.
#
#   gate_room      0 when TMPDIR has at least SPAWN_GATE_MIN_FREE_GB
#                  (default 10, whole GiB) free. Otherwise it says why on
#                  stderr and returns:
#                    2  room is short. It sets disk_free (KiB), disk_dir
#                       and disk_min_gb. The gate writes them as a no-room
#                       fact to its verdict file and exits 5. The integrator
#                       records `no_room` (crates/spawn-cli/src/gate.rs),
#                       which is no verdict on the candidate and no abort. It
#                       does not count toward a mission's halt, and it is
#                       judged again later with a backoff.
#                    1  the floor is not whole GiB, or the free space cannot
#                       be read. That is a problem that repeats, so the gate
#                       exits 3 and the integrator counts an abort.
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
        echo "gate: $(awk -v kb="$disk_free" 'BEGIN { printf "%.1f", kb / 1048576 }') GiB free in $disk_dir (TMPDIR), under SPAWN_GATE_MIN_FREE_GB=$disk_min_gb: not starting. No room to judge now, not a verdict: the integrator judges the candidate again later." >&2
        return 2
    fi
}

gate_scratch() {
    mktemp -d "${TMPDIR:-/tmp}/$GATE_SCRATCH_PREFIX.XXXXXX"
}
