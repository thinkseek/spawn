#!/bin/sh
# Black box: a gate that rejects a task because a scenario's run fails
# names, where the reader sees the rejection, the first check that run
# failed and how many it failed (objective 169, GitHub #28; 129 did it for
# check-tests.sh alone).
#
# The problem: a rejection for any scenario but check-tests.sh reads only
# "scenario fails on the candidate" or "regression: <scenario> fails on the
# candidate" on the task's last gate, in the note and in the record. The
# failing check is the scenario's `blackbox: ` line in the gate's output,
# which the record keeps but nobody reads.
#
# Required: the record of such a rejection has `failed`: {scenario, first,
# count}: the scenario of the gate's deciding stage line, its first
# `blackbox: ` line and how many it printed. Its reason reads
# "<scenario> failed: <first>", then "; and <n> more" when there are more,
# then the gate's reason in brackets. A quoted line is masked (credential
# shapes become [redacted]) and cut to 200 chars. A run that printed no
# blackbox line is as before.
#
# The world: three gated missions, one task each. A recording gate
# (SPAWN_GATE) rejects each candidate as its case.txt says:
#   own:    the task's own scenario 149.sh fails on one blackbox line, after
#           an earlier regression section that passed with a blackbox line
#           of its own (not the deciding run's);
#   masked: 092.sh fails on two blackbox lines, the first holding a GitHub
#           token and a PGPASSWORD= value;
#   none:   046.sh fails, printing no blackbox line;
#   long:   093.sh fails on one blackbox line of 214 chars, its 200th char
#           a two-byte "é" (bytes 200 and 201 of the check text).
# One `spawn integrate --once` pass judges all three; `spawn serve` answers
# the Missions page's reads.
#   (1) /api/missions: "own"'s last_gate is rejected, its reason exactly
#       "149.sh failed: (2) the checkout has no made-up.service (scenario
#       fails on the candidate)"; /api/missions/task/<id>'s review_note
#       begins "The gate rejected this work: <that reason>.".
#   (2) "own"'s task.gated record has failed {"scenario":"149.sh","first":
#       "(2) the checkout has no made-up.service","count":1}.
#   (3) "masked"'s reason and failed.first hold [redacted] and neither the
#       token nor the password; the reason says "; and 1 more"; failed.count
#       is 2 and failed.scenario 092.sh.
#   (4) "none"'s reason is exactly "regression: 046.sh fails on the
#       candidate", and its record has no failed.
#   (5) "long"'s failed.first is exactly the check's first 200 chars (the
#       "é" whole, nothing after it), and its reason is "093.sh failed:
#       <those 200 chars> (regression: 093.sh fails on the candidate)"
#       (rev-g's F1 on R1: the cap, at a char boundary).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "169: harness: $*" >&2
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
SPAWN_DB_NAME="bb_169_checks_$run"
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
    if [ "$rc" -ne 0 ]; then
        echo "169: the gate runs:" >&2
        cat "$WORK/judged.log" >&2 2>/dev/null || true
        echo "169: the integrator's last lines:" >&2
        grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 20 >&2 || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "169: harness: unexpected exit $rc" >&2
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

note() { echo "169: $*" >&2; }
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

# The recording gate: by the candidate's case.txt, a scenario's run fails
# as the header says, its lines `| `-prefixed as gate.sh prints them.
# The long check: "(5) long: " (10 chars), 189 x, then "é" five times and
# " end": 214 chars, char 200 the first "é".
LONG_HEAD="(5) long: $(printf '%189s' '' | tr ' ' x)"
LONG_CHECK="${LONG_HEAD}ééééé end"
LONG_WANT="${LONG_HEAD}é"
TOKEN=ghp_169madeup0123456789abcdefghijklmnopqrs
PASSWORD=hunter169hunter169
cat >"$WORK/fake-gate.sh" <<EOF2
#!/bin/sh
case=\$(git --git-dir="\$1" cat-file -p "\$3:case.txt" 2>/dev/null || echo none)
printf '%s %s %s\n' "\$(date +%s)" "\$case" "\$3" >>'$WORK/judged.log'
echo "== candidate: build"
echo "| build: ok"
echo "| stage gate ok 900ms candidate: build"
case \$case in
own)
    echo "| == regression: 012.sh must still pass"
    echo "| blackbox: (9) not the deciding run's line"
    echo "| stage gate ok 700ms regression: 012.sh must still pass"
    echo "| == candidate: 149.sh must pass"
    echo "| 149: (1) checked: made-up"
    echo "| blackbox: (2) the checkout has no made-up.service"
    echo "| 149: the stand-in's last requests:"
    echo "| stage gate rejected 2323ms candidate: 149.sh must pass"
    reason='scenario fails on the candidate'
    ;;
masked)
    echo "| == regression: 092.sh must still pass"
    echo "| blackbox: (3) curl -H 'Authorization: Bearer $TOKEN' failed with PGPASSWORD=$PASSWORD set"
    echo "| blackbox: (4) a second made-up check"
    echo "| stage gate rejected 400ms regression: 092.sh must still pass"
    reason='regression: 092.sh fails on the candidate'
    ;;
long)
    echo "| == regression: 093.sh must still pass"
    echo "| blackbox: $LONG_CHECK"
    echo "| stage gate rejected 500ms regression: 093.sh must still pass"
    reason='regression: 093.sh fails on the candidate'
    ;;
*)
    echo "| == regression: 046.sh must still pass"
    echo "| 046: made-up failure"
    echo "| stage gate rejected 300ms regression: 046.sh must still pass"
    reason='regression: 046.sh fails on the candidate'
    ;;
esac
echo "| stage gate rejected 5000ms total"
printf '{"verdict":"rejected","reason":"%s"}\n' "\$reason" >"\$SPAWN_GATE_VERDICT"
printf '{"verdict":"rejected","reason":"%s"}\n' "\$reason"
exit 0
EOF2
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
    capture "$SPAWN_BIN" mission add --text "169 $mc" --source "$src" --gate "$gate" --accept gate
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
t_own=$(make_case own)
t_masked=$(make_case masked)
t_none=$(make_case none)
t_long=$(make_case long)

mkdir -p "$WORK/integrator"
capture env -u DBUS_SESSION_BUS_ADDRESS -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
    XDG_CACHE_HOME="$WORK/cache" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
    SPAWN_GATE_JOBS=1 SPAWN_GATE_WORKER_CORES=0 SPAWN_INTEGRATE_WARM=0 \
    "$SPAWN_BIN" integrate --root "$WORK/integrator" --once
cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
[ "$status" -eq 0 ] || harness "integrate --once exited $status: $(err | tail -n 3 | tr '\n' ' ')"
[ "$(grep -c . "$WORK/judged.log")" -eq 4 ] ||
    harness "the gate ran $(grep -c . "$WORK/judged.log") times, not 4: $(tr '\n' ' ' <"$WORK/judged.log")"

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
# The `failed` of a task's rejected task.gated record, compact JSON ("" if none).
failed_of() {
    q "select coalesce((kind -> 'detail' -> 'record' -> 'failed')::text, '') from spawn_events
        where kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
          and kind -> 'detail' -> 'record' ->> 'verdict' = 'rejected'
          and (kind -> 'detail' -> 'record' ->> 'id')::bigint = $1
        order by id desc limit 1" | jq -c . 2>/dev/null || true
}

own_want="149.sh failed: (2) the checkout has no made-up.service (scenario fails on the candidate)"
[ "$(gate_of "$t_own" | jq -r '.verdict // ""')" = rejected ] ||
    check_failed "(1) task $t_own's last gate is not rejected: $(gate_of "$t_own" | head -c 300)"
got=$(reason_of "$t_own")
[ "$got" = "$own_want" ] ||
    check_failed "(1) task $t_own's last gate reason: expected [$own_want], got [$got]"
get "/api/missions/task/$t_own" "$WORK/task-own.json"
note_text=$(jq -r '.review_note // ""' "$WORK/task-own.json")
case $note_text in
"The gate rejected this work: $own_want."*) ;;
*) check_failed "(1) task $t_own's review note does not begin [The gate rejected this work: $own_want.]: [$(printf '%s' "$note_text" | head -c 300 | tr '\n' ' ')]" ;;
esac
note "(1) checked: the last gate and the note name the first failed check"

want='{"count":1,"first":"(2) the checkout has no made-up.service","scenario":"149.sh"}'
got=$(failed_of "$t_own" | jq -cS . 2>/dev/null || true)
[ "$got" = "$want" ] ||
    check_failed "(2) task $t_own's task.gated failed: expected [$want], got [$got]"
note "(2) checked: the record holds failed {scenario, first, count}"

got=$(reason_of "$t_masked")
first=$(failed_of "$t_masked" | jq -r '.first // ""' 2>/dev/null || true)
for text in "$got" "$first"; do
    case $text in
    *"$TOKEN"* | *"$PASSWORD"*) check_failed "(3) task $t_masked shows a credential unmasked: [$text]" ;;
    *"[redacted]"*) ;;
    *) check_failed "(3) task $t_masked's quoted check is not masked with [redacted]: [$text]" ;;
    esac
done
case $got in
"092.sh failed: (3) "*"; and 1 more (regression: 092.sh fails on the candidate)") ;;
*) check_failed "(3) task $t_masked's reason: expected [092.sh failed: (3) …; and 1 more (regression: 092.sh fails on the candidate)], got [$got]" ;;
esac
[ "$(failed_of "$t_masked" | jq -r '"\(.scenario) \(.count)"' 2>/dev/null)" = "092.sh 2" ] ||
    check_failed "(3) task $t_masked's failed: expected scenario 092.sh, count 2, got [$(failed_of "$t_masked")]"
note "(3) checked: a quoted line is masked, the rest counted"

got=$(reason_of "$t_none")
[ "$got" = "regression: 046.sh fails on the candidate" ] ||
    check_failed "(4) task $t_none's last gate reason: expected [regression: 046.sh fails on the candidate], got [$got]"
got=$(failed_of "$t_none")
[ -z "$got" ] || check_failed "(4) task $t_none's record has failed [$got]: a run that names no check has none"
note "(4) checked: a run that names no check is as before"

got=$(failed_of "$t_long" | jq -r '.first // ""' 2>/dev/null || true)
[ "$got" = "$LONG_WANT" ] ||
    check_failed "(5) task $t_long's failed.first: expected the check's first 200 chars [$LONG_WANT], got [$got]"
want="093.sh failed: $LONG_WANT (regression: 093.sh fails on the candidate)"
got=$(reason_of "$t_long")
[ "$got" = "$want" ] ||
    check_failed "(5) task $t_long's reason: expected [$want], got [$got]"
note "(5) checked: a long check is cut to 200 chars at a char boundary"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "169: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "169: a rejection names the first check its deciding run failed, masked, with the count"
exit 0
