#!/bin/sh
# `spawn hook` ingests one Claude Code hook payload from standard input.
#
# Every expected value below comes from crates/spawn-cli/src/main.rs
# (`cmd_hook`), crates/spawn-core/src/ingest.rs (`claude_hook`) and the
# spawn_append body in crates/spawn-postgres/sql/schema.sql.

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"
assert_eq "$(scalar 'select count(*) from spawn_events')" 0 "log starts empty"

# Feed one payload file on standard input, with extra environment assignments.
# Mirrors `capture`, which cannot supply stdin.
feed() {
    file=$1
    shift
    status=0
    env "$@" "$SPAWN_BIN" hook <"$file" >"$WORK/out.txt" 2>"$WORK/err.txt" || status=$?
    return 0
}

# --- a valid tool hook appends one event --------------------------------
printf '%s\n' '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"toolu_01"}' >"$WORK/pre.json"
feed "$WORK/pre.json" SPAWN_AGENT_ID=agent_h1 SPAWN_SESSION_ID=session_valid
assert_eq "$status" 0 "valid hook exit"
assert_eq "$(out)" "" "valid hook writes nothing to stdout"

assert_eq "$(scalar "select count(*) from spawn_events where session_id = 'session_valid'")" 1 \
    "valid hook appends exactly one event"
assert_eq "$(scalar "select event_name from spawn_events where session_id = 'session_valid'")" \
    tool.started "PreToolUse normalizes to tool.started"
assert_eq "$(scalar "select source from spawn_events where session_id = 'session_valid'")" \
    claude "hook events have source claude"
assert_eq "$(scalar "select agent_id from spawn_events where session_id = 'session_valid'")" \
    agent_h1 "agent_id comes from SPAWN_AGENT_ID"
assert_eq "$(scalar "select source_event_id from spawn_events where session_id = 'session_valid'")" \
    'toolu_01:started' "source_event_id is <tool_use_id>:started"
assert_eq "$(scalar "select kind -> 'detail' ->> 'tool_name' from spawn_events where session_id = 'session_valid'")" \
    Bash "kind detail keeps the tool name"
assert_eq "$(scalar "select kind -> 'detail' ->> 'tool_use_id' from spawn_events where session_id = 'session_valid'")" \
    toolu_01 "kind detail keeps the tool_use_id"

# --- the same payload twice appends one row -----------------------------
printf '%s\n' '{"hook_event_name":"PreToolUse","tool_name":"Read","tool_use_id":"toolu_dup"}' >"$WORK/dup.json"
feed "$WORK/dup.json" SPAWN_AGENT_ID=agent_h1 SPAWN_SESSION_ID=session_dup
assert_eq "$status" 0 "first duplicate feed exit"
assert_eq "$(scalar "select count(*) from spawn_events where session_id = 'session_dup'")" 1 \
    "first feed of the duplicate payload appends one row"

feed "$WORK/dup.json" SPAWN_AGENT_ID=agent_h1 SPAWN_SESSION_ID=session_dup
assert_eq "$status" 0 "second duplicate feed exit"
assert_eq "$(scalar "select count(*) from spawn_events where session_id = 'session_dup'")" 1 \
    "same session+source+source_event_id appends only one row"
assert_eq "$(scalar "select event_count from spawn_session_state where session_id = 'session_dup'")" 1 \
    "dedupe leaves the session projection at one event"

# The key includes the session, so the same tool_use_id in another session is
# a different event. This proves the dedupe is not a global key on the tool id.
feed "$WORK/dup.json" SPAWN_AGENT_ID=agent_h1 SPAWN_SESSION_ID=session_other
assert_eq "$status" 0 "same payload in another session exit"
assert_eq "$(scalar "select count(*) from spawn_events where source_event_id = 'toolu_dup:started'")" 2 \
    "dedupe key is scoped to the session"

# --- SPAWN_EXECUTION_ID is recorded when set ----------------------------
printf '%s\n' '{"hook_event_name":"PostToolUse","tool_use_id":"toolu_02","duration_ms":12}' >"$WORK/post.json"
feed "$WORK/post.json" SPAWN_AGENT_ID=agent_h1 SPAWN_SESSION_ID=session_exec SPAWN_EXECUTION_ID=exec_h1
assert_eq "$status" 0 "hook with execution exit"
assert_eq "$(scalar "select event_name from spawn_events where session_id = 'session_exec'")" \
    tool.completed "PostToolUse normalizes to tool.completed"
assert_eq "$(scalar "select execution_id from spawn_events where session_id = 'session_exec'")" \
    exec_h1 "SPAWN_EXECUTION_ID is stored on the event"
assert_eq "$(scalar "select source_event_id from spawn_events where session_id = 'session_exec'")" \
    'toolu_02:completed' "PostToolUse key is <tool_use_id>:completed"

# --- a hook without an engine id is never deduplicated ------------------
# ingest.rs sets a source key only where Claude supplies a unique id, so a
# UserPromptSubmit carries none and two deliveries are two events.
printf '%s\n' '{"hook_event_name":"UserPromptSubmit"}' >"$WORK/prompt.json"
feed "$WORK/prompt.json" SPAWN_AGENT_ID=agent_h1 SPAWN_SESSION_ID=session_prompt
feed "$WORK/prompt.json" SPAWN_AGENT_ID=agent_h1 SPAWN_SESSION_ID=session_prompt
assert_eq "$status" 0 "prompt hook exit"
assert_eq "$(scalar "select count(*) from spawn_events where session_id = 'session_prompt'")" 2 \
    "a hook without a source_event_id is appended on every delivery"
assert_eq "$(scalar "select count(*) from spawn_events where session_id = 'session_prompt' and source_event_id is null")" 2 \
    "prompt events carry no source_event_id"
assert_eq "$(scalar "select event_name from spawn_events where session_id = 'session_prompt' order by session_seq limit 1")" \
    message.received "UserPromptSubmit normalizes to message.received"

# --- a malformed payload never blocks the engine ------------------------
printf '%s\n' 'this is not json' >"$WORK/bad.json"
feed "$WORK/bad.json" SPAWN_AGENT_ID=agent_h1 SPAWN_SESSION_ID=session_bad
assert_eq "$status" 0 "malformed payload still exits 0"
assert_contains "$(err)" "spawn hook: invalid payload" "malformed payload is reported on stderr"
assert_eq "$(scalar "select count(*) from spawn_events where session_id = 'session_bad'")" 0 \
    "malformed payload appends nothing"

# --- a known hook missing a required field is also non-fatal ------------
printf '%s\n' '{"hook_event_name":"PreToolUse","tool_name":"Bash"}' >"$WORK/nofield.json"
feed "$WORK/nofield.json" SPAWN_AGENT_ID=agent_h1 SPAWN_SESSION_ID=session_nofield
assert_eq "$status" 0 "hook missing a field exits 0"
assert_contains "$(err)" 'lacks `tool_use_id`' "missing field is named on stderr"
assert_eq "$(scalar "select count(*) from spawn_events where session_id = 'session_nofield'")" 0 \
    "hook missing a field appends nothing"

# --- an unknown hook name is dropped silently ---------------------------
printf '%s\n' '{"hook_event_name":"PreCompact"}' >"$WORK/unknown.json"
feed "$WORK/unknown.json" SPAWN_AGENT_ID=agent_h1 SPAWN_SESSION_ID=session_unknown
assert_eq "$status" 0 "unknown hook exit"
assert_eq "$(err)" "" "unknown hook is silent"
assert_eq "$(scalar "select count(*) from spawn_events where session_id = 'session_unknown'")" 0 \
    "unknown hook appends nothing"

# --- empty stdin is a no-op before any environment check ----------------
printf '' >"$WORK/empty.json"
feed "$WORK/empty.json"
assert_eq "$status" 0 "empty stdin exits 0"
assert_eq "$(scalar 'select count(*) from spawn_events')" 6 \
    "empty stdin appends nothing"

# --- the required environment is enforced -------------------------------
feed "$WORK/pre.json" SPAWN_AGENT_ID= SPAWN_SESSION_ID=session_noagent
assert_eq "$status" 1 "missing SPAWN_AGENT_ID exit"
assert_contains "$(err)" "SPAWN_AGENT_ID is not set" "missing agent is named"

feed "$WORK/pre.json" SPAWN_AGENT_ID=agent_h1 SPAWN_SESSION_ID=
assert_eq "$status" 1 "missing SPAWN_SESSION_ID exit"
assert_contains "$(err)" "SPAWN_SESSION_ID is not set" "missing session is named"

feed "$WORK/pre.json" SPAWN_AGENT_ID=not-an-agent-id SPAWN_SESSION_ID=session_badid
assert_eq "$status" 1 "invalid SPAWN_AGENT_ID exit"
assert_contains "$(err)" "identifier must be" "invalid agent id is explained"

# --- total: four accepted tool hooks and two prompt hooks; nothing from
# any rejection path ----------------------------------------------------
assert_eq "$(scalar 'select count(*) from spawn_events')" 6 "only accepted payloads were stored"
assert_eq "$(scalar "select count(*) from spawn_events where agent_id <> 'agent_h1'")" 0 \
    "every stored row used SPAWN_AGENT_ID"
