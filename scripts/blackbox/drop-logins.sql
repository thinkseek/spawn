-- The restricted logins a test database leaves on its server, and the one
-- rule that drops them. init-db makes three logins per database,
-- spawn_supervisor_<db>, spawn_reader_<db> and spawn_human_<db> (named by
-- spawn_login_role, crates/spawn-postgres/sql/schema.sql), and roles belong
-- to the whole server: dropping the database leaves them. Run by
-- scripts/blackbox/scenario-db.sh (every scenario's cleanup and start) and
-- by crates/spawn-postgres/tests/api.rs (include_str!).
--
-- Connected to the server's `postgres` database, after setting, for the
-- session (set_config(name, value, false)):
--   spawn_logins.database  a database name: that database's three logins
--                          (run after dropping it); or empty, and
--   spawn_logins.prefix    the logins of every database whose name starts
--                          with it (a sweep; empty: every database).
-- It writes spawn_logins.result: `dropped <n> of <m>` (m: the logins in
-- scope, by name), or `skipped: <why>`.
--
-- A login is dropped only when all of these hold:
--   - its name has the shape spawn_(supervisor|reader|human)_[a-z0-9_]+;
--   - no database on the server has it as one of its logins (the name
--     spawn_login_role gives, the long-name hash included);
--   - it holds no privilege and owns nothing anywhere on the server
--     (pg_shdepend): a live database's login holds CONNECT on it;
--   - it is in no role membership, and no session is logged in as it.
-- No init-db can be midway: a migration makes a database's logins and their
-- grants in one transaction, after the database exists, so a sweep sees
-- either nothing of them or logins their database owns. A sweep also holds
-- the server's migration lock (the advisory lock PostgresStore::migrate
-- takes in this database, key 1399957400) while it runs, and skips when
-- the lock is not free within 10 s. DROP ROLE also refuses a login that
-- still holds anything. It never drops a database, and never touches a
-- server whose port is 15433 (the live server's, over TCP and over its
-- socket).
do $drop_logins$
declare
    v_database text := coalesce(current_setting('spawn_logins.database', true), '');
    v_prefix text := coalesce(current_setting('spawn_logins.prefix', true), '');
    v_login record;
    v_scope integer := 0;
    v_lock_timeout text := current_setting('lock_timeout');
    v_dropped integer := 0;
begin
    perform set_config('spawn_logins.result', 'skipped: did not finish', false);
    if current_setting('port') = '15433' then
        perform set_config('spawn_logins.result', 'skipped: port 15433 is the live server', false);
        return;
    end if;
    if current_database() <> 'postgres' then
        perform set_config('spawn_logins.result',
                           'skipped: run in the postgres database, where the migration lock is', false);
        return;
    end if;
    -- A sweep waits for the migrations under way. One database's logins,
    -- dropped after the database, need no lock: no init-db can be making
    -- the logins of a database that is gone.
    if v_database = '' then
        perform set_config('lock_timeout', '10s', true);
        begin
            perform pg_advisory_xact_lock(1399957400);
        exception when lock_not_available then
            perform set_config('spawn_logins.result', 'skipped: a migration holds the lock', false);
            return;
        end;
        perform set_config('lock_timeout', v_lock_timeout, true);
    end if;

    for v_login in
        with bases(base) as (values ('spawn_supervisor'), ('spawn_reader'), ('spawn_human')),
             databases(d, present) as (
                 select datname::text, true from pg_database
                 union all
                 select v_database, false where v_database <> ''),
             -- spawn_login_role over each name.
             logins(rolname, present) as (
                 select case when d ~ '^[a-z0-9_]+$' and length(base) + 1 + length(d) <= 63
                             then base || '_' || d
                             else left(base || '_' || regexp_replace(lower(d), '[^a-z0-9_]+', '_', 'g'), 54)
                                  || '_' || left(md5(d), 8) end,
                        present
                   from databases cross join bases)
        select r.rolname::text as rolname,
               not exists (select 1 from logins l where l.present and l.rolname = r.rolname)
               and not exists (select 1 from pg_shdepend s
                                where s.refclassid = 'pg_authid'::regclass and s.refobjid = r.oid)
               and not exists (select 1 from pg_auth_members m where r.oid in (m.roleid, m.member))
               and not exists (select 1 from pg_stat_activity a where a.usesysid = r.oid)
                   as droppable
          from pg_roles r
         where r.rolname ~ '^spawn_(supervisor|reader|human)_[a-z0-9_]+$'
           and case when v_database <> ''
                    then exists (select 1 from logins l where not l.present and l.rolname = r.rolname)
                    else starts_with(regexp_replace(r.rolname, '^spawn_(supervisor|reader|human)_', ''),
                                     v_prefix) end
         order by r.rolname
    loop
        v_scope := v_scope + 1;
        continue when not v_login.droppable;
        begin
            execute format('drop role %I', v_login.rolname);
            v_dropped := v_dropped + 1;
        exception when undefined_object or dependent_objects_still_exist then
            -- Gone already (another run's cleanup), or it holds something
            -- now: either way it is not this run's to drop.
            null;
        end;
    end loop;
    perform set_config('spawn_logins.result', format('dropped %s of %s', v_dropped, v_scope), false);
end
$drop_logins$;
