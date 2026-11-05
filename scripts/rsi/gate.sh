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
# - With a task scenario: it must fail on the base (exit exactly 1) and pass
#   on the candidate.
# - Every other scenario in the directory must pass on the candidate, except
#   ratchet scenarios (a line `# rsi: ratchet`), which compare against the
#   base and so fail on any tree that did not improve, and objective
#   scenarios (`<digits>.sh`) the base fails too: an objective nobody has
#   solved yet is not a regression, and must not block unrelated work.
#   `check-*` scenarios are invariants and always must pass.
#
# Exit-code contract for objective scenarios and the task scenario: 0 =
# pass, 1 = an assertion failed, anything else = a harness error (a missing
# tool, an unreachable database, a timeout kill). Each is judged on the base
# BEFORE any candidate code runs, since both share the test server and a
# candidate could otherwise sabotage the base's run: 0 = solved, 1 (twice,
# from two runs that agree) = unsolved, anything else aborts the gate (exit
# 4, no verdict: the integrator records an abort, never a skip). Results are
# cached per (base commit, sha256 of the scenario, of the build hook, of the
# controller's scripts/blackbox helpers, of this file) in
# ${XDG_CACHE_HOME:-~/.cache}/spawn/gate-base, written outside the sandbox
# and hidden from it (a tmpfs over it, and the gate refuses to run when the
# directory lies under anything the sandbox binds back); the base moves only
# on landings, so most runs build and run nothing on the base. The base is
# built only on a cache miss.
#
# The cache's limit is the same-uid limit: candidate code in the sandbox
# can neither see nor write it, but any process outside the sandbox that
# runs as this uid can (on a host without containers, every agent). One that
# writes "unsolved" for an objective under the current key lets a candidate
# that breaks that objective land. Closing that needs the gate under a uid
# agents do not have.
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
# relay) is reachable. The checks reach the test database through one Unix
# socket: its own socket file when SPAWN_TEST_DATABASE_URL names a socket
# directory, or, for a TCP URL (127.0.0.1, localhost, [::1], a name), a
# socket the gate forwards to that host and port; inside, the URL names
# that socket. The environment is cleared down to the list below. Cargo
# gets a private CARGO_HOME whose registry and git caches are the real
# ones, read-only, and runs offline: a candidate cannot fetch a new crate or
# rewrite a cached one.
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
#   SPAWN_TEST_DATABASE_URL    the throwaway database the checks may use: a
#                              socket URL (postgres://u:p@%2Fdir/db, or
#                              ?host=/dir) or a TCP one, read by
#                              scripts/pg-url.sh
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
# Stage timing in the format every suite shares (scripts/stage.sh): one
# `| stage gate <result> <ms>ms <name>` line as each section ends, and a
# total with the verdict.
STAGE_PREFIX="| "
# shellcheck source=scripts/stage.sh
. "$(dirname "$0")/../stage.sh"
gate_started=$(stage_now)
# section <name>: a gate stage starts (and the one before it ends as ok).
section() {
    stage_open gate "$1"
    echo "| == $1"
}
trap 'stage_close aborted; rm -rf "$work"' EXIT
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

# verdict <accepted|rejected> [reason]: the verdict names the commits it
# judged, resolved, so a reader can check what it is about (deploy.sh does).
verdict() {
    # The open section passed unless the verdict is a rejection of it.
    if [ "$1" = accepted ]; then stage_close ok; else stage_close "$1"; fi
    stage_emit gate "$1" "$(($(stage_now) - gate_started))" total
    json="{\"verdict\":\"$1\""
    [ -n "${2-}" ] && json="$json,\"reason\":\"$(json_string "$2")\""
    [ -n "$metric" ] && json="$json,\"metric\":\"$(json_string "$metric")\""
    json="$json,\"base\":\"$base_commit\",\"candidate\":\"$candidate_commit\"}"
    if [ -n "${SPAWN_GATE_VERDICT-}" ]; then
        printf '%s\n' "$json" >"$SPAWN_GATE_VERDICT"
    fi
    echo "$json"
}

# What the verdict is about: both commits, resolved once.
base_commit=$(git --git-dir="$repo" rev-parse --verify --quiet "$baseline^{commit}") || {
    echo "gate: no commit $baseline in $repo" >&2
    exit 3
}
candidate_commit=$(git --git-dir="$repo" rev-parse --verify --quiet "$candidate^{commit}") || {
    echo "gate: no commit $candidate in $repo" >&2
    exit 3
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
# The base's objective results (see "The base's result" below): written
# here, outside the sandbox, which never sees this directory.
base_cache=${XDG_CACHE_HOME:-$HOME/.cache}/spawn/gate-base
mkdir -p "$base_cache"
chmod 700 "$base_cache"
base_cache=$(cd "$base_cache" && pwd -P)
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

# The base cache must stay hidden: the sandbox puts a tmpfs over it, and a
# bind of a directory above it, made after that tmpfs, would show it again.
# Those binds: the scenarios, the task scenario, the allow-listed $HOME
# paths and the test database's socket directory (below). The tree under
# test and the base tree are fresh directories under $work.
cache_exposed() {
    for exposed in "$@"; do
        [ -n "$exposed" ] && [ -e "$exposed" ] || continue
        exposed=$(cd "$(dirname "$exposed")" && pwd -P)/$(basename "$exposed")
        case "$base_cache/" in "$exposed"/*)
            echo "gate: the base cache $base_cache lies under $exposed, which the sandbox shows" >&2
            return 0
            ;;
        esac
    done
    return 1
}

# The test database, reached over one Unix socket file: the sandbox has no
# network but its own loopback. The URL is read as libpq reads it
# (scripts/pg-url.sh, percent-decoded):
# - a socket directory (postgres://u:p@%2Fdir:5432/db, or ?host=/dir): the
#   one file <dir>/.s.PGSQL.<port> is bound in, not the directory, so a
#   neighbour in it (another cluster's socket) stays out;
# - a TCP host (127.0.0.1, localhost, [::1], any name): the gate forwards a
#   socket of its own, $work/test-db/.s.PGSQL.<port>, to that host and port,
#   outside the sandbox (python3), and the sandbox reaches that server and
#   nothing else of the network. The forwarded connection is plain: a
#   server that requires TLS (hostssl) refuses it.
# Inside, SPAWN_TEST_DATABASE_URL names that socket
# (postgres://u:p@%2Fdir:<port>/db?<other parameters>), whatever form the
# owner gave. An unreadable URL or a missing socket is an infrastructure
# error (exit 3), not a verdict.
# shellcheck source=scripts/pg-url.sh
. "$(dirname "$0")/../pg-url.sh"
db_socket=""
db_forward=""
sandbox_db_url=""
if [ -n "${SPAWN_TEST_DATABASE_URL-}" ]; then
    if ! pg_url_parse "$SPAWN_TEST_DATABASE_URL"; then
        echo "gate: SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL" >&2
        exit 3
    fi
    case "$pg_url_host" in
    /*)
        db_socket=$pg_url_host/.s.PGSQL.$pg_url_port
        if [ ! -S "$db_socket" ]; then
            echo "gate: no test database socket at $db_socket" >&2
            exit 3
        fi
        db_socket=$(cd "$pg_url_host" && pwd -P)/.s.PGSQL.$pg_url_port
        ;;
    "")
        echo "gate: SPAWN_TEST_DATABASE_URL names no host; name the socket directory or the address" >&2
        exit 3
        ;;
    *)
        command -v python3 >/dev/null 2>&1 || {
            echo "gate: a TCP test database URL needs python3 to forward it into the sandbox" >&2
            exit 3
        }
        mkdir -m 700 "$work/test-db"
        db_socket=$work/test-db/.s.PGSQL.$pg_url_port
        db_forward="$pg_url_host $pg_url_port"
        ;;
    esac
    sandbox_db_url="$pg_url_scheme://${pg_url_userinfo:+$pg_url_userinfo@}$(pg_url_encode_host \
        "$(dirname "$db_socket")"):$pg_url_port/$(pg_url_encode_component "$pg_url_database")"
    [ -z "$pg_url_options" ] || sandbox_db_url="$sandbox_db_url?$pg_url_options"
fi

# The forwarder of a TCP test database: a Unix socket in $work/test-db that
# relays each connection to the test server's host and port. It dies with
# the gate (the EXIT trap, and the parent-death signal if the gate is
# killed).
db_forwarder=""
if [ -n "$db_forward" ]; then
    cat >"$work/test-db-forward.py" <<'EOF'
import ctypes, os, socket, sys, threading
ctypes.CDLL(None).prctl(1, 15)  # PR_SET_PDEATHSIG: SIGTERM when the gate dies
path, host, port = sys.argv[1], sys.argv[2], int(sys.argv[3])
listener = socket.socket(socket.AF_UNIX)
listener.bind(path)
listener.listen(64)

def pump(source, sink):
    try:
        while True:
            data = source.recv(65536)
            if not data:
                break
            sink.sendall(data)
    except OSError:
        pass
    for end in (source, sink):
        try:
            end.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

def relay(client):
    try:
        upstream = socket.create_connection((host, port), 10)
    except OSError:
        client.close()
        return
    upstream.settimeout(None)
    back = threading.Thread(target=pump, args=(upstream, client), daemon=True)
    back.start()
    pump(client, upstream)
    back.join()
    client.close()
    upstream.close()

while True:
    client, _ = listener.accept()
    threading.Thread(target=relay, args=(client,), daemon=True).start()
EOF
    # shellcheck disable=SC2086 # host and port
    python3 "$work/test-db-forward.py" "$db_socket" $db_forward </dev/null >/dev/null 2>"$work/test-db-forward.log" &
    db_forwarder=$!
    trap 'stage_close aborted; kill "$db_forwarder" 2>/dev/null; rm -rf "$work"' EXIT
    waited=0
    while [ ! -S "$db_socket" ]; do
        if ! kill -0 "$db_forwarder" 2>/dev/null || [ "$waited" -ge 100 ]; then
            echo "gate: the test database forwarder did not start:" >&2
            cat "$work/test-db-forward.log" >&2
            exit 3
        fi
        sleep 0.1
        waited=$((waited + 1))
    done
fi

old_ifs=$IFS
IFS='
'
# shellcheck disable=SC2086 # one path per line
if cache_exposed "$scenarios" "$scenario" "$db_socket" $home_binds; then
    IFS=$old_ifs
    exit 3
fi
IFS=$old_ifs

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
    set -- "$@" --tmpfs "$work" --tmpfs "$base_cache"
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
    for name in LANG RUSTUP_HOME; do
        eval "value=\${$name-}"
        [ -n "$value" ] && set -- "$@" --setenv "$name" "$value"
    done
    [ -n "$sandbox_db_url" ] && set -- "$@" --setenv SPAWN_TEST_DATABASE_URL "$sandbox_db_url"
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

# The base's result for each objective scenario and for the task scenario,
# decided before any candidate code runs: the base shares the test server
# with the candidate, which could otherwise sabotage the base's run and turn
# a solved objective into "unsolved". Exit 0 = solved, 1 = unsolved; any
# other exit is a harness error and aborts the gate (no verdict). An
# unsolved result needs two runs that agree. Results are cached per (base
# commit, scenario, build hook, gate) in $base_cache, which the sandbox
# cannot see, so a base that did not move costs nothing.
sha() { sha256sum "$1" | cut -d' ' -f1; }
gate_sha=$(sha "$0")
build_sha=$([ -f "$scenarios/build" ] && sha "$scenarios/build" || echo none)
# What scenarios source from the controller: a changed helper is a changed
# scenario.
if [ -d "$controller/scripts/blackbox" ]; then
    helpers_sha=$(cd "$controller/scripts/blackbox" && find . -type f | LC_ALL=C sort |
        while IFS= read -r helper; do printf '%s %s\n' "$(sha "$helper")" "$helper"; done |
        sha256sum | cut -d' ' -f1)
else
    helpers_sha=none
fi
# The stage helper is sourced by the black-box runner too, the URL parser
# by the scenarios (and by this gate, whose own copy is keyed as well).
for helper in "$controller/scripts/stage.sh" "$controller/scripts/pg-url.sh" \
    "$(dirname "$0")/../pg-url.sh"; do
    [ -f "$helper" ] || continue
    helpers_sha=$(printf '%s %s\n' "$helpers_sha" "$(sha "$helper")" | sha256sum | cut -d' ' -f1)
done
: >"$work/base-results"
for objective in "$scenarios"/*.sh ${scenario:+"$scenario"}; do
    [ -f "$objective" ] || continue
    if [ "$objective" != "$scenario" ]; then
        case "$(basename "$objective")" in *[!0-9].sh | .sh) continue ;; esac
        grep -q '^# rsi: ratchet' "$objective" && continue
    fi
    grep -qxF "$objective" "$work/base-results.seen" 2>/dev/null && continue
    printf '%s\n' "$objective" >>"$work/base-results.seen"
    objective_name=$(basename "$objective")
    key=$(printf '%s %s %s %s %s\n' "$base_commit" "$(sha "$objective")" "$build_sha" \
        "$helpers_sha" "$gate_sha" | sha256sum | cut -d' ' -f1)
    if [ -f "$base_cache/$key" ]; then
        result=$(cat "$base_cache/$key")
        echo "| == base: $objective_name: $result (cached)"
    else
        if [ -z "${base_built-}" ]; then
            section "base: build"
            if ! build "$work/baseline"; then
                verdict rejected "base does not build"
                exit 0
            fi
            base_built=1
        fi
        section "base: $objective_name"
        first=0
        run_scenario "$work/baseline" "$objective" || first=$?
        result=""
        case "$first" in
        0) result=solved ;;
        1)
            stage_close "exit 1"
            section "base: $objective_name again (unsolved needs two runs that agree)"
            again=0
            run_scenario "$work/baseline" "$objective" || again=$?
            [ "$again" -eq 1 ] && result=unsolved
            first="1, then $again"
            ;;
        esac
        if [ -z "$result" ]; then
            echo "| $objective_name exited $first on the base: a harness error, not a verdict; the gate aborts"
            stage_close aborted
            exit 4
        fi
        printf '%s\n' "$result" >"$base_cache/$key.$$"
        mv "$base_cache/$key.$$" "$base_cache/$key"
        echo "| == base: $objective_name: $result"
        stage_close "$result"
    fi
    printf '%s|%s\n' "$result" "$objective" >>"$work/base-results"
done
# base_result <scenario>: solved, unsolved, or none (not judged on the base).
base_result() {
    awk -F'|' -v want="$1" '
        $2 == want { print $1; found = 1; exit }
        END { if (!found) print "none" }' "$work/base-results"
}

if [ -n "$scenario" ] && [ "$(base_result "$scenario")" = solved ]; then
    verdict rejected "scenario already passes on the base"
    exit 0
fi

section "candidate: build"
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
    section "tests: what cargo runs, base and candidate"
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
    section "candidate: $(basename "$scenario") must pass"
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
    section "regression: $(basename "$other") must still pass"
    if run_scenario "$work/candidate" "$other"; then
        continue
    fi
    # An objective scenario is a regression only if the base solved it, as
    # judged before any candidate code ran.
    if [ "$(base_result "$other")" = unsolved ]; then
        echo "| $(basename "$other") fails on the base too: an unsolved objective, not a regression"
        stage_close unsolved
        continue
    fi
    verdict rejected "regression: $(basename "$other") fails on the candidate"
    exit 0
done
verdict accepted
