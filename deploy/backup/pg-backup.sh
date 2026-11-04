#!/usr/bin/env bash
# Back up Spawn's PostgreSQL event log to Cloudflare R2, encrypted.
#
#   pg-backup.sh             dump, verify, encrypt, upload, prune
#   pg-backup.sh --init-key  create the encryption key if it does not exist
#
# Steps, any failure exits non-zero with the reason on stderr:
#   1. pg_dump -Fc of SPAWN_DATABASE_URL (spawn.env); -Fc is already
#      compressed, nothing compresses it again
#   2. pg_restore -l on the dump: it must parse and hold spawn_events' data
#   3. gpg --symmetric AES256 with the key in backup.key; decrypt it again and
#      compare SHA-256 with the dump
#   4. PUT to R2 as postgres/spawn-YYYYMMDD-HHMMSS.dump.gpg (UTC), the
#      dump's SHA-256 in x-amz-meta-sha256; the returned ETag must be the
#      MD5 of what was sent
#   5. keep the dump as backups/spawn-auto-<ts>.dump, drop older spawn-auto-*
#   6. retention, from one ListObjectsV2 call: keep the newest object of each
#      of the newest 14 days and of each of the newest 8 ISO weeks; delete the
#      rest. Objects whose names do not match the pattern are left alone.
set -euo pipefail
umask 077
PROG=pg-backup
. "$(dirname "$(readlink -f "$0")")/lib.sh"

KEEP_DAILY=${SPAWN_BACKUP_KEEP_DAILY:-14}
KEEP_WEEKLY=${SPAWN_BACKUP_KEEP_WEEKLY:-8}

if [ "${1-}" = --init-key ]; then
  if [ -e "$SPAWN_BACKUP_KEY_FILE" ]; then
    log "$SPAWN_BACKUP_KEY_FILE already exists; not replacing it"
  else
    mkdir -p "$(dirname "$SPAWN_BACKUP_KEY_FILE")"
    head -c 32 /dev/urandom | base64 > "$SPAWN_BACKUP_KEY_FILE"
    chmod 600 "$SPAWN_BACKUP_KEY_FILE"
    log "created $SPAWN_BACKUP_KEY_FILE; copy it somewhere off this host now (without it no backup can be decrypted)"
  fi
  exit 0
fi
[ $# -eq 0 ] || die "usage: $0 [--init-key]"

need_tools pg_dump pg_restore gpg curl flock sha256sum md5sum stat date
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

ts=$(date -u +%Y%m%d-%H%M%S)
key="${R2_PREFIX}spawn-$ts.dump.gpg"
dump="$work/spawn-$ts.dump"
enc="$dump.gpg"

# 1. dump
with_db "$LIVE_URL" pg_dump -Fc -f "$dump" || die "pg_dump failed"
dump_size=$(stat -c %s "$dump")
log "dumped $dump_size bytes"

# 2. the archive parses and carries the event log
toc=$(pg_restore -l "$dump") || die "pg_restore -l cannot read the dump"
grep -q 'TABLE DATA public spawn_events ' <<<"$toc" || die "the dump has no data for spawn_events"
dump_sha=$(sha256sum "$dump" | cut -d' ' -f1)

# 3. encrypt, and prove it decrypts back to the same bytes
gpg "${gpg_common[@]}" --passphrase-file "$SPAWN_BACKUP_KEY_FILE" \
  --symmetric --cipher-algo AES256 --compress-algo none -o "$enc" "$dump" \
  || die "gpg encryption failed"
check_sha=$(gpg "${gpg_common[@]}" --passphrase-file "$SPAWN_BACKUP_KEY_FILE" -d "$enc" | sha256sum | cut -d' ' -f1)
[ "$check_sha" = "$dump_sha" ] || die "the encrypted file does not decrypt to the dump"
enc_size=$(stat -c %s "$enc")
enc_md5=$(md5sum "$enc" | cut -d' ' -f1)

# 4. upload
headers="$work/put.headers"
body=$(s3 -T "$enc" -D "$headers" \
  -H "Content-Type: application/octet-stream" \
  -H "x-amz-meta-sha256: $dump_sha" \
  "$(object_url "$key")") || die "upload of $key failed: $body"
etag=$(grep -i '^etag:' "$headers" | tail -1 | tr -d '\r"' | sed 's/^[Ee][Tt][Aa][Gg]: *//')
[ "$etag" = "$enc_md5" ] || die "R2 stored $key with ETag '$etag', expected the MD5 '$enc_md5' of what was sent"
log "uploaded s3://$R2_BUCKET/$key ($enc_size bytes, sha256 of dump $dump_sha)"

# 5. latest local copy (manual dumps in the directory are not touched)
local_copy="$SPAWN_BACKUP_DIR/spawn-auto-$ts.dump"
mv "$dump" "$local_copy"
chmod 600 "$local_copy"
for old in "$SPAWN_BACKUP_DIR"/spawn-auto-*.dump; do
  [ "$old" = "$local_copy" ] || [ ! -e "$old" ] || rm -f -- "$old"
done
log "local copy $local_copy"

# 6. retention
listing=$(list_backups)
grep -q "^$key $enc_size\$" <<<"$listing" || die "$key ($enc_size bytes) is not in the bucket listing after upload"

declare -A keep=()
kept=$(retention_keep "$KEEP_DAILY" "$KEEP_WEEKLY" <<<"$listing") || die "retention selection failed"
while read -r k; do
  [ -z "$k" ] || keep[$k]=1
done <<<"$kept"
[ -n "${keep[$key]-}" ] || die "retention would delete the backup just made; not pruning"

deleted=0 total=0
while read -r k _; do
  if ! [[ $k =~ $R2_KEY_RE ]]; then
    [ -z "$k" ] || log "leaving unrecognised object alone: $k"
    continue
  fi
  total=$((total + 1))
  [ -z "${keep[$k]-}" ] || continue
  out=$(s3 -X DELETE "$(object_url "$k")") || die "deleting $k failed: $out"
  log "pruned $k"
  deleted=$((deleted + 1))
done <<<"$listing"

log "ok: $key; ${#keep[@]} kept, $deleted pruned, of $total backups in the bucket"
