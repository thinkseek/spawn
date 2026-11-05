# timing-shim (scripts/timing): tmux, with Spawn's calls perturbed. Spawn
# runs tmux with every SPAWN_* variable removed; a call with one is the
# harness's own and runs untouched.
real=$(sed -n "s/^REAL_TMUX='\(.*\)'$/\1/p" "$D/config")
if [ -n "${SPAWN_DATABASE_URL+x}${SPAWN_TMUX_SOCKET+x}${SPAWN_BIN+x}${SPAWN_TEST_DATABASE_URL+x}" ]; then
    exec "$real" "$@"
fi
sub=
i=1
while [ "$i" -le $# ]; do
    a=${!i}
    case "$a" in
    -L | -S | -f | -T) i=$((i + 2)) ;;
    -*) i=$((i + 1)) ;;
    *) sub=$a; break ;;
    esac
done
case "$sub" in
new-session | capture-pane | set-buffer | paste-buffer | send-keys) ;;
*) exec "$real" "$@" ;;
esac
. "$D/lib.sh"
case "$sub" in
new-session)
    # The engine's terminal: `<spawn> run -- <engine> ...` becomes
    # `<spawn> run -- engine-proxy <engine> ...`, whose output reaches the
    # pane through the lagging relay.
    if t_on screen; then
        n=$#
        k=0
        prev=
        inserted=
        while [ "$k" -lt "$n" ]; do
            a=$1
            shift
            set -- "$@" "$a"
            if [ -z "$inserted" ] && [ "$prev" = run ] && [ "$a" = -- ]; then
                set -- "$@" "$D/bin/engine-proxy"
                inserted=1
            fi
            prev=$a
            k=$((k + 1))
        done
    fi ;;
capture-pane)
    # flaky: an empty capture, or a refusal; tmux: a slow one.
    t_roll capture-pane
    if t_on flaky && [ "$t_roll" -lt "$P_CAPTURE_EMPTY" ]; then
        t_log capture-pane empty ""
        exit 0
    elif t_on flaky && [ "$t_roll" -lt $((P_CAPTURE_EMPTY + P_CAPTURE_REFUSE)) ]; then
        t_log capture-pane refused ""
        echo "timing: capture-pane refused (adversarial timing)" >&2
        exit 1
    elif t_on tmux && [ "$t_roll" -ge $((P_CAPTURE_EMPTY + P_CAPTURE_REFUSE)) ] &&
        [ "$t_roll" -lt $((P_CAPTURE_EMPTY + P_CAPTURE_REFUSE + P_CAPTURE_SLOW)) ]; then
        ms=$(t_ms capture-pane "$CAPTURE_MIN" "$CAPTURE_MAX")
        t_log capture-pane "delay=${ms}ms" ""
        t_sleep "$ms"
    fi ;;
set-buffer)
    # flaky: refused before anything is typed (the pane is unchanged);
    # tmux: slow.
    t_roll set-buffer
    if t_on flaky && [ "$t_roll" -lt "$P_SET_BUFFER_REFUSE" ]; then
        t_log set-buffer refused ""
        echo "timing: set-buffer refused (adversarial timing)" >&2
        exit 1
    elif t_on tmux && [ "$t_roll" -ge "$P_SET_BUFFER_REFUSE" ] &&
        [ "$t_roll" -lt $((P_SET_BUFFER_REFUSE + P_SET_BUFFER_SLOW)) ]; then
        ms=$(t_ms set-buffer "$SET_BUFFER_MIN" "$SET_BUFFER_MAX")
        t_log set-buffer "delay=${ms}ms" ""
        t_sleep "$ms"
    fi ;;
paste-buffer)
    t_on tmux && t_maybe_delay paste-buffer "$P_PASTE_SLOW" "$PASTE_MIN" "$PASTE_MAX" "" ;;
send-keys)
    # Slow only: the text may be pasted and on screen a while before its
    # Enter goes in.
    t_on tmux && t_maybe_delay send-keys "$P_SEND_KEYS_SLOW" "$SEND_KEYS_MIN" "$SEND_KEYS_MAX" "${@: -1}" ;;
esac
exec "$real" "$@"
