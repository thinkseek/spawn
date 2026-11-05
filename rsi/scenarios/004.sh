#!/bin/sh
# Black box: the integrator lands what the pusher published (docs/storage.md):
# the source's `refs/heads/spawn/<agent>`, never the worker's
# mounted workspace, and never a pushed branch that lags the Bridge's latest
# checkpoint.
#
# One local source repository with `main`, one gated mission on it (accept =
# gate), and a recording gate (SPAWN_GATE) that accepts every candidate. Real
# Bridges (`spawn tick`, a fake Claude engine) do the work, each on its own
# workspace root ("host"); their pusher publishes `spawn/<agent>` to the
# source. Tasks are reported through the dispatcher's own SQL bookkeeping.
#
#   (1) Work done on another host lands. Agent X works on host C, which is
#       then deleted. The integrator runs on a fresh root that never held X:
#       the task lands, the source's main carries X's file, and the gate
#       judged a candidate with the pushed branch's tree.
#   (2) A pushed branch that lags is stale. Agent Y makes checkpoints C1 then
#       C2 on host A; the source's spawn/Y is moved back to C1 (a pusher that
#       has not caught up). On the fresh root and on host A alike, nothing
#       lands, main does not move, the task goes back to `reported` (verdict
#       `stale`, not a rejection), and host A's own branch of Y is untouched.
#   (3) The mounted workspace is not the candidate. While Y is mounted on
#       host A with a file written after its last checkpoint, spawn/Y catches
#       up to C2 and the integrator on host A lands exactly C2: Y's file at
#       its C2 content, and not the unpushed file.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on an integrator that looks for the agent's branch in
# its own mirror only. Runs inside the gate's sandbox
# (scripts/rsi/gate.sh): needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates
# and drops its own database, uses a private tmux socket, no network. Every
# wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "004: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od sed; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_004_pushed_$run"
SPAWN_TMUX_SOCKET="bb-004-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# psql reaches the test server the way check-blackbox.sh does.
url=$SPAWN_TEST_DATABASE_URL
rest=${url#*://}
credentials=${rest%%@*}
address=${rest#*@}
address=${address%%/*}
PGUSER=${credentials%%:*}
PGPASSWORD=${credentials#*:}
case "$address" in
*:*) host=${address%:*} PGPORT=${address##*:} ;;
*) host=$address PGPORT=5432 ;;
esac
PGHOST=$(printf '%s' "$host" | sed 's/%2[Ff]/\//g')
export PGUSER PGPASSWORD PGHOST PGPORT

database_made=""
cleanup() {
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
    if [ -n "$database_made" ]; then
        psql -d postgres -tA -c "drop database if exists $SPAWN_DB_NAME with (force)" \
            >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "004: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

# An assertion exits 1, unless the database is gone: then nothing the
# scenario read can be trusted, and that is the harness's failure.
fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}

# --- the world ----------------------------------------------------------------

# git reads this config, not the user's: an identity, no signing.
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

source=$WORK/source.git
default=$WORK/default.git
git init -q --bare --initial-branch=main "$source"
git init -q --bare --initial-branch=main "$default"
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git -C "$WORK/seed" push -q "$source" main:main
git -C "$WORK/seed" push -q "$default" main:main
mkdir -p "$WORK/gate"
: >"$WORK/judged.log"

# The recording gate: accepts every candidate and notes what it judged.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
printf '%s %s\n' "\$2" "\$3" >>'$WORK/judged.log'
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
chmod 755 "$WORK/fake-gate.sh"

# The fake engine: a typed `put <file> <content> exit|stay` writes the file
# (logging it outside the workspace), then exits or waits for more.
cat >"$WORK/claude" <<EOF
#!/bin/sh
printf 'Welcome to Claude Code\n\n'
printf '❯ '
while IFS= read -r line; do
  case "\$line" in
  put\ *)
    set -- \${line#put }
    printf '%s\n' "\$2" >"\$1" || exit 3
    printf '%s %s\n' "\$1" "\$2" >>'$WORK/engine.log'
    [ "\$3" = exit ] && exit 0
    printf '\nwrote %s\n❯ ' "\$1" ;;
  *)
    printf '\n%s\n❯ ' "echo: \$line" ;;
  esac
done
EOF
chmod 755 "$WORK/claude"

cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_EXEC_IMAGE SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL \
    SPAWN_SUPERVISED SPAWN_PUSH_SOURCES SPAWN_CHUNK_STORE SPAWN_GATE CLAUDE_CONFIG_DIR; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="${url%/*}/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$default
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER

host_c=$WORK/host-c
host_a=$WORK/host-a
fresh=$WORK/integrator
mkdir -p "$host_c" "$host_a" "$fresh"

# The test server is shared: another run's init-db can collide on cluster-wide
# objects ("tuple concurrently updated"). init-db is idempotent.
tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- helpers ----------------------------------------------------------------

now() { date +%s; }
q() { scalar "$1"; }

# tick_on <root>: one Bridge pass on that host.
tick_on() {
    capture env SPAWN_WORKSPACE_ROOT="$1" "$SPAWN_BIN" tick
    [ "$status" -eq 0 ] || fail "tick on $1 exited $status: $(err)"
    [ -s "$WORK/err.txt" ] && cat "$WORK/err.txt" >>"$WORK/ticks.log"
    return 0
}

# submit <agent> <session> <key> <action>
submit() {
    capture "$SPAWN_BIN" submit --agent "$1" --session "$2" --key "$3" --action "$4"
    [ "$status" -eq 0 ] || fail "submit $3 exited $status: $(err)"
}

command_ok() {
    state=$(q "select state || ' ' || coalesce(succeeded::text, '') || ' ' || coalesce(error, '')
               from spawn_commands where idempotency_key = '$1'")
    case "$state" in
    "finished true "*) ;;
    *"pane never became ready"* | *"does not accept input"* | *"terminal disappeared"*)
        harness "$2: $state" ;;
    *) fail "$2: $state" ;;
    esac
}

ended() {
    q "select count(*) from agent_executions where session_id = '$1' and ended_at_ms is not null"
}

# tick_until_ended <root> <session> <n>
tick_until_ended() {
    deadline=$(($(now) + 180))
    while :; do
        tick_on "$1"
        [ "$(ended "$2")" -ge "$3" ] && return 0
        [ "$(now)" -lt "$deadline" ] ||
            harness "execution $3 of $2 did not end within 180 s; tick warnings: $(tail -n 5 "$WORK/ticks.log" 2>/dev/null)"
        sleep 0.25
    done
}

# run_exec <root> <agent> <session> <key> <action> <n>: one execution that
# ends by itself (the engine exits after its `put ... exit`).
run_exec() {
    submit "$2" "$3" "$4" "$5"
    tick_on "$1"
    command_ok "$4" "$4"
    tick_until_ended "$1" "$3" "$6"
}

engine_wrote() { grep -qx "$1" "$WORK/engine.log" 2>/dev/null; }

# assign <worker>: an ephemeral task of the mission; prints its id.
assign() {
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$1" \
        --brief "work of $1"
    [ "$status" -eq 0 ] || harness "task assign failed: $(err)"
    jq -r .task "$WORK/out.txt"
}

task_agent() { q "select agent_id from mission_tasks where id = $1"; }
task_session() { q "select session_id from mission_tasks where id = $1"; }
task_state() { q "select state from mission_tasks where id = $1"; }

# report <task> <start key>: the dispatcher's bookkeeping, through its own
# SQL functions: delivered by the start command, then reported.
report() {
    command=$(q "select id from spawn_commands where idempotency_key = '$2'")
    [ "$(q "select spawn_task_delivered($1, $command)")" = t ] || harness "task $1 not delivered"
    [ "$(q "select spawn_task_reported($1, 'done')")" = t ] || harness "task $1 not reported"
}

integrate_on() {
    capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        "$SPAWN_BIN" integrate --once --root "$1"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || fail "integrate on $1 exited $status: $(err)"
    return 0
}

src_ref() { git --git-dir="$source" rev-parse --verify --quiet "$1" || echo none; }
tree_of() { git --git-dir="$source" rev-parse --verify --quiet "$1^{tree}" || echo none; }
main_file() { git --git-dir="$source" show "main:$1" 2>/dev/null || echo missing; }
latest_checkpoint() {
    q "select kind -> 'detail' -> 'artifact' ->> 'commit' from spawn_events
       where agent_id = '$1' and event_name = 'artifact.committed' order by id desc limit 1"
}
# The agent's branch in host A's mirror of the source.
host_branch() {
    for mirror in "$host_a"/.sources/*.git; do
        [ "$(basename "$mirror")" = scratch.git ] && continue
        git --git-dir="$mirror" rev-parse --verify --quiet "refs/heads/$1" && return 0
    done
    echo none
}
judged_tree() {
    candidate=$(tail -n 1 "$WORK/judged.log" 2>/dev/null | cut -d' ' -f2)
    [ -n "$candidate" ] || { echo none; return 0; }
    git --git-dir="$source" rev-parse --verify --quiet "$candidate^{tree}" 2>/dev/null ||
        for mirror in "$fresh"/.sources/*.git "$host_a"/.sources/*.git; do
            git --git-dir="$mirror" rev-parse --verify --quiet "$candidate^{tree}" 2>/dev/null &&
                return 0
        done
}
log_tail() { tail -n 6 "$WORK/integrate.log" 2>/dev/null | tr '\n' ' '; }

capture "$SPAWN_BIN" mission add --text "004 pushed branches" --source "$source" \
    --gate "$WORK/gate" --accept gate
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
main0=$(src_ref refs/heads/main)

# --- (1) work done on another host lands from spawn/<agent> -------------------

task_x=$(assign x)
agent_x=$(task_agent "$task_x")
session_x=$(task_session "$task_x")
run_exec "$host_c" "$agent_x" "$session_x" kx1 \
    '{"type":"start","engine":"claude","prompt":"put X.txt from-x exit"}' 1
engine_wrote "X.txt from-x" || harness "the engine did not write X.txt"
pushed_x=$(src_ref "refs/heads/spawn/$agent_x")
checkpoint_x=$(latest_checkpoint "$agent_x")
[ -n "$checkpoint_x" ] || fail "(1) the Bridge recorded no checkpoint of $agent_x"
assert_eq "$pushed_x" "$checkpoint_x" "(1) the pusher published X's last checkpoint as spawn/$agent_x"
# Host C goes away: its workspace root, its mirror, everything.
rm -rf "$host_c"

report "$task_x" kx1
integrate_on "$fresh"
assert_eq "$(task_state "$task_x")" landed \
    "(1) task X, pushed from a host that is gone, judged by an integrator that never held it (integrate: $(log_tail))"
assert_eq "$(main_file X.txt)" from-x "(1) X.txt on the source's main"
assert_eq "$(tree_of refs/heads/main)" "$(tree_of "$pushed_x")" "(1) main has the pushed branch's tree"
assert_eq "$(judged_tree)" "$(tree_of "$pushed_x")" "(1) the gate judged the pushed branch's tree"
main1=$(src_ref refs/heads/main)
[ "$main1" != "$main0" ] || fail "(1) main did not move"

# --- (2) a pushed branch that lags the last checkpoint is stale ---------------

task_y=$(assign y)
agent_y=$(task_agent "$task_y")
session_y=$(task_session "$task_y")
run_exec "$host_a" "$agent_y" "$session_y" ky1 \
    '{"type":"start","engine":"claude","prompt":"put Y.txt one exit"}' 1
c1=$(latest_checkpoint "$agent_y")
run_exec "$host_a" "$agent_y" "$session_y" ky2 \
    '{"type":"resume","prompt":"put Y.txt two exit"}' 2
engine_wrote "Y.txt two" || harness "the engine did not rewrite Y.txt"
c2=$(latest_checkpoint "$agent_y")
[ -n "$c1" ] && [ -n "$c2" ] && [ "$c1" != "$c2" ] ||
    fail "(2) Y's two executions did not make two checkpoints ($c1, $c2)"
assert_eq "$(src_ref "refs/heads/spawn/$agent_y")" "$c2" "(2) the pusher published C2"
assert_eq "$(host_branch "$agent_y")" "$c2" "(2) host A's branch of Y is at C2"

# Y is mounted on host A again, with a file written after its last checkpoint.
submit "$agent_y" "$session_y" ky3 '{"type":"resume","prompt":"put UNPUSHED.txt local stay"}'
tick_on "$host_a"
command_ok ky3 "third execution of Y"
deadline=$(($(now) + 60))
until engine_wrote "UNPUSHED.txt local"; do
    [ "$(now)" -lt "$deadline" ] || harness "the engine never wrote UNPUSHED.txt"
    sleep 0.25
done
[ -f "$host_a/$agent_y/UNPUSHED.txt" ] || harness "UNPUSHED.txt is not in Y's mounted workspace"
assert_eq "$(latest_checkpoint "$agent_y")" "$c2" "(2) no checkpoint since C2"

# The pusher has not caught up: spawn/Y is still at C1. (Set after the last
# tick: a tick's pusher would publish C2 again.)
git --git-dir="$source" update-ref "refs/heads/spawn/$agent_y" "$c1" "$c2"

report "$task_y" ky1
judged_before=$(wc -l <"$WORK/judged.log" | tr -d ' ')
integrate_on "$fresh"
assert_eq "$(src_ref refs/heads/main)" "$main1" \
    "(2) main moved while spawn/$agent_y lags the last checkpoint (fresh root; integrate: $(log_tail))"
assert_eq "$(task_state "$task_y")" reported "(2) a lagging push is stale, the task stays reported (fresh root)"
integrate_on "$host_a"
assert_eq "$(src_ref refs/heads/main)" "$main1" \
    "(2) main moved while spawn/$agent_y lags the last checkpoint (host A; integrate: $(log_tail))"
assert_eq "$(task_state "$task_y")" reported "(2) a lagging push is stale, the task stays reported (host A)"
assert_eq "$(host_branch "$agent_y")" "$c2" "(2) the integrator left host A's own branch of Y alone"
assert_eq "$(q "select revisions from mission_tasks where id = $task_y")" 0 \
    "(2) a stale task is not sent back as a rejection"

# --- (3) the pusher catches up; the mounted workspace is not the candidate ----

git --git-dir="$source" update-ref "refs/heads/spawn/$agent_y" "$c2" "$c1"
integrate_on "$host_a"
assert_eq "$(task_state "$task_y")" landed "(3) task Y once spawn/$agent_y caught up (integrate: $(log_tail))"
assert_eq "$(main_file Y.txt)" two "(3) Y.txt on main is the content of the last checkpoint"
assert_eq "$(main_file UNPUSHED.txt)" missing \
    "(3) a file only in the mounted workspace, never checkpointed or pushed, must not land"
assert_eq "$(main_file X.txt)" from-x "(3) X's landing is still on main"
assert_eq "$(judged_tree)" "$(tree_of "$c2")" "(3) the gate judged C2's tree"
[ "$(wc -l <"$WORK/judged.log" | tr -d ' ')" -gt "$judged_before" ] || fail "(3) the gate never ran for Y"

echo "004: landed from spawn/<agent> on a fresh root, refused a lagging push as stale, ignored the mounted workspace"
exit 0
