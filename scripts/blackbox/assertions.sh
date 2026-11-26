#!/bin/sh
# Shared helpers for black-box scenarios.
#
# A scenario is a POSIX shell script that drives the `spawn` binary and reads
# only what an outside observer can see: exit codes, standard output, the
# database, the workspace directory and the terminal pane.
#
# The runner exports, for every scenario:
#
#   SPAWN_BIN               the binary under test
#   SPAWN_DATABASE_URL      a database of this scenario alone
#   SPAWN_DB_NAME           its name, for `sql`
#   SPAWN_PSQL              command prefix of psql, e.g. `docker exec c psql -U postgres`
#   SPAWN_GIT_REPO          a bare repository holding one `main` commit
#   SPAWN_WORKSPACE_ROOT    an empty directory
#   SPAWN_CLAUDE_PROGRAM    a fake engine that echoes each typed line
#   SPAWN_TMUX_SOCKET       a private tmux socket
#   WORK                    scratch directory of this scenario
#
# Transient output of `capture` lands in `$WORK/out.txt` and `$WORK/err.txt`.

set -eu

fail() {
    echo "blackbox: $*" >&2
    exit 1
}

assert_eq() {
    [ "$1" = "$2" ] || fail "${3:-value}: expected [$2], got [$1]"
}

assert_contains() {
    case "$1" in
        *"$2"*) ;;
        *) fail "${3:-text}: expected to contain [$2], got [$1]" ;;
    esac
}

assert_not_contains() {
    case "$1" in
        *"$2"*) fail "${3:-text}: must not contain [$2]" ;;
    esac
}

assert_file() { [ -f "$1" ] || fail "missing file $1"; }
assert_no_file() { [ ! -e "$1" ] || fail "unexpected path $1"; }

# One SQL statement, tuples only.
sql() { $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c "$1"; }

# One SQL statement reduced to its first line, without its trailing newline.
# Internal spaces are kept: a scalar may be a prompt, not only a number.
scalar() { sql "$1" | head -1; }

# Run a command and record its exit code, without tripping `set -e`.
capture() {
    status=0
    "$@" >"$WORK/out.txt" 2>"$WORK/err.txt" || status=$?
    return 0
}

# bb_http <file> <curl arguments...>: one HTTP request through curl, its
# body into <file>; prints the status code once (000 when curl got no
# answer). Why curl got none goes to <file>.why (empty on an answer) and to
# stderr: curl's exit code and error text, and whether serve still runs
# (`serve_pid`, when the scenario sets it). A connection-level failure is
# retried once, and said: curl exit 7 (refused: nothing was sent) for any
# request; 52 and 56 (empty reply, reset) for a GET only, since a POST may
# have been acted on. A serve that has died fails the retry too.
bb_http() {
    bb_http_out=$1
    shift
    bb_http_url=""
    bb_http_method=GET
    for bb_http_arg in "$@"; do
        case $bb_http_arg in
        http://* | https://*) bb_http_url=$bb_http_arg ;;
        -X | --request | -d | --data | --data-* | -F | --form) bb_http_method=POST ;;
        esac
    done
    bb_http_try=1
    bb_http_first=""
    while :; do
        bb_http_rc=0
        bb_http_code=$(curl -sS -o "$bb_http_out" -w '%{http_code}' "$@" 2>"$bb_http_out.curl") || bb_http_rc=$?
        if [ "$bb_http_rc" -eq 0 ]; then
            if [ -n "$bb_http_first" ]; then
                printf 'answered on a retry; the first try: %s\n' "$bb_http_first" >"$bb_http_out.why"
            else
                : >"$bb_http_out.why"
            fi
            break
        fi
        bb_http_text=$(tr '\n' ' ' <"$bb_http_out.curl" | head -c 300)
        bb_http_alive=unknown
        if [ -n "${serve_pid-}" ]; then
            bb_http_alive=no
            if kill -0 "$serve_pid" 2>/dev/null; then bb_http_alive=yes; fi
        fi
        bb_http_why="curl exit $bb_http_rc: $bb_http_text (serve running: $bb_http_alive)"
        bb_http_again=no
        case $bb_http_rc in
        7) bb_http_again=yes ;;
        52 | 56) if [ "$bb_http_method" = GET ]; then bb_http_again=yes; fi ;;
        esac
        if [ "$bb_http_try" -eq 1 ] && [ "$bb_http_again" = yes ]; then
            bb_http_first=$bb_http_why
            echo "blackbox: $bb_http_method $bb_http_url: $bb_http_why; retrying once" >&2
            bb_http_try=2
            sleep 1
            continue
        fi
        printf '%s%s\n' "$bb_http_why" "${bb_http_first:+; it failed on the retry too, the first try: $bb_http_first}" >"$bb_http_out.why"
        echo "blackbox: $bb_http_method $bb_http_url: no answer: $(cat "$bb_http_out.why")" >&2
        bb_http_code=000
        break
    done
    printf '%s\n' "${bb_http_code:-000}"
}
# bb_http_why <file>: " (why)" after a request bb_http made, if curl said why.
bb_http_why() { [ -s "$1.why" ] && printf ' (%s)' "$(cat "$1.why")"; }

out() { cat "$WORK/out.txt"; }
err() { cat "$WORK/err.txt"; }

# Poll a command until it succeeds, up to `limit` quarter seconds.
wait_for() {
    limit=$1
    shift
    n=0
    while [ "$n" -lt "$limit" ]; do
        if "$@" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.25
        n=$((n + 1))
    done
    return 1
}

_bb_hc() {
    bb_hc=${SPAWN_HERDR_CORE_PROGRAM-}
    if [ -z "$bb_hc" ] && [ -n "${SPAWN_BIN-}" ]; then
        bb_hc_dir=$(dirname "$SPAWN_BIN")
        if [ -x "$bb_hc_dir/spawn-hc" ]; then
            bb_hc=$bb_hc_dir/spawn-hc
        elif [ -x "$(dirname "$bb_hc_dir")/spawn-hc" ]; then
            bb_hc=$(dirname "$bb_hc_dir")/spawn-hc
        fi
    fi
    if [ -z "$bb_hc" ]; then
        bb_hc=$(command -v spawn-hc 2>/dev/null || true)
    fi
    [ -n "$bb_hc" ] && [ -x "$bb_hc" ] || return 1
    env -u HERDR_SOCKET_PATH -u HERDR_SESSION -u HERDR_PANE_ID \
        HC_SESSION="$SPAWN_HERDR_SESSION" \
        HC_SOCKET="$XDG_RUNTIME_DIR/herdr-core/$SPAWN_HERDR_SESSION.sock" \
        "$bb_hc" "$@"
}

# Last lines of the only terminal of this scenario.
pane() {
    if [ -n "${SPAWN_HERDR_SESSION-}" ]; then
        bb_t=$(terminal)
        [ -n "$bb_t" ] || return 1
        _bb_hc read --lines 80 "$bb_t"
    else
        tmux -L "$SPAWN_TMUX_SOCKET" capture-pane -p -t "=$(terminal):" -S -80
    fi
}

terminal() {
    if [ -n "${SPAWN_HERDR_SESSION-}" ]; then
        _bb_hc ls --json --label "spawn_owner=$SPAWN_HERDR_SESSION" 2>/dev/null |
            jq -r '[.terminals[]? | select(.state == "live") | .address][0] // empty'
    else
        tmux -L "$SPAWN_TMUX_SOCKET" list-sessions -F '#{session_name}' | head -1
    fi
}

# What a scenario prints to stderr when it gives up waiting for a
# supervisor's turn, so the gate log shows where the line stopped: the
# commands submitted, the newest events, and every pane on the scenario's
# own terminal server. Everything comes from the scenario's own database and
# socket, never a live one. The credentials of any URL are masked.
supervisor_turn_dump() { # <what the scenario waited for>
    {
        echo "-- gave up waiting for: $1"
        echo "-- spawn_commands, newest 10:"
        sql "select concat_ws(' | ', id, principal, idempotency_key, session_id, state, 'attempt ' || lease_attempt,
                              'succeeded ' || coalesce(succeeded::text, '-'), 'created ' || created_at_ms,
                              'finished ' || coalesce(finished_at_ms::text, '-'), 'error ' || coalesce(left(error, 200), '-'),
                              coalesce(action ->> 'type', '-'), left(coalesce(action ->> 'text', ''), 160))
               from spawn_commands order by id desc limit 10" 2>&1
        echo "-- spawn_events, newest 40, oldest first:"
        sql "select concat_ws(' | ', e.id, e.recorded_at_ms, e.session_id, e.event_name, left(e.kind::text, 300))
               from (select * from spawn_events order by id desc limit 40) e order by e.id" 2>&1
        if [ -n "${SPAWN_HERDR_SESSION-}" ]; then
            case "${WORK:+w}:${XDG_RUNTIME_DIR-}/" in
                :*) echo "-- panes skipped: WORK is not set" ;;
                w:"$WORK"/*)
                    bb_sock="$XDG_RUNTIME_DIR/herdr-core/$SPAWN_HERDR_SESSION.sock"
                    if [ ! -S "$bb_sock" ]; then
                        echo "-- panes skipped: no herdr socket ($bb_sock)"
                    else
                        for bb_addr in $(_bb_hc ls --json --label "spawn_owner=$SPAWN_HERDR_SESSION" 2>/dev/null |
                            jq -r '.terminals[]? | select(.state == "live") | .address // empty' 2>/dev/null || true); do
                            echo "-- pane $bb_addr, last 30 lines:"
                            _bb_hc read --lines 30 "$bb_addr" 2>&1 | tail -n 30
                        done
                    fi
                    ;;
                *) echo "-- panes skipped: XDG_RUNTIME_DIR is not under \$WORK" ;;
            esac
        else
            # Panes only from a named socket under the scenario's own
            # TMUX_TMPDIR: `tmux -L ""`, or no TMUX_TMPDIR, is the host's own
            # server, whose panes must never reach a gate log.
            case "${WORK:+w}:${SPAWN_TMUX_SOCKET-}:${TMUX_TMPDIR-}/" in
                :*) echo "-- panes skipped: WORK is not set" ;;
                w::* | w:*:/) echo "-- panes skipped: no scenario tmux socket (SPAWN_TMUX_SOCKET and TMUX_TMPDIR under \$WORK)" ;;
                w:*:"$WORK"/*)
                    for session in $(tmux -L "$SPAWN_TMUX_SOCKET" list-sessions -F '#{session_name}' 2>/dev/null); do
                        echo "-- pane $session, last 30 lines:"
                        tmux -L "$SPAWN_TMUX_SOCKET" capture-pane -p -t "=$session:" -S -30 2>&1 | tail -n 30
                    done
                    ;;
                *) echo "-- panes skipped: TMUX_TMPDIR is not under \$WORK" ;;
            esac
        fi
    } 2>&1 | sed -E 's#(://)[^@[:space:]]*@#\1***@#g' >&2 || true
}

# bb_engine >FILE <<'ENGINE' ... ENGINE: writes a fake engine (a bash
# script, its `#!` line first) with bb_read_line defined right after that
# line, for its input loop:
#
#   if bb_read_line typed; then ...    # a whole typed line in $typed
#   elif ...                           # a second with no whole line
#
# bb_read_line VAR waits a second for a line, and returns read's status
# when none came (the engine's quiet). bash's `read -t` keeps what it read of
# a line when it times out, which happens when a paste straddles the second
# (a loaded host): reading that part into the line's variable and dropping it
# turned "BB:SAY ..." into "AY ..." (070's gate failure, sync/flake-070.md).
# bb_read_line holds that part and goes on reading, so a cut line is no quiet
# and a turn does not end mid-paste (objective 078's engine did the same);
# the rest of the line joins it. A part held 10 s with no end of line is
# returned as the line, so a line that never ends cannot stall the engine.
bb_engine() {
    IFS= read -r bb_engine_first || fail "bb_engine: no engine on standard input"
    case $bb_engine_first in
    '#!'*bash*) ;;
    *) fail "bb_engine: the engine's first line is not a bash #! line: $bb_engine_first" ;;
    esac
    printf '%s\n' "$bb_engine_first"
    cat <<'READ'
# bb_read_line VAR: scripts/blackbox/assertions.sh's (bb_engine).
bb_rl_held=
bb_read_line() {
  local bb_rl_part bb_rl_status bb_rl_waits=0
  while :; do
    bb_rl_part='' bb_rl_status=0
    IFS= read -r -t 1 bb_rl_part || bb_rl_status=$?
    bb_rl_held=$bb_rl_held$bb_rl_part
    if [ "$bb_rl_status" != 0 ]; then
      # Quiet (or the end of input) with no part held: no line.
      [ "$bb_rl_status" -gt 128 ] && [ -n "$bb_rl_held" ] || return "$bb_rl_status"
      bb_rl_waits=$((bb_rl_waits + 1))
      [ "$bb_rl_waits" -ge 10 ] || continue
    fi
    printf -v "$1" '%s' "$bb_rl_held"
    bb_rl_held=
    return 0
  done
}
READ
    cat
}
