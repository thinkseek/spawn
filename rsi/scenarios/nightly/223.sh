#!/bin/sh
# Black box: retiring an objective as already on main (`--kind on-main`)
# needs the gate's own record of its scenario passing on main; the kind is
# required on every retirement (objective 223, the root fix for the 9/61
# retire of 2026-11-18 15:04).
#
# The problem. `spawn objective retire` (objective 210) takes a free-text
# reason and nothing checks a claim that the work is on main. The
# supervisor retired 9/61 "already on main" on the strength of the owner's
# contract landing title (87315e67) while 061.sh was red on main. No
# un-retire exists, so the objective was closed with its work undone.
#
# Required:
# - `spawn objective retire --mission <m> --objective <n> --kind
#   on-main|renumbered|obsolete --by <name> --reason <text>`: --kind is
#   required, its value one of the three, and kept in the record
#   (`objective.retired` {id, objective, kind, by, reason, caller}).
# - kind on-main is recorded only when the store holds an accepted
#   `task.gated` of a task of the same mission that then landed, whose
#   `passed` or `reused` names the objective's scenario `<NNN>.sh` (the
#   number at least three digits), and no later `task.gated` of the
#   mission names it in `main_red_scenarios`. Otherwise it is refused,
#   recording nothing, with:
#     objective <m>/<n> is not shown on main: no accepted gate of a landed
#     task of mission <m> lists <NNN>.sh as passed (a landing's title is not
#     a gate record); run its scenario through a gate, or retire it as
#     renumbered or obsolete
#   or, with a red since:
#     objective <m>/<n> is not shown on main: <NNN>.sh is red on main since
#     task #<t>'s gate (<that gate's base>); run its scenario through a
#     gate, or retire it as renumbered or obsolete
# - renumbered and obsolete need no record. 210's refusals and callers are
#   unchanged (210.sh, with --kind).
#
# The world: two objective missions, M and N, each on its own source, one
# resident each, M with workers 3. Each task is delivered and reported by
# the scenario, its branch holding case.txt, which is the verdict a
# recording gate (SPAWN_GATE) writes as it is. `spawn integrate --once`
# passes assign, judge and land. Objectives 9, 12, 13, 1234, 20 and 21 of
# M are never assigned (not in its backlog), as a renumbered objective's.
#   (k) Before any gate: retire M/20 without --kind, and with --kind
#       landed: each exits non-zero and records nothing.
#   (0) Before any gate: retire M/9 --kind on-main is refused with the
#       first text.
#   (1) M's T1 (objective 1): rejected, its verdict's passed [009.sh] and
#       main_red_scenarios [014.sh] (R2, rev-j);
#       M/9 on-main still refused (a rejected candidate's pass). Then T1
#       again: accepted with passed [001.sh, 1009.sh, 015.sh] and landed; M/9
#       on-main still refused (that landed task's pass of 009.sh was in a
#       rejected gate, and 1009.sh is another scenario).
#   (2) N's task (N's objective 1): accepted with passed [009.sh], landed
#       on N's source. M/9 on-main still refused (another mission's gate).
#   (3) M's T2 (objective 2), its branch made after T1 landed: accepted
#       with passed [009.sh, 1234.sh, 014.sh] and reused [012.sh, 013.sh],
#       landed. M/9 on-main: exit 0, the record exactly {objective.retired,
#       M, 9, kind on-main, by, reason, caller owner}; M/1234 (scenario
#       1234.sh) on-main: exit 0; (R2, rev-j) M/14 on-main: exit 0, T1's
#       red on 014.sh being older than T2's pass.
#   (4) M's T3 (objective 3), its branch made after T2 landed: rejected
#       with main_red_scenarios [012.sh, 015.sh]. M/12 on-main is refused
#       with the second text, naming T3 and the base its task.gated names;
#       M/13 (reused, not red) on-main: exit 0; (R3, rev-j's F1) M/15
#       (passed by T1, then red) is refused with the second text.
#   (4b) (R3) T3 again, accepted with passed [015.sh, 016.sh] and (R4,
#       rev-b's F1) main_red_scenarios [016.sh], landed: M/15 on-main exits
#       0 (the newest pass is after the red); M/16 on-main exits 0 (the gate
#       that repairs a red scenario names it in both lists).
#   (5) M/20 --kind renumbered and M/21 --kind obsolete, no record of
#       either: exit 0, the records naming their kinds.
#   In the end exactly 8 objective.retired are recorded (9, 1234, 14, 13,
#   15, 16, 20, 21), in that order.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# On main 7bb9688e it exits 1: a retire without --kind is recorded (k), and
# --kind is an unknown flag, so every other retire exits non-zero. The
# refusals at (k) (--kind landed), (0), (1), (2) and (4) fail on their
# texts, and the retires at (3), (4) and (5) fail.

set -eu

asserted=""
harness() {
    echo "223: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od git sed grep cut curl; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
[ -n "$WORK" ] && [ -d "$WORK" ] || harness "mktemp gave no directory"
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_223_onmain_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL
HOME=$WORK/home
SPAWN_CLAUDE_PROJECTS=$WORK/home/projects
export HOME SPAWN_CLAUDE_PROJECTS
mkdir -p "$SPAWN_CLAUDE_PROJECTS" || harness "mkdir $SPAWN_CLAUDE_PROJECTS failed"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
XDG_RUNTIME_DIR=$WORK/runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
serve_pid=""
cleanup() {
    [ -z "$serve_pid" ] || kill -KILL "$serve_pid" 2>/dev/null || true
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        echo "223: the gate runs:" >&2
        cat "$WORK/judged.log" >&2 2>/dev/null || true
        echo "223: the integrator's last lines:" >&2
        grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 20 >&2 || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "223: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

: >"$WORK/failures"
check_failed() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL \
    SPAWN_GATE_MIN_FREE_GB SPAWN_GATE_JOBS SPAWN_GATE_WORKER_CORES SPAWN_GATE_MIN_FREE_MEMORY_MB \
    SPAWN_INTEGRATE_GATES SPAWN_TEST_DATABASE_LOG SPAWN_INTEGRATE_DRAIN_SECS SPAWN_GATE_ENV_FILE \
    SPAWN_INTEGRATE_WARM SPAWN_INTEGRATE_SHADOW SPAWN_GATE_RUNNER; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_WORKSPACE_ROOT=$WORK/ws
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT
mkdir -p "$SPAWN_WORKSPACE_ROOT"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

note() { echo "223: $*" >&2; }
q() { scalar "$1"; }
# The control records of one type, oldest first, one JSON object a line.
records() { # <type>
    sql "select kind -> 'detail' -> 'record' from spawn_events
          where session_id = 'session_control' and event_name = 'control.recorded'
            and kind -> 'detail' -> 'record' ->> 'type' = '$1' order by session_seq"
}
count_records() { # <type>
    q "select count(*) from spawn_events
        where session_id = 'session_control' and event_name = 'control.recorded'
          and kind -> 'detail' -> 'record' ->> 'type' = '$1'"
}


# --- the world -------------------------------------------------------------------------

cat >"$WORK/gitconfig" <<'GITEOF'
[user]
	name = bb
	email = bb@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
GITEOF
GIT_CONFIG_GLOBAL=$WORK/gitconfig
export GIT_CONFIG_GLOBAL

source_repo() { # <name>: a bare source with one commit on main
    git init -q --initial-branch=main "$WORK/seed-$1"
    printf 'base\n' >"$WORK/seed-$1/README.md"
    git -C "$WORK/seed-$1" add -A
    git -C "$WORK/seed-$1" commit -qm base
    git init -q --bare --initial-branch=main "$WORK/$1.git"
    git -C "$WORK/seed-$1" push -q "$WORK/$1.git" main:main
}
source_repo m
source_repo n

gate=$WORK/ctl/rsi/scenarios
mkdir -p "$gate"
for n in 001 002 003; do printf '#!/bin/sh\nexit 0\n' >"$gate/$n.sh"; done
printf '# nothing protected\n' >"$gate/protected"
: >"$gate/no-test-database"
: >"$WORK/judged.log"
: >"$WORK/integrate.log"
mkdir -p "$WORK/cache" "$WORK/integrator"

# The recording gate: the candidate's case.txt is its verdict, as written.
cat >"$WORK/fake-gate.sh" <<GATEEOF
#!/bin/sh
verdict=\$(git --git-dir="\$1" cat-file -p "\$3:case.txt" 2>/dev/null || echo '{"verdict":"rejected","reason":"no case.txt"}')
printf '%s %s %s\n' "\$(date +%s)" "\$3" "\$verdict" >>'$WORK/judged.log'
echo "| verdict \$verdict"
printf '%s\n' "\$verdict" >"\$SPAWN_GATE_VERDICT"
exit 0
GATEEOF
chmod 755 "$WORK/fake-gate.sh"

objective() { # <backlog dir> <n>
    mkdir -p "$1"
    printf '{"id": %s, "title": "Made-up objective %s", "prompt": "Made-up work %s."}\n' "$2" "$2" "$2" \
        >"$1/$2.json"
}
for n in 1 2 3; do objective "$WORK/backlog-m" "$n"; done
objective "$WORK/backlog-n" 1

add_mission() { # <name> <resident> <workers>: prints the id
    capture "$SPAWN_BIN" resident propose --name "$2" --charter "Owns the 223 objectives of $1."
    [ "$status" -eq 0 ] || harness "resident propose $2 failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$2"
    [ "$status" -eq 0 ] || harness "resident approve $2 failed: $(err)"
    capture "$SPAWN_BIN" mission add --text "223 objectives $1" --source "$WORK/$1.git"
    [ "$status" -eq 0 ] || harness "mission add $1 failed: $(err)"
    id=$(jq -r .mission "$WORK/out.txt")
    [ -n "$id" ] && [ "$id" != null ] || harness "mission add $1 printed no id: $(out)"
    capture "$SPAWN_BIN" mission configure --id "$id" --gate "$gate" --accept gate \
        --backlog "$WORK/backlog-$1" --resident "$2" --workers "$3"
    [ "$status" -eq 0 ] || harness "mission configure $1 exited $status: $(err)"
    echo "$id"
}
mission=$(add_mission m rm 3)
other=$(add_mission n rn 1)

pass() { # one `spawn integrate --once`
    capture env -u DBUS_SESSION_BUS_ADDRESS -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        XDG_CACHE_HOME="$WORK/cache" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        SPAWN_GATE_JOBS=1 SPAWN_GATE_WORKER_CORES=0 SPAWN_INTEGRATE_WARM=0 \
        "$SPAWN_BIN" integrate --root "$WORK/integrator" --once
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || harness "integrate --once exited $status: $(err | tail -n 3 | tr '\n' ' ')"
}
task_of() { # <mission> <objective>: its lowest task, or empty
    q "select coalesce(min(id)::text, '') from mission_tasks where mission_id = $1 and objective = $2"
}
state_of() { q "select state from mission_tasks where id = $1"; }
tasks_line() {
    sql "select string_agg('#' || id || ' m' || mission_id || ' o' || coalesce(objective::text, '-') || ' ' || state, ', ' order by id)
           from mission_tasks"
}
# Deliver and report task <id> from <source>, its branch holding case.txt =
# <verdict>, made from the source's main as it is now (a revised task's
# branch is replaced).
deliveries=0
work() { # <task> <source name> <verdict JSON>
    deliveries=$((deliveries + 1))
    key="k$1-$deliveries"
    agent=$(q "select agent_id from mission_tasks where id = $1")
    session=$(q "select session_id from mission_tasks where id = $1")
    capture "$SPAWN_BIN" submit --agent "$agent" --session "$session" --key "$key" \
        --action '{"type":"start","engine":"claude","prompt":"made-up work"}'
    [ "$status" -eq 0 ] || harness "submit (task $1) failed: $(err)"
    rm -rf "$WORK/w$1"
    git clone -q "$WORK/$2.git" "$WORK/w$1"
    printf '%s\n' "$3" >"$WORK/w$1/case.txt"
    git -C "$WORK/w$1" add -A
    git -C "$WORK/w$1" commit -qm "case of task $1"
    git -C "$WORK/w$1" push -q -f origin "HEAD:refs/heads/spawn/$agent"
    command=$(q "select id from spawn_commands where idempotency_key = '$key'")
    [ "$(q "select spawn_task_delivered($1, $command)")" = t ] || harness "task $1 not delivered"
    [ "$(q "select spawn_task_reported($1, 'done')")" = t ] || harness "task $1 not reported"
}
until_state() { # <task> <state> <what>: passes until the task is in <state>
    passes=0
    until [ "$(state_of "$1")" = "$2" ]; do
        [ "$passes" -lt 6 ] || harness "$3: task #$1 is $(state_of "$1") after 6 passes, not $2 (tasks: $(tasks_line))"
        pass
        passes=$((passes + 1))
    done
}

# --- retiring ---------------------------------------------------------------------------

retire() { # <objective> <kind or -, no flag> <reason>: of mission M, by the owner
    if [ "$2" = - ]; then
        capture "$SPAWN_BIN" objective retire --mission "$mission" --objective "$1" --by "bb 223" --reason "$3"
    else
        capture "$SPAWN_BIN" objective retire --mission "$mission" --objective "$1" --kind "$2" \
            --by "bb 223" --reason "$3"
    fi
}
refused() { # <check> <objective> <kind> <text the refusal holds, or empty>
    before=$(count_records objective.retired)
    retire "$2" "$3" "made-up reason: its work is on main"
    if [ "$status" -eq 0 ]; then
        check_failed "$1 retire of $mission/$2 (kind $3) exited 0: $(out | head -c 200)"
    elif [ -n "$4" ] && ! grep -qF -- "$4" "$WORK/err.txt"; then
        check_failed "$1 retire of $mission/$2 (kind $3) was refused without [$4]: $(err | head -c 400)"
    fi
    [ "$(count_records objective.retired)" -eq "$before" ] ||
        check_failed "$1 retire of $mission/$2 (kind $3) recorded: $(records objective.retired | tail -n 1 | head -c 300)"
}
no_record() { # <objective> <NNN.sh>
    printf 'objective %s/%s is not shown on main: no accepted gate of a landed task of mission %s lists %s as passed (a landing'"'"'s title is not a gate record); run its scenario through a gate, or retire it as renumbered or obsolete' \
        "$mission" "$1" "$mission" "$2"
}
retired() { # <check> <objective> <kind> <reason>: exit 0 and the exact record
    retire "$2" "$3" "$4"
    if [ "$status" -ne 0 ]; then
        check_failed "$1 retire of $mission/$2 --kind $3 exited $status: $(err | head -c 400)"
        return 0
    fi
    rec=$(records objective.retired | tail -n 1)
    want=$(jq -cn --argjson m "$mission" --argjson o "$2" --arg k "$3" --arg r "$4" \
        '{type: "objective.retired", id: $m, objective: $o, kind: $k, by: "bb 223", reason: $r, caller: "owner"}')
    [ "$(printf '%s' "$rec" | jq -cS .)" = "$(printf '%s' "$want" | jq -cS .)" ] ||
        check_failed "$1 the record of $mission/$2: [$rec], expected [$want]"
}

passes=0
until [ -n "$(task_of "$mission" 1)" ] && [ -n "$(task_of "$mission" 2)" ] && [ -n "$(task_of "$mission" 3)" ] &&
    [ -n "$(task_of "$other" 1)" ]; do
    [ "$passes" -lt 6 ] || harness "the four tasks were not assigned in 6 passes (tasks: $(tasks_line))"
    pass
    passes=$((passes + 1))
done
t1=$(task_of "$mission" 1)
t2=$(task_of "$mission" 2)
t3=$(task_of "$mission" 3)
tn=$(task_of "$other" 1)

# (k) the kind is required, and one of three.
refused "(k)" 20 - ""
refused "(k)" 20 landed "kind is on-main, renumbered or obsolete"
# (0) no gate at all.
refused "(0)" 9 on-main "$(no_record 9 009.sh)"
note "(k), (0) checked"

# (1) a rejected candidate's pass.
work "$t1" m '{"verdict":"rejected","reason":"scenario fails on the candidate","passed":["009.sh"],"main_red_scenarios":["014.sh"]}'
until_state "$t1" revise "(1)"
got=$(q "select coalesce(kind -> 'detail' -> 'record' ->> 'passed', '') from spawn_events
          where session_id = 'session_control' and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
            and kind -> 'detail' -> 'record' -> 'id' = to_jsonb($t1::bigint)")
[ "$got" = '["009.sh"]' ] || harness "(1) task #$t1's task.gated passed: [$got], expected [[\"009.sh\"]]"
refused "(1)" 9 on-main "$(no_record 9 009.sh)"
# T1 again: accepted without 009.sh (1009.sh only looks like it), landed.
# Its rejected gate's pass still shows nothing.
work "$t1" m '{"verdict":"accepted","passed":["001.sh","1009.sh","015.sh"]}'
until_state "$t1" landed "(1)"
refused "(1)" 9 on-main "$(no_record 9 009.sh)"
note "(1) checked"

# (2) another mission's landed gate.
work "$tn" n '{"verdict":"accepted","passed":["009.sh"]}'
until_state "$tn" landed "(2)"
refused "(2)" 9 on-main "$(no_record 9 009.sh)"
note "(2) checked"

# (3) M's own landed gate.
work "$t2" m '{"verdict":"accepted","passed":["009.sh","1234.sh","014.sh"],"reused":["012.sh","013.sh"]}'
until_state "$t2" landed "(3)"
retired "(3)" 9 on-main "009.sh passed in task #$t2's gate"
retired "(3)" 1234 on-main "1234.sh passed in task #$t2's gate"
# R2 (rev-j): T1's gate found main red on 014.sh before T2's passed it.
retired "(3)" 14 on-main "014.sh passed in task #$t2's gate, after T1's red"
note "(3) checked"

# (4) a gate since that finds main red on 012.sh; T3's branch is made from
# main after T2's landing.
work "$t3" m '{"verdict":"rejected","reason":"scenario fails on the candidate","main_red_scenarios":["012.sh","015.sh"]}'
until_state "$t3" revise "(4)"
base3=$(q "select coalesce(kind -> 'detail' -> 'record' ->> 'base', '') from spawn_events
            where session_id = 'session_control' and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
              and kind -> 'detail' -> 'record' -> 'id' = to_jsonb($t3::bigint)
              and kind -> 'detail' -> 'record' -> 'main_red_scenarios' ? '012.sh'")
[ -n "$base3" ] || harness "(4) task #$t3 has no task.gated naming 012.sh red on main"
refused "(4)" 12 on-main "objective $mission/12 is not shown on main: 012.sh is red on main since task #$t3's gate ($base3); run its scenario through a gate, or retire it as renumbered or obsolete"
retired "(4)" 13 on-main "013.sh reused in task #$t2's gate"
# R3 (rev-j's F1): 015.sh passed (T1), then red (T3), and now refused.
refused "(4)" 15 on-main "objective $mission/15 is not shown on main: 015.sh is red on main since task #$t3's gate ($base3)"
note "(4) checked"

# (4b) R3 (rev-j's F1): T3 again, accepted with passed [015.sh], landed:
# the newest pass is after the red.
work "$t3" m '{"verdict":"accepted","passed":["015.sh","016.sh"],"main_red_scenarios":["016.sh"]}'
until_state "$t3" landed "(4b)"
retired "(4b)" 15 on-main "015.sh passed again in task #$t3's gate"
# R4 (rev-b's F1): T3's gate repairs 016.sh: red on its base, passing on
# the candidate, both named in the one gate; landed, it shows it.
retired "(4b)" 16 on-main "016.sh repaired by task #$t3's gate"
note "(4b) checked"

# (5) renumbered and obsolete need no record.
retired "(5)" 20 renumbered "its work is objective 2 in this made-up backlog"
retired "(5)" 21 obsolete "no longer wanted"
got=$(records objective.retired | jq -s -c 'map("\(.objective) \(.kind)")')
want='["9 on-main","1234 on-main","14 on-main","13 on-main","15 on-main","16 on-main","20 renumbered","21 obsolete"]'
[ "$got" = "$want" ] || check_failed "(5) the objective.retired records: $got, expected $want"
note "(5) checked"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "223: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "223: a retire as on main needs an accepted, landed gate of the mission that saw its scenario pass, with no red since; every retire names its kind"
exit 0
