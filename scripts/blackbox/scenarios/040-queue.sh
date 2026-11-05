#!/bin/sh
# Queue ordering and lease semantics, seen from the binary and the database.
#
# Facts under test, each derived from schema.sql and main.rs:
# - spawn_submit keys on (principal, idempotency_key); a repeated key returns
#   the stored id and writes no second row and no second accepted event.
# - spawn_claim takes at most one command, and takes only the lowest id of a
#   session whose earlier command is unfinished. It writes lease_worker,
#   lease_attempt + 1 and lease_until_ms; it does not finish the command.
# - a command of another session is claimed independently of a held one.
# - spawn_finish rejects an attempt that is not the stored one and accepts the
#   current one, which lets the next command of the session be claimed.
#
# "One claim pass" is the unit spawn_claim offers: one call returns one row.
# The tick loop calls it repeatedly and finishes each row, which is why the
# per-session head rule is asserted at the claim level.

worker=$SPAWN_WORKER
# Each session is its own agent's (one session per agent).

# Parse `command=<id>` from the last capture into $cmd_id.
read_command_id() {
    line=$(out)
    case "$line" in
        command=?*) value=${line#command=} ;;
        *) fail "submit printed [$line], expected command=<id>" ;;
    esac
    case "$value" in
        '' | *[!0-9]*) fail "submit printed a non-numeric command id: [$value]" ;;
    esac
    cmd_id=$value
}

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"
assert_contains "$(out)" "schema installed" "init-db output"

# --- a repeated idempotency key queues nothing -------------------------
capture "$SPAWN_BIN" submit --principal spawn --agent agent_q1 \
    --session session_q1 --key key_a1 --action '{"type":"stop"}'
assert_eq "$status" 0 "first submit exit"
read_command_id
cmd1=$cmd_id
assert_eq "$(scalar "select state from spawn_commands where id = $cmd1")" \
    pending "a fresh command starts pending"
assert_eq "$(scalar "select lease_attempt from spawn_commands where id = $cmd1")" \
    0 "a fresh command has lease_attempt 0"
assert_eq "$(scalar "select coalesce(lease_worker, 'null') from spawn_commands where id = $cmd1")" \
    null "a fresh command has no lease worker"

capture "$SPAWN_BIN" submit --principal spawn --agent agent_q1 \
    --session session_q1 --key key_a1 --action '{"type":"stop"}'
assert_eq "$status" 0 "repeated submit exit"
read_command_id
assert_eq "$cmd_id" "$cmd1" "a repeated key returns the stored command id"
assert_eq "$(scalar "select count(*) from spawn_commands where principal = 'spawn' and idempotency_key = 'key_a1'")" \
    1 "one row per principal and idempotency key"
assert_eq "$(scalar "select count(*) from spawn_events where event_name = 'command.accepted' and kind->'detail'->>'command_id' = '$cmd1'")" \
    1 "the repeated key writes no second accepted event"

# The key wins even when the repeated call names another session.
capture "$SPAWN_BIN" submit --principal spawn --agent agent_q2 \
    --session session_q2 --key key_a1 --action '{"type":"stop"}'
assert_eq "$status" 0 "repeat with another session exit"
read_command_id
assert_eq "$cmd_id" "$cmd1" "the stored key wins over the new session"
assert_eq "$(scalar "select count(*) from spawn_commands where session_id = 'session_q2'")" \
    0 "the rejected repeat queues nothing"

# --- one claim takes one head, per session -----------------------------
capture "$SPAWN_BIN" submit --principal spawn --agent agent_q1 \
    --session session_q1 --key key_a2 --action '{"type":"stop"}'
assert_eq "$status" 0 "second command of session_q1 exit"
read_command_id
cmd2=$cmd_id
[ "$cmd2" != "$cmd1" ] || fail "the second submit reused the first command id"

capture "$SPAWN_BIN" submit --principal spawn --agent agent_q2 \
    --session session_q2 --key key_b1 --action '{"type":"stop"}'
assert_eq "$status" 0 "command of session_q2 exit"
read_command_id
cmd3=$cmd_id

claim=$(sql "select out_id, out_attempt from spawn_claim('$worker', 3600000)")
assert_eq "$claim" "$cmd1|1" \
    "the first claim takes only the lowest id of session_q1"

assert_eq "$(scalar "select lease_worker from spawn_commands where id = $cmd1")" \
    "$worker" "the claim records SPAWN_WORKER as lease_worker"
assert_eq "$(scalar "select lease_attempt from spawn_commands where id = $cmd1")" \
    1 "the claim increments lease_attempt to 1"
assert_eq "$(scalar "select state from spawn_commands where id = $cmd1")" \
    pending "the claim leaves the command pending"
assert_eq "$(scalar "select lease_until_ms > spawn_now_ms() from spawn_commands where id = $cmd1")" \
    t "the claim writes a live lease_until_ms"

# The later command of the same session is untouched.
assert_eq "$(scalar "select state from spawn_commands where id = $cmd2")" \
    pending "the second command of session_q1 stays pending"
assert_eq "$(scalar "select lease_attempt from spawn_commands where id = $cmd2")" \
    0 "the second command of session_q1 keeps lease_attempt 0"
assert_eq "$(scalar "select coalesce(lease_worker, 'null') from spawn_commands where id = $cmd2")" \
    null "the second command of session_q1 has no lease worker"

# The command of the other session is claimed by the next pass.
claim=$(sql "select out_id, out_attempt from spawn_claim('$worker', 3600000)")
assert_eq "$claim" "$cmd3|1" \
    "the next claim takes the command of the other session"

# With one live lease per open session and session_q1 blocked by its head,
# a further claim has nothing left.
assert_eq "$(scalar "select count(*) from spawn_claim('$worker', 3600000)")" \
    0 "live leases and a pending head leave nothing claimable"

# --- one tick hands each open session to the worker --------------------
capture "$SPAWN_BIN" submit --principal spawn --agent agent_q3 \
    --session session_q3 --key key_c1 --action '{"type":"stop"}'
assert_eq "$status" 0 "command of session_q3 exit"
read_command_id
cmd4=$cmd_id
capture "$SPAWN_BIN" submit --principal spawn --agent agent_q4 \
    --session session_q4 --key key_c2 --action '{"type":"stop"}'
assert_eq "$status" 0 "command of session_q4 exit"
read_command_id
cmd5=$cmd_id

capture "$SPAWN_BIN" tick
assert_eq "$status" 0 "tick exit"
assert_contains "$(out)" "commands=2 observations=" \
    "one tick handles the two unleased sessions and skips the held ones"

for cmd in "$cmd4" "$cmd5"; do
    assert_eq "$(scalar "select state from spawn_commands where id = $cmd")" \
        finished "the tick finishes the claimed command $cmd"
    assert_eq "$(scalar "select lease_worker from spawn_commands where id = $cmd")" \
        "$worker" "the tick leases command $cmd to SPAWN_WORKER"
    assert_eq "$(scalar "select lease_attempt from spawn_commands where id = $cmd")" \
        1 "the tick claims command $cmd once"
    assert_eq "$(scalar "select succeeded from spawn_commands where id = $cmd")" \
        t "a stop of an idle session succeeds for command $cmd"
done

# The tick did not touch the held commands.
assert_eq "$(scalar "select state from spawn_commands where id = $cmd1")" \
    pending "the tick leaves the held head of session_q1 pending"
assert_eq "$(scalar "select lease_attempt from spawn_commands where id = $cmd2")" \
    0 "the tick leaves the blocked command of session_q1 unclaimed"

# --- finish fencing and the release of the session head ----------------
assert_eq "$(scalar "select spawn_finish($cmd1, 0, true, null)")" \
    stale "finish with an older attempt is rejected"
assert_eq "$(scalar "select state from spawn_commands where id = $cmd1")" \
    pending "the rejected finish leaves the command open"

assert_eq "$(scalar "select spawn_finish($cmd1, 1, true, null)")" \
    done "finish with the stored attempt is accepted"
assert_eq "$(scalar "select state from spawn_commands where id = $cmd1")" \
    finished "the accepted finish closes the command"
assert_eq "$(scalar "select succeeded from spawn_commands where id = $cmd1")" \
    t "the accepted finish records the outcome"
assert_eq "$(scalar "select finished_at_ms is not null from spawn_commands where id = $cmd1")" \
    t "the accepted finish records finished_at_ms"

claim=$(sql "select out_id, out_attempt from spawn_claim('$worker', 3600000)")
assert_eq "$claim" "$cmd2|1" \
    "finishing the head releases the next command of the session"
