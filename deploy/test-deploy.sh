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
# real deploy target directory (~/.cache/spawn/deploy-target, incremental).
#
# Covered: the disk guard; the sandbox has no network and no $HOME; an
# untracked file in the checkout reaches neither the binary nor the UI; a
# deploy killed after its push leaves deploy.pending and the next deploy
# completes it; a deploy failing after its push completes it on exit; a
# pending deploy that main has moved past stops the deploy.
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

test_url_file=$HOME/.config/spawn/test-database.url
[ -f "$test_url_file" ] || die "no $test_url_file"
test_url=$(cat "$test_url_file")
case $test_url in *:15433/*) die "the test URL names port 15433, the live database" ;; esac
db=spawn_deploy_test_$$
live_url=${test_url%/*}/$db

scratch=$(mktemp -d "${TMPDIR:-/tmp}/spawn-deploy-test.XXXXXX")
cleanup() {
  with_db "$test_url" psql -XAtq -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" >/dev/null 2>&1 || true
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
echo "$*" >>"$SYSTEMCTL_LOG"
EOF
real_git=$(command -v git)
cat >"$scratch/fakebin/git" <<EOF
#!/bin/sh
# DEPLOY_TEST_AFTER_PUSH=kill: the deploy dies outright right after its push
# (no trap runs); =fail: the push "fails" after it landed.
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

# The checkout to deploy: HEAD, with untracked files that a build of the
# checkout itself would pick up (cargo reads .cargo/config.toml, vite copies
# public/).
checkout=$scratch/checkout
git clone -q --shared --no-checkout "$repo" "$checkout"
git -C "$checkout" checkout -q --detach "$(git -C "$repo" rev-parse HEAD)"
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
# deploy [ENV=VALUE...]: run deploy.sh in the checkout; output in $out, status in $status.
deploy() {
  out=$scratch/deploy.out
  set +e
  (cd "$checkout" && env SPAWN_LIVE="$live" TMPDIR="$scratch/tmp" PATH="$scratch/fakebin:$PATH" "$@" \
    bash deploy/deploy.sh "$verdict") >"$out" 2>&1
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
check "the UI is installed" [ -f "$live/web/index.html" ]

echo "== the next deploy completes it"
# Main is HEAD now: the operator runs the gate again, over it.
verdict_over "$head"
: >"$SYSTEMCTL_LOG"
deploy
check "succeeded (status $status)" [ "$status" -eq 0 ]
check "completed the pending deploy" grep -q "completed the pending deploy: main $base -> $head" "$out"
check "deploy.pending is gone" [ ! -e "$live/deploy.pending" ]
check "the integrator was stopped, started, and the services restarted" \
  bash -c 'grep -qx -- "--user stop spawn-integrate" "$1" && grep -qx -- "--user start spawn-integrate" "$1" &&
    grep -qx -- "--user restart spawn-loop spawn-integrate spawn-serve" "$1"' _ "$SYSTEMCTL_LOG"
check "the export is removed" bash -c '! compgen -G "$1/spawn-deploy-export-*" >/dev/null' _ "$scratch/tmp"

echo "== a deploy failing after its push completes it on exit"
git --git-dir="$missions" update-ref refs/heads/main "$base"
verdict_over "$base"
: >"$SYSTEMCTL_LOG"
deploy DEPLOY_TEST_AFTER_PUSH=fail
check "failed (status $status)" [ "$status" -ne 0 ]
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

echo
if [ "$failures" -eq 0 ]; then echo "test-deploy: all passed"; else echo "test-deploy: $failures failed"; exit 1; fi
