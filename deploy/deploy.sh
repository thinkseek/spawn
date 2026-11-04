#!/usr/bin/env bash
# Deploy the checked-out commit to the live fleet on this host.
#
#   deploy/deploy.sh <verdict-file>   # build, install, move missions main, restart
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
# Steps, each one stops the deploy on failure:
#   1. the verdict is accepted for HEAD over the current missions main, the
#      tree is clean and HEAD descends from that main (a deploy never drops
#      landed work);
#   2. a pg_dump of the live database into the backups directory (the
#      password reaches pg_dump through PGPASSWORD, never argv);
#   3. a release build without the compiler cache (kache stays out of deploys)
#      and the UI build (web/, served from $live/web);
#   4. the binary is installed next to the old one (spawn.prev), by rename;
#   5. the integrator is stopped, main is read again (it must still be the
#      verdict's base), the schema migrated, main moves forward (never
#      forced), and every gated mission whose baseline was the old main takes
#      the new one, so the deploy is not mistaken for a push outside the gate;
#   6. the services restart.
# Any failure after the integrator stopped restarts the services before the
# deploy exits (with whatever binary is installed by then).
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
repo=$(git rev-parse --show-toplevel)
cd "$repo"
# shellcheck source=deploy/backup/lib.sh
. "$repo/deploy/backup/lib.sh"

say() { printf 'deploy: %s\n' "$*"; }

[ $# -eq 1 ] || die "usage: deploy/deploy.sh <verdict-file> (see the header)"
verdict_file=$1
[ -f "$verdict_file" ] || die "no verdict file $verdict_file"
field() { jq -er --arg name "$1" '.[$name] // empty | strings' "$verdict_file"; }

[ -z "$(git status --porcelain --untracked-files=no)" ] || die "the tree has uncommitted changes"
head=$(git rev-parse HEAD)
old=$(git --git-dir="$missions" rev-parse main)
verdict=$(field verdict) || die "$verdict_file holds no verdict"
candidate=$(field candidate) || die "$verdict_file names no candidate: run the gate again"
base=$(field base) || die "$verdict_file names no base: run the gate again"
[ "$verdict" = accepted ] || die "the gate's verdict is $verdict, not accepted"
[ "$candidate" = "$head" ] || die "the verdict is for $candidate, HEAD is $head"
[ "$base" = "$old" ] || die "the verdict judged against $base, the missions main is $old: run the gate again"
git cat-file -e "$old^{commit}" 2>/dev/null || git fetch -q "$missions" main
git merge-base --is-ancestor "$old" "$head" ||
  die "HEAD does not contain the missions main $old: merge it first"

set -a
# shellcheck disable=SC1091
. "$live/spawn.env"
set +a

dump=$live/backups/spawn-$(date +%Y%m%d-%H%M%S).dump
with_db "$SPAWN_DATABASE_URL" pg_dump -Fc -f "$dump"
say "backup $dump ($(stat -c %s "$dump") bytes)"

RUSTC_WRAPPER= cargo build --release -q
# The UI is built from the same commit and served from the live directory,
# so edits in the checkout never reach the served UI before a deploy.
rm -rf "$live/web.new"
(cd web && ./node_modules/.bin/tsc --noEmit && ./node_modules/.bin/vite build --logLevel warn --outDir "$live/web.new" --emptyOutDir)
cp target/release/spawn "$live/bin/spawn.new"
cp "$live/bin/spawn" "$live/bin/spawn.prev"
mv "$live/bin/spawn.new" "$live/bin/spawn"
rm -rf "$live/web.prev"
[ ! -d "$live/web" ] || mv "$live/web" "$live/web.prev"
mv "$live/web.new" "$live/web"
say "installed $("$live/bin/spawn" version 2>/dev/null || echo "$head") and the UI"

restart() { systemctl --user restart spawn-loop spawn-integrate spawn-serve; }
# From here on, a failure must not leave the integrator stopped.
trap 'status=$?; if [ "$status" -ne 0 ]; then
  printf "deploy: failed (exit %s); restarting the services\n" "$status" >&2
  restart || printf "deploy: the restart failed too: systemctl --user status spawn-integrate\n" >&2
fi' EXIT
systemctl --user stop spawn-integrate
# The integrator may have landed between the check above and its stop.
[ "$(git --git-dir="$missions" rev-parse main)" = "$old" ] ||
  die "the missions main moved during the deploy: run the gate on the new main and deploy again"
"$live/bin/spawn" init-db >/dev/null
if [ "$old" != "$head" ]; then
  git push -q "$missions" "$head:refs/heads/main"
  moved=$("$live/bin/spawn" mission moved --source "$missions" --from "$old" --to "$head")
  say "main $old -> $head; $(printf '%s' "$moved" | jq -r '.missions') mission(s) took it as baseline"
fi
restart
trap - EXIT
say "restarted; live at $head"
