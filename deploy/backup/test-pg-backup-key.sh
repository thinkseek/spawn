#!/usr/bin/env bash
# Tests of the key files as pg-backup.sh and lib.sh see them, in a scratch
# HOME with made-up keys: no database, no R2, no network, nothing of the real
# HOME is read.
#
#   deploy/backup/test-pg-backup-key.sh
#
# Cases:
# 1. k1's file by default: backup.key.k1 in the key directory
#    (SPAWN_BACKUP_KEY_DIR, by default ~/.config/spawn) where it exists, else
#    backup.key there; SPAWN_BACKUP_KEY_FILE, when set, is kept.
# 2. --init-key makes the first key of a host that holds none (mode 600), and
#    refuses, changing nothing, when any key file exists in the key directory
#    or beside k1's file: the same file, a renamed k1, a k2, a stray copy. It
#    creates its file or fails: it writes through no link.
# 3. a run that would make a backup refuses on a host that holds a key file
#    other than k1's, in the key directory or beside k1's file, before it
#    reads a setting; without one it goes on to its settings (which this test
#    leaves missing).
set -euo pipefail
umask 077
here=$(cd "$(dirname "$0")" && pwd)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failed=0
ok() { echo "ok: $*"; }
bad() { echo "FAILED: $*" >&2; failed=1; }

# run HOME [NAME=VALUE ...] -- ARGS: pg-backup.sh in a clean environment whose
# settings files do not exist; its output in $work/out, its status in $status.
run() {
  local home=$1; shift
  local -a vars=()
  while [ "$1" != -- ]; do vars+=("$1"); shift; done
  shift
  status=0
  env -i PATH="$PATH" HOME="$home" SPAWN_ENV_FILE="$work/no-spawn.env" \
    SPAWN_R2_ENV_FILE="$work/no-r2.env" SPAWN_BACKUP_DIR="$home/backups" "${vars[@]}" \
    bash "$here/pg-backup.sh" "$@" >"$work/out" 2>&1 || status=$?
}

# default_file HOME [NAME=VALUE]: the path lib.sh takes for k1's file.
default_file() {
  local home=$1; shift
  env -i PATH="$PATH" HOME="$home" "$@" bash -c '. "$1"; printf "%s" "$SPAWN_BACKUP_KEY_FILE"' _ "$here/lib.sh"
}

# listing DIR: names, sizes and content hashes of what the key directory holds.
listing() {
  [ -d "$1" ] || { echo none; return 0; }
  (cd "$1" && find . -type f | LC_ALL=C sort | while IFS= read -r f; do
    printf '%s %s %s\n' "$f" "$(stat -c '%a %s' "$f")" "$(sha256sum < "$f" | cut -c1-16)"
  done)
}

made_up() { printf 'made-up-key-%s\n' "$1" > "$2"; chmod 600 "$2"; }

# 1. k1's file by default.
h=$work/h1; mkdir -p "$h/.config/spawn"
[ "$(default_file "$h")" = "$h/.config/spawn/backup.key" ] && ok "no key file: the default is backup.key" || bad "default with no key file"
made_up a "$h/.config/spawn/backup.key"
[ "$(default_file "$h")" = "$h/.config/spawn/backup.key" ] && ok "backup.key only: it is k1's file" || bad "default with backup.key only"
made_up a "$h/.config/spawn/backup.key.k1"
[ "$(default_file "$h")" = "$h/.config/spawn/backup.key.k1" ] && ok "backup.key.k1 is taken before backup.key" || bad "default with backup.key.k1"
[ "$(default_file "$h" SPAWN_BACKUP_KEY_FILE="$work/elsewhere.key")" = "$work/elsewhere.key" ] && ok "SPAWN_BACKUP_KEY_FILE is kept" || bad "an explicit key file"

[ "$(default_file "$h" SPAWN_BACKUP_KEY_DIR="$work/kd")" = "$work/kd/backup.key" ] && ok "SPAWN_BACKUP_KEY_DIR with no key file: backup.key in it" || bad "default in a named key directory"
mkdir -p "$work/kd"; made_up a "$work/kd/backup.key"; made_up a "$work/kd/backup.key.k1"
[ "$(default_file "$h" SPAWN_BACKUP_KEY_DIR="$work/kd")" = "$work/kd/backup.key.k1" ] && ok "SPAWN_BACKUP_KEY_DIR: backup.key.k1 in it is taken before backup.key" || bad "default in a named key directory with backup.key.k1"

# 2. --init-key.
h=$work/h2; mkdir -p "$h"
run "$h" -- --init-key
f=$h/.config/spawn/backup.key
if [ "$status" = 0 ] && [ -f "$f" ] && [ "$(stat -c '%a %s' "$f")" = "600 45" ]; then
  ok "--init-key on a host with no key: backup.key, mode 600, 32 random bytes in base64"
else
  bad "--init-key on a host with no key (status $status)"
fi
before=$(listing "$h/.config/spawn")
run "$h" -- --init-key
if [ "$status" = 1 ] && grep -q 'makes the first key only' "$work/out" && [ "$(listing "$h/.config/spawn")" = "$before" ]; then
  ok "--init-key again: refused, the key unchanged"
else
  bad "--init-key on its own key (status $status)"
fi

for held in "backup.key.k1" "backup.key.k1 backup.key.k2" "backup.key.k2" "backup.key.copy"; do
  h=$work/h2-$(printf '%s' "$held" | tr -c 'a-z0-9.' -); mkdir -p "$h/.config/spawn"
  for name in $held; do made_up "$name" "$h/.config/spawn/$name"; done
  before=$(listing "$h/.config/spawn")
  run "$h" -- --init-key
  if [ "$status" = 1 ] && grep -q 'makes the first key only' "$work/out" && [ "$(listing "$h/.config/spawn")" = "$before" ]; then
    ok "--init-key beside $held: refused, nothing created"
  else
    bad "--init-key beside $held (status $status)"
  fi
done

h=$work/h2-explicit; mkdir -p "$h/keys"; made_up b "$h/keys/backup.key.k2"
before=$(listing "$h/keys")
run "$h" SPAWN_BACKUP_KEY_FILE="$h/keys/mine.key" -- --init-key
if [ "$status" = 1 ] && [ "$(listing "$h/keys")" = "$before" ]; then
  ok "--init-key with SPAWN_BACKUP_KEY_FILE beside a k2: refused, nothing created"
else
  bad "--init-key with an explicit file beside a k2 (status $status)"
fi

h=$work/h2-keydir; mkdir -p "$h/.config/spawn" "$h/keys"; made_up b "$h/.config/spawn/backup.key.k2"
before=$(listing "$h")
run "$h" SPAWN_BACKUP_KEY_FILE="$h/keys/backup.key" -- --init-key
if [ "$status" = 1 ] && grep -q 'makes the first key only' "$work/out" && [ "$(listing "$h")" = "$before" ]; then
  ok "--init-key with SPAWN_BACKUP_KEY_FILE elsewhere and a k2 in the key directory: refused, nothing created"
else
  bad "--init-key with an explicit file and a k2 in the key directory (status $status)"
fi

# A link to nothing under a key file's name is a key file to the check; under
# any other name only the write itself can refuse it.
h=$work/h2-link; mkdir -p "$h/.config/spawn" "$h/target"
ln -s "$h/target/made-through-a-link" "$h/.config/spawn/backup.key"
run "$h" -- --init-key
if [ "$status" = 1 ] && grep -q 'makes the first key only' "$work/out" && [ -L "$h/.config/spawn/backup.key" ] && [ -z "$(ls -A "$h/target")" ]; then
  ok "--init-key where backup.key is a link to nothing: refused, nothing made through the link"
else
  bad "--init-key where backup.key is a dangling link (status $status)"
fi
h=$work/h2-link-named; mkdir -p "$h/keys" "$h/target"
ln -s "$h/target/made-through-a-link" "$h/keys/first.key"
run "$h" SPAWN_BACKUP_KEY_FILE="$h/keys/first.key" -- --init-key
if [ "$status" = 1 ] && grep -q 'as a new file' "$work/out" && [ -L "$h/keys/first.key" ] && [ -z "$(ls -A "$h/target")" ]; then
  ok "--init-key where SPAWN_BACKUP_KEY_FILE is a link to nothing: the write refuses, nothing made through the link"
else
  bad "--init-key through a dangling link named by SPAWN_BACKUP_KEY_FILE (status $status)"
fi

# 3. A run that would make a backup.
h=$work/h3-keydir; mkdir -p "$h/.config/spawn" "$h/keys"
made_up b "$h/.config/spawn/backup.key.k2"; made_up a "$h/keys/backup.key"
run "$h" SPAWN_BACKUP_KEY_FILE="$h/keys/backup.key" --
if [ "$status" = 1 ] && grep -q "other than k1's" "$work/out" && [ ! -e "$h/backups" ]; then
  ok "a backup run with SPAWN_BACKUP_KEY_FILE elsewhere and a k2 in the key directory: refused"
else
  bad "a backup run with an explicit file and a k2 in the key directory (status $status)"
fi
h=$work/h3-named; mkdir -p "$h/kd"; made_up a "$h/kd/backup.key.k1"; made_up b "$h/kd/backup.key.k2"
run "$h" SPAWN_BACKUP_KEY_DIR="$h/kd" --
if [ "$status" = 1 ] && grep -q "other than k1's" "$work/out" && [ ! -e "$h/backups" ]; then
  ok "a backup run with SPAWN_BACKUP_KEY_DIR holding k1 and k2: refused"
else
  bad "a backup run in a named key directory with a k2 (status $status)"
fi
h=$work/h3-named-k1; mkdir -p "$h/kd"; made_up a "$h/kd/backup.key.k1"
run "$h" SPAWN_BACKUP_KEY_DIR="$h/kd" --
if [ "$status" != 0 ] && ! grep -q "other than k1's" "$work/out" && [ ! -e "$h/backups" ]; then
  ok "a backup run with SPAWN_BACKUP_KEY_DIR holding k1 only: not refused for its keys"
else
  bad "a backup run in a named key directory with k1 only (status $status)"
fi
for held in "backup.key.k1 backup.key.k2" "backup.key backup.key.k3" "backup.key.k1 backup.key.k1.copy"; do
  h=$work/h3-$(printf '%s' "$held" | tr -c 'a-z0-9.' -); mkdir -p "$h/.config/spawn"
  for name in $held; do made_up "$name" "$h/.config/spawn/$name"; done
  run "$h" --
  if [ "$status" = 1 ] && grep -q "other than k1's" "$work/out" && [ ! -e "$h/backups" ]; then
    ok "a backup run beside $held: refused before anything is read or made"
  else
    bad "a backup run beside $held (status $status)"
  fi
done
for held in "backup.key" "backup.key.k1" "backup.key backup.key.k1"; do
  h=$work/h3-$(printf '%s' "$held" | tr -c 'a-z0-9.' -); mkdir -p "$h/.config/spawn"
  for name in $held; do made_up "$name" "$h/.config/spawn/$name"; done
  run "$h" --
  if [ "$status" != 0 ] && ! grep -q "other than k1's" "$work/out" && [ ! -e "$h/backups" ]; then
    ok "a backup run beside $held: not refused for its keys (it stops at its missing settings)"
  else
    bad "a backup run beside $held (status $status)"
  fi
done

# 4. Keep Wave K3 CREDENTIALS_DIRECTORY precedence, mode 400/600, fail-closed checks, and fallback.
cdir=$work/creds; mkdir -p "$cdir"
h4=$work/h4; mkdir -p "$h4/.config/spawn" "$h4/.local/share/spawn-rsi"
made_up disk "$h4/.config/spawn/backup.key.k1"
[ "$(default_file "$h4" CREDENTIALS_DIRECTORY="$cdir")" = "$h4/.config/spawn/backup.key.k1" ] &&
  ok "CREDENTIALS_DIRECTORY without key files falls back to disk backup.key.k1" || bad "cred fallback key"

printf 'made-up-keep-enc\n' >"$cdir/backup.encryption-key"; chmod 400 "$cdir/backup.encryption-key"
[ "$(default_file "$h4" CREDENTIALS_DIRECTORY="$cdir")" = "$cdir/backup.encryption-key" ] &&
  ok "CREDENTIALS_DIRECTORY/backup.encryption-key (0400) chosen when backup.key.k1 absent" || bad "cred alias key"

printf 'made-up-keep-k1\n' >"$cdir/backup.key.k1"; chmod 400 "$cdir/backup.key.k1"
[ "$(default_file "$h4" CREDENTIALS_DIRECTORY="$cdir")" = "$cdir/backup.key.k1" ] &&
  ok "CREDENTIALS_DIRECTORY/backup.key.k1 overrides backup.encryption-key and disk" || bad "cred k1 key"

r2_legacy=$h4/.config/spawn/r2.env
cat >"$r2_legacy" <<'EOF'
SPAWN_R2_ENDPOINT=https://legacy.example
SPAWN_R2_BUCKET=legacy-bkt
SPAWN_R2_ACCESS_KEY_ID=leg-id
SPAWN_R2_SECRET_ACCESS_KEY=leg-sec
EOF
chmod 600 "$r2_legacy"

cred_r2=$cdir/r2.credentials
cat >"$cred_r2" <<'EOF'
SPAWN_R2_ENDPOINT=https://keep.example/
SPAWN_R2_BUCKET=keep-bkt
SPAWN_R2_ACCESS_KEY_ID=keep-id
SPAWN_R2_SECRET_ACCESS_KEY=keep-sec
EOF
chmod 400 "$cred_r2"
if env -i PATH="$PATH" HOME="$h4" CREDENTIALS_DIRECTORY="$cdir" SPAWN_R2_ENV_FILE="$r2_legacy" \
  bash -c '. "$1"; PROG=test; load_r2; [ "$R2_BUCKET" = "keep-bkt" ] && [ "$R2_ENDPOINT" = "https://keep.example" ] && [ "$R2_ACCESS_KEY_ID" = "keep-id" ] && [ "$R2_SECRET_ACCESS_KEY" = "keep-sec" ]' _ "$here/lib.sh"; then
  ok "load_r2 prefers CREDENTIALS_DIRECTORY/r2.credentials (0400) over legacy r2.env"
else
  bad "load_r2 Keep precedence"
fi

chmod 644 "$cred_r2"
if ! env -i PATH="$PATH" HOME="$h4" CREDENTIALS_DIRECTORY="$cdir" SPAWN_R2_ENV_FILE="$r2_legacy" \
  bash -c '. "$1"; PROG=test; load_r2' _ "$here/lib.sh" 2>"$work/r2err"; then
  ok "load_r2 fails closed when r2.credentials has mode 644 (no r2.env fallback)"
else
  bad "load_r2 should fail closed on mode 644"
fi
chmod 400 "$cred_r2"

cred_db=$cdir/spawn.database-url
printf 'postgres://keep@127.0.0.1/spawn_k\n' >"$cred_db"; chmod 400 "$cred_db"
if env -i PATH="$PATH" HOME="$h4" CREDENTIALS_DIRECTORY="$cdir" \
  bash -c '. "$1"; PROG=test; load_live_url; [ "$LIVE_URL" = "postgres://keep@127.0.0.1/spawn_k" ]' _ "$here/lib.sh"; then
  ok "load_live_url reads single-line CREDENTIALS_DIRECTORY/spawn.database-url (0400)"
else
  bad "load_live_url Keep read"
fi

chmod 600 "$cred_db"
printf 'postgres://one@127.0.0.1/db\npostgres://two@127.0.0.1/db\n' >"$cred_db" && chmod 400 "$cred_db"
if ! env -i PATH="$PATH" HOME="$h4" CREDENTIALS_DIRECTORY="$cdir" \
  bash -c '. "$1"; PROG=test; load_live_url' _ "$here/lib.sh" 2>"$work/dberr"; then
  ok "load_live_url fails closed on multi-line spawn.database-url"
else
  bad "load_live_url should fail closed on multi-line file"
fi

ln -sf "$h4/.config/spawn/backup.key.k1" "$cdir/sym-db"
rm -f "$cred_db" && mv "$cdir/sym-db" "$cred_db"
if ! env -i PATH="$PATH" HOME="$h4" CREDENTIALS_DIRECTORY="$cdir" \
  bash -c '. "$1"; PROG=test; load_live_url' _ "$here/lib.sh" 2>"$work/symerr"; then
  ok "load_live_url fails closed on symlink credential"
else
  bad "load_live_url should fail closed on symlink"
fi

[ "$failed" = 0 ] || exit 1
echo "test-pg-backup-key: all passed"
