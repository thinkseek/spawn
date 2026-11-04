#!/bin/sh
# Black box: a large file an engine writes is kept (docs/storage.md).
#
# A fake Claude engine writes a 20 MiB file (deterministic, incompressible)
# and a small text file into its workspace and exits. The execution ends:
# final checkpoint, unmount. The session is resumed in a fresh mount, and
# then:
#   (a) the large file is there again, byte for byte (sha256);
#   (b) it is not in git: the bare repository grew by less than 2 MiB, no
#       blob over 8 MiB is on the agent's branch, the text file is, and the
#       checkpoint commit names its large-file manifest in a `Spawn-Large:`
#       trailer;
#   (c) the chunk store lives where SPAWN_CHUNK_STORE says. A 1 MiB in-place
#       edit, checkpointed when the engine exits, is a commit with the same
#       tree and a new trailer; the repository still grows by less than
#       2 MiB, the chunk store by less than 8 MiB (content-defined chunks),
#       and the edited content survives another remount;
#   (e) old checkpoints still restore: a second agent started from the first
#       checkpoint commit (`start {"from": <commit>}`) gets the original
#       bytes in its fresh mount;
#   (f) objects are immutable: every non-empty file the chunk store held
#       after the first checkpoint is still there, byte for byte, after the
#       edit, the fork and their checkpoints;
#   (d) chunks are verified on read: with every file in the chunk store
#       corrupted, a remount under a fresh workspace root (same repository,
#       same chunk store, so only the store can supply the bytes) either
#       leaves the large file out or fails; it never puts other bytes there.
#       The exact original bytes are accepted too.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure (a tool, the
# database, a wait that ran out). Any unexpected error exits 2, never 1.
#
# Fails on the baseline with 1 at (a): large files stay out of git and are
# lost at unmount. Runs inside the gate's sandbox (scripts/rsi/gate.sh):
# needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own
# database, and uses a private tmux socket. Every wait is bounded in time.
set -eu

# Assertion failures set this before exiting 1; any other non-zero exit is
# turned into 2 by the exit trap.
asserted=""
harness() {
    echo "003: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git openssl sha256sum dd od du find sort; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_003_large_$run"
SPAWN_TMUX_SOCKET="bb-003-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# psql reaches the test server the way check-blackbox.sh does: address or
# percent-encoded socket directory, optional port.
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
    chmod -R u+w "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "003: harness: unexpected exit $rc" >&2
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

# --- the world: one bare repository, one root, one chunk store -----------

repo=$WORK/repo.git
git init -q --bare --initial-branch=main "$repo"
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" -c user.name=bb -c user.email=bb@localhost commit -qm base
git -C "$WORK/seed" push -q "$repo" main:main
mkdir -p "$WORK/ws"

MIB=1048576
# Deterministic and incompressible: an AES-CTR keystream with a fixed key.
# Compressible bytes would let git store the file small and hide (b).
keystream() {
    head -c "$2" /dev/zero |
        openssl enc -aes-256-ctr -nosalt -K "$1" -iv 00000000000000000000000000000000
}
keystream "$(printf '%064d' 3)" $((20 * MIB)) >"$WORK/big.v1"
keystream "$(printf '%064d' 7)" "$MIB" >"$WORK/patch"
cp "$WORK/big.v1" "$WORK/big.v2"
dd if="$WORK/patch" of="$WORK/big.v2" bs="$MIB" seek=7 conv=notrunc 2>/dev/null
[ "$(wc -c <"$WORK/big.v1" | tr -d ' ')" -eq $((20 * MIB)) ] ||
    harness "openssl did not produce 20 MiB"
sha_v1=$(sha256sum "$WORK/big.v1" | cut -d' ' -f1)
sha_v2=$(sha256sum "$WORK/big.v2" | cut -d' ' -f1)
[ "$sha_v1" != "$sha_v2" ] || harness "the edit changed nothing"

# The fake engine: `write-large` writes the large file and a text file,
# `edit-large` rewrites 1 MiB of the large file in place; both then exit,
# which ends the execution (final checkpoint, unmount). Anything else is
# echoed, like the harness's fake.
cat >"$WORK/claude" <<EOF
#!/bin/sh
printf 'Welcome to Claude Code\n\n'
printf '❯ '
while IFS= read -r line; do
  case "\$line" in
  *write-large*)
    mkdir -p data || exit 3
    cp '$WORK/big.v1' data/big.bin || exit 3
    printf 'small text\n' >data/notes.txt || exit 3
    printf 'wrote\n' >>'$WORK/engine.log'
    printf '\nwrote\n'
    exit 0 ;;
  *edit-large*)
    dd if='$WORK/patch' of=data/big.bin bs=$MIB seek=7 conv=notrunc 2>/dev/null || exit 3
    printf 'edited\n' >>'$WORK/engine.log'
    printf '\nedited\n'
    exit 0 ;;
  *)
    printf '\n%s\n' "echo: \$line"
    printf '❯ ' ;;
  esac
done
EOF
chmod 755 "$WORK/claude"

# The local tmux host runs SPAWN_TMUX_PROGRAM; pin the private socket so
# the ticks and the pane helpers share one server (as in 030).
cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

# Only what this scenario sets reaches the binary.
for name in SPAWN_EXEC_IMAGE SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL \
    SPAWN_SUPERVISED CLAUDE_CONFIG_DIR; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="${url%/*}/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CHUNK_STORE=$WORK/chunks
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CODEX_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CHUNK_STORE \
    SPAWN_CLAUDE_PROGRAM SPAWN_CODEX_PROGRAM SPAWN_CLAUDE_PROJECTS SPAWN_TMUX_PROGRAM \
    SPAWN_WORKER

agent=agent_large
session=session_large
branch=refs/heads/$agent
fork_agent=agent_fork
fork_session=session_fork

# The test server is shared: another run's init-db can collide with this one
# on cluster-wide objects (roles, grants: "tuple concurrently updated").
# init-db is idempotent, so a few tries absorb that.
tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- helpers ----------------------------------------------------------------

bytes() { du -sb "$1" 2>/dev/null | cut -f1; }
repo_bytes() { bytes "$repo"; }
store_bytes() {
    if [ -d "$SPAWN_CHUNK_STORE" ]; then bytes "$SPAWN_CHUNK_STORE"; else echo 0; fi
}
mib() { awk -v b="$1" 'BEGIN { printf "%.2f MiB", b / 1048576 }'; }
now() { date +%s; }

# submit <agent> <session> <key> <action>
submit() {
    capture "$SPAWN_BIN" submit --agent "$1" --session "$2" --key "$3" --action "$4"
    [ "$status" -eq 0 ] || fail "submit $3 exited $status: $(err)"
}

tick() {
    capture "$SPAWN_BIN" tick
    [ "$status" -eq 0 ] || fail "tick exited $status: $(err)"
    [ -s "$WORK/err.txt" ] && cat "$WORK/err.txt" >>"$WORK/ticks.log"
    return 0
}

# Ended executions of one session.
ended() {
    scalar "select count(*) from agent_executions
            where session_id = '$1' and ended_at_ms is not null"
}

# tick_until_ended <session> <n>: tick until the session has <n> ended
# executions. Generous for a loaded host; a run that ends on time never waits.
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

# engine_did <word>: the fake engine finished that work. Read from its own
# log, not the execution's exit code: the Bridge may record a clean exit as
# `lost` when the pane closes between its exit-file read and its liveness
# check, and the final checkpoint runs either way.
engine_did() { grep -qx "$1" "$WORK/engine.log" 2>/dev/null; }

# command_ok <key> <what>: the command finished and succeeded. A pane that
# never got ready is the host's failure, not the behaviour's.
command_ok() {
    state=$(scalar "select state || ' ' || coalesce(succeeded::text, '') || ' ' || coalesce(error, '')
                    from spawn_commands where idempotency_key = '$1'")
    case "$state" in
    "finished true "*) ;;
    *"pane never became ready"* | *"does not accept input"* | *"terminal disappeared"*)
        harness "$2: $state" ;;
    *) fail "$2: $state" ;;
    esac
}

pane_ready() { pane | grep -F 'Welcome to Claude Code' >/dev/null; }

# sha256 of a mount's large file, or "missing".
sha_in() {
    if [ -f "$1/data/big.bin" ]; then sha256sum "$1/data/big.bin" | cut -d' ' -f1; else echo missing; fi
}

head_commit() { git --git-dir="$repo" rev-parse --verify --quiet "$branch" || echo none; }
head_tree() { git --git-dir="$repo" rev-parse --verify --quiet "$branch^{tree}" || echo none; }
head_trailer() {
    git --git-dir="$repo" log -1 --format='%(trailers:key=Spawn-Large,valueonly,separator=%x20)' \
        "$branch" 2>/dev/null | tr -d '\n' || true
}
# Largest blob on the branch head, in bytes.
largest_blob() {
    git --git-dir="$repo" ls-tree -r -l "$branch" |
        awk '$2 == "blob" && $4 > max { max = $4 } END { print max + 0 }'
}

workspace=$SPAWN_WORKSPACE_ROOT/$agent
repo_start=$(repo_bytes)

# --- 1. the engine writes the large file; the execution ends ---------------

submit "$agent" "$session" k_write '{"type":"start","engine":"claude","prompt":"write-large"}'
tick
command_ok k_write "start command"
tick_until_ended "$session" 1
engine_did wrote || harness "the engine did not write its files"
# The unmount removed the workspace: what comes back is a fresh mount.
[ ! -e "$workspace" ] || fail "the workspace is still mounted after the execution ended"

# --- 2. resume in a fresh mount: (a) the large file is back ------------------

submit "$agent" "$session" k_resume1 '{"type":"resume"}'
tick
command_ok k_resume1 "first resume"
[ -d "$workspace" ] || fail "the resume mounted no workspace"
assert_eq "$(cat "$workspace/data/notes.txt" 2>/dev/null || echo missing)" "small text" \
    "(a) the text file after the remount"
got=$(sha_in "$workspace")
[ "$got" != missing ] ||
    fail "(a) data/big.bin is missing after the remount: the large file was lost at unmount"
assert_eq "$got" "$sha_v1" "(a) sha256 of data/big.bin after the remount"

# (b) it is not in git.
grown=$(($(repo_bytes) - repo_start))
[ "$grown" -lt $((2 * MIB)) ] ||
    fail "(b) the bare repository grew by $(mib "$grown") for a 20 MiB file: it went into git"
largest=$(largest_blob)
[ "$largest" -le $((8 * MIB)) ] ||
    fail "(b) a blob of $(mib "$largest") is on the branch: large files stay out of git"
git --git-dir="$repo" cat-file -e "$branch:data/notes.txt" 2>/dev/null ||
    fail "(b) data/notes.txt is not on the branch: text stays in git"
trailer1=$(head_trailer)
[ -n "$trailer1" ] ||
    fail "(b) the checkpoint commit has no Spawn-Large trailer: $(git --git-dir="$repo" log -1 --format=%B "$branch")"
tree1=$(head_tree)
commit1=$(head_commit)

# (c) the chunk store is where SPAWN_CHUNK_STORE says.
store1=$(store_bytes)
[ "$store1" -ge $((18 * MIB)) ] ||
    fail "(c) SPAWN_CHUNK_STORE ($SPAWN_CHUNK_STORE) holds $(mib "$store1"), not the 20 MiB file's chunks"

# (f) every object so far, by path and checksum; checked after the edit.
: >"$WORK/objects.sha256"
if [ -d "$SPAWN_CHUNK_STORE" ]; then
    (cd "$SPAWN_CHUNK_STORE" && find . -type f -size +0 | LC_ALL=C sort |
        while IFS= read -r object; do sha256sum "$object"; done) >"$WORK/objects.sha256"
fi
objects1=$(wc -l <"$WORK/objects.sha256" | tr -d ' ')

# --- 3. a 1 MiB in-place edit, checkpointed at the engine's exit -------------

start=$(now)
until pane_ready; do
    [ $(($(now) - start)) -lt 90 ] ||
        harness "the resumed engine never showed its prompt in 90 s: $(pane 2>&1 | tail -n 5)"
    sleep 0.25
done
submit "$agent" "$session" k_edit '{"type":"input","text":"edit-large"}'
tick
command_ok k_edit "edit input"
tick_until_ended "$session" 2
engine_did edited || harness "the engine did not edit the file"
[ ! -e "$workspace" ] || fail "the workspace is still mounted after the second execution ended"

commit2=$(head_commit)
[ "$commit2" != "$commit1" ] ||
    fail "(c) the edit of the large file made no checkpoint commit"
assert_eq "$(head_tree)" "$tree1" "(c) a large-file-only change keeps the tree"
trailer2=$(head_trailer)
[ -n "$trailer2" ] || fail "(c) the edit's checkpoint commit has no Spawn-Large trailer"
[ "$trailer2" != "$trailer1" ] || fail "(c) the edit's checkpoint names the same manifest"
grown=$(($(repo_bytes) - repo_start))
[ "$grown" -lt $((2 * MIB)) ] ||
    fail "(c) after the edit the bare repository has grown by $(mib "$grown")"
store_grown=$(($(store_bytes) - store1))
[ "$store_grown" -lt $((8 * MIB)) ] ||
    fail "(c) a 1 MiB edit grew the chunk store by $(mib "$store_grown"): chunks are not shared"

# --- 4. resume again: the edited content is back -----------------------------

submit "$agent" "$session" k_resume2 '{"type":"resume"}'
tick
command_ok k_resume2 "second resume"
assert_eq "$(sha_in "$workspace")" "$sha_v2" "(c) sha256 of the edited data/big.bin after the remount"

submit "$agent" "$session" k_stop '{"type":"stop"}'
tick
command_ok k_stop "stop"
assert_eq "$(ended "$session")" 3 "three executions of the session ended"
[ ! -e "$workspace" ] || fail "the workspace is still mounted after the stop"

# --- 5. (e) the first checkpoint still restores, in a fork from its commit ---

fork_workspace=$SPAWN_WORKSPACE_ROOT/$fork_agent
submit "$fork_agent" "$fork_session" k_fork \
    "{\"type\":\"start\",\"engine\":\"claude\",\"prompt\":\"\",\"from\":\"$commit1\"}"
tick
command_ok k_fork "start of a second agent from the first checkpoint"
assert_eq "$(git --git-dir="$repo" rev-parse --verify --quiet "refs/heads/$fork_agent" || echo none)" \
    "$commit1" "(e) the fork's branch starts at the first checkpoint"
assert_eq "$(cat "$fork_workspace/data/notes.txt" 2>/dev/null || echo missing)" "small text" \
    "(e) the text file in the fork"
assert_eq "$(sha_in "$fork_workspace")" "$sha_v1" \
    "(e) sha256 of data/big.bin in a mount of the first checkpoint (older versions must stay restorable)"
submit "$fork_agent" "$fork_session" k_fork_stop '{"type":"stop"}'
tick
command_ok k_fork_stop "stop of the fork"

# --- 6. (f) nothing the store held was rewritten or removed -------------------

[ "$objects1" -gt 0 ] || fail "(f) the chunk store held no objects after the first checkpoint"
if ! (cd "$SPAWN_CHUNK_STORE" && sha256sum -c --quiet "$WORK/objects.sha256") \
    >"$WORK/immutable.txt" 2>&1; then
    fail "(f) objects of the first checkpoint were rewritten or removed: $(head -n 3 "$WORK/immutable.txt" | tr '\n' ' ')"
fi

# --- 7. (d) verified on read: corrupt every stored file, remount fresh ---------

find "$SPAWN_CHUNK_STORE" -type f -size +0 >"$WORK/stored" 2>/dev/null || true
[ -s "$WORK/stored" ] || fail "(d) the chunk store holds no files"
while IFS= read -r file; do
    chmod u+w "$file"
    size=$(wc -c <"$file" | tr -d ' ')
    offset=$((size / 2))
    old=$(od -An -tu1 -j "$offset" -N1 "$file" | tr -d ' ')
    new=$((255 - old))
    printf "\\$(printf '%03o' "$new")" |
        dd of="$file" bs=1 seek="$offset" count=1 conv=notrunc 2>/dev/null
done <"$WORK/stored"

# A fresh workspace root: no stat cache, no leftover of any earlier mount.
SPAWN_WORKSPACE_ROOT=$WORK/ws-fresh
mkdir -p "$SPAWN_WORKSPACE_ROOT"
submit "$agent" "$session" k_resume3 '{"type":"resume"}'
tick
got=$(sha_in "$SPAWN_WORKSPACE_ROOT/$agent")
case "$got" in
missing | "$sha_v2") ;;
*) fail "(d) with the chunk store corrupted, a fresh mount put other bytes in data/big.bin" ;;
esac

echo "003: large file kept across remounts and forks; repository +$(mib "$grown"), chunk store $(mib "$store1") +$(mib "$store_grown") for the edit, $objects1 objects unchanged"
exit 0
