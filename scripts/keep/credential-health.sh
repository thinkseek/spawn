#!/bin/bash
# credential-health: is each Keep-delivered credential present, current, loaded by its service and
# accepted by its provider, and is no legacy secret env file back? (owner order t475u, 2026-11-17)
#
# One level-triggered check, run every 5 minutes by spawn-credential-health.timer, over the rows of
# credentials.manifest beside it (one per Keep-delivered credential; its header says the fields).
# It reads only names, modes, sizes, times, unit text, each unit's health URL and its status lines
# ("<tag>: <code> <ms> <bytes>"). It never opens a credential, an env file or a scanner source:
# unit text is matched for names (grep -q), never printed.
#
# Each fault class of a row is ONE owner alert (kind credential_health, subject the row's id, key
# credential_health:<id>:<class>:<unix time it started>), recorded through the store's owner-only
# spawn_alert_record like the nightly's: fired once when the class starts, cleared when it ends.
# Nothing else is sent anywhere.
#
#   delivery_missing  /run/keep/<id> absent, empty or not mode 600, once the host has been up longer
#                     than the boot grace (CH_BOOT_GRACE_S, 900 s): after a reboot Keep re-delivers
#                     at the owner's login, so a short gap is normal
#   not_loaded        the delivered file is newer than its unit's start (by > 60 s): the unit holds
#                     an older delivery (a .path unit starts a stopped unit but never restarts a
#                     running one; the fix is systemctl --user restart <unit>)
#   service_down      the unit is not active, or its health URL does not answer 200
#   auth_failure      the running unit's newest status line is 401 or 403 (the provider refused it)
#   call_failure      the running unit's 3 newest status lines are all non-200 (fewer than 3: none)
#   legacy_env        the row's legacy file exists again; or a user unit file or drop-in names it;
#                     or a spawn-* unit (as systemctl --user cat shows it, drop-ins included) has an
#                     EnvironmentFile= naming it (with or without '-', '~/', or '%h/') or an
#                     Environment= setting one of its legacy keys; or the row's own unit has any
#                     EnvironmentFile= or lacks LoadCredential=<id>
#   scanner_source    the row's scanner source and /run/keep/<id> are both missing after the boot
#                     grace: secret-values.py then refuses every value scan (pushes, redaction)
#
# And host-wide checks (subject spawn-units):
#   unit_drift        opt-in via CH_DEPLOY_SYSTEMD_DIR: an installed spawn-* unit file differs from
#                     its deploy/systemd copy (cmp; uninstalled or host-only units are not drift)
#   go_unit_pg_isolation  active when CH_DEPLOY_SYSTEMD_DIR is set or any spawn-node-{edge,gw,out}.service
#                     unit/drop-in is present: asserts zero spawn.env or *DATABASE_URL* references
#
# Test seams (scripts/keep/test-credential-health.sh): CH_MANIFEST, CH_KEEP_DIR, CH_LEGACY_FILE and
# CH_HEALTH_URL (the first row's legacy file and health URL), CH_UNIT_DIR, CH_UPTIME_S,
# CH_BOOT_GRACE_S, CH_DEPLOY_SYSTEMD_DIR, CH_FAKE_ALERTS (a file standing in for the store: open
# keys in, records appended). Without CH_FAKE_ALERTS it needs SPAWN_DATABASE_URL.
#
# Subcommands / read-only modes:
#   (no args)                      periodic 5-minute health check over credentials.manifest
#   --audit-keep-migration [--json]  read-only migration classification of spawn-*.service units
#                                    (no DB, no alerts, never reads .env or /run/keep files)
#   --validate-manifest <path>     dry-run validation of a multi-row manifest (exit 2 on error)
#
# Exit 0, faults or not (periodic / audit modes); 2 on invalid arguments, missing audit dir, or when
# the manifest cannot be read/validated (zero rows checked, zero alerts emitted).
set -u
kind=credential_health
here=$(cd "$(dirname "$0")" && pwd)
controller=$(cd "$here/../.." && pwd)
manifest=${CH_MANIFEST:-$here/credentials.manifest}
keep_dir=${CH_KEEP_DIR:-/run/keep}
unit_dir=${CH_UNIT_DIR:-$HOME/.config/systemd/user}
grace=${CH_BOOT_GRACE_S:-900}
deploy_units=${CH_DEPLOY_SYSTEMD_DIR-}
host_subject=spawn-units
cred_classes="delivery_missing not_loaded service_down auth_failure call_failure legacy_env scanner_source"
row_sep=$'\x1f'

say() { printf 'credential-health: %s\n' "$*"; }
refuse() { say "$* (in $manifest): nothing checked"; exit 2; }
usage_error() {
  say "$* (usage: credential-health.sh [--audit-keep-migration [--json] | --validate-manifest <path>])"
  exit 2
}

mode=check
audit_json=0
while [ "$#" -gt 0 ]; do
  case $1 in
    --audit-keep-migration)
      [ "$mode" = check ] || usage_error "conflicting flags: $1"
      mode=audit
      ;;
    --json)
      [ "$mode" = audit ] || usage_error "--json requires --audit-keep-migration"
      audit_json=1
      ;;
    --validate-manifest)
      [ "$mode" = check ] || usage_error "conflicting flags: $1"
      [ "$#" -ge 2 ] || usage_error "--validate-manifest requires <path>"
      shift
      manifest=$1
      mode=validate
      ;;
    *)
      usage_error "unknown argument: $1"
      ;;
  esac
  shift
done

uptime_s() {
  if [ -n "${CH_UPTIME_S-}" ]; then echo "$CH_UPTIME_S"; else cut -d. -f1 /proc/uptime; fi
}
home_path() { case $1 in "~/"*) printf '%s\n' "$HOME/${1#"~/"}" ;; *) printf '%s\n' "$1" ;; esac; }
trim() { local s=$1; s=${s#"${s%%[![:space:]]*}"}; printf '%s' "${s%"${s##*[![:space:]]}"}"; }

expand_socket_path() {
  local s=$1 rt=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
  case $s in
    "~/"*) printf '%s\n' "$HOME/${s#"~/"}" ;;
    "%t/"*) printf '%s\n' "$rt/${s#"%t/"}" ;;
    *) printf '%s\n' "$s" ;;
  esac
}

valid_health_url() {
  local u=$1
  [[ $u == - || $u =~ ^https?://[^[:space:]]+$ || $u =~ ^unix://(/|~/|%t/)[^[:space:]|]+\|/[^[:space:]|]*$ ]]
}

probe_health_url() {
  local u=$1 rest sock http_path code=""
  case $u in
    unix://*)
      rest=${u#unix://}
      sock=${rest%%|*}
      http_path=${rest#*|}
      if [ "$sock" = "$rest" ] || [ -z "$sock" ] || [[ $http_path != /* ]]; then
        printf '000'
        return 0
      fi
      sock=$(expand_socket_path "$sock")
      if [ -L "$sock" ] || [ ! -S "$sock" ]; then
        printf '000'
        return 0
      fi
      code=$(curl -s -m 5 --connect-timeout 5 --unix-socket "$sock" -o /dev/null -w '%{http_code}' -- "http://localhost${http_path}" 2>/dev/null) || true
      ;;
    *)
      code=$(curl -s -m 5 --connect-timeout 5 -o /dev/null -w '%{http_code}' -- "$u" 2>/dev/null) || true
      ;;
  esac
  if [[ $code =~ ^[0-9]{3}$ ]]; then
    printf '%s' "$code"
  else
    printf '000'
  fi
}

health_url_label() {
  local u=$1 rest
  case $u in
    unix://*\|*) printf '%s' "${u#*|}" ;;
    http://* | https://*)
      rest=${u#*://}
      if [[ $rest == */* ]]; then
        printf '/%s' "${rest#*/}"
      else
        printf '/'
      fi
      ;;
    *) printf '%s' "/healthz" ;;
  esac
}

unit_has_legacy_env_file() { # unit_has_legacy_env_file <unit_text> <legacy_expanded> <legacy_raw>
  local text=$1 leg=$2 leg_raw=${3-} base leg_raw_exp line rhs
  base=${leg##*/}
  leg_raw_exp=$(home_path "$leg_raw")
  while IFS= read -r line; do
    line=$(trim "$line")
    [[ $line =~ ^EnvironmentFile[[:space:]]*= ]] || continue
    rhs=${line#*=}
    rhs=$(trim "$rhs")
    rhs=${rhs#\"}; rhs=${rhs%\"}
    rhs=${rhs#-}
    case $rhs in
      "%h/"*) rhs="$HOME/${rhs#"%h/"}" ;;
      "~/"*)  rhs="$HOME/${rhs#"~/"}" ;;
    esac
    if [ "$rhs" = "$leg" ] || [ "$rhs" = "$leg_raw" ] || [ "$rhs" = "$leg_raw_exp" ] ||
       { [ -n "$base" ] && [ "${rhs##*/}" = "$base" ]; }; then
      return 0
    fi
  done <<<"$text"
  return 1
}

# --- the manifest: id|unit|health_url|status_tag|legacy_file|legacy_keys|scanner_source ---
rows=()
load_and_validate_manifest() {
  local line combined i r
  local -a f
  rows=()
  [ -f "$manifest" ] && [ -r "$manifest" ] || refuse "no readable manifest file"
  while IFS= read -r line || [ -n "$line" ]; do
    case $(trim "$line") in "" | "#"*) continue ;; esac
    [[ $line != *"#"* ]] || refuse "a '#' inside a row (a comment goes on a line of its own)"
    IFS='|' read -r -a f <<<"$line"
    if [ "${#f[@]}" = 8 ] && [[ $(trim "${f[2]}") == unix://* ]] && [[ $(trim "${f[3]}") == /* ]]; then
      combined="$(trim "${f[2]}")|$(trim "${f[3]}")"
      f=("${f[0]}" "${f[1]}" "$combined" "${f[4]}" "${f[5]}" "${f[6]}" "${f[7]}")
    fi
    [ "${#f[@]}" = 7 ] || refuse "a row without 7 fields"
    for i in 0 1 2 3 4 5 6; do f[i]=$(trim "${f[i]}"); [ -n "${f[i]}" ] || refuse "a row with an empty field"; done
    [[ ${f[0]} =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || refuse "an id that is not a file name"
    [ "${f[0]}" != "$host_subject" ] || refuse "the id $host_subject (the unit_drift alert's subject)"
    valid_health_url "${f[2]}" || refuse "a health URL that is not http(s):// or unix://<socket>|<path>"
    [[ ${f[4]} != */ && ${f[6]} != */ ]] || refuse "a legacy file or scanner source ending in / (a folder)"
    [[ ${f[1]} == - || ${f[1]} =~ ^[A-Za-z0-9][A-Za-z0-9@._-]*$ ]] || refuse "a unit that is not a unit name"
    [[ ${f[3]} == - || ${f[3]} =~ ^[A-Za-z0-9_-]+$ ]] || refuse "a status tag that is not a plain word"
    [[ ${f[5]} == - || ${f[5]} =~ ^[A-Za-z_][A-Za-z0-9_]*(,[A-Za-z_][A-Za-z0-9_]*)*$ ]] || refuse "legacy keys that are not env names"
    for r in "${rows[@]}"; do [ "${r%%"$row_sep"*}" != "${f[0]}" ] || refuse "the id ${f[0]} twice"; done
    rows+=("$(IFS="$row_sep"; printf '%s' "${f[*]}")")
  done <"$manifest"
  [ "${#rows[@]}" -gt 0 ] || refuse "no row"
}

# --- read-only Keep migration audit (--audit-keep-migration): directives and names only ---
append_unique_item() { # append_unique_item <array_name> <value>
  local arr_name=$1 val=$2 existing
  eval "local -a cur=(\"\${${arr_name}[@]-}\")"
  for existing in "${cur[@]}"; do
    [ "$existing" = "$val" ] && return 0
  done
  eval "${arr_name}+=(\"\$val\")"
}

is_optional_go_override_env_file() {
  local raw=$1 base
  [[ $raw == -* ]] || return 1
  raw=${raw#-}
  base=${raw##*/}
  case $base in
    node-out.env|node-edge.env|node-gw.env) return 0 ;;
    *) return 1 ;;
  esac
}

json_string_array() {
  local -a non_empty=()
  local item
  for item in "$@"; do
    [ -n "$item" ] && non_empty+=("$item")
  done
  if [ "${#non_empty[@]}" -eq 0 ]; then
    printf '[]'
  else
    jq -cn '$ARGS.positional' --args -- "${non_empty[@]}"
  fi
}

audit_keep_migration() {
  local audit_dir="" u path drop line rhs cred_id env_path status ef has_sensitive
  local total=0 n_keep=0 n_none=0 n_pending=0
  local go_checked=false go_ok=true
  local -a unit_names=() scan_files=() load_creds=() env_files=() suggested_ids=()
  local -a go_violations=() unit_records=() human_lines=()
  local lc_json ef_json sk_json go_v_json lc_disp ef_disp sk_disp

  if [ -n "${deploy_units-}" ]; then
    audit_dir=$deploy_units
  elif [ -n "${CH_UNIT_DIR-}" ] && [ -d "$CH_UNIT_DIR" ]; then
    audit_dir=$CH_UNIT_DIR
  elif [ -d "$controller/deploy/systemd" ]; then
    audit_dir=$controller/deploy/systemd
  else
    audit_dir=$unit_dir
  fi
  [ -d "$audit_dir" ] || { say "audit directory not found: $audit_dir"; exit 2; }

  mapfile -t unit_names < <(
    for path in "$audit_dir"/spawn-*.service; do
      [ -f "$path" ] || continue
      printf '%s\n' "${path##*/}"
    done | LC_ALL=C sort -u
  )

  for u in "${unit_names[@]}"; do
    scan_files=("$audit_dir/$u")
    for drop in "$audit_dir/${u}.d"/*.conf; do
      [ -f "$drop" ] && scan_files+=("$drop")
    done

    load_creds=()
    env_files=()
    for path in "${scan_files[@]}"; do
      while IFS= read -r line || [ -n "$line" ]; do
        line=$(trim "$line")
        case $line in "" | "#"* | ";"*) continue ;; esac
        if [[ $line =~ ^LoadCredential[[:space:]]*= ]]; then
          rhs=$(trim "${line#*=}")
          rhs=${rhs#\"}; rhs=${rhs%\"}
          cred_id=$(trim "${rhs%%:*}")
          [ -n "$cred_id" ] && append_unique_item load_creds "$cred_id"
        elif [[ $line =~ ^EnvironmentFile[[:space:]]*= ]]; then
          rhs=$(trim "${line#*=}")
          rhs=${rhs#\"}; rhs=${rhs%\"}
          env_path=$(trim "$rhs")
          [ -n "$env_path" ] && append_unique_item env_files "$env_path"
        fi
      done <"$path"
    done

    case $u in
      spawn-node-edge.service|spawn-node-gw.service|spawn-node-out.service)
        go_checked=true
        for path in "${scan_files[@]}"; do
          if grep -qiE 'spawn\.env|DATABASE_URL' "$path" 2>/dev/null; then
            go_ok=false
            append_unique_item go_violations "$u"
            break
          fi
        done
        ;;
    esac

    has_sensitive=0
    for ef in "${env_files[@]}"; do
      if ! is_optional_go_override_env_file "$ef"; then
        has_sensitive=1
        break
      fi
    done

    if [ "${#load_creds[@]}" -gt 0 ] && [ "$has_sensitive" -eq 0 ]; then
      status=keep_managed
      n_keep=$((n_keep + 1))
    elif [ "${#load_creds[@]}" -eq 0 ] && [ "${#env_files[@]}" -eq 0 ]; then
      status=no_secrets
      n_none=$((n_none + 1))
    else
      status=pending_migration
      n_pending=$((n_pending + 1))
    fi
    total=$((total + 1))

    suggested_ids=()
    if [ "$status" = keep_managed ]; then
      for cred_id in "${load_creds[@]}"; do
        append_unique_item suggested_ids "$cred_id"
      done
    elif [ "$status" = pending_migration ]; then
      for cred_id in "${load_creds[@]}"; do
        append_unique_item suggested_ids "$cred_id"
      done
      for ef in "${env_files[@]}"; do
        ef=${ef#-}
        case ${ef##*/} in
          model-relay.env) append_unique_item suggested_ids "deepseek.api-key" ;;
          typesafe.env) append_unique_item suggested_ids "typesafe.api-key" ;;
          node-gateway.env) append_unique_item suggested_ids "spawn-node-gateway.database-url" ;;
          node-edge.env) append_unique_item suggested_ids "spawn-serve.token" ;;
          node-gw.env|node-out.env) ;;
          r2.env|r2-store.env) append_unique_item suggested_ids "r2.credentials" ;;
          spawn.env)
            case $u in
              spawn-backup.service|spawn-backup-bundle.service|spawn-backup-verify.service|spawn-backup-restore-drill*.service)
                append_unique_item suggested_ids "spawn.database-url"
                append_unique_item suggested_ids "backup.encryption-key"
                append_unique_item suggested_ids "r2.credentials"
                ;;
              spawn-serve.service|spawn-core.service)
                append_unique_item suggested_ids "spawn.database-url"
                append_unique_item suggested_ids "spawn-serve.token"
                ;;
              *)
                append_unique_item suggested_ids "spawn.database-url"
                ;;
            esac
            ;;
          *)
            cred_id=${ef##*/}
            cred_id=${cred_id%.env}
            [ -n "$cred_id" ] && append_unique_item suggested_ids "${cred_id}.secret"
            ;;
        esac
      done
    fi

    lc_json=$(json_string_array ${load_creds[@]+"${load_creds[@]}"})
    ef_json=$(json_string_array ${env_files[@]+"${env_files[@]}"})
    sk_json=$(json_string_array ${suggested_ids[@]+"${suggested_ids[@]}"})
    unit_records+=("$(jq -cn \
      --arg unit "$u" \
      --arg status "$status" \
      --argjson lc "$lc_json" \
      --argjson ef "$ef_json" \
      --argjson sk "$sk_json" \
      '{unit: $unit, status: $status, load_credentials: $lc, env_files: $ef, suggested_keep_ids: $sk}')")

    lc_disp=$(IFS=,; printf '%s' "${load_creds[*]-}"); [ -n "$lc_disp" ] || lc_disp="-"
    ef_disp=$(IFS=,; printf '%s' "${env_files[*]-}"); [ -n "$ef_disp" ] || ef_disp="-"
    sk_disp=$(IFS=,; printf '%s' "${suggested_ids[*]-}"); [ -n "$sk_disp" ] || sk_disp="-"
    human_lines+=("$(printf '%-18s %-42s LoadCredential=%-18s EnvironmentFile=%-44s suggested=%s' \
      "$status" "$u" "$lc_disp" "$ef_disp" "$sk_disp")")
  done

  if [ "$audit_json" -eq 1 ]; then
    go_v_json=$(json_string_array ${go_violations[@]+"${go_violations[@]}"})
    if [ "${#unit_records[@]}" -eq 0 ]; then
      printf '[]'
    else
      printf '%s\n' "${unit_records[@]}" | jq -s .
    fi | jq \
      --arg audit_dir "$audit_dir" \
      --argjson total "$total" \
      --argjson keep_managed "$n_keep" \
      --argjson no_secrets "$n_none" \
      --argjson pending_migration "$n_pending" \
      --argjson go_checked "$go_checked" \
      --argjson go_ok "$go_ok" \
      --argjson go_violations "$go_v_json" \
      '{
        audit_dir: $audit_dir,
        counts: {
          total: $total,
          keep_managed: $keep_managed,
          no_secrets: $no_secrets,
          pending_migration: $pending_migration
        },
        go_units_pg_isolation: {
          checked: $go_checked,
          ok: $go_ok,
          violations: $go_violations
        },
        units: .
      }'
  else
    say "keep migration audit ($audit_dir):"
    for line in "${human_lines[@]}"; do
      printf '  %s\n' "$line"
    done
    printf 'summary: total=%d keep_managed=%d no_secrets=%d pending_migration=%d go_units_pg_isolation=%s\n' \
      "$total" "$n_keep" "$n_none" "$n_pending" "$([ "$go_ok" = true ] && printf ok || printf violation)"
  fi
  return 0
}

case $mode in
  validate)
    load_and_validate_manifest
    say "manifest ok: ${#rows[@]} row(s) valid ($manifest)"
    exit 0
    ;;
  audit)
    audit_keep_migration
    exit 0
    ;;
esac

load_and_validate_manifest

declare -A why=()
fault() { why[$1|$2]=$3; } # fault <subject> <class> <reason>
add() { printf '%s' "${1:+$1; }$2"; }

# The spawn-* units as systemd sees them (drop-ins included), read once for every row.
declare -A unit_text=()
spawn_units=()
while read -r u _; do
  [[ $u == spawn-* ]] || continue
  spawn_units+=("$u")
  unit_text[$u]=$(systemctl --user cat "$u" 2>/dev/null)
done < <(systemctl --user list-unit-files --no-legend 'spawn-*' 2>/dev/null | sort)

# --- the checks of one row: each sets a class with its reason (names and numbers only) ---
check_row() {
  local id=$1 unit=$2 health_url=$3 tag=$4 legacy=$5 keys=$6 scanner=$7 legacy_raw=${8-}
  local file=$keep_dir/$id in_grace="" up active code started started_s delivered_s inv newest
  local legacy_why="" named="" own u fpath k base
  local -a jsel codes
  if [ ! -f "$file" ] || [ ! -s "$file" ] || [ "$(stat -c %a "$file" 2>/dev/null)" != 600 ]; then
    up=$(uptime_s)
    if [ "$up" -gt "$grace" ]; then
      fault "$id" delivery_missing "$file absent, empty or not mode 600; host up ${up}s (grace ${grace}s)"
    else
      say "$file not delivered yet; host up ${up}s, within the boot grace (${grace}s)"
      in_grace=1
    fi
  fi

  if [ "$unit" != - ]; then
    active=$(systemctl --user is-active "$unit" 2>/dev/null)
    code=200
    [ "$health_url" = - ] || code=$(probe_health_url "$health_url")
    if [ "$active" != active ] || [ "$code" != 200 ]; then
      if [ -n "$in_grace" ]; then
        # After a boot the unit stays failed until Keep delivers its key: a normal gap, not a fault.
        say "$unit is ${active:-unknown}, waiting for its key within the boot grace"
      elif [ "$health_url" = - ]; then
        fault "$id" service_down "$unit is ${active:-unknown}"
      else
        fault "$id" service_down "$unit is ${active:-unknown}; $(health_url_label "$health_url") answered ${code:-nothing}"
      fi
    fi

    if [ "$active" = active ] && [ -s "$file" ]; then
      started=$(systemctl --user show "$unit" -p ExecMainStartTimestamp --value 2>/dev/null)
      started_s=$(date -d "$started" +%s 2>/dev/null || echo 0)
      delivered_s=$(stat -c %Y "$file" 2>/dev/null || echo 0)
      if [ "$started_s" -gt 0 ] && [ $((delivered_s - started_s)) -gt 60 ]; then
        fault "$id" not_loaded "$file delivered $((delivered_s - started_s))s after $unit started"
      fi
    fi

    # Only the running unit's lines count: after a restart with a fixed key, an old 401 must not
    # keep auth_failure open (or fire it on a fresh good start). Without an invocation id, the
    # newest 400 lines.
    if [ "$tag" != - ]; then
      inv=$(systemctl --user show "$unit" -p InvocationID --value 2>/dev/null)
      if [ -n "$inv" ]; then jsel=("_SYSTEMD_INVOCATION_ID=$inv"); else jsel=(-u "$unit" -n 400); fi
      mapfile -t codes < <(journalctl --user "${jsel[@]}" -o cat --no-pager 2>/dev/null |
        sed -n "s/^$tag: \([0-9][0-9][0-9]\) .*/\1/p" | tail -n 3)
      if [ "${#codes[@]}" -gt 0 ]; then
        newest=${codes[${#codes[@]}-1]}
        case $newest in 401|403) fault "$id" auth_failure "the newest relayed call answered $newest" ;; esac
        if [ "${#codes[@]}" -eq 3 ] && ! printf '%s\n' "${codes[@]}" | grep -qx 200; then
          fault "$id" call_failure "the 3 newest relayed calls answered ${codes[*]}"
        fi
      fi
    fi
  fi

  if [ "$legacy" != - ]; then
    [ -e "$legacy" ] && legacy_why="$legacy exists"
    base=${legacy##*/}
    for fpath in "$unit_dir"/*.service "$unit_dir"/*.path "$unit_dir"/*.d/*.conf; do
      [ -f "$fpath" ] || continue
      if grep -qsF -- "$base" "$fpath" || unit_has_legacy_env_file "$(cat "$fpath" 2>/dev/null)" "$legacy" "$legacy_raw"; then
        named="${named}${fpath} "
      fi
    done
    [ -n "$named" ] && legacy_why=$(add "$legacy_why" "units naming $base: $named")
    for u in "${spawn_units[@]}"; do
      unit_has_legacy_env_file "${unit_text[$u]}" "$legacy" "$legacy_raw" &&
        legacy_why=$(add "$legacy_why" "$u has an EnvironmentFile= naming $base")
    done
  fi
  if [ "$keys" != - ]; then
    for u in "${spawn_units[@]}"; do
      for k in ${keys//,/ }; do
        grep -qE "^[[:space:]]*Environment[[:space:]]*=(.*[[:space:]\"])?$k=" <<<"${unit_text[$u]}" &&
          legacy_why=$(add "$legacy_why" "$u sets $k by Environment=")
      done
    done
  fi
  if [ "$unit" != - ]; then
    own=$(systemctl --user cat "$unit" 2>/dev/null)
    if [ -n "$own" ]; then
      grep -qE '^[[:space:]]*EnvironmentFile[[:space:]]*=' <<<"$own" &&
        legacy_why=$(add "$legacy_why" "$unit has an EnvironmentFile=")
      grep -q "^LoadCredential=${id//./\\.}:" <<<"$own" ||
        legacy_why=$(add "$legacy_why" "$unit has no LoadCredential=$id")
    fi
  fi
  [ -n "$legacy_why" ] && fault "$id" legacy_env "$legacy_why"

  if [ "$scanner" != - ] && [ ! -e "$scanner" ] && [ ! -s "$file" ]; then
    if [ -n "$in_grace" ]; then
      say "the scanner source $scanner and $file are both missing, within the boot grace"
    else
      fault "$id" scanner_source "the scanner source $scanner and $file are both missing: every value scan refuses"
    fi
  fi
  [ -z "$in_grace" ] || return 0
  for c in $cred_classes; do [ -z "${why[$id|$c]-}" ] || return 0; done
  if [ "$unit" = - ]; then
    say "ok: $id delivered, no legacy env"
  elif [ "$health_url" = - ]; then
    say "ok: $id delivered, loaded by $unit, no legacy env"
  else
    say "ok: $id delivered, loaded by $unit, healthz 200, no legacy env"
  fi
}

first=1
for row in "${rows[@]}"; do
  IFS="$row_sep" read -r id unit health_url tag legacy keys scanner <<<"$row"
  legacy_raw=$legacy
  [ "$legacy" = - ] || legacy=$(home_path "$legacy")
  [ "$scanner" = - ] || scanner=$(home_path "$scanner")
  if [ -n "$first" ]; then
    legacy=${CH_LEGACY_FILE:-$legacy}
    if [ "$health_url" != - ] && [ -n "${CH_HEALTH_URL-}" ]; then
      valid_health_url "$CH_HEALTH_URL" || refuse "CH_HEALTH_URL is not http(s):// or unix://<socket>|<path>"
      health_url=$CH_HEALTH_URL
    fi
    first=""
  fi
  check_row "$id" "$unit" "$health_url" "$tag" "$legacy" "$keys" "$scanner" "$legacy_raw"
done

# --- the host: unit drift (opt-in) & Go units zero-PG isolation ---
subjects=()
for row in "${rows[@]}"; do subjects+=("${row%%"$row_sep"*}"); done
host_classes=""
go_units=(spawn-node-edge.service spawn-node-gw.service spawn-node-out.service)
check_go_units=""

if [ -n "$deploy_units" ]; then
  if [ -d "$deploy_units" ]; then
    drift=""
    for src in "$deploy_units"/spawn-*; do
      [ -f "$src" ] && [ -f "$unit_dir/${src##*/}" ] || continue
      cmp -s -- "$src" "$unit_dir/${src##*/}" || drift="$drift ${src##*/}"
    done
    [ -z "$drift" ] && say "ok: installed spawn-* units match $deploy_units" ||
      fault "$host_subject" unit_drift "installed units differing from $deploy_units:$drift"
    host_classes=unit_drift
    check_go_units=1
  else
    say "no folder $deploy_units (CH_DEPLOY_SYSTEMD_DIR): unit drift not checked"
  fi
fi

if [ -z "$check_go_units" ]; then
  for gu in "${go_units[@]}"; do
    if [ -f "$unit_dir/$gu" ] || [ -n "${unit_text[$gu]-}" ]; then
      check_go_units=1
      break
    fi
    for drop in "$unit_dir/${gu}.d"/*.conf; do
      if [ -f "$drop" ]; then
        check_go_units=1
        break 2
      fi
    done
  done
fi

if [ -n "$check_go_units" ]; then
  pg_leaks=""
  note_pg_leak() {
    case " $pg_leaks " in
      *" $1 "*) ;;
      *) pg_leaks="$pg_leaks $1" ;;
    esac
  }
  for gu in "${go_units[@]}"; do
    if [ -n "$deploy_units" ] && [ -f "$deploy_units/$gu" ]; then
      grep -qiE 'spawn\.env|DATABASE_URL' "$deploy_units/$gu" && note_pg_leak "deploy/$gu"
    fi
    if [ -f "$unit_dir/$gu" ]; then
      grep -qiE 'spawn\.env|DATABASE_URL' "$unit_dir/$gu" && note_pg_leak "installed/$gu"
    fi
    for drop in "$unit_dir/${gu}.d"/*.conf; do
      [ -f "$drop" ] || continue
      grep -qiE 'spawn\.env|DATABASE_URL' "$drop" && note_pg_leak "dropin/$gu/${drop##*/}"
    done
    if [ -n "${unit_text[$gu]-}" ]; then
      grep -qiE 'spawn\.env|DATABASE_URL' <<<"${unit_text[$gu]}" && note_pg_leak "unit/$gu"
    fi
  done
  [ -z "$pg_leaks" ] && say "ok: Go front units (spawn-node-{edge,gw,out}) contain zero spawn.env or DATABASE_URL" ||
    fault "$host_subject" go_unit_pg_isolation "Go front units referencing spawn.env or DATABASE_URL:$pg_leaks"
  host_classes=${host_classes:+$host_classes }go_unit_pg_isolation
fi

[ -z "$host_classes" ] || subjects+=("$host_subject")
classes_of() { if [ "$1" = "$host_subject" ]; then echo "$host_classes"; else echo "$cred_classes"; fi; }

for s in "${subjects[@]}"; do
  for c in $(classes_of "$s"); do [ -n "${why[$s|$c]-}" ] && say "FAULT $c: $s: ${why[$s|$c]}"; done
done

# --- the alerts: fire what started, clear what ended ---
db_ready=""
db_open() {
  [ -n "${CH_FAKE_ALERTS-}" ] && return 0
  [ -z "$db_ready" ] || return 0
  if [ -z "${SPAWN_DATABASE_URL-}" ] || [ ! -f "$controller/scripts/pg-url.sh" ]; then
    say "no SPAWN_DATABASE_URL: no alert raised or cleared"
    return 1
  fi
  # shellcheck source=scripts/pg-url.sh
  . "$controller/scripts/pg-url.sh"
  if ! pg_url_export "$SPAWN_DATABASE_URL"; then
    say "SPAWN_DATABASE_URL is not one postgres URL: no alert raised or cleared"
    return 1
  fi
  PGDATABASE=$pg_url_database
  export PGDATABASE
  unset SPAWN_DATABASE_URL
  db_ready=1
}
alert_record() {
  if [ -n "${CH_FAKE_ALERTS-}" ]; then printf 'RECORD %s\n' "$1" >>"$CH_FAKE_ALERTS"; return 0; fi
  psql -X -At -q -v ON_ERROR_STOP=1 -v r="$1" <<'SQL'
select spawn_alert_record(:'r'::jsonb);
SQL
}
open_alerts() { # open_alerts <subject>
  if [ -n "${CH_FAKE_ALERTS-}" ]; then sed -n 's/^OPEN //p' "$CH_FAKE_ALERTS" 2>/dev/null; return 0; fi
  psql -X -At -q -v ON_ERROR_STOP=1 -v s="$1" -v k="$kind" <<'SQL'
select key from spawn_alerts where kind = :'k' and subject = :'s' and state = 'open' order by key;
SQL
}

db_open || exit 0
for subject in "${subjects[@]}"; do
  if ! open=$(open_alerts "$subject"); then
    say "the alerts of $subject could not be read: none raised or cleared"
    continue
  fi
  for c in $(classes_of "$subject"); do
    prefix="$kind:$subject:$c:"
    mine=$(while IFS= read -r k; do
      [[ $k == "$prefix"* && ${k#"$prefix"} =~ ^[0-9]+$ ]] && printf '%s\n' "$k"
    done <<<"$open")
    if [ -n "${why[$subject|$c]-}" ]; then
      [ -n "$mine" ] && continue                      # open already: nothing raised
      key="$prefix$(date +%s)"
      if alert_record "$(jq -cn --arg key "$key" --arg kind "$kind" --arg subject "$subject" --arg c "$c" \
        --arg text "$subject: ${why[$subject|$c]}" '{type: "alert.fired", key: $key, kind: $kind, subject: $subject,
          evidence: {class: $c}, text: $text, notice: false}')" >/dev/null; then
        say "alert fired: $key"
      else
        say "alert $key could not be recorded"
      fi
    else
      while IFS= read -r key; do
        [ -n "$key" ] || continue
        if alert_record "$(jq -cn --arg key "$key" --arg kind "$kind" --arg subject "$subject" --arg c "$c" \
          '{type: "alert.cleared", key: $key, kind: $kind, subject: $subject, evidence: {class: $c}}')" >/dev/null; then
          say "alert cleared: $key"
        else
          say "alert $key could not be cleared"
        fi
      done <<<"$mine"
    fi
  done
done
exit 0
