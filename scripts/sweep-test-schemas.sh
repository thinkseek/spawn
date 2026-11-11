#!/bin/bash
# sweep-test-schemas.sh [--min-idle-hours N]: which spawn_test_* schemas of
# the test database (SPAWN_TEST_DATABASE_URL) a sweep would drop, and why
# each other one stays (backlog P21). PRINT-ONLY: it drops nothing. Running
# a sweep for real waits for the owner's word that sweeps may resume.
#
# A crate test names its schema spawn_test_<pid>_<...>_<case> (P19's
# guard, spawn_postgres::testing::TestSchema) and drops it when it passes.
# What is left is a failed test's (kept on purpose, and named), or a killed
# run's (no Drop ran). A schema would be dropped only when both hold:
# - its pid runs no process on this host (/proc/<pid>), and
# - nothing has written to it for --min-idle-hours (default 24): its
#   newest relation file is that old (pg_stat_file, a superuser's or a
#   grantee's). A schema whose files cannot be read stays: its age is
#   unknown. Any access may refresh that time (a read that sets hint bits
#   writes the page), so the age errs young: a sweep keeps more, never
#   less.
# The second rule covers what the first cannot see: a gate runs its tests
# in a PID namespace, so a live test's pid there may run nothing here.
# Other names (no pid in them) always stay. The /proc check sees this
# host only: another host's tests (a gate runner's) are covered by the
# idle rule alone. A real mode, when the owner allows one, must refuse a
# --min-idle-hours below a floor (0 is accepted here, print-only).
#
# Output, one line per schema, then a summary:
#   would-drop <schema> pid=<pid> idle=<h>h size=<MB>MB
#   keep       <schema> pid=<pid> idle=<h>h size=<MB>MB <why>
# The URL goes to psql as PG* variables (scripts/pg-url.sh), never an
# argument. Exit 0; 2 when it cannot read the database.
set -euo pipefail

min_idle_hours=24
while [ "$#" -gt 0 ]; do
    case $1 in
    --min-idle-hours)
        min_idle_hours=${2-}
        shift 2 || { echo "sweep-test-schemas: --min-idle-hours needs a value" >&2; exit 2; }
        ;;
    --dry-run) shift ;; # the only mode there is
    *)
        echo "usage: sweep-test-schemas.sh [--min-idle-hours N] (print-only)" >&2
        exit 2
        ;;
    esac
done
case $min_idle_hours in '' | *[!0-9]*)
    echo "sweep-test-schemas: --min-idle-hours is a whole number of hours" >&2
    exit 2
    ;;
esac
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || { echo "sweep-test-schemas: SPAWN_TEST_DATABASE_URL is required" >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=scripts/pg-url.sh
. "$here/pg-url.sh"
# Print-only by construction too: every transaction of this run's psql is
# read-only, so not even a mistake below could drop anything.
PGOPTIONS="${PGOPTIONS:+$PGOPTIONS }-c default_transaction_read_only=on"
export PGOPTIONS
pg_url_export "$SPAWN_TEST_DATABASE_URL" || { echo "sweep-test-schemas: SPAWN_TEST_DATABASE_URL is not one postgres URL" >&2; exit 2; }
[ -n "${PGDATABASE-}" ] || { echo "sweep-test-schemas: the URL names no database" >&2; exit 2; }

# name|bytes|idle seconds (empty when the files cannot be read)
listing() {
    psql -X -qtA -F '|' -v ON_ERROR_STOP=1 <<SQL
select n.nspname,
       coalesce(sum(pg_total_relation_size(c.oid)) filter (where c.relkind in ('r', 'm')), 0),
       $1
  from pg_namespace n
  left join pg_class c on c.relnamespace = n.oid and c.relkind in ('r', 'i', 't', 'm', 'S')
 where n.nspname like 'spawn\_test\_%'
 group by n.nspname
 order by n.nspname
SQL
}
idle_sql="floor(extract(epoch from now() - max((pg_stat_file(pg_relation_filepath(c.oid), true)).modification)))::bigint"
if ! rows=$(listing "$idle_sql" 2>/dev/null); then
    # No right to read the files: ages unknown, so every schema stays.
    rows=$(listing "null") || { echo "sweep-test-schemas: cannot list the schemas" >&2; exit 2; }
    echo "sweep-test-schemas: cannot read the relation files' times here; every schema stays" >&2
fi

min_idle=$((min_idle_hours * 3600))
drop_count=0 drop_bytes=0 keep_count=0 keep_bytes=0
while IFS='|' read -r name bytes idle; do
    [ -n "$name" ] || continue
    mb=$(((bytes + 524287) / 1048576))
    pid=$(printf '%s\n' "$name" | sed -nE 's/^spawn_test_([a-z]+_)?([0-9]+)_.*/\2/p')
    hours=${idle:+$((idle / 3600))}
    why=""
    if [ -z "$pid" ]; then
        why="no pid in the name"
    elif [ -d "/proc/$pid" ]; then
        why="pid $pid runs here"
    elif [ -z "$idle" ]; then
        why="its age is unknown"
    elif [ "$idle" -lt "$min_idle" ]; then
        why="written ${hours}h ago (< ${min_idle_hours}h)"
    fi
    if [ -z "$why" ]; then
        printf 'would-drop %s pid=%s idle=%sh size=%sMB\n' "$name" "$pid" "$hours" "$mb"
        drop_count=$((drop_count + 1)) drop_bytes=$((drop_bytes + bytes))
    else
        printf 'keep       %s pid=%s idle=%sh size=%sMB %s\n' "$name" "${pid:--}" "${hours:-?}" "$mb" "$why"
        keep_count=$((keep_count + 1)) keep_bytes=$((keep_bytes + bytes))
    fi
done <<<"$rows"
printf 'sweep-test-schemas (print-only, nothing dropped): would drop %s schema(s), %s MB; keeps %s, %s MB\n' \
    "$drop_count" "$(((drop_bytes + 524287) / 1048576))" "$keep_count" "$(((keep_bytes + 524287) / 1048576))"
