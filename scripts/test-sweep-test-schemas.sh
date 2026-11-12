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
old_viewed=spawn_test_${dead}_aa06_6_viewed_from_outside
old_fk=spawn_test_${dead}_aa07_7_referenced_from_outside
alive=spawn_test_$$_bb01_1_alive
young=spawn_test_${dead}_cc01_1_young
nopid=spawn_test_sweepprobe_nopid
for s in "$old_gone" "$old_young" "$old_vanish" "$old_locked" "$old_keep2" "$old_viewed" "$old_fk"; do make "$s" old; done
make "$alive" old
make "$young"
make "$nopid" old

echo "== 1. print-only"
mkdir -p "$fake_home/.local/share/spawn-rsi"
run HOME="$HOME" PATH="$PATH" SPAWN_TEST_DATABASE_URL="$target_url"
check "exit 0 (status $status)" [ "$status" -eq 0 ]
check "the section line names the database and port" has "^database sweep_db port $port\$"
check "and the list's time and next transaction id" has "^listed [0-9]+ xid [0-9]+\$"
check "a gone pid with 2-day-old files would be dropped" has "^would-drop $old_gone pid=$dead idle=4[78]h "
check "a pid that runs here stays" has "^keep +$alive pid=$$ .* pid $$ runs here$"
check "one written now stays" has "^keep +$young .*written 0h ago \\(< 24h\\)$"
check "one with no pid stays" has "^keep +$nopid pid=- .* no pid in the name$"
check "it says it dropped nothing" has "^sweep-test-schemas \\(print-only, nothing dropped\\): would drop 7 schema"
check "all 10 schemas are still there" [ "$(schema_count)" = 10 ]
cp "$out" "$scratch/list-fresh.md"
# --apply takes a list at least 24 h old: this one is made to read 25 h old
# (its transaction id stays this run's, so what changes after it is seen).
# aged_listed: a `listed` line 25 h old with the transaction id of now.
aged() { sed -E "s/^listed [0-9]+ /listed $(($(date +%s) - 90000)) /" "$1"; }
aged_listed() { echo "listed $(($(date +%s) - 90000)) xid $(admin -c "select pg_snapshot_xmax(pg_current_snapshot())")"; }
aged "$scratch/list-fresh.md" >"$scratch/list.md"
# list_of <name>...: a list made now by the print-only run, aged 25 h, with
# the would-drop lines of the names given (each must qualify) and any
# padding lines in $padding.
list_of() {
  run HOME="$HOME" PATH="$PATH" SPAWN_TEST_DATABASE_URL="$target_url"
  aged "$out" | grep -E '^(database|listed) '
  for lo_name in "$@"; do grep "^would-drop $lo_name " "$out"; done
  [ -z "${padding-}" ] || printf '%s\n' "$padding"
}

echo "== 2. --apply refusals, each alone: exit 2, nothing dropped"
refused() { # refused <description> <message pattern>
  check "$1: refused (status $status)" [ "$status" -eq 2 ]
  check "$1: says why" has "$2"
  check "$1: nothing dropped" [ "$(schema_count)" = 10 ]
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
run "${live[@]}" -- --apply "$scratch/list-fresh.md"
refused "a list less than 24 h old" "refused: the list is 0h old; --apply needs it at least 24h old"
grep -v '^listed ' "$scratch/list.md" >"$scratch/unlisted.md"
run "${live[@]}" -- --apply "$scratch/unlisted.md"
refused "a list without its listed line" "refused: the list's section has no single"
run "${live[@]}" -- --dry-run --apply "$scratch/list.md"
refused "--dry-run with --apply (no rehearsal that deletes)" "refused: --dry-run with --apply"
run "${live[@]}" -- --apply "$scratch/list.md" --dry-run
refused "  in either order" "refused: --dry-run with --apply"
run "${live[@]}" -- --preview
refused "--preview without --apply" "refused: --preview goes with --apply"
sed "s/^database sweep_db port $port\$/database other_db port $port/" "$scratch/list.md" >"$scratch/other.md"
run "${live[@]}" -- --apply "$scratch/other.md"
refused "a list with no section for the target" "refused: the list holds 0 sections"
cat "$scratch/list.md" "$scratch/list.md" >"$scratch/twice.md"
run "${live[@]}" -- --apply "$scratch/twice.md"
refused "a list with two sections for the target" "refused: the list holds 2 sections"
{ cat "$scratch/list.md"; echo "would-drop public pid=1 idle=99h size=0MB mods=0 seq=0"; } >"$scratch/public.md"
run "${live[@]}" -- --apply "$scratch/public.md"
refused "a name that is not a test schema's" "refused: names in the list that are not spawn_test_<pid>_...: public"
{ cat "$scratch/list.md"; echo "would-drop spawn_test_nopid_x pid=- idle=99h size=0MB mods=0 seq=0"; } >"$scratch/nopid.md"
run "${live[@]}" -- --apply "$scratch/nopid.md"
refused "a test schema's name without a pid" "refused: names in the list that are not spawn_test_<pid>_...: spawn_test_nopid_x"
{ cat "$scratch/list.md"; echo "would-drop $old_gone pid=$dead idle=48h size=0MB mods=1 seq=0"; } >"$scratch/dup.md"
run "${live[@]}" -- --apply "$scratch/dup.md"
refused "a name listed twice" "refused: names listed twice: $old_gone"

echo "== 3. --apply of the reviewed list"
# Since the list: one is written to, one is dropped by someone else, one is
# held by another session's lock; a qualifying schema appears that is not
# on the list, and another database's section lists one more.
admin -c "insert into $old_young.t values (2)" -c "checkpoint" >/dev/null
# And another schema (not a test schema's) now depends on two listed ones: a
# view over one's table, a foreign key to the other's. Their files are aged
# again after (the key's index, any page a check wrote).
admin -c "create schema other_app" -c "create view other_app.v as select x from $old_viewed.t" \
  -c "alter table $old_fk.t add primary key (x)" \
  -c "create table other_app.r (x int references $old_fk.t (x))" >/dev/null
age_old "$old_viewed"
age_old "$old_fk"
admin -c "drop schema $old_vanish cascade" >/dev/null
not_listed=spawn_test_${dead}_dd01_1_not_listed
other_only=spawn_test_${dead}_ee01_1_other_section
make "$not_listed" old
make "$other_only" old
{ cat "$scratch/list.md"; printf 'database other_db port %s\nwould-drop %s pid=%s idle=48h size=0MB\n' "$port" "$other_only" "$dead"; } \
  >"$scratch/reviewed.md"
admin -c "begin" -c "lock table $old_locked.t in access share mode" -c "select pg_sleep(60)" >/dev/null 2>&1 &
holder_pid=$!
# The holder must hold its lock before the apply starts; a holder that never
# got it would let the apply drop the held schema and fail two checks for a
# slow start, so that is a harness error instead (exit 2).
held=""
for _ in $(seq 300); do
  if [ "$(admin -c "select count(*) from pg_locks l join pg_class c on c.oid = l.relation where c.relname = 't' and c.relnamespace = '$old_locked'::regnamespace and l.granted")" -ge 1 ]; then
    held=1
    break
  fi
  sleep 0.1
done
[ -n "$held" ] || { echo "$PROG: harness error: the holder never got its lock on $old_locked.t in 30 s" >&2; exit 2; }
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
check "one written since is skipped and kept" has "^skipped $old_young changed since the list \\(1 rows of $old_young\\.t\\)$"
exists "$old_young" && pass "  it is still there" || fail "  it is still there"
check "one gone since is said" has "^skipped $old_vanish gone$"
check "a held one fails alone, said" has "^skipped $old_locked the drop failed: "
exists "$old_locked" && pass "  it is still there" || fail "  it is still there"
exists "$not_listed" && pass "a qualifying schema not on the list stays" || fail "a qualifying schema not on the list stays"
exists "$other_only" && pass "one listed only in another database's section stays" || fail "one listed only in another database's section stays"
for s in "$alive" "$young" "$nopid"; do exists "$s" && pass "kept: $s" || fail "kept: $s"; done
check "a qualifying schema not on the list is said, kept" has "^not-on-list $not_listed pid=$dead idle=4[78]h size=[0-9]+MB mods=1 seq=0 \\(qualifies now, not reviewed: kept\\)$"
check "  so is the one only another database's section lists" has "^not-on-list $other_only "
check "  with their count" has "^sweep-test-schemas --apply: 2 qualifying schema\\(s\\) not on the list, kept$"
check "a listed one another schema's view depends on is skipped" has "^skipped $old_viewed dependents-outside \\(1: \"_RETURN\" on other_app\\.v\\)$"
exists "$old_viewed" && pass "  it is still there" || fail "  it is still there"
check "  and so is the view" [ "$(admin -c "select count(*) from pg_views where schemaname = 'other_app' and viewname = 'v'")" = 1 ]
check "a listed one another schema's foreign key references is skipped" has "^skipped $old_fk dependents-outside \\([0-9]+: .* on other_app\\.r\\)$"
exists "$old_fk" && pass "  it is still there" || fail "  it is still there"
check "  and so is the key" [ "$(admin -c "select count(*) from pg_constraint where conrelid = 'other_app.r'::regclass and contype = 'f'")" = 1 ]
check "the totals are said" has "^sweep-test-schemas --apply: dropped 2, skipped 4, failed 1$"
check "before and after: count and MB" has "^sweep-test-schemas --apply: spawn_test_\\* before: 11 schema\\(s\\), [0-9]+ MB; after: 9 schema\\(s\\), [0-9]+ MB$"

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
{ echo "database sweep_db port $port"; aged_listed; echo "would-drop $unknown pid=$dead idle=48h size=0MB mods=1 seq=0"; } >"$scratch/unknown.md"
run "${plain[@]}" -- --apply "$scratch/unknown.md"
check "--apply: exit 0, nothing failed (status $status)" [ "$status" -eq 0 ]
check "--apply: skipped, its age unknown" has "^skipped $unknown its age is unknown$"
exists "$unknown" && pass "--apply: it is still there" || fail "--apply: it is still there"

echo "== 5. a long list: every listed name is on the list (none said not-on-list)"
# A list longer than a pipe's buffer, its qualifying names first: the
# membership test must see the whole list. (Piped into grep -q under
# pipefail, the writer died of SIGPIPE when grep stopped early, and listed
# names were said to be not on the list: 77 of 1230 on ccd, 2026-11-12.)
long1=spawn_test_${dead}_ab01_1_long_list_first
long2=spawn_test_${dead}_ab02_2_long_list_second
make "$long1" old
make "$long2" old
padding=$(for i in $(seq 1100); do
  echo "would-drop spawn_test_${dead}_ff${i}_padding_padding_padding_padding_padding_padding pid=$dead idle=48h size=0MB mods=0 seq=0"
done)
list_of "$long1" "$long2" >"$scratch/long.md"
padding=""
check "its names alone are longer than a pipe's buffer" \
  [ "$(sed -n 's/^would-drop \([^ ]*\) .*/\1/p' "$scratch/long.md" | wc -c)" -gt 65536 ]
run "${live[@]}" -- --apply "$scratch/long.md"
check "exit 0 (status $status)" [ "$status" -eq 0 ]
check "the first listed name is not said not-on-list" bash -c "! grep -q '^not-on-list $long1 ' '$out'"
check "nor the second" bash -c "! grep -q '^not-on-list $long2 ' '$out'"
check "both are dropped" bash -c "grep -qx 'dropped $long1' '$out' && grep -qx 'dropped $long2' '$out'"

echo "== 6. under the drop's locks: a change since the list, an outside operator; --preview drops nothing"
# Made old, listed (25 h back, the transaction id of now); then, after the
# list: a function in one, a row inserted into another and one deleted from
# a third (no checkpoint: their files keep their old times), and an operator
# in another schema over a fourth's function.
c_func=spawn_test_${dead}_ca01_1_function_since
c_ins=spawn_test_${dead}_ca02_2_insert_since
c_del=spawn_test_${dead}_ca03_3_delete_since
c_op=spawn_test_${dead}_ca04_4_operator_outside
c_ok=spawn_test_${dead}_ca05_5_untouched
for s in "$c_func" "$c_ins" "$c_del" "$c_op" "$c_ok"; do make "$s"; done
admin -c "create function $c_op.lt(int, int) returns boolean language sql immutable as 'select \$1 < \$2'" >/dev/null
for s in "$c_func" "$c_ins" "$c_del" "$c_op" "$c_ok"; do age_old "$s"; done
list_of "$c_func" "$c_ins" "$c_del" "$c_op" "$c_ok" >"$scratch/changes.md"
check "the changes list holds all five" [ "$(grep -c '^would-drop ' "$scratch/changes.md")" = 5 ]
admin -c "create function $c_func.g() returns int language sql as 'select 1'" \
  -c "insert into $c_ins.t values (5)" -c "delete from $c_del.t" \
  -c "create operator public.<<< (function = $c_op.lt, leftarg = int, rightarg = int)" >/dev/null
run "${live[@]}" -- --apply "$scratch/changes.md" --preview
check "--preview: exit 0 (status $status)" [ "$status" -eq 0 ]
check "--preview: the untouched one would be dropped" has "^would drop $c_ok \\(preview: rolled back\\)\$"
check "--preview: says it dropped nothing" has "^sweep-test-schemas --apply --preview \\(nothing dropped\\): would drop 1, skipped 4, failed 0\$"
for s in "$c_func" "$c_ins" "$c_del" "$c_op" "$c_ok"; do exists "$s" && pass "--preview: kept $s" || fail "--preview: kept $s"; done
run "${live[@]}" -- --apply "$scratch/changes.md"
check "exit 0 (status $status)" [ "$status" -eq 0 ]
check "a function made since the list: kept" has "^skipped $c_func changed since the list \\([0-9]+ in pg_proc\\)\$"
check "a row inserted since the list, not checkpointed: kept" has "^skipped $c_ins changed since the list \\([0-9]+ rows of $c_ins\\.t\\)\$"
check "a row deleted since the list, not checkpointed: kept" has "^skipped $c_del changed since the list \\(rows modified: 1 then, 2 now\\)\$"
check "an operator in another schema over its function: kept" has "^skipped $c_op dependents-outside \\(1: public\\.<<<\\(integer,integer\\)\\)\$"
check "  and so is the operator" [ "$(admin -c "select count(*) from pg_operator where oprname = '<<<'")" = 1 ]
check "the untouched one is dropped" has "^dropped $c_ok\$"
for s in "$c_func" "$c_ins" "$c_del" "$c_op"; do exists "$s" && pass "kept: $s" || fail "kept: $s"; done
! exists "$c_ok" && pass "gone: $c_ok" || fail "gone: $c_ok"

guard_home
if [ "$failures" -eq 0 ]; then
  echo "test-sweep-test-schemas: all cases pass"
else
  echo "test-sweep-test-schemas: $failures FAIL"
  exit 1
fi
