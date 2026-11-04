#!/bin/sh
# The gate: one black-box judge for every landing, RSI or fleet
# (docs/fleet-architecture.md §10.2).
#
# usage: gate.sh <bare-repo> <base-commit> <candidate-commit> <scenarios-dir> [<scenario>]
#
# Exports both commits to temporary directories and judges the candidate
# with the controller's scenarios, never the trees' own: a candidate cannot
# weaken the test that judges it.
#
# - The candidate may not change a path listed in `<scenarios-dir>/protected`
#   (one prefix per line, `#` comments): the controller's tasks, scenarios,
#   gate and black-box tests.
# - It may not add build-time code: a changed `build.rs` or `.cargo/`, or a
#   `build =` key in any Cargo.toml of the tree (cargo would run the file it
#   names as a build script).
# - `<scenarios-dir>/build`, when present, builds each tree (run inside it).
# - With a task scenario: it must fail on the base and pass on the candidate.
# - Every other scenario in the directory must pass on the candidate, except
#   ratchet scenarios (a line `# rsi: ratchet`), which compare against the
#   base and so fail on any tree that did not improve.
#
# Scenarios run inside the tree with SPAWN_BIN (the tree's `spawn`, when the
# build made one) and SPAWN_BASELINE_TREE set. Their output is printed with a
# `| ` prefix. The verdict is one JSON line, last, on stdout; when the
# candidate's task run printed exactly one `METRIC` line on its stdout, the
# verdict carries it as "metric" (the controller reads the metric from the
# verdict only).
#
# The sandbox. Everything the candidate controls runs inside bubblewrap
# (`bwrap`, required): the build hook, the scenarios and the checks, and
# `cargo metadata` on the candidate's manifests. Inside it the root is
# read-only, /tmp is private, the PID namespace is new (the gate and the
# integrator are not in /proc), the tree under test is the only writable
# path, the base tree is read-only, and the owner's secrets under $HOME (see
# `masked` below), the user's runtime directory and the docker socket are
# hidden. The environment is cleared down to the list below. The network is
# shared: the checks reach the test database. Cargo gets a private
# CARGO_HOME whose registry and git caches are the real ones, read-only, and
# runs offline: a candidate cannot fetch a new crate or rewrite a cached one.
# The checks find the controller's checkout through the read-only root, so
# the scenarios directory must not live under /tmp or a masked path.
#
# Environment (the integrator passes these and nothing else, gate.rs):
#   PATH, HOME, LANG, TMPDIR   as usual
#   CARGO_HOME, RUSTUP_HOME    when the toolchain is not under $HOME
#   SPAWN_TEST_DATABASE_URL    the throwaway database the checks may use
#   SPAWN_GATE_VERDICT         a file in a directory the integrator owns; the
#                              verdict is written there too, and the
#                              integrator believes that file only, and only
#                              when the gate exits 0. The directory is hidden
#                              from the sandbox.
#
# Exit status: 0 with a verdict; anything else is an infrastructure failure
# (no bwrap, a git error, a killed gate), which the integrator records as
# an abort.

set -eu

repo=$1
baseline=$2
candidate=$3
scenarios=$4
scenario=${5-}

if ! command -v bwrap >/dev/null 2>&1; then
    echo "gate: bwrap is required to run candidate code" >&2
    exit 3
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

metric=""

# JSON string body: backslash, quote and tab escaped, newlines as \n, other
# control characters dropped.
json_string() {
    printf '%s' "$1" | tr -d '\000-\010\013-\037' |
        sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e "s/$(printf '\t')/\\\\t/g" |
        awk 'NR > 1 { printf "%s", "\\n" } { printf "%s", $0 }'
}

# verdict <accepted|rejected> [reason]
verdict() {
    json="{\"verdict\":\"$1\""
    [ -n "${2-}" ] && json="$json,\"reason\":\"$(json_string "$2")\""
    [ -n "$metric" ] && json="$json,\"metric\":\"$(json_string "$metric")\""
    json="$json}"
    if [ -n "${SPAWN_GATE_VERDICT-}" ]; then
        printf '%s\n' "$json" >"$SPAWN_GATE_VERDICT"
    fi
    echo "$json"
}

# The controller resolves these paths against its own checkout. A relative
# path would resolve inside the tree under test.
for path in "$scenarios" ${scenario:+"$scenario"}; do
    case "$path" in
    /*) ;;
    *)
        verdict rejected "scenario path is not absolute"
        exit 0
        ;;
    esac
done
if [ ! -d "$scenarios" ]; then
    verdict rejected "scenarios directory is missing"
    exit 0
fi
if [ -n "$scenario" ] && [ ! -f "$scenario" ]; then
    verdict rejected "scenario is missing"
    exit 0
fi

# The owner's secrets: hidden from every sandboxed process.
masked=".config .claude .claude.json .ssh .local/share/spawn-rsi .codex .docker .gnupg
.netrc .git-credentials .npmrc .pgpass .aws .kube .cargo/credentials .cargo/credentials.toml"
cargo_home=${CARGO_HOME:-$HOME/.cargo}

# sandbox <tree> <command...>: run a command inside the tree, in bwrap.
sandbox() {
    tree=$1
    shift
    count=$#
    set -- "$@" bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
        --unshare-pid --die-with-parent --new-session
    for name in $masked; do
        if [ -d "$HOME/$name" ]; then
            set -- "$@" --tmpfs "$HOME/$name"
        elif [ -e "$HOME/$name" ]; then
            set -- "$@" --ro-bind /dev/null "$HOME/$name"
        fi
    done
    runtime=/run/user/$(id -u)
    [ -d "$runtime" ] && set -- "$@" --tmpfs "$runtime"
    for socket in /run/docker.sock /var/run/docker.sock; do
        [ -S "$socket" ] && set -- "$@" --ro-bind /dev/null "$socket"
    done
    # The host's TMPDIR when it is not /tmp (unless it holds $HOME), the
    # gate's own files and the integrator's verdict directory, wherever
    # TMPDIR put them.
    case "${TMPDIR:-/tmp}" in
    / | /tmp | /tmp/ | "$HOME" | "$HOME"/*) ;;
    *)
        case "$HOME/" in
        "${TMPDIR%/}"/*) ;;
        *) [ -d "$TMPDIR" ] && set -- "$@" --tmpfs "$TMPDIR" ;;
        esac
        ;;
    esac
    set -- "$@" --tmpfs "$work"
    if [ -n "${SPAWN_GATE_VERDICT-}" ]; then
        set -- "$@" --tmpfs "$(dirname "$SPAWN_GATE_VERDICT")"
    fi
    set -- "$@" --ro-bind "$scenarios" "$scenarios"
    [ -n "$scenario" ] && set -- "$@" --ro-bind "$scenario" "$scenario"
    if [ "$tree" != "$work/baseline" ] && [ -d "$work/baseline" ]; then
        set -- "$@" --ro-bind "$work/baseline" "$work/baseline"
    fi
    set -- "$@" --bind "$tree" "$tree"
    # Cargo: a private home over the real caches, read-only, offline.
    set -- "$@" --dir /tmp/cargo-home
    for name in registry git config config.toml; do
        [ -e "$cargo_home/$name" ] && set -- "$@" --ro-bind "$cargo_home/$name" "/tmp/cargo-home/$name"
    done
    set -- "$@" --clearenv --setenv HOME "$HOME" --setenv PATH "$PATH" --setenv TMPDIR /tmp \
        --setenv CARGO_HOME /tmp/cargo-home --setenv CARGO_NET_OFFLINE true \
        --setenv npm_config_cache /tmp/npm-cache
    for name in LANG RUSTUP_HOME SPAWN_TEST_DATABASE_URL; do
        eval "value=\${$name-}"
        [ -n "$value" ] && set -- "$@" --setenv "$name" "$value"
    done
    set -- "$@" --chdir "$tree" --
    # The command was given first: move it after the options.
    while [ "$count" -gt 0 ]; do
        set -- "$@" "$1"
        shift
        count=$((count - 1))
    done
    "$@" </dev/null
}

extract() {
    mkdir -p "$2"
    git --git-dir="$repo" archive "$1" | tar -x -C "$2"
}

# 0 = built (or nothing to build), 2 = did not build.
build() {
    [ -f "$scenarios/build" ] || return 0
    sandbox "$1" sh "$scenarios/build" >"$work/build.log" 2>&1 || {
        sed 's/^/| build: /' "$work/build.log" | tail -n 30
        return 2
    }
}

# Run one scenario in a built tree, echoing its output prefixed with `| `.
# Its stdout is also kept apart in run.out: the metric is read from there,
# never from stderr.
run_scenario() {
    rm -f "$work/run.status"
    {
        {
            status=0
            sandbox "$1" env SPAWN_BIN="$1/target/debug/spawn" SPAWN_BASELINE_TREE="$work/baseline" \
                CARGO_TARGET_DIR="$1/target" sh "$2" || status=$?
            echo "$status" >"$work/run.status"
        } | tee "$work/run.out"
    } >"$work/run.log" 2>&1
    sed 's/^/| /' "$work/run.log" | tail -n 60
    return "$(cat "$work/run.status" 2>/dev/null || echo 1)"
}

changed=$(git --git-dir="$repo" diff --name-only "$baseline" "$candidate")

touched=""
if [ -f "$scenarios/protected" ]; then
    touched=$(printf '%s\n' "$changed" | awk -v list="$scenarios/protected" '
        BEGIN { while ((getline line < list) > 0) if (line != "" && line !~ /^#/) prefix[++n] = line }
        { for (i = 1; i <= n; i++) if (index($0, prefix[i]) == 1) { print; break } }')
fi
if [ -n "$touched" ]; then
    echo "| candidate touches protected paths:$(echo "$touched" | tr '\n' ' ')"
    verdict rejected "candidate touches protected paths"
    exit 0
fi

# Build scripts and cargo configuration run code at build time, before any
# scenario judges the candidate. No task needs them; refuse them.
buildtime=$(printf '%s\n' "$changed" | grep -E '(^|/)build\.rs$|(^|/)\.cargo/' || true)
if [ -n "$buildtime" ]; then
    echo "| candidate adds build-time code: $(echo "$buildtime" | tr '\n' ' ')"
    verdict rejected "candidate adds build-time code (build.rs or .cargo/)"
    exit 0
fi

extract "$baseline" "$work/baseline"
extract "$candidate" "$work/candidate"

# `[package] build = "tools/gen.rs"` names a build script by another name.
# Any `build` key in any Cargo.toml is refused, quoted or dotted, in a
# comment or not: a false refusal costs a human a look, a miss runs code.
buildkey=$(cd "$work/candidate" && find . -name Cargo.toml -type f -exec \
    grep -lE '(^|[^A-Za-z0-9_-])["'\'']?build["'\'']?[[:space:]]*=' {} + || true)
if [ -n "$buildkey" ]; then
    echo "| candidate names a build script in: $(echo "$buildkey" | sed 's|^\./||' | tr '\n' ' ')"
    verdict rejected "candidate adds build-time code (a build key in Cargo.toml)"
    exit 0
fi
# What the text check cannot see (an escaped key, a `build.rs` the base
# already had), cargo's own reading of the manifests does: no workspace
# member may have a build script. `cargo metadata --no-deps` reads manifests
# and runs nothing; it runs sandboxed all the same.
if [ -f "$work/candidate/Cargo.toml" ] && command -v cargo >/dev/null 2>&1; then
    if ! sandbox "$work/candidate" cargo metadata --no-deps --offline --format-version 1 \
        >"$work/metadata.json" 2>"$work/metadata.log"; then
        sed 's/^/| metadata: /' "$work/metadata.log" | tail -n 20
        verdict rejected "candidate's manifests do not parse"
        exit 0
    fi
    if grep -q '"custom-build"' "$work/metadata.json"; then
        echo "| a workspace member of the candidate has a build script"
        verdict rejected "candidate adds build-time code (a build script target)"
        exit 0
    fi
fi

if [ -n "$scenario" ]; then
    echo "| == base: $(basename "$scenario") must fail"
    if ! build "$work/baseline"; then
        verdict rejected "base does not build"
        exit 0
    fi
    red=0
    run_scenario "$work/baseline" "$scenario" || red=$?
    if [ "$red" -eq 0 ]; then
        verdict rejected "scenario already passes on the base"
        exit 0
    fi
fi

echo "| == candidate: build"
if ! build "$work/candidate"; then
    verdict rejected "candidate does not build"
    exit 0
fi

if [ -n "$scenario" ]; then
    echo "| == candidate: $(basename "$scenario") must pass"
    green=0
    run_scenario "$work/candidate" "$scenario" || green=$?
    # The metric is the scenario's: one line on its stdout. Stderr is never
    # read, and two lines mean candidate code printed one too, so neither
    # counts.
    metrics=$(grep -E '^METRIC [A-Za-z0-9_]+( [a-z]+=[0-9.]+)*$' "$work/run.out" 2>/dev/null || true)
    case "$(printf '%s' "$metrics" | grep -c . || true)" in
    0) ;;
    1) metric=$metrics ;;
    *) echo "| two or more METRIC lines on the scenario's stdout: no metric" ;;
    esac
    if [ "$green" -ne 0 ]; then
        verdict rejected "scenario fails on the candidate"
        exit 0
    fi
fi

for other in "$scenarios"/*.sh; do
    [ -f "$other" ] || continue
    [ "$other" = "$scenario" ] && continue
    grep -q '^# rsi: ratchet' "$other" && continue
    echo "| == regression: $(basename "$other") must still pass"
    if ! run_scenario "$work/candidate" "$other"; then
        verdict rejected "regression: $(basename "$other") fails on the candidate"
        exit 0
    fi
done
verdict accepted
