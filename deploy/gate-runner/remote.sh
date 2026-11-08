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
#   remote remove <job>                 delete the job's directory and scratch
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
#
# POSIX sh. Requires: git, tar, gzip, sha256sum, nice, timeout (coreutils
# >= 8.5, for --kill-after), and docker or a PostgreSQL bin directory.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd -P) || {
    echo "gate-runner: cannot find my root above $0" >&2
    exit 3
}
config=$root/config
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
    [ -n "$cfg_jobs" ] || cfg_jobs=$(($(nproc 2>/dev/null || echo 2) / 2))
    [ "$cfg_jobs" -ge 1 ] || cfg_jobs=1
    [ "$cfg_jobs" -le 8 ] || cfg_jobs=8
    [ -n "$cfg_min_free" ] || cfg_min_free=10
    [ -n "$cfg_nice" ] || cfg_nice=10
    [ -n "$cfg_path" ] || cfg_path=$PATH
    [ -n "$cfg_pg_image$cfg_pg_bin" ] || {
        echo "gate-runner: $config names no private server (GATE_RUNNER_PG_IMAGE or GATE_RUNNER_PG_BIN); a runner never uses a shared test database" >&2
        exit 3
    }
    [ -z "$cfg_pg_image" ] || [ -z "$cfg_pg_bin" ] || {
        echo "gate-runner: $config names both a docker image and a bin directory; the gate takes one" >&2
        exit 3
    }
    case "$cfg_jobs$cfg_min_free$cfg_nice" in *[!0-9]*) echo "gate-runner: $config: jobs, the disk floor and nice are numbers" >&2; exit 3 ;; esac
}

# ensure_dirs: the root's own directories, this uid's only.
ensure_dirs() {
    for dir in "$jobs" "$cache" "$work"; do
        [ -d "$dir" ] || mkdir -p "$dir"
        chmod 700 "$dir" 2>/dev/null || :
    done
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
    version=$(sha256sum "$root/bin/remote" 2>/dev/null | cut -d' ' -f1) || version="unknown"
    printf 'version=%s\n' "$version"
    printf 'root=%s\n' "$root"
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
    printf 'nproc=%s\n' "$(nproc 2>/dev/null || echo 1)"
    printf 'pg_image=%s\n' "$cfg_pg_image"
    printf 'pg_bin=%s\n' "$cfg_pg_bin"
    printf 'disk_root=%s\n' "$root"
    free_kb=$(df -Pk -- "$root" 2>/dev/null | awk 'NR == 2 { print $4 }') || free_kb=""
    printf 'disk_free_kb=%s\n' "${free_kb:-unreadable}"
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
    # about 40 bytes more). A root whose path is too long gets /tmp for the
    # job's scratch: a shorter path, at the price of the disk floor being
    # measured on /tmp instead of the root's disk (README.md).
    if [ $((${#scratch} + 40)) -gt 107 ]; then
        scratch=/tmp/gate-runner/$job
    fi
    mkdir -p "$scratch"
    export PATH=$cfg_path
    export XDG_CACHE_HOME=$cache
    export TMPDIR=$scratch
    export SPAWN_GATE_JOBS=$cfg_jobs
    export SPAWN_GATE_MIN_FREE_GB=$cfg_min_free
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

remove_job() {
    job_name "$1"
    load_config
    rm -rf "$jobs/$1" "$jobs/$1.partial" "$work/$1" "/tmp/gate-runner/$1"
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
    [ $# -eq 1 ] || { echo "usage: remote remove <job>" >&2; exit 2; }
    remove_job "$1"
    ;;
*)
    echo "usage: remote version|preflight|accept <job>|run <job> <seconds>|keys <job> <seconds>|collect <job>|remove <job>" >&2
    exit 2
    ;;
esac
