#!/bin/sh
# The runner side of a remote gate run (subject: remote-gate-runner).
# deploy/gate-runner.sh on the primary packages a job and ships it here;
# this helper receives it, runs the controller's own scripts/rsi/gate.sh
# with the runner's private PostgreSQL and its own scratch, and hands the
# verdict and the whole log back. Installed by deploy/gate-runner/setup.sh
# as <root>/bin/remote; the dispatcher refuses a runner whose copy of this
# file differs from its own (the subcommands below are the protocol).
#
#   remote version                      this file's sha256, one line
#   remote preflight                    key=value lines: the tools, the
#                                       server, disk and jobs this machine
#                                       gives a gate (deploy/gate-runner.sh
#                                       --check reads them; the README says
#                                       what each must be)
#   remote accept <job>                 read the job's archive (tar.gz) from
#                                       stdin into <root>/jobs/<job>, whole
#                                       or not at all
#   remote run <job> <seconds>          run the job's gate under nice and
#                                       that wall-clock limit; its exit
#                                       status goes to out/exit, its output
#                                       to out/gate.log
#   remote keys <job> <seconds>         the same for `gate.sh --base-keys`,
#                                       its output to out/keys.log (the
#                                       dispatcher's --check)
#   remote collect <job>                tar.gz of the result on stdout:
#                                       MANIFEST (sha256 of each file),
#                                       exit, seconds, gate.log (or
#                                       keys.log) and verdict when present
#   remote remove <job> [force]         delete the job's directory and scratch;
#                                       refuses a job whose run has not
#                                       finished (out/exit is missing: a gate
#                                       may still be running there) unless
#                                       `force` is given
#   remote registry-need                read the primary's cargo registry
#                                       listing (`<path>\t<size>` lines) on
#                                       stdin; print the paths this machine
#                                       lacks or holds at another size
#   remote registry-accept              unpack a tar of registry files from
#                                       stdin into this machine's registry
#                                       (adds and replaces, never deletes: a
#                                       running gate binds it read-only)
#
# <job> is letters, digits, '.', '_' and '-' (deploy/gate-runner.sh makes
# one per attempt). The root is the directory two levels above this file.
# <root>/config, written by setup.sh, says what this machine gives a gate:
#
#   GATE_RUNNER_JOBS          SPAWN_GATE_JOBS (scenarios at once)
#   GATE_RUNNER_PG_IMAGE      SPAWN_GATE_PG_IMAGE (a docker image, or empty)
#   GATE_RUNNER_PG_BIN        SPAWN_GATE_PG_BIN (a PostgreSQL bin directory,
#                             or empty; never both)
#   GATE_RUNNER_MIN_FREE_GB   SPAWN_GATE_MIN_FREE_GB
#   GATE_RUNNER_NICE          nice level for the gate (default 10)
#   GATE_RUNNER_PATH          the PATH a gate runs with (the toolchain)
#   GATE_RUNNER_KEEP_JOBS     1: keep every job directory (the dispatcher's
#                             --keep does the same for one job)
#   GATE_RUNNER_KEEP_DAYS     prune job directories (and their scratch) older
#                             than this many days on preflight and accept
#                             (default 7; 0 keeps them)
#   GATE_RUNNER_DISK_LIMIT_GB the machine's own disk limit when df cannot see
#                             it (an OrbStack --disk limit is a btrfs quota:
#                             df shows the shared disk); 0 or unset: none
#   GATE_RUNNER_CARGO_HOME    the CARGO_HOME whose registry, git and config
#                             the gate binds (default $HOME/.cargo)
#
# What a gate really gets is measured where it runs, not where it is easy to
# read: the cores are nproc capped by the cgroup's cpu.max, the memory is
# MemAvailable capped by memory.max less memory.current (a machine inside a
# shared VM sees the whole VM in nproc and /proc/meminfo), and the disk is
# df capped by GATE_RUNNER_DISK_LIMIT_GB less what du counts on the root
# filesystem. GATE_RUNNER_CGROUP, GATE_RUNNER_MEMINFO and GATE_RUNNER_MOUNTS
# move /sys/fs/cgroup, /proc/meminfo and /proc/mounts, for tests only.
#
# POSIX sh. Requires: git, tar, gzip, sha256sum, nice, timeout (coreutils
# >= 8.5, for --kill-after), and docker or a PostgreSQL bin directory.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd -P) || {
    echo "gate-runner: cannot find my root above $0" >&2
    exit 3
}
config=$root/config
# A job whose scratch path is too long for the gate's Unix sockets gets one
# here instead: short, on the machine's disk, under $HOME like the root (the
# gate's sandbox binds what it needs of a TMPDIR under $HOME). Never /tmp:
# on OrbStack machines and many containers /tmp is a tmpfs, so a gate's
# builds there are memory, and a nested integrator finds no room (measured on
# spawn-runner, 2026-11-09: check-e2e.sh failed on base and candidate alike).
short=${GATE_RUNNER_SHORT:-$HOME/.grs}
jobs=$root/jobs
cache=$root/cache
work=$root/work

# job_name <job>: the dispatcher's ids only (letters, digits, '.', '_', '-').
job_name() {
    case "${1-}" in
    '' | *[!A-Za-z0-9._-]* | . | ..)
        echo "gate-runner: not a job name: '${1-}'" >&2
        exit 2
        ;;
    esac
}

# seconds <value>: a whole number from 1.
seconds() {
    case "${1-}" in
    '' | *[!0-9]* | 0*)
        echo "gate-runner: not a whole number of seconds: '${1-}'" >&2
        exit 2
        ;;
    esac
}

# load_config: the runner's settings, defaults for the ones setup.sh leaves
# out.
load_config() {
    cfg_jobs="" cfg_pg_image="" cfg_pg_bin="" cfg_min_free="" cfg_nice="" cfg_path="" cfg_keep=""
    cfg_keep_days="" cfg_disk_limit="" cfg_cargo_home=""
    if [ -f "$config" ]; then
        # shellcheck source=/dev/null
        . "$config"
    fi
    cfg_jobs=${GATE_RUNNER_JOBS:-}
    cfg_pg_image=${GATE_RUNNER_PG_IMAGE:-}
    cfg_pg_bin=${GATE_RUNNER_PG_BIN:-}
    cfg_min_free=${GATE_RUNNER_MIN_FREE_GB:-}
    cfg_nice=${GATE_RUNNER_NICE:-}
    cfg_path=${GATE_RUNNER_PATH:-}
    cfg_keep=${GATE_RUNNER_KEEP_JOBS:-}
    cfg_keep_days=${GATE_RUNNER_KEEP_DAYS:-}
    cfg_disk_limit=${GATE_RUNNER_DISK_LIMIT_GB:-0}
    cfg_cargo_home=${GATE_RUNNER_CARGO_HOME:-}
    cargo_home=${cfg_cargo_home:-$HOME/.cargo}
    [ -n "$cfg_jobs" ] || cfg_jobs=$(($(cpus) / 2))
    [ "$cfg_jobs" -ge 1 ] || cfg_jobs=1
    [ "$cfg_jobs" -le 8 ] || cfg_jobs=8
    [ -n "$cfg_min_free" ] || cfg_min_free=10
    [ -n "$cfg_nice" ] || cfg_nice=10
    [ -n "$cfg_keep_days" ] || cfg_keep_days=7
    [ -n "$cfg_path" ] || cfg_path=$PATH
    [ -n "$cfg_pg_image$cfg_pg_bin" ] || {
        echo "gate-runner: $config names no private server (GATE_RUNNER_PG_IMAGE or GATE_RUNNER_PG_BIN); a runner never uses a shared test database" >&2
        exit 3
    }
    [ -z "$cfg_pg_image" ] || [ -z "$cfg_pg_bin" ] || {
        echo "gate-runner: $config names both a docker image and a bin directory; the gate takes one" >&2
        exit 3
    }
    case "$cfg_jobs$cfg_min_free$cfg_nice$cfg_keep_days$cfg_disk_limit" in *[!0-9]*) echo "gate-runner: $config: jobs, the disk floor, nice, the keep days and the disk limit are numbers" >&2; exit 3 ;; esac
}

# cgroup <file>: the first line of a cgroup v2 file of this machine, empty
# when there is none.
cgroup() {
    head -n 1 "${GATE_RUNNER_CGROUP:-/sys/fs/cgroup}/$1" 2>/dev/null || :
}

# cpus: the cores a gate gets: nproc, capped by cpu.max's quota (rounded up).
cpus() {
    n=$(nproc 2>/dev/null || echo 1)
    # shellcheck disable=SC2046 # cpu.max is "<quota|max> <period>"
    set -- $(cgroup cpu.max)
    case "${1-}" in
    '' | *[!0-9]*) ;;
    *)
        case "${2-}" in
        '' | *[!0-9]* | 0) ;;
        *) q=$((($1 + $2 - 1) / $2)); [ "$q" -ge "$n" ] || n=$q ;;
        esac
        ;;
    esac
    [ "$n" -ge 1 ] || n=1
    echo "$n"
}

# mem_available_kb: MemAvailable, capped by what memory.max leaves above
# memory.current; "unreadable" when neither can be read.
mem_available_kb() {
    avail=$(awk '/^MemAvailable:/ { print $2 }' "${GATE_RUNNER_MEMINFO:-/proc/meminfo}" 2>/dev/null) || avail=""
    max=$(cgroup memory.max)
    cur=$(cgroup memory.current)
    case "$max$cur" in
    '' | *[!0-9]*) ;;
    *)
        left=$(((max - cur) / 1024))
        [ "$left" -ge 0 ] || left=0
        if [ -z "$avail" ] || [ "$left" -lt "$avail" ]; then avail=$left; fi
        ;;
    esac
    echo "${avail:-unreadable}"
}

# host_shares: mount points that are the host's files (virtiofs, 9p, a fuse
# file system other than lxcfs, or OrbStack's /mnt/mac) or other machines'
# (OrbStack's /mnt/machines/<name>, read-write: the primary's own disk, its
# live database included), comma-separated, or "none". A gate's sandbox
# starts from a read-only / : on such a machine candidate code reads them,
# and the gate's socket scan walks them.
host_shares() {
    shares=$(awk '($3 ~ /^(virtiofs|9p)$/ || ($3 ~ /^fuse\./ && $3 != "fuse.lxcfs") || $2 ~ /^\/mnt\/(mac|machines)(\/|$)/) { print $2 }' \
        "${GATE_RUNNER_MOUNTS:-/proc/mounts}" 2>/dev/null | sort -u | paste -sd, -) || shares=""
    echo "${shares:-none}"
}

# disk_free_kb: the free KiB a gate has under the root: df's, and under
# GATE_RUNNER_DISK_LIMIT_GB the limit less what du counts on / (sudo when it
# may, so directories this user cannot read are counted), whichever is less.
disk_free_kb() {
    free=$(df -Pk -- "$root" 2>/dev/null | awk 'NR == 2 { print $4 }') || free=""
    case "$free" in '' | *[!0-9]*) echo unreadable; return 0 ;; esac
    if [ "$cfg_disk_limit" != 0 ]; then
        used=$({ sudo -n du -sxk / 2>/dev/null || du -sxk / 2>/dev/null; } | awk 'END { print $1 }') || used=""
        case "$used" in '' | *[!0-9]*) echo unreadable; return 0 ;; esac
        left=$((cfg_disk_limit * 1048576 - used))
        [ "$left" -ge 0 ] || left=0
        [ "$left" -ge "$free" ] || free=$left
    fi
    echo "$free"
}

# cargo_wrapper: the rustc-wrapper the cargo config names (the gate binds
# config and config.toml into its sandbox), "none", or "<name> missing" when
# the gate's PATH has no such program: every build would fail there.
cargo_wrapper() {
    w=$(sed -n 's/^[[:space:]]*rustc-wrapper[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
        "$cargo_home/config.toml" "$cargo_home/config" 2>/dev/null | head -n 1) || w=""
    if [ -z "$w" ]; then
        echo none
    elif PATH=$cfg_path command -v "$w" >/dev/null 2>&1; then
        echo "$w"
    else
        echo "$w missing"
    fi
}

# registry_list <dir>: `<path>\t<size>` of every file under a registry.
registry_list() {
    [ ! -d "$1" ] || (cd "$1" && find . -type f -printf '%P\t%s\n')
}

# ensure_dirs: the root's own directories, this uid's only.
ensure_dirs() {
    for dir in "$jobs" "$cache" "$work"; do
        [ -d "$dir" ] || mkdir -p "$dir"
        chmod 700 "$dir" 2>/dev/null || :
    done
}

# prune_jobs: job directories (and their scratch) older than the keep days
# go, on preflight and accept. A gate's own timeout is hours, so a directory
# that old belongs to no run of this runner's; what is kept is anything
# younger, whatever state it is in (so a job left for a look survives the
# next dispatch for a while).
prune_jobs() {
    [ "$cfg_keep_days" != 0 ] || return 0
    pruned=0
    for dir in "$jobs"/* "$work"/* "$short"/*; do
        [ -e "$dir" ] || [ -L "$dir" ] || continue
        [ -n "$(find "$dir" -maxdepth 0 -mtime "+$cfg_keep_days" 2>/dev/null)" ] || continue
        rm -rf "$dir"
        pruned=$((pruned + 1))
    done
    [ "$pruned" = 0 ] || echo "gate-runner: pruned $pruned job director(ies) older than ${cfg_keep_days} days" >&2
}

# tool <name> <command...>: `name=<first line>` or `name=missing`.
tool() {
    name=$1
    shift
    if command -v "$name" >/dev/null 2>&1; then
        line=$("$@" 2>&1 | head -n 1) || line="unreadable"
        printf '%s=%s\n' "$name" "$line"
    else
        printf '%s=missing\n' "$name"
    fi
}

preflight() {
    load_config
    ensure_dirs
    prune_jobs
    version=$(sha256sum "$root/bin/remote" 2>/dev/null | cut -d' ' -f1) || version="unknown"
    printf 'version=%s\n' "$version"
    printf 'root=%s\n' "$root"
    printf 'host=%s\n' "$(uname -n)"
    tool sh sh -c 'echo sh ok'
    tool git git --version
    tool tar tar --version
    tool gzip gzip --version
    tool sha256sum sha256sum --version
    tool nice nice --version
    tool timeout timeout --version
    tool bwrap bwrap --version
    tool docker docker --version
    tool rustc rustc -Vv
    tool cargo cargo -V
    tool node node --version
    tool psql psql -V
    tool tmux tmux -V
    tool python3 python3 -V
    tool jq jq --version
    printf 'jobs=%s\n' "$cfg_jobs"
    printf 'nice_level=%s\n' "$cfg_nice"
    printf 'min_free_gb=%s\n' "$cfg_min_free"
    printf 'keep_days=%s\n' "$cfg_keep_days"
    printf 'nproc=%s\n' "$(nproc 2>/dev/null || echo 1)"
    printf 'cpus=%s\n' "$(cpus)"
    printf 'cpu_max=%s\n' "$(cgroup cpu.max)"
    printf 'mem_available_kb=%s\n' "$(mem_available_kb)"
    printf 'mem_max=%s\n' "$(cgroup memory.max)"
    printf 'swap_max=%s\n' "$(cgroup memory.swap.max)"
    printf 'host_shares=%s\n' "$(host_shares)"
    printf 'tmp_fs=%s\n' "$(df -PT /tmp 2>/dev/null | awk 'NR == 2 { print $2 }')"
    printf 'cargo_home=%s\n' "$cargo_home"
    printf 'cargo_wrapper=%s\n' "$(cargo_wrapper)"
    printf 'pg_image=%s\n' "$cfg_pg_image"
    printf 'pg_bin=%s\n' "$cfg_pg_bin"
    printf 'disk_root=%s\n' "$root"
    printf 'disk_limit_gb=%s\n' "$cfg_disk_limit"
    printf 'disk_free_kb=%s\n' "$(disk_free_kb)"
    if [ -d "$root/web/node_modules" ]; then
        printf 'web=%s\n' "$root/web/node_modules"
        [ ! -f "$root/web/lock.sha256" ] || printf 'web_lock=%s\n' "$(cat "$root/web/lock.sha256")"
    else
        printf 'web=none\n'
    fi
}

# accept_job <job>: the archive from stdin, unpacked into a partial
# directory and renamed into place only when whole (meta names the job).
accept_job() {
    job_name "$1"
    job=$1
    load_config
    ensure_dirs
    prune_jobs
    dir=$jobs/$job
    partial=$dir.partial
    rm -rf "$partial"
    mkdir -p "$partial"
    if ! tar -xzf - -C "$partial"; then
        rm -rf "$partial"
        echo "gate-runner: the archive for $job did not arrive whole" >&2
        exit 2
    fi
    if [ ! -f "$partial/meta" ] || ! grep -qx "job=$job" "$partial/meta"; then
        rm -rf "$partial"
        echo "gate-runner: the archive for $job carries no meta for it" >&2
        exit 2
    fi
    if [ ! -f "$partial/gate.bundle" ] || [ ! -f "$partial/run.sh" ] || [ ! -f "$partial/keys.sh" ]; then
        rm -rf "$partial"
        echo "gate-runner: the archive for $job is incomplete" >&2
        exit 2
    fi
    # The bundle becomes the job's bare repository, the one the gate exports
    # its two trees from; both commits must be in it.
    if ! git init -q --bare "$partial/repo.git" ||
        ! git -C "$partial/repo.git" fetch -q "$partial/gate.bundle" 'refs/gate/*:refs/gate/*'; then
        rm -rf "$partial"
        echo "gate-runner: the bundle in $job did not apply" >&2
        exit 2
    fi
    for sha in $(sed -n 's/^base=//p;s/^candidate=//p' "$partial/meta"); do
        if ! git -C "$partial/repo.git" cat-file -e "$sha^{commit}" 2>/dev/null; then
            rm -rf "$partial"
            echo "gate-runner: the bundle in $job lacks $sha" >&2
            exit 2
        fi
    done
    # The controller's web/node_modules, when this runner has the install
    # (check-web.sh builds the UI against it). Its lockfile stamp is what
    # --check compares; a missing install is the check's to report.
    if [ -d "$root/web/node_modules" ]; then
        mkdir -p "$partial/controller/web"
        ln -sfn "$root/web/node_modules" "$partial/controller/web/node_modules"
    fi
    rm -rf "$dir"
    mv "$partial" "$dir"
}

# run_job <job> <seconds> <mode>: the job's gate (mode run) or its base
# keys (mode keys), under nice and the limit, its output and status kept in
# out/. The exit status of the gate is out/exit, never this command's: the
# dispatcher reads it and judges it as it judges a local gate.
run_job() {
    job_name "$1"
    seconds "$2"
    job=$1
    limit=$2
    mode=$3
    load_config
    dir=$jobs/$job
    [ -d "$dir" ] || {
        echo "gate-runner: no job $job (not accepted)" >&2
        exit 2
    }
    out=$dir/out
    rm -rf "$out"
    mkdir -p "$out"
    scratch=$work/$job
    # A Unix socket path is 107 bytes at most, and the gate's own server puts
    # one under TMPDIR (<scratch>/spawn-gate.XXXXXX/test-db/.s.PGSQL.5432,
    # about 40 bytes more). A root whose path is too long gets the short
    # directory for the job's scratch (above: on disk, never /tmp).
    if [ $((${#scratch} + 40)) -gt 107 ]; then
        scratch=$short/$job
    fi
    mkdir -p "$scratch"
    export PATH=$cfg_path
    export XDG_CACHE_HOME=$cache
    export TMPDIR=$scratch
    export SPAWN_GATE_JOBS=$cfg_jobs
    export SPAWN_GATE_MIN_FREE_GB=$cfg_min_free
    [ -z "$cfg_cargo_home" ] || export CARGO_HOME=$cfg_cargo_home
    [ -z "$cfg_pg_image" ] || export SPAWN_GATE_PG_IMAGE=$cfg_pg_image
    [ -z "$cfg_pg_bin" ] || export SPAWN_GATE_PG_BIN=$cfg_pg_bin
    if [ "$mode" = keys ]; then
        log=$out/keys.log
        unset SPAWN_GATE_VERDICT
        set -- sh "$dir/keys.sh"
    else
        log=$out/gate.log
        export SPAWN_GATE_VERDICT=$out/verdict
        set -- sh "$dir/run.sh"
    fi
    started=$(date +%s)
    # shellcheck disable=SC2086 # cfg_nice is a number, checked in load_config
    code=0
    ( cd "$dir" && nice -n "$cfg_nice" timeout --signal=TERM --kill-after=60 "$limit" "$@" ) \
        >"$log" 2>&1 || code=$?
    printf '%s\n' "$code" >"$out/exit"
    printf '%s\n' "$(( $(date +%s) - started ))" >"$out/seconds"
    exit 0
}

# collect_job <job>: the result, with a manifest of what the dispatcher must
# receive, as one tar.gz on stdout. A run that left no out/exit has no
# result and says so with exit 4 (the caller reads no result, never a pass).
collect_job() {
    job_name "$1"
    load_config
    job=$1
    out=$jobs/$job/out
    [ -d "$out" ] || {
        echo "gate-runner: no job $job (not accepted)" >&2
        exit 2
    }
    [ -f "$out/exit" ] || {
        echo "gate-runner: no result for $job (the run did not finish)" >&2
        exit 4
    }
    files="exit"
    for name in seconds gate.log keys.log verdict; do
        [ ! -f "$out/$name" ] || files="$files $name"
    done
    # shellcheck disable=SC2086 # the file names are ours, above
    ( cd "$out" && sha256sum $files >MANIFEST ) || exit 2
    # shellcheck disable=SC2086
    tar -czf - -C "$out" MANIFEST $files
}

remove_job() { # <job> [force]
    job_name "$1"
    load_config
    out=$jobs/$1/out
    # A job whose run has not finished has no out/exit: its gate may still
    # be running (a run whose SSH connection died keeps going under the
    # runner's own timeout), and deleting the directory under it would strand
    # its server and scratch. A human may force it.
    if [ -d "$out" ] && [ ! -f "$out/exit" ] && [ "${2-}" != force ]; then
        echo "gate-runner: $1 has no result yet (out/exit): a gate may still be running there; not removed (remove $1 force to remove it anyway)" >&2
        exit 4
    fi
    rm -rf "$jobs/$1" "$jobs/$1.partial" "$work/$1" "$short/$1"
}

# registry_need: of the primary's registry listing on stdin, the paths this
# machine's registry lacks or holds at another size. The gate builds
# offline from the registry it binds, so a crate the primary has and this
# machine lacks would make the gate reject work the primary accepts.
registry_need() {
    load_config
    have=$(mktemp "${TMPDIR:-/tmp}/gate-runner-registry.XXXXXX")
    registry_list "$cargo_home/registry" >"$have"
    code=0
    # The listing here is read in BEGIN, not as a first file: an empty one (a
    # runner with no registry yet) would leave NR == FNR true for stdin.
    awk -F '\t' -v have="$have" 'BEGIN {
            while ((getline line < have) > 0) { split(line, f, "\t"); size[f[1]] = f[2] }
        }
        !($1 in size) || size[$1] != $2 { print $1 }' || code=$?
    rm -f "$have"
    exit "$code"
}

# registry_accept: registry files (a tar on stdin) into this machine's
# registry. It only adds and replaces: a running gate binds the registry
# read-only, and a file it reads must not go from under it.
registry_accept() {
    load_config
    mkdir -p "$cargo_home/registry"
    tar -xf - -C "$cargo_home/registry" || {
        echo "gate-runner: the registry files did not arrive whole" >&2
        exit 2
    }
}

case "${1-}" in
version)
    sha256sum "$root/bin/remote" 2>/dev/null | cut -d' ' -f1 || {
        echo "gate-runner: cannot hash $root/bin/remote" >&2
        exit 3
    }
    ;;
preflight) preflight ;;
accept)
    shift
    [ $# -eq 1 ] || { echo "usage: remote accept <job>" >&2; exit 2; }
    accept_job "$1"
    ;;
run)
    shift
    [ $# -eq 2 ] || { echo "usage: remote run <job> <seconds>" >&2; exit 2; }
    run_job "$1" "$2" run
    ;;
keys)
    shift
    [ $# -eq 2 ] || { echo "usage: remote keys <job> <seconds>" >&2; exit 2; }
    run_job "$1" "$2" keys
    ;;
collect)
    shift
    [ $# -eq 1 ] || { echo "usage: remote collect <job>" >&2; exit 2; }
    collect_job "$1"
    ;;
remove)
    shift
    [ $# -ge 1 ] && [ $# -le 2 ] || { echo "usage: remote remove <job> [force]" >&2; exit 2; }
    remove_job "$@"
    ;;
registry-need) registry_need ;;
registry-accept) registry_accept ;;
*)
    echo "usage: remote version|preflight|accept <job>|run <job> <seconds>|keys <job> <seconds>|collect <job>|remove <job>|registry-need|registry-accept" >&2
    exit 2
    ;;
esac
