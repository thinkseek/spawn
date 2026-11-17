#!/bin/bash
# Fault tests for credential-health.sh. Everything is made up and lives in a scratch folder:
# a fake /run/keep, fake systemctl/journalctl/curl on PATH, and a file standing in for the
# alert store. It never touches the real HOME, the real units, any credential or the database.
set -u
here=$(cd "$(dirname "$0")" && pwd)
check=$here/credential-health.sh
scratch=$(mktemp -d) || exit 2
[ -n "$scratch" ] && [ -d "$scratch" ] || exit 2
case $scratch in "$HOME"|"$HOME"/.config*|/run/keep*) echo "refusing scratch $scratch"; exit 2 ;; esac
trap 'rm -rf -- "$scratch"' EXIT
fails=0
pass() { printf 'ok   %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1"; fails=$((fails + 1)); }

bin=$scratch/bin; mkdir -p "$bin" "$scratch/keep" "$scratch/units" "$scratch/home"
cat >"$bin/systemctl" <<'EOF'
#!/bin/bash
case "$*" in
  *is-active*) echo "$FAKE_ACTIVE" ;;
  *ExecMainStartTimestamp*) echo "$FAKE_STARTED" ;;
  *InvocationID*) echo "${FAKE_INV-}" ;;
  *" cat "*) cat "$FAKE_UNIT" ;;
esac
EOF
cat >"$bin/journalctl" <<'EOF'
#!/bin/bash
# "@inv <id>" lines mark a relay start; asked for one invocation, print only that run's lines.
want=""; for a in "$@"; do case $a in _SYSTEMD_INVOCATION_ID=*) want=${a#*=} ;; esac; done
if [ -n "$want" ]; then awk -v w="$want" '/^@inv /{on=($2==w); next} on' "$FAKE_JOURNAL"
else grep -v '^@inv ' "$FAKE_JOURNAL"; fi
EOF
cat >"$bin/curl" <<'EOF'
#!/bin/bash
printf '%s' "$FAKE_HEALTH"
EOF
chmod +x "$bin"/*

good_unit() {
  printf '[Service]\nLoadCredential=typesafe.api-key:/run/keep/typesafe.api-key\nExecStart=/usr/bin/python3 relay\n' >"$scratch/unit"
}
reset() {
  rm -f "$scratch/alerts" "$scratch/home/typesafe.env" "$scratch/units"/*
  printf 'made-up-test-value' >"$scratch/keep/typesafe.api-key"; chmod 600 "$scratch/keep/typesafe.api-key"
  touch -d '-10 min' "$scratch/keep/typesafe.api-key"
  good_unit
  printf 'jev-relay: 200 600ms 5000B\n' >"$scratch/journal"
  export FAKE_ACTIVE=active FAKE_STARTED="$(date -d '-5 min' '+%a %F %T %z')" FAKE_HEALTH=200 CH_UPTIME_S=100000
}
run() {
  PATH="$bin:/usr/bin:/bin" HOME="$scratch/home" FAKE_UNIT="$scratch/unit" FAKE_JOURNAL="$scratch/journal" \
    CH_KEEP_DIR="$scratch/keep" CH_LEGACY_FILE="$scratch/home/typesafe.env" CH_UNIT_DIR="$scratch/units" \
    CH_FAKE_ALERTS="$scratch/alerts" bash "$check"
}
fired() { grep -c "\"type\":\"alert.fired\".*credential_health:typesafe.api-key:$1:" "$scratch/alerts" 2>/dev/null; }
cleared() { grep -c "\"type\":\"alert.cleared\".*credential_health:typesafe.api-key:$1:" "$scratch/alerts" 2>/dev/null; }
expect_only() { # expect_only <class>: exactly one alert fired, of that class
  local n; n=$(grep -c '"type":"alert.fired"' "$scratch/alerts" 2>/dev/null)
  [ "$n" = 1 ] && [ "$(fired "$1")" = 1 ] && pass "$2" || { fail "$2"; cat "$scratch/alerts" 2>/dev/null; }
}

reset; out=$(run)
[ ! -s "$scratch/alerts" ] && grep -q '^credential-health: ok' <<<"$out" && pass "healthy: no alert" || fail "healthy: no alert"

reset; rm "$scratch/keep/typesafe.api-key"; CH_UPTIME_S=300 run >/dev/null
[ ! -s "$scratch/alerts" ] && pass "delivery missing within the boot grace: no alert" || fail "boot grace"
reset; rm "$scratch/keep/typesafe.api-key"; CH_UPTIME_S=300 FAKE_ACTIVE=failed FAKE_HEALTH=000 run >/dev/null
[ ! -s "$scratch/alerts" ] && pass "after a reboot: relay failed, no key, within the grace: no alert" || fail "reboot grace"
reset; rm "$scratch/keep/typesafe.api-key"; FAKE_ACTIVE=failed FAKE_HEALTH=000 run >/dev/null
[ "$(grep -c '"type":"alert.fired"' "$scratch/alerts")" = 2 ] && [ "$(fired delivery_missing)" = 1 ] && [ "$(fired service_down)" = 1 ] && pass "relay failed, no key, after the grace: delivery_missing and service_down" || fail "after the grace, relay failed"
reset; rm "$scratch/keep/typesafe.api-key"; run >/dev/null; expect_only delivery_missing "delivery missing after the grace"
reset; chmod 644 "$scratch/keep/typesafe.api-key"; run >/dev/null; expect_only delivery_missing "delivery with a wrong mode"
reset; : >"$scratch/keep/typesafe.api-key"; run >/dev/null; expect_only delivery_missing "empty delivery"

reset; touch "$scratch/keep/typesafe.api-key"; FAKE_STARTED="$(date -d '-10 min' '+%a %F %T %z')" run >/dev/null
expect_only not_loaded "delivered after the relay started: not loaded"

reset; FAKE_ACTIVE=failed run >/dev/null; expect_only service_down "relay unit not active"
reset; FAKE_HEALTH=502 run >/dev/null; expect_only service_down "healthz not 200"

reset; printf 'jev-relay: 200 1ms 1B\njev-relay: 401 1ms 1B\n' >"$scratch/journal"; run >/dev/null
expect_only auth_failure "provider refused the key (401)"
reset; printf 'jev-relay: 502 1ms 1B\njev-relay: 500 1ms 1B\njev-relay: 502 1ms 1B\n' >"$scratch/journal"; run >/dev/null
expect_only call_failure "3 failed calls in a row"
reset; printf 'jev-relay: 502 1ms 1B\njev-relay: 200 1ms 1B\njev-relay: 502 1ms 1B\n' >"$scratch/journal"; run >/dev/null
[ ! -s "$scratch/alerts" ] && pass "one success among failures: no call alert" || fail "mixed calls"
reset; printf 'jev-relay: 502 1ms 1B\njev-relay: 500 1ms 1B\n' >"$scratch/journal"; run >/dev/null
[ ! -s "$scratch/alerts" ] && pass "only 2 failed calls so far: no call alert" || fail "fewer than 3 calls"
reset; printf '@inv old\njev-relay: 401 1ms 1B\n@inv now\n' >"$scratch/journal"; FAKE_INV=now run >/dev/null
[ ! -s "$scratch/alerts" ] && pass "old relay run's 401, restarted with no call yet: no auth alert" || fail "stale 401 across a restart"
reset; printf '@inv old\njev-relay: 200 1ms 1B\n@inv now\njev-relay: 403 1ms 1B\n' >"$scratch/journal"; FAKE_INV=now run >/dev/null
expect_only auth_failure "running relay's 403: auth alert"

reset; : >"$scratch/home/typesafe.env"; run >/dev/null; expect_only legacy_env "legacy file back"
reset; printf 'EnvironmentFile=%%h/.config/spawn/typesafe.env\n' >"$scratch/units/x.service"; run >/dev/null
expect_only legacy_env "a unit names typesafe.env"
reset; printf '[Service]\nEnvironmentFile=/x\nLoadCredential=typesafe.api-key:/run/keep/typesafe.api-key\n' >"$scratch/unit"; run >/dev/null
expect_only legacy_env "relay unit has an EnvironmentFile"
reset; printf '[Service]\nExecStart=relay\n' >"$scratch/unit"; run >/dev/null
expect_only legacy_env "relay unit lost its LoadCredential"

# Episodes: an open alert is not fired twice, and is cleared when the fault ends.
reset; FAKE_ACTIVE=failed run >/dev/null
key=$(sed -n 's/.*"key":"\([^"]*\)".*/\1/p' "$scratch/alerts" | head -1)
printf 'OPEN %s\n' "$key" >>"$scratch/alerts"
FAKE_ACTIVE=failed run >/dev/null
[ "$(fired service_down)" = 1 ] && pass "open alert not fired again" || fail "open alert not fired again"
run >/dev/null
[ "$(cleared service_down)" = 1 ] && grep -q "alert.cleared.*$key" "$scratch/alerts" && pass "cleared when the fault ends" || fail "cleared"

# Nothing it prints or records holds the made-up value.
reset; rm "$scratch/keep/typesafe.api-key"; out=$(run)
printf 'made-up-test-value' >"$scratch/keep/typesafe.api-key"; chmod 600 "$scratch/keep/typesafe.api-key"
FAKE_STARTED="$(date -d '-20 min' '+%a %F %T %z')" out2=$(run)
! grep -q made-up-test-value <<<"$out$out2" && ! grep -q made-up-test-value "$scratch/alerts" &&
  pass "no credential value in output or records" || fail "value leaked"

echo "$fails failed"
[ "$fails" -eq 0 ]
