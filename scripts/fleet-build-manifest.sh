#!/usr/bin/env bash
# Build release artifacts:
#   - Managed nodes (scripts/fleet-sync-node.sh):
#       scripts/fleet-build-manifest.sh --spawn-sha <40 hex> --node-sha <40 hex> \
#           --bin-dir <dir> --out-dir <dir> [--web-dir <dir>] \
#           [--artifacts-url <http(s) url>] [--mode node|full] \
#           [--feature-root <dir>] [--feature-registry <file>] \
#           [--feature-coverage <file>] [--spawn-bin <path>]
#   - Control-plane self-upgrade release (deploy/self-upgrade.sh --role control-plane):
#       scripts/fleet-build-manifest.sh --mode control-plane --bin-dir <dir> \
#           --web-dir <dir> --out-dir <dir> \
#           (--feature-root <dir> | --feature-coverage <file> --feature-registry <file>) \
#           [--feature-registry <file>] [--spawn-bin <path>]
#
# Steps, each one stops the build on failure:
#   1. check the arguments: SHAs (when node|full) 40 lowercase hex, every input present;
#   2. stage the artifacts in --out-dir:
#      - node: spawn-node;
#      - full: spawn-node, spawn, spawn-hc, deterministic web.tar.gz (plus optional
#        feature-coverage.json + registry.json when --feature-root or --feature-coverage given);
#      - control-plane: spawn, spawn-node-gateway, optional spawn-hc, web.tar.gz,
#        feature-coverage.json (0644), registry.json (0644);
#   3. hash every staged artifact (SHA-256) and write <out-dir>/release.manifest
#      (node|full) or <out-dir>/SHA256SUMS (control-plane), atomic temp+rename;
#   4. hash the staged files again and compare them with the published checksums.
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
feature_root=""
feature_registry=""
feature_coverage=""
spawn_bin=""

need_arg() { if [ "$#" -lt 2 ] || [ -z "$2" ]; then warn "$1 needs a value"; exit 2; fi; }
while [ "$#" -gt 0 ]; do
  case "$1" in
    --spawn-sha) need_arg "$@"; spawn_sha=$2; shift ;;
    --node-sha) need_arg "$@"; node_sha=$2; shift ;;
    --bin-dir) need_arg "$@"; bin_dir=$2; shift ;;
    --web-dir) need_arg "$@"; web_dir=$2; shift ;;
    --out-dir) need_arg "$@"; out_dir=$2; shift ;;
    --artifacts-url) need_arg "$@"; artifacts_url=$2; shift ;;
    --feature-root) need_arg "$@"; feature_root=$2; shift ;;
    --feature-registry) need_arg "$@"; feature_registry=$2; shift ;;
    --feature-coverage) need_arg "$@"; feature_coverage=$2; shift ;;
    --spawn-bin) need_arg "$@"; spawn_bin=$2; shift ;;
    --mode) need_arg "$@"; mode=$2; shift ;;
    -h | --help) usage ;;
    *) warn "unknown argument: $1"; exit 2 ;;
  esac
  shift
done

case "$mode" in
  node | full | control-plane) ;;
  *) warn "--mode must be node, full, or control-plane"; exit 2 ;;
esac
[ -n "$bin_dir" ] || { warn "give --bin-dir"; exit 2; }
[ -n "$out_dir" ] || { warn "give --out-dir"; exit 2; }

if [ "$mode" = control-plane ]; then
  [ -n "$web_dir" ] || { warn "--mode control-plane needs --web-dir"; exit 2; }
  [ -z "$artifacts_url" ] || { warn "--artifacts-url is not used with --mode control-plane"; exit 2; }
  if [ -z "$feature_root" ] && [ -z "$feature_coverage" ]; then
    warn "--mode control-plane needs --feature-root or --feature-coverage + --feature-registry"
    exit 2
  fi
  if [ -n "$feature_coverage" ] && [ -z "$feature_registry" ]; then
    warn "--feature-coverage requires --feature-registry"
    exit 2
  fi
else
  if [ -z "$spawn_sha" ] || [ -z "$node_sha" ]; then
    warn "give both --spawn-sha and --node-sha"
    exit 2
  fi
  if [ "$mode" = full ]; then
    [ -n "$web_dir" ] || { warn "--mode full needs --web-dir"; exit 2; }
    if [ -n "$feature_coverage" ] && [ -z "$feature_registry" ]; then
      warn "--feature-coverage requires --feature-registry"
      exit 2
    fi
  else
    [ -z "$web_dir" ] || { warn "--web-dir is only used with --mode full or control-plane"; exit 2; }
    [ -z "$feature_root" ] && [ -z "$feature_registry" ] && [ -z "$feature_coverage" ] && [ -z "$spawn_bin" ] || {
      warn "--feature-* and --spawn-bin are not used with --mode node"
      exit 2
    }
  fi
fi

if [ -n "$spawn_sha" ]; then
  [[ "$spawn_sha" =~ ^[0-9a-f]{40}$ ]] || die "spawn sha is not 40 lowercase hex"
fi
if [ -n "$node_sha" ]; then
  [[ "$node_sha" =~ ^[0-9a-f]{40}$ ]] || die "node sha is not 40 lowercase hex"
fi
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

want_features=0
if [ "$mode" = control-plane ] || [ -n "$feature_root" ] || [ -n "$feature_coverage" ] || [ -n "$feature_registry" ]; then
  want_features=1
  if [ -n "$feature_root" ]; then
    [ -d "$feature_root" ] || die "--feature-root not found: $feature_root"
    [ -n "$feature_registry" ] || feature_registry="$feature_root/docs/feature-map/registry.json"
  fi
  [ -n "$feature_registry" ] || die "missing feature registry path (pass --feature-root or --feature-registry)"
  [ -f "$feature_registry" ] && [ -s "$feature_registry" ] || die "feature registry missing or empty: $feature_registry"
  if [ -n "$feature_coverage" ]; then
    [ -f "$feature_coverage" ] && [ -s "$feature_coverage" ] || die "feature coverage snapshot missing or empty: $feature_coverage"
  else
    snap_bin=${spawn_bin:-$bin_dir/spawn}
    [ -x "$snap_bin" ] || die "spawn binary for coverage snapshot not executable: $snap_bin (pass --spawn-bin or --feature-coverage)"
  fi
fi

if [ "$mode" = control-plane ]; then
  binaries=(spawn spawn-node-gateway)
  for name in "${binaries[@]}"; do
    [ -f "$bin_dir/$name" ] || die "binary missing: $bin_dir/$name"
  done
  [ ! -f "$bin_dir/spawn-hc" ] || binaries+=(spawn-hc)
else
  binaries=(spawn-node)
  [ "$mode" = full ] && binaries+=(spawn spawn-hc)
  for name in "${binaries[@]}"; do
    [ -f "$bin_dir/$name" ] || die "binary missing: $bin_dir/$name"
  done
fi
if [ "$mode" != node ] && [ -z "$(ls -A -- "$web_dir")" ]; then
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
sums_file="$out_dir/SHA256SUMS"
# Remove previous checksum index first so no half-built release can validate.
rm -f -- "$manifest" "$manifest.tmp" "$sums_file" "$sums_file.tmp"

artifacts=()
for name in "${binaries[@]}"; do
  rm -f -- "$out_dir/$name.tmp"
  cp -- "$bin_dir/$name" "$out_dir/$name.tmp" || die "could not copy $bin_dir/$name"
  chmod 0755 "$out_dir/$name.tmp"
  mv -f -- "$out_dir/$name.tmp" "$out_dir/$name"
  artifacts+=("$name")
  say "staged $name"
done
if [ "$mode" != node ]; then
  # The web UI as fleet-sync-node.sh / self-upgrade.sh unpack it: contents at archive root.
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

if [ "$want_features" = 1 ]; then
  rm -f -- "$out_dir/registry.json.tmp" "$out_dir/feature-coverage.json.tmp"
  cp -- "$feature_registry" "$out_dir/registry.json.tmp" || die "could not copy $feature_registry"
  chmod 0644 "$out_dir/registry.json.tmp"
  [ -s "$out_dir/registry.json.tmp" ] || {
    rm -f -- "$out_dir/registry.json.tmp"
    die "staged registry.json is empty"
  }
  mv -f -- "$out_dir/registry.json.tmp" "$out_dir/registry.json"
  artifacts+=(registry.json)
  say "staged registry.json"

  if [ -n "$feature_coverage" ]; then
    cp -- "$feature_coverage" "$out_dir/feature-coverage.json.tmp" || die "could not copy $feature_coverage"
  else
    abs_feature_root=$(cd -- "$feature_root" && pwd -P)
    reg_dir=$(cd -- "$(dirname -- "$feature_registry")" && pwd -P)
    abs_feature_registry="$reg_dir/$(basename -- "$feature_registry")"
    case "$snap_bin" in
      */*)
        abs_snap_dir=$(cd -- "$(dirname -- "$snap_bin")" && pwd -P)
        abs_snap_bin="$abs_snap_dir/$(basename -- "$snap_bin")"
        ;;
      *)
        abs_snap_bin=$snap_bin
        ;;
    esac
    scan_reg=$abs_feature_registry
    if [ "${abs_feature_registry#"$abs_feature_root/"}" != "$abs_feature_registry" ]; then
      scan_reg=${abs_feature_registry#"$abs_feature_root/"}
    fi
    (
      cd -- "$abs_feature_root"
      "$abs_snap_bin" features scan --root . --registry "$scan_reg" --snapshot --out "$out_dir/feature-coverage.json.tmp"
    ) || {
      rm -f -- "$out_dir/feature-coverage.json.tmp"
      die "failed to generate feature-coverage.json via $snap_bin features scan --snapshot"
    }
  fi
  chmod 0644 "$out_dir/feature-coverage.json.tmp"
  [ -s "$out_dir/feature-coverage.json.tmp" ] || {
    rm -f -- "$out_dir/feature-coverage.json.tmp"
    die "staged feature-coverage.json is empty"
  }
  mv -f -- "$out_dir/feature-coverage.json.tmp" "$out_dir/feature-coverage.json"
  artifacts+=(feature-coverage.json)
  say "staged feature-coverage.json"
fi

declare -A sums=()
if [ "$mode" = control-plane ]; then
  mapfile -t sorted_artifacts < <(printf '%s\n' "${artifacts[@]}" | LC_ALL=C sort)
  {
    for name in "${sorted_artifacts[@]}"; do
      sums[$name]=$(sha256_of "$out_dir/$name")
      printf '%s  %s\n' "${sums[$name]}" "$name"
    done
  } > "$sums_file.tmp"

  for name in "${sorted_artifacts[@]}"; do
    [ "$(sha256_of "$out_dir/$name")" = "${sums[$name]}" ] || {
      rm -f -- "$sums_file.tmp"
      die "$name changed while SHA256SUMS was written"
    }
  done
  mv -f -- "$sums_file.tmp" "$sums_file"

  for name in "${sorted_artifacts[@]}"; do say "sha256 ${sums[$name]}  $name"; done
  say "wrote $sums_file (mode=$mode artifacts=${#sorted_artifacts[@]})"
else
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
fi
