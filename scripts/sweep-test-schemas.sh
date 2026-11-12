#!/bin/bash
# sweep-test-schemas.sh [--min-idle-hours N]: which spawn_test_* schemas of
# the test database (SPAWN_TEST_DATABASE_URL) a sweep would drop, and why
# each other one stays (backlog P21). PRINT-ONLY: it drops nothing.
#
# sweep-test-schemas.sh --apply <list-file> [--preview] [--min-idle-hours N]:
# drops exactly the would-drop names of <list-file>'s section for this
# database, a list a print-only run wrote and a reviewer passed (owner's
# decision of 2026-11-12 15:45: list first, a reviewer checks it, then only
# the reviewed names go). --preview walks the same path, locks and checks
# and DROP included, and rolls each transaction back: the rehearsal of the
# real mode, which drops nothing (`would drop <name> (preview: rolled
# back)`). --dry-run with --apply is refused: it would read as a rehearsal
# and was none (cc-astra A2).
#
# A crate test names its schema spawn_test_<pid>_<...>_<case> (P19's
# guard, spawn_postgres::testing::TestSchema) and drops it when it passes.
# What is left is a failed test's (kept on purpose, and named), or a killed
# run's (no Drop ran). The print-only run lists a schema as would-drop only
# when both hold:
# - its pid runs no process on this host (/proc/<pid>), and
# - its newest relation file is --min-idle-hours old (default 24;
#   pg_stat_file, a superuser's or a grantee's). A schema whose files cannot
#   be read stays: its age is unknown.
# A file time is no proof of idleness (a new function writes no file of the
# schema, and a committed write may wait in shared buffers), and a gate runs
# its tests in a PID namespace, so a live test's pid there may run nothing
# here. So --apply proves it instead (cc-astra A3; the lead's decision):
# - the list must be at least --min-idle-hours old (its `listed` line), and
# - nothing may have changed in the schema since the list, checked under the
#   schema's locks: no catalog row of any object in it, no column,
#   dependency or comment, and no data row newer than the list's transaction
#   id mark (xmin, or a set xmax; age() comparisons, safe across wraparound,
#   a frozen row old). The mark is the oldest transaction still running when
#   the list was made (pg_snapshot_xmin), so a writer already open then is
#   newer than it (rev-e r2 F1). And as the list has them: its tables'
#   modified-row counts (the statistics collector: deletes leave no row to
#   see), and the objects a DROP ... CASCADE of it removes, their count and
#   a hash of their identities (a DROP of one of them since leaves no row to
#   see; r2 F2).
# So "nothing changed since a list that old" is "nothing changed for that
# long". The pid is checked again too, and an unknown age still keeps it.
# Other names (no pid in them) always stay. A schema with a sequence whose
# use cannot be seen always stays (cc-astra A14; the lead's decision 20:2x
# on rev-e r4 N1): one with CACHE above 1, where a backend hands out values
# from a cached block without moving anything stored, or one this login
# may not read (its last_value reads null). A CACHE 1 sequence writes its
# last_value at every nextval, so its use is seen: the list carries the
# schema's CACHE 1 sequences' values (seqv=, a hash of each name and
# last_value), and --apply keeps the schema when they differ under the
# locks.
# And the drop runs only on a quiet database (cc-astra A15; the lead's
# decision): inside the drop's transaction, after the schema's locks, no
# other client session may be connected to the target database (a new
# outside dependent, an operator over the schema's function, needs no lock
# the sweep holds, only a session), and that, and the outside dependents,
# are checked again immediately before the DROP. A session the login may
# not inspect (another role's, to a login without pg_read_all_stats)
# counts as a client session too (rev-e r4 F1).
#
# Output of the print-only run: the section line, the time and transaction
# id it looked at, one line per schema, then a summary. The section line
# names the database the list is for (its name and port, never the URL), and
# --apply takes only that section:
#   database <name> port <port>
#   listed <epoch> xid <oldest running transaction id>
#   would-drop <schema> pid=<pid> idle=<h>h size=<MB>MB mods=<n> seq=0 seqv=<md5> objs=<n> ids=<md5>
# (seq: the schema's sequences whose use cannot be seen, 0 on every
# would-drop line; seqv: its CACHE 1 sequences' values)
#   keep       <schema> pid=<pid> idle=<h>h size=<MB>MB <why>
# A list file may hold several runs' output, one section each, in any text
# around them (a markdown file with code blocks): only lines that start with
# `database `, `listed ` or `would-drop ` are read.
#
# --apply refuses (exit 2, nothing dropped), never skips silently, when:
# - no live database URL is known (SPAWN_DATABASE_URL, in the environment or
#   spawn.env), or the target is on the live URL's port (its cluster) or
#   has its database name: nothing about the live URL is printed;
# - --min-idle-hours is below 24, or the list is younger than it;
# - the list file is missing, holds no section for this database, or more
#   than one, or the section has no single `listed` line, or a would-drop
#   line without mods=, seq=, seqv=, objs= and ids=;
# - any would-drop name in the section is not spawn_test_<pid>_<...>
#   (lower-case letters, digits and `_`), or is listed twice.
# First, each schema that qualifies now but is not on the list:
# `not-on-list <name> ...`, kept, and their count. Then for each name, in
# the list's order: its dry-run line (as the print-only run says it now),
# then `dropped <name>` or `skipped <name> <why>`, and below it, each line
# indented `  | `, all psql said (the NOTICE `drop cascades to ...` and its
# DETAIL, or the error), never discarded. Each drop is one
# `DROP SCHEMA "<name>" CASCADE` in a transaction of its own, under its
# locks, with a lock timeout, so one that fails is said and the others go
# on. One whose CASCADE would also drop something of another schema (any
# object class: a view, a foreign key, a column of its type, an operator
# over its function...) is kept: `skipped <name> dependents-outside (<n>:
# <first>)`; one changed since the list: `skipped <name> changed since the
# list (<what>)`. Last, the spawn_test_* schemas before and after: count
# and MB.
#
# The URL goes to psql as PG* variables (scripts/pg-url.sh), never an
# argument. Exit 0; 1 when a drop failed; 2 when refused or the database
# cannot be read.
set -euo pipefail

prog=sweep-test-schemas
min_idle_hours=24
apply_list=""
apply=0
preview=0
dry_run=0
while [ "$#" -gt 0 ]; do
    case $1 in
    --min-idle-hours)
        min_idle_hours=${2-}
        shift 2 || { echo "$prog: --min-idle-hours needs a value" >&2; exit 2; }
        ;;
    --dry-run)
        dry_run=1
        shift
        ;;
    --preview)
        preview=1
        shift
        ;;
    --apply)
        apply=1
        apply_list=${2-}
        [ -n "$apply_list" ] || { echo "$prog: refused: --apply needs the reviewed list file" >&2; exit 2; }
        shift 2
        ;;
    *)
        echo "usage: sweep-test-schemas.sh [--min-idle-hours N] | --apply <list-file> [--preview] [--min-idle-hours N]" >&2
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
# --dry-run is the print-only listing; with --apply it would read as a
# rehearsal that is not one, so the two together are refused (cc-astra A2).
# The rehearsal of the real mode is --apply <list> --preview.
if [ "$apply" = 1 ] && [ "$dry_run" = 1 ]; then
    echo "$prog: refused: --dry-run with --apply: for a rehearsal of the apply, use --apply <list> --preview (it drops nothing)" >&2
    exit 2
fi
if [ "$preview" = 1 ] && [ "$apply" = 0 ]; then
    echo "$prog: refused: --preview goes with --apply <list>" >&2
    exit 2
fi
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
    # When the list was made, and the oldest transaction running then (the
    # print-only run's `listed <epoch> xid <xid>` line): everything written
    # by a transaction not yet committed then has an xmin or xmax no older
    # than it, so a change since the list is seen
    # exactly (below); and the list must be at least the idle floor old, so
    # "nothing changed since the list" is "nothing changed for that long",
    # whatever the file times say (cc-astra A3; the lead's decision 17:2x).
    listed_lines=$(awk -v want="database $target_database port $target_port" '
        /^database / { inside = ($0 == want); next }
        inside && /^listed / { print }' "$apply_list")
    [ "$(printf '%s\n' "$listed_lines" | grep -c . || true)" = 1 ] ||
        { echo "$prog: refused: the list's section has no single \`listed <epoch> xid <xid>\` line (make the list again with this version)" >&2; exit 2; }
    listed_at=$(printf '%s\n' "$listed_lines" | sed -nE 's/^listed ([0-9]+) xid ([0-9]+)$/\1/p')
    listed_xid=$(printf '%s\n' "$listed_lines" | sed -nE 's/^listed ([0-9]+) xid ([0-9]+)$/\2/p')
    [ -n "$listed_at" ] && [ -n "$listed_xid" ] ||
        { echo "$prog: refused: the list's \`listed\` line does not read as \`listed <epoch> xid <xid>\`" >&2; exit 2; }
    list_age=$(($(date +%s) - listed_at))
    [ "$list_age" -ge $((min_idle_hours * 3600)) ] ||
        { echo "$prog: refused: the list is $((list_age / 3600))h old; --apply needs it at least ${min_idle_hours}h old, so that no change since it means none for that long" >&2; exit 2; }
    # Each name's mods, seq, objs, ids and seqv when listed (see schema_rows).
    listed_counts=$(awk -v want="database $target_database port $target_port" '
        /^database / { inside = ($0 == want); next }
        inside && /^would-drop / {
            m = ""; q = ""; o = ""; h = ""; v = ""
            for (i = 3; i <= NF; i++) {
                if ($i ~ /^mods=[0-9]+$/) m = substr($i, 6)
                if ($i ~ /^seq=[0-9]+$/) q = substr($i, 5)
                if ($i ~ /^objs=[0-9]+$/) o = substr($i, 6)
                if ($i ~ /^ids=[0-9a-f]{32}$/) h = substr($i, 5)
                if ($i ~ /^seqv=[0-9a-f]{32}$/) v = substr($i, 6)
            }
            print $2, m, q, o, h, v
        }' "$apply_list")
    no_counts=$(printf '%s\n' "$listed_counts" | awk 'NF > 0 && NF < 6 { print $1 }')
    [ -z "$no_counts" ] ||
        { echo "$prog: refused: would-drop lines without mods=, seq=, seqv=, objs= and ids= (make the list again with this version): $(printf '%s\n' "$no_counts" | head -n 3 | tr '\n' ' ')" >&2; exit 2; }
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
# (empty when the files cannot be read)|mods|seq|objs|ids|seqv, of every
# spawn_test_* schema or of the one named (none when it is gone). mods: the
# rows its tables have had inserted, updated and deleted, by the statistics
# collector; seq: how many sequences it has whose use cannot be seen (CACHE
# above 1, or not readable here; one keeps it, see judge); seqv: the md5 of
# its CACHE 1 sequences' sorted `<name>=<last_value>`; objs and ids: the
# objects DROP ... CASCADE removes with it (the pg_depend closure from the
# namespace, the one --apply walks below), their count and the md5 of their
# sorted `<catalog> <identity>` lines. A list carries all four for each
# would-drop name, and --apply finds them unchanged or keeps the schema: a
# DELETE leaves no row a scan sees, and a DROP of one
# of its objects leaves no row at all.
schema_rows() {
    psql -X -qtA -F '|' -v ON_ERROR_STOP=1 -v sr_one="${2-}" <<SQL
select g.nspname, g.bytes, g.idle, g.mods, g.seq, f.objs, f.ids, g.seqv
  from (select n.oid, n.nspname,
               coalesce(sum(pg_total_relation_size(c.oid)) filter (where c.relkind in ('r', 'm')), 0) as bytes,
               $1 as idle,
               coalesce(sum(pg_stat_get_tuples_inserted(c.oid) + pg_stat_get_tuples_updated(c.oid)
                            + pg_stat_get_tuples_deleted(c.oid)) filter (where c.relkind in ('r', 'm')), 0) as mods,
               count(c.oid) filter (where c.relkind = 'S'
                                    and ((select q.seqcache from pg_sequence q where q.seqrelid = c.oid) > 1
                                         or not has_sequence_privilege(c.oid, 'SELECT,USAGE'))) as seq,
               (select md5(coalesce(string_agg(s.sequencename || '=' || coalesce(s.last_value::text, 'null'), ','
                                               order by s.sequencename collate "C"), ''))
                  from pg_sequences s where s.schemaname = n.nspname and s.cache_size = 1) as seqv
          from pg_namespace n
          left join pg_class c on c.relnamespace = n.oid and c.relkind in ('r', 'i', 't', 'm', 'S')
         where n.nspname like 'spawn\_test\_%' and (:'sr_one' = '' or n.nspname = :'sr_one')
         group by n.oid, n.nspname) g
 cross join lateral (
    with recursive k(classid, objid, objsubid) as (
        select 'pg_namespace'::regclass::oid, g.oid, 0
        union
        select d.classid, d.objid, d.objsubid
          from pg_depend d join k on d.refclassid = k.classid and d.refobjid = k.objid
         where d.deptype <> 'p'
    )
    select count(*) as objs,
           md5(coalesce(string_agg(k.classid::regclass::text || ' ' || i.identity, E'\n'
                                   order by k.classid::regclass::text || ' ' || i.identity collate "C"), '')) as ids
      from k, pg_identify_object(k.classid, k.objid, k.objsubid) i) f
 order by g.nspname
SQL
}
# When this run looks, and the oldest transaction still running (or, none
# running, the next): a print-only run's list carries them (`listed <epoch>
# xid <xid>`), taken before anything is read. Not the next transaction id: a
# writer open now has an older one, and its change, committed after the
# list, would read as old (rev-e r2 F1).
listed_mark=$(psql -X -qtA -F ' ' -v ON_ERROR_STOP=1 \
    -c "select extract(epoch from now())::bigint, 'xid', pg_snapshot_xmin(pg_current_snapshot())") ||
    { echo "$prog: cannot read the database" >&2; exit 2; }
idle_sql="floor(extract(epoch from now() - max((pg_stat_file(pg_relation_filepath(c.oid), true)).modification)))::bigint"
ages_readable=1
if ! rows=$(schema_rows "$idle_sql" 2>/dev/null); then
    # No right to read the files: ages unknown, so every schema stays.
    ages_readable=0
    rows=$(schema_rows "null") || { echo "$prog: cannot list the schemas" >&2; exit 2; }
    echo "$prog: cannot read the relation files' times here; every schema stays" >&2
fi

min_idle=$((min_idle_hours * 3600))
# judge <name> <bytes> <idle> [<mods> <seq> <objs> <ids> <seqv> [listed]]:
# sets jd_line (the dry-run line, with mods, seq, seqv, objs and ids when
# given) and jd_why (empty when the
# schema qualifies). `listed`: the age floor was the list's (its files were
# that old when it was made, and the list is that old now), so only an
# unknown age keeps it here.
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
    elif [ "${5:-0}" != 0 ]; then
        jd_why="has $5 sequence(s) with a cache or not readable: their use leaves no trace to check"
    elif [ "${9-}" != listed ] && [ "$3" -lt "$min_idle" ]; then
        jd_why="written ${jd_hours}h ago (< ${min_idle_hours}h)"
    fi
    if [ -z "$jd_why" ]; then
        jd_line=$(printf 'would-drop %s pid=%s idle=%sh size=%sMB%s' "$1" "$jd_pid" "$jd_hours" "$jd_mb" "${4:+ mods=$4 seq=${5-0} objs=${6-0} ids=${7-} seqv=${8-}}")
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
    printf 'listed %s\n' "$listed_mark"
    drop_count=0 drop_bytes=0 keep_count=0 keep_bytes=0
    while IFS='|' read -r name bytes idle mods seqs objs ids seqv; do
        [ -n "$name" ] || continue
        judge "$name" "$bytes" "$idle" "$mods" "$seqs" "$objs" "$ids" "$seqv"
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
# The difference the other way (owner's step 4): what qualifies now and is
# not on the reviewed list is said, and kept.
not_listed=0
while IFS='|' read -r name bytes idle mods seqs objs ids seqv; do
    [ -n "$name" ] || continue
    judge "$name" "$bytes" "$idle" "$mods" "$seqs" "$objs" "$ids" "$seqv"
    [ -z "$jd_why" ] || continue
    # A here-string, not a pipe: under pipefail, grep -q stopping at its
    # match killed printf with SIGPIPE on a list longer than a pipe's
    # buffer, and a listed name read as not on the list (ccd, 2026-11-12:
    # 77 of 1230 said not-on-list, then dropped as listed).
    grep -qxF -- "$name" <<<"$reviewed" && continue
    echo "not-on-list ${jd_line#would-drop } (qualifies now, not reviewed: kept)"
    not_listed=$((not_listed + 1))
done <<<"$rows"
echo "$prog --apply: $not_listed qualifying schema(s) not on the list, kept"
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
    IFS='|' read -r _ now_bytes now_idle now_mods now_seqs now_objs now_ids now_seqv <<<"$now_row"
    judge "$name" "$now_bytes" "$now_idle" "$now_mods" "$now_seqs" "$now_objs" "$now_ids" "$now_seqv" listed
    printf '%s\n' "$jd_line"
    if [ -n "$jd_why" ]; then
        echo "skipped $name $jd_why"
        skipped=$((skipped + 1))
        continue
    fi
    # One transaction per name, all of it under the schema's own locks:
    # - every table, view and materialized view of the schema is locked
    #   ACCESS EXCLUSIVE first (lock_timeout 10s), so nothing writes to it or
    #   adds a view or key over it from here to the drop;
    # - a quiet database (cc-astra A15): no other client session connected
    #   to it, `skipped <name> not quiescent (<n> other session(s)...)`. An
    #   outside dependent over a non-relation (an operator over its
    #   function) takes no lock the sweep holds, only a session;
    # - a sequence in it keeps it (cc-astra A14), `skipped <name> has
    #   sequences (<n>)`;
    # - dependents-outside (cc-astra A9, rev-e F2): what DROP ... CASCADE
    #   removes is the pg_depend closure from the namespace; every object in
    #   it must be the schema's own, by pg_identify_object's schema (a
    #   default, rule, trigger or policy by its table's; the schema's own
    #   tables' TOAST in pg_toast). One that is not (a view, a foreign key, a
    #   column of its type, an operator or cast over its function, a
    #   collation...) keeps the whole schema: `skipped <name>
    #   dependents-outside (<n>: <first>)`;
    # - changed since the list (cc-astra A3): a catalog row of any object in
    #   the closure, a column, a dependency or comment on one, or a data row
    #   of its tables (xmin, or a set xmax) newer than the list's transaction
    #   id. The comparison is age(x) <= age(list), safe across wraparound,
    #   and a frozen row reads as old. A DELETE leaves no row a scan sees,
    #   so the rows its tables have had modified (the statistics collector's
    #   counts) must be the list's. No file time is read here: the list is at least the floor old
    #   and nothing changed since it, which is the proof (a scan here sets
    #   hint bits, so file times after a --preview say nothing);
    # - immediately before the DROP, again: no other session connected, and
    #   no dependent outside the schema in its closure read afresh;
    # - then the DROP, and COMMIT (ROLLBACK with --preview).
    # The name, the list's xid and time go in as settings (psql quotes them),
    # and to SQL through format('%I') and casts.
    if [ "$preview" = 1 ]; then tx_end=rollback; else tx_end=commit; fi
    read -r _ name_mods _ name_objs name_ids name_seqv < <(awk -v n="$name" '$1 == n' <<<"$listed_counts")
    if drop_err=$(PGOPTIONS=$pgoptions_drop psql -X -q -v ON_ERROR_STOP=1 -v dn_name="$name" \
        -v dn_xid="$listed_xid" -v dn_end="$tx_end" \
        -v dn_mods="$name_mods" -v dn_objs="$name_objs" -v dn_ids="$name_ids" -v dn_seqv="$name_seqv" 2>&1 >/dev/null <<'SQL'
begin;
set local lock_timeout = '10s';
select set_config('sweep.name', :'dn_name', true), set_config('sweep.xid', :'dn_xid', true),
       set_config('sweep.mods', :'dn_mods', true),
       set_config('sweep.objs', :'dn_objs', true), set_config('sweep.ids', :'dn_ids', true),
       set_config('sweep.seqv', :'dn_seqv', true);
do $sweep$
declare
    nsp oid := (select oid from pg_namespace where nspname = current_setting('sweep.name'));
    list_xid xid := ((current_setting('sweep.xid')::numeric % 4294967296)::bigint)::text::xid;
    rel regclass;
    cls regclass;
    oids oid[];
    found bigint;
    first text;
    ids text;
begin
    if nsp is null then
        raise exception 'sweep: gone';
    end if;
    for rel in select c.oid::regclass from pg_class c
                where c.relnamespace = nsp and c.relkind in ('r', 'p', 'v', 'm', 'f') order by c.oid loop
        execute format('lock table %s in access exclusive mode', rel);
    end loop;
    -- A quiet database (cc-astra A15): no other client session. A session
    -- of another role reads with a null backend_type to a login without
    -- pg_read_all_stats (rev-e r4 F1): it counts, so a login that cannot
    -- tell is never told the database is quiet.
    select count(*), string_agg(distinct coalesce(a.application_name, ''), ', ') into found, first
      from pg_stat_activity a
     where a.datname = current_database() and a.pid <> pg_backend_pid()
       and coalesce(a.backend_type, 'client backend') = 'client backend';
    if found > 0 then
        raise exception 'sweep: not quiescent (% other session(s) connected to %: %)', found, current_database(), first;
    end if;
    -- The closure as it is now, read again at each use (a view), and as it
    -- was here (a table), for the checks below.
    create temporary view sweep_closure_now as
    with recursive c(classid, objid, objsubid) as (
        select 'pg_namespace'::regclass::oid,
               (select oid from pg_namespace where nspname = current_setting('sweep.name')), 0
        union
        select d.classid, d.objid, d.objsubid
          from pg_depend d join c on d.refclassid = c.classid and d.refobjid = c.objid
         where d.deptype <> 'p'
    )
    select c.classid, c.objid, c.objsubid,
           coalesce(i.schema,
                    case c.classid
                    when 'pg_attrdef'::regclass then (select n.nspname from pg_attrdef a join pg_class r on r.oid = a.adrelid join pg_namespace n on n.oid = r.relnamespace where a.oid = c.objid)
                    when 'pg_rewrite'::regclass then (select n.nspname from pg_rewrite w join pg_class r on r.oid = w.ev_class join pg_namespace n on n.oid = r.relnamespace where w.oid = c.objid)
                    when 'pg_trigger'::regclass then (select n.nspname from pg_trigger t join pg_class r on r.oid = t.tgrelid join pg_namespace n on n.oid = r.relnamespace where t.oid = c.objid)
                    when 'pg_policy'::regclass then (select n.nspname from pg_policy y join pg_class r on r.oid = y.polrelid join pg_namespace n on n.oid = r.relnamespace where y.oid = c.objid)
                    end) as schema,
           i.identity
      from c, pg_identify_object(c.classid, c.objid, c.objsubid) i;
    create temporary table sweep_closure on commit drop as select * from sweep_closure_now;

    select count(*), min(identity) into found, first from sweep_closure
     where not (classid = 'pg_namespace'::regclass and objid = nsp)
       and schema is distinct from current_setting('sweep.name') and schema is distinct from 'pg_toast';
    if found > 0 then
        raise exception 'sweep: dependents-outside (%: %)', found, first;
    end if;
    -- A sequence, here or owned from here, whose use cannot be seen (cc-astra
    -- A14): CACHE above 1 (a cached nextval moves nothing stored) or not
    -- readable here (its last_value reads null).
    select count(*) into found from sweep_closure s join pg_class c on c.oid = s.objid
     where s.classid = 'pg_class'::regclass and c.relkind = 'S'
       and ((select q.seqcache from pg_sequence q where q.seqrelid = c.oid) > 1
            or not has_sequence_privilege(c.oid, 'SELECT,USAGE'));
    if found > 0 then
        raise exception 'sweep: has sequences (%)', found;
    end if;

    -- Changed since the list: the objects themselves, as schema_rows hashes
    -- them (a DROP of one leaves no row to see; rev-e r2 F2).
    select count(*), md5(coalesce(string_agg(classid::regclass::text || ' ' || identity, E'\n'
                                             order by classid::regclass::text || ' ' || identity collate "C"), ''))
      into found, ids from sweep_closure;
    if found <> current_setting('sweep.objs')::bigint then
        raise exception 'sweep: changed since the list (objects: % then, % now)', current_setting('sweep.objs'), found;
    end if;
    if ids <> current_setting('sweep.ids') then
        raise exception 'sweep: changed since the list (objects: % then and now, other identities)', found;
    end if;
    -- Changed since the list: catalog rows of every class in the closure.
    for cls in select distinct classid::regclass from sweep_closure loop
        select array_agg(objid) into oids from sweep_closure where classid = cls;
        execute format('select count(*) from %s where oid = any($1) and age(xmin) <= age($2)', cls)
            into found using oids, list_xid;
        if found > 0 then
            raise exception 'sweep: changed since the list (% in %)', found, cls;
        end if;
    end loop;
    select array_agg(objid) into oids from sweep_closure where classid = 'pg_class'::regclass;
    select count(*) into found from pg_attribute where attrelid = any(oids) and age(xmin) <= age(list_xid);
    if found = 0 then
        select count(*) into found from pg_depend d join sweep_closure s on d.classid = s.classid and d.objid = s.objid
         where age(d.xmin) <= age(list_xid);
    end if;
    if found = 0 then
        select count(*) into found from pg_description d join sweep_closure s on d.classoid = s.classid and d.objoid = s.objid
         where age(d.xmin) <= age(list_xid);
    end if;
    if found > 0 then
        raise exception 'sweep: changed since the list (% column, dependency or comment rows)', found;
    end if;
    -- Its data: a row written (xmin) or deleted, updated or locked (xmax)
    -- since the list.
    for rel in select c.oid::regclass from pg_class c
                where c.oid = any(oids) and c.relkind in ('r', 'm', 't') order by c.oid loop
        execute format('select count(*) from %s where age(xmin) <= age($1) or (xmax <> ''0''::xid and age(xmax) <= age($1))', rel)
            into found using list_xid;
        if found > 0 then
            raise exception 'sweep: changed since the list (% rows of %)', found, rel;
        end if;
    end loop;
    -- A CACHE 1 sequence's nextval since the list: its last_value moved.
    select md5(coalesce(string_agg(s.sequencename || '=' || coalesce(s.last_value::text, 'null'), ','
                                   order by s.sequencename collate "C"), ''))
      into ids from pg_sequences s where s.schemaname = current_setting('sweep.name') and s.cache_size = 1;
    if ids <> current_setting('sweep.seqv') then
        raise exception 'sweep: changed since the list (sequence values)';
    end if;
    -- What a scan cannot see: deletes (and every insert or update), by the
    -- statistics collector's counts.
    select coalesce(sum(pg_stat_get_tuples_inserted(c.oid) + pg_stat_get_tuples_updated(c.oid)
                        + pg_stat_get_tuples_deleted(c.oid)), 0)
      into found from pg_class c where c.relnamespace = nsp and c.relkind in ('r', 'm');
    if found <> current_setting('sweep.mods')::bigint then
        raise exception 'sweep: changed since the list (rows modified: % then, % now)', current_setting('sweep.mods'), found;
    end if;

    -- Immediately before the DROP (cc-astra A15): still no other session,
    -- and no outside dependent in the closure read afresh.
    select count(*), string_agg(distinct coalesce(a.application_name, ''), ', ') into found, first
      from pg_stat_activity a
     where a.datname = current_database() and a.pid <> pg_backend_pid()
       and coalesce(a.backend_type, 'client backend') = 'client backend';
    if found > 0 then
        raise exception 'sweep: not quiescent (% other session(s) connected to %: %)', found, current_database(), first;
    end if;
    select count(*), min(identity) into found, first from sweep_closure_now
     where not (classid = 'pg_namespace'::regclass and objid = nsp)
       and schema is distinct from current_setting('sweep.name') and schema is distinct from 'pg_toast';
    if found > 0 then
        raise exception 'sweep: dependents-outside (%: %)', found, first;
    end if;
    execute format('drop schema %I cascade', current_setting('sweep.name'));
end
$sweep$;
:dn_end;
SQL
    ); then
        if [ "$preview" = 1 ]; then
            echo "would drop $name (preview: rolled back)"
        else
            echo "dropped $name"
        fi
        dropped=$((dropped + 1))
    elif grep -q 'sweep: dependents-outside' <<<"$drop_err"; then
        echo "skipped $name dependents-outside ($(sed -n 's/.*sweep: dependents-outside (\(.*\))$/\1/p' <<<"$drop_err" | sed -n 1p))"
        skipped=$((skipped + 1))
    elif grep -q 'sweep: changed since the list' <<<"$drop_err"; then
        echo "skipped $name changed since the list ($(sed -n 's/.*sweep: changed since the list (\(.*\))$/\1/p' <<<"$drop_err" | sed -n 1p))"
        skipped=$((skipped + 1))
    elif grep -q 'sweep: not quiescent' <<<"$drop_err"; then
        echo "skipped $name not quiescent ($(sed -n 's/.*sweep: not quiescent (\(.*\))$/\1/p' <<<"$drop_err" | sed -n 1p))"
        skipped=$((skipped + 1))
    elif grep -q 'sweep: has sequences' <<<"$drop_err"; then
        echo "skipped $name has sequences ($(sed -n 's/.*sweep: has sequences (\(.*\))$/\1/p' <<<"$drop_err" | sed -n 1p))"
        skipped=$((skipped + 1))
    elif grep -q 'sweep: gone' <<<"$drop_err"; then
        echo "skipped $name gone"
        skipped=$((skipped + 1))
    else
        echo "skipped $name the drop failed: $(grep -m1 ERROR <<<"$drop_err" || sed -n 1p <<<"$drop_err")"
        failed=$((failed + 1))
    fi
    # Everything psql said, kept as evidence (the lead, 2026-11-12): the
    # DROP's `NOTICE:  drop cascades to ...` and its DETAIL, or the error.
    [ -z "$drop_err" ] || sed 's/^/  | /' <<<"$drop_err"
done
after_rows=$(schema_rows "null") || after_rows=""
if [ "$preview" = 1 ]; then
    echo "$prog --apply --preview (nothing dropped): would drop $dropped, skipped $skipped, failed $failed"
else
    echo "$prog --apply: dropped $dropped, skipped $skipped, failed $failed"
fi
echo "$prog --apply: spawn_test_* before: $before; after: $(totals "$after_rows")"
[ "$failed" = 0 ]
