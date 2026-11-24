#!/bin/sh
# Objective 122 [recoverability]: the store of secrets by name, and its admin
# command (sync/design-secret-injection.md, section 5 B, under section 0's
# owner decisions: nothing injectable, no install here).
#
# `spawn secret` keeps secrets in a store directory that SPAWN_SECRET_STORE names
# (an existing directory of the running user's, mode 700, not a link): one file
# per name holding the value exactly (<store>/<NAME>, 600), and its metadata in
# <store>/<NAME>.json (600; name, tier, created; never the value).
#   set NAME [--tier 1]     the value from a pipe or a terminal (no echo),
#                           never an argument, a file or /dev/null
#   generate NAME --bytes N N random bytes (16..1024), base64url, made inside
#                           the store
#   list                    names, tiers, creation times; never a value
# No command prints a value (there is no get); tier 2 is refused (nothing is
# injectable); an existing name is refused; a refusal is exit 2 with a sentence.
#
# Black box, made-up values (random per run) under a scratch root:
#   (1) a store that is unset, relative, missing, mode 755 or a symlink is
#       refused (exit 2) by every command, and nothing is written;
#   (2) list on an empty store prints nothing, exit 0;
#   (3) set from a pipe stores the value exactly (one "\n" or "\r\n" dropped),
#       600, with metadata that holds no value;
#   (4) a value given as an argument is refused and never repeated;
#   (5) stdin that is /dev/null or a regular file is refused; an empty value is
#       refused; nothing is stored;
#   (6) an existing name is refused and keeps its value;
#   (7) --tier 2 is refused as not injectable, before any value is read;
#   (8) generate makes N random bytes as base64url, different each time;
#       --bytes outside 16..1024 is refused;
#   (9) list names every secret with tier=1 and created=, and no value; the
#       store holds each secret's two files and nothing else;
#  (10) get (and any other verb) is refused with the usage, exit 2;
#  (11) a name that is not [A-Z][A-Z0-9_]{0,63} is refused and creates nothing;
#  (12) from a terminal, the value is read without echo;
#  (13) while set reads, no process's argv holds the value;
#  (14) set killed in the middle of reading stores nothing, and the part read
#       is in no file;
#  (15) no value is in any output or in any file outside the store;
#  (16) what a write killed after its read leaves (a staged copy, a value
#       without its metadata, metadata without its value; planted by hand)
#       is named by list, never with a value, and cleared by the next set or
#       generate (the staged copy) or by a set or generate of that name.
#  (12) also: a character erased at the terminal is not part of the value;
#       Ctrl-C at the prompt stores nothing, exits 2, and leaves echo on.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "122: harness: $*" >&2
    exit 2
}
[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
command -v python3 >/dev/null 2>&1 || harness "python3 is not installed"
command -v mkfifo >/dev/null 2>&1 || harness "mkfifo is not installed"

WORK=$(mktemp -d) || harness "mktemp failed"
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -m 700 "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
TMPDIR=$WORK/tmp
mkdir -m 700 "$TMPDIR"
export TMPDIR
finish() {
    rc=$?
    trap - EXIT
    [ -z "${bgpid-}" ] || kill -9 "$bgpid" 2>/dev/null || true
    rm -rf "$WORK"
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "122: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM
: >"$WORK/failures"
check_failed() {
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
: >"$WORK/printed"
: >"$WORK/values"
made_up() { # a made-up value, random per run, remembered for (15)
    v="made-up-$1-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
    printf '%s\n' "$v" >>"$WORK/values"
    printf '%s' "$v"
}
STORE=$WORK/store
mkdir -m 700 "$STORE"
SPAWN_SECRET_STORE=$STORE
export SPAWN_SECRET_STORE
# run <tag> <spawn args...>: stdin as given; stdout/stderr kept for (15)
run() {
    tag=$1
    shift
    status=0
    "$SPAWN_BIN" "$@" >"$WORK/$tag.out" 2>"$WORK/$tag.err" || status=$?
    cat "$WORK/$tag.out" "$WORK/$tag.err" >>"$WORK/printed"
}
# piped <tag> <bytes> <spawn args...>: the bytes on a pipe (in this shell, so status is kept)
nl='
'
piped() {
    tag=$1
    data=$2
    shift 2
    status=0
    printf '%s' "$data" | "$SPAWN_BIN" "$@" >"$WORK/$tag.out" 2>"$WORK/$tag.err" || status=$?
    cat "$WORK/$tag.out" "$WORK/$tag.err" >>"$WORK/printed"
}
err() { head -c 300 "$WORK/$1.err" | tr '\n' ' '; }
names_in_store() { (cd "$STORE" && ls -A) | tr '\n' ' '; }

# --- (1) the store itself ----------------------------------------------------------------------------
v_store=$(made_up store)
status=0
env -u SPAWN_SECRET_STORE "$SPAWN_BIN" secret list >"$WORK/1a.out" 2>"$WORK/1a.err" || status=$?
cat "$WORK/1a.out" "$WORK/1a.err" >>"$WORK/printed"
[ "$status" -eq 2 ] && grep -q "SPAWN_SECRET_STORE is not set" "$WORK/1a.err" ||
    check_failed "(1) with SPAWN_SECRET_STORE unset, list exited $status: $(err 1a)"
mkdir -m 755 "$WORK/open"
mkdir -m 700 "$WORK/target"
ln -s "$WORK/target" "$WORK/link"
for bad in relative/store "$WORK/missing" "$WORK/open" "$WORK/link"; do
    for verb in list set generate; do
        status=0
        case $verb in
        list) SPAWN_SECRET_STORE=$bad "$SPAWN_BIN" secret list >"$WORK/1b.out" 2>"$WORK/1b.err" </dev/null || status=$? ;;
        set) printf '%s\n' "$v_store" | SPAWN_SECRET_STORE=$bad "$SPAWN_BIN" secret set BB_STORE >"$WORK/1b.out" 2>"$WORK/1b.err" || status=$? ;;
        generate) SPAWN_SECRET_STORE=$bad "$SPAWN_BIN" secret generate BB_STORE --bytes 32 >"$WORK/1b.out" 2>"$WORK/1b.err" </dev/null || status=$? ;;
        esac
        cat "$WORK/1b.out" "$WORK/1b.err" >>"$WORK/printed"
        [ "$status" -eq 2 ] && grep -q "must be an existing directory of yours, mode 700" "$WORK/1b.err" ||
            check_failed "(1) store $bad, $verb: exit $status, not 2 with 'must be an existing directory of yours, mode 700': $(err 1b)"
    done
done
[ ! -e "$WORK/missing" ] || check_failed "(1) the missing store was created"
[ -z "$(ls -A "$WORK/open")" ] && [ -z "$(ls -A "$WORK/target")" ] || check_failed "(1) something was written in a refused store"
echo "122: (1) checked: unset, relative, missing, 755 and symlinked stores are refused" >&2

# --- (2) an empty store ----------------------------------------------------------------------------------
run 2 secret list </dev/null
[ "$status" -eq 0 ] && [ ! -s "$WORK/2.out" ] || check_failed "(2) list on an empty store: exit $status, $(wc -c <"$WORK/2.out" | tr -d ' ') bytes printed"
echo "122: (2) checked: an empty store lists nothing" >&2

# --- (3) set from a pipe -----------------------------------------------------------------------------------
v1=$(made_up one)
piped 3 "$v1$nl" secret set BB_ONE
[ "$status" -eq 0 ] || check_failed "(3) set BB_ONE from a pipe exited $status: $(err 3)"
if [ -f "$STORE/BB_ONE" ]; then
    [ "$(cat "$STORE/BB_ONE"; echo .)" = "$v1." ] || check_failed "(3) <store>/BB_ONE does not hold the value exactly (one line end dropped)"
    [ "$(stat -c %a "$STORE/BB_ONE")" = 600 ] || check_failed "(3) <store>/BB_ONE is mode $(stat -c %a "$STORE/BB_ONE"), not 600"
else
    check_failed "(3) no <store>/BB_ONE after set"
fi
if [ -f "$STORE/BB_ONE.json" ]; then
    [ "$(stat -c %a "$STORE/BB_ONE.json")" = 600 ] || check_failed "(3) <store>/BB_ONE.json is not mode 600"
    meta=$(python3 -I -c 'import json,sys; m=json.load(open(sys.argv[1])); print(m.get("name"), m.get("tier"), bool(m.get("created")))' "$STORE/BB_ONE.json" 2>/dev/null || echo unreadable)
    [ "$meta" = "BB_ONE 1 True" ] || check_failed "(3) <store>/BB_ONE.json is not {name: BB_ONE, tier: 1, created}: $meta"
    ! printf '%s\n' "$v1" | grep -qF -f - "$STORE/BB_ONE.json" || check_failed "(3) <store>/BB_ONE.json holds the value"
else
    check_failed "(3) no <store>/BB_ONE.json after set"
fi
# one line end is "\n" or "\r\n"; either is dropped whole
v1c=$(made_up crlf)
cr=$(printf '\r')
piped 3c "$v1c$cr$nl" secret set BB_CRLF
[ "$status" -eq 0 ] && [ "$(cat "$STORE/BB_CRLF" 2>/dev/null; echo .)" = "$v1c." ] ||
    check_failed "(3) set from a pipe of value + CR LF: exit $status, or <store>/BB_CRLF is not the value exactly"
echo "122: (3) checked: set from a pipe" >&2

# --- (4) a value as an argument --------------------------------------------------------------------------
v2=$(made_up argv)
run 4 secret set BB_TWO "$v2" </dev/null
[ "$status" -eq 2 ] || check_failed "(4) set with the value as an argument exited $status, not 2"
printf '%s\n' "$v2" | grep -qF -f - "$WORK/4.out" "$WORK/4.err" && check_failed "(4) the refusal repeats the value given as an argument"
[ ! -e "$STORE/BB_TWO" ] || check_failed "(4) the value given as an argument was stored"
echo "122: (4) checked: a value is never an argument" >&2

# --- (5) stdin that is neither a pipe nor a terminal; an empty value ------------------------------------
run 5a secret set BB_THREE </dev/null
[ "$status" -eq 2 ] || check_failed "(5) set with stdin /dev/null exited $status, not 2"
v3=$(made_up file)
umask 077
printf '%s\n' "$v3" >"$WORK/value-in-a-file"
umask 022
run 5b secret set BB_THREE <"$WORK/value-in-a-file"
[ "$status" -eq 2 ] || check_failed "(5) set with stdin a regular file exited $status, not 2"
piped 5c "" secret set BB_THREE
[ "$status" -eq 2 ] && grep -qi "empty" "$WORK/5c.err" || check_failed "(5) an empty value: exit $status: $(err 5c)"
piped 5d "$nl" secret set BB_THREE
[ "$status" -eq 2 ] || check_failed "(5) a value of one line end exited $status, not 2"
[ ! -e "$STORE/BB_THREE" ] && [ ! -e "$STORE/BB_THREE.json" ] || check_failed "(5) something was stored for BB_THREE"
echo "122: (5) checked: /dev/null, a file and an empty value are refused" >&2

# --- (6) an existing name ---------------------------------------------------------------------------------
v4=$(made_up again)
piped 6 "$v4$nl" secret set BB_ONE
[ "$status" -eq 2 ] || check_failed "(6) set on an existing name exited $status, not 2"
[ "$(cat "$STORE/BB_ONE" 2>/dev/null; echo .)" = "$v1." ] || check_failed "(6) the existing value changed"
echo "122: (6) checked: an existing name keeps its value" >&2

# --- (7) the tier -------------------------------------------------------------------------------------------
v5=$(made_up tier)
piped 7a "$v5$nl" secret set BB_TIER --tier 2
[ "$status" -eq 2 ] && grep -qi "injectable" "$WORK/7a.err" || check_failed "(7) --tier 2: exit $status, not refused as not injectable: $(err 7a)"
[ ! -e "$STORE/BB_TIER" ] || check_failed "(7) a tier 2 value was stored"
piped 7b "$v5$nl" secret set BB_TIER --tier 3
[ "$status" -eq 2 ] && [ ! -e "$STORE/BB_TIER" ] || check_failed "(7) --tier 3: exit $status"
v5b=$(made_up tierone)
piped 7c "$v5b$nl" secret set BB_TIER --tier 1
[ "$status" -eq 0 ] && [ "$(cat "$STORE/BB_TIER" 2>/dev/null; echo .)" = "$v5b." ] || check_failed "(7) --tier 1: exit $status: $(err 7c)"
echo "122: (7) checked: nothing is injectable" >&2

# --- (8) generate ------------------------------------------------------------------------------------------
run 8a secret generate BB_GEN1 --bytes 32 </dev/null
s1=$status
run 8b secret generate BB_GEN2 --bytes 32 </dev/null
g1=$(cat "$STORE/BB_GEN1" 2>/dev/null || true)
g2=$(cat "$STORE/BB_GEN2" 2>/dev/null || true)
printf '%s\n%s\n' "$g1" "$g2" >>"$WORK/values"
[ "$s1" -eq 0 ] && [ "$status" -eq 0 ] || check_failed "(8) generate exited $s1 and $status: $(err 8a)"
for g in "$g1" "$g2"; do
    printf '%s' "$g" | grep -Eq '^[A-Za-z0-9_-]{43}$' || check_failed "(8) a generated value is not 32 bytes as base64url without padding (43 characters)"
done
[ "$g1" != "$g2" ] || check_failed "(8) two generated values are equal"
[ "$(stat -c %a "$STORE/BB_GEN1" 2>/dev/null)" = 600 ] || check_failed "(8) <store>/BB_GEN1 is not mode 600"
for count in 8 15 1025 x; do
    run 8c secret generate BB_GEN3 --bytes "$count" </dev/null
    [ "$status" -eq 2 ] || check_failed "(8) generate --bytes $count exited $status, not 2"
done
[ ! -e "$STORE/BB_GEN3" ] || check_failed "(8) a refused generate stored a value"
echo "122: (8) checked: generate" >&2

# --- (9) list -------------------------------------------------------------------------------------------------
run 9 secret list </dev/null
[ "$status" -eq 0 ] || check_failed "(9) list exited $status"
for name in BB_CRLF BB_GEN1 BB_GEN2 BB_ONE BB_TIER; do
    grep -E "^$name +tier=1 +created=[0-9]{4}-" "$WORK/9.out" >/dev/null || check_failed "(9) list has no line '$name  tier=1  created=<time>'"
done
[ "$(wc -l <"$WORK/9.out" | tr -d ' ')" = 5 ] || check_failed "(9) list printed $(wc -l <"$WORK/9.out" | tr -d ' ') lines, not 5"
# the store holds each secret's two files and nothing else (no staged copy is left behind)
want=$(for name in BB_CRLF BB_GEN1 BB_GEN2 BB_ONE BB_TIER; do echo "$name"; echo "$name.json"; done | sort | tr '\n' ' ')
have=$( (cd "$STORE" && ls -A) | sort | tr '\n' ' ')
[ "$want" = "$have" ] || check_failed "(9) the store holds [$have], not exactly [$want]"
echo "122: (9) checked: list" >&2

# --- (10) no get ----------------------------------------------------------------------------------------------
for verb in get dump show export; do
    run 10 secret "$verb" BB_ONE </dev/null
    [ "$status" -eq 2 ] && grep -q "usage: spawn secret" "$WORK/10.err" ||
        check_failed "(10) spawn secret $verb exited $status without the usage: $(err 10)"
done
echo "122: (10) checked: there is no get" >&2

# --- (11) names ---------------------------------------------------------------------------------------------------
v6=$(made_up name)
before=$(cd "$WORK" && find . -path ./printed -prune -o -print | sort | tr '\n' ' ')
for name in bb_low ../BB_ESC A/B 9START BB.json "$(printf 'A%.0s' $(seq 1 65))"; do
    piped 11 "$v6$nl" secret set "$name"
    [ "$status" -eq 2 ] || check_failed "(11) set '$name' exited $status, not 2"
    run 11 secret generate "$name" --bytes 32 </dev/null
    [ "$status" -eq 2 ] || check_failed "(11) generate '$name' exited $status, not 2"
done
after=$(cd "$WORK" && find . -path ./printed -prune -o -print | grep -v '^\./11\.\(out\|err\)$' | sort | tr '\n' ' ')
before=$(printf '%s' "$before" | tr ' ' '\n' | grep -v '^\./11\.\(out\|err\)$' | tr '\n' ' ')
[ "$before" = "$after" ] || check_failed "(11) a refused name created or removed something"
echo "122: (11) checked: names" >&2

# --- (12) a terminal ---------------------------------------------------------------------------------------------
v7=$(made_up tty)
status=0
python3 -I - "$SPAWN_BIN" "$v7" "$WORK/12.out" <<'PY' || status=$?
import os, pty, select, sys, time
spawn, value, out = sys.argv[1], sys.argv[2].encode(), sys.argv[3]
pid, fd = pty.fork()
if pid == 0:
    os.execv(spawn, [spawn, "secret", "set", "BB_TTY"])
seen = b""
deadline = time.time() + 30
def read_some(limit):
    global seen
    r, _, _ = select.select([fd], [], [], limit)
    if not r:
        return False
    try:
        chunk = os.read(fd, 4096)
    except OSError:
        return None
    if not chunk:
        return None
    seen += chunk
    return True
while b"not echoed" not in seen and time.time() < deadline:
    if read_some(0.5) is None:
        break
os.write(fd, value + b"Z\x7f\n")
while time.time() < deadline:
    if read_some(0.5) is None:
        break
done, code = os.waitpid(pid, os.WNOHANG)
if done == 0:
    os.killpg(pid, 9)
    _, code = os.waitpid(pid, 0)
open(out, "wb").write(seen)
sys.exit(0 if os.WIFEXITED(code) and os.WEXITSTATUS(code) == 0 else 3)
PY
cat "$WORK/12.out" >>"$WORK/printed" 2>/dev/null || true
[ "$status" -eq 0 ] || check_failed "(12) set from a terminal did not exit 0 (python said $status): $(head -c 200 "$WORK/12.out" 2>/dev/null | tr -d '\r' | tr '\n' ' ')"
printf '%s\n' "$v7" | grep -qF -f - "$WORK/12.out" && check_failed "(12) the terminal echoed the value"
[ "$(cat "$STORE/BB_TTY" 2>/dev/null; echo .)" = "$v7." ] || check_failed "(12) the value typed at the terminal was not stored exactly"
# Ctrl-C at the prompt cancels: nothing stored, exit 2, and the terminal's echo is on again
status=0
python3 -I - "$SPAWN_BIN" "$WORK/12b.out" <<'PY' || status=$?
import os, pty, select, sys, time
spawn, out = sys.argv[1], sys.argv[2]
pid, fd = pty.fork()
if pid == 0:
    os.execv("/bin/sh", ["sh", "-c", '"$0" secret set BB_INT; echo "rc=$?"; stty -a', spawn])
seen = b""
deadline = time.time() + 30
def read_some(limit):
    global seen
    r, _, _ = select.select([fd], [], [], limit)
    if not r:
        return False
    try:
        chunk = os.read(fd, 4096)
    except OSError:
        return None
    if not chunk:
        return None
    seen += chunk
    return True
while b"not echoed" not in seen and time.time() < deadline:
    if read_some(0.5) is None:
        break
os.write(fd, b"\x03")
deadline = time.time() + 30
while time.time() < deadline:
    if read_some(0.5) is None:
        break
done, code = os.waitpid(pid, os.WNOHANG)
if done == 0:
    os.killpg(pid, 9)
    _, code = os.waitpid(pid, 0)
open(out, "wb").write(seen)
PY
cat "$WORK/12b.out" >>"$WORK/printed" 2>/dev/null || true
tr -d '\r' <"$WORK/12b.out" | grep -q '^rc=2$' || check_failed "(12) Ctrl-C at the prompt: set did not exit 2 (python said $status): $(tr -d '\r' <"$WORK/12b.out" | head -c 300 | tr '\n' ' ')"
tr ' ;\r' '\n\n\n' <"$WORK/12b.out" | grep -qx 'echo' && ! tr ' ;\r' '\n\n\n' <"$WORK/12b.out" | grep -qx -- '-echo' ||
    check_failed "(12) after Ctrl-C at the prompt the terminal's echo is not on again"
[ ! -e "$STORE/BB_INT" ] && [ ! -e "$STORE/BB_INT.json" ] || check_failed "(12) Ctrl-C at the prompt stored something"
echo "122: (12) checked: a terminal, without echo; Ctrl-C cancels and restores it" >&2

# --- (13) no value in any process's argv while set reads ----------------------------------------------------
v8=$(made_up ps)
first=$(printf '%s' "$v8" | cut -c1-14)
mkfifo -m 600 "$WORK/fifo13"
"$SPAWN_BIN" secret set BB_PS <"$WORK/fifo13" >"$WORK/13.out" 2>"$WORK/13.err" &
bgpid=$!
exec 3>"$WORK/fifo13"
# writes go through a subshell: if set has already exited, SIGPIPE ends the subshell, not this script
(printf '%s' "$first" >&3) 2>/dev/null || true
sleep 1
python3 -I - "$v8" "$first" <<'PY' >"$WORK/13.ps" || true
import os, sys
needles = [n.encode() for n in sys.argv[1:]]
for pid in os.listdir("/proc"):
    if not pid.isdigit() or int(pid) == os.getpid():
        continue
    try:
        argv = open(f"/proc/{pid}/cmdline", "rb").read()
    except OSError:
        continue
    if any(n in argv for n in needles):
        print(pid)
PY
(printf '%s\n' "$(printf '%s' "$v8" | cut -c15-)" >&3) 2>/dev/null || true
exec 3>&-
status=0
wait "$bgpid" || status=$?
bgpid=""
rm -f "$WORK/fifo13"
cat "$WORK/13.out" "$WORK/13.err" >>"$WORK/printed"
[ ! -s "$WORK/13.ps" ] || check_failed "(13) while set read its value, the argv of pid(s) $(tr '\n' ' ' <"$WORK/13.ps")held it"
[ "$status" -eq 0 ] && [ "$(cat "$STORE/BB_PS" 2>/dev/null; echo .)" = "$v8." ] || check_failed "(13) set from a slow pipe: exit $status, or the value not stored exactly"
echo "122: (13) checked: no argv holds the value" >&2

# --- (14) killed while reading -----------------------------------------------------------------------------------
v9=$(made_up crash)
part=$(printf '%s' "$v9" | cut -c1-16)
mkfifo -m 600 "$WORK/fifo14"
"$SPAWN_BIN" secret set BB_CRASH <"$WORK/fifo14" >"$WORK/14.out" 2>"$WORK/14.err" &
bgpid=$!
exec 4>"$WORK/fifo14"
(printf '%s' "$part" >&4) 2>/dev/null || true
sleep 1
kill -9 "$bgpid" 2>/dev/null || true
wait "$bgpid" 2>/dev/null || true
bgpid=""
exec 4>&-
rm -f "$WORK/fifo14"
cat "$WORK/14.out" "$WORK/14.err" >>"$WORK/printed"
[ ! -e "$STORE/BB_CRASH" ] && [ ! -e "$STORE/BB_CRASH.json" ] || check_failed "(14) set killed while reading left BB_CRASH in the store"
if printf '%s\n' "$part" | grep -rlF -f - "$WORK" 2>/dev/null | grep -v '/values$' >"$WORK/14.hits"; then
    check_failed "(14) the part read before the kill is in: $(sed "s|$WORK|<scratch>|" "$WORK/14.hits" | tr '\n' ' ')"
fi
run 14b secret list </dev/null
grep -q '^BB_CRASH' "$WORK/14b.out" && check_failed "(14) list names BB_CRASH after the kill"
echo "122: (14) checked: a kill mid-read stores nothing" >&2

# --- (16) what a write killed after its read leaves (planted by hand: the kill cannot be timed from outside) ---
v10=$(made_up half)
v11=$(made_up staged)
v12=$(made_up again)
(umask 077; printf '%s' "$v10" >"$STORE/BB_HALF"; printf '%s' "$v11" >"$STORE/.BB_STAGE.4194301.new"
 printf '{"name":"BB_META","tier":1,"created":"2026-01-01T00:00:00Z"}' >"$STORE/BB_META.json")
run 16a secret list </dev/null
[ "$status" -eq 0 ] || check_failed "(16) list with a killed write's leftovers exited $status"
grep -q '^BB_HALF .*incomplete' "$WORK/16a.out" || check_failed "(16) list does not name BB_HALF (a value without its metadata) as incomplete"
grep -q '^BB_META .*incomplete' "$WORK/16a.out" || check_failed "(16) list does not name BB_META (metadata without its value) as incomplete"
grep -q '^\.BB_STAGE\.4194301\.new .*staged' "$WORK/16a.out" || check_failed "(16) list does not name the staged file"
piped 16b "$v12$nl" secret set BB_LATER
[ "$status" -eq 0 ] || check_failed "(16) set BB_LATER beside the leftovers exited $status: $(err 16b)"
[ ! -e "$STORE/.BB_STAGE.4194301.new" ] || check_failed "(16) the next set left the staged file in the store"
v13=$(made_up redo)
piped 16c "$v13$nl" secret set BB_HALF
[ "$status" -eq 0 ] && [ "$(cat "$STORE/BB_HALF" 2>/dev/null; echo .)" = "$v13." ] && [ -f "$STORE/BB_HALF.json" ] ||
    check_failed "(16) set on the incomplete BB_HALF: exit $status, or not stored with its metadata: $(err 16c)"
run 16d secret generate BB_META --bytes 32 </dev/null
[ "$status" -eq 0 ] && [ -f "$STORE/BB_META" ] || check_failed "(16) generate on the incomplete BB_META: exit $status: $(err 16d)"
run 16e secret list </dev/null
grep -q 'incomplete\|staged' "$WORK/16e.out" && check_failed "(16) list still names leftovers after they were replaced"
grep -Eq '^BB_HALF +tier=1 +created=' "$WORK/16e.out" && grep -Eq '^BB_META +tier=1 +created=' "$WORK/16e.out" ||
    check_failed "(16) list does not name BB_HALF and BB_META as stored after they were replaced"
echo "122: (16) checked: a killed write's leftovers are named and cleared" >&2

# --- (15) no value outside the store -------------------------------------------------------------------------------
leaks=""
while IFS= read -r value; do
    [ -n "$value" ] || continue
    printf '%s\n' "$value" | grep -qF -f - "$WORK/printed" && leaks="$leaks output;"
    for hit in $(printf '%s\n' "$value" | grep -rlF -f - "$WORK" 2>/dev/null); do
        case $hit in
        "$STORE"/*|"$WORK/values"|"$WORK/value-in-a-file") ;;
        *) leaks="$leaks $(printf '%s' "$hit" | sed "s|$WORK|<scratch>|");" ;;
        esac
    done
done <"$WORK/values"
[ -z "$leaks" ] || check_failed "(15) a value is outside the store:$leaks"
[ -z "$(ls -A "$HOME")" ] || check_failed "(15) something was written in HOME: $(ls -A "$HOME" | tr '\n' ' ')"
echo "122: (15) checked: no value in any output or any file outside the store" >&2

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "122: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "122: the store keeps secrets by name; no command prints a value"
exit 0
