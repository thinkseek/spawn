#!/bin/sh
# `spawn run -- <program>` (`run` in crates/spawn-cli/src/commands/engine.rs).
#
# The runner starts the program, waits for it and returns its exit code. The
# pane holds no database login: the runner writes the exit status to
# SPAWN_EXIT_FILE (whole, then renamed) for the Bridge to read, and leaves
# the program's output to the pane. It never writes to the store, whatever
# the environment holds.

# --- the program is required ------------------------------------------------

capture "$SPAWN_BIN" run
assert_eq "$status" 1 "run without a program exit"
assert_contains "$(err)" "spawn: run needs a program after --" \
    "run without a program message"

capture "$SPAWN_BIN" run --
assert_eq "$status" 1 "run with an empty -- exit"
assert_contains "$(err)" "spawn: run needs a program after --" \
    "run with an empty -- message"

# --- no exit file: the program still runs and its code is forwarded ---------

capture env SPAWN_EXIT_FILE= \
    "$SPAWN_BIN" run -- sh -c 'printf "engine-out\n"; exit 7'
assert_eq "$status" 7 "exit code without SPAWN_EXIT_FILE"
assert_eq "$(out)" "engine-out" "stdout forwarded without SPAWN_EXIT_FILE"
assert_eq "$(err)" "" "no error without SPAWN_EXIT_FILE"

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"

# --- the full path: run, forward stdout, write the real exit code ----------
# The identity variables and the database URL are set, as an older pane had
# them; the runner still appends nothing.

exits=$WORK/exits
capture env SPAWN_ENGINE=claude SPAWN_AGENT_ID=agent_bb SPAWN_SESSION_ID=session_bb \
    SPAWN_EXECUTION_ID=exec_bb SPAWN_EXIT_FILE="$exits/exec_bb" \
    "$SPAWN_BIN" run -- sh -c 'printf "engine-said-hi\n"; exit 7'
assert_eq "$status" 7 "exit code with an exit file"
assert_eq "$(out)" "engine-said-hi" "stdout forwarded with an exit file"
assert_eq "$(err)" "" "no error on the exit-file path"
assert_file "$exits/exec_bb"
assert_eq "$(cat "$exits/exec_bb")" 7 "the exit file holds the exit code"
assert_no_file "$exits/exec_bb.partial"
assert_eq "$(scalar 'select count(*) from spawn_events')" 0 \
    "the runner appends no event"

# A second run of the same execution replaces the file whole.
capture env SPAWN_EXIT_FILE="$exits/exec_bb" "$SPAWN_BIN" run -- sh -c 'exit 0'
assert_eq "$status" 0 "exit code of a clean run"
assert_eq "$(cat "$exits/exec_bb")" 0 "the exit file holds the new code"

# --- a program that cannot start writes no exit file ------------------------

capture env SPAWN_EXIT_FILE="$exits/exec_missing" \
    "$SPAWN_BIN" run -- "$WORK/no-such-program"
assert_eq "$status" 1 "exit code for a program that cannot start"
assert_contains "$(err)" "spawn: start $WORK/no-such-program:" \
    "start failure names the program"
assert_no_file "$exits/exec_missing"

# --- a program killed by a signal has no exit code ---------------------------
# run returns 1 (code.unwrap_or(1)) and the exit file is empty: a death the
# Bridge records without a code.

capture env SPAWN_EXIT_FILE="$exits/exec_signal" \
    "$SPAWN_BIN" run -- sh -c 'kill -TERM $$'
assert_eq "$status" 1 "signal-killed program exits 1"
assert_file "$exits/exec_signal"
assert_eq "$(cat "$exits/exec_signal")" "" "a signal death leaves an empty exit file"

# --- the output goes to the pane only -----------------------------------------
# The runner makes no output copy, whatever the environment asks for
# (SPAWN_ENGINE=codex with SPAWN_ENGINE_OUTPUT): every line reaches the pane.

capture env SPAWN_ENGINE=codex SPAWN_ENGINE_OUTPUT="$WORK/copy/out.jsonl" \
    SPAWN_EXIT_FILE="$exits/exec_lines" \
    "$SPAWN_BIN" run -- sh -c 'printf "line-1\nline-2\n"; exit 5'
assert_eq "$status" 5 "exit code with an output copy asked for"
assert_eq "$(out)" "line-1
line-2" "every output line reaches the pane"
assert_eq "$(err)" "" "no error with an output copy asked for"
assert_no_file "$WORK/copy/out.jsonl"
assert_eq "$(cat "$exits/exec_lines")" 5 "the exit file holds the exit code"

assert_eq "$(scalar 'select count(*) from spawn_events')" 0 \
    "no run appended an event"
