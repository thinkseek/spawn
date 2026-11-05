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
#   (one prefix per line, `#` comments, `!<prefix>` for an exception): the
#   controller's tasks, scenarios, gate and black-box tests, and the UI's
#   build configuration (all of `web/` but `web/src/` and `web/index.html`). Paths are compared as git stores them (no
#   rename detection, no quoting); a path with a newline is refused, and so
#   is every candidate when the list is missing (an empty list protects
#   nothing on purpose).
# - It may not change anything under a `node_modules/` or `target/`
#   directory.
# - It may not add build-time code: a changed `build.rs` or `.cargo/`, a
#   `build =` key in any Cargo.toml of the tree (cargo would run the file it
#   names as a build script), a workspace member with a build script or a
#   proc-macro target, or a local crate outside the workspace with either
#   that is new or whose directory the diff touches.
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
# cached in ${XDG_CACHE_HOME:-~/.cache}/spawn/gate-base, keyed by everything
# that decides them (the base commit, the scenario's blob, this file's, the
# toolchain, and the files the scenario names: "The base cache" below),
# written outside the sandbox and hidden from it (a tmpfs over it, and the
# gate refuses to run when the directory lies under anything the sandbox
# binds back); the base moves only on landings, so most runs build and run
# nothing on the base. The base is built only on a cache miss.
#
# The cache's limit is the same-uid limit: candidate code in the sandbox
# can neither see nor write it, but any process outside the sandbox that
# runs as this uid can (on a host without containers, every agent). One that
# writes "unsolved" for an objective under the current key lets a candidate
# that breaks that objective land. Closing that needs the gate under a uid
# agents do not have.
#
# A tree that is not Spawn names its owner-installed inputs (dependencies the
# sandbox cannot download) in `<scenarios-dir>/bind`: absolute directories
# under $HOME, bound read-only at the same path.
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
# path, the base tree is read-only, and /run (the user's runtime directory,
# the docker socket, the system bus), /var/tmp and the host's TMPDIR are
# empty: no Unix socket of the host is reachable but the test database's
# (see "Unix sockets" at `sandbox` below). $HOME is an empty tmpfs
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
#   `#[cfg(any())]` is not listed. Every runnable test name of the base must
#   be runnable on the candidate too: a renamed test, or one moved to
#   another module path, is refused even when the count holds. A test the
#   diff adds or edits may not have an empty body (`{}`, or only comments,
#   `;` and `()`). The listing comes from the trees' own test binaries, so
#   code that forges libtest's output at start-up could inflate it, and a
#   body that asserts nothing real (`assert!(true)`) is not empty text: this
#   stops weakening, not a forger.
#   A test the owner retired with its feature is named in
#   `<scenarios-dir>/retired-tests` and may go.
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

. "$(dirname "$0")/../disk.sh"
gate_room || exit 3
work=$(gate_scratch)
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
# The owner's own read-only inputs for a tree that is not Spawn (a project's
# installed dependencies, which the sandbox cannot download): one absolute
# directory per line in `<scenarios-dir>/bind`, `#` comments. Only under
# $HOME (anything else outside the emptied directories is visible already),
# and a missing one stops the gate (exit 3) rather than judging without it.
if [ -f "$scenarios/bind" ]; then
    while IFS= read -r path || [ -n "$path" ]; do
        case "$path" in '' | '#'*) continue ;; esac
        case "$path" in
        /*) ;;
        *)
            echo "gate: $scenarios/bind: not an absolute path: $path" >&2
            exit 3
            ;;
        esac
        if [ ! -d "$path" ]; then
            echo "gate: $scenarios/bind: no directory $path" >&2
            exit 3
        fi
        under_home "$path" && ! bound "$path" && bind_home "$path"
    done <"$scenarios/bind"
fi

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
# network but its own loopback, and sees no other socket of the host (see
# "Unix sockets" below). The URL is read as libpq reads it
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

# PATH directories outside $HOME under the directories the sandbox empties
# (a toolchain in /run/current-system): bound back read-only, one per line.
hidden_path_dirs=""
old_ifs=$IFS
IFS=:
for path in $PATH; do
    [ -d "$path" ] || continue
    # $HOME has its own allow-list (home_binds).
    under_home "$path" && continue
    case "$path" in
    /tmp/* | /run/* | /var/run/* | /var/tmp/*) ;;
    *)
        case "${TMPDIR:-/tmp}" in / | /tmp | /tmp/) continue ;; esac
        case "$path" in "${TMPDIR%/}"/*) ;; *) continue ;; esac
        ;;
    esac
    hidden_path_dirs="$hidden_path_dirs$path
"
done
IFS=$old_ifs
socket_masks=""

# sandbox <tree> <command...>: run a command inside the tree, in bwrap.
#
# Unix sockets. A read-only bind does not stop connect() on a socket, so
# every directory where the host keeps them is an empty tmpfs inside: /tmp,
# /run (with /var/run, the user's runtime directory /run/user/<uid>, the
# docker, podman and containerd sockets, the system bus, /run/postgresql,
# ssh and gpg agents), /var/tmp, the host's TMPDIR and $HOME. /dev is a
# fresh one (no /dev/log), and the network namespace is new, which also
# hides the host's abstract sockets (@/tmp/.X11-unix/X0 and the like). Bound
# back: the PATH directories under those, the allow-listed $HOME paths, and
# the test database's one socket file. Whatever other socket the host still
# shows is found by a scan inside the sandbox before any candidate code
# runs (`socket_masks` below), and hidden too.
sandbox() {
    tree=$1
    shift
    count=$#
    set -- "$@" bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --tmpfs /run \
        --unshare-pid --unshare-net --die-with-parent --new-session
    if [ -d /var/run ] && [ ! -L /var/run ]; then set -- "$@" --tmpfs /var/run; fi
    if [ -d /var/tmp ] && [ ! -L /var/tmp ]; then set -- "$@" --tmpfs /var/tmp; fi
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
    for path in $hidden_path_dirs; do
        set -- "$@" --ro-bind "$path" "$path"
    done
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
    # Last, so no bind above shows them again: the sockets the scan found.
    # One whose directory or file is gone since (a host process's temporary
    # directory) is skipped: bwrap cannot mount over what is not there, and
    # failing every later command would misjudge the candidate.
    old_ifs=$IFS
    IFS='
'
    for mask in $socket_masks; do
        case "$mask" in
        "directory "*)
            [ -d "${mask#directory }" ] && set -- "$@" --tmpfs "${mask#directory }"
            ;;
        "file "*)
            [ -e "${mask#file }" ] && set -- "$@" --ro-bind /dev/null "${mask#file }"
            ;;
        esac
    done
    IFS=$old_ifs
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

# The socket scan: every socket the sandbox still shows that its uid may
# connect to (`find -writable`: connect() needs write permission, and a
# read-only mount does not take it away), other than the test database's,
# found inside the sandbox with the host's own find before any candidate
# code runs. Each is hidden from then on: its directory under an empty
# tmpfs when nothing the sandbox needs is in, above or below that directory
# (a socket in /data/tmp/chrome.X when TMPDIR did not name /data/tmp), else
# the socket file alone under /dev/null. The limit: a socket created after
# the scan, in a directory no tmpfs covers, is reachable.
mkdir "$work/socket-scan"
sandbox "$work/socket-scan" find / \( -path /proc -o -path /sys -o -path /dev \) -prune -o \
    -type s -writable -print >"$work/sockets" 2>/dev/null || true
# needed_by_sandbox <directory>: 0 when hiding the directory would hide
# something the sandbox uses (a system directory, PATH, a bound path), or
# the directory is one of those or above one.
sandbox_paths=$(printf '%s\n' /bin /etc /lib /lib32 /lib64 /libx32 /nix /gnu /opt /sbin /usr \
    "$work" "$scenarios" "$HOME" "$cargo_home" "$rustup_home" "$controller/scripts" \
    "$controller/web/node_modules" "$scenario" "$db_socket" "$PATH" | tr ':' '\n'
printf '%s%s' "$home_binds" "$hidden_path_dirs")
needed_by_sandbox() {
    [ "$1" = / ] && return 0
    printf '%s\n' "$sandbox_paths" | awk -v dir="${1%/}" '
        $0 == "" || $0 == "/" { next }
        { sub(/\/$/, "") }
        index(dir "/", $0 "/") == 1 || index($0 "/", dir "/") == 1 { found = 1; exit }
        END { exit !found }'
}
while IFS= read -r found_socket; do
    [ "$found_socket" = "$db_socket" ] && continue
    found_dir=$(dirname "$found_socket")
    if needed_by_sandbox "$found_dir"; then
        mask="file $found_socket"
    else
        mask="directory $found_dir"
    fi
    case "
$socket_masks" in *"
$mask
"*) continue ;; esac
    socket_masks="$socket_masks$mask
"
done <"$work/sockets"
[ -z "$socket_masks" ] ||
    echo "| sockets hidden from the sandbox: $(printf '%s' "$socket_masks" | grep -c .)"

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

# The changed paths, as they are: no rename detection (a move lists the old
# path too, so a protected file cannot be moved away), no quoting
# (core.quotePath would print "scripts/\303\251.sh", quotes included, and a
# quoted path matches no prefix), NUL-separated. A path holding a newline is
# refused: one line per path is what the checks below read.
git -c core.quotePath=false --git-dir="$repo" diff --no-renames --no-ext-diff --name-only -z \
    "$base_commit" "$candidate_commit" | tr '\n\000' '\001\n' >"$work/changed"
if grep -q "$(printf '\001')" "$work/changed"; then
    echo "| a changed path holds a newline: $(grep "$(printf '\001')" "$work/changed" | tr '\001' '?' | head -n 3 | tr '\n' ' ')"
    verdict rejected "a changed path holds a newline"
    exit 0
fi
changed=$(cat "$work/changed")

# Without a protected list nothing is protected, which is never meant: a
# controller that lost its list (a candidate that moved it away once) judges
# nothing. An owner who means "nothing protected" gives an empty list.
if [ ! -f "$scenarios/protected" ]; then
    verdict rejected "the scenarios directory has no protected list"
    exit 0
fi
# A line `!<prefix>` excepts paths under it from the prefixes above it, so
# the owner can protect a directory but its sources (`web/`, `!web/src/`).
touched=$(printf '%s\n' "$changed" | awk -v list="$scenarios/protected" '
    BEGIN { while ((getline line < list) > 0) if (line != "" && line !~ /^#/) {
        if (substr(line, 1, 1) == "!") allowed[++a] = substr(line, 2); else prefix[++n] = line } }
    { for (i = 1; i <= a; i++) if (index($0, allowed[i]) == 1) next
      for (i = 1; i <= n; i++) if (index($0, prefix[i]) == 1) { print; break } }')
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

# Nothing under a node_modules/ or target/ directory: both are gitignored,
# and git overwrites an ignored file with a committed one when it checks out
# or merges, so a committed web/node_modules/.bin/tsc or target/ file would
# replace the owner's build tools (deploy.sh runs them, and the sandbox
# binds the controller's web/node_modules).
planted=$(printf '%s\n' "$changed" | grep -E '(^|/)(node_modules|target)/' || true)
if [ -n "$planted" ]; then
    echo "| candidate commits build output or installed packages: $(echo "$planted" | head -n 5 | tr '\n' ' ')"
    verdict rejected "candidate changes a path under node_modules/ or target/"
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
    # A proc-macro crate of the workspace runs at compile time too, and the
    # deploy compiles outside any sandbox.
    if grep -q '"proc-macro"' "$work/metadata.json"; then
        echo "| a workspace member of the candidate is a proc-macro crate"
        verdict rejected "candidate adds build-time code (a proc-macro crate)"
        exit 0
    fi
    # A local crate outside the workspace (a path dependency the workspace
    # excludes, or does not include) is compiled too, and `--no-deps` does
    # not list it. The whole dependency graph, offline, in the sandbox: a
    # package with no source (a local path) and a proc-macro or build-script
    # target is refused when the base has none at that path, or when the
    # diff touches its directory. Registry crates are the lockfile's, which
    # a reviewer reads; they are not this check's.
    if ! command -v jq >/dev/null 2>&1; then
        echo "gate: jq is required to read cargo metadata" >&2
        exit 3
    fi
    # local_build_time <writable dir> <tree>: `<directory>\t<name>` of each
    # local package with a build-time target, the directory relative to the
    # tree.
    # The host's platform only: the graph of every platform names crates
    # (wasm's js-sys) this host never fetched, and offline cannot resolve
    # them; this host's build compiles only its own platform's.
    host_triple=$(cd "$work" && rustc -vV 2>/dev/null | sed -n 's/^host: //p')
    local_build_time() {
        sandbox "$1" cargo metadata --offline --format-version 1 --manifest-path "$2/Cargo.toml" \
            ${host_triple:+--filter-platform "$host_triple"} \
            >"$work/metadata-full.json" 2>>"$work/metadata.log" || return 1
        jq -r --arg tree "$2/" '
            .packages[] | select(.source == null)
            | select([.targets[].kind[]] | any(. == "proc-macro" or . == "custom-build"))
            | "\(.manifest_path | ltrimstr($tree) | sub("/?Cargo.toml$"; ""))\t\(.name)"' \
            "$work/metadata-full.json" | LC_ALL=C sort
    }
    : >"$work/build-time.base"
    if ! local_build_time "$work/candidate" "$work/candidate" >"$work/build-time.candidate"; then
        sed 's/^/| metadata: /' "$work/metadata.log" | tail -n 20
        verdict rejected "candidate's dependency graph does not resolve offline"
        exit 0
    fi
    # The base's: from its read-only tree, cargo writing nowhere but a
    # directory of its own. A base cargo cannot read counts as having none.
    if [ -s "$work/build-time.candidate" ] && [ -f "$work/baseline/Cargo.toml" ]; then
        mkdir -p "$work/base-metadata"
        local_build_time "$work/base-metadata" "$work/baseline" >"$work/build-time.base" ||
            : >"$work/build-time.base"
    fi
    new_build_time=$(LC_ALL=C comm -23 "$work/build-time.candidate" "$work/build-time.base" | cut -f2)
    # One the base had, at the same path, is new to the candidate when the
    # diff touches its directory.
    edited_build_time=$(LC_ALL=C comm -12 "$work/build-time.candidate" "$work/build-time.base" |
        while IFS='	' read -r crate_dir crate_name; do
            printf '%s\n' "$changed" | awk -v dir="$crate_dir/" 'dir == "/" || index($0, dir) == 1 { found = 1 } END { exit !found }' &&
                printf '%s\n' "$crate_name"
        done || true)
    new_build_time=$(printf '%s\n%s\n' "$new_build_time" "$edited_build_time" | grep . || true)
    if [ -n "$new_build_time" ]; then
        echo "| local crates with build-time code, new or changed: $(echo "$new_build_time" | tr '\n' ' ')"
        verdict rejected "candidate adds build-time code (a local proc-macro or build script outside the workspace: $(echo "$new_build_time" | tr '\n' ' ' | sed 's/ $//'))"
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

# A test function the diff adds or edits may not have an empty body: `{}`,
# or nothing but comments, `;` and `()`. Text only, on the candidate's
# files the diff touches, with comments and string literals blanked first
# (a `"#[test] fn x() {}"` string is not a test). A test is any function
# under an attribute whose last path segment is `test` (#[test],
# #[tokio::test(..)], #[sqlx::test]). What it cannot see: a body that does
# something and asserts nothing, `assert!(true)`, a test that returns
# before its assertions; the base's own empty tests, untouched.
#
# rust_code <file>: the file with comments and the contents of string and
# char literals replaced by spaces, line for line.
rust_code() {
    LC_ALL=C awk '
        {
            line = $0; out = ""; n = length(line); i = 1
            while (i <= n) {
                c = substr(line, i, 1)
                if (depth > 0) {
                    two = substr(line, i, 2)
                    if (two == "/*") { depth++; out = out "  "; i += 2 }
                    else if (two == "*/") { depth--; out = out "  "; i += 2 }
                    else { out = out " "; i++ }
                    continue
                }
                if (quote != "") {
                    if (raw < 0 && c == "\\") { out = out "  "; i += 2; continue }
                    if (c == "\"" && (raw < 0 || substr(line, i + 1, raw) == hashes)) {
                        out = out "\"" hashes; i += 1 + (raw < 0 ? 0 : raw); quote = ""; continue
                    }
                    out = out " "; i++
                    continue
                }
                two = substr(line, i, 2)
                if (two == "//") break
                if (two == "/*") { depth = 1; out = out "  "; i += 2; continue }
                # A raw string starts at `r` (or `br`) that ends no identifier.
                prev = i > 1 ? substr(line, i - 1, 1) : ""
                if (prev == "b") prev = i > 2 ? substr(line, i - 2, 1) : ""
                if (c == "r" && prev !~ /[A-Za-z0-9_]/ && match(substr(line, i), /^r#*"/)) {
                    raw = RLENGTH - 2; hashes = substr(line, i + 1, raw)
                    out = out substr(line, i, RLENGTH); i += RLENGTH; quote = "\""
                    continue
                }
                if (c == "\"") { raw = -1; hashes = ""; quote = "\""; out = out c; i++; continue }
                # A char literal (an escaped one, or one byte); else a lifetime.
                if (c == "'\''") {
                    if (substr(line, i + 1, 1) == "\\" && match(substr(line, i + 3), /'\''/)) {
                        out = out "'\'' '\''"; i += 3 + RSTART; continue
                    }
                    if (substr(line, i + 2, 1) == "'\''") { out = out "'\'' '\''"; i += 3; continue }
                }
                out = out c; i++
            }
            print out
        }' "$1"
}
# empty_tests <file> <touched ranges>: `<line> <name>` of each test with an
# empty body whose lines (attribute to closing brace) meet a touched range
# (`first-last`, space-separated).
empty_tests() {
    rust_code "$1" | LC_ALL=C awk -v ranges="$2" '
        { code = code $0 "\n" }
        END {
            count = split(ranges, range, " ")
            rest = code; offset = 0
            attribute = "#[[:space:]]*\\[[[:space:]]*([A-Za-z_][A-Za-z0-9_]*[[:space:]]*::[[:space:]]*)*test[[:space:]]*[](]"
            while (match(rest, attribute)) {
                start = offset + RSTART
                after = start + RLENGTH - 1
                tail = substr(code, after + 1)
                offset = after; rest = tail
                if (!match(tail, /(^|[^A-Za-z0-9_])fn[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/)) continue
                name = substr(tail, RSTART, RLENGTH); sub(/^.*fn[[:space:]]+/, "", name)
                fn_end = RSTART + RLENGTH - 1
                open = index(substr(tail, fn_end + 1), "{")
                if (!open) continue
                body = substr(tail, fn_end + open + 1)
                if (!match(body, /[^[:space:];()]/) || substr(body, RSTART, 1) != "}") continue
                first = lines(substr(code, 1, start))
                last = lines(substr(code, 1, after + fn_end + open + RSTART))
                for (r = 1; r <= count; r++) {
                    split(range[r], bound, "-")
                    if (bound[1] <= last && bound[2] >= first) { print first " " name; break }
                }
            }
        }
        function lines(text) { return gsub(/\n/, "", text) + 1 }'
}
# The candidate's added and changed lines per .rs file, from the diff (a
# deletion touches the lines on both sides of it).
git -c core.quotePath=false --git-dir="$repo" diff -U0 --no-renames --no-color --no-ext-diff --no-textconv --src-prefix=a/ --dst-prefix=b/ \
    "$base_commit" "$candidate_commit" -- '*.rs' | awk '
    /^\+\+\+ / { file = ($0 == "+++ /dev/null") ? "" : substr($0, 7); next }
    /^@@ / && file != "" {
        split($3, added, ",")
        first = substr(added[1], 2) + 0
        size = (2 in added) ? added[2] + 0 : 1
        last = size == 0 ? first + 1 : first + size - 1
        touched[file] = touched[file] " " first "-" last
    }
    END { for (file in touched) print file "\t" substr(touched[file], 2) }' >"$work/touched-rust"
empty=""
while IFS='	' read -r file ranges; do
    [ -f "$work/candidate/$file" ] || continue
    found=$(empty_tests "$work/candidate/$file" "$ranges" | sed "s|^|$file:|")
    [ -n "$found" ] && empty="$empty$found
"
done <"$work/touched-rust"
if [ -n "$empty" ]; then
    echo "| tests with an empty body the candidate adds or edits: $(printf '%s' "$empty" | tr '\n' ' ')"
    verdict rejected "candidate adds or edits a test with an empty body ($(printf '%s' "$empty" | head -n 1))"
    exit 0
fi

# The base's result for each objective scenario and for the task scenario,
# decided before any candidate code runs: the base shares the test server
# with the candidate, which could otherwise sabotage the base's run and turn
# a solved objective into "unsolved". Exit 0 = solved, 1 = unsolved; any
# other exit is a harness error and aborts the gate (no verdict). An
# unsolved result needs two runs that agree.
#
# The base cache. A result is cached in $base_cache (which the sandbox
# cannot see, let alone write), keyed by everything that decides it:
# - the base commit;
# - the scenario's blob (`git hash-object`);
# - the gate: this file's blob and that of the URL parser it sources (it
#   shapes the sandbox's database URL);
# - the scenarios directory's other files, but its scenarios and the lists
#   only the gate reads (`protected`, `retired-tests`): the build hook, the
#   `bind` list, helpers a scenario runs from beside it;
# - the files the scenario names on its code lines, and those they name in
#   turn: a path
#   `scripts/...` is the controller's (the scenarios source
#   `$controller/scripts/blackbox/assertions.sh` and
#   `$controller/scripts/pg-url.sh`), a directory is all of its files, a
#   `<name>.sh` is a sibling scenario; one that does not exist is keyed as
#   missing. A controller file no scenario names (the black-box runner,
#   the e2e) changes no objective's key;
# - the toolchain the base builds with: `rustc -vV`, `cargo -V` and
#   `node --version`, asked in the sandbox inside the base tree, whose
#   rust-toolchain.toml picks the toolchain.
# An entry is written only here, after a completed base run, and only
# "solved" or "unsolved"; an entry that cannot be read, or holds anything
# else, is a miss, judged again and rewritten. Candidate runs are never
# cached. The log says which results came from the cache.
blob() { git hash-object --no-filters -- "$1"; }
gate_key="gate $(blob "$0")"
[ -f "$(dirname "$0")/../pg-url.sh" ] && gate_key="$gate_key $(blob "$(dirname "$0")/../pg-url.sh")"
scenarios_key=$(cd "$scenarios" && find . -type f ! -path './protected' ! -path './retired-tests' |
    grep -vE '^\./[^/]*\.sh$' | LC_ALL=C sort |
    while IFS= read -r file; do printf '%s %s\n' "$(blob "$file")" "$file"; done |
    sha256sum | cut -d' ' -f1)
# named_by <file>: the paths a file names on its code lines, one per line
# (see above). Comments do not count: this file's own comments name
# scripts/blackbox, and a scenario that mentions the gate would otherwise
# key every black-box suite.
named_by() {
    sed -e 's/^[[:space:]]*#.*//' -e 's/[[:space:]]#.*//' "$1" 2>/dev/null >"$work/deps.code" || :
    grep -oE 'scripts/[A-Za-z0-9_./-]*[A-Za-z0-9_-]' "$work/deps.code" | sed "s|^|$controller/|" || true
    grep -oE '[A-Za-z0-9_-]+\.sh' "$work/deps.code" | while IFS= read -r name; do
        [ ! -f "$scenarios/$name" ] || printf '%s\n' "$scenarios/$name"
    done
}
# depends_on <scenario>: `<blob> <path>` (or `missing <path>`) of the
# scenario and everything it names, transitively, sorted.
depends_on() {
    printf '%s\n' "$1" >"$work/deps.files"
    while :; do
        while IFS= read -r file; do
            [ -f "$file" ] && named_by "$file"
        done <"$work/deps.files" | while IFS= read -r path; do
            if [ -d "$path" ]; then find "$path" -type f; else printf '%s\n' "$path"; fi
        done | cat "$work/deps.files" - | LC_ALL=C sort -u >"$work/deps.next"
        cmp -s "$work/deps.next" "$work/deps.files" && break
        mv "$work/deps.next" "$work/deps.files"
    done
    while IFS= read -r file; do
        if [ -f "$file" ]; then printf '%s %s\n' "$(blob "$file")" "$file"; else printf 'missing %s\n' "$file"; fi
    done <"$work/deps.files"
}
# The toolchain, asked once, when the first objective needs a key.
toolchain_key=""
toolchain() {
    [ -z "$toolchain_key" ] || return 0
    sandbox "$work/baseline" sh -c 'rustc -vV 2>&1; echo "rustc: $?"; cargo -V 2>&1; echo "cargo: $?"
        node --version 2>&1; echo "node: $?"' >"$work/toolchain" 2>&1 || true
    toolchain_key=$(sha256sum <"$work/toolchain" | cut -d' ' -f1)
    echo "| base cache: toolchain $(sed -n 's/^release: /rustc /p' "$work/toolchain" | head -n 1)" \
        "$(sed -n 's/^\(cargo [^ ]*\).*/\1/p' "$work/toolchain" | head -n 1)"
}
: >"$work/base-results"
from_cache=""
run_on_base=""
for objective in "$scenarios"/*.sh ${scenario:+"$scenario"}; do
    [ -f "$objective" ] || continue
    if [ "$objective" != "$scenario" ]; then
        case "$(basename "$objective")" in *[!0-9].sh | .sh) continue ;; esac
        grep -q '^# rsi: ratchet' "$objective" && continue
    fi
    grep -qxF "$objective" "$work/base-results.seen" 2>/dev/null && continue
    printf '%s\n' "$objective" >>"$work/base-results.seen"
    objective_name=$(basename "$objective")
    toolchain
    key=$( {
        printf 'base %s\nscenario %s\n%s\ntoolchain %s\nscenarios %s\n' "$base_commit" \
            "$(blob "$objective")" "$gate_key" "$toolchain_key" "$scenarios_key"
        depends_on "$objective"
    } | sha256sum | cut -d' ' -f1)
    entry=$base_cache/$key
    result=""
    if [ -e "$entry" ] || [ -L "$entry" ]; then
        result=$(cat -- "$entry" 2>/dev/null) || result=""
        case "$result" in
        solved | unsolved) ;;
        *)
            echo "| == base: $objective_name: its cache entry cannot be read; judged again"
            result=""
            ;;
        esac
    fi
    if [ -n "$result" ]; then
        echo "| == base: $objective_name: $result (cached)"
        from_cache="$from_cache $objective_name"
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
        # A completed run, and only one, writes its entry. One that cannot
        # be written costs the next gate a run, not this one its verdict.
        if [ -d "$entry" ] && [ ! -L "$entry" ]; then rm -rf -- "$entry"; fi
        if ! { printf '%s\n' "$result" >"$entry.$$" && mv -f -- "$entry.$$" "$entry"; } 2>/dev/null; then
            rm -f -- "$entry.$$"
            echo "| base cache: could not write the entry of $objective_name"
        fi
        echo "| == base: $objective_name: $result"
        run_on_base="$run_on_base $objective_name"
        stage_close "$result"
    fi
    printf '%s|%s\n' "$result" "$objective" >>"$work/base-results"
done
if [ -n "$from_cache$run_on_base" ]; then
    echo "| base results from the cache:${from_cache:- none}; run on the base:${run_on_base:- none}"
fi
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

# runnable_tests <writable dir> <tree> <target dir> <names file>: how many
# tests cargo runs in the tree, from inside the sandbox: libtest's
# `<name>: test` lines of `--list`, minus those of `--list --ignored`. Their
# names go to <names file>, sorted, one line per test (a name two test
# binaries share is there twice), a doctest's ` (line N)` dropped so that
# an edit above it does not rename it.
runnable_tests() {
    list_dir=$1
    list_tree=$2
    list_target=$3
    list_names=$4
    for list_mode in all ignored; do
        set -- --list --format terse
        [ "$list_mode" = ignored ] && set -- "$@" --ignored
        sandbox "$list_dir" env CARGO_TARGET_DIR="$list_target" cargo test --quiet --workspace \
            --manifest-path "$list_tree/Cargo.toml" -- "$@" \
            >"$work/tests.$list_mode" 2>>"$work/tests.log" || return 1
        sed -n 's/: test$//p' "$work/tests.$list_mode" | sed -E 's/ \(line [0-9]+\)$//' |
            LC_ALL=C sort >"$work/names.$list_mode"
    done
    LC_ALL=C comm -23 "$work/names.all" "$work/names.ignored" >"$list_names"
    grep -c . "$list_names" || true
}

# The candidate may not run fewer tests than the base, nor stop running one
# of the base's by name. Read from what cargo runs, so a test deleted,
# ignored, commented out, left in a file no `mod` includes, or behind
# `#[cfg(any())]` is gone; one renamed or moved to another module path is
# gone too (its name is its module path; a move that keeps the path, the
# same `mod tests` in another file, keeps the name). The base tree stays
# read-only: its cargo writes to a target directory of its own. The
# candidate's target directory is the one check-tests reuses. What names
# cannot see: a test kept by name whose body no longer tests anything (the
# empty-body check above catches only an empty one), and one of two
# same-named tests in different crates traded for a third.
if [ -f "$work/baseline/Cargo.toml" ] && [ -f "$work/candidate/Cargo.toml" ]; then
    section "tests: what cargo runs, base and candidate"
    listing_started=$(date +%s)
    mkdir -p "$work/base-target"
    : >"$work/tests.log"
    if ! base_tests=$(runnable_tests "$work/base-target" "$work/baseline" "$work/base-target" \
        "$work/tests.base-names"); then
        sed 's/^/| tests: /' "$work/tests.log" | tail -n 30
        verdict rejected "base tests do not build"
        exit 0
    fi
    : >"$work/tests.log"
    if ! candidate_tests=$(runnable_tests "$work/candidate" "$work/candidate" "$work/candidate/target" \
        "$work/tests.candidate-names"); then
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
    dropped=$(LC_ALL=C comm -23 "$work/tests.base-names" "$work/tests.candidate-names")
    # The owner retires tests with the features they removed: names listed in
    # `<scenarios-dir>/retired-tests` (one per line, `#` comments) may go.
    if [ -n "$dropped" ] && [ -f "$scenarios/retired-tests" ]; then
        grep -v '^#' "$scenarios/retired-tests" | grep . | LC_ALL=C sort -u >"$work/tests.retired"
        dropped=$(printf '%s\n' "$dropped" | LC_ALL=C comm -23 - "$work/tests.retired")
    fi
    if [ -n "$dropped" ]; then
        echo "| base tests the candidate does not run by name: $(printf '%s\n' "$dropped" | head -n 20 | tr '\n' ' ')"
        verdict rejected "candidate drops or renames base tests ($(printf '%s\n' "$dropped" | grep -c .): $(printf '%s\n' "$dropped" | head -n 3 | tr '\n' ' ' | sed 's/ $//'))"
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
