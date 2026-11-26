#!/usr/bin/env bash
# The systemd user units this host has installed, kept the same as a
# commit's deploy/systemd (issue #98, PR-4).
#
#   deploy/sync-units.sh --check [<units-dir>]   # names what differs; changes nothing
#   deploy/sync-units.sh --apply [<units-dir>]   # installs what differs, daemon-reload
#
# <units-dir> is a commit's deploy/systemd (default: the one beside this
# script). deploy/deploy.sh runs --apply with the deployed controller's copy
# on every deploy, after main moved and before the services restart, and
# deploy/cutover-controller.sh step 3 runs it too. Before this, nothing
# installed a unit after the first time: the cutover did it once per host
# for four units, the README's `cp` by hand for the rest, and a deploy only
# the binary, the UI, the controller and the relays' scripts. So
# spawn-maintenance.service kept SPAWN_MAINTENANCE_SCRATCH_AGE_MIN=180 for
# two weeks after main removed it (6d852b3a6, 2026-11-10): the timer ran the
# new script with the old unit.
#
# Only units already installed are updated: which units a host runs, and
# enabling one, stays the owner's step (deploy/systemd/README.md). A unit
# here and not installed is named, on one line. Each installed unit that
# differs byte for byte gets the commit's file (written beside it with its
# mode kept, then renamed over it), and one daemon-reload follows. A
# changed .timer or .path is try-restarted, so its new schedule or trigger
# holds now; a changed service takes effect at its next start (a oneshot at
# its next run; deploy.sh restarts spawn-loop, spawn-integrate and
# spawn-serve right after, and try-restarts a relay whose unit changed);
# any other is named with the command that applies it now. An installed
# unit that is a symlink is left alone and named.
#
# Host-local units and drop-ins: an installed spawn-* unit with no file in
# <units-dir>, and every drop-in (spawn-*.d/*.conf), is this host's own.
# Each must be listed in <units-dir>/host-local with its reason; one that is
# not is named as unlisted (--check then exits 1). Neither mode changes a
# host-local file.
#
# It never prints a unit's text: a host's unit or drop-in may carry an
# Environment= value that is not the commit's. To see a difference, run the
# `diff -u` it names, by hand.
#
# Exit status: 0 in sync (--check) or synced (--apply); 1 --check found a
# unit to install/sync or an unlisted host-local file; 2 usage, invalid
# SPAWN_DEPLOY_EDGE_MODE, failed Phase 2 go-uds pre-check, or a unit that
# could not be written (--apply stops at the first).
#
# SPAWN_DEPLOY_EDGE_MODE=legacy (default): existing behavior; absent units are
# only named. SPAWN_DEPLOY_EDGE_MODE=go-uds: validates the 4 Phase 2 units
# (spawn-core, spawn-node-{edge,gw,out}) for PG credential isolation and
# Keep LoadCredential/RestartPreventExitStatus first, and installs missing
# Phase 2 units on --apply (or flags them as missing on --check).
set -euo pipefail

say() { printf 'units: %s\n' "$*"; }
mode=${1-}
case $mode in
--check | --apply) ;;
*)
  echo "usage: deploy/sync-units.sh --check|--apply [<units-dir>]" >&2
  exit 2
  ;;
esac
edge_mode=${SPAWN_DEPLOY_EDGE_MODE:-legacy}
case $edge_mode in
legacy | go-uds) ;;
*)
  echo "units: invalid SPAWN_DEPLOY_EDGE_MODE '$edge_mode' (expected legacy or go-uds)" >&2
  exit 2
  ;;
esac
src=${2:-$(cd "$(dirname "$0")" && pwd)/systemd}
[ -d "$src" ] || {
  echo "units: no directory $src" >&2
  exit 2
}
unit_dir=${SPAWN_UNIT_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user}

is_phase2_unit() {
  case $1 in
  spawn-core.service | spawn-node-edge.service | spawn-node-gw.service | spawn-node-out.service)
    return 0
    ;;
  *) return 1 ;;
  esac
}

check_phase2_units_dir() {
  local dir=$1 u conf
  for u in spawn-core.service spawn-node-edge.service spawn-node-gw.service spawn-node-out.service; do
    if [ ! -f "$dir/$u" ] || [ -L "$dir/$u" ]; then
      echo "units: go-uds mode requires regular unit file $dir/$u" >&2
      exit 2
    fi
  done
  for u in spawn-node-edge.service spawn-node-gw.service spawn-node-out.service; do
    if grep -qiE 'spawn\.env|DATABASE_URL' "$dir/$u"; then
      echo "units: go-uds mode refused: Go front unit $u references spawn.env or DATABASE_URL" >&2
      exit 2
    fi
    shopt -s nullglob
    for conf in "$unit_dir/${u}.d"/*.conf; do
      [ -f "$conf" ] || continue
      if grep -qiE 'spawn\.env|DATABASE_URL' "$conf"; then
        shopt -u nullglob
        echo "units: go-uds mode refused: drop-in $conf references spawn.env or DATABASE_URL" >&2
        exit 2
      fi
    done
    shopt -u nullglob
  done
  if ! grep -qE '^[[:space:]]*LoadCredential=typesafe\.api-key:/run/keep/typesafe\.api-key[[:space:]]*$' "$dir/spawn-node-out.service"; then
    echo "units: go-uds mode refused: spawn-node-out.service missing LoadCredential=typesafe.api-key:/run/keep/typesafe.api-key" >&2
    exit 2
  fi
  if ! grep -qE '^[[:space:]]*RestartPreventExitStatus=243[[:space:]]+78[[:space:]]*$' "$dir/spawn-node-out.service"; then
    echo "units: go-uds mode refused: spawn-node-out.service missing RestartPreventExitStatus=243 78" >&2
    exit 2
  fi
}

[ "$edge_mode" != go-uds ] || check_phase2_units_dir "$src"

# listed <name>: <name> (a unit, or <unit>.d/<file>) is in host-local.
listed() {
  [ -f "$src/host-local" ] &&
    sed -e 's/#.*//' "$src/host-local" | awk 'NF { print $1 }' | grep -qxF -- "$1"
}

same=0
changed=() absent=() linked=() unlisted=() local_ok=()
shopt -s nullglob
for file in "$src"/spawn-*.service "$src"/spawn-*.timer "$src"/spawn-*.path; do
  name=${file##*/}
  dest=$unit_dir/$name
  if [ -L "$dest" ]; then
    linked+=("$name")
  elif [ ! -e "$dest" ]; then
    if [ "$edge_mode" = go-uds ] && is_phase2_unit "$name"; then
      changed+=("$name")
      if [ "$mode" = --apply ]; then
        if ! mkdir -p "$unit_dir"; then
          echo "units: could not create $unit_dir" >&2
          exit 2
        fi
        tmp=$unit_dir/.$name.sync.$$
        if ! { cp "$file" "$tmp" && chmod 644 "$tmp" && mv -f "$tmp" "$dest"; }; then
          rm -f "$tmp"
          echo "units: could not install $dest from $file" >&2
          exit 2
        fi
        say "installed $name"
      else
        say "missing (go-uds): $name (diff -u $dest $file)"
      fi
    else
      absent+=("$name")
    fi
  elif cmp -s "$file" "$dest"; then
    same=$((same + 1))
  else
    changed+=("$name")
    if [ "$mode" = --apply ]; then
      tmp=$unit_dir/.$name.sync.$$
      if ! { cp "$file" "$tmp" && chmod --reference="$dest" "$tmp" && mv -f "$tmp" "$dest"; }; then
        rm -f "$tmp"
        echo "units: could not install $dest from $file" >&2
        exit 2
      fi
      say "installed $name"
    else
      say "differs: $name (diff -u $dest $file)"
    fi
  fi
done
for dest in "$unit_dir"/spawn-*.service "$unit_dir"/spawn-*.timer "$unit_dir"/spawn-*.path; do
  name=${dest##*/}
  [ -e "$src/$name" ] && continue
  if listed "$name"; then local_ok+=("$name"); else unlisted+=("$name"); fi
done
for dest in "$unit_dir"/spawn-*.d/*.conf; do
  name=${dest#"$unit_dir"/}
  if listed "$name"; then local_ok+=("$name"); else unlisted+=("$name"); fi
done
shopt -u nullglob

if [ "${#changed[@]}" -gt 0 ] && [ "$mode" = --apply ]; then
  systemctl --user daemon-reload
  say "daemon-reload"
  for name in "${changed[@]}"; do
    case $name in
    *.timer | *.path)
      systemctl --user try-restart "$name"
      say "try-restarted $name"
      ;;
    spawn-loop.service | spawn-integrate.service | spawn-serve.service) ;;
    spawn-core.service | spawn-node-edge.service | spawn-node-gw.service | spawn-node-out.service)
      [ "$edge_mode" = go-uds ] ||
        say "$name takes effect at its next start (now: systemctl --user try-restart $name)"
      ;;
    *) say "$name takes effect at its next start (now: systemctl --user try-restart $name)" ;;
    esac
  done
fi
verb=differ
[ "$mode" = --check ] || verb=installed
say "$unit_dir against $src: $same the same, ${#changed[@]} $verb"
[ "${#absent[@]}" -eq 0 ] || say "not installed here (the owner installs by hand, deploy/systemd/README.md): ${absent[*]}"
[ "${#linked[@]}" -eq 0 ] || say "symlinks, left alone: ${linked[*]}"
[ "${#local_ok[@]}" -eq 0 ] || say "host-local, listed: ${local_ok[*]}"
[ "${#unlisted[@]}" -eq 0 ] ||
  say "WARNING host-local and not listed in $src/host-local (move it into deploy/systemd, list it with its reason, or remove it): ${unlisted[*]}"
if [ "$mode" = --check ] && { [ "${#changed[@]}" -gt 0 ] || [ "${#unlisted[@]}" -gt 0 ]; }; then
  exit 1
fi
exit 0
