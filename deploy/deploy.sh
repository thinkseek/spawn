#!/bin/sh
# Deploy the checked-out commit to the live fleet on this host.
#
#   deploy/deploy.sh            # build, install, move missions main, restart
#
# Steps, each one stops the deploy on failure:
#   1. the tree is clean and HEAD descends from the missions repo's main (a
#      deploy never drops landed work);
#   2. a pg_dump of the live database into the backups directory;
#   3. a release build without the compiler cache (kache stays out of deploys);
#   4. the binary is installed next to the old one (spawn.prev), by rename;
#   5. the integrator is stopped, the schema migrated, main moves forward (never forced), and every
#      gated mission whose baseline was the old main takes the new one, so the
#      deploy is not mistaken for a push outside the gate;
#   6. the services restart.
set -eu

live=${SPAWN_LIVE:-$HOME/.local/share/spawn-rsi}
missions=$live/missions/spawn.git
repo=$(git rev-parse --show-toplevel)
cd "$repo"

say() { printf 'deploy: %s\n' "$*"; }
die() { printf 'deploy: %s\n' "$*" >&2; exit 1; }

[ -z "$(git status --porcelain --untracked-files=no)" ] || die "the tree has uncommitted changes"
head=$(git rev-parse HEAD)
old=$(git --git-dir="$missions" rev-parse main)
git cat-file -e "$old^{commit}" 2>/dev/null || git fetch -q "$missions" main
git merge-base --is-ancestor "$old" "$head" ||
  die "HEAD does not contain the missions main $old: merge it first"

set -a
# shellcheck disable=SC1091
. "$live/spawn.env"
set +a

dump=$live/backups/spawn-$(date +%Y%m%d-%H%M%S).dump
pg_dump -Fc -f "$dump" "$SPAWN_DATABASE_URL"
say "backup $dump ($(stat -c %s "$dump") bytes)"

RUSTC_WRAPPER= cargo build --release -q
cp target/release/spawn "$live/bin/spawn.new"
cp "$live/bin/spawn" "$live/bin/spawn.prev"
mv "$live/bin/spawn.new" "$live/bin/spawn"
say "installed $("$live/bin/spawn" version 2>/dev/null || echo "$head")"

systemctl --user stop spawn-integrate
"$live/bin/spawn" init-db >/dev/null
if [ "$old" != "$head" ]; then
  git push -q "$missions" "$head:refs/heads/main"
  moved=$("$live/bin/spawn" mission moved --source "$missions" --from "$old" --to "$head")
  say "main $old -> $head; $(printf '%s' "$moved" | jq -r '.missions') mission(s) took it as baseline"
fi
systemctl --user restart spawn-loop spawn-integrate spawn-serve
say "restarted; live at $head"
