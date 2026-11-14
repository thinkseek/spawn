#!/bin/sh
# Objective 177 [recoverability]: the agents' commits, read through the
# history rewrite's commit map. 173 loads git filter-repo's commit map and
# reads the owner's requests, missions' baselines and gate bases through it;
# the rewrite covers every ref of the source, the agents' spawn/<agent>
# branches included, and the integrator still compared what an agent
# recorded before it (the checkpoint a report names, the agent's latest
# checkpoint, a cancelled delivery's start and set-aside, an earlier task's
# work, a parked task's pushed commit, a red main) by its old id: a task
# reported before the rewrite was judged `stale` on every pass ("the pushed
# branch (W') does not contain the report's checkpoint (W)"), and never
# landed. Now those readers read the recorded id through the map
# (`spawn_commit_now`), and a checkpoint that neither the map nor the source
# holds is named an unknown commit.
#
# Black box, through `spawn task assign`, `spawn integrate --once` and
# `spawn commit-map load`, on a source the scenario rewrites as filter-repo
# would: the root m0 unchanged, m1 rewritten to m1', the agent's work W (on
# m1, its spawn/<agent> head) rewritten to W' (parent m1'); the old objects
# stay in the source; main and spawn/<agent> move to the new history. Before
# the rewrite: main moved to m1 by an owner's landing, and the agent's task
# reported naming W as its checkpoint, W its latest checkpoint (as the
# Bridge records them).
#   (1) the map loaded, one pass: the task is judged on W' and lands: it is
#       `landed`, main holds W's file, main's history holds neither m1 nor W,
#       and no `stale` verdict names the task.
#   (2) a second task reported naming U, a commit in neither the map nor the
#       source, its branch pushed on the new main: its verdict is `stale`
#       and says "the report's checkpoint U is an unknown commit (in neither
#       the commit map nor the source)".
#   (3) a third task's work D (on m1, pushed as its spawn/<agent> before the
#       rewrite, and the head of the agent's branch in this host's mirror, as
#       the Bridge's workspace leaves it) is dropped by the rewrite (the map
#       names no new id; its spawn/<agent> moves to m1'); its checkpoint and
#       its report name D, so the source's branch lacks the recorded
#       checkpoint, the case the integrator's re-push of a lost checkpoint is
#       for. One pass: the re-push refuses D (objective 177 R2, the lead's
#       ruling on rev-g's F1): a `rewrite_refused` alert names the writer
#       (push-again), the task and D; no ref of the source reaches D or m1;
#       the task is not landed, its verdict `stale`, and main still holds
#       nothing from before the rewrite.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on the base (173, e0bf18d8): (1) the task is judged `stale`,
# "the pushed branch (W') does not contain the report's checkpoint (W)";
# (3) "rewrite_refused alerts naming push-again, task 2 and D: expected
# [1], got [0]". Without the refusal (mutant push-again-unguarded) it fails
# with the same line.

set -eu

asserted=""
harness() {
    echo "177: harness: $*" >&2
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
SPAWN_DB_NAME="bb_177_agent_commits_$run"
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
        echo "177: the integrator's last lines:" >&2
        grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 20 >&2 || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "177: harness: unexpected exit $rc" >&2
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

note() { echo "177: $*" >&2; }
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

capture "$SPAWN_BIN" mission add --text "177 rewrite" --source "$src" --gate "$WORK/ctl/rsi/scenarios" --accept gate
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
# move to it); the agent's task reports W, its work on m1.
m1=$(commit m1)
push "$m1" "m1, before the rewrite"
integrate
[ "$(main_now)" = "$m1" ] || harness "the owner's m1 did not land before the rewrite: main is $(main_now)"
task_for w1
task_w=$task
agent_w=$agent
w=$(work_of w.txt)
checkpoint "$task_w" "$agent_w" "$w"
reported "$task_w" "$w"
# The third task's work D, on m1: pushed as its spawn/<agent>, and the head
# of the agent's branch in the integrator's mirror (the Bridge's workspace
# commits there; its checkpoint and report come after (2), so the passes of
# (1) and (2) never judge it).
task_for w3
task_d=$task
agent_d=$agent
d=$(work_of d.txt)
mirror=""
for candidate in "$WORK/integrator/.sources/"*.git; do
    [ "$candidate" = "$WORK/integrator/.sources/scratch.git" ] || mirror=$candidate
done
[ -n "$mirror" ] && [ -d "$mirror" ] || harness "the integrator has no mirror of the source"
git --git-dir="$mirror" fetch -q "$src" "+refs/heads/spawn/$agent_d:refs/heads/$agent_d" ||
    harness "D was not fetched into the mirror's branch of $agent_d"

# --- the rewrite, as filter-repo would --------------------------------------------------------
tree_of() { git --git-dir="$src" rev-parse "$1^{tree}"; }
rewrite() { # <commit> <new parent>: the commit again, its tree and message, a new id
    GIT_COMMITTER_DATE="2026-11-14T12:00:00Z" GIT_AUTHOR_DATE="2026-11-14T12:00:00Z" \
        git --git-dir="$src" commit-tree "$(tree_of "$1")" -p "$2" -m "$(git --git-dir="$src" log -1 --format=%s "$1") (rewritten)"
}
m1n=$(rewrite "$m1" "$m0")
wn=$(rewrite "$w" "$m1n")
git --git-dir="$src" update-ref refs/heads/main "$m1n" "$m1"
git --git-dir="$src" update-ref "refs/heads/spawn/$agent_w" "$wn" "$w"
# D became empty and was dropped: its branch moves to its rewritten parent.
git --git-dir="$src" update-ref "refs/heads/spawn/$agent_d" "$m1n" "$d"
# Every other ref on m1 (the owner's landing request's) moves to m1' too.
git --git-dir="$src" for-each-ref --format='%(refname) %(objectname)' | while read -r name id; do
    [ "$id" = "$m1" ] || continue
    git --git-dir="$src" update-ref "$name" "$m1n" "$m1" || exit 1
done || harness "the refs on m1 were not moved to m1'"
printf 'old                                      new\n%s %s\n%s %s\n%s %s\n%s %s\n' \
    "$m0" "$m0" "$m1" "$m1n" "$w" "$wn" "$d" 0000000000000000000000000000000000000000 \
    >"$WORK/commit-map"
git --git-dir="$src" cat-file -e "$w^{commit}" || harness "the old W is not kept in the source"
capture "$SPAWN_BIN" commit-map load "$WORK/commit-map" --source "$src"
[ "$status" -eq 0 ] || harness "spawn commit-map load exited $status: $(err | head -c 300)"

# --- (1) the task reported before the rewrite -------------------------------------------------
passes=0
while [ "$passes" -lt 3 ]; do
    integrate
    passes=$((passes + 1))
    [ "$(q "select state from mission_tasks where id = $task_w")" = landed ] && break
done
last=$(gated_of "$task_w")
expect_eq "$(q "select state from mission_tasks where id = $task_w")" landed \
    "(1) the task reported before the rewrite, after $passes pass(es) (its last verdict: $(printf '%s' "$last" | jq -c '{verdict, reason}'))"
git --git-dir="$src" cat-file -e "$(main_now):w.txt" 2>/dev/null ||
    check_failed "(1) main does not hold the task's w.txt"
old_in_main=$(git --git-dir="$src" rev-list "$(main_now)" | grep -cxF -e "$m1" -e "$w" || true)
expect_eq "$old_in_main" 0 "(1) commits from before the rewrite in main's history (m1, W)"
expect_eq "$(q "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'task.gated' and kind -> 'detail' -> 'record' ->> 'id' = '$task_w' and kind -> 'detail' -> 'record' ->> 'verdict' = 'stale'")" \
    0 "(1) stale verdicts of the task reported before the rewrite"
note "(1) done"

# --- (2) a report naming a commit nobody holds ------------------------------------------------
task_for w2
work_of u.txt >/dev/null
unknown=1111111111111111111111111111111111111111
reported "$task" "$unknown"
integrate
last=$(gated_of "$task")
expect_eq "$(printf '%s' "$last" | jq -r '.verdict // ""')" stale "(2) the verdict of a report naming an unknown commit"
printf '%s' "$last" | jq -r '.reason // ""' |
    grep -qF "the report's checkpoint $unknown is an unknown commit (in neither the commit map nor the source)" ||
    check_failed "(2) the stale verdict does not name the unknown commit: $(printf '%s' "$last" | jq -r '.reason // ""' | head -c 300)"
note "(2) done"

# --- (3) a lost checkpoint from before the rewrite is never pushed again ---------------------
checkpoint "$task_d" "$agent_d" "$d"
reported "$task_d" "$d"
integrate
expect_eq "$(q "select count(*) from spawn_alerts where kind = 'rewrite_refused' and key = 'rewrite_refused:push-again:$task_d:$d'")" \
    1 "(3) rewrite_refused alerts naming push-again, task $task_d and D"
expect_eq "$(q "select coalesce(evidence ->> 'commit', '') || ' ' || coalesce(evidence ->> 'writer', '') from spawn_alerts where key = 'rewrite_refused:push-again:$task_d:$d'")" \
    "$d push-again" "(3) the refusal's evidence (commit, writer)"
old_in_refs=$(git --git-dir="$src" rev-list --all | grep -cxF -e "$d" -e "$m1" || true)
expect_eq "$old_in_refs" 0 "(3) commits from before the rewrite any ref of the source reaches (D, m1)"
expect_eq "$(git --git-dir="$src" rev-parse "refs/heads/spawn/$agent_d")" "$m1n" "(3) the dropped work's spawn/<agent>"
expect_eq "$(q "select state from mission_tasks where id = $task_d")" reported "(3) the task whose checkpoint was dropped"
last=$(gated_of "$task_d")
expect_eq "$(printf '%s' "$last" | jq -r '.verdict // ""')" stale "(3) the verdict of the task whose checkpoint was dropped"
old_in_main=$(git --git-dir="$src" rev-list "$(main_now)" | grep -cxF -e "$m1" -e "$w" -e "$d" || true)
expect_eq "$old_in_main" 0 "(3) commits from before the rewrite in main's history (m1, W, D)"
note "(3) done"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "177: $(wc -l <"$WORK/failures") checks failed" >&2
    exit 1
fi
echo "177: the integrator reads the agents' commits recorded before the rewrite through the commit map"
exit 0
