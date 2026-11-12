#!/usr/bin/env bash
# scripts/test-vacuum-catalogs.sh: scripts/vacuum-catalogs.sh (P21's
# catalog VACUUM, maintenance step 5) against a throwaway PostgreSQL cluster
# of its own (initdb in scratch, a socket in scratch, no TCP), under a fake
# HOME: no live, fleet or ccd server is touched, and maintenance.sh's
# deleting steps never run.
#
#   scripts/test-vacuum-catalogs.sh      # needs initdb/pg_ctl (PG_BIN, default /usr/lib/postgresql/16/bin) and psql
#
# Cases:
# 1. no SPAWN_FLEET_TEST_DATABASE_URL: skipped, exit 0;
# 2. no live URL known (no SPAWN_DATABASE_URL, no spawn.env): refused (2),
#    nothing vacuumed;
# 3. spawn.env's live URL on the test URL's port, or the environment's live
#    URL naming its database: refused (2), nothing vacuumed, and neither the
#    port nor the database name in the output;
# 4. a dry run: says what it would do, vacuums nothing;
# 5. the fleet's login (the database's owner, no superuser), after 300
#    schemas made and dropped: the catalogs are vacuumed (pg_class's
#    last_vacuum set), exit 0, and every schema (a left spawn_test_* one
#    too) and every table is still there;
# 6. a login that does not own the database: exit 1, the catalogs it may not
#    vacuum said; a server that does not answer: exit 1; in both the port
#    and the database name are withheld;
# 7. --url-var of any other name, and --apply without --full: refused (2);
# 8. --url-var SPAWN_TEST_DATABASE_URL with that URL only in spawn.env: the
#    gates' test database is vacuumed;
# 9. another session connected to the database: a plain run and a
#    --full --apply both back off (3), nothing vacuumed or rewritten;
# 10. after 300 schemas alive at once and dropped: --full alone is a dry run
#    (each catalog's size said, nothing rewritten); --full --apply as the
#    database's owner rewrites them, says each before and after, the
#    catalogs' size falls by more than half, and every catalog keeps its
#    row count (and the left test schema its table).
set -euo pipefail
umask 077

here=$(cd "$(dirname "$0")" && pwd)
PROG=test-vacuum-catalogs
pg_bin=${PG_BIN:-/usr/lib/postgresql/16/bin}
for tool in "$pg_bin/initdb" "$pg_bin/pg_ctl"; do
  [ -x "$tool" ] || { echo "$PROG: no $tool (PG_BIN)" >&2; exit 2; }
done
command -v psql >/dev/null || { echo "$PROG: no psql" >&2; exit 2; }

real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
real_home=$(cd "$real_home" && pwd -P)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/vacuum-catalogs-test.XXXXXX")
scratch=$(cd "$scratch" && pwd -P)
case "$scratch/" in
"$real_home"/*)
  rmdir "$scratch"
  echo "$PROG: refusing: the scratch is inside the real home (set TMPDIR elsewhere)" >&2
  exit 2
  ;;
esac
fake_home=$scratch/home
mkdir -m 700 "$fake_home"
cleanup() {
  "$pg_bin/pg_ctl" -D "$scratch/data" -m immediate stop >/dev/null 2>&1 || true
  rm -rf "$scratch"
}
trap cleanup EXIT
export HOME=$fake_home XDG_CONFIG_HOME=$fake_home/.config XDG_CACHE_HOME=$fake_home/.cache
guard_home() {
  [ "$(cd "$HOME" && pwd -P)" = "$fake_home" ] && [ "$fake_home" != "$real_home" ] || {
    echo "$PROG: refusing: HOME is not this test's fake home" >&2
    exit 2
  }
}
guard_home
# Nothing from the caller's environment reaches the step.
unset SPAWN_DATABASE_URL SPAWN_FLEET_TEST_DATABASE_URL SPAWN_ENV_FILE SPAWN_MAINTENANCE_DRY_RUN \
  SPAWN_MAINTENANCE_CATALOGS PGHOST PGPORT PGDATABASE PGUSER PGPASSWORD PGOPTIONS

failures=0
pass() { printf 'ok   %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; failures=$((failures + 1)); }
check() { # check <description> <command...>
  local what=$1
  shift
  if "$@"; then pass "$what"; else fail "$what"; fi
}

# The throwaway cluster: a socket in scratch, a port no other server here
# uses (it is only the socket's name), no TCP.
port=$((20000 + RANDOM % 20000))
socket=$scratch/sock
mkdir -m 700 "$socket"
"$pg_bin/initdb" -D "$scratch/data" -A trust -U drill_admin >/dev/null
"$pg_bin/pg_ctl" -D "$scratch/data" -l "$scratch/pg.log" -w \
  -o "-k $socket -p $port -c listen_addresses=''" start >/dev/null
enc_socket=$(printf '%s' "$socket" | sed 's|/|%2F|g')
admin() { psql -X -qtA -v ON_ERROR_STOP=1 -h "$socket" -p "$port" -U drill_admin "$@"; }
database=fleet_catalog_test
admin -d postgres -c "create role fleet_login login" -c "create role other_login login" \
  -c "create database $database owner fleet_login" >/dev/null
admin -d "$database" -c "grant connect on database $database to other_login" >/dev/null
fleet_url="postgres://fleet_login@$enc_socket:$port/$database"
other_url="postgres://other_login@$enc_socket:$port/$database"
live_elsewhere="postgres://live@$enc_socket:$((port + 1))/live_db"

last_vacuum() { admin -d "$database" -c "select coalesce(last_vacuum::text, 'never') from pg_stat_all_tables where relid = 'pg_catalog.pg_class'::regclass"; }
schemas() { admin -d "$database" -c "select count(*) from pg_namespace"; }
# step VAR=VALUE... [-- ARG...]: the step with only what is named added to
# the environment (exported in a subshell: no URL is ever an argument, of
# env(1) or anything else), and the step's own arguments; output in $out.
step() {
  local st_vars=()
  while [ $# -gt 0 ] && [ "$1" != -- ]; do
    st_vars+=("$1")
    shift
  done
  [ $# -eq 0 ] || shift
  guard_home
  out=$scratch/out
  set +e
  (
    for st_var in "${st_vars[@]}"; do export "${st_var?}"; done
    exec sh "$here/vacuum-catalogs.sh" "$@"
  ) >"$out" 2>&1
  status=$?
  set -e
  sed 's/^/  | /' "$out"
}
withheld() { ! grep -qF -e "$port" -e "$database" "$out"; }

echo "== 1. no test URL: skipped"
step HOME="$HOME" PATH="$PATH"
check "exit 0 (status $status)" [ "$status" -eq 0 ]
check "says skipped" grep -q "skipped: SPAWN_FLEET_TEST_DATABASE_URL is not set" "$out"

echo "== 2. no live URL known: refused"
step HOME="$HOME" PATH="$PATH" SPAWN_FLEET_TEST_DATABASE_URL="$fleet_url"
check "refused (status $status)" [ "$status" -eq 2 ]
check "says nothing proves it is not live" grep -q "no live database URL is known" "$out"
check "nothing vacuumed" [ "$(last_vacuum)" = never ]

echo "== 3. the live URL's port or database: refused, neither printed"
mkdir -p "$fake_home/.local/share/spawn-rsi"
printf 'SPAWN_DATABASE_URL="postgres://live@%s:%s/live_db"\n' "$enc_socket" "$port" \
  >"$fake_home/.local/share/spawn-rsi/spawn.env"
step HOME="$HOME" PATH="$PATH" SPAWN_FLEET_TEST_DATABASE_URL="$fleet_url"
check "spawn.env's live port: refused (status $status)" [ "$status" -eq 2 ]
check "says the live port" grep -q "names the live database's port" "$out"
check "neither the port nor the database printed" withheld
printf 'SPAWN_DATABASE_URL=%s\n' "$live_elsewhere" >"$fake_home/.local/share/spawn-rsi/spawn.env"
step HOME="$HOME" PATH="$PATH" SPAWN_FLEET_TEST_DATABASE_URL="$fleet_url" \
  SPAWN_DATABASE_URL="postgres://live@$enc_socket:$((port + 2))/$database"
check "the environment's live database name: refused (status $status)" [ "$status" -eq 2 ]
check "says the live name" grep -q "names the live database's name" "$out"
check "neither the port nor the database printed" withheld
check "nothing vacuumed" [ "$(last_vacuum)" = never ]

echo "== 4. a dry run"
step HOME="$HOME" PATH="$PATH" SPAWN_FLEET_TEST_DATABASE_URL="$fleet_url" SPAWN_MAINTENANCE_DRY_RUN=1
check "exit 0 (status $status)" [ "$status" -eq 0 ]
check "says what it would vacuum" grep -qE "would vacuum [0-9]+ catalogs of the fleet's test database \([0-9]+ MB\) \(dry run\)" "$out"
check "nothing vacuumed" [ "$(last_vacuum)" = never ]

echo "== 5. the fleet's login: vacuumed, nothing dropped"
{
echo "set client_min_messages = warning;"
for n in $(seq 1 300); do
  printf 'create schema churn_%s; create table churn_%s.t (a int, b text); drop schema churn_%s cascade;\n' "$n" "$n" "$n"
done
} | psql -X -q -v ON_ERROR_STOP=1 -h "$socket" -p "$port" -U fleet_login -d "$database" >/dev/null
psql -X -q -v ON_ERROR_STOP=1 -h "$socket" -p "$port" -U fleet_login -d "$database" \
  -c "create schema spawn_test_4242_kept; create table spawn_test_4242_kept.t (a int)" >/dev/null
before=$(schemas)
step HOME="$HOME" PATH="$PATH" SPAWN_FLEET_TEST_DATABASE_URL="$fleet_url"
check "exit 0 (status $status)" [ "$status" -eq 0 ]
check "says it vacuumed, before and after" \
  grep -qE "vacuumed [0-9]+ catalogs of the fleet's test database: [0-9]+ MB before, [0-9]+ MB after \(space made reusable; nothing dropped\)" "$out"
check "pg_class was vacuumed" [ "$(last_vacuum)" != never ]
check "every schema is still there" [ "$(schemas)" = "$before" ]
check "the left test schema and its table are still there" \
  [ "$(admin -d "$database" -c "select count(*) from spawn_test_4242_kept.t")" = 0 ]
check "neither the port nor the database printed" withheld

echo "== 6. failures withhold the server"
step HOME="$HOME" PATH="$PATH" SPAWN_FLEET_TEST_DATABASE_URL="$other_url"
check "a login that does not own the database: exit 1 (status $status)" [ "$status" -eq 1 ]
check "says catalogs were not vacuumed" grep -q "some catalogs were not vacuumed" "$out"
check "neither the port nor the database printed" withheld
step HOME="$HOME" PATH="$PATH" \
  SPAWN_FLEET_TEST_DATABASE_URL="postgres://fleet_login@$enc_socket%2Fnone:$port/$database"
check "a server that does not answer: exit 1 (status $status)" [ "$status" -eq 1 ]
check "says it cannot read the database" grep -q "the test database cannot be read" "$out"
check "neither the port nor the database printed" withheld

echo "== 7. what it refuses to be asked"
step HOME="$HOME" PATH="$PATH" SPAWN_FLEET_TEST_DATABASE_URL="$fleet_url" -- --url-var SPAWN_DATABASE_URL
check "--url-var of any other name: refused (status $status)" [ "$status" -eq 2 ]
check "says which names it takes" grep -q "names SPAWN_FLEET_TEST_DATABASE_URL or SPAWN_TEST_DATABASE_URL" "$out"
step HOME="$HOME" PATH="$PATH" SPAWN_FLEET_TEST_DATABASE_URL="$fleet_url" -- --apply
check "--apply without --full: refused (status $status)" [ "$status" -eq 2 ]

echo "== 8. the gates' test database, its URL only in spawn.env"
printf 'SPAWN_DATABASE_URL=%s\nSPAWN_TEST_DATABASE_URL="%s"\n' "$live_elsewhere" "$fleet_url" \
  >"$fake_home/.local/share/spawn-rsi/spawn.env"
before_vacuum=$(last_vacuum)
sleep 1
step HOME="$HOME" PATH="$PATH" -- --url-var SPAWN_TEST_DATABASE_URL
check "vacuumed (status $status)" [ "$status" -eq 0 ]
check "names the gates' test database" grep -q "vacuumed [0-9]* catalogs of the gates' test database" "$out"
check "pg_class was vacuumed again" [ "$(last_vacuum)" != "$before_vacuum" ]
check "neither the port nor the database printed" withheld
printf 'SPAWN_DATABASE_URL=%s\n' "$live_elsewhere" >"$fake_home/.local/share/spawn-rsi/spawn.env"

# bloat: 1500 schemas (a wide table with an index, a view and a function
# each) alive at once, then all dropped: the catalogs keep the room they
# took (as the gates' spawn_test kept the P19 leak's).
bloat() {
  {
    echo "set client_min_messages = warning;"
    for n in $(seq 1 1500); do
      printf 'create schema b_%s; create table b_%s.t (a int primary key, b text, c jsonb, d timestamptz, e int, f int, g text, h text, i bigint, j bigint, k bool, l bool); create view b_%s.v as select * from b_%s.t; create function b_%s.f(x int) returns int language sql as $$ select x + 1 $$;\n' "$n" "$n" "$n" "$n" "$n"
    done
    for n in $(seq 1 1500); do printf 'drop schema b_%s cascade;\n' "$n"; done
  } | psql -X -q -v ON_ERROR_STOP=1 -h "$socket" -p "$port" -U fleet_login -d "$database" >/dev/null
}
catalog_bytes() { admin -d "$database" -c "select sum(pg_total_relation_size(oid)) from pg_class where relnamespace = 'pg_catalog'::regnamespace and relkind = 'r' and not relisshared"; }
# Every non-shared catalog's row count, one line each; not pg_statistic's
# (and pg_statistic_ext_data's): ANALYZE rewrites the statistics, by design.
catalog_rows() {
  admin -d "$database" -c "select relname from pg_class where relnamespace = 'pg_catalog'::regnamespace and relkind = 'r' and not relisshared and relname not in ('pg_statistic', 'pg_statistic_ext_data') order by relname" |
    while read -r cr_name; do
      printf '%s %s\n' "$cr_name" "$(admin -d "$database" -c "select count(*) from pg_catalog.$cr_name")"
    done
}

echo "== 9. another session on the database: it backs off, in both modes"
bloat
psql -X -q -h "$socket" -p "$port" -U other_login -d "$database" -c "select pg_sleep(60)" >"$scratch/sleeper.out" 2>&1 &
sleeper=$!
for _ in $(seq 1 50); do
  [ "$(admin -d "$database" -c "select count(*) from pg_stat_activity where usename = 'other_login'")" = 1 ] && break
  sleep 0.1
done
check "the other session is connected" \
  [ "$(admin -d "$database" -c "select count(*) from pg_stat_activity where usename = 'other_login'")" = 1 ]
sed 's/^/  sleeper | /' "$scratch/sleeper.out"
before_vacuum=$(last_vacuum)
bytes_before=$(catalog_bytes)
step HOME="$HOME" PATH="$PATH" SPAWN_FLEET_TEST_DATABASE_URL="$fleet_url"
check "plain: backed off (status $status)" [ "$status" -eq 3 ]
check "says it backed off from one other session" grep -q "backed off: 1 other session(s) on the fleet's test database" "$out"
check "plain: nothing vacuumed" [ "$(last_vacuum)" = "$before_vacuum" ]
step HOME="$HOME" PATH="$PATH" SPAWN_FLEET_TEST_DATABASE_URL="$fleet_url" -- --full --apply
check "--full --apply: backed off (status $status)" [ "$status" -eq 3 ]
check "--full --apply: nothing rewritten (the catalogs' size is as it was)" [ "$(catalog_bytes)" = "$bytes_before" ]
# The other session ends (its backend too: a killed psql leaves pg_sleep
# running on the server).
admin -d "$database" -c "select pg_terminate_backend(pid) from pg_stat_activity where usename = 'other_login'" >/dev/null
wait "$sleeper" 2>/dev/null || true
for _ in $(seq 1 50); do
  [ "$(admin -d "$database" -c "select count(*) from pg_stat_activity where usename = 'other_login'")" = 0 ] && break
  sleep 0.1
done
check "the other session is gone" \
  [ "$(admin -d "$database" -c "select count(*) from pg_stat_activity where usename = 'other_login'")" = 0 ]

echo "== 10. --full: a dry run by default, then --apply gives the room back and keeps every row"
rows_before=$(catalog_rows)
bytes_before=$(catalog_bytes)
step HOME="$HOME" PATH="$PATH" SPAWN_FLEET_TEST_DATABASE_URL="$fleet_url" -- --full
check "dry run: exit 0 (status $status)" [ "$status" -eq 0 ]
check "dry run: says each catalog's size" grep -qE "^maintenance: catalogs:   pg_attribute: [0-9]+ (MB|kB)$" "$out"
check "dry run: says what --apply would do" grep -q "would VACUUM (FULL, ANALYZE) [0-9]* catalogs of the fleet's test database, [0-9]* MB now (dry run" "$out"
check "dry run: nothing rewritten" [ "$(catalog_bytes)" = "$bytes_before" ]
step HOME="$HOME" PATH="$PATH" SPAWN_FLEET_TEST_DATABASE_URL="$fleet_url" -- --full --apply
check "--apply, as the database's owner (no superuser): exit 0 (status $status)" [ "$status" -eq 0 ]
check "--apply: says each catalog before and after" grep -qE "^maintenance: catalogs:   pg_attribute: [0-9]+ (MB|kB) -> [0-9]+ (MB|kB)$" "$out"
check "--apply: says the total before and after" \
  grep -qE "VACUUM \(FULL, ANALYZE\) of [0-9]+ catalogs of the fleet's test database: [0-9]+ MB before, [0-9]+ MB after \(no row removed\)" "$out"
bytes_after=$(catalog_bytes)
check "the catalogs' size fell by more than half ($bytes_before -> $bytes_after bytes)" [ "$bytes_after" -lt $((bytes_before / 2)) ]
check "every catalog keeps its row count" [ "$(catalog_rows)" = "$rows_before" ]
check "the left test schema and its table are still there" \
  [ "$(admin -d "$database" -c "select count(*) from spawn_test_4242_kept.t")" = 0 ]
check "neither the port nor the database printed" withheld

if [ "$failures" -gt 0 ]; then
  echo "$PROG: $failures check(s) failed"
  exit 1
fi
echo "$PROG: all cases pass"
