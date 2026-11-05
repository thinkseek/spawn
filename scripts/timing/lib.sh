# Adversarial timing: the shared roll, sleep and log of every shim
# (scripts/timing/timing.sh writes the shims; each sets D and sources this).
#
# A decision is a pure function of (seed, kind, call number): the n-th
# `commit-tree` of a run under seed S is delayed by the same amount in every
# run. Which real call is the n-th depends on the run's own interleaving, so
# a seed reproduces the perturbation sequence, not the wall clock.
#
# Every perturbation applied is one line of $D/perturbations.log:
#   <hh:mm:ss.mmm> <kind>#<n> <action> <detail>
# bash; sourced, never run.

# shellcheck source=/dev/null
. "$D/config"

# Whether perturbations of `kind` are on (TIMING_KINDS: a comma list of
# kinds, `all`, and `-<kind>` to leave one out: `all,-flaky`).
t_on() {
    case ",$TIMING_KINDS," in
    *",-$1,"*) return 1 ;;
    esac
    case ",$TIMING_KINDS," in
    *",all,"* | *",$1,"*) return 0 ;;
    esac
    return 1
}

# 0..999 from the seed and a salt.
t_hash() {
    local sum
    sum=$(printf '%s:%s' "$TIMING_SEED" "$1" | cksum)
    echo $(((${sum%% *}) % 1000))
}

# The next call of `kind`: sets t_n (its number) and t_roll (0..999).
# Without a writable timing directory (a read-only sandbox, as inside the
# gate) nothing is perturbed: t_roll is 1000.
t_roll() {
    t_n=0
    t_roll=1000
    [ -w "$D/n" ] || return 0
    t_n=$({
        flock 9
        local n
        n=$(cat "$D/n/$1" 2>/dev/null)
        n=$((${n:-0} + 1))
        echo "$n" >"$D/n/$1"
        echo "$n"
    } 9>>"$D/n/$1.lock")
    t_roll=$(t_hash "$1:$t_n")
}

# A delay in ms in [min, max], scaled by TIMING_SCALE percent, fixed by
# the current call (t_n) of `kind`.
t_ms() { # <kind> <min> <max>
    local h
    h=$(t_hash "$1:$t_n:delay")
    echo $((($2 + h * ($3 - $2) / 999) * TIMING_SCALE / 100))
}

t_sleep() { # <ms>
    sleep "$(printf '%d.%03d' $(($1 / 1000)) $(($1 % 1000)))"
}

t_log() { # <kind> <action> <detail>
    printf '%s %s#%s %s %s\n' "$(date +%H:%M:%S.%3N)" "$1" "$t_n" "$2" "$3" \
        >>"$D/perturbations.log" 2>/dev/null || true
}

# Roll `kind`; with probability `per_mille`, sleep [min, max] ms and log it.
t_maybe_delay() { # <kind> <per_mille> <min> <max> <detail>
    t_roll "$1"
    [ "$t_roll" -lt "$2" ] || return 1
    local ms
    ms=$(t_ms "$1" "$3" "$4")
    t_log "$1" "delay=${ms}ms" "$5"
    t_sleep "$ms"
    return 0
}
