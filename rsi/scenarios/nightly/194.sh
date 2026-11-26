#!/bin/sh
# Objective 194 [recoverability]: no new commit built on the history before
# a rewrite is pushed, and its task is told so once. 177's guard
# (rewrite_guard::why_old) refused a recorded id the commit map names as
# changed or dropped, and a commit whose history OUTSIDE every ref of the
# integrator's mirror held one. After the rewrite of 2026-11-15 the mirror
# (ws/.sources/spawn-*.git) still held the old history on its refs, the
# resident's branch included, and the resident's next checkpoint (task 402's
# 1f9ef20a) was a NEW commit on it: the guard passed it, the integrator's
# re-push of the lost checkpoint was refused by git as non-fast-forward, and
# every pass recorded the task `stale`, "the pushed branch ... does not
# contain the report's checkpoint" (sync/queue.md, the lead's 02:38 step-12
# audit). Now the guard refuses any commit whose history holds a commit the
# map names as changed or dropped, whatever ref holds it, and the
# integrator's stale verdict on it says "built on the history before the
# rewrite", recorded once.
#
# Black box, through `spawn resident propose/approve`, `spawn task assign`,
# `spawn commit-map load` and `spawn integrate --once`, on a source the
# scenario rewrites as filter-repo would (the root m0 kept, m1 rewritten to
# m1', the resident's work W on m1 to W'); the old objects stay in the
# source and in the integrator's mirror, where the resident's branch is W
# (the Bridge's workspace commits there). After the map is loaded the
# resident commits C on W (a new id the map does not name), its checkpoint,
# and reports C.
#   (1) one integrator, as on the host (passes 300 ms apart), until it has
#       judged the resident's task three times (each pass says C "is not
#       pushed again") and (2) has landed: the resident's task has exactly
#       one `stale` verdict,
#       and its reason says the checkpoint C "is built on the history before
#       the rewrite"; its spawn/<agent> in the source is still W' (nothing
#       pushed), and no ref of the source reaches C, W or m1; one
#       `rewrite_refused` alert names push-again, the task and C.
#   (2) a second mission on the same source, in the same run: a worker's
#       work N on the new main, its checkpoint, its report: it lands, main
#       holds its file and nothing from before the rewrite (the guard does
#       not refuse the new history).
# "Once" is once in an integrator's life (the lead's ruling): a restart,
# each deploy's too, records it once more; so the passes are one process's.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on the base (live main 53a3be15): (1) a `stale` verdict of
# the resident's task on every pass, "the pushed branch (W') does not
# contain the report's checkpoint (C)", and no `rewrite_refused` alert
# (the re-push of C is tried; git refuses it as non-fast-forward).

set -eu

asserted=""
harness() {
    echo "194: harness: $*" >&2
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
SPAWN_DB_NAME="bb_194_old_ancestry_$run"
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
        echo "194: the integrator's last lines:" >&2
        grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 20 >&2 || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "194: harness: unexpected exit $rc" >&2
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

note() { echo "194: $*" >&2; }
q() { scalar "$1"; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# --- the world ---------------------------------------------------------------------------
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

seed=$WORK/seed
mkdir -p "$seed/rsi/scenarios"
git init -q --initial-branch=main "$seed"
printf '#!/bin/sh\nexit 0\n' >"$seed/rsi/scenarios/check-ok.sh"
printf '# nothing protected\n' >"$seed/rsi/scenarios/protected"
: >"$seed/rsi/scenarios/no-test-database"
printf 'm0\n' >"$seed/file"
git -C "$seed" add -A
git -C "$seed" commit -qm m0
m0=$(git -C "$seed" rev-parse HEAD)
src=$WORK/source.git
git clone -q --bare "$seed" "$src"
git clone -q "$src" "$WORK/ctl"
cat >"$WORK/fake-gate.sh" <<'GATEEOF'
#!/bin/sh
printf '{"verdict":"accepted"}\n' >"$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
exit 0
GATEEOF
chmod 755 "$WORK/fake-gate.sh"

capture "$SPAWN_BIN" mission add --text "194 rewrite" --source "$src" --gate "$WORK/ctl/rsi/scenarios" --accept gate
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(q "select max(id) from mission_missions")

git clone -q "$src" "$WORK/owner"
push() { # <commit> <reason>: the owner's ungated request; sets request
    capture sh -c 'cd "$1" && shift && exec "$@"' sh "$WORK/owner" "$SPAWN_BIN" mission push --source "$src" --commit "$1" --reason "$2"
    [ "$status" -eq 0 ] || harness "mission push ($2) failed: $(err)"
    request=$(jq -r '.request // empty' "$WORK/out.txt")
    [ -n "$request" ] || harness "mission push ($2) printed no request: $(head -c 300 "$WORK/out.txt")"
}
commit() { # <subject> [--allow-empty]: an owner commit; prints its id
    if [ "${2-}" = --allow-empty ]; then
        git -C "$WORK/owner" commit -q --allow-empty -m "$1"
    else
        printf '%s\n' "$1" >>"$WORK/owner/file"
        git -C "$WORK/owner" add -A
        git -C "$WORK/owner" commit -qm "$1"
    fi
    git -C "$WORK/owner" rev-parse HEAD
}
mkdir -p "$WORK/integrator"
integrate() {
    capture env -u DBUS_SESSION_BUS_ADDRESS -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        XDG_CACHE_HOME="$WORK/cache" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        SPAWN_GATE_JOBS=1 SPAWN_GATE_WORKER_CORES=0 SPAWN_INTEGRATE_WARM=0 \
        "$SPAWN_BIN" integrate --root "$WORK/integrator" --once
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || harness "integrate --once exited $status: $(err | tail -n 3 | tr '\n' ' ')"
}
main_now() { git --git-dir="$src" rev-parse refs/heads/main; }

gated_of() { # <task>: its latest task.gated record's JSON
    q "select kind -> 'detail' -> 'record' from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'task.gated' and kind -> 'detail' -> 'record' ->> 'id' = '$1' order by id desc limit 1"
}
task_for() { # <worker>: assigns a task of the mission; sets task and agent
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$1" --brief "work of $1"
    [ "$status" -eq 0 ] || harness "task assign for $1 failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $mission")
    agent=$(q "select agent_id from mission_tasks where id = $task")
    [ -n "$agent" ] || harness "task $task has no agent"
}
work_of() { # <file>: the agent's commit of <file> on main, pushed as spawn/<agent>; prints it
    rm -rf "$WORK/agent-$1"
    git clone -q "$src" "$WORK/agent-$1"
    printf 'work\n' >"$WORK/agent-$1/$1"
    git -C "$WORK/agent-$1" add -A
    git -C "$WORK/agent-$1" commit -qm "work $1"
    git -C "$WORK/agent-$1" push -q origin "HEAD:refs/heads/spawn/$agent"
    git -C "$WORK/agent-$1" rev-parse HEAD
}
checkpoint() { # <task> <agent> <commit>: the agent's checkpoint, as the Bridge appends it
    session=$(q "select session_id from mission_tasks where id = $1")
    q "select out_id from spawn_append_trusted('$2', '$session', null, 'bridge', 'commit:$3', '{\"name\":\"artifact.committed\",\"detail\":{\"reason\":\"turn_stopped\",\"artifact\":{\"project\":\"missions\",\"branch\":\"$2\",\"commit\":\"$3\"},\"files\":[]}}'::jsonb)" >/dev/null ||
        harness "the checkpoint $3 of $2 was not appended"
}
reported() { # <task> <commit>: delivered and reported naming <commit>, as the Bridge records them
    q "select spawn_task_delivered($1, 1)" >/dev/null
    [ "$(q "select spawn_task_reported($1, 'done', '$2')")" = t ] || harness "task $1's report was not taken"
}

# Before the rewrite: the owner lands m1 (the mission's main and baseline
# move to it); the resident's work W on m1, pushed as its spawn/<agent> and
# the head of its branch in the integrator's mirror.
m1=$(commit m1)
push "$m1" "m1, before the rewrite"
integrate
[ "$(main_now)" = "$m1" ] || harness "the owner's m1 did not land before the rewrite: main is $(main_now)"
capture "$SPAWN_BIN" resident propose --name keep --charter "Owns the 194 work."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name keep
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker keep --brief "work of keep"
[ "$status" -eq 0 ] || harness "task assign for the resident failed: $(err)"
task_r=$(q "select max(id) from mission_tasks where mission_id = $mission")
agent=$(q "select agent_id from mission_tasks where id = $task_r")
agent_r=$agent
[ -n "$agent_r" ] || harness "the resident's task $task_r has no agent"
w=$(work_of w.txt)
mirror=""
for candidate in "$WORK/integrator/.sources/"*.git; do
    [ "$candidate" = "$WORK/integrator/.sources/scratch.git" ] || mirror=$candidate
done
[ -n "$mirror" ] && [ -d "$mirror" ] || harness "the integrator has no mirror of the source"
git --git-dir="$mirror" fetch -q "$src" "+refs/heads/spawn/$agent_r:refs/heads/$agent_r" ||
    harness "W was not fetched into the mirror's branch of $agent_r"

# --- the rewrite, as filter-repo would --------------------------------------------------------
tree_of() { git --git-dir="$src" rev-parse "$1^{tree}"; }
rewrite() { # <commit> <new parent>: the commit again, its tree and message, a new id
    GIT_COMMITTER_DATE="2026-11-14T12:00:00Z" GIT_AUTHOR_DATE="2026-11-14T12:00:00Z" \
        git --git-dir="$src" commit-tree "$(tree_of "$1")" -p "$2" -m "$(git --git-dir="$src" log -1 --format=%s "$1") (rewritten)"
}
m1n=$(rewrite "$m1" "$m0")
wn=$(rewrite "$w" "$m1n")
git --git-dir="$src" update-ref refs/heads/main "$m1n" "$m1"
git --git-dir="$src" update-ref "refs/heads/spawn/$agent_r" "$wn" "$w"
git --git-dir="$src" for-each-ref --format='%(refname) %(objectname)' | while read -r name id; do
    [ "$id" = "$m1" ] || continue
    git --git-dir="$src" update-ref "$name" "$m1n" "$m1" || exit 1
done || harness "the refs on m1 were not moved to m1'"
printf 'old                                      new\n%s %s\n%s %s\n%s %s\n' \
    "$m0" "$m0" "$m1" "$m1n" "$w" "$wn" >"$WORK/commit-map"
capture "$SPAWN_BIN" commit-map load "$WORK/commit-map" --source "$src"
[ "$status" -eq 0 ] || harness "spawn commit-map load exited $status: $(err | head -c 300)"

# After the load, the resident's workspace (still on the old history)
# commits C on W in the mirror: a new id, its checkpoint and its report.
printf 'more\n' >"$WORK/more.txt"
blob=$(git --git-dir="$mirror" hash-object -w "$WORK/more.txt")
tree=$( (git --git-dir="$mirror" ls-tree "$w"; printf '100644 blob %s\tmore.txt\n' "$blob") | git --git-dir="$mirror" mktree)
c=$(git --git-dir="$mirror" commit-tree "$tree" -p "$w" -m "the resident's next checkpoint, on the old history")
git --git-dir="$mirror" update-ref "refs/heads/$agent_r" "$c" "$w" || harness "the mirror's branch of $agent_r did not move to C"
checkpoint "$task_r" "$agent_r" "$c"
reported "$task_r" "$c"
stale_of() { # <task>: its stale verdicts
    q "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'task.gated' and kind -> 'detail' -> 'record' ->> 'id' = '$1' and kind -> 'detail' -> 'record' ->> 'verdict' = 'stale'"
}
old_in_refs() { git --git-dir="$src" rev-list --all | grep -cxF -e "$c" -e "$w" -e "$m1" || true; }

# The new history beside it (2): a second mission on the same source, a
# worker's work N on the new main, its checkpoint, its report.
capture "$SPAWN_BIN" mission add --text "194 new history" --source "$src" --gate "$WORK/ctl/rsi/scenarios" --accept gate
[ "$status" -eq 0 ] || harness "mission add (new history) failed: $(err)"
mission=$(q "select max(id) from mission_missions")
task_for n1
task_n=$task
agent_n=$agent
n=$(work_of n.txt)
checkpoint "$task_n" "$agent_n" "$n"
reported "$task_n" "$n"

# One integrator, as on the host (its passes 300 ms apart): what it says
# once is once in its life. Each pass that judges the resident's task tries
# the lost checkpoint's re-push first and says it is not pushed again.
SPAWN_LOOP_INTERVAL_MS=300 env -u DBUS_SESSION_BUS_ADDRESS -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
    XDG_CACHE_HOME="$WORK/cache" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
    SPAWN_GATE_JOBS=1 SPAWN_GATE_WORKER_CORES=0 SPAWN_INTEGRATE_WARM=0 \
    "$SPAWN_BIN" integrate --root "$WORK/integrator" >>"$WORK/integrate.log" 2>&1 &
integrator=$!
echo "$integrator" >>"$WORK/pids"
# Passes that judged the resident's task: each says the lost checkpoint's
# re-push (refused, or tried), or else its stale verdict (the base's).
judged_r() {
    tried=$(grep -cE "integrate: task $task_r: $c (is not )?pushed again" "$WORK/integrate.log" || true)
    said=$(grep -c "integrate: task $task_r stale" "$WORK/integrate.log" || true)
    if [ "$tried" -gt "$said" ]; then echo "$tried"; else echo "$said"; fi
}
done_both() {
    [ "$(judged_r)" -ge 3 ] && [ "$(q "select state from mission_tasks where id = $task_n")" = landed ]
}
waited=0
until done_both; do
    kill -0 "$integrator" 2>/dev/null || harness "the integrator exited: $(tail -n 3 "$WORK/integrate.log" | tr '\n' ' ')"
    [ "$waited" -lt 240 ] || break
    sleep 1
    waited=$((waited + 1))
done
kill "$integrator" 2>/dev/null || true
wait "$integrator" 2>/dev/null || true
passes=$(judged_r)
[ "$passes" -ge 3 ] ||
    check_failed "(1) passes that judged the resident's task in 240 s: $passes, not 3 (no line of a re-push of C or of its stale verdict): $(grep -c "task $task_r" "$WORK/integrate.log" || true) lines name the task"

# --- (1) one stale verdict that says why, nothing pushed ------------------------------------
last=$(gated_of "$task_r")
expect_eq "$(stale_of "$task_r")" 1 \
    "(1) stale verdicts of the resident's task in $passes passes of one integrator (its last: $(printf '%s' "$last" | jq -c '{verdict, reason}' | head -c 300))"
printf '%s' "$last" | jq -r '.reason // ""' | grep -qF "the commit $c is built on the history before the rewrite" ||
    check_failed "(1) the stale verdict does not say C is built on the history before the rewrite: $(printf '%s' "$last" | jq -r '.reason // ""' | head -c 300)"
expect_eq "$(git --git-dir="$src" rev-parse "refs/heads/spawn/$agent_r")" "$wn" "(1) the resident's spawn/<agent> in the source"
expect_eq "$(old_in_refs)" 0 "(1) commits from before the rewrite (or on it) any ref of the source reaches (C, W, m1)"
expect_eq "$(q "select count(*) from spawn_alerts where kind = 'rewrite_refused' and key = 'rewrite_refused:push-again:$task_r:$c'")" \
    1 "(1) rewrite_refused alerts naming push-again, task $task_r and C"
note "(1) done"

# --- (2) the new history is judged as before ----------------------------------------------------
last=$(gated_of "$task_n")
expect_eq "$(q "select state from mission_tasks where id = $task_n")" landed \
    "(2) the task on the new history (its last verdict: $(printf '%s' "$last" | jq -c '{verdict, reason}' | head -c 300))"
git --git-dir="$src" cat-file -e "$(main_now):n.txt" 2>/dev/null || check_failed "(2) main does not hold the task's n.txt"
old_in_main=$(git --git-dir="$src" rev-list "$(main_now)" | grep -cxF -e "$m1" -e "$w" -e "$c" || true)
expect_eq "$old_in_main" 0 "(2) commits from before the rewrite (or on it) in main's history"
note "(2) done"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "194: $(wc -l <"$WORK/failures") checks failed" >&2
    exit 1
fi
echo "194: a commit built on the history before the rewrite is never pushed and its task is told once; the new history lands"
exit 0
