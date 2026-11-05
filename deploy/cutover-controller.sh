#!/usr/bin/env bash
# Once per host: move the live system off the owner's checkouts onto the
# controllers (deploy/controller.sh, deploy/systemd/README.md "The
# controller").
#
#   deploy/cutover-controller.sh --dry-run   # what it would do; changes nothing
#   deploy/cutover-controller.sh
#
# Steps, each one stops the cutover on failure:
#   1. the live Spawn commit (main of missions/spawn.git) must contain this
#      change (deploy/controller.sh): its integrator pins the controller and
#      reads a task's scenario from the mission's directory, and its gate is
#      the one that runs from now on. Deploy it first (deploy/deploy.sh,
#      which also builds and switches the `spawn` controller);
#   2. the `spawn` controller at that commit, with web/node_modules from
#      SPAWN_CUTOVER_WEB_MODULES (default ~/projects/Spawn/web/node_modules,
#      the one the gate used so far), and the `supply-pa` controller at the
#      main of missions/supply-pa.git: each built and switched unless it
#      already is current;
#   3. the units spawn-integrate, spawn-maintenance, spawn-loop and
#      spawn-serve from the controller's deploy/systemd, installed into
#      ~/.config/systemd/user where they differ, then daemon-reload. Only
#      spawn-integrate is restarted (step 5); spawn-loop and spawn-serve take
#      their new working directory at their next restart (a deploy), and
#      read nothing from it;
#   4. missions 4 (Spawn's RSI) and 6 (Supply PA) are read from
#      `spawn mission list`; each must have as gate either its owner checkout
#      path or already the controller's. The scenario and backlog files that
#      differ between the two directories are listed: those are owner edits
#      that were live without a deploy and stop being live now;
#   5. once no gate runs in spawn-integrate's cgroup (it waits, and says so),
#      spawn-integrate stops, each mission still on its checkout path is
#      configured to the controller's with `spawn mission configure`,
#      keeping its accept, minutes, tool calls, resident and workers as read
#      in 4, and spawn-integrate starts again (also when a configure fails).
#      A gate that starts in the instant between the check and the stop is
#      aborted and judged again later, as with any restart.
#
# Safe to run again: what is already done is said and skipped. It prints no
# secrets: spawn.env is only sourced for the `spawn` commands, and only paths,
# commits, mission settings and process ids are printed.
set -euo pipefail

dry=0
case ${1-} in
--dry-run) dry=1 ;;
"") ;;
*)
  echo "usage: deploy/cutover-controller.sh [--dry-run]" >&2
  exit 2
  ;;
esac

live=${SPAWN_LIVE:-$HOME/.local/share/spawn-rsi}
unit_dir=${SPAWN_UNIT_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user}
repo=$(cd "$(dirname "$0")/.." && pwd)
ctl=$repo/deploy/controller.sh
web_modules=${SPAWN_CUTOVER_WEB_MODULES:-$HOME/projects/Spawn/web/node_modules}
spawn_git=$live/missions/spawn.git
supply_git=$live/missions/supply-pa.git
controllers=$live/controllers
units="spawn-integrate.service spawn-maintenance.service spawn-loop.service spawn-serve.service"
# <mission> <controller> <gate under it> <backlog under it> <the owner checkout's gate>
missions="4 spawn rsi/scenarios rsi/backlog $HOME/projects/Spawn/rsi/scenarios
6 supply-pa gate/scenarios gate/backlog $HOME/projects/supply-pa/gate/scenarios"

say() { printf 'cutover: %s\n' "$*"; }
would() { printf 'cutover: [dry run] would %s\n' "$*"; }
die() {
  printf 'cutover: %s\n' "$*" >&2
  exit 1
}
blocked=0
# block <reason>: a real run stops here; a dry run says so and goes on, so
# the whole plan is shown.
block() {
  if [ "$dry" = 1 ]; then
    printf 'cutover: [dry run] a real run would STOP here: %s\n' "$*"
    blocked=1
  else
    die "$*"
  fi
}
# spawn_cli <args>: the live binary with the live settings; the environment
# file is sourced in this subshell only, never printed.
spawn_cli() (
  set -a
  # shellcheck disable=SC1091
  . "$live/spawn.env"
  set +a
  exec "$live/bin/spawn" "$@"
)

for need in "$live/spawn.env" "$live/bin/spawn" "$spawn_git" "$supply_git" "$ctl"; do
  [ -e "$need" ] || die "no $need"
done
command -v jq >/dev/null || die "jq is required"
[ "$dry" = 1 ] && say "dry run: nothing is built, installed, configured or restarted"

# 1. The live commits.
spawn_main=$(git --git-dir="$spawn_git" rev-parse main)
supply_main=$(git --git-dir="$supply_git" rev-parse main)
say "live Spawn main: $spawn_main"
say "live Supply PA main: $supply_main"
if ! git --git-dir="$spawn_git" cat-file -e "$spawn_main:deploy/controller.sh" 2>/dev/null; then
  block "the live Spawn commit $spawn_main predates the controller: gate and deploy this change (deploy/deploy.sh) first, then run the cutover"
fi

# 2. The controllers.
# controller <name> <git-dir> <commit> [<node_modules>]
controller() {
  local name=$1 git_dir=$2 commit=$3 modules=${4-}
  if [ -f "$controllers/$name/.controller" ] && [ "$(cat "$controllers/$name/.controller")" = "$commit" ]; then
    say "controller $name is already $commit"
    return
  fi
  if [ "$dry" = 1 ]; then
    if [ -n "$modules" ]; then
      if [ ! -d "$modules" ]; then
        block "no node_modules at $modules (SPAWN_CUTOVER_WEB_MODULES)"
      elif ! git --git-dir="$git_dir" show "$commit:web/pnpm-lock.yaml" | cmp -s - "$modules/.pnpm/lock.yaml"; then
        block "$modules was installed from another lockfile than $commit's"
      else
        say "$modules was installed from $commit's web/pnpm-lock.yaml"
      fi
    fi
    would "build $controllers/$name-$commit (git archive of $commit from $git_dir${modules:+, web/node_modules reflinked from $modules}) and point $controllers/$name at it"
  else
    bash "$ctl" update "$name" "$git_dir" "$commit" ${modules:+"$modules"} >/dev/null
  fi
}
controller spawn "$spawn_git" "$spawn_main" "$web_modules"
controller supply-pa "$supply_git" "$supply_main"
# What the missions and units will name must exist in those commits.
for check in "$spawn_git $spawn_main rsi/scenarios" "$spawn_git $spawn_main rsi/backlog" \
  "$spawn_git $spawn_main scripts/rsi/gate.sh" "$spawn_git $spawn_main scripts/maintenance.sh" \
  "$supply_git $supply_main gate/scenarios" "$supply_git $supply_main gate/backlog"; do
  set -- $check
  git --git-dir="$1" cat-file -e "$2:$3" 2>/dev/null || die "$3 is not in $2 ($1)"
done
say "the controllers' commits hold rsi/scenarios, rsi/backlog, scripts/rsi/gate.sh, scripts/maintenance.sh (Spawn) and gate/scenarios, gate/backlog (Supply PA)"

# 3. The units: the controller's (the deployed commit's), or in a dry run
# before the deploy, this checkout's.
unit_source() {
  if [ -f "$controllers/spawn/.controller" ] && [ "$(cat "$controllers/spawn/.controller")" = "$spawn_main" ] &&
    [ -f "$controllers/spawn/deploy/systemd/$1" ]; then
    cat "$controllers/spawn/deploy/systemd/$1"
  elif git --git-dir="$spawn_git" cat-file -e "$spawn_main:deploy/controller.sh" 2>/dev/null; then
    git --git-dir="$spawn_git" show "$spawn_main:deploy/systemd/$1"
  else
    cat "$repo/deploy/systemd/$1"
  fi
}
git --git-dir="$spawn_git" cat-file -e "$spawn_main:deploy/controller.sh" 2>/dev/null ||
  say "units below are this checkout's ($repo), the live commit has none for the controller"
reload=0
for unit in $units; do
  new=$(unit_source "$unit")
  if printf '%s\n' "$new" | grep -q 'projects/Spawn'; then
    die "the new $unit still names projects/Spawn"
  fi
  if [ -f "$unit_dir/$unit" ] && [ "$(cat "$unit_dir/$unit")" = "$new" ]; then
    say "$unit is installed as it should be"
    continue
  fi
  reload=1
  if [ "$dry" = 1 ]; then
    would "install $unit_dir/$unit:"
    diff -u --label "installed $unit" --label "new $unit" "$unit_dir/$unit" <(printf '%s\n' "$new") | sed 's/^/    /' || true
  else
    printf '%s\n' "$new" >"$unit_dir/.$unit.new"
    chmod 644 "$unit_dir/.$unit.new"
    mv "$unit_dir/.$unit.new" "$unit_dir/$unit"
    say "installed $unit"
  fi
done
if [ "$reload" = 1 ]; then
  if [ "$dry" = 1 ]; then would "systemctl --user daemon-reload"; else
    systemctl --user daemon-reload
    say "daemon-reload"
  fi
fi

# 4. The missions, as they are.
# local_ids <dir> / commit_ids <git-dir> <commit> <path>: "<blob id> <file>"
# per file, to compare a checkout's directory with a commit's.
local_ids() {
  (cd "$1" && find . -type f -printf '%P\n' | LC_ALL=C sort | while IFS= read -r file; do
    printf '%s %s\n' "$(git hash-object --no-filters -- "$file")" "$file"
  done)
}
commit_ids() {
  git --git-dir="$1" ls-tree -r "$2:$3" | awk -F'\t' '{ split($1, f, " "); print f[3] " " $2 }' | LC_ALL=C sort -k2
}
board=$(spawn_cli mission list --limit 1000)
configure=() # one line per mission to configure: its arguments, tab-separated
while read -r id name gate_sub backlog_sub old_gate; do
  [ -n "$id" ] || continue
  new_gate=$controllers/$name/$gate_sub
  new_backlog=$controllers/$name/$backlog_sub
  mission=$(printf '%s' "$board" | jq -c --argjson id "$id" '.missions[] | select(.id == $id)')
  [ -n "$mission" ] || die "mission $id is not in spawn mission list"
  state=$(jq -r '.state' <<<"$mission")
  gate=$(jq -r '.gate // ""' <<<"$mission")
  backlog=$(jq -r '.objectives.backlog // ""' <<<"$mission")
  accept=$(jq -r '.accept' <<<"$mission")
  say "mission $id ($state): gate $gate, backlog ${backlog:-none}, accept $accept, budget $(jq -c '.budget' <<<"$mission"), objectives $(jq -c '.objectives | if . then del(.backlog) else . end' <<<"$mission")"
  case $state in open | active) ;; *) die "mission $id is $state: only an open or active mission is configured" ;; esac
  if [ "$gate" = "$new_gate" ] && { [ -z "$backlog" ] || [ "$backlog" = "$new_backlog" ]; }; then
    say "mission $id already reads the controller"
    continue
  fi
  [ "$gate" = "$old_gate" ] || die "mission $id's gate is $gate, neither $old_gate nor $new_gate: look at it before running this"
  # Only the settings this script knows how to keep: anything else stops it.
  jq -e '(.budget // {} | keys - ["minutes", "tool_calls"] | length == 0) and
         (.objectives // {} | keys - ["backlog", "resident", "workers"] | length == 0)' <<<"$mission" >/dev/null ||
    die "mission $id has settings this script would not keep: $(jq -c '{budget, objectives}' <<<"$mission")"
  args=(--id "$id" --gate "$new_gate" --accept "$accept")
  minutes=$(jq -r '.budget.minutes // empty' <<<"$mission")
  calls=$(jq -r '.budget.tool_calls // empty' <<<"$mission")
  [ -z "$minutes" ] || args+=(--minutes "$minutes")
  [ -z "$calls" ] || args+=(--tool-calls "$calls")
  if [ -n "$backlog" ]; then
    resident=$(jq -r '.objectives.resident' <<<"$mission")
    workers=$(jq -r '.objectives.workers // empty' <<<"$mission")
    args+=(--backlog "$new_backlog" --resident "$resident")
    [ -z "$workers" ] || args+=(--workers "$workers")
  fi
  # What was live without a deploy and stops being live: files that differ
  # between the checkout's directories and the controller's commit.
  commit=$([ "$name" = spawn ] && echo "$spawn_main" || echo "$supply_main")
  git_dir=$([ "$name" = spawn ] && echo "$spawn_git" || echo "$supply_git")
  for pair in "$old_gate:$gate_sub" "${backlog:+$backlog:$backlog_sub}"; do
    [ -n "$pair" ] || continue
    local_dir=${pair%%:*} sub=${pair#*:}
    drift=$(diff <(local_ids "$local_dir") <(commit_ids "$git_dir" "$commit" "$sub") |
      awk '/^[<>]/ { print $3 }' | LC_ALL=C sort -u | tr '\n' ' ') || true
    if [ -n "$drift" ]; then
      say "mission $id: $local_dir differs from $commit:$sub in: $drift(live until now without a deploy; not after the cutover)"
    else
      say "mission $id: $local_dir is the same as $commit:$sub"
    fi
  done
  configure+=("$(printf '%s\t' "${args[@]}")")
done <<<"$missions"

# 5. The restart.
if [ "${#configure[@]}" -eq 0 ] && [ "$reload" = 0 ]; then
  say "nothing to configure and no unit changed: spawn-integrate is not restarted"
  [ "$blocked" = 0 ] || exit 1
  exit 0
fi
# gates: the process ids of gates running in spawn-integrate's cgroup.
gates() {
  local cgroup pid
  cgroup=$(systemctl --user show -p ControlGroup --value spawn-integrate 2>/dev/null) || true
  if [ -n "$cgroup" ] && [ -d "/sys/fs/cgroup$cgroup" ]; then
    find "/sys/fs/cgroup$cgroup" -name cgroup.procs -exec cat {} + 2>/dev/null | while read -r pid; do
      if tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -q 'scripts/rsi/gate\.sh'; then echo "$pid"; fi
    done
  else
    pgrep -u "$(id -u)" -f 'scripts/rsi/gate\.sh' || true
  fi
}
running=$(gates | tr '\n' ' ')
if [ "$dry" = 1 ]; then
  if [ -n "$running" ]; then
    say "a gate runs now in spawn-integrate (pid $running): a real run would wait for it"
  else
    say "no gate runs now in spawn-integrate"
  fi
  would "wait until no gate runs in spawn-integrate, then systemctl --user stop spawn-integrate"
  for line in "${configure[@]}"; do
    IFS=$'\t' read -r -a args <<<"$line"
    would "spawn mission configure ${args[*]}"
  done
  would "systemctl --user start spawn-integrate"
  [ "$blocked" = 0 ] || {
    say "dry run done; a real run would stop at the step marked above"
    exit 1
  }
  say "dry run done"
  exit 0
fi

waited=0
while running=$(gates | tr '\n' ' ') && [ -n "${running// /}" ]; do
  [ $((waited % 60)) -eq 0 ] && say "waiting: a gate runs in spawn-integrate (pid $running); waited ${waited}s"
  sleep 5
  waited=$((waited + 5))
done
say "no gate runs in spawn-integrate (waited ${waited}s); stopping it"
started=0
on_exit() {
  [ "$started" = 1 ] && return
  systemctl --user start spawn-integrate || printf 'cutover: starting spawn-integrate failed: systemctl --user status spawn-integrate\n' >&2
}
trap on_exit EXIT
systemctl --user stop spawn-integrate
for line in "${configure[@]}"; do
  IFS=$'\t' read -r -a args <<<"$line"
  spawn_cli mission configure "${args[@]}" >/dev/null
  say "configured: spawn mission configure ${args[*]}"
done
systemctl --user start spawn-integrate
started=1
say "spawn-integrate started: $(systemctl --user is-active spawn-integrate)"
say "done: the live system reads $controllers; an owner edit is live only after its deploy"
