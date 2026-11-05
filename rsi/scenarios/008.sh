#!/bin/sh
# Black box: Spawn requires git 2.43, and a landing whose merge git cannot run
# is an environment error, never a content conflict (GitHub issue #5).
#
#   (a) The git floor. With a `git` first in PATH that answers `git version
#       2.42.0` and passes every other call to the real git, `spawn tick`
#       and `spawn integrate --once` refuse to start: a non-zero exit and a
#       message that names 2.43. Answering `git version 2.43.0`, and with the
#       real git, both run. README.md (in the tree under test) states the
#       requirement as "2.43 or later".
#   (b) A merge git cannot run. A gated mission's task is reported after the
#       source's main moved, so landing it needs a merge (`git --attr-source=
#       <main> merge-tree`). With a `git` that fails every `merge-tree` call
#       carrying `--attr-source` as git 2.40 does (exit 129, "unknown option")
#       and passes every other call on, one `spawn integrate --once` pass:
#       records no gate verdict for the task (no `task.gated`, so no
#       "conflict" sent back to its worker), leaves it `reported` with no
#       revision, and shows the operator git's own error (the pass's output
#       contains "unknown option"). A later pass with the real git lands it.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (a) where the floor is 2.40. Runs inside the gate's sandbox
# (scripts/rsi/gate.sh), from the root of the tree under test: needs
# SPAWN_BIN and SPAWN_TEST_DATABASE_URL, a real git 2.43 or later, creates
# and drops its own database, uses a private tmux socket, no network. Every
# wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "008: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
[ -f README.md ] || harness "run from the root of the tree under test (no README.md here)"
tree=$(pwd)

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od sed; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
real_git=$(command -v git)
real_version=$("$real_git" --version | sed -n 's/^git version \([0-9]*\)\.\([0-9]*\).*/\1 \2/p')
[ -n "$real_version" ] || harness "cannot read the real git's version"
set -- $real_version
[ "$1" -gt 2 ] || { [ "$1" -eq 2 ] && [ "$2" -ge 43 ]; } ||
    harness "the real git is $1.$2; this scenario needs 2.43 or later"

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_008_git_$run"
SPAWN_TMUX_SOCKET="bb-008-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

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
    [ "$rc" -eq 2 ] || echo "008: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}

# --- the world ----------------------------------------------------------------

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

# git_answering <directory> <version>: a git in <directory> that answers
# `--version` with <version> and passes everything else to the real git.
git_answering() {
    mkdir -p "$1"
    cat >"$1/git" <<EOF
#!/bin/sh
if [ "\$#" -eq 1 ] && [ "\$1" = --version ]; then
  echo 'git version $2'
  exit 0
fi
exec '$real_git' "\$@"
EOF
    chmod 755 "$1/git"
}
git_answering "$WORK/git-2.42" 2.42.0
git_answering "$WORK/git-2.43" 2.43.0

# A git without a working `--attr-source` for merge-tree, as git 2.40 is:
# exit 129 and its usage error; everything else goes to the real git.
mkdir -p "$WORK/git-no-attr-source"
cat >"$WORK/git-no-attr-source/git" <<EOF
#!/bin/sh
attr=""
merge=""
for argument in "\$@"; do
  case "\$argument" in
  --attr-source=*) attr=\$argument ;;
  merge-tree) merge=1 ;;
  esac
done
if [ -n "\$attr" ] && [ -n "\$merge" ]; then
  echo "unknown option: \$attr" >&2
  echo "usage: git [-v | --version] [-h | --help] [-C <path>] [-c <name>=<value>]" >&2
  exit 129
fi
exec '$real_git' "\$@"
EOF
chmod 755 "$WORK/git-no-attr-source/git"

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

# The fake engine: a typed `put <file> <content> exit` writes the file
# (logging it outside the workspace) and exits.
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

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_SUPERVISOR SPAWN_GATE \
    CLAUDE_CONFIG_DIR; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$default
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER

host=$WORK/host
integrator=$WORK/integrator
mkdir -p "$host" "$integrator"

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

# with_git <directory> <command...>: the command with that directory's git
# first in PATH.
with_git() {
    directory=$1
    shift
    capture env PATH="$directory:$PATH" "$@"
}

tick_on() {
    capture env SPAWN_WORKSPACE_ROOT="$1" "$SPAWN_BIN" tick
    [ "$status" -eq 0 ] || fail "tick on $1 exited $status: $(err)"
    return 0
}

ended() {
    q "select count(*) from agent_executions where session_id = '$1' and ended_at_ms is not null"
}

src_ref() { git --git-dir="$source" rev-parse --verify --quiet "$1" || echo none; }
main_file() { git --git-dir="$source" show "main:$1" 2>/dev/null || echo missing; }
task_state() { q "select state from mission_tasks where id = $task"; }
gated_records() {
    q "select count(*) from spawn_events
        where session_id = 'session_control' and event_name = 'control.recorded'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
          and kind -> 'detail' -> 'record' ->> 'id' = '$task'"
}

# --- (a) the git floor ----------------------------------------------------------

grep -q '2\.43 or later' README.md ||
    fail "(a) README.md does not state the git requirement as \"2.43 or later\": $(grep -n -i 'git' README.md | grep -i 'later' | head -n 3)"

with_git "$WORK/git-2.42" env SPAWN_WORKSPACE_ROOT="$host" "$SPAWN_BIN" tick
[ "$status" -ne 0 ] || fail "(a) spawn tick ran with git 2.42"
assert_contains "$(err)$(out)" "2.43" "(a) spawn tick's refusal of git 2.42"
with_git "$WORK/git-2.42" env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
    "$SPAWN_BIN" integrate --once --root "$integrator"
[ "$status" -ne 0 ] || fail "(a) spawn integrate ran with git 2.42"
assert_contains "$(err)$(out)" "2.43" "(a) spawn integrate's refusal of git 2.42"

with_git "$WORK/git-2.43" env SPAWN_WORKSPACE_ROOT="$host" "$SPAWN_BIN" tick
[ "$status" -eq 0 ] || fail "(a) spawn tick refused git 2.43: $(err)"
with_git "$WORK/git-2.43" env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
    "$SPAWN_BIN" integrate --once --root "$integrator"
[ "$status" -eq 0 ] || fail "(a) spawn integrate refused git 2.43: $(err)"
tick_on "$host"

# --- (b) a merge git cannot run is no conflict ------------------------------------

capture "$SPAWN_BIN" mission add --text "008 landing" --source "$source" \
    --gate "$WORK/gate" --accept gate
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker x \
    --brief "work of x"
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(jq -r .task "$WORK/out.txt")
agent=$(q "select agent_id from mission_tasks where id = $task")
session=$(q "select session_id from mission_tasks where id = $task")

capture "$SPAWN_BIN" submit --agent "$agent" --session "$session" --key kx1 \
    --action '{"type":"start","engine":"claude","prompt":"put X.txt from-x exit"}'
[ "$status" -eq 0 ] || harness "submit failed: $(err)"
deadline=$(($(now) + 180))
while [ "$(ended "$session")" -lt 1 ]; do
    tick_on "$host"
    [ "$(now)" -lt "$deadline" ] || harness "the worker's execution did not end within 180 s"
    sleep 0.25
done
grep -qx "X.txt from-x" "$WORK/engine.log" 2>/dev/null || harness "the engine did not write X.txt"
[ "$(src_ref "refs/heads/spawn/$agent")" != none ] || harness "the pusher did not publish spawn/$agent"

# The source's main moves on (a file the worker never touched): landing the
# task needs a merge.
git -C "$WORK/seed" pull -q "$source" main
printf 'other\n' >"$WORK/seed/OTHER.txt"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "main moves"
git -C "$WORK/seed" push -q "$source" main:main
moved=$(src_ref refs/heads/main)

command=$(q "select id from spawn_commands where idempotency_key = 'kx1'")
[ "$(q "select spawn_task_delivered($task, $command)")" = t ] || harness "task $task not delivered"
[ "$(q "select spawn_task_reported($task, 'done')")" = t ] || harness "task $task not reported"

with_git "$WORK/git-no-attr-source" env -u SPAWN_TEST_DATABASE_URL \
    SPAWN_GATE="$WORK/fake-gate.sh" "$SPAWN_BIN" integrate --once --root "$integrator"
output="$(out) $(err)"
assert_eq "$(gated_records)" 0 \
    "(b) gate verdicts recorded for task $task when git could not merge (integrate: $output)"
assert_eq "$(task_state)" reported "(b) task $task's state after a merge git could not run"
assert_eq "$(q "select revisions from mission_tasks where id = $task")" 0 \
    "(b) revisions of task $task after a merge git could not run"
assert_contains "$output" "unknown option" "(b) what the integrator showed of git's error"
assert_eq "$(src_ref refs/heads/main)" "$moved" "(b) the source's main after a failed merge"

capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
    "$SPAWN_BIN" integrate --once --root "$integrator"
assert_eq "$(task_state)" landed \
    "(b) task $task once git can merge again (integrate: $(out) $(err))"
assert_eq "$(main_file X.txt)" from-x "(b) X.txt on the source's main"
assert_eq "$(main_file OTHER.txt)" other "(b) main's own change kept by the landing"

echo "008: git 2.42 refused and 2.43 accepted; a merge git could not run was an error, not a conflict, and landed once git could"
exit 0
