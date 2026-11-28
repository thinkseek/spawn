#!/usr/bin/env bash
# Zero-downtime smooth self-upgrade for immutable Spawn releases (T0.3a).
#
#   deploy/self-upgrade.sh --role <control-plane|worker-node> --version <tag> [options]
#   deploy/self-upgrade.sh --role <control-plane|worker-node> --rollback [--reason <text>] [options]
#
# Immutable release layout under --rsi-dir (default ~/.local/share/spawn-rsi):
#   releases/<tag>/          spawn, [spawn-node-gateway], [spawn-hc], [spawn-node],
#                            [web/dist/... | web.tar.gz], SHA256SUMS
#   web-extracted/<tag>/     verified web payload extracted from web.tar.gz (outside releases/)
#   current -> releases/<tag>
#   previous -> releases/<prev-tag>
#   bin/<binary> -> ../current/<binary>
#   web -> web-extracted/<tag> | current/web/dist | current/web
#   upgrade-state.json       sanitized closed-state record
#                            (status: upgrading|active|rolled_back|rollback_failed|superseded)
#   upgrade-audit.jsonl      sanitized append-only transition log (secrets redacted)
#
# Safety invariants:
#   - Verifies SHA256SUMS (strict multi-path mapping + symlink escape check),
#     refuses unlisted entries in the release root (and links only executables
#     verified in SHA256SUMS into bin/), checks tar member types/paths/modes
#     (only '-' files and 'd' dirs; zero symlinks, hardlinks, devices/FIFOs, or
#     setuid/setgid bits), and refuses unlisted web/ or web.tar.gz BEFORE touching
#     current, previous, bin/, or web symlinks.
#   - First upgrade from a non-symlink live layout (bin/* regular executables +
#     web/ directory, no current/previous) snapshots the live binaries and web/
#     into releases/pre-upgrade-<utc-ts>/ with generated SHA256SUMS before
#     switching symlinks so rollback preserves every legacy binary and UI asset.
#   - Never reads or prints spawn.env or credential files; URLs (`scheme://...`),
#     `Authorization:` / `Cookie:` / `Set-Cookie:` headers, `Bearer`/`Basic` tokens,
#     JSON/key-value/CLI-flag secrets (`password`, `passwd`, `passphrase`, `token`,
#     `secret`, `api[_-]?key`, `access[_-]?token`, `refresh[_-]?token`,
#     `private[_-]?key`, `client[_-]?secret`, `session`, `key`), and bare `sk-...`
#     tokens are redacted across script stdout/stderr, `upgrade-state.json`,
#     `upgrade-audit.jsonl`, and captured backup hook / drain / `init-db` output.
#     Lock FD is closed before invoking hooks or restarted processes.
#   - Owner-stopped units (`ActiveState=inactive`, `SubState=dead`, `Result=success`)
#     are never started during upgrades or rollbacks and are excluded from watchdog
#     failure counts; restarts pass `--job-mode=ignore-requirements` so `Wants=`
#     dependencies (e.g. `spawn-integrate.service` -> `spawn-loop.service`) never
#     pull up an owner-stopped peer unit. Zero active units fail closed on
#     upgrade unless --allow-no-active-units is given (which records `staged (0
#     active units; no units restarted or verified)` instead of `verified healthy`).
#   - Role boundaries (honest systemd contract):
#     * --role control-plane: runs pre-backup hook when configured, flips
#       symlinks, runs `spawn init-db`, restarts spawn-node-gateway then
#       spawn-serve, probes health, and auto-rolls back on failure (exit 3).
#     * --role worker-node --db-blind: for stateless remote nodes (e.g.
#       spawn-node.service); hard-refuses BEFORE switching symlinks if any target
#       unit's ExecStartPre invokes `init-db`. Safe to roll out before control-plane.
#     * --role worker-node (fleet host with spawn-loop.service): spawn-loop's
#       systemd ExecStartPre runs `spawn init-db` with the new binary on restart,
#       so fleet workers upgrade AFTER control-plane R2 backup & migration (or
#       pass --pre-backup-cmd / --backup-unit). Drains spawn-integrate via Epoch
#       Drain, preserves independent spawn-hc process trees, and enforces a
#       worker settle window (--worker-settle-secs, default 5) against crash loops.
#
# Exit codes:
#   0  upgraded cleanly, rolled back explicitly, or --dry-run verification passed
#   1  refused pre-switch (checksum, layout, tarball, db-blind, 0 active units, lock, backup)
#   2  usage / argument error
#   3  activation, restart, probe, or settle failed; automatic rollback succeeded
#   4  activation, restart, probe, or settle failed AND automatic rollback also failed

set -euo pipefail

if [ "${BASH_VERSINFO[0]}" -lt 4 ] || { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -lt 4 ]; }; then
  echo "self-upgrade: bash >= 4.4 is required" >&2
  exit 1
fi

UPGRADE_PROG=${UPGRADE_PROG:-self-upgrade}
LOCK_FD=9
_TMP_SUMS=""
_TMP_CMD_OUT=""
_TMP_CMD_ERR=""

cleanup_self_upgrade_tmp() {
  if [ -n "${_TMP_SUMS-}" ] && [ -e "$_TMP_SUMS" ]; then
    rm -f -- "$_TMP_SUMS"
  fi
  if [ -n "${_TMP_CMD_OUT-}" ] && [ -e "$_TMP_CMD_OUT" ]; then
    rm -f -- "$_TMP_CMD_OUT"
  fi
  if [ -n "${_TMP_CMD_ERR-}" ] && [ -e "$_TMP_CMD_ERR" ]; then
    rm -f -- "$_TMP_CMD_ERR"
  fi
}

redact_secrets_stream() {
  sed -E \
    -e 's#[A-Za-z][A-Za-z0-9+.-]*://[^[:space:]"'\''<>]+#<url>#g' \
    -e 's#([Aa][Uu][Tt][Hh][Oo][Rr][Ii][Zz][Aa][Tt][Ii][Oo][Nn][[:space:]]*:[[:space:]]*)(([Bb][Ee][Aa][Rr][Ee][Rr]|[Bb][Aa][Ss][Ii][Cc])[[:space:]]+)?[^[:space:]"'\'',;]+#\1<redacted>#g' \
    -e 's#(([Ss][Ee][Tt]-)?[Cc][Oo][Oo][Kk][Ii][Ee][[:space:]]*:[[:space:]]*)([A-Za-z0-9_.-]+=[^[:space:]"'\'',;]+([[:space:]]*;[[:space:]]*[A-Za-z0-9_.-]+=[^[:space:]"'\'',;]+)*|[^[:space:]"'\'',;]+)#\1<redacted>#g' \
    -e 's#([Bb][Ee][Aa][Rr][Ee][Rr]|[Bb][Aa][Ss][Ii][Cc])[[:space:]]+[^[:space:]"'\'',;]+#\1 <redacted>#g' \
    -e 's#("(([Kk][Ee][Yy])|([A-Za-z0-9_.-]*[_-][Kk][Ee][Yy])|([A-Za-z0-9_.-]*([Pp][Aa][Ss][Ss][Ww][Oo][Rr][Dd]|[Pp][Aa][Ss][Ss][Ww][Dd]|[Pp][Aa][Ss][Ss][Pp][Hh][Rr][Aa][Ss][Ee]|[Tt][Oo][Kk][Ee][Nn]|[Ss][Ee][Cc][Rr][Ee][Tt]|[Ss][Ee][Ss][Ss][Ii][Oo][Nn])))"[[:space:]]*:[[:space:]]*)"([^"\\]|\\.)*"#\1"<redacted>"#g' \
    -e 's#('\''(([Kk][Ee][Yy])|([A-Za-z0-9_.-]*[_-][Kk][Ee][Yy])|([A-Za-z0-9_.-]*([Pp][Aa][Ss][Ss][Ww][Oo][Rr][Dd]|[Pp][Aa][Ss][Ss][Ww][Dd]|[Pp][Aa][Ss][Ss][Pp][Hh][Rr][Aa][Ss][Ee]|[Tt][Oo][Kk][Ee][Nn]|[Ss][Ee][Cc][Rr][Ee][Tt]|[Ss][Ee][Ss][Ss][Ii][Oo][Nn])))'\''[[:space:]]*:[[:space:]]*)'\''[^'\'']*'\''#\1'\''<redacted>'\''#g' \
    -e 's#(--([Pp][Aa][Ss][Ss][Ww][Oo][Rr][Dd]|[Pp][Aa][Ss][Ss][Ww][Dd]|[Pp][Aa][Ss][Ss][Pp][Hh][Rr][Aa][Ss][Ee]|[Tt][Oo][Kk][Ee][Nn]|[Ss][Ee][Cc][Rr][Ee][Tt]|[Aa][Pp][Ii][_-]?[Kk][Ee][Yy]|[Aa][Cc][Cc][Ee][Ss][Ss][_-]?[Tt][Oo][Kk][Ee][Nn]|[Rr][Ee][Ff][Rr][Ee][Ss][Hh][_-]?[Tt][Oo][Kk][Ee][Nn]|[Pp][Rr][Ii][Vv][Aa][Tt][Ee][_-]?[Kk][Ee][Yy]|[Cc][Ll][Ii][Ee][Nn][Tt][_-]?[Ss][Ee][Cc][Rr][Ee][Tt]|[Ss][Ee][Ss][Ss][Ii][Oo][Nn]|[Kk][Ee][Yy])[[:space:]]+)("([^"\\]|\\.)*"|'\''[^'\'']*'\''|[^[:space:]"'\'',;]+)#\1<redacted>#g' \
    -e 's#(([Pp][Aa][Ss][Ss][Ww][Oo][Rr][Dd]|[Pp][Aa][Ss][Ss][Ww][Dd]|[Pp][Aa][Ss][Ss][Pp][Hh][Rr][Aa][Ss][Ee]|[Tt][Oo][Kk][Ee][Nn]|[Ss][Ee][Cc][Rr][Ee][Tt]|[Aa][Pp][Ii][_-]?[Kk][Ee][Yy]|[Aa][Cc][Cc][Ee][Ss][Ss][_-]?[Tt][Oo][Kk][Ee][Nn]|[Rr][Ee][Ff][Rr][Ee][Ss][Hh][_-]?[Tt][Oo][Kk][Ee][Nn]|[Pp][Rr][Ii][Vv][Aa][Tt][Ee][_-]?[Kk][Ee][Yy]|[Cc][Ll][Ii][Ee][Nn][Tt][_-]?[Ss][Ee][Cc][Rr][Ee][Tt]|[Ss][Ee][Ss][Ss][Ii][Oo][Nn])[[:space:]]*[:=][[:space:]]*)"([^"\\]|\\.)*"#\1"<redacted>"#g' \
    -e 's#(([Pp][Aa][Ss][Ss][Ww][Oo][Rr][Dd]|[Pp][Aa][Ss][Ss][Ww][Dd]|[Pp][Aa][Ss][Ss][Pp][Hh][Rr][Aa][Ss][Ee]|[Tt][Oo][Kk][Ee][Nn]|[Ss][Ee][Cc][Rr][Ee][Tt]|[Aa][Pp][Ii][_-]?[Kk][Ee][Yy]|[Aa][Cc][Cc][Ee][Ss][Ss][_-]?[Tt][Oo][Kk][Ee][Nn]|[Rr][Ee][Ff][Rr][Ee][Ss][Hh][_-]?[Tt][Oo][Kk][Ee][Nn]|[Pp][Rr][Ii][Vv][Aa][Tt][Ee][_-]?[Kk][Ee][Yy]|[Cc][Ll][Ii][Ee][Nn][Tt][_-]?[Ss][Ee][Cc][Rr][Ee][Tt]|[Ss][Ee][Ss][Ss][Ii][Oo][Nn])[[:space:]]*[:=][[:space:]]*)'\''[^'\'']*'\''#\1'\''<redacted>'\''#g' \
    -e 's#(([Pp][Aa][Ss][Ss][Ww][Oo][Rr][Dd]|[Pp][Aa][Ss][Ss][Ww][Dd]|[Pp][Aa][Ss][Ss][Pp][Hh][Rr][Aa][Ss][Ee]|[Tt][Oo][Kk][Ee][Nn]|[Ss][Ee][Cc][Rr][Ee][Tt]|[Aa][Pp][Ii][_-]?[Kk][Ee][Yy]|[Aa][Cc][Cc][Ee][Ss][Ss][_-]?[Tt][Oo][Kk][Ee][Nn]|[Rr][Ee][Ff][Rr][Ee][Ss][Hh][_-]?[Tt][Oo][Kk][Ee][Nn]|[Pp][Rr][Ii][Vv][Aa][Tt][Ee][_-]?[Kk][Ee][Yy]|[Cc][Ll][Ii][Ee][Nn][Tt][_-]?[Ss][Ee][Cc][Rr][Ee][Tt]|[Ss][Ee][Ss][Ss][Ii][Oo][Nn])[[:space:]]*[:=][[:space:]]*)[^][:space:]"'\'',;}]+#\1<redacted>#g' \
    -e 's#(^|[^A-Za-z0-9])([Kk][Ee][Yy][[:space:]]*[:=][[:space:]]*)"([^"\\]|\\.)*"#\1\2"<redacted>"#g' \
    -e 's#(^|[^A-Za-z0-9])([Kk][Ee][Yy][[:space:]]*[:=][[:space:]]*)'\''[^'\'']*'\''#\1\2'\''<redacted>'\''#g' \
    -e 's#(^|[^A-Za-z0-9])([Kk][Ee][Yy][[:space:]]*[:=][[:space:]]*)[^][:space:]"'\'',;}]+#\1\2<redacted>#g' \
    -e 's#(^|[^A-Za-z0-9_-])sk-[A-Za-z0-9._-]{4,}#\1<redacted>#g'
}

redact_secrets() {
  local raw=${1-}
  printf '%s' "$raw" | tr '\n\r\t' '   ' | redact_secrets_stream
}

sanitize_text() {
  redact_secrets "${1-}" | head -c 300
}

say() { printf '%s: %s\n' "$UPGRADE_PROG" "$(redact_secrets "$*")"; }
warn() { printf '%s: %s\n' "$UPGRADE_PROG" "$(redact_secrets "$*")" >&2; }
die_usage() { warn "$*"; exit 2; }
die_refuse() { warn "refused: $*"; exit 1; }

usage() {
  sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d; s/^# \{0,1\}//' >&2
  exit 2
}

init_upgrade_defaults() {
  rsi_dir=${SPAWN_RSI_DIR:-${SPAWN_LIVE:-$HOME/.local/share/spawn-rsi}}
  unit_dir=${SPAWN_UNIT_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user}
  role=""
  version=""
  rollback=0
  reason=""
  dry_run=0
  units_csv=""
  units_set=0
  db_blind=${SPAWN_UPGRADE_DB_BLIND:-0}
  allow_no_active_units=${SPAWN_UPGRADE_ALLOW_NO_ACTIVE_UNITS:-0}
  serve_url=${SPAWN_UPGRADE_SERVE_URL:-http://127.0.0.1:8787/}
  serve_expect_codes=${SPAWN_UPGRADE_SERVE_EXPECT_CODES:-200}
  gateway_url=${SPAWN_UPGRADE_GATEWAY_URL:-http://127.0.0.1:18096/}
  gateway_expect_codes=${SPAWN_UPGRADE_GATEWAY_EXPECT_CODES:-200,400,401,403,404,405}
  pre_backup_cmd=${SPAWN_UPGRADE_PRE_BACKUP_CMD:-}
  backup_unit=${SPAWN_UPGRADE_BACKUP_UNIT:-}
  skip_backup=0
  skip_init_db=${SPAWN_UPGRADE_SKIP_INIT_DB:-0}
  skip_drain=0
  drain_timeout=${SPAWN_UPGRADE_DRAIN_TIMEOUT_SECS:-300}
  probe_retries=${SPAWN_UPGRADE_PROBE_RETRIES:-5}
  probe_interval=${SPAWN_UPGRADE_PROBE_INTERVAL:-1}
  probe_timeout=${SPAWN_UPGRADE_PROBE_TIMEOUT:-5}
  worker_settle_secs=${SPAWN_UPGRADE_WORKER_SETTLE_SECS:-5}
  watch_window_secs=${SPAWN_UPGRADE_WATCH_WINDOW_SECS:-600}
  systemctl_cmd=${SPAWN_UPGRADE_SYSTEMCTL:-systemctl --user}
  curl_cmd=${SPAWN_UPGRADE_CURL:-curl}
  control_units_default=${SPAWN_UPGRADE_CONTROL_UNITS:-spawn-node-gateway.service,spawn-serve.service}
  worker_units_default=${SPAWN_UPGRADE_WORKER_UNITS:-spawn-loop.service,spawn-integrate.service,spawn-node.service}
  LAST_PROBE_REASON=""
  STATE_UNITS=()
}

run_without_lock_fd() {
  (
    eval "exec ${LOCK_FD}>&-"
    "$@"
  )
}

run_without_lock_fd_redacted() {
  local mode=$1
  shift
  local out_tmp err_tmp rc=0
  out_tmp=$(mktemp "${TMPDIR:-/tmp}/spawn-self-upgrade-out.XXXXXX")
  chmod 0600 "$out_tmp"
  _TMP_CMD_OUT=$out_tmp
  if [ "$mode" = "stderr-only" ]; then
    run_without_lock_fd "$@" >/dev/null 2>"$out_tmp" || rc=$?
    if [ -s "$out_tmp" ]; then
      redact_secrets_stream <"$out_tmp" >&2
    fi
    rm -f -- "$out_tmp"
    _TMP_CMD_OUT=""
    return "$rc"
  fi
  err_tmp=$(mktemp "${TMPDIR:-/tmp}/spawn-self-upgrade-err.XXXXXX")
  chmod 0600 "$err_tmp"
  _TMP_CMD_ERR=$err_tmp
  run_without_lock_fd "$@" >"$out_tmp" 2>"$err_tmp" || rc=$?
  if [ -s "$out_tmp" ]; then
    redact_secrets_stream <"$out_tmp"
  fi
  if [ -s "$err_tmp" ]; then
    redact_secrets_stream <"$err_tmp" >&2
  fi
  rm -f -- "$out_tmp" "$err_tmp"
  _TMP_CMD_OUT=""
  _TMP_CMD_ERR=""
  return "$rc"
}

now_epoch() {
  if [ -n "${SPAWN_UPGRADE_NOW_EPOCH:-}" ]; then
    printf '%s' "$SPAWN_UPGRADE_NOW_EPOCH"
  else
    date -u +%s
  fi
}

iso_from_epoch() {
  local ts=$1
  date -u -d "@$ts" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null ||
    date -u -r "$ts" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null ||
    date -u +%Y-%m-%dT%H:%M:%SZ
}

compact_utc_from_epoch() {
  local ts=$1
  date -u -d "@$ts" +%Y%m%dT%H%M%SZ 2>/dev/null ||
    date -u -r "$ts" +%Y%m%dT%H%M%SZ 2>/dev/null ||
    date -u +%Y%m%dT%H%M%SZ
}

require_nonneg_int() {
  local name=$1 val=$2 min=${3:-0}
  [[ "$val" =~ ^[0-9]+$ ]] || die_usage "$name must be a non-negative integer, not '$val'"
  [ "$val" -ge "$min" ] || die_usage "$name must be >= $min, not '$val'"
}

split_cmd_array() {
  local cmd_str=$1 out_name=$2
  # Intentionally splits on whitespace into command + args without eval.
  local -a parts=()
  IFS=' ' read -r -a parts <<<"$cmd_str"
  [ "${#parts[@]}" -gt 0 ] || die_usage "empty command override"
  eval "$out_name=(\"\${parts[@]}\")"
}

validate_release_tag() {
  local tag=$1
  [[ "$tag" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || return 1
  [[ "$tag" != */* ]] || return 1
  return 0
}

link_release_tag() {
  local link_path=$1 raw base dir_name parent_name
  [ -L "$link_path" ] || return 0
  raw=$(readlink -- "$link_path") || return 0
  raw=${raw%/}
  base=${raw##*/}
  parent_name=$(dirname -- "$raw")
  dir_name=${parent_name##*/}
  if [ "$dir_name" = "releases" ] && validate_release_tag "$base"; then
    printf '%s' "$base"
    return 0
  fi
  return 0
}

atomic_symlink() {
  local target=$1 link=$2 parent base tmp
  parent=$(dirname -- "$link")
  base=$(basename -- "$link")
  mkdir -p "$parent"
  tmp="$parent/.${base}.tmp.$$"
  rm -f -- "$tmp"
  ln -s -- "$target" "$tmp"
  mv -Tf -- "$tmp" "$link"
}

validate_archive_member_path() {
  local rel=$1
  [ -n "$rel" ] || return 0
  [[ "$rel" != /* ]] || die_refuse "web.tar.gz contains absolute path: $rel"
  [[ "$rel" != *\\* ]] || die_refuse "web.tar.gz contains backslash: $rel"
  if [[ "$rel" == ".." || "$rel" == ../* || "$rel" == */../* || "$rel" == */.. ]]; then
    die_refuse "web.tar.gz contains path traversal: $rel"
  fi
}

validate_web_tarball() {
  local tarball=$1
  command -v tar >/dev/null 2>&1 || die_refuse "required tool not found in PATH: tar"

  local tz_out tv_out
  tz_out=$(LC_ALL=C tar -tzf "$tarball" 2>/dev/null) ||
    die_refuse "cannot list web.tar.gz: $tarball"
  tv_out=$(LC_ALL=C tar -tvzf "$tarball" 2>/dev/null) ||
    die_refuse "cannot inspect web.tar.gz members: $tarball"

  [ -n "$tz_out" ] && [ -n "$tv_out" ] || die_refuse "web.tar.gz is empty: $tarball"

  local entry tz_count=0 tv_count=0 has_reg_file=0
  while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    tz_count=$((tz_count + 1))
    validate_archive_member_path "$entry"
  done <<<"$tz_out"

  local line ftype perms mpath
  local tv_re='^([a-zA-Z-])([rwxstST-]{9})[+.]?[[:space:]]+[^[:space:]]+[[:space:]]+[0-9,]+[[:space:]]+[0-9]{4}-[0-9]{2}-[0-9]{2}[[:space:]]+[0-9]{2}:[0-9]{2}(:[0-9]{2})?[[:space:]]+(.+)$'
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    tv_count=$((tv_count + 1))
    if [[ ! "$line" =~ $tv_re ]]; then
      die_refuse "web.tar.gz contains unparseable tar member listing"
    fi
    ftype=${BASH_REMATCH[1]}
    perms=${BASH_REMATCH[2]}
    mpath=${BASH_REMATCH[4]}
    case "$ftype" in
      -)
        has_reg_file=1
        ;;
      d) ;;
      l | h)
        die_refuse "web.tar.gz rejects link member (type=$ftype): $mpath"
        ;;
      *)
        die_refuse "web.tar.gz rejects non-regular member (type=$ftype): $mpath"
        ;;
    esac
    if [[ "$perms" == *[sS]* ]]; then
      die_refuse "web.tar.gz rejects setuid/setgid permissions ($perms): $mpath"
    fi
    validate_archive_member_path "$mpath"
  done <<<"$tv_out"

  [ "$tz_count" -eq "$tv_count" ] && [ "$has_reg_file" -eq 1 ] ||
    die_refuse "web.tar.gz contains no regular web assets"
}

verify_release() {
  local tag=$1 check_role=$2
  validate_release_tag "$tag" || die_refuse "invalid release version tag '$tag'"
  local release_dir="$rsi_dir/releases/$tag"
  [ -d "$release_dir" ] && [ ! -L "$release_dir" ] ||
    die_refuse "release directory missing or symlink: $release_dir"
  local release_real
  release_real=$(readlink -f -- "$release_dir") || die_refuse "cannot resolve $release_dir"

  local sums_file="$release_dir/SHA256SUMS"
  [ -f "$sums_file" ] && [ ! -L "$sums_file" ] && [ -s "$sums_file" ] ||
    die_refuse "missing or empty SHA256SUMS in $release_dir"

  _TMP_SUMS=$(mktemp "${TMPDIR:-/tmp}/spawn-self-upgrade-sums.XXXXXX")
  : >"$_TMP_SUMS"

  local -A seen_paths=()
  local line lineno=0 hex rel target_real
  while IFS= read -r line || [ -n "$line" ]; do
    lineno=$((lineno + 1))
    [[ "$line" != *$'\r' ]] || die_refuse "SHA256SUMS:$lineno contains CR"
    [[ "$line" =~ ^[[:space:]]*$ ]] && continue
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    if [[ ! "$line" =~ ^([0-9a-fA-F]{64})\ ([ \*])(.+)$ ]]; then
      die_refuse "SHA256SUMS:$lineno is malformed"
    fi
    hex=${BASH_REMATCH[1]}
    rel=${BASH_REMATCH[3]}
    while [[ "$rel" == ./* ]]; do
      rel=${rel#./}
    done
    [ -n "$rel" ] || die_refuse "SHA256SUMS:$lineno empty path"
    [[ "$rel" != /* ]] || die_refuse "SHA256SUMS:$lineno absolute path rejected: $rel"
    [[ "$rel" != *\\* ]] || die_refuse "SHA256SUMS:$lineno backslash rejected: $rel"
    [[ "$rel" != . ]] || die_refuse "SHA256SUMS:$lineno invalid '.' path"
    [[ "$rel" != */ ]] || die_refuse "SHA256SUMS:$lineno directory path rejected: $rel"
    if [[ "$rel" == ".." || "$rel" == ../* || "$rel" == */../* || "$rel" == */.. || "$rel" == */./* || "$rel" == */. ]]; then
      die_refuse "SHA256SUMS:$lineno path traversal rejected: $rel"
    fi
    [ -z "${seen_paths[$rel]+x}" ] || die_refuse "SHA256SUMS:$lineno duplicate entry: $rel"
    seen_paths["$rel"]=1

    [ -f "$release_dir/$rel" ] && [ ! -L "$release_dir/$rel" ] ||
      die_refuse "SHA256SUMS:$lineno missing or symlink file: $rel"
    target_real=$(readlink -f -- "$release_dir/$rel") ||
      die_refuse "SHA256SUMS:$lineno cannot resolve $rel"
    case "$target_real" in
      "$release_real"/*) ;;
      *) die_refuse "SHA256SUMS:$lineno resolves outside release dir: $rel" ;;
    esac

    printf '%s  %s\n' "${hex,,}" "$rel" >>"$_TMP_SUMS"
  done <"$sums_file"

  [ "${#seen_paths[@]}" -gt 0 ] || die_refuse "SHA256SUMS contains no entries"

  if ! (cd "$release_dir" && sha256sum -c --strict "$_TMP_SUMS" >/dev/null 2>&1); then
    die_refuse "SHA256SUMS checksum verification failed for release $tag"
  fi
  rm -f -- "$_TMP_SUMS"
  _TMP_SUMS=""

  # Verify required and optional binaries are checksummed regular executable files.
  local bin
  [ -n "${seen_paths[spawn]+x}" ] || die_refuse "SHA256SUMS missing required entry 'spawn'"
  [ -x "$release_dir/spawn" ] || die_refuse "release binary not executable: $release_dir/spawn"

  if [ "$check_role" = "control-plane" ]; then
    [ -n "${seen_paths[spawn-node-gateway]+x}" ] ||
      die_refuse "SHA256SUMS missing required control-plane binary 'spawn-node-gateway'"
    [ -x "$release_dir/spawn-node-gateway" ] ||
      die_refuse "release binary not executable: $release_dir/spawn-node-gateway"
  fi

  for bin in spawn-node-gateway spawn-hc spawn-node; do
    if [ -e "$release_dir/$bin" ] || [ -L "$release_dir/$bin" ]; then
      [ -n "${seen_paths[$bin]+x}" ] ||
        die_refuse "present binary '$bin' is missing from SHA256SUMS"
      [ -f "$release_dir/$bin" ] && [ ! -L "$release_dir/$bin" ] && [ -s "$release_dir/$bin" ] && [ -x "$release_dir/$bin" ] ||
        die_refuse "release binary '$bin' must be a non-empty regular executable file"
    fi
  done

  # Refuse any unlisted or non-regular entry in the release root (web/ is verified below).
  local entry base
  shopt -s nullglob dotglob
  for entry in "$release_dir"/*; do
    base=$(basename -- "$entry")
    case "$base" in
      . | .. | SHA256SUMS | web) continue ;;
    esac
    if [ -L "$entry" ]; then
      shopt -u nullglob dotglob
      die_refuse "symlink entry rejected in release root: $base"
    fi
    if [ -d "$entry" ]; then
      shopt -u nullglob dotglob
      die_refuse "unlisted directory rejected in release root: $base"
    fi
    if [ ! -f "$entry" ]; then
      shopt -u nullglob dotglob
      die_refuse "non-regular entry rejected in release root: $base"
    fi
    if [ -z "${seen_paths[$base]+x}" ]; then
      shopt -u nullglob dotglob
      die_refuse "unverified entry in release root missing from SHA256SUMS: $base"
    fi
    if [ "$base" != "web.tar.gz" ] && [ -x "$entry" ] && [ ! -s "$entry" ]; then
      shopt -u nullglob dotglob
      die_refuse "release executable '$base' must be non-empty"
    fi
  done
  shopt -u nullglob dotglob

  local has_web_tar=0 has_web_dist_entry=0 has_any_web_entry=0 key
  [ -n "${seen_paths[web.tar.gz]+x}" ] && has_web_tar=1
  for key in "${!seen_paths[@]}"; do
    case "$key" in
      web/dist/*)
        has_web_dist_entry=1
        has_any_web_entry=1
        ;;
      web/*)
        has_any_web_entry=1
        ;;
    esac
  done

  if [ "$check_role" = "control-plane" ] && [ "$has_web_tar" -eq 0 ] && [ "$has_web_dist_entry" -eq 0 ]; then
    die_refuse "control-plane release missing SHA256SUMS coverage for web/dist/* or web.tar.gz"
  fi

  if [ "$has_web_tar" -eq 1 ]; then
    # Refuse unlisted pre-existing web/ directories alongside web.tar.gz so unverified UI content cannot slip through.
    if [ -e "$release_dir/web" ] || [ -L "$release_dir/web" ]; then
      die_refuse "release with web.tar.gz cannot contain pre-existing web/ directory: $release_dir/web"
    fi
    if [ "$has_any_web_entry" -eq 1 ]; then
      die_refuse "SHA256SUMS cannot mix web.tar.gz with web/* file entries"
    fi
    validate_web_tarball "$release_dir/web.tar.gz"
  else
    if [ -e "$release_dir/web.tar.gz" ] || [ -L "$release_dir/web.tar.gz" ]; then
      die_refuse "unlisted web.tar.gz present without SHA256SUMS entry"
    fi
    if [ -e "$release_dir/web" ] || [ -L "$release_dir/web" ]; then
      [ -d "$release_dir/web" ] && [ ! -L "$release_dir/web" ] ||
        die_refuse "web must be a regular directory"
      if [ -e "$release_dir/web/dist" ] || [ -L "$release_dir/web/dist" ]; then
        [ -d "$release_dir/web/dist" ] && [ ! -L "$release_dir/web/dist" ] ||
          die_refuse "web/dist must be a regular directory"
      fi
      local wf wrel
      while IFS= read -r -d '' wf; do
        [ -f "$wf" ] && [ ! -L "$wf" ] || die_refuse "non-regular or symlink entry rejected inside web/: $wf"
        wrel=${wf#"$release_dir/"}
        [ -n "${seen_paths[$wrel]+x}" ] ||
          die_refuse "unverified file inside web/ missing from SHA256SUMS: $wrel"
      done < <(find "$release_dir/web" ! -type d -print0)
      if [ -n "$(find "$release_dir/web" -perm /6000 -print -quit 2>/dev/null)" ]; then
        die_refuse "setuid/setgid permissions rejected inside $release_dir/web"
      fi
    fi
  fi
}

prepare_web_assets() {
  local tag=$1
  local release_dir="$rsi_dir/releases/$tag"
  if [ -f "$release_dir/web.tar.gz" ]; then
    command -v tar >/dev/null 2>&1 || die_refuse "required tool not found in PATH: tar"
    local extract_tmp="$rsi_dir/.web-extract-${tag}.$$"
    rm -rf -- "$extract_tmp"
    mkdir -m 0755 -p "$extract_tmp"
    if ! tar --no-same-owner --no-same-permissions -xzf "$release_dir/web.tar.gz" -C "$extract_tmp"; then
      chmod -R u+rwX "$extract_tmp" 2>/dev/null || true
      rm -rf -- "$extract_tmp"
      die_refuse "failed to extract $release_dir/web.tar.gz"
    fi
    # Explicitly strip setuid/setgid and normalize assets before post-extract verification.
    chmod -R u-s,g-s "$extract_tmp" 2>/dev/null || true
    if [ -n "$(find "$extract_tmp" -type l -print -quit 2>/dev/null)" ] ||
       [ -n "$(find "$extract_tmp" ! -type f ! -type d -print -quit 2>/dev/null)" ] ||
       [ -n "$(find "$extract_tmp" -type f -links +1 -print -quit 2>/dev/null)" ] ||
       [ -n "$(find "$extract_tmp" -perm /6000 -print -quit 2>/dev/null)" ]; then
      chmod -R u+rwX "$extract_tmp" 2>/dev/null || true
      rm -rf -- "$extract_tmp"
      die_refuse "extracted web.tar.gz failed post-extract symlink/link/mode validation"
    fi
    # Ensure no 0600/0500 permissions linger inside extracted assets or directories.
    chmod -R u+rwX,go+rX,go-w "$extract_tmp"

    local top_count=0 single_child="" child payload_dir
    shopt -s nullglob dotglob
    for child in "$extract_tmp"/*; do
      top_count=$((top_count + 1))
      single_child=$child
    done
    shopt -u nullglob dotglob
    if [ "$top_count" -eq 1 ] && [ -d "$single_child" ] && [ "$(basename -- "$single_child")" = "dist" ]; then
      payload_dir=$single_child
    else
      payload_dir=$extract_tmp
    fi

    mkdir -m 0755 -p "$rsi_dir/web-extracted"
    local dest="$rsi_dir/web-extracted/$tag"
    local prev_dest="$rsi_dir/web-extracted/.${tag}.old.$$"
    rm -rf -- "$prev_dest"
    if [ -e "$dest" ] || [ -L "$dest" ]; then
      mv -Tf -- "$dest" "$prev_dest" 2>/dev/null || mv -- "$dest" "$prev_dest"
    fi
    mv -T -- "$payload_dir" "$dest"
    chmod -R u-s,g-s,u+rwX,go+rX,go-w "$dest"
    rm -rf -- "$extract_tmp" "$prev_dest" 2>/dev/null || true
  elif [ -d "$release_dir/web" ]; then
    chmod -R u-s,g-s,u+rwX,go+rX,go-w "$release_dir/web"
  fi
}

list_verified_release_bins() {
  local release_dir=$1
  local sums_file="$release_dir/SHA256SUMS"
  [ -f "$sums_file" ] && [ ! -L "$sums_file" ] || return 1
  local -A listed_bins=()
  local line rel
  while IFS= read -r line || [ -n "$line" ]; do
    [[ "$line" != *$'\r' ]] || continue
    [[ "$line" =~ ^[[:space:]]*$ ]] && continue
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" =~ ^([0-9a-fA-F]{64})\ ([ \*])(.+)$ ]] || continue
    rel=${BASH_REMATCH[3]}
    while [[ "$rel" == ./* ]]; do
      rel=${rel#./}
    done
    case "$rel" in
      "" | . | .. | */* | SHA256SUMS | web.tar.gz | .*) continue ;;
    esac
    if [ -z "${listed_bins[$rel]+x}" ] &&
       [ -f "$release_dir/$rel" ] && [ ! -L "$release_dir/$rel" ] && [ -x "$release_dir/$rel" ]; then
      listed_bins["$rel"]=1
      printf '%s\n' "$rel"
    fi
  done <"$sums_file"
}

activate_release_links() {
  local tag=$1 update_previous=${2:-1}
  local release_dir="$rsi_dir/releases/$tag"
  [ -d "$release_dir" ] || return 1

  local verified_bins
  verified_bins=$(list_verified_release_bins "$release_dir") || return 1

  if [ "$update_previous" -eq 1 ]; then
    local cur_tag
    cur_tag=$(link_release_tag "$rsi_dir/current")
    if [ -n "$cur_tag" ] && [ "$cur_tag" != "$tag" ] && [ -d "$rsi_dir/releases/$cur_tag" ]; then
      atomic_symlink "releases/$cur_tag" "$rsi_dir/previous"
    fi
  fi

  atomic_symlink "releases/$tag" "$rsi_dir/current"

  mkdir -p "$rsi_dir/bin"
  local -A release_bins=()
  local f base
  while IFS= read -r base; do
    [ -n "$base" ] || continue
    release_bins["$base"]=1
    atomic_symlink "../current/$base" "$rsi_dir/bin/$base"
  done <<<"$verified_bins"

  shopt -s nullglob
  for f in "$rsi_dir/bin"/*; do
    base=$(basename -- "$f")
    if [ -L "$f" ] && [ -z "${release_bins[$base]+x}" ]; then
      # Remove stale symlinks pointing at ../current/<bin> when switching to a release without that binary.
      case "$(readlink -- "$f" || true)" in
        "../current/$base") rm -f -- "$f" ;;
      esac
    fi
  done
  shopt -u nullglob

  if [ ! -L "$rsi_dir/web" ] && [ -e "$rsi_dir/web" ]; then
    chmod -R u+rwX "$rsi_dir/web" 2>/dev/null || true
    rm -rf -- "$rsi_dir/web.pre-symlink"
    mv -- "$rsi_dir/web" "$rsi_dir/web.pre-symlink"
  fi
  if [ -f "$release_dir/web.tar.gz" ] && [ -d "$rsi_dir/web-extracted/$tag" ]; then
    atomic_symlink "web-extracted/$tag" "$rsi_dir/web"
  elif [ -d "$release_dir/web/dist" ]; then
    atomic_symlink "current/web/dist" "$rsi_dir/web"
  elif [ -d "$release_dir/web" ]; then
    atomic_symlink "current/web" "$rsi_dir/web"
  fi
}

bootstrap_legacy_live_release() {
  local out_var=$1
  if [ -e "$rsi_dir/current" ] && [ ! -L "$rsi_dir/current" ]; then
    die_refuse "$rsi_dir/current exists and is not a symlink"
  fi

  local need_adopt=0 cur_is_link=0
  if [ ! -L "$rsi_dir/current" ]; then
    need_adopt=1
  else
    cur_is_link=1
    if [ -f "$rsi_dir/bin/spawn" ] && [ ! -L "$rsi_dir/bin/spawn" ]; then
      # Live layout was updated outside releases/ (e.g., deploy.sh install_build).
      need_adopt=1
    fi
  fi
  [ "$need_adopt" -eq 1 ] || return 0

  local -a live_bins=()
  local f base has_live_web=0
  if [ -d "$rsi_dir/bin" ]; then
    shopt -s nullglob
    for f in "$rsi_dir/bin"/*; do
      base=$(basename -- "$f")
      case "$base" in
        *.staged | *.prev | *.tmp.* | .*) continue ;;
      esac
      if [ -f "$f" ] && [ -x "$f" ] && { [ "$cur_is_link" -eq 1 ] || [ ! -L "$f" ]; }; then
        live_bins+=("$f")
      fi
    done
    shopt -u nullglob
  fi
  if [ -d "$rsi_dir/web" ] && { [ "$cur_is_link" -eq 1 ] || [ ! -L "$rsi_dir/web" ]; }; then
    has_live_web=1
  fi

  if [ "${#live_bins[@]}" -eq 0 ] && [ "$has_live_web" -eq 0 ]; then
    return 0
  fi
  if [ "${#live_bins[@]}" -eq 0 ]; then
    die_refuse "cannot snapshot legacy layout without executable binaries under $rsi_dir/bin"
  fi

  local stamp base_tag snap_tag seq=0
  stamp=$(compact_utc_from_epoch "$(now_epoch)")
  base_tag="pre-upgrade-${stamp}"
  snap_tag=$base_tag
  while [ -e "$rsi_dir/releases/$snap_tag" ]; do
    seq=$((seq + 1))
    snap_tag="${base_tag}-${seq}"
  done

  mkdir -p "$rsi_dir/releases"
  local snap_tmp="$rsi_dir/releases/.${snap_tag}.tmp.$$"
  rm -rf -- "$snap_tmp"
  mkdir -m 0755 -p "$snap_tmp"

  for f in "${live_bins[@]}"; do
    base=$(basename -- "$f")
    cp -pL -- "$f" "$snap_tmp/$base"
    chmod 0755 "$snap_tmp/$base"
  done
  if [ "$has_live_web" -eq 1 ]; then
    mkdir -m 0755 -p "$snap_tmp/web"
    cp -aL -- "$rsi_dir/web" "$snap_tmp/web/dist"
    chmod -R u-s,g-s,u+rwX,go+rX,go-w "$snap_tmp/web"
  fi
  if ! (
    cd "$snap_tmp"
    find . -type f ! -name 'SHA256SUMS' | sed 's#^\./##' | LC_ALL=C sort | xargs -r sha256sum >SHA256SUMS
  ); then
    chmod -R u+rwX "$snap_tmp" 2>/dev/null || true
    rm -rf -- "$snap_tmp"
    die_refuse "failed to generate SHA256SUMS for legacy snapshot $snap_tag"
  fi

  mv -T -- "$snap_tmp" "$rsi_dir/releases/$snap_tag"
  atomic_symlink "releases/$snap_tag" "$rsi_dir/current"
  say "adopted existing non-symlink live layout into releases/$snap_tag"
  eval "$out_var=\"\$snap_tag\""
}

json_array_from_args() {
  if [ "$#" -eq 0 ]; then
    printf '[]'
  else
    printf '%s\n' "$@" | jq -R . | jq -s .
  fi
}

write_upgrade_state() {
  local status=$1 role_val=$2 cur_ver=$3 prev_ver=$4 started_iso=$5 started_ep=$6
  local watch_until=$7 consec_fail=$8 rb_from=$9 reason_raw=${10}
  local now_ep now_iso clean_reason units_json tmp
  now_ep=$(now_epoch)
  now_iso=$(iso_from_epoch "$now_ep")
  clean_reason=$(sanitize_text "$reason_raw")
  units_json=$(json_array_from_args ${STATE_UNITS[@]+"${STATE_UNITS[@]}"})
  tmp="$rsi_dir/.upgrade-state.json.tmp.$$"
  jq -n \
    --argjson schema_version 1 \
    --arg status "$status" \
    --arg role "$role_val" \
    --arg current_version "$cur_ver" \
    --arg previous_version "$prev_ver" \
    --arg started_at "$started_iso" \
    --argjson started_at_epoch "$started_ep" \
    --arg updated_at "$now_iso" \
    --argjson updated_at_epoch "$now_ep" \
    --argjson watch_window_until "$watch_until" \
    --argjson consecutive_failures "$consec_fail" \
    --arg rolled_back_from "$rb_from" \
    --argjson units "$units_json" \
    --arg reason "$clean_reason" \
    '{
      schema_version: $schema_version,
      status: $status,
      role: $role,
      current_version: $current_version,
      previous_version: $previous_version,
      started_at: $started_at,
      started_at_epoch: $started_at_epoch,
      updated_at: $updated_at,
      updated_at_epoch: $updated_at_epoch,
      watch_window_until: $watch_window_until,
      consecutive_failures: $consecutive_failures,
      rolled_back_from: $rolled_back_from,
      units: $units,
      reason: $reason
    }' >"$tmp"
  chmod 0644 "$tmp"
  mv -f -- "$tmp" "$rsi_dir/upgrade-state.json"
}

append_upgrade_audit() {
  local actor=$1 event=$2 role_val=$3 from_ver=$4 to_ver=$5 reason_raw=$6
  local now_ep now_iso clean_reason
  now_ep=$(now_epoch)
  now_iso=$(iso_from_epoch "$now_ep")
  clean_reason=$(sanitize_text "$reason_raw")
  jq -cn \
    --arg timestamp "$now_iso" \
    --argjson timestamp_epoch "$now_ep" \
    --arg actor "$actor" \
    --arg event "$event" \
    --arg role "$role_val" \
    --arg from_version "$from_ver" \
    --arg to_version "$to_ver" \
    --arg reason "$clean_reason" \
    '{
      timestamp: $timestamp,
      timestamp_epoch: $timestamp_epoch,
      actor: $actor,
      event: $event,
      role: $role,
      from_version: $from_version,
      to_version: $to_version,
      reason: $reason
    }' >>"$rsi_dir/upgrade-audit.jsonl"
}

resolve_units_for_role() {
  local target_role=$1 out_name=$2 mode=${3:-active}
  local -a sysctl_arr=() raw_units=() resolved=()
  split_cmd_array "$systemctl_cmd" sysctl_arr

  local csv
  if [ "$units_set" -eq 1 ]; then
    csv=$units_csv
  elif [ "$target_role" = "control-plane" ]; then
    csv=$control_units_default
  else
    csv=$worker_units_default
  fi

  local item load_state
  IFS=',' read -r -a raw_units <<<"$csv"
  for item in "${raw_units[@]}"; do
    item=${item#"${item%%[![:space:]]*}"}
    item=${item%"${item##*[![:space:]]}"}
    [ -n "$item" ] || continue
    if [ "$mode" = "active" ]; then
      # Only manage currently active units; never start units intentionally stopped by the operator.
      if ! "${sysctl_arr[@]}" is-active --quiet "$item" 2>/dev/null; then
        continue
      fi
    elif [ "$units_set" -eq 0 ]; then
      load_state=$("${sysctl_arr[@]}" show -p LoadState --value "$item" 2>/dev/null || true)
      if [ "$load_state" = "not-found" ]; then
        continue
      fi
    fi
    resolved+=("$item")
  done
  if [ "${#resolved[@]}" -eq 0 ]; then
    eval "$out_name=()"
  else
    eval "$out_name=(\"\${resolved[@]}\")"
  fi
}

assert_units_db_blind() {
  [ "$db_blind" -eq 1 ] || return 0
  if [ "$role" = "control-plane" ] && [ "$skip_init_db" -eq 0 ]; then
    die_refuse "--db-blind refuses control-plane upgrade unless --skip-init-db is set"
  fi

  local -a sysctl_arr=()
  split_cmd_array "$systemctl_cmd" sysctl_arr
  local unit exec_pre frag_path dropins path file_lines
  for unit in "$@"; do
    if ! exec_pre=$("${sysctl_arr[@]}" show -p ExecStartPre --value "$unit" 2>/dev/null); then
      die_refuse "--db-blind failed to inspect ExecStartPre for $unit"
    fi
    frag_path=$("${sysctl_arr[@]}" show -p FragmentPath --value "$unit" 2>/dev/null || true)
    dropins=$("${sysctl_arr[@]}" show -p DropInPaths --value "$unit" 2>/dev/null || true)
    file_lines=""
    for path in $frag_path $dropins; do
      if [ -n "$path" ] && [ -f "$path" ]; then
        file_lines+=$'\n'$(grep -E '^[[:space:]]*ExecStartPre[[:space:]]*=' "$path" 2>/dev/null || true)
      fi
    done
    if printf '%s\n%s\n' "$exec_pre" "$file_lines" |
       grep -Eq '(^|[^A-Za-z0-9_-])init-db([^A-Za-z0-9_-]|$)'; then
      die_refuse "--db-blind refuses $unit: ExecStartPre invokes init-db"
    fi
  done
}

unit_is_cleanly_stopped() {
  local unit=$1
  local -a sysctl_arr=()
  split_cmd_array "$systemctl_cmd" sysctl_arr
  local active_state sub_state result
  active_state=$("${sysctl_arr[@]}" show -p ActiveState --value "$unit" 2>/dev/null || echo "")
  sub_state=$("${sysctl_arr[@]}" show -p SubState --value "$unit" 2>/dev/null || echo "")
  result=$("${sysctl_arr[@]}" show -p Result --value "$unit" 2>/dev/null || echo "")
  [ "$active_state" = "inactive" ] && [ "$sub_state" = "dead" ] && [ "$result" = "success" ]
}

filter_out_cleanly_stopped_units() {
  local arr_name=$1
  local -a orig=() kept=()
  eval "orig=(\${${arr_name}[@]+\"\${${arr_name}[@]}\"})"
  local u
  for u in ${orig[@]+"${orig[@]}"}; do
    if ! unit_is_cleanly_stopped "$u"; then
      kept+=("$u")
    fi
  done
  if [ "${#kept[@]}" -eq 0 ]; then
    eval "$arr_name=()"
  else
    eval "$arr_name=(\"\${kept[@]}\")"
  fi
}

restart_units() {
  local -a sysctl_arr=()
  split_cmd_array "$systemctl_cmd" sysctl_arr
  local unit
  for unit in "$@"; do
    # Isolate each unit restart from Wants=/Requires= so restarting e.g. spawn-integrate never pulls up an owner-stopped spawn-loop.
    if ! run_without_lock_fd "${sysctl_arr[@]}" restart --job-mode=ignore-requirements "$unit"; then
      LAST_PROBE_REASON="systemctl restart $unit failed"
      return 1
    fi
  done
  return 0
}

unit_is_healthy() {
  local unit=$1
  local -a sysctl_arr=()
  split_cmd_array "$systemctl_cmd" sysctl_arr
  if ! "${sysctl_arr[@]}" is-active --quiet "$unit"; then
    LAST_PROBE_REASON="unit $unit is not active"
    return 1
  fi
  local active_state sub_state nrestarts
  active_state=$("${sysctl_arr[@]}" show -p ActiveState --value "$unit" 2>/dev/null || echo "")
  if [ "$active_state" != "active" ]; then
    LAST_PROBE_REASON="unit $unit ActiveState=${active_state:-unknown} (expected active)"
    return 1
  fi
  sub_state=$("${sysctl_arr[@]}" show -p SubState --value "$unit" 2>/dev/null || echo "")
  if [ "$sub_state" != "running" ]; then
    LAST_PROBE_REASON="unit $unit SubState=${sub_state:-unknown} (expected running)"
    return 1
  fi
  nrestarts=$("${sysctl_arr[@]}" show -p NRestarts --value "$unit" 2>/dev/null || echo "")
  if [[ ! "$nrestarts" =~ ^[0-9]+$ ]] || [ "$nrestarts" -ne 0 ]; then
    LAST_PROBE_REASON="unit $unit NRestarts=${nrestarts:-unknown} (expected 0)"
    return 1
  fi
  return 0
}

http_probe() {
  local label=$1 url=$2 expect_csv=$3
  if [ "$url" = "off" ] || [ -z "$url" ]; then
    return 0
  fi
  local -a curl_arr=()
  split_cmd_array "$curl_cmd" curl_arr
  local code
  if ! code=$(run_without_lock_fd "${curl_arr[@]}" -sS -o /dev/null -w '%{http_code}' \
    --connect-timeout "$probe_timeout" --max-time "$probe_timeout" "$url" 2>/dev/null); then
    LAST_PROBE_REASON="$label HTTP probe failed at $url"
    return 1
  fi
  if [[ ",$expect_csv," != *",$code,"* ]]; then
    LAST_PROBE_REASON="$label returned HTTP $code at $url (expected one of $expect_csv)"
    return 1
  fi
  return 0
}

unit_list_has() {
  local needle=$1
  shift
  local u
  for u in "$@"; do
    [ "$u" = "$needle" ] && return 0
  done
  return 1
}

probe_role_health() {
  local target_role=$1 retries=$2 interval=$3
  shift 3
  local -a units=("$@")
  if [ "${#units[@]}" -eq 0 ]; then
    return 0
  fi
  local attempt ok unit check_serve=0 check_gw=0

  if [ "$target_role" = "control-plane" ]; then
    if unit_list_has "spawn-serve.service" "${units[@]}"; then
      check_serve=1
    fi
    if unit_list_has "spawn-node-gateway.service" "${units[@]}"; then
      check_gw=1
    fi
  fi

  for ((attempt = 1; attempt <= retries; attempt++)); do
    ok=1
    LAST_PROBE_REASON=""
    for unit in "${units[@]}"; do
      if ! unit_is_healthy "$unit"; then
        ok=0
        break
      fi
    done
    if [ "$ok" -eq 1 ] && [ "$target_role" = "control-plane" ]; then
      if [ "$check_serve" -eq 1 ] && ! http_probe "serve" "$serve_url" "$serve_expect_codes"; then
        ok=0
      elif [ "$check_gw" -eq 1 ] && ! http_probe "node-gateway" "$gateway_url" "$gateway_expect_codes"; then
        ok=0
      fi
    fi
    if [ "$ok" -eq 1 ] && [ -n "${SPAWN_UPGRADE_HEALTH_CMD:-}" ]; then
      if ! run_without_lock_fd bash -c "$SPAWN_UPGRADE_HEALTH_CMD"; then
        LAST_PROBE_REASON="custom SPAWN_UPGRADE_HEALTH_CMD failed"
        ok=0
      fi
    fi
    if [ "$ok" -eq 1 ]; then
      return 0
    fi
    if [ "$attempt" -lt "$retries" ] && [ "$interval" -gt 0 ]; then
      sleep "$interval"
    fi
  done
  return 1
}

verify_worker_settle() {
  local settle_secs=$1
  shift
  local -a units=("$@")
  [ "$settle_secs" -gt 0 ] || return 0
  [ "${#units[@]}" -gt 0 ] || return 0

  say "verifying worker stability window (${settle_secs}s)"
  local tick unit
  for ((tick = 1; tick <= settle_secs; tick++)); do
    sleep 1
    for unit in "${units[@]}"; do
      if ! unit_is_healthy "$unit"; then
        LAST_PROBE_REASON="worker stability window failed at ${tick}/${settle_secs}s: $LAST_PROBE_REASON"
        return 1
      fi
    done
    if [ -n "${SPAWN_UPGRADE_HEALTH_CMD:-}" ]; then
      if ! run_without_lock_fd bash -c "$SPAWN_UPGRADE_HEALTH_CMD"; then
        LAST_PROBE_REASON="worker stability window failed at ${tick}/${settle_secs}s: custom SPAWN_UPGRADE_HEALTH_CMD failed"
        return 1
      fi
    fi
  done
  return 0
}

release_has_verified_feature_snapshots() {
  local release_dir=$1
  local sums_file="$release_dir/SHA256SUMS"
  [ -f "$sums_file" ] && [ ! -L "$sums_file" ] || return 1

  local has_cov=0 has_reg=0 line rel
  while IFS= read -r line || [ -n "$line" ]; do
    [[ "$line" != *$'\r' ]] || return 1
    [[ "$line" =~ ^[[:space:]]*$ ]] && continue
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" =~ ^([0-9a-fA-F]{64})\ ([ \*])(.+)$ ]] || continue
    rel=${BASH_REMATCH[3]}
    while [[ "$rel" == ./* ]]; do
      rel=${rel#./}
    done
    case "$rel" in
      feature-coverage.json) has_cov=1 ;;
      registry.json) has_reg=1 ;;
    esac
  done <"$sums_file"

  [ "$has_cov" -eq 1 ] && [ "$has_reg" -eq 1 ] || return 1
  [ -f "$release_dir/feature-coverage.json" ] && [ ! -L "$release_dir/feature-coverage.json" ] && [ -s "$release_dir/feature-coverage.json" ] || return 1
  [ -f "$release_dir/registry.json" ] && [ ! -L "$release_dir/registry.json" ] && [ -s "$release_dir/registry.json" ] || return 1
  return 0
}

sync_control_plane_feature_snapshot_dropins() {
  local tag=$1 target_role=$2 active_unit_count=${3:-0}
  [ "$target_role" = "control-plane" ] || return 0

  local release_dir="$rsi_dir/releases/$tag"
  local rsi_clean=${rsi_dir%/}
  [ -n "$rsi_clean" ] || rsi_clean="/"
  local dropin_rsi_dir
  if [ -n "${HOME-}" ] && [ "$rsi_clean" = "${HOME%/}/.local/share/spawn-rsi" ]; then
    dropin_rsi_dir='%h/.local/share/spawn-rsi'
  else
    dropin_rsi_dir=$rsi_clean
  fi
  case "$dropin_rsi_dir" in
    *$'\n'* | *$'\r'*)
      LAST_PROBE_REASON="invalid newline/CR in rsi_dir for control-plane snapshot drop-in"
      return 1
      ;;
  esac

  local want_dropin=0
  if release_has_verified_feature_snapshots "$release_dir"; then
    want_dropin=1
  fi

  local changed=0 unit dropin_dir dropin_file tmp
  for unit in spawn-serve.service spawn-core.service; do
    dropin_dir="$unit_dir/${unit}.d"
    dropin_file="$dropin_dir/50-feature-coverage-snapshot.conf"
    if [ "$want_dropin" -eq 1 ]; then
      if ! mkdir -p "$dropin_dir"; then
        LAST_PROBE_REASON="failed to create drop-in dir $dropin_dir"
        return 1
      fi
      tmp="$dropin_dir/.50-feature-coverage-snapshot.conf.tmp.$$"
      if ! {
        printf '# Managed by deploy/self-upgrade.sh (--role control-plane).\n'
        printf '# Enabled only when verified current/feature-coverage.json and current/registry.json exist.\n'
        printf '[Service]\n'
        printf 'Environment=SPAWN_FEATURE_COVERAGE_SNAPSHOT=%s/current/feature-coverage.json\n' "$dropin_rsi_dir"
        printf 'Environment=SPAWN_FEATURE_REGISTRY=%s/current/registry.json\n' "$dropin_rsi_dir"
      } >"$tmp"; then
        rm -f -- "$tmp"
        LAST_PROBE_REASON="failed to write temp drop-in $tmp"
        return 1
      fi
      if [ ! -f "$dropin_file" ] || [ -L "$dropin_file" ] || ! cmp -s "$tmp" "$dropin_file"; then
        if ! { chmod 0644 "$tmp" && mv -f -- "$tmp" "$dropin_file"; }; then
          rm -f -- "$tmp"
          LAST_PROBE_REASON="failed to install drop-in $dropin_file"
          return 1
        fi
        changed=1
      else
        rm -f -- "$tmp"
      fi
    else
      if [ -e "$dropin_file" ] || [ -L "$dropin_file" ]; then
        if ! rm -f -- "$dropin_file"; then
          LAST_PROBE_REASON="failed to remove stale drop-in $dropin_file"
          return 1
        fi
        rmdir -- "$dropin_dir" 2>/dev/null || true
        changed=1
      fi
    fi
  done

  if [ "$changed" -eq 1 ]; then
    local -a sysctl_arr=()
    split_cmd_array "$systemctl_cmd" sysctl_arr
    if [ "$active_unit_count" -gt 0 ]; then
      if ! run_without_lock_fd "${sysctl_arr[@]}" daemon-reload; then
        LAST_PROBE_REASON="systemctl daemon-reload failed"
        return 1
      fi
    else
      run_without_lock_fd "${sysctl_arr[@]}" daemon-reload >/dev/null 2>&1 || true
    fi
  fi
  return 0
}

perform_rollback() {
  local actor=$1 from_tag=$2 prev_tag=$3 target_role=$4 reason_raw=$5
  shift 5
  local -a units=("$@")
  local started_ep started_iso
  started_ep=$(now_epoch)
  started_iso=$(iso_from_epoch "$started_ep")

  if [ -z "$prev_tag" ] || ! validate_release_tag "$prev_tag" || [ ! -d "$rsi_dir/releases/$prev_tag" ]; then
    write_upgrade_state "rollback_failed" "$target_role" "$from_tag" "$prev_tag" \
      "$started_iso" "$started_ep" 0 0 "$from_tag" "rollback unavailable: missing release '$prev_tag' ($reason_raw)"
    append_upgrade_audit "$actor" "rollback_failed" "$target_role" "$from_tag" "$prev_tag" "$reason_raw"
    return 1
  fi

  warn "rolling back ($actor): $from_tag -> $prev_tag ($reason_raw)"
  if [ -f "$rsi_dir/releases/$prev_tag/web.tar.gz" ] && [ ! -d "$rsi_dir/web-extracted/$prev_tag" ]; then
    if ! (validate_web_tarball "$rsi_dir/releases/$prev_tag/web.tar.gz" && prepare_web_assets "$prev_tag"); then
      write_upgrade_state "rollback_failed" "$target_role" "$from_tag" "$prev_tag" \
        "$started_iso" "$started_ep" 0 0 "$from_tag" "rollback web restore failed: $reason_raw"
      append_upgrade_audit "$actor" "rollback_failed" "$target_role" "$from_tag" "$prev_tag" "$reason_raw"
      return 1
    fi
  fi
  if ! activate_release_links "$prev_tag" 0; then
    write_upgrade_state "rollback_failed" "$target_role" "$from_tag" "$prev_tag" \
      "$started_iso" "$started_ep" 0 0 "$from_tag" "rollback link restore failed: $reason_raw"
    append_upgrade_audit "$actor" "rollback_failed" "$target_role" "$from_tag" "$prev_tag" "$reason_raw"
    return 1
  fi
  # Pin previous symlink to the rolled-back target so repeat runs remain idempotent/debounced.
  atomic_symlink "releases/$prev_tag" "$rsi_dir/previous"

  # Never start units that the owner cleanly stopped prior to or during the rollback window.
  local -a rollback_units=(${units[@]+"${units[@]}"})
  filter_out_cleanly_stopped_units rollback_units

  if ! sync_control_plane_feature_snapshot_dropins "$prev_tag" "$target_role" "${#rollback_units[@]}" || ! restart_units ${rollback_units[@]+"${rollback_units[@]}"} || ! probe_role_health "$target_role" "$probe_retries" "$probe_interval" ${rollback_units[@]+"${rollback_units[@]}"}; then
    local full_reason="$reason_raw; rollback recovery failed: ${LAST_PROBE_REASON:-restart error}"
    warn "rollback recovery failed ($actor): $full_reason"
    write_upgrade_state "rollback_failed" "$target_role" "$prev_tag" "$prev_tag" \
      "$started_iso" "$started_ep" 0 0 "$from_tag" "$full_reason"
    append_upgrade_audit "$actor" "rollback_failed" "$target_role" "$from_tag" "$prev_tag" "$full_reason"
    return 1
  fi

  write_upgrade_state "rolled_back" "$target_role" "$prev_tag" "$prev_tag" \
    "$started_iso" "$started_ep" 0 0 "$from_tag" "$reason_raw"
  append_upgrade_audit "$actor" "rolled_back" "$target_role" "$from_tag" "$prev_tag" "$reason_raw"
  say "rolled back cleanly to $prev_tag"
  return 0
}

run_pre_backup() {
  [ "$skip_backup" -eq 0 ] || return 0
  if [ -n "$pre_backup_cmd" ]; then
    say "running pre-upgrade backup hook"
    run_without_lock_fd_redacted all bash -c "$pre_backup_cmd" || die_refuse "pre-upgrade backup command failed"
  elif [ -n "$backup_unit" ]; then
    local -a sysctl_arr=()
    split_cmd_array "$systemctl_cmd" sysctl_arr
    say "starting pre-upgrade backup unit $backup_unit"
    run_without_lock_fd_redacted all "${sysctl_arr[@]}" start "$backup_unit" ||
      die_refuse "pre-upgrade backup unit $backup_unit failed"
  fi
}

drain_worker_before_switch() {
  [ "$skip_drain" -eq 0 ] || return 0
  local -a sysctl_arr=()
  split_cmd_array "$systemctl_cmd" sysctl_arr
  local should_drain=0
  if [ "${SPAWN_UPGRADE_DRAIN_ALWAYS:-0}" = "1" ] || "${sysctl_arr[@]}" is-active --quiet spawn-integrate.service; then
    should_drain=1
  fi
  [ "$should_drain" -eq 1 ] || return 0

  say "draining spawn-integrate before switch (timeout=${drain_timeout}s; spawn-hc trees stay untouched)"
  if [ -n "${SPAWN_UPGRADE_DRAIN_CMD:-}" ]; then
    run_without_lock_fd_redacted all timeout --kill-after=30 "$drain_timeout" bash -c "$SPAWN_UPGRADE_DRAIN_CMD" ||
      die_refuse "worker epoch drain hook failed"
  elif [ -x "$rsi_dir/bin/spawn" ]; then
    run_without_lock_fd_redacted all timeout --kill-after=30 "$drain_timeout" "$rsi_dir/bin/spawn" integrate --drain ||
      die_refuse "spawn integrate --drain failed or timed out"
  else
    die_refuse "spawn-integrate is active but no executable $rsi_dir/bin/spawn is present to drain"
  fi
}

run_init_db() {
  [ "$role" = "control-plane" ] || return 0
  [ "$skip_init_db" != "1" ] || return 0
  say "running schema migration check (spawn init-db)"
  if [ -n "${SPAWN_UPGRADE_INIT_DB_CMD:-}" ]; then
    run_without_lock_fd_redacted all bash -c "$SPAWN_UPGRADE_INIT_DB_CMD" || {
      LAST_PROBE_REASON="SPAWN_UPGRADE_INIT_DB_CMD failed"
      return 1
    }
  else
    run_without_lock_fd_redacted stderr-only "$rsi_dir/bin/spawn" init-db || {
      LAST_PROBE_REASON="spawn init-db failed"
      return 1
    }
  fi
}

parse_self_upgrade_args() {
  local need_val
  need_val() {
    [ "$#" -ge 2 ] && [ -n "$2" ] || die_usage "$1 requires a non-empty value"
  }
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --rsi-dir) need_val "$@"; rsi_dir=$2; shift ;;
      --unit-dir) need_val "$@"; unit_dir=$2; shift ;;
      --role) need_val "$@"; role=$2; shift ;;
      --version) need_val "$@"; version=$2; shift ;;
      --rollback) rollback=1 ;;
      --reason) [ "$#" -ge 2 ] || die_usage "--reason requires a value"; reason=$2; shift ;;
      --dry-run) dry_run=1 ;;
      --units) need_val "$@"; units_csv=$2; units_set=1; shift ;;
      --db-blind) db_blind=1 ;;
      --allow-no-active-units) allow_no_active_units=1 ;;
      --serve-url) need_val "$@"; serve_url=$2; shift ;;
      --serve-expect-codes) need_val "$@"; serve_expect_codes=$2; shift ;;
      --gateway-url) need_val "$@"; gateway_url=$2; shift ;;
      --gateway-expect-codes) need_val "$@"; gateway_expect_codes=$2; shift ;;
      --pre-backup-cmd) need_val "$@"; pre_backup_cmd=$2; shift ;;
      --backup-unit) need_val "$@"; backup_unit=$2; shift ;;
      --skip-backup) skip_backup=1 ;;
      --skip-init-db) skip_init_db=1 ;;
      --skip-drain) skip_drain=1 ;;
      --drain-timeout) need_val "$@"; drain_timeout=$2; shift ;;
      --probe-retries) need_val "$@"; probe_retries=$2; shift ;;
      --probe-interval) need_val "$@"; probe_interval=$2; shift ;;
      --probe-timeout) need_val "$@"; probe_timeout=$2; shift ;;
      --worker-settle-secs) need_val "$@"; worker_settle_secs=$2; shift ;;
      --watch-window-secs) need_val "$@"; watch_window_secs=$2; shift ;;
      --systemctl) need_val "$@"; systemctl_cmd=$2; shift ;;
      --curl) need_val "$@"; curl_cmd=$2; shift ;;
      -h | --help) usage ;;
      *) die_usage "unknown argument: $1" ;;
    esac
    shift
  done

  case "$role" in
    control-plane | worker-node) ;;
    *) die_usage "--role must be 'control-plane' or 'worker-node'" ;;
  esac
  if [ "$rollback" -eq 1 ] && [ -n "$version" ]; then
    die_usage "cannot combine --rollback with --version"
  fi
  if [ "$rollback" -eq 0 ] && [ -z "$version" ]; then
    die_usage "give either --version <tag> or --rollback"
  fi
  require_nonneg_int "--drain-timeout" "$drain_timeout" 1
  require_nonneg_int "--probe-retries" "$probe_retries" 1
  require_nonneg_int "--probe-interval" "$probe_interval" 0
  require_nonneg_int "--probe-timeout" "$probe_timeout" 1
  require_nonneg_int "--worker-settle-secs" "$worker_settle_secs" 0
  require_nonneg_int "--watch-window-secs" "$watch_window_secs" 0
}

read_state_units_if_available() {
  local out_var=$1 state_file="$rsi_dir/upgrade-state.json"
  [ -f "$state_file" ] || return 1
  local -a loaded=()
  local u
  while IFS= read -r u; do
    [ -n "$u" ] && loaded+=("$u")
  done < <(jq -r '(.units // [])[]' "$state_file" 2>/dev/null || true)
  [ "${#loaded[@]}" -gt 0 ] || return 1
  eval "$out_var=(\"\${loaded[@]}\")"
  return 0
}

main() {
  init_upgrade_defaults
  trap cleanup_self_upgrade_tmp EXIT
  parse_self_upgrade_args "$@"

  local tool
  for tool in jq sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || die_refuse "required tool not found in PATH: $tool"
  done

  [ -d "$rsi_dir" ] || die_refuse "rsi directory not found: $rsi_dir"
  eval "exec ${LOCK_FD}>>\"\$rsi_dir/self-upgrade.lock\""
  flock -n "$LOCK_FD" || die_refuse "another upgrade or watchdog holds $rsi_dir/self-upgrade.lock"

  local -a units=()
  if [ "$rollback" -eq 1 ]; then
    if [ "$units_set" -eq 1 ]; then
      resolve_units_for_role "$role" units installed
    elif ! read_state_units_if_available units; then
      resolve_units_for_role "$role" units installed
    fi
    filter_out_cleanly_stopped_units units
  else
    resolve_units_for_role "$role" units active
  fi

  if [ "$rollback" -eq 0 ] && [ "${#units[@]}" -eq 0 ] && [ "$allow_no_active_units" -eq 0 ]; then
    die_refuse "no active units found for role $role; pass --allow-no-active-units to override"
  fi
  STATE_UNITS=(${units[@]+"${units[@]}"})

  assert_units_db_blind ${units[@]+"${units[@]}"}

  local cur_tag prev_tag
  cur_tag=$(link_release_tag "$rsi_dir/current")
  prev_tag=$(link_release_tag "$rsi_dir/previous")

  if [ "$rollback" -eq 1 ]; then
    [ -n "$prev_tag" ] || die_refuse "no previous symlink under $rsi_dir"
    validate_release_tag "$prev_tag" || die_refuse "invalid previous release tag '$prev_tag'"
    [ -d "$rsi_dir/releases/$prev_tag" ] || die_refuse "previous release directory missing: $rsi_dir/releases/$prev_tag"
    verify_release "$prev_tag" "$role"
    if [ "$dry_run" -eq 1 ]; then
      say "dry-run ok: would roll back ${cur_tag:-none} -> $prev_tag (role=$role units=${units[*]:-none})"
      exit 0
    fi
    local rb_reason=${reason:-"manual --rollback"}
    if perform_rollback "self-upgrade" "${cur_tag:-none}" "$prev_tag" "$role" "$rb_reason" ${units[@]+"${units[@]}"}; then
      exit 0
    else
      exit 4
    fi
  fi

  verify_release "$version" "$role"

  if [ "$dry_run" -eq 1 ]; then
    say "dry-run ok: verified release $version for role $role (units: ${units[*]:-none})"
    exit 0
  fi

  prepare_web_assets "$version"
  run_pre_backup
  if [ "$role" = "worker-node" ]; then
    drain_worker_before_switch
  fi
  bootstrap_legacy_live_release cur_tag

  local started_ep started_iso prev_for_state watch_until
  started_ep=$(now_epoch)
  started_iso=$(iso_from_epoch "$started_ep")
  if [ -n "$cur_tag" ] && [ "$cur_tag" != "$version" ] && [ -d "$rsi_dir/releases/$cur_tag" ]; then
    prev_for_state=$cur_tag
  else
    prev_for_state=$prev_tag
  fi

  write_upgrade_state "upgrading" "$role" "$version" "$prev_for_state" \
    "$started_iso" "$started_ep" 0 0 "" "activating $version"
  append_upgrade_audit "self-upgrade" "upgrading" "$role" "${cur_tag:-none}" "$version" "activating $version"

  activate_release_links "$version" 1

  local fail_reason=""
  if ! sync_control_plane_feature_snapshot_dropins "$version" "$role" "${#units[@]}"; then
    fail_reason=${LAST_PROBE_REASON:-"control-plane feature snapshot drop-in sync failed"}
  elif ! run_init_db; then
    fail_reason=${LAST_PROBE_REASON:-"init-db failed"}
  elif ! restart_units ${units[@]+"${units[@]}"}; then
    fail_reason=${LAST_PROBE_REASON:-"unit restart failed"}
  elif ! probe_role_health "$role" "$probe_retries" "$probe_interval" ${units[@]+"${units[@]}"}; then
    fail_reason=${LAST_PROBE_REASON:-"health probe failed"}
  elif [ "$role" = "worker-node" ] && ! verify_worker_settle "$worker_settle_secs" ${units[@]+"${units[@]}"}; then
    fail_reason=${LAST_PROBE_REASON:-"worker stability window failed"}
  fi

  if [ -n "$fail_reason" ]; then
    if perform_rollback "self-upgrade" "$version" "$prev_for_state" "$role" "$fail_reason" ${units[@]+"${units[@]}"}; then
      exit 3
    else
      exit 4
    fi
  fi

  local finish_ep active_reason state_reason
  finish_ep=$(now_epoch)
  watch_until=$((finish_ep + watch_window_secs))
  active_reason="verified healthy"
  state_reason=""
  if [ "${#units[@]}" -eq 0 ]; then
    active_reason="staged (0 active units; no units restarted or verified)"
    state_reason=$active_reason
  fi
  write_upgrade_state "active" "$role" "$version" "$prev_for_state" \
    "$started_iso" "$started_ep" "$watch_until" 0 "" "$state_reason"
  append_upgrade_audit "self-upgrade" "active" "$role" "${cur_tag:-none}" "$version" "$active_reason"
  if [ "${#units[@]}" -eq 0 ]; then
    say "upgrade staged (0 active units; no units verified): role=$role current=$version previous=${prev_for_state:-none} watch_until=$watch_until"
  else
    say "upgrade active: role=$role current=$version previous=${prev_for_state:-none} watch_until=$watch_until"
  fi
  exit 0
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
