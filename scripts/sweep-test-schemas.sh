#!/bin/bash
# sweep-test-schemas.sh [--min-idle-hours N]: which spawn_test_* schemas of
# the test database (SPAWN_TEST_DATABASE_URL) a sweep would drop, and why
# each other one stays (backlog P21). PRINT-ONLY: it drops nothing.
#
# sweep-test-schemas.sh --apply <list-file> [--min-idle-hours N]: drops
# exactly the would-drop names of <list-file>'s section for this database,
# a list a print-only run wrote and a reviewer passed (owner's decision of
# 2026-11-12 15:45: list first, a reviewer checks it, then only the reviewed
# names go). Each name is checked again when it is its turn; one that no
# longer qualifies, or is gone, is said (`skipped <name> <why>`) and kept.
#
# A crate test names its schema spawn_test_<pid>_<...>_<case> (P19's
# guard, spawn_postgres::testing::TestSchema) and drops it when it passes.
# What is left is a failed test's (kept on purpose, and named), or a killed
# run's (no Drop ran). A schema qualifies only when both hold:
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
# idle rule alone.
#
# Output of the print-only run: the section line, one line per schema, then
# a summary. The section line names the database the list is for (its name
# and port, never the URL), and --apply takes only that section:
#   database <name> port <port>
#   would-drop <schema> pid=<pid> idle=<h>h size=<MB>MB
#   keep       <schema> pid=<pid> idle=<h>h size=<MB>MB <why>
# A list file may hold several runs' output, one section each, in any text
# around them (a markdown file with code blocks): only lines that start with
# `database ` or `would-drop ` are read.
#
# --apply refuses (exit 2, nothing dropped), never skips silently, when:
# - no live database URL is known (SPAWN_DATABASE_URL, in the environment or
#   spawn.env), or the target is on the live URL's port (its cluster) or
#   has its database name: nothing about the live URL is printed;
# - --min-idle-hours is below 24;
# - the list file is missing, holds no section for this database, or more
#   than one;
# - any would-drop name in the section is not spawn_test_<pid>_<...>
#   (lower-case letters, digits and `_`).
# For each name, in the list's order: its dry-run line (as the print-only
# run says it now), then `dropped <name>` or `skipped <name> <why>`. Each
# drop is one `DROP SCHEMA "<name>" CASCADE` in a transaction of its own,
# with a lock timeout, so one that fails is said and the others go on. Last,
# the spawn_test_* schemas before and after: count and MB.
#
# The URL goes to psql as PG* variables (scripts/pg-url.sh), never an
# argument. Exit 0; 1 when a drop failed; 2 when refused or the database
# cannot be read.
set -euo pipefail

prog=sweep-test-schemas
min_idle_hours=24
apply_list=""
apply=0
while [ "$#" -gt 0 ]; do
    case $1 in
    --min-idle-hours)
        min_idle_hours=${2-}
        shift 2 || { echo "$prog: --min-idle-hours needs a value" >&2; exit 2; }
        ;;
    --dry-run) shift ;; # the default
    --apply)
        apply=1
        apply_list=${2-}
        [ -n "$apply_list" ] || { echo "$prog: refused: --apply needs the reviewed list file" >&2; exit 2; }
        shift 2
        ;;
    *)
        echo "usage: sweep-test-schemas.sh [--min-idle-hours N] | --apply <list-file> [--min-idle-hours N]" >&2
        exit 2
        ;;
    esac
done
case $min_idle_hours in '' | *[!0-9]*)
    echo "$prog: --min-idle-hours is a whole number of hours" >&2
    exit 2
    ;;
esac
apply_floor_hours=24
if [ "$apply" = 1 ] && [ "$min_idle_hours" -lt "$apply_floor_hours" ]; then
    echo "$prog: refused: --apply needs --min-idle-hours of at least $apply_floor_hours (given $min_idle_hours)" >&2
    exit 2
fi
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || { echo "$prog: SPAWN_TEST_DATABASE_URL is required" >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=scripts/pg-url.sh
. "$here/pg-url.sh"
pg_url_parse "$SPAWN_TEST_DATABASE_URL" || { echo "$prog: SPAWN_TEST_DATABASE_URL is not one postgres URL" >&2; exit 2; }
target_database=$pg_url_database
target_port=${pg_url_port:-5432}
[ -n "$target_database" ] || { echo "$prog: the URL names no database" >&2; exit 2; }

# The live database, for --apply: from the environment or spawn.env (parsed,
# never sourced), compared by port and name, never printed.
if [ "$apply" = 1 ]; then
    env_file=${SPAWN_ENV_FILE:-${HOME:-}/.local/share/spawn-rsi/spawn.env}
    live_from_file=""
    if [ -f "$env_file" ]; then
        live_from_file=$(sed -n 's/^SPAWN_DATABASE_URL=//p' "$env_file" | tail -n 1)
        case $live_from_file in
        \"*\") live_from_file=${live_from_file#\"} live_from_file=${live_from_file%\"} ;;
        \'*\') live_from_file=${live_from_file#\'} live_from_file=${live_from_file%\'} ;;
        esac
    fi
    live_from_env=${SPAWN_DATABASE_URL-}
    [ -n "$live_from_env$live_from_file" ] ||
        { echo "$prog: refused: no live database URL is known (SPAWN_DATABASE_URL, in the environment or spawn.env): nothing proves the target is not the live database" >&2; exit 2; }
    for live_url in "$live_from_env" "$live_from_file"; do
        [ -n "$live_url" ] || continue
        pg_url_parse "$live_url" ||
            { echo "$prog: refused: a live SPAWN_DATABASE_URL cannot be read, so it cannot be told apart" >&2; exit 2; }
        [ "${pg_url_port:-5432}" != "$target_port" ] ||
            { echo "$prog: refused: the target is on the live database's port (the live server's cluster)" >&2; exit 2; }
        [ "$pg_url_database" != "$target_database" ] ||
            { echo "$prog: refused: the target has the live database's name" >&2; exit 2; }
    done

    # The list: exactly one section for this database, every name a test
    # schema's. Read whole before anything is dropped.
    [ -f "$apply_list" ] && [ -r "$apply_list" ] ||
        { echo "$prog: refused: no readable list file $apply_list" >&2; exit 2; }
    sections=$(grep -cFx "database $target_database port $target_port" "$apply_list" || true)
    [ "$sections" = 1 ] ||
        { echo "$prog: refused: the list holds $sections sections for database $target_database port $target_port (one is needed)" >&2; exit 2; }
    reviewed=$(awk -v want="database $target_database port $target_port" '
        /^database / { inside = ($0 == want); next }
        inside && /^would-drop / { print $2 }' "$apply_list")
    bad_names=$(printf '%s\n' "$reviewed" | grep . | grep -vE '^spawn_test_[0-9]+_[a-z0-9_]+$' || true)
    [ -z "$bad_names" ] ||
        { echo "$prog: refused: names in the list that are not spawn_test_<pid>_...: $(printf '%s\n' "$bad_names" | head -n 5 | tr '\n' ' ')" >&2; exit 2; }
    dup_names=$(printf '%s\n' "$reviewed" | grep . | sort | uniq -d)
    [ -z "$dup_names" ] ||
        { echo "$prog: refused: names listed twice: $(printf '%s\n' "$dup_names" | head -n 5 | tr '\n' ' ')" >&2; exit 2; }
fi

pg_url_export "$SPAWN_TEST_DATABASE_URL" || { echo "$prog: SPAWN_TEST_DATABASE_URL is not one postgres URL" >&2; exit 2; }
# Every read is in a read-only transaction; only the drops (--apply) are not.
pgoptions_drop=${PGOPTIONS-}
PGOPTIONS="${PGOPTIONS:+$PGOPTIONS }-c default_transaction_read_only=on"
export PGOPTIONS

# schema_rows <idle expression> [<one schema>]: name|bytes|idle seconds
# (empty when the files cannot be read), of every spawn_test_* schema or of
# the one named (none when it is gone).
schema_rows() {
    psql -X -qtA -F '|' -v ON_ERROR_STOP=1 -v sr_one="${2-}" <<SQL
select n.nspname,
       coalesce(sum(pg_total_relation_size(c.oid)) filter (where c.relkind in ('r', 'm')), 0),
       $1
  from pg_namespace n
  left join pg_class c on c.relnamespace = n.oid and c.relkind in ('r', 'i', 't', 'm', 'S')
 where n.nspname like 'spawn\_test\_%' and (:'sr_one' = '' or n.nspname = :'sr_one')
 group by n.nspname
 order by n.nspname
SQL
}
idle_sql="floor(extract(epoch from now() - max((pg_stat_file(pg_relation_filepath(c.oid), true)).modification)))::bigint"
ages_readable=1
if ! rows=$(schema_rows "$idle_sql" 2>/dev/null); then
    # No right to read the files: ages unknown, so every schema stays.
    ages_readable=0
    rows=$(schema_rows "null") || { echo "$prog: cannot list the schemas" >&2; exit 2; }
    echo "$prog: cannot read the relation files' times here; every schema stays" >&2
fi

min_idle=$((min_idle_hours * 3600))
# judge <name> <bytes> <idle>: sets jd_line (the dry-run line) and jd_why
# (empty when the schema qualifies).
judge() {
    jd_mb=$((($2 + 524287) / 1048576))
    jd_pid=$(printf '%s\n' "$1" | sed -nE 's/^spawn_test_([a-z]+_)?([0-9]+)_.*/\2/p')
    jd_hours=${3:+$(($3 / 3600))}
    jd_why=""
    if [ -z "$jd_pid" ]; then
        jd_why="no pid in the name"
    elif [ -d "/proc/$jd_pid" ]; then
        jd_why="pid $jd_pid runs here"
    elif [ -z "$3" ]; then
        jd_why="its age is unknown"
    elif [ "$3" -lt "$min_idle" ]; then
        jd_why="written ${jd_hours}h ago (< ${min_idle_hours}h)"
    fi
    if [ -z "$jd_why" ]; then
        jd_line=$(printf 'would-drop %s pid=%s idle=%sh size=%sMB' "$1" "$jd_pid" "$jd_hours" "$jd_mb")
    else
        jd_line=$(printf 'keep       %s pid=%s idle=%sh size=%sMB %s' "$1" "${jd_pid:--}" "${jd_hours:-?}" "$jd_mb" "$jd_why")
    fi
}
# totals <rows>: "<count> schema(s), <MB> MB"
totals() {
    printf '%s\n' "$1" | awk -F'|' 'NF >= 2 && $1 != "" { n++; b += $2 } END { printf "%d schema(s), %d MB", n, int((b + 524287) / 1048576) }'
}

if [ "$apply" = 0 ]; then
    echo "database $target_database port $target_port"
    drop_count=0 drop_bytes=0 keep_count=0 keep_bytes=0
    while IFS='|' read -r name bytes idle; do
        [ -n "$name" ] || continue
        judge "$name" "$bytes" "$idle"
        printf '%s\n' "$jd_line"
        if [ -z "$jd_why" ]; then
            drop_count=$((drop_count + 1)) drop_bytes=$((drop_bytes + bytes))
        else
            keep_count=$((keep_count + 1)) keep_bytes=$((keep_bytes + bytes))
        fi
    done <<<"$rows"
    printf '%s (print-only, nothing dropped): would drop %s schema(s), %s MB; keeps %s, %s MB\n' \
        "$prog" "$drop_count" "$(((drop_bytes + 524287) / 1048576))" "$keep_count" "$(((keep_bytes + 524287) / 1048576))"
    exit 0
fi

echo "database $target_database port $target_port"
echo "$prog --apply: $(printf '%s\n' "$reviewed" | grep -c . || true) reviewed name(s) from $apply_list"
before=$(totals "$rows")
dropped=0 skipped=0 failed=0
for name in $reviewed; do
    if [ "$ages_readable" = 1 ]; then
        now_row=$(schema_rows "$idle_sql" "$name") || now_row="?"
    else
        now_row=$(schema_rows "null" "$name") || now_row="?"
    fi
    if [ "$now_row" = "?" ]; then
        echo "skipped $name cannot read it now"
        skipped=$((skipped + 1))
        continue
    fi
    if [ -z "$now_row" ]; then
        echo "skipped $name gone"
        skipped=$((skipped + 1))
        continue
    fi
    IFS='|' read -r _ now_bytes now_idle <<<"$now_row"
    judge "$name" "$now_bytes" "$now_idle"
    printf '%s\n' "$jd_line"
    if [ -n "$jd_why" ]; then
        echo "skipped $name $jd_why"
        skipped=$((skipped + 1))
        continue
    fi
    # One statement, one transaction; the name quoted by psql (:"..."), and
    # checked against spawn_test_<pid>_[a-z0-9_]+ above besides.
    if drop_err=$(printf 'set lock_timeout = %s;\ndrop schema :"dn_name" cascade;\n' "'10s'" |
        PGOPTIONS=$pgoptions_drop psql -X -q -v ON_ERROR_STOP=1 -v dn_name="$name" 2>&1 >/dev/null); then
        echo "dropped $name"
        dropped=$((dropped + 1))
    else
        echo "skipped $name the drop failed: $(printf '%s\n' "$drop_err" | head -n 1)"
        failed=$((failed + 1))
    fi
done
after_rows=$(schema_rows "null") || after_rows=""
echo "$prog --apply: dropped $dropped, skipped $skipped, failed $failed"
echo "$prog --apply: spawn_test_* before: $before; after: $(totals "$after_rows")"
[ "$failed" = 0 ]
