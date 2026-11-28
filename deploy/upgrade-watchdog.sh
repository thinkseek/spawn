#!/usr/bin/env bash
# Independent external rollback watchdog for Spawn self-upgrades (T0.3a).
#
#   deploy/upgrade-watchdog.sh [--rsi-dir <dir>] [--failure-threshold <N>] [options]
#
# Operates completely independently of the upgraded `bin/spawn` binary so a
# crashing or hung Spawn build cannot blind its own recovery path. Uses the shared
# lock (`self-upgrade.lock`), inspects `upgrade-state.json`, yields (`superseded`)
# if an external deploy replaced the managed symlinks, probes active services /
# endpoints, counts consecutive failures across timer ticks, triggers automatic
# rollback (`current -> previous`) at threshold, and debounces repeat
# invocations once `status` transitions out of `active`.

set -euo pipefail

if [ "${BASH_VERSINFO[0]}" -lt 4 ] || { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -lt 4 ]; }; then
  echo "upgrade-watchdog: bash >= 4.4 is required" >&2
  exit 1
fi

WATCHDOG_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
UPGRADE_PROG=upgrade-watchdog
# shellcheck source=deploy/self-upgrade.sh
. "$WATCHDOG_DIR/self-upgrade.sh"

watchdog_usage() {
  cat >&2 <<'EOF'
Usage: deploy/upgrade-watchdog.sh [options]
  --rsi-dir <dir>              RSI directory (default: $SPAWN_RSI_DIR or ~/.local/share/spawn-rsi)
  --failure-threshold <N>      Consecutive unhealthy ticks before rollback (default: 3)
  --always-watch               Ignore watch_window_until expiry while status==active
  --role <control-plane|worker-node>
  --units <csv>                Explicit systemd units to check/restart
  --serve-url <url>            Control-plane serve HTTP URL
  --serve-expect-codes <csv>   Allowed HTTP codes for serve probe
  --gateway-url <url|off>      Control-plane gateway HTTP URL
  --gateway-expect-codes <csv> Allowed HTTP codes for gateway probe
  --probe-timeout <secs>       Per-probe curl timeout (default: 5)
  --probe-retries <N>          Retries during post-rollback verification (default: 3)
  --probe-interval <secs>      Sleep between post-rollback retries (default: 1)
  --systemctl <cmd>            Systemctl command override (default: systemctl --user)
  --curl <cmd>                 Curl command override (default: curl)
EOF
  exit 2
}

resolve_canon_path() {
  readlink -f -- "$1" 2>/dev/null || true
}

deployment_is_superseded() {
  local expected_tag=$1 watch_role=$2
  [ -n "$expected_tag" ] || return 0
  [ -L "$rsi_dir/current" ] || return 0
  [ "$(link_release_tag "$rsi_dir/current")" = "$expected_tag" ] || return 0

  local rel_dir="$rsi_dir/releases/$expected_tag"
  local rel_real
  rel_real=$(resolve_canon_path "$rel_dir")
  [ -n "$rel_real" ] && [ -d "$rel_real" ] || return 0

  [ -L "$rsi_dir/bin/spawn" ] || return 0
  [ "$(resolve_canon_path "$rsi_dir/bin/spawn")" = "$rel_real/spawn" ] || return 0

  local verified_bins b
  verified_bins=$(list_verified_release_bins "$rel_dir") || return 0
  while IFS= read -r b; do
    [ -n "$b" ] || continue
    if [ -e "$rsi_dir/bin/$b" ] || [ -L "$rsi_dir/bin/$b" ]; then
      [ -L "$rsi_dir/bin/$b" ] || return 0
      [ "$(resolve_canon_path "$rsi_dir/bin/$b")" = "$rel_real/$b" ] || return 0
    fi
  done <<<"$verified_bins"

  if [ "$watch_role" = "control-plane" ]; then
    if [ -e "$rsi_dir/web" ] || [ -L "$rsi_dir/web" ]; then
      [ -L "$rsi_dir/web" ] || return 0
      local web_real expected_web_real=""
      web_real=$(resolve_canon_path "$rsi_dir/web")
      if [ -f "$rel_dir/web.tar.gz" ] && [ -d "$rsi_dir/web-extracted/$expected_tag" ]; then
        expected_web_real=$(resolve_canon_path "$rsi_dir/web-extracted/$expected_tag")
      elif [ -d "$rel_dir/web/dist" ]; then
        expected_web_real=$(resolve_canon_path "$rel_dir/web/dist")
      elif [ -d "$rel_dir/web" ]; then
        expected_web_real=$(resolve_canon_path "$rel_dir/web")
      fi
      [ -n "$expected_web_real" ] && [ "$web_real" = "$expected_web_real" ] || return 0
    fi
  fi

  return 1
}

main_watchdog() {
  init_upgrade_defaults
  UPGRADE_PROG=upgrade-watchdog

  local failure_threshold=${SPAWN_WATCHDOG_FAILURE_THRESHOLD:-3}
  local always_watch=${SPAWN_WATCHDOG_ALWAYS:-0}
  local role_override=""

  local need_val
  need_val() {
    [ "$#" -ge 2 ] && [ -n "$2" ] || die_usage "$1 requires a non-empty value"
  }
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --rsi-dir) need_val "$@"; rsi_dir=$2; shift ;;
      --failure-threshold) need_val "$@"; failure_threshold=$2; shift ;;
      --always-watch) always_watch=1 ;;
      --role) need_val "$@"; role_override=$2; shift ;;
      --units) need_val "$@"; units_csv=$2; units_set=1; shift ;;
      --serve-url) need_val "$@"; serve_url=$2; shift ;;
      --serve-expect-codes) need_val "$@"; serve_expect_codes=$2; shift ;;
      --gateway-url) need_val "$@"; gateway_url=$2; shift ;;
      --gateway-expect-codes) need_val "$@"; gateway_expect_codes=$2; shift ;;
      --probe-timeout) need_val "$@"; probe_timeout=$2; shift ;;
      --probe-retries) need_val "$@"; probe_retries=$2; shift ;;
      --probe-interval) need_val "$@"; probe_interval=$2; shift ;;
      --systemctl) need_val "$@"; systemctl_cmd=$2; shift ;;
      --curl) need_val "$@"; curl_cmd=$2; shift ;;
      -h | --help) watchdog_usage ;;
      *) die_usage "unknown argument: $1" ;;
    esac
    shift
  done

  require_nonneg_int "--failure-threshold" "$failure_threshold" 1
  require_nonneg_int "--probe-timeout" "$probe_timeout" 1
  require_nonneg_int "--probe-retries" "$probe_retries" 1
  require_nonneg_int "--probe-interval" "$probe_interval" 0

  command -v jq >/dev/null 2>&1 || die_refuse "required tool not found in PATH: jq"

  [ -d "$rsi_dir" ] || exit 0
  eval "exec ${LOCK_FD}>>\"\$rsi_dir/self-upgrade.lock\""
  if ! flock -n "$LOCK_FD"; then
    say "upgrade lock held; skipping watchdog tick"
    exit 0
  fi

  local state_file="$rsi_dir/upgrade-state.json"
  [ -f "$state_file" ] || exit 0

  local parsed=""
  if ! parsed=$(jq -r '[
      (.status // ""),
      (.role // ""),
      (.current_version // ""),
      (.previous_version // ""),
      (.started_at // ""),
      (.started_at_epoch // 0 | tostring),
      (.watch_window_until // 0 | tostring),
      (.consecutive_failures // 0 | tostring)
    ] | @tsv' "$state_file" 2>/dev/null); then
    warn "ignoring unreadable state file $state_file"
    exit 0
  fi

  local status state_role cur_ver prev_ver started_at started_ep watch_until consec_fail
  IFS=$'\t' read -r status state_role cur_ver prev_ver started_at started_ep watch_until consec_fail <<<"$parsed"

  # Anti-loop debounce: only watch an active post-upgrade window.
  if [ "$status" != "active" ]; then
    exit 0
  fi

  local watch_role=${role_override:-$state_role}
  case "$watch_role" in
    control-plane | worker-node) ;;
    *)
      warn "unrecognized role '$watch_role' in state file; skipping"
      exit 0
      ;;
  esac

  local -a units=()
  if [ "$units_set" -eq 1 ]; then
    resolve_units_for_role "$watch_role" units installed
  elif ! read_state_units_if_available units; then
    resolve_units_for_role "$watch_role" units installed
  fi
  STATE_UNITS=(${units[@]+"${units[@]}"})

  # Respect external deployments (e.g., `deploy.sh install_build`) that replaced managed symlinks.
  if deployment_is_superseded "$cur_ver" "$watch_role"; then
    local sup_reason="superseded by external deployment replacing managed symlinks for ${cur_ver:-unknown}"
    write_upgrade_state "superseded" "$watch_role" "$cur_ver" "$prev_ver" \
      "$started_at" "$started_ep" 0 0 "" "$sup_reason"
    append_upgrade_audit "watchdog" "superseded" "$watch_role" "${cur_ver:-none}" "${cur_ver:-none}" "$sup_reason"
    say "$sup_reason; leaving bin/ and web/ untouched"
    exit 0
  fi

  local now_ep
  now_ep=$(now_epoch)
  if [ "$always_watch" != "1" ] && [[ "$watch_until" =~ ^[0-9]+$ ]] && [ "$now_ep" -gt "$watch_until" ]; then
    exit 0
  fi

  local live_cur live_prev
  live_cur=$(link_release_tag "$rsi_dir/current")
  live_prev=$(link_release_tag "$rsi_dir/previous")
  [ -n "$live_cur" ] || live_cur=$cur_ver
  [ -n "$live_prev" ] || live_prev=$prev_ver

  if [ -z "$live_prev" ] || [ "$live_prev" = "$live_cur" ] || [ ! -d "$rsi_dir/releases/$live_prev" ]; then
    warn "no distinct valid previous release available for $live_cur; skipping rollback check"
    exit 0
  fi

  # Ignore units intentionally and cleanly stopped by the owner (ActiveState=inactive, SubState=dead, Result=success).
  local -a watch_units=(${units[@]+"${units[@]}"})
  filter_out_cleanly_stopped_units watch_units
  if [ "${#watch_units[@]}" -eq 0 ]; then
    if [[ "$consec_fail" =~ ^[0-9]+$ ]] && [ "$consec_fail" -gt 0 ]; then
      write_upgrade_state "active" "$watch_role" "$live_cur" "$live_prev" \
        "$started_at" "$started_ep" "$watch_until" 0 "" ""
    fi
    exit 0
  fi

  # Never run `bin/spawn` in the watchdog: health check + rollback rely strictly on systemd, HTTP, and symlinks.
  if probe_role_health "$watch_role" 1 0 "${watch_units[@]}"; then
    if [[ "$consec_fail" =~ ^[0-9]+$ ]] && [ "$consec_fail" -gt 0 ]; then
      write_upgrade_state "active" "$watch_role" "$live_cur" "$live_prev" \
        "$started_at" "$started_ep" "$watch_until" 0 "" ""
      say "recovered cleanly during watch window ($live_cur); reset consecutive_failures=0"
    fi
    exit 0
  fi

  local next_fail=$((consec_fail + 1))
  local probe_why=${LAST_PROBE_REASON:-"health probe failed"}
  if [ "$next_fail" -lt "$failure_threshold" ]; then
    write_upgrade_state "active" "$watch_role" "$live_cur" "$live_prev" \
      "$started_at" "$started_ep" "$watch_until" "$next_fail" "" "$probe_why"
    append_upgrade_audit "watchdog" "probe_failed" "$watch_role" "$live_cur" "$live_cur" \
      "failure $next_fail/$failure_threshold: $probe_why"
    warn "health check failed ($next_fail/$failure_threshold) for $live_cur: $probe_why"
    exit 0
  fi

  local rb_reason="watchdog threshold reached ($next_fail/$failure_threshold): $probe_why"
  if perform_rollback "watchdog" "$live_cur" "$live_prev" "$watch_role" "$rb_reason" "${watch_units[@]}"; then
    exit 0
  else
    exit 4
  fi
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main_watchdog "$@"
fi
