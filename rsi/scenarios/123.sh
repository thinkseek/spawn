#!/bin/sh
# Objective 123 [recoverability]: rotation, retired values and the keyed
# fingerprint in the store of secrets by name (sync/design-secret-injection.md
# section 3.2, after objective 122; the lead's decision B: nothing here masks).
#
# `spawn secret rotate NAME [--generate --bytes N]` replaces NAME's value. The
# new value is read as `set` reads it (a pipe, or a terminal without echo) or
# made as `generate` makes it. The previous value stays in the store as
# <store>/<NAME>.retired.<k> (600), served by nothing. A value NAME holds now
# or held before is refused. The metadata gains rotated, fingerprint and
# retired [{n, retired, fingerprint}]. The fingerprint is the first 12 hex
# characters of HMAC-SHA256 under <store>/.fingerprint.key (32 bytes, 600),
# made at the first rotation. list keeps 122's line prefix and appends
# "  rotated=<time|->  retired=<n>  fp=<12 hex|-|?>".
#
# Black box, made-up values (random per run) under a scratch root:
#   (1) rotate of a name not in the store is refused and creates nothing (no
#       key either);
#   (2) set A, rotate A from a pipe: A holds the new value exactly;
#       A.retired.1 holds the old one exactly, 600; the metadata has rotated,
#       a fingerprint and retired [{n: 1}], and no value; the key is 32
#       bytes, 600;
#   (3) rotating to the current value, or to a retired one, is refused and
#       changes nothing;
#   (4) rotate --generate --bytes 32: A is 43 base64url characters;
#       A.retired.2 holds the previous value;
#   (5) list: A's line keeps 122's prefix, with rotated=, retired=2 and a
#       12-hex fp that changed at each rotation and is the metadata's; no
#       value;
#   (6) in a store with no key, a set shows fp=-, and a refused first
#       rotation (the current value again) changes nothing and makes no key;
#       after that store's first rotation, a new set shows a 12-hex fp;
#   (7) the fingerprint is keyed: one value in two stores gives two fps, and
#       no fp is any 12-character cut of the value's sha256, sha1 or md5;
#   (8) rotate killed (SIGKILL) while reading changes nothing in the store;
#   (9) rotate refuses a value as an argument, /dev/null, a regular file, an
#       empty value and --tier 2, as set does; at a terminal it does not echo;
#  (10) a killed rotation's leftovers (planted by hand): a retired name that
#       is the current file is removed by the next rotation; a value rotated
#       without its metadata shows fp=? and the next rotation repairs it; a
#       staged copy of the store's key (a kill as a first rotation made it) is
#       listed as a killed write's copy and the next rotation removes it;
#  (11) no value, current or retired, is in any output or in any file outside
#       the stores, and nothing is written in HOME.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "123: harness: $*" >&2
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
    [ "$rc" -eq 2 ] || echo "123: harness: unexpected exit $rc" >&2
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
made_up() { # a made-up value, random per run, remembered for (11)
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
# snap: every file of the store with its sha256 (the store is unchanged when two snaps are equal)
snap() { (cd "$1" && for f in $(ls -A); do printf '%s %s\n' "$f" "$(sha256sum <"$f" | cut -c1-64)"; done); }
# has <value> <file...>: the value is in a file (the value on grep's stdin, never in its argv)
has() { v=$1; shift; printf '%s\n' "$v" | grep -qF -f - "$@" 2>/dev/null; }
meta() { # <store> <name> <python expression over m>
    python3 -I -c 'import json,sys; m=json.load(open(sys.argv[1])); print(eval(sys.argv[2]))' "$1/$2.json" "$3" 2>/dev/null || echo unreadable
}
fp_of() { # <list output file> <name>
    sed -n "s/^$2  .*  fp=\([^ ]*\)\$/\1/p" "$1"
}

# --- (1) a name not in the store --------------------------------------------------------
v0=$(made_up none)
piped 1 "$v0$nl" secret rotate BB_NONE
[ "$status" -eq 2 ] && grep -q "not in the store" "$WORK/1.err" ||
    check_failed "(1) rotate of a missing name: exit $status, not 2 with 'not in the store': $(err 1)"
[ -z "$(ls -A "$STORE")" ] || check_failed "(1) rotate of a missing name left [$(names_in_store)] in the store"
echo "123: (1) checked: a missing name" >&2

# --- (2) set, then rotate from a pipe ---------------------------------------------------
v1=$(made_up one)
v2=$(made_up two)
piped 2a "$v1$nl" secret set BB_A
[ "$status" -eq 0 ] || harness "set BB_A failed: $(err 2a)"
piped 2b "$v2$nl" secret rotate BB_A
[ "$status" -eq 0 ] || check_failed "(2) rotate BB_A from a pipe exited $status: $(err 2b)"
[ "$(cat "$STORE/BB_A" 2>/dev/null; echo .)" = "$v2." ] || check_failed "(2) <store>/BB_A does not hold the new value exactly"
[ "$(cat "$STORE/BB_A.retired.1" 2>/dev/null; echo .)" = "$v1." ] || check_failed "(2) <store>/BB_A.retired.1 does not hold the old value exactly"
[ "$(stat -c %a "$STORE/BB_A.retired.1" 2>/dev/null)" = 600 ] || check_failed "(2) <store>/BB_A.retired.1 is not mode 600"
got=$(meta "$STORE" BB_A '(m["name"], m["tier"], bool(m.get("created")), bool(m.get("rotated")), len(m.get("fingerprint","")), [e["n"] for e in m.get("retired",[])], all(len(e.get("fingerprint",""))==12 and e.get("retired") for e in m.get("retired",[])))')
[ "$got" = "('BB_A', 1, True, True, 12, [1], True)" ] || check_failed "(2) <store>/BB_A.json after one rotation: $got"
has "$v1" "$STORE/BB_A.json" && check_failed "(2) the metadata holds the old value"
has "$v2" "$STORE/BB_A.json" && check_failed "(2) the metadata holds the new value"
[ "$(stat -c '%a %s' "$STORE/.fingerprint.key" 2>/dev/null)" = "600 32" ] || check_failed "(2) <store>/.fingerprint.key is not 32 bytes, mode 600: $(stat -c '%a %s' "$STORE/.fingerprint.key" 2>&1)"
run 2c secret list </dev/null
fp1=$(fp_of "$WORK/2c.out" BB_A)
echo "123: (2) checked: rotate from a pipe" >&2

# --- (3) a value held now or before ------------------------------------------------------
before=$(snap "$STORE")
piped 3a "$v2$nl" secret rotate BB_A
[ "$status" -eq 2 ] || check_failed "(3) rotate to the current value exited $status, not 2"
piped 3b "$v1$nl" secret rotate BB_A
[ "$status" -eq 2 ] || check_failed "(3) rotate back to the retired value exited $status, not 2"
[ "$(snap "$STORE")" = "$before" ] || check_failed "(3) a refused rotation changed the store"
echo "123: (3) checked: a value held now or before is refused" >&2

# --- (4) rotate --generate ---------------------------------------------------------------
run 4 secret rotate BB_A --generate --bytes 32 </dev/null
[ "$status" -eq 0 ] || check_failed "(4) rotate --generate exited $status: $(err 4)"
g1=$(cat "$STORE/BB_A" 2>/dev/null || true)
[ -z "$g1" ] || printf '%s\n' "$g1" >>"$WORK/values"
printf '%s' "$g1" | grep -Eq '^[A-Za-z0-9_-]{43}$' || check_failed "(4) the generated value is not 43 base64url characters"
[ "$(cat "$STORE/BB_A.retired.2" 2>/dev/null; echo .)" = "$v2." ] || check_failed "(4) <store>/BB_A.retired.2 does not hold the previous value"
echo "123: (4) checked: rotate --generate" >&2

# --- (5) list ----------------------------------------------------------------------------
run 5 secret list </dev/null
[ "$status" -eq 0 ] || check_failed "(5) list exited $status"
grep -Eq '^BB_A +tier=1 +created=[0-9]{4}-[^ ]* +rotated=[0-9]{4}-[^ ]* +retired=2 +fp=[0-9a-f]{12}$' "$WORK/5.out" ||
    check_failed "(5) list has no line 'BB_A  tier=1  created=<time>  rotated=<time>  retired=2  fp=<12 hex>': $(grep '^BB_A' "$WORK/5.out")"
fp2=$(fp_of "$WORK/5.out" BB_A)
[ -n "$fp1" ] && [ -n "$fp2" ] && [ "$fp1" != "$fp2" ] || check_failed "(5) the fingerprint did not change at the rotation ($fp1, $fp2)"
[ "$fp2" = "$(meta "$STORE" BB_A 'm.get("fingerprint")')" ] || check_failed "(5) list's fp is not the metadata's"
echo "123: (5) checked: list" >&2

# --- (6) a store with no key -------------------------------------------------------------
STORE2=$WORK/store2
mkdir -m 700 "$STORE2"
v3=$(made_up three)
printf '%s\n' "$v3" | SPAWN_SECRET_STORE=$STORE2 "$SPAWN_BIN" secret set BB_C >"$WORK/6a.out" 2>"$WORK/6a.err" || harness "set BB_C failed: $(err 6a)"
SPAWN_SECRET_STORE=$STORE2 "$SPAWN_BIN" secret list </dev/null >"$WORK/6b.out" 2>"$WORK/6b.err" || true
grep -Eq '^BB_C +tier=1 +created=[^ ]+ +rotated=- +retired=0 +fp=-$' "$WORK/6b.out" ||
    check_failed "(6) a secret set before the store's key does not list rotated=- retired=0 fp=-: $(grep '^BB_C' "$WORK/6b.out")"
[ ! -e "$STORE2/.fingerprint.key" ] || check_failed "(6) set made the fingerprint key (only a rotation makes it)"
# a refused first rotation (the current value again) makes no key (rev-g's F1)
before=$(snap "$STORE2")
status=0
printf '%s\n' "$v3" | SPAWN_SECRET_STORE=$STORE2 "$SPAWN_BIN" secret rotate BB_C >"$WORK/6f.out" 2>"$WORK/6f.err" || status=$?
[ "$status" -eq 2 ] || check_failed "(6) a first rotation to BB_C's current value exited $status, not 2"
[ ! -e "$STORE2/.fingerprint.key" ] || check_failed "(6) a refused first rotation made the fingerprint key"
[ "$(snap "$STORE2")" = "$before" ] || check_failed "(6) a refused first rotation changed the store"
v4=$(made_up four)
printf '%s\n' "$v4" | SPAWN_SECRET_STORE=$STORE2 "$SPAWN_BIN" secret rotate BB_C >"$WORK/6c.out" 2>"$WORK/6c.err" || check_failed "(6) rotate BB_C failed: $(err 6c)"
v5=$(made_up five)
printf '%s\n' "$v5" | SPAWN_SECRET_STORE=$STORE2 "$SPAWN_BIN" secret set BB_D >"$WORK/6d.out" 2>"$WORK/6d.err" || harness "set BB_D failed: $(err 6d)"
SPAWN_SECRET_STORE=$STORE2 "$SPAWN_BIN" secret list </dev/null >"$WORK/6e.out" 2>"$WORK/6e.err" || true
grep -Eq '^BB_D +tier=1 +created=[^ ]+ +rotated=- +retired=0 +fp=[0-9a-f]{12}$' "$WORK/6e.out" ||
    check_failed "(6) a secret set after the store's first rotation does not list a 12-hex fp: $(grep '^BB_D' "$WORK/6e.out")"
grep -Eq '^BB_C .* retired=1 +fp=[0-9a-f]{12}$' "$WORK/6e.out" || check_failed "(6) BB_C after its rotation: $(grep '^BB_C' "$WORK/6e.out")"
cat "$WORK"/6?.out "$WORK"/6?.err >>"$WORK/printed"
echo "123: (6) checked: fp=- before the key, a fingerprint after it" >&2

# --- (7) the fingerprint is keyed ---------------------------------------------------------
v6=$(made_up same)
piped 7a "$v6$nl" secret set BB_SAME
printf '%s\n' "$v6" | SPAWN_SECRET_STORE=$STORE2 "$SPAWN_BIN" secret set BB_SAME >"$WORK/7b.out" 2>"$WORK/7b.err" || harness "set BB_SAME in store2 failed: $(err 7b)"
cat "$WORK/7b.out" "$WORK/7b.err" >>"$WORK/printed"
fa=$(meta "$STORE" BB_SAME 'm.get("fingerprint")')
fb=$(meta "$STORE2" BB_SAME 'm.get("fingerprint")')
printf '%s' "$fa" | grep -Eq '^[0-9a-f]{12}$' && printf '%s' "$fb" | grep -Eq '^[0-9a-f]{12}$' && [ "$fa" != "$fb" ] ||
    check_failed "(7) one value in two stores: fingerprints [$fa] and [$fb], not two different 12-hex ones"
unkeyed=$(printf '%s' "$v6" | python3 -I -c '
import hashlib, sys
value, fps = sys.stdin.buffer.read(), sys.argv[1:]
cuts = set()
for algorithm in ("sha256", "sha1", "md5"):
    hexed = hashlib.new(algorithm, value).hexdigest()
    cuts |= {hexed[i:i + 12] for i in range(len(hexed) - 11)}
print(" ".join(fp for fp in fps if fp in cuts))' "$fa" "$fb")
[ -z "$unkeyed" ] || check_failed "(7) a fingerprint is a cut of an unkeyed hash of the value: $unkeyed"
echo "123: (7) checked: the fingerprint is keyed" >&2

# --- (8) killed while reading ----------------------------------------------------------------
before=$(snap "$STORE")
v7=$(made_up crash)
part=$(printf '%s' "$v7" | cut -c1-16)
mkfifo -m 600 "$WORK/fifo8"
"$SPAWN_BIN" secret rotate BB_A <"$WORK/fifo8" >"$WORK/8.out" 2>"$WORK/8.err" &
bgpid=$!
exec 4>"$WORK/fifo8"
(printf '%s' "$part" >&4) 2>/dev/null || true
sleep 1
kill -9 "$bgpid" 2>/dev/null || true
wait "$bgpid" 2>/dev/null || true
bgpid=""
exec 4>&-
rm -f "$WORK/fifo8"
cat "$WORK/8.out" "$WORK/8.err" >>"$WORK/printed"
[ "$(snap "$STORE")" = "$before" ] || check_failed "(8) rotate killed while reading changed the store"
if printf '%s\n' "$part" | grep -rlF -f - "$WORK" 2>/dev/null | grep -v '/values$' >"$WORK/8.hits"; then
    check_failed "(8) the part read before the kill is in: $(sed "s|$WORK|<scratch>|" "$WORK/8.hits" | tr '\n' ' ')"
fi
echo "123: (8) checked: a kill mid-read changes nothing" >&2

# --- (9) refusals as set's, and the terminal ----------------------------------------------------
before=$(snap "$STORE")
v8=$(made_up argv)
run 9a secret rotate BB_A "$v8" </dev/null
[ "$status" -eq 2 ] || check_failed "(9) rotate with the value as an argument exited $status, not 2"
has "$v8" "$WORK/9a.out" "$WORK/9a.err" && check_failed "(9) the refusal repeats the value given as an argument"
run 9b secret rotate BB_A </dev/null
[ "$status" -eq 2 ] || check_failed "(9) rotate with stdin /dev/null exited $status, not 2"
v9=$(made_up file)
printf '%s\n' "$v9" >"$WORK/value-in-a-file"
run 9c secret rotate BB_A <"$WORK/value-in-a-file"
[ "$status" -eq 2 ] || check_failed "(9) rotate with stdin a regular file exited $status, not 2"
piped 9d "" secret rotate BB_A
[ "$status" -eq 2 ] && grep -qi empty "$WORK/9d.err" || check_failed "(9) rotate with an empty value: exit $status: $(err 9d)"
v10=$(made_up tier)
piped 9e "$v10$nl" secret rotate BB_A --tier 2
[ "$status" -eq 2 ] && grep -qi injectable "$WORK/9e.err" || check_failed "(9) rotate --tier 2: exit $status: $(err 9e)"
[ "$(snap "$STORE")" = "$before" ] || check_failed "(9) a refused rotation changed the store"
v11=$(made_up tty)
status=0
python3 -I - "$SPAWN_BIN" "$v11" "$WORK/9f.out" <<'PY' || status=$?
import os, pty, select, sys, time
spawn, value, out = sys.argv[1], sys.argv[2].encode(), sys.argv[3]
pid, fd = pty.fork()
if pid == 0:
    os.execv(spawn, [spawn, "secret", "rotate", "BB_A"])
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
os.write(fd, value + b"\n")
deadline = time.time() + 30
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
cat "$WORK/9f.out" >>"$WORK/printed" 2>/dev/null || true
[ "$status" -eq 0 ] || check_failed "(9) rotate at a terminal did not exit 0 (python said $status)"
has "$v11" "$WORK/9f.out" && check_failed "(9) the terminal echoed the value"
[ "$(cat "$STORE/BB_A" 2>/dev/null; echo .)" = "$v11." ] || check_failed "(9) the value typed at the terminal is not BB_A's"
[ "$(cat "$STORE/BB_A.retired.3" 2>/dev/null; echo .)" = "$g1." ] || check_failed "(9) BB_A.retired.3 is not the generated value"
echo "123: (9) checked: refusals as set's; a terminal without echo" >&2

# --- (10) a killed rotation's leftovers (planted by hand) --------------------------------------------
# a kill between the link and the rename: the current file also under the next retired name
ln "$STORE/BB_A" "$STORE/BB_A.retired.4" 2>/dev/null ||
    check_failed "(10) cannot plant BB_A.retired.4 (it exists: the earlier rotations numbered their retired values otherwise)"
v12=$(made_up after)
piped 10a "$v12$nl" secret rotate BB_A
[ "$status" -eq 0 ] || check_failed "(10) rotate beside a leftover link exited $status: $(err 10a)"
[ "$(cat "$STORE/BB_A.retired.4" 2>/dev/null; echo .)" = "$v11." ] && [ "$(stat -c %i "$STORE/BB_A.retired.4")" != "$(stat -c %i "$STORE/BB_A")" ] ||
    check_failed "(10) after a leftover link, BB_A.retired.4 is not the previous value in its own file"
[ ! -e "$STORE/BB_A.retired.5" ] || check_failed "(10) the leftover link was kept as a retired value of its own"
[ "$(meta "$STORE" BB_A '[e["n"] for e in m["retired"]]')" = "[1, 2, 3, 4]" ] || check_failed "(10) retired after the leftover link: $(meta "$STORE" BB_A '[e["n"] for e in m["retired"]]')"
# a kill between the rename and the metadata: a new value, the old metadata
v13=$(made_up halfway)
ln "$STORE/BB_A" "$STORE/BB_A.retired.5" 2>/dev/null ||
    check_failed "(10) cannot plant BB_A.retired.5 (it exists: the earlier rotations numbered their retired values otherwise)"
(umask 077; printf '%s' "$v13" >"$STORE/.planted" && mv "$STORE/.planted" "$STORE/BB_A") ||
    check_failed "(10) cannot plant the half-rotated value"
# a kill while the store's key was being made: its staged copy
(umask 077; head -c 32 /dev/urandom >"$STORE/..fingerprint.key.4194301.new") ||
    check_failed "(10) cannot plant a staged key"
run 10b secret list </dev/null
grep -q '^\.\.fingerprint\.key\.4194301\.new  staged' "$WORK/10b.out" ||
    check_failed "(10) list does not name the key's staged copy as a killed write's: $(grep -c . "$WORK/10b.out") lines"
grep -Eq '^BB_A .* retired=4 +fp=\?$' "$WORK/10b.out" || check_failed "(10) a value rotated without its metadata does not list fp=?: $(grep '^BB_A' "$WORK/10b.out")"
v14=$(made_up repaired)
piped 10c "$v14$nl" secret rotate BB_A
[ "$status" -eq 0 ] || check_failed "(10) rotate after a half rotation exited $status: $(err 10c)"
[ ! -e "$STORE/..fingerprint.key.4194301.new" ] || check_failed "(10) the next rotation left the key's staged copy"
run 10d secret list </dev/null
grep -Eq '^BB_A .* retired=6 +fp=[0-9a-f]{12}$' "$WORK/10d.out" || check_failed "(10) the next rotation did not repair BB_A: $(grep '^BB_A' "$WORK/10d.out")"
[ "$(cat "$STORE/BB_A.retired.6" 2>/dev/null; echo .)" = "$v13." ] || check_failed "(10) BB_A.retired.6 is not the half-rotated value"
echo "123: (10) checked: a killed rotation's leftovers" >&2

# --- (11) no value outside the stores -----------------------------------------------------------------
leaks=""
while IFS= read -r value; do
    [ -n "$value" ] || continue
    has "$value" "$WORK/printed" && leaks="$leaks output;"
    for hit in $(printf '%s\n' "$value" | grep -rlF -f - "$WORK" 2>/dev/null); do
        case $hit in
        "$STORE"/* | "$STORE2"/* | "$WORK/values" | "$WORK/value-in-a-file") ;;
        *) leaks="$leaks $(printf '%s' "$hit" | sed "s|$WORK|<scratch>|");" ;;
        esac
    done
done <"$WORK/values"
[ -z "$leaks" ] || check_failed "(11) a value is outside the stores:$leaks"
[ -z "$(ls -A "$HOME")" ] || check_failed "(11) something was written in HOME: $(ls -A "$HOME" | tr '\n' ' ')"
echo "123: (11) checked: no value in any output or any file outside the stores" >&2

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "123: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "123: secrets rotate; retired values are kept and never served; the fingerprint is keyed"
exit 0
