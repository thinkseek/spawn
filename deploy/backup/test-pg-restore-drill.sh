#!/usr/bin/env bash
# Tests of the restore drill (pg-restore-drill.sh --file), without R2 and
# without the live database: a throwaway database on the test server plays
# "live", and a dump of it made by the drill image's own pg_dump is the
# backup.
#
#   SPAWN_TEST_DATABASE_URL=postgres://u:p@%2Fsocket%2Fdir/db deploy/backup/test-pg-restore-drill.sh
#
# Cases:
# 1. the drill restores the dump and its counts match "live";
# 2. a drill server that restarts after the identity check (a `docker` shim
#    restarts the container before the drill's first write) is refused.
#
# Needs docker and psql, and a socket URL (the dump is made in a container
# that mounts the socket directory). The throwaway database is dropped on
# exit; no password is printed.
set -euo pipefail
umask 077
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
# shellcheck source=scripts/pg-url.sh
. "$root/scripts/pg-url.sh"
: "${SPAWN_TEST_DATABASE_URL:?SPAWN_TEST_DATABASE_URL is required}"
pg_url_export "$SPAWN_TEST_DATABASE_URL" || { echo "not a single-host postgres URL" >&2; exit 2; }
case "${PGHOST-}" in
/*) ;;
*) echo "test-pg-restore-drill: needs a socket URL" >&2; exit 2 ;;
esac
image=${SPAWN_PG_IMAGE:-postgres:16-alpine}
real_docker=$(command -v docker)

work=$(mktemp -d)
db="drill_test_$$_$RANDOM"
cleanup() {
  psql -XAtq -d postgres -c "drop database if exists $db with (force)" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

fail() { echo "FAIL $*"; exit 1; }
ok() { echo "ok   $*"; }

# "Live": the drill's tables, with rows.
psql -XAtq -v ON_ERROR_STOP=1 -d postgres -c "create database $db" >/dev/null
psql -XAtq -v ON_ERROR_STOP=1 -d "$db" >/dev/null <<'SQL'
create table spawn_events (id bigint primary key);
create table spawn_commands (id bigint primary key);
create table mission_missions (id bigint primary key);
create table mission_tasks (id bigint primary key);
insert into spawn_events select generate_series(1, 30);
insert into spawn_commands select generate_series(1, 4);
insert into mission_missions select generate_series(1, 2);
insert into mission_tasks select generate_series(1, 5);
SQL
# The backup: the drill image's own pg_dump (its pg_restore reads no newer
# archive), through the socket directory, the password from the environment.
"$real_docker" run --rm --network none -v "$PGHOST:/socket" -e PGPASSWORD "$image" \
  pg_dump -h /socket -p "$PGPORT" -U "$PGUSER" -Fc "$db" >"$work/spawn-20260101-000000.dump"
[ -s "$work/spawn-20260101-000000.dump" ] || fail "no dump"

# The drill reads "live" from an env file, as it reads spawn.env.
printf 'SPAWN_DATABASE_URL=%s/%s\n' "$(pg_url_server "$SPAWN_TEST_DATABASE_URL")" "$db" >"$work/spawn.env"
chmod 600 "$work/spawn.env"
drill() {
  SPAWN_ENV_FILE=$work/spawn.env SPAWN_PG_IMAGE=$image SPAWN_DRILL_TMPFS_SIZE=256m \
    bash "$here/pg-restore-drill.sh" --file "$work/spawn-20260101-000000.dump"
}

# 1. A restore that works.
status=0
drill >"$work/drill.out" 2>&1 || status=$?
[ "$status" -eq 0 ] || { cat "$work/drill.out"; fail "the drill failed on a good dump"; }
grep -q '^pg-restore-drill: ok: ' "$work/drill.out" || { cat "$work/drill.out"; fail "no ok line"; }
grep -Eq '^spawn_events +30 +30 +\+0$' "$work/drill.out" || { cat "$work/drill.out"; fail "counts differ"; }
ok "a local dump restores and its counts match"

# 2. The drill server restarts after the identity check: the shim restarts
# the container before the drill's `create database`, and waits until the
# new server answers, as a restart window would look.
mkdir "$work/bin"
cat >"$work/bin/docker" <<EOF
#!/bin/sh
if [ "\$1" = exec ] && [ "\$2" = -i ] && [ "\$4" = psql ]; then
  for arg in "\$@"; do
    if [ "\$arg" = "create database spawn_drill" ]; then
      "$real_docker" restart "\$3" >/dev/null
      for _ in \$(seq 120); do
        "$real_docker" exec "\$3" pg_isready -q -h 127.0.0.1 -U postgres 2>/dev/null && break
        sleep 0.5
      done
    fi
  done
fi
exec "$real_docker" "\$@"
EOF
chmod 755 "$work/bin/docker"
status=0
PATH="$work/bin:$PATH" drill >"$work/drill.out" 2>&1 || status=$?
[ "$status" -ne 0 ] || { cat "$work/drill.out"; fail "a drill server that restarted was accepted"; }
grep -q 'the drill server restarted during the restore' "$work/drill.out" ||
  { cat "$work/drill.out"; fail "refused for another reason"; }
ok "a drill server that restarts after the identity check is refused"
echo "restore drill: all cases pass"
