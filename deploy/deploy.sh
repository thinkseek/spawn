#!/usr/bin/env bash
# Deploy the checked-out commit to the live fleet on this host.
#
#   deploy/deploy.sh <verdict-file>            # build, install, move missions main, restart
#   deploy/deploy.sh --landed <outcome-file>   # build, install, restart: main already moved
#
# The live system runs only what a deploy installed: the binary and UI in
# $live, and the controller ($live/controllers/spawn, deploy/controller.sh),
# the deployed commit's tree, from which the integrator runs the gate, its
# scenarios and the objective backlog, and the maintenance timer its script.
# Nothing live reads the owner's checkout: an owner edit (to gate.sh, a
# scenario, an objective in rsi/backlog) is live only after its deploy.
#
# <verdict-file> is what the gate wrote for this exact HEAD, judged against the
# missions repo's current main. Make it from the checkout being deployed:
#
#   main=$(git --git-dir="$HOME/.local/share/spawn-rsi/missions/spawn.git" rev-parse main)
#   git fetch -q "$HOME/.local/share/spawn-rsi/missions/spawn.git" main
#   SPAWN_GATE_VERDICT=$PWD/verdict.json sh scripts/rsi/gate.sh \
#     "$(git rev-parse --absolute-git-dir)" "$main" HEAD <the RSI mission's scenarios dir>
#
# The deploy refuses unless the verdict is "accepted", its "candidate" is HEAD
# and its "base" is the missions main it replaces. Without that, anything
# that could add a commit to this checkout (on a host without containers,
# every agent runs as the owner's uid) would be built, installed and pushed to
# main ungated, and `spawn mission moved` would make it look gated. The
# verdict file is only as trustworthy as the gate run that wrote it: a
# same-uid process could forge one too, so run the gate yourself, read its
# output, and keep this checkout where agents cannot write.
#
# --landed <outcome-file>: HEAD already landed on the missions main by the
# owner's gated push (`spawn mission push --gated --wait`, whose output is the
# file; design-owner-push-between-gates.md): the integrator judged it between
# gates and moved main itself. The deploy refuses unless the outcome is
# applied, a `source.moved` with verdict accepted, on this missions repo, to
# HEAD, and the missions main is still HEAD; it then pushes nothing and
# records no move (the integrator's source.moved did), and builds, installs
# and restarts as below. The file is trusted as a verdict file is: written by
# the owner's own `spawn mission push`, in a checkout agents cannot write.
#
# Steps, each one stops the deploy on failure:
#   1. no other deploy runs (flock on $live/deploy.lock), and there is room:
#      $TMPDIR and the target directory each have at least
#      SPAWN_DEPLOY_MIN_FREE_GB (default 10) free;
#   2. a deploy left pending by a deploy that died after its push (see 6) is
#      completed, with the integrator stopped;
#   3. the verdict is accepted for HEAD over the current missions main, the
#      tree is clean, HEAD descends from that main (a deploy never drops
#      landed work), and HEAD tracks nothing under a node_modules/ (the UI
#      build runs the tools there);
#   4. a pg_dump of the live database into the backups directory (the
#      password reaches pg_dump through PGPASSWORD, never argv);
#   5. HEAD is exported (`git archive`, so nothing untracked or ignored in
#      the checkout is built) to $TMPDIR/spawn-deploy-export-<uid>, with the
#      checkout's web/node_modules linked in; the controller for HEAD is
#      built beside the current one ($live/controllers/spawn-<head>, its
#      web/node_modules a reflink copy of the checkout's, which must be
#      installed from HEAD's lockfile); a release build without the
#      compiler cache into the persistent SPAWN_DEPLOY_TARGET (default
#      ~/.cache/spawn/deploy-target) and the UI build (only
#      web/vite.config.ts, named with --config; postcss configured inline)
#      run there, both in the bubblewrap sandbox of scripts/sandbox.sh (no
#      network, $HOME hidden, writable only the export and the target
#      directory): proc-macros and the UI's build tools run the candidate's
#      code, not the owner's. Binary and UI are staged in $live (spawn.staged,
#      web.staged), not installed; the export and anything still staged are
#      removed on every exit;
#   6. the integrator is stopped, main is read again (it must still be the
#      verdict's base), the staged binary migrates the schema, and only then
#      binary and UI are installed: the replaced copies become spawn.prev and
#      web.prev, unless the live copy already is this build (a deploy run
#      again keeps its rollback copies), and the controller switches to
#      HEAD's (one atomic rename; the one it replaces stays as
#      controllers/spawn.prev, older ones are removed unless a process
#      still uses them). Then $live/deploy.pending is written
#      ("<main it replaces> <head>"), main moves forward (never forced),
#      every gated mission whose baseline was the old main takes the new one
#      (so the deploy is not mistaken for a push outside the gate), and
#      deploy.pending is removed;
#   7. the services restart; the base cache is warmed for the new main in
#      the background (see warm_base_cache); then each relay whose script
#      changed gets it and a try-restart; a relay unit that is not installed
#      is skipped, any other relay failure is reported after the deploy is
#      live.
# Any failure after the integrator stopped completes a pending deploy (when
# main is its head) and restarts the services before the deploy exits (with
# whatever binary is installed by then: the previous one, unless the failure
# came after the install). A deploy killed outright leaves deploy.pending;
# the next deploy completes it, or stops and says so when main is neither
# the main it replaced nor its head.
#
# The owner's hold. Before the owner gate, deploy/hold.sh holds landings on
# the missions repo (`spawn mission hold`), so no task lands while the gate
# judges against a fixed main and the verdict's base stays the live main;
# spawn-integrate keeps running (every gate, every other source, objective
# assignment). Once it holds the deploy lock, this script releases that hold
# (`spawn mission release`) on every exit path: after a success, once main
# moved forward and `spawn mission moved` recorded it (the tasks that waited
# land at the next pass, judged again on the new main), and after any
# failure (after a pending deploy is completed and the services restarted).
# A release with no hold records nothing, and a release that fails is said,
# never the deploy's failure. A deploy refused by the lock leaves the hold to
# the deploy that holds the lock. A hold left by a deploy killed outright
# expires on its own (SPAWN_SOURCE_HOLD_SECS, 3 h by default).
#
# Rolling back. spawn.prev is the previous binary. Once the deployed binary
# has written an event kind the previous one does not know (EventKind is a
# closed enum: docs/upgrading.md, "Version-coupled boundaries"), the previous
# binary fails to decode the log and halts on it, so swapping spawn.prev back
# is safe only before any such event exists. After that, roll forward with a
# fix instead.
set -euo pipefail

PROG=deploy
live=${SPAWN_LIVE:-$HOME/.local/share/spawn-rsi}
missions=$live/missions/spawn.git
pending=$live/deploy.pending
target=${SPAWN_DEPLOY_TARGET:-${XDG_CACHE_HOME:-$HOME/.cache}/spawn/deploy-target}
min_free_gb=${SPAWN_DEPLOY_MIN_FREE_GB:-10}
repo=$(git rev-parse --show-toplevel)
cd "$repo"
# shellcheck source=deploy/backup/lib.sh
. "$repo/deploy/backup/lib.sh"
# shellcheck source=scripts/sandbox.sh
. "$repo/scripts/sandbox.sh"

say() { printf 'deploy: %s\n' "$*"; }

# One deploy at a time: two would share the export directory, the pending
# record and the services. The lock is released when this process exits.
[ -d "$live" ] || die "no live directory $live"
exec 9>>"$live/deploy.lock"
flock -n 9 || die "another deploy holds $live/deploy.lock"

# release_hold: end the owner's hold on the missions repo, if one is active
# (deploy/hold.sh). Never fails: a release that cannot run is said, and the
# hold then expires on its own. Its own environment, so it runs on any exit.
release_hold() {
  local released hold
  if released=$(set -a && . "$live/spawn.env" && set +a &&
    "$live/bin/spawn" mission release --source "$missions" 2>&1); then
    hold=$(printf '%s' "$released" | jq -r '.released // empty' 2>/dev/null) || hold=""
    if [ -n "$hold" ]; then
      say "released the owner's hold $hold on $missions (tasks waiting behind it: $(printf '%s' "$released" | jq -c '.waiting' 2>/dev/null || echo '?'))"
    fi
  else
    printf 'deploy: could not release the owner'\''s hold on %s (%s); it expires on its own, or: spawn mission release --source %s\n' \
      "$missions" "$(printf '%s' "$released" | head -c 300 | tr '\n' ' ')" "$missions" >&2
  fi
  return 0
}

# Every exit from here on: the export goes; after a failure with the
# integrator stopped, a pending deploy is completed and the services
# restart; then the owner's hold on the missions repo is released.
export_dir=${TMPDIR:-/tmp}/spawn-deploy-export-$(id -u)
stopped=0
completing_at_start=0
on_exit() {
  local status=$?
  rm -rf "$export_dir" "$live/bin/spawn.staged" "$live/web.staged"
  if [ "$status" -ne 0 ] && [ "$stopped" = 1 ]; then
    printf "deploy: failed (exit %s); restarting the services\n" "$status" >&2
    [ "$completing_at_start" = 1 ] || (complete_pending) || printf "deploy: the pending deploy is not complete: %s\n" "$pending" >&2
    restart || printf "deploy: the restart failed too: systemctl --user status spawn-integrate\n" >&2
  fi
  release_hold
}
trap on_exit EXIT

landed=""
[ "${1-}" = --landed ] && { landed=1; shift; }
[ $# -eq 1 ] || die "usage: deploy/deploy.sh <verdict-file> | --landed <outcome-file> (see the header)"
verdict_file=$1
[ -f "$verdict_file" ] || die "no ${landed:+outcome}${landed:-verdict} file $verdict_file"
field() { jq -er --arg name "$1" '.[$name] // empty | strings' "$verdict_file"; }
# outcome_field <name>: a field of the outcome record of a --landed file.
outcome_field() { jq -er --arg name "$1" '.outcome[$name] // empty | strings' "$verdict_file"; }

# The disk guard: a build that fills the disk takes the live fleet's
# database and logs down with it.
mkdir -p "$target"
for dir in "${TMPDIR:-/tmp}" "$target"; do
  free=$(df -P -BG "$dir" | awk 'NR == 2 { sub("G", "", $4); print $4 }')
  [ "$free" -ge "$min_free_gb" ] ||
    die "$dir has ${free} GB free, the deploy needs $min_free_gb (SPAWN_DEPLOY_MIN_FREE_GB): free some space first"
done

set -a
# shellcheck disable=SC1091
. "$live/spawn.env"
set +a

restart() { systemctl --user restart spawn-loop spawn-integrate spawn-serve; }

# complete_pending: finish a deploy that pushed main but did not record
# `spawn mission moved` (it died in between). Run with the integrator
# stopped. $pending holds "<main it replaced> <deployed head>".
complete_pending() {
  [ -f "$pending" ] || return 0
  local from to main moved
  read -r from to <"$pending" || true
  [[ $from =~ ^[0-9a-f]{40}$ && $to =~ ^[0-9a-f]{40}$ ]] ||
    die "$pending does not hold two commits; read it and remove it by hand"
  main=$(git --git-dir="$missions" rev-parse main)
  if [ "$main" = "$to" ]; then
    moved=$("$live/bin/spawn" mission moved --source "$missions" --from "$from" --to "$to") ||
      die "spawn mission moved --from $from --to $to failed; $pending stays for the next deploy"
    rm -f "$pending"
    say "completed the pending deploy: main $from -> $to; $(printf '%s' "$moved" | jq -r '.missions') mission(s) took it as baseline"
  elif [ "$main" = "$from" ]; then
    # It died before its push landed: nothing to record.
    rm -f "$pending"
    say "the pending deploy of $to never moved main (still $from); dropped it"
  else
    die "a deploy of $from -> $to is pending, but the missions main is $main; record it by hand (spawn mission moved --source $missions --from <old> --to <new>) and remove $pending"
  fi
}

# install_build: the staged binary and UI become the live ones. The copies
# they replace become spawn.prev and web.prev, unless the live copy already
# is this build (a deploy run again): then the rollback copies stay.
install_build() {
  if cmp -s "$live/bin/spawn.staged" "$live/bin/spawn"; then
    rm -f "$live/bin/spawn.staged"
  else
    cp "$live/bin/spawn" "$live/bin/spawn.prev"
    mv "$live/bin/spawn.staged" "$live/bin/spawn"
  fi
  if [ -d "$live/web" ] && diff -rq "$live/web.staged" "$live/web" >/dev/null; then
    rm -rf "$live/web.staged"
  else
    rm -rf "$live/web.prev"
    [ ! -d "$live/web" ] || mv "$live/web" "$live/web.prev"
    mv "$live/web.staged" "$live/web"
  fi
  say "installed $("$live/bin/spawn" version 2>/dev/null || echo "$head") and the UI"
}

if [ -f "$pending" ]; then
  say "a deploy is pending ($(cat "$pending")); completing it"
  stopped=1 completing_at_start=1
  systemctl --user stop spawn-integrate
  complete_pending
  systemctl --user start spawn-integrate
  stopped=0 completing_at_start=0
fi

[ -z "$(git status --porcelain --untracked-files=no)" ] || die "the tree has uncommitted changes"
head=$(git rev-parse HEAD)
old=$(git --git-dir="$missions" rev-parse main)
if [ -n "$landed" ]; then
  # The owner's gated push landed HEAD (see the header): main moved already.
  state=$(field state) || die "$verdict_file holds no push state"
  [ "$state" = applied ] || die "the owner's push is $state, not applied"
  [ "$(outcome_field type)" = source.moved ] || die "$verdict_file's outcome is no source.moved"
  [ "$(outcome_field verdict)" = accepted ] ||
    die "the owner's push landed without a gate's acceptance (verdict $(outcome_field verdict || echo none)): deploy with a verdict file"
  moved_source=$(outcome_field source) || die "$verdict_file's outcome names no source"
  [ "$(readlink -f "$moved_source")" = "$(readlink -f "$missions")" ] ||
    die "the owner's push landed on $moved_source, not on $missions"
  landed_to=$(outcome_field to) || die "$verdict_file's outcome names no commit"
  [ "$landed_to" = "$head" ] || die "the owner's push landed $landed_to, HEAD is $head: check it out (git merge --ff-only $landed_to)"
  [ "$old" = "$head" ] ||
    die "the missions main is $old, not the landed $head: main moved since the landing; push and deploy again"
  say "HEAD $head landed by the owner's gated push (request $(jq -r '.request // "?"' "$verdict_file")): no push, no move to record"
else
  verdict=$(field verdict) || die "$verdict_file holds no verdict"
  candidate=$(field candidate) || die "$verdict_file names no candidate: run the gate again"
  base=$(field base) || die "$verdict_file names no base: run the gate again"
  [ "$verdict" = accepted ] || die "the gate's verdict is $verdict, not accepted"
  [ "$candidate" = "$head" ] || die "the verdict is for $candidate, HEAD is $head"
  [ "$base" = "$old" ] || die "the verdict judged against $base, the missions main is $old: run the gate again"
fi
git cat-file -e "$old^{commit}" 2>/dev/null || git fetch -q "$missions" main
git merge-base --is-ancestor "$old" "$head" ||
  die "HEAD does not contain the missions main $old: merge it first"
# The UI build runs the tools in the checkout's web/node_modules. A commit
# that tracks files under a node_modules/ would put its own in their place
# (git overwrites ignored files on checkout), so such a commit is refused.
tracked_modules=$(git ls-tree -r --name-only "$head" | grep -E '(^|/)node_modules/' | head -n 3) || true
[ -z "$tracked_modules" ] ||
  die "HEAD tracks files under node_modules/ ($(printf '%s' "$tracked_modules" | tr '\n' ' ')...): remove them from the commit"

dump=$live/backups/spawn-$(date +%Y%m%d-%H%M%S).dump
with_db "$SPAWN_DATABASE_URL" pg_dump -Fc -f "$dump"
say "backup $dump ($(stat -c %s "$dump") bytes)"

# The build: an export of HEAD (untracked and ignored files in the checkout
# stay out), with fresh mtimes so cargo never takes an older build in the
# persistent target directory for this commit's, in the sandbox.
rm -rf "$export_dir"
mkdir -m 700 "$export_dir"
git archive "$head" | tar -x -m -C "$export_dir"
node_modules=$(readlink -f "$repo/web/node_modules") ||
  die "no web/node_modules in $repo: install the UI's dependencies"
[ -d "$node_modules" ] || die "no web/node_modules in $repo: install the UI's dependencies"
ln -s "$node_modules" "$export_dir/web/node_modules"
# The controller for HEAD (deploy/controller.sh): built now, beside the
# current one, and made current with the install below. It refuses a
# node_modules installed from another lockfile than HEAD's.
bash "$repo/deploy/controller.sh" build spawn "$(git rev-parse --absolute-git-dir)" "$head" "$node_modules" >/dev/null
sandbox_rw "$export_dir" "$target"
sandbox_ro "$node_modules"
sandbox_env CARGO_TARGET_DIR "$target"
sandbox_run "$export_dir" cargo build --release -q
# The UI is built from the same commit and served from the live directory,
# so edits in the checkout never reach the served UI before a deploy. Vite
# loads its config without writing it into node_modules (read-only here),
# and only the protected vite.config.ts: named explicitly, so a
# vite.config.js or .mjs in the tree is never looked for (its postcss
# config is inline for the same reason).
sandbox_run "$export_dir/web" sh -c './node_modules/.bin/tsc --noEmit &&
  ./node_modules/.bin/vite build --config vite.config.ts --configLoader runner --logLevel warn --outDir "$1" --emptyOutDir' \
  sh "$export_dir/web-dist"
# Staged, not installed: the live binary and UI change only once every
# step before the push has passed (init-db runs the staged binary).
cp "$target/release/spawn" "$live/bin/spawn.staged"
rm -rf "$live/web.staged"
cp -r "$export_dir/web-dist" "$live/web.staged"

# From here on, a failure must not leave the integrator stopped (on_exit).
stopped=1
systemctl --user stop spawn-integrate
# The integrator may have landed between the check above and its stop.
[ "$(git --git-dir="$missions" rev-parse main)" = "$old" ] ||
  die "the missions main moved during the deploy: run the gate on the new main and deploy again"
"$live/bin/spawn.staged" init-db >/dev/null
install_build
# What the integrator (restarted below) and the maintenance timer read: the
# gate, the scenarios, the backlog. Switched with the integrator stopped, so
# no gate of this host runs across it.
bash "$repo/deploy/controller.sh" switch spawn "$head"
if [ "$old" != "$head" ]; then
  # Written before the push, removed once the missions took the new main:
  # a deploy that dies in between is completed by on_exit or the next deploy.
  printf '%s %s\n' "$old" "$head" >"$pending.tmp"
  mv "$pending.tmp" "$pending"
  git push -q "$missions" "$head:refs/heads/main"
  moved=$("$live/bin/spawn" mission moved --source "$missions" --from "$old" --to "$head")
  rm -f "$pending"
  say "main $old -> $head; $(printf '%s' "$moved" | jq -r '.missions') mission(s) took it as baseline"
fi
restart
stopped=0
say "restarted; live at $head"

# warm_base_cache: the gate's base phase for the new main, run now in the
# background (scripts/rsi/gate.sh --base-only), so the next gates judged
# against it (the integrator's, the owner's) take their base results from
# the cache instead of 30-60 minutes of base runs. The same key those gates
# compute: the controller's gate and scenarios, and the integrator's own
# gate environment (the variables gate.rs passes the gate), from the file
# the running integrator writes at its start ($SPAWN_GATE_ENV_FILE in its
# unit, ~/.config/spawn/gate-env, 0600, /proc/<pid>/environ's format with
# its pid first; the integrator is not dumpable, so its environ is not
# readable), and never from a command line, as its database URL must not
# be. A file whose pid is not the integrator's running MainPID (an older
# integrator's, or one not written yet after the restart above) is waited
# for up to 30 s, then the cache is not warmed. With a server of the gate's own
# (SPAWN_GATE_PG_IMAGE or SPAWN_GATE_PG_BIN) it does not get the shared
# SPAWN_TEST_DATABASE_URL; without one it gets it only when its server is
# not the owner's (by text; the integrator also asks the servers), and its
# base runs share that server with whatever else uses it, the integrator's
# next gate included. It never delays or fails the
# deploy: it runs in a session of its own, with the deploy's lock closed,
# and anything that keeps it from starting is said and skipped. A gate that
# starts meanwhile waits on the per-key lock for what it is running, and
# judges the rest itself; results are written whole or not at all. The log
# is $live/gate-warm/<head>.log (logs older than a week go).
warm_base_cache() {
  local pid controller log assignment name keep=() cached host_jobs="" slots_dir
  controller=$(readlink -f "$live/controllers/spawn") && [ -f "$controller/scripts/rsi/gate.sh" ] || {
    say "base cache: no controller at $live/controllers/spawn; not warmed"
    return 0
  }
  pid=$(systemctl --user show -p MainPID --value spawn-integrate 2>/dev/null) || pid=""
  if ! [[ $pid =~ ^[1-9][0-9]*$ ]]; then
    say "base cache: spawn-integrate is not running; not warmed"
    return 0
  fi
  local env_file=${SPAWN_GATE_ENV_FILE:-$HOME/.config/spawn/gate-env} first="" waited=0
  while :; do
    first=""
    [ -r "$env_file" ] && IFS= read -r -d '' first <"$env_file" || true
    [ "$first" != "SPAWN_INTEGRATE_PID=$pid" ] || break
    if [ "$waited" -ge 30 ]; then
      say "base cache: $env_file is not the running integrator's (pid $pid; is SPAWN_GATE_ENV_FILE in its unit?); not warmed"
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done
  # shellcheck source=scripts/pg-url.sh
  . "$controller/scripts/pg-url.sh"
  local own="" test_url="" home=$HOME
  # shellcheck source=deploy/gate-env.sh
  . "$repo/deploy/gate-env.sh"
  gate_env_read "$env_file"
  if [ -z "$own" ] && [ -n "$test_url" ]; then
    local owner_server="" test_server=""
    pg_url_parse "${SPAWN_DATABASE_URL-}" && owner_server="$pg_url_host:$pg_url_port"
    pg_url_parse "$test_url" && test_server="$pg_url_host:$pg_url_port"
    if [ -n "$test_server" ] && [ "$test_server" != "$owner_server" ]; then
      keep+=("SPAWN_TEST_DATABASE_URL=$test_url")
    fi
  fi
  # The host's scenario budget, stated once for every gate on this host: the
  # integrator's own value, when its gate environment names one. The gates
  # only read $home/.cache/spawn/gate-slots/host_jobs (scripts/rsi/gate.sh,
  # "Host scenario slots"), so the number no longer depends on what
  # SPAWN_GATE_JOBS happens to be for a hand-run gate, and a gate started
  # with a larger value cannot exceed it. No value in the environment: the
  # file is left as it is, never removed (the owner may have set one).
  for assignment in "${keep[@]}"; do
    case $assignment in SPAWN_GATE_HOST_JOBS=*) host_jobs=${assignment#*=} ;; esac
  done
  if [[ $host_jobs =~ ^[1-9][0-9]*$ ]]; then
    slots_dir=$home/.cache/spawn/gate-slots
    if mkdir -p "$slots_dir" && chmod 700 "$slots_dir" 2>/dev/null &&
      printf '%s\n' "$host_jobs" >"$slots_dir/host_jobs.new" &&
      mv -f "$slots_dir/host_jobs.new" "$slots_dir/host_jobs"; then
      say "host scenario slots: $host_jobs (stated in $slots_dir/host_jobs)"
    else
      say "host scenario slots: could not write $slots_dir/host_jobs; gates fall back to SPAWN_GATE_HOST_JOBS"
    fi
  fi
  mkdir -p "$live/gate-warm"
  find "$live/gate-warm" -maxdepth 1 -name '*.log' -type f -mtime +7 -delete 2>/dev/null || true
  log=$live/gate-warm/$head.log
  (
    exec 8>&- 9>&- </dev/null >>"$log" 2>&1
    # Nor anything else the deploy's caller left open (a pipe it reads to
    # its end would wait for the warm-up). 255 is bash's own, close-on-exec.
    local fd
    for fd in /proc/"$BASHPID"/fd/*; do
      fd=${fd##*/}
      case $fd in
      0 | 1 | 2 | 255) ;;
      *) eval "exec $fd>&-" 2>/dev/null || true ;;
      esac
    done
    # The integrator's gate environment, and nothing else of the deploy's.
    for name in $(compgen -e); do unset "$name" 2>/dev/null || true; done
    for assignment in "${keep[@]}"; do export "$assignment"; done
    echo "deploy: base-only gate of $head, started $(date -Is)"
    exec setsid sh "$controller/scripts/rsi/gate.sh" --base-only "$missions" "$head" "$controller/rsi/scenarios"
  ) &
  cached=${home}/.cache/spawn/gate-base
  say "base cache: warming it for $head in the background (pid $!), with ${own:-the shared test server}: log $log, results in $cached"
}
warm_base_cache || say "base cache: not warmed (above)"
# The relays run from the live bin directory too; restart one only when its
# script changed, so a deploy does not drop engines' model calls needlessly.
# Last, so a relay cannot stop the deploy halfway. A relay whose unit is not
# installed (systemctl exit 5) gets its script and nothing else.
relays_failed=0
for relay in jev-relay model-relay; do
  cmp -s "$export_dir/scripts/$relay.py" "$live/bin/$relay.py" && continue
  install -m 755 "$export_dir/scripts/$relay.py" "$live/bin/$relay.py"
  relay_status=0
  systemctl --user try-restart "spawn-$relay" || relay_status=$?
  case $relay_status in
  0) say "installed $relay.py" ;;
  5) say "installed $relay.py (unit spawn-$relay is not installed: nothing restarted)" ;;
  *)
    printf 'deploy: installed %s, but restarting spawn-%s failed (exit %s): systemctl --user status spawn-%s\n' \
      "$relay" "$relay" "$relay_status" "$relay" >&2
    relays_failed=1
    ;;
  esac
done
[ "$relays_failed" = 0 ] || die "the deploy is live at $head, but a relay did not restart (above)"
