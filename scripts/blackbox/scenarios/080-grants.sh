#!/bin/sh
# The command gateway and grants.
#
# `agent_commands` is the only public entry point: an insert runs the
# `instead of insert` trigger `spawn_gateway_insert`, which refuses the row
# unless `spawn_grants` holds a row for the session user and the agent. The
# CLI is not on that path -- `spawn submit` calls `spawn_submit` directly as
# the owner -- so grants never apply to it.

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"
assert_contains "$(out)" "schema installed" "init-db output"

# The principal under test is whatever role psql connects as; the trigger
# compares `session_user` with that value.
me=$(scalar 'select session_user')
[ -n "$me" ] || fail "could not read session_user"

# --- No grant at all -------------------------------------------------------

assert_eq "$(scalar 'select count(*) from spawn_grants')" 0 "spawn_grants starts empty"

capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into agent_commands (agent_id, session_id, idempotency_key, action) values ('agent_x', 'session_x', 'gw_1', '{\"type\":\"stop\"}'::jsonb)"
[ "$status" -ne 0 ] || fail "an insert into agent_commands without a grant was accepted"
assert_contains "$(err)" "has no grant for agent agent_x" "the gateway names the agent it refused"
assert_contains "$(err)" "principal $me" "the gateway names the principal it refused"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 0 "a refused insert queues no command"
assert_eq "$(scalar 'select count(*) from spawn_events')" 0 "a refused insert records no event"

# --- The CLI path bypasses the gateway -------------------------------------

capture "$SPAWN_BIN" submit --agent agent_cli --session session_cli --key cli_1 --action '{"type":"stop"}'
assert_eq "$status" 0 "submit succeeds with zero grant rows"
assert_contains "$(out)" "command=" "submit prints the command id"
cli_line=$(out)
cli_id=${cli_line#command=}
assert_eq "$cli_id" "$(scalar 'select max(id) from spawn_commands')" "submit returns the queued id"
assert_eq "$(scalar "select principal from spawn_commands where id = $cli_id")" "spawn" "submit uses the default principal"
assert_eq "$(scalar "select agent_id from spawn_commands where id = $cli_id")" "agent_cli" "submit queues the requested agent"
assert_eq "$(scalar "select session_id from spawn_commands where id = $cli_id")" "session_cli" "submit queues the requested session"
assert_eq "$(scalar 'select count(*) from spawn_grants')" 0 "submit needed no grant row"
assert_eq "$(scalar "select count(*) from spawn_events where event_name = 'command.accepted'")" 1 "submit records the accepted event"

# --- A grant for one agent only --------------------------------------------

capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into spawn_grants (principal, agent_id) values (session_user, 'agent_x')"
assert_eq "$status" 0 "inserting a grant row succeeds"
assert_eq "$(scalar 'select count(*) from spawn_grants')" 1 "one grant row exists"
assert_eq "$(scalar "select agent_id from spawn_grants where principal = '$me'")" "agent_x" "the grant names agent_x"

# A different agent is still refused.
capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into agent_commands (agent_id, session_id, idempotency_key, action) values ('agent_y', 'session_y', 'gw_y', '{\"type\":\"input\",\"text\":\"hi\"}'::jsonb)"
[ "$status" -ne 0 ] || fail "a grant for agent_x authorized agent_y"
assert_contains "$(err)" "has no grant for agent agent_y" "the gateway refuses the unlisted agent"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 1 "the refused agent_y insert queues nothing"

# The granted agent is accepted and lands in spawn_commands.
capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into agent_commands (agent_id, session_id, idempotency_key, action) values ('agent_x', 'session_x', 'gw_2', '{\"type\":\"stop\"}'::jsonb) returning id"
assert_eq "$status" 0 "the granted insert is accepted"
gw_id=$(out | head -1)
[ -n "$gw_id" ] || fail "the granted insert returned no id"
assert_eq "$gw_id" "$(scalar 'select max(id) from spawn_commands')" "the trigger returns the queued command id"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 2 "the granted insert lands in spawn_commands"
assert_eq "$(scalar "select agent_id from spawn_commands where id = $gw_id")" "agent_x" "the queued row carries the agent"
assert_eq "$(scalar "select session_id from spawn_commands where id = $gw_id")" "session_x" "the queued row carries the session"
assert_eq "$(scalar "select idempotency_key from spawn_commands where id = $gw_id")" "gw_2" "the queued row carries the idempotency key"
assert_eq "$(scalar "select principal from spawn_commands where id = $gw_id")" "$me" "the trigger uses session_user as the principal"
assert_eq "$(scalar "select state from spawn_commands where id = $gw_id")" "pending" "the queued command starts pending"
assert_eq "$(scalar "select action ->> 'type' from spawn_commands where id = $gw_id")" "stop" "the queued row carries the action"
assert_eq "$(scalar "select count(*) from spawn_events where source_event_id = 'cmd_${gw_id}:accepted'")" 1 "the gateway records the accepted event"

# The gateway is idempotent through spawn_submit's conflict path.
capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into agent_commands (agent_id, session_id, idempotency_key, action) values ('agent_x', 'session_x', 'gw_2', '{\"type\":\"stop\"}'::jsonb) returning id"
assert_eq "$status" 0 "a repeated granted insert is accepted"
assert_eq "$(out | head -1)" "$gw_id" "a repeated key returns the first command id"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 2 "a repeated key queues no second command"

# --- Grants still do not gate the CLI --------------------------------------

capture "$SPAWN_BIN" submit --agent agent_cli2 --session session_cli2 --key cli_2 --action '{"type":"stop"}'
assert_eq "$status" 0 "submit succeeds for an agent with no grant"
assert_eq "$(scalar "select count(*) from spawn_grants where principal = '$me' and agent_id = 'agent_cli2'")" 0 "no grant was added for the CLI agent"
assert_eq "$(scalar "select count(*) from spawn_commands where agent_id = 'agent_cli2'")" 1 "the CLI submit queued its command"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 3 "only the CLI command was added"

# --- A wildcard grant authorizes any agent ---------------------------------

capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into spawn_grants (principal, agent_id) values (session_user, '*')"
assert_eq "$status" 0 "inserting a wildcard grant succeeds"
capture $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c \
    "insert into agent_commands (agent_id, session_id, idempotency_key, action) values ('agent_z', 'session_z', 'gw_3', '{\"type\":\"stop\"}'::jsonb) returning id"
assert_eq "$status" 0 "a wildcard grant authorizes an agent with no exact grant"
star_id=$(out | head -1)
assert_eq "$star_id" "$(scalar 'select max(id) from spawn_commands')" "the wildcard insert returns the queued id"
assert_eq "$(scalar "select agent_id from spawn_commands where id = $star_id")" "agent_z" "the wildcard insert lands in spawn_commands"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 4 "the wildcard insert added one command"
