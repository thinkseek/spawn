#!/bin/sh
# The command line contract: install is repeatable, an unknown subcommand
# fails, and a missing required flag fails before touching the store.

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"
assert_contains "$(out)" "schema installed" "init-db output"

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "second init-db exit"

capture "$SPAWN_BIN" nonsense
assert_eq "$status" 1 "unknown subcommand exit"
assert_contains "$(err)" "unknown command nonsense" "unknown subcommand message"

capture "$SPAWN_BIN" submit --agent agent_x --session session_x
assert_eq "$status" 1 "missing --key exit"
assert_contains "$(err)" "--key" "missing --key message"

capture "$SPAWN_BIN" submit --agent agent_x --session session_x --key k1 --action 'nope'
assert_eq "$status" 1 "invalid action exit"
assert_contains "$(err)" "--action" "invalid action message"

assert_eq "$(scalar 'select count(*) from spawn_commands')" 0 "no command was written"
