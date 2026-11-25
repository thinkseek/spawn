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
# name in both); deploy.sh syncs before it restarts the services, and
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
check "deploy.sh syncs the units before it restarts the services" \
  bash -c 'awk "/sync-units.sh\" --apply/ { s = NR } /^restart\$/ && s && !r { r = NR } END { exit !(s && r > s) }" "$1"' _ "$here/deploy/deploy.sh"
check "cutover-controller.sh runs the sync" grep -q 'deploy/sync-units.sh" --apply' "$here/deploy/cutover-controller.sh"

echo "sync-units: $passed passed, $failures failed"
[ "$failures" = 0 ] && echo "sync-units: all cases pass"
[ "$failures" = 0 ]
