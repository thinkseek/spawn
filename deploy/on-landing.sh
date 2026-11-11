#!/usr/bin/env bash
# Deploy-on-landing (#54): the live system deploys what the fleet landed on
# the Spawn source, without a hand step.
#
#   deploy/on-landing.sh      # one tick; spawn-deploy-on-landing.timer runs it every minute
#
# Spawn's missions main (<live>/missions/spawn.git) is the live system: a
# task the integrator landed there is live only after a deploy. Each tick
# decides, from the store and the host, whether to deploy main now, and then
# runs deploy/deploy.sh --main from a checkout of main of its own. Only the
# Spawn source: a landing on any other source (Supply PA's) deploys nothing
# here.
#
# Main is deployed only when all of this holds, and the tick says the first
# that does not (once: a line is said again only when it changes):
# - SPAWN_DEPLOY_ON_LANDING is not 0 (in spawn.env or the environment);
# - main is not the live controller's commit (<live>/controllers/spawn);
# - the store holds a gate-accepted landing of exactly main on the source
#   (`spawn mission landing`: a task.landed of a task of a mission on it,
#   which the integrator writes only after the task's gate accepted it, or
#   a source.moved there with verdict accepted, an owner's gated push);
#   deploy.sh --main checks it again, under its lock. Never a file: every
#   process of this uid can write one;
# - nobody holds the source (`spawn mission hold`): an owner gate, or a
#   deploy, is under way, and its deploy follows;
# - no owner's landing request on the source is waiting for its judging or
#   in it (`spawn mission landing`'s owner_request: a source.push_requested
#   with no source.moved or source.push_refused naming it). Such a judging
#   holds nothing until it is accepted, and a deploy restarts it (S23);
# - no deploy is pending (<live>/deploy.pending: a deploy died after its
#   push; the owner's next deploy completes it);
# - the nightly is not red on the source (its fact,
#   $SPAWN_WORKSPACE_ROOT/nightly/spawn.json, has no red_since; a fact that
#   is there and does not read as a JSON object counts as red, and a
#   source with no fact at all is deployed, the deploy's line saying red
#   was not checked): a red main
#   is a known regression, and a deploy would put it live. Task landings
#   wait while it is red too (gate v2 step 5b), so what waits here is what
#   landed before the red was found; the first tick after a green night
#   deploys it. The nightly's own alert says it is red; this says nothing
#   more;
# - the last deploy is not a failure of this main younger than
#   SPAWN_DEPLOY_RETRY_SECS (default 1800): a failed main is tried again
#   after that, or as soon as another landing moves main. Its alert stays
#   open meanwhile (one per occurrence: alerts.rs `deploy_failed`);
# - the debounce: no landing for SPAWN_DEPLOY_QUIET_SECS (default 300)
#   since main's, so a burst of landings makes one deploy, or else
#   SPAWN_DEPLOY_MAX_WAIT_SECS (default 1800) have passed since the first
#   landing after the live commit's, so landings that keep coming still
#   reach the live system within that bound;
# - no deploy holds <live>/deploy.lock.
#
# Then it brings its checkout (<live>/deploy-checkout, a clone of the
# missions repo made on the first tick) to main, installs the UI's
# dependencies when their lockfile changed (pnpm install --frozen-lockfile;
# web/ is protected, so the lockfile is an owner's), and holds the source
# itself, `spawn mission hold` with the reason "deploy-on-landing <main>",
# so no landing moves main while the deploy builds and the integrator is
# stopped: deploy.sh would fail on a main that moved. deploy.sh releases the
# hold on every exit, records how the deploy ended (deploy.finished, `by`
# landing) and restarts the services; gates keep running in their own units
# (deploy.sh, "Gates outlive the integrator"). A tick that fails before
# deploy.sh runs (the checkout, the dependencies) records that failure the
# same way, so it raises the same alert.
#
# A hold that outlives its deploy. The hold's bound is
# SPAWN_DEPLOY_HOLD_SECS (default 3600: longer than a deploy, which builds
# in minutes and stops the integrator without a drain), not the owner's
# 3 h. A tick, or its deploy, killed outright (SIGKILL, the OOM killer, a
# reboot) runs no trap and leaves its hold; the next tick finds a hold with
# this reason while no tick and no deploy run (its own lock and
# deploy.lock are free: a killed process holds neither), and releases it.
# So landings wait at most a minute after such a kill, and the bound is the
# last resort.
#
# It removes nothing: it empties its own said-file, and a checkout path
# that is no clone stops the tick (recorded as a failure) for the owner to
# move away.
#
# The binary and the database are the live ones ($SPAWN_LIVE, which must be
# set: no default; bin/spawn and spawn.env); the URL never reaches a
# command line.
set -euo pipefail

PROG=on-landing
# Named, never defaulted (sync/cc-rules.md): a run that forgot it must not
# act on the live directory. The unit names it; deploy.sh gets it too.
if [ -z "${SPAWN_LIVE-}" ]; then
  printf 'on-landing: error: SPAWN_LIVE is not set (the unit sets it): nothing was done\n' >&2
  exit 2
fi
live=$SPAWN_LIVE
export SPAWN_LIVE
missions=$live/missions/spawn.git
checkout=$live/deploy-checkout
said_file=$live/on-landing.said
reason_prefix="deploy-on-landing "

say() { printf 'on-landing: %s\n' "$*"; }
# say_once <line>: said when it differs from the last line said, so a tick a
# minute that waits says so once, not every minute.
say_once() {
  [ "$(cat "$said_file" 2>/dev/null)" = "$*" ] && return 0
  say "$*"
  printf '%s\n' "$*" >"$said_file.tmp" && mv "$said_file.tmp" "$said_file"
}
die() {
  printf 'on-landing: error: %s\n' "$*" >&2
  exit 1
}
# whole <name> <value>: a whole number of seconds, or the tick is refused.
whole() {
  [[ $2 =~ ^[1-9][0-9]{0,5}$ ]] || die "$1 must be a whole number of seconds (1 to 999999), not '$2'"
}
spawn_live() { "$live/bin/spawn" "$@"; }
# failed <head> <reason>: a tick that failed before deploy.sh ran is recorded
# as deploy.sh records its own failures (deploy.finished, by landing), and
# the tick exits 1.
failed() {
  local out
  out=$(spawn_live mission deployed --head "$1" --ok false --by landing --reason "$2" 2>&1) ||
    printf 'on-landing: could not record the failure (%s)\n' "$(printf '%s' "$out" | head -c 300 | tr '\n' ' ')" >&2
  die "$2"
}

tick() {
  [ -d "$live" ] || die "no live directory $live"
  # One tick at a time; a tick the timer starts while one runs ends here.
  exec 8>>"$live/on-landing.lock"
  flock -n 8 || exit 0
  [ -f "$live/spawn.env" ] || die "no $live/spawn.env"
  local paused=${SPAWN_DEPLOY_ON_LANDING-}
  set -a
  # shellcheck disable=SC1091
  . "$live/spawn.env"
  set +a
  # The environment's value first, as deploy.sh reads its mirror.
  [ -z "$paused" ] || SPAWN_DEPLOY_ON_LANDING=$paused
  if [ "${SPAWN_DEPLOY_ON_LANDING-1}" = 0 ]; then
    say_once "paused: SPAWN_DEPLOY_ON_LANDING=0"
    return 0
  fi
  local quiet=${SPAWN_DEPLOY_QUIET_SECS:-300} max_wait=${SPAWN_DEPLOY_MAX_WAIT_SECS:-1800}
  local retry=${SPAWN_DEPLOY_RETRY_SECS:-1800} hold_secs=${SPAWN_DEPLOY_HOLD_SECS:-3600}
  whole SPAWN_DEPLOY_QUIET_SECS "$quiet"
  whole SPAWN_DEPLOY_MAX_WAIT_SECS "$max_wait"
  whole SPAWN_DEPLOY_RETRY_SECS "$retry"
  whole SPAWN_DEPLOY_HOLD_SECS "$hold_secs"
  [ -x "$live/bin/spawn" ] || die "no live binary $live/bin/spawn"

  local deployed read main hold_reason
  deployed=$(readlink "$live/controllers/spawn" 2>/dev/null) || deployed=""
  deployed=${deployed#spawn-}
  [[ $deployed =~ ^[0-9a-f]{40}$ ]] || deployed=""
  read=$(spawn_live mission landing --source "$missions" ${deployed:+--deployed "$deployed"}) ||
    die "could not read main's landing from the store (above)"
  main=$(printf '%s' "$read" | jq -er '.main') || die "the store's answer names no main"

  # A hold this script took whose tick and deploy are gone (header, "A hold
  # that outlives its deploy"): we hold our own lock, so no tick runs, and a
  # free deploy.lock means no deploy runs.
  hold_reason=$(printf '%s' "$read" | jq -r '.hold.reason // empty')
  if [[ $hold_reason == "$reason_prefix"* ]] && flock -n "$live/deploy.lock" true; then
    local released
    released=$(spawn_live mission release --source "$missions") ||
      die "could not release the hold a killed deploy-on-landing left (above)"
    say "released hold $(printf '%s' "$released" | jq -r '.released') ($hold_reason): its tick or deploy was killed before it could"
    : >"$said_file"
    read=$(spawn_live mission landing --source "$missions" ${deployed:+--deployed "$deployed"}) ||
      die "could not read main's landing from the store (above)"
    main=$(printf '%s' "$read" | jq -er '.main') || die "the store's answer names no main"
  fi

  if [ "$main" = "$deployed" ]; then
    say_once "live at main $main"
    return 0
  fi
  if [ "$(printf '%s' "$read" | jq -r '.hold')" != null ]; then
    say_once "main $main waits: the source is held ($(printf '%s' "$read" | jq -r '.hold.reason // "no reason"')); the holder's deploy follows"
    return 0
  fi
  # An owner's landing in its judging holds nothing on the source until it
  # is accepted, and a deploy would stop its gate (S23: 06:24 restarted the
  # S3 owner judging): wait while the store has one with no outcome.
  if [ "$(printf '%s' "$read" | jq -r '.owner_request')" != null ]; then
    say_once "main $main waits: the owner's landing request $(printf '%s' "$read" | jq -r '.owner_request.request') ($(printf '%s' "$read" | jq -r '.owner_request.commit')) is waiting for its judging or in it; deployed once it has an outcome"
    return 0
  fi
  if [ -e "$live/deploy.pending" ]; then
    say_once "main $main waits: a deploy is pending ($(cat "$live/deploy.pending")); the owner's next deploy completes it"
    return 0
  fi
  if [ "$(printf '%s' "$read" | jq -r '.landing')" = null ]; then
    say_once "main $main has no gate-accepted landing in the store: not deployed here (a deploy with a gate's verdict file deploys it)"
    return 0
  fi
  local fact red nightly_note=""
  fact=${SPAWN_WORKSPACE_ROOT:-}/nightly/$(basename "$missions" .git).json
  if [ -n "${SPAWN_WORKSPACE_ROOT-}" ] && [ -f "$fact" ]; then
    # A fact that is there but does not read is no green: closed, not open.
    if ! red=$(jq -er 'if type == "object" then (.red_since // "") else error("not an object") end' "$fact" 2>/dev/null); then
      say_once "main $main waits: the nightly's fact $fact does not read as JSON; deployed once it does (a nightly run writes it again), or by hand"
      return 0
    fi
    if [ -n "$red" ]; then
      say_once "main $main waits: the nightly is red since $red (failed: $(jq -r '(.failed // []) | join(" ")' "$fact" 2>/dev/null)); deployed after a green night, or by hand"
      return 0
    fi
  else
    # No nightly on this source (yet): nothing says main is red, and the
    # deploy says it did not look.
    nightly_note="; no nightly fact at ${fact#/}, so red was not checked"
  fi
  local now at first last_ok last_head last_at
  now=$(printf '%s' "$read" | jq -r '.now_ms')
  # tostring, not `// empty`: jq's `//` takes false for missing.
  last_ok=$(printf '%s' "$read" | jq -r '.last_deploy.ok | tostring')
  last_head=$(printf '%s' "$read" | jq -r '.last_deploy.head // empty')
  last_at=$(printf '%s' "$read" | jq -r '.last_deploy.at_ms // 0')
  if [ "$last_ok" = false ] && [ "$last_head" = "$main" ] && [ $((now - last_at)) -lt $((retry * 1000)) ]; then
    say_once "main $main waits: its deploy failed $(((now - last_at) / 1000)) s ago; tried again $retry s after it (SPAWN_DEPLOY_RETRY_SECS), or at the next landing"
    return 0
  fi
  at=$(printf '%s' "$read" | jq -r '.landing.at_ms')
  first=$(printf '%s' "$read" | jq -r '.first_undeployed_ms // .landing.at_ms')
  if [ $((now - at)) -lt $((quiet * 1000)) ] && [ $((now - first)) -lt $((max_wait * 1000)) ]; then
    say_once "main $main waits: landed $(((now - at) / 1000)) s ago; deployed after $quiet s without a landing (SPAWN_DEPLOY_QUIET_SECS), at the latest $max_wait s after the first landing not live (SPAWN_DEPLOY_MAX_WAIT_SECS)"
    return 0
  fi
  if ! flock -n "$live/deploy.lock" true; then
    say_once "main $main waits: another deploy holds $live/deploy.lock"
    return 0
  fi

  say "deploying main $main (landed by $(printf '%s' "$read" | jq -r '.landing.record'), event $(printf '%s' "$read" | jq -r '.landing.event'); live is ${deployed:-unknown}$nightly_note)"
  : >"$said_file"
  # The checkout: a clone of the missions repo, at main, nothing in it but
  # the commit (deploy.sh refuses a tree with changes and exports HEAD).
  # Made once; this script removes nothing (sync/cc-rules.md): a path there
  # that is no clone is the owner's to look at.
  if [ ! -d "$checkout/.git" ]; then
    [ ! -e "$checkout" ] && [ ! -L "$checkout" ] ||
      failed "$main" "$checkout is there but is no clone of $missions: move it away by hand, and the next tick makes the clone"
    git clone -q --no-checkout "$missions" "$checkout" ||
      failed "$main" "could not clone $missions into $checkout"
  fi
  git -C "$checkout" fetch -q "$missions" main ||
    failed "$main" "could not fetch main into $checkout"
  git -C "$checkout" -c advice.detachedHead=false checkout -q --force --detach "$main" ||
    failed "$main" "could not check out $main in $checkout"
  # The UI's dependencies, installed from main's lockfile when it changed:
  # deploy.sh builds the UI with them and controller.sh refuses others.
  if [ -f "$checkout/web/pnpm-lock.yaml" ] &&
    ! cmp -s "$checkout/web/pnpm-lock.yaml" "$checkout/web/node_modules/.pnpm/lock.yaml"; then
    command -v pnpm >/dev/null || failed "$main" "pnpm is not on PATH: the UI's dependencies for $main cannot be installed"
    (cd "$checkout/web" && pnpm install --frozen-lockfile --prefer-offline --reporter=silent) ||
      failed "$main" "pnpm install --frozen-lockfile failed in $checkout/web"
  fi
  # The source held at main, so no landing moves it during the deploy; the
  # deploy releases it on every exit.
  local held
  held=$(SPAWN_SOURCE_HOLD_SECS=$hold_secs spawn_live mission hold --source "$missions" --reason "$reason_prefix$main") ||
    die "could not hold $missions (above)"
  if [ "$(printf '%s' "$held" | jq -r '.main')" != "$main" ]; then
    spawn_live mission release --source "$missions" >/dev/null ||
      say "could not release hold $(printf '%s' "$held" | jq -r '.hold'); the next tick releases it"
    say "main moved past $main before the hold; the next tick decides again"
    return 0
  fi
  say "held $missions at $main (hold $(printf '%s' "$held" | jq -r '.hold'), at most $hold_secs s)"
  cd "$checkout"
  # Its own deploy.sh: main's, which the fleet cannot have changed (deploy/
  # is protected), as the owner runs the checkout's own.
  bash deploy/deploy.sh --main
}

# All of it read before it runs: a deploy switches the controller this file
# lives in.
tick "$@"
exit 0
