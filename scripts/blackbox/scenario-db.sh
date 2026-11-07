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
# sweep  drops the logins of databases whose name starts with <prefix> and
#        that no longer exist: what a run killed before its cleanup (a
#        timeout, SIGKILL, a gate abort) left. Every scenario sweeps its own
#        prefix (`bb_011_covered_`) when it starts, before it makes its
#        database. An empty prefix sweeps every database's logins (by hand,
#        once).
#
# The rule, and why a sweep cannot take a live run's logins, is in
# scripts/blackbox/drop-logins.sql: never a database, never a login a
# database on the server still has, holds anything or is logged in as, only
# under the server's migration lock, and never on a server whose port is
# 15433 (the live server). It prints one line, `<what>: dropped <n> of <m>`
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
