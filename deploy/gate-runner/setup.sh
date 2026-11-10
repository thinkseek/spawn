#!/bin/sh
# Installs the runner side of the remote gate runner (subject:
# remote-gate-runner): <root>/config, <root>/bin/remote (remote.sh, the
# dispatcher's protocol) and the directories a job uses, on the machine
# that runs gates for the primary. Run it once per machine, from a login
# shell whose PATH finds the same rustc/cargo toolchain the primary builds
# with (the gate's sandbox probe asks them; the config records the PATH).
#
#   deploy/gate-runner/setup.sh [options]
#
#   --root <dir>        where the runner keeps its files
#                       (default $HOME/.local/share/gate-runner)
#   --web <dir>         a checkout's web/ with its node_modules installed
#                       (the same lockfile as the controller's): linked in
#                       as <root>/web/node_modules, so check-web.sh builds
#                       the UI; without it --check reports the gap
#   --pg-image <image>  the private server's docker image
#                       (default postgres:16-alpine when docker is here)
#   --pg-bin <dir>      or the PostgreSQL programs' directory
#                       (default: the newest /usr/lib/postgresql/*/bin)
#   --jobs <n>          scenarios at once, SPAWN_GATE_JOBS (default: half
#                       the cores the gate really gets, at most 8, measured
#                       by remote.sh on every run: nproc capped by cpu.max)
#   --min-free-gb <n>   the gate's disk floor (default 10)
#   --disk-limit-gb <n> the machine's disk limit when df cannot see it (an
#                       OrbStack --disk limit; default 0, none)
#   --cargo-home <dir>  the CARGO_HOME the gate binds (default $HOME/.cargo)
#   --nice <n>          the gate's nice level (default 10)
#   --keep-days <n>     prune job directories older than this on the runner
#                       (default 7; 0 keeps them)
#   --force             rewrite an existing config (it is the owner's; a
#                       plain rerun keeps it and only reinstalls remote.sh)
#
# What the machine must have is deploy/gate-runner/README.md; the run ends
# by printing `remote preflight`'s report, which is what --check reads.
set -eu

here=$(cd "$(dirname "$0")" && pwd -P)
root=${GATE_RUNNER_ROOT:-$HOME/.local/share/gate-runner}
web=""
pg_image=""
pg_bin=""
jobs=""
min_free=""
nice=""
keep_days=""
disk_limit=""
cargo_home=""
force=""

say() { printf '%s\n' "$*"; }
die() { printf 'gate-runner setup: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case $1 in
    --root) shift; [ $# -gt 0 ] || die "--root needs a directory"; root=$1 ;;
    --web) shift; [ $# -gt 0 ] || die "--web needs a directory"; web=$1 ;;
    --pg-image) shift; [ $# -gt 0 ] || die "--pg-image needs an image"; pg_image=$1 ;;
    --pg-bin) shift; [ $# -gt 0 ] || die "--pg-bin needs a directory"; pg_bin=$1 ;;
    --jobs) shift; [ $# -gt 0 ] || die "--jobs needs a number"; jobs=$1 ;;
    --min-free-gb) shift; [ $# -gt 0 ] || die "--min-free-gb needs a number"; min_free=$1 ;;
    --nice) shift; [ $# -gt 0 ] || die "--nice needs a number"; nice=$1 ;;
    --keep-days) shift; [ $# -gt 0 ] || die "--keep-days needs a number"; keep_days=$1 ;;
    --disk-limit-gb) shift; [ $# -gt 0 ] || die "--disk-limit-gb needs a number"; disk_limit=$1 ;;
    --cargo-home) shift; [ $# -gt 0 ] || die "--cargo-home needs a directory"; cargo_home=$1 ;;
    --force) force=1 ;;
    -h | --help)
        awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
        exit 0
        ;;
    *) die "unknown argument $1 (see --help)" ;;
    esac
    shift
done

for tool in git tar gzip sha256sum nice timeout flock bwrap nproc; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required (deploy/gate-runner/README.md)"
done
case "$(timeout --version 2>&1 | head -n 1)" in
*" (GNU coreutils) "*) ;;
*) die "GNU coreutils' timeout with --kill-after is required (deploy/gate-runner/README.md)" ;;
esac
case "$root" in
/*) ;;
*) die "--root must be absolute: $root" ;;
esac
# Not where the gate's sandbox replaces the host's directories: it would not
# see the controller's scripts inside the root (scripts/ is bound back only
# under $HOME; elsewhere the sandbox shows the host's files, except /tmp,
# /run and /var/tmp, which it empties). This shell's TMPDIR is not one of
# them: the gate never runs with it (deploy/gate-runner/remote.sh gives each
# job a scratch of its own), so a root under it is fine.
case "$root" in
/tmp/* | /run/* | /var/tmp/*)
    die "the root must not be under /tmp, /run or /var/tmp: the gate's sandbox empties those, so it could not see the controller's scripts in $root (the default, \$HOME/.local/share/gate-runner, is under \$HOME)"
    ;;
esac

# The defaults, from what this machine has. No --jobs leaves it to remote.sh,
# which measures the cores a gate gets on every run (a machine's limits can
# change after setup; nproc here would freeze the VM's count).
if [ -n "$jobs" ]; then
    case "$jobs" in '' | *[!0-9]*) die "not a whole number: $jobs" ;; esac
    [ "$jobs" -ge 1 ] || jobs=1
    [ "$jobs" -le 8 ] || jobs=8
fi
[ -n "$min_free" ] || min_free=10
[ -n "$nice" ] || nice=10
[ -n "$keep_days" ] || keep_days=7
[ -n "$disk_limit" ] || disk_limit=0
for number in "$min_free" "$nice" "$keep_days" "$disk_limit"; do
    case "$number" in '' | *[!0-9]*) die "not a whole number: $number" ;; esac
done
case "$cargo_home" in
'' | /*) ;;
*) die "--cargo-home must be absolute: $cargo_home" ;;
esac
if [ -z "$pg_image$pg_bin" ]; then
    if command -v docker >/dev/null 2>&1; then
        pg_image=postgres:16-alpine
    else
        for dir in /usr/lib/postgresql/*/bin; do
            [ -x "$dir/postgres" ] && pg_bin=$dir
        done
        pg_bin=$(printf '%s\n' $pg_bin | sort -V | tail -n 1)
    fi
fi
[ -n "$pg_image$pg_bin" ] ||
    die "no private server: docker is not here and no /usr/lib/postgresql/*/bin has postgres; give --pg-image or --pg-bin"
[ -z "$pg_image" ] || [ -z "$pg_bin" ] || die "give one of --pg-image and --pg-bin, not both"
if [ -n "$pg_bin" ] && [ ! -x "$pg_bin/postgres" ]; then
    die "$pg_bin has no postgres program"
fi
if [ -n "$pg_image" ] && ! command -v docker >/dev/null 2>&1; then
    die "--pg-image $pg_image needs docker"
fi

for dir in "$root" "$root/bin" "$root/jobs" "$root/cache" "$root/work"; do
    [ -d "$dir" ] || mkdir -p "$dir"
    chmod 700 "$dir"
done

# remote.sh: the protocol the primary speaks. Its sha256 is what the
# dispatcher checks against its own copy, so never edit the installed copy.
install -m 755 "$here/remote.sh" "$root/bin/remote"

config=$root/config
if [ -e "$config" ] && [ -z "$force" ]; then
    say "kept the existing $config (--force rewrites it)"
else
    {
        printf '%s\n' "# Written by deploy/gate-runner/setup.sh. This machine's gate settings."
        printf 'GATE_RUNNER_JOBS=%s\n' "$jobs"
        printf "GATE_RUNNER_PG_IMAGE='%s'\n" "$pg_image"
        printf "GATE_RUNNER_PG_BIN='%s'\n" "$pg_bin"
        printf 'GATE_RUNNER_MIN_FREE_GB=%s\n' "$min_free"
        printf 'GATE_RUNNER_NICE=%s\n' "$nice"
        printf "GATE_RUNNER_PATH='%s'\n" "$PATH"
        printf 'GATE_RUNNER_KEEP_JOBS=%s\n' 0
        printf 'GATE_RUNNER_KEEP_DAYS=%s\n' "$keep_days"
        printf 'GATE_RUNNER_DISK_LIMIT_GB=%s\n' "$disk_limit"
        printf "GATE_RUNNER_CARGO_HOME='%s'\n" "$cargo_home"
    } >"$config"
    chmod 600 "$config"
    say "wrote $config"
fi

# The UI's installed dependencies, for check-web.sh: linked in, never
# copied (one install serves every job). The lockfile's stamp is what
# --check compares with the controller's.
if [ -n "$web" ]; then
    [ -d "$web/node_modules" ] || die "$web has no node_modules (install its dependencies first)"
    [ -f "$web/pnpm-lock.yaml" ] || [ -f "$web/package-lock.json" ] ||
        die "$web has neither pnpm-lock.yaml nor package-lock.json"
    lockfile=$web/pnpm-lock.yaml
    [ -f "$lockfile" ] || lockfile=$web/package-lock.json
    mkdir -p "$root/web"
    ln -sfn "$(cd "$web" && pwd -P)/node_modules" "$root/web/node_modules"
    sha256sum "$lockfile" | cut -d' ' -f1 >"$root/web/lock.sha256"
    chmod 600 "$root/web/lock.sha256"
    say "linked $(cd "$web" && pwd -P)/node_modules as $root/web/node_modules ($(basename "$lockfile") $(cut -c1-12 "$root/web/lock.sha256")…)"
fi

say "checks:"
"$root/bin/remote" preflight | sed 's/^/  /'
say ""
say "next: deploy/gate-runner.sh --check <this machine> <source.git> <base> <scenarios> on the primary"
