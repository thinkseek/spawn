#!/bin/sh
# Black box: an objective whose scenario already passes on the base is
# closed even when the backlog no longer holds it, and an owner or the
# supervisor can retire an objective by hand, naming who and why
# (GitHub #65; the supervisor's "176: reconcile or park").
#
# The problem. When the gate says "scenario already passes on the base"
# for an objective's task, the integrator records `objective.solved` and
# ends the task without a rejection, but only if `objectives::one_shot`
# can read the mission's backlog and finds the objective there,
# non-recurring. Otherwise the verdict is an ordinary rejection: the task
# goes back to its worker (`revise`), it counts toward the three that
# halt the mission, and the objective stays open. That happens to an
# objective renumbered or removed from the backlog, or to any objective
# in a backlog with one unreadable file. The worker can never make the
# scenario "fail on the base", so the objective can neither land nor
# close. And nothing closes an objective except a landing or that gate
# verdict, so a supervisor told to "reconcile or park" one has no action.
#
# Required:
# - "scenario already passes on the base" on an objective's task ends it
#   as solved on the base (`objective.solved`, the task cancelled, no
#   rejection counted). The one exception is an objective the backlog
#   reads as recurring.
# - `spawn objective retire --mission <m> --objective <n> --kind <kind>
#   --by <name> --reason <text>` records `objective.retired` {id: m,
#   objective: n, kind, by, reason, caller} (--kind, objective 223: on-main,
#   renumbered or obsolete; this scenario retires as renumbered and
#   obsolete, which need no gate record, and 223.sh covers on-main). The store sets caller to `owner` or `spawn_supervisor`;
#   any other login is refused. A retired objective is closed the way a
#   landing closes one: no task for it is assigned again.
# - It refuses, recording nothing:
#   - a blank by or reason;
#   - a mission that does not exist or has no backlog;
#   - an objective with a task in flight;
#   - an objective already closed (landed, solved or retired).
# - `spawn mission list` (and GET /api/missions) shows each mission's
#   `retired`: [{objective, by, reason, caller, at_ms}], oldest first.
#
# The world: one source, one gated objective mission on it (resident `r`,
# workers 3), and a backlog the scenario writes. Each task is delivered and
# reported by the scenario, its branch holding case.txt. A recording gate
# (SPAWN_GATE) answers by case.txt:
#   solved: "scenario already passes on the base";
#   fails:  "scenario fails on the candidate".
# `spawn integrate --once` passes assign and judge.
#   (a) Objectives 1 and 2 are assigned (tasks T1, T2). T1's work is
#       `solved`, T2's `fails`. Then 1.json leaves the backlog (renumbered
#       away), and a malformed x.json joins it while both are judged (R3,
#       rev-j's F3: a backlog that cannot be read at all; it leaves before
#       (b)). Once both are judged:
#       - T1 is cancelled; one objective.solved names objective 1, T1 and
#         the gate's base; T1 has no rejected task.gated.
#       - T2 is `revise`, its last gate rejected "scenario fails on the
#         candidate" (c).
#       - The mission's gate_rejections is 1 and it is not halted.
#   (b) Objectives 3 and 4 join the backlog. Retire objective 3:
#       - exit 0; the record names mission, objective 3, by, reason, and
#         caller `owner`;
#       - `spawn mission list` shows it in the mission's `retired`.
#       Objective 5, a RECURRING one (a metric and a target), joins too, and
#       is retired the same way (R2, rev-h's F1: a retired objective is
#       closed whatever its kind, never by a metric it has no value for),
#       through the supervisor's own login (R3, rev-j's F1): its record and
#       its `retired` row name caller `spawn_supervisor`. Objective 6, with
#       no file in the backlog (renumbered away, never assigned), is retired
#       by the owner (R3, rev-j's F2). mission list's retired is 3, 5, 6.
#       Then passes run. T2 still holds one of the 3 slots; of the two free
#       ones, only objective 4 takes one (T4), never 3 or 5. In the end
#       objectives 3 and 5 have no task, and objective 1 has only T1.
#   (p) (R2) The RSI page's read, GET /api/rsi from `spawn serve`:
#       the mission's objective rows for 3 and 5 have status `retired`, each
#       with its `retired` {objective, by, reason, caller, at_ms}, and
#       `next_objective` is null (no objective is open: 1 solved, 2 and 4
#       in flight, 3 and 5 retired).
#   (r) Refusals, each exiting non-zero and recording nothing, each with
#       one reason only:
#       - retiring objective 2 (T2 in flight);
#       - objective 1 (solved), or 3 again (retired);
#       - objective 3 with a blank by, with a blank reason, of mission
#         999999 (none), or of a mission with no backlog;
#       - objective 3 by the reader login, or by the human login (R3).
#       The logins other than the owner's get a password made up here, in a
#       0600 passfile, never on a command line (212.sh's pattern).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# On main b63df7b2 it exits 1:
# - at (a), T1 is `revise`, rejected, and gate_rejections is 2;
# - at (b), `spawn objective` is no command and mission list has no
#   `retired`. Objective 4 gets no task: T1 and T2 in revise hold two of
#   the 3 slots, and objective 3 takes the third. (r)'s refusals hold
#   vacuously there.
# - (c) holds there too.

set -eu

asserted=""
harness() {
    echo "210: harness: $*" >&2
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
SPAWN_DB_NAME="bb_210_retire_$run"
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
    set +e
    if [ "$rc" -ne 0 ]; then
        echo "210: the gate runs:" >&2
        cat "$WORK/judged.log" >&2 2>/dev/null || true
        echo "210: the integrator's last lines:" >&2
        grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 20 >&2 || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "210: harness: unexpected exit $rc" >&2
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

note() { echo "210: $*" >&2; }
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

# Logins other than the owner's (R3, rev-j's F1), as a pane gets one: this
# database's login for <base>, with a password made up here and handed over
# in a 0600 passfile, never on a command line (212.sh's pattern).
escape_pass() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/:/\\:/g'; }
login_for() { # <base> <name>: sets <name>_url and writes $WORK/<name>.pass
    role=$(q "select spawn_login_role('$1')")
    [ -n "$role" ] || harness "this database has no $1 login"
    pw="bb210$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
    printf 'alter role "%s" password '"'"'%s'"'"'\n' "$role" "$pw" |
        psql -d "$SPAWN_DB_NAME" -qtA -v ON_ERROR_STOP=1 -f - >/dev/null 2>"$WORK/role.err" ||
        harness "cannot set the $1 login's password: $(head -c 300 "$WORK/role.err")"
    printf '%s:%s:%s:%s:%s\n' "$(escape_pass "$pg_url_host")" "$pg_url_port" "$SPAWN_DB_NAME" "$role" "$pw" \
        >"$WORK/$2.pass"
    chmod 600 "$WORK/$2.pass"
    printf '%s://%s@%s:%s/%s\n' "$pg_url_scheme" "$role" "$(pg_url_encode_host "$pg_url_host")" "$pg_url_port" \
        "$SPAWN_DB_NAME" >"$WORK/$2.url"
}
login_for spawn_supervisor sup
login_for spawn_reader reader
login_for spawn_human human

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

src=$WORK/source.git
seed=$WORK/seed
git init -q --initial-branch=main "$seed"
printf 'base\n' >"$seed/README.md"
git -C "$seed" add -A
git -C "$seed" commit -qm base
git init -q --bare --initial-branch=main "$src"
git -C "$seed" push -q "$src" main:main
base_commit=$(git --git-dir="$src" rev-parse main)

gate=$WORK/ctl/rsi/scenarios
mkdir -p "$gate"
for n in 001 002 003 004 005; do printf '#!/bin/sh\nexit 0\n' >"$gate/$n.sh"; done
printf '# nothing protected\n' >"$gate/protected"
: >"$gate/no-test-database"
: >"$WORK/judged.log"
: >"$WORK/integrate.log"
mkdir -p "$WORK/cache" "$WORK/integrator"

# The recording gate: by the candidate's case.txt.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
case=\$(git --git-dir="\$1" cat-file -p "\$3:case.txt" 2>/dev/null || echo none)
printf '%s %s %s %s\n' "\$(date +%s)" "\$case" "\$2" "\$3" >>'$WORK/judged.log'
case \$case in
solved)
    echo "| == base: the task's scenario"
    echo "| verdict rejected scenario already passes on the base"
    printf '{"verdict":"rejected","reason":"scenario already passes on the base"}\n' >"\$SPAWN_GATE_VERDICT"
    ;;
*)
    echo "| == candidate: the task's scenario"
    echo "| verdict rejected scenario fails on the candidate"
    printf '{"verdict":"rejected","reason":"scenario fails on the candidate"}\n' >"\$SPAWN_GATE_VERDICT"
    ;;
esac
exit 0
EOF
chmod 755 "$WORK/fake-gate.sh"

backlog=$WORK/backlog
mkdir -p "$backlog"
objective() { # <n>: writes <n>.json into the backlog
    printf '{"id": %s, "title": "Made-up objective %s", "prompt": "Made-up work %s."}\n' "$1" "$1" "$1" \
        >"$backlog/$1.json"
}
objective 1
objective 2

capture "$SPAWN_BIN" resident propose --name r --charter "Owns the 210 objectives."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name r
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
capture "$SPAWN_BIN" mission add --text "210 objectives" --source "$src"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate \
    --backlog "$backlog" --resident r --workers 3
[ "$status" -eq 0 ] || harness "mission configure exited $status: $(err)"

pass() { # one `spawn integrate --once`
    capture env -u DBUS_SESSION_BUS_ADDRESS -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        XDG_CACHE_HOME="$WORK/cache" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        SPAWN_GATE_JOBS=1 SPAWN_GATE_WORKER_CORES=0 SPAWN_INTEGRATE_WARM=0 \
        "$SPAWN_BIN" integrate --root "$WORK/integrator" --once
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || harness "integrate --once exited $status: $(err | tail -n 3 | tr '\n' ' ')"
}
task_of() { # <objective>: the lowest task of it, or empty
    q "select coalesce(min(id)::text, '') from mission_tasks where mission_id = $mission and objective = $1"
}
tasks_of() { q "select count(*) from mission_tasks where mission_id = $mission and objective = $1"; }
state_of() { q "select state from mission_tasks where id = $1"; }
tasks_line() {
    sql "select string_agg('#' || id || ' o' || coalesce(objective::text, '-') || ' ' || state, ', ' order by id)
           from mission_tasks where mission_id = $mission"
}
# Deliver and report task <id>, its branch holding case.txt = <case>.
work() { # <task> <case>
    agent=$(q "select agent_id from mission_tasks where id = $1")
    session=$(q "select session_id from mission_tasks where id = $1")
    capture "$SPAWN_BIN" submit --agent "$agent" --session "$session" --key "k$1" \
        --action '{"type":"start","engine":"claude","prompt":"made-up work"}'
    [ "$status" -eq 0 ] || harness "submit (task $1) failed: $(err)"
    rm -rf "$WORK/w$1"
    git clone -q "$src" "$WORK/w$1"
    printf '%s\n' "$2" >"$WORK/w$1/case.txt"
    git -C "$WORK/w$1" add -A
    git -C "$WORK/w$1" commit -qm "$2"
    git -C "$WORK/w$1" push -q origin "HEAD:refs/heads/spawn/$agent"
    command=$(q "select id from spawn_commands where idempotency_key = 'k$1'")
    [ "$(q "select spawn_task_delivered($1, $command)")" = t ] || harness "task $1 not delivered"
    [ "$(q "select spawn_task_reported($1, 'done')")" = t ] || harness "task $1 not reported"
}
gated_rejected() { # <task>: its rejected task.gated records
    q "select count(*) from spawn_events
        where session_id = 'session_control' and event_name = 'control.recorded'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
          and kind -> 'detail' -> 'record' ->> 'verdict' = 'rejected'
          and kind -> 'detail' -> 'record' -> 'id' = to_jsonb($1::bigint)"
}

# --- (a) solved on the base with the objective gone from the backlog -------------------

passes=0
until [ -n "$(task_of 1)" ] && [ -n "$(task_of 2)" ]; do
    [ "$passes" -lt 4 ] || harness "objectives 1 and 2 were not both assigned in 4 passes (tasks: $(tasks_line))"
    pass
    passes=$((passes + 1))
done
t1=$(task_of 1)
t2=$(task_of 2)
work "$t1" solved
work "$t2" fails
rm "$backlog/1.json"
# R3 (rev-j's F3): the backlog also holds a malformed file while T1 is
# judged, so it cannot be read at all; it goes before (b), whose
# assignments read the same backlog.
printf '{\n' >"$backlog/x.json"
passes=0
until [ "$(grep -c . "$WORK/judged.log")" -ge 2 ]; do
    [ "$passes" -lt 6 ] || harness "the gate judged $(grep -c . "$WORK/judged.log") of 2 tasks in 6 passes (tasks: $(tasks_line))"
    pass
    passes=$((passes + 1))
done
mv "$backlog/x.json" "$WORK/x.json.out"

got=$(state_of "$t1")
[ "$got" = cancelled ] ||
    check_failed "(a) task #$t1 (objective 1, its scenario passing on the base, 1.json gone): state $got, expected cancelled (tasks: $(tasks_line))"
solved=$(records objective.solved | jq -c --argjson t "$t1" 'select(.task == $t)')
[ "$(printf '%s' "$solved" | grep -c .)" -eq 1 ] ||
    check_failed "(a) objective.solved records naming task #$t1: [$solved], expected exactly one"
[ -z "$solved" ] || {
    [ "$(printf '%s' "$solved" | jq -r '"\(.id) \(.objective) \(.base)"')" = "$mission 1 $base_commit" ] ||
        check_failed "(a) the objective.solved of task #$t1 names [$(printf '%s' "$solved" | jq -c '{id, objective, base}')], expected mission $mission, objective 1, base $base_commit"
}
[ "$(gated_rejected "$t1")" -eq 0 ] ||
    check_failed "(a) task #$t1 has $(gated_rejected "$t1") rejected task.gated record(s): a base-solved objective is no rejection"
got=$(state_of "$t2")
[ "$got" = revise ] || check_failed "(c) task #$t2 (fails on the candidate): state $got, expected revise"
got=$(q "select coalesce(kind -> 'detail' -> 'record' ->> 'reason', '') from spawn_events
          where session_id = 'session_control' and event_name = 'control.recorded'
            and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
            and kind -> 'detail' -> 'record' -> 'id' = to_jsonb($t2::bigint)
          order by session_seq desc limit 1")
[ "$got" = "scenario fails on the candidate" ] ||
    check_failed "(c) task #$t2's last task.gated reason: [$got], expected [scenario fails on the candidate]"
got=$(q "select gate_rejections || ' ' || (halted is not null) from mission_missions where id = $mission")
case $got in
"1 false") ;;
*) check_failed "(a) the mission's gate_rejections and halted: [$got], expected [1 false]: only T2's rejection counts" ;;
esac
note "(a), (c) checked"

# --- (b) and (r): retire ---------------------------------------------------------------

as=""
retire() { # <mission> <objective> <by> <reason> [<kind>, obsolete]: as the login $as names (sup, reader, human), else the owner
    if [ -n "$as" ]; then
        capture env -u PGPASSWORD SPAWN_DATABASE_URL="$(cat "$WORK/$as.url")" PGPASSFILE="$WORK/$as.pass" \
            "$SPAWN_BIN" objective retire --mission "$1" --objective "$2" --kind "${5:-obsolete}" --by "$3" --reason "$4"
    else
        capture "$SPAWN_BIN" objective retire --mission "$1" --objective "$2" --kind "${5:-obsolete}" --by "$3" --reason "$4"
    fi
}
before=$(count_records objective.retired)
refused() { # <what> <mission> <objective> <by> <reason>
    what=$1
    shift
    retire "$@"
    [ "$status" -ne 0 ] || check_failed "(r) retire $what exited 0: $(out | head -c 200)"
    [ "$(count_records objective.retired)" -eq "$before" ] ||
        check_failed "(r) retire $what recorded an objective.retired: $(records objective.retired | tail -n 1 | head -c 300)"
}
refused "of objective 2 (task #$t2 in flight)" "$mission" 2 lead "made-up reason"
refused "of objective 1 (solved on the base)" "$mission" 1 lead "made-up reason"
capture "$SPAWN_BIN" mission add --text "210 no backlog" --source "$src"
[ "$status" -eq 0 ] || harness "mission add (no backlog) failed: $(err)"
plain=$(jq -r .mission "$WORK/out.txt")

objective 3
objective 4
printf '{"id": 5, "title": "Made-up recurring objective 5", "prompt": "Made-up work 5.", "recurring": true, "metric": "made_up_ms", "target": 100, "direction": "lower"}\n' \
    >"$backlog/5.json"
refused "of objective 3 with a blank by" "$mission" 3 "  " "made-up reason"
refused "of objective 3 with a blank reason" "$mission" 3 lead " "
refused "of objective 3 of mission 999999 (none)" 999999 3 lead "made-up reason"
refused "of objective 3 of mission $plain (no backlog)" "$plain" 3 lead "made-up reason"
as=reader
refused "of objective 3 by the reader login" "$mission" 3 lead "made-up reason"
as=human
refused "of objective 3 by the human login" "$mission" 3 lead "made-up reason"
as=""
reason="renumbered: its work is objective 4 in this made-up backlog"
retire "$mission" 3 "rev-f bb" "$reason" renumbered
if [ "$status" -ne 0 ]; then
    check_failed "(b) retire of objective 3 exited $status: $(err | head -c 300)"
else
    rec=$(records objective.retired | tail -n 1)
    want=$(jq -cn --argjson m "$mission" --arg r "$reason" \
        '{type: "objective.retired", id: $m, objective: 3, kind: "renumbered", by: "rev-f bb", reason: $r, caller: "owner"}')
    [ "$(printf '%s' "$rec" | jq -cS .)" = "$(printf '%s' "$want" | jq -cS .)" ] ||
        check_failed "(b) the objective.retired record: [$rec], expected [$want]"
fi
before=$(count_records objective.retired)
refused "of objective 3 again (retired)" "$mission" 3 lead "made-up reason"
reason5="a recurring objective parked by hand"
as=sup
retire "$mission" 5 "rev-f bb" "$reason5"
as=""
if [ "$status" -ne 0 ]; then
    check_failed "(b) retire of recurring objective 5 by the supervisor's login exited $status: $(err | head -c 300)"
else
    rec=$(records objective.retired | tail -n 1)
    want=$(jq -cn --argjson m "$mission" --arg r "$reason5" \
        '{type: "objective.retired", id: $m, objective: 5, kind: "obsolete", by: "rev-f bb", reason: $r, caller: "spawn_supervisor"}')
    [ "$(printf '%s' "$rec" | jq -cS .)" = "$(printf '%s' "$want" | jq -cS .)" ] ||
        check_failed "(b) the supervisor's objective.retired record: [$rec], expected [$want]"
fi
# R3 (rev-j's F2): an objective with no file in the backlog (renumbered
# away before it was ever assigned) is retired too.
reason6="renumbered away before it was ever assigned"
retire "$mission" 6 "rev-f bb" "$reason6" renumbered
[ "$status" -eq 0 ] || check_failed "(b) retire of objective 6 (no file in the backlog) exited $status: $(err | head -c 300)"

capture "$SPAWN_BIN" mission list
[ "$status" -eq 0 ] || harness "mission list exited $status: $(err | head -c 200)"
got=$(jq -c --argjson m "$mission" '[.missions[] | select(.id == $m) | .retired][0]
        | if type == "array" then map({objective, by, reason, caller, at: (.at_ms | type)}) else . end' "$WORK/out.txt")
want=$(jq -cn --arg r "$reason" --arg r5 "$reason5" --arg r6 "$reason6" '[{objective: 3, by: "rev-f bb", reason: $r, caller: "owner", at: "number"}, {objective: 5, by: "rev-f bb", reason: $r5, caller: "spawn_supervisor", at: "number"}, {objective: 6, by: "rev-f bb", reason: $r6, caller: "owner", at: "number"}]')
[ "$got" = "$want" ] || check_failed "(b) mission list's retired for mission $mission: [$got], expected [$want]"

passes=0
until [ -n "$(task_of 4)" ] || [ -n "$(task_of 3)" ] || [ -n "$(task_of 5)" ] || [ "$passes" -ge 4 ]; do
    pass
    passes=$((passes + 1))
done
pass
pass
[ "$(tasks_of 3)" -eq 0 ] ||
    check_failed "(b) retired objective 3 has $(tasks_of 3) task(s), expected none (tasks: $(tasks_line))"
[ "$(tasks_of 5)" -eq 0 ] ||
    check_failed "(b) retired recurring objective 5 has $(tasks_of 5) task(s), expected none (tasks: $(tasks_line))"
[ "$(tasks_of 4)" -eq 1 ] ||
    check_failed "(b) objective 4 has $(tasks_of 4) task(s), expected one: the free slot is its (tasks: $(tasks_line))"
[ "$(tasks_of 1)" -eq 1 ] ||
    check_failed "(a) objective 1 has $(tasks_of 1) task(s), expected only #$t1: solved, it is not assigned again (tasks: $(tasks_line))"
note "(b), (r) checked"

# --- (p) the RSI page's read ----------------------------------------------------------
mkdir -p "$WORK/web"
printf '<!doctype html><title>spawn</title>\n' >"$WORK/web/index.html"
# The reader login's password is the one login_for set (a restricted
# login's own password derives from the owner's, and login_for replaced it),
# so serve reads through that login and its passfile.
env -u PGPASSWORD SPAWN_SERVE_DATABASE_URL="$(cat "$WORK/reader.url")" PGPASSFILE="$WORK/reader.pass" \
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$WORK/web" >"$WORK/serve.log" 2>&1 &
serve_pid=$!
port=""
i=0
while [ "$i" -lt 100 ]; do
    i=$((i + 1))
    port=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$WORK/serve.log" 2>/dev/null | head -n 1)
    [ -n "$port" ] && break
    sleep 0.1
done
[ -n "$port" ] || harness "serve did not start: $(head -c 400 "$WORK/serve.log")"
code=$(curl -s --noproxy '*' -o "$WORK/missions.json" -w '%{http_code}' "http://127.0.0.1:$port/api/rsi" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/rsi answered $code: $(head -c 300 "$WORK/missions.json") $(tail -n 3 "$WORK/serve.log")"
got=$(jq -c --argjson m "$mission" '[.missions[] | select(.id == $m)][0]
        | {rows: [(.objectives // [])[] | select(.id == 3 or .id == 5)
                  | {id, status, retired: (.retired | if type == "object" then {objective, by, caller} else . end)}],
           next: .next_objective}' "$WORK/missions.json" 2>&1) || got="unreadable: $(head -c 300 "$WORK/missions.json")"
want=$(jq -cn '{rows: [{id: 3, status: "retired", retired: {objective: 3, by: "rev-f bb", caller: "owner"}},
                       {id: 5, status: "retired", retired: {objective: 5, by: "rev-f bb", caller: "spawn_supervisor"}}],
                next: null}')
[ "$got" = "$want" ] || check_failed "(p) GET /api/rsi for mission $mission, objectives 3 and 5 and next_objective: [$got], expected [$want]"
note "(p) checked"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "210: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "210: an objective solved on the base closes without a rejection though the backlog no longer has it; a retired objective, naming who and why, is never assigned"
exit 0
