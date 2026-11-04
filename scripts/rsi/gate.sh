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
# path, the base tree is read-only, and the user's runtime directory, the
# docker socket and the host's TMPDIR are hidden. $HOME is an empty tmpfs
# with an allow-list bound back read-only (see `home_binds` below): the
# Rust toolchain, the install root of node/npm/npx when it lives there,
# and the controller's `scripts` and `web/node_modules` (the controller is
# the directory two levels above the scenarios). The network namespace is
# new: loopback only, so no host service (the live database, serve, the Jev
# relay) is reachable. The checks reach the test database through its Unix
# socket: when SPAWN_TEST_DATABASE_URL's host is a percent-encoded path, that
# directory is bound in; a TCP test URL does not work inside. The
# environment is cleared down to the list below. Cargo gets a private
# CARGO_HOME whose registry and git caches are the real ones, read-only, and
# runs offline: a candidate cannot fetch a new crate or rewrite a cached one.
# Git reads the gate's own config (an identity, no signing), not the
# owner's.
#
# The candidate's own tests and lints judge it too (check-tests,
# check-clippy), so it may not weaken them:
# - Tests: the number of tests cargo runs may not fall from base to
#   candidate. After the candidate's build, inside the sandbox, each tree
#   runs `cargo test --workspace -- --list --format terse`, and the same
#   with `--ignored`; runnable = listed minus ignored. A test deleted,
#   ignored, commented out, in a file no `mod` includes, or behind
#   `#[cfg(any())]` is not listed. The listing comes from the trees' own
#   test binaries, so code that forges libtest's output at start-up could
#   inflate it: this stops weakening, not a forger.
# - Lint configuration (text, before anything runs): a change to a
#   `[lints]` / `[workspace.lints]` table (or `lints.` keys) in any
#   Cargo.toml, any change to clippy.toml or .clippy.toml, a new crate-level
#   `#![allow(..)]` / `#![expect(..)]` (or one in `cfg_attr`), or a removed
#   `#![warn|deny|forbid(..)]` is refused. An item-level `#[allow]` is a
#   reviewer's job.
#
# Environment (the integrator passes these and nothing else, gate.rs):
#   PATH, HOME, LANG, TMPDIR   as usual
#   CARGO_HOME, RUSTUP_HOME    when the toolchain is not under $HOME
#   SPAWN_TEST_DATABASE_URL    the throwaway database the checks may use, as
#                              a socket URL (postgres://u:p@%2Fdir/db)
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
# Sandboxed git reads this, never the owner's ~/.gitconfig.
cat >"$work/gitconfig" <<'EOF'
[user]
	name = spawn-gate
	email = gate@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
[tag]
	gpgsign = false
EOF

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

cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
# The controller's checkout, as the checks locate it (two levels above the
# scenarios directory).
controller=$(cd "$scenarios/../.." && pwd)

# $HOME in the sandbox is an empty tmpfs. Bound back read-only, one path per
# line: the Rust toolchain (rustup's proxies and toolchains), the install
# root of each tool in `home_tools` that lives under $HOME (with a symlink
# where PATH found it), and the controller's `scripts` and
# `web/node_modules`, which the checks use. Nothing else of $HOME is
# visible: not ~/.gitconfig, not ~/.claude.json.backup, not shell history.
home_tools="node npm npx"
home_binds=""
home_links=""
under_home() {
    case "$1" in "$HOME"/*) return 0 ;; esac
    return 1
}
bind_home() {
    [ -e "$1" ] || return 0
    home_binds="$home_binds$1
"
}
bound() {
    old_ifs=$IFS
    IFS='
'
    for bound_root in $home_binds; do
        case "$1" in "$bound_root" | "$bound_root"/*)
            IFS=$old_ifs
            return 0
            ;;
        esac
    done
    IFS=$old_ifs
    return 1
}
under_home "$cargo_home/bin" && bind_home "$cargo_home/bin"
under_home "$rustup_home" && bind_home "$rustup_home"
for tool in $home_tools; do
    found=$(command -v "$tool" 2>/dev/null) || continue
    real=$(readlink -f "$found")
    under_home "$real" || continue
    root=$(dirname "$real")
    [ "$(basename "$root")" = bin ] && root=$(dirname "$root")
    # A plain file in ~/bin or ~/.local/bin: that file, not the directory
    # above it.
    case "$root" in "$HOME" | "$HOME/.local") root=$real ;; esac
    bound "$real" || bind_home "$root"
    if [ "$found" != "$real" ] && under_home "$found" && ! bound "$found"; then
        home_links="$home_links$real|$found
"
    fi
done
for path in "$controller/scripts" "$controller/web/node_modules"; do
    under_home "$path" && bind_home "$path"
done

# The test database, reached over its Unix socket: the sandbox has no
# network but its own loopback. A URL whose host is a percent-encoded path
# (postgres://user:pw@%2Fpath%2Fto%2Fdir/db) names the socket directory.
db_socket=""
case "${SPAWN_TEST_DATABASE_URL-}" in
*://*)
    db_host=${SPAWN_TEST_DATABASE_URL#*://}
    db_host=${db_host##*@}
    db_host=${db_host%%/*}
    db_host=${db_host%%\?*}
    case "$db_host" in
    %2[Ff]*)
        db_host=${db_host%:[0-9]*}
        db_socket=$(printf '%s' "$db_host" | sed 's/%2[Ff]/\//g')
        case "$db_socket" in *%*) db_socket="" ;; esac
        [ -d "$db_socket" ] || db_socket=""
        ;;
    esac
    ;;
esac

# sandbox <tree> <command...>: run a command inside the tree, in bwrap.
sandbox() {
    tree=$1
    shift
    count=$#
    set -- "$@" bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
        --unshare-pid --unshare-net --die-with-parent --new-session
    runtime=/run/user/$(id -u)
    [ -d "$runtime" ] && set -- "$@" --tmpfs "$runtime"
    for socket in /run/docker.sock /var/run/docker.sock; do
        [ -S "$socket" ] && set -- "$@" --ro-bind /dev/null "$socket"
    done
    for name in credentials credentials.toml; do
        [ -e "$cargo_home/$name" ] && set -- "$@" --ro-bind /dev/null "$cargo_home/$name"
    done
    # The host's TMPDIR when it is not /tmp, the gate's own files and the
    # integrator's verdict directory, wherever TMPDIR put them.
    case "${TMPDIR:-/tmp}" in
    / | /tmp | /tmp/) ;;
    *) [ -d "$TMPDIR" ] && set -- "$@" --tmpfs "$TMPDIR" ;;
    esac
    set -- "$@" --tmpfs "$work"
    if [ -n "${SPAWN_GATE_VERDICT-}" ]; then
        set -- "$@" --tmpfs "$(dirname "$SPAWN_GATE_VERDICT")"
    fi
    set -- "$@" --tmpfs "$HOME"
    old_ifs=$IFS
    IFS='
'
    for path in $home_binds; do
        set -- "$@" --ro-bind "$path" "$path"
    done
    for link in $home_links; do
        set -- "$@" --symlink "${link%%|*}" "${link#*|}"
    done
    IFS=$old_ifs
    set -- "$@" --ro-bind "$scenarios" "$scenarios"
    [ -n "$scenario" ] && set -- "$@" --ro-bind "$scenario" "$scenario"
    [ -n "$db_socket" ] && set -- "$@" --ro-bind "$db_socket" "$db_socket"
    if [ "$tree" != "$work/baseline" ] && [ -d "$work/baseline" ]; then
        set -- "$@" --ro-bind "$work/baseline" "$work/baseline"
    fi
    set -- "$@" --bind "$tree" "$tree"
    # Git: the gate's config, not the owner's (signing keys, credential
    # helpers).
    set -- "$@" --ro-bind "$work/gitconfig" /tmp/gitconfig
    # Cargo: a private home over the real caches, read-only, offline.
    set -- "$@" --dir /tmp/cargo-home
    for name in registry git config config.toml; do
        [ -e "$cargo_home/$name" ] && set -- "$@" --ro-bind "$cargo_home/$name" "/tmp/cargo-home/$name"
    done
    set -- "$@" --clearenv --setenv HOME "$HOME" --setenv PATH "$PATH" --setenv TMPDIR /tmp \
        --setenv CARGO_HOME /tmp/cargo-home --setenv CARGO_NET_OFFLINE true \
        --setenv npm_config_cache /tmp/npm-cache \
        --setenv GIT_CONFIG_GLOBAL /tmp/gitconfig
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

# The candidate's own lints judge it (check-clippy): it may not relax them
# (the rules are in the header). Text only, on the exported trees, before
# anything is built. The test ratchet runs after the build, below.

# lint_config <tree>: every [lints]/[workspace.lints] line (and `lints.`
# key) of every Cargo.toml, and every clippy.toml, one fact per line.
lint_config() {
    (
        cd "$1" || exit 1
        find . -name Cargo.toml -type f | sort | while IFS= read -r manifest; do
            awk -v file="$manifest" '
                { sub(/#.*/, "") }
                /^[[:space:]]*\[/ { inlints = ($0 ~ /^[[:space:]]*\[(workspace\.)?lints([].])/) }
                inlints || /^[[:space:]]*(workspace\.)?lints[[:space:]]*[.=]/ {
                    gsub(/[[:space:]]+/, "")
                    if ($0 != "") print file ": " $0
                }' "$manifest"
        done
        find . \( -name clippy.toml -o -name .clippy.toml \) -type f | sort | while IFS= read -r file; do
            sed "s|^|$file: |" "$file"
        done
    )
}
if [ "$(lint_config "$work/baseline")" != "$(lint_config "$work/candidate")" ]; then
    echo "| candidate changes lint configuration ([lints] tables or clippy.toml)"
    verdict rejected "candidate changes lint configuration"
    exit 0
fi

# crate_lints <tree>: crate-level lint attributes, `file|attribute`, with
# whitespace removed and a multi-line attribute joined.
crate_lints() {
    (
        cd "$1" || exit 1
        find . -name '*.rs' -type f -exec awk '
            FNR == 1 { open = 0 }
            {
                line = $0
                if (!open && match(line, /#![[:space:]]*\[/)) { open = 1; text = ""; line = substr(line, RSTART) }
                if (open) {
                    text = text line
                    if (index(line, "]")) {
                        open = 0
                        gsub(/[[:space:]]+/, "", text)
                        text = substr(text, 1, index(text, "]"))
                        if (text ~ /^#!\[(allow|expect|warn|deny|forbid|cfg_attr)\(/) print FILENAME "|" text
                    }
                }
            }' {} + | sort -u
    )
}
crate_lints "$work/baseline" >"$work/lints.base"
crate_lints "$work/candidate" >"$work/lints.candidate"
relaxed=$( {
    comm -13 "$work/lints.base" "$work/lints.candidate" | grep -E '\|#!\[(allow|expect|cfg_attr)\(' || true
    comm -23 "$work/lints.base" "$work/lints.candidate" | grep -E '\|#!\[(warn|deny|forbid|cfg_attr)\(' || true
})
if [ -n "$relaxed" ]; then
    echo "| candidate relaxes crate-level lints: $(echo "$relaxed" | sed 's|^\./||' | tr '\n' ' ')"
    verdict rejected "candidate relaxes crate-level lints"
    exit 0
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

# runnable_tests <writable dir> <tree> <target dir>: how many tests cargo
# runs in the tree, from inside the sandbox: libtest's `<name>: test` lines
# of `--list`, minus those of `--list --ignored`.
runnable_tests() {
    list_dir=$1
    list_tree=$2
    list_target=$3
    for list_mode in all ignored; do
        set -- --list --format terse
        [ "$list_mode" = ignored ] && set -- "$@" --ignored
        sandbox "$list_dir" env CARGO_TARGET_DIR="$list_target" cargo test --quiet --workspace \
            --manifest-path "$list_tree/Cargo.toml" -- "$@" \
            >"$work/tests.$list_mode" 2>>"$work/tests.log" || return 1
    done
    echo $(($(grep -c ': test$' "$work/tests.all") - $(grep -c ': test$' "$work/tests.ignored")))
}

# The candidate may not run fewer tests than the base. Counted from what
# cargo runs, so a test deleted, ignored, commented out, left in a file no
# `mod` includes, or behind `#[cfg(any())]` is gone. The base tree stays
# read-only: its cargo writes to a target directory of its own. The
# candidate's target directory is the one check-tests reuses.
if [ -f "$work/baseline/Cargo.toml" ] && [ -f "$work/candidate/Cargo.toml" ]; then
    echo "| == tests: what cargo runs, base and candidate"
    listing_started=$(date +%s)
    mkdir -p "$work/base-target"
    : >"$work/tests.log"
    if ! base_tests=$(runnable_tests "$work/base-target" "$work/baseline" "$work/base-target"); then
        sed 's/^/| tests: /' "$work/tests.log" | tail -n 30
        verdict rejected "base tests do not build"
        exit 0
    fi
    : >"$work/tests.log"
    if ! candidate_tests=$(runnable_tests "$work/candidate" "$work/candidate" "$work/candidate/target"); then
        sed 's/^/| tests: /' "$work/tests.log" | tail -n 30
        verdict rejected "candidate tests do not build"
        exit 0
    fi
    echo "| tests cargo runs: $base_tests on the base, $candidate_tests on the candidate" \
        "(listed in $(($(date +%s) - listing_started)) s)"
    if [ "$candidate_tests" -lt "$base_tests" ]; then
        verdict rejected "candidate runs fewer tests ($base_tests -> $candidate_tests)"
        exit 0
    fi
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
