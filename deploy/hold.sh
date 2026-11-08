#!/usr/bin/env bash
# Hold landings on the Spawn source for the owner gate.
#
#   deploy/hold.sh [<candidate>]   # <candidate>: what the gate judges, HEAD by default
#
# Run it before the owner gate, instead of stopping spawn-integrate. It records
# the owner's hold on the missions repo (`spawn mission hold`, objective 034,
# docs/fleet-architecture.md §10.3, "Landing: the owner's hold") with the
# reason "owner gate <candidate>": no task lands on that main while the gate
# judges against it, so the verdict's base stays the live main and
# deploy/deploy.sh accepts it. Everything else goes on: every mission's gates
# (an accepted Spawn task waits to land), objective assignment, the other
# sources.
#
# It prints the hold as the command does, `{hold, source, main, expires_ms,
# renews}`, then the gate command over that `main`. Gate against it, then run
# deploy/deploy.sh <verdict>: the deploy releases the hold on every exit path
# (once main moved forward and `spawn mission moved` recorded it, or on any
# failure), and the tasks that waited land at the integrator's next pass
# (judged again on the new main). Not deploying after all:
# `spawn mission release --source <the missions repo>`. A hold nobody
# releases expires by itself after SPAWN_SOURCE_HOLD_SECS (default 10800, 3 h:
# longer than the slowest owner gate plus a deploy), and the supervisor is
# told. Running it again renews the hold (a new bound, the main as it is then).
#
# The binary and the database are the live ones ($SPAWN_LIVE, default
# ~/.local/share/spawn-rsi: bin/spawn and spawn.env); the URL never reaches a
# command line.
set -euo pipefail

PROG=hold
live=${SPAWN_LIVE:-$HOME/.local/share/spawn-rsi}
missions=$live/missions/spawn.git
repo=$(git rev-parse --show-toplevel)
# shellcheck source=deploy/backup/lib.sh
. "$repo/deploy/backup/lib.sh"

[ $# -le 1 ] || die "usage: deploy/hold.sh [<candidate>] (see the header)"
candidate=${1:-$(git -C "$repo" rev-parse --short=12 HEAD)}
[ -n "$candidate" ] || die "no candidate: name the commit the owner gate judges"
[ -x "$live/bin/spawn" ] || die "no live binary $live/bin/spawn"
[ -f "$live/spawn.env" ] || die "no $live/spawn.env"

set -a
# shellcheck disable=SC1091
. "$live/spawn.env"
set +a

held=$("$live/bin/spawn" mission hold --source "$missions" --reason "owner gate $candidate") ||
  die "the hold on $missions was refused (above); nothing was held"
printf '%s\n' "$held"
main=$(printf '%s' "$held" | jq -er '.main') || die "the hold printed no main"
hold=$(printf '%s' "$held" | jq -r '.hold')
expires=$(printf '%s' "$held" | jq -r '.expires_ms')
log "held landings on $missions at main $main (hold $hold, expires at $(date -d "@$((expires / 1000))" '+%F %T %Z' 2>/dev/null || echo "${expires} ms"))"
log "gate $candidate against that main, then deploy/deploy.sh <verdict> releases the hold:"
cat <<EOF
  git fetch -q "$missions" main
  SPAWN_GATE_VERDICT=\$PWD/verdict.json sh scripts/rsi/gate.sh \\
    "\$(git rev-parse --absolute-git-dir)" $main $candidate <the RSI mission's scenarios dir>
EOF
