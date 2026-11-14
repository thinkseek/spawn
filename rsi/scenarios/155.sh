#!/bin/sh
# Black box: the scratch walk collects a large root within its bound
# (objective 155). A folder whose claim's lease runs is never walked whole
# (its build paths only; its bytes read null); an unmarked folder's walk
# stops at its first change in the last day; a root inside another root is
# walked as its own, each of its folders an entry with its own owner and
# age, never counted inside the root around it; the Core lines file's
# storage.scratch_bound sets how far a root's walk may go.
#
# Required:
#   (1) with "scratch_bound": {"entries": 900, "ms": 30000}, a root holding a
#       claimed folder whose lease runs (2000 files, a 3000-byte target/),
#       an unmarked folder changed now (2000 files) and an idle one (3
#       files, 2 days old), and an unmarked session folder sess (2 days old)
#       holding sess/o, claimed with a running lease, and sess/old-tree,
#       unclaimed (R2: rev-g's F2, the lead's (a)): storage.
#       unowned_scratch_bytes is collected and is the idle folder's bytes
#       plus sess/old-tree's; scratch.unowned is idle and sess/old-tree, each
#       its own entry (sess is none: one claim hides none of its siblings);
#       scratch.claimed lists the leased folder (bytes null, build_bytes 3000)
#       and sess/o (bytes null, build_bytes 0).
#   (2) roots [S, S/ccd], with exclude [S/ccd] and again with no exclude
#       (R2, rev-g's F1): S/ccd's idle folders x and y are each listed once,
#       as their own entries, beside S's idle folder; S/ccd itself is never an
#       entry; the figure is their sum.
#   (3) "scratch_bound": {"entries": 5, "ms": 30000}: the walk runs over it,
#       the figure is not collected (null) at a first reading, and serve's
#       log says "ran over its bound (5 entries".
#   (4) the card's words for the leased folder (web/src/coreLines.ts):
#       "build <size> (not walked whole while leased) · <age> old · lease
#       until <time> · …".
#   (5) read only: nothing under the scratch changed.
#   (6) an exclude that holds a root (R2, the lead's (b)): roots [N/ccd],
#       exclude [N]: the figure is not collected (null) and scratch.refused
#       names N/ccd, "exclude N hides root N/ccd: a root under an exclude is
#       never read"; never a silent nothing.
#
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure.

set -eu

asserted=""
harness() {
    echo "155: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
# The tree under test: the gate runs a scenario from the candidate's tree,
# and $controller is the deployed controller's copy, which lacks the
# candidate's web/src until it lands.
tree=$(pwd)
[ -f "$tree/web/package.json" ] && [ -d "$tree/web/src" ] ||
    harness "run from the tree under test: no web/src under $tree"
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od curl sed grep sha1sum cut node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_155_walk_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL
# A scratch HOME and Claude Code projects dir: nothing here reads or writes
# the real ones.
HOME=$WORK/home
SPAWN_CLAUDE_PROJECTS=$WORK/home/projects
export HOME SPAWN_CLAUDE_PROJECTS
mkdir -p "$SPAWN_CLAUDE_PROJECTS" || harness "mkdir $SPAWN_CLAUDE_PROJECTS failed"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
# The runtime directory too (backlog S20): what Spawn keeps there (a
# launch's environment file, its terminals' leaders) stays in WORK, never
# in the real one. systemd's user manager stays reachable by its bus.
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
serve_pid=""
cleanup() {
    for pid in "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "155: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "155: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

# Logins of this scenario's databases that a run killed before its cleanup
# left behind (scripts/blackbox/scenario-db.sh): this prefix only.
sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

# A failed check is reported and the run goes on; the run fails at its end
# if any did. Unless the database is gone: then nothing read can be trusted.
: >"$WORK/failures"
check_failed() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL SPAWN_FLEET_TEST_DATABASE_URL; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_WORKSPACE_ROOT=$WORK/ws
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT
mkdir -p "$SPAWN_WORKSPACE_ROOT"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

note() { echo "155: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

S=$(cd "$WORK" && mkdir -p scratch && cd scratch && pwd -P)
fill() { # <file> <bytes>
    mkdir -p "$(dirname "$1")"
    head -c "$2" /dev/zero >"$1" || harness "cannot write $1"
}
size_of() { find "$1" -type f -printf '%s\n' | awk '{ s += $1 } END { print s + 0 }'; }
many() { # <dir> <count>: count one-byte files
    mkdir -p "$1" || harness "mkdir $1"
    i=0
    while [ "$i" -lt "$2" ]; do
        printf x >"$1/f$i" || harness "cannot write $1/f$i"
        i=$((i + 1))
    done
}

# --- the scratch: one root for (1), (3) and (4); a nested root for (2) --------------------------
many "$S/held/data" 2000
fill "$S/held/t/target/x" 3000
capture "$SPAWN_BIN" scratch claim "$S/held" --agent rev-e --objective 1/155 --build t/target
[ "$status" -eq 0 ] || harness "claiming held: $(err)"
many "$S/fresh/data" 2000
fill "$S/idle/a" 100
fill "$S/idle/b" 200
fill "$S/idle/c" 300
# A session folder, unmarked and unclaimed, holding a running claim one level
# down: someone's, never "nobody owns" with that claim's bytes inside.
fill "$S/sess/o/z" 500
capture "$SPAWN_BIN" scratch claim "$S/sess/o" --agent rev-e --objective 1/155
[ "$status" -eq 0 ] || harness "claiming sess/o: $(err)"
# Its sibling, unclaimed and old: nobody's, whatever sess/o's claim says.
fill "$S/sess/old-tree/f" 1000
N=$WORK/nested
N=$(mkdir -p "$N/ccd" && cd "$N" && pwd -P)
fill "$N/top/z" 50
fill "$N/ccd/x/f" 7
fill "$N/ccd/y/f" 11
# Everything old but fresh's own files (a session writing now) and the claim.
find "$S/held" "$S/idle" "$S/sess" "$S/fresh/data" -exec touch -h -d '2 days ago' {} + || harness "cannot age"
touch -h "$S/fresh/data/f0" || harness "cannot touch fresh"
find "$N" -exec touch -h -d '2 days ago' {} + || harness "cannot age the nested root"

snapshot() { find "$S" "$N" -printf '%p %y %s %T@ %l\n' | sort; }
snapshot >"$WORK/before.txt"

web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
lines() { # <storage json>: the Core lines file
    printf '{"ranges": {}, "storage": %s}\n' "$1" >"$SPAWN_WORKSPACE_ROOT/core-lines.json.new" &&
        mv "$SPAWN_WORKSPACE_ROOT/core-lines.json.new" "$SPAWN_WORKSPACE_ROOT/core-lines.json" ||
        harness "cannot write core-lines.json"
}
start_serve() {
    # Emptied first: the redirect below truncates it only inside the new
    # process, so a read right after could still see the last serve's port.
    : >"$WORK/serve.log" || harness "cannot empty serve.log"
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" >"$WORK/serve.log" 2>&1 &
    serve_pid=$!
    port=""
    i=0
    while [ "$i" -lt 100 ]; do
        i=$((i + 1))
        port=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$WORK/serve.log" 2>/dev/null | head -n 1)
        [ -n "$port" ] && break
        sleep 0.1
    done
    [ -n "$port" ] || harness "serve did not start: $(head -c 400 "$WORK/serve.log")"
}
stop_serve() {
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
}
ask() { # the answer into $WORK/api.json
    asked=$(date +%s%3N)
    code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/core-lines answered $code: $(head -c 300 "$WORK/api.json")"
}
fig() { jq -c ".lines[] | select(.line == \"storage\") | ([.north_star] + .guardrails)[] | select(.key == \"$1\") | .value" "$WORK/api.json"; }

# --- (1) a large root within a tight bound ---------------------------------------------------------
lines "{\"scratch_roots\": [\"$S\"], \"scratch_bound\": {\"entries\": 900, \"ms\": 30000}}"
start_serve
ask
expect_eq "$(fig storage.unowned_scratch_bytes)" "$(($(size_of "$S/idle") + $(size_of "$S/sess/old-tree")))" \
    "(1) scratch nobody owns is collected within 900 entries: idle and sess/old-tree count (held and sess/o are leased, fresh changed now)"
expect_eq "$(jq -c '[.scratch.claimed[] | {path, bytes, build_bytes}] | sort_by(.path)' "$WORK/api.json")" \
    "[{\"path\":\"$S/held\",\"bytes\":null,\"build_bytes\":3000},{\"path\":\"$S/sess/o\",\"bytes\":null,\"build_bytes\":0}]" \
    "(1) the leased folders: never walked whole (bytes null), held's build path counted"
expect_eq "$(jq -c '[.scratch.unowned[] | .path] | sort' "$WORK/api.json")" "[\"$S/idle\",\"$S/sess/old-tree\"]" \
    "(1) the entries nobody owns: idle, and sess/old-tree as its own entry (sess, holding a running claim, is none)"

# --- (4) the card's words --------------------------------------------------------------------------
cat >"$WORK/view.test.mjs" <<'NODE'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
const { coreLinesView } = await import(`${process.env.BB_TREE}/web/src/coreLines.ts`);
test("the leased folder in the card's words", () => {
  const answer = JSON.parse(readFileSync(`${process.env.BB_WORK}/api.json`, "utf8"));
  const view = coreLinesView(answer, Number(process.env.BB_ASKED));
  const held = view.scratch.filter((s) => s.detail === process.env.BB_HELD);
  assert.equal(held.length, 1);
  assert.match(held[0].text, /^build \d+\.\d MB \(not walked whole while leased\) · .+ old · lease until \d\d-\d\d \d\d:\d\d · /);
});
NODE
if ! BB_TREE=$tree BB_HELD=$S/held BB_WORK=$WORK BB_ASKED=$asked node --test "$WORK/view.test.mjs" >"$WORK/view.out" 2>&1; then
    check_failed "(4) the card's words for the leased folder: $(grep -m 8 -E 'Error|expected|actual|\+ |- |^not ok' "$WORK/view.out" | tr '\n' ' ' | head -c 700)"
fi
stop_serve

# --- (2) a root inside a root, with the exclude and without it ---------------------------------------
for exclude in "[\"$N/ccd\"]" "[]"; do
    lines "{\"scratch_roots\": [\"$N\", \"$N/ccd\"], \"scratch_exclude\": $exclude}"
    start_serve
    ask
    expect_eq "$(jq -c '[.scratch.unowned[] | {path, bytes}] | sort_by(.path)' "$WORK/api.json")" \
        "[{\"path\":\"$N/ccd/x\",\"bytes\":7},{\"path\":\"$N/ccd/y\",\"bytes\":11},{\"path\":\"$N/top\",\"bytes\":50}]" \
        "(2) exclude $exclude: each folder of the inner root is its own entry, once; the inner root is no entry of the outer"
    expect_eq "$(fig storage.unowned_scratch_bytes)" 68 "(2) exclude $exclude: the figure counts each once"
    stop_serve
done

# --- (3) the bound is the file's ----------------------------------------------------------------------
lines "{\"scratch_roots\": [\"$S\"], \"scratch_bound\": {\"entries\": 5, \"ms\": 30000}}"
start_serve
ask
expect_eq "$(fig storage.unowned_scratch_bytes)" null "(3) over a 5-entry bound (the root needs about 7 even with 155): not collected"
i=0
while [ "$i" -lt 50 ] && ! grep -q 'ran over its bound (5 entries' "$WORK/serve.log"; do
    i=$((i + 1))
    sleep 0.1
done
grep -q 'ran over its bound (5 entries' "$WORK/serve.log" ||
    check_failed "(3) serve's log does not say the walk ran over its 5-entry bound: $(grep -m 3 'core-lines' "$WORK/serve.log" | tr '\n' ' ' | head -c 400)"
stop_serve

# --- (6) an exclude that holds a root is refused, and said ---------------------------------------------
lines "{\"scratch_roots\": [\"$N/ccd\"], \"scratch_exclude\": [\"$N\"]}"
start_serve
ask
expect_eq "$(fig storage.unowned_scratch_bytes)" null "(6) a root under an exclude: not collected"
expect_eq "$(jq -c '.scratch.refused' "$WORK/api.json")" \
    "[{\"root\":\"$N/ccd\",\"why\":\"exclude $N hides root $N/ccd: a root under an exclude is never read\"}]" \
    "(6) the answer names the root and the exclude that hides it"
stop_serve

# --- (5) read only -------------------------------------------------------------------------------------
snapshot >"$WORK/after.txt"
cmp -s "$WORK/before.txt" "$WORK/after.txt" ||
    check_failed "(5) the scratch changed: $(diff "$WORK/before.txt" "$WORK/after.txt" | head -n 6 | tr '\n' ' ' | head -c 400)"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "155: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "155: the scratch walk collects a large root within its bound; a session folder hides none of its children; a nested root counts once; the bound is the file's; an exclude above a root is said"
exit 0
