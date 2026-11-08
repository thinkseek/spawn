#!/bin/sh
# The SQL read views after one real tick. One session receives two start
# commands: the first starts an execution, the second is rejected because the
# first still runs. The views must fold the log and the projection the same
# way, and agent_timeline must drop every Observer-written row.

agent=agent_views
session=session_views
absent=session_absent

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"
assert_contains "$(out)" "schema installed" "init-db output"

# Empty store: every view is empty, and a session with no events has no row.
assert_eq "$(scalar 'select count(*) from agent_commands')" 0 "agent_commands is empty"
assert_eq "$(scalar 'select count(*) from agent_tasks')" 0 "agent_tasks is empty"
assert_eq "$(scalar 'select count(*) from agent_executions')" 0 "agent_executions is empty"
assert_eq "$(scalar 'select count(*) from agent_timeline')" 0 "agent_timeline is empty"
assert_eq "$(scalar 'select count(*) from agent_state')" 0 "agent_state is empty"
assert_eq "$(scalar 'select count(*) from agent_status')" 0 "agent_status is empty"
assert_eq "$(scalar 'select count(*) from agent_freshness')" 0 "agent_freshness is empty"
assert_eq "$(scalar "select count(*) from agent_status where session_id='$absent'")" 0 \
    "agent_status has no phantom row for an unknown session"
assert_eq "$(scalar "select count(*) from agent_freshness where session_id='$absent'")" 0 \
    "agent_freshness has no phantom row for an unknown session"

# Two start actions are queued before the tick. The first must run; the
# second must be rejected because the session already runs an execution.
capture "$SPAWN_BIN" submit --agent "$agent" --session "$session" --key start-1 \
    --action '{"type":"start","engine":"claude","prompt":"say hello"}'
assert_eq "$status" 0 "first submit exit"
assert_contains "$(out)" "command=" "first submit prints the command id"
command_id=$(scalar "select id from spawn_commands where principal='spawn' and idempotency_key='start-1'")
[ -n "$command_id" ] || fail "first command id is empty"
assert_contains "$(out)" "command=$command_id" "first submit id matches the stored row"

capture "$SPAWN_BIN" submit --agent "$agent" --session "$session" --key start-2 \
    --action '{"type":"start","engine":"claude","prompt":"never"}'
assert_eq "$status" 0 "second submit exit"
rejected_id=$(scalar "select id from spawn_commands where principal='spawn' and idempotency_key='start-2'")
[ -n "$rejected_id" ] || fail "second command id is empty"

# agent_commands exposes the command row. Its state lives in agent_tasks,
# which selects the same table: the view itself has no state column.
assert_eq "$(scalar 'select count(*) from agent_commands')" 2 "agent_commands shows both commands"
assert_eq "$(scalar "select count(*) from agent_commands where id=$command_id and agent_id='$agent' and session_id='$session' and idempotency_key='start-1' and action->>'type'='start'")" 1 \
    "agent_commands carries the first command fields"
assert_eq "$(scalar "select count(*) from agent_commands where id=$rejected_id and idempotency_key='start-2' and action->>'type'='start'")" 1 \
    "agent_commands carries the second command fields"
assert_eq "$(scalar 'select count(*) from (select id, agent_id, session_id, idempotency_key, action, created_at_ms from agent_commands) c')" 2 \
    "agent_commands exposes exactly its documented columns; state comes from agent_tasks"
assert_eq "$(scalar "select state from agent_tasks where command_id=$command_id")" "pending" \
    "queued command is pending before the tick"
assert_eq "$(scalar "select prompt from agent_tasks where command_id=$command_id")" "say hello" \
    "agent_tasks exposes the start prompt"
assert_eq "$(scalar "select count(*) from agent_executions")" 0 "no execution row before the tick"
assert_eq "$(scalar "select count(*) from agent_state")" 0 "no judgement before the tick"

# Both submissions are already non-observer events, before any tick.
assert_eq "$(scalar "select count(*) from agent_timeline where session_id='$session' and event_name='command.accepted' and source='gateway'")" 2 \
    "agent_timeline shows both command.accepted events"
assert_eq "$(scalar "select count(*) from agent_freshness where session_id='$session'")" 1 \
    "agent_freshness has one row after the queued commands"
# agent_state filters on a non-null observation, so the session is absent.
assert_eq "$(scalar "select count(*) from agent_state where session_id='$session'")" 0 \
    "agent_state hides a session with no observation"

capture "$SPAWN_BIN" tick
assert_eq "$status" 0 "tick exit"
assert_contains "$(out)" "commands=2" "tick handled both commands"
assert_contains "$(out)" "observations=1" "Observer wrote one judgement"
assert_contains "$(out)" "sessions=1" "Bridge tracked one session"
assert_not_contains "$(err)" "spawn tick warning" "tick reported no warning"

# The rejected start is visible as a finished, failed command; it created no
# execution.
assert_eq "$(scalar "select state from agent_tasks where command_id=$rejected_id")" "finished" \
    "rejected command is finished"
assert_eq "$(scalar "select succeeded from agent_tasks where command_id=$rejected_id")" "f" \
    "rejected command did not succeed"
assert_contains "$(sql "select error from agent_tasks where command_id=$rejected_id")" \
    "already runs an execution" "rejected command records why"
assert_eq "$(scalar "select state from agent_tasks where command_id=$command_id")" "finished" \
    "accepted command is finished"
assert_eq "$(scalar "select succeeded from agent_tasks where command_id=$command_id")" "t" \
    "accepted command succeeded"
assert_eq "$(scalar "select count(*) from agent_tasks where command_id=$command_id and error is null")" 1 \
    "accepted command records no error"
assert_eq "$(scalar "select count(*) from agent_executions where session_id='$session'")" 1 \
    "the rejected start added no execution row"
assert_eq "$(scalar "select count(*) from agent_commands where id in ($command_id,$rejected_id) and session_id='$session'")" 2 \
    "agent_commands keeps both commands after the tick"

# agent_executions: one started execution, early columns set, late columns
# still empty because the Bridge has not ended it.
assert_eq "$(scalar "select execution_id from agent_executions where session_id='$session'")" "exec_$command_id" \
    "execution id is derived from the command id"
assert_eq "$(scalar "select terminal from agent_executions where session_id='$session'")" "spawn-exec_$command_id" \
    "terminal name is derived from the execution id"
assert_eq "$(scalar "select engine from agent_executions where session_id='$session'")" "claude" \
    "execution engine is claude"
assert_eq "$(scalar "select command_id from agent_executions where session_id='$session'")" "$command_id" \
    "execution records its command id"
assert_eq "$(scalar "select split_part(workspace, ':', 1) from agent_executions where session_id='$session'")" "$agent" \
    "workspace is <branch>:<path> for the agent branch"
assert_eq "$(scalar "select started_at_ms > 0 from agent_executions where session_id='$session'")" "t" \
    "started_at_ms is set"
assert_eq "$(scalar "select (ended_at_ms is null and cause is null and exit_code is null and duration_ms is null) from agent_executions where session_id='$session'")" "t" \
    "late columns are empty while the execution runs"
assert_eq "$(scalar "select exit_reported from agent_executions where session_id='$session'")" "f" \
    "no engine exit was reported"

# agent_timeline: every non-Observer event of the session, and never an
# Observer row. agent_judgements carries the observation instead.
total=$(scalar "select count(*) from spawn_events where session_id='$session'")
observer_rows=$(scalar "select count(*) from spawn_events where session_id='$session' and source='observer'")
timeline_rows=$(scalar "select count(*) from agent_timeline where session_id='$session'")
[ "$total" -gt 0 ] || fail "the session has no events"
[ "$observer_rows" -gt 0 ] || fail "the Observer wrote no observation event"
assert_eq "$timeline_rows" "$((total - observer_rows))" \
    "agent_timeline holds exactly the non-observer events"
assert_eq "$(scalar "select count(*) from agent_timeline where session_id='$session' and source='observer'")" 0 \
    "agent_timeline drops every observer row"
assert_eq "$(scalar "select count(*) from agent_timeline where event_name='observation.recorded'")" 0 \
    "the judgement is not a timeline event"
assert_eq "$(scalar "select count(*) from agent_timeline where session_id='$session' and event_name='execution.started' and source='bridge' and execution_id='exec_$command_id'")" 1 \
    "agent_timeline shows the execution.started event"
assert_eq "$(scalar "select count(*) from agent_timeline where session_id='$session' and event_name='screen.observed' and source='bridge'")" 1 \
    "agent_timeline shows the screen.observed event"
assert_eq "$(scalar "select count(*) from agent_judgements where session_id='$session'")" 1 \
    "agent_judgements holds the observation"
# The latest observation's field, as the views must expose it.
observed() { scalar "select kind -> 'detail' -> 'observation' ->> '$1' from spawn_events where session_id='$session' and source='observer' order by session_seq desc limit 1"; }
judged=$(scalar "select kind -> 'detail' -> 'observation' ->> 'state' from spawn_events where session_id='$session' and source='observer' order by session_seq desc limit 1")
judged_reason=$(scalar "select coalesce(kind -> 'detail' -> 'observation' ->> 'reason', '') from spawn_events where session_id='$session' and source='observer' order by session_seq desc limit 1")
[ -n "$judged" ] || fail "the observation has no state"
assert_eq "$(scalar "select state from agent_judgements where session_id='$session'")" "$judged" \
    "agent_judgements parses the judged state"

# agent_freshness: the log position of the session, updated by the Observer
# append too.
assert_eq "$(scalar "select count(*) from agent_freshness where session_id='$session'")" 1 \
    "agent_freshness has one row for the session"
assert_eq "$(scalar "select event_count from agent_freshness where session_id='$session'")" "$total" \
    "event_count equals the stored event count"
assert_eq "$(scalar "select last_session_seq from agent_freshness where session_id='$session'")" \
    "$(scalar "select max(session_seq) from spawn_events where session_id='$session'")" \
    "last_session_seq is the highest session sequence"
assert_eq "$(scalar "select last_event_id from agent_freshness where session_id='$session'")" \
    "$(scalar "select max(id) from spawn_events where session_id='$session'")" \
    "last_event_id is the highest global event id"
assert_eq "$(scalar "select last_event_at_ms from agent_freshness where session_id='$session'")" \
    "$(scalar "select max(recorded_at_ms) from spawn_events where session_id='$session'")" \
    "last_event_at_ms is the time of the latest event"

# agent_state: the latest observation, judged on the latest evidence.
assert_eq "$(scalar "select count(*) from agent_state where session_id='$session'")" 1 \
    "agent_state has one row after the tick"
assert_eq "$(scalar "select state from agent_state where session_id='$session'")" "$judged" \
    "agent_state state is the judged state"
assert_eq "$(scalar "select reason from agent_state where session_id='$session'")" "$judged_reason" \
    "agent_state reason is the judged reason"
assert_eq "$(scalar "select judge from agent_state where session_id='$session'")" "$(observed judge)" \
    "agent_state names the judge"
assert_eq "$(scalar "select confidence::text from agent_state where session_id='$session'")" "$(scalar "select (kind -> 'detail' -> 'observation' ->> 'confidence')::float8::text from spawn_events where session_id='$session' and source='observer' order by session_seq desc limit 1")" \
    "agent_state exposes the judge confidence"
assert_eq "$(scalar "select based_on_seq from agent_state where session_id='$session'")" \
    "$(scalar "select max(session_seq) from spawn_events where session_id='$session' and source<>'observer'")" \
    "based_on_seq is the latest non-observer sequence"
assert_eq "$(scalar "select session_seq from agent_state where session_id='$session'")" \
    "$(scalar "select last_session_seq from agent_freshness where session_id='$session'")" \
    "agent_state session_seq is the observation sequence"
assert_eq "$(scalar "select event_id from agent_state where session_id='$session'")" \
    "$(scalar "select last_event_id from agent_freshness where session_id='$session'")" \
    "the latest event is the judgement"
assert_eq "$(scalar "select count(*) from agent_state where session_id='$session' and observation->>'state'='$judged'")" 1 \
    "agent_state keeps the raw observation"

# agent_status: one row per session, the latest judgement and the latest
# execution.
assert_eq "$(scalar "select count(*) from agent_status where session_id='$session'")" 1 \
    "agent_status has one row for the session"
assert_eq "$(scalar "select state from agent_status where session_id='$session'")" "$judged" \
    "agent_status state is the judged state"
assert_eq "$(scalar "select reason from agent_status where session_id='$session'")" "$judged_reason" \
    "agent_status reason matches the observation"
assert_eq "$(scalar "select observation_seq from agent_status where session_id='$session'")" \
    "$(scalar "select session_seq from agent_state where session_id='$session'")" \
    "agent_status observation_seq matches agent_state"
assert_eq "$(scalar "select last_session_seq from agent_status where session_id='$session'")" \
    "$(scalar "select last_session_seq from agent_freshness where session_id='$session'")" \
    "agent_status last_session_seq matches agent_freshness"
assert_eq "$(scalar "select execution_id from agent_status where session_id='$session'")" "exec_$command_id" \
    "agent_status reports the latest execution"
assert_eq "$(scalar "select engine from agent_status where session_id='$session'")" "claude" \
    "agent_status reports the engine"
assert_eq "$(scalar "select started_at_ms from agent_status where session_id='$session'")" \
    "$(scalar "select started_at_ms from agent_executions where session_id='$session'")" \
    "agent_status started_at_ms matches agent_executions"
assert_eq "$(scalar "select ended_at_ms is null from agent_status where session_id='$session'")" "t" \
    "agent_status has no end time while the execution runs"
assert_eq "$(scalar "select exit_reported from agent_status where session_id='$session'")" "f" \
    "agent_status reports no engine exit"
assert_eq "$(scalar "select live from agent_status where session_id='$session'")" "t" \
    "agent_status marks the execution live"

# A second tick with no queued command and a live terminal writes nothing:
# the Observer does not repeat an unchanged judgement.
capture "$SPAWN_BIN" tick
assert_eq "$status" 0 "second tick exit"
assert_contains "$(out)" "commands=0 observations=0 sessions=1" "idle tick writes no observation"
assert_eq "$(scalar "select count(*) from spawn_events where session_id='$session'")" "$total" \
    "idle tick appends no event"
assert_eq "$(scalar "select event_count from agent_freshness where session_id='$session'")" "$total" \
    "idle tick leaves freshness unchanged"

# A stop command ends the execution. The Bridge writes its own
# execution.ended, so the late columns fill in, no engine exit is reported,
# and the Observer judges the ended session COMPLETED.
capture "$SPAWN_BIN" submit --agent "$agent" --session "$session" --key stop-1 \
    --action '{"type":"stop"}'
assert_eq "$status" 0 "stop submit exit"
stop_id=$(scalar "select id from spawn_commands where principal='spawn' and idempotency_key='stop-1'")
[ -n "$stop_id" ] || fail "stop command id is empty"

capture "$SPAWN_BIN" tick
assert_eq "$status" 0 "stop tick exit"
assert_contains "$(out)" "commands=1" "stop tick handled the stop command"
assert_contains "$(out)" "observations=1" "stop tick wrote the new judgement"
assert_not_contains "$(err)" "spawn tick warning" "stop tick reported no warning"

assert_eq "$(scalar "select state from agent_tasks where command_id=$stop_id")" "finished" \
    "stop command is finished"
assert_eq "$(scalar "select succeeded from agent_tasks where command_id=$stop_id")" "t" \
    "stop command succeeded"
assert_eq "$(scalar 'select count(*) from agent_commands')" 3 \
    "agent_commands shows the stop command as a third row"

assert_eq "$(scalar "select count(*) from agent_executions where session_id='$session'")" 1 \
    "the execution stays one row after it ends"
assert_eq "$(scalar "select cause from agent_executions where session_id='$session'")" "stopped" \
    "agent_executions records the stop cause"
assert_eq "$(scalar "select count(*) from agent_executions where session_id='$session' and ended_at_ms is not null and exit_code is null and exit_reported = false")" 1 \
    "late columns: end time set, no exit code, no engine exit reported"
assert_eq "$(scalar "select count(*) from agent_executions where session_id='$session' and duration_ms = ended_at_ms - started_at_ms")" 1 \
    "duration_ms is ended_at_ms minus started_at_ms"

assert_eq "$(scalar "select count(*) from agent_state where session_id='$session'")" 1 \
    "agent_state still has one row after the end"
assert_eq "$(scalar "select state from agent_state where session_id='$session'")" "COMPLETED" \
    "agent_state moves to COMPLETED on a stopped execution"
assert_eq "$(scalar "select count(*) from agent_state where session_id='$session' and reason is null")" 1 \
    "COMPLETED has no wait reason"
assert_eq "$(scalar "select confidence::text from agent_state where session_id='$session'")" "$(scalar "select (kind -> 'detail' -> 'observation' ->> 'confidence')::float8::text from spawn_events where session_id='$session' and source='observer' order by session_seq desc limit 1")" \
    "agent_state confidence follows the completed judgement"
assert_eq "$(scalar "select state from agent_status where session_id='$session'")" "COMPLETED" \
    "agent_status reports the completed state"
assert_eq "$(scalar "select count(*) from agent_status where session_id='$session' and reason is null")" 1 \
    "agent_status has no reason for a completed session"
assert_eq "$(scalar "select live from agent_status where session_id='$session'")" "f" \
    "agent_status marks the ended execution not live"
assert_eq "$(scalar "select ended_at_ms from agent_status where session_id='$session'")" \
    "$(scalar "select ended_at_ms from agent_executions where session_id='$session'")" \
    "agent_status end time matches agent_executions"

total=$(scalar "select count(*) from spawn_events where session_id='$session'")
observer_rows=$(scalar "select count(*) from spawn_events where session_id='$session' and source='observer'")
assert_eq "$(scalar "select event_count from agent_freshness where session_id='$session'")" "$total" \
    "agent_freshness counts every event including the end"
assert_eq "$(scalar "select last_session_seq from agent_freshness where session_id='$session'")" \
    "$(scalar "select max(session_seq) from spawn_events where session_id='$session'")" \
    "last_session_seq follows the newest event"
assert_eq "$(scalar "select last_event_id from agent_freshness where session_id='$session'")" \
    "$(scalar "select max(id) from spawn_events where session_id='$session'")" \
    "last_event_id follows the newest event"
assert_eq "$(scalar "select count(*) from agent_timeline where session_id='$session'")" \
    "$((total - observer_rows))" "agent_timeline still drops every observer row after the end"
assert_eq "$(scalar "select count(*) from agent_timeline where session_id='$session' and event_name='execution.ended' and source='bridge'")" 1 \
    "agent_timeline shows the bridge end event"
assert_eq "$(scalar "select count(*) from agent_timeline where session_id='$session' and event_name='execution.ended' and source<>'bridge'")" 0 \
    "no engine-sourced end event was written"
