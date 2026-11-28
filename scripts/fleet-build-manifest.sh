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
  command -v jq >/dev/null 2>&1 || die "jq is required when staging feature coverage artifacts"
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

validate_feature_registry_json() {
  local reg_file=$1
  jq -e '
    def is_str: type == "string";
    def is_nonempty_str: is_str and (gsub("^[[:space:]]+|[[:space:]]+$"; "") | length > 0);
    def is_rel_path:
      is_nonempty_str
      and (startswith("/") | not)
      and (contains("\\") | not)
      and (test("^[A-Za-z]:") | not);
    type == "object"
    and .schema_version == 1
    and (.initiative_id | is_nonempty_str)
    and (.boundary | type == "object")
    and (.boundary.project_name | is_nonempty_str)
    and (.boundary.description | is_str)
    and (.boundary.included_categories | type == "array" and all(.[]; is_str))
    and (.boundary.exclusions | type == "array" and all(.[];
          type == "object"
          and (.id | is_nonempty_str)
          and (.pattern | is_nonempty_str)
          and (.category | is_nonempty_str)
          and (.reason | is_str)))
    and (.features | type == "array" and all(.[];
          type == "object"
          and (.id | is_nonempty_str)
          and (.title | is_str)
          and (.kind == "domain" or .kind == "capability" or .kind == "initiative" or .kind == "slice" or .kind == "research-initiative" or .kind == "subsystem")
          and (.summary | is_str)
          and ((.files // []) | type == "array" and all(.[]; is_rel_path))))
  ' "$reg_file" >/dev/null 2>&1
}

validate_feature_coverage_json() {
  local cov_file=$1
  jq -e '
    def is_str: type == "string";
    def is_nonempty_str: is_str and (gsub("^[[:space:]]+|[[:space:]]+$"; "") | length > 0);
    def is_str_array: type == "array" and all(.[]; is_str);
    def is_int: type == "number" and floor == .;
    def is_nonneg_int: is_int and . >= 0;
    def is_rel_path:
      is_nonempty_str
      and (startswith("/") | not)
      and (contains("\\") | not)
      and (test("^[A-Za-z]:") | not);
    def is_valid_kind:
      . == "domain" or . == "capability" or . == "initiative" or . == "slice" or . == "research-initiative" or . == "subsystem";
    def is_valid_boundary:
      type == "object"
      and (.project_name | is_nonempty_str)
      and (.description | is_str)
      and (.included_categories | is_str_array)
      and (.exclusions | type == "array" and all(.[];
            type == "object"
            and (.id | is_nonempty_str)
            and (.pattern | is_nonempty_str)
            and (.category | is_nonempty_str)
            and (.reason | is_str)));
    type == "object"
    and .root == "."
    and .registry_path == "docs/feature-map/registry.json"
    and .registry_writable == false
    and (.read_only_reason | is_nonempty_str)
    and (.report | type == "object")
    and .report.schema_version == 1
    and (.report.scanned_at_ms | is_int)
    and (.report.initiative_id | is_nonempty_str)
    and (.report.boundary | is_valid_boundary)
    and (.report.counts | type == "object"
          and (.total_seen | is_nonneg_int)
          and (.in_scope | is_nonneg_int)
          and (.covered | is_nonneg_int)
          and (.unassociated_wild | is_nonneg_int)
          and (.stale_references | is_nonneg_int)
          and (.explicit_exclusions | is_nonneg_int)
          and (.multi_feature_files | is_nonneg_int)
          and (.orphan_features | is_nonneg_int)
          and (.invalid_relations | is_nonneg_int))
    and (.report.unassociated_files | type == "array" and all(.[];
          type == "object"
          and (.path | is_rel_path)
          and (.tracked | type == "boolean")
          and (.size_bytes | is_nonneg_int)
          and (.reason | is_str)
          and (.evidence | is_str_array)
          and (.suggested_features | is_str_array)))
    and (.report.stale_references | type == "array" and all(.[];
          type == "object"
          and (.path | is_rel_path)
          and (.referencing_features | is_str_array)
          and (.rename_candidates | is_str_array)))
    and (.report.excluded_files | type == "array" and all(.[];
          type == "object"
          and (.path | is_rel_path)
          and (.rule_id | is_nonempty_str)
          and (.category | is_nonempty_str)
          and (.reason | is_str)))
    and (.report.bidirectional_index | type == "object"
          and (.feature_to_files | type == "object" and (to_entries | all(.[]; (.value | type == "array" and all(.[]; is_rel_path)))))
          and (.file_to_features | type == "object" and (to_entries | all(.[]; (.key | is_rel_path) and (.value | is_str_array)))))
    and (.report.consistency | type == "object"
          and (.orphan_features | is_str_array)
          and (.dangling_relations | is_str_array)
          and (.self_links | is_str_array)
          and (.cycle_errors | is_str_array)
          and (.duplicate_feature_ids | is_str_array)
          and (.invalid_paths | is_str_array))
    and (.features | type == "array" and all(.[];
          type == "object"
          and (.id | is_nonempty_str)
          and (.title | is_str)
          and (.kind | is_valid_kind)
          and (.parent == null or (.parent | is_str))
          and (.summary | is_str)
          and (.status_note == null or (.status_note | is_str))
          and (.file_count | is_nonneg_int)
          and ((.relations == null) or (.relations | type == "array" and all(.[]; type == "object" and (.target | is_nonempty_str) and (.kind | is_nonempty_str))))
          and ((.provenance == null) or (.provenance | type == "object"))))
  ' "$cov_file" >/dev/null 2>&1
}

if [ "$want_features" = 1 ]; then
  rm -f -- "$out_dir/registry.json.tmp" "$out_dir/feature-coverage.json.tmp" "$out_dir/feature-coverage.json.raw"
  cp -- "$feature_registry" "$out_dir/registry.json.tmp" || die "could not copy $feature_registry"
  chmod 0644 "$out_dir/registry.json.tmp"
  [ -s "$out_dir/registry.json.tmp" ] || {
    rm -f -- "$out_dir/registry.json.tmp"
    die "staged registry.json is empty"
  }
  validate_feature_registry_json "$out_dir/registry.json.tmp" || {
    rm -f -- "$out_dir/registry.json.tmp"
    die "staged registry.json failed schema validation: $feature_registry"
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
      "$abs_snap_bin" features scan --root . --registry "$scan_reg" --snapshot --out "$out_dir/feature-coverage.json.raw"
    ) || {
      rm -f -- "$out_dir/feature-coverage.json.raw"
      die "failed to generate feature-coverage.json via $snap_bin features scan --snapshot"
    }
    jq -c '.root = "." | .registry_path = "docs/feature-map/registry.json"' \
      "$out_dir/feature-coverage.json.raw" > "$out_dir/feature-coverage.json.tmp" || {
      rm -f -- "$out_dir/feature-coverage.json.raw" "$out_dir/feature-coverage.json.tmp"
      die "failed to normalize generated feature-coverage.json portable metadata"
    }
    rm -f -- "$out_dir/feature-coverage.json.raw"
  fi
  chmod 0644 "$out_dir/feature-coverage.json.tmp"
  [ -s "$out_dir/feature-coverage.json.tmp" ] || {
    rm -f -- "$out_dir/feature-coverage.json.tmp"
    die "staged feature-coverage.json is empty"
  }
  validate_feature_coverage_json "$out_dir/feature-coverage.json.tmp" || {
    rm -f -- "$out_dir/feature-coverage.json.tmp"
    die "staged feature-coverage.json failed schema/portable-path validation (requires root=\".\", registry_path=\"docs/feature-map/registry.json\", registry_writable=false, complete report & features)"
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
