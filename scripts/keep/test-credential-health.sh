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
  # FAKE_UNITS (unset in the first cases): a folder of spawn-* unit texts, drop-ins included, as
  # `systemctl --user cat` shows them; any other unit is the relay's ($FAKE_UNIT).
  *list-unit-files*) [ -n "${FAKE_UNITS-}" ] && ls "$FAKE_UNITS" ;;
  *" cat "*) u=${!#}; if [ -n "${FAKE_UNITS-}" ] && [ -f "$FAKE_UNITS/$u" ]; then cat "$FAKE_UNITS/$u"; else cat "$FAKE_UNIT"; fi ;;
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
  PATH="$bin:$PATH" HOME="$scratch/home" FAKE_UNIT="$scratch/unit" FAKE_JOURNAL="$scratch/journal" \
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

# --- The manifest (issue 98): several rows, unit drop-ins, a scanner's source, unit drift ---
# A second, made-up row (demo.token: no unit, a legacy file and keys, a scanner source), beside the
# relay's; spawn-* units as systemctl shows them in $scratch/fakeunits (one with spawn.env's
# EnvironmentFile=, as live: no fault); deploy/systemd stand-in in $scratch/deploy.
reset2() {
  rm -rf "$scratch/units"/*
  reset
  rm -rf "$scratch/fakeunits" "$scratch/home/.config" "$scratch/deploy" "$scratch/keep/demo.token"
  mkdir -p "$scratch/fakeunits" "$scratch/home/.config/spawn" "$scratch/deploy"
  printf 'made-up-demo-value' >"$scratch/keep/demo.token"; chmod 600 "$scratch/keep/demo.token"
  touch -d '-10 min' "$scratch/keep/demo.token"
  : >"$scratch/home/.config/spawn/demo-scan.env"
  printf '[Service]\nEnvironmentFile=%%h/.local/share/spawn-rsi/spawn.env\n' >"$scratch/fakeunits/spawn-credential-health.service"
  { printf '# made-up rows\n\n'
    printf 'typesafe.api-key | spawn-jev-relay.service | http://127.0.0.1:8790/healthz | jev-relay | ~/.config/spawn/typesafe.env | TYPESAFE_API_KEY | -\n'
    printf 'demo.token|-|-|-|~/.config/spawn/demo.env|DEMO_TOKEN,DEMO_SECRET|~/.config/spawn/demo-scan.env\n'
  } >"$scratch/manifest"
}
run2() { CH_MANIFEST="$scratch/manifest" FAKE_UNITS="$scratch/fakeunits" run; }
n_fired() { local n; n=$(grep -c "\"type\":\"alert.fired\".*credential_health:$1" "$scratch/alerts" 2>/dev/null); echo "${n:-0}"; }
expect_fired() { # expect_fired <name> <subject:class>...: exactly these alerts fired
  local want=$# name=$1 s; shift; want=$((want - 1))
  for s in "$@"; do [ "$(n_fired "$s:")" = 1 ] || { fail "$name"; cat "$scratch/alerts" 2>/dev/null; return; }; done
  [ "$(n_fired "")" = "$want" ] && pass "$name" || { fail "$name"; cat "$scratch/alerts" 2>/dev/null; }
}

reset2; out=$(run2)
[ ! -s "$scratch/alerts" ] && [ "$(grep -c '^credential-health: ok: ' <<<"$out")" = 2 ] &&
  pass "two rows healthy, another unit's EnvironmentFile=spawn.env: no alert, an ok line each" || fail "two rows healthy"
reset2; rm "$scratch/keep/demo.token"; run2 >/dev/null
expect_fired "second row's Keep file gone, its scanner source there: delivery_missing on its own subject" demo.token:delivery_missing
reset2; rm "$scratch/keep/demo.token" "$scratch/home/.config/spawn/demo-scan.env"; run2 >/dev/null
expect_fired "scanner source and its Keep twin both gone: scanner_source too" demo.token:delivery_missing demo.token:scanner_source
reset2; rm "$scratch/keep/demo.token" "$scratch/home/.config/spawn/demo-scan.env"; CH_UPTIME_S=300 run2 >/dev/null
expect_fired "scanner source and Keep twin gone within the boot grace: no alert"
reset2; : >"$scratch/home/.config/spawn/demo.env"; run2 >/dev/null
expect_fired "second row's legacy file back: legacy_env on its subject only" demo.token:legacy_env
reset2; printf '# /u/spawn-loop.service\n[Service]\nExecStart=x\n\n# /u/spawn-loop.service.d/env.conf\n[Service]\nEnvironmentFile=-%%h/.config/spawn/demo.env\n' \
  >"$scratch/fakeunits/spawn-loop.service"; run2 >/dev/null
expect_fired "a spawn-* unit's drop-in loads a legacy file (systemctl cat): legacy_env" demo.token:legacy_env
reset2; mkdir -p "$scratch/units/spawn-serve.service.d"
printf '[Service]\nEnvironmentFile=%%h/.config/spawn/typesafe.env\n' >"$scratch/units/spawn-serve.service.d/override.conf"; run2 >/dev/null
expect_fired "a drop-in file in the unit folder names typesafe.env: legacy_env" typesafe.api-key:legacy_env
reset2; printf '[Service]\nEnvironment=A=1 "DEMO_SECRET=made-up-demo-value"\n' >"$scratch/fakeunits/spawn-serve.service"; out=$(run2)
expect_fired "a spawn-* unit sets a legacy key by Environment=: legacy_env" demo.token:legacy_env
! grep -q made-up-demo-value <<<"$out" && ! grep -q made-up-demo-value "$scratch/alerts" &&
  pass "the Environment= value is in no output or record" || fail "Environment= value leaked"
reset2; printf '[Service]\nEnvironment=NOT_DEMO_SECRET=1 DEMO_TOKENS=2\n' >"$scratch/fakeunits/spawn-serve.service"; run2 >/dev/null
expect_fired "longer names holding a legacy key's name: no alert"

reset2; printf '[Unit]\n' >"$scratch/deploy/spawn-a.service"; cp "$scratch/deploy/spawn-a.service" "$scratch/units/"
printf '[Unit]\n' >"$scratch/deploy/spawn-b.timer"; printf 'x\n' >"$scratch/units/spawn-own.service"
CH_DEPLOY_SYSTEMD_DIR="$scratch/deploy" run2 >/dev/null
expect_fired "unit drift opt-in, installed units as deployed (one not installed, one not deployed): none"
printf '[Unit]\nDescription=edited\n' >"$scratch/units/spawn-a.service"; CH_DEPLOY_SYSTEMD_DIR="$scratch/deploy" run2 >/dev/null
expect_fired "an installed unit differs from deploy/systemd: unit_drift" spawn-units:unit_drift
key=$(sed -n 's/.*"key":"\(credential_health:spawn-units:unit_drift:[0-9]*\)".*/\1/p' "$scratch/alerts" | head -1)
printf 'OPEN %s\n' "$key" >>"$scratch/alerts"; run2 >/dev/null
! grep -q 'alert.cleared.*unit_drift' "$scratch/alerts" && pass "drift not opted in: its open alert left alone" || fail "drift touched without opt-in"
cp "$scratch/deploy/spawn-a.service" "$scratch/units/"; CH_DEPLOY_SYSTEMD_DIR="$scratch/deploy" run2 >/dev/null
grep -q "alert.cleared.*$key" "$scratch/alerts" && pass "drift mended: unit_drift cleared" || fail "drift cleared"

reset2; printf 'demo.token|-|-|-|-|-\n' >>"$scratch/manifest"; out=$(run2); st=$?
[ "$st" = 2 ] && [ ! -s "$scratch/alerts" ] && grep -q 'nothing checked' <<<"$out" &&
  pass "a manifest row without 7 fields: refused (exit 2), nothing checked" || fail "bad manifest row"
reset2; out=$(CH_MANIFEST="$scratch/none" run); st=$?
[ "$st" = 2 ] && [ ! -s "$scratch/alerts" ] && pass "no manifest: refused (exit 2)" || fail "no manifest"
reset2; out=$(CH_MANIFEST="$scratch/fakeunits" run 2>&1); st=$?
[ "$st" = 2 ] && [ ! -s "$scratch/alerts" ] && grep -q 'nothing checked' <<<"$out" &&
  pass "a folder as the manifest: refused (exit 2), nothing checked" || fail "a folder as the manifest"
bad_row() { # bad_row <name> <row>: a manifest whose second row is <row> is refused
  reset2; printf '%s\n' "$2" >>"$scratch/manifest"; out=$(run2 2>&1); st=$?
  [ "$st" = 2 ] && [ ! -s "$scratch/alerts" ] && grep -q 'nothing checked' <<<"$out" && pass "$1" || fail "$1"
}
bad_row "the reserved id spawn-units: refused" 'spawn-units|-|-|-|-|-|-'
bad_row "a health URL that is a curl option (-K/...): refused" 'demo.other|spawn-x.service|-K/tmp/x|-|-|-|-'
bad_row "a legacy file ending in /: refused" 'demo.other|-|-|-|~/.config/spawn/|-|-'
bad_row "an inline # comment in a row: refused" 'demo.other|-|-|-|-|-|~/.config/spawn/x.env # note'
rows=$(grep -v '^[[:space:]]*\(#\|$\)' "$here/credentials.manifest")
[ "$(wc -l <<<"$rows")" = 1 ] && [[ $rows == "typesafe.api-key "*"| spawn-jev-relay.service "*"| ~/.config/spawn/typesafe.env "* ]] &&
  pass "the manifest holds the one live row, typesafe.api-key by spawn-jev-relay" || fail "the manifest's live row"

echo "$fails failed"
[ "$fails" -eq 0 ]
