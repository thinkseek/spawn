# Disk room for Spawn's scripts (POSIX sh, sourced): the gate refuses to
# start without room, and names its scratch so routine cleanup
# (scripts/maintenance.sh) can tell it from everyone else's files in TMPDIR.
#
#   gate_room      0 when TMPDIR has at least SPAWN_GATE_MIN_FREE_GB
#                  (default 10, whole GiB) free. Otherwise it says why on
#                  stderr and returns:
#                    2  room is short. It sets disk_free (KiB), disk_dir
#                       and disk_min_gb. The gate writes them as a no-room
#                       fact to its verdict file and exits 5, even when that
#                       write fails (then the integrator measures). The integrator
#                       records `no_room` (crates/spawn-cli/src/gate.rs),
#                       which is no verdict on the candidate and no abort. It
#                       does not count toward a mission's halt, and it is
#                       judged again later with a backoff.
#                    1  the floor is not whole GiB, or the free space cannot
#                       be read. That is a problem that repeats, so the gate
#                       exits 3 and the integrator counts an abort.
#   gate_scratch   mktemp -d in TMPDIR named $GATE_SCRATCH_PREFIX.XXXXXX,
#                  with the owner's record in it ($GATE_SCRATCH_OWNER): one
#                  line, `<pid> <start time> <boot id> <pid namespace>` of
#                  the shell that asked (the gate: `$$`, also inside `$(...)`).
#                  A pid alone names whatever process has it next; with its
#                  start time (/proc/<pid>/stat, field 22: clock ticks since
#                  boot) and the boot, it names this process only. The
#                  record is best effort: a scratch without one is cleaned
#                  by its age, as before.
#   gate_scratch_owner <dir>
#                  prints what the record proves: `alive` (that process
#                  runs), `gone` (it does not: no such pid, another start
#                  time, or another boot), `unknown` (no record, one that
#                  does not read as one, one written in another pid
#                  namespace, or /proc cannot be read here).
#   proc_start <pid>
#                  the process's start time, nothing when there is none
#   disk_free_kb   free KiB on the filesystem holding a path (df -P)

GATE_SCRATCH_PREFIX=spawn-gate
GATE_SCRATCH_OWNER=.gate-owner

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

proc_start() { # <pid>
    # After the command's name, which may hold spaces and parentheses.
    tr '\n' ' ' <"/proc/$1/stat" 2>/dev/null | sed 's/^.*) //' | cut -d' ' -f20
}

# proc_identity: `<boot id> <pid namespace>` of this process, nothing when
# /proc does not say.
proc_identity() {
    proc_boot=$(cat /proc/sys/kernel/random/boot_id 2>/dev/null) || proc_boot=""
    proc_ns=$(readlink /proc/self/ns/pid 2>/dev/null) || proc_ns=""
    [ -z "$proc_boot" ] || [ -z "$proc_ns" ] || printf '%s %s\n' "$proc_boot" "$proc_ns"
}

gate_scratch() {
    gate_scratch_dir=$(mktemp -d "${TMPDIR:-/tmp}/$GATE_SCRATCH_PREFIX.XXXXXX") || return
    gate_scratch_start=$(proc_start "$$") || gate_scratch_start=""
    gate_scratch_identity=$(proc_identity) || gate_scratch_identity=""
    if [ -n "$gate_scratch_start" ] && [ -n "$gate_scratch_identity" ]; then
        printf '%s %s %s\n' "$$" "$gate_scratch_start" "$gate_scratch_identity" \
            >"$gate_scratch_dir/$GATE_SCRATCH_OWNER" 2>/dev/null || :
    fi
    printf '%s\n' "$gate_scratch_dir"
}

gate_scratch_owner() { # <dir>
    owner_file=$1/$GATE_SCRATCH_OWNER
    owner_here=$(proc_identity) || owner_here=""
    if [ ! -f "$owner_file" ] || [ -L "$owner_file" ] || [ -z "$owner_here" ] || [ -z "$(proc_start "$$")" ]; then
        echo unknown
        return 0
    fi
    owner_pid="" owner_start="" owner_boot="" owner_ns="" owner_rest=""
    IFS=' ' read -r owner_pid owner_start owner_boot owner_ns owner_rest <"$owner_file" 2>/dev/null || :
    case "$owner_pid$owner_start" in '' | *[!0-9]*) echo unknown; return 0 ;; esac
    if [ -z "$owner_pid" ] || [ -z "$owner_start" ] || [ -z "$owner_boot" ] || [ -z "$owner_ns" ] || [ -n "$owner_rest" ]; then
        echo unknown
    elif [ "$owner_boot" != "${owner_here% *}" ]; then
        echo gone
    elif [ "$owner_ns" != "${owner_here#* }" ]; then
        echo unknown
    elif [ "$(proc_start "$owner_pid")" = "$owner_start" ]; then
        echo alive
    else
        echo gone
    fi
}
