#!/usr/bin/env bash
# Smoke tests for scripts/fleet-build-manifest.sh, paired with
# scripts/fleet-sync-node.sh: every manifest built here is read back by
# `fleet-sync-node.sh --dry-run --manifest`, and one is installed for real
# into a scratch bin dir. Everything happens in a temporary directory: fake
# binaries, a fake systemctl that only logs. Nothing on the host changes.
#
#   bash scripts/test-fleet-build-manifest.sh

# Checks are strings eval'd by check(), so their $vars expand there, late.
# shellcheck disable=SC2016,SC2034

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
build="$here/fleet-build-manifest.sh"
sync="$here/fleet-sync-node.sh"
root=$(mktemp -d "${TMPDIR:-/tmp}/test-fleet-build-manifest.XXXXXX")
trap 'rm -rf "$root"' EXIT

pass=0
fail=0
ok() { pass=$((pass + 1)); printf 'ok   %s\n' "$1"; }
bad() { fail=$((fail + 1)); printf 'FAIL %s\n' "$1"; [ -z "${out:-}" ] || printf '%s\n' "$out" | sed 's/^/     | /'; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

if command -v sha256sum >/dev/null 2>&1; then
  sum() { sha256sum "$1" | cut -d' ' -f1; }
else
  sum() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi

spawn_sha=$(printf 'c%.0s' {1..40})
node_sha=$(printf 'd%.0s' {1..40})

# make_bin <path> <version>: a fake binary answering --version.
make_bin() {
  printf '#!/bin/sh\n[ "$1" = --version ] && { echo "%s"; exit 0; }\nexit 0\n' "$2" > "$1"
  chmod 0755 "$1"
}

fake_systemctl="$root/systemctl"
printf '#!/bin/sh\necho "$*" >> "%s/systemctl.log"\n' "$root" > "$fake_systemctl"
chmod 0755 "$fake_systemctl"

# The build's outputs: binaries (deliberately not executable, to show the
# staged copies are) and a web UI tree.
bins="$root/target/release"
web="$root/web/dist"
mkdir -p "$bins" "$web/assets"
make_bin "$bins/spawn-node" v3-node
make_bin "$bins/spawn" v3-spawn
make_bin "$bins/spawn-hc" v3-hc
chmod 0644 "$bins/spawn-node"
echo '<html>v3</html>' > "$web/index.html"
echo 'console.log(3)' > "$web/assets/app.js"

run_build() {
  set +e
  out=$(bash "$build" "$@" 2>&1)
  rc=$?
  set -e
}
run_sync() {
  set +e
  out=$(bash "$sync" --systemctl "$fake_systemctl" --health-interval 0 --health-timeout 5 "$@" 2>&1)
  rc=$?
  set -e
}
# manifest_line <manifest> <artifact>: the artifact's sha256 in the manifest.
manifest_sum() { awk -v n="$2" '$2 == n { print $1 }' "$1"; }

# 1. Node mode: spawn-node alone, exact checksums, the manifest round-trips.
out1="$root/out-node"
run_build --spawn-sha "$spawn_sha" --node-sha "$node_sha" --bin-dir "$bins" --out-dir "$out1"
check "node: build exits 0" '[ "$rc" = 0 ]'
m="$out1/release.manifest"
check "node: manifest written, no temporary left" '[ -f "$m" ] && [ ! -e "$m.tmp" ]'
check "node: out dir holds spawn-node and the manifest only" \
  '[ "$(ls -A "$out1" | sort | tr "\n" " ")" = "release.manifest spawn-node " ]'
check "node: staged spawn-node is executable" '[ -x "$out1/spawn-node" ]'
check "node: spawn_sha line" 'grep -qx "spawn_sha=$spawn_sha" "$m"'
check "node: node_sha line" 'grep -qx "node_sha=$node_sha" "$m"'
check "node: artifacts line is the absolute out dir" \
  'grep -qx "artifacts=$(cd "$out1" && pwd -P)" "$m"'
check "node: checksum equals sha256 of the staged file" \
  '[ "$(manifest_sum "$m" spawn-node)" = "$(sum "$out1/spawn-node")" ]'
check "node: checksum equals sha256 of the input binary" \
  '[ "$(manifest_sum "$m" spawn-node)" = "$(sum "$bins/spawn-node")" ]'
check "node: no full-mode artifacts listed" '! grep -Eq "  (spawn|spawn-hc|web\.tar\.gz)$" "$m"'
check "node: every line is one fleet-sync-node.sh parses" \
  '! grep -Ev "^(#.*|spawn_sha=[0-9a-f]{40}|node_sha=[0-9a-f]{40}|artifacts=.+|[0-9a-f]{64}  [A-Za-z0-9._-]+)$" "$m"'
sync_bin="$root/sync-node/bin"
mkdir -p "$sync_bin"
run_sync --dry-run --manifest "$m" --bin-dir "$sync_bin"
check "node: fleet-sync-node.sh --dry-run accepts the manifest" '[ "$rc" = 0 ]'
check "node: dry run verified the checksums" \
  'grep -q "checksums: verified" <<<"$out" && grep -q "dry run: verified" <<<"$out"'
check "node: dry run names both SHAs" 'grep -q "spawn=$spawn_sha node=$node_sha" <<<"$out"'
check "node: dry run changed nothing" '[ -z "$(ls -A "$sync_bin")" ]'

# 2. Full mode: spawn-node, spawn, spawn-hc and web.tar.gz.
out2="$root/out-full"
run_build --mode full --spawn-sha "$spawn_sha" --node-sha "$node_sha" \
  --bin-dir "$bins" --web-dir "$web" --out-dir "$out2"
check "full: build exits 0" '[ "$rc" = 0 ]'
m2="$out2/release.manifest"
for f in spawn-node spawn spawn-hc web.tar.gz; do
  check "full: $f checksum equals its staged sha256" \
    '[ -n "$(manifest_sum "$m2" '"$f"')" ] && [ "$(manifest_sum "$m2" '"$f"')" = "$(sum "$out2/'"$f"'")" ]'
done
check "full: manifest lists exactly four artifacts" '[ "$(grep -Ec "^[0-9a-f]{64}  " "$m2")" = 4 ]'
mkdir -p "$root/unpack"
tar -xzf "$out2/web.tar.gz" -C "$root/unpack"
check "full: web.tar.gz holds the web dir's contents at its root" \
  'cmp -s "$root/unpack/index.html" "$web/index.html" && cmp -s "$root/unpack/assets/app.js" "$web/assets/app.js"'
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
  first=$(sum "$out2/web.tar.gz")
  sleep 1
  touch "$web/index.html"
  run_build --mode full --spawn-sha "$spawn_sha" --node-sha "$node_sha" \
    --bin-dir "$bins" --web-dir "$web" --out-dir "$out2"
  check "full: web.tar.gz is the same bytes for the same tree (GNU tar)" \
    '[ "$rc" = 0 ] && [ "$(sum "$out2/web.tar.gz")" = "$first" ]'
fi
sync_full="$root/sync-full/bin"
mkdir -p "$sync_full"
run_sync --dry-run --mode full --manifest "$m2" --bin-dir "$sync_full"
check "full: fleet-sync-node.sh --dry-run --mode full accepts the manifest" '[ "$rc" = 0 ]'
check "full: dry run staged the web UI" 'grep -q "staged web" <<<"$out"'

# 3. A real (non-dry) sync from the built release, then already current.
: > "$root/systemctl.log"
run_sync --mode full --manifest "$m2" --bin-dir "$sync_full" --units "spawn-core.service"
check "install: sync from the built manifest exits 0" '[ "$rc" = 0 ]'
check "install: installed spawn-node is the built one" 'cmp -s "$sync_full/spawn-node" "$bins/spawn-node"'
check "install: web UI installed" 'cmp -s "$root/sync-full/web/index.html" "$web/index.html"'
check "install: state names both SHAs" \
  '[ "$(cat "$sync_full/.fleet-sync.state")" = "spawn_sha=$spawn_sha node_sha=$node_sha mode=full" ]'
check "install: units restarted" 'grep -q "restart -- spawn-core.service" "$root/systemctl.log"'
run_sync --mode full --manifest "$m2" --bin-dir "$sync_full"
check "install: second sync is already current" '[ "$rc" = 0 ] && grep -q "already current" <<<"$out"'

# 4. --artifacts-url goes into the manifest; --artifacts overrides it.
out3="$root/out-url"
run_build --spawn-sha "$spawn_sha" --node-sha "$node_sha" --bin-dir "$bins" --out-dir "$out3" \
  --artifacts-url "https://releases.example.invalid/spawn/$spawn_sha"
check "url: build exits 0" '[ "$rc" = 0 ]'
check "url: artifacts line is the URL" \
  'grep -qx "artifacts=https://releases.example.invalid/spawn/$spawn_sha" "$out3/release.manifest"'
mkdir -p "$root/sync-url/bin"
run_sync --dry-run --manifest "$out3/release.manifest" --artifacts "$out3" --bin-dir "$root/sync-url/bin"
check "url: dry run against the staged copy (--artifacts) passes" '[ "$rc" = 0 ]'

# 5. A changed artifact after the build is refused by the sync (fail closed).
echo tampered >> "$out1/spawn-node"
run_sync --dry-run --manifest "$m" --bin-dir "$sync_bin"
check "tamper: dry run refuses a changed artifact" '[ "$rc" = 1 ] && grep -q "sha256 mismatch for spawn-node" <<<"$out"'

# 6. A rebuild replaces the manifest and the artifacts.
make_bin "$bins/spawn-node" v4-node
run_build --spawn-sha "$spawn_sha" --node-sha "$node_sha" --bin-dir "$bins" --out-dir "$out1"
check "rebuild: exits 0" '[ "$rc" = 0 ]'
check "rebuild: checksum follows the new binary" \
  '[ "$(manifest_sum "$m" spawn-node)" = "$(sum "$bins/spawn-node")" ]'
check "rebuild: one spawn-node line" '[ "$(grep -c "  spawn-node$" "$m")" = 1 ]'
run_sync --dry-run --manifest "$m" --bin-dir "$sync_bin"
check "rebuild: round-trips again" '[ "$rc" = 0 ]'

# 7. Refusals: nothing published, and no secret-bearing URL written.
refused() { # <label> <want rc> <args...>
  local label=$1 want=$2 dir="$root/refused-$pass-$fail"
  shift 2
  run_build "$@" --out-dir "$dir"
  check "refuse: $label (exit $want, no manifest)" '[ "$rc" = '"$want"' ] && [ ! -e "$dir/release.manifest" ]'
}
refused "short spawn sha" 1 --spawn-sha abc1234 --node-sha "$node_sha" --bin-dir "$bins"
refused "upper-case node sha" 1 --spawn-sha "$spawn_sha" --node-sha "${node_sha^^}" --bin-dir "$bins"
refused "missing bin dir" 1 --spawn-sha "$spawn_sha" --node-sha "$node_sha" --bin-dir "$root/nope"
mkdir -p "$root/half"
make_bin "$root/half/spawn-node" x
refused "full mode without spawn/spawn-hc" 1 --mode full --spawn-sha "$spawn_sha" --node-sha "$node_sha" \
  --bin-dir "$root/half" --web-dir "$web"
mkdir -p "$root/empty-web"
refused "full mode with an empty web dir" 1 --mode full --spawn-sha "$spawn_sha" --node-sha "$node_sha" \
  --bin-dir "$bins" --web-dir "$root/empty-web"
refused "full mode without --web-dir" 2 --mode full --spawn-sha "$spawn_sha" --node-sha "$node_sha" --bin-dir "$bins"
refused "--web-dir in node mode" 2 --spawn-sha "$spawn_sha" --node-sha "$node_sha" --bin-dir "$bins" --web-dir "$web"
refused "unknown mode" 2 --mode half --spawn-sha "$spawn_sha" --node-sha "$node_sha" --bin-dir "$bins"
refused "unknown argument" 2 --spawn-sha "$spawn_sha" --node-sha "$node_sha" --bin-dir "$bins" --what x
refused "missing --node-sha" 2 --spawn-sha "$spawn_sha" --bin-dir "$bins"
refused "URL with userinfo" 1 --spawn-sha "$spawn_sha" --node-sha "$node_sha" --bin-dir "$bins" \
  --artifacts-url "https://user:pw@releases.example.invalid/x"
check "refuse: the userinfo is not echoed" '! grep -q "pw@" <<<"$out"'
refused "URL with a query string" 1 --spawn-sha "$spawn_sha" --node-sha "$node_sha" --bin-dir "$bins" \
  --artifacts-url "https://releases.example.invalid/x?token=abc"
refused "non-http URL" 1 --spawn-sha "$spawn_sha" --node-sha "$node_sha" --bin-dir "$bins" \
  --artifacts-url "file:///srv/x"
run_build --spawn-sha "$spawn_sha" --node-sha "$node_sha" --bin-dir "$bins" --out-dir "$bins"
check "refuse: --out-dir equal to --bin-dir" '[ "$rc" = 1 ] && [ ! -e "$bins/release.manifest" ]'

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" = 0 ]
