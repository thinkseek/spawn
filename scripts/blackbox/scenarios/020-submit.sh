#!/bin/sh
# `spawn submit` writes exactly one queue row and exactly one
# command.accepted event, returns the same id for a repeated
# (principal, key), starts a new row for a new key, records --principal,
# and writes nothing when the arguments are rejected.

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"

# --- first submit ---------------------------------------------------------
# cmd_submit prints `command={}` with the raw CommandId number.
capture "$SPAWN_BIN" submit --agent agent_001 --session session_001 \
    --key key-1 --action '{"type":"start","engine":"claude","prompt":"hello"}'
assert_eq "$status" 0 "first submit exit"
first=$(out | sed -n 's/^command=//p')
assert_contains "$(out)" "command=" "submit output names the command"
case "$first" in
    '' | *[!0-9]*) fail "submit printed no numeric command id: [$(out)]" ;;
esac

assert_eq "$(scalar 'select count(*) from spawn_commands')" 1 "one command row"
assert_eq "$(scalar "select state from spawn_commands where id = $first")" \
    "pending" "new command state"
assert_eq "$(scalar "select principal from spawn_commands where id = $first")" \
    "spawn" "default principal"
assert_eq "$(scalar "select idempotency_key from spawn_commands where id = $first")" \
    "key-1" "stored idempotency key"
assert_eq "$(scalar "select agent_id from spawn_commands where id = $first")" \
    "agent_001" "stored agent"
assert_eq "$(scalar "select session_id from spawn_commands where id = $first")" \
    "session_001" "stored session"
assert_eq "$(scalar "select action ->> 'type' from spawn_commands where id = $first")" \
    "start" "stored action type"
assert_eq "$(scalar "select action ->> 'prompt' from spawn_commands where id = $first")" \
    "hello" "stored action prompt"

# spawn_submit appends one gateway event keyed `cmd_<id>:accepted`.
assert_eq "$(scalar 'select count(*) from spawn_events')" 1 "one accepted event"
assert_eq "$(scalar 'select event_name from spawn_events')" \
    "command.accepted" "event name"
assert_eq "$(scalar 'select source from spawn_events')" "gateway" "event source"
assert_eq "$(scalar "select source_event_id from spawn_events")" \
    "cmd_${first}:accepted" "event source key"
assert_eq "$(scalar "select kind -> 'detail' ->> 'command_id' from spawn_events")" \
    "$first" "event command id"
assert_eq "$(scalar 'select agent_id from spawn_events')" "agent_001" "event agent"
assert_eq "$(scalar 'select session_id from spawn_events')" "session_001" "event session"
assert_eq "$(scalar 'select session_seq from spawn_events')" 1 "event session sequence"

# --- same (principal, key) is the same command ----------------------------
capture "$SPAWN_BIN" submit --agent agent_001 --session session_001 \
    --key key-1 --action '{"type":"start","engine":"claude","prompt":"hello"}'
assert_eq "$status" 0 "repeat submit exit"
assert_eq "$(out)" "command=$first" "repeat returns the same id"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 1 \
    "repeat leaves one command row"
assert_eq "$(scalar 'select count(*) from spawn_events')" 1 \
    "repeat appends no second accepted event"

# --- a different key is a different command -------------------------------
capture "$SPAWN_BIN" submit --agent agent_001 --session session_001 \
    --key key-2 --action '{"type":"input","text":"go"}'
assert_eq "$status" 0 "second key exit"
second=$(out | sed -n 's/^command=//p')
case "$second" in
    '' | *[!0-9]*) fail "second submit printed no numeric command id: [$(out)]" ;;
esac
[ "$second" != "$first" ] || fail "new key returned the old id $first"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 2 \
    "new key adds one command row"
assert_eq "$(scalar "select state from spawn_commands where id = $second")" \
    "pending" "second command state"
assert_eq "$(scalar "select action ->> 'type' from spawn_commands where id = $second")" \
    "input" "second command action type"
assert_eq "$(scalar 'select count(*) from spawn_events')" 2 \
    "new key appends one more accepted event"
assert_eq "$(scalar "select source_event_id from spawn_events where source_event_id = 'cmd_${second}:accepted'")" \
    "cmd_${second}:accepted" "second event source key"
assert_eq "$(scalar "select session_seq from spawn_events where source_event_id = 'cmd_${second}:accepted'")" \
    2 "second event follows the first on the session"

# --- --principal is recorded, and keys are scoped per principal -----------
# The same key under another principal is a different command.
capture "$SPAWN_BIN" submit --principal alice --agent agent_002 \
    --session session_002 --key key-1 --action '{"type":"stop"}'
assert_eq "$status" 0 "principal submit exit"
third=$(out | sed -n 's/^command=//p')
case "$third" in
    '' | *[!0-9]*) fail "principal submit printed no numeric command id: [$(out)]" ;;
esac
[ "$third" != "$first" ] || fail "same key under another principal returned $first"
assert_eq "$(scalar "select principal from spawn_commands where id = $third")" \
    "alice" "--principal is recorded"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 3 \
    "same key under another principal adds a row"
assert_eq "$(scalar 'select count(*) from spawn_events')" 3 \
    "same key under another principal adds an event"
assert_eq "$(scalar "select source_event_id from spawn_events where source_event_id = 'cmd_${third}:accepted'")" \
    "cmd_${third}:accepted" "principal event source key"

# Repeating it under the same principal returns that same id.
capture "$SPAWN_BIN" submit --principal alice --agent agent_002 \
    --session session_002 --key key-1 --action '{"type":"stop"}'
assert_eq "$status" 0 "principal repeat exit"
assert_eq "$(out)" "command=$third" "principal repeat returns the same id"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 3 \
    "principal repeat leaves three rows"

# --- rejections write nothing ---------------------------------------------
# Invalid agent id: validate() rejects a missing `agent_` prefix.
capture "$SPAWN_BIN" submit --agent bad --session session_001 \
    --key key-3 --action '{"type":"stop"}'
assert_eq "$status" 1 "invalid agent id exit"
assert_contains "$(err)" "identifier must be" "invalid agent id message"

# Empty principal: Principal::new rejects length 0.
capture "$SPAWN_BIN" submit --principal '' --agent agent_001 \
    --session session_001 --key key-4 --action '{"type":"stop"}'
assert_eq "$status" 1 "empty principal exit"
assert_contains "$(err)" "label length 0" "empty principal message"

# Action outside the closed CommandAction enum.
capture "$SPAWN_BIN" submit --agent agent_001 --session session_001 \
    --key key-5 --action '{"type":"nope"}'
assert_eq "$status" 1 "invalid action exit"
assert_contains "$(err)" "--action is not a command action" "invalid action message"

# Missing --key fails before the store is touched.
capture "$SPAWN_BIN" submit --agent agent_001 --session session_001 \
    --action '{"type":"stop"}'
assert_eq "$status" 1 "missing key exit"
assert_contains "$(err)" "--key is required" "missing key message"

assert_eq "$(scalar 'select count(*) from spawn_commands')" 3 \
    "rejections add no command row"
assert_eq "$(scalar 'select count(*) from spawn_events')" 3 \
    "rejections add no event"
