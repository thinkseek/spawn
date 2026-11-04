#!/usr/bin/env bash
# Restore drill: prove the newest R2 backup restores, on the TEST server only.
#
#   pg-restore-drill.sh          restore into a scratch database, compare, drop it
#   pg-restore-drill.sh --keep   leave the scratch database for inspection
#
# 1. one ListObjectsV2 call; the newest postgres/spawn-*.dump.gpg
# 2. GET it, decrypt with backup.key, check the SHA-256 recorded at upload
# 3. CREATE DATABASE spawn_drill_<utc ts> on the server in
#    ~/.config/spawn/test-database.url and pg_restore into it
# 4. row counts of the core tables there and on the live database (read-only
#    transactions on live), side by side
#
# Never restores into the live server: the test URL must not name port
# 15433 or equal the live URL, and the two servers must have different
# postmaster start times (i.e. be different running servers).
set -euo pipefail
umask 077
PROG=pg-restore-drill
. "$(dirname "$(readlink -f "$0")")/lib.sh"

TABLES=(spawn_events spawn_commands mission_missions mission_tasks)
LIVE_PORT=15433

keep_db=
case "${1-}" in
  "") ;;
  --keep) keep_db=1 ;;
  *) die "usage: $0 [--keep]" ;;
esac

need_tools pg_restore psql gpg curl sha256sum date
load_live_url
load_r2
check_private "$SPAWN_BACKUP_KEY_FILE"
if [ -n "${SPAWN_TEST_DATABASE_URL-}" ]; then
  TEST_URL=$SPAWN_TEST_DATABASE_URL
else
  check_private "$SPAWN_TEST_DATABASE_URL_FILE"
  TEST_URL=$(head -n1 "$SPAWN_TEST_DATABASE_URL_FILE")
fi
[ -n "$TEST_URL" ] || die "no test database URL"

# Guard 1: by text.
[ "$TEST_URL" != "$LIVE_URL" ] || die "the test URL is the live URL; refusing"
case $TEST_URL in
  *":$LIVE_PORT"* | *"port=$LIVE_PORT"*) die "the test URL names port $LIVE_PORT (the live server); refusing" ;;
esac

live_sql() { with_db "$LIVE_URL" env PGOPTIONS='-c default_transaction_read_only=on' psql -XAtq -v ON_ERROR_STOP=1 "$@"; }
test_sql() { with_db "$TEST_URL" psql -XAtq -v ON_ERROR_STOP=1 "$@"; }

# Guard 2: they are different running servers.
ident="select pg_postmaster_start_time()::text || ' ' || current_setting('server_version')"
live_ident=$(live_sql -c "$ident") || die "cannot reach the live database"
test_ident=$(test_sql -c "$ident") || die "cannot reach the test database"
[ "$live_ident" != "$test_ident" ] || die "the test URL reaches the live server; refusing"

work=$(mktemp -d "${TMPDIR:-/tmp}/pg-restore-drill.XXXXXX")
drill_db="spawn_drill_$(date -u +%Y%m%d%H%M%S)"
drill_url=$(url_with_dbname "$TEST_URL" "$drill_db")
created=
cleanup() {
  if [ -n "$created" ] && [ -z "$keep_db" ]; then
    test_sql -c "drop database if exists \"$drill_db\" with (force)" >/dev/null 2>&1 \
      || echo "$PROG: warning: could not drop $drill_db on the test server" >&2
  fi
  rm -rf "$work"
}
trap cleanup EXIT

# 1. newest backup
newest=$(list_backups | awk '{print $1}' | grep -E "$R2_KEY_RE" | tail -n1) || true
[ -n "$newest" ] || die "no backups under s3://$R2_BUCKET/$R2_PREFIX"
log "newest backup: $newest"

# 2. download, decrypt, check
enc="$work/backup.dump.gpg"
dump="$work/backup.dump"
out=$(s3 -D "$work/get.headers" -o "$enc" "$(object_url "$newest")") || die "download of $newest failed: $out"
gpg "${gpg_common[@]}" --passphrase-file "$SPAWN_BACKUP_KEY_FILE" -o "$dump" -d "$enc" \
  || die "decryption failed (wrong backup.key, or the object is corrupt)"
want_sha=$(grep -i '^x-amz-meta-sha256:' "$work/get.headers" | tail -1 | tr -d '\r' | sed 's/^[^:]*: *//')
got_sha=$(sha256sum "$dump" | cut -d' ' -f1)
if [ -n "$want_sha" ]; then
  [ "$got_sha" = "$want_sha" ] || die "decrypted dump SHA-256 $got_sha does not match $want_sha recorded at upload"
  log "decrypted $(stat -c %s "$dump") bytes; SHA-256 matches the upload"
else
  log "decrypted $(stat -c %s "$dump") bytes; no SHA-256 recorded at upload to compare"
fi
pg_restore -l "$dump" >/dev/null || die "pg_restore cannot read the decrypted dump"

# 3. restore into a scratch database on the test server
test_sql -c "create database \"$drill_db\"" >/dev/null || die "cannot create $drill_db on the test server"
created=1
# Through SQL text in one transaction, stopping at the first error. pg_restore
# 17+ starts its script with SET transaction_timeout, which servers before 17
# reject (live is 16), so that one line is dropped for them.
server_num=$(with_db "$drill_url" psql -XAtq -c 'show server_version_num') || die "cannot reach $drill_db"
filter=(cat)
[ "$server_num" -ge 170000 ] || filter=(sed '0,/^SET transaction_timeout = 0;$/{//d}')
pg_restore --no-owner --no-acl -f - "$dump" | "${filter[@]}" |
  with_db "$drill_url" psql -XAq -1 -v ON_ERROR_STOP=1 >/dev/null \
  || die "restore into $drill_db failed"
log "restored into $drill_db on the test server"

# 4. compare row counts
count_sql() {
  local sep="" t
  for t in "${TABLES[@]}"; do
    printf "%sselect '%s', (select count(*) from %s)" "$sep" "$t" "$t"
    sep=" union all "
  done
}
restored=$(with_db "$drill_url" psql -XAtq -F' ' -v ON_ERROR_STOP=1 -c "$(count_sql)") \
  || die "counting rows in the restored database failed (a table is missing?)"
live=$(live_sql -F' ' -c "$(count_sql)") || die "counting rows on live failed"

stamp=${newest#"${R2_PREFIX}"spawn-}
stamp=${stamp%%.*}
age_s=$(($(date -u +%s) - $(date -u -d "${stamp:0:8} ${stamp:9:2}:${stamp:11:2}:${stamp:13:2}" +%s)))
log "backup taken ${stamp} UTC, $((age_s / 3600))h$(((age_s % 3600) / 60))m ago; live counted now"
printf '%-18s %10s %10s %8s\n' table backup live delta
fail=
for t in "${TABLES[@]}"; do
  b=$(awk -v t="$t" '$1 == t {print $2}' <<<"$restored")
  l=$(awk -v t="$t" '$1 == t {print $2}' <<<"$live")
  printf '%-18s %10s %10s %+8d\n' "$t" "$b" "$l" "$((l - b))"
  if [ "$l" -gt 0 ] && [ "$b" -eq 0 ]; then fail=1; fi
  if [ "$b" -gt "$l" ]; then log "note: $t has fewer rows on live than in the backup (rows deleted since)"; fi
done
[ -z "$fail" ] || die "a table that has rows on live restored empty"
[ -z "$keep_db" ] || log "kept $drill_db on the test server (drop it when done)"
log "ok: $newest restores; positive delta = rows written to live since the backup"
