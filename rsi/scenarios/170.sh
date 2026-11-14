#!/bin/sh
# Black box: what a gate rejection stores is masked (objective 170). The
# integrator's control records were stored as written: a gate's output that
# printed a token or a password (a failing curl, a psql line) reached the
# record's `log`, the note sent back to the worker (`review_note`), the
# task's gate log and the page.
#
# Required: every string of a control record the Rust side writes is masked
# with the store's credential shapes ([redacted]) except the fields the
# folds and readers match (ids, commits, verdicts, judging, …), which keep
# their bytes.
#
# The world: one gated mission with one reported task. A recording gate
# (SPAWN_GATE) rejects it: 092.sh fails, its output printing a GitHub token
# behind `Authorization: Bearer` and a PGPASSWORD= value, and its verdict's
# reason naming the token too. One `spawn integrate --once` pass judges it;
# `spawn serve` answers.
#   (1) The task.gated record's log, note and reason hold neither value,
#       and each holds [redacted].
#   (2) /api/missions/task/<id>'s review_note and the Missions page's
#       last_gate.reason hold neither value.
#   (3) The record keeps what the folds read: verdict rejected, its task id,
#       its judging, and the candidate commit in its run's inputs
#       (inputs.candidate, the commit the gate was called with); the task is
#       back in `revise`.
#   (4) The rest of the gate's output is as written: its "| stage gate
#       rejected 400ms regression: 092.sh must still pass" line is in log.
# Then, as deploy/on-landing.sh records a failed deploy (R2, rev-g's F1):
#   (5) `spawn mission deployed --head <c> --ok false --by landing --reason
#       <text with the token and PGPASSWORD=…>` exits 0, and the stored
#       deploy.finished reason holds neither value.
#   (6) The same with a reason of exactly 500 chars ending in a short
#       secret (" PWD=x"): it exits 0 (masking never makes the store refuse
#       it), and the stored reason is at most 500 chars and holds no
#       "PWD=x".
# And an owner's gated push whose gate gives no verdict, a host fault whose
# reason holds the password, twice (R2):
#   (7) Three `spawn integrate --once` passes judge it twice, never a third
#       time: the second no-verdict for the same reason refuses the request
#       (one source.push_refused), although its stored reason is masked; each
#       stored source.push_no_verdict reason holds [redacted] and not the
#       password.
#   (8) (R3, cc-astra's A73) The same with a fault whose formatted reason
#       ("the host's fault on check-tests.sh: …") is exactly 500 chars and
#       ends in a short secret (" PWD=x"): judged twice, never a third time,
#       one source.push_refused, and each stored reason is kept whole,
#       masked: longer than 500 chars (only the fields a store check limits
#       are cut), holding no "PWD=x".
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "170: harness: $*" >&2
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
SPAWN_DB_NAME="bb_170_masked_$run"
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
        echo "170: the gate runs:" >&2
        cat "$WORK/judged.log" >&2 2>/dev/null || true
        echo "170: the integrator's last lines:" >&2
        grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 20 >&2 || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "170: harness: unexpected exit $rc" >&2
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

note() { echo "170: $*" >&2; }
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

# The recording gate: one run of 092.sh fails, printing a token and a
# password, its lines `| `-prefixed as gate.sh prints them.
TOKEN=ghp_170madeup0123456789abcdefghijklmnopq
PASSWORD=hunter170hunter170
cat >"$WORK/fake-gate.sh" <<EOF2
#!/bin/sh
printf '%s %s\n' "\$(date +%s)" "\$3" >>'$WORK/judged.log'
echo "== candidate: build"
echo "| build: ok"
echo "| stage gate ok 900ms candidate: build"
echo "| == regression: 092.sh must still pass"
echo "| 092: curl -H 'Authorization: Bearer $TOKEN' https://api.example/x"
echo "| 092: PGPASSWORD=$PASSWORD psql -c 'select 1'"
echo "| stage gate rejected 400ms regression: 092.sh must still pass"
echo "| stage gate rejected 5000ms total"
reason="regression: 092.sh fails on the candidate (token $TOKEN)"
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
    capture "$SPAWN_BIN" mission add --text "170 $mc" --source "$src" --gate "$gate" --accept gate
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
t_one=$(make_case one)

mkdir -p "$WORK/integrator"
capture env -u DBUS_SESSION_BUS_ADDRESS -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
    XDG_CACHE_HOME="$WORK/cache" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
    SPAWN_GATE_JOBS=1 SPAWN_GATE_WORKER_CORES=0 SPAWN_INTEGRATE_WARM=0 \
    "$SPAWN_BIN" integrate --root "$WORK/integrator" --once
cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
[ "$status" -eq 0 ] || harness "integrate --once exited $status: $(err | tail -n 3 | tr '\n' ' ')"
[ "$(grep -c . "$WORK/judged.log")" -eq 1 ] ||
    harness "the gate ran $(grep -c . "$WORK/judged.log") times, not 1: $(tr '\n' ' ' <"$WORK/judged.log")"

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
reason_of() { jq -r --argjson id "$1" '[.missions[].tasks[] | select(.id == $id) | .last_gate.reason][0] // ""' "$WORK/missions.json"; }
record=$(q "select (kind -> 'detail' -> 'record')::text from spawn_events
             where kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
               and (kind -> 'detail' -> 'record' ->> 'id')::bigint = $t_one
             order by id desc limit 1")
[ -n "$record" ] || harness "no task.gated record for task $t_one"
leaks() { case $1 in *"$TOKEN"* | *"$PASSWORD"*) return 0 ;; esac; return 1; }

for key in log note reason; do
    text=$(printf '%s' "$record" | jq -r --arg k "$key" '.[$k] // ""')
    if leaks "$text"; then
        check_failed "(1) the task.gated record's $key holds a credential unmasked: [$(printf '%s' "$text" | head -c 300 | tr '\n' ' ')]"
    elif [ "${text#*\[redacted\]}" = "$text" ]; then
        check_failed "(1) the task.gated record's $key holds no [redacted]: [$(printf '%s' "$text" | head -c 300 | tr '\n' ' ')]"
    fi
done
note "(1) checked: log, note and reason are masked"

get "/api/missions/task/$t_one" "$WORK/task.json"
note_text=$(jq -r '.review_note // ""' "$WORK/task.json")
leaks "$note_text" && check_failed "(2) the review note holds a credential unmasked: [$(printf '%s' "$note_text" | head -c 300 | tr '\n' ' ')]"
got=$(reason_of "$t_one")
leaks "$got" && check_failed "(2) the last gate's reason holds a credential unmasked: [$got]"
[ -n "$note_text" ] || check_failed "(2) the task has no review note"
note "(2) checked: the note and the page are masked"

candidate=$(cut -d' ' -f2 "$WORK/judged.log" | head -n 1)
[ "$(printf '%s' "$record" | jq -r '.verdict')" = rejected ] ||
    check_failed "(3) the record's verdict is not rejected: $(printf '%s' "$record" | jq -c '{verdict}')"
[ "$(printf '%s' "$record" | jq -r '.id')" = "$t_one" ] ||
    check_failed "(3) the record's id is not task $t_one"
judging=$(printf '%s' "$record" | jq -r '.judging // ""')
printf '%s' "$judging" | grep -Eq '^[0-9a-f]{28,32}$' ||
    check_failed "(3) the record's judging [$judging] is not a judging id"
[ "$(printf '%s' "$record" | jq -r '.inputs.candidate // ""')" = "$candidate" ] ||
    check_failed "(3) the record's inputs.candidate [$(printf '%s' "$record" | jq -r '.inputs.candidate // ""')] is not the commit the gate judged [$candidate]"
[ "$(q "select state from mission_tasks where id = $t_one")" = revise ] ||
    check_failed "(3) task $t_one is $(q "select state from mission_tasks where id = $t_one"), not revise"
note "(3) checked: what the folds read keeps its bytes"

case $(printf '%s' "$record" | jq -r '.log // ""') in
*"| stage gate rejected 400ms regression: 092.sh must still pass"*) ;;
*) check_failed "(4) the record's log lost its stage line" ;;
esac
note "(4) checked: the rest of the output is as written"

# --- (5) and (6): a failed deploy's reason, as on-landing.sh records it -------
head40=$(printf '%040d' 170 | tr '0' 'a')
capture "$SPAWN_BIN" mission deployed --head "$head40" --ok false --by landing \
    --reason "deploy.sh failed: curl -H 'Authorization: Bearer $TOKEN' and PGPASSWORD=$PASSWORD"
[ "$status" -eq 0 ] || check_failed "(5) mission deployed exited $status: $(err | tail -n 2 | tr '\n' ' ')"
deploy_reason() {
    q "select coalesce(kind -> 'detail' -> 'record' ->> 'reason', '') from spawn_events
        where kind -> 'detail' -> 'record' ->> 'type' = 'deploy.finished' order by id desc limit 1"
}
got=$(deploy_reason)
if leaks "$got"; then check_failed "(5) the stored deploy reason holds a credential unmasked: [$got]"; fi
[ -n "$got" ] || check_failed "(5) no deploy.finished reason was stored"
note "(5) checked: a failed deploy's reason is masked"

long_reason="$(printf '%494s' '' | tr ' ' r) PWD=x"
[ "$(printf '%s' "$long_reason" | wc -c | tr -d ' ')" = 500 ] || harness "the long reason is not 500 bytes"
capture "$SPAWN_BIN" mission deployed --head "$head40" --ok false --by landing --reason "$long_reason"
[ "$status" -eq 0 ] ||
    check_failed "(6) a 500-char reason with a short secret was refused, exit $status: $(err | tail -n 2 | tr '\n' ' ')"
got=$(deploy_reason)
length=$(q "select coalesce(length(kind -> 'detail' -> 'record' ->> 'reason'), 0) from spawn_events
             where kind -> 'detail' -> 'record' ->> 'type' = 'deploy.finished' order by id desc limit 1")
[ "$length" -le 500 ] || check_failed "(6) the stored reason is $length chars, over 500"
case $got in *"PWD=x"*) check_failed "(6) the stored reason holds the secret: [...$(printf '%s' "$got" | tail -c 40)]" ;; esac
note "(6) checked: a reason at its limit is taken, masked, within it"

# --- (7) an owner's push given no verdict twice for one masked reason -------
cat >"$WORK/fault-gate.sh" <<EOF2
#!/bin/sh
printf '%s\n' "\$3" >>'$WORK/faults.log'
echo "| == regression: check-tests.sh must still pass"
echo "| connect: PGPASSWORD=$PASSWORD refused"
printf '%s\n' '{"host_fault":true,"scenario":"check-tests.sh","on":"base","runs":"1, then 1 on the base","reason":"connect: PGPASSWORD=$PASSWORD refused"}' >"\$SPAWN_GATE_VERDICT"
exit 6
EOF2
chmod 755 "$WORK/fault-gate.sh"
: >"$WORK/faults.log"
osrc=$WORK/owner.git
oseed=$WORK/seed-owner
git init -q --initial-branch=main "$oseed"
printf 'base\n' >"$oseed/README.md"
git -C "$oseed" add -A
git -C "$oseed" commit -qm base
git init -q --bare --initial-branch=main "$osrc"
git -C "$oseed" push -q "$osrc" main:main
capture "$SPAWN_BIN" mission add --text "170 owner" --source "$osrc" --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission add (owner) failed: $(err)"
mkdir -p "$oseed/scenarios"
: >"$oseed/scenarios/protected"
printf 'owner\n' >"$oseed/OWNER.md"
git -C "$oseed" add -A
git -C "$oseed" commit -qm "owner: a"
ocommit=$(git -C "$oseed" rev-parse HEAD)
capture "$SPAWN_BIN" mission push --source "$osrc" --commit "$ocommit" --gated --repo "$oseed"
[ "$status" -eq 0 ] || harness "mission push failed: $(err | tail -n 2 | tr '\n' ' ')"
orequest=$(jq -r .request "$WORK/out.txt")
for pass in 1 2 3; do
    capture env -u DBUS_SESSION_BUS_ADDRESS -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fault-gate.sh" \
        XDG_CACHE_HOME="$WORK/cache" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        SPAWN_GATE_JOBS=1 SPAWN_GATE_WORKER_CORES=0 SPAWN_INTEGRATE_WARM=0 \
        SPAWN_GATE_HOST_FAULT_BACKOFF_SECS=1 \
        "$SPAWN_BIN" integrate --root "$WORK/integrator" --once
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || harness "integrate --once (owner, pass $pass) exited $status: $(err | tail -n 3 | tr '\n' ' ')"
    sleep 2
done
runs=$(grep -c . "$WORK/faults.log" || true)
[ "$runs" -ge 1 ] || harness "the owner's request was never judged"
[ "$runs" -eq 2 ] ||
    check_failed "(7) the owner's request was judged $runs times: refused at the second no-verdict for one reason, never a third"
refused=$(q "select count(*) from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'source.push_refused'
                and (kind -> 'detail' -> 'record' ->> 'request')::bigint = $orequest")
[ "$refused" = 1 ] || check_failed "(7) $refused source.push_refused records for request $orequest, not 1"
q "select coalesce(kind -> 'detail' -> 'record' ->> 'reason', '') from spawn_events
    where kind -> 'detail' -> 'record' ->> 'type' = 'source.push_no_verdict'
      and (kind -> 'detail' -> 'record' ->> 'request')::bigint = $orequest order by id" >"$WORK/no-verdicts.txt"
[ -s "$WORK/no-verdicts.txt" ] || check_failed "(7) no source.push_no_verdict record for request $orequest"
while IFS= read -r line; do
    if leaks "$line"; then check_failed "(7) a no-verdict reason holds the password: [$line]"; fi
    case $line in *"[redacted]"*) ;; *) check_failed "(7) a no-verdict reason holds no [redacted]: [$line]" ;; esac
done <"$WORK/no-verdicts.txt"
note "(7) checked: no verdict twice for one masked reason refuses the request"

# --- (8) the same at the 500-char boundary (R3, cc-astra's A73) ------------
# The integrator's formatted reason, "the host's fault on check-tests.sh: "
# and the fault's reason, is exactly 500 chars and ends in " PWD=x", whose
# mask is longer. R2 cut the stored reason back to 500 and compared it with
# the uncut mask: never equal, so a third judging.
prefix="the host's fault on check-tests.sh: "
tail=" PWD=x"
pad=$((500 - ${#prefix} - ${#tail}))
breason="$(printf "%${pad}s" '' | tr ' ' r)$tail"
[ $((${#prefix} + ${#breason})) -eq 500 ] || harness "(8) the boundary reason is $((${#prefix} + ${#breason})) chars, not 500"
cat >"$WORK/boundary-gate.sh" <<EOF2
#!/bin/sh
printf '%s\n' "\$3" >>'$WORK/boundary.log'
echo "| == regression: check-tests.sh must still pass"
printf '%s\n' '{"host_fault":true,"scenario":"check-tests.sh","on":"base","runs":"1, then 1 on the base","reason":"$breason"}' >"\$SPAWN_GATE_VERDICT"
exit 6
EOF2
chmod 755 "$WORK/boundary-gate.sh"
: >"$WORK/boundary.log"
bsrc=$WORK/boundary.git
bseed=$WORK/seed-boundary
git init -q --initial-branch=main "$bseed"
printf 'base\n' >"$bseed/README.md"
git -C "$bseed" add -A
git -C "$bseed" commit -qm base
git init -q --bare --initial-branch=main "$bsrc"
git -C "$bseed" push -q "$bsrc" main:main
capture "$SPAWN_BIN" mission add --text "170 boundary" --source "$bsrc" --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission add (boundary) failed: $(err)"
mkdir -p "$bseed/scenarios"
: >"$bseed/scenarios/protected"
printf 'boundary\n' >"$bseed/OWNER.md"
git -C "$bseed" add -A
git -C "$bseed" commit -qm "owner: b"
bcommit=$(git -C "$bseed" rev-parse HEAD)
capture "$SPAWN_BIN" mission push --source "$bsrc" --commit "$bcommit" --gated --repo "$bseed"
[ "$status" -eq 0 ] || harness "mission push (boundary) failed: $(err | tail -n 2 | tr '\n' ' ')"
brequest=$(jq -r .request "$WORK/out.txt")
for pass in 1 2 3; do
    capture env -u DBUS_SESSION_BUS_ADDRESS -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/boundary-gate.sh" \
        XDG_CACHE_HOME="$WORK/cache" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        SPAWN_GATE_JOBS=1 SPAWN_GATE_WORKER_CORES=0 SPAWN_INTEGRATE_WARM=0 \
        SPAWN_GATE_HOST_FAULT_BACKOFF_SECS=1 \
        "$SPAWN_BIN" integrate --root "$WORK/integrator" --once
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || harness "integrate --once (boundary, pass $pass) exited $status: $(err | tail -n 3 | tr '\n' ' ')"
    sleep 2
done
bruns=$(grep -c . "$WORK/boundary.log" || true)
[ "$bruns" -ge 1 ] || harness "the boundary request was never judged"
[ "$bruns" -eq 2 ] ||
    check_failed "(8) the boundary request was judged $bruns times: refused at the second no-verdict for one 500-char reason, never a third"
brefused=$(q "select count(*) from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'source.push_refused'
                and (kind -> 'detail' -> 'record' ->> 'request')::bigint = $brequest")
[ "$brefused" = 1 ] || check_failed "(8) $brefused source.push_refused records for request $brequest, not 1"
q "select coalesce(kind -> 'detail' -> 'record' ->> 'reason', '') from spawn_events
    where kind -> 'detail' -> 'record' ->> 'type' = 'source.push_no_verdict'
      and (kind -> 'detail' -> 'record' ->> 'request')::bigint = $brequest order by id" >"$WORK/boundary-verdicts.txt"
[ -s "$WORK/boundary-verdicts.txt" ] || check_failed "(8) no source.push_no_verdict record for request $brequest"
while IFS= read -r line; do
    case $line in *"PWD=x"*) check_failed "(8) a boundary no-verdict reason holds the secret: [...$(printf '%s' "$line" | tail -c 40)]" ;; esac
    # Only the fields a store check limits are ever cut (A73): this one is
    # kept whole, masked, so longer than the 500 chars it was written with.
    [ "${#line}" -gt 500 ] ||
        check_failed "(8) a boundary no-verdict reason is cut to ${#line} chars: a no-verdict's reason is kept whole, masked [...$(printf '%s' "$line" | tail -c 40)]"
done <"$WORK/boundary-verdicts.txt"
note "(8) checked: no verdict twice at the 500-char boundary refuses the request"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "170: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "170: a gate rejection's output, note and reason are stored masked, its keys as written"
exit 0
