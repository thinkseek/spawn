#!/bin/sh
# Black box: a large file an engine writes is kept (docs/storage.md, phase 1).
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
#   (d) chunks are verified on read: with every file in the chunk store
#       corrupted, a remount never puts other bytes in the large file (it
#       may fail, or leave the file out, but not hand back wrong content).
#
# Fails on the baseline, where large files stay out of git and are lost at
# unmount. Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs
# SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own database,
# and uses a private tmux socket. Every wait is bounded.
set -eu

: "${SPAWN_BIN:?SPAWN_BIN is required}"
: "${SPAWN_TEST_DATABASE_URL:?SPAWN_TEST_DATABASE_URL is required}"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] || {
    echo "003: harness: no $controller/scripts/blackbox/assertions.sh" >&2
    exit 2
}
for tool in psql tmux git openssl sha256sum dd od du; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "003: harness: $tool is not installed" >&2
        exit 2
    }
done

WORK=$(mktemp -d)
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
trap cleanup EXIT
trap 'exit 130' INT TERM

if ! psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1; then
    echo "003: harness: cannot create the scenario database" >&2
    exit 2
fi
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

# --- the world: one bare repository, one root, one chunk store -----------

# Exit codes (scripts/rsi/gate.sh): 0 pass, 1 an assertion failed (`fail`),
# 2 a harness error. Setting up the world is harness: each step says so.
harness() {
    echo "003: harness: $*" >&2
    exit 2
}

repo=$WORK/repo.git
git init -q --bare --initial-branch=main "$repo" || harness "git init"
git init -q --initial-branch=main "$WORK/seed" || harness "git init"
printf 'base\n' >"$WORK/seed/README.md" || harness "seed"
git -C "$WORK/seed" add -A || harness "git add"
git -C "$WORK/seed" -c user.name=bb -c user.email=bb@localhost commit -qm base || harness "git commit"
git -C "$WORK/seed" push -q "$repo" main:main || harness "git push"
mkdir -p "$WORK/ws" || harness "mkdir"

MIB=1048576
# Deterministic and incompressible: an AES-CTR keystream with a fixed key.
# Compressible bytes would let git store the file small and hide (b).
keystream() {
    head -c "$2" /dev/zero |
        openssl enc -aes-256-ctr -nosalt -K "$1" -iv 00000000000000000000000000000000
}
keystream "$(printf '%064d' 3)" $((20 * MIB)) >"$WORK/big.v1" || harness "openssl"
keystream "$(printf '%064d' 7)" "$MIB" >"$WORK/patch" || harness "openssl"
cp "$WORK/big.v1" "$WORK/big.v2" || harness "cp"
dd if="$WORK/patch" of="$WORK/big.v2" bs="$MIB" seek=7 conv=notrunc 2>/dev/null || harness "dd"
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
    printf '\nwrote\n'
    exit 0 ;;
  *edit-large*)
    dd if='$WORK/patch' of=data/big.bin bs=$MIB seek=7 conv=notrunc 2>/dev/null || exit 3
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
workspace=$SPAWN_WORKSPACE_ROOT/$agent
big=$workspace/data/big.bin

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"

# --- helpers ----------------------------------------------------------------

bytes() { du -sb "$1" 2>/dev/null | cut -f1; }
repo_bytes() { bytes "$repo"; }
store_bytes() {
    if [ -d "$SPAWN_CHUNK_STORE" ]; then bytes "$SPAWN_CHUNK_STORE"; else echo 0; fi
}
mib() { awk -v b="$1" 'BEGIN { printf "%.2f MiB", b / 1048576 }'; }

submit() {
    capture "$SPAWN_BIN" submit --agent "$agent" --session "$session" --key "$1" \
        --action "$2"
    assert_eq "$status" 0 "submit $1 exit"
}

tick() {
    capture "$SPAWN_BIN" tick
    [ "$status" -eq 0 ] || fail "tick failed: $(err)"
    [ -s "$WORK/err.txt" ] && cat "$WORK/err.txt" >>"$WORK/ticks.log"
    return 0
}

ended() { scalar 'select count(*) from agent_executions where ended_at_ms is not null'; }

# Tick until <n> executions have ended, at most <limit> ticks.
tick_until_ended() {
    n=0
    while [ "$n" -lt "$2" ]; do
        tick
        [ "$(ended)" -ge "$1" ] && return 0
        sleep 0.25
        n=$((n + 1))
    done
    harness "timeout: execution $1 never ended after $2 ticks; tick warnings: $(tail -n 5 "$WORK/ticks.log" 2>/dev/null)"
}

command_state() {
    scalar "select state || ' ' || coalesce(succeeded::text, '') || ' ' || coalesce(error, '')
            from spawn_commands where idempotency_key = '$1'"
}

pane_ready() { pane | grep -F 'Welcome to Claude Code' >/dev/null; }

# The large file in the current mount, as a sha256 or "missing".
mounted_sha() {
    if [ -f "$big" ]; then sha256sum "$big" | cut -d' ' -f1; else echo missing; fi
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

repo_start=$(repo_bytes)

# --- 1. the engine writes the large file; the execution ends ---------------

submit k_write '{"type":"start","engine":"claude","prompt":"write-large"}'
tick
assert_eq "$(command_state k_write)" "finished true " "start command"
tick_until_ended 1 120
assert_eq "$(scalar "select coalesce(exit_code::text, cause) from agent_executions order by started_at_ms limit 1")" \
    0 "the engine wrote its files and exited 0"
# The unmount removed the workspace: what comes back is a fresh mount.
[ ! -e "$workspace" ] || fail "the workspace is still mounted after the execution ended"

# --- 2. resume in a fresh mount: (a) the large file is back ------------------

submit k_resume1 '{"type":"resume"}'
tick
assert_eq "$(command_state k_resume1)" "finished true " "first resume"
[ -d "$workspace" ] || fail "the resume mounted no workspace"
assert_eq "$(cat "$workspace/data/notes.txt" 2>/dev/null || echo missing)" "small text" \
    "(a) the text file after the remount"
got=$(mounted_sha)
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

# --- 3. a 1 MiB in-place edit, checkpointed at the engine's exit -------------

wait_for 60 pane_ready || harness "timeout: the resumed engine never showed its prompt: $(pane 2>&1)"
submit k_edit '{"type":"input","text":"edit-large"}'
tick
assert_eq "$(command_state k_edit)" "finished true " "edit input"
tick_until_ended 2 120
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

submit k_resume2 '{"type":"resume"}'
tick
assert_eq "$(command_state k_resume2)" "finished true " "second resume"
assert_eq "$(mounted_sha)" "$sha_v2" "(c) sha256 of the edited data/big.bin after the remount"

submit k_stop '{"type":"stop"}'
tick
assert_eq "$(command_state k_stop)" "finished true " "stop"
assert_eq "$(ended)" 3 "three executions ended"
[ ! -e "$workspace" ] || fail "the workspace is still mounted after the stop"

# --- 5. (d) verified on read: corrupt every stored byte range, remount --------

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

submit k_resume3 '{"type":"resume"}'
tick
got=$(mounted_sha)
case "$got" in
missing | "$sha_v2") ;;
*) fail "(d) with the chunk store corrupted, the remount put other bytes in data/big.bin" ;;
esac

echo "003: large file kept across remounts; repository +$(mib "$grown"), chunk store $(mib "$store1") +$(mib "$store_grown") for the edit"
