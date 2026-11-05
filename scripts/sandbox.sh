# shellcheck shell=sh
# Run a build of code the owner did not write (a registry proc-macro, a
# vite config) in bubblewrap. Sourced, not run. Used by deploy/deploy.sh.
#
# This is a copy of the sandbox in scripts/rsi/gate.sh, cut down to what a
# build needs; the gate keeps its own (it also hides its base cache, binds
# the scenarios and the test database's socket). Keep the two alike.
#
# Inside: the root is read-only, /tmp is private, the PID and network
# namespaces are new (loopback only: no host service, no internet), the
# user's runtime directory, the docker socket and the host's TMPDIR are
# hidden, and $HOME is an empty tmpfs. Bound back read-only: the Rust
# toolchain (rustup's home, cargo's bin), the install root of node/npm/npx
# when it lives under $HOME, and whatever `sandbox_ro` named. Writable: only
# what `sandbox_rw` named. Cargo gets a private CARGO_HOME whose registry and
# git caches are the real ones, read-only, and runs offline. The owner's
# cargo config is not bound (it may name a compiler cache such as kache).
# The environment is cleared down to HOME, PATH, LANG, TMPDIR=/tmp,
# RUSTUP_HOME and what `sandbox_env` set.
#
#   sandbox_ro <path>...          bind read-only (missing paths are skipped)
#   sandbox_rw <path>...          bind read-write
#   sandbox_env <name> <value>    set in the sandbox
#   sandbox_run <dir> <command...>  run the command in <dir>

if ! command -v bwrap >/dev/null 2>&1; then
    echo "sandbox: bwrap is required to build untrusted code" >&2
    exit 3
fi

sandbox_cargo_home=${CARGO_HOME:-$HOME/.cargo}
sandbox_rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
sandbox_ro_paths=""
sandbox_rw_paths=""
sandbox_env_pairs=""
sandbox_links=""

sandbox_ro() {
    for sandbox_path in "$@"; do
        [ -e "$sandbox_path" ] && sandbox_ro_paths="$sandbox_ro_paths$sandbox_path
"
    done
    return 0
}
sandbox_rw() {
    for sandbox_path in "$@"; do
        [ -e "$sandbox_path" ] || {
            echo "sandbox: $sandbox_path does not exist" >&2
            return 1
        }
        sandbox_rw_paths="$sandbox_rw_paths$sandbox_path
"
    done
}
sandbox_env() {
    sandbox_env_pairs="$sandbox_env_pairs$1=$2
"
}

sandbox_under_home() {
    case "$1" in "$HOME"/*) return 0 ;; esac
    return 1
}
sandbox_bound() {
    sandbox_ifs=$IFS
    IFS='
'
    for sandbox_bound_root in $sandbox_ro_paths; do
        case "$1" in "$sandbox_bound_root" | "$sandbox_bound_root"/*)
            IFS=$sandbox_ifs
            return 0
            ;;
        esac
    done
    IFS=$sandbox_ifs
    return 1
}

# The toolchain, and node/npm/npx when installed under $HOME (with a symlink
# where PATH found it), as gate.sh's `home_binds`.
sandbox_under_home "$sandbox_cargo_home/bin" && sandbox_ro "$sandbox_cargo_home/bin"
sandbox_under_home "$sandbox_rustup_home" && sandbox_ro "$sandbox_rustup_home"
for sandbox_tool in node npm npx; do
    sandbox_found=$(command -v "$sandbox_tool" 2>/dev/null) || continue
    sandbox_real=$(readlink -f "$sandbox_found")
    sandbox_under_home "$sandbox_real" || continue
    sandbox_root=$(dirname "$sandbox_real")
    [ "$(basename "$sandbox_root")" = bin ] && sandbox_root=$(dirname "$sandbox_root")
    case "$sandbox_root" in "$HOME" | "$HOME/.local") sandbox_root=$sandbox_real ;; esac
    sandbox_bound "$sandbox_real" || sandbox_ro "$sandbox_root"
    if [ "$sandbox_found" != "$sandbox_real" ] && sandbox_under_home "$sandbox_found" &&
        ! sandbox_bound "$sandbox_found"; then
        sandbox_links="$sandbox_links$sandbox_real|$sandbox_found
"
    fi
done

sandbox_run() {
    sandbox_dir=$1
    shift
    sandbox_count=$#
    set -- "$@" bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
        --unshare-pid --unshare-net --die-with-parent --new-session
    sandbox_runtime=/run/user/$(id -u)
    [ -d "$sandbox_runtime" ] && set -- "$@" --tmpfs "$sandbox_runtime"
    for sandbox_socket in /run/docker.sock /var/run/docker.sock; do
        [ -S "$sandbox_socket" ] && set -- "$@" --ro-bind /dev/null "$sandbox_socket"
    done
    case "${TMPDIR:-/tmp}" in
    / | /tmp | /tmp/) ;;
    *) [ -d "$TMPDIR" ] && set -- "$@" --tmpfs "$TMPDIR" ;;
    esac
    set -- "$@" --tmpfs "$HOME"
    sandbox_ifs=$IFS
    IFS='
'
    for sandbox_path in $sandbox_ro_paths; do
        set -- "$@" --ro-bind "$sandbox_path" "$sandbox_path"
    done
    for sandbox_link in $sandbox_links; do
        set -- "$@" --symlink "${sandbox_link%%|*}" "${sandbox_link#*|}"
    done
    for sandbox_path in $sandbox_rw_paths; do
        set -- "$@" --bind "$sandbox_path" "$sandbox_path"
    done
    set -- "$@" --dir /tmp/cargo-home
    for sandbox_name in registry git; do
        [ -e "$sandbox_cargo_home/$sandbox_name" ] &&
            set -- "$@" --ro-bind "$sandbox_cargo_home/$sandbox_name" "/tmp/cargo-home/$sandbox_name"
    done
    set -- "$@" --clearenv --setenv HOME "$HOME" --setenv PATH "$PATH" --setenv TMPDIR /tmp \
        --setenv CARGO_HOME /tmp/cargo-home --setenv CARGO_NET_OFFLINE true \
        --setenv npm_config_cache /tmp/npm-cache
    [ -n "${LANG-}" ] && set -- "$@" --setenv LANG "$LANG"
    [ -n "${RUSTUP_HOME-}" ] && set -- "$@" --setenv RUSTUP_HOME "$RUSTUP_HOME"
    for sandbox_pair in $sandbox_env_pairs; do
        set -- "$@" --setenv "${sandbox_pair%%=*}" "${sandbox_pair#*=}"
    done
    IFS=$sandbox_ifs
    set -- "$@" --chdir "$sandbox_dir" --
    # The command was given first: move it after the options.
    while [ "$sandbox_count" -gt 0 ]; do
        set -- "$@" "$1"
        shift
        sandbox_count=$((sandbox_count - 1))
    done
    "$@" </dev/null
}
