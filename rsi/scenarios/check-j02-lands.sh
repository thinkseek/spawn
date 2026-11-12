#!/bin/sh
# Journey j02, lands: objective work lands, once per task, from what the
# pusher published, with a message that says what landed.
#
# Folds Spawn scenarios 009, 010, 014 and 004 (design-gate-journeys-spawn.md
# §2), every numbered check of each:
#   009 (1) N=3 (`--workers 3`), a backlog of 4: three objective tasks in
#           flight on 3 distinct agents, one of them the resident, intervals
#           overlap, never more than 3 open, never two for one objective,
#           the fourth assigned once a slot is free; all 4 land once and main
#           holds the four files.
#   009 (2) N=1, the default: one after another on the resident; all 3 land.
#   010 (1) a report made in the turn of the work, (2) a report made in a
#           turn of its own after the checkpoint (slowed by 4 s) was pushed:
#           each lands with no `rejected` verdict, against the checkpoint of
#           the turn that wrote the file, never "changes nothing".
#   014 (1)-(5) main's first-parent line is one commit per task, its message
#           names the task, the objective, and the report's summary; its
#           tree is the gated candidate's; `task.landed` names it; the
#           resident's checkpoints stay reachable.
#   004 (1)-(3) work pushed from another host lands from spawn/<agent>; a
#           lagging push is stale; a lost push of the recorded checkpoint is
#           made again, and a file only in the mounted workspace never lands.
#
# How the parts share setup:
#   Section L, one database, one tmux socket, one `spawn loop`: 009's
#     missions (3 then 1 workers), 014's mission (resident scribe) and 010's
#     gated mission (ephemeral workers one, two). 009 (1), 014 and 010 run
#     side by side; 009 (2) runs after 009 (1), as in 009 (its barrier
#     settings differ). One engine serves all: the prompt's marker picks
#     009's (`BB:TASK`, the barrier), 014's (`BB:SUM`), or 010's (`BB:ONE`,
#     `BB:TWO`) behaviour. 010's slow checkpoint (a `git` first in PATH
#     that adds 4 s to `spawn checkpoint ...` commits) applies to commits in
#     010's source mirror only, so 009 and 014 keep their own conditions.
#   Section 004, side by side with L, in a database of its own: it drives
#     the Bridge by `spawn tick` on several workspace roots ("hosts") and
#     reports through the dispatcher's SQL, which a running loop's
#     dispatcher would race, and its recording gate logs another format.
# The control records (spawn_events) and mission_* tables are read where
# the folded scenarios read them, with their assertions.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
# Named check-*: the gate runs a failing check again, and on the base if it fails twice (design-gate-v2.md C2)
#
# Needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own
# databases, uses private tmux sockets, no network. Every wait is bounded in
# time.
set -eu

asserted=""
harness() {
    echo "j02: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME, and the tmux server's socket, in WORK: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$HOME" "$TMUX_TMPDIR"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME TMUX_TMPDIR
# The runtime directory too (backlog S20): what Spawn keeps there (a
# launch's environment file, its terminals' leaders) stays in WORK, never
# in the real one. systemd's user manager stays reachable by its bus.
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_j02_lands_$run"
SPAWN_TMUX_SOCKET="bb-j02-$run"
DB004="bb_j02_pushed_$run"
SOCKET004="bb-j02p-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

. "$controller/scripts/blackbox/assertions.sh"

# An assertion exits 1, unless the database is gone: then nothing the
# journey read can be trusted, and that is the harness's failure.
fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}

database_made=""
loop_pid=""
s004_pid=""
cleanup() {
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
    fi
    if [ -n "$s004_pid" ]; then
        kill "$s004_pid" 2>/dev/null || true
        wait "$s004_pid" 2>/dev/null || true
    fi
    for socket in "$SPAWN_TMUX_SOCKET" "$SOCKET004"; do
        tmux -L "$socket" kill-server >/dev/null 2>&1 || true
    done
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    if [ -n "$s004_pid" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$DB004" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "j02: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "j02: the integrator's last lines:" >&2
            tail -n 15 "$WORK/integrate.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "j02: the gate verdicts:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select r ->> 'id', r ->> 'verdict', left(coalesce(r ->> 'reason', ''), 200)
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control' and r ->> 'type' = 'task.gated'
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "j02: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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

now() { date +%s; }
q() { scalar "$1"; }

# =============================================================================
# Section 004: the second Bridge root (scenario 004), in a database of its
# own, run in a subshell beside section L. Its body is 004's.
# =============================================================================
section_004() {
    WORK=$WORK/s004
    mkdir -p "$WORK"
    SPAWN_DB_NAME=$DB004
    SPAWN_TMUX_SOCKET=$SOCKET004
    export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET
    asserted=""
    database_made=""
    harness() {
        echo "j02: [004] harness: $*" >&2
        exit 2
    }
    s004_finish() {
        rc=$?
        trap - EXIT
        if [ "$rc" -ne 0 ] && [ -s "$WORK/integrate.log" ]; then
            echo "j02: [004] the integrator's last lines:" >&2
            tail -n 15 "$WORK/integrate.log" >&2
        fi
        tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
        if [ -n "$database_made" ]; then
            sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
        fi
        if [ "$rc" -eq 0 ]; then exit 0; fi
        if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
        [ "$rc" -eq 2 ] || echo "j02: [004] harness: unexpected exit $rc" >&2
        exit 2
    }
    trap s004_finish EXIT
    trap 'exit 130' INT TERM

    sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
    psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
        harness "cannot create the scenario database"
    database_made=1

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

    # The fake engine: a typed `put <file> <content> exit|stay` writes the
    # file (logging it outside the workspace), then exits or waits for more.
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

    for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_GATE CLAUDE_CONFIG_DIR; do
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

    host_c=$WORK/host-c
    host_a=$WORK/host-a
    fresh=$WORK/integrator
    mkdir -p "$host_c" "$host_a" "$fresh"

    tries=0
    while :; do
        capture "$SPAWN_BIN" init-db
        [ "$status" -eq 0 ] && break
        tries=$((tries + 1))
        [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
        sleep 1
    done

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
    # run_exec <root> <agent> <session> <key> <action> <n>
    run_exec() {
        submit "$2" "$3" "$4" "$5"
        tick_on "$1"
        command_ok "$4" "$4"
        tick_until_ended "$1" "$3" "$6"
    }
    engine_wrote() { grep -qx "$1" "$WORK/engine.log" 2>/dev/null; }
    assign() {
        capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$1" \
            --brief "work of $1"
        [ "$status" -eq 0 ] || harness "task assign failed: $(err)"
        jq -r .task "$WORK/out.txt"
    }
    task_agent() { q "select agent_id from mission_tasks where id = $1"; }
    task_session() { q "select session_id from mission_tasks where id = $1"; }
    task_state() { q "select state from mission_tasks where id = $1"; }
    # report <task> <start key>: the dispatcher's bookkeeping, through its
    # own SQL functions: delivered by the start command, then reported.
    report() {
        command=$(q "select id from spawn_commands where idempotency_key = '$2'")
        [ "$(q "select spawn_task_delivered($1, $command)")" = t ] || harness "task $1 not delivered"
        [ "$(q "select spawn_task_reported($1, 'done')")" = t ] || harness "task $1 not reported"
    }
    integrate_on() {
        capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
            SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
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
    host_mirror() {
        for mirror in "$host_a"/.sources/*.git; do
            [ "$(basename "$mirror")" = scratch.git ] && continue
            git --git-dir="$mirror" rev-parse --verify --quiet "refs/heads/$1" >/dev/null &&
                echo "$mirror" && return 0
        done
        harness "host A holds no branch of $1"
    }
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

    echo "j02: [004 (1)] work done on another host lands from spawn/<agent>"
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

    echo "j02: [004 (2)] a pushed branch that lags the recorded checkpoint is stale"
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

    # The push of C2 was lost: spawn/Y is still at C1. (Set after the last
    # tick: a tick's pusher would publish C2 again.)
    git --git-dir="$source" update-ref "refs/heads/spawn/$agent_y" "$c1" "$c2"
    # Host A's branch of Y is past C2 at a commit with no record: not the
    # recorded checkpoint, so it is not pushed.
    mirror_a=$(host_mirror "$agent_y")
    c3=$(git --git-dir="$mirror_a" commit-tree "$c2^{tree}" -p "$c2" -m "no record") ||
        harness "could not make an unrecorded commit on host A"
    git --git-dir="$mirror_a" update-ref "refs/heads/$agent_y" "$c3" "$c2" ||
        harness "could not move host A's branch of Y past C2"

    report "$task_y" ky1
    judged_before=$(wc -l <"$WORK/judged.log" | tr -d ' ')
    integrate_on "$fresh"
    assert_eq "$(src_ref refs/heads/main)" "$main1" \
        "(2) main moved while spawn/$agent_y lags the last checkpoint (fresh root; integrate: $(log_tail))"
    assert_eq "$(task_state "$task_y")" reported "(2) a lagging push is stale, the task stays reported (fresh root)"
    integrate_on "$host_a"
    assert_eq "$(src_ref refs/heads/main)" "$main1" \
        "(2) main moved while host A's branch of $agent_y is not the recorded checkpoint (host A; integrate: $(log_tail))"
    assert_eq "$(task_state "$task_y")" reported \
        "(2) a lagging push is stale while no host holds the recorded checkpoint (host A)"
    assert_eq "$(src_ref "refs/heads/spawn/$agent_y")" "$c1" \
        "(2) a branch head that is not the recorded checkpoint was pushed"
    assert_eq "$(host_branch "$agent_y")" "$c3" "(2) the integrator left host A's own branch of Y alone"
    assert_eq "$(q "select revisions from mission_tasks where id = $task_y")" 0 \
        "(2) a stale task is not sent back as a rejection"

    echo "j02: [004 (3)] the lost push of the recorded checkpoint is made again; the mounted workspace is not the candidate"
    git --git-dir="$mirror_a" update-ref "refs/heads/$agent_y" "$c2" "$c3" ||
        harness "could not move host A's branch of Y back to C2"
    integrate_on "$host_a"
    assert_eq "$(src_ref "refs/heads/spawn/$agent_y")" "$c2" \
        "(3) the recorded checkpoint C2 was pushed again from host A (integrate: $(log_tail))"
    assert_eq "$(task_state "$task_y")" landed "(3) task Y once C2 was pushed again (integrate: $(log_tail))"
    assert_eq "$(main_file Y.txt)" two "(3) Y.txt on main is the content of the last checkpoint"
    assert_eq "$(main_file UNPUSHED.txt)" missing \
        "(3) a file only in the mounted workspace, never checkpointed or pushed, must not land"
    assert_eq "$(main_file X.txt)" from-x "(3) X's landing is still on main"
    assert_eq "$(judged_tree)" "$(tree_of "$c2")" "(3) the gate judged C2's tree"
    [ "$(wc -l <"$WORK/judged.log" | tr -d ' ')" -gt "$judged_before" ] || fail "(3) the gate never ran for Y"

    echo "j02: [004] landed from spawn/<agent> on a fresh root, refused a lagging push as stale, pushed a lost recorded checkpoint again, ignored the mounted workspace"
    exit 0
}

( section_004 ) &
s004_pid=$!

# =============================================================================
# Section L: one database, one loop: 009, 010 and 014.
# =============================================================================

# Logins of this journey's databases that a run killed before its cleanup
# left behind (scripts/blackbox/scenario-db.sh): this prefix only.
sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

# --- the world ----------------------------------------------------------------

repo=$WORK/repo.git
src3=$WORK/src3.git
src1=$WORK/src1.git
src14=$WORK/src14.git
src10=$WORK/src10slow.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
for bare in "$repo" "$src3" "$src1" "$src14" "$src10"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done
base=$(git --git-dir="$src14" rev-parse main)

# 010's slow checkpoint: the Bridge's commit of a workspace of 010's source
# (its mirror, .sources/src10slow-<hash>.git) takes 4 s more. Each one is
# noted, so the journey knows the condition held.
real_git=$(command -v git)
mkdir -p "$WORK/bin"
: >"$WORK/slow.log"
cat >"$WORK/bin/git" <<EOF
#!/bin/sh
slow=""
mirror=""
for argument in "\$@"; do
  case "\$argument" in
  --git-dir=*/src10slow-*) mirror=1 ;;
  "spawn checkpoint "*) slow=1 ;;
  esac
done
if [ -n "\$slow" ] && [ -n "\$mirror" ]; then
  date +%s >>'$WORK/slow.log'
  sleep 4
fi
exec '$real_git' "\$@"
EOF
chmod 755 "$WORK/bin/git"
PATH=$WORK/bin:$PATH
export PATH

# The missions' scenarios directory: the recording gate below judges, this
# only has to exist and hold a scenario (the integrator counts them).
gate=$WORK/gate
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
: >"$WORK/judged.log"

# The recording gate (009, 010, 014): accepts every candidate and notes
# what it judged.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
printf '%s %s %s\n' "\$(date +%s)" "\$2" "\$3" >>'$WORK/judged.log'
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
chmod 755 "$WORK/fake-gate.sh"

# Backlogs: 009's four independent objectives for N=3, the first three for
# N=1; 014's two objectives with titles of their own (marker BB:SUM).
objective() {
    printf '{"id": %s, "title": "Write %s", "prompt": "Write the file %s.\\nBB:TASK %s %s\\nNothing else."}\n' \
        "$2" "$3" "$3" "$3" "$4" >"$1/$2.json"
}
objective14() {
    printf '{"id": %s, "title": "%s", "prompt": "Write the file %s.\\nBB:SUM %s %s\\nNothing else."}\n' \
        "$2" "$3" "$4" "$4" "$5" >"$1/$2.json"
}
mkdir -p "$WORK/backlog3" "$WORK/backlog1" "$WORK/backlog14"
objective "$WORK/backlog3" 901 ALPHA.txt alpha
objective "$WORK/backlog3" 902 BRAVO.txt bravo
objective "$WORK/backlog3" 903 CHARLIE.txt charlie
objective "$WORK/backlog3" 904 DELTA.txt delta
objective "$WORK/backlog1" 901 ALPHA.txt alpha
objective "$WORK/backlog1" 902 BRAVO.txt bravo
objective "$WORK/backlog1" 903 CHARLIE.txt charlie
objective14 "$WORK/backlog14" 931 "Greet the harbour with a kestrel" HARBOUR.txt kestrel
objective14 "$WORK/backlog14" 932 "Mark the lighthouse with an osprey" LIGHT.txt osprey

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet. It writes its session transcript where Claude Code does,
# so the Bridge and the Observer see turns as they see a real engine's.
#   BB:TASK <file> <word>  (009) notes its arrival, waits at the barrier
#       (until $BB_WORK/barrier tasks have arrived, or $BB_WORK/hold seconds
#       pass), writes the file, ends the turn, and writes the report in a
#       second turn once its checkpoint is pushed (or 30 s passed).
#   BB:SUM <file> <word>   (014) writes the file and ends; once pushed (or
#       30 s passed), a second turn writes the report, whose `## What
#       changed` says `summary: <file> now holds <word>`.
#   BB:ONE <file> <word>   (010) the turn writes the file and the report.
#   BB:TWO <file> <word>   (010) the turn writes the file and ends; once
#       pushed (or 60 s passed), a second turn writes the report alone.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=$2 ;; esac
  shift
done
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
# turn_ends <text>: the screen settles at the prompt first, then the
# transcript ends the turn.
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1
  line '"type":"system","subtype":"turn_duration","durationMs":1'
}
# pushed <agent> <file>: a source's spawn/<agent> holds the file.
pushed() {
  for source in "$BB_WORK"/src*.git; do
    git --git-dir="$source" cat-file -e "refs/heads/spawn/$1:$2" 2>/dev/null && return 0
  done
  return 1
}
# until_pushed <file> <seconds>
until_pushed() {
  agent=agent_${SPAWN_SESSION_ID#session_}
  start=$(date +%s)
  until pushed "$agent" "$1"; do
    [ $(($(date +%s) - start)) -ge "$2" ] && break
    sleep 0.2
  done
}
# A turn of its own, open long enough for the Observer to see it working.
report_turn() {
  line '"type":"user","message":{"role":"user","content":"report"}'
  sleep 2
}
report() {
  mkdir -p .spawn
  printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$1" >.spawn/report.md
  printf 'reported %s %s %s\n' "$(date +%s)" "$SPAWN_SESSION_ID" "$2" >>"$BB_WORK/engine.log"
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
task=
while :; do
  if bb_read_line typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:TASK "*) [ -n "$task" ] || task="task ${typed#*BB:TASK }" ;;
    *"BB:SUM "*) [ -n "$task" ] || task="sum ${typed#*BB:SUM }" ;;
    *"BB:ONE "*) [ -n "$task" ] || task="one ${typed#*BB:ONE }" ;;
    *"BB:TWO "*) [ -n "$task" ] || task="two ${typed#*BB:TWO }" ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$task" ]; then
      set -- $task
      mode=$1
      file=$2
      word=$3
      case "$mode" in
      task)
        mkdir -p "$BB_WORK/arrived"
        : >"$BB_WORK/arrived/$SPAWN_SESSION_ID-$file"
        printf 'arrived %s %s %s\n' "$(date +%s)" "$SPAWN_SESSION_ID" "$file" >>"$BB_WORK/engine.log"
        start=$(date +%s)
        while :; do
          arrived=$(ls "$BB_WORK/arrived" | wc -l)
          [ "$arrived" -ge "$(cat "$BB_WORK/barrier")" ] && break
          [ $(($(date +%s) - start)) -ge "$(cat "$BB_WORK/hold")" ] && break
          sleep 0.2
        done
        printf '%s\n' "$word" >"$file"
        turn_ends "wrote $file"
        until_pushed "$file" 30
        report_turn
        report "$file" "$file"
        ;;
      sum)
        printf '%s\n' "$word" >"$file"
        printf 'wrote %s %s %s\n' "$(date +%s)" "$SPAWN_SESSION_ID" "$file" >>"$BB_WORK/engine.log"
        turn_ends "wrote $file"
        until_pushed "$file" 30
        report_turn
        report "summary: $file now holds $word" "$file"
        ;;
      one | two)
        printf '%s\n' "$word" >"$file"
        printf 'wrote %s %s %s\n' "$(date +%s)" "$SPAWN_SESSION_ID" "$file" >>"$BB_WORK/engine.log"
        if [ "$mode" = two ]; then
          turn_ends "wrote $file"
          until_pushed "$file" 60
          report_turn
        fi
        report "$file" "$file"
        ;;
      esac
      task=
    fi
    turn_ends idle
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# Free disk is not what this journey is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"

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

# barrier <tasks> <seconds>: what the fake engine waits for, from now on.
barrier() {
    rm -rf "$WORK/arrived"
    mkdir -p "$WORK/arrived"
    printf '%s\n' "$1" >"$WORK/barrier.tmp" && mv "$WORK/barrier.tmp" "$WORK/barrier"
    printf '%s\n' "$2" >"$WORK/hold.tmp" && mv "$WORK/hold.tmp" "$WORK/hold"
}

integrate_once() {
    capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || fail "integrate exited $status: $(err)"
    return 0
}

# 009: the time of a task's first control record of a type, or empty.
record_at() {
    q "select min(recorded_at_ms) from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = '$2'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
tasks_of() {
    sql "select id from mission_tasks where mission_id = $1 and objective is not null order by id"
}
open_tasks() {
    q "select count(*) from mission_tasks
        where mission_id = $1 and state not in ('landed', 'cancelled')"
}
doubled() {
    q "select count(*) from (select objective from mission_tasks
        where mission_id = $1 and objective is not null and state not in ('landed', 'cancelled')
        group by objective having count(*) > 1) d"
}
# Tasks of the mission ever delivered (a task.delivered record), whatever
# their state now (009's note: a poll can miss three at once in state
# delivered; that they were in flight together is checked from the records).
delivered_ever() {
    q "select count(distinct kind -> 'detail' -> 'record' ->> 'id') from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.delivered'
          and (kind -> 'detail' -> 'record' ->> 'id')::bigint in
              (select id from mission_tasks where mission_id = $1)"
}
landed() {
    q "select count(*) from mission_tasks where mission_id = $1 and state = 'landed'"
}
main_file() { git --git-dir="$1" show "main:$2" 2>/dev/null || echo missing; }
log_tail() { tail -n 6 "$WORK/integrate.log" 2>/dev/null | tr '\n' ' '; }
mission_line() {
    q "select string_agg(id || ':' || state || ':' || agent_id, ' ' order by id)
         from mission_tasks where mission_id = $1"
}
# watch <mission> <limit>: the invariants every poll checks.
watch() {
    open=$(open_tasks "$1")
    [ "$open" -le "$2" ] ||
        fail "$open tasks of mission $1 open at once (at most $2): $(mission_line "$1")"
    [ "$(doubled "$1")" -eq 0 ] ||
        fail "two open tasks for one objective in mission $1: $(mission_line "$1")"
    if [ "$open" -gt "$peak" ]; then peak=$open; fi
}
resident() {
    capture "$SPAWN_BIN" resident propose --name "$1" --charter "Pursues the 009 objectives."
    [ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$1"
    [ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
}

# 010: a control record of the task: its event id, or empty.
record_id() {
    q "select min(id) from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = '$2'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
reported_commit() {
    q "select kind -> 'detail' -> 'record' ->> 'commit' from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.reported'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'
        order by id desc limit 1"
}
checkpoint_id() {
    q "select min(id) from spawn_events
        where agent_id = '$1' and event_name = 'artifact.committed'
          and kind -> 'detail' -> 'artifact' ->> 'commit' = '$2'"
}
checkpoints() {
    sql "select kind -> 'detail' -> 'artifact' ->> 'commit' from spawn_events
          where agent_id = '$1' and event_name = 'artifact.committed' order by id"
}
rejections() {
    q "select count(*) from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
          and kind -> 'detail' -> 'record' ->> 'verdict' = 'rejected'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
task_state() { q "select state from mission_tasks where id = $1"; }
task_agent() { q "select agent_id from mission_tasks where id = $1"; }
holds() {
    git --git-dir="$src10" cat-file -e "$1:$2" 2>/dev/null && return 0
    for mirror in "$WORK"/integrator/.sources/*.git; do
        git --git-dir="$mirror" cat-file -e "$1:$2" 2>/dev/null && return 0
    done
    return 1
}
assign() {
    capture "$SPAWN_BIN" task assign --mission "$mission10" --kind ephemeral --worker "$1" \
        --brief "$2"
    [ "$status" -eq 0 ] || harness "task assign failed: $(err)"
    jq -r .task "$WORK/out.txt"
}
# 010's work, the poll part: <task> <deadline> <what>. Returns 0 once landed.
work_poll() {
    [ "$(rejections "$1")" -eq 0 ] ||
        fail "$3: the gate rejected task $1 ($(q "select kind -> 'detail' -> 'record' ->> 'reason' from spawn_events
            where kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
              and kind -> 'detail' -> 'record' ->> 'id' = '$1' order by id desc limit 1"))"
    [ "$(task_state "$1")" = landed ] && return 0
    [ "$(now)" -lt "$2" ] ||
        fail "$3: task $1 did not land within 150 s (state: $(task_state "$1"); integrate: $(log_tail))"
    return 1
}
# 010's work, the checks once landed: <task> <file> <word> <what>.
work_checks() {
    assert_eq "$(rejections "$1")" 0 "$4: rejections of task $1"
    assert_eq "$(main_file "$src10" "$2")" "$3" "$4: $2 on main"
    agent=$(task_agent "$1")
    commit=$(reported_commit "$1")
    [ -n "$commit" ] || fail "$4: task $1's task.reported record names no commit"
    at=$(checkpoint_id "$agent" "$commit")
    [ -n "$at" ] || fail "$4: task $1 reported commit $commit, which is no checkpoint of $agent"
    [ "$at" -lt "$(record_id "$1" task.reported)" ] ||
        fail "$4: task $1's checkpoint $commit was recorded after its report"
    holds "$commit" "$2" || fail "$4: task $1's reported checkpoint $commit does not hold $2"
}

# 014.
record_field() {
    q "select kind -> 'detail' -> 'record' ->> '$3' from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = '$2'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'
          and ('$4' = '' or kind -> 'detail' -> 'record' ->> 'verdict' = '$4')
        order by id desc limit 1"
}
mission_line14() {
    q "select string_agg(id || ':' || state || ':' || coalesce(objective::text, '-'), ' ' order by id)
         from mission_tasks where mission_id = $1"
}
tree_of() {
    git --git-dir="$src14" rev-parse --verify -q "$1^{tree}" 2>/dev/null && return 0
    for mirror in "$WORK"/integrator/.sources/*.git; do
        git --git-dir="$mirror" rev-parse --verify -q "$1^{tree}" 2>/dev/null && return 0
    done
    echo "missing:$1"
}
# The message names id <n> after <word>: `task 12`, `Task #12`, `objective 0931`.
names() {
    printf '%s\n' "$1" | grep -Eiq "$2[^0-9a-z]{0,3}0*$3([^0-9]|\$)"
}

# --- the missions -------------------------------------------------------------

capture "$SPAWN_BIN" mission add --text "009 three at once" --source "$src3"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
m3=$(jq -r .mission "$WORK/out.txt")
[ -n "$m3" ] && [ "$m3" != null ] || harness "mission add printed no id: $(out)"
resident wide

capture "$SPAWN_BIN" mission add --text "014 landing messages" --source "$src14"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission14=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission14" ] && [ "$mission14" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" resident propose --name scribe --charter "Pursues the 014 objectives."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name scribe
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"

capture "$SPAWN_BIN" mission add --text "010 reports and checkpoints" --source "$src10" \
    --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission10=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission10" ] && [ "$mission10" != null ] || harness "mission add printed no id: $(out)"

barrier 3 120

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

# --- phase 1: 009 (1), 014 and 010 side by side ------------------------------------

echo "j02: [009 (1)] N=3: mission configure --workers 3, a backlog of 4"
capture "$SPAWN_BIN" mission configure --id "$m3" --gate "$gate" --accept gate \
    --backlog "$WORK/backlog3" --resident wide --workers 3
[ "$status" -eq 0 ] || fail "(1) mission configure --workers 3 exited $status: $(err)"
peak=0
deadline=$(($(now) + 90))
phase009=deliver

capture "$SPAWN_BIN" mission configure --id "$mission14" --gate "$gate" --accept gate \
    --backlog "$WORK/backlog14" --resident scribe
[ "$status" -eq 0 ] || harness "mission configure exited $status: $(err)"
deadline14=$(($(now) + 240))

echo "j02: [010 (1)][010 (2)] a report in the work's turn, and a report in a turn of its own, under a 4 s checkpoint"
one=$(assign one "Write the file.
BB:ONE ONE.txt one")
deadline_one=$(($(now) + 150))
two=$(assign two "Write the file, then report.
BB:TWO TWO.txt two")
deadline_two=$(($(now) + 150))

echo "j02: [009 (1)] three objective tasks delivered within 90 s, never more than 3 open, never two for one objective; all 4 land within 180 s"
echo "j02: [014] both objectives land"
done14=""
done_one=""
done_two=""
while :; do
    if [ "$phase009" = deliver ]; then
        if [ "$(delivered_ever "$m3")" -ge 3 ]; then
            phase009=land
            deadline=$(($(now) + 180))
        elif [ "$(now)" -ge "$deadline" ]; then
            fail "(1) three objective tasks were not all delivered within 90 s (most open at once: $peak; tasks: $(mission_line "$m3"))"
        fi
    fi
    if [ "$phase009" = land ]; then
        if [ "$(landed "$m3")" -ge 4 ]; then
            phase009=done
        elif [ "$(now)" -ge "$deadline" ]; then
            fail "(1) the 4 objectives did not all land within 180 s (tasks: $(mission_line "$m3"); integrate: $(log_tail))"
        fi
    fi
    if [ -z "$done14" ]; then
        if [ "$(landed "$mission14")" -ge 2 ]; then
            done14=1
        elif [ "$(now)" -ge "$deadline14" ]; then
            harness "the 2 objectives did not both land within 240 s (tasks: $(mission_line14 "$mission14"); integrate: $(log_tail))"
        fi
    fi
    if [ -z "$done_one" ] && work_poll "$one" "$deadline_one" "(1) one turn"; then done_one=1; fi
    if [ -z "$done_two" ] && work_poll "$two" "$deadline_two" "(2) a report turn that changes nothing"; then
        done_two=1
    fi
    if [ "$phase009" = done ] && [ -n "$done14" ] && [ -n "$done_one" ] && [ -n "$done_two" ]; then
        break
    fi
    integrate_once
    watch "$m3" 3
    sleep 0.3
done
# A few more passes: nothing more to assign.
for _ in 1 2 3; do integrate_once; done

# --- 009 (1): the records and main --------------------------------------------------

echo "j02: [009 (1)] four tasks, one per objective; the first three on 3 agents, one the resident; intervals overlap; the fourth after a free slot; main holds four files; one landing each"
tasks=$(tasks_of "$m3" | tr '\n' ' ')
set -- $tasks
assert_eq "$#" 4 "(1) objective tasks of mission $m3, one per objective ($(mission_line "$m3"))"
assert_eq "$(q "select count(distinct objective) from mission_tasks where mission_id = $m3")" 4 \
    "(1) distinct objectives among the mission's tasks"
first3="$1, $2, $3"
assert_eq "$(q "select count(distinct agent_id) from mission_tasks where id in ($first3)")" 3 \
    "(1) distinct agents of the first three objective tasks ($(mission_line "$m3"))"
assert_eq "$(q "select count(*) from mission_tasks where id in ($first3)
                  and agent_id = 'agent_resident_wide'")" 1 \
    "(1) the first three tasks include exactly one on the mission's resident"
latest_delivery=0
first_report=""
for task in $1 $2 $3; do
    delivered=$(record_at "$task" task.delivered)
    reported=$(record_at "$task" task.reported)
    [ -n "$delivered" ] && [ -n "$reported" ] ||
        fail "(1) task $task has no delivered or reported record"
    [ "$delivered" -gt "$latest_delivery" ] && latest_delivery=$delivered
    if [ -z "$first_report" ] || [ "$reported" -lt "$first_report" ]; then
        first_report=$reported
    fi
done
[ "$latest_delivery" -lt "$first_report" ] ||
    fail "(1) the three tasks' delivered-to-reported intervals do not overlap (last delivery at $latest_delivery ms, first report at $first_report ms)"
# The fourth waited for a free slot: assigned after the first landing.
fourth_at=$(record_at "$4" task.assigned)
first_landing=$(q "select min(recorded_at_ms) from spawn_events
    where session_id = 'session_control'
      and kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
      and kind -> 'detail' -> 'record' ->> 'id' in ('$1', '$2', '$3')")
[ -n "$fourth_at" ] && [ -n "$first_landing" ] && [ "$fourth_at" -ge "$first_landing" ] ||
    fail "(1) the fourth objective was assigned before a slot was free (assigned $fourth_at, first landing $first_landing)"
assert_eq "$(main_file "$src3" ALPHA.txt)" alpha "(1) ALPHA.txt on main"
assert_eq "$(main_file "$src3" BRAVO.txt)" bravo "(1) BRAVO.txt on main"
assert_eq "$(main_file "$src3" CHARLIE.txt)" charlie "(1) CHARLIE.txt on main"
assert_eq "$(main_file "$src3" DELTA.txt)" delta "(1) DELTA.txt on main"
assert_eq "$(q "select count(*) from spawn_events where session_id = 'session_control'
                  and kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
                  and (kind -> 'detail' -> 'record' ->> 'id')::bigint in
                      (select id from mission_tasks where mission_id = $m3)")" 4 \
    "(1) landings of mission $m3, one per task"

# --- 010: each report against its turn's checkpoint ---------------------------------

[ -s "$WORK/slow.log" ] || harness "no checkpoint of 010's source was slowed: the condition did not hold"
echo "j02: [010 (1)] one turn: landed, no rejection, main holds the file, the report names a checkpoint recorded before it that holds the file"
work_checks "$one" ONE.txt one "(1) one turn"
echo "j02: [010 (2)] a report turn that changes nothing: the same, and the report names the first turn's checkpoint"
work_checks "$two" TWO.txt two "(2) a report turn that changes nothing"
agent_two=$(task_agent "$two")
first=$(checkpoints "$agent_two" | while read -r commit; do
    if holds "$commit" TWO.txt; then echo "$commit"; break; fi
done)
assert_eq "$(reported_commit "$two")" "$first" \
    "(2) task $two's report names the checkpoint of the turn that wrote TWO.txt"
echo "j02: [010] the mission's gate rejections stay 0, it never halts, ONE.txt stays on main"
assert_eq "$(q "select gate_rejections from mission_missions where id = $mission10")" 0 \
    "the mission's gate rejections"
assert_eq "$(q "select coalesce(halted, '') from mission_missions where id = $mission10")" "" \
    "the mission's halt"
assert_eq "$(main_file "$src10" ONE.txt)" one "ONE.txt still on main"

# --- 014: main's history --------------------------------------------------------------

tasks=$(q "select string_agg(id::text, ' ' order by id) from mission_tasks
            where mission_id = $mission14 and state = 'landed'")
set -- $tasks
[ "$#" -eq 2 ] || harness "expected 2 landed tasks, got: $(mission_line14 "$mission14")"
first_task=$1
second_task=$2
[ "$(q "select objective from mission_tasks where id = $first_task")" = 931 ] &&
    [ "$(q "select objective from mission_tasks where id = $second_task")" = 932 ] ||
    harness "the tasks are not objectives 931 then 932: $(mission_line14 "$mission14")"
main14=$(git --git-dir="$src14" rev-parse main)

echo "j02: [014 (1)] one commit per task on main's first-parent line"
line=$(git --git-dir="$src14" rev-list --first-parent "$base..$main14" | tr '\n' ' ')
set -- $line
[ "$#" -eq 2 ] ||
    fail "(1) main's first-parent line holds $# commits above the base after two landings, not 2: $(git --git-dir="$src14" log --first-parent --format='%h %s' "$base..$main14" | head -8 | tr '\n' ';')"
second_landing=$1
first_landing=$2
assert_eq "$(git --git-dir="$src14" rev-parse "$first_landing^1")" "$base" \
    "(1) the first landing's first parent (the base)"

echo "j02: [014 (2)][014 (3)][014 (4)] each landing's message, tree and record"
check() { # <commit> <task> <objective> <title> <summary> <other task> <other summary>
    message=$(git --git-dir="$src14" show -s --format=%B "$1")
    subject=$(printf '%s\n' "$message" | head -1)
    body=$(printf '%s\n' "$message" | tail -n +2)
    case "$subject" in
    "spawn checkpoint"*) fail "(2) the landing of task $2 reads like a checkpoint: [$subject]" ;;
    esac
    assert_contains "$subject" "$4" "(2) the first line of task $2's landing $1"
    names "$message" task "$2" ||
        fail "(2) the landing of task $2 does not name task $2: [$(printf '%s' "$message" | tr '\n' '|')]"
    names "$message" objective "$3" ||
        fail "(2) the landing of task $2 does not name objective $3: [$(printf '%s' "$message" | tr '\n' '|')]"
    assert_contains "$body" "$5" "(2) the body of task $2's landing (after its first line)"
    assert_not_contains "$message" "$7" "(2) the landing of task $2 (the other task's summary)"
    if names "$message" task "$6"; then
        fail "(2) the landing of task $2 names task $6 too: [$(printf '%s' "$message" | tr '\n' '|')]"
    fi
    candidate=$(record_field "$2" task.gated candidate accepted)
    [ -n "$candidate" ] || fail "(3) task $2 has no accepted task.gated record naming a candidate"
    assert_eq "$(tree_of "$1")" "$(tree_of "$candidate")" \
        "(3) the tree of task $2's landing $1 against its gated candidate $candidate"
    assert_eq "$(record_field "$2" task.landed to '')" "$1" "(4) task $2's task.landed 'to'"
}
check "$first_landing" "$first_task" 931 "Greet the harbour with a kestrel" \
    "summary: HARBOUR.txt now holds kestrel" "$second_task" "summary: LIGHT.txt now holds osprey"
check "$second_landing" "$second_task" 932 "Mark the lighthouse with an osprey" \
    "summary: LIGHT.txt now holds osprey" "$first_task" "summary: HARBOUR.txt now holds kestrel"
assert_eq "$(git --git-dir="$src14" show "$main14:HARBOUR.txt")" kestrel "HARBOUR.txt on main"
assert_eq "$(git --git-dir="$src14" show "$main14:LIGHT.txt")" osprey "LIGHT.txt on main"

echo "j02: [014 (5)] the resident's checkpoints stay reachable from main"
gated_at=$(q "select min(id) from spawn_events
    where session_id = 'session_control'
      and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
      and kind -> 'detail' -> 'record' ->> 'verdict' = 'accepted'
      and kind -> 'detail' -> 'record' ->> 'id' = '$second_task'")
checkpoints=$(sql "select kind -> 'detail' -> 'artifact' ->> 'commit' from spawn_events
    where agent_id = 'agent_resident_scribe' and event_name = 'artifact.committed'
      and id < $gated_at order by id")
[ -n "$checkpoints" ] || harness "the resident recorded no checkpoint"
for commit in $checkpoints; do
    git --git-dir="$src14" merge-base --is-ancestor "$commit" "$main14" 2>/dev/null ||
        fail "(5) the resident's checkpoint $commit is not reachable from main"
done

# --- phase 2: 009 (2) N=1, the default: one at a time ----------------------------------

echo "j02: [009 (2)] N=1, the default: never two open, each assigned after the one before landed, all 3 land within 240 s"
capture "$SPAWN_BIN" mission add --text "009 one at a time" --source "$src1"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
m1=$(jq -r .mission "$WORK/out.txt")
[ -n "$m1" ] && [ "$m1" != null ] || harness "mission add printed no id: $(out)"
resident narrow
# No barrier to fill: each task holds its worker 3 s, long enough for a
# second task, if one were delivered, to be caught in flight with it.
barrier 1000 3
capture "$SPAWN_BIN" mission configure --id "$m1" --gate "$gate" --accept gate \
    --backlog "$WORK/backlog1" --resident narrow
[ "$status" -eq 0 ] || fail "(2) mission configure without --workers exited $status: $(err)"

peak=0
deadline=$(($(now) + 240))
until [ "$(landed "$m1")" -ge 3 ]; do
    if [ "$(now)" -ge "$deadline" ]; then
        fail "(2) the 3 objectives did not all land within 240 s (tasks: $(mission_line "$m1"); integrate: $(log_tail))"
    fi
    integrate_once
    watch "$m1" 1
    sleep 0.5
done
for _ in 1 2 3; do integrate_once; done

echo "j02: [009 (2)] three tasks, intervals disjoint, the resident only, main holds the three files"
tasks=$(tasks_of "$m1" | tr '\n' ' ')
set -- $tasks
assert_eq "$#" 3 "(2) objective tasks of mission $m1 ($(mission_line "$m1"))"
previous_landed=""
previous_reported=""
for task in "$@"; do
    assigned=$(record_at "$task" task.assigned)
    delivered=$(record_at "$task" task.delivered)
    reported=$(record_at "$task" task.reported)
    [ -n "$delivered" ] && [ -n "$reported" ] ||
        fail "(2) task $task has no delivered or reported record"
    if [ -n "$previous_landed" ]; then
        [ "$assigned" -ge "$previous_landed" ] ||
            fail "(2) task $task was assigned before the task before it landed"
        [ "$delivered" -ge "$previous_reported" ] ||
            fail "(2) task $task was delivered while the task before it was in flight"
    fi
    previous_landed=$(record_at "$task" task.landed)
    previous_reported=$reported
done
assert_eq "$(q "select count(distinct agent_id) from mission_tasks where mission_id = $m1")" 1 \
    "(2) agents of mission $m1 (its resident only)"
assert_eq "$(main_file "$src1" ALPHA.txt)" alpha "(2) ALPHA.txt on main"
assert_eq "$(main_file "$src1" BRAVO.txt)" bravo "(2) BRAVO.txt on main"
assert_eq "$(main_file "$src1" CHARLIE.txt)" charlie "(2) CHARLIE.txt on main"

# --- section 004's verdict ------------------------------------------------------------

rc004=0
wait "$s004_pid" || rc004=$?
s004_pid=""
sh "$controller/scripts/blackbox/scenario-db.sh" drop "$DB004" >/dev/null 2>&1 || true
case "$rc004" in
0) ;;
1)
    asserted=1
    echo "j02: [004] an assertion failed (above)" >&2
    exit 1
    ;;
*) harness "section 004 exited $rc004" ;;
esac

echo "j02: N=3 kept three in flight and landed all four, N=1 one at a time; each report judged against its turn's checkpoint under a slow checkpoint; one first-parent commit per task that says what landed, checkpoints reachable; work pushed from another host landed, a lagging push was stale, the mounted workspace never landed"
exit 0
