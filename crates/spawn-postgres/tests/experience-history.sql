-- A history for the experience views (tests/experience.rs, and spawn-cli's
-- serve tests, which check the scorecard counts the same): hours after
-- B = 2023-11-15T00:00:00Z, the window [B, B + 100 h). Worked by hand:
--   interventions: command 2 (human at 5, spawn at 15), owner_task 1 (task 1
--     at 10; task 2's mission is accepted by its gate), cancel_reassign 1
--     (task 5 cancelled by a person at 45, task 6 of the same objective at
--     50; task 7 at 55 is not a second; task 3's cancellation was a
--     loop's), resume 1 (70): 5.
--   outcomes: accepted 1 (task 1 at 20; task 2's review on a gated mission
--     closes nothing), landed 1 (task 2 at 30), cancelled 2 (task 3 by
--     spawn-dispatch at 35, task 5 at 45), over_budget 1 (task 6 at 60;
--     its cancellation at 61 closes nothing), aborted 1 (task 7 at 62);
--     task 1's cancellation at 21 closes nothing; task 4 is open.
--   decisions: rules 1, model 3 (jev twice, one with no judge), human 2,
--     dispatcher 3, supervisor 1, liveness 1, spawn-integrate 1.
--   escalations: 3 ("s a" and "s b" at 25, "s c" at 65; "s z" went to a
--     worker, not the supervisor).
--   agent hours: 20 (exec a from -10 to 20) + 10 (exec b from 90, running).
--   missions: 1 open 99 h (from 1), busy 78 (10-20, 31-35, 36-100), idle
--     21; 2 open 78 h (2 to 80), busy 18 (12-30), idle 60: idle.
create function pg_temp.ms(h numeric) returns bigint language sql as $$
    select 1700006400000 + round(h * 3600000)::bigint $$;
create temp sequence seed_seq;
create function pg_temp.event(h numeric, agent text, session text, execution text, source text,
                              name text, detail jsonb) returns void language sql as $$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values (agent, session, execution, source, null,
            jsonb_build_object('name', name, 'detail', detail), nextval('seed_seq'), pg_temp.ms(h)) $$;
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as $$
    select pg_temp.event(h, 'agent_control', 'session_control', null, 'control', 'control.recorded',
                         jsonb_build_object('record', r)) $$;
create function pg_temp.cmd(p text, k text, s text, act jsonb, h numeric) returns void
language sql as $$
    insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action, state,
                                succeeded, error, created_at_ms, finished_at_ms)
    values (p, k, replace(s, 'session_', 'agent_'), s, act, 'finished', true, null, pg_temp.ms(h),
            pg_temp.ms(h) + 1000) $$;
create function pg_temp.assign(h numeric, id int, m int, by_ text, brief text, objective int)
returns void language sql as $$
    select pg_temp.rec(h, jsonb_strip_nulls(jsonb_build_object('type', 'task.assigned', 'id', id,
        'mission_id', m, 'kind', 'ephemeral', 'agent_id', 'agent_worker_w_' || id,
        'session_id', 'session_worker_w_' || id, 'brief', brief, 'created_by', by_,
        'objective', objective, 'model', 'default'))) $$;
create function pg_temp.judged(h numeric, observation jsonb) returns void language sql as $$
    select pg_temp.event(h, 'agent_worker_w_4', 'session_worker_w_4', null, 'judge',
        'observation.recorded', jsonb_build_object('observation', observation)) $$;

select pg_temp.rec(1, '{"type":"mission.added","id":1,"text":"busy","origin":"human","sources":[]}');
select pg_temp.rec(2, '{"type":"mission.added","id":2,"text":"gated","origin":"human","sources":[]}');
select pg_temp.rec(2, '{"type":"mission.configured","id":2,"gate":"scripts/gate.sh","accept":"gate"}');
select pg_temp.assign(10, 1, 1, 'human', 'one', null);
select pg_temp.assign(12, 2, 2, 'human', 'two', null);
select pg_temp.rec(20, '{"type":"task.reviewed","id":1,"accepted":true,"note":null}');
select pg_temp.rec(21, '{"type":"task.cancelled","id":1,"by":"human"}');
select pg_temp.rec(22, '{"type":"task.reviewed","id":2,"accepted":true,"note":null}');
select pg_temp.rec(30, '{"type":"task.landed","id":2,"from":"m0","to":"m1"}');
select pg_temp.assign(31, 3, 1, 'spawn_supervisor', 'three', null);
select pg_temp.rec(35, '{"type":"task.cancelled","id":3,"by":"spawn-dispatch"}');
select pg_temp.assign(36, 4, 1, 'spawn_supervisor', 'three', null);
select pg_temp.assign(40, 5, 1, 'spawn_supervisor', 'five', 9);
select pg_temp.rec(45, '{"type":"task.cancelled","id":5}');
select pg_temp.assign(50, 6, 1, 'spawn_supervisor', 'six', 9);
select pg_temp.assign(55, 7, 1, 'spawn_supervisor', 'seven', 9);
select pg_temp.rec(60, '{"type":"task.over_budget","id":6,"reason":"tool calls"}');
select pg_temp.rec(61, '{"type":"task.cancelled","id":6,"by":"human"}');
select pg_temp.rec(62, '{"type":"task.aborted","id":7,"reason":"budget"}');
select pg_temp.rec(70, '{"type":"mission.resumed","id":1}');
select pg_temp.rec(80, '{"type":"mission.closed","id":2,"state":"done","summary":"done"}');
select spawn_control_refold();

select pg_temp.judged(3, '{"state":"WORKING","judge":"rules"}');
select pg_temp.judged(4, '{"state":"WORKING","judge":"jev","latency_ms":100,"rules_state":"WAITING"}');
select pg_temp.judged(6, '{"state":"WORKING"}');
select pg_temp.judged(7, '{"state":"WORKING","judge":"jev","latency_ms":300,"reason":"JUDGE_UNAVAILABLE"}');

select pg_temp.event(-10, 'agent_worker_w_1', 'session_worker_w_1', 'exec_a', 'bridge',
                     'execution.started', '{"engine":"claude","workspace":"/w/a"}');
select pg_temp.event(20, 'agent_worker_w_1', 'session_worker_w_1', 'exec_a', 'bridge',
                     'execution.ended', '{"cause":"exited","exit_code":0}');
select pg_temp.event(90, 'agent_worker_w_4', 'session_worker_w_4', 'exec_b', 'bridge',
                     'execution.started', '{"engine":"claude","workspace":"/w/b"}');

select pg_temp.cmd('human', 'board-1', 'session_worker_w_1', '{"type":"input","text":"a note"}', 5);
select pg_temp.cmd('spawn', 'cli-1', 'session_worker_w_2', '{"type":"input","text":"a note"}', 15);
select pg_temp.cmd('spawn-dispatch', 'digest-1', 'session_supervisor',
                   '{"type":"input","text":"two","tags":["s a","s b"]}', 25);
select pg_temp.cmd('spawn_supervisor', 'sup-1', 'session_worker_w_4', '{"type":"input","text":"go"}', 26);
select pg_temp.cmd('spawn-liveness', 'live-1', 'session_supervisor', '{"type":"resume"}', 27);
select pg_temp.cmd('spawn-integrate', 'integrate-1', 'session_worker_w_2',
                   '{"type":"input","text":"merged main"}', 28);
select pg_temp.cmd('spawn-dispatch', 'digest-2', 'session_supervisor',
                   '{"type":"input","text":"two","tags":["s a","s c"]}', 65);
select pg_temp.cmd('spawn-dispatch', 'nudge-1', 'session_worker_w_4',
                   '{"type":"input","text":"continue","tags":["s z"]}', 66);
