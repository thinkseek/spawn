#!/bin/sh
# Black box: `spawn chunks gc` collects unreferenced chunks (docs/storage.md).
#
# Real Bridges (`spawn tick`, a fake Claude engine) write 10 MiB files
# (deterministic, incompressible) through the chunk store: agents of the
# default repository (SPAWN_GIT_REPO) and one agent of a mission, whose branch
# lives in the mission's mirror under <root>/.sources/. The objects each
# checkpoint added are read from the store directory (<2 hex>/<64 hex>)
# before and after it; a branch is dropped with `git update-ref -d`; an
# object is aged with `touch`. Then:
#   (a) objects named by any reachable commit survive every collection: the
#       head of a branch, an older commit of the same branch (the file before
#       an in-place edit), and a branch that lives only in a mission mirror;
#       and they still materialize: a fork from the old commit, and resumes
#       of both agents, get the right bytes;
#   (b) unreferenced objects older than the grace period are removed;
#   (c) unreferenced objects younger than the grace period survive, and a
#       checkpoint that writes an object the store already holds makes it
#       young again (chunks are written before the commit that names them);
#   (d) collection is idempotent, and deletes nothing when it cannot read a
#       repository: it exits non-zero and names the repository;
#   (e) a manifest that a reachable commit names but that does not verify is
#       reported by name, never treated as unreferenced: nothing is deleted,
#       and the exit is non-zero.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (c) on a binary without `spawn chunks gc`. Runs inside the
# gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a private
# tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "005: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq openssl sha256sum dd od find sort comm cmp touch; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_005_gc_$run"
SPAWN_TMUX_SOCKET="bb-005-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# psql reaches the test server the way check-blackbox.sh does.
url=$SPAWN_TEST_DATABASE_URL
rest=${url#*://}
credentials=${rest%%@*}
address=${rest#*@}
address=${address%%/*}
PGUSER=${credentials%%:*}
PGPASSWORD=${credentials#*:}
case "$address" in
*:*) host=${address%:*} PGPORT=${address##*:} ;;
*) host=$address PGPORT=5432 ;;
esac
PGHOST=$(printf '%s' "$host" | sed 's/%2[Ff]/\//g')
export PGUSER PGPASSWORD PGHOST PGPORT

database_made=""
cleanup() {
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
    if [ -n "$database_made" ]; then
        psql -d postgres -tA -c "drop database if exists $SPAWN_DB_NAME with (force)" \
            >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "005: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

# An assertion exits 1, unless the database is gone: then nothing the
# scenario read can be trusted, and that is the harness's failure.
fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}

# --- the world ----------------------------------------------------------------

cat >"$WORK/gitconfig" <<'EOF'
[user]
	name = bb
	email = bb@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
EOF
GIT_CONFIG_GLOBAL=$WORK/gitconfig
export GIT_CONFIG_GLOBAL

default=$WORK/default.git
source=$WORK/source.git
root=$WORK/ws
store=$WORK/chunks
git init -q --bare --initial-branch=main "$default"
git init -q --bare --initial-branch=main "$source"
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git -C "$WORK/seed" push -q "$default" main:main
git -C "$WORK/seed" push -q "$source" main:main
mkdir -p "$root"

MIB=1048576
keystream() {
    head -c "$2" /dev/zero |
        openssl enc -aes-256-ctr -nosalt -K "$1" -iv 00000000000000000000000000000000
}
for n in 1 2 3 4; do
    keystream "$(printf '%064d' "$n")" $((10 * MIB)) >"$WORK/big$n"
    [ "$(wc -c <"$WORK/big$n" | tr -d ' ')" -eq $((10 * MIB)) ] ||
        harness "openssl did not produce 10 MiB"
done
keystream "$(printf '%064d' 9)" "$MIB" >"$WORK/patch"
cp "$WORK/big1" "$WORK/big1.edited"
dd if="$WORK/patch" of="$WORK/big1.edited" bs="$MIB" seek=3 conv=notrunc 2>/dev/null
sha() { sha256sum "$1" | cut -d' ' -f1; }
sha_1=$(sha "$WORK/big1")
sha_1e=$(sha "$WORK/big1.edited")
sha_2=$(sha "$WORK/big2")

# The fake engine: `write <name>` copies $WORK/<name> to data/big.bin, `edit`
# rewrites 1 MiB of it in place; both then exit (final checkpoint, unmount).
cat >"$WORK/claude" <<EOF
#!/bin/sh
printf 'Welcome to Claude Code\n\n'
printf '❯ '
while IFS= read -r line; do
  case "\$line" in
  write\ *)
    mkdir -p data || exit 3
    cp "$WORK/\${line#write }" data/big.bin || exit 3
    printf '%s\n' "\$line" >>'$WORK/engine.log'
    exit 0 ;;
  edit)
    dd if='$WORK/patch' of=data/big.bin bs=$MIB seek=3 conv=notrunc 2>/dev/null || exit 3
    printf 'edit\n' >>'$WORK/engine.log'
    exit 0 ;;
  *)
    printf '\n%s\n❯ ' "echo: \$line" ;;
  esac
done
EOF
chmod 755 "$WORK/claude"

cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_EXEC_IMAGE SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL \
    SPAWN_SUPERVISED SPAWN_PUSH_SOURCES CLAUDE_CONFIG_DIR; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="${url%/*}/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$default
SPAWN_WORKSPACE_ROOT=$root
SPAWN_CHUNK_STORE=$store
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CHUNK_STORE \
    SPAWN_CLAUDE_PROGRAM SPAWN_CLAUDE_PROJECTS SPAWN_TMUX_PROGRAM \
    SPAWN_WORKER

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- helpers ----------------------------------------------------------------

now() { date +%s; }
q() { scalar "$1"; }

tick() {
    capture "$SPAWN_BIN" tick
    [ "$status" -eq 0 ] || fail "tick exited $status: $(err)"
    [ -s "$WORK/err.txt" ] && cat "$WORK/err.txt" >>"$WORK/ticks.log"
    return 0
}

submit() {
    capture "$SPAWN_BIN" submit --agent "$1" --session "$2" --key "$3" --action "$4"
    [ "$status" -eq 0 ] || fail "submit $3 exited $status: $(err)"
}

command_ok() {
    state=$(q "select state || ' ' || coalesce(succeeded::text, '') || ' ' || coalesce(error, '')
               from spawn_commands where idempotency_key = '$1'")
    case "$state" in
    "finished true "*) ;;
    *"pane never became ready"* | *"does not accept input"* | *"terminal disappeared"*)
        harness "$2: $state" ;;
    *) fail "$2: $state" ;;
    esac
}

ended() {
    q "select count(*) from agent_executions where session_id = '$1' and ended_at_ms is not null"
}

tick_until_ended() {
    deadline=$(($(now) + 180))
    while :; do
        tick
        [ "$(ended "$1")" -ge "$2" ] && return 0
        [ "$(now)" -lt "$deadline" ] ||
            harness "execution $2 of $1 did not end within 180 s; tick warnings: $(tail -n 5 "$WORK/ticks.log" 2>/dev/null)"
        sleep 0.25
    done
}

# The store's object names, one per line, sorted.
objects() {
    if [ -d "$store" ]; then
        find "$store" -type f | sed 's|.*/||' | grep -E '^[0-9a-f]{64}$' | LC_ALL=C sort
    fi
    return 0
}

# work <agent> <session> <key> <action> <n> <set name>: one execution that
# ends by itself; $WORK/<set name> gets the objects its checkpoint added.
work() {
    objects >"$WORK/before"
    submit "$1" "$2" "$3" "$4"
    tick
    command_ok "$3" "$3"
    tick_until_ended "$2" "$5"
    objects >"$WORK/after"
    LC_ALL=C comm -13 "$WORK/before" "$WORK/after" >"$WORK/$6"
}

engine_did() { grep -qx "$1" "$WORK/engine.log" 2>/dev/null || harness "the engine did not do: $1"; }

# all_present <set> <what> / all_absent <set> <what>
all_present() {
    objects >"$WORK/now"
    missing=$(LC_ALL=C comm -23 "$WORK/$1" "$WORK/now" | wc -l | tr -d ' ')
    [ "$missing" -eq 0 ] || fail "$2: $missing of $(wc -l <"$WORK/$1" | tr -d ' ') objects are gone"
}
all_absent() {
    objects >"$WORK/now"
    kept=$(LC_ALL=C comm -12 "$WORK/$1" "$WORK/now" | wc -l | tr -d ' ')
    [ "$kept" -eq 0 ] || fail "$2: $kept of $(wc -l <"$WORK/$1" | tr -d ' ') objects are still there"
}
nonempty() { [ -s "$WORK/$1" ] || fail "$2 added no object to the chunk store"; }

# gc <grace minutes>: one collection; $status, out, err as `capture` leaves them.
gc() { capture "$SPAWN_BIN" chunks gc --grace "$1"; }
gc_output() { cat "$WORK/out.txt" "$WORK/err.txt" | tr '\n' ' ' | cut -c1-400; }

# Every object two hours old.
age_all() {
    find "$store" -type f -exec touch -d "@$(($(now) - 7200))" {} +
}

drop_branch() { git --git-dir="$default" update-ref -d "refs/heads/$1"; }
head_of() { git --git-dir="$default" rev-parse --verify --quiet "refs/heads/$1" || echo none; }
trailer_of() {
    git --git-dir="$default" log -1 --format='%(trailers:key=Spawn-Large,valueonly,separator=%x20)' \
        "refs/heads/$1" 2>/dev/null | tr -d '\n' || true
}
mounted_sha() {
    if [ -f "$root/$1/data/big.bin" ]; then sha "$root/$1/data/big.bin"; else echo missing; fi
}

# --- the checkpoints ------------------------------------------------------------

# A1, default repository: a file, then an in-place edit of it.
work agent_one session_one k1a '{"type":"start","engine":"claude","prompt":"write big1"}' 1 set1a
engine_did "write big1"
nonempty set1a "the first checkpoint of agent_one"
commit_1=$(head_of agent_one)
work agent_one session_one k1b '{"type":"resume","prompt":"edit"}' 2 set1b
engine_did edit
nonempty set1b "the edit of agent_one"
[ "$(head_of agent_one)" != "$commit_1" ] || fail "the edit made no checkpoint commit"

# A2, a mission's agent: its branch lives in the mission's mirror under the root.
capture "$SPAWN_BIN" mission add --text "005 mirror" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker two --brief "write"
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(jq -r .task "$WORK/out.txt")
agent_two=$(q "select agent_id from mission_tasks where id = $task")
session_two=$(q "select session_id from mission_tasks where id = $task")
work "$agent_two" "$session_two" k2 '{"type":"start","engine":"claude","prompt":"write big2"}' 1 set2
engine_did "write big2"
nonempty set2 "the mission agent's checkpoint"
mirror=""
for candidate in "$root"/.sources/*.git; do
    if git --git-dir="$candidate" rev-parse --verify --quiet "refs/heads/$agent_two" >/dev/null 2>&1; then
        mirror=$candidate
    fi
done
[ -n "$mirror" ] || harness "no mirror under $root/.sources holds $agent_two"
# Only the mirror names A2's objects: the default repository does not.
git --git-dir="$default" rev-parse --verify --quiet "refs/heads/$agent_two" >/dev/null 2>&1 &&
    harness "the default repository has $agent_two's branch"

# A3 and A4, default repository: written, then their branches dropped.
work agent_three session_three k3 '{"type":"start","engine":"claude","prompt":"write big3"}' 1 set3
engine_did "write big3"
nonempty set3 "agent_three's checkpoint"
drop_branch agent_three
work agent_four session_four k4 '{"type":"start","engine":"claude","prompt":"write big4"}' 1 set4
engine_did "write big4"
nonempty set4 "agent_four's checkpoint"
drop_branch agent_four

# --- (c) young objects survive -------------------------------------------------

gc 30
[ "$status" -eq 0 ] || fail "(c) spawn chunks gc --grace 30 exited $status: $(gc_output)"
for set in set1a set1b set2 set3 set4; do
    all_present "$set" "(c) $set, written minutes ago, after gc --grace 30"
done

# Everything is two hours old. A5 then writes big3 again, the same bytes as
# A3: its checkpoint finds A3's objects in the store and must make them young
# again; then its branch is dropped too (as if its commit never came).
age_all
work agent_five session_five k5 '{"type":"start","engine":"claude","prompt":"write big3"}' 1 set5
engine_did "write big3"
drop_branch agent_five

# --- (d) an unreadable repository: nothing is deleted ---------------------------

objects >"$WORK/listing.d"
chmod 000 "$mirror"
gc 30
chmod 755 "$mirror"
[ "$status" -ne 0 ] ||
    fail "(d) gc exited 0 while the mirror $(basename "$mirror") was unreadable: $(gc_output)"
objects >"$WORK/now"
cmp -s "$WORK/listing.d" "$WORK/now" ||
    fail "(d) gc deleted $(LC_ALL=C comm -23 "$WORK/listing.d" "$WORK/now" | wc -l | tr -d ' ') objects although it could not read a repository"
assert_contains "$(gc_output)" "$(basename "$mirror")" "(d) gc's report names the unreadable repository"

# --- (b) old unreferenced objects go; (a) referenced ones stay ------------------

gc 30
[ "$status" -eq 0 ] || fail "(b) gc --grace 30 exited $status: $(gc_output)"
all_absent set4 "(b) agent_four's objects, unreferenced and two hours old"
all_present set3 "(c) agent_three's objects, unreferenced but written again by agent_five's checkpoint"
all_present set1a "(a) objects of an older commit of agent_one (before the edit)"
all_present set1b "(a) objects of agent_one's head"
all_present set2 "(a) objects named only from the mission mirror"

# (d) idempotent.
objects >"$WORK/listing.idem"
gc 30
[ "$status" -eq 0 ] || fail "(d) a second gc exited $status: $(gc_output)"
objects >"$WORK/now"
cmp -s "$WORK/listing.idem" "$WORK/now" || fail "(d) a second gc with nothing new changed the store"

# Once A3/A5's objects are old again, they go too.
age_all
gc 30
[ "$status" -eq 0 ] || fail "(b) gc --grace 30 exited $status: $(gc_output)"
all_absent set3 "(b) agent_three's objects, once two hours old again"
all_present set1a "(a) objects of an older commit of agent_one (before the edit)"
all_present set1b "(a) objects of agent_one's head"
all_present set2 "(a) objects named only from the mission mirror"

# (a) what survived still materializes.
submit agent_old session_old k_old "{\"type\":\"start\",\"engine\":\"claude\",\"prompt\":\"\",\"from\":\"$commit_1\"}"
tick
command_ok k_old "a fork from agent_one's first checkpoint"
assert_eq "$(mounted_sha agent_old)" "$sha_1" "(a) the file of agent_one's older commit, after collection"
submit agent_one session_one k1r '{"type":"resume"}'
tick
command_ok k1r "a resume of agent_one"
assert_eq "$(mounted_sha agent_one)" "$sha_1e" "(a) the edited file at agent_one's head, after collection"
submit "$agent_two" "$session_two" k2r '{"type":"resume"}'
tick
command_ok k2r "a resume of the mission agent"
assert_eq "$(mounted_sha "$agent_two")" "$sha_2" "(a) the mission agent's file, after collection"
for pair in "agent_old session_old" "agent_one session_one" "$agent_two $session_two"; do
    set -- $pair
    submit "$1" "$2" "stop-$1" '{"type":"stop"}'
    tick
    command_ok "stop-$1" "stop of $1"
done

# --- (e) a reachable manifest that does not verify ------------------------------

work agent_six session_six k6 '{"type":"start","engine":"claude","prompt":"write big4"}' 1 set6
engine_did "write big4"
nonempty set6 "agent_six's checkpoint"
drop_branch agent_six
age_all
manifest=$(trailer_of agent_one)
case "$manifest" in
[0-9a-f]*) ;;
*) fail "(e) agent_one's head names no manifest: [$manifest]" ;;
esac
manifest_file=$(find "$store" -type f -name "$manifest" | head -n 1)
[ -n "$manifest_file" ] || fail "(e) the manifest $manifest is not in the store"
chmod u+w "$manifest_file"
size=$(wc -c <"$manifest_file" | tr -d ' ')
offset=$((size / 2))
old=$(od -An -tu1 -j "$offset" -N1 "$manifest_file" | tr -d ' ')
printf "\\$(printf '%03o' $((255 - old)))" |
    dd of="$manifest_file" bs=1 seek="$offset" count=1 conv=notrunc 2>/dev/null
objects >"$WORK/listing.e"
gc 30
[ "$status" -ne 0 ] || fail "(e) gc exited 0 with a corrupt manifest named by agent_one's head: $(gc_output)"
objects >"$WORK/now"
cmp -s "$WORK/listing.e" "$WORK/now" ||
    fail "(e) gc deleted $(LC_ALL=C comm -23 "$WORK/listing.e" "$WORK/now" | wc -l | tr -d ' ') objects although a reachable manifest did not verify"
assert_contains "$(gc_output)" "$manifest" "(e) gc's report names the manifest that does not verify"

echo "005: gc kept every reachable object, removed old unreferenced ones, spared young and refreshed ones, refused on read errors"
exit 0
