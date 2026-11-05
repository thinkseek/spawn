#!/usr/bin/env bash
# Restore drill: prove the newest R2 backup restores, into a throwaway
# PostgreSQL started for the drill and removed after it.
#
#   pg-restore-drill.sh              the newest postgres/daily/ backup
#   pg-restore-drill.sh KEY          a given object, e.g. postgres/monthly/spawn-...
#   pg-restore-drill.sh --file DUMP  a local, unencrypted pg_dump -Fc file (no R2,
#                                    no backup.key): to test the drill itself
#
# 1. one ListObjectsV2 call on postgres/daily/; the newest backup
# 2. GET it, decrypt with backup.key, check the SHA-256 recorded at upload
# 3. start a container from the live server's image: --network none (no port,
#    nothing outside can connect, so its local trust login is safe), data on
#    tmpfs, label spawn.restore-drill; removed on exit, and leftovers of
#    earlier drills are removed at start
# 4. pg_restore with the container's own pg_restore into a new database
# 5. row counts of the core tables there and on live (read-only
#    transactions), side by side
#
# Never restores into the live server: the restore only goes through
# `docker exec` into the container this run started, and before it the two
# servers' identities (system identifier, postmaster start time) must differ.
#
# The drill server's restart. A fresh data directory starts twice: the
# image's entrypoint runs initdb under a temporary server (on its socket
# only, listen_addresses=''), stops it, and starts the real one. The drill
# waits for the real one: the entrypoint's "init process complete" line
# followed by a "ready to accept connections" line in the container's log,
# and TCP on the container's loopback answering. It then records that
# server's pg_postmaster_start_time before the identity check and checks it
# is unchanged after the restore and after the counts: every statement went
# to the one server it checked.
set -euo pipefail
umask 077
PROG=pg-restore-drill
. "$(dirname "$(readlink -f "$0")")/lib.sh"

TABLES=(spawn_events spawn_commands mission_missions mission_tasks)
LABEL=spawn.restore-drill
TMPFS_SIZE=${SPAWN_DRILL_TMPFS_SIZE:-4g}

local_dump=
case "${1-}" in
  --file)
    [ $# -eq 2 ] || die "usage: $0 [KEY | --file DUMP]"
    local_dump=$2
    [ -f "$local_dump" ] || die "no dump at $local_dump"
    want_key=
    ;;
  *)
    [ $# -le 1 ] || die "usage: $0 [KEY | --file DUMP]"
    want_key=${1-}
    ;;
esac

need_tools docker psql sha256sum date awk
load_live_url
if [ -z "$local_dump" ]; then
  need_tools gpg curl
  load_r2
  check_private "$SPAWN_BACKUP_KEY_FILE"
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/pg-restore-drill.XXXXXX")
ctr=
cleanup() {
  [ -z "$ctr" ] || docker rm -f "$ctr" >/dev/null 2>&1 || warn "could not remove container $ctr"
  rm -rf "$work"
}
trap cleanup EXIT

old=$(docker ps -aq --filter "label=$LABEL")
if [ -n "$old" ]; then
  # shellcheck disable=SC2086
  docker rm -f $old >/dev/null || die "cannot remove leftover drill containers"
  log "removed $(wc -w <<<"$old") leftover drill container(s)"
fi

if [ -n "$local_dump" ]; then
  # 1-2. a local dump: nothing to fetch, decrypt or compare with an upload
  key=$local_dump
  dump=$local_dump
  recorded_events=
  log "backup: $key (local file)"
else
  # 1. which backup
  if [ -n "$want_key" ]; then
    key=$want_key
  else
    key=$(list_objects postgres/daily/ | awk '{print $1}' | grep -E "$R2_KEY_RE" | tail -n1) || true
    [ -n "$key" ] || die "no backups under s3://$R2_BUCKET/postgres/daily/"
  fi
  log "backup: $key"

  # 2. download, decrypt, check
  enc="$work/backup.dump.gpg"
  dump="$work/backup.dump"
  out=$(s3 -D "$work/get.headers" -o "$enc" "$(object_url "$key")") || die "download of $key failed: $out"
  gpg "${gpg_common[@]}" --passphrase-file "$SPAWN_BACKUP_KEY_FILE" -o "$dump" -d "$enc" \
    || die "decryption failed (wrong backup.key, or the object is corrupt)"
  header() { grep -i "^$1:" "$work/get.headers" | tail -1 | tr -d '\r' | sed 's/^[^:]*: *//'; }
  want_sha=$(header x-amz-meta-sha256)
  recorded_events=$(header x-amz-meta-spawn-events)
  got_sha=$(sha256sum "$dump" | cut -d' ' -f1)
  if [ -n "$want_sha" ]; then
    [ "$got_sha" = "$want_sha" ] || die "decrypted dump SHA-256 $got_sha does not match $want_sha recorded at upload"
    log "decrypted $(stat -c %s "$dump") bytes; SHA-256 matches the upload"
  else
    warn "no SHA-256 recorded at upload to compare"
  fi
fi

# 3. the throwaway server
image=$(pg_image)
ctr="spawn-restore-drill-$(head -c 6 /dev/urandom | od -An -tx1 | tr -d ' \n')"
docker run -d --rm --name "$ctr" --label "$LABEL=1" --network none \
  --tmpfs "/var/lib/postgresql/data:rw,size=$TMPFS_SIZE" \
  -e POSTGRES_HOST_AUTH_METHOD=trust "$image" >/dev/null \
  || die "cannot start a $image container"
# The real server, not the entrypoint's temporary one (see the header): its
# log shows initdb's server stopped and the real one ready, and TCP on the
# container's loopback (which the temporary server never opens) answers.
final_server_ready() {
  docker logs "$ctr" 2>&1 | awk '
    /PostgreSQL init process complete; ready for start up/ { init = 1 }
    init && /database system is ready to accept connections/ { ready = 1 }
    END { exit !ready }' &&
    docker exec "$ctr" pg_isready -q -h 127.0.0.1 -U postgres 2>/dev/null
}
for _ in $(seq 240); do
  final_server_ready && break
  sleep 0.5
done
final_server_ready || die "the drill server did not finish its first start (init, then the real server)"
drill_sql() { docker exec -i "$ctr" psql -XAtq -v ON_ERROR_STOP=1 -h 127.0.0.1 -U postgres "$@"; }
started="select pg_postmaster_start_time()::text"
drill_started=$(drill_sql -c "$started") || die "cannot read the drill server's start time"
[ -n "$drill_started" ] || die "the drill server has no start time"
# same_drill_server <step>: the drill server is the one that was checked.
same_drill_server() {
  local now
  now=$(drill_sql -c "$started") || die "cannot read the drill server's start time after $1"
  [ "$now" = "$drill_started" ] \
    || die "the drill server restarted during $1 (started $drill_started, now $now); refusing"
}

# Never the live server: different cluster, different running server.
ident="select system_identifier::text from pg_control_system()"
start="select pg_postmaster_start_time()::text || ' ' || current_setting('server_version')"
live_id=$(live_sql -c "$ident") || die "cannot read the live server's identity"
drill_id=$(drill_sql -c "$ident") || die "cannot read the drill server's identity"
[ -n "$live_id" ] && [ "$live_id" != "$drill_id" ] || die "the drill server has the live server's system identifier; refusing"
[ "$(live_sql -c "$start")" != "$(drill_sql -c "$start")" ] || die "the drill server looks like the live server; refusing"
same_drill_server "the identity check"

# 4. restore with the container's own pg_restore
drill_sql -c "create database spawn_drill" >/dev/null || die "cannot create the drill database"
docker exec -i "$ctr" pg_restore -h 127.0.0.1 -U postgres -d spawn_drill \
  --no-owner --no-acl --exit-on-error < "$dump" || die "pg_restore ($image) failed"
same_drill_server "the restore"
log "restored into a throwaway $image container ($ctr)"

# 5. compare row counts
count_sql() {
  local sep="" t
  for t in "${TABLES[@]}"; do
    printf "%sselect '%s', (select count(*) from %s)" "$sep" "$t" "$t"
    sep=" union all "
  done
}
restored=$(drill_sql -d spawn_drill -F' ' -c "$(count_sql)") \
  || die "counting rows in the restored database failed (a table is missing?)"
live=$(live_sql -F' ' -c "$(count_sql)") || die "counting rows on live failed"
same_drill_server "the counts"

stamp=${key##*/spawn-}
stamp=${stamp%%.*}
if [[ $stamp =~ ^[0-9]{8}-[0-9]{6}$ ]]; then
  age_s=$(($(date -u +%s) - $(date -u -d "${stamp:0:8} ${stamp:9:2}:${stamp:11:2}:${stamp:13:2}" +%s)))
  log "backup taken $stamp UTC, $((age_s / 3600))h$(((age_s % 3600) / 60))m ago; live counted now"
else
  log "backup time unknown (the name carries no spawn-YYYYMMDD-HHMMSS stamp); live counted now"
fi
printf '%-18s %10s %10s %8s\n' table backup live delta
fail=
for t in "${TABLES[@]}"; do
  b=$(awk -v t="$t" '$1 == t {print $2}' <<<"$restored")
  l=$(awk -v t="$t" '$1 == t {print $2}' <<<"$live")
  printf '%-18s %10s %10s %+8d\n' "$t" "$b" "$l" "$((l - b))"
  if [ "$l" -gt 0 ] && [ "$b" -eq 0 ]; then fail=1; fi
  if [ "$b" -gt "$l" ]; then log "note: $t has fewer rows on live than in the backup (rows deleted since)"; fi
done
if [ -n "$recorded_events" ]; then
  b=$(awk '$1 == "spawn_events" {print $2}' <<<"$restored")
  [ "$b" = "$recorded_events" ] || die "restored $b spawn_events rows, the upload recorded $recorded_events"
fi
[ -z "$fail" ] || die "a table that has rows on live restored empty"
log "ok: $key restores; positive delta = rows written to live since the backup"
