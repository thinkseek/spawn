#!/bin/sh
# Black box: the build output of landed objectives goes (objective 145).
# `spawn scratch sweep [--apply]` reads the claimed folders (objective 140's
# owner files) under the Core lines file's scratch roots. A build path an
# owner file names goes only when its claim's lease has ended, its
# objective has a task.landed in the store, no component is a symlink, it
# holds a CACHEDIR.TAG, nothing under it is a transcript (*.jsonl), and no
# process uses it (cwd, an open file). Nothing else ever goes.
#
# Required:
#   (1) print only (no --apply): one line per build path, "would-remove
#       <path> <bytes> <agent> <mission>/<objective>" or "stays <path>
#       <agent> <mission>/<objective>: <why>", then the total; nothing under
#       the scratch root changes. The list ends with its digest. An unreadable process the in-use proof
#       excuses (the host's "(sd-pam)", started before the claim) is named
#       on an "excused" line; this scenario does not depend on there being one.
#   (2) without the store's landings (a database that is not there) the
#       sweep is refused and nothing changes.
#   (3) the list read with this host's unreadable processes excused
#       would remove two build paths; --apply with its digest removes
#       exactly those two landed, released, idle, tagged build paths (one
#       at a root's top level, one a level below), prints each as
#       "removed", and nothing else changes: not their folders' sources or
#       owner files, a running lease, an objective not landed, a build path
#       a process has as its cwd or holds a file in, one through a symlink
#       (nor what the symlink points at), one with no CACHEDIR.TAG, one
#       in a folder holding a .jsonl (the walk never reads such a folder,
#       so it is not listed), a target no owner file names, or an unclaimed
#       folder, or a claimed folder one level down whose name the walk
#       never counts (incident-*).
#   (4) --apply needs --expect-digest, the digest the print-only list
#       ends with; a digest not of the list now is refused, nothing removed.
#       --excuse <pid>:<start ticks> is taken only for an unreadable
#       process of this user running now; else refused, nothing removed.
#       --apply prints its excused set first.
#
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure.

set -eu

asserted=""
harness() {
    echo "145: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od sed grep sha1sum sha256sum cut sort comm; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_145_sweep_$run"
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
held=""
cleanup() {
    for pid in $held; do
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
    if [ "$rc" -ne 0 ] && [ -s "$WORK/sweep.out" ]; then
        echo "145: the last sweep output (first 60 lines):" >&2
        head -n 60 "$WORK/sweep.out" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "145: harness: unexpected exit $rc" >&2
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

note() { echo "145: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

seed_now=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]" ;; esac
A=$(printf 'bb-145-a' | sha1sum | cut -c1-40)
B=$(printf 'bb-145-b' | sha1sum | cut -c1-40)
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
begin;
create temp sequence seed_seq start 100000;
create function pg_temp.rec(at bigint, r jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), at) \$\$;
create function pg_temp.objective(at bigint, task int, objective int) returns void language sql as \$\$
    select pg_temp.rec(at, jsonb_build_object('type', 'task.assigned', 'id', task,
        'mission_id', 1, 'kind', 'ephemeral', 'created_by', 'resident',
        'agent_id', 'agent_worker_' || task, 'session_id', 'session_worker_' || task,
        'objective', objective, 'brief', 'Objective ' || objective || '.')) \$\$;
create function pg_temp.landed(at bigint, task int) returns void language sql as \$\$
    select pg_temp.rec(at, jsonb_build_object('type', 'task.landed', 'id', task,
        'from', '$A', 'to', '$B')) \$\$;
select pg_temp.rec($seed_now - 7200000, '{"type":"mission.added","id":1,"text":"145 mission","origin":"human","sources":[]}');
select pg_temp.objective($seed_now - 7200000, 1, 7);
select pg_temp.landed($seed_now - 3600000, 1);
select pg_temp.objective($seed_now - 7200000, 2, 8);
select pg_temp.objective($seed_now - 7200000, 3, 9);
select pg_temp.landed($seed_now - 3600000, 3);
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(scalar "select string_agg(id || ':' || mission_id || '/' || objective || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:1/7:landed 2:1/8:assigned 3:1/9:landed" ] || harness "the seed folded to tasks [$tasks]"

S=$(cd "$WORK" && mkdir -p scratch && cd scratch && pwd -P)
fill() { # <file> <bytes>
    mkdir -p "$(dirname "$1")"
    head -c "$2" /dev/zero >"$1" || harness "cannot write $1"
}
tag() { # <dir>
    mkdir -p "$1"
    printf 'Signature: 8a477f597d28d172789f06886806bc55\n# made by 145.sh\n' >"$1/CACHEDIR.TAG" || harness "cannot tag $1"
}
size_of() { find "$1" -type f -printf '%s\n' | awk '{ s += $1 } END { print s + 0 }'; }
scratch() { capture "$SPAWN_BIN" scratch "$@"; }
claimed() { # <dir> <objective> <build>... : claimed by rev-e, then released
    dir=$1
    objective=$2
    shift 2
    set -- claim "$dir" --agent rev-e --objective "$objective" $(for b in "$@"; do printf -- '--build %s ' "$b"; done)
    scratch "$@"
    [ "$status" -eq 0 ] || harness "claiming $dir: $(err)"
    scratch release "$dir"
    [ "$status" -eq 0 ] || harness "releasing $dir: $(err)"
}

# Each folder breaks one rule, but go and sess/deep, which break none.
for f in go lease notl cwd fd link notag jsonl sess/deep sess/incident-9; do
    fill "$S/$f/src.rs" 100
done
for f in go/t lease notl cwd fd notag/x jsonl sess/deep sess/incident-9 go/other; do
    fill "$S/$f/target/debug/x" 3000
done
for f in go/t lease notl cwd fd jsonl sess/deep sess/incident-9 go/other; do
    tag "$S/$f/target"
done
fill "$S/jsonl/target/debug/session.jsonl" 10
fill "$S/unclaimed/target/debug/x" 3000
tag "$S/unclaimed/target"
fill "$WORK/outside/target/secret" 9000
tag "$WORK/outside/target"

claimed "$S/go" 1/7 t/target
claimed "$S/notl" 1/8 target
claimed "$S/cwd" 1/7 target
claimed "$S/fd" 1/7 target
claimed "$S/link" 1/7 x/target
claimed "$S/notag" 1/7 x/target
claimed "$S/jsonl" 1/7 target
claimed "$S/sess/deep" 1/9 target
# A name the walk never counts, one level down: never swept (rev-g's N3 on 140).
claimed "$S/sess/incident-9" 1/9 target
scratch claim "$S/lease" --agent rev-e --objective 1/7 --build target
[ "$status" -eq 0 ] || harness "claiming $S/lease: $(err)"
# After its claim, link's x is a symlink to a tagged target outside.
ln -s "$WORK/outside" "$S/link/x" || harness "ln -s failed"
(cd "$S/cwd/target/debug" && exec sleep 600) &
cwd_pid=$!
held="$held $cwd_pid"
sh -c 'exec 3<"$1"; exec sleep 600' sh "$S/fd/target/debug/x" &
fd_pid=$!
held="$held $fd_pid"
i=0
until [ "$(readlink "/proc/$cwd_pid/cwd" 2>/dev/null)" = "$S/cwd/target/debug" ] &&
    [ "$(readlink "/proc/$fd_pid/fd/3" 2>/dev/null)" = "$S/fd/target/debug/x" ]; do
    i=$((i + 1))
    [ "$i" -lt 100 ] || harness "the holding processes did not start"
    sleep 0.1
done

cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<JSON
{"ranges": {}, "storage": {"scratch_roots": ["$S"]}}
JSON
snapshot() { find "$S" "$WORK/outside" -printf '%p %y %s %T@ %l\n' | sort; }
snapshot >"$WORK/before.txt"
# A row's identities (cc-astra's A64): "<dev>:<ino>" of the folder and of
# each component down to the build path, comma-separated; and the sha256 of
# the claim as its owner file holds it (the line, without its newline).
ids_of() { # ids_of <folder> <component>...
    io_at=$1
    io_out=$(stat -c '%d:%i' "$io_at") || harness "stat $io_at"
    shift
    for io_part in "$@"; do
        io_at=$io_at/$io_part
        io_out=$io_out,$(stat -c '%d:%i' "$io_at") || harness "stat $io_at"
    done
    printf '%s' "$io_out"
}
claim_sha() { head -n 1 "$1/.spawn-owner" | tr -d '\n' | sha256sum | cut -d' ' -f1; }
go_bytes=$(size_of "$S/go/t/target")
deep_bytes=$(size_of "$S/sess/deep/target")
cat >"$WORK/stays.txt" <<TXT
stays	$S/cwd/target	rev-e 1/007: in use: process $cwd_pid's cwd
stays	$S/fd/target	rev-e 1/007: in use: process $fd_pid's open file
stays	$S/lease/target	rev-e 1/007: lease runs
stays	$S/link/x/target	rev-e 1/007: $S/link/x is a symlink
stays	$S/notag/x/target	rev-e 1/007: no CACHEDIR.TAG
stays	$S/notl/target	rev-e 1/008: not landed
TXT

# --- (1) print only -------------------------------------------------------------------------------
# This host's unreadable processes of this user, if any, keep every path
# with no definite use ("in use unknown") until --apply excuses them.
"$SPAWN_BIN" scratch sweep >"$WORK/sweep.out" 2>"$WORK/sweep.err" && status=0 || status=$?
expect_eq "$status" 0 "(1) the print-only sweep exits 0 ($(head -c 300 "$WORK/sweep.err"))"
unknown='in use unknown: process [0-9]+ \([^)]*\) of its owner cannot be read'
if grep -q '^unreadable	' "$WORK/sweep.out"; then
    note "unreadable processes here: $(grep -c '^unreadable	' "$WORK/sweep.out")"
    {
        printf 'stays\t%s\trev-e 1/007: UNKNOWN\n' "$S/go/t/target"
        printf 'stays\t%s\trev-e 1/009: UNKNOWN\n' "$S/sess/deep/target"
        cat "$WORK/stays.txt"
    } | sort >"$WORK/want1.txt"
    want_total="would remove (print only; nothing removed) 0 build path(s), 0 bytes"
else
    {
        printf 'would-remove\t%s\t%s\trev-e 1/007\tids %s\tclaim %s\n' "$S/go/t/target" "$go_bytes" \
            "$(ids_of "$S/go" t target)" "$(claim_sha "$S/go")"
        printf 'would-remove\t%s\t%s\trev-e 1/009\tids %s\tclaim %s\n' "$S/sess/deep/target" "$deep_bytes" \
            "$(ids_of "$S/sess/deep" target)" "$(claim_sha "$S/sess/deep")"
        cat "$WORK/stays.txt"
    } | sort >"$WORK/want1.txt"
    want_total="would remove (print only; nothing removed) 2 build path(s), $((go_bytes + deep_bytes)) bytes"
fi
grep -v -e '^would remove ' -e '^excused	' -e '^unreadable	' -e '^digest	' "$WORK/sweep.out" | sed -E "s/$unknown\$/UNKNOWN/" | sort >"$WORK/got1.txt"
bad=$(grep '^unreadable	' "$WORK/sweep.out" | grep -v -E '^unreadable	[0-9]+:[0-9]+	' || true)
[ -z "$bad" ] || check_failed "(1) an unreadable line is not <pid>:<start ticks>: $bad"
cmp -s "$WORK/want1.txt" "$WORK/got1.txt" ||
    check_failed "(1) the print-only list: $(diff "$WORK/want1.txt" "$WORK/got1.txt" | grep '^[<>]' | head -n 8 | tr '\n' ' ' | head -c 900)"
expect_eq "$(grep '^would remove ' "$WORK/sweep.out")" "$want_total" "(1) the print-only total"
tail -n 1 "$WORK/sweep.out" | grep -q -E '^digest	[0-9a-f]{64}$' ||
    check_failed "(1) the print-only list does not end with its digest: $(tail -n 1 "$WORK/sweep.out")"
# The host's unreadable login helpers, each named: (sd-pam) only.
bad=$(grep '^excused	' "$WORK/sweep.out" | grep -v -E '	process [0-9]+ \(sd-pam\): cannot be read, started before the claim$' || true)
[ -z "$bad" ] || check_failed "(1) an excused process that is not (sd-pam): $bad"
snapshot >"$WORK/after1.txt"
cmp -s "$WORK/before.txt" "$WORK/after1.txt" ||
    check_failed "(1) print only changed the scratch: $(diff "$WORK/before.txt" "$WORK/after1.txt" | head -n 6 | tr '\n' ' ' | head -c 400)"

# --- (2) no landings, no sweep ---------------------------------------------------------------------
# The digest an empty list has: a sweep that went on without the landings
# would find nothing landed, match it, and run.
empty=$(printf '' | sha256sum | cut -d' ' -f1)
SPAWN_DATABASE_URL="$server_url/${SPAWN_DB_NAME}_not_there" "$SPAWN_BIN" scratch sweep --apply --expect-digest "$empty" >"$WORK/sweep2.out" 2>&1 && status=0 || status=$?
[ "$status" -ne 0 ] || check_failed "(2) a sweep without the store's landings was taken: $(head -c 300 "$WORK/sweep2.out")"
if grep -q 'postgres://' "$WORK/sweep2.out"; then
    check_failed "(2) the refusal prints a database URL"
elif ! grep -q 'nothing removed' "$WORK/sweep2.out"; then
    check_failed "(2) the refusal does not say nothing was removed: $(head -c 300 "$WORK/sweep2.out")"
fi
snapshot >"$WORK/after2.txt"
cmp -s "$WORK/before.txt" "$WORK/after2.txt" ||
    check_failed "(2) a refused sweep changed the scratch: $(diff "$WORK/before.txt" "$WORK/after2.txt" | head -n 6 | tr '\n' ' ' | head -c 400)"

# --- (4) --apply only with the digest of the list read; excuses only of running unreadable processes ----
"$SPAWN_BIN" scratch sweep --apply >"$WORK/sweep4.out" 2>&1 && status=0 || status=$?
[ "$status" -ne 0 ] && grep -q 'go together' "$WORK/sweep4.out" ||
    check_failed "(4) --apply without --expect-digest was not refused: $(head -c 300 "$WORK/sweep4.out")"
"$SPAWN_BIN" scratch sweep --apply --expect-digest 0000 >"$WORK/sweep4.out" 2>&1 && status=0 || status=$?
[ "$status" -ne 0 ] && grep -q 'is not the one read.*nothing removed' "$WORK/sweep4.out" ||
    check_failed "(4) --apply with a digest not of this list was not refused: $(head -c 300 "$WORK/sweep4.out")"
"$SPAWN_BIN" scratch sweep --excuse "$$:1" >"$WORK/sweep4.out" 2>&1 && status=0 || status=$?
[ "$status" -ne 0 ] && grep -q 'nothing removed' "$WORK/sweep4.out" ||
    check_failed "(4) an excuse that names no unreadable process was not refused: $(head -c 300 "$WORK/sweep4.out")"
snapshot >"$WORK/after4.txt"
cmp -s "$WORK/before.txt" "$WORK/after4.txt" ||
    check_failed "(4) a refused sweep changed the scratch: $(diff "$WORK/before.txt" "$WORK/after4.txt" | head -n 6 | tr '\n' ' ' | head -c 400)"

# --- (3) the reviewed list, then --apply on exactly it ------------------------------------------------
# Another session's process of this user can start, unreadable, at any time
# (a gate's sandboxed spawn): the reviewed list then keeps the paths, or
# --apply finds the list changed and removes nothing; one that was excused
# can end before --apply, which then refuses its excuse. Each is the sweep
# being right; the round is tried again, three times at most, and only a
# round that saw no new unreadable process is judged.
round=0
while :; do
    round=$((round + 1))
    "$SPAWN_BIN" scratch sweep >"$WORK/list.out" 2>&1 || check_failed "(3) the list before --apply failed: $(head -c 300 "$WORK/list.out")"
    grep '^unreadable	' "$WORK/list.out" | cut -f2 | sort >"$WORK/pairs.txt"
    set --
    while read -r pair; do set -- "$@" --excuse "$pair"; done <"$WORK/pairs.txt"
    # The list a reviewer reads, with this host's unreadable processes
    # excused: it would remove exactly the two build paths.
    "$SPAWN_BIN" scratch sweep "$@" >"$WORK/reviewed.out" 2>&1 || check_failed "(3) the reviewed list failed: $(head -c 300 "$WORK/reviewed.out")"
    if grep -q '^unreadable	' "$WORK/reviewed.out" && [ "$round" -lt 3 ]; then
        note "(3) round $round: an unreadable process started since the list; again"
        continue
    fi
    reviewed=$(sed -n 's/^digest	//p' "$WORK/reviewed.out")
    "$SPAWN_BIN" scratch sweep --apply "$@" --expect-digest "$reviewed" >"$WORK/sweep.out" 2>"$WORK/sweep.err" && status=0 || status=$?
    if [ "$status" -ne 0 ] && grep -q 'is not the one read' "$WORK/sweep.err" && [ "$round" -lt 3 ]; then
        note "(3) round $round: the list changed before --apply (nothing removed); again"
        continue
    fi
    if [ "$status" -ne 0 ] && grep -q 'is no unreadable process of yours running now; nothing removed' "$WORK/sweep.err" && [ "$round" -lt 3 ]; then
        note "(3) round $round: an excused process ended before --apply (nothing removed); again"
        continue
    fi
    break
done
expect_eq "$(grep '^would-remove	' "$WORK/reviewed.out" | cut -f2 | sort | tr '\n' ' ')" "$S/go/t/target $S/sess/deep/target " \
    "(3) the reviewed list would remove the two build paths"
grep '^excuse	' "$WORK/sweep.out" | cut -f2 | sort >"$WORK/excused.txt"
cmp -s "$WORK/pairs.txt" "$WORK/excused.txt" ||
    check_failed "(3) the run does not print its excused set first: $(diff "$WORK/pairs.txt" "$WORK/excused.txt" | head -n 4 | tr '\n' ' ')"
expect_eq "$status" 0 "(3) the sweep exits 0 ($(head -c 300 "$WORK/sweep.err"))"
{
    printf 'removed\t%s\t%s\trev-e 1/007\n' "$S/go/t/target" "$go_bytes"
    printf 'removed\t%s\t%s\trev-e 1/009\n' "$S/sess/deep/target" "$deep_bytes"
    cat "$WORK/stays.txt"
} | sort >"$WORK/want3.txt"
grep -v -e '^removed [0-9]' -e '^excused	' -e '^excuse	' "$WORK/sweep.out" | sort >"$WORK/got3.txt"
cmp -s "$WORK/want3.txt" "$WORK/got3.txt" ||
    check_failed "(3) the sweep's list: $(diff "$WORK/want3.txt" "$WORK/got3.txt" | grep '^[<>]' | head -n 8 | tr '\n' ' ' | head -c 900)"
expect_eq "$(tail -n 1 "$WORK/sweep.out")" "removed 2 build path(s), $((go_bytes + deep_bytes)) bytes" "(3) the total"
snapshot >"$WORK/after3.txt"
# The two build paths go; their parents' size and time change with them.
grep -v -e "^$S/go/t/target[ /]" -e "^$S/sess/deep/target[ /]" -e "^$S/go/t d " -e "^$S/sess/deep d " "$WORK/before.txt" >"$WORK/want-after3.txt"
grep -v -e "^$S/go/t d " -e "^$S/sess/deep d " "$WORK/after3.txt" >"$WORK/after3-cut.txt"
mv "$WORK/after3-cut.txt" "$WORK/after3.txt" || harness "mv failed"
cmp -s "$WORK/want-after3.txt" "$WORK/after3.txt" ||
    check_failed "(3) the scratch after the sweep, beyond the two build paths: $(diff "$WORK/want-after3.txt" "$WORK/after3.txt" | head -n 8 | tr '\n' ' ' | head -c 600)"
[ ! -e "$S/go/t/target" ] && [ ! -e "$S/sess/deep/target" ] || check_failed "(3) a landed build path is still there"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "145: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "145: (1) to (4) checked"
