#!/bin/sh
# Check: the candidate's schema migrates the base's database. Every start of
# Spawn runs crates/spawn-postgres/sql/schema.sql over whatever the database
# already holds (PostgresStore::migrate: the whole file, one transaction), so
# a schema.sql that a fresh database takes but an earlier one refuses passes
# every other check and fails only at deploy: 554a4bd4's deploy, on
# "cannot change name of view column ended_at_ms to signal" (42P16), because
# 3806aa92 put agent_executions' new columns mid-list and `create or replace
# view` may only append. tests/upgrade.rs migrates from the first shape of
# every table (tests/first-shapes.sql); its views are made fresh there, so it
# cannot see this.
#
# In a scratch database on the test server (the gate's own, where it runs
# one): the base's schema.sql, then a few rows as a live database holds them
# (a session's execution started and ended, a mission with a task), then the
# candidate's schema.sql, then the candidate's again (every start applies it:
# it must apply over itself), then the rows read back through the
# candidate's views. Each schema is applied as migrate applies it, one
# transaction, stopping at the first error, whose words are printed.
#
# Why base to candidate is enough, and the assumption it rests on: the live
# database can lag main by several landings (2026-11-09: live 08bfa3be, main
# 3806aa92), so a deploy applies main's schema over one some landings old,
# never the one just before. Checking each landing's step composes into that
# jump only while schema.sql stays declarative and append-only: every
# statement says what is to exist (`if not exists`, `create or replace`,
# columns added, never renamed, reordered or retyped in place), so the
# newest file migrates any earlier database the steps between did. A
# schema.sql that took a path (a step that only works from the shape just
# before) would break that composition; this check, applied at every step,
# refuses the shapes that would need one: a view whose columns move, a
# statement that does not apply over itself.
#
# The one known exception: 3806aa92 -> daec18a1. 3806aa92 put
# agent_executions' signal and evidence mid-list and daec18a1 appends them,
# so a database that took 3806aa92's view refuses daec18a1's the same way
# (42P16, the other way round), and this check refuses that step. It is safe
# only because no database kept 3806aa92's view: live's deploy of it rolled
# back, and a scan on 2026-11-09 04:2x of every database on the ccd server (2)
# and the fleet test server (7), in every schema, found no agent_executions
# with signal before ended_at_ms. The jump live makes, 08bfa3be -> daec18a1,
# passes.
#
# Rows: inserts naming only the columns of tests/first-shapes.sql, which every
# later schema keeps, so the same SQL writes into any base; each skipped where
# the base has no such table (a tree that is not Spawn's, the suite's
# fixture). Then the migrated database's columns are compared with those of
# a fresh database that took the candidate's schema alone: every table's
# columns, in order, with their types and nullability. An `alter table … add
# column if not exists` left out passes everything above (the base's table
# exists, so `create table if not exists` adds nothing, and plpgsql does not
# read columns until it runs) and fails every write that names the column;
# here it is the missing column. Defaults are not compared. Not done:
# migrate's second step, the logins' passwords (role
# attributes, set again on every start).
#
# Exit status: 0 the candidate's schema migrates the base's database (or
# there is no schema to check: said); 1 it does not (psql's error printed);
# 2 a harness failure (no psql, no server, no scratch database).
set -eu
: "${SPAWN_TEST_DATABASE_URL:?check-schema-upgrade needs SPAWN_TEST_DATABASE_URL}"
controller=$(cd "$(dirname "$0")/../.." && pwd)
schema=crates/spawn-postgres/sql/schema.sql
candidate=$(pwd)
base=${SPAWN_BASELINE_TREE-}

harness() {
    echo "check-schema-upgrade: $*" >&2
    exit 2
}
if [ ! -f "$candidate/$schema" ]; then
    echo "check-schema-upgrade: the candidate has no $schema: nothing to check"
    exit 0
fi
if [ -z "$base" ] || [ ! -f "$base/$schema" ]; then
    echo "check-schema-upgrade: no base tree with $schema (SPAWN_BASELINE_TREE): nothing to check"
    exit 0
fi
command -v psql >/dev/null 2>&1 || harness "no psql"

# libpq's variables from the test URL (scripts/pg-url.sh), never a URL on a
# command line.
# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"

# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
db="check_schema_upgrade_$run"
work=$(mktemp -d) || harness "mktemp failed"
fresh="${db}_fresh"
made=""
made_fresh=""
cleanup() {
    if [ -n "$made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$db" >/dev/null 2>&1 || true
    fi
    if [ -n "$made_fresh" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$fresh" >/dev/null 2>&1 || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "check_schema_upgrade_" >/dev/null 2>&1 || true
psql -X -d postgres -tA -c "create database $db" >/dev/null 2>"$work/create.err" ||
    harness "cannot create the scratch database: $(head -n 3 "$work/create.err")"
made=1

# apply <tree> [database]: its schema.sql over the scratch database (or
# the one named), as migrate does.
apply() {
    psql -X -q -d "${2:-$db}" -1 -v ON_ERROR_STOP=1 -f "$1/$schema" >"$work/apply.out" 2>&1
}
# columns <database>: every column of its tables in the schema the file
# made them in, table by table, in order (renumbered: a column dropped in
# place leaves a gap a fresh table does not have), with type and nullability.
columns() {
    psql -X -d "$1" -tA -c "select format('%s #%s %s %s %s', c.table_name,
                                          row_number() over (partition by c.table_name
                                                             order by c.ordinal_position),
                                          c.column_name, c.data_type, c.is_nullable)
                              from information_schema.columns c
                              join information_schema.tables t
                                on t.table_schema = c.table_schema
                               and t.table_name = c.table_name
                             where c.table_schema = current_schema()
                               and t.table_type = 'BASE TABLE'
                             order by c.table_name, c.ordinal_position"
}
# reachable: the server still answers (a failure that is the server's is a
# harness failure, never the candidate's).
reachable() { psql -X -d "$db" -tA -c 'select 1' >/dev/null 2>&1; }
said() { grep -E 'ERROR|FATAL|error' "$work/apply.out" | head -n 5 | sed 's/^/  /'; }

if ! apply "$base"; then
    reachable || harness "the server stopped answering while the base's schema applied"
    echo "check-schema-upgrade: the base's own schema.sql does not apply to an empty database:"
    said
    # The base's fault, not the candidate's: the gate runs this check on the
    # base too, and a check failing there is no rejection.
    exit 1
fi

# Rows as a live database holds them, through first-shape columns only.
psql -X -q -d "$db" -v ON_ERROR_STOP=1 >"$work/rows.out" 2>&1 <<'SQL' || {
do $$
begin
    if to_regclass('spawn_events') is not null then
        insert into spawn_events (agent_id, session_id, execution_id, source, kind, session_seq,
                                  recorded_at_ms)
        values ('agent_check', 'session_check', 'exec_check', 'bridge',
                '{"name": "execution.started",
                  "detail": {"engine": "claude", "workspace": "/w/check"}}', 1, 1000),
               ('agent_check', 'session_check', 'exec_check', 'bridge',
                '{"name": "execution.ended",
                  "detail": {"cause": "exited", "exit_code": 0}}', 2, 2000);
    end if;
    if to_regclass('mission_missions') is not null and to_regclass('mission_tasks') is not null then
        insert into mission_missions (id, text, origin, created_at_ms, updated_at_ms)
        values (900001, 'check-schema-upgrade', 'check', 1000, 1000);
        insert into mission_tasks (mission_id, kind, agent_id, session_id, brief, created_by,
                                   created_at_ms, updated_at_ms)
        values (900001, 'ephemeral', 'agent_check', 'session_check', 'a task', 'check', 1000, 1000);
    end if;
end
$$;
SQL
    reachable || harness "the server stopped answering while the rows were written"
    echo "check-schema-upgrade: the rows a live database holds cannot be written into the base's schema:"
    grep -E 'ERROR|error' "$work/rows.out" | head -n 5 | sed 's/^/  /'
    exit 1
}

if ! apply "$candidate"; then
    reachable || harness "the server stopped answering while the candidate's schema applied"
    echo "check-schema-upgrade: the candidate's schema.sql cannot migrate the base's database (a deploy would refuse it the same way):"
    said
    exit 1
fi
if ! apply "$candidate"; then
    reachable || harness "the server stopped answering while the candidate's schema applied again"
    echo "check-schema-upgrade: the candidate's schema.sql applied once but not over itself (applied twice: every start applies it):"
    said
    exit 1
fi

# The rows, read back through the candidate's views.
if [ -n "$(psql -X -d "$db" -tA -c "select to_regclass('agent_executions')")" ]; then
    ended=$(psql -X -d "$db" -tA -c "select count(*) from agent_executions
        where session_id = 'session_check' and execution_id = 'exec_check'
          and ended_at_ms = 2000 and started_at_ms = 1000" 2>"$work/read.err") ||
        { reachable || harness "the server stopped answering"; echo "check-schema-upgrade: reading agent_executions failed:"; sed 's/^/  /' "$work/read.err"; exit 1; }
    if [ "$ended" != 1 ]; then
        echo "check-schema-upgrade: the migration lost an execution: agent_executions does not show the base's started-and-ended execution (found $ended)"
        exit 1
    fi
fi
if [ -n "$(psql -X -d "$db" -tA -c "select to_regclass('mission_tasks')")" ]; then
    tasks=$(psql -X -d "$db" -tA -c "select count(*) from mission_tasks where mission_id = 900001") ||
        harness "the server stopped answering"
    if [ "$tasks" != 1 ]; then
        echo "check-schema-upgrade: the migration lost a task: mission 900001 has $tasks tasks, not 1"
        exit 1
    fi
fi
# The migrated columns against a fresh database's.
psql -X -d postgres -tA -c "create database $fresh" >/dev/null 2>"$work/create.err" ||
    harness "cannot create the fresh database: $(head -n 3 "$work/create.err")"
made_fresh=1
if ! apply "$candidate" "$fresh"; then
    reachable || harness "the server stopped answering while the candidate's schema applied fresh"
    echo "check-schema-upgrade: the candidate's schema.sql does not apply to an empty database:"
    said
    exit 1
fi
columns "$db" >"$work/migrated.cols" 2>"$work/cols.err" ||
    { reachable || harness "the server stopped answering"; harness "reading the migrated columns failed: $(head -n 3 "$work/cols.err")"; }
columns "$fresh" >"$work/fresh.cols" 2>"$work/cols.err" ||
    harness "reading the fresh columns failed: $(head -n 3 "$work/cols.err")"
[ -s "$work/fresh.cols" ] || harness "the fresh database has no columns to compare"
if ! diff "$work/fresh.cols" "$work/migrated.cols" >"$work/cols.diff"; then
    echo "check-schema-upgrade: the migrated database's columns differ from a fresh one's (< fresh, > migrated; an added column needs its \`alter table ... add column if not exists\`, appended):"
    grep -E '^[<>]' "$work/cols.diff" | head -n 10 | sed 's/^/  /'
    exit 1
fi
echo "check-schema-upgrade: the candidate's schema.sql migrates the base's database, and applies over itself; every table's columns are as a fresh database has them"
