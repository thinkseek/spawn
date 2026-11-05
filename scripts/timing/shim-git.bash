# timing-shim (scripts/timing): git, with checkpoint commits, ref updates
# and pushes run late. Never fails a call.
sub=
i=1
while [ "$i" -le $# ]; do
    a=${!i}
    case "$a" in
    -C | -c | --git-dir | --work-tree | --namespace) i=$((i + 2)) ;;
    -*) i=$((i + 1)) ;;
    *) sub=$a; break ;;
    esac
done
case "$sub" in
commit-tree | update-ref | push)
    . "$D/lib.sh"
    case "$sub" in
    commit-tree)
        if t_on checkpoint; then
            what=commit
            for a in "$@"; do
                case "$a" in "spawn checkpoint"*) what=checkpoint ;; "Land task"*) what=landing ;; esac
            done
            t_maybe_delay commit-tree "$P_COMMIT_TREE" "$COMMIT_TREE_MIN" "$COMMIT_TREE_MAX" "$what" || true
        fi ;;
    update-ref)
        if t_on checkpoint; then
            ref=
            j=$((i + 1))
            while [ "$j" -le $# ]; do
                a=${!j}
                case "$a" in -m) j=$((j + 2)) ;; -*) j=$((j + 1)) ;; *) ref=$a; break ;; esac
            done
            t_maybe_delay update-ref "$P_UPDATE_REF" "$UPDATE_REF_MIN" "$UPDATE_REF_MAX" "$ref" || true
        fi ;;
    push)
        if t_on push; then
            t_maybe_delay push "$P_PUSH" "$PUSH_MIN" "$PUSH_MAX" "${@: -1}" || true
        fi ;;
    esac
    exec "$REAL_GIT" "$@" ;;
esac
exec "$(sed -n "s/^REAL_GIT='\(.*\)'$/\1/p" "$D/config")" "$@"
