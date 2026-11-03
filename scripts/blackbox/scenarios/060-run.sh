#!/bin/sh
# `spawn run -- <program>` (cmd_run in crates/spawn-cli/src/main.rs).
#
# The runner starts the program, waits for it and returns its exit code. When
# SPAWN_DATABASE_URL, SPAWN_AGENT_ID, SPAWN_SESSION_ID and SPAWN_EXECUTION_ID
# all resolve it also appends one execution.ended event for the execution.
# A missing store or a malformed identity is ignored: the program still runs
# and its exit code is still forwarded.

run_ident() {
    env SPAWN_ENGINE=claude SPAWN_AGENT_ID=agent_bb SPAWN_SESSION_ID=session_bb \
        SPAWN_EXECUTION_ID=exec_bb "$@"
}

run_no_ident() {
    env SPAWN_ENGINE=claude SPAWN_AGENT_ID= SPAWN_SESSION_ID= SPAWN_EXECUTION_ID= "$@"
}

# --- the program is required ------------------------------------------------

capture "$SPAWN_BIN" run
assert_eq "$status" 1 "run without a program exit"
assert_contains "$(err)" "spawn: run needs a program after --" \
    "run without a program message"

capture "$SPAWN_BIN" run --
assert_eq "$status" 1 "run with an empty -- exit"
assert_contains "$(err)" "spawn: run needs a program after --" \
    "run with an empty -- message"

# --- no identity: the program still runs and its code is forwarded ----------

capture run_no_ident "$SPAWN_BIN" run -- sh -c 'printf "engine-out\n"; exit 7'
assert_eq "$status" 7 "exit code without identity variables"
assert_eq "$(out)" "engine-out" "stdout forwarded without identity variables"
assert_eq "$(err)" "" "no error without identity variables"

# --- no store at all: an empty SPAWN_DATABASE_URL counts as unset -----------

capture env SPAWN_DATABASE_URL= SPAWN_ENGINE=claude \
    SPAWN_AGENT_ID= SPAWN_SESSION_ID= SPAWN_EXECUTION_ID= \
    "$SPAWN_BIN" run -- sh -c 'printf "no-store-out\n"; exit 0'
assert_eq "$status" 0 "exit code without a store"
assert_eq "$(out)" "no-store-out" "stdout forwarded without a store"
assert_eq "$(err)" "" "no error without a store"

# --- identity set but the schema is missing: append fails softly ------------

capture run_ident "$SPAWN_BIN" run -- sh -c 'exit 7'
assert_eq "$status" 7 "exit code when the schema is missing"
assert_eq "$(err)" "" "missing schema does not surface as an error"

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"

# --- the full path: run, forward stdout, record the real exit code ----------

capture run_ident "$SPAWN_BIN" run -- sh -c 'printf "engine-said-hi\n"; exit 7'
assert_eq "$status" 7 "exit code with identity variables"
assert_eq "$(out)" "engine-said-hi" "stdout forwarded with identity variables"
assert_eq "$(err)" "" "no error on the recorded path"

assert_eq "$(scalar 'select count(*) from spawn_events')" 1 \
    "exactly one event was appended"
assert_eq "$(scalar "select kind->>'name' from spawn_events")" "execution.ended" \
    "event name is execution.ended"
assert_eq "$(sql "select source || '|' || source_event_id || '|' || execution_id
                     || '|' || agent_id || '|' || session_id
                     || '|' || (kind->'detail'->>'cause')
                     || '|' || (kind->'detail'->>'exit_code')
              from spawn_events")" \
    "claude|exec_bb:exited|exec_bb|agent_bb|session_bb|exited|7" \
    "execution.ended carries source, key, identities, cause and exit code"

# --- no execution id: nothing is appended -----------------------------------

capture env SPAWN_ENGINE=claude SPAWN_AGENT_ID=agent_bb SPAWN_SESSION_ID=session_bb \
    SPAWN_EXECUTION_ID= "$SPAWN_BIN" run -- sh -c 'exit 3'
assert_eq "$status" 3 "exit code without SPAWN_EXECUTION_ID"
assert_eq "$(scalar 'select count(*) from spawn_events')" 1 \
    "no event without SPAWN_EXECUTION_ID"

# --- malformed identity: ignored, program runs, nothing is appended ---------

capture env SPAWN_ENGINE=claude SPAWN_AGENT_ID=agent_bb SPAWN_SESSION_ID=session_bb \
    SPAWN_EXECUTION_ID=bad "$SPAWN_BIN" run -- sh -c 'exit 4'
assert_eq "$status" 4 "exit code with a malformed execution id"
assert_eq "$(err)" "" "malformed execution id is not an error"
assert_eq "$(scalar 'select count(*) from spawn_events')" 1 \
    "no event with a malformed execution id"

# --- the (session, source, key) store deduplicates a repeated exit ----------

capture run_ident "$SPAWN_BIN" run -- sh -c 'exit 9'
assert_eq "$status" 9 "exit code on the repeated execution"
assert_eq "$(scalar 'select count(*) from spawn_events')" 1 \
    "a repeated exec exit key is stored once"
assert_eq "$(scalar "select kind->'detail'->>'exit_code' from spawn_events")" 7 \
    "the stored exit code stays the first one"

# --- a program that cannot start fails before touching the store ------------

capture run_ident "$SPAWN_BIN" run -- "$WORK/no-such-program"
assert_eq "$status" 1 "exit code for a program that cannot start"
assert_contains "$(err)" "spawn: start $WORK/no-such-program:" \
    "start failure names the program"
assert_eq "$(scalar 'select count(*) from spawn_events')" 1 \
    "a start failure appends no event"

# --- a program killed by a signal has no exit code: run returns 1 -----------

capture run_no_ident "$SPAWN_BIN" run -- sh -c 'kill -TERM $$'
assert_eq "$status" 1 "signal-killed program exits 1 (code.unwrap_or(1))"

# --- SPAWN_ENGINE=codex pipes stdout and still forwards every line ----------

capture env SPAWN_DATABASE_URL= SPAWN_AGENT_ID= SPAWN_SESSION_ID= SPAWN_EXECUTION_ID= \
    SPAWN_ENGINE=codex "$SPAWN_BIN" run -- sh -c 'printf "codex-line\n"; exit 5'
assert_eq "$status" 5 "exit code on the codex stdout path"
assert_eq "$(out)" "codex-line" "codex stdout is echoed line by line"
assert_eq "$(err)" "" "no error on the codex stdout path"
