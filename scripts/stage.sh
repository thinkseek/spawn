# Stage timing: one line format for every suite (the gate, the e2e test,
# the black-box scenarios), so a slow or failing stage reads the same in each:
#
#   stage <suite> <result> <ms>ms <name>
#
# Source this file (POSIX sh), then either
#   stage_open <suite> <name> ... stage_close <result>   (a stage named at its start)
# or
#   stage_mark <suite> <result> <name>                    (a stage that ends with what it showed,
#                                                          timed from the previous mark)
# stage_open closes a stage still open as `ok`. `STAGE_PREFIX` is printed
# before each line (the gate prefixes its log with "| "). With
# `SPAWN_STAGE_LOG` set, each stage is also appended there as one JSON line:
#   {"suite":"…","stage":"…","result":"…","ms":N}

stage_now() { date +%s%3N; }

stage_json() { printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'; }

stage_emit() { # <suite> <result> <ms> <name>
    printf '%sstage %s %s %sms %s\n' "${STAGE_PREFIX-}" "$1" "$2" "$3" "$4"
    if [ -n "${SPAWN_STAGE_LOG-}" ]; then
        printf '{"suite":"%s","stage":"%s","result":"%s","ms":%s}\n' \
            "$(stage_json "$1")" "$(stage_json "$4")" "$(stage_json "$2")" "$3" >>"$SPAWN_STAGE_LOG"
    fi
}

stage_open() { # <suite> <name>
    stage_close ok
    stage_suite=$1
    stage_name=$2
    stage_at=$(stage_now)
}

stage_close() { # <result>
    [ -n "${stage_name-}" ] || return 0
    stage_emit "$stage_suite" "$1" "$(($(stage_now) - stage_at))" "$stage_name"
    stage_name=
}

stage_mark() { # <suite> <result> <name>
    stage_time=$(stage_now)
    stage_emit "$1" "$2" "$((stage_time - ${stage_last:-$stage_time}))" "$3"
    stage_last=$stage_time
}

# Start the clock for stage_mark without emitting anything.
stage_start() { stage_last=$(stage_now); }
