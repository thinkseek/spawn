#!/bin/sh
# The command line contract: install is repeatable, the usage names every
# command, an unknown subcommand fails, and a missing required flag fails
# before touching the store.

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"
assert_contains "$(out)" "schema installed" "init-db output"

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "second init-db exit"

capture "$SPAWN_BIN"
assert_eq "$status" 1 "no subcommand exit"
assert_contains "$(err)" \
    "usage: spawn <init-db|submit|tick|loop|mission|task|resident|integrate|chunks|serve|fleet|tail|run|version>" \
    "the usage names every command"

capture "$SPAWN_BIN" nonsense
assert_eq "$status" 1 "unknown subcommand exit"
assert_contains "$(err)" "unknown command nonsense" "unknown subcommand message"

# Commands that fleet, tail and tick cover, and the judge corpus tools, are
# not commands.
for gone in status dispatch charter hook corpus judge; do
    capture "$SPAWN_BIN" "$gone"
    assert_eq "$status" 1 "$gone exit"
    assert_contains "$(err)" "unknown command $gone" "$gone is not a command"
done

capture "$SPAWN_BIN" submit --agent agent_x --session session_x
assert_eq "$status" 1 "missing --key exit"
assert_contains "$(err)" "--key" "missing --key message"

capture "$SPAWN_BIN" submit --agent agent_x --session session_x --key k1 --action 'nope'
assert_eq "$status" 1 "invalid action exit"
assert_contains "$(err)" "--action" "invalid action message"

assert_eq "$(scalar 'select count(*) from spawn_commands')" 0 "no command was written"
