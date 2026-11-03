#!/bin/sh
# `spawn evolve`, the RSI controller: the first call seeds rsi_state from
# refs/heads/main and opens iteration 1 on the lowest open backlog task, with
# one pending command; a second immediate call advances nothing because the
# Starting iteration's command is still pending.
#
# Every expected string and column below comes from
# crates/spawn-cli/src/evolve.rs and crates/spawn-postgres/src/rsi.rs.

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"

main_commit=$(git --git-dir="$SPAWN_GIT_REPO" rev-parse refs/heads/main)
case "$main_commit" in
    '' | *[!0-9a-f]*) fail "no main commit in $SPAWN_GIT_REPO: [$main_commit]" ;;
esac

# --- rejections write nothing ---------------------------------------------
# evolve resolves the repo and the root before it takes the controller lock
# or seeds rsi_state (evolve.rs:37-63), so both errors leave the tables empty.
capture env SPAWN_GIT_REPO= "$SPAWN_BIN" evolve
assert_eq "$status" 1 "evolve without a repo exit"
assert_contains "$(err)" "--repo or SPAWN_GIT_REPO is required" \
    "evolve without a repo message"
assert_eq "$(scalar 'select count(*) from rsi_state')" 0 \
    "evolve without a repo seeds nothing"

capture env SPAWN_WORKSPACE_ROOT= "$SPAWN_BIN" evolve
assert_eq "$status" 1 "evolve without a root exit"
assert_contains "$(err)" "--root or SPAWN_WORKSPACE_ROOT is required" \
    "evolve without a root message"
assert_eq "$(scalar 'select count(*) from rsi_state')" 0 \
    "evolve without a root seeds nothing"

# Flags::parse rejects a flag with no value and a bare word before dispatch.
capture "$SPAWN_BIN" evolve --repo
assert_eq "$status" 1 "flag without a value exit"
assert_contains "$(err)" "--repo needs a value" "flag without a value message"

capture "$SPAWN_BIN" evolve stray
assert_eq "$status" 1 "stray argument exit"
assert_contains "$(err)" "unexpected argument stray" "stray argument message"

assert_eq "$(scalar 'select count(*) from spawn_commands')" 0 \
    "rejections submit no command"
assert_eq "$(scalar 'select count(*) from rsi_iterations')" 0 \
    "rejections open no iteration"

# --- first call: seed from main and open iteration 1 ----------------------
# The runner points SPAWN_RSI_BACKLOG at the real backlog, whose lowest task
# id is 1 (rsi/backlog/001.json).
assert_file "$SPAWN_RSI_BACKLOG/001.json"

capture "$SPAWN_BIN" evolve
assert_eq "$status" 0 "first evolve exit"
assert_eq "$(out)" "evolve: iteration 1 starts task 1" "first evolve output"

# rsi_seed stores the head of refs/heads/main as the baseline.
assert_eq "$(scalar 'select count(*) from rsi_state')" 1 "one state row"
assert_eq "$(scalar 'select baseline_commit from rsi_state where id = 1')" \
    "$main_commit" "baseline is the head of refs/heads/main"
assert_eq "$(scalar 'select iteration from rsi_state where id = 1')" \
    1 "state iteration is 1"
assert_eq "$(scalar 'select consecutive_rejected from rsi_state where id = 1')" \
    0 "state starts with no rejection"

# rsi_begin writes one Starting iteration named after the iteration number.
assert_eq "$(scalar 'select count(*) from rsi_iterations')" 1 "one iteration row"
assert_eq "$(scalar 'select phase from rsi_iterations where iteration = 1')" \
    "Starting" "iteration phase"
assert_eq "$(scalar 'select task_id from rsi_iterations where iteration = 1')" \
    1 "iteration task is the lowest backlog id"
assert_eq "$(scalar 'select agent_id from rsi_iterations where iteration = 1')" \
    "agent_rsi" "iteration agent"
assert_eq "$(scalar 'select session_id from rsi_iterations where iteration = 1')" \
    "session_rsi" "iteration session"
assert_eq "$(scalar 'select command_id is not null from rsi_iterations where iteration = 1')" \
    "t" "iteration has a command"
assert_eq "$(scalar 'select patch is null from rsi_iterations where iteration = 1')" \
    "t" "no patch captured yet"
assert_eq "$(scalar 'select candidate is null from rsi_iterations where iteration = 1')" \
    "t" "no candidate committed yet"

# The iteration points at the one command rsi_begin submitted.
cmd_id=$(scalar 'select command_id from rsi_iterations where iteration = 1')
case "$cmd_id" in
    '' | *[!0-9]*) fail "iteration command_id is not numeric: [$cmd_id]" ;;
esac
assert_eq "$(scalar "select count(*) from spawn_commands where id = $cmd_id")" \
    1 "iteration command exists"

# spawn_submit wrote exactly one pending command under principal spawn.
assert_eq "$(scalar 'select count(*) from spawn_commands')" 1 "one command row"
assert_eq "$(scalar "select state from spawn_commands where id = $cmd_id")" \
    "pending" "command is pending"
assert_eq "$(scalar "select idempotency_key from spawn_commands where id = $cmd_id")" \
    "rsi-1-start" "command idempotency key"
assert_eq "$(scalar "select principal from spawn_commands where id = $cmd_id")" \
    "spawn" "command principal"
assert_eq "$(scalar "select agent_id from spawn_commands where id = $cmd_id")" \
    "agent_rsi" "command agent"
assert_eq "$(scalar "select session_id from spawn_commands where id = $cmd_id")" \
    "session_rsi" "command session"
assert_eq "$(scalar "select action ->> 'type' from spawn_commands where id = $cmd_id")" \
    "start" "command action type"
assert_eq "$(scalar "select action ->> 'engine' from spawn_commands where id = $cmd_id")" \
    "claude" "command engine"

# The prompt is the backlog task prompt plus the controller's completion
# instruction (evolve.rs:102-107).
prompt=$(sql "select action ->> 'prompt' from spawn_commands where id = $cmd_id")
assert_contains "$prompt" 'Add a `version` subcommand to the `spawn` binary.' \
    "prompt carries the backlog task"
assert_contains "$prompt" 'create the file `.spawn/done`' \
    "prompt carries the completion signal"
assert_contains "$prompt" 'Do not run git commands' \
    "prompt forbids commits"

# spawn_submit is the only writer of events here: one command.accepted.
assert_eq "$(scalar 'select count(*) from spawn_events')" 1 "one event total"
assert_eq "$(scalar 'select event_name from spawn_events')" \
    "command.accepted" "event is command.accepted"
assert_eq "$(scalar 'select source from spawn_events')" "gateway" "event source"
assert_eq "$(scalar 'select source_event_id from spawn_events')" \
    "cmd_${cmd_id}:accepted" "event source key"
assert_eq "$(scalar 'select count(*) from rsi_attempts')" 0 "no attempt recorded"

# The controller never moves the baseline just by starting an iteration.
assert_eq "$(git --git-dir="$SPAWN_GIT_REPO" rev-parse refs/heads/main)" \
    "$main_commit" "refs/heads/main is unchanged"

# --- second call: the Starting iteration waits on its pending command -----
# evolve.rs:150-152: a command that is not `finished` keeps the phase as is.
capture "$SPAWN_BIN" evolve
assert_eq "$status" 0 "second evolve exit"
assert_eq "$(out)" "" "second evolve prints nothing"
assert_eq "$(err)" "" "second evolve writes no error"

assert_eq "$(scalar 'select phase from rsi_iterations where iteration = 1')" \
    "Starting" "phase unchanged by the second call"
assert_eq "$(scalar 'select count(*) from rsi_iterations')" 1 \
    "second call opens no iteration"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 1 \
    "second call submits no command"
assert_eq "$(scalar "select state from spawn_commands where id = $cmd_id")" \
    "pending" "command is still pending"
assert_eq "$(scalar 'select task_id from rsi_iterations where iteration = 1')" \
    1 "task unchanged"
assert_eq "$(scalar 'select iteration from rsi_state where id = 1')" 1 \
    "state iteration unchanged"
assert_eq "$(scalar 'select count(*) from spawn_events')" 1 \
    "second call appends no event"
assert_eq "$(scalar 'select count(*) from rsi_state')" 1 "state row still one"
assert_eq "$(scalar 'select count(*) from rsi_attempts')" 0 \
    "second call records no attempt"
assert_eq "$(git --git-dir="$SPAWN_GIT_REPO" rev-parse refs/heads/main)" \
    "$main_commit" "second call leaves refs/heads/main unchanged"
