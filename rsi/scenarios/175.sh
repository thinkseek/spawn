#!/bin/sh
# Objective 175 [recoverability]: the transcript archive masks credential
# values (D9). Spawn's archive (`<root>/.transcripts`) is its own copy of
# every engine's transcript, kept for good. An engine that prints a
# credential it read writes the value into its transcript, and the archive
# kept it in the clear. Required: before a line is copied, every value the
# value collector places (SPAWN_ARCHIVE_LOCATIONS_TOOL, a tool that prints
# where each value lies, never the value) is replaced by
# `[REDACTED:<name>]`, in each of its forms (as written, percent-encoded,
# base64); the transcript itself is never written; and a pass that cannot
# have every value (a credential file unreadable) copies nothing, says so in
# the loop's log naming the file and no value, and the lines wait for the
# next pass that can.
#
# A real `spawn loop` runs one ephemeral worker on the fake engine below. The
# credential file is made up (`$WORK/cred/spawn.env`, KEY=VALUE lines, the
# first ended by a lone CR, which the collector reads as a line's end),
# and so is the location tool: a stand-in that reads that file and prints
# its place (path, line, span, name) and the file's stamp, as the
# collector's emit-locations.py does; it lists the file as unreadable when
# it cannot read it. The engine, asked BB:SAY, writes one assistant line
# with the value the agent knows ($WORK/said) as written, percent-encoded
# and base64-encoded, in two writes 1.5 s apart cut inside the value (a
# pass may read the first part alone); it writes a copy of each line it
# writes to $WORK/mirror.
# Checked:
#   (1) after the task's turn and one BB:SAY turn, the archive holds
#       `[REDACTED:spawn.env:API_TOKEN]` and none of the value's three
#       forms, nor any part of it (`made-up`, which every made-up value
#       begins with: a line read half written is copied whole, later); the
#       transcript is byte for byte the engine's mirror (the archive never
#       wrote it) and holds the value.
#   (2) the credential is rotated (a new value written to the file) and the
#       file made unreadable; a BB:SAY turn prints the new value. Its lines
#       are not copied: the archive is unchanged 8 s after the turn, the
#       loop's log says why naming the file, and no log holds either value.
#       Once the file is readable again, within 20 s the archive holds the
#       waiting lines, the new value masked in its three forms, and no form
#       of either value. (A mask is kept while its files are unchanged: a
#       file made unreadable alone keeps masking from memory; a rotation is
#       what needs it read again.)
#   (3) the transcript loses its head under the running engine (removed;
#       the engine's next turn, a plain one, writes a new file holding only
#       its lines, as when the projects directory is removed under it), then
#       the session is stopped and resumed. The new file's copy is its own
#       bytes and the archive's join is longer and holds every byte of it,
#       as before 175 when a file was replaced; but an older copy is masked,
#       so the file is left as it is: the resumed engine's file begins with
#       exactly the bytes it held.
#       The copies' records are removed first: a copy that holds the marker
#       is taken as masked without one.
#   (4) stopped again, the transcript removed, and resumed: it is put back
#       from the archive, masked: it holds the marker and no part of either
#       value, and the loop says it restored it. A BB:SAY turn after it is
#       archived in the same copy: no new copy, and the archive grows by
#       less than the restored file (it is not copied whole again).
#   (5) the credential rotated back and the file unreadable, a BB:SAY turn,
#       then stopped and resumed: the resume's restore cannot have the mask,
#       so it copies nothing (the archive unchanged 5 s after the resumed
#       engine's turn) and leaves the file as it is; once the file is
#       readable, within 20 s the archive holds the waiting lines and no
#       part of a value.
#   (6) an older copy appears beside the credential file (spawn.env.bak,
#       holding another value; the file itself unchanged), and a BB:SAY turn
#       prints that value: within 20 s it is archived masked, the tool run
#       again because the directory the tool watches changed.
#   (7) a workspace under a watched base (`$WORK/wsroot/a`, there from the
#       start) gains a `.claude/creds.env` holding another value: no watched
#       directory changes (the workspace itself is not watched), only the
#       probed path `a/.claude` appears. A BB:SAY turn printing that value
#       is archived masked within 20 s.
#   (8) the credential file rewritten in place with another value of the
#       same size and its mtime set back (touch -r): no stamp the collector
#       gives changes, only the ctime; a BB:SAY turn printing the new value
#       is archived masked within 20 s.
#   (9) R4 (cc-astra A77): the tool answers, and right after it (before the
#       Bridge reads the file) the file is rewritten in place to the same
#       size with its mtime set back: the placed line now holds an ordinary
#       value of the old value's length, and a new credential is on a line no
#       place names. A BB:SAY turn printing the new credential is archived
#       masked within 20 s: a source whose device, inode or ctime is not the
#       tool's is refused, and the tool run again.
#   (10) R5 (lead-mac's condition 2): the loop started again with the
#       stand-in still named but without SPAWN_GATE_SANDBOX=1 (as a live
#       loop would be if one were set by mistake): a BB:SAY turn is not
#       archived (the archive unchanged 8 s after it), and the loop's log
#       says "SPAWN_ARCHIVE_LOCATIONS_TOOL is honoured only in a gate's
#       sandbox". The same again with SPAWN_GATE_SANDBOX=0: only `1` marks
#       a gate. A stand-in is never used outside a gate.
#   (12) R6 (cc-astra A81): the loop that accepted 175.sh's stand-in says
#       so in its log, one line naming the tool and its roots: "spawn:
#       WARNING: the transcript archive is masked by the stand-in location
#       tool <tool> (SPAWN_GATE_SANDBOX=1), not the installed collector, for
#       the roots <workspace root> and <transcripts>". (A stand-in refused
#       for roots inside the account's home is a unit test: a scenario
#       never writes under the real home.)
#   (13) R7 (cc-astra A81: aliases below the roots): with the stand-in
#       accepted, the loop started again with `<root>/.transcripts` a link
#       to another directory: a BB:SAY turn writes nothing there (the
#       archive is reached from its root's descriptor, no link followed),
#       and the loop's log says the link is "not followed". Then, with the
#       archive back, the engine's project directory under the transcripts
#       root made a link to another directory (the engine writes through
#       it): a BB:SAY turn is not archived (the archive unchanged 8 s after
#       it), and the log says "not followed". Last, the agent's latest copy
#       made a relative link to a file of the same bytes inside the workspace
#       root (where the agents' own workspaces are; RESOLVE_BENEATH alone
#       would follow it): a BB:SAY turn writes nothing to that file, and the
#       log says "not followed". And the copy written last made a hard link
#       to a file outside the workspace root (as `cp -al` of a live archive
#       makes): a BB:SAY turn writes nothing to it, and the log says "hard
#       links: not followed" (rev-e's F1 on R7). Then (R8, cc-astra A88)
#       the projects tree moved into the teller's own workspace, the loop
#       started again with SPAWN_CLAUDE_PROJECTS there (a native path then
#       starts with the workspace's), and the teller's native transcript
#       given a second name outside the roots (a hard link): its turn is
#       not archived and the log says "hard links: not followed". (A link
#       into the real home is a unit test: a scenario never touches it.)
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main: the archive holds the value.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN,
# SPAWN_TEST_DATABASE_URL, tmux, jq, python3, base64 and git; creates and
# drops its own database, uses a private tmux socket, loopback only. Every
# wait is bounded in time. On a passing tree it takes about a minute.

set -eu

asserted=""
harness() {
    echo "175: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed awk python3 base64 cmp; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
real_tmux=$(command -v tmux)

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME, and the tmux server's socket, in WORK: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$HOME" "$TMUX_TMPDIR"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME TMUX_TMPDIR
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
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_175_mask_$run"
SPAWN_TMUX_SOCKET="bb-175-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
wrapper_pid=""
serve_pid=""
stop_loop() {
    [ -n "$wrapper_pid" ] || return 0
    : >"$WORK/loop.stop"
    child=$(cat "$WORK/loop.pid" 2>/dev/null || true)
    if [ -n "$child" ]; then kill -TERM "$child" 2>/dev/null || true; fi
    i=0
    while kill -0 "$wrapper_pid" 2>/dev/null; do
        i=$((i + 1))
        if [ "$i" -eq 150 ] && [ -n "$child" ]; then kill -KILL "$child" 2>/dev/null || true; fi
        if [ "$i" -ge 200 ]; then
            kill -KILL "$wrapper_pid" 2>/dev/null || true
            break
        fi
        sleep 0.2
    done
    wait "$wrapper_pid" 2>/dev/null || true
    wrapper_pid=""
}
stop_serve() {
    [ -n "$serve_pid" ] || return 0
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
}
cleanup() {
    stop_serve
    stop_loop
    "$real_tmux" -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        for log in loop serve; do
            if [ -s "$WORK/$log.log" ]; then
                echo "175: the $log's last lines:" >&2
                grep -v '^\s*$' "$WORK/$log.log" | tail -n 8 >&2
            fi
        done
        if [ -s "$WORK/engine.log" ]; then
            echo "175: the engines' last lines (ms agent what):" >&2
            tail -n 8 "$WORK/engine.log" >&2
        fi
        # What says a window is settling (flake notes, sync/flake-047-056.md):
        # each progress record as it stands, with the time it was written.
        if [ -d "${SPAWN_WORKSPACE_ROOT-}/.transcripts-progress" ]; then
            echo "175: the archive's progress records at $(date +%s%3N) (written-ms agent live known_ms behind_ms archived):" >&2
            find "$SPAWN_WORKSPACE_ROOT/.transcripts-progress" -type f -name '*.json' 2>/dev/null |
                while IFS= read -r record; do
                    printf '%s %s\n' "$(stat -c %.3Y "$record" | tr -d .)" \
                        "$(jq -r '"\(.agent) \(.live) \(.known_ms) \(.behind_ms) \(.archived)"' "$record" 2>/dev/null)"
                done >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "175: harness: unexpected exit $rc" >&2
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
note() { echo "175: $*"; }

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

repo=$WORK/repo.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git init -q --bare --initial-branch=main "$repo"
git -C "$WORK/seed" push -q "$repo" main:main

mkdir -p "$WORK/claude-config" "$WORK/ctl" "$WORK/state"


# The fake engine: a prompt; what is typed into it is one message, written
# after a second of quiet to its session transcript as Claude Code writes a
# prompt. A turn writes one assistant line and ends 2 s after the prompt;
# a prompt holding BB:SAY makes that line hold the credential value the
# agent knows ($BB_WORK/said), as written, percent-encoded and base64-encoded. Each line
# is also appended to $BB_WORK/mirror/<native>.jsonl. Every prompt and turn
# end is logged (engine.log: ms agent what).
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in
  --resume | --session-id) native=${2-}; shift ;;
  esac
  shift
done
agent=$SPAWN_AGENT_ID
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir" "$BB_WORK/mirror"
transcript=$dir/$native.jsonl
count=0
line() {
  count=$((count + 1))
  text=$(printf '{"uuid":"%s-%s-%s","timestamp":"%s","sessionId":"%s",%s}' \
    "$SPAWN_EXECUTION_ID" "$$" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)" "$native" "$1")
  printf '%s\n' "$text" >>"$BB_WORK/mirror/$native.jsonl"
  printf '%s\n' "$text" >>"$transcript"
}
# split_line <json fields> <value>: a line written in two parts, cut eight
# bytes into the value, 1.5 s apart: a pass may read the first part alone.
split_line() {
  count=$((count + 1))
  text=$(printf '{"uuid":"%s-%s-%s","timestamp":"%s","sessionId":"%s",%s}' \
    "$SPAWN_EXECUTION_ID" "$$" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)" "$native" "$1")
  prefix=${text%%"$2"*}
  cut=$((${#prefix} + 8))
  printf '%s' "${text:0:cut}" | tee -a "$BB_WORK/mirror/$native.jsonl" >>"$transcript"
  sleep 1.5
  printf '%s\n' "${text:cut}" | tee -a "$BB_WORK/mirror/$native.jsonl" >>"$transcript"
}
log() { printf '%s %s %s\n' "$(date +%s%3N)" "$agent" "$1" >>"$BB_WORK/engine.log"; }
turn() { # <the prompt's text>
  said="working"
  case $1 in
  *BB:SAY*)
    value=$(cat "$BB_WORK/said")
    pct=$(printf '%s' "$value" | jq -sRr @uri)
    b64=$(printf '%s' "$value" | base64 -w0)
    said="the token is $value; in a link: https://h.example/?k=$pct; encoded: $b64; linked: $(cat "$BB_WORK/linked"); leaf: $(cat "$BB_WORK/leaf"); db: postgres://bb:mUp5x@127.0.0.1/d"
    ;;
  esac
  fields="\"type\":\"assistant\",\"message\":{\"id\":\"msg_${SPAWN_EXECUTION_ID}_$$_$count\",\"model\":\"bb-model\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":$(printf '%s' "$said" | jq -Rs .)}]}"
  case $1 in
  *BB:SAY*) split_line "$fields" "$value" ;;
  *) line "$fields" ;;
  esac
  sleep 2
  line '"type":"system","subtype":"turn_duration","durationMs":2000'
  log "turn-ended"
  printf '\r\033[2Kdone\n\n❯ '
}
printf 'Welcome to Claude Code\n\n❯ '
buffer=""
while :; do
  if bb_read_line typed; then
    buffer="${buffer:+$buffer
}$typed"
  elif [ -n "$buffer" ]; then
    raw=$buffer
    buffer=""
    line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":$(printf '%s' "$raw" | jq -Rs .)}"
    log "prompt"
    printf '\n✻ Working… (esc to interrupt)'
    turn "$raw"
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# The stand-in location tool: the credential file's one place and stamp, as
# emit-locations.py prints them (never the value); the file listed as
# unreadable when it cannot be read.
cat >"$WORK/locations.py" <<'PY'
import glob, json, os
cred, doc = os.environ["BB_CRED"], os.environ["BB_DOC"]
out = {"sources": [], "entries": [], "missing": [], "unreadable": [], "accepted": [],
       "accepted_paths": [], "watch": [], "probe": []}
def read(path):
    try:
        with open(path, "rb") as f:
            st = os.fstat(f.fileno())
            data = f.read()
    except PermissionError:
        out["unreadable"].append(path)
        return None
    out["sources"].append({"path": path, "size": st.st_size, "mtime_ns": st.st_mtime_ns,
                           "dev": st.st_dev, "ino": st.st_ino, "ctime_ns": st.st_ctime_ns})
    return data
def place(path, number, ptr, **rest):
    out["entries"].append(dict(path=path, line=number, ptr=ptr, **rest))
# The file and its older copies beside it (spawn.env.<anything>), as the collector reads them.
for path in [cred] + sorted(glob.glob(glob.escape(cred) + ".*")):
  data = read(path)
  cred_now = path
  # As the collector's read() splits: universal newlines (a lone CR ends a line) and NUL.
  for number, line in enumerate(__import__("re").split(rb"\r\n|\r|\n|\0", data or b"")):
      if line.startswith(b"API_TOKEN="):
          place(cred_now, number, None, t="raw", span=[10, len(line)], name="spawn.env:API_TOKEN")
      elif line.startswith(b"LINK="):
          at = line.index(b"?k=") + 3
          plain = __import__("urllib.parse").parse.unquote(line[at:].decode())
          place(cred_now, number, None, t="pct", span=[at, len(line)], sub=[0, len(plain.encode())],
                name="spawn.env:LINK")
      elif line.startswith(b"DB="):
          user = line.index(b"://") + 3
          colon = line.index(b":", user)
          at = line.index(b"@", colon)
          place(cred_now, number, None, t="short", form="pair", pre="", post="@", k=0,
                user={"t": "raw", "span": [user, colon]}, pw={"t": "raw", "span": [colon + 1, at]},
                name="spawn.env:DB")
if read(doc) is not None:
    leaf = json.loads(open(doc).read())["zeta"]["token"]
    # "zeta" is the second key in sorted order, "token" its only one.
    place(doc, None, [1, 0], t="raw", span=[0, len(leaf.encode())], name="creds.json:token")
# The directory new copies appear in.
out["watch"].append({"path": os.path.dirname(cred), "mtime_ns": os.stat(os.path.dirname(cred)).st_mtime_ns})
# Workspaces under a base, as a settings glob's: the base watched, each workspace's .claude probed
# (never the workspace itself), and a .claude's creds.env read like the file.
base = os.environ["BB_WSROOT"]
out["watch"].append({"path": base, "mtime_ns": os.stat(base).st_mtime_ns})
for workspace in sorted(glob.glob(glob.escape(base) + "/*")):
    claude = os.path.join(workspace, ".claude")
    out["probe"].append({"path": claude, "exists": os.path.isdir(claude)})
    path = os.path.join(claude, "creds.env")
    if os.path.isfile(path):
        data = read(path)
        for number, line in enumerate((data or b"").split(b"\n")):
            if line.startswith(b"API_TOKEN="):
                place(path, number, None, t="raw", span=[10, len(line)], name="creds.env:API_TOKEN")
# (9): once, after the answer is made and before it is printed, the file is rewritten in place
# from rewrite-after (the same size), its mtime set back to the one stamped.
after = os.path.join(os.path.dirname(os.path.dirname(cred)), "rewrite-after")
if os.path.exists(after):
    st = os.stat(cred)
    new = open(after, "rb").read()
    if len(new) == st.st_size:
        with open(cred, "r+b") as f:
            f.write(new)
        os.utime(cred, ns=(st.st_atime_ns, st.st_mtime_ns))
    os.remove(after)
print(json.dumps(out))
PY
mkdir -p "$WORK/cred"
value="made-up/token+0123456789abcdefXYZ"
value2="made-up/rotated+9876543210zyxwvuQRS"
linked="made-up/linked=4242424242aabbcc"
leaf="made-up-json-leaf-55556666777788"
# cred_file <API_TOKEN value> [unreadable]: the made-up credential file,
# written beside it and renamed over it, so a pass sees the old file or the
# new one whole; `unreadable` makes the new one unreadable before the
# rename (a rotation that cannot be read, never one a pass read between).
cred_file() {
    printf 'NOTE=a lone CR ends this line\rAPI_TOKEN=%s\nLINK=https://h.example/?k=%s\nDB=postgres://bb:mUp5x@127.0.0.1/d\n' \
        "$1" "$(printf '%s' "$linked" | jq -sRr @uri)" >"$WORK/cred/.spawn.env.new"
    if [ "${2-}" = unreadable ]; then chmod 000 "$WORK/cred/.spawn.env.new"; else chmod 600 "$WORK/cred/.spawn.env.new"; fi
    mv -f "$WORK/cred/.spawn.env.new" "$WORK/cred/spawn.env"
}
cred_file "$value"
chmod 600 "$WORK/cred/spawn.env"
jq -cn --arg leaf "$leaf" '{zeta: {token: $leaf}, alpha: 1}' >"$WORK/cred/creds.json"
printf '%s' "$value" >"$WORK/said"
printf '%s' "$linked" >"$WORK/linked"
printf '%s' "$leaf" >"$WORK/leaf"
# The three forms of each value, one per line, as a check reads them.
for v in "$value" "$value2"; do
    printf '%s\n' "$v" "$(printf '%s' "$v" | jq -sRr @uri)" "$(printf '%s' "$v" | base64 -w0)"
done >"$WORK/forms"
[ "$(sort -u "$WORK/forms" | wc -l)" -eq 6 ] || harness "the made-up values' forms are not six distinct strings"
BB_CRED=$WORK/cred/spawn.env
BB_DOC=$WORK/cred/creds.json
BB_WSROOT=$WORK/wsroot
mkdir -p "$BB_WSROOT/a/src"
SPAWN_ARCHIVE_LOCATIONS_TOOL=$WORK/locations.py
# The binary honours a stand-in only inside a gate's sandbox (175 R5): the
# gate sets the marker; set here too, so a run outside the gate is the same.
SPAWN_GATE_SANDBOX=1
export BB_CRED BB_DOC BB_WSROOT SPAWN_ARCHIVE_LOCATIONS_TOOL SPAWN_GATE_SANDBOX

# Spawn's tmux, on the private socket.
cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec '$real_tmux' -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

# `spawn loop` as systemd runs spawn-loop.service: started again 5 s after
# it exits (Restart=always, RestartSec=5).
cat >"$WORK/run-loop" <<'EOF'
#!/bin/sh
while [ ! -f "$WORK/loop.stop" ]; do
    "$SPAWN_BIN" loop --repo "$BB_REPO" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    child=$!
    printf '%s\n' "$child" >"$WORK/loop.pid"
    status=0
    wait "$child" || status=$?
    [ ! -f "$WORK/loop.stop" ] || exit 0
    sleep 5
done
EOF

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_GATE SPAWN_DISPATCH \
    SPAWN_SUPERVISOR SPAWN_LOOP_INTERVAL_MS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_RELAY http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
BB_REPO=$repo
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
CLAUDE_CONFIG_DIR=$WORK/claude-config
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
BB_SPAWN=$SPAWN_BIN
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_MESSAGE_TAKEN_SECS=6
SPAWN_ALERT_CHECK_SECS=1
# No stall is under test.
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
export SPAWN_DATABASE_URL BB_REPO SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS CLAUDE_CONFIG_DIR BB_PROJECTS BB_WORK BB_SPAWN SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_MESSAGE_TAKEN_SECS SPAWN_ALERT_CHECK_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS \
    SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$WORK/engine.log"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

start_loop() {
    rm -f "$WORK/loop.stop" "$WORK/loop.pid"
    sh "$WORK/run-loop" &
    wrapper_pid=$!
    i=0
    until [ -s "$WORK/loop.pid" ]; do
        i=$((i + 1))
        [ "$i" -lt 100 ] || harness "the loop did not start"
        sleep 0.1
    done
}
web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
start_serve() {
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
    base="http://127.0.0.1:$port"
}

# --- helpers ----------------------------------------------------------------

q() { scalar "$1"; }
put() { printf '%s\n' "$3" >"$WORK/state/$1.$2"; }
get() { cat "$WORK/state/$1.$2" 2>/dev/null || true; }
# send <label> <key> <text>: a person's input.
send() {
    capture "$SPAWN_BIN" submit --principal bb-person --agent "$(get "$1" agent)" \
        --session "$(get "$1" session)" --key "$2" \
        --action "$(jq -cn --arg text "$3" '{type: "input", text: $text}')"
    [ "$status" -eq 0 ] || harness "submit $2 failed: $(err)"
}
ended() { # <label>: how many turns the agent's engine has ended
    awk -v a="$(get "$1" agent)" '$2 == a && $3 == "turn-ended"' "$WORK/engine.log" | wc -l
}
# until_ended <label> <n> <seconds>: until the engine has ended n turns.
until_ended() {
    deadline=$(($(date +%s) + $3))
    until [ "$(ended "$1")" -ge "$2" ]; do
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
archive=$SPAWN_WORKSPACE_ROOT/.transcripts
archived() { # every archived byte of the agent, copies in order
    find "$archive/$(get "$1" agent)" -name '*.jsonl' -type f 2>/dev/null | sort | xargs -r cat
}
# forms_in <file> [first|second|both]: how many of the values' forms
# (three each: as written, percent-encoded, base64) the file holds.
forms_in() {
    case ${2-both} in first) range=1,3 ;; second) range=4,6 ;; *) range=1,6 ;; esac
    n=0
    while IFS= read -r form; do
        if grep -qF -- "$form" "$1"; then n=$((n + 1)); fi
    done <<FORMS
$(sed -n "${range}p" "$WORK/forms")
FORMS
    echo "$n"
}
marker='[REDACTED:spawn.env:API_TOKEN]'
markers_in() { grep -oF -- "$marker" "$1" | wc -l; }
# parts_in <file>: lines holding any part of a made-up value (each begins
# with `made-up`) or the short password.
parts_in() { grep -cE 'made-up|mUp5x' "$1" || true; }
# others_in <file>: how many of the other places' markers it holds.
others_in() {
    n=0
    for name in spawn.env:LINK spawn.env:DB creds.json:token; do
        if grep -qF -- "[REDACTED:$name]" "$1"; then n=$((n + 1)); fi
    done
    echo "$n"
}

# --- the agent -----------------------------------------------------------------

capture "$SPAWN_BIN" mission add --text "175 mask"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker teller \
    --brief "Do the 175 task: work as you are asked."
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(q "select max(id) from mission_tasks where mission_id = $mission")
put teller agent "$(q "select agent_id from mission_tasks where id = $task")"
put teller session "$(q "select session_id from mission_tasks where id = $task")"

start_loop
until_ended teller 1 120 || harness "the engine did not take its task and end its turn within 120 s"
note "the worker took its task"

# --- (1) a value is masked, the transcript untouched ----------------------------------

send teller bb-say-1 "BB:SAY the token, please"
until_ended teller 2 60 || harness "the BB:SAY turn did not end within 60 s"
deadline=$(($(date +%s) + 20))
while :; do
    archived teller >"$WORK/archive-1"
    # The turn's end is the last line the engine wrote: archived, it is done.
    if grep -q '"durationMs":2000' "$WORK/archive-1" && [ "$(grep -c '"durationMs":2000' "$WORK/archive-1")" -ge 2 ]; then break; fi
    [ "$(date +%s)" -lt "$deadline" ] || fail "(1) the BB:SAY turn was not archived within 20 s"
    sleep 0.5
done
transcript=$(find "$SPAWN_CLAUDE_PROJECTS" -name '*.jsonl' -type f | head -n 1)
native=$(basename "$transcript" .jsonl)
[ -n "$transcript" ] && [ -s "$WORK/mirror/$native.jsonl" ] || harness "no transcript or no mirror of it"
f1=$(forms_in "$WORK/archive-1")
m1=$(markers_in "$WORK/archive-1")
[ "$f1" -eq 0 ] ||
    fail "(1) the archive holds $f1 of the value's 3 forms (as written, percent-encoded, base64) after a turn that printed them: a value must be masked before it is copied"
[ "$(parts_in "$WORK/archive-1")" -eq 0 ] ||
    fail "(1) the archive holds part of a value on $(parts_in "$WORK/archive-1") line(s): every value must be masked whole (a line the engine is still writing is copied once whole)"
[ "$(others_in "$WORK/archive-1")" -eq 3 ] ||
    fail "(1) the archive holds $(others_in "$WORK/archive-1") of the 3 other places' markers (a percent-encoded part, a JSON leaf, a short password's shape)"
[ "$m1" -ge 3 ] ||
    fail "(1) the archive holds the marker $marker $m1 times; the turn printed the value in 3 forms"
cmp -s "$transcript" "$WORK/mirror/$native.jsonl" ||
    fail "(1) the transcript is not byte for byte what the engine wrote: the archive must never write it"
[ "$(forms_in "$transcript" first)" -eq 3 ] || harness "the transcript does not hold the value the engine printed"
note "(1) the archive masks the value (3 forms, $m1 markers); the transcript is untouched"

# --- (2) an unreadable credential file: nothing copied, then the waiting lines -------

# Rotated, then unreadable: the kept mask's file changed, and it cannot be read.
cred_file "$value2" unreadable
printf '%s' "$value2" >"$WORK/said"
cp "$WORK/archive-1" "$WORK/archive-before"
log_lines=$(wc -l <"$WORK/loop.log")
send teller bb-say-2 "BB:SAY the token again"
until_ended teller 3 60 || harness "the second BB:SAY turn did not end within 60 s"
sleep 8
archived teller >"$WORK/archive-2"
cmp -s "$WORK/archive-before" "$WORK/archive-2" ||
    fail "(2) with the credential file unreadable, the archive changed ($(wc -c <"$WORK/archive-before") -> $(wc -c <"$WORK/archive-2") bytes): a pass that cannot have every value must copy nothing"
tail -n +"$((log_lines + 1))" "$WORK/loop.log" >"$WORK/loop-2.log"
grep -F -- "$WORK/cred/spawn.env" "$WORK/loop-2.log" | grep -qi "mask" ||
    fail "(2) the loop's log does not say why nothing was copied, naming the credential file"
for log in "$WORK/loop.log" "$WORK/engine.log"; do
    [ "$(forms_in "$log")" -eq 0 ] || fail "(2) $(basename "$log") holds the value"
done
chmod 600 "$WORK/cred/spawn.env"
deadline=$(($(date +%s) + 20))
while :; do
    archived teller >"$WORK/archive-3"
    [ "$(grep -c '"durationMs":2000' "$WORK/archive-3")" -ge 3 ] && break
    [ "$(date +%s)" -lt "$deadline" ] ||
        fail "(2) the credential file readable again, the waiting turn was not archived within 20 s"
    sleep 0.5
done
f3=$(forms_in "$WORK/archive-3")
m3=$(markers_in "$WORK/archive-3")
[ "$f3" -eq 0 ] || fail "(2) the waiting lines were copied with $f3 of the value's forms in the clear"
[ "$(parts_in "$WORK/archive-3")" -eq 0 ] || fail "(2) the archive holds part of a value"
[ "$m3" -ge $((m1 + 3)) ] || fail "(2) the waiting turn's 3 forms of the new value are not masked: $m3 markers, $m1 before it"
cmp -s "$transcript" "$WORK/mirror/$native.jsonl" ||
    fail "(2) the transcript is not byte for byte what the engine wrote"
note "(2) unreadable: nothing copied, the log says why; readable again: the waiting lines copied masked ($m3 markers)"

# --- (3) a present file is never replaced by masked copies ----------------------------

executions() { q "select count(*) from agent_executions where session_id = '$(get teller session)'"; }
live_execution() { q "select count(*) from agent_executions where session_id = '$(get teller session)' and ended_at_ms is null"; }
# stop_and_resume <key>: stop the session's execution, run <between> while
# it is stopped, resume it, and wait for the resumed engine's first turn.
stop_session() {
    capture "$SPAWN_BIN" submit --principal bb-person --agent "$(get teller agent)" \
        --session "$(get teller session)" --key "$1" --action '{"type":"stop"}'
    [ "$status" -eq 0 ] || harness "submit stop $1 failed: $(err)"
    deadline=$(($(date +%s) + 60))
    until [ "$(live_execution)" -eq 0 ]; do
        [ "$(date +%s)" -lt "$deadline" ] || harness "the execution did not stop within 60 s"
        sleep 0.5
    done
}
resume_session() {
    turns=$(ended teller)
    capture "$SPAWN_BIN" submit --principal bb-person --agent "$(get teller agent)" \
        --session "$(get teller session)" --key "$1" --action '{"type":"resume","prompt":"go on"}'
    [ "$status" -eq 0 ] || harness "submit resume $1 failed: $(err)"
    until_ended teller $((turns + 1)) 90 || harness "the resumed engine did not end a turn within 90 s"
}

rm -f "$transcript"
# A plain turn: the new file holds no value, so its copy is its own bytes,
# and only the older, masked copy tells the join from the file.
send teller bb-plain-3 "go on with the work"
until_ended teller 4 60 || harness "the plain turn did not end within 60 s"
[ -s "$transcript" ] || harness "the engine wrote no new transcript"
deadline=$(($(date +%s) + 20))
until [ "$(find "$archive/$(get teller agent)" -name '*.jsonl' -type f | wc -l)" -ge 2 ]; do
    [ "$(date +%s)" -lt "$deadline" ] || fail "(3) the new file was not archived as a new copy within 20 s"
    sleep 0.5
done
stop_session bb-stop-1
# The copies' records lost: a copy that holds the marker is masked all the same.
rm -rf "$SPAWN_WORKSPACE_ROOT/.transcripts-sources"
cp "$transcript" "$WORK/headless"
resume_session bb-resume-1
head -c "$(wc -c <"$WORK/headless")" "$transcript" | cmp -s - "$WORK/headless" ||
    fail "(3) the transcript that lost its head was replaced at the resume: a present file must never be replaced by masked copies"
note "(3) a present file is left as it is at the resume"

# --- (4) a gone file is put back masked ------------------------------------------------

stop_session bb-stop-2
rm -f "$transcript"
# The archive before the restore: the next turn must go on in the same copy.
copies_4=$(find "$archive/$(get teller agent)" -name '*.jsonl' -type f | wc -l)
archived teller >"$WORK/archive-4"
log_lines=$(wc -l <"$WORK/loop.log")
resume_session bb-resume-2
restored_4=$(wc -c <"$transcript")
[ -s "$transcript" ] || fail "(4) the transcript gone at the resume was not put back"
[ "$(markers_in "$transcript")" -ge 3 ] ||
    fail "(4) the transcript put back holds the marker $(markers_in "$transcript") times: it must be the archive's masked copies"
[ "$(parts_in "$transcript")" -eq 0 ] ||
    fail "(4) the transcript put back holds part of a value: what the archive restores is masked"
tail -n +"$((log_lines + 1))" "$WORK/loop.log" | grep -q "was missing; restored" ||
    fail "(4) the loop did not say it restored the transcript"
# The put-back file is the transcript from now on: the next turn's lines go on
# in the same copy, and the restored bytes are not copied again (rev-e F1).
archived teller >"$WORK/archive-4t"
turns=$(ended teller)
send teller bb-say-4 "BB:SAY the first one"
until_ended teller $((turns + 1)) 60 || harness "the BB:SAY turn after the restore did not end within 60 s"
deadline=$(($(date +%s) + 20))
until [ "$(grep -c '"durationMs":2000' "$(archived teller >"$WORK/archive-4b"; echo "$WORK/archive-4b")")" -gt "$(grep -c '"durationMs":2000' "$WORK/archive-4t")" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || fail "(4) the turn after the restore was not archived within 20 s"
    sleep 0.5
done
copies_4b=$(find "$archive/$(get teller agent)" -name '*.jsonl' -type f | wc -l)
grown_4=$(($(wc -c <"$WORK/archive-4b") - $(wc -c <"$WORK/archive-4")))
[ "$copies_4b" -eq "$copies_4" ] && [ "$grown_4" -lt "$restored_4" ] ||
    fail "(4) after the restore ($restored_4 bytes) and a turn, the archive has $((copies_4b - copies_4)) new copy(ies) and grew by $grown_4 bytes: the restored file must be archived on in the same copy, never copied whole again"
[ "$(parts_in "$WORK/archive-4b")" -eq 0 ] || fail "(4) the turn after the restore was archived with part of a value"
note "(4) a gone file is put back masked; the next turn goes on in the same copy"

# --- (5) a resume without the mask copies nothing --------------------------------------

cred_file "$value" unreadable
printf '%s' "$value" >"$WORK/said"
archived teller >"$WORK/archive-5"
send teller bb-say-5 "BB:SAY the first one"
until_ended teller "$(($(ended teller) + 1))" 60 || harness "the fifth BB:SAY turn did not end within 60 s"
stop_session bb-stop-3
cp "$transcript" "$WORK/before-5"
resume_session bb-resume-3
sleep 5
archived teller >"$WORK/archive-5b"
cmp -s "$WORK/archive-5" "$WORK/archive-5b" ||
    fail "(5) the archive changed while the mask could not be had ($(wc -c <"$WORK/archive-5") -> $(wc -c <"$WORK/archive-5b") bytes), across a resume: nothing may be copied without it"
head -c "$(wc -c <"$WORK/before-5")" "$transcript" | cmp -s - "$WORK/before-5" ||
    fail "(5) the transcript was replaced at a resume without the mask"
chmod 600 "$WORK/cred/spawn.env"
deadline=$(($(date +%s) + 20))
until [ "$(wc -c <"$(archived teller >"$WORK/archive-5c"; echo "$WORK/archive-5c")")" -gt "$(wc -c <"$WORK/archive-5")" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || fail "(5) the mask had again, the waiting lines were not archived within 20 s"
    sleep 0.5
done
[ "$(parts_in "$WORK/archive-5c")" -eq 0 ] || fail "(5) the waiting lines were archived with part of a value"
note "(5) a resume without the mask copies nothing; the waiting lines are copied masked once it is had"

# --- (6) a new older copy is masked from the first pass that can see it ------------------

older="made-up/older+copy0123456789abcdef"
printf 'API_TOKEN=%s\n' "$older" >"$WORK/cred/spawn.env.bak"
chmod 600 "$WORK/cred/spawn.env.bak"
printf '%s' "$older" >"$WORK/said"
archived teller >"$WORK/archive-6"
turns=$(ended teller)
send teller bb-say-6 "BB:SAY the older one"
until_ended teller $((turns + 1)) 60 || harness "the sixth BB:SAY turn did not end within 60 s"
deadline=$(($(date +%s) + 20))
until [ "$(grep -c '"durationMs":2000' "$(archived teller >"$WORK/archive-6b"; echo "$WORK/archive-6b")")" -gt "$(grep -c '"durationMs":2000' "$WORK/archive-6")" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || fail "(6) the turn after a new older copy was not archived within 20 s"
    sleep 0.5
done
grep -qF -- "$older" "$WORK/archive-6b" &&
    fail "(6) a value in a new older copy was archived in the clear: the tool must run again when its watched directory changes"
[ "$(parts_in "$WORK/archive-6b")" -eq 0 ] || fail "(6) the archive holds part of a value"
note "(6) a new older copy's value is masked from the first pass that sees it"

# --- (7) a .claude appearing in a workspace is seen by the probe -------------------------

probed="made-up/probed+claude0123456789ab"
mkdir -p "$BB_WSROOT/a/.claude"
printf 'API_TOKEN=%s\n' "$probed" >"$BB_WSROOT/a/.claude/creds.env"
printf '%s' "$probed" >"$WORK/said"
archived teller >"$WORK/archive-7"
turns=$(ended teller)
send teller bb-say-7 "BB:SAY the probed one"
until_ended teller $((turns + 1)) 60 || harness "the seventh BB:SAY turn did not end within 60 s"
deadline=$(($(date +%s) + 20))
until [ "$(grep -c '"durationMs":2000' "$(archived teller >"$WORK/archive-7b"; echo "$WORK/archive-7b")")" -gt "$(grep -c '"durationMs":2000' "$WORK/archive-7")" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || fail "(7) the turn after a new .claude was not archived within 20 s"
    sleep 0.5
done
grep -qF -- "$probed" "$WORK/archive-7b" &&
    fail "(7) a value in a new workspace .claude was archived in the clear: the tool must run again when a probed path appears"
[ "$(parts_in "$WORK/archive-7b")" -eq 0 ] || fail "(7) the archive holds part of a value"
note "(7) a .claude added inside a workspace is seen by its probe; its value is masked"

# --- (8) a same-size rewrite with its mtime set back is seen ---------------------------

value8=$(printf '%s' "$value" | tr '0-9' '1-90')
[ "${#value8}" -eq "${#value}" ] && [ "$value8" != "$value" ] || harness "the rewritten value is not a same-size other value"
size_8=$(wc -c <"$WORK/cred/spawn.env")
touch -r "$WORK/cred/spawn.env" "$WORK/mtime-ref"
printf 'NOTE=a lone CR ends this line\rAPI_TOKEN=%s\nLINK=https://h.example/?k=%s\nDB=postgres://bb:mUp5x@127.0.0.1/d\n' \
    "$value8" "$(printf '%s' "$linked" | jq -sRr @uri)" >"$WORK/cred/spawn.env"
touch -r "$WORK/mtime-ref" "$WORK/cred/spawn.env"
[ "$(wc -c <"$WORK/cred/spawn.env")" -eq "$size_8" ] || harness "the rewrite changed the file's size"
[ "$(stat -c %Y.%y "$WORK/cred/spawn.env")" = "$(stat -c %Y.%y "$WORK/mtime-ref")" ] || harness "the mtime was not set back"
printf '%s' "$value8" >"$WORK/said"
archived teller >"$WORK/archive-8"
turns=$(ended teller)
send teller bb-say-8 "BB:SAY the rewritten one"
until_ended teller $((turns + 1)) 60 || harness "the eighth BB:SAY turn did not end within 60 s"
deadline=$(($(date +%s) + 20))
until [ "$(grep -c '"durationMs":2000' "$(archived teller >"$WORK/archive-8b"; echo "$WORK/archive-8b")")" -gt "$(grep -c '"durationMs":2000' "$WORK/archive-8")" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || fail "(8) the turn after a same-size rewrite was not archived within 20 s"
    sleep 0.5
done
grep -qF -- "$value8" "$WORK/archive-8b" &&
    fail "(8) a value rewritten in place, same size, its mtime set back, was archived in the clear: a source that is not the file read (its ctime moved) must build the mask again"
[ "$(parts_in "$WORK/archive-8b")" -eq 0 ] || fail "(8) the archive holds part of a value"
note "(8) a same-size rewrite with its mtime set back is masked"

# --- (9) a rewrite between the tool's answer and the Bridge's read is refused -----------

value9="made-up/r4+01234567"
plain9="an ordinary setting of 33 bytes!!"
[ "$(printf 'API_TOKEN=%s' "$value9" | wc -c)" -eq 29 ] && [ "${#plain9}" -eq "${#value8}" ] ||
    harness "the (9) lines do not keep the file's size"
# Line 0 (the NOTE line, 29 bytes) becomes the new credential's line; line 1 (the placed one)
# an ordinary setting of the old value's length.
printf 'API_TOKEN=%s\rPLAIN_SET=%s\nLINK=https://h.example/?k=%s\nDB=postgres://bb:mUp5x@127.0.0.1/d\n' \
    "$value9" "$plain9" "$(printf '%s' "$linked" | jq -sRr @uri)" >"$WORK/rewrite-after"
[ "$(wc -c <"$WORK/rewrite-after")" -eq "$(wc -c <"$WORK/cred/spawn.env")" ] || harness "the (9) rewrite is not the file's size"
# A new mtime makes the kept mask stale: the turn's lines make the Bridge run the tool, and the
# tool rewrites the file right after its answer, before the Bridge reads it.
touch "$WORK/cred/spawn.env"
printf '%s' "$value9" >"$WORK/said"
archived teller >"$WORK/archive-9"
turns=$(ended teller)
send teller bb-say-9 "BB:SAY the one moved after the answer"
until_ended teller $((turns + 1)) 60 || harness "the ninth BB:SAY turn did not end within 60 s"
deadline=$(($(date +%s) + 20))
until [ "$(grep -c '"durationMs":2000' "$(archived teller >"$WORK/archive-9b"; echo "$WORK/archive-9b")")" -gt "$(grep -c '"durationMs":2000' "$WORK/archive-9")" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || fail "(9) the turn after a rewrite behind the tool's answer was not archived within 20 s"
    sleep 0.5
done
# The rewrite happened behind an answer (else this check tested nothing).
[ ! -e "$WORK/rewrite-after" ] || harness "the tool was not run for the ninth turn: the rewrite never happened"
grep -qF -- "PLAIN_SET=$plain9" "$WORK/cred/spawn.env" || harness "the stand-in did not rewrite the file"
grep -qF -- "$value9" "$WORK/archive-9b" &&
    fail "(9) a value moved in place after the tool's answer (same size, mtime set back) was archived in the clear: a source whose device, inode or ctime is not the tool's stamp must be refused"
[ "$(parts_in "$WORK/archive-9b")" -eq 0 ] || fail "(9) the archive holds part of a value"
note "(9) a rewrite between the tool's answer and the Bridge's read is refused; the moved value is masked"

# --- (12) an accepted stand-in is said -----------------------------------------------------
grep -qF "spawn: WARNING: the transcript archive is masked by the stand-in location tool $SPAWN_ARCHIVE_LOCATIONS_TOOL (SPAWN_GATE_SANDBOX=1), not the installed collector, for the roots $SPAWN_WORKSPACE_ROOT and $SPAWN_CLAUDE_PROJECTS" "$WORK/loop.log" ||
    fail "(12) the loop's log does not say WARNING with the stand-in it accepted and its roots: $(grep -m1 'WARNING' "$WORK/loop.log" | cut -c1-240)"
note "(12) an accepted stand-in is said with its roots"

# --- (13) a link below the roots is never followed (R7) ---------------------------------
stop_loop
mkdir -p "$WORK/elsewhere" || harness "no directory for (13)"
mv "$archive" "$WORK/archive-kept" || harness "the archive could not be moved aside"
ln -s "$WORK/elsewhere" "$archive" || harness "the archive link could not be made"
log_lines=$(wc -l <"$WORK/loop.log")
start_loop
turns=$(ended teller)
send teller bb-say-13 "BB:SAY the one said with the archive a link"
until_ended teller $((turns + 1)) 60 || harness "the BB:SAY turn with the archive a link did not end within 60 s"
sleep 8
[ -z "$(find "$WORK/elsewhere" -mindepth 1 2>/dev/null)" ] ||
    fail "(13) the archive wrote through a .transcripts that is a link: $(find "$WORK/elsewhere" -mindepth 1 | head -n 3 | tr '\n' ' ')"
tail -n "+$((log_lines + 1))" "$WORK/loop.log" | grep -qF "not followed" ||
    fail "(13) with the archive a link, the loop's log does not say the link is not followed: $(tail -n "+$((log_lines + 1))" "$WORK/loop.log" | grep -m1 'transcripts' | cut -c1-200)"
stop_loop
rm -f "$archive" || harness "the archive link could not be removed"
mv "$WORK/archive-kept" "$archive" || harness "the archive could not be put back"
native_file=$(find "$SPAWN_CLAUDE_PROJECTS" -type f -name '*.jsonl' | head -n 1)
[ -n "$native_file" ] || harness "no transcript under $SPAWN_CLAUDE_PROJECTS for (13)"
project=$(dirname "$native_file")
mkdir -p "$WORK/elsewhere-projects" || harness "no directory for (13)'s projects"
mv "$project" "$WORK/elsewhere-projects/" || harness "the project directory could not be moved aside"
ln -s "$WORK/elsewhere-projects/$(basename "$project")" "$project" || harness "the project link could not be made"
log_lines=$(wc -l <"$WORK/loop.log")
start_loop
archived teller >"$WORK/archive-13"
turns=$(ended teller)
send teller bb-say-13b "BB:SAY the one said with the project directory a link"
until_ended teller $((turns + 1)) 60 || harness "the BB:SAY turn with the project a link did not end within 60 s"
sleep 8
archived teller >"$WORK/archive-13b"
cmp -s "$WORK/archive-13" "$WORK/archive-13b" ||
    fail "(13) with the engine's project directory a link, its turn was archived: a link below the transcripts root must not be followed"
tail -n "+$((log_lines + 1))" "$WORK/loop.log" | grep -qF "not followed" ||
    fail "(13) with the project directory a link, the loop's log does not say it is not followed: $(tail -n "+$((log_lines + 1))" "$WORK/loop.log" | grep -m1 'jsonl' | cut -c1-200)"
stop_loop
rm -f "$project" || harness "the project link could not be removed"
mv "$WORK/elsewhere-projects/$(basename "$project")" "$project" || harness "the project directory could not be put back"
# The copy written last (the one the next turn appends to; names do not sort by copy).
copy=$(find "$archive/$(get teller agent)" -name '*.jsonl' -type f -printf '%T@ %p\n' | sort -n | tail -n 1 | cut -d' ' -f2-)
[ -n "$copy" ] || harness "no copy of the teller for (13)"
mkdir -p "$SPAWN_WORKSPACE_ROOT/inside" || harness "no directory inside the workspace root for (13)"
cp "$copy" "$SPAWN_WORKSPACE_ROOT/inside/copy.jsonl" || harness "the copy could not be put inside the root"
cp "$copy" "$WORK/copy-kept" || harness "the copy could not be kept"
rm -f "$copy" || harness "the copy could not be removed"
# Relative, so it resolves below the workspace root: only RESOLVE_NO_SYMLINKS refuses it.
ln -s ../../inside/copy.jsonl "$copy" || harness "the copy's link could not be made"
log_lines=$(wc -l <"$WORK/loop.log")
start_loop
turns=$(ended teller)
send teller bb-say-13c "BB:SAY the one said with the agent's copy a link inside the root"
until_ended teller $((turns + 1)) 60 || harness "the BB:SAY turn with the copy a link did not end within 60 s"
# Up to 20 s for the turn's lines to reach the file through the link (they never should).
deadline=$(($(date +%s) + 20))
while [ "$(date +%s)" -lt "$deadline" ]; do
    cmp -s "$WORK/copy-kept" "$SPAWN_WORKSPACE_ROOT/inside/copy.jsonl" ||
        fail "(13) the archive wrote through a copy that links to a file inside the workspace root"
    sleep 0.5
done
tail -n "+$((log_lines + 1))" "$WORK/loop.log" | grep -qF "not followed" ||
    fail "(13) with the agent's copy a link inside the root, the loop's log does not say it is not followed"
stop_loop
rm -f "$copy" || harness "the copy's link could not be removed"
mv "$WORK/copy-kept" "$copy" || harness "the copy could not be put back"
copy=$(find "$archive/$(get teller agent)" -name '*.jsonl' -type f -printf '%T@ %p\n' | sort -n | tail -n 1 | cut -d' ' -f2-)
cp "$copy" "$WORK/outside-copy" || harness "the copy could not be put outside the root"
rm -f "$copy" || harness "the copy could not be removed"
ln "$WORK/outside-copy" "$copy" || harness "the copy's hard link could not be made"
cp "$WORK/outside-copy" "$WORK/outside-kept" || harness "the outside copy could not be kept"
log_lines=$(wc -l <"$WORK/loop.log")
start_loop
turns=$(ended teller)
send teller bb-say-13d "BB:SAY the one said with the agent's copy a hard link outside the root"
until_ended teller $((turns + 1)) 60 || harness "the BB:SAY turn with the copy a hard link did not end within 60 s"
deadline=$(($(date +%s) + 20))
while [ "$(date +%s)" -lt "$deadline" ]; do
    cmp -s "$WORK/outside-kept" "$WORK/outside-copy" ||
        fail "(13) the archive wrote into a copy that is a hard link to a file outside the workspace root"
    sleep 0.5
done
tail -n "+$((log_lines + 1))" "$WORK/loop.log" | grep -qF "hard links: not followed" ||
    fail "(13) with the agent's copy a hard link, the loop's log does not say it is not followed"
stop_loop
rm -f "$copy" || harness "the copy's hard link could not be removed"
cp "$WORK/outside-kept" "$copy" || harness "the copy could not be put back"
# R8 (cc-astra A88): the projects root inside the teller's own workspace.
# The tree moves into the workspace and the engine keeps writing through a
# link left at its old place; the loop starts again with
# SPAWN_CLAUDE_PROJECTS there, so a native path starts with the workspace's.
# The teller's native transcript then gets a second name outside the roots
# (a hard link): its turn must not be archived.
teller_ws=$SPAWN_WORKSPACE_ROOT/$(get teller agent)
[ -d "$teller_ws" ] || harness "no workspace $teller_ws for the teller"
teller_project=$SPAWN_CLAUDE_PROJECTS/$(printf '%s' "$teller_ws" | sed 's/[^A-Za-z0-9]/-/g')
teller_native=$(find "$teller_project" -maxdepth 1 -type f -name '*.jsonl' -printf '%T@ %p\n' 2>/dev/null | sort -n | tail -n 1 | cut -d' ' -f2-)
[ -n "$teller_native" ] || harness "no native transcript of the teller under $teller_project"
projects_kept=$SPAWN_CLAUDE_PROJECTS
inside=$teller_ws/projects
[ ! -e "$inside" ] || harness "$inside is there already"
mv "$projects_kept" "$inside" || harness "the projects tree could not be moved into the workspace"
ln -s "$inside" "$projects_kept" || harness "the projects link could not be made"
SPAWN_CLAUDE_PROJECTS=$inside
export SPAWN_CLAUDE_PROJECTS
inside_native=$inside/${teller_native#"$projects_kept"/}
[ -f "$inside_native" ] || harness "the teller's native transcript is not at $inside_native"
ln "$inside_native" "$WORK/outside-native" || harness "the native transcript's second name could not be made"
log_lines=$(wc -l <"$WORK/loop.log")
start_loop
archived teller >"$WORK/archive-13e"
turns=$(ended teller)
send teller bb-say-13e "BB:SAY the one said with the projects inside the workspace and the transcript hard-linked"
until_ended teller $((turns + 1)) 60 || harness "the BB:SAY turn with the projects inside the workspace did not end within 60 s"
sleep 8
archived teller >"$WORK/archive-13e2"
cmp -s "$WORK/archive-13e" "$WORK/archive-13e2" ||
    fail "(13) with the projects root inside the workspace, a native transcript with a second name outside was archived: it must be read through the pinned projects root"
tail -n "+$((log_lines + 1))" "$WORK/loop.log" | grep -qF "hard links: not followed" ||
    fail "(13) with the projects root inside the workspace, the loop's log does not say the hard-linked transcript is not followed"
stop_loop
rm -f "$WORK/outside-native" || harness "the native transcript's second name could not be removed"
rm -f "$projects_kept" || harness "the projects link could not be removed"
mv "$inside" "$projects_kept" || harness "the projects tree could not be put back"
SPAWN_CLAUDE_PROJECTS=$projects_kept
export SPAWN_CLAUDE_PROJECTS
note "(13) a link below the roots is never followed, and the loop says so"

# --- (10) a stand-in outside a gate's sandbox is refused ---------------------------------

refused_turn() { # refused_turn <n> <what>: the loop started again as it is set; a turn archives nothing
    stop_loop
    log_lines=$(wc -l <"$WORK/loop.log")
    start_loop
    archived teller >"$WORK/archive-$1"
    turns=$(ended teller)
    send teller "bb-say-$1" "BB:SAY the one said with $2"
    until_ended teller $((turns + 1)) 60 || harness "the BB:SAY turn with $2 did not end within 60 s"
    sleep 8
    archived teller >"$WORK/archive-$1b"
    cmp -s "$WORK/archive-$1" "$WORK/archive-$1b" ||
        fail "(10) with a stand-in named and $2, the turn was archived: a stand-in must be refused outside a gate's sandbox"
    tail -n "+$((log_lines + 1))" "$WORK/loop.log" | grep -qF "SPAWN_ARCHIVE_LOCATIONS_TOOL is honoured only in a gate's sandbox" ||
        fail "(10) with $2, the loop's log does not say the stand-in is honoured only in a gate's sandbox: $(tail -n "+$((log_lines + 1))" "$WORK/loop.log" | grep -m1 'archive' | cut -c1-200)"
}
unset SPAWN_GATE_SANDBOX
refused_turn 10 "no SPAWN_GATE_SANDBOX"
SPAWN_GATE_SANDBOX=0
export SPAWN_GATE_SANDBOX
refused_turn 11 "SPAWN_GATE_SANDBOX=0"
note "(10) a stand-in outside a gate's sandbox is refused, and the loop says so"

echo "175: the transcript archive masks credential values"
exit 0
