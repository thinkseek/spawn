#!/bin/sh
# vacuum-catalogs.sh: VACUUM (ANALYZE) the system catalogs of the fleet's
# test database (backlog P21, its non-deleting part), as step 5 of
# scripts/maintenance.sh.
#
# Every crate test run makes schemas in that database and drops them again
# (P19's TestSchema), and even a clean run grows its catalogs (pg_class,
# pg_attribute, pg_depend, ...) by 74-134 MB of dead rows. A plain VACUUM
# makes that space reusable, so the catalogs stop growing with every run;
# ANALYZE keeps the planner's view of them current. It takes no lock that
# stops a test (no VACUUM FULL), drops no schema and removes no file: the
# sweep of left schemas (scripts/sweep-test-schemas.sh) stays print-only
# until the owner allows it.
#
# The database is the one SPAWN_FLEET_TEST_DATABASE_URL names (spawn.env;
# the step is skipped when it is unset, or with
# SPAWN_MAINTENANCE_CATALOGS=0). It is refused (exit 2, nothing run) when
# its port or its database is the live database's: SPAWN_DATABASE_URL in
# the environment and in spawn.env (SPAWN_ENV_FILE, default
# ~/.local/share/spawn-rsi/spawn.env), each compared; and when no live URL
# is known at all, since then nothing proves it is not the live one.
# Neither URL, nor its port or database, is printed: not in a refusal, and
# not in psql's errors (their server names are replaced).
#
# Only the database's own catalogs (`pg_catalog` tables that are not shared
# by the cluster): a database's owner may vacuum those, and the fleet's
# login owns its database and is no superuser (deploy/fleet-test-db.sh). A
# catalog it was not allowed to vacuum fails the step, named.
#
# SPAWN_MAINTENANCE_DRY_RUN=1 says what it would vacuum and its size, and
# runs nothing. Exit 0 done or skipped, 1 a failure, 2 refused.
set -u
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=scripts/pg-url.sh
. "$here/pg-url.sh"

say() { echo "maintenance: catalogs: $*"; }
refuse() {
    echo "maintenance: catalogs: refused: $*" >&2
    exit 2
}

url=${SPAWN_FLEET_TEST_DATABASE_URL-}
if [ -z "$url" ]; then
    say "skipped: SPAWN_FLEET_TEST_DATABASE_URL is not set"
    exit 0
fi
if [ "${SPAWN_MAINTENANCE_CATALOGS:-1}" = 0 ]; then
    say "skipped: SPAWN_MAINTENANCE_CATALOGS=0"
    exit 0
fi

# spawn.env's SPAWN_DATABASE_URL: parsed, never sourced; quotes stripped.
env_file=${SPAWN_ENV_FILE:-${HOME:-}/.local/share/spawn-rsi/spawn.env}
live_file=""
if [ -f "$env_file" ]; then
    live_file=$(sed -n 's/^SPAWN_DATABASE_URL=//p' "$env_file" | tail -n 1)
    case $live_file in
    \"*\") live_file=${live_file#\"} live_file=${live_file%\"} ;;
    \'*\') live_file=${live_file#\'} live_file=${live_file%\'} ;;
    esac
fi
live_env=${SPAWN_DATABASE_URL-}
[ -n "$live_env$live_file" ] ||
    refuse "no live database URL is known (SPAWN_DATABASE_URL, in the environment or spawn.env): nothing proves the URL is not the live database"

pg_url_parse "$url" || refuse "SPAWN_FLEET_TEST_DATABASE_URL is not one postgres URL"
test_port=${pg_url_port:-5432}
test_database=$pg_url_database
test_host=$pg_url_host
[ -n "$test_database" ] || refuse "SPAWN_FLEET_TEST_DATABASE_URL names no database"
for live in "$live_env" "$live_file"; do
    [ -n "$live" ] || continue
    pg_url_parse "$live" || refuse "a live SPAWN_DATABASE_URL cannot be read, so it cannot be told apart"
    if [ "${pg_url_port:-5432}" = "$test_port" ]; then
        refuse "SPAWN_FLEET_TEST_DATABASE_URL names the live database's port (the live server's cluster)"
    fi
    if [ "$pg_url_database" = "$test_database" ]; then
        refuse "SPAWN_FLEET_TEST_DATABASE_URL names the live database's name"
    fi
done

# withheld <file>: psql's message, its server's host, port and database
# replaced (they are not printed).
withheld() {
    awk -v host="$test_host" -v port="$test_port" -v db="$test_database" '
        function swap(text, from, to,    at, out) {
            if (from == "") return text
            out = ""
            while ((at = index(text, from)) > 0) {
                out = out substr(text, 1, at - 1) to
                text = substr(text, at + length(from))
            }
            return out text
        }
        { line = swap($0, host, "<host>"); line = swap(line, db, "<database>"); print swap(line, port, "<port>") }
    ' "$1" | head -n 5
}

scratch=$(mktemp -d "${TMPDIR:-/tmp}/vacuum-catalogs.XXXXXX") || {
    echo "maintenance: catalogs: error: no scratch directory" >&2
    exit 1
}
trap 'rm -f "$scratch/err" "$scratch/out"; rmdir "$scratch"' EXIT
pg_url_export "$url" || exit 1
PGOPTIONS="${PGOPTIONS:+$PGOPTIONS }-c lock_timeout=60s"
export PGOPTIONS
q() { psql -X -qtA -v ON_ERROR_STOP=1 "$@" 2>"$scratch/err"; }

catalogs="select c.oid from pg_class c
            where c.relnamespace = 'pg_catalog'::regnamespace and c.relkind = 'r'
              and not c.relisshared"
size_sql="select coalesce(sum(pg_total_relation_size(oid)), 0) from ($catalogs) x"
list_sql="select string_agg(format('pg_catalog.%I', c.relname), ', ' order by c.relname)
            from pg_class c where c.oid in ($catalogs)"

if ! before=$(q -c "$size_sql") || ! tables=$(q -c "$list_sql"); then
    echo "maintenance: catalogs: error: the test database cannot be read:" >&2
    withheld "$scratch/err" >&2
    exit 1
fi
count=$(printf '%s\n' "$tables" | tr ',' '\n' | grep -c .)
mb() { echo $((($1 + 524287) / 1048576)); }
if [ "${SPAWN_MAINTENANCE_DRY_RUN:-0}" = 1 ]; then
    say "would vacuum $count catalogs of the fleet's test database ($(mb "$before") MB) (dry run)"
    exit 0
fi
if ! q -c "VACUUM (ANALYZE) $tables" >"$scratch/out"; then
    echo "maintenance: catalogs: error: VACUUM failed:" >&2
    withheld "$scratch/err" >&2
    exit 1
fi
# A catalog the login may not vacuum is skipped with a warning, not an
# error: the step fails, naming it.
if grep -q 'WARNING' "$scratch/err"; then
    echo "maintenance: catalogs: error: some catalogs were not vacuumed:" >&2
    withheld "$scratch/err" >&2
    exit 1
fi
after=$(q -c "$size_sql") || after=$before
say "vacuumed $count catalogs of the fleet's test database: $(mb "$before") MB before, $(mb "$after") MB after (space made reusable; nothing dropped)"
