#!/usr/bin/env bash
# deploy/sync-units.sh against a scratch unit directory and a fake
# `systemctl` that only logs; no live unit, no systemd. About 1 s.
#
#   bash deploy/test-sync-units.sh
#
# Covered: --check names each installed unit that differs and changes
# nothing, calls no systemctl, and exits 1 (also for an unlisted host-local
# unit or drop-in alone), 0 once in sync; --apply installs each differing
# unit with its mode kept, leaves units not installed, symlinked units and
# host-local units and drop-ins alone, daemon-reloads once, try-restarts a
# changed timer or path unit and no service, says which service needs a
# restart, and run again changes nothing and calls no systemctl; a unit it
# cannot write exits 2 and leaves no temporary file; usage exits 2; no
# unit's text is ever printed (a value planted in a unit and a drop-in is
# never in the output); the repo's deploy/systemd and host-local agree (no
# name in both); spawn-node-gateway.service is in deploy/systemd and reads
# its own node-gateway.env, never spawn.env; deploy.sh syncs before it restarts the services, and
# cutover-controller.sh runs the sync.
#
# TEST_SYNC_UNITS_SCRIPT=<path> runs that script instead of the checkout's.
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
script=${TEST_SYNC_UNITS_SCRIPT:-$here/deploy/sync-units.sh}
scratch=$(mktemp -d "${TMPDIR:-/tmp}/test-sync-units.XXXXXX")
trap 'chmod -R u+w "$scratch" 2>/dev/null; rm -rf "$scratch"' EXIT

failures=0 passed=0
check() { # check <description> <command...>
  local what=$1
  shift
  if "$@" >/dev/null 2>&1; then
    printf 'ok   %s\n' "$what"
    passed=$((passed + 1))
  else
    printf 'FAIL %s\n' "$what"
    failures=$((failures + 1))
  fi
}

src=$scratch/src units=$scratch/units bin=$scratch/bin
mkdir -p "$src" "$units/spawn-serve.service.d" "$units/spawn-x.service.d" "$bin"
cat >"$bin/systemctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >>"$SYSTEMCTL_LOG"
EOF
chmod 755 "$bin/systemctl"
export SYSTEMCTL_LOG=$scratch/systemctl.log SPAWN_UNIT_DIR=$units PATH="$bin:$PATH"

# The commit's units.
printf '[Service]\nExecStart=/bin/true\n' >"$src/spawn-same.service"
printf '[Service]\nExecStart=/bin/new\n' >"$src/spawn-changed.service"
printf '[Service]\nExecStart=/bin/new\n' >"$src/spawn-ro.service"
printf '[Timer]\nOnCalendar=hourly\n' >"$src/spawn-changed.timer"
printf '[Path]\nPathChanged=/new\n' >"$src/spawn-changed.path"
printf '[Service]\nExecStart=/bin/new\n' >"$src/spawn-serve.service"
printf '[Service]\nExecStart=/bin/absent\n' >"$src/spawn-absent.service"
printf '[Service]\nExecStart=/bin/linked\n' >"$src/spawn-linked.service"
cat >"$src/host-local" <<'EOF'
# comment
spawn-local.service   the host's own   # trailing comment
spawn-x.service.d/50-local.conf
EOF
# What the host installed.
cp "$src/spawn-same.service" "$units/"
printf '[Service]\nExecStart=/bin/old\nEnvironment=PLANTED=unit-value-never-printed\n' >"$units/spawn-changed.service"
printf '[Service]\nExecStart=/bin/old\n' >"$units/spawn-ro.service"
chmod 444 "$units/spawn-ro.service"
printf '[Timer]\nOnCalendar=daily\n' >"$units/spawn-changed.timer"
printf '[Path]\nPathChanged=/old\n' >"$units/spawn-changed.path"
printf '[Service]\nExecStart=/bin/old\n' >"$units/spawn-serve.service"
printf '[Service]\nExecStart=/bin/elsewhere\n' >"$scratch/elsewhere.service"
ln -s "$scratch/elsewhere.service" "$units/spawn-linked.service"
printf '[Service]\nExecStart=/bin/local\n' >"$units/spawn-local.service"
printf '[Service]\nEnvironment=LOCAL=dropin-value-never-printed\n' >"$units/spawn-x.service.d/50-local.conf"
printf '[Service]\nEnvironment=STRAY=stray-value-never-printed\n' >"$units/spawn-serve.service.d/stray.conf"
printf '[Service]\nExecStart=/bin/stray\n' >"$units/spawn-stray.service"
printf 'not a unit\n' >"$units/spawn-notes.txt"

snapshot() { (cd "$units" && find . -printf '%p %m %l\n' | LC_ALL=C sort && find . -type f -exec sha256sum {} + | LC_ALL=C sort); }
no_values() { ! grep -q 'never-printed' "$1"; }

# --check
before=$(snapshot)
status=0
bash "$script" --check "$src" >"$scratch/check.out" 2>&1 || status=$?
check "--check exits 1 when a unit differs (got $status)" [ "$status" = 1 ]
for name in spawn-changed.service spawn-ro.service spawn-changed.timer spawn-changed.path spawn-serve.service; do
  check "--check names $name" grep -q "^units: differs: $name " "$scratch/check.out"
done
check "--check does not name the unit that is the same" bash -c '! grep -q "differs: spawn-same.service" "$1"' _ "$scratch/check.out"
check "--check names the unit not installed" grep -q "^units: not installed here.*spawn-absent.service" "$scratch/check.out"
check "--check names the symlink" grep -q "^units: symlinks, left alone: spawn-linked.service" "$scratch/check.out"
check "--check names the listed host-local unit and drop-in" \
  grep -q "^units: host-local, listed: spawn-local.service spawn-x.service.d/50-local.conf" "$scratch/check.out"
check "--check warns of the unlisted unit and drop-in" \
  grep -q "^units: WARNING host-local and not listed.*spawn-stray.service.*spawn-serve.service.d/stray.conf" "$scratch/check.out"
check "--check ignores a file that is no unit" bash -c '! grep -q "spawn-notes.txt" "$1"' _ "$scratch/check.out"
check "--check changes nothing" [ "$before" = "$(snapshot)" ]
check "--check calls no systemctl" [ ! -e "$SYSTEMCTL_LOG" ]
check "--check prints no unit text" no_values "$scratch/check.out"

# --apply
status=0
bash "$script" --apply "$src" >"$scratch/apply.out" 2>&1 || status=$?
check "--apply exits 0 (got $status)" [ "$status" = 0 ]
for name in spawn-changed.service spawn-ro.service spawn-changed.timer spawn-changed.path spawn-serve.service; do
  check "--apply installed $name" cmp -s "$src/$name" "$units/$name"
  check "--apply says it installed $name" grep -qx "units: installed $name" "$scratch/apply.out"
done
check "--apply kept a read-only unit read-only" [ "$(stat -c %a "$units/spawn-ro.service")" = 444 ]
check "--apply kept the default mode" [ "$(stat -c %a "$units/spawn-changed.service")" = "$(stat -c %a "$units/spawn-same.service")" ]
check "--apply did not install the unit not installed" [ ! -e "$units/spawn-absent.service" ]
check "--apply left the symlink a symlink to its file" \
  bash -c '[ -L "$1/spawn-linked.service" ] && grep -q elsewhere "$2/elsewhere.service"' _ "$units" "$scratch"
check "--apply left the host-local unit" grep -q /bin/local "$units/spawn-local.service"
check "--apply left the listed drop-in" grep -q LOCAL= "$units/spawn-x.service.d/50-local.conf"
check "--apply left the unlisted drop-in and unit" \
  bash -c '[ -f "$1/spawn-serve.service.d/stray.conf" ] && [ -f "$1/spawn-stray.service" ]' _ "$units"
check "--apply still warns of the unlisted" grep -q "^units: WARNING host-local and not listed" "$scratch/apply.out"
check "--apply left no temporary file" bash -c '[ -z "$(find "$1" -name ".*.sync.*")" ]' _ "$units"
check "--apply daemon-reloaded once, first" bash -c '[ "$(grep -c daemon-reload "$1")" = 1 ] && head -n 1 "$1" | grep -qx -- "--user daemon-reload"' _ "$SYSTEMCTL_LOG"
check "--apply try-restarted the timer" grep -qx -- "--user try-restart spawn-changed.timer" "$SYSTEMCTL_LOG"
check "--apply try-restarted the path unit" grep -qx -- "--user try-restart spawn-changed.path" "$SYSTEMCTL_LOG"
check "--apply restarted no service" bash -c '! grep -q "\.service" "$1"' _ "$SYSTEMCTL_LOG"
check "--apply names the restart a changed service needs" \
  grep -q "^units: spawn-changed.service takes effect at its next start (now: systemctl --user try-restart spawn-changed.service)" "$scratch/apply.out"
check "--apply leaves spawn-serve's restart to deploy.sh" bash -c '! grep -q "spawn-serve.service takes effect" "$1"' _ "$scratch/apply.out"
check "--apply prints no unit text" no_values "$scratch/apply.out"

# Again: nothing to do.
rm -f "$SYSTEMCTL_LOG"
before=$(snapshot)
status=0
bash "$script" --apply "$src" >"$scratch/again.out" 2>&1 || status=$?
check "--apply again exits 0 (got $status)" [ "$status" = 0 ]
check "--apply again changes nothing" [ "$before" = "$(snapshot)" ]
check "--apply again calls no systemctl" [ ! -e "$SYSTEMCTL_LOG" ]
check "--apply again counts 6 the same, 0 installed" grep -q ": 6 the same, 0 installed$" "$scratch/again.out"

# --check: an unlisted host-local file alone exits 1; with none, 0.
status=0
bash "$script" --check "$src" >/dev/null 2>&1 || status=$?
check "--check exits 1 for an unlisted host-local file alone (got $status)" [ "$status" = 1 ]
rm -f "$units/spawn-stray.service" "$units/spawn-serve.service.d/stray.conf"
status=0
bash "$script" --check "$src" >"$scratch/clean.out" 2>&1 || status=$?
check "--check exits 0 in sync (got $status)" [ "$status" = 0 ]
check "--check in sync warns of nothing" bash -c '! grep -q WARNING "$1"' _ "$scratch/clean.out"

# A unit it cannot write (not as root, whom a mode does not stop).
if [ "$(id -u)" != 0 ]; then
  printf '[Service]\nExecStart=/bin/newer\n' >"$src/spawn-changed.service"
  chmod 555 "$units"
  status=0
  bash "$script" --apply "$src" >"$scratch/ro.out" 2>&1 || status=$?
  chmod 755 "$units"
  check "--apply exits 2 when it cannot write a unit (got $status)" [ "$status" = 2 ]
  check "and says which" grep -q "could not install $units/spawn-changed.service" "$scratch/ro.out"
  check "and left the unit and no temporary file" \
    bash -c 'grep -q /bin/new "$1/spawn-changed.service" && [ -z "$(find "$1" -name ".*.sync.*")" ]' _ "$units"
fi

# Usage.
status=0
bash "$script" >/dev/null 2>&1 || status=$?
check "no mode exits 2 (got $status)" [ "$status" = 2 ]
status=0
bash "$script" --check "$scratch/none" >/dev/null 2>&1 || status=$?
check "a missing units directory exits 2 (got $status)" [ "$status" = 2 ]

# The repo's own units and list.
repo_units=$here/deploy/systemd
check "deploy/systemd/host-local exists" [ -f "$repo_units/host-local" ]
both=$(sed -e 's/#.*//' "$repo_units/host-local" | awk 'NF { print $1 }' | while read -r name; do
  [ ! -e "$repo_units/$name" ] || echo "$name"
done)
check "host-local names no unit deploy/systemd has (${both:-none})" [ -z "$both" ]
check "every deploy/systemd/spawn-* is a regular file" \
  bash -c 'for f in "$1"/spawn-*; do [ -f "$f" ] && [ ! -L "$f" ] || exit 1; done' _ "$repo_units"
check "spawn-node-gateway.service is in deploy/systemd" [ -f "$repo_units/spawn-node-gateway.service" ]
check "spawn-node-gateway.service uses node-gateway.env, never spawn.env" \
  bash -c 'grep -qxF "EnvironmentFile=%h/.config/spawn/node-gateway.env" "$1" && ! grep -q "spawn\.env" "$1"' \
  _ "$repo_units/spawn-node-gateway.service"
check "deploy.sh syncs the units before it restarts the services" \
  bash -c 'awk "/sync-units.sh\" --apply/ { s = NR } /^restart\$/ && s && !r { r = NR } END { exit !(s && r > s) }" "$1"' _ "$here/deploy/deploy.sh"
check "cutover-controller.sh runs the sync" grep -q 'deploy/sync-units.sh" --apply' "$here/deploy/cutover-controller.sh"

# Phase 2 opt-in SPAWN_DEPLOY_EDGE_MODE=legacy|go-uds tests.
p2_src=$scratch/p2-src p2_units=$scratch/p2-units
mkdir -p "$p2_src" "$p2_units"
cat >"$p2_src/spawn-core.service" <<'EOF'
[Service]
EnvironmentFile=%h/.local/share/spawn-rsi/spawn.env
ExecStart=%h/.local/share/spawn-rsi/bin/spawn serve --ipc --ipc-sock %t/spawn/core.sock
EOF
cat >"$p2_src/spawn-node-edge.service" <<'EOF'
[Service]
ExecStart=%h/.local/share/spawn-rsi/bin/spawn-node front --serve edge --core-sock %t/spawn/core.sock
EOF
cat >"$p2_src/spawn-node-gw.service" <<'EOF'
[Service]
EnvironmentFile=-%h/.config/spawn/node-gw.env
ExecStart=%h/.local/share/spawn-rsi/bin/spawn-node front --serve node --core-sock %t/spawn/core.sock
EOF
cat >"$p2_src/spawn-node-out.service" <<'EOF'
[Service]
LoadCredential=typesafe.api-key:/run/keep/typesafe.api-key
RestartPreventExitStatus=243 78
ExecStart=%h/.local/share/spawn-rsi/bin/spawn-node front --serve outbound --out-sock %t/spawn/out.sock
EOF

rm -f "$SYSTEMCTL_LOG"
status=0
SPAWN_UNIT_DIR="$p2_units" SPAWN_DEPLOY_EDGE_MODE=bogus bash "$script" --check "$p2_src" >"$scratch/p2-bogus.out" 2>&1 || status=$?
check "invalid SPAWN_DEPLOY_EDGE_MODE exits 2 (got $status)" [ "$status" = 2 ]
check "invalid SPAWN_DEPLOY_EDGE_MODE calls no systemctl" [ ! -e "$SYSTEMCTL_LOG" ]

status=0
SPAWN_UNIT_DIR="$p2_units" SPAWN_DEPLOY_EDGE_MODE=legacy bash "$script" --apply "$p2_src" >"$scratch/p2-legacy.out" 2>&1 || status=$?
check "legacy mode does not install absent Phase 2 units" \
  bash -c '[ "$1" = 0 ] && [ ! -e "$2/spawn-core.service" ] && [ ! -e "$2/spawn-node-edge.service" ]' _ "$status" "$p2_units"

rm -f "$SYSTEMCTL_LOG"
status=0
SPAWN_UNIT_DIR="$p2_units" SPAWN_DEPLOY_EDGE_MODE=go-uds bash "$script" --check "$p2_src" >"$scratch/p2-check.out" 2>&1 || status=$?
check "go-uds --check exits 1 when Phase 2 units are not installed yet (got $status)" [ "$status" = 1 ]
check "go-uds --check installs nothing and calls no systemctl" \
  bash -c '[ ! -e "$1/spawn-core.service" ] && [ ! -e "$2" ]' _ "$p2_units" "$SYSTEMCTL_LOG"

status=0
SPAWN_UNIT_DIR="$p2_units" SPAWN_DEPLOY_EDGE_MODE=go-uds bash "$script" --apply "$p2_src" >"$scratch/p2-apply.out" 2>&1 || status=$?
check "go-uds --apply exits 0 (got $status)" [ "$status" = 0 ]
for u in spawn-core.service spawn-node-edge.service spawn-node-gw.service spawn-node-out.service; do
  check "go-uds --apply installed $u (0644)" \
    bash -c 'cmp -s "$1/$3" "$2/$3" && [ "$(stat -c %a "$2/$3")" = 644 ]' _ "$p2_src" "$p2_units" "$u"
  check "go-uds --apply suppresses manual try-restart hint for $u" \
    bash -c '! grep -q "$2 takes effect at its next start" "$1"' _ "$scratch/p2-apply.out" "$u"
done
check "go-uds --apply daemon-reloaded once" [ "$(grep -cx -- "--user daemon-reload" "$SYSTEMCTL_LOG")" -eq 1 ]

rm -f "$SYSTEMCTL_LOG"
status=0
SPAWN_UNIT_DIR="$p2_units" SPAWN_DEPLOY_EDGE_MODE=go-uds bash "$script" --apply "$p2_src" >"$scratch/p2-again.out" 2>&1 || status=$?
check "go-uds --apply again is a no-op (exits 0, no systemctl)" \
  bash -c '[ "$1" = 0 ] && [ ! -e "$2" ]' _ "$status" "$SYSTEMCTL_LOG"

# Fail-Closed go-uds pre-validations (missing unit, PG leak in unit or drop-in, missing LoadCredential/RestartPreventExitStatus).
p2_bad=$scratch/p2-bad p2_bad_units=$scratch/p2-bad-units
check_go_uds_refused() {
  local label=$1 prep=$2
  rm -rf "$p2_bad" "$p2_bad_units" "$SYSTEMCTL_LOG"
  cp -r "$p2_src" "$p2_bad"
  mkdir -p "$p2_bad_units"
  eval "$prep"
  status=0
  SPAWN_UNIT_DIR="$p2_bad_units" SPAWN_DEPLOY_EDGE_MODE=go-uds bash "$script" --apply "$p2_bad" >"$scratch/p2-refused.out" 2>&1 || status=$?
  check "$label: exits 2 (got $status)" [ "$status" = 2 ]
  check "$label: installed nothing" [ -z "$(ls -A "$p2_bad_units" | grep -v '\.d$' || true)" ]
  check "$label: calls no systemctl" [ ! -e "$SYSTEMCTL_LOG" ]
  check "$label: prints no planted secret sentinel" bash -c '! grep -qF P2_UNIT_LEAK_SENTINEL "$1"' _ "$scratch/p2-refused.out"
}
check_go_uds_refused "missing Phase 2 unit" 'rm -f "$p2_bad/spawn-node-out.service"'
check_go_uds_refused "Go edge unit referencing spawn.env" 'printf "EnvironmentFile=%s/spawn.env\n# P2_UNIT_LEAK_SENTINEL\n" "$p2_bad" >>"$p2_bad/spawn-node-edge.service"'
check_go_uds_refused "Go gw unit referencing DATABASE_URL" 'printf "Environment=SPAWN_DATABASE_URL=P2_UNIT_LEAK_SENTINEL\n" >>"$p2_bad/spawn-node-gw.service"'
check_go_uds_refused "Go out drop-in referencing DATABASE_URL" 'mkdir -p "$p2_bad_units/spawn-node-out.service.d" && printf "[Service]\nEnvironment=SPAWN_DATABASE_URL=P2_UNIT_LEAK_SENTINEL\n" >"$p2_bad_units/spawn-node-out.service.d/99-leak.conf"'
check_go_uds_refused "spawn-node-out missing LoadCredential" 'grep -v "^LoadCredential=" "$p2_bad/spawn-node-out.service" >"$p2_bad/out.tmp" && mv "$p2_bad/out.tmp" "$p2_bad/spawn-node-out.service"'
check_go_uds_refused "spawn-node-out missing RestartPreventExitStatus" 'grep -v "^RestartPreventExitStatus=" "$p2_bad/spawn-node-out.service" >"$p2_bad/out.tmp" && mv "$p2_bad/out.tmp" "$p2_bad/spawn-node-out.service"'

# Real repository deploy/systemd Phase 2 contract checks.
for u in spawn-core.service spawn-node-edge.service spawn-node-gw.service spawn-node-out.service; do
  check "repo deploy/systemd/$u is a regular file" bash -c '[ -f "$1/$2" ] && [ ! -L "$1/$2" ]' _ "$repo_units" "$u"
done
for u in spawn-node-edge.service spawn-node-gw.service spawn-node-out.service; do
  check "repo $u has zero spawn.env or DATABASE_URL references" bash -c '! grep -qiE "spawn\.env|DATABASE_URL" "$1/$2"' _ "$repo_units" "$u"
done
check "repo spawn-node-out.service declares LoadCredential and RestartPreventExitStatus=243 78" \
  bash -c 'grep -qE "^[[:space:]]*LoadCredential=typesafe\.api-key:/run/keep/typesafe\.api-key[[:space:]]*$" "$1" && grep -qE "^[[:space:]]*RestartPreventExitStatus=243[[:space:]]+78[[:space:]]*$" "$1"' \
  _ "$repo_units/spawn-node-out.service"

echo "sync-units: $passed passed, $failures failed"
[ "$failures" = 0 ] && echo "sync-units: all cases pass"
[ "$failures" = 0 ]
