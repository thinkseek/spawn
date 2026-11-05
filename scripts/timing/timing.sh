# Adversarial timing for the black-box scenarios and the e2e: perturb the
# orderings between Spawn's independently ticking loops (Bridge, Observer,
# dispatcher, integrator) through wrappers and the fake engine only, never
# by changing Spawn.
#
#   SPAWN_TIMING=adversarial   turn it on (run-scenarios.sh and e2e.sh call
#                              timing_enable; scripts/timing/run.sh too)
#   SPAWN_TIMING_SEED=<n>      the seed (default: a random one, printed)
#   SPAWN_TIMING_KINDS=<list>  comma list of kinds, default all; `-<kind>`
#                              leaves one out (`all,-flaky`):
#       checkpoint  git commit-tree and update-ref run late
#       push        git push runs late (the pusher lags the checkpoint)
#       tmux        Spawn's tmux calls run late: capture-pane, set-buffer,
#                   paste-buffer, send-keys (text on screen before its Enter)
#       flaky       Spawn's tmux calls fail now and then: capture-pane
#                   empty or refused, set-buffer refused (before anything
#                   is typed)
#       screen      the engine's output reaches the pane late, the prompt
#                   later still (the screen lags the transcript)
#       transcript  the Bridge reads a mirror of the projects directory that
#                   gets each line late, a turn's end later still (the
#                   transcript lags the screen)
#       integrate   an `integrate` pass starts late
#   SPAWN_TIMING_SCALE=<pct>   multiply every delay (default 100)
#
# timing_enable <dir> writes <dir>/config, <dir>/bin/{git,tmux,spawn,
# engine-proxy} and puts <dir>/bin first in PATH. Afterwards TIMING_SPAWN is
# the spawn wrapper to use as SPAWN_BIN. Only calls Spawn makes are
# perturbed: Spawn starts tmux with every SPAWN_* variable removed, so a
# tmux call with one (SPAWN_DATABASE_URL, SPAWN_TMUX_SOCKET, SPAWN_BIN) is
# the harness's own and runs untouched. Git calls are only ever delayed.
# Every perturbation is a line of <dir>/perturbations.log; timing_report
# <dir> prints the seed and a count per kind and action.
#
# POSIX sh; sourced.

timing_lib=${timing_lib:-$(cd "$(dirname "${BASH_SOURCE:-$0}")" 2>/dev/null && pwd)}

timing_seed() {
    if [ -n "${SPAWN_TIMING_SEED:-}" ]; then
        echo "$SPAWN_TIMING_SEED"
    else
        od -An -N2 -tu2 /dev/urandom | tr -d ' '
    fi
}

# timing_enable <dir> [<spawn-binary>]
timing_enable() {
    timing_dir=$1
    timing_real_spawn=${2:-${SPAWN_BIN:-}}
    # Enabled again inside an enabled run (with.sh around check-blackbox.sh):
    # wrap the real binary, not the outer wrapper.
    if [ -f "$timing_real_spawn" ] && grep -q 'timing-shim' "$timing_real_spawn" 2>/dev/null; then
        timing_real_spawn=$(sed -n "s/^D='\(.*\)'$/\1/p" "$timing_real_spawn")/config
        timing_real_spawn=$(sed -n "s/^REAL_SPAWN='\(.*\)'$/\1/p" "$timing_real_spawn")
    fi
    mkdir -p "$timing_dir/bin" "$timing_dir/n" || return 1
    timing_dir=$(cd "$timing_dir" && pwd)
    # The real tools: the first in PATH that is not a timing shim.
    timing_real_git=$(timing_which git) || return 1
    timing_real_tmux=$(timing_which tmux) || return 1
    TIMING_SEED=$(timing_seed)
    cat >"$timing_dir/config" <<EOF
# Adversarial timing profile (scripts/timing/timing.sh). Per mille, ms.
TIMING_SEED='$TIMING_SEED'
TIMING_KINDS='${SPAWN_TIMING_KINDS:-all}'
TIMING_SCALE='${SPAWN_TIMING_SCALE:-100}'
REAL_GIT='$timing_real_git'
REAL_TMUX='$timing_real_tmux'
REAL_SPAWN='$timing_real_spawn'
P_COMMIT_TREE=500
COMMIT_TREE_MIN=300
COMMIT_TREE_MAX=4000
P_UPDATE_REF=300
UPDATE_REF_MIN=100
UPDATE_REF_MAX=1500
P_PUSH=400
PUSH_MIN=200
PUSH_MAX=3000
P_CAPTURE_EMPTY=40
P_CAPTURE_REFUSE=30
P_CAPTURE_SLOW=100
CAPTURE_MIN=100
CAPTURE_MAX=1500
P_SET_BUFFER_REFUSE=80
P_SET_BUFFER_SLOW=100
SET_BUFFER_MIN=100
SET_BUFFER_MAX=800
P_PASTE_SLOW=150
PASTE_MIN=100
PASTE_MAX=1000
P_SEND_KEYS_SLOW=200
SEND_KEYS_MIN=100
SEND_KEYS_MAX=1200
P_SCREEN_CHUNK=250
SCREEN_CHUNK_MIN=100
SCREEN_CHUNK_MAX=1200
P_SCREEN_PROMPT=500
SCREEN_PROMPT_MIN=300
SCREEN_PROMPT_MAX=2500
P_TRANSCRIPT_LINE=250
TRANSCRIPT_LINE_MIN=100
TRANSCRIPT_LINE_MAX=1200
P_TRANSCRIPT_END=500
TRANSCRIPT_END_MIN=300
TRANSCRIPT_END_MAX=2500
P_INTEGRATE=500
INTEGRATE_MIN=100
INTEGRATE_MAX=3000
EOF
    cp "$timing_lib/lib.sh" "$timing_lib/relay.py" "$timing_lib/mirror.py" "$timing_dir/" || return 1
    for shim in git tmux spawn engine-proxy; do
        {
            echo '#!/usr/bin/env bash'
            echo "D='$timing_dir'"
            cat "$timing_lib/shim-$shim.bash"
        } >"$timing_dir/bin/$shim" || return 1
        chmod 755 "$timing_dir/bin/$shim"
    done
    : >>"$timing_dir/perturbations.log"
    PATH=$timing_dir/bin:$PATH
    TIMING_SPAWN=$timing_dir/bin/spawn
    export PATH TIMING_SPAWN
    echo "timing: adversarial seed=$TIMING_SEED kinds=${SPAWN_TIMING_KINDS:-all} scale=${SPAWN_TIMING_SCALE:-100} dir=$timing_dir" >&2
}

# The first `name` in PATH that is not one of our shims (a shim says
# "timing-shim"): a nested enable, as the runner's around run-scenarios.sh,
# chains to the real tool, not to an outer shim.
timing_which() {
    timing_old_ifs=$IFS
    IFS=:
    for timing_path in $PATH; do
        [ -x "$timing_path/$1" ] && [ ! -d "$timing_path/$1" ] || continue
        grep -q 'timing-shim' "$timing_path/$1" 2>/dev/null && continue
        IFS=$timing_old_ifs
        echo "$timing_path/$1"
        return 0
    done
    IFS=$timing_old_ifs
    echo "timing: no $1 in PATH" >&2
    return 1
}

# timing_report <dir>: the seed and what was applied, per kind and action.
timing_report() {
    [ -f "$1/config" ] || return 0
    timing_report_seed=$(sed -n "s/^TIMING_SEED='\(.*\)'$/\1/p" "$1/config")
    timing_report_total=$(wc -l <"$1/perturbations.log" | tr -d ' ')
    echo "timing: seed=$timing_report_seed perturbations=$timing_report_total" >&2
    awk '{ kind = $2; sub(/#.*/, "", kind); action = $3; sub(/=.*/, "", action);
           count[kind " " action]++ }
         END { for (k in count) printf "timing:   %-28s %d\n", k, count[k] }' \
        "$1/perturbations.log" | sort >&2
}
