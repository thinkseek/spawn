#!/bin/sh
# Black box: a gate that rejects a task because check-tests.sh fails names,
# where the reader sees the rejection, each test that failed (GitHub #28).
#
# The problem: when check-tests.sh fails on a candidate, the task's last
# gate and the note sent back say only "regression: check-tests.sh fails on
# the candidate". Which test failed is in the gate's output, which the
# record keeps in full, but the reader (the task page, the worker reading
# the note, the integrator's line) has to dig it out of a 60-line tail.
#
# Required: the record of such a rejection names each failing test
# check-tests.sh printed, as "check-tests.sh failed: <test>", the first five
# and then "and <n> more", followed by the gate's own reason in brackets.
# The note quotes that reason. Any other rejection's reason is unchanged.
# So is the gate's own verdict, in its output.
#
# The world: three gated missions, each on its own source, and one task
# each. A recording gate (SPAWN_GATE) rejects each candidate as the case it
# holds says (case.txt):
#   two:   check-tests.sh fails, its output printing two FAILED lines;
#   seven: check-tests.sh fails with seven FAILED lines;
#   other: 046.sh fails (no check-tests output).
# One `spawn integrate --once` pass judges all three. `spawn serve` answers
# the Missions page's reads.
#   (1) /api/missions: the "two" task's last_gate is rejected, its reason
#       exactly "check-tests.sh failed: bb::alpha::adds_up; check-tests.sh
#       failed: bb::beta::keeps_order (regression: check-tests.sh fails on
#       the candidate)".
#   (2) /api/missions/task/<id>: its review_note begins "The gate rejected
#       this work: <that reason>.".
#   (3) the "seven" task's reason names bb::t::case_1 to bb::t::case_5,
#       then "; and 2 more (regression: check-tests.sh fails on the
#       candidate)", and neither case_6 nor case_7.
#   (4) the "other" task's reason is exactly "regression: 046.sh fails on
#       the candidate".
#   (5) the gate's own verdict line, kept in the record's gate output (the
#       note's tail), still reads "regression: check-tests.sh fails on the
#       candidate", as the gate wrote it.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "129: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od git sed grep curl cut; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_129_named_$run"
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
: >"$WORK/pids"
cleanup() {
    while read -r pid; do
        kill -KILL "$pid" 2>/dev/null || true
    done <"$WORK/pids"
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    set +e
    if [ "$rc" -ne 0 ]; then
        echo "129: the gate runs:" >&2
        cat "$WORK/judged.log" >&2 2>/dev/null || true
        echo "129: the integrator's last lines:" >&2
        grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 20 >&2 || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "129: harness: unexpected exit $rc" >&2
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

note() { echo "129: $*" >&2; }
q() { scalar "$1"; }

# --- the world -------------------------------------------------------------------------

cat >"$WORK/gitconfig" <<'EOF'
[user]
	name = bb
	email = bb@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
EOF
GIT_CONFIG_GLOBAL=$WORK/gitconfig
export GIT_CONFIG_GLOBAL

gate=$WORK/ctl/rsi/scenarios
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
: >"$gate/no-test-database"
: >"$WORK/judged.log"
: >"$WORK/integrate.log"
mkdir -p "$WORK/cache"

# The recording gate: by the candidate's case.txt, check-tests.sh fails
# with two or seven FAILED lines (as check-tests.sh prints them, at the end
# of its section), or 046.sh fails.
REGRESSION='regression: check-tests.sh fails on the candidate'
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
case=\$(git --git-dir="\$1" cat-file -p "\$3:case.txt" 2>/dev/null || echo none)
printf '%s %s %s\n' "\$(date +%s)" "\$case" "\$3" >>'$WORK/judged.log'
echo "== candidate: build"
echo "| build: ok"
echo "| stage gate ok 900ms candidate: build"
case \$case in
two | seven)
    echo "| == regression: check-tests.sh must still pass"
    echo "| running 40 tests"
    if [ "\$case" = two ]; then
        echo "| FAILED bb::alpha::adds_up: assertion \\\`left == right\\\` failed: made-up sum"
        echo "| FAILED bb::beta::keeps_order: made-up order lost"
    else
        for n in 1 2 3 4 5 6 7; do echo "| FAILED bb::t::case_\$n: made-up failure \$n"; done
    fi
    echo "| stage gate rejected 1200ms regression: check-tests.sh must still pass"
    printf '{"verdict":"rejected","reason":"$REGRESSION"}\n' >"\$SPAWN_GATE_VERDICT"
    echo '{"verdict":"rejected","reason":"$REGRESSION"}'
    ;;
*)
    echo "| == regression: 046.sh must still pass"
    echo "| 046: made-up failure"
    echo "| stage gate rejected 300ms regression: 046.sh must still pass"
    printf '{"verdict":"rejected","reason":"regression: 046.sh fails on the candidate"}\n' >"\$SPAWN_GATE_VERDICT"
    echo '{"verdict":"rejected","reason":"regression: 046.sh fails on the candidate"}'
    ;;
esac
exit 0
EOF
chmod 755 "$WORK/fake-gate.sh"

# One gated mission per case, on its own source, with one reported task
# whose branch holds case.txt.
make_case() { # <case>: prints the task id
    mc=$1
    src=$WORK/$mc.git
    seed=$WORK/seed-$mc
    git init -q --initial-branch=main "$seed"
    printf 'base\n' >"$seed/README.md"
    git -C "$seed" add -A
    git -C "$seed" commit -qm base
    git init -q --bare --initial-branch=main "$src"
    git -C "$seed" push -q "$src" main:main
    capture "$SPAWN_BIN" mission add --text "129 $mc" --source "$src" --gate "$gate" --accept gate
    [ "$status" -eq 0 ] || harness "mission add ($mc) failed: $(err)"
    mission=$(jq -r .mission "$WORK/out.txt")
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "w$mc" --brief "work of $mc"
    [ "$status" -eq 0 ] || harness "task assign ($mc) failed: $(err)"
    mc_task=$(jq -r .task "$WORK/out.txt")
    agent=$(q "select agent_id from mission_tasks where id = $mc_task")
    session=$(q "select session_id from mission_tasks where id = $mc_task")
    capture "$SPAWN_BIN" submit --agent "$agent" --session "$session" --key "k$mc" \
        --action '{"type":"start","engine":"claude","prompt":"made-up work"}'
    [ "$status" -eq 0 ] || harness "submit ($mc) failed: $(err)"
    git -C "$seed" checkout -q -b work
    printf '%s\n' "$mc" >"$seed/case.txt"
    git -C "$seed" add -A
    git -C "$seed" commit -qm "$mc"
    git -C "$seed" push -q "$src" "work:refs/heads/spawn/$agent"
    command=$(q "select id from spawn_commands where idempotency_key = 'k$mc'")
    [ "$(q "select spawn_task_delivered($mc_task, $command)")" = t ] || harness "task $mc_task not delivered"
    [ "$(q "select spawn_task_reported($mc_task, 'done')")" = t ] || harness "task $mc_task not reported"
    echo "$mc_task"
}
t_two=$(make_case two)
t_seven=$(make_case seven)
t_other=$(make_case other)

mkdir -p "$WORK/integrator"
capture env -u DBUS_SESSION_BUS_ADDRESS -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
    XDG_CACHE_HOME="$WORK/cache" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
    SPAWN_GATE_JOBS=1 SPAWN_GATE_WORKER_CORES=0 SPAWN_INTEGRATE_WARM=0 \
    "$SPAWN_BIN" integrate --root "$WORK/integrator" --once
cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
[ "$status" -eq 0 ] || harness "integrate --once exited $status: $(err | tail -n 3 | tr '\n' ' ')"
[ "$(grep -c . "$WORK/judged.log")" -eq 3 ] ||
    harness "the gate ran $(grep -c . "$WORK/judged.log") times, not 3: $(tr '\n' ' ' <"$WORK/judged.log")"

# --- what the reader sees ------------------------------------------------------------------

web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
"$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" >"$WORK/serve.log" 2>&1 &
echo "$!" >>"$WORK/pids"
port=""
i=0
while [ "$i" -lt 100 ]; do
    i=$((i + 1))
    port=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$WORK/serve.log" 2>/dev/null | head -n 1)
    [ -n "$port" ] && break
    sleep 0.1
done
[ -n "$port" ] || harness "serve did not start: $(head -c 400 "$WORK/serve.log")"
get() { # <path> <out>
    code=$(curl -s --noproxy '*' -o "$2" -w '%{http_code}' "http://127.0.0.1:$port$1" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET $1 answered $code"
}
get /api/missions "$WORK/missions.json"
gate_of() { jq -c --argjson id "$1" '[.missions[].tasks[] | select(.id == $id) | .last_gate][0] // null' "$WORK/missions.json"; }
reason_of() { jq -r --argjson id "$1" '[.missions[].tasks[] | select(.id == $id) | .last_gate.reason][0] // ""' "$WORK/missions.json"; }

two_want="check-tests.sh failed: bb::alpha::adds_up; check-tests.sh failed: bb::beta::keeps_order ($REGRESSION)"
[ "$(gate_of "$t_two" | jq -r '.verdict // ""')" = rejected ] ||
    check_failed "(1) task $t_two's last gate is not rejected: $(gate_of "$t_two" | head -c 300)"
got=$(reason_of "$t_two")
[ "$got" = "$two_want" ] ||
    check_failed "(1) task $t_two's last gate reason: expected [$two_want], got [$got]"
note "(1) checked: the last gate names both failing tests"

get "/api/missions/task/$t_two" "$WORK/task-two.json"
note_text=$(jq -r '.review_note // ""' "$WORK/task-two.json")
case $note_text in
"The gate rejected this work: $two_want."*) ;;
*) check_failed "(2) task $t_two's review note does not begin [The gate rejected this work: $two_want.]: [$(printf '%s' "$note_text" | head -c 300 | tr '\n' ' ')]" ;;
esac
note "(2) checked: the note sent back names them"

got=$(reason_of "$t_seven")
seven_want="check-tests.sh failed: bb::t::case_1; check-tests.sh failed: bb::t::case_2; check-tests.sh failed: bb::t::case_3; check-tests.sh failed: bb::t::case_4; check-tests.sh failed: bb::t::case_5; and 2 more ($REGRESSION)"
[ "$got" = "$seven_want" ] ||
    check_failed "(3) task $t_seven's last gate reason: expected [$seven_want], got [$got]"
note "(3) checked: past five, \"and <n> more\""

got=$(reason_of "$t_other")
[ "$got" = "regression: 046.sh fails on the candidate" ] ||
    check_failed "(4) task $t_other's last gate reason: expected [regression: 046.sh fails on the candidate], got [$got]"
note "(4) checked: another scenario's rejection is as the gate gave it"

case $note_text in
*"{\"verdict\":\"rejected\",\"reason\":\"$REGRESSION\"}"*) ;;
*) check_failed "(5) task $t_two's note does not quote the gate's own verdict line {\"verdict\":\"rejected\",\"reason\":\"$REGRESSION\"}" ;;
esac
note "(5) checked: the gate's own verdict is as it wrote it"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "129: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "129: a check-tests rejection names each failing test where the reader sees it"
exit 0
