#!/usr/bin/env bash
# deploy.sh end to end against a fake live directory; the live fleet is never
# touched.
#
#   deploy/test-deploy.sh
#
# The fake live directory ($SPAWN_LIVE) holds a bare missions repo whose main
# is HEAD~1, a spawn.env naming a throwaway database created on the test
# server (~/.config/spawn/test-database.url, never printed) and dropped at the
# end, and a bin/ with a stand-in `spawn` that the deploy replaces with the
# real build. A fake `systemctl` first in PATH only logs; a `git` wrapper
# breaks a deploy right after its push. The deploy runs from a clone of HEAD
# (commit first) with untracked files planted in it, and builds into the
# real deploy target directory (~/.cache/spawn/deploy-target, incremental;
# SPAWN_DEPLOY_TARGET names another).
#
# Covered: the disk guard; the deploy lock; a HEAD tracking files under
# node_modules/ is refused; the sandbox has no network and no $HOME; an
# untracked file in the checkout reaches neither the binary nor the UI; a
# committed web/vite.config.js or web/postcss.config.js is never run; a
# failure before the install leaves the live binary, UI and rollback copies
# alone; a deploy killed after its push leaves deploy.pending and the next
# deploy completes it; the controller becomes HEAD's tree with the install
# (with the UI's node_modules, without the checkout's untracked files), and
# not before it; a relay unit that is not installed does not stop a
# deploy; a deploy run again keeps its rollback copies; a deploy failing
# after its push completes it on exit; a pending deploy that main has moved
# past stops the deploy; deploy/hold.sh holds the missions repo at its main,
# and the deploy releases that hold on success and on failure; --landed
# deploys a HEAD the owner's gated push already landed (no push, no move),
# and refuses an outcome that is not an accepted landing of HEAD on this
# missions repo, or a main that moved since; the drain
# (objective 037): a binary that can drain is drained before the stop, and
# the stop follows within the unit's restart delay; a binary that cannot is
# never asked (no --drain, no SIGUSR1); a drain past SPAWN_DEPLOY_DRAIN_SECS
# or one that fails falls back to the stop and the deploy goes on, the
# owner's hold released; so does a drain that ignores TERM (timeout's
# --kill-after, status 137); an integrator whose own bound
# (SPAWN_INTEGRATE_DRAIN_SECS) is below the deploy's is warned about and is
# not said to have judged every gate; main moving during the drain stops
# the deploy, with the services restarted and the hold released; a pending
# deploy is completed after a plain stop, never a drain; an
# integrator not running is not asked; a bad SPAWN_DEPLOY_DRAIN_SECS is
# refused before anything is done, and says the owner's hold stays; the
# mirror (SPAWN_DEPLOY_MIRROR, a local
# bare repo): without one nothing is pushed or said; a mirror behind is
# fast-forwarded to the deployed main, after the restart and the hold's
# release, and gets no tag even with push.followTags set; a mirror that
# diverged keeps its main and the deploy names both
# commits; a mirror that is not there, and one that never answers (bounded
# by SPAWN_DEPLOY_MIRROR_SECS), are one line each; none of them changes the
# deploy's status; an empty value in the environment turns off the mirror
# spawn.env names; a deploy that failed publishes nothing, even with main
# moved; a bad SPAWN_DEPLOY_MIRROR_SECS is refused before anything is done;
# a mirror that carries a credential (in its URL, in the URL of the remote
# it names, or after url.*.insteadOf) is refused before anything is done,
# and the credential reaches neither the output nor any git command line.
#
# TEST_DEPLOY_SCRIPT=<path> runs that deploy script instead of the
# checkout's deploy/deploy.sh (to show which checks fail without a fix).
set -euo pipefail

repo=$(git rev-parse --show-toplevel)
PROG=test-deploy
# shellcheck source=deploy/backup/lib.sh
. "$repo/deploy/backup/lib.sh"

failures=0
pass() { printf 'ok   %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; failures=$((failures + 1)); }
check() { # check <description> <command...>
  local what=$1; shift
  if "$@"; then pass "$what"; else fail "$what"; fi
}

# SPAWN_DEPLOY_TEST_URL_FILE names another file holding the test server's URL
# (a reviewer's own server; the file, never the URL, goes in the environment).
test_url_file=${SPAWN_DEPLOY_TEST_URL_FILE:-$HOME/.config/spawn/test-database.url}
[ -f "$test_url_file" ] || die "no $test_url_file"
test_url=$(cat "$test_url_file")
case $test_url in *:15433/*) die "the test URL names port 15433, the live database" ;; esac
db=spawn_deploy_test_$$
live_url=${test_url%/*}/$db

scratch=$(mktemp -d "${TMPDIR:-/tmp}/spawn-deploy-test.XXXXXX")
cleanup() {
  with_db "$test_url" psql -XAtq -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" >/dev/null 2>&1 || true
  # The controllers are read-only (deploy/controller.sh).
  chmod -R u+w "$scratch" 2>/dev/null || true
  rm -rf "$scratch"
}
trap cleanup EXIT
with_db "$test_url" psql -XAtq -v ON_ERROR_STOP=1 -c "CREATE DATABASE $db" >/dev/null

live=$scratch/live
missions=$live/missions/spawn.git
mkdir -p "$live/bin" "$live/backups" "$scratch/tmp" "$scratch/fakebin"
printf 'SPAWN_DATABASE_URL=%q\n' "$live_url" >"$live/spawn.env"
chmod 600 "$live/spawn.env"
cat >"$live/bin/spawn" <<'EOF'
#!/bin/sh
# The stand-in the first deploy replaces.
case "$1" in mission) echo '{"missions":0}' ;; esac
EOF
chmod 755 "$live/bin/spawn"

export SYSTEMCTL_LOG=$scratch/systemctl.log
: >"$SYSTEMCTL_LOG"
cat >"$scratch/fakebin/systemctl" <<'EOF'
#!/bin/sh
# SYSTEMCTL_FAIL="<arguments>=<status>": that one call fails with <status>.
# This host has no spawn-model-relay unit (the README installs relays by
# hand): systemctl says so with status 5, as it does for "Unit not found".
echo "$*" >>"$SYSTEMCTL_LOG"
[ -z "${SYSTEMCTL_TIMES-}" ] || echo "$(date +%s.%N) $*" >>"$SYSTEMCTL_TIMES"
# FAKE_INTEGRATOR_INACTIVE: the integrator unit is not running.
case "$*" in "--user is-active --quiet spawn-integrate") [ -z "${FAKE_INTEGRATOR_INACTIVE-}" ] || exit 3 ;; esac
case "${SYSTEMCTL_FAIL-}" in "$*="*) exit "${SYSTEMCTL_FAIL##*=}" ;; esac
case "$*" in "--user try-restart spawn-model-relay") exit 5 ;; esac
EOF
real_git=$(command -v git)
cat >"$scratch/fakebin/git" <<EOF
#!/bin/sh
# DEPLOY_TEST_AFTER_PUSH=kill: the deploy dies outright right after its push
# (no trap runs); =fail: the push "fails" after it landed. GIT_ARGV_LOG=<file>:
# every git command line of the deploy, one per line.
[ -z "\${GIT_ARGV_LOG-}" ] || echo "\$*" >>"\$GIT_ARGV_LOG"
if [ "\${1-}" = push ] && [ -n "\${DEPLOY_TEST_AFTER_PUSH-}" ]; then
  "$real_git" "\$@" || exit
  case \$DEPLOY_TEST_AFTER_PUSH in
  kill) kill -KILL "\$PPID" ;;
  esac
  exit 1
fi
exec "$real_git" "\$@"
EOF
chmod 755 "$scratch/fakebin/systemctl" "$scratch/fakebin/git"

# The checkout to deploy: HEAD plus a commit that adds UI config files the
# build must never run (each throws when loaded), with untracked files that a
# build of the checkout itself would pick up (cargo reads
# .cargo/config.toml, vite copies public/).
commit() { git -C "$1" -c user.name=test -c user.email=test@localhost commit -q -m "$2"; }
checkout=$scratch/checkout
git clone -q --shared --no-checkout "$repo" "$checkout"
git -C "$checkout" checkout -q --detach "$(git -C "$repo" rev-parse HEAD)"
for config in vite.config.js postcss.config.js; do
  echo "throw new Error(\"the planted web/$config ran\");" >"$checkout/web/$config"
done
git -C "$checkout" add web/vite.config.js web/postcss.config.js
commit "$checkout" "web: config files the deploy must not load"
head=$(git -C "$checkout" rev-parse HEAD)
base=$(git -C "$checkout" rev-parse HEAD~1)
ln -s "$(readlink -f "$repo/web/node_modules")" "$checkout/web/node_modules"
mkdir -p "$checkout/.cargo" "$checkout/web/public"
build_id=5370617766537472617921
printf '[build]\nrustflags = ["-C", "link-arg=-Wl,--build-id=0x%s"]\n' "$build_id" >"$checkout/.cargo/config.toml"
echo stray >"$checkout/web/public/stray.txt"

git init -q --bare "$missions"
git -C "$checkout" push -q "$missions" "$base:refs/heads/main"
verdict=$scratch/verdict.json
# verdict_over <main>: the gate's verdict for HEAD over that main.
verdict_over() {
  jq -n --arg c "$head" --arg b "$1" '{verdict: "accepted", candidate: $c, base: $b}' >"$verdict"
}
verdict_over "$base"

main() { git --git-dir="$missions" rev-parse main; }
# spawn_live <args>: the installed binary against the fake live database
# (its URL read from spawn.env, never on a command line).
spawn_live() { (set -a && . "$live/spawn.env" && set +a && "$live/bin/spawn" "$@"); }
# The owner's active hold on the missions repo (its event id), empty if none.
missions_hold() {
  spawn_live mission list |
    jq -r --arg s "$missions" '[.holds[]? | select(.source == $s)] | first | .hold // empty'
}
# hold_missions: deploy/hold.sh, run from the checkout as the owner would.
hold_missions() {
  (cd "$checkout" && SPAWN_LIVE="$live" bash deploy/hold.sh "$head") >"$scratch/hold.out" 2>&1
}
# deploy [ENV=VALUE...]: run deploy.sh in $deploy_from (default the
# checkout); output in $out, status in $status.
deploy_from=$checkout
landed=""
deploy() {
  out=$scratch/deploy.out
  set +e
  (cd "$deploy_from" && env SPAWN_LIVE="$live" TMPDIR="$scratch/tmp" PATH="$scratch/fakebin:$PATH" "$@" \
    bash "${TEST_DEPLOY_SCRIPT:-deploy/deploy.sh}" ${landed:+--landed} "$verdict") >"$out" 2>&1
  status=$?
  set -e
  sed 's/^/  | /' "$out"
}

echo "== the disk guard"
deploy SPAWN_DEPLOY_MIN_FREE_GB=1000000
check "refused" [ "$status" -ne 0 ]
check "says how much is free and needed" grep -q "GB free, the deploy needs 1000000" "$out"
check "before any backup" [ -z "$(ls "$live/backups")" ]
check "before touching a service" [ ! -s "$SYSTEMCTL_LOG" ]

echo "== the deploy lock"
# -o: the lock stays with flock itself, not with sleep, so killing flock
# releases it.
flock -o "$live/deploy.lock" sleep 120 &
holder=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
  flock -n "$live/deploy.lock" true || break
  sleep 0.2
done
deploy
pkill -P "$holder" 2>/dev/null || true
kill "$holder" 2>/dev/null || true
wait "$holder" 2>/dev/null || true
check "the lock is free again" flock -n "$live/deploy.lock" true
check "refused while another deploy holds the lock" [ "$status" -ne 0 ]
check "says so" grep -q "another deploy holds $live/deploy.lock" "$out"
check "before any backup" [ -z "$(ls "$live/backups")" ]
check "before touching a service" [ ! -s "$SYSTEMCTL_LOG" ]

echo "== a HEAD that tracks files under node_modules/"
planted=$scratch/planted
git -c advice.detachedHead=false clone -q --shared "$checkout" "$planted"
mkdir -p "$planted/web/node_modules/.bin"
printf '#!/bin/sh\necho planted tsc\n' >"$planted/web/node_modules/.bin/tsc"
git -C "$planted" add -f web/node_modules/.bin/tsc
commit "$planted" "web: a planted tsc"
planted_head=$(git -C "$planted" rev-parse HEAD)
jq -n --arg c "$planted_head" --arg b "$base" '{verdict: "accepted", candidate: $c, base: $b}' >"$verdict"
deploy_from=$planted
deploy
deploy_from=$checkout
verdict_over "$base"
check "refused" [ "$status" -ne 0 ]
check "names the file" grep -q "HEAD tracks files under node_modules/ (web/node_modules/.bin/tsc" "$out"
check "before any backup" [ -z "$(ls "$live/backups")" ]
check "before touching a service" [ ! -s "$SYSTEMCTL_LOG" ]
rm -rf "$planted"

echo "== the sandbox"
check "deploy.sh builds both in the sandbox" \
  [ "$(grep -c '^sandbox_run "\$export_dir' "$repo/deploy/deploy.sh")" -eq 2 ]
check "the sandbox unshares the network" grep -q -- '--unshare-net' "$repo/scripts/sandbox.sh"
interfaces=$(bash -c '. "$1/scripts/sandbox.sh"; sandbox_run / cat /proc/net/dev' _ "$repo" |
  awk -F: 'NR > 2 { gsub(/ /, "", $1); print $1 }' | sort | tr '\n' ' ')
check "inside, the only interface is lo (got: $interfaces)" [ "$interfaces" = "lo " ]
check "inside, a TCP connect to 1.1.1.1 fails" \
  bash -c '. "$1/scripts/sandbox.sh"; ! sandbox_run / bash -c "exec 3<>/dev/tcp/1.1.1.1/53" 2>/dev/null' _ "$repo"
check "outside, the same connect succeeds (the check above means something)" \
  bash -c 'timeout 5 bash -c "exec 3<>/dev/tcp/1.1.1.1/53"'
check "inside, ~/.config is hidden" \
  bash -c '. "$1/scripts/sandbox.sh"; sandbox_run / test ! -e "$HOME/.config"' _ "$repo"

echo "== a failure before the install"
cp "$live/bin/spawn" "$scratch/stand-in"
deploy SYSTEMCTL_FAIL="--user stop spawn-integrate=1"
check "failed (status $status)" [ "$status" -ne 0 ]
check "the planted UI configs did not run" bash -c '! grep -q "the planted web/" "$1"' _ "$out"
check "the live binary is untouched" cmp -s "$live/bin/spawn" "$scratch/stand-in"
check "no rollback copy was written" [ ! -e "$live/bin/spawn.prev" ]
check "no UI was installed" [ ! -e "$live/web" ]
check "nothing is left staged" bash -c '[ ! -e "$1/bin/spawn.staged" ] && [ ! -e "$1/web.staged" ]' _ "$live"
check "main did not move" [ "$(main)" = "$base" ]
check "no controller was made current" [ ! -e "$live/controllers/spawn" ]
check "the services restarted" grep -qx -- "--user restart spawn-loop spawn-integrate spawn-serve" "$SYSTEMCTL_LOG"
: >"$SYSTEMCTL_LOG"

echo "== a deploy killed right after its push"
deploy DEPLOY_TEST_AFTER_PUSH=kill
check "it died (status $status)" [ "$status" -eq 137 ]
check "main moved to HEAD" [ "$(main)" = "$head" ]
check "deploy.pending holds the move" [ "$(cat "$live/deploy.pending" 2>/dev/null)" = "$base $head" ]
check "the integrator was stopped and the services not restarted" \
  bash -c 'grep -qx -- "--user stop spawn-integrate" "$1" && ! grep -q "restart spawn-loop" "$1"' _ "$SYSTEMCTL_LOG"
check "the real binary is installed" "$live/bin/spawn" version
check "the untracked .cargo/config.toml did not reach the binary" \
  bash -c '! readelf -n "$1" | grep -qi "$2"' _ "$live/bin/spawn" "$build_id"
check "the untracked web/public/stray.txt did not reach the UI" [ ! -e "$live/web/stray.txt" ]
check "the UI is installed (the planted vite.config.js and postcss.config.js did not run)" \
  [ -f "$live/web/index.html" ]
check "spawn.prev is the binary it replaced" cmp -s "$live/bin/spawn.prev" "$scratch/stand-in"
check "the controller is HEAD's tree" \
  bash -c '[ "$(cat "$1/controllers/spawn/.controller")" = "$2" ] && [ -f "$1/controllers/spawn/scripts/rsi/gate.sh" ]' _ "$live" "$head"
check "the controller has the UI's node_modules" [ -x "$live/controllers/spawn/web/node_modules/.bin/vite" ]
check "the untracked web/public/stray.txt did not reach the controller" \
  [ ! -e "$live/controllers/spawn/web/public/stray.txt" ]

echo "== the owner's hold before the gate (deploy/hold.sh)"
spawn_live mission add --text "test-deploy: the Spawn mission" --source "$missions" >/dev/null
: >"$SYSTEMCTL_LOG"
hold_missions
sed 's/^/  | /' "$scratch/hold.out"
held=$(missions_hold)
check "deploy/hold.sh holds the missions repo (hold ${held:-none})" [ -n "$held" ]
check "at its main, printed for the gate" grep -q "held landings on $missions at main $(main)" "$scratch/hold.out"
check "the integrator was not touched" [ ! -s "$SYSTEMCTL_LOG" ]

echo "== the next deploy completes it (the model relay's unit is not installed)"
# Main is HEAD now: the operator runs the gate again, over it.
verdict_over "$head"
: >"$SYSTEMCTL_LOG"
deploy
check "succeeded (status $status)" [ "$status" -eq 0 ]
check "released the owner's hold" [ -z "$(missions_hold)" ]
check "says so" grep -q "released the owner's hold $held on $missions" "$out"
check "says the relay unit is not installed" grep -q "installed model-relay.py (unit spawn-model-relay is not installed" "$out"
check "both relay scripts are installed" \
  bash -c 'cmp -s "$1/scripts/jev-relay.py" "$2/jev-relay.py" && cmp -s "$1/scripts/model-relay.py" "$2/model-relay.py"' _ "$checkout" "$live/bin"
check "the same build again keeps spawn.prev the binary it replaced" cmp -s "$live/bin/spawn.prev" "$scratch/stand-in"
check "the same UI again leaves no web.prev of itself" [ ! -e "$live/web.prev" ]
check "completed the pending deploy" grep -q "completed the pending deploy: main $base -> $head" "$out"
check "the controller stays HEAD's" [ "$(readlink "$live/controllers/spawn")" = "spawn-$head" ]
check "deploy.pending is gone" [ ! -e "$live/deploy.pending" ]
check "the integrator was stopped, started, and the services restarted" \
  bash -c 'grep -qx -- "--user stop spawn-integrate" "$1" && grep -qx -- "--user start spawn-integrate" "$1" &&
    grep -qx -- "--user restart spawn-loop spawn-integrate spawn-serve" "$1"' _ "$SYSTEMCTL_LOG"
check "the export is removed" bash -c '! compgen -G "$1/spawn-deploy-export-*" >/dev/null' _ "$scratch/tmp"

echo "== a deploy failing after its push completes it on exit"
git --git-dir="$missions" update-ref refs/heads/main "$base"
hold_missions
held=$(missions_hold)
check "held again (hold ${held:-none})" [ -n "$held" ]
verdict_over "$base"
: >"$SYSTEMCTL_LOG"
deploy DEPLOY_TEST_AFTER_PUSH=fail
check "failed (status $status)" [ "$status" -ne 0 ]
check "released the owner's hold on its failure" [ -z "$(missions_hold)" ]
check "says so" grep -q "released the owner's hold $held on $missions" "$out"
check "main moved to HEAD" [ "$(main)" = "$head" ]
check "completed the pending deploy on exit" grep -q "completed the pending deploy: main $base -> $head" "$out"
check "deploy.pending is gone" [ ! -e "$live/deploy.pending" ]
check "the services restarted" grep -qx -- "--user restart spawn-loop spawn-integrate spawn-serve" "$SYSTEMCTL_LOG"
check "the export is removed" bash -c '! compgen -G "$1/spawn-deploy-export-*" >/dev/null' _ "$scratch/tmp"

echo "== a pending deploy that main has moved past"
other=$(git -C "$checkout" rev-parse HEAD~2)
printf '%s %s\n' "$base" "$other" >"$live/deploy.pending"
: >"$SYSTEMCTL_LOG"
deploy
check "refused (status $status)" [ "$status" -ne 0 ]
check "says main is elsewhere, once" [ "$(grep -c "is pending, but the missions main is $head" "$out")" -eq 1 ]
check "deploy.pending stays" [ "$(cat "$live/deploy.pending")" = "$base $other" ]
check "the integrator is not left stopped" grep -qx -- "--user restart spawn-loop spawn-integrate spawn-serve" "$SYSTEMCTL_LOG"

echo "== --landed: a HEAD the owner's gated push landed"
rm -f "$live/deploy.pending"
# outcome <state> <type> <verdict> <to> [source]: what `spawn mission push
# --gated --wait` prints, for the deploy to read.
outcome() {
  jq -n --arg st "$1" --arg ty "$2" --arg v "$3" --arg to "$4" --arg src "${5:-$missions}" --arg b "$base" \
    '{request: 7, source: $src, commit: $to, state: $st,
      outcome: {type: $ty, source: $src, from: $b, to: $to, request: 7, by: "human", verdict: $v}}' >"$verdict"
}
landed=1
check "main is HEAD, as the landing left it" [ "$(main)" = "$head" ]
for refused in "refused source.push_refused rejected $head|the owner's push is refused, not applied" \
  "applied source.moved accepted $base|the owner's push landed $base, HEAD is $head" \
  "applied source.moved none $head|landed without a gate's acceptance" \
  "applied source.moved accepted $head $scratch/elsewhere.git|not on $missions"; do
  set -- ${refused%%|*}
  outcome "$@"
  : >"$SYSTEMCTL_LOG"
  backups=$(ls "$live/backups" | wc -l)
  deploy
  check "refused: ${refused#*|} (status $status)" [ "$status" -ne 0 ]
  check "says so" grep -qF -- "${refused#*|}" "$out"
  check "before any backup" [ "$(ls "$live/backups" | wc -l)" -eq "$backups" ]
  check "before touching a service" [ ! -s "$SYSTEMCTL_LOG" ]
done
outcome applied source.moved accepted "$head"
git --git-dir="$missions" update-ref refs/heads/main "$base"
: >"$SYSTEMCTL_LOG"
deploy
check "refused when main is not the landed HEAD (status $status)" [ "$status" -ne 0 ]
check "says main moved since the landing" grep -q "the missions main is $base, not the landed $head" "$out"
check "before touching a service" [ ! -s "$SYSTEMCTL_LOG" ]
git --git-dir="$missions" update-ref refs/heads/main "$head"
: >"$SYSTEMCTL_LOG"
deploy
check "an accepted landing of HEAD deploys (status $status)" [ "$status" -eq 0 ]
check "says it was landed by the owner's gated push" grep -q "landed by the owner's gated push (request 7): no push, no move to record" "$out"
check "main stays HEAD" [ "$(main)" = "$head" ]
check "nothing is pending" [ ! -e "$live/deploy.pending" ]
check "the controller is HEAD's" [ "$(readlink "$live/controllers/spawn")" = "spawn-$head" ]
check "the services restarted" grep -qx -- "--user restart spawn-loop spawn-integrate spawn-serve" "$SYSTEMCTL_LOG"
landed=""

echo "== the drain (objective 037)"
rm -f "$live/deploy.pending"
export SPAWN_CALLS=$scratch/spawn-calls.log DRAINED_AT=$scratch/drained-at SYSTEMCTL_TIMES=$scratch/systemctl-times.log
# installed <capable|old>: the installed binary behind a shim that logs its
# calls; "capable" also names --drain in its usage, as a binary with
# objective 037 does, and plays its drain (FAKE_DRAIN: "" a gate that
# records its verdict in FAKE_GATE_SECS, fail, hang, deaf: hangs and ignores
# TERM; FAKE_DRAIN_MOVES_MAIN=<commit>: a task lands it during the drain).
# A deploy that failed before its install left the shim in place: the
# binary behind it stays.
installed() {
  [ "$(head -c 2 "$live/bin/spawn")" = '#!' ] || cp "$live/bin/spawn" "$scratch/real-spawn"
  if [ "$1" = capable ]; then
    cat >"$live/bin/spawn" <<EOF
#!/bin/sh
# usage: spawn integrate [--once | --drain] [--root <dir>] [--gates <n>]
echo "\$*" >>"\$SPAWN_CALLS"
if [ "\${1-} \${2-}" = "integrate --drain" ]; then
  case "\${FAKE_DRAIN-}" in
  fail) echo "integrate: no integrator to drain" >&2; exit 1 ;;
  hang) exec sleep 600 ;;
  deaf) trap '' TERM; sleep 600 & echo "\$\$ \$!" >"\$DRAINED_AT"; wait; exit 0 ;;
  esac
  echo "integrate: asked integrator 4242 to drain; waiting for it to end"
  sleep "\${FAKE_GATE_SECS:-2}"
  [ -z "\${FAKE_DRAIN_MOVES_MAIN-}" ] ||
    "$real_git" --git-dir="$missions" update-ref refs/heads/main "\$FAKE_DRAIN_MOVES_MAIN"
  date +%s.%N >"\$DRAINED_AT"
  exit 0
fi
exec "$scratch/real-spawn" "\$@"
EOF
  else
    printf '#!/bin/sh\necho "$*" >>"$SPAWN_CALLS"\nexec "%s" "$@"\n' "$scratch/real-spawn" >"$live/bin/spawn"
  fi
  chmod 755 "$live/bin/spawn"
  : >"$SPAWN_CALLS"
  : >"$SYSTEMCTL_LOG"
  : >"$SYSTEMCTL_TIMES"
  rm -f "$DRAINED_AT"
  git --git-dir="$missions" update-ref refs/heads/main "$base"
  verdict_over "$base"
}
stop_at() { awk '$2 == "--user" && $3 == "stop" && $4 == "spawn-integrate" { print $1; exit }' "$SYSTEMCTL_TIMES"; }

echo "-- a binary that drains, with a gate running"
installed capable
deploy FAKE_GATE_SECS=3
check "succeeded (status $status)" [ "$status" -eq 0 ]
check "asked the integrator to drain, once" [ "$(grep -cx -- "integrate --drain" "$SPAWN_CALLS")" -eq 1 ]
check "said what it waits for" grep -q "draining spawn-integrate: no new gate starts; waiting up to 3600 s" "$out"
check "passed the drain's own words on" grep -q "deploy:   integrate: asked integrator 4242 to drain" "$out"
check "said it drained" grep -q "drained: every gate that was running recorded its verdict" "$out"
drained=$(cat "$DRAINED_AT" 2>/dev/null || echo 0); stopped_at=$(stop_at)
check "stopped the unit after the drain ended, within its 10 s restart delay ($drained -> ${stopped_at:-none})" \
  awk -v d="$drained" -v s="${stopped_at:-0}" 'BEGIN { exit !(s >= d && s - d < 10) }'
check "main moved to HEAD" [ "$(main)" = "$head" ]
check "the services restarted" grep -qx -- "--user restart spawn-loop spawn-integrate spawn-serve" "$SYSTEMCTL_LOG"

echo "-- a binary that cannot drain"
installed old
deploy
check "succeeded (status $status)" [ "$status" -eq 0 ]
check "never ran --drain (so no SIGUSR1 reached anything)" bash -c '! grep -q -- "--drain" "$1"' _ "$SPAWN_CALLS"
check "says the stop loses the running gates" grep -q "the installed binary cannot drain" "$out"
check "stopped the integrator" grep -qx -- "--user stop spawn-integrate" "$SYSTEMCTL_LOG"
check "main moved to HEAD" [ "$(main)" = "$head" ]

echo "-- a drain past its bound"
installed capable
hold_missions
held=$(missions_hold)
check "held (hold ${held:-none})" [ -n "$held" ]
start=$(date +%s)
deploy FAKE_DRAIN=hang SPAWN_DEPLOY_DRAIN_SECS=2
check "succeeded (status $status)" [ "$status" -eq 0 ]
check "said the bound was reached" grep -q "the drain reached its bound (2 s)" "$out"
check "stopped the integrator" grep -qx -- "--user stop spawn-integrate" "$SYSTEMCTL_LOG"
check "waited the bound, not the drain's 600 s ($(($(date +%s) - start)) s in all)" [ $(($(date +%s) - start)) -lt 300 ]
check "main moved to HEAD" [ "$(main)" = "$head" ]
check "released the owner's hold" [ -z "$(missions_hold)" ]
check "says so" grep -q "released the owner's hold $held on $missions" "$out"

echo "-- a drain that ignores TERM at its bound (timeout's --kill-after)"
installed capable
start=$(date +%s)
deploy FAKE_DRAIN=deaf SPAWN_DEPLOY_DRAIN_SECS=2
check "succeeded (status $status)" [ "$status" -eq 0 ]
check "said the bound was reached" grep -q "the drain reached its bound (2 s)" "$out"
check "the drain was killed, 30 s after its bound ($(($(date +%s) - start)) s in all)" \
  bash -c '[ "$1" -ge 32 ] && [ "$1" -lt 300 ]' _ "$(($(date +%s) - start))"
read -r drain_pid drain_child <"$DRAINED_AT" || true
check "the drain ran (pids ${drain_pid:-none} ${drain_child:-none})" [ -n "${drain_child-}" ]
check "nothing of the drain is left running" bash -c '! kill -0 "$1" 2>/dev/null && ! kill -0 "$2" 2>/dev/null' _ "${drain_pid:-0}" "${drain_child:-0}"
check "stopped the integrator" grep -qx -- "--user stop spawn-integrate" "$SYSTEMCTL_LOG"
check "main moved to HEAD" [ "$(main)" = "$head" ]

echo "-- a drain that fails"
installed capable
hold_missions
held=$(missions_hold)
check "held (hold ${held:-none})" [ -n "$held" ]
deploy FAKE_DRAIN=fail
check "succeeded (status $status)" [ "$status" -eq 0 ]
check "said the drain failed and it stops as before" grep -q "the drain failed (exit 1): stopping the integrator as before" "$out"
check "stopped the integrator" grep -qx -- "--user stop spawn-integrate" "$SYSTEMCTL_LOG"
check "main moved to HEAD" [ "$(main)" = "$head" ]
check "released the owner's hold" [ -z "$(missions_hold)" ]
check "says so" grep -q "released the owner's hold $held on $missions" "$out"

echo "-- an integrator whose own bound is below the deploy's"
installed capable
deploy FAKE_GATE_SECS=1 SPAWN_INTEGRATE_DRAIN_SECS=60
check "succeeded (status $status)" [ "$status" -eq 0 ]
check "warned before the drain" \
  bash -c 'grep -A99 "warning: the integrator.s own bound (SPAWN_INTEGRATE_DRAIN_SECS, 60 s) is below the deploy.s (3600 s)" "$1" | grep -q "draining spawn-integrate"' _ "$out"
check "asked the integrator to drain, once" [ "$(grep -cx -- "integrate --drain" "$SPAWN_CALLS")" -eq 1 ]
check "did not say every gate recorded its verdict" bash -c '! grep -q "every gate that was running recorded its verdict" "$1"' _ "$out"
check "said the drain ended, and by which bound it may have" grep -q "the drain ended, by the integrator's own bound or with every gate judged" "$out"
check "main moved to HEAD" [ "$(main)" = "$head" ]

echo "-- main moves during the drain"
installed capable
hold_missions
held=$(missions_hold)
check "held (hold ${held:-none})" [ -n "$held" ]
deploy FAKE_GATE_SECS=1 FAKE_DRAIN_MOVES_MAIN="$other"
check "failed (status $status)" [ "$status" -ne 0 ]
check "says main moved" grep -q "the missions main moved during the deploy" "$out"
check "main is where the landing left it" [ "$(main)" = "$other" ]
check "nothing is pending" [ ! -e "$live/deploy.pending" ]
check "nothing is left staged" bash -c '[ ! -e "$1/bin/spawn.staged" ] && [ ! -e "$1/web.staged" ]' _ "$live"
check "stopped the integrator, then restarted the services" \
  bash -c 'grep -x -A99 -- "--user stop spawn-integrate" "$1" | grep -qx -- "--user restart spawn-loop spawn-integrate spawn-serve"' _ "$SYSTEMCTL_LOG"
check "released the owner's hold" [ -z "$(missions_hold)" ]
check "says so" grep -q "released the owner's hold $held on $missions" "$out"

echo "-- a pending deploy is completed after a plain stop, not a drain"
installed capable
# The deploy of HEAD died after its push; the verdict is still the one over
# the main it replaced, so this deploy stops once the pending one is complete.
git --git-dir="$missions" update-ref refs/heads/main "$head"
printf '%s %s\n' "$base" "$head" >"$live/deploy.pending"
deploy
check "refused after completing it (status $status)" [ "$status" -ne 0 ]
check "says the verdict is over the old main" grep -q "the verdict judged against $base, the missions main is $head" "$out"
check "never ran --drain" bash -c '! grep -q -- "--drain" "$1"' _ "$SPAWN_CALLS"
check "says it stops without a drain" grep -q "stopping the integrator without a drain" "$out"
check "completed the pending deploy" grep -q "completed the pending deploy: main $base -> $head" "$out"
check "deploy.pending is gone" [ ! -e "$live/deploy.pending" ]
check "the integrator was stopped, then started" \
  bash -c 'grep -x -A99 -- "--user stop spawn-integrate" "$1" | grep -qx -- "--user start spawn-integrate"' _ "$SYSTEMCTL_LOG"

echo "-- an integrator that is not running"
installed capable
deploy FAKE_INTEGRATOR_INACTIVE=1
check "succeeded (status $status)" [ "$status" -eq 0 ]
check "asked nothing to drain" bash -c '! grep -q -- "--drain" "$1"' _ "$SPAWN_CALLS"
check "main moved to HEAD" [ "$(main)" = "$head" ]

echo "-- a bad SPAWN_DEPLOY_DRAIN_SECS"
installed capable
hold_missions
held=$(missions_hold)
check "held (hold ${held:-none})" [ -n "$held" ]
backups_before=$(ls "$live/backups" | wc -l)
for bad in 0 abc 86401 1.5 -3 999999999999999999999; do
  : >"$SYSTEMCTL_LOG"
  deploy SPAWN_DEPLOY_DRAIN_SECS="$bad"
  check "'$bad' refused (status $status)" [ "$status" -ne 0 ]
  check "'$bad': says why" grep -q "SPAWN_DEPLOY_DRAIN_SECS must be a whole number of seconds from 1 to 86400" "$out"
  check "'$bad': before touching a service" [ ! -s "$SYSTEMCTL_LOG" ]
done
check "says the owner's hold stays, and how to release it" \
  grep -q "an owner's hold on $missions stays held (deploy again, or: spawn mission release --source $missions)" "$out"
check "the owner's hold stays (hold $held)" [ "$(missions_hold)" = "$held" ]
spawn_live mission release --source "$missions" >/dev/null
check "before any backup" [ "$(ls "$live/backups" | wc -l)" -eq "$backups_before" ]
check "main did not move" [ "$(main)" = "$base" ]

echo "== the mirror (SPAWN_DEPLOY_MIRROR)"
# The real binary again, without the drain's shim; no integrator to drain.
cp "$scratch/real-spawn" "$live/bin/spawn"
export FAKE_INTEGRATOR_INACTIVE=1
mirror=$scratch/mirror.git
git init -q --bare "$mirror"
mirror_main() { git --git-dir="$mirror" rev-parse refs/heads/main; }
# mirror_at <commit>: the mirror's main is that commit, main is the base
# again with a verdict over it.
mirror_at() {
  git -C "$checkout" push -q -f "$mirror" "$1:refs/heads/main"
  git --git-dir="$missions" update-ref refs/heads/main "$base"
  verdict_over "$base"
  : >"$SYSTEMCTL_LOG"
}
# said_before <text> <text>: the deploy's output holds the first, on a line
# before the second.
said_before() {
  local first second
  first=$(grep -nF -- "$1" "$out" | head -n 1 | cut -d: -f1)
  second=$(grep -nF -- "$2" "$out" | head -n 1 | cut -d: -f1)
  [ -n "$first" ] && [ -n "$second" ] && [ "$first" -lt "$second" ]
}
# A git-remote-hang first in PATH: a mirror "hang::<any>" never answers.
printf '#!/bin/sh\nexec sleep 611\n' >"$scratch/fakebin/git-remote-hang"
chmod 755 "$scratch/fakebin/git-remote-hang"

echo "-- no mirror set"
mirror_at "$base"
deploy
check "succeeded (status $status)" [ "$status" -eq 0 ]
check "main moved to HEAD" [ "$(main)" = "$head" ]
check "nothing was pushed to the mirror" [ "$(mirror_main)" = "$base" ]
check "nothing is said of a mirror" bash -c '! grep -qi mirror "$1"' _ "$out"

echo "-- a mirror behind the deployed main"
mirror_at "$base"
hold_missions
held=$(missions_hold)
check "held (hold ${held:-none})" [ -n "$held" ]
# An annotated tag on HEAD, and a configuration that pushes such tags along.
git --git-dir="$missions" -c user.name=test -c user.email=test@localhost tag -a -m "test-deploy" deployed-test "$head"
git --git-dir="$missions" config push.followTags true
deploy SPAWN_DEPLOY_MIRROR="$mirror"
git --git-dir="$missions" config --unset push.followTags
git --git-dir="$missions" tag -d deployed-test >/dev/null
check "succeeded (status $status)" [ "$status" -eq 0 ]
check "the mirror's main is HEAD" [ "$(mirror_main)" = "$head" ]
check "the mirror got main alone: no tag, with push.followTags set" \
  [ "$(git --git-dir="$mirror" for-each-ref --format='%(refname)')" = refs/heads/main ]
check "says so" grep -q "published main $head to the mirror $mirror" "$out"
check "after the restart" said_before "restarted; live at $head" "published main $head"
check "and after the hold's release" said_before "released the owner's hold $held on $missions" "published main $head"

echo "-- a mirror that diverged (the mirror named in spawn.env)"
fork=$(git -C "$checkout" -c user.name=test -c user.email=test@localhost \
  commit-tree "$base^{tree}" -p "$base" -m "pushed to the mirror by hand")
mirror_at "$fork"
cp -p "$live/spawn.env" "$scratch/spawn.env.orig"
printf 'SPAWN_DEPLOY_MIRROR=%q\n' "$mirror" >>"$live/spawn.env"
deploy
check "the deploy still succeeded (status $status)" [ "$status" -eq 0 ]
check "main moved to HEAD" [ "$(main)" = "$head" ]
check "the services restarted" grep -qx -- "--user restart spawn-loop spawn-integrate spawn-serve" "$SYSTEMCTL_LOG"
check "the mirror keeps its main" [ "$(mirror_main)" = "$fork" ]
check "says the mirror refused, naming both commits" \
  grep -q "the mirror $mirror refused main $head: its main is $fork, which $head does not contain; never forced" "$out"
check "in one line" [ "$(grep -c mirror "$out")" -eq 1 ]

echo "-- an empty SPAWN_DEPLOY_MIRROR in the environment, over spawn.env's"
mirror_at "$base"
deploy SPAWN_DEPLOY_MIRROR=
check "succeeded (status $status)" [ "$status" -eq 0 ]
check "nothing was pushed to the mirror" [ "$(mirror_main)" = "$base" ]
check "nothing is said of a mirror" bash -c '! grep -qi mirror "$1"' _ "$out"
cp -p "$scratch/spawn.env.orig" "$live/spawn.env"

echo "-- a mirror that is not there"
mirror_at "$base"
deploy SPAWN_DEPLOY_MIRROR="$scratch/no-such-mirror.git"
check "the deploy still succeeded (status $status)" [ "$status" -eq 0 ]
check "main moved to HEAD" [ "$(main)" = "$head" ]
check "says it could not publish" \
  grep -q "could not publish main $head to the mirror $scratch/no-such-mirror.git (git push exit " "$out"
check "in one line" [ "$(grep -c mirror "$out")" -eq 1 ]

echo "-- a mirror that never answers"
mirror_at "$base"
start=$(date +%s)
deploy SPAWN_DEPLOY_MIRROR=hang::mirror SPAWN_DEPLOY_MIRROR_SECS=3
check "the deploy still succeeded (status $status)" [ "$status" -eq 0 ]
check "main moved to HEAD" [ "$(main)" = "$head" ]
check "says the bound was reached" \
  grep -q "the mirror hang::mirror did not take main $head within 3 s (SPAWN_DEPLOY_MIRROR_SECS)" "$out"
check "in one line" [ "$(grep -c mirror "$out")" -eq 1 ]
check "waited the bound, not the mirror's 611 s ($(($(date +%s) - start)) s in all)" [ $(($(date +%s) - start)) -lt 300 ]
check "the push is not left running" bash -c '! pgrep -u "$(id -u)" -fx "sleep 611" >/dev/null'

echo "-- a deploy that failed, with main moved"
mirror_at "$base"
deploy SPAWN_DEPLOY_MIRROR="$mirror" DEPLOY_TEST_AFTER_PUSH=fail
check "failed (status $status)" [ "$status" -ne 0 ]
check "main moved to HEAD" [ "$(main)" = "$head" ]
check "nothing was pushed to the mirror" [ "$(mirror_main)" = "$base" ]
check "nothing is said of a mirror" bash -c '! grep -qi mirror "$1"' _ "$out"

echo "-- a bad SPAWN_DEPLOY_MIRROR_SECS"
mirror_at "$base"
for bad in 0 61 abc; do
  deploy SPAWN_DEPLOY_MIRROR="$mirror" SPAWN_DEPLOY_MIRROR_SECS="$bad"
  check "'$bad' refused (status $status)" [ "$status" -ne 0 ]
  check "'$bad': says why" grep -q "SPAWN_DEPLOY_MIRROR_SECS must be a whole number of seconds from 1 to 60" "$out"
  check "'$bad': before touching a service" [ ! -s "$SYSTEMCTL_LOG" ]
done
check "main did not move" [ "$(main)" = "$base" ]
check "nothing was pushed to the mirror" [ "$(mirror_main)" = "$base" ]

echo "-- a mirror that carries a credential"
mirror_at "$base"
export GIT_ARGV_LOG=$scratch/git-argv.log
secret=FAKEs3cretT0ken
git --git-dir="$missions" remote add leaky "https://$secret@127.0.0.1:1/x.git"
git --git-dir="$missions" remote add leaky-push "$mirror"
git --git-dir="$missions" config remote.leaky-push.pushurl "ssh://deploy:$secret@127.0.0.1:1/x.git"
backups_before=$(ls "$live/backups" | wc -l)
# <what>|<SPAWN_DEPLOY_MIRROR>|<the refusal's words>[|<a git configuration, key=value>]
for refused in "a password in an https URL|https://user:$secret@127.0.0.1:1/x.git|SPAWN_DEPLOY_MIRROR is a URL that carries a credential" \
  "a token as the user of an HTTPS URL|HTTPS://$secret@127.0.0.1:1/x.git|SPAWN_DEPLOY_MIRROR is a URL that carries a credential" \
  "a password in an ssh URL|ssh://git:$secret@127.0.0.1:1/x.git|SPAWN_DEPLOY_MIRROR is a URL that carries a credential" \
  "a password behind a transport prefix|hang::https://user:$secret@127.0.0.1:1/x.git|SPAWN_DEPLOY_MIRROR is a URL that carries a credential" \
  "a remote whose URL carries a token|leaky|the mirror leaky (SPAWN_DEPLOY_MIRROR) is pushed to a URL that carries a credential" \
  "a remote whose pushurl carries a password|leaky-push|the mirror leaky-push (SPAWN_DEPLOY_MIRROR) is pushed to a URL that carries a credential" \
  "a URL that url.*.insteadOf rewrites to one with a token|https://127.0.0.1:1/x.git|the mirror https://127.0.0.1:1/x.git (SPAWN_DEPLOY_MIRROR) is pushed to a URL that carries a credential|url.https://$secret@127.0.0.1:1/.insteadOf=https://127.0.0.1:1/"; do
  IFS='|' read -r what value words config <<<"$refused"
  [ -z "$config" ] || git --git-dir="$missions" config "${config%%=*}" "${config#*=}"
  : >"$SYSTEMCTL_LOG"
  : >"$GIT_ARGV_LOG"
  deploy SPAWN_DEPLOY_MIRROR="$value"
  [ -z "$config" ] || git --git-dir="$missions" config --unset "${config%%=*}"
  check "$what: refused (status $status)" [ "$status" -ne 0 ]
  check "$what: says why, and where the credential goes" \
    bash -c 'grep -F -- "$2" "$1" | grep -q "credential helper (gh auth setup-git) or use a deploy key over ssh"' _ "$out" "$words"
  check "$what: the credential is not in the output" bash -c '! grep -qF -- "$2" "$1"' _ "$out" "$secret"
  check "$what: nor on any git command line (of $(wc -l <"$GIT_ARGV_LOG"))" \
    bash -c '[ -s "$1" ] && ! grep -qF -- "$2" "$1"' _ "$GIT_ARGV_LOG" "$secret"
  check "$what: no push ran" bash -c '! grep -q "^push \| push " "$1"' _ "$GIT_ARGV_LOG"
  check "$what: before touching a service" [ ! -s "$SYSTEMCTL_LOG" ]
done
unset GIT_ARGV_LOG
git --git-dir="$missions" remote remove leaky
git --git-dir="$missions" remote remove leaky-push
check "before any backup" [ "$(ls "$live/backups" | wc -l)" -eq "$backups_before" ]
check "main did not move" [ "$(main)" = "$base" ]
check "nothing was pushed to the mirror" [ "$(mirror_main)" = "$base" ]

echo
if [ "$failures" -eq 0 ]; then echo "test-deploy: all passed"; else echo "test-deploy: $failures failed"; exit 1; fi
