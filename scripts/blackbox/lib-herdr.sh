# Shared POSIX-sh helper for black-box scenarios running against spawn-hc (herdr-core).
# Sourced by scripts/blackbox/{phase,run-scenarios}.sh and rsi/scenarios/*.sh.
# Strict POSIX /bin/sh (dash-safe): no `local`, no `[[ ... ]]`, no `VAR=val fn`.

herdr_harness_fail() {
    if command -v harness >/dev/null 2>&1; then
        harness "$*"
    fi
    echo "lib-herdr: harness: $*" >&2
    exit 2
}

herdr_dir() {
    if [ -n "${controller-}" ] && [ -f "$controller/scripts/blackbox/lib-herdr.sh" ]; then
        printf '%s\n' "$controller/scripts/blackbox"
    elif [ -n "${root-}" ] && [ -f "$root/scripts/blackbox/lib-herdr.sh" ]; then
        printf '%s\n' "$root/scripts/blackbox"
    else
        herdr_harness_fail "cannot locate scripts/blackbox/lib-herdr.sh"
    fi
}

herdr_resolve_bin() {
    if [ -n "${SPAWN_HERDR_REAL_CORE_PROGRAM-}" ] && [ -x "$SPAWN_HERDR_REAL_CORE_PROGRAM" ]; then
        return 0
    fi
    if [ -n "${SPAWN_HERDR_CORE_PROGRAM-}" ] && [ -x "$SPAWN_HERDR_CORE_PROGRAM" ]; then
        SPAWN_HERDR_REAL_CORE_PROGRAM=$SPAWN_HERDR_CORE_PROGRAM
        export SPAWN_HERDR_REAL_CORE_PROGRAM
        return 0
    fi
    if [ -n "${SPAWN_BIN-}" ]; then
        hs_cand="$(dirname "$SPAWN_BIN")/spawn-hc"
        if [ -x "$hs_cand" ]; then
            SPAWN_HERDR_REAL_CORE_PROGRAM=$hs_cand
            export SPAWN_HERDR_REAL_CORE_PROGRAM
            return 0
        fi
    fi
    hs_cand=$(command -v spawn-hc 2>/dev/null || true)
    if [ -n "$hs_cand" ] && [ -x "$hs_cand" ]; then
        SPAWN_HERDR_REAL_CORE_PROGRAM=$hs_cand
        export SPAWN_HERDR_REAL_CORE_PROGRAM
        return 0
    fi
    herdr_harness_fail "spawn-hc executable not found (set SPAWN_HERDR_CORE_PROGRAM or build beside SPAWN_BIN)"
}

# herdr_setup <work_dir> [session_prefix]
herdr_setup() {
    hs_work=$1
    hs_prefix=$(printf '%s' "${2:-s}" | tr -cd 'A-Za-z0-9_-' | cut -c1-6)
    [ -n "$hs_prefix" ] || hs_prefix=s
    herdr_resolve_bin

    HERDR_SEQ=$(( ${HERDR_SEQ:-0} + 1 ))
    export HERDR_SEQ
    SPAWN_HERDR_SESSION="${hs_prefix}_${$}_${HERDR_SEQ}"
    if [ "${#SPAWN_HERDR_SESSION}" -gt 16 ]; then
        hs_rand=$(od -An -N2 -tx2 /dev/urandom 2>/dev/null | tr -d ' ' || printf '%s' "$$")
        SPAWN_HERDR_SESSION="${hs_prefix}_${hs_rand}_${HERDR_SEQ}"
    fi

    # Linux sockaddr_un::sun_path limit is 107 bytes; holder sockets add
    # `/herdr-core/<session>.holder.<8chars>.sock` (33 + len(session) bytes).
    hs_rt="$hs_work/r"
    hs_probe="$hs_rt/herdr-core/$SPAWN_HERDR_SESSION.holder.12345678.sock"
    if [ "${#hs_probe}" -gt 96 ]; then
        HERDR_SHORT_RUNTIME=$(mktemp -d /tmp/shc.XXXXXX) || herdr_harness_fail "mktemp /tmp/shc failed"
        chmod 700 "$HERDR_SHORT_RUNTIME"
        hs_rt=$HERDR_SHORT_RUNTIME
    else
        HERDR_SHORT_RUNTIME=""
    fi

    XDG_RUNTIME_DIR=$hs_rt
    XDG_STATE_HOME=$hs_work/hs
    XDG_CONFIG_HOME=$hs_work/hc
    mkdir -p "$XDG_RUNTIME_DIR/herdr-core" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
    chmod 700 "$XDG_RUNTIME_DIR" "$XDG_RUNTIME_DIR/herdr-core" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"

    SPAWN_TERMINAL_BACKEND=herdr
    SPAWN_HERDR_MODE=core
    SPAWN_HERDR_CORE_PROGRAM=$SPAWN_HERDR_REAL_CORE_PROGRAM
    HC_SESSION=$SPAWN_HERDR_SESSION
    HC_SOCKET="$XDG_RUNTIME_DIR/herdr-core/$SPAWN_HERDR_SESSION.sock"
    HERDR_WORK_DIR=$hs_work
    HERDR_UPSTREAM_SOCKET=""
    HERDR_UPSTREAM_NAME=""
    HERDR_PROXY_PIDFILE=""

    unset SPAWN_TMUX_PROGRAM SPAWN_TMUX_SOCKET TMUX_TMPDIR SPAWN_HERDR_SHARDS SPAWN_HERDR_PROGRAM 2>/dev/null || true
    export XDG_RUNTIME_DIR XDG_STATE_HOME XDG_CONFIG_HOME \
        SPAWN_TERMINAL_BACKEND SPAWN_HERDR_MODE SPAWN_HERDR_CORE_PROGRAM SPAWN_HERDR_SESSION \
        HC_SESSION HC_SOCKET HERDR_WORK_DIR HERDR_SHORT_RUNTIME \
        HERDR_UPSTREAM_SOCKET HERDR_UPSTREAM_NAME HERDR_PROXY_PIDFILE
}

# herdr_use_slow_wrapper <work_dir> <flag_file> [sleep_secs]
herdr_use_slow_wrapper() {
    hs_w=$1
    hs_flag=$2
    hs_secs=${3:-3.5}
    herdr_resolve_bin
    hs_wrap="$hs_w/hc-slow"
    cat >"$hs_wrap" <<EOF
#!/bin/sh
if [ "\${1-}" = server ] && [ "\${2-}" = start ] && [ -e '$hs_flag' ]; then
    sleep '$hs_secs'
fi
exec '$SPAWN_HERDR_REAL_CORE_PROGRAM' "\$@"
EOF
    chmod 755 "$hs_wrap"
    SPAWN_HERDR_CORE_PROGRAM=$hs_wrap
    export SPAWN_HERDR_CORE_PROGRAM
}

# herdr_use_fault_proxy <work_dir> [proxy CLI args...]
herdr_use_fault_proxy() {
    hs_w=$1
    shift
    herdr_resolve_bin
    hs_sdir=$(herdr_dir)
    hs_up_sess="${SPAWN_HERDR_SESSION}u"
    hs_up_sock="$XDG_RUNTIME_DIR/herdr-core/${hs_up_sess}.sock"
    hs_pidfile="$hs_w/hc-proxy.pid"
    hs_wrap="$hs_w/hc-proxy"

    # Build POSIX-quoted extra args file so paths/substrings survive any dash quoting.
    python3 - "$hs_w/hc-proxy.args" "$@" <<'PY'
import json, sys
with open(sys.argv[1], "w") as f:
    json.dump(sys.argv[2:], f)
PY

    cat >"$hs_wrap" <<EOF
#!/bin/sh
if [ "\${1-}" = server ] && [ "\${2-}" = start ]; then
    exec python3 -B '$hs_sdir/herdr-fault-proxy.py' \\
        --server-start \\
        --upstream-program '$SPAWN_HERDR_REAL_CORE_PROGRAM' \\
        --upstream-session '$hs_up_sess' \\
        --upstream-socket '$hs_up_sock' \\
        --listen '$HC_SOCKET' \\
        --pidfile '$hs_pidfile' \\
        --args-file '$hs_w/hc-proxy.args'
fi
HC_SESSION='$hs_up_sess'
HC_SOCKET='$hs_up_sock'
export HC_SESSION HC_SOCKET
exec '$SPAWN_HERDR_REAL_CORE_PROGRAM' "\$@"
EOF
    chmod 755 "$hs_wrap"
    SPAWN_HERDR_CORE_PROGRAM=$hs_wrap
    HERDR_UPSTREAM_SOCKET=$hs_up_sock
    HERDR_UPSTREAM_NAME=$hs_up_sess
    HERDR_PROXY_PIDFILE=$hs_pidfile
    export SPAWN_HERDR_CORE_PROGRAM HERDR_UPSTREAM_SOCKET HERDR_UPSTREAM_NAME HERDR_PROXY_PIDFILE
}

# Print the active direct real spawn-hc socket (bypassing any fault proxy).
herdr_real_socket() {
    if [ -n "${HERDR_UPSTREAM_SOCKET-}" ] && [ -S "$HERDR_UPSTREAM_SOCKET" ]; then
        printf '%s\n' "$HERDR_UPSTREAM_SOCKET"
    else
        printf '%s\n' "${HC_SOCKET-}"
    fi
}

herdr_real_session() {
    if [ -n "${HERDR_UPSTREAM_SOCKET-}" ] && [ -S "$HERDR_UPSTREAM_SOCKET" ]; then
        hs_real_sess=${HERDR_UPSTREAM_NAME-}
    else
        hs_real_sess=${HC_SESSION-}
    fi
}

herdr_server_status_json() {
    herdr_resolve_bin
    hs_sock=$(herdr_real_socket)
    herdr_real_session
    hs_sess=$hs_real_sess
    [ -n "$hs_sock" ] && [ -S "$hs_sock" ] || return 1
    (
        HC_SOCKET=$hs_sock
        HC_SESSION=$hs_sess
        export HC_SOCKET HC_SESSION
        exec "$SPAWN_HERDR_REAL_CORE_PROGRAM" server status --json
    ) 2>/dev/null
}

herdr_server_alive() {
    herdr_status=$(herdr_server_status_json) || return 1
    [ -n "$herdr_status" ] || return 1
    hs_pid=$(printf '%s' "$herdr_status" | jq -r '.pid // empty' 2>/dev/null || true)
    [ -n "$hs_pid" ] && kill -0 "$hs_pid" 2>/dev/null
}

herdr_server_pid() {
    herdr_status=$(herdr_server_status_json) || return 0
    printf '%s' "$herdr_status" | jq -r '.pid // empty' 2>/dev/null || true
}

# Echo server process start time in epoch seconds (integer) or milliseconds (`herdr_server_start_ms`).
herdr_server_start_ms() {
    hs_pid=$(herdr_server_pid)
    [ -n "$hs_pid" ] || return 1
    python3 - "$hs_pid" <<'PY'
import os, sys
pid = int(sys.argv[1])
stat = open(f"/proc/{pid}/stat").read()
after = stat.rsplit(") ", 1)[1].split()
start_ticks = int(after[19])
clk = os.sysconf(os.sysconf_names["SC_CLK_TCK"])
btime = 0
for line in open("/proc/stat"):
    if line.startswith("btime "):
        btime = int(line.split()[1])
        break
print(int((btime + start_ticks / clk) * 1000))
PY
}

herdr_ls_json() {
    herdr_resolve_bin
    hs_sock=$(herdr_real_socket)
    herdr_real_session
    hs_sess=$hs_real_sess
    [ -n "$hs_sock" ] && [ -S "$hs_sock" ] || { printf '{"terminals":[]}\n'; return 0; }
    (
        HC_SOCKET=$hs_sock
        HC_SESSION=$hs_sess
        export HC_SOCKET HC_SESSION
        exec "$SPAWN_HERDR_REAL_CORE_PROGRAM" ls --json
    ) 2>/dev/null || printf '{"terminals":[]}\n'
}

herdr_terminal_count() {
    herdr_ls_json | jq -r '(.terminals // []) | length' 2>/dev/null || echo 0
}

herdr_pane_pids() {
    herdr_resolve_bin
    hs_sock=$(herdr_real_socket)
    herdr_real_session
    hs_sess=$hs_real_sess
    [ -n "$hs_sock" ] && [ -S "$hs_sock" ] || return 0
    for hs_a in $(herdr_ls_json | jq -r '(.terminals // [])[] | .address // empty' 2>/dev/null || true); do
        [ -n "$hs_a" ] || continue
        HC_SOCKET=$hs_sock HC_SESSION=$hs_sess "$SPAWN_HERDR_REAL_CORE_PROGRAM" get "$hs_a" --json 2>/dev/null | jq -r '.pid // empty' || true
    done
}

herdr_first_terminal() {
    herdr_ls_json | jq -r '(.terminals // [])[0] | (.address // empty)' 2>/dev/null || true
}

herdr_pane_text() {
    herdr_resolve_bin
    hs_t=$1
    hs_lines=${2:-80}
    hs_sock=$(herdr_real_socket)
    herdr_real_session
    hs_sess=$hs_real_sess
    [ -n "$hs_t" ] && [ -n "$hs_sock" ] && [ -S "$hs_sock" ] || return 1
    (
        HC_SOCKET=$hs_sock
        HC_SESSION=$hs_sess
        export HC_SOCKET HC_SESSION
        exec "$SPAWN_HERDR_REAL_CORE_PROGRAM" read --lines "$hs_lines" "$hs_t"
    )
}

# Forcefully stop proxy (if any), all live terminals + holders, and server daemon.
herdr_kill_server() {
    herdr_resolve_bin >/dev/null 2>&1 || true
    if [ -n "${HERDR_PROXY_PIDFILE-}" ] && [ -f "$HERDR_PROXY_PIDFILE" ]; then
        hs_ppid=$(cat "$HERDR_PROXY_PIDFILE" 2>/dev/null || true)
        if [ -n "$hs_ppid" ]; then
            kill -KILL "$hs_ppid" 2>/dev/null || true
        fi
        rm -f "$HERDR_PROXY_PIDFILE" 2>/dev/null || true
    fi
    for hs_sock in "${HERDR_UPSTREAM_SOCKET-}" "${HC_SOCKET-}"; do
        [ -n "$hs_sock" ] || continue
        if [ "$hs_sock" = "${HERDR_UPSTREAM_SOCKET-}" ]; then
            hs_sess=${HERDR_UPSTREAM_NAME-}
        else
            hs_sess=${HC_SESSION-}
        fi
        if [ -S "$hs_sock" ] && [ -n "${SPAWN_HERDR_REAL_CORE_PROGRAM-}" ] && [ -x "${SPAWN_HERDR_REAL_CORE_PROGRAM-}" ]; then
            hs_status=$(
                HC_SOCKET=$hs_sock HC_SESSION=$hs_sess "$SPAWN_HERDR_REAL_CORE_PROGRAM" server status --json 2>/dev/null || true
            )
            hs_srv_pid=$(printf '%s' "$hs_status" | jq -r '.pid // empty' 2>/dev/null || true)
            hs_holders=$(printf '%s' "$hs_status" | jq -r 'if (.holders | type) == "array" then (.holders[] | .pid? // empty) else empty end' 2>/dev/null || true)
            hs_ls=$(
                HC_SOCKET=$hs_sock HC_SESSION=$hs_sess "$SPAWN_HERDR_REAL_CORE_PROGRAM" ls --json 2>/dev/null || true
            )
            hs_addrs=$(printf '%s' "$hs_ls" | jq -r '(.terminals // [])[] | .address // empty' 2>/dev/null || true)
            hs_term_pids=$(printf '%s' "$hs_ls" | jq -r '(.terminals // [])[] | .pid // empty' 2>/dev/null || true)
            for hs_a in $hs_addrs; do
                HC_SOCKET=$hs_sock HC_SESSION=$hs_sess "$SPAWN_HERDR_REAL_CORE_PROGRAM" kill "$hs_a" >/dev/null 2>&1 || true
            done
            HC_SOCKET=$hs_sock HC_SESSION=$hs_sess "$SPAWN_HERDR_REAL_CORE_PROGRAM" server stop >/dev/null 2>&1 || true
            for hs_p in $hs_term_pids $hs_holders $hs_srv_pid; do
                [ -n "$hs_p" ] || continue
                kill -KILL "$hs_p" 2>/dev/null || true
            done
        fi
        rm -f "$hs_sock" 2>/dev/null || true
        if [ -n "${XDG_RUNTIME_DIR-}" ] && [ -n "$hs_sess" ]; then
            rm -f "$XDG_RUNTIME_DIR/herdr-core/$hs_sess".* 2>/dev/null || true
        fi
    done
    # Fallback reap by /proc/*/environ in case a daemon's socket disappeared before teardown.
    for hs_sess in "${HERDR_UPSTREAM_NAME-}" "${HC_SESSION-}" "${SPAWN_HERDR_SESSION-}"; do
        [ -n "$hs_sess" ] || continue
        for hs_env in /proc/[0-9]*/environ; do
            [ -r "$hs_env" ] || continue
            hs_p=${hs_env#/proc/}
            hs_p=${hs_p%/environ}
            [ "$hs_p" != "$$" ] || continue
            if tr '\0' '\n' <"$hs_env" 2>/dev/null | grep -qx "HC_SESSION=$hs_sess"; then
                hs_cmd=$(tr '\0' ' ' <"/proc/$hs_p/cmdline" 2>/dev/null || true)
                case " $hs_cmd " in
                *spawn-hc* | *herdr-fault-proxy.py*)
                    kill -KILL "$hs_p" 2>/dev/null || true
                    ;;
                esac
            fi
        done
    done
    return 0
}

herdr_cleanup() {
    herdr_kill_server >/dev/null 2>&1 || true
    if [ -n "${HERDR_SHORT_RUNTIME-}" ] && [ -d "$HERDR_SHORT_RUNTIME" ]; then
        chmod -R u+rwX "$HERDR_SHORT_RUNTIME" 2>/dev/null || true
        rm -rf "$HERDR_SHORT_RUNTIME" 2>/dev/null || true
        HERDR_SHORT_RUNTIME=""
    fi
    return 0
}
