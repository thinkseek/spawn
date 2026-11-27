#!/usr/bin/env bash
# Smoke tests for scripts/fleet-sync-node.sh. Everything happens in a
# temporary directory: fake artifacts, a fake systemctl that only logs, and
# health commands that read files there. Nothing on the host changes.
#
#   bash scripts/test-fleet-sync-node.sh

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
sync="$here/fleet-sync-node.sh"
root=$(mktemp -d "${TMPDIR:-/tmp}/test-fleet-sync-node.XXXXXX")
trap 'rm -rf "$root"' EXIT

pass=0
fail=0
ok() { pass=$((pass + 1)); printf 'ok   %s\n' "$1"; }
bad() { fail=$((fail + 1)); printf 'FAIL %s\n' "$1"; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

if command -v sha256sum >/dev/null 2>&1; then
  sum() { sha256sum "$1" | cut -d' ' -f1; }
else
  sum() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi

spawn_sha=$(printf 'a%.0s' {1..40})
node_sha=$(printf 'b%.0s' {1..40})

# make_bin <path> <version>: a fake binary answering --version.
make_bin() {
  printf '#!/bin/sh\n[ "$1" = --version ] && { echo "%s"; exit 0; }\nexit 0\n' "$2" > "$1"
  chmod 0755 "$1"
}

# A fake systemctl: logs its arguments.
fake_systemctl="$root/systemctl"
printf '#!/bin/sh\necho "$*" >> "%s/systemctl.log"\n' "$root" > "$fake_systemctl"
chmod 0755 "$fake_systemctl"

# Artifacts for build v2, and its manifest.
art="$root/artifacts"
mkdir -p "$art/web"
make_bin "$art/spawn-node" v2-node
make_bin "$art/spawn" v2-spawn
make_bin "$art/spawn-hc" v2-hc
echo v2 > "$art/web/index.html"
tar -czf "$art/web.tar.gz" -C "$art/web" .
manifest="$root/manifest"
{
  echo "# test manifest"
  echo "spawn_sha=$spawn_sha"
  echo "node_sha=$node_sha"
  echo "artifacts=$art"
  for f in spawn-node spawn spawn-hc web.tar.gz; do printf '%s  %s\n' "$(sum "$art/$f")" "$f"; done
} > "$manifest"

new_bin_dir() {
  local d="$root/$1/bin"
  mkdir -p "$d"
  make_bin "$d/spawn-node" v1-node
  printf '%s' "$d"
}

run_sync() {
  set +e
  out=$(bash "$sync" --systemctl "$fake_systemctl" --health-interval 0 --health-timeout 5 "$@" 2>&1)
  rc=$?
  set -e
}

# 1. --dry-run verifies and changes nothing.
bin=$(new_bin_dir dry)
before=$(sum "$bin/spawn-node")
: > "$root/systemctl.log"
run_sync --dry-run --manifest "$manifest" --bin-dir "$bin"
check "dry-run exits 0" '[ "$rc" = 0 ]'
check "dry-run says nothing changed" 'grep -q "dry run: verified" <<<"$out"'
check "dry-run leaves the live binary" '[ "$(sum "$bin/spawn-node")" = "$before" ]'
check "dry-run writes no .staged/.prev/state" '[ "$(ls -A "$bin")" = spawn-node ]'
check "dry-run restarts nothing" '[ ! -s "$root/systemctl.log" ]'

# 2. SHA-256 mismatch is refused before any change (fail closed).
bin=$(new_bin_dir mismatch)
bad_manifest="$root/manifest.bad"
sed "s/^[0-9a-f]\{64\}  spawn-node$/$(printf '0%.0s' {1..64})  spawn-node/" "$manifest" > "$bad_manifest"
: > "$root/systemctl.log"
run_sync --manifest "$bad_manifest" --bin-dir "$bin"
check "mismatch exits 1" '[ "$rc" = 1 ]'
check "mismatch is named" 'grep -q "sha256 mismatch for spawn-node" <<<"$out"'
check "mismatch leaves the live binary" '[ "$(sum "$bin/spawn-node")" = "$before" ]'
check "mismatch leaves no .staged/.prev/lock" '[ "$(ls -A "$bin")" = spawn-node ]'
check "mismatch restarts nothing" '[ ! -s "$root/systemctl.log" ]'

# 2b. A manifest line that does not parse is refused.
garbled="$root/manifest.garbled"
{ cat "$manifest"; echo "this is not a checksum"; } > "$garbled"
run_sync --dry-run --manifest "$garbled" --bin-dir "$bin"
check "unparsable manifest exits 1" '[ "$rc" = 1 ] && grep -q "does not parse" <<<"$out"'

# 2c. A missing checksum entry is refused.
partial="$root/checksums.partial"
grep -v ' spawn-node$' "$manifest" | grep -E '^[0-9a-f]{64} ' > "$partial"
run_sync --dry-run --spawn-sha "$spawn_sha" --node-sha "$node_sha" --artifacts "$art" \
  --checksums "$partial" --bin-dir "$bin"
check "missing checksum entry exits 1" '[ "$rc" = 1 ] && grep -q "no checksum listed for spawn-node" <<<"$out"'

# 2d. A staged binary failing its smoke run is refused.
broken="$root/broken"
mkdir -p "$broken"
printf '#!/bin/sh\nexit 7\n' > "$broken/spawn-node"
run_sync --spawn-sha "$spawn_sha" --node-sha "$node_sha" --artifacts "$broken" --bin-dir "$bin"
check "failing smoke run exits 1" '[ "$rc" = 1 ] && grep -q "smoke run" <<<"$out"'
check "failing smoke run leaves the live binary" '[ "$(sum "$bin/spawn-node")" = "$before" ] && [ "$(ls -A "$bin")" = spawn-node ]'

# 3. Atomic swap with .prev preserved, units restarted, health passes, state written.
bin=$(new_bin_dir swap)
: > "$root/systemctl.log"
run_sync --manifest "$manifest" --bin-dir "$bin" --units "a.service b.service" \
  --health-cmd "true"
check "swap exits 0" '[ "$rc" = 0 ]'
check "live binary is the new build" '[ "$("$bin/spawn-node" --version)" = v2-node ]'
check ".prev is the old build" '[ "$("$bin/spawn-node.prev" --version)" = v1-node ]'
check "no .staged left" '! ls "$bin"/*.staged >/dev/null 2>&1'
check "units restarted" 'grep -qx "restart -- a.service b.service" "$root/systemctl.log"'
check "state file records the SHAs" 'grep -q "spawn_sha=$spawn_sha node_sha=$node_sha mode=node" "$bin/.fleet-sync.state"'
run_sync --manifest "$manifest" --bin-dir "$bin" --units "a.service"
check "second run is already current" '[ "$rc" = 0 ] && grep -q "already current" <<<"$out"'

# 3b. Full mode installs spawn, spawn-hc and web; web.prev kept.
bin=$(new_bin_dir full)
web="$root/full/web"
mkdir -p "$web"
echo v1 > "$web/index.html"
run_sync --mode full --manifest "$manifest" --bin-dir "$bin" --web-dir "$web" --units "" \
  --health-cmd "true"
check "full mode exits 0" '[ "$rc" = 0 ]'
check "full mode installs spawn and spawn-hc" '[ "$("$bin/spawn" --version)" = v2-spawn ] && [ "$("$bin/spawn-hc" --version)" = v2-hc ]'
check "full mode swaps web and keeps web.prev" 'grep -qx v2 "$web/index.html" && grep -qx v1 "$web.prev/index.html"'

# 4. Failing health check rolls back to .prev and restarts again.
bin=$(new_bin_dir rollback)
web="$root/rollback/web"
mkdir -p "$web"
echo v1 > "$web/index.html"
: > "$root/systemctl.log"
# Healthy only while the live spawn-node is v1.
health="test \"\$('$bin/spawn-node' --version)\" = v1-node"
run_sync --mode full --manifest "$manifest" --bin-dir "$bin" --web-dir "$web" \
  --units "a.service" --health-cmd "$health" --health-retries 2
check "rollback exits 3" '[ "$rc" = 3 ]'
check "rollback restores the old binary" '[ "$("$bin/spawn-node" --version)" = v1-node ]'
check "rollback keeps .prev" '[ "$("$bin/spawn-node.prev" --version)" = v1-node ]'
check "rollback removes first installs" '[ ! -e "$bin/spawn" ] && [ ! -e "$bin/spawn-hc" ]'
check "rollback restores web" 'grep -qx v1 "$web/index.html"'
check "rollback restarted twice" '[ "$(grep -c "restart -- a.service" "$root/systemctl.log")" = 2 ]'
check "rollback writes no state" '[ ! -e "$bin/.fleet-sync.state" ]'
check "rollback releases the lock" '[ ! -e "$bin/.fleet-sync.lock" ]'

# 5. A health check that hangs is bounded by --health-timeout.
bin=$(new_bin_dir hang)
start=$(date +%s)
run_sync --manifest "$manifest" --bin-dir "$bin" --units "" --health-cmd "sleep 30" \
  --health-retries 1 --health-timeout 1
elapsed=$(($(date +%s) - start))
check "hanging health check times out and rolls back" '[ "$rc" = 4 ] || [ "$rc" = 3 ]'
check "hanging health check is bounded" '[ "$elapsed" -lt 20 ]'

# 6. Usage errors.
run_sync --manifest "$manifest" --spawn-sha "$spawn_sha"
check "manifest with --spawn-sha is a usage error" '[ "$rc" = 2 ]'
run_sync --mode weird --manifest "$manifest"
check "bad mode is a usage error" '[ "$rc" = 2 ]'

# 7. The script prints no URL query strings or userinfo.
run_sync --dry-run --spawn-sha "$spawn_sha" --node-sha "$node_sha" \
  --artifacts "https://user:pw@example.invalid/a?token=zzz" --bin-dir "$bin"
check "URL secrets are not printed" '! grep -q -e "pw@" -e "token=zzz" <<<"$out"'

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" = 0 ]
