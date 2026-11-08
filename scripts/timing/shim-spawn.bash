# timing-shim (scripts/timing): spawn. A Bridge (`loop`, `tick`) reads its
# transcripts from a lagging mirror (mirror.py); an `integrate` pass starts
# late. Everything else runs the real binary at once.
real=$(sed -n "s/^REAL_SPAWN='\(.*\)'$/\1/p" "$D/config")
case "${1:-}" in
loop | tick)
    . "$D/lib.sh"
    if t_on transcript && [ -n "${SPAWN_CLAUDE_PROJECTS:-}" ] && [ -w "$D" ]; then
        source_dir=$SPAWN_CLAUDE_PROJECTS
        sum=$(printf '%s' "$source_dir" | cksum)
        mirror_dir=$D/projects/${sum%% *}
        mkdir -p "$source_dir" "$mirror_dir"
        setsid python3 -I "$D/mirror.py" "$D" "$source_dir" "$mirror_dir" </dev/null >/dev/null 2>&1 &
        export SPAWN_CLAUDE_PROJECTS=$mirror_dir
    fi ;;
integrate)
    . "$D/lib.sh"
    if t_on integrate; then
        t_maybe_delay integrate "$P_INTEGRATE" "$INTEGRATE_MIN" "$INTEGRATE_MAX" "${*:2}" || true
    fi ;;
esac
exec "$real" "$@"
