#!/usr/bin/env bash
# Build the release a managed node pulls with scripts/fleet-sync-node.sh.
#
#   scripts/fleet-build-manifest.sh --spawn-sha <40 hex> --node-sha <40 hex> \
#       --bin-dir <dir> --out-dir <dir> [--web-dir <dir>] \
#       [--artifacts-url <http(s) url>] [--mode node|full]
#
# Steps, each one stops the build on failure:
#   1. check the arguments: both SHAs 40 lowercase hex, every input present;
#   2. stage the artifacts in --out-dir: spawn-node from --bin-dir (full mode
#      adds spawn and spawn-hc from --bin-dir, and web.tar.gz packed from
#      --web-dir's contents);
#   3. hash every staged artifact (SHA-256) and write
#      <out-dir>/release.manifest in the format fleet-sync-node.sh parses:
#        spawn_sha=<40 hex>
#        node_sha=<40 hex>
#        artifacts=<--artifacts-url, else the absolute --out-dir>
#        <64 hex sha256>  <artifact name>
#      (written to a temporary file and renamed, so a reader never sees half
#      of one);
#   4. hash the staged files again and compare them with the manifest.
#
# Publish <out-dir> as it is (the manifest beside its artifacts), or upload
# the artifacts to --artifacts-url and serve the manifest from anywhere; a
# node then runs
#   scripts/fleet-sync-node.sh --manifest <url or path of release.manifest>
# and `--dry-run` there verifies a build without installing it.
#
# --artifacts-url is written into the manifest, which is meant to be
# published: one with userinfo (user:pass@), a query string or a fragment
# is refused, so no credential lands in it. This script holds and prints
# no secrets. It uses bash, coreutils, sha256sum or shasum, tar and gzip.
#
# Exit codes: 0 built, 1 refused (nothing published: the manifest is
# written last), 2 usage.

set -euo pipefail

if [ "${BASH_VERSINFO[0]}" -lt 4 ] || { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -lt 4 ]; }; then
  echo "fleet-build-manifest: bash >= 4.4 is required" >&2
  exit 1
fi

prog=fleet-build-manifest
say() { printf '%s: %s\n' "$prog" "$*"; }
warn() { printf '%s: %s\n' "$prog" "$*" >&2; }
die() { warn "refused: $*"; exit 1; }
usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }

mode=node
spawn_sha=""
node_sha=""
bin_dir=""
web_dir=""
out_dir=""
artifacts_url=""

need_arg() { if [ "$#" -lt 2 ] || [ -z "$2" ]; then warn "$1 needs a value"; exit 2; fi; }
while [ "$#" -gt 0 ]; do
  case "$1" in
    --spawn-sha) need_arg "$@"; spawn_sha=$2; shift ;;
    --node-sha) need_arg "$@"; node_sha=$2; shift ;;
    --bin-dir) need_arg "$@"; bin_dir=$2; shift ;;
    --web-dir) need_arg "$@"; web_dir=$2; shift ;;
    --out-dir) need_arg "$@"; out_dir=$2; shift ;;
    --artifacts-url) need_arg "$@"; artifacts_url=$2; shift ;;
    --mode) need_arg "$@"; mode=$2; shift ;;
    -h | --help) usage ;;
    *) warn "unknown argument: $1"; exit 2 ;;
  esac
  shift
done

case "$mode" in node | full) ;; *) warn "--mode must be node or full"; exit 2 ;; esac
if [ -z "$spawn_sha" ] || [ -z "$node_sha" ]; then warn "give both --spawn-sha and --node-sha"; exit 2; fi
[ -n "$bin_dir" ] || { warn "give --bin-dir"; exit 2; }
[ -n "$out_dir" ] || { warn "give --out-dir"; exit 2; }
if [ "$mode" = full ]; then
  [ -n "$web_dir" ] || { warn "--mode full needs --web-dir"; exit 2; }
elif [ -n "$web_dir" ]; then
  warn "--web-dir is only used with --mode full"; exit 2
fi

[[ "$spawn_sha" =~ ^[0-9a-f]{40}$ ]] || die "spawn sha is not 40 lowercase hex"
[[ "$node_sha" =~ ^[0-9a-f]{40}$ ]] || die "node sha is not 40 lowercase hex"
if [ -n "$artifacts_url" ]; then
  [[ "$artifacts_url" =~ ^https?://[^[:space:]]+$ ]] || die "--artifacts-url is not an http(s) URL without spaces"
  case "$artifacts_url" in *\?* | *\#*) die "--artifacts-url must not carry a query string or fragment" ;; esac
  authority=${artifacts_url#*://}
  authority=${authority%%/*}
  case "$authority" in *@*) die "--artifacts-url must not carry userinfo" ;; esac
  [ -n "$authority" ] || die "--artifacts-url has no host"
fi
[ -d "$bin_dir" ] || die "--bin-dir not found: $bin_dir"
[ "$mode" = node ] || [ -d "$web_dir" ] || die "--web-dir not found: $web_dir"

binaries=(spawn-node)
[ "$mode" = full ] && binaries+=(spawn spawn-hc)
for name in "${binaries[@]}"; do
  [ -f "$bin_dir/$name" ] || die "binary missing: $bin_dir/$name"
done
if [ "$mode" = full ] && [ -z "$(ls -A -- "$web_dir")" ]; then
  die "--web-dir is empty: $web_dir"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum -- "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 -- "$1" | cut -d' ' -f1; }
else
  die "neither sha256sum nor shasum is installed"
fi

mkdir -p -- "$out_dir"
out_dir=$(cd -- "$out_dir" && pwd -P)
abs_bin=$(cd -- "$bin_dir" && pwd -P)
[ "$out_dir" != "$abs_bin" ] || die "--out-dir must differ from --bin-dir"
manifest="$out_dir/release.manifest"
# The old manifest goes first: until the new one is written, the directory
# names no release, so a half-built one is never pulled.
rm -f -- "$manifest" "$manifest.tmp"

artifacts=()
for name in "${binaries[@]}"; do
  rm -f -- "$out_dir/$name.tmp"
  cp -- "$bin_dir/$name" "$out_dir/$name.tmp" || die "could not copy $bin_dir/$name"
  chmod 0755 "$out_dir/$name.tmp"
  mv -f -- "$out_dir/$name.tmp" "$out_dir/$name"
  artifacts+=("$name")
  say "staged $name"
done
if [ "$mode" = full ]; then
  # The web UI as fleet-sync-node.sh unpacks it: the directory's contents at
  # the archive's root. With GNU tar the archive is the same bytes for the
  # same tree (sorted names, fixed owner and mtime, gzip -n).
  tar_opts=()
  if tar --version 2>/dev/null | grep -q 'GNU tar'; then
    tar_opts=(--sort=name --owner=0 --group=0 --numeric-owner --mtime=@0)
  fi
  rm -f -- "$out_dir/web.tar.gz.tmp"
  tar -c "${tar_opts[@]}" -C "$web_dir" -f - . | gzip -n -9 > "$out_dir/web.tar.gz.tmp" ||
    die "could not pack $web_dir"
  mv -f -- "$out_dir/web.tar.gz.tmp" "$out_dir/web.tar.gz"
  artifacts+=(web.tar.gz)
  say "staged web.tar.gz"
fi

declare -A sums=()
{
  printf '# Spawn release manifest, read by scripts/fleet-sync-node.sh --manifest.\n'
  printf '# mode=%s\n' "$mode"
  printf 'spawn_sha=%s\n' "$spawn_sha"
  printf 'node_sha=%s\n' "$node_sha"
  printf 'artifacts=%s\n' "${artifacts_url:-$out_dir}"
  for name in "${artifacts[@]}"; do
    sums[$name]=$(sha256_of "$out_dir/$name")
    printf '%s  %s\n' "${sums[$name]}" "$name"
  done
} > "$manifest.tmp"

# Read the staged files back before publishing the manifest.
for name in "${artifacts[@]}"; do
  [ "$(sha256_of "$out_dir/$name")" = "${sums[$name]}" ] || {
    rm -f -- "$manifest.tmp"
    die "$name changed while the manifest was written"
  }
done
mv -f -- "$manifest.tmp" "$manifest"

for name in "${artifacts[@]}"; do say "sha256 ${sums[$name]}  $name"; done
say "wrote $manifest (mode=$mode spawn=$spawn_sha node=$node_sha)"
