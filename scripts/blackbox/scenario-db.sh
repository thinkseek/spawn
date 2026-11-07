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
# sweep  drops the databases and the logins of databases whose name starts
#        with <prefix> and that no longer exist: what a run killed before its
#        cleanup (a timeout, SIGKILL, a gate abort) left. Every scenario
#        sweeps its own prefix (`bb_011_covered_`) when it starts, before it
#        makes its database. A leftover database goes only when nothing is
#        connected to it (a connected one is a live run, possibly a parallel
#        gate's, and is left alone); then its logins go. An empty prefix
#        sweeps every database's logins only, never a database (by hand,
#        once).
#
# The rule, and why a sweep cannot take a live run's database or logins, is
# in scripts/blackbox/drop-logins.sql: it never drops a database (the sweep
# drops a leftover database itself, above, only one nothing is connected to),
# never a login a database on the server still has, holds anything or is
# logged in as, and never on a server whose port is 15433 (the live server).
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
    if [ -n "$prefix" ]; then
        # Leftover databases of this prefix, dropped before their logins: a
        # database nothing is connected to is a run killed before its
        # cleanup. One that has a connection is a live run (possibly a
        # parallel gate's), left alone. `starts_with`, not `like`: a prefix
        # is a plain name, and `_` is a wildcard under `like`. No
        # `with (force)`: a connection that appears between the check and the
        # drop fails the drop, it is not cut. The server's own databases are
        # never in scope. An empty prefix sweeps logins only (below).
        # shellcheck disable=SC2086 # the command is a prefix, split on purpose
        for left in $($psql_command -X -d postgres -tA -c \
            "select d.datname from pg_database d
              where starts_with(d.datname, '$prefix')
                and d.datname not in ('postgres', 'template0', 'template1', 'spawn_test')
                and not exists (select 1 from pg_stat_activity a where a.datname = d.datname)" 2>/dev/null); do
            case "$left" in
            *[!A-Za-z0-9_]*) ;;
            *) $psql_command -X -d postgres -tA -c "drop database if exists $left" >/dev/null 2>&1 ||
                echo "scenario-db: could not drop database $left" >&2 ;;
            esac
        done
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
