#!/bin/sh
# Objective 121 [recoverability]: overlapping values are masked whole.
#
# The problem (rev-e's read of landed 117, c6646e3e, finding L1;
# sync/review-obj-117.md, "Landed"): the mask takes the longest form at
# the leftmost place and jumps past it, so a form of another value that
# starts inside that match and runs past its end is never seen. With
# BB_A = made-up-AAAA-<tail> and BB_C = PREFIX-made-up-AAAA (BB_C ends
# with BB_A's first 12 bytes), the text "PREFIX-" + BB_A comes out
# "[SECRET:BB_C]-<tail>": BB_A's tail in the clear. The same in the stream,
# on a whole text and in the hook.
#
# Required: the union of overlapping matches is masked as one
# [SECRET:<the first match's name>], in a stream however it is cut; a values
# file that is a directory is refused as "not a file" (rev-e's LN1); and
# `spawn mask`'s usage says base64 wrapped across lines (MIME) is not
# masked (LN2, a stated limit).
#
# Black box, made-up values under a scratch HOME:
#   (1) `spawn mask` over "x PREFIX-" + BB_A + " y" gives exactly
#       "x [SECRET:BB_C] y", with the values file in either order.
#   (2) The same text written in two parts, cut at every byte, gives the
#       same output each time. So does the hex form of "PREFIX-" + BB_A
#       (hex of BB_A, the longest form, starts 14 characters into hex of
#       BB_C: a cut can fall inside it while the span is being read).
#   (3) `spawn mask-hook` given a Bash response holding "PREFIX-" + BB_A
#       replaces its stdout with "[SECRET:BB_C]".
#   (4) A values file that is a directory: `spawn mask` exits 2, prints
#       nothing of its input, and its error names the directory and "not a
#       file"; the hook withholds the response.
#   (5) `spawn mask` with no arguments prints its usage, which says base64
#       wrapped across lines is not masked.
#   (6) No output of any run holds BB_A's tail.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "121: harness: $*" >&2
    exit 2
}
[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
command -v python3 >/dev/null 2>&1 || harness "python3 is not installed"

WORK=$(mktemp -d) || harness "mktemp failed"
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -m 700 "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
finish() {
    rc=$?
    trap - EXIT
    set +e
    rm -rf "$WORK"
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "121: harness: unexpected exit $rc" >&2
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

# BB_A's tail is made up per run; BB_C ends with BB_A's first 12 bytes.
tail_a=$(od -An -N10 -tx1 /dev/urandom | tr -d ' \n')
umask 077
printf 'BB_A=made-up-AAAA-%s\nBB_C=PREFIX-made-up-AAAA\n' "$tail_a" >"$WORK/values-ac"
printf 'BB_C=PREFIX-made-up-AAAA\nBB_A=made-up-AAAA-%s\n' "$tail_a" >"$WORK/values-ca"
printf 'x PREFIX-made-up-AAAA-%s y' "$tail_a" >"$WORK/text"
umask 022
want="x [SECRET:BB_C] y"

# --- (1) whole, either order ---------------------------------------------------------------
for order in ac ca; do
    status=0
    "$SPAWN_BIN" mask --values "$WORK/values-$order" <"$WORK/text" >"$WORK/out1" 2>"$WORK/err1" || status=$?
    cat "$WORK/out1" "$WORK/err1" >>"$WORK/printed"
    [ "$status" -eq 0 ] || check_failed "(1) spawn mask (values $order) exited $status: $(head -c 200 "$WORK/err1")"
    got=$(cat "$WORK/out1")
    [ "$got" = "$want" ] || check_failed "(1) spawn mask (values $order): expected [$want], got [$(printf '%s' "$got" | sed "s/$tail_a/<BB_A's tail>/g")]"
done
echo "121: (1) checked: whole, in either order" >&2

# --- (2) cut at every byte ---------------------------------------------------------------------
# Two writes with a pause between them, so the mask reads them apart.
cat >"$WORK/cut.py" <<'PY'
import sys, time, os
data = open(sys.argv[1], "rb").read()
cut = int(sys.argv[2])
os.write(1, data[:cut])
time.sleep(0.05)
os.write(1, data[cut:])
PY
length=$(wc -c <"$WORK/text" | tr -d ' ')
bad=""
cut=0
while [ "$cut" -le "$length" ]; do
    status=0
    python3 -I "$WORK/cut.py" "$WORK/text" "$cut" | "$SPAWN_BIN" mask --values "$WORK/values-ac" >"$WORK/out2" 2>"$WORK/err2" || status=$?
    cat "$WORK/out2" "$WORK/err2" >>"$WORK/printed"
    if [ "$status" -ne 0 ] || [ "$(cat "$WORK/out2")" != "$want" ]; then bad="$bad $cut"; fi
    cut=$((cut + 1))
done
[ -z "$bad" ] || check_failed "(2) written in two parts, the output differs from [$want] at the cuts:$bad"
# The hex form of the overlap, whole and cut at every byte.
printf 'x %s y' "$(printf 'PREFIX-made-up-AAAA-%s' "$tail_a" | od -An -tx1 | tr -d ' \n')" >"$WORK/hex"
length=$(wc -c <"$WORK/hex" | tr -d ' ')
bad=""
cut=0
while [ "$cut" -le "$length" ]; do
    status=0
    python3 -I "$WORK/cut.py" "$WORK/hex" "$cut" | "$SPAWN_BIN" mask --values "$WORK/values-ac" >"$WORK/out2" 2>"$WORK/err2" || status=$?
    cat "$WORK/out2" "$WORK/err2" >>"$WORK/printed"
    if [ "$status" -ne 0 ] || [ "$(cat "$WORK/out2")" != "$want" ]; then bad="$bad $cut"; fi
    cut=$((cut + 1))
done
[ -z "$bad" ] || check_failed "(2) the hex form written in two parts: the output differs from [$want] at the cuts:$bad"
echo "121: (2) checked: cut at each of $((length + 1)) places" >&2

# --- (3) the hook ----------------------------------------------------------------------------------
python3 -I - "$WORK/values-ac" "$WORK/event" <<'PY'
import json, sys
a = [l.split("=", 1)[1] for l in open(sys.argv[1]).read().splitlines() if l.startswith("BB_A=")][0]
event = {"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": {"command": "cat config"},
         "tool_response": {"stdout": "PREFIX-" + a, "stderr": "", "interrupted": False}}
open(sys.argv[2], "w").write(json.dumps(event))
PY
chmod 600 "$WORK/event"
status=0
"$SPAWN_BIN" mask-hook --values "$WORK/values-ac" <"$WORK/event" >"$WORK/hook" 2>"$WORK/hook.err" || status=$?
cat "$WORK/hook" "$WORK/hook.err" >>"$WORK/printed"
got=$(jq -S -c '.hookSpecificOutput.updatedToolOutput' "$WORK/hook" 2>/dev/null || echo unreadable)
expected=$(printf '%s' '{"stdout":"[SECRET:BB_C]","stderr":"","interrupted":false}' | jq -S -c .)
[ "$status" -eq 0 ] && [ "$got" = "$expected" ] ||
    check_failed "(3) the hook (exit $status): expected $expected, got $(printf '%s' "$got" | sed "s/$tail_a/<BB_A's tail>/g")"
echo "121: (3) checked: the hook" >&2

# --- (4) a directory as the values file -----------------------------------------------------------
mkdir -m 700 "$WORK/valuesdir"
status=0
"$SPAWN_BIN" mask --values "$WORK/valuesdir" <"$WORK/text" >"$WORK/out4" 2>"$WORK/err4" || status=$?
cat "$WORK/out4" "$WORK/err4" >>"$WORK/printed"
[ "$status" -eq 2 ] || check_failed "(4) spawn mask with a directory exited $status, not 2"
[ ! -s "$WORK/out4" ] || check_failed "(4) spawn mask with a directory printed $(wc -c <"$WORK/out4" | tr -d ' ') bytes"
grep -qF "$WORK/valuesdir: not a file" "$WORK/err4" ||
    check_failed "(4) spawn mask with a directory: its error does not say '<dir>: not a file': $(head -c 200 "$WORK/err4")"
status=0
"$SPAWN_BIN" mask-hook --values "$WORK/valuesdir" <"$WORK/event" >"$WORK/hook4" 2>"$WORK/hook4.err" || status=$?
cat "$WORK/hook4" "$WORK/hook4.err" >>"$WORK/printed"
withheld=$(jq -r '.hookSpecificOutput.updatedToolOutput | if type == "string" then . else "not withheld" end' "$WORK/hook4" 2>/dev/null || echo unreadable)
case $withheld in *withheld*) ;; *) check_failed "(4) the hook with a directory did not withhold the response (exit $status)" ;; esac
echo "121: (4) checked: a directory is not a file" >&2

# --- (5) the stated limit ------------------------------------------------------------------------------
status=0
"$SPAWN_BIN" mask >"$WORK/out5" 2>"$WORK/err5" </dev/null || status=$?
[ "$status" -ne 0 ] || check_failed "(5) spawn mask with no arguments exited 0"
grep -qi "usage: spawn mask" "$WORK/out5" "$WORK/err5" || check_failed "(5) spawn mask with no arguments printed no usage: $(head -c 200 "$WORK/err5")"
grep -qiE "wrapped across lines|MIME" "$WORK/out5" "$WORK/err5" ||
    check_failed "(5) spawn mask's usage does not say that base64 wrapped across lines (MIME) is not masked: $(head -c 300 "$WORK/err5")"
echo "121: (5) checked: the usage states the limit" >&2

# --- (6) no tail anywhere -------------------------------------------------------------------------------
if grep -qF "$tail_a" "$WORK/printed"; then
    check_failed "(6) something a run printed holds BB_A's tail"
fi
echo "121: (6) checked: BB_A's tail is in no output" >&2

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "121: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "121: overlapping values are masked as one span, on a stream cut anywhere and in the hook"
exit 0
