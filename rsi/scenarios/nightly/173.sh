#!/bin/sh
# Objective 173 [recoverability][observability]: the commit map of a history
# rewrite. The owner rewrites the Spawn repository's history (every changed
# commit gets a new id); the event log keeps the old ids. Before 173 every
# reader that compares a recorded id with git compared the old one: the
# integrator read an owner's request recorded before the rewrite by its old
# commit, and with that object still in the source it merged the old
# history back into the rewritten main (owner.rs `apply`, site 11 of rev-b's
# survey). Now the owner loads git filter-repo's commit-map once (`spawn
# commit-map load <file> --source <git dir>`, owner only, never changed),
# and the readers read a recorded id through it (`spawn_commit_now`).
#
# Black box, through `spawn mission push`, `spawn integrate --once` and
# `spawn commit-map load`, on a source the scenario rewrites as filter-repo
# would: its root m0 unchanged, m1 rewritten to m1', an empty commit E
# dropped, X rewritten to X' (parent m1'); the old objects stay in the
# source (no gc); the source's main and refs/spawn-owner/<X> move to the new
# history (the ref keeps its name). Before the rewrite: the mission's main
# moved to m1 by an owner's landing; a task gate-accepted while the owner
# holds the source (its work W on m0, so its candidate C is the integrator's
# merge of W into m1, under no ref of the source: the rewrite does not map
# it), its landing waiting behind the hold, released after the rewrite; and
# the owner's request for X (on E on m1) recorded, not yet applied.
#   (1) the map loaded, one pass: the request is applied as X': main is X'
#       (a fast-forward of m1'), `source.moved` from m1' to X', main's history
#       holds no old commit (none of m1, E, X), and the mission is not halted
#       and its baseline is X'. The task accepted before the rewrite is
#       never pushed: its verdict is `stale`, naming why ("the candidate C
#       does not descend from its base m1'", or "the base m1' is an unknown
#       commit ..." while the integrator's mirror has not fetched it), C is
#       not in main's history, and the task is `reported` (judged again on
#       its checkpoint).
#   (2) a request for Z, a commit on the old X made after the rewrite (no
#       map entry): refused, "would bring 3 commit(s) from before the history
#       rewrite onto main", main unchanged.
#   (3) a request for E itself (the map names it dropped): refused,
#       "was dropped by the history rewrite", main unchanged.
#   (4) the load again with the same file: "the same 4 pair(s) are loaded
#       already"; a different map: exit 1, "a different commit map is
#       loaded"; a map naming a new commit the source does not hold: exit 1,
#       "not commits of", nothing loaded; the table still 4 pairs.
#   (5) no record of the event log changed: every record before the load is
#       byte-for-byte as it was after (1) to (4).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on the base (52f62475): no `spawn commit-map`, and the
# request's old X is merged into main ((1): main's history holds m1).

set -eu

asserted=""
harness() {
    echo "173: harness: $*" >&2
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
SPAWN_DB_NAME="bb_173_commit_map_$run"
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
        echo "173: the integrator's last lines:" >&2
        grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 20 >&2 || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "173: harness: unexpected exit $rc" >&2
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

note() { echo "173: $*" >&2; }
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

capture "$SPAWN_BIN" mission add --text "173 rewrite" --source "$src" --gate "$WORK/ctl/rsi/scenarios" --accept gate
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
record_of() { # <type> <request>: the latest such record's JSON
    q "select kind -> 'detail' -> 'record' from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = '$1' and (kind -> 'detail' -> 'record' ->> 'request')::bigint = $2 order by id desc limit 1"
}
main_now() { git --git-dir="$src" rev-parse refs/heads/main; }

# Before the rewrite: the owner lands m1 (the mission's main and baseline
# move to it), then asks for X on an empty E, not applied yet.
m1=$(commit m1)
push "$m1" "m1, before the rewrite"
integrate
[ "$(main_now)" = "$m1" ] || harness "the owner's m1 did not land before the rewrite: main is $(main_now)"
baseline=$(q "select baseline from mission_missions where id = $mission")
[ "$baseline" = "$m1" ] || harness "the mission's baseline is [$baseline], not m1 $m1, before the rewrite"
# A task gate-accepted before the rewrite, its landing held: its work W is
# on m0, so its candidate is the integrator's merge of W into m1, a commit
# under no ref of the source, which the rewrite does not map.
capture "$SPAWN_BIN" mission hold --source "$src" --reason "the rewrite"
[ "$status" -eq 0 ] || harness "mission hold failed: $(err)"
capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker w1 --brief "work of w1"
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(q "select max(id) from mission_tasks where mission_id = $mission")
agent=$(q "select agent_id from mission_tasks where id = $task")
git clone -q "$src" "$WORK/agent"
git -C "$WORK/agent" checkout -q "$m0"
printf 'work\n' >"$WORK/agent/w.txt"
git -C "$WORK/agent" add -A
git -C "$WORK/agent" commit -qm "work of w1"
git -C "$WORK/agent" push -q origin "HEAD:refs/heads/spawn/$agent"
w=$(git -C "$WORK/agent" rev-parse HEAD)
q "select spawn_task_delivered($task, 1)" >/dev/null
[ "$(q "select spawn_task_reported($task, 'done', '$w')")" = t ] || harness "task $task's report was not taken"
integrate
[ "$(q "select state from mission_tasks where id = $task")" = landing ] ||
    harness "task $task is not landing behind the hold before the rewrite: $(q "select state from mission_tasks where id = $task")"
candidate=$(q "select candidate from mission_tasks where id = $task")
[ -n "$candidate" ] && [ "$candidate" != "$w" ] || harness "task $task's candidate [$candidate] is not a merge of its work"
e=$(commit E --allow-empty)
x=$(commit X)
push "$x" "X, recorded before the rewrite"
request_x=$request

# --- the rewrite, as filter-repo would --------------------------------------------------------
tree_of() { git --git-dir="$src" rev-parse "$1^{tree}"; }
rewrite() { # <commit> <new parent>: the commit again, its tree and message, a new id
    GIT_COMMITTER_DATE="2026-11-14T12:00:00Z" GIT_AUTHOR_DATE="2026-11-14T12:00:00Z" \
        git --git-dir="$src" commit-tree "$(tree_of "$1")" -p "$2" -m "$(git --git-dir="$src" log -1 --format=%s "$1") (rewritten)"
}
m1n=$(rewrite "$m1" "$m0")
xn=$(rewrite "$x" "$m1n")
git --git-dir="$src" update-ref refs/heads/main "$m1n" "$m1"
git --git-dir="$src" update-ref "refs/spawn-owner/$x" "$xn"
zeros=0000000000000000000000000000000000000000
printf 'old                                      new\n%s %s\n%s %s\n%s %s\n%s %s\n' \
    "$m0" "$m0" "$m1" "$m1n" "$e" "$zeros" "$x" "$xn" >"$WORK/commit-map"
git --git-dir="$src" cat-file -e "$x^{commit}" || harness "the old X is not kept in the source"
events_before=$(q "select md5(string_agg(id || ':' || kind::text, ',' order by id)) from spawn_events")
last_before=$(q "select max(id) from spawn_events")

capture "$SPAWN_BIN" mission release --source "$src"
[ "$status" -eq 0 ] || harness "mission release failed: $(err)"

capture "$SPAWN_BIN" commit-map load "$WORK/commit-map" --source "$src"
if [ "$status" -ne 0 ]; then
    check_failed "(1) spawn commit-map load exited $status: $(err | head -c 300)"
else
    grep -qF "commit map: 4 pair(s) loaded (1 dropped commit(s))" "$WORK/out.txt" ||
        check_failed "(1) the load does not say what it loaded: $(head -c 300 "$WORK/out.txt")"
fi

# --- (1) the request recorded before the rewrite ----------------------------------------------
integrate
expect_eq "$(main_now)" "$xn" "(1) main after the request recorded with the old X"
moved=$(record_of source.moved "$request_x")
expect_eq "$(printf '%s' "$moved" | jq -c '{from, to}')" "{\"from\":\"$m1n\",\"to\":\"$xn\"}" \
    "(1) the request's source.moved"
old_in_main=$(git --git-dir="$src" rev-list "$(main_now)" | grep -cxF -e "$m1" -e "$e" -e "$x" || true)
expect_eq "$old_in_main" 0 "(1) commits from before the rewrite in main's history (m1, E, X)"
expect_eq "$(q "select coalesce(halted, 'not halted') || ' ' || coalesce(baseline, 'none') from mission_missions where id = $mission")" \
    "not halted $xn" "(1) the mission after the pass"
# The task accepted before the rewrite: never pushed, stale, naming why.
gated=$(q "select kind -> 'detail' -> 'record' from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'task.gated' and kind -> 'detail' -> 'record' ->> 'id' = '$task' order by id desc limit 1")
expect_eq "$(printf '%s' "$gated" | jq -r '.verdict // ""')" stale "(1) the verdict of the task accepted before the rewrite"
# Why, as the integrator's mirror sees it at the landing: the candidate off
# m1''s line, or m1' not fetched into the mirror yet (an unknown commit).
printf '%s' "$gated" | jq -r '.reason // ""' |
    grep -qxF -e "the candidate $candidate does not descend from its base $m1n" \
        -e "the base $m1n is an unknown commit (in neither the commit map nor the source)" ||
    check_failed "(1) the stale verdict does not say the candidate left its base's line: $(printf '%s' "$gated" | head -c 400)"
in_main=$(git --git-dir="$src" rev-list "$(main_now)" | grep -cxF "$candidate" || true)
expect_eq "$in_main" 0 "(1) the candidate accepted before the rewrite in main's history"
expect_eq "$(q "select state from mission_tasks where id = $task")" reported "(1) the task accepted before the rewrite, after the pass"
# Out of the way of (2) and (3): its next gating would land W on main.
capture "$SPAWN_BIN" task cancel --id "$task"
[ "$status" -eq 0 ] || harness "task cancel failed: $(err)"
note "(1) done"

# --- (2) a commit made on the old history after the rewrite -----------------------------------
z=$(commit Z)
push "$z" "Z on the old X"
main_before=$(main_now)
integrate
refused=$(record_of source.push_refused "$request")
printf '%s' "$refused" | jq -r '.reason // ""' | grep -qF "would bring 3 commit(s) from before the history rewrite onto main" ||
    check_failed "(2) the request for Z is not refused for its old commits: $(printf '%s' "$refused" | head -c 400)"
expect_eq "$(main_now)" "$main_before" "(2) main after the refused Z"
note "(2) done"

# --- (3) the dropped commit -----------------------------------------------------------------
push "$e" "E, dropped by the rewrite"
integrate
refused=$(record_of source.push_refused "$request")
printf '%s' "$refused" | jq -r '.reason // ""' | grep -qF "was dropped by the history rewrite" ||
    check_failed "(3) the request for the dropped E is not refused so: $(printf '%s' "$refused" | head -c 400)"
expect_eq "$(main_now)" "$main_before" "(3) main after the refused E"
note "(3) done"

# --- (4) loading again --------------------------------------------------------------------------
capture "$SPAWN_BIN" commit-map load "$WORK/commit-map" --source "$src"
expect_eq "$status" 0 "(4) the same map again: its exit"
grep -qF "the same 4 pair(s) are loaded already" "$WORK/out.txt" ||
    check_failed "(4) the same map again does not say so: $(head -c 300 "$WORK/out.txt") $(err | head -c 200)"
sed "s/$xn/$m1n/" "$WORK/commit-map" >"$WORK/other-map"
capture "$SPAWN_BIN" commit-map load "$WORK/other-map" --source "$src"
expect_eq "$status" 1 "(4) a different map: its exit"
grep -qF "a different commit map is loaded" "$WORK/err.txt" ||
    check_failed "(4) a different map is not refused so: $(err | head -c 300)"
printf '%s %s\n' "$(printf '1%.0s' $(seq 1 40))" "$(printf '2%.0s' $(seq 1 40))" >"$WORK/absent-map"
capture "$SPAWN_BIN" commit-map load "$WORK/absent-map" --source "$src"
expect_eq "$status" 1 "(4) a map naming a commit the source does not hold: its exit"
grep -qF "not commits of" "$WORK/err.txt" ||
    check_failed "(4) a map naming an absent commit is not refused so: $(err | head -c 300)"
expect_eq "$(q "select count(*) from spawn_commit_map")" 4 "(4) the map's pairs after the refusals"
note "(4) done"

# --- (5) the event log ----------------------------------------------------------------------------
expect_eq "$(q "select md5(string_agg(id || ':' || kind::text, ',' order by id)) from spawn_events where id <= $last_before")" \
    "$events_before" "(5) the records written before the load"
note "(5) done"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "173: $(wc -l <"$WORK/failures") checks failed" >&2
    exit 1
fi
echo "173: the commit map is loaded once and the integrator reads the requests recorded before the rewrite through it"
exit 0
