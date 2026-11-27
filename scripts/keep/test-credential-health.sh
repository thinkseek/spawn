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
uds_dir=""
uds_pid=""
cleanup() {
  [ -n "${uds_pid-}" ] && kill "$uds_pid" >/dev/null 2>&1 && wait "$uds_pid" 2>/dev/null || true
  [ -n "${uds_dir-}" ] && rm -rf -- "$uds_dir"
  rm -rf -- "$scratch"
}
trap cleanup EXIT
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
if [ "${FAKE_REAL_CURL-}" = 1 ]; then
  exec /usr/bin/curl "$@"
fi
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

# --- Issue #98 hardening: unix:// UDS health probe, EnvironmentFile= rollback, Go units PG isolation ---
uds_dir=$(mktemp -d /tmp/ch-uds.XXXXXX) || exit 2
python3 -B -I - "$uds_dir/out.sock" "$uds_dir/status" <<'PY' >/dev/null 2>&1 &
import os, socketserver, sys
sock_path, status_file = sys.argv[1], sys.argv[2]
try:
    os.unlink(sock_path)
except FileNotFoundError:
    pass
class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        line = self.rfile.readline(4096).decode("ascii", errors="replace").strip()
        while self.rfile.readline(4096) not in (b"\r\n", b"\n", b""):
            pass
        parts = line.split()
        req_path = parts[1] if len(parts) >= 2 else ""
        if req_path != "/out/v1/health":
            code, reason, body = "404", "Not Found", b'{"error":"not found"}\n'
        else:
            code = open(status_file).read().strip() if os.path.exists(status_file) else "503"
            reason = "OK" if code == "200" else "Service Unavailable"
            body = b'{"status":"ok"}\n' if code == "200" else b'{"error":"down"}\n'
        resp = (
            f"HTTP/1.1 {code} {reason}\r\n"
            f"Content-Type: application/json\r\n"
            f"Content-Length: {len(body)}\r\n"
            f"Connection: close\r\n\r\n"
        ).encode() + body
        self.wfile.write(resp)
class UnixHTTPServer(socketserver.UnixStreamServer):
    allow_reuse_address = True
srv = UnixHTTPServer(sock_path, Handler)
srv.serve_forever()
PY
uds_pid=$!
for _ in $(seq 1 40); do [ -S "$uds_dir/out.sock" ] && break; sleep 0.05; done

setup_uds_manifest() {
  reset2
  printf '200\n' >"$uds_dir/status"
  printf '[Service]\nLoadCredential=typesafe.api-key:/run/keep/typesafe.api-key\nExecStart=/usr/bin/spawn-node front --serve outbound\n' >"$scratch/unit"
  printf 'typesafe.api-key | spawn-node-out.service | unix://%s|/out/v1/health | jev-relay | ~/.config/spawn/typesafe.env | TYPESAFE_API_KEY | -\n' \
    "$uds_dir/out.sock" >"$scratch/manifest"
}
run_uds() {
  FAKE_REAL_CURL=1 CH_MANIFEST="$scratch/manifest" CH_HEALTH_URL="" FAKE_UNITS="$scratch/fakeunits" run
}

setup_uds_manifest
out=$(run_uds)
[ ! -s "$scratch/alerts" ] && grep -q '^credential-health: ok: typesafe.api-key' <<<"$out" &&
  pass "unix:// UDS /out/v1/health 200: healthy, no alert" || fail "unix:// UDS /out/v1/health 200"

printf '503\n' >"$uds_dir/status"
out=$(run_uds)
expect_fired "unix:// UDS /out/v1/health 503: service_down" typesafe.api-key:service_down
grep -q '/out/v1/health answered 503' <<<"$out" &&
  pass "unix:// UDS 503 fault names /out/v1/health answered 503" || fail "unix:// UDS fault text ($out)"

key=$(sed -n 's/.*"key":"\(credential_health:typesafe.api-key:service_down:[0-9]*\)".*/\1/p' "$scratch/alerts" | head -1)
printf 'OPEN %s\n' "$key" >>"$scratch/alerts"
printf '200\n' >"$uds_dir/status"
run_uds >/dev/null
grep -q "alert.cleared.*$key" "$scratch/alerts" &&
  pass "unix:// UDS recovered to 200: service_down cleared" || fail "unix:// UDS recovery clear"

# Symlinked UDS socket and %t/ expansion against real curl.
mkdir -p "$scratch/rt/spawn"
ln -s "$uds_dir/out.sock" "$scratch/rt/spawn/sym.sock"
reset2
printf '[Service]\nLoadCredential=typesafe.api-key:/run/keep/typesafe.api-key\nExecStart=/usr/bin/spawn-node front --serve outbound\n' >"$scratch/unit"
printf 'typesafe.api-key | spawn-node-out.service | unix://%%t/spawn/sym.sock|/out/v1/health | jev-relay | ~/.config/spawn/typesafe.env | TYPESAFE_API_KEY | -\n' \
  >"$scratch/manifest"
XDG_RUNTIME_DIR="$scratch/rt" run_uds >/dev/null
expect_fired "unix:// symlinked socket refused: service_down" typesafe.api-key:service_down

kill "$uds_pid" >/dev/null 2>&1 && wait "$uds_pid" 2>/dev/null || true
uds_pid=""
rm -f "$uds_dir/out.sock"
setup_uds_manifest
: >"$scratch/alerts"
run_uds >/dev/null
expect_fired "unix:// missing socket: service_down" typesafe.api-key:service_down

bad_row "unix:// URL missing |<path>: refused" 'demo.other|spawn-x.service|unix:///tmp/missing-path|-|-|-|-'
bad_row "unix:// URL with option injection (-K): refused" 'demo.other|spawn-x.service|unix://-K/tmp/bad|/health|-|-|-|-'
reset2
printf 'demo.token|spawn-node-out.service|unix://%%t/spawn/out.sock|/out/v1/health|-|~/.config/spawn/demo.env|DEMO_TOKEN|-\n' >"$scratch/manifest"
FAKE_ACTIVE=active FAKE_HEALTH=200
CH_MANIFEST="$scratch/manifest" FAKE_UNITS="$scratch/fakeunits" run >/dev/null 2>&1
st=$?
[ "$st" = 0 ] && pass "unix://%t/spawn/out.sock|/out/v1/health manifest syntax accepted" || fail "unix://%t/ manifest syntax refused ($st)"

# Plaintext EnvironmentFile= rollback variants on spawn-* units (#98 item 3).
reset2
printf '[Service]\nEnvironmentFile=-~/.config/spawn/typesafe.env\n' >"$scratch/fakeunits/spawn-loop.service"
run2 >/dev/null
expect_fired "EnvironmentFile=-~/...typesafe.env rollback caught: legacy_env" typesafe.api-key:legacy_env

reset2
printf '[Service]\nEnvironmentFile=%%h/.config/spawn/typesafe.env\n' >"$scratch/fakeunits/spawn-loop.service"
run2 >/dev/null
expect_fired "EnvironmentFile=%h/...typesafe.env rollback caught: legacy_env" typesafe.api-key:legacy_env

reset2
printf '[Service]\n  EnvironmentFile = -%%h/.config/spawn/typesafe.env\n' >"$scratch/fakeunits/spawn-loop.service"
out=$(run2)
expect_fired "indented spaced EnvironmentFile = -%h/... rollback caught: legacy_env" typesafe.api-key:legacy_env
key=$(sed -n 's/.*"key":"\(credential_health:typesafe.api-key:legacy_env:[0-9]*\)".*/\1/p' "$scratch/alerts" | head -1)
printf 'OPEN %s\n' "$key" >>"$scratch/alerts"
rm -f "$scratch/fakeunits/spawn-loop.service"
run2 >/dev/null
grep -q "alert.cleared.*$key" "$scratch/alerts" &&
  pass "EnvironmentFile rollback removed: legacy_env cleared" || fail "EnvironmentFile rollback cleared"

# Go front units (spawn-node-{edge,gw,out}.service) zero spawn.env / DATABASE_URL isolation.
repo_root=$(cd "$here/../.." && pwd)
setup_clean_go_deploy() {
  reset2
  cp "$repo_root/deploy/systemd/spawn-node-edge.service" "$scratch/deploy/"
  cp "$repo_root/deploy/systemd/spawn-node-gw.service" "$scratch/deploy/"
  cp "$repo_root/deploy/systemd/spawn-node-out.service" "$scratch/deploy/"
}

setup_clean_go_deploy
CH_DEPLOY_SYSTEMD_DIR="$scratch/deploy" run2 >/dev/null
expect_fired "clean Go front units under CH_DEPLOY_SYSTEMD_DIR: zero alerts"

setup_clean_go_deploy
printf 'EnvironmentFile=%%h/.local/share/spawn-rsi/spawn.env\n' >>"$scratch/deploy/spawn-node-edge.service"
CH_DEPLOY_SYSTEMD_DIR="$scratch/deploy" run2 >/dev/null
expect_fired "Go edge unit referencing spawn.env: go_unit_pg_isolation" spawn-units:go_unit_pg_isolation
key=$(sed -n 's/.*"key":"\(credential_health:spawn-units:go_unit_pg_isolation:[0-9]*\)".*/\1/p' "$scratch/alerts" | head -1)
printf 'OPEN %s\n' "$key" >>"$scratch/alerts"
cp "$repo_root/deploy/systemd/spawn-node-edge.service" "$scratch/deploy/"
CH_DEPLOY_SYSTEMD_DIR="$scratch/deploy" run2 >/dev/null
grep -q "alert.cleared.*$key" "$scratch/alerts" &&
  pass "Go edge unit spawn.env removed: go_unit_pg_isolation cleared" || fail "go_unit_pg_isolation clear (spawn.env)"

setup_clean_go_deploy
printf '%s\n' 'Environment=SPAWN_DATABASE_URL=postgres://synthetic-value-99' >>"$scratch/deploy/spawn-node-gw.service"
out=$(CH_DEPLOY_SYSTEMD_DIR="$scratch/deploy" run2)
expect_fired "Go gw unit referencing DATABASE_URL: go_unit_pg_isolation" spawn-units:go_unit_pg_isolation
! grep -q 'synthetic-value-99' <<<"$out" && ! grep -q 'synthetic-value-99' "$scratch/alerts" &&
  pass "Go unit DATABASE_URL violation prints no secret value" || fail "Go unit violation leaked value"

setup_clean_go_deploy
mkdir -p "$scratch/units/spawn-node-out.service.d"
printf '%s\n' '[Service]' 'Environment=CUSTOM_DATABASE_URL=postgres://synthetic-dropin' >"$scratch/units/spawn-node-out.service.d/override.conf"
printf 'OPEN credential_health:spawn-units:unit_drift:12345\n' >"$scratch/alerts"
run2 >/dev/null
[ "$(n_fired "spawn-units:go_unit_pg_isolation:")" = 1 ] && ! grep -q 'alert.cleared.*unit_drift' "$scratch/alerts" &&
  pass "installed Go drop-in DATABASE_URL caught even without CH_DEPLOY_SYSTEMD_DIR while leaving unit_drift untouched" || fail "installed Go drop-in isolation"

setup_clean_go_deploy
cp "$repo_root/deploy/systemd/spawn-node-gw.service" "$scratch/units/"
mkdir -p "$scratch/units/spawn-node-gw.service.d"
printf '%s\n' '[Service]' 'EnvironmentFile=%h/.local/share/spawn-rsi/spawn.env' >"$scratch/units/spawn-node-gw.service.d/override.conf"
run2 >/dev/null
expect_fired "installed Go gw drop-in spawn.env caught without CH_DEPLOY_SYSTEMD_DIR: go_unit_pg_isolation" spawn-units:go_unit_pg_isolation
key=$(sed -n 's/.*"key":"\(credential_health:spawn-units:go_unit_pg_isolation:[0-9]*\)".*/\1/p' "$scratch/alerts" | head -1)
printf 'OPEN %s\n' "$key" >>"$scratch/alerts"
rm -f "$scratch/units/spawn-node-gw.service.d/override.conf"
run2 >/dev/null
grep -q "alert.cleared.*$key" "$scratch/alerts" &&
  pass "installed Go gw drop-in spawn.env removed: go_unit_pg_isolation cleared" || fail "go_unit_pg_isolation clear (gw drop-in)"

# Static compliance audit of committed deploy/systemd/spawn-{core,node-edge,node-gw,node-out}.service.
reset2
out=$(CH_DEPLOY_SYSTEMD_DIR="$repo_root/deploy/systemd" run2)
[ ! -s "$scratch/alerts" ] &&
  grep -q 'ok: Go front units (spawn-node-{edge,gw,out}) contain zero spawn.env or DATABASE_URL' <<<"$out" &&
  pass "real repo deploy/systemd Go front units pass credential-health zero-PG audit" || fail "real repo Go units audit"

core_unit=$repo_root/deploy/systemd/spawn-core.service
[ -f "$core_unit" ] &&
  grep -q '^WorkingDirectory=%h/\.local/share/spawn-rsi$' "$core_unit" &&
  grep -q '^EnvironmentFile=%h/\.local/share/spawn-rsi/spawn\.env$' "$core_unit" &&
  grep -q '^Environment=SPAWN_WEB_DIR=' "$core_unit" &&
  grep -q '^Environment=SPAWN_FEATURE_ROOT=' "$core_unit" &&
  grep -q '^RuntimeDirectory=spawn$' "$core_unit" &&
  grep -q '^RuntimeDirectoryMode=0700$' "$core_unit" &&
  grep -q '^UMask=0077$' "$core_unit" &&
  grep -q 'spawn serve --ipc --ipc-sock %t/spawn/core\.sock' "$core_unit" &&
  pass "deploy/systemd/spawn-core.service matches Phase 2 IPC & runtime permissions contract" || fail "spawn-core.service static audit"

go_static_ok=1
for u in spawn-node-edge.service spawn-node-gw.service spawn-node-out.service; do
  p="$repo_root/deploy/systemd/$u"
  [ -f "$p" ] &&
    grep -q '^MemoryMax=[0-9]' "$p" &&
    grep -q '^Environment=GOMEMLIMIT=' "$p" &&
    ! grep -qiE 'spawn\.env|DATABASE_URL' "$p" || go_static_ok=""
done
grep -q -- '--bind-retry-timeout 5s' "$repo_root/deploy/systemd/spawn-node-edge.service" &&
  grep -q -- '--bind-retry-timeout 5s' "$repo_root/deploy/systemd/spawn-node-gw.service" &&
  grep -q '^LoadCredential=typesafe\.api-key:/run/keep/typesafe\.api-key$' "$repo_root/deploy/systemd/spawn-node-out.service" &&
  grep -q '^RestartPreventExitStatus=243 78$' "$repo_root/deploy/systemd/spawn-node-out.service" || go_static_ok=""
[ -n "$go_static_ok" ] &&
  pass "deploy/systemd/spawn-node-{edge,gw,out}.service enforce MemoryMax, GOMEMLIMIT, bind-retry, LoadCredential, zero PG" || fail "spawn-node-*.service static audit"

model_relay_path=$repo_root/deploy/systemd/spawn-model-relay.path
model_relay_svc=$repo_root/deploy/systemd/spawn-model-relay.service
[ -f "$model_relay_path" ] &&
  grep -qxF 'PathChanged=/run/keep/deepseek.api-key' "$model_relay_path" &&
  grep -qxF 'Unit=spawn-model-relay.service' "$model_relay_path" &&
  grep -qxF 'EnvironmentFile=%h/.config/spawn/model-relay.env' "$model_relay_svc" &&
  ! grep -q '^LoadCredential=' "$model_relay_svc" &&
  pass "Wave K1 spawn-model-relay.path staged and spawn-model-relay.service preserves live model-relay.env safety" ||
  fail "Wave K1 spawn-model-relay unit/path static audit"

node_edge_path=$repo_root/deploy/systemd/spawn-node-edge.path
node_edge_svc=$repo_root/deploy/systemd/spawn-node-edge.service
[ -f "$node_edge_path" ] &&
  grep -qxF 'PathChanged=/run/keep/spawn-serve.token' "$node_edge_path" &&
  ! grep -q '^PathExists=' "$node_edge_path" &&
  grep -qxF 'Unit=spawn-node-edge.service' "$node_edge_path" &&
  grep -qxF 'WantedBy=default.target' "$node_edge_path" &&
  grep -qxF 'EnvironmentFile=-%h/.config/spawn/node-edge.env' "$node_edge_svc" &&
  ! grep -q '^LoadCredential=' "$node_edge_svc" &&
  ! grep -qiE 'spawn\.env|DATABASE_URL' "$node_edge_svc" &&
  pass "Wave K2 spawn-node-edge.path staged and spawn-node-edge.service preserves live node-edge.env safety and zero-PG isolation" ||
  fail "Wave K2 spawn-node-edge unit/path static audit"

# --- --validate-manifest dry-run validation (multi-row + unix://%t/..., exit 2 on invalid, zero alerts) ---
rm -f "$scratch/alerts"
cat >"$scratch/valid-multi.manifest" <<'EOF'
# synthetic multi-row manifest for dry-run validation
typesafe.api-key | spawn-node-out.service | unix://%t/spawn/out.sock|/out/v1/health | jev-relay | ~/.config/spawn/typesafe.env | TYPESAFE_API_KEY | -
deepseek.api-key | spawn-model-relay.service | http://127.0.0.1:8791/healthz | model-relay | ~/.config/spawn/model-relay.env | DEEPSEEK_API_KEY | -
EOF
val_out=$(CH_FAKE_ALERTS="$scratch/alerts" bash "$check" --validate-manifest "$scratch/valid-multi.manifest" 2>&1)
[ "$?" -eq 0 ] && grep -q 'manifest ok: 2 row(s) valid' <<<"$val_out" && [ ! -e "$scratch/alerts" ] &&
  pass "--validate-manifest accepts valid multi-row unix://%t + http:// manifest with zero alerts" || fail "--validate-manifest valid ($val_out)"

# Wave K1 staged manifest (legacy jev-relay + model-relay) dry-run validation.
cat >"$scratch/wave-k1.manifest" <<'EOF'
# Wave K1 staged manifest: active typesafe.api-key + staged deepseek.api-key
typesafe.api-key | spawn-jev-relay.service | http://127.0.0.1:8790/healthz | jev-relay | ~/.config/spawn/typesafe.env | TYPESAFE_API_KEY | -
deepseek.api-key | spawn-model-relay.service | http://127.0.0.1:8791/healthz | model-relay | ~/.config/spawn/model-relay.env | DEEPSEEK_API_KEY | -
EOF
k1_out=$(CH_FAKE_ALERTS="$scratch/alerts" bash "$check" --validate-manifest "$scratch/wave-k1.manifest" 2>&1)
[ "$?" -eq 0 ] && grep -q 'manifest ok: 2 row(s) valid' <<<"$k1_out" && [ ! -e "$scratch/alerts" ] &&
  pass "--validate-manifest accepts Wave K1 multi-row manifest (spawn-jev-relay + spawn-model-relay) with zero alerts" || fail "--validate-manifest Wave K1 ($k1_out)"

# Wave K2 staged 3-row manifest (jev-relay + model-relay + node-edge) dry-run validation.
cat >"$scratch/wave-k2.manifest" <<'EOF'
# Wave K2 staged manifest: active typesafe.api-key + Wave K1 deepseek.api-key + Wave K2 spawn-serve.token
typesafe.api-key  | spawn-jev-relay.service   | http://127.0.0.1:8790/healthz | jev-relay   | ~/.config/spawn/typesafe.env    | TYPESAFE_API_KEY  | -
deepseek.api-key  | spawn-model-relay.service | http://127.0.0.1:8791/healthz | model-relay | ~/.config/spawn/model-relay.env | DEEPSEEK_API_KEY  | -
spawn-serve.token | spawn-node-edge.service   | -                             | -           | ~/.config/spawn/node-edge.env   | SPAWN_SERVE_TOKEN | -
EOF
k2_out=$(CH_FAKE_ALERTS="$scratch/alerts" bash "$check" --validate-manifest "$scratch/wave-k2.manifest" 2>&1)
[ "$?" -eq 0 ] && grep -q 'manifest ok: 3 row(s) valid' <<<"$k2_out" && [ ! -e "$scratch/alerts" ] &&
  pass "--validate-manifest accepts Wave K2 3-row manifest (typesafe + deepseek + spawn-serve.token) with zero alerts" || fail "--validate-manifest Wave K2 ($k2_out)"

printf 'bad.row|only-three-fields|-\n' >"$scratch/invalid.manifest"
val_bad_out=$(CH_FAKE_ALERTS="$scratch/alerts" bash "$check" --validate-manifest "$scratch/invalid.manifest" 2>&1); st=$?
[ "$st" -eq 2 ] && grep -q 'nothing checked' <<<"$val_bad_out" && [ ! -e "$scratch/alerts" ] &&
  pass "--validate-manifest rejects invalid row with exit 2 and zero alerts" || fail "--validate-manifest invalid row ($st: $val_bad_out)"

val_miss_out=$(CH_FAKE_ALERTS="$scratch/alerts" bash "$check" --validate-manifest "$scratch/does-not-exist.manifest" 2>&1); st=$?
[ "$st" -eq 2 ] && [ ! -e "$scratch/alerts" ] &&
  pass "--validate-manifest rejects missing file with exit 2" || fail "--validate-manifest missing ($st)"

val_noarg_out=$(bash "$check" --validate-manifest 2>&1); st=$?
[ "$st" -eq 2 ] &&
  pass "--validate-manifest without path exits 2" || fail "--validate-manifest noarg ($st)"

# --- --audit-keep-migration (real deploy/systemd + synthetic sandbox + canary secret isolation) ---
rm -f "$scratch/alerts"
real_audit_json=$(CH_DEPLOY_SYSTEMD_DIR="" CH_UNIT_DIR="" CH_FAKE_ALERTS="$scratch/alerts" \
  SPAWN_DATABASE_URL="" bash "$check" --audit-keep-migration --json)
st=$?
[ "$st" -eq 0 ] && [ ! -e "$scratch/alerts" ] && jq -e '
  .go_units_pg_isolation.checked == true and
  .go_units_pg_isolation.ok == true and
  (.go_units_pg_isolation.violations | length == 0) and
  (.counts.total == .counts.keep_managed + .counts.no_secrets + .counts.pending_migration) and
  (.counts.keep_managed >= 2) and
  (.counts.no_secrets == 5) and
  (.counts.pending_migration == 20) and
  (.units | map(select(.unit == "spawn-jev-relay.service" and .status == "keep_managed" and (.load_credentials | index("typesafe.api-key")) != null)) | length == 1) and
  (.units | map(select(.unit == "spawn-node-out.service" and .status == "keep_managed" and (.load_credentials | index("typesafe.api-key")) != null)) | length == 1) and
  (.units | map(select(.unit == "spawn-agent-presence.service" and .status == "no_secrets")) | length == 1) and
  (.units | map(select(.unit == "spawn-node-gw.service" and .status == "no_secrets" and (.suggested_keep_ids | length == 0))) | length == 1) and
  (.units | map(select(.unit == "spawn-node-edge.service" and .status == "pending_migration" and (.suggested_keep_ids | index("spawn-serve.token")) != null)) | length == 1) and
  (.units | map(select(.unit == "spawn-model-relay.service" and .status == "pending_migration" and (.suggested_keep_ids | index("deepseek.api-key")) != null)) | length == 1) and
  (.units | map(select(.unit == "spawn-loop.service" and .status == "pending_migration" and (.env_files | length > 0))) | length == 1)
' <<<"$real_audit_json" >/dev/null &&
  pass "--audit-keep-migration --json accurately classifies real repo deploy/systemd units" || fail "--audit-keep-migration --json real ($real_audit_json)"

real_audit_human=$(CH_DEPLOY_SYSTEMD_DIR="" CH_UNIT_DIR="" SPAWN_DATABASE_URL="" bash "$check" --audit-keep-migration)
grep -q 'keep_managed.*spawn-jev-relay\.service' <<<"$real_audit_human" &&
  grep -q 'keep_managed.*spawn-node-out\.service' <<<"$real_audit_human" &&
  grep -q 'no_secrets.*spawn-node-gw\.service' <<<"$real_audit_human" &&
  grep -q 'pending_migration.*spawn-node-edge\.service' <<<"$real_audit_human" &&
  grep -q 'pending_migration.*spawn-model-relay\.service' <<<"$real_audit_human" &&
  grep -q 'go_units_pg_isolation=ok' <<<"$real_audit_human" &&
  pass "--audit-keep-migration human output summarizes repo units and PG isolation" || fail "--audit-keep-migration human"

# Synthetic sandbox audit directory with canary secret inside referenced .env files.
audit_home=$scratch/audit-home
audit_sys=$scratch/audit-units
mkdir -p "$audit_home/.config/spawn" "$audit_home/.local/share/spawn-rsi" "$audit_sys/spawn-node-edge.service.d"
printf 'CANARY_SECRET=made-up-audit-canary-value-9941\n' >"$audit_home/.config/spawn/demo.env"
printf 'SPAWN_DATABASE_URL=postgres://user:made-up-audit-canary-value-9941@127.0.0.1:5432/db\n' >"$audit_home/.local/share/spawn-rsi/spawn.env"
chmod 600 "$audit_home/.config/spawn/demo.env" "$audit_home/.local/share/spawn-rsi/spawn.env"

cat >"$audit_sys/spawn-alpha-keep.service" <<'EOF'
[Service]
LoadCredential=alpha.key:/run/keep/alpha.key
EOF
cat >"$audit_sys/spawn-node-out.service" <<'EOF'
[Service]
LoadCredential=typesafe.api-key:/run/keep/typesafe.api-key
EnvironmentFile=-%h/.config/spawn/node-out.env
EOF
cat >"$audit_sys/spawn-mixed-pending.service" <<'EOF'
[Service]
LoadCredential=alpha.key:/run/keep/alpha.key
EnvironmentFile=%h/.config/spawn/demo.env
EOF
cat >"$audit_sys/spawn-plain-pending.service" <<'EOF'
[Service]
EnvironmentFile=%h/.local/share/spawn-rsi/spawn.env
EOF
cat >"$audit_sys/spawn-clean.service" <<'EOF'
[Service]
Environment=PATH=/usr/bin:/bin
EOF
cat >"$audit_sys/spawn-node-edge.service" <<'EOF'
[Service]
EnvironmentFile=-%h/.config/spawn/node-edge.env
EOF
cat >"$audit_sys/spawn-node-edge.service.d/bad.conf" <<'EOF'
[Service]
Environment=BAD_DATABASE_URL=postgres://synthetic-dropin-leak
EOF
cat >"$audit_sys/spawn-node-gw.service" <<'EOF'
[Service]
EnvironmentFile=-%h/.config/spawn/node-gw.env
EOF

syn_json=$(HOME="$audit_home" CH_DEPLOY_SYSTEMD_DIR="$audit_sys" CH_FAKE_ALERTS="$scratch/alerts" \
  bash "$check" --audit-keep-migration --json)
syn_human=$(HOME="$audit_home" CH_DEPLOY_SYSTEMD_DIR="$audit_sys" CH_FAKE_ALERTS="$scratch/alerts" \
  bash "$check" --audit-keep-migration)

jq -e '
  .counts.total == 7 and
  .counts.keep_managed == 2 and
  .counts.no_secrets == 2 and
  .counts.pending_migration == 3 and
  .go_units_pg_isolation.checked == true and
  .go_units_pg_isolation.ok == false and
  (.go_units_pg_isolation.violations == ["spawn-node-edge.service"]) and
  (.units | map(select(.unit == "spawn-node-gw.service" and .status == "no_secrets" and (.suggested_keep_ids | length == 0))) | length == 1) and
  (.units | map(select(.unit == "spawn-node-edge.service" and .status == "pending_migration" and (.suggested_keep_ids | index("spawn-serve.token")) != null)) | length == 1) and
  (.units | map(select(.unit == "spawn-mixed-pending.service" and .status == "pending_migration")) | length == 1) and
  (.units | map(select(.unit == "spawn-plain-pending.service" and .status == "pending_migration" and (.suggested_keep_ids | index("spawn.database-url")) != null)) | length == 1)
' <<<"$syn_json" >/dev/null &&
  [ ! -e "$scratch/alerts" ] &&
  pass "--audit-keep-migration classifies synthetic mixed/optional/Go units and writes zero alerts" || fail "--audit-keep-migration synthetic ($syn_json)"

# Also verify go_units_pg_isolation catches spawn-node-gw.service violations in --audit-keep-migration.
mkdir -p "$audit_sys/spawn-node-gw.service.d"
printf '%s\n' '[Service]' 'EnvironmentFile=%h/.local/share/spawn-rsi/spawn.env' >"$audit_sys/spawn-node-gw.service.d/bad.conf"
syn_gw_json=$(HOME="$audit_home" CH_DEPLOY_SYSTEMD_DIR="$audit_sys" CH_FAKE_ALERTS="$scratch/alerts" \
  bash "$check" --audit-keep-migration --json)
jq -e '
  .go_units_pg_isolation.checked == true and
  .go_units_pg_isolation.ok == false and
  (.go_units_pg_isolation.violations == ["spawn-node-edge.service", "spawn-node-gw.service"]) and
  (.units | map(select(.unit == "spawn-node-gw.service" and .status == "pending_migration")) | length == 1)
' <<<"$syn_gw_json" >/dev/null &&
  pass "--audit-keep-migration catches spawn-node-gw.service PG violation" || fail "--audit-keep-migration gw violation ($syn_gw_json)"

! grep -q 'made-up-audit-canary-value-9941' <<<"$syn_json$syn_human$syn_gw_json" &&
  ! grep -q 'synthetic-dropin-leak' <<<"$syn_json$syn_human$syn_gw_json" &&
  pass "--audit-keep-migration never reads or echoes env file contents or canary secrets" || fail "canary leaked in audit"

echo "$fails failed"
[ "$fails" -eq 0 ]
