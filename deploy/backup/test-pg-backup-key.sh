#!/usr/bin/env bash
# Tests of the key files as pg-backup.sh and lib.sh see them, in a scratch
# HOME with made-up keys: no database, no R2, no network, nothing of the real
# HOME is read.
#
#   deploy/backup/test-pg-backup-key.sh
#
# Cases:
# 1. k1's file by default: backup.key.k1 where it exists, else backup.key;
#    SPAWN_BACKUP_KEY_FILE, when set, is kept.
# 2. --init-key makes the first key of a host that holds none (mode 600), and
#    refuses, changing nothing, when any key file exists: the same file, a
#    renamed k1, a k2, a stray copy.
# 3. a run that would make a backup refuses on a host that holds a key file
#    other than k1's, before it reads a setting; without one it goes on to
#    its settings (which this test leaves missing).
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

# 3. A run that would make a backup.
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

[ "$failed" = 0 ] || exit 1
echo "test-pg-backup-key: all passed"
