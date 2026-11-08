#!/usr/bin/env bash
# Remote gate runner, primary side (subject: remote-gate-runner, brief
# /var/tmp/spawn/sync/brief-remote-gate-runner.md). A gate run is
# self-contained and stateless, so it is the unit that can leave this
# machine: this script packages what a gate needs (a git bundle of the
# base and the candidate, the controller's scripts and scenarios, and the
# job's own run script), ships it to a runner over SSH, runs the same
# scripts/rsi/gate.sh there with the runner's private PostgreSQL and its
# own scratch, and brings the verdict and the whole log back whole.
#
#   deploy/gate-runner.sh [options] <runner> <source.git> <base> <candidate> <scenarios> <out>
#   deploy/gate-runner.sh --check [options] <runner> <source.git> <base> <scenarios>
#
# <runner> is a host `ssh` reaches (or any name the transport takes);
# <source.git> a bare repository holding both commits; <scenarios> the
# scenario directory the gate is to run (normally the controller's
# rsi/scenarios; the scripts/ of the tree two levels above it are shipped
# with it). <out> is a directory the dispatcher owns: it writes
# <out>/<job>/{verdict.json,gate.log,record.json} at the end, or nothing
# and says why.
#
# The job id names every file, log line and record on both sides: the
# candidate's first 12 characters and a nonce. Every transport retry is a
# new attempt with its own id (<job>.a1, .a2, …), its own runner
# directory, and its own line in record.json. Retries are for the
# transport only, never for a gate's verdict or for a missing result: a
# gate that ran and answered is the answer.
#
# Stages, each under its own wall-clock limit, and all under a total:
#   preflight   the runner's report (tools, server, disk, jobs)
#   ship        the archive, streamed over SSH
#   run         the gate on the runner, under `timeout` there as well (so
#               a broken SSH connection cannot leave it running)
#   collect     the result as one tar.gz carrying a MANIFEST of sha256s,
#               retried in place (it reruns nothing); a truncated stream, a
#               missing verdict, or a verdict that names other commits is a
#               failure (exit 2), never a pass.
#
# Exit status: the gate's own (0 with a verdict, accepted or rejected — the
# verdict says which — 2 abort, 3 setup, 4 harness, 5 no room, 6 host fault
# or cannot_judge) when the gate answered; 2 when the dispatcher could not
# bring a whole result back
# (transport after the retries, a stage's timeout, a missing result); 3
# when the runner is not ready (no helper, another helper version,
# missing tools, no private server). `--check` exits 0 when the runner
# would judge exactly as this host does, 1 when not, 3 when the runner
# could not be asked.
#
# Environment (the flags override): GATE_RUNNER_SSH (the transport, one
# word, default ssh), GATE_RUNNER_REMOTE_BIN (the helper's remote path,
# default $HOME/.local/share/gate-runner/bin/remote), GATE_RUNNER_RETRIES
# (default 2), GATE_RUNNER_PREFLIGHT_TIMEOUT (300), GATE_RUNNER_SHIP_TIMEOUT
# (900), GATE_RUNNER_RUN_TIMEOUT (7200), GATE_RUNNER_COLLECT_TIMEOUT (600),
# GATE_RUNNER_TOTAL_TIMEOUT (10800), GATE_RUNNER_ENV_FILE (--check: the
# integrator's gate environment file, as deploy/deploy.sh reads it),
# GATE_RUNNER_MIN_FREE_MEMORY_MB (the runner's memory floor, default 2048, 0
# off; the integrator's SPAWN_GATE_MIN_FREE_MEMORY_MB for a gate run here),
# GATE_RUNNER_REGISTRY (the cargo registry the runner's must hold, default
# ${CARGO_HOME:-$HOME/.cargo}/registry; empty: no sync).
#
# The runner must judge in the environment this host judges in, or its
# verdict is not this host's verdict. So before any job the dispatcher
# refuses (exit 3) a runner that shows the host's files to the gate (an
# OrbStack machine that is not --isolated), whose cargo config names a
# rustc-wrapper it does not have, or whose memory is under the floor; and it
# sends the runner whatever files of this host's cargo registry it lacks (the
# gate builds offline from the registry: a crate missing there is a rejection
# here of work this host accepts).
#
# Nothing here needs the live fleet: the runner gets its own server, its
# own scratch and no database URL (a URL never goes on a command line, and
# a runner never sees the primary's).
set -euo pipefail

prog=gate-runner
retries=${GATE_RUNNER_RETRIES:-2}
preflight_timeout=${GATE_RUNNER_PREFLIGHT_TIMEOUT:-300}
ship_timeout=${GATE_RUNNER_SHIP_TIMEOUT:-900}
run_timeout=${GATE_RUNNER_RUN_TIMEOUT:-7200}
collect_timeout=${GATE_RUNNER_COLLECT_TIMEOUT:-600}
total_timeout=${GATE_RUNNER_TOTAL_TIMEOUT:-10800}
ssh_cmd=${GATE_RUNNER_SSH:-ssh}
remote_bin=${GATE_RUNNER_REMOTE_BIN:-'$HOME/.local/share/gate-runner/bin/remote'}
env_file=${GATE_RUNNER_ENV_FILE:-}
min_free_memory_mb=${GATE_RUNNER_MIN_FREE_MEMORY_MB:-2048}
registry_src=${GATE_RUNNER_REGISTRY-${CARGO_HOME:-$HOME/.cargo}/registry}
keep_jobs=0
check=0
stage_dir=""

say() { printf '%s\n' "$*"; }
note() { printf '%s: %s\n' "$prog" "$*" >&2; }
die() { # <message> [status]
    local message=$1 code=${2:-2}
    printf '%s: %s\n' "$prog" "$message" >&2
    exit "$code"
}

usage() {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

while [ $# -gt 0 ]; do
    case $1 in
    --retries | --preflight-timeout | --ship-timeout | --run-timeout | \
        --collect-timeout | --total-timeout | --remote-bin | --ssh | --env-file)
        [ $# -ge 2 ] || die "$1 needs a value" 2
        case $1 in
        --retries) retries=$2 ;;
        --preflight-timeout) preflight_timeout=$2 ;;
        --ship-timeout) ship_timeout=$2 ;;
        --run-timeout) run_timeout=$2 ;;
        --collect-timeout) collect_timeout=$2 ;;
        --total-timeout) total_timeout=$2 ;;
        --remote-bin) remote_bin=$2 ;;
        --ssh) ssh_cmd=$2 ;;
        --env-file) env_file=$2 ;;
        esac
        shift 2
        ;;
    --keep) keep_jobs=1; shift ;;
    --check) check=1; shift ;;
    -h | --help) usage; exit 0 ;;
    --) shift; break ;;
    -*) die "unknown option $1 (see --help)" 2 ;;
    *) break ;;
    esac
done
case $min_free_memory_mb in '' | *[!0-9]*) die "GATE_RUNNER_MIN_FREE_MEMORY_MB is a whole number of MiB, not '$min_free_memory_mb'" 2 ;; esac
for number in "$retries" "$preflight_timeout" "$ship_timeout" "$run_timeout" "$collect_timeout" "$total_timeout"; do
    case $number in '' | *[!0-9]*) die "not a whole number of seconds: $number" 2 ;; esac
done
command -v jq >/dev/null 2>&1 || die "jq is required (the record and the verdict are read with it)" 2
command -v git >/dev/null 2>&1 || die "git is required" 2
command -v timeout >/dev/null 2>&1 || die "GNU coreutils' timeout is required" 2
command -v tar >/dev/null 2>&1 || die "tar is required" 2
command -v gzip >/dev/null 2>&1 || die "gzip is required" 2
command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required" 2

if [ "$check" = 1 ]; then
    [ $# -eq 4 ] || die "usage: $prog --check [options] <runner> <source.git> <base> <scenarios>" 3
else
    [ $# -eq 6 ] || die "usage: $prog [options] <runner> <source.git> <base> <candidate> <scenarios> <out>" 3
fi
runner=$1
source=$2
base_arg=$3
if [ "$check" = 1 ]; then
    candidate_arg=$3
    scenarios=$4
    out=""
else
    candidate_arg=$4
    scenarios=$5
    out=$6
fi

stage_dir=$(mktemp -d "${TMPDIR:-/tmp}/gate-runner.XXXXXX")
cleanup() { rm -rf "$stage_dir"; }
trap cleanup EXIT

# The commits, resolved once: everything after this names them so.
base_sha=$(git --git-dir="$source" rev-parse --verify --quiet "$base_arg^{commit}") ||
    die "no commit $base_arg in $source" 3
candidate_sha=$(git --git-dir="$source" rev-parse --verify --quiet "$candidate_arg^{commit}") ||
    die "no commit $candidate_arg in $source" 3
[ -d "$scenarios" ] || die "no scenarios directory $scenarios" 3

# The controller the scenarios belong to (the gate finds it two levels above
# them): its scripts/ are what the gate and the scenarios run. A scenario
# directory without one (a test fixture) ships this tree's.
controller=$(cd "$scenarios/../.." 2>/dev/null && pwd -P) || controller=""
if [ -z "$controller" ] || [ ! -f "$controller/scripts/rsi/gate.sh" ]; then
    controller=$(cd "$(dirname "$0")/.." && pwd -P)
    [ -f "$controller/scripts/rsi/gate.sh" ] ||
        die "no scripts/rsi/gate.sh above $scenarios, and none in $controller" 3
    say "$prog: $scenarios has no controller above it; shipping $controller/scripts"
fi
gate_script=$controller/scripts/rsi/gate.sh
local_remote_sha=$(sha256sum "$(dirname "$0")/gate-runner/remote.sh" | cut -d' ' -f1)

# The job: the candidate's first 12 characters and a nonce, every file and
# record on both sides named for it. The base is the candidate in --check.
nonce=$(od -An -N4 -tx4 /dev/urandom | tr -d ' \n')
if [ "$check" = 1 ]; then
    job="check-$(printf '%s' "$base_sha" | cut -c1-12)-$nonce"
else
    job="$(printf '%s' "$candidate_sha" | cut -c1-12)-$nonce"
fi

started_at=$(date -Is)
deadline=$((SECONDS + total_timeout))
# stage_limit <setting>: the stage's own limit, or what the total leaves;
# dies when the total is spent.
stage_limit() {
    local left=$((deadline - SECONDS)) setting=$1
    [ "$left" -gt 0 ] || die "the total timeout (${total_timeout} s) is spent" 2
    if [ "$left" -lt "$setting" ]; then
        printf '%s' "$left"
    else
        printf '%s' "$setting"
    fi
}
# ssh_run <remote command string>: the transport. The remote shell expands
# $HOME in the helper's default path; job ids carry no shell characters.
ssh_run() {
    "$ssh_cmd" -o BatchMode=yes -o ConnectTimeout=15 "$runner" "$1"
}

# ssh_limited <seconds> <remote command string>: the same under the stage's
# wall clock (timeout cannot run a shell function, so the transport itself is
# what it starts).
ssh_limited() {
    local limit=$1
    timeout --signal=TERM --kill-after=30 "$limit" \
        "$ssh_cmd" -o BatchMode=yes -o ConnectTimeout=15 "$runner" "$2"
}

# package_archive <dir> <job> <base> <candidate>: the bundle (both commits,
# with their history), the controller's scripts, the scenarios, the job's
# run scripts and a meta naming the job.
package_archive() { # <dir> <job> <base> <candidate>
    local dir=$1 pjob=$2 pbase=$3 pcandidate=$4
    local refs=$dir/.refs.git
    mkdir -p "$dir/controller"
    git init -q --bare "$refs"
    printf '%s\n' "$(git --git-dir="$source" rev-parse --absolute-git-dir)/objects" \
        >"$refs/objects/info/alternates"
    git -C "$refs" update-ref refs/gate/base "$pbase"
    git -C "$refs" update-ref refs/gate/candidate "$pcandidate"
    git -C "$refs" bundle create "$dir/gate.bundle" refs/gate/base refs/gate/candidate >/dev/null
    rm -rf "$refs"
    cp -R "$controller/scripts" "$dir/controller/scripts"
    mkdir -p "$dir/controller/rsi"
    cp -R "$scenarios" "$dir/controller/rsi/scenarios"
    printf 'job=%s\nbase=%s\ncandidate=%s\ncreated=%s\n' \
        "$pjob" "$pbase" "$pcandidate" "$(date -Is)" >"$dir/meta"
    cat >"$dir/run.sh" <<'SH'
#!/bin/sh
# The job's gate, shipped by deploy/gate-runner.sh: the controller's own
# gate, the same script judged the same way, on this runner's server and
# scratch.
set -eu
job_dir=$(cd "$(dirname "$0")" && pwd)
base=$(sed -n 's/^base=//p' "$job_dir/meta")
candidate=$(sed -n 's/^candidate=//p' "$job_dir/meta")
exec sh "$job_dir/controller/scripts/rsi/gate.sh" "$job_dir/repo.git" "$base" "$candidate" \
    "$job_dir/controller/rsi/scenarios"
SH
    cat >"$dir/keys.sh" <<'SH'
#!/bin/sh
# The job's base-cache keys, shipped by deploy/gate-runner.sh --check: the
# same gate's --base-keys, which runs no scenario and builds nothing.
set -eu
job_dir=$(cd "$(dirname "$0")" && pwd)
base=$(sed -n 's/^base=//p' "$job_dir/meta")
exec sh "$job_dir/controller/scripts/rsi/gate.sh" --base-keys "$job_dir/repo.git" "$base" \
    "$job_dir/controller/rsi/scenarios"
SH
    chmod 755 "$dir/run.sh" "$dir/keys.sh"
}

# preflight <file>: the runner's report; its status is the transport's (0
# asked and answered, 255 no connection, 124 the limit).
preflight() { # <report file>
    local file=$1 limit
    limit=$(stage_limit "$preflight_timeout")
    ssh_limited "$limit" "$remote_bin preflight" >"$file" 2>"$file.err" </dev/null
}

# pv <report> <name>: one preflight value, empty when absent.
pv() {
    sed -n "s/^$2=//p" "$1" | head -n 1
}

# ask_preflight <dir>: the report in <dir>/preflight, or a die.
ask_preflight() {
    local dir=$1 status=0
    preflight "$dir/preflight" || status=$?
    if [ "$status" != 0 ]; then
        die "the preflight failed (exit $status): $(head -n 2 "$dir/preflight.err" 2>/dev/null | tr '\n' ' ')" 3
    fi
}

# require_runner <report>: what every job needs of the runner. A missing
# tool is the runner's setup, not the candidate's: exit 3.
require_runner() {
    local report=$1 name value bad="" floor free there memory jobs cpus
    for name in sh git tar gzip sha256sum nice timeout bwrap; do
        value=$(pv "$report" "$name")
        case $value in "" | missing | unreadable) bad="$bad $name" ;; esac
    done
    [ -z "$bad" ] || die "the runner lacks:$bad (deploy/gate-runner/README.md)" 3
    case "$(pv "$report" pg_image)$(pv "$report" pg_bin)" in
    "") die "the runner names no private server (its config: GATE_RUNNER_PG_IMAGE or GATE_RUNNER_PG_BIN)" 3 ;;
    esac
    floor=$(pv "$report" min_free_gb)
    free=$(pv "$report" disk_free_kb)
    case "$floor" in '' | *[!0-9]*) die "the runner reports no usable disk floor" 3 ;; esac
    case "$free" in '' | *[!0-9]*) die "the runner cannot read its free disk" 3 ;; esac
    [ "$free" -ge $((floor * 1048576)) ] ||
        die "the runner has $((free / 1048576)) GiB free, under its floor of ${floor} GiB" 3
    case "$(pv "$report" root)" in
    /tmp/* | /run/* | /var/tmp/*)
        die "the runner's root is under a directory the gate's sandbox empties ($(pv "$report" root)): the controller's scripts in it would be invisible to the gate (reinstall under \$HOME)" 3
        ;;
    "") die "the runner reports no root" 3 ;;
    esac
    there=$(pv "$report" version)
    [ "$there" = "$local_remote_sha" ] ||
        die "the runner's helper is another version ($there, this tree's $local_remote_sha): rerun deploy/gate-runner/setup.sh there" 3
    # The environment the gate judges in (the header): not the host's
    # files, no build that cannot start, room in memory.
    case "$(pv "$report" host_shares)" in
    none) ;;
    "") die "the runner reports no host_shares (its helper predates the check)" 3 ;;
    *) die "the runner shows the host's files to the gate ($(pv "$report" host_shares)): candidate code could read them, and the gate's socket scan walks them (an OrbStack machine must be --isolated)" 3 ;;
    esac
    case "$(pv "$report" cargo_wrapper)" in
    *" missing") die "the runner's cargo config ($(pv "$report" cargo_home)) names rustc-wrapper $(pv "$report" cargo_wrapper | cut -d' ' -f1), which its gate PATH lacks: every build would fail" 3 ;;
    esac
    memory=$(pv "$report" mem_available_kb)
    if [ "$min_free_memory_mb" != 0 ]; then
        case "$memory" in '' | *[!0-9]*) die "the runner cannot read its available memory" 3 ;; esac
        [ "$memory" -ge $((min_free_memory_mb * 1024)) ] ||
            die "the runner has $((memory / 1024)) MiB of memory available, under the floor of ${min_free_memory_mb} MiB (GATE_RUNNER_MIN_FREE_MEMORY_MB)" 3
    fi
    jobs=$(pv "$report" jobs)
    cpus=$(pv "$report" cpus)
    case "$jobs$cpus" in
    '' | *[!0-9]*) ;;
    *) [ "$jobs" -le "$cpus" ] || note "the runner runs $jobs scenarios at once on $cpus cores (its config's GATE_RUNNER_JOBS)" ;;
    esac
}

# registry_missing <file>: ask the runner which files of this host's cargo
# registry it lacks; their paths into <file>. Status: the transport's.
registry_missing() {
    local file=$1 limit
    limit=$(stage_limit "$preflight_timeout")
    (cd "$registry_src" && find . -type f -printf '%P\t%s\n') |
        ssh_limited "$limit" "$remote_bin registry-need" >"$file" 2>"$file.err"
}

# sync_registry: the runner's registry holds every file of this host's, or
# a die (exit 3: the runner cannot judge as this host does). Only what it
# lacks is sent; nothing there is deleted.
sync_registry() {
    local need=$stage_dir/registry.need status=0 count limit
    [ -n "$registry_src" ] || return 0
    [ -d "$registry_src" ] || { note "no cargo registry at $registry_src; nothing to send"; return 0; }
    registry_missing "$need" || status=$?
    [ "$status" = 0 ] || die "the runner could not list its registry (exit $status): $(head -n 2 "$need.err" 2>/dev/null | tr '\n' ' ')" 3
    count=$(wc -l <"$need")
    [ "$count" != 0 ] || return 0
    say "$prog: sending $count file(s) of the cargo registry the runner lacks"
    limit=$(stage_limit "$ship_timeout")
    tar -cf - -C "$registry_src" -T "$need" |
        ssh_limited "$limit" "$remote_bin registry-accept" 2>"$need.err" ||
        die "the registry files did not reach the runner: $(head -n 2 "$need.err" 2>/dev/null | tr '\n' ' ')" 3
    registry_missing "$need" || die "the runner could not list its registry after the send" 3
    [ ! -s "$need" ] || die "the runner still lacks $(wc -l <"$need") file(s) of the registry after the send" 3
}

# verify_collected <dir>: the MANIFEST, then the gate's exit and verdict.
# Returns 0 with gate_exit set, or 1 with the reason on stdout and
# verify_kind set: `timeout` when the runner's own timeout killed the gate
# (coreutils' 124 and 137, which gate.sh never exits with itself), else
# `result`.
verify_kind=result
verify_collected() { # <dir>
    local dir=$1 verdict
    verify_kind=result
    ( cd "$dir" && sha256sum -c --quiet MANIFEST >/dev/null 2>&1 ) ||
        { echo "the result did not arrive whole (its MANIFEST does not match)"; return 1; }
    gate_exit=$(cat "$dir/exit" 2>/dev/null || true)
    case "$gate_exit" in '' | *[!0-9]*) echo "the result carries no gate exit"; return 1 ;; esac
    case "$gate_exit" in
    124 | 137)
        verify_kind=timeout
        echo "the gate was killed by the runner's timeout (exit $gate_exit)"
        return 1
        ;;
    esac
    verdict=$dir/verdict
    if [ -f "$verdict" ]; then
        [ "$(wc -l <"$verdict" | tr -d ' ')" = 1 ] ||
            { echo "the verdict is truncated or has more than one line"; return 1; }
        jq -e . "$verdict" >/dev/null 2>&1 || { echo "the verdict is not one JSON line"; return 1; }
        jq -e --arg base "$base_sha" --arg candidate "$candidate_sha" \
            '.base == $base and .candidate == $candidate' "$verdict" >/dev/null 2>&1 ||
            { echo "the verdict names other commits"; return 1; }
    fi
    case "$gate_exit" in
    # The gate's answer is 0 with a verdict, accepted or rejected; 1 is not
    # in its contract but is read the same way rather than as an abort.
    0 | 1)
        [ -f "$verdict" ] || { echo "the gate exited $gate_exit without a verdict"; return 1; }
        case "$(jq -r '.verdict // empty' "$verdict")" in
        accepted | rejected) ;;
        *) echo "the verdict is neither accepted nor rejected"; return 1 ;;
        esac
        ;;
    6)
        [ -f "$verdict" ] || { echo "the gate exited 6 without its fact"; return 1; }
        jq -e '.host_fault == true or .cannot_judge == true' "$verdict" >/dev/null ||
            { echo "the gate exited 6 with no host-fault or cannot_judge fact"; return 1; }
        ;;
    5) : ;; # the no_room fact is missing when not even that write worked
    *) : ;; # an abort or a setup failure: the gate's log is the record
    esac
    return 0
}

# result_kind <exit>: the record's word for what came back.
result_kind() {
    case $1 in
    0 | 1) echo verdict ;;
    2) echo aborted ;;
    3) echo setup ;;
    4) echo harness ;;
    5) echo no_room ;;
    6) echo host_fault ;;
    *) echo unknown ;;
    esac
}

# place <dir> <record json>: <out>/<job>, written whole or not at all.
place() {
    local dir=$1 record=$2 final=$out/$job staging=$out/.$job.staging
    [ -d "$out" ] || mkdir -p "$out"
    rm -rf "$staging"
    mkdir -p "$staging"
    printf '%s\n' "$record" >"$staging/record.json"
    [ ! -f "$dir/verdict" ] || cp "$dir/verdict" "$staging/verdict.json"
    if [ -f "$dir/gate.log" ]; then
        cp "$dir/gate.log" "$staging/gate.log"
    fi
    if [ -e "$final" ]; then
        die "$final already exists (an old job with this id)" 2
    fi
    mv "$staging" "$final"
}

# the_record <result> <exit|-> <attempts json>: the record.json, one line.
the_record() { # <result> <exit> <attempts-json> [<reason>]
    local result=$1 code=$2 attempts=$3 reason=${4:-}
    jq -cn \
        --arg job "$job" --arg runner "$runner" --arg source "$source" \
        --arg base "$base_sha" --arg candidate "$candidate_sha" --arg scenarios "$scenarios" \
        --arg result "$result" --arg reason "$reason" --argjson exit "$code" \
        --arg started "$started_at" --argjson attempts "$attempts" \
        '{job: $job, runner: $runner, source: $source, base: $base, candidate: $candidate,
          scenarios: $scenarios, started: $started, attempts: $attempts,
          result: $result, exit: $exit} + (if $reason == "" then {} else {reason: $reason} end)'
}

# ---------------------------------------------------------------------------
# --check: would this runner judge as this host does? The same archive, the
# same gate, its --base-keys on both sides, compared line for line; the
# toolchain, the server, the disk and the UI's dependencies reported too.
if [ "$check" = 1 ]; then
    bad=0
    report() { # <label> <ok|FAIL> <detail>
        local mark=ok
        [ "$2" = ok ] || { mark=FAIL; bad=1; }
        printf '  %-12s %-4s %s\n' "$1" "$mark" "$3"
    }
    package_archive "$stage_dir/pkg" "$job" "$base_sha" "$base_sha"
    say "$prog: check $runner (job $job)"
    ask_preflight "$stage_dir"
    reportlines=${stage_dir}/preflight
    if [ "$(pv "$reportlines" version)" = "$local_remote_sha" ]; then
        report "helper" ok "$(printf '%s' "$local_remote_sha" | cut -c1-12)… (same protocol as this tree's)"
    else
        report "helper" FAIL "$(pv "$reportlines" version | cut -c1-12)… on the runner, $(printf '%s' "$local_remote_sha" | cut -c1-12)… here: rerun setup.sh there"
        say "$prog: the helper's protocol differs; not asking it for keys"
        exit 1
    fi
    case "$(pv "$reportlines" root)" in
    /tmp/* | /run/* | /var/tmp/*)
        report "root" FAIL "$(pv "$reportlines" root) is under a directory the gate's sandbox empties: its controller's scripts would be invisible to the gate (reinstall under \$HOME)"
        ;;
    "") report "root" FAIL "the runner reports no root" ;;
    *) report "root" ok "$(pv "$reportlines" root)" ;;
    esac
    for name in git tar gzip sha256sum nice timeout bwrap; do
        value=$(pv "$reportlines" "$name")
        case $value in "" | missing | unreadable) report "$name" FAIL "missing" ;; *) report "$name" ok "$value" ;; esac
    done
    if [ -n "$(pv "$reportlines" pg_image)" ]; then
        report "server" ok "image $(pv "$reportlines" pg_image)"
    elif [ -n "$(pv "$reportlines" pg_bin)" ]; then
        report "server" ok "programs $(pv "$reportlines" pg_bin)"
    else
        report "server" FAIL "no private server (its config names none)"
    fi
    free=$(pv "$reportlines" disk_free_kb)
    floor=$(pv "$reportlines" min_free_gb)
    case "$free$floor" in
    *[!0-9]*) report "disk" FAIL "cannot read the free disk" ;;
    *)
        if [ "$free" -ge $((floor * 1048576)) ]; then
            report "disk" ok "$((free / 1048576)) GiB free, floor ${floor} GiB"
        else
            report "disk" FAIL "$((free / 1048576)) GiB free, floor ${floor} GiB"
        fi
        ;;
    esac
    case "$(pv "$reportlines" host_shares)" in
    none) report "isolation" ok "no host files visible to the gate" ;;
    "") report "isolation" FAIL "the runner reports no host_shares" ;;
    *) report "isolation" FAIL "the host's files are visible to the gate ($(pv "$reportlines" host_shares)): an OrbStack machine must be --isolated" ;;
    esac
    report "cores" ok "$(pv "$reportlines" cpus) (nproc $(pv "$reportlines" nproc), cpu.max '$(pv "$reportlines" cpu_max)'), $(pv "$reportlines" jobs) scenario(s) at once"
    memory=$(pv "$reportlines" mem_available_kb)
    case "$memory" in
    '' | *[!0-9]*) report "memory" FAIL "cannot read the available memory" ;;
    *)
        detail="$((memory / 1024)) MiB available (memory.max '$(pv "$reportlines" mem_max)', swap '$(pv "$reportlines" swap_max)'), floor ${min_free_memory_mb} MiB"
        if [ "$min_free_memory_mb" = 0 ] || [ "$memory" -ge $((min_free_memory_mb * 1024)) ]; then
            report "memory" ok "$detail"
        else
            report "memory" FAIL "$detail"
        fi
        ;;
    esac
    case "$(pv "$reportlines" cargo_wrapper)" in
    *" missing") report "cargo" FAIL "its config names rustc-wrapper $(pv "$reportlines" cargo_wrapper | cut -d' ' -f1), which the gate's PATH lacks" ;;
    *) report "cargo" ok "rustc-wrapper $(pv "$reportlines" cargo_wrapper), home $(pv "$reportlines" cargo_home)" ;;
    esac
    if [ -n "$registry_src" ] && [ -d "$registry_src" ]; then
        if registry_missing "$stage_dir/registry.need"; then
            report "registry" ok "$(wc -l <"$stage_dir/registry.need") file(s) of this host's to send at dispatch"
        else
            report "registry" FAIL "the runner could not list its registry: $(head -n 1 "$stage_dir/registry.need.err" 2>/dev/null)"
        fi
    fi
    controller_lock=""
    [ ! -f "$controller/web/pnpm-lock.yaml" ] ||
        controller_lock=$(sha256sum "$controller/web/pnpm-lock.yaml" | cut -d' ' -f1)
    if [ -z "$controller_lock" ]; then
        report "web deps" ok "the controller ships no web lockfile"
    else
        case "$(pv "$reportlines" web_lock)" in
        "$controller_lock") report "web deps" ok "pnpm-lock $(printf '%s' "$controller_lock" | cut -c1-12)…" ;;
        "") report "web deps" FAIL "no web/node_modules install on the runner (setup.sh --web)" ;;
        *) report "web deps" FAIL "the runner's install is another lockfile ($(pv "$reportlines" web_lock | cut -c1-12)…, the controller's $(printf '%s' "$controller_lock" | cut -c1-12)…)" ;;
        esac
    fi
    for name in rustc cargo node psql python3 jq; do
        value=$(pv "$reportlines" "$name")
        case $value in
        "" | missing | unreadable) report "$name" FAIL "missing on the runner (a gate builds and asks them)" ;;
        *) report "$name" ok "runner: $value" ;;
        esac
    done
    if [ "$bad" != 0 ]; then
        say "$prog: the runner is not ready (above); not asking it for keys"
        exit 1
    fi

    # The keys: the runner's from the shipped gate, this host's from the
    # same script under the gate environment the integrator gives a gate
    # (--env-file) or this shell's.
    limit=$(stage_limit "$ship_timeout")
    if ! tar -czf - -C "$stage_dir/pkg" . |
        ssh_limited "$limit" "$remote_bin accept $job" 2>"$stage_dir/ship.err"; then
        note "the ship failed: $(head -n 2 "$stage_dir/ship.err" 2>/dev/null | tr '\n' ' ')"
        exit 3
    fi
    limit=$(stage_limit "$run_timeout")
    run_status=0
    ssh_limited "$limit" "$remote_bin keys $job $limit" \
        </dev/null >"$stage_dir/keys.out" 2>"$stage_dir/keys.err" || run_status=$?
    if [ "$run_status" != 0 ]; then
        note "the runner's key run failed (exit $run_status): $(head -n 2 "$stage_dir/keys.err" 2>/dev/null | tr '\n' ' ')"
        exit 3
    fi
    mkdir -p "$stage_dir/remote"
    limit=$(stage_limit "$collect_timeout")
    if ! ssh_limited "$limit" "$remote_bin collect $job" </dev/null |
        tar -xzf - -C "$stage_dir/remote" 2>"$stage_dir/collect.err"; then
        note "the runner's keys did not come back whole: $(head -n 2 "$stage_dir/collect.err" 2>/dev/null | tr '\n' ' ')"
        exit 3
    fi
    grep '^key ' "$stage_dir/remote/keys.log" 2>/dev/null | LC_ALL=C sort >"$stage_dir/remote.keys" || true
    remote_exit=$(cat "$stage_dir/remote/exit" 2>/dev/null || echo "?")
    local_status=0
    if [ -n "$env_file" ]; then
        [ -f "$env_file" ] || die "no gate environment file $env_file" 3
        keep=() own="" test_url="" home=""
        # shellcheck source=deploy/gate-env.sh
        . "$(dirname "$0")/gate-env.sh"
        gate_env_read "$env_file"
        env -i "${keep[@]}" sh "$gate_script" --base-keys "$source" "$base_sha" "$scenarios" \
            >"$stage_dir/local.log" 2>&1 || local_status=$?
    else
        sh "$gate_script" --base-keys "$source" "$base_sha" "$scenarios" \
            >"$stage_dir/local.log" 2>&1 || local_status=$?
    fi
    if [ "$local_status" != 0 ]; then
        note "this host's key run failed (exit $local_status): $(tail -n 3 "$stage_dir/local.log" | tr '\n' ' ')"
        exit 3
    fi
    grep '^key ' "$stage_dir/local.log" 2>/dev/null | LC_ALL=C sort >"$stage_dir/local.keys" || true
    local_count=$(grep -c '^key ' "$stage_dir/local.keys" || true)
    remote_count=$(grep -c '^key ' "$stage_dir/remote.keys" || true)
    report "this host" ok "$local_count key(s), $(sed -n 's/^| base cache: toolchain //p' "$stage_dir/local.log" | head -n 1)"
    report "runner" ok "$remote_count key(s), exit $remote_exit, $(pv "$reportlines" root)/jobs/$job/out/keys.log"
    if [ "$remote_exit" != 0 ]; then
        report "base keys" FAIL "the runner's key run exited $remote_exit"
        note "its log: $(tail -n 3 "$stage_dir/remote/keys.log" 2>/dev/null | tr '\n' ' ')"
    elif [ "$local_count" = 0 ] && [ "$remote_count" = 0 ]; then
        # Equal and empty is no comparison at all: the scenarios directory
        # holds no objective the base phase would judge.
        report "base keys" FAIL "no objective scenario to compare (this directory has none the base phase judges; nothing would be shared)"
    elif cmp -s "$stage_dir/local.keys" "$stage_dir/remote.keys"; then
        report "base keys" ok "equal ($local_count scenario(s)): a base result cached here is a hit there"
    else
        report "base keys" FAIL "differ (this host $local_count, runner $remote_count)"
        diff -u "$stage_dir/local.keys" "$stage_dir/remote.keys" 2>/dev/null | sed -n '3,20p' | sed 's/^/      /' || true
        note "toolchain, the server's version or LANG differ; the two logs are $stage_dir/local.log and the runner's job log"
    fi
    if [ "$keep_jobs" = 0 ]; then
        ssh_run "$remote_bin remove $job" </dev/null >/dev/null 2>&1 ||
            note "could not remove the runner's job $job"
    else
        say "$prog: kept the runner's job $job"
    fi
    [ "$bad" = 0 ] || exit 1
    exit 0
fi

# ---------------------------------------------------------------------------
# The dispatch.
say "$prog: job $job ($runner): $(printf '%s' "$base_sha" | cut -c1-12) -> $(printf '%s' "$candidate_sha" | cut -c1-12)"
ask_preflight "$stage_dir"
require_runner "$stage_dir/preflight"
sync_registry

attempt=0
max_attempts=$((retries + 1))
attempts_json=()
attempt_ids=()
gate_exit=""
collected_ok=0
while [ "$attempt" -lt "$max_attempts" ]; do
    attempt=$((attempt + 1))
    id="$job.a$attempt"
    attempt_ids+=("$id")
    attempt_dir=$stage_dir/$id
    mkdir -p "$attempt_dir"
    started=$SECONDS
    record='{"id":"'"$id"'"'
    failed=""
    package_archive "$attempt_dir/pkg" "$id" "$base_sha" "$candidate_sha"
    say "$prog: attempt $attempt/$max_attempts ($id)"
    # ship
    ship_status=0
    limit=$(stage_limit "$ship_timeout")
    tar -czf - -C "$attempt_dir/pkg" . |
        ssh_limited "$limit" "$remote_bin accept $id" \
            2>"$attempt_dir/ship.err" || ship_status=$?
    if [ "$ship_status" = 0 ]; then
        record="$record,\"ship\":\"ok\""
    else
        if [ "$ship_status" = 124 ] || [ "$ship_status" = 137 ]; then
            failed=timeout
        else
            failed=transport
        fi
        record="$record,\"ship\":\"failed($ship_status)\""
        note "attempt $id: the ship failed (exit $ship_status): $(head -n 2 "$attempt_dir/ship.err" 2>/dev/null | tr '\n' ' ')"
    fi
    # run
    if [ -z "$failed" ]; then
        limit=$(stage_limit "$run_timeout")
        run_status=0
        ssh_limited "$((limit + 60))" "$remote_bin run $id $limit" \
            </dev/null >"$attempt_dir/run.out" 2>"$attempt_dir/run.err" || run_status=$?
        case $run_status in
        0) record="$record,\"run\":\"ok\"" ;;
        124 | 137)
            failed=timeout
            record="$record,\"run\":\"timeout\""
            note "attempt $id: the run lasted past $((limit + 60)) s here (the runner's own limit was $limit s): $(head -n 2 "$attempt_dir/run.err" 2>/dev/null | tr '\n' ' ')"
            ;;
        255)
            failed=transport
            record="$record,\"run\":\"no-connection\""
            note "attempt $id: the SSH connection died in the run"
            ;;
        *)
            failed=result
            record="$record,\"run\":\"failed($run_status)\""
            note "attempt $id: the runner's helper failed the run (exit $run_status): $(head -n 2 "$attempt_dir/run.err" 2>/dev/null | tr '\n' ' ')"
            ;;
        esac
    fi
    # collect: retried in place (it reruns nothing), then the attempt ends.
    if [ -z "$failed" ]; then
        tries=0
        collect_label=""
        while [ "$tries" -lt 3 ]; do
            tries=$((tries + 1))
            rm -rf "$attempt_dir/collected" "$attempt_dir/collect.why"
            mkdir -p "$attempt_dir/collected"
            collect_status=0
            limit=$(stage_limit "$collect_timeout")
            ssh_limited "$limit" "$remote_bin collect $id" </dev/null |
                tar -xzf - -C "$attempt_dir/collected" 2>"$attempt_dir/collect.err" || collect_status=$?
            if [ "$collect_status" = 0 ] &&
                verify_collected "$attempt_dir/collected" >"$attempt_dir/collect.why"; then
                collected_ok=1
                collect_label=ok
                break
            fi
            collect_label="failed($collect_status)"
            note "attempt $id: the collection failed (try $tries, exit $collect_status): $(head -n 1 "$attempt_dir/collect.why" "$attempt_dir/collect.err" 2>/dev/null | tr '\n' ' ')"
            if [ "$verify_kind" = timeout ]; then
                failed=timeout
                break
            fi
        done
        [ -z "$collect_label" ] || record="$record,\"collect\":\"$collect_label\""
        if [ "$collected_ok" != 1 ] && [ -z "$failed" ]; then
            failed=result
        fi
    fi
    seconds=$((SECONDS - started))
    if [ "$collected_ok" = 1 ]; then
        record="$record,\"gate_exit\":$gate_exit,\"seconds\":$seconds,\"result\":\"$(result_kind "$gate_exit")\"}"
        attempts_json+=("$record")
        break
    fi
    record="$record,\"seconds\":$seconds,\"result\":\"${failed}\"}"
    attempts_json+=("$record")
    if [ "$failed" != transport ]; then
        break
    fi
    if [ "$attempt" -lt "$max_attempts" ]; then
        note "attempt $id: transport failure; retrying"
    else
        note "attempt $id: transport failure; no attempts left"
    fi
done

attempts=$(printf '%s\n' "${attempts_json[@]+"${attempts_json[@]}"}" | jq -s '.')

if [ "$collected_ok" != 1 ]; then
    case "${failed:-transport}" in
    timeout) reason="a stage timed out" ;;
    transport) reason="the transport failed on every attempt" ;;
    *) reason="the result did not come back whole" ;;
    esac
    mkdir -p "$stage_dir/none"
    record=$(the_record "${failed:-transport}" null "$attempts" "$reason")
    place "$stage_dir/none" "$record"
    printf '%s\n' "$record"
    note "no verdict: $reason (record in $out/$job; the runner's job(s) ${attempt_ids[*]} were left for a look)"
    exit 2
fi

verdict_word=""
[ ! -f "$attempt_dir/collected/verdict" ] || verdict_word=$(jq -r '.verdict // empty' "$attempt_dir/collected/verdict")
record=$(the_record "$(result_kind "$gate_exit")" "$gate_exit" "$attempts")
place "$attempt_dir/collected" "$record"
if [ "$keep_jobs" = 0 ]; then
    # The job lives on the runner under its attempts' ids. Every attempt is
    # asked to go, but the helper refuses one whose run has not finished
    # (out/exit is missing: a run whose connection died keeps going under
    # the runner's own timeout, and deleting its directory under it would
    # strand its server). What stays is said.
    for attempt_job in "${attempt_ids[@]}"; do
        remove_status=0
        ssh_run "$remote_bin remove $attempt_job" </dev/null >/dev/null 2>"$stage_dir/remove.err" ||
            remove_status=$?
        if [ "$remove_status" != 0 ]; then
            note "left the runner's job $attempt_job: $(head -n 1 "$stage_dir/remove.err" 2>/dev/null)"
        fi
    done
else
    say "$prog: kept the runner's jobs ${attempt_ids[*]}"
fi
printf '%s\n' "$record"
case "$gate_exit" in
0 | 1) say "$prog: $verdict_word (job $job, record in $out/$job)" ;;
*) say "$prog: $(result_kind "$gate_exit") (exit $gate_exit, job $job, record in $out/$job)" ;;
esac
exit "$gate_exit"
