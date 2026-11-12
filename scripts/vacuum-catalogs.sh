#!/bin/sh
# vacuum-catalogs.sh: VACUUM (ANALYZE) the system catalogs of a test
# database (backlog P21, its non-deleting part), as step 5 of
# scripts/maintenance.sh; and, run once by hand, VACUUM (FULL, ANALYZE)
# them to give back the space a leak left (sync/storage-testdb-bloat.md).
#
#   vacuum-catalogs.sh [--url-var NAME]                  # plain, nightly (step 5)
#   vacuum-catalogs.sh [--url-var NAME] --full           # FULL: a dry run, the default
#   vacuum-catalogs.sh [--url-var NAME] --full --apply   # FULL, for real
#
# Every crate test run makes schemas in a test database and drops them
# again (P19's TestSchema), and even a clean run grows its catalogs
# (pg_class, pg_attribute, pg_depend, ...) by 74-134 MB of dead rows. A
# plain VACUUM makes that space reusable, so the catalogs stop growing with
# every run; ANALYZE keeps the planner's view of them current. It takes no
# lock that stops a test, drops no schema and removes no file: the sweep of
# left schemas (scripts/sweep-test-schemas.sh) stays print-only until the
# owner allows it.
#
# A plain VACUUM never gives a file back (it only cuts empty pages off its
# end): the gates' spawn_test kept 1.9 GB of catalogs, 99% empty, after the
# P19 leak was swept. --full rewrites each catalog (and its TOAST, and
# rebuilds its indexes), so the file is its live rows again; it removes no
# row. It takes each catalog's exclusive lock while it rewrites it, so a
# session that needs one waits: it is for a quiet moment, run once by hand
# (the lead), never on the timer. Without --apply it says, for each
# catalog, its size now, and runs nothing; with --apply it says each one's
# size before and after.
#
# The database is the one the variable NAME holds: SPAWN_FLEET_TEST_DATABASE_URL
# (the default, the fleet's test database) or SPAWN_TEST_DATABASE_URL (the
# gates' shared test server's spawn_test); no other name. The URL is read
# from the environment, else from spawn.env (parsed, never sourced), and is
# never an argument. The step is skipped when it is unset, or with
# SPAWN_MAINTENANCE_CATALOGS=0. It is refused (exit 2, nothing run, before
# connecting) when its port or its database is the live database's:
# SPAWN_DATABASE_URL in the environment and in spawn.env (SPAWN_ENV_FILE,
# default ~/.local/share/spawn-rsi/spawn.env), each compared; and when no
# live URL is known at all, since then nothing proves it is not the live one.
# Neither URL, nor its port or database, is printed: not in a refusal, and
# not in psql's errors (their server names are replaced).
#
# It backs off (exit 3, nothing vacuumed) when another session is connected
# to the database (pg_stat_activity: a backend of that database with a login,
# other than its own), in both modes: a test in flight is left alone, and a FULL
# never waits on one, nor makes one wait. Run again when it is quiet.
#
# Only the database's own catalogs (`pg_catalog` tables that are not shared
# by the cluster): a database's owner may vacuum those, and the fleet's
# login owns its database and is no superuser (deploy/fleet-test-db.sh). A
# catalog it was not allowed to vacuum fails the step, named.
#
# SPAWN_MAINTENANCE_DRY_RUN=1 says what a plain run would vacuum and its
# size, and runs nothing. Exit 0 done, a dry run or skipped; 1 a failure;
# 2 refused; 3 backed off (another session on the database).
set -u
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=scripts/pg-url.sh
. "$here/pg-url.sh"

say() { echo "maintenance: catalogs: $*"; }
refuse() {
    echo "maintenance: catalogs: refused: $*" >&2
    exit 2
}

url_var=SPAWN_FLEET_TEST_DATABASE_URL
full=0
apply=0
while [ $# -gt 0 ]; do
    case $1 in
    --url-var)
        [ $# -ge 2 ] || refuse "--url-var needs a name"
        url_var=$2
        shift
        ;;
    --full) full=1 ;;
    --apply) apply=1 ;;
    *) refuse "usage: vacuum-catalogs.sh [--url-var SPAWN_FLEET_TEST_DATABASE_URL|SPAWN_TEST_DATABASE_URL] [--full [--apply]]" ;;
    esac
    shift
done
case $url_var in
SPAWN_FLEET_TEST_DATABASE_URL) what="the fleet's test database" ;;
SPAWN_TEST_DATABASE_URL) what="the gates' test database" ;;
*) refuse "--url-var names SPAWN_FLEET_TEST_DATABASE_URL or SPAWN_TEST_DATABASE_URL, not $url_var" ;;
esac
[ "$apply" = 0 ] || [ "$full" = 1 ] || refuse "--apply is for --full (a plain VACUUM runs without it)"

# env_value <name>: spawn.env's last <name>=..., parsed, never sourced;
# quotes stripped.
env_file=${SPAWN_ENV_FILE:-${HOME:-}/.local/share/spawn-rsi/spawn.env}
env_value() {
    ev_value=""
    if [ -f "$env_file" ]; then
        ev_value=$(sed -n "s/^$1=//p" "$env_file" | tail -n 1)
        case $ev_value in
        \"*\") ev_value=${ev_value#\"} ev_value=${ev_value%\"} ;;
        \'*\') ev_value=${ev_value#\'} ev_value=${ev_value%\'} ;;
        esac
    fi
    printf '%s' "$ev_value"
}

eval "url=\${$url_var-}"
[ -n "$url" ] || url=$(env_value "$url_var")
if [ -z "$url" ]; then
    say "skipped: $url_var is not set"
    exit 0
fi
if [ "${SPAWN_MAINTENANCE_CATALOGS:-1}" = 0 ]; then
    say "skipped: SPAWN_MAINTENANCE_CATALOGS=0"
    exit 0
fi

live_file=$(env_value SPAWN_DATABASE_URL)
live_env=${SPAWN_DATABASE_URL-}
[ -n "$live_env$live_file" ] ||
    refuse "no live database URL is known (SPAWN_DATABASE_URL, in the environment or spawn.env): nothing proves the URL is not the live database"

pg_url_parse "$url" || refuse "$url_var is not one postgres URL"
test_port=${pg_url_port:-5432}
test_database=$pg_url_database
test_host=$pg_url_host
[ -n "$test_database" ] || refuse "$url_var names no database"
for live in "$live_env" "$live_file"; do
    [ -n "$live" ] || continue
    pg_url_parse "$live" || refuse "a live SPAWN_DATABASE_URL cannot be read, so it cannot be told apart"
    if [ "${pg_url_port:-5432}" = "$test_port" ]; then
        refuse "$url_var names the live database's port (the live server's cluster)"
    fi
    if [ "$pg_url_database" = "$test_database" ]; then
        refuse "$url_var names the live database's name"
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
trap 'rm -f "$scratch/err" "$scratch/out" "$scratch/before"; rmdir "$scratch"' EXIT
pg_url_export "$url" || exit 1
PGDATABASE=$test_database
export PGDATABASE
# A FULL never queues behind a lock (a queued exclusive lock would hold up
# every session behind it): it fails at once instead.
if [ "$full" = 1 ]; then lock_timeout=5s; else lock_timeout=60s; fi
PGOPTIONS="${PGOPTIONS:+$PGOPTIONS }-c lock_timeout=$lock_timeout"
export PGOPTIONS
q() { psql -X -qtA -v ON_ERROR_STOP=1 "$@" 2>"$scratch/err"; }

catalogs="select c.oid from pg_class c
            where c.relnamespace = 'pg_catalog'::regnamespace and c.relkind = 'r'
              and not c.relisshared"
size_sql="select coalesce(sum(pg_total_relation_size(oid)), 0) from ($catalogs) x"
list_sql="select string_agg(format('pg_catalog.%I', c.relname), ', ' order by c.relname)
            from pg_class c where c.oid in ($catalogs)"
each_sql="select c.relname || ' ' || pg_total_relation_size(c.oid)
            from pg_class c where c.oid in ($catalogs) order by c.relname"
# Another session: a backend of this database with a login (a background
# worker such as autovacuum has none); its backend_type is the reader's to
# see only with pg_read_all_stats, so a null one counts.
others_sql="select count(*) from pg_stat_activity
              where datname = current_database() and pid <> pg_backend_pid()
                and usename is not null
                and coalesce(backend_type, 'client backend') = 'client backend'"

if ! before=$(q -c "$size_sql") || ! tables=$(q -c "$list_sql"); then
    echo "maintenance: catalogs: error: the test database cannot be read:" >&2
    withheld "$scratch/err" >&2
    exit 1
fi
count=$(printf '%s\n' "$tables" | tr ',' '\n' | grep -c .)
mb() { echo $((($1 + 524287) / 1048576)); }
# size <bytes>: MB, or kB under 1 MB (a catalog's own line).
size() { if [ "$1" -ge 1048576 ]; then echo "$(mb "$1") MB"; else echo "$((($1 + 512) / 1024)) kB"; fi; }
# backs_off: another session on the database; the step leaves it alone.
backs_off() {
    others=$(q -c "$others_sql") || {
        echo "maintenance: catalogs: error: pg_stat_activity cannot be read:" >&2
        withheld "$scratch/err" >&2
        exit 1
    }
    if [ "$others" != 0 ]; then
        say "backed off: $others other session(s) on $what; nothing vacuumed (run again when it is quiet)"
        exit 3
    fi
}

if [ "$full" = 0 ]; then
    if [ "${SPAWN_MAINTENANCE_DRY_RUN:-0}" = 1 ]; then
        say "would vacuum $count catalogs of $what ($(mb "$before") MB) (dry run)"
        exit 0
    fi
    backs_off
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
    say "vacuumed $count catalogs of $what: $(mb "$before") MB before, $(mb "$after") MB after (space made reusable; nothing dropped)"
    exit 0
fi

# --full: each catalog's size (bytes), then the rewrite.
q -c "$each_sql" >"$scratch/before" || {
    echo "maintenance: catalogs: error: the catalogs' sizes cannot be read:" >&2
    withheld "$scratch/err" >&2
    exit 1
}
if [ "$apply" = 0 ]; then
    while read -r name bytes; do
        say "  $name: $(size "$bytes")"
    done <"$scratch/before"
    say "would VACUUM (FULL, ANALYZE) $count catalogs of $what, $(mb "$before") MB now (dry run: --apply runs it; it removes no row)"
    exit 0
fi
backs_off
if ! q -c "VACUUM (FULL, ANALYZE) $tables" >"$scratch/out"; then
    echo "maintenance: catalogs: error: VACUUM FULL failed (nothing is lost: a catalog not rewritten is as it was):" >&2
    withheld "$scratch/err" >&2
    exit 1
fi
if grep -q 'WARNING' "$scratch/err"; then
    echo "maintenance: catalogs: error: some catalogs were not rewritten:" >&2
    withheld "$scratch/err" >&2
    exit 1
fi
q -c "$each_sql" | while read -r name bytes; do
    was=$(awk -v n="$name" '$1 == n { print $2 }' "$scratch/before")
    say "  $name: $(size "${was:-0}") -> $(size "$bytes")"
done
after=$(q -c "$size_sql") || after=$before
say "VACUUM (FULL, ANALYZE) of $count catalogs of $what: $(mb "$before") MB before, $(mb "$after") MB after (no row removed)"
