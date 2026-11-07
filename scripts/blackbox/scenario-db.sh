#!/bin/sh
# A test database and the logins init-db makes for it, dropped together.
#
# usage: sh scripts/blackbox/scenario-db.sh drop <database>
#        sh scripts/blackbox/scenario-db.sh sweep <prefix>
#
# drop   drops the database (`with (force)`), then its three logins
#        (spawn_supervisor_/spawn_reader_/spawn_human_<database>): roles
#        belong to the whole server, so dropping the database leaves them.
#        Every scenario's cleanup (rsi/scenarios) and the black-box runner
#        (scripts/blackbox/run-scenarios.sh) drop their database this way.
# sweep  drops what a run killed before its cleanup (a timeout, SIGKILL, a
#        gate abort) left of databases whose name starts with <prefix>: the
#        leftover databases, then the logins of databases that no longer
#        exist. Every scenario sweeps its own prefix (`bb_011_covered_`) when
#        it starts, before it makes its database. An empty prefix sweeps
#        every database's logins only, never a database (by hand, once).
#
# A leftover database is one that nothing is connected to AND that is older
# than SPAWN_SCENARIO_DB_SWEEP_MINUTES (default 180). "Nothing connected"
# alone is not proof that a run is dead. A live run holds no connection
# right after it makes its database (before init-db), between two psql
# calls, or while its daemons reconnect. If another copy of the same
# scenario started in such a moment (an agent's run by hand beside a gate on
# the shared test server, or two gates), its sweep would drop the live run's
# database: the base aborts, or the candidate reads as a regression. The age
# is the database's own: the time CREATE DATABASE wrote base/<oid>/PG_VERSION,
# a file it never writes again, read with pg_stat_file. The test server's
# login is a superuser, and so is a gate's own server's. A database whose age
# cannot be read is never dropped: one outside the default tablespace (no
# such file), or a server that refuses pg_stat_file. The default is longer
# than any run lives: a gate's wall clock is 120 minutes, and a scenario run
# by hand takes minutes. A setting that is not a whole number of minutes
# drops no database. 0 drops every leftover nothing is connected to, for a
# test that kills a run and sweeps right after.
#
# The rule for logins, and why a sweep cannot take a live run's logins, is
# in scripts/blackbox/drop-logins.sql: it never drops a database, never a
# login a database on the server still has, holds anything or is logged in
# as, and never on a server whose port is 15433 (the live server).
# It prints one
# line, `<what>: dropped <n> of <m>`
# or `<what>: skipped: <why>`, and exits 0 when the statement ran, 1 when it
# could not (no server, no psql), 2 on bad usage.
#
# The server is psql's: libpq's variables (scripts/pg-url.sh pg_url_export
# sets them from SPAWN_TEST_DATABASE_URL), and SPAWN_PSQL as the command
# (default psql; the runner's may be `docker exec c psql -U postgres`, so the
# SQL goes on the command line, not in a file psql would have to read).
set -u

here=$(cd "$(dirname "$0")/../.." && pwd)
sql_file=$here/scripts/blackbox/drop-logins.sql
psql_command=${SPAWN_PSQL:-psql}

usage() {
    echo "usage: scenario-db.sh drop <database> | sweep <prefix>" >&2
    exit 2
}
[ "$#" -eq 2 ] || usage
action=$1
name=$2
# Names reach SQL as text literals and identifiers: plain ones only.
case "$name" in
*[!A-Za-z0-9_]*)
    echo "scenario-db: not a plain name: $name" >&2
    exit 2
    ;;
esac
case "$action" in
drop) [ -n "$name" ] || usage ;;
sweep) ;;
*) usage ;;
esac
# Before any connection: the live server is never touched, whatever its
# spelling (the SQL checks the server's own port too).
if [ "${PGPORT-}" = 15433 ]; then
    echo "$action $name: skipped: port 15433 is the live server"
    exit 0
fi
sql=$(cat "$sql_file") || exit 1

if [ "$action" = drop ]; then
    # shellcheck disable=SC2086 # the command is a prefix, split on purpose
    $psql_command -X -d postgres -tA -c "drop database if exists $name with (force)" >/dev/null 2>&1 ||
        echo "scenario-db: could not drop database $name" >&2
    database=$name
    prefix=""
else
    database=""
    prefix=$name
    minutes=${SPAWN_SCENARIO_DB_SWEEP_MINUTES:-180}
    case "$minutes" in
    *[!0-9]*)
        echo "scenario-db: SPAWN_SCENARIO_DB_SWEEP_MINUTES is a whole number of minutes, not '$minutes': no database is swept" >&2
        minutes=""
        ;;
    esac
    if [ -n "$prefix" ] && [ -n "$minutes" ]; then
        # Leftover databases of this prefix, dropped before their logins:
        # nothing is connected to one, and it is older than the setting
        # (above: without the age, a live run between two connections would
        # pass for a dead one). A database whose PG_VERSION cannot be read
        # (missing_ok gives no time) never passes; a server that refuses
        # pg_stat_file fails the query, and nothing is dropped. `starts_with`,
        # not `like`: a prefix is a plain name, and `_` is a wildcard under
        # `like`. No `with (force)`: a connection that appears between the
        # check and the drop fails the drop, it is not cut. The server's own
        # databases are never in scope. An empty prefix sweeps logins only
        # (below).
        # shellcheck disable=SC2086 # the command is a prefix, split on purpose
        leftovers=$($psql_command -X -d postgres -tA -c \
            "select d.datname from pg_database d
              where starts_with(d.datname, '$prefix')
                and d.datname not in ('postgres', 'template0', 'template1', 'spawn_test')
                and not exists (select 1 from pg_stat_activity a where a.datname = d.datname)
                and (pg_stat_file('base/' || d.oid || '/PG_VERSION', true)).modification
                    <= now() - make_interval(mins => $minutes)" 2>/dev/null) || {
            echo "scenario-db: sweep $prefix: cannot read the age of its databases: no database is swept" >&2
            leftovers=""
        }
        # One name per line, split without globbing: a name that is not
        # plain is skipped, never expanded.
        set -f
        for left in $leftovers; do
            case "$left" in
            *[!A-Za-z0-9_]*) ;;
            *) $psql_command -X -d postgres -tA -c "drop database if exists $left" >/dev/null 2>&1 ||
                echo "scenario-db: could not drop database $left" >&2 ;;
            esac
        done
        set +f
    fi
fi
# One session: the settings, the statement, its result.
# shellcheck disable=SC2086
output=$($psql_command -X -d postgres -tAq \
    -c "select set_config('spawn_logins.database', '$database', false) is null,
               set_config('spawn_logins.prefix', '$prefix', false) is null" \
    -c "$sql" \
    -c "select 'result: ' || current_setting('spawn_logins.result', true)" 2>&1)
result=$(printf '%s\n' "$output" | sed -n 's/^result: //p' | tail -n 1)
case "$result" in
"dropped "* | "skipped: "*)
    echo "$action $name: $result"
    exit 0
    ;;
esac
echo "scenario-db: $action $name failed:" >&2
printf '%s\n' "$output" | tail -n 5 >&2
exit 1
