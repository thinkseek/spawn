#!/bin/bash
# credential-health: is each Keep-delivered credential present, current, loaded by its service and
# accepted by its provider, and is no legacy secret env file back? (owner order t475u, 2026-11-17)
#
# One level-triggered check, run every 5 minutes by spawn-credential-health.timer. It reads only
# names, modes, sizes, times, unit text, the relay's /healthz and the relay's status lines
# ("jev-relay: <code> <ms> <bytes>"). It never opens a credential.
#
# Each fault class is ONE owner alert (kind credential_health, subject typesafe.api-key,
# key credential_health:typesafe.api-key:<class>:<unix time it started>), recorded through the store's owner-only
# spawn_alert_record like the nightly's: fired once when the class starts, cleared when it ends.
# Nothing else is sent anywhere.
#
#   delivery_missing  /run/keep/typesafe.api-key absent, empty or not mode 600, once the host has
#                     been up longer than the boot grace (CH_BOOT_GRACE_S, 900 s): after a reboot
#                     Keep re-delivers at the owner's login, so a short gap is normal
#   not_loaded        the delivered file is newer than the relay's start (by > 60 s): the relay
#                     holds an older delivery (its .path unit starts a stopped relay but never
#                     restarts a running one; the fix is systemctl --user restart spawn-jev-relay)
#   service_down      the relay unit is not active, or /healthz does not answer 200
#   auth_failure      the running relay's newest status line is 401 or 403 (the provider refused the key)
#   call_failure      the running relay's 3 newest status lines are all non-200 (fewer than 3: no alert)
#   legacy_env        ~/.config/spawn/typesafe.env exists again, or a user unit names it, or the
#                     relay unit has an EnvironmentFile= or lacks LoadCredential=typesafe.api-key
#
# Test seams (scripts/keep/test-credential-health.sh): CH_KEEP_DIR, CH_LEGACY_FILE, CH_UNIT_DIR,
# CH_UPTIME_S, CH_BOOT_GRACE_S, CH_HEALTH_URL, CH_FAKE_ALERTS (a file standing in for the
# store: open keys in, records appended). Without CH_FAKE_ALERTS it needs SPAWN_DATABASE_URL.
set -u
name=typesafe.api-key
unit=spawn-jev-relay.service
subject=$name
kind=credential_health
keep_dir=${CH_KEEP_DIR:-/run/keep}
legacy=${CH_LEGACY_FILE:-$HOME/.config/spawn/typesafe.env}
unit_dir=${CH_UNIT_DIR:-$HOME/.config/systemd/user}
grace=${CH_BOOT_GRACE_S:-900}
health_url=${CH_HEALTH_URL:-http://127.0.0.1:8790/healthz}
here=$(cd "$(dirname "$0")" && pwd)
controller=$(cd "$here/../.." && pwd)

say() { printf 'credential-health: %s\n' "$*"; }

uptime_s() {
  if [ -n "${CH_UPTIME_S-}" ]; then echo "$CH_UPTIME_S"; else cut -d. -f1 /proc/uptime; fi
}

declare -A why=()
fault() { why[$1]=$2; }

# --- the checks: each sets a class with its reason (names and numbers only) ---
file=$keep_dir/$name
in_grace=""
if [ ! -f "$file" ] || [ ! -s "$file" ] || [ "$(stat -c %a "$file" 2>/dev/null)" != 600 ]; then
  up=$(uptime_s)
  if [ "$up" -gt "$grace" ]; then
    fault delivery_missing "$file absent, empty or not mode 600; host up ${up}s (grace ${grace}s)"
  else
    say "$file not delivered yet; host up ${up}s, within the boot grace (${grace}s)"
    in_grace=1
  fi
fi

active=$(systemctl --user is-active "$unit" 2>/dev/null)
code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$health_url" 2>/dev/null)
if [ "$active" != active ] || [ "$code" != 200 ]; then
  if [ -n "$in_grace" ]; then
    # After a boot the relay stays failed until Keep delivers its key: a normal gap, not a fault.
    say "$unit is ${active:-unknown}, waiting for its key within the boot grace"
  else
    fault service_down "$unit is ${active:-unknown}; /healthz answered ${code:-nothing}"
  fi
fi

if [ "$active" = active ] && [ -s "$file" ]; then
  started=$(systemctl --user show "$unit" -p ExecMainStartTimestamp --value 2>/dev/null)
  started_s=$(date -d "$started" +%s 2>/dev/null || echo 0)
  delivered_s=$(stat -c %Y "$file" 2>/dev/null || echo 0)
  if [ "$started_s" -gt 0 ] && [ $((delivered_s - started_s)) -gt 60 ]; then
    fault not_loaded "$file delivered $((delivered_s - started_s))s after $unit started"
  fi
fi

# Only the running relay's lines count: after a restart with a fixed key, an old 401 must not keep
# auth_failure open (or fire it on a fresh good start). Without an invocation id, the newest 400 lines.
inv=$(systemctl --user show "$unit" -p InvocationID --value 2>/dev/null)
if [ -n "$inv" ]; then jsel=("_SYSTEMD_INVOCATION_ID=$inv"); else jsel=(-u "$unit" -n 400); fi
mapfile -t codes < <(journalctl --user "${jsel[@]}" -o cat --no-pager 2>/dev/null |
  sed -n 's/^jev-relay: \([0-9][0-9][0-9]\) .*/\1/p' | tail -n 3)
if [ "${#codes[@]}" -gt 0 ]; then
  newest=${codes[${#codes[@]}-1]}
  case $newest in 401|403) fault auth_failure "the newest relayed call answered $newest" ;; esac
  if [ "${#codes[@]}" -eq 3 ] && ! printf '%s\n' "${codes[@]}" | grep -qx 200; then
    fault call_failure "the 3 newest relayed calls answered ${codes[*]}"
  fi
fi

legacy_why=""
[ -e "$legacy" ] && legacy_why="$legacy exists"
named=$(grep -ls -- 'typesafe\.env' "$unit_dir"/*.service "$unit_dir"/*.path 2>/dev/null | tr '\n' ' ')
[ -n "$named" ] && legacy_why="${legacy_why:+$legacy_why; }units naming typesafe.env: $named"
unit_text=$(systemctl --user cat "$unit" 2>/dev/null)
if [ -n "$unit_text" ]; then
  grep -q '^EnvironmentFile=' <<<"$unit_text" && legacy_why="${legacy_why:+$legacy_why; }$unit has an EnvironmentFile="
  grep -q "^LoadCredential=$name:" <<<"$unit_text" || legacy_why="${legacy_why:+$legacy_why; }$unit has no LoadCredential=$name"
fi
[ -n "$legacy_why" ] && fault legacy_env "$legacy_why"

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
open_alerts() {
  if [ -n "${CH_FAKE_ALERTS-}" ]; then sed -n 's/^OPEN //p' "$CH_FAKE_ALERTS" 2>/dev/null; return 0; fi
  psql -X -At -q -v ON_ERROR_STOP=1 -v s="$subject" -v k="$kind" <<'SQL'
select key from spawn_alerts where kind = :'k' and subject = :'s' and state = 'open' order by key;
SQL
}

classes="delivery_missing not_loaded service_down auth_failure call_failure legacy_env"
if [ "${#why[@]}" -eq 0 ]; then
  say "ok: $name delivered, loaded by $unit, healthz 200, no legacy env"
else
  for c in $classes; do [ -n "${why[$c]-}" ] && say "FAULT $c: ${why[$c]}"; done
fi
db_open || exit 0
if ! open=$(open_alerts); then
  say "the alerts could not be read: none raised or cleared"
  exit 0
fi
for c in $classes; do
  prefix="$kind:$subject:$c:"
  mine=$(grep -F -- "$prefix" <<<"$open" | grep -E -- "^$kind:$subject:$c:[0-9]+$")
  if [ -n "${why[$c]-}" ]; then
    [ -n "$mine" ] && continue                      # open already: nothing raised
    key="$prefix$(date +%s)"
    if alert_record "$(jq -cn --arg key "$key" --arg kind "$kind" --arg subject "$subject" --arg c "$c" \
      --arg text "$name: ${why[$c]}" '{type: "alert.fired", key: $key, kind: $kind, subject: $subject,
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
exit 0
