#!/usr/bin/env bash
# Back up Spawn's PostgreSQL event log to Cloudflare R2, encrypted.
#
#   pg-backup.sh               dump, check, encrypt, upload
#   pg-backup.sh --accept-drop upload as good even if spawn_events fell or the
#                              dump shrank (only after you know why)
#   pg-backup.sh --init-key    create the encryption key if it does not exist
#
# Steps; any failure exits non-zero with the reason on stderr:
#   1. pg_dump -Fc of SPAWN_DATABASE_URL (spawn.env) with the pg_dump of the
#      live server's own image (a PostgreSQL 16 dump: pg_restore 16 reads it,
#      a newer pg_dump's archive it cannot); -Fc is already compressed
#   2. pg_restore -l parses it; spawn_events' rows are counted in the dump
#   3. plausibility against the last good upload (pg-backup.state): the log is
#      append-only, so fewer spawn_events rows, or a dump under half the size,
#      means something is wrong. Such a dump goes to postgres/suspect/ only,
#      nothing else is touched, and the run exits 3
#   4. gpg --symmetric AES256 with backup.key; decrypt again, compare SHA-256
#   5. PUT to postgres/daily/, plus postgres/weekly/ for the first good backup
#      of an ISO week and postgres/monthly/ for the first of a month (UTC);
#      each returned ETag must be the MD5 of what was sent
#   6. keep the dump as backups/spawn-auto-<ts>.dump, drop older spawn-auto-*
#   7. record this run in pg-backup.state
# Nothing here deletes from R2: lifecycle rules expire old backups, bucket
# locks make them undeletable until then (see README.md).
set -euo pipefail
umask 077
PROG=pg-backup
. "$(dirname "$(readlink -f "$0")")/lib.sh"

STATE="$SPAWN_BACKUP_DIR/pg-backup.state"
EXIT_SUSPECT=3
PUT_WARN=4000000000  # R2 takes at most 5 GiB in one PUT
PUT_MAX=5000000000

accept_drop=
case "${1-}" in
  "") ;;
  --accept-drop) accept_drop=1 ;;
  --init-key)
    if [ -e "$SPAWN_BACKUP_KEY_FILE" ]; then
      log "$SPAWN_BACKUP_KEY_FILE already exists; not replacing it"
    else
      mkdir -p "$(dirname "$SPAWN_BACKUP_KEY_FILE")"
      head -c 32 /dev/urandom | base64 > "$SPAWN_BACKUP_KEY_FILE"
      chmod 600 "$SPAWN_BACKUP_KEY_FILE"
      log "created $SPAWN_BACKUP_KEY_FILE; copy it somewhere off this host now (without it no backup can be decrypted)"
    fi
    exit 0 ;;
  *) die "usage: $0 [--accept-drop | --init-key]" ;;
esac

need_tools pg_restore gpg curl flock sha256sum md5sum stat date awk
# A host without docker names a native pg_dump of the live server's major
# version (SPAWN_PG_DUMP, e.g. /usr/lib/postgresql/16/bin/pg_dump); otherwise
# the dump runs in the live server's own image, as before.
[ -n "${SPAWN_PG_DUMP-}" ] || need_tools docker
load_live_url
load_r2
check_private "$SPAWN_BACKUP_KEY_FILE"
[ -s "$SPAWN_BACKUP_KEY_FILE" ] || die "$SPAWN_BACKUP_KEY_FILE is empty"

mkdir -p "$SPAWN_BACKUP_DIR"
chmod 700 "$SPAWN_BACKUP_DIR"
exec 9>"$SPAWN_BACKUP_DIR/.pg-backup.lock"
flock -n 9 || die "another pg-backup is running"

work=$(mktemp -d "$SPAWN_BACKUP_DIR/.pg-backup.XXXXXX")
trap 'rm -rf "$work"' EXIT

now=$(date -u +%s)
ts=$(date -u -d "@$now" +%Y%m%d-%H%M%S)
week=$(date -u -d "@$now" +%G-W%V)
month=$(date -u -d "@$now" +%Y-%m)
name="spawn-$ts.dump.gpg"
dump="$work/spawn-$ts.dump"
enc="$dump.gpg"

# 1. dump, with the server's own major version of pg_dump
pg_split "$LIVE_URL"
if [ -n "${SPAWN_PG_DUMP-}" ]; then
  image=$("$SPAWN_PG_DUMP" --version) || die "SPAWN_PG_DUMP $SPAWN_PG_DUMP does not run"
  PGPASSWORD=$PG_PASS PGCONNECT_TIMEOUT=10 "$SPAWN_PG_DUMP" -Fc -w -d "$PG_URL" > "$dump" ||
    die "pg_dump ($image) failed"
else
  image=$(pg_image)
  docker run --rm -i --network host \
    --env-file <(printf 'PGPASSWORD=%s\nPGCONNECT_TIMEOUT=10\n' "$PG_PASS") \
    "$image" pg_dump -Fc -w -d "$PG_URL" > "$dump" || die "pg_dump (in $image) failed"
fi
dump_size=$(stat -c %s "$dump")

# 2. it parses, and how much of the event log it holds
toc=$(pg_restore -l "$dump") || die "pg_restore -l cannot read the dump"
grep -q 'TABLE DATA public spawn_events ' <<<"$toc" || die "the dump has no data for spawn_events"
events=$(pg_restore -a -n public -t spawn_events -f - "$dump" |
  awk '/^COPY public\.spawn_events /{on=1; next} on && /^\\\.$/{on=0} on{n++} END{print n+0}') \
  || die "cannot count spawn_events in the dump"
log "dumped $dump_size bytes with $image; spawn_events: $events rows"

# 3. plausibility against the last good upload
last_events=$(env_value "$STATE" last_events 2>/dev/null || true)
last_size=$(env_value "$STATE" last_size 2>/dev/null || true)
last_key=$(env_value "$STATE" last_key 2>/dev/null || true)
last_week=$(env_value "$STATE" last_week 2>/dev/null || true)
last_month=$(env_value "$STATE" last_month 2>/dev/null || true)
suspect=
if [ -n "$last_events" ] && [ "$events" -lt "$last_events" ]; then
  suspect="spawn_events fell from $last_events rows ($last_key) to $events; the log is append-only"
elif [ -n "$last_size" ] && [ $((dump_size * 2)) -lt "$last_size" ]; then
  suspect="the dump shrank from $last_size bytes ($last_key) to $dump_size, under half"
fi
if [ -n "$suspect" ] && [ -n "$accept_drop" ]; then
  warn "accepted by --accept-drop: $suspect"
  suspect=
fi

# 4. encrypt, and prove it decrypts back to the same bytes
dump_sha=$(sha256sum "$dump" | cut -d' ' -f1)
gpg "${gpg_common[@]}" --passphrase-file "$SPAWN_BACKUP_KEY_FILE" \
  --symmetric --cipher-algo AES256 --compress-algo none -o "$enc" "$dump" \
  || die "gpg encryption failed"
check_sha=$(gpg "${gpg_common[@]}" --passphrase-file "$SPAWN_BACKUP_KEY_FILE" -d "$enc" | sha256sum | cut -d' ' -f1)
[ "$check_sha" = "$dump_sha" ] || die "the encrypted file does not decrypt to the dump"
enc_size=$(stat -c %s "$enc")
enc_md5=$(md5sum "$enc" | cut -d' ' -f1)
[ "$enc_size" -lt "$PUT_MAX" ] || die "the backup is $enc_size bytes, over R2's single-PUT limit (5 GiB); this script needs multipart upload now"
[ "$enc_size" -lt "$PUT_WARN" ] || warn "the backup is $enc_size bytes; R2 refuses a single PUT over 5 GiB, switch to multipart upload soon"

# 5. upload
put() { # put KEY
  local headers="$work/put.headers" body etag
  body=$(s3 -T "$enc" -D "$headers" \
    -H "Content-Type: application/octet-stream" \
    -H "x-amz-meta-sha256: $dump_sha" \
    -H "x-amz-meta-spawn-events: $events" \
    "$(object_url "$1")") || die "upload of $1 failed: $body"
  etag=$(grep -i '^etag:' "$headers" | tail -1 | tr -d '\r"' | sed 's/^[Ee][Tt][Aa][Gg]: *//')
  [ "$etag" = "$enc_md5" ] || die "R2 stored $1 with ETag '$etag', expected the MD5 '$enc_md5' of what was sent"
  log "uploaded s3://$R2_BUCKET/$1 ($enc_size bytes)"
}

if [ -n "$suspect" ]; then
  put "postgres/suspect/$name"
  echo "$PROG: error: SUSPECT: $suspect. Uploaded only to postgres/suspect/$name (expires in 30 days); the good backups and pg-backup.state are untouched. If the drop is intended, run pg-backup.sh --accept-drop." >&2
  exit "$EXIT_SUSPECT"
fi

put "postgres/daily/$name"
[ "$week" = "$last_week" ] || put "postgres/weekly/$name"
[ "$month" = "$last_month" ] || put "postgres/monthly/$name"

# 6. latest local copy (manual dumps in the directory are not touched)
local_copy="$SPAWN_BACKUP_DIR/spawn-auto-$ts.dump"
mv "$dump" "$local_copy"
chmod 600 "$local_copy"
for old in "$SPAWN_BACKUP_DIR"/spawn-auto-*.dump; do
  [ "$old" = "$local_copy" ] || [ ! -e "$old" ] || rm -f -- "$old"
done

# 7. state for the next run's plausibility check
cat > "$work/state" <<EOF
last_key=postgres/daily/$name
last_events=$events
last_size=$dump_size
last_week=$week
last_month=$month
EOF
chmod 600 "$work/state"
mv "$work/state" "$STATE"

log "ok: postgres/daily/$name; $events spawn_events rows; local copy $local_copy"
