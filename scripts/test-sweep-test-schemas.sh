#!/usr/bin/env bash
# scripts/test-sweep-test-schemas.sh: sweep-test-schemas.sh (P21) against a
# throwaway PostgreSQL cluster of its own (initdb in scratch, a socket in
# scratch, no TCP), under a fake HOME that refuses the real one: no live,
# fleet, gates' or ccd server is touched.
#
#   scripts/test-sweep-test-schemas.sh   # needs initdb/pg_ctl (PG_BIN, default /usr/lib/postgresql/16/bin) and psql
#
# Cases:
# 1. print-only: the section line; a schema whose pid is gone and whose files
#    are 2 days old would be dropped; one whose pid runs here, one written
#    now and one with no pid in its name stay; nothing is dropped.
# 2. --apply refuses (exit 2, every schema still there), each alone: no live
#    URL known; the live URL on the target's port; the live URL with the
#    target's name (neither printed); --min-idle-hours 23; no list file;
#    --apply without one; a list with no section for the target, or two; a
#    name in the section that is not spawn_test_<pid>_...; a name twice.
# 3. --apply of a reviewed list drops exactly its would-drop names in the
#    target's section, each re-checked: one written since stays (skipped,
#    written 0h ago), one gone is said (skipped gone), one whose drop cannot
#    take its lock fails alone (the others still go; exit 1); a qualifying
#    schema not in the list, and one listed only in another database's
#    section, stay; each drop follows its dry-run line; the counts before
#    and after are said.
# 4. A login that cannot read file times (not a superuser): the age is
#    unknown, so a schema whose pid is gone is kept by print-only and
#    skipped by --apply, never dropped (a gate's PID namespace makes a gone
#    pid no proof).
set -euo pipefail
umask 077

here=$(cd "$(dirname "$0")" && pwd)
PROG=test-sweep-test-schemas
pg_bin=${PG_BIN:-/usr/lib/postgresql/16/bin}
for tool in "$pg_bin/initdb" "$pg_bin/pg_ctl"; do
  [ -x "$tool" ] || { echo "$PROG: no $tool (PG_BIN)" >&2; exit 2; }
done
command -v psql >/dev/null || { echo "$PROG: no psql" >&2; exit 2; }

real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
real_home=$(cd "$real_home" && pwd -P)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/sweep-schemas-test.XXXXXX")
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
holder_pid=""
cleanup() {
  [ -z "$holder_pid" ] || kill "$holder_pid" 2>/dev/null || true
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
unset SPAWN_DATABASE_URL SPAWN_TEST_DATABASE_URL SPAWN_ENV_FILE PGHOST PGPORT PGDATABASE PGUSER PGPASSWORD PGOPTIONS

failures=0
pass() { printf 'ok   %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; failures=$((failures + 1)); }
check() { # check <description> <command...>
  local ck_what=$1
  shift
  if "$@"; then pass "$ck_what"; else fail "$ck_what"; fi
}

port=$((20000 + RANDOM % 20000))
socket=$scratch/sock
mkdir -m 700 "$socket"
"$pg_bin/initdb" -D "$scratch/data" -A trust -U drill_admin >/dev/null
"$pg_bin/pg_ctl" -D "$scratch/data" -l "$scratch/pg.log" -w \
  -o "-k $socket -p $port -c listen_addresses=''" start >/dev/null
enc_socket=$(printf '%s' "$socket" | sed 's|/|%2F|g')
admin() { psql -X -qtA -v ON_ERROR_STOP=1 -h "$socket" -p "$port" -U drill_admin -d sweep_db "$@"; }
psql -X -q -h "$socket" -p "$port" -U drill_admin -d postgres -c "create database sweep_db" \
  -c "create role plain_login login" >/dev/null
admin -c "grant create on database sweep_db to plain_login" >/dev/null
target_url="postgres://drill_admin@$enc_socket:$port/sweep_db"
plain_url="postgres://plain_login@$enc_socket:$port/sweep_db"
live_elsewhere="postgres://live@$enc_socket:$((port + 1))/live_db"

dead=$(($(cat /proc/sys/kernel/pid_max) + 1))
# make <schema> [old]: a schema with a table and a row; `old` sets its files'
# times 2 days back.
make() {
  admin -c "create schema $1" -c "create table $1.t (x int)" -c "insert into $1.t values (1)" >/dev/null
  [ "${2-}" != old ] || age_old "$1"
}
age_old() {
  admin -c "checkpoint" >/dev/null
  admin -c "select pg_relation_filepath(c.oid) from pg_class c join pg_namespace n on n.oid = c.relnamespace where n.nspname = '$1' and pg_relation_filepath(c.oid) is not null" |
    while IFS= read -r ao_file; do
      [ -e "$scratch/data/$ao_file" ] && touch -d '2 days ago' "$scratch/data/$ao_file"
    done
}
exists() { [ "$(admin -c "select count(*) from pg_namespace where nspname = '$1'")" = 1 ]; }
schema_count() { admin -c "select count(*) from pg_namespace where nspname like 'spawn\_test\_%'"; }

# run VAR=VALUE... [-- ARG...]: the sweep with only what is named added to
# the environment (exported in a subshell: no URL is ever an argument), and
# its own arguments; output in $out, status in $status.
run() {
  local rn_vars=()
  while [ $# -gt 0 ] && [ "$1" != -- ]; do
    rn_vars+=("$1")
    shift
  done
  [ $# -eq 0 ] || shift
  guard_home
  out=$scratch/out
  set +e
  (
    for rn_var in "${rn_vars[@]}"; do export "${rn_var?}"; done
    exec bash "$here/sweep-test-schemas.sh" "$@"
  ) >"$out" 2>&1
  status=$?
  set -e
  sed 's/^/  | /' "$out"
}
has() { grep -qE "$1" "$out"; }

old_gone=spawn_test_${dead}_aa01_1_old_gone
old_young=spawn_test_${dead}_aa02_2_written_since
old_vanish=spawn_test_${dead}_aa03_3_vanishes
old_locked=spawn_test_${dead}_aa04_4_locked
old_keep2=spawn_test_${dead}_aa05_5_also_goes
alive=spawn_test_$$_bb01_1_alive
young=spawn_test_${dead}_cc01_1_young
nopid=spawn_test_sweepprobe_nopid
for s in "$old_gone" "$old_young" "$old_vanish" "$old_locked" "$old_keep2"; do make "$s" old; done
make "$alive" old
make "$young"
make "$nopid" old

echo "== 1. print-only"
mkdir -p "$fake_home/.local/share/spawn-rsi"
run HOME="$HOME" PATH="$PATH" SPAWN_TEST_DATABASE_URL="$target_url"
check "exit 0 (status $status)" [ "$status" -eq 0 ]
check "the section line names the database and port" has "^database sweep_db port $port\$"
check "a gone pid with 2-day-old files would be dropped" has "^would-drop $old_gone pid=$dead idle=4[78]h "
check "a pid that runs here stays" has "^keep +$alive pid=$$ .* pid $$ runs here$"
check "one written now stays" has "^keep +$young .*written 0h ago \\(< 24h\\)$"
check "one with no pid stays" has "^keep +$nopid pid=- .* no pid in the name$"
check "it says it dropped nothing" has "^sweep-test-schemas \\(print-only, nothing dropped\\): would drop 5 schema"
check "all 8 schemas are still there" [ "$(schema_count)" = 8 ]
cp "$out" "$scratch/list.md"

echo "== 2. --apply refusals, each alone: exit 2, nothing dropped"
refused() { # refused <description> <message pattern>
  check "$1: refused (status $status)" [ "$status" -eq 2 ]
  check "$1: says why" has "$2"
  check "$1: nothing dropped" [ "$(schema_count)" = 8 ]
}
run HOME="$HOME" PATH="$PATH" SPAWN_TEST_DATABASE_URL="$target_url" -- --apply "$scratch/list.md"
refused "no live URL known" "refused: no live database URL is known"
printf 'SPAWN_DATABASE_URL="postgres://live@%s:%s/live_db"\n' "$enc_socket" "$port" \
  >"$fake_home/.local/share/spawn-rsi/spawn.env"
run HOME="$HOME" PATH="$PATH" SPAWN_TEST_DATABASE_URL="$target_url" -- --apply "$scratch/list.md"
refused "spawn.env's live URL on the target's port" "refused: the target is on the live database's port"
check "the live database's name is not printed" bash -c "! grep -q live_db '$out'"
printf 'SPAWN_DATABASE_URL=%s\n' "$live_elsewhere" >"$fake_home/.local/share/spawn-rsi/spawn.env"
run HOME="$HOME" PATH="$PATH" SPAWN_TEST_DATABASE_URL="$target_url" \
  SPAWN_DATABASE_URL="postgres://live@$enc_socket:$((port + 2))/sweep_db" -- --apply "$scratch/list.md"
refused "the environment's live URL with the target's name" "refused: the target has the live database's name"
check "the live port is not printed" bash -c "! grep -q '$((port + 2))' '$out'"
live=(HOME="$HOME" PATH="$PATH" SPAWN_TEST_DATABASE_URL="$target_url")
run "${live[@]}" -- --apply "$scratch/list.md" --min-idle-hours 23
refused "--min-idle-hours 23" "refused: --apply needs --min-idle-hours of at least 24"
run "${live[@]}" -- --apply "$scratch/no-such-list"
refused "a missing list file" "refused: no readable list file"
run "${live[@]}" -- --apply
refused "--apply without a list" "refused: --apply needs the reviewed list file"
sed "s/^database sweep_db port $port\$/database other_db port $port/" "$scratch/list.md" >"$scratch/other.md"
run "${live[@]}" -- --apply "$scratch/other.md"
refused "a list with no section for the target" "refused: the list holds 0 sections"
cat "$scratch/list.md" "$scratch/list.md" >"$scratch/twice.md"
run "${live[@]}" -- --apply "$scratch/twice.md"
refused "a list with two sections for the target" "refused: the list holds 2 sections"
{ cat "$scratch/list.md"; echo "would-drop public pid=1 idle=99h size=0MB"; } >"$scratch/public.md"
run "${live[@]}" -- --apply "$scratch/public.md"
refused "a name that is not a test schema's" "refused: names in the list that are not spawn_test_<pid>_...: public"
{ cat "$scratch/list.md"; echo "would-drop spawn_test_nopid_x pid=- idle=99h size=0MB"; } >"$scratch/nopid.md"
run "${live[@]}" -- --apply "$scratch/nopid.md"
refused "a test schema's name without a pid" "refused: names in the list that are not spawn_test_<pid>_...: spawn_test_nopid_x"
{ cat "$scratch/list.md"; echo "would-drop $old_gone pid=$dead idle=48h size=0MB"; } >"$scratch/dup.md"
run "${live[@]}" -- --apply "$scratch/dup.md"
refused "a name listed twice" "refused: names listed twice: $old_gone"

echo "== 3. --apply of the reviewed list"
# Since the list: one is written to, one is dropped by someone else, one is
# held by another session's lock; a qualifying schema appears that is not
# on the list, and another database's section lists one more.
admin -c "insert into $old_young.t values (2)" -c "checkpoint" >/dev/null
admin -c "drop schema $old_vanish cascade" >/dev/null
not_listed=spawn_test_${dead}_dd01_1_not_listed
other_only=spawn_test_${dead}_ee01_1_other_section
make "$not_listed" old
make "$other_only" old
{ cat "$scratch/list.md"; printf 'database other_db port %s\nwould-drop %s pid=%s idle=48h size=0MB\n' "$port" "$other_only" "$dead"; } \
  >"$scratch/reviewed.md"
admin -c "begin" -c "lock table $old_locked.t in access share mode" -c "select pg_sleep(60)" >/dev/null 2>&1 &
holder_pid=$!
for _ in $(seq 50); do
  [ "$(admin -c "select count(*) from pg_locks l join pg_class c on c.oid = l.relation where c.relname = 't' and c.relnamespace = '$old_locked'::regnamespace and l.granted")" -ge 1 ] && break
  sleep 0.1
done
started=$(date +%s)
run "${live[@]}" -- --apply "$scratch/reviewed.md"
kill "$holder_pid" 2>/dev/null || true
holder_pid=""
check "exit 1: one drop failed (status $status)" [ "$status" -eq 1 ]
check "the lock wait is bounded (under 40 s)" [ $(($(date +%s) - started)) -lt 40 ]
check "the listed, still qualifying ones are dropped" has "^dropped $old_gone\$"
check "  both of them" has "^dropped $old_keep2\$"
check "after its dry-run line" bash -c "grep -A1 '^would-drop $old_gone ' '$out' | grep -q '^dropped $old_gone\$'"
! exists "$old_gone" && ! exists "$old_keep2" && pass "  gone from the database" || fail "  gone from the database"
check "one written since is skipped and kept" has "^skipped $old_young written 0h ago \\(< 24h\\)$"
exists "$old_young" && pass "  it is still there" || fail "  it is still there"
check "one gone since is said" has "^skipped $old_vanish gone$"
check "a held one fails alone, said" has "^skipped $old_locked the drop failed: "
exists "$old_locked" && pass "  it is still there" || fail "  it is still there"
exists "$not_listed" && pass "a qualifying schema not on the list stays" || fail "a qualifying schema not on the list stays"
exists "$other_only" && pass "one listed only in another database's section stays" || fail "one listed only in another database's section stays"
for s in "$alive" "$young" "$nopid"; do exists "$s" && pass "kept: $s" || fail "kept: $s"; done
check "the totals are said" has "^sweep-test-schemas --apply: dropped 2, skipped 2, failed 1$"
check "before and after: count and MB" has "^sweep-test-schemas --apply: spawn_test_\\* before: 9 schema\\(s\\), [0-9]+ MB; after: 7 schema\\(s\\), [0-9]+ MB$"

echo "== 4. no file times (not a superuser): a gone pid alone never qualifies"
unknown=spawn_test_${dead}_ff01_1_age_unknown
make "$unknown" old
admin -c "alter schema $unknown owner to plain_login" -c "alter table $unknown.t owner to plain_login" >/dev/null
plain=(HOME="$HOME" PATH="$PATH" SPAWN_TEST_DATABASE_URL="$plain_url")
run "${plain[@]}"
check "print-only: exit 0 (status $status)" [ "$status" -eq 0 ]
check "it says the times cannot be read" has "cannot read the relation files' times here; every schema stays"
check "the gone pid's schema is kept: age unknown" has "^keep +$unknown pid=$dead idle=\\?h .* its age is unknown$"
check "nothing would be dropped" has "would drop 0 schema"
{ echo "database sweep_db port $port"; echo "would-drop $unknown pid=$dead idle=48h size=0MB"; } >"$scratch/unknown.md"
run "${plain[@]}" -- --apply "$scratch/unknown.md"
check "--apply: exit 0, nothing failed (status $status)" [ "$status" -eq 0 ]
check "--apply: skipped, its age unknown" has "^skipped $unknown its age is unknown$"
exists "$unknown" && pass "--apply: it is still there" || fail "--apply: it is still there"

guard_home
if [ "$failures" -eq 0 ]; then
  echo "test-sweep-test-schemas: all cases pass"
else
  echo "test-sweep-test-schemas: $failures FAIL"
  exit 1
fi
