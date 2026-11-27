#!/usr/bin/env bash
# Pull-based sync of a managed node to a published Spawn build.
#
#   scripts/fleet-sync-node.sh --manifest <file|url> [options]
#   scripts/fleet-sync-node.sh --spawn-sha <sha> --node-sha <sha> --artifacts <dir|url> \
#       [--checksums <file|url>] [options]
#   scripts/fleet-sync-node.sh --spawn-sha <sha> --node-sha <sha> --source-dir <checkout> [options]
#
# The node pulls; nothing pushes to it. Steps, each one stops the sync on failure:
#   1. resolve the plan: target SHAs, artifact source, checksums, install paths;
#   2. stage every artifact next to its live copy as <name>.staged (same file
#      system, so the swap is a rename) and verify its SHA-256 against the
#      manifest / checksums file. With checksums given, a missing entry, a
#      malformed line or a mismatch refuses the sync (fail closed);
#   3. smoke-run every staged binary (`--version`, then `--help`; one must
#      exit 0 within --smoke-timeout seconds);
#   4. swap: each live copy becomes <name>.prev (hard link, else copy; the old
#      .prev is replaced atomically), then <name>.staged is renamed over it;
#   5. restart the target systemd user units (--units);
#   6. run --health-cmd up to --health-retries times, --health-interval seconds
#      apart, each bounded by --health-timeout seconds;
#   7. on a failed restart or health check, roll every swapped artifact back
#      to its .prev copy (a first install is removed), restart the units again
#      and exit 3. The state file records the synced SHAs only after health
#      passes.
#
# --check / --dry-run resolves the plan and verifies the artifacts in a
# scratch directory outside --bin-dir, and changes nothing on the host.
#
# Modes: node (default) installs spawn-node; full installs spawn-node, spawn,
# spawn-hc and the web UI (web.tar.gz or a web/ directory).
#
# Manifest format (one entry per line, '#' comments and blank lines ignored;
# anything else is a parse failure):
#   spawn_sha=<40 hex>
#   node_sha=<40 hex>
#   artifacts=<dir or http(s) base url>      (optional; --artifacts overrides)
#   <64 hex sha256>  <artifact name>         (sha256sum format, e.g. spawn-node)
#
# This script holds and prints no secrets: URLs are printed without their
# query string or userinfo. It uses bash, coreutils, curl (for URLs),
# sha256sum or shasum, tar (web.tar.gz), git (--source-dir) and systemctl.
#
# Exit codes: 0 synced or already current, 1 refused before any change,
# 2 usage, 3 rolled back after a failed restart/health check, 4 rollback failed.

set -euo pipefail

if [ "${BASH_VERSINFO[0]}" -lt 4 ] || { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -lt 4 ]; }; then
  echo "fleet-sync-node: bash >= 4.4 is required" >&2
  exit 1
fi

prog=fleet-sync-node
say() { printf '%s: %s\n' "$prog" "$*"; }
warn() { printf '%s: %s\n' "$prog" "$*" >&2; }
die() { warn "refused: $*"; exit 1; }
usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }

dry_run=0
mode=node
manifest=""
checksums=""
artifacts=""
source_dir=""
spawn_sha=""
node_sha=""
bin_dir="${SPAWN_SYNC_BIN_DIR:-$HOME/.local/share/spawn-rsi/bin}"
web_dir=""
units=""
units_set=0
health_cmd=""
health_retries=5
health_interval=2
health_timeout=10
smoke_timeout=10
systemctl_cmd="${SPAWN_SYNC_SYSTEMCTL:-systemctl --user}"
force=0

need_arg() { [ "$#" -ge 2 ] && [ -n "$2" ] || { warn "$1 needs a value"; exit 2; }; }
while [ "$#" -gt 0 ]; do
  case "$1" in
    --check | --dry-run) dry_run=1 ;;
    --force) force=1 ;;
    --mode) need_arg "$@"; mode=$2; shift ;;
    --manifest) need_arg "$@"; manifest=$2; shift ;;
    --checksums) need_arg "$@"; checksums=$2; shift ;;
    --artifacts) need_arg "$@"; artifacts=$2; shift ;;
    --source-dir) need_arg "$@"; source_dir=$2; shift ;;
    --spawn-sha) need_arg "$@"; spawn_sha=$2; shift ;;
    --node-sha) need_arg "$@"; node_sha=$2; shift ;;
    --bin-dir) need_arg "$@"; bin_dir=$2; shift ;;
    --web-dir) need_arg "$@"; web_dir=$2; shift ;;
    --units) [ "$#" -ge 2 ] || { warn "--units needs a value"; exit 2; }; units=$2; units_set=1; shift ;;
    --health-cmd) need_arg "$@"; health_cmd=$2; shift ;;
    --health-retries) need_arg "$@"; health_retries=$2; shift ;;
    --health-interval) need_arg "$@"; health_interval=$2; shift ;;
    --health-timeout) need_arg "$@"; health_timeout=$2; shift ;;
    --smoke-timeout) need_arg "$@"; smoke_timeout=$2; shift ;;
    --systemctl) need_arg "$@"; systemctl_cmd=$2; shift ;;
    -h | --help) usage ;;
    *) warn "unknown argument: $1"; exit 2 ;;
  esac
  shift
done

case "$mode" in node | full) ;; *) warn "--mode must be node or full"; exit 2 ;; esac
for n in "$health_retries" "$health_interval" "$health_timeout" "$smoke_timeout"; do
  [[ "$n" =~ ^[0-9]+$ ]] || { warn "retry/interval/timeout values must be whole numbers"; exit 2; }
done
[ "$health_retries" -ge 1 ] || { warn "--health-retries must be at least 1"; exit 2; }
if [ -n "$manifest" ] && { [ -n "$spawn_sha" ] || [ -n "$node_sha" ] || [ -n "$checksums" ]; }; then
  warn "--manifest excludes --spawn-sha/--node-sha/--checksums"; exit 2
fi
if [ -z "$manifest" ] && { [ -z "$spawn_sha" ] || [ -z "$node_sha" ]; }; then
  warn "give --manifest, or both --spawn-sha and --node-sha"; exit 2
fi
[ -n "$web_dir" ] || web_dir="$(dirname "$bin_dir")/web"
if [ "$units_set" = 0 ]; then
  units="spawn-node-edge.service spawn-node-gw.service spawn-node-out.service"
  [ "$mode" = full ] && units="spawn-core.service $units"
fi

is_url() { case "$1" in http://* | https://*) return 0 ;; *) return 1 ;; esac; }
# A URL as printed: no userinfo, no query string or fragment.
shown() {
  local s=$1
  if is_url "$s"; then
    s=${s%%\?*}; s=${s%%#*}
    s=$(printf '%s' "$s" | sed -E 's#^(https?://)[^/@]*@#\1#')
  fi
  printf '%s' "$s"
}

if command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum -- "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 -- "$1" | cut -d' ' -f1; }
else
  die "neither sha256sum nor shasum is installed"
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/fleet-sync-node.XXXXXX")
lock_dir=""
cleanup() {
  rm -rf "$scratch"
  [ -z "$lock_dir" ] || rmdir "$lock_dir" 2>/dev/null || true
}
trap cleanup EXIT

# fetch <file|url> <dest>
fetch() {
  if is_url "$1"; then
    command -v curl >/dev/null 2>&1 || die "curl is needed to fetch $(shown "$1")"
    curl -fsSL --proto '=https,http' --retry 3 --max-time 600 -o "$2" -- "$1" ||
      die "could not fetch $(shown "$1")"
  else
    [ -f "$1" ] || die "no such file: $1"
    cp -- "$1" "$2"
  fi
}

declare -A want_sha=()
have_checksums=0
# parse_checksum_line <line> <origin>: returns 1 when not a checksum line.
parse_checksum_line() {
  local line=$1
  if [[ "$line" =~ ^([0-9a-fA-F]{64})[[:space:]]+\*?([A-Za-z0-9._-]+)$ ]]; then
    local sum=${BASH_REMATCH[1],,} name=${BASH_REMATCH[2]}
    if [ -n "${want_sha[$name]:-}" ] && [ "${want_sha[$name]}" != "$sum" ]; then
      die "$2: two different checksums for $name"
    fi
    want_sha[$name]=$sum
    have_checksums=1
    return 0
  fi
  return 1
}
strip() { local s=${1%$'\r'}; s=${s#"${s%%[![:space:]]*}"}; printf '%s' "${s%"${s##*[![:space:]]}"}"; }

if [ -n "$manifest" ]; then
  fetch "$manifest" "$scratch/manifest"
  n=0
  while IFS= read -r raw || [ -n "$raw" ]; do
    n=$((n + 1))
    line=$(strip "$raw")
    case "$line" in '' | '#'*) continue ;; esac
    case "$line" in
      spawn_sha=*) spawn_sha=${line#spawn_sha=} ;;
      node_sha=*) node_sha=${line#node_sha=} ;;
      artifacts=*) [ -n "$artifacts" ] || artifacts=${line#artifacts=} ;;
      *) parse_checksum_line "$line" "manifest line $n" || die "manifest line $n does not parse" ;;
    esac
  done < "$scratch/manifest"
  [ "$have_checksums" = 1 ] || die "manifest lists no checksums"
elif [ -n "$checksums" ]; then
  fetch "$checksums" "$scratch/checksums"
  n=0
  while IFS= read -r raw || [ -n "$raw" ]; do
    n=$((n + 1))
    line=$(strip "$raw")
    case "$line" in '' | '#'*) continue ;; esac
    parse_checksum_line "$line" "checksums line $n" || die "checksums line $n does not parse"
  done < "$scratch/checksums"
  [ "$have_checksums" = 1 ] || die "checksums file lists no checksums"
fi

[[ "$spawn_sha" =~ ^[0-9a-f]{40}$ ]] || die "spawn sha is not 40 lowercase hex"
[[ "$node_sha" =~ ^[0-9a-f]{40}$ ]] || die "node sha is not 40 lowercase hex"

if [ -n "$source_dir" ]; then
  command -v git >/dev/null 2>&1 || die "git is needed for --source-dir"
  head=$(git -C "$source_dir" rev-parse HEAD 2>/dev/null) || die "$source_dir is not a git checkout"
  [ "$head" = "$spawn_sha" ] || die "$source_dir is at $head, not spawn sha $spawn_sha"
  [ -n "$artifacts" ] || artifacts="$source_dir/target/release"
fi
[ -n "$artifacts" ] || die "no artifact source: give --artifacts, --source-dir or artifacts= in the manifest"
if is_url "$artifacts"; then
  [ "$have_checksums" = 1 ] || die "artifacts from a URL need checksums (--manifest or --checksums)"
else
  [ -d "$artifacts" ] || die "artifact directory not found: $artifacts"
fi

binaries=(spawn-node)
[ "$mode" = full ] && binaries+=(spawn spawn-hc)

state_file="$bin_dir/.fleet-sync.state"
current=""
[ -f "$state_file" ] && current=$(cat "$state_file")
target="spawn_sha=$spawn_sha node_sha=$node_sha mode=$mode"

say "plan: mode=$mode spawn=$spawn_sha node=$node_sha"
say "plan: artifacts from $(shown "$artifacts"); checksums: $([ "$have_checksums" = 1 ] && echo verified || echo none)"
say "plan: install ${binaries[*]}$([ "$mode" = full ] && echo ' web') into $bin_dir$([ "$mode" = full ] && echo " (web: $web_dir)")"
say "plan: restart units: ${units:-<none>}; health: ${health_cmd:-<none>}"
if [ "$current" = "$target" ] && [ "$force" = 0 ]; then
  say "already current ($target)"
  exit 0
fi

# stage_one <name> <dest>: copy/fetch the artifact to <dest> and verify it.
stage_one() {
  local name=$1 dest=$2 src
  if is_url "$artifacts"; then
    src="${artifacts%/}/$name"
    fetch "$src" "$dest"
  else
    src="$artifacts/$name"
    [ -f "$src" ] || die "artifact missing: $src"
    cp -- "$src" "$dest" || die "could not copy $src"
  fi
  if [ "$have_checksums" = 1 ]; then
    local want=${want_sha[$name]:-}
    [ -n "$want" ] || die "no checksum listed for $name"
    local got
    got=$(sha256_of "$dest")
    [ "$got" = "$want" ] || die "sha256 mismatch for $name (want $want, got $got)"
  fi
}

# run_bounded <seconds> <cmd...>: run with a wall-clock bound, without
# requiring coreutils `timeout` (absent on macOS).
run_bounded() {
  local secs=$1; shift
  if command -v timeout >/dev/null 2>&1; then
    timeout -k 2 "$secs" "$@"
    return
  fi
  "$@" &
  local pid=$! waited=0
  while kill -0 "$pid" 2>/dev/null; do
    if [ "$waited" -ge "$((secs * 10))" ]; then
      kill -TERM "$pid" 2>/dev/null || true
      sleep 1
      kill -KILL "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      return 124
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  wait "$pid"
}

smoke() {
  local bin=$1
  run_bounded "$smoke_timeout" "$bin" --version >/dev/null 2>&1 && return 0
  run_bounded "$smoke_timeout" "$bin" --help >/dev/null 2>&1 && return 0
  return 1
}

# Where staged copies go: next to the live copy (dry run: the scratch dir).
if [ "$dry_run" = 1 ]; then
  stage_dir="$scratch/stage"
  mkdir -p "$stage_dir"
else
  mkdir -p "$bin_dir"
  stage_dir="$bin_dir"
  lock_dir="$bin_dir/.fleet-sync.lock"
  mkdir "$lock_dir" 2>/dev/null || { lock_dir=""; die "another sync holds $bin_dir/.fleet-sync.lock"; }
fi

staged=()
discard_staged() {
  local f
  for f in "${staged[@]}"; do rm -rf -- "$f"; done
}
for name in "${binaries[@]}"; do
  dest="$stage_dir/$name.staged"
  rm -f -- "$dest"
  staged+=("$dest")
  ( stage_one "$name" "$dest" ) || { discard_staged; exit 1; }
  chmod 0755 "$dest"
  smoke "$dest" || { discard_staged; die "staged $name failed its smoke run (--version/--help)"; }
  say "staged $name ($(sha256_of "$dest"))"
done
web_staged=""
if [ "$mode" = full ]; then
  if [ "$dry_run" = 1 ]; then web_staged="$scratch/web.staged"; else web_staged="${web_dir%/}.staged"; fi
  rm -rf -- "$web_staged"
  staged+=("$web_staged")
  if is_url "$artifacts" || [ -f "$artifacts/web.tar.gz" ] || [ -n "${want_sha[web.tar.gz]:-}" ]; then
    ( stage_one web.tar.gz "$scratch/web.tar.gz" ) || { discard_staged; exit 1; }
    mkdir -p "$web_staged"
    tar -xzf "$scratch/web.tar.gz" -C "$web_staged" || { discard_staged; die "web.tar.gz does not extract"; }
  elif [ -d "$artifacts/web" ]; then
    [ "$have_checksums" = 0 ] || { discard_staged; die "checksums given but web.tar.gz is not listed"; }
    cp -R -- "$artifacts/web" "$web_staged"
  else
    discard_staged
    die "full mode needs web.tar.gz or a web/ directory in the artifacts"
  fi
  say "staged web"
fi

if [ "$dry_run" = 1 ]; then
  say "dry run: verified and smoke-tested; nothing changed"
  exit 0
fi

# Swap. `swapped` lists what was replaced, for the rollback.
swapped=()
first_install=()
keep_prev() {
  local live=$1
  rm -f -- "$live.prev.tmp"
  ln -- "$live" "$live.prev.tmp" 2>/dev/null || cp -p -- "$live" "$live.prev.tmp"
  mv -f -- "$live.prev.tmp" "$live.prev"
}
for name in "${binaries[@]}"; do
  live="$bin_dir/$name"
  if [ -e "$live" ]; then
    if cmp -s -- "$live" "$live.staged"; then
      rm -f -- "$live.staged"
      say "$name unchanged"
      continue
    fi
    keep_prev "$live"
    swapped+=("$name")
  else
    first_install+=("$name")
  fi
  mv -f -- "$live.staged" "$live"
  say "installed $name"
done
web_swapped=0
web_first=0
if [ "$mode" = full ]; then
  if [ -d "$web_dir" ]; then
    rm -rf -- "${web_dir%/}.prev"
    mv -- "$web_dir" "${web_dir%/}.prev"
    web_swapped=1
  else
    mkdir -p "$(dirname "$web_dir")"
    web_first=1
  fi
  mv -- "$web_staged" "$web_dir"
  say "installed web"
fi

restart_units() {
  [ -n "$units" ] || return 0
  # shellcheck disable=SC2086 # the command and unit list are word lists
  $systemctl_cmd restart -- $units
}

health_ok() {
  [ -n "$health_cmd" ] || return 0
  local try
  for ((try = 1; try <= health_retries; try++)); do
    if run_bounded "$health_timeout" bash -c "$health_cmd" >/dev/null 2>&1; then
      say "health ok (try $try)"
      return 0
    fi
    warn "health check failed (try $try/$health_retries)"
    [ "$try" -lt "$health_retries" ] && sleep "$health_interval"
  done
  return 1
}

rollback() {
  local failed=0 name live
  warn "rolling back to the .prev copies"
  for name in "${swapped[@]}"; do
    live="$bin_dir/$name"
    { cp -p -- "$live.prev" "$live.rollback" && mv -f -- "$live.rollback" "$live"; } || failed=1
  done
  for name in "${first_install[@]}"; do rm -f -- "$bin_dir/$name" || failed=1; done
  if [ "$web_swapped" = 1 ]; then
    { rm -rf -- "${web_dir%/}.failed" && mv -- "$web_dir" "${web_dir%/}.failed" &&
      cp -R -- "${web_dir%/}.prev" "$web_dir" && rm -rf -- "${web_dir%/}.failed"; } || failed=1
  elif [ "$web_first" = 1 ]; then
    rm -rf -- "$web_dir" || failed=1
  fi
  restart_units || failed=1
  if [ "$failed" = 0 ] && health_ok; then
    warn "rolled back; the previous build is healthy"
    exit 3
  fi
  warn "rollback failed or the previous build is unhealthy too: fix by hand"
  exit 4
}

if ! restart_units; then
  warn "restarting units failed"
  rollback
fi
if ! health_ok; then
  rollback
fi
printf '%s\n' "$target" > "$state_file.tmp"
mv -f -- "$state_file.tmp" "$state_file"
say "synced ($target)"
