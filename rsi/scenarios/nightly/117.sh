#!/bin/sh
# Objective 117 [recoverability], secrets by name, part A
# (sync/design-secret-injection.md 3.4 and 5.A, with section 0's owner
# decisions): the mask. `spawn mask` puts a stream through it, and
# `spawn mask-hook` is a Claude Code PostToolUse hook that replaces a tool's
# response holding a known value (the lead's probe, 11-13 10:22: the
# replacement is what the session file keeps). It is built and tested
# here with made-up values under a scratch HOME. Nothing installs the hook
# in any settings, and switching it on is a later step, with the owner's
# word.
#
# Black box. The values file is `NAME=value` lines, mode 600, read by path
# and never put on an argv. Its values are made up here: a current value
# of BB_ONE holding `:`, `/` and `+`, a retired value of BB_ONE, a
# value of BB_TWO, and a value of BB_THREE whose fixed start
# (`bb-made-up???value-`) gives its base64 a `+` or `/` at every alignment,
# so its URL-safe forms are never its standard ones (rev-e's F2).
#   (1) `spawn mask --values <file>` over text holding each value in each
#       form exits 0. The output holds none of the forms and holds
#       [SECRET:BB_ONE] and [SECRET:BB_TWO] where they were. The forms:
#       plain, JSON-escaped, URL-encoded with upper- and lower-case hex (the
#       password of a postgres://user:password@ URL among them), base64 at
#       each of the three alignments (standard and URL-safe), and hex in both
#       cases. The text around them is unchanged.
#   (2) The same text written into the pipe 7 bytes at a time gives the same
#       output: a value split across reads is caught.
#   (3) The retired value is masked as [SECRET:BB_ONE].
#   (4) It fails closed. With the values file at mode 644, with a line that
#       is not NAME=value, with a value under 8 bytes, or with a file that
#       names no value (only a comment), it exits 2,
#       prints nothing of its input, and its error names the file (and the
#       line), never a value.
#   (5) `spawn mask-hook --values <file>`, given a PostToolUse event whose
#       Bash response holds a value, prints {"hookSpecificOutput":
#       {"hookEventName": "PostToolUse", "updatedToolOutput": ...}}: the
#       response with its strings masked and every other field as it was.
#       Given a response with no value, it prints nothing. With the values
#       file at 644, it prints a replacement that withholds the response
#       and holds no value. Given an event that is not JSON, one cut short
#       inside a value, or a stdin it cannot read (a directory), it prints
#       a replacement that withholds the response and nothing of its input
#       (rev-e's F1). Each exits 0.
#   (6) The scratch HOME holds no Claude Code settings file afterwards, and
#       nothing any run printed, on stdout or stderr, holds a value in any
#       form.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "117: harness: $*" >&2
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
    rm -rf "$WORK"
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "117: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

umask 077
VALUES=$WORK/values
{
    printf '# made up by 117\n'
    printf 'BB_ONE=made-up:%s/one+%s\n' "$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')" "$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
    printf 'BB_ONE=retired-%s\n' "$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
    printf 'BB_TWO=two-%s\n' "$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
    printf 'BB_THREE=bb-made-up???value-%s\n' "$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
} >"$VALUES"
umask 022
check_failed() {
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
: >"$WORK/printed"

# The text and the forms, made by a script that reads the values from the
# file (never an argv): every form of each value between two marker words,
# and the list of forms (one per line) for grep -F -f.
cat >"$WORK/make.py" <<'PY'
import base64, binascii, json, sys, urllib.parse
values = [line.split("=", 1) for line in open(sys.argv[1]).read().splitlines() if line and not line.startswith("#")]
out_text, out_forms, out_want = open(sys.argv[2], "w"), open(sys.argv[3], "w"), open(sys.argv[4], "w")
def interior(v, shift, alt):
    enc = base64.b64encode(b"\0" * shift + v, altchars=alt).rstrip(b"=")
    skip = {0: 0, 1: 2, 2: 3}[shift]
    keep = len(enc) if (shift + len(v)) % 3 == 0 else len(enc) - 1
    return enc[skip:keep].decode()
n = 0
for name, value in values:
    v = value.encode()
    forms = [value, json.dumps(value)[1:-1], urllib.parse.quote(value, safe="-._~"),
             urllib.parse.quote(value, safe="-._~").replace("%3A", "%3a").replace("%2F", "%2f").replace("%2B", "%2b"),
             binascii.hexlify(v).decode(), binascii.hexlify(v).decode().upper()]
    forms += [interior(v, s, alt) for s in range(3) for alt in (b"+/", b"-_")]
    if name == "BB_THREE" and any(interior(v, s, b"+/") == interior(v, s, b"-_") for s in range(3)):
        sys.exit("BB_THREE's URL-safe forms are its standard ones")
    for form in dict.fromkeys(forms):
        n += 1
        out_text.write("before-%d %s after-%d\n" % (n, form, n))
        out_want.write("before-%d [SECRET:%s] after-%d\n" % (n, name, n))
        out_forms.write(form + "\n")
    # in a URL, and base64 at each alignment inside other bytes
    n += 1
    out_text.write("url-%d postgres://bb:%s@127.0.0.1:5432/db\n" % (n, urllib.parse.quote(value, safe="-._~")))
    out_want.write("url-%d postgres://bb:[SECRET:%s]@127.0.0.1:5432/db\n" % (n, name))
    for shift in range(3):
        n += 1
        out_text.write("b64-%d %s\n" % (n, base64.b64encode(b"x" * shift + v + b"tail").decode()))
PY
python3 -I "$WORK/make.py" "$VALUES" "$WORK/text" "$WORK/forms" "$WORK/want" || harness "cannot make the text"
chmod 600 "$WORK/text" "$WORK/forms" "$WORK/want"

# --- (1) every form -------------------------------------------------------------------
status=0
"$SPAWN_BIN" mask --values "$VALUES" <"$WORK/text" >"$WORK/out1" 2>"$WORK/err1" || status=$?
cat "$WORK/out1" "$WORK/err1" >>"$WORK/printed"
if [ "$status" -ne 0 ]; then
    check_failed "(1) spawn mask exited $status: $(head -c 300 "$WORK/err1")"
else
    if grep -qF -f "$WORK/forms" "$WORK/out1"; then
        check_failed "(1) the masked output still holds a form of a value (line $(grep -nF -f "$WORK/forms" "$WORK/out1" | head -n 1 | cut -d: -f1))"
    fi
    grep -v '^b64-' "$WORK/out1" >"$WORK/out1.plain"
    if ! cmp -s "$WORK/out1.plain" "$WORK/want"; then
        check_failed "(1) the masked output is not the text with [SECRET:NAME] in place of each form (first difference at line $(cmp "$WORK/out1.plain" "$WORK/want" 2>/dev/null | sed -n 's/.*line \([0-9]*\).*/\1/p'))"
    fi
    b64_masked=$(grep '^b64-' "$WORK/out1" | grep -c '\[SECRET:' || true)
    b64_all=$(grep -c '^b64-' "$WORK/out1" || true)
    [ "$b64_all" -gt 0 ] && [ "$b64_masked" = "$b64_all" ] ||
        check_failed "(1) base64 inside other bytes: $b64_masked of $b64_all lines masked"
fi
echo "117: (1) checked: every form of every value" >&2

# --- (2) 7 bytes at a time ----------------------------------------------------------------
cat >"$WORK/slow.py" <<'PY'
import os, sys, time
data = open(sys.argv[1], "rb").read()
for at in range(0, len(data), 7):
    os.write(1, data[at:at + 7])
    time.sleep(0.0005)
PY
status=0
python3 -I "$WORK/slow.py" "$WORK/text" | "$SPAWN_BIN" mask --values "$VALUES" >"$WORK/out2" 2>"$WORK/err2" || status=$?
cat "$WORK/out2" "$WORK/err2" >>"$WORK/printed"
[ "$status" -eq 0 ] && cmp -s "$WORK/out1" "$WORK/out2" ||
    check_failed "(2) the text written 7 bytes at a time gave another output (exit $status)"
echo "117: (2) checked: a value split across reads" >&2

# --- (3) the retired value -----------------------------------------------------------------
retired_line=$(grep -n '' "$WORK/want" | grep -m1 -F 'before-' | cut -d: -f1)
retired_n=$(python3 -I -c '
import sys
values=[l.split("=",1) for l in open(sys.argv[1]).read().splitlines() if l and not l.startswith("#")]
text=open(sys.argv[2]).read().splitlines()
retired=values[1][1]
print(next(i for i,l in enumerate(text) if (" "+retired+" ") in l))' "$VALUES" "$WORK/text")
[ "$(sed -n "$((retired_n + 1))p" "$WORK/out1" | grep -c '\[SECRET:BB_ONE\]')" = 1 ] ||
    check_failed "(3) the retired value of BB_ONE is not masked as [SECRET:BB_ONE]"
: "$retired_line"
echo "117: (3) checked: a retired value" >&2

# --- (4) fail closed ---------------------------------------------------------------------------
cp "$VALUES" "$WORK/v644" && chmod 644 "$WORK/v644"
umask 077
{ cat "$VALUES"; printf 'not a line\n'; } >"$WORK/vbad"
printf 'BB_SHORT=seven77\n' >"$WORK/vshort"
printf '# made up by 117, no value yet\n' >"$WORK/vempty"
umask 022
for case in "v644:group or world access" "vbad:line 6" "vshort:shorter than 8" "vempty:names no value"; do
    file=${case%%:*}
    says=${case#*:}
    status=0
    "$SPAWN_BIN" mask --values "$WORK/$file" <"$WORK/text" >"$WORK/out4" 2>"$WORK/err4" || status=$?
    cat "$WORK/out4" "$WORK/err4" >>"$WORK/printed"
    [ "$status" -eq 2 ] || check_failed "(4) spawn mask with $file exited $status, not 2"
    [ ! -s "$WORK/out4" ] || check_failed "(4) spawn mask with $file printed $(wc -c <"$WORK/out4" | tr -d ' ') bytes of its input"
    grep -qF "$WORK/$file" "$WORK/err4" && grep -qF "$says" "$WORK/err4" ||
        check_failed "(4) spawn mask with $file: its error does not name the file with '$says': $(head -c 200 "$WORK/err4")"
done
echo "117: (4) checked: it fails closed" >&2

# --- (5) the hook --------------------------------------------------------------------------------
python3 -I - "$VALUES" "$WORK/event-holds" "$WORK/event-clean" "$WORK/event-notjson" "$WORK/event-cut" <<'PY'
import json, sys
values = [l.split("=", 1) for l in open(sys.argv[1]).read().splitlines() if l and not l.startswith("#")]
one = values[0][1]
holds = {"session_id": "bb117", "hook_event_name": "PostToolUse", "tool_name": "Bash",
         "tool_input": {"command": "cat config"},
         "tool_response": {"stdout": "url=postgres://bb:%s@h/db\nok" % one, "stderr": "", "interrupted": False, "isImage": False}}
clean = dict(holds, tool_response={"stdout": "nothing to see", "stderr": "", "interrupted": False, "isImage": False})
open(sys.argv[2], "w").write(json.dumps(holds))
open(sys.argv[3], "w").write(json.dumps(clean))
open(sys.argv[4], "w").write("cat config\nurl=postgres://bb:%s@h/db\n" % one)
text = json.dumps(holds)
open(sys.argv[5], "w").write(text[:text.index(one) + len(one) // 2])
PY
chmod 600 "$WORK/event-holds" "$WORK/event-clean" "$WORK/event-notjson" "$WORK/event-cut"
hook() { # hook <values> <event> <out>
    status=0
    "$SPAWN_BIN" mask-hook --values "$1" <"$2" >"$3" 2>"$3.err" || status=$?
    cat "$3" "$3.err" >>"$WORK/printed"
}
hook "$VALUES" "$WORK/event-holds" "$WORK/hook1"
if [ "$status" -ne 0 ]; then
    check_failed "(5) spawn mask-hook on a response holding a value exited $status: $(head -c 200 "$WORK/hook1.err")"
else
    got=$(jq -S -c '.hookSpecificOutput | {hookEventName, out: .updatedToolOutput}' "$WORK/hook1" 2>/dev/null || echo unreadable)
    want=$(printf '%s' '{"hookEventName":"PostToolUse","out":{"stdout":"url=postgres://bb:[SECRET:BB_ONE]@h/db\nok","stderr":"","interrupted":false,"isImage":false}}' | jq -S -c .)
    [ "$got" = "$want" ] || check_failed "(5) the hook's answer: expected $want, got $got"
fi
hook "$VALUES" "$WORK/event-clean" "$WORK/hook2"
[ "$status" -eq 0 ] && [ ! -s "$WORK/hook2" ] ||
    check_failed "(5) spawn mask-hook on a response with no value: exit $status, $(wc -c <"$WORK/hook2" | tr -d ' ') bytes printed (expected 0 and none)"
hook "$WORK/v644" "$WORK/event-holds" "$WORK/hook3"
withheld=$(jq -r '.hookSpecificOutput.updatedToolOutput | if type == "string" then . else "not withheld" end' "$WORK/hook3" 2>/dev/null || echo unreadable)
[ "$status" -eq 0 ] || check_failed "(5) spawn mask-hook with a values file at 644 exited $status"
case $withheld in *withheld*) ;; *) check_failed "(5) spawn mask-hook with a values file at 644 did not withhold the response: $(head -c 200 "$WORK/hook3")" ;; esac
# An event it cannot read is withheld, never passed (rev-e's F1): not JSON,
# cut short inside a value, and a stdin that cannot be read (a directory).
for case in "event-notjson:an event that is not JSON" "event-cut:an event cut short inside a value" "/:a stdin it cannot read"; do
    event=${case%%:*}
    what=${case#*:}
    case $event in /) input=/ ;; *) input=$WORK/$event ;; esac
    hook "$VALUES" "$input" "$WORK/hook4"
    [ "$status" -eq 0 ] || check_failed "(5) spawn mask-hook on $what exited $status, not 0"
    withheld=$(jq -r '.hookSpecificOutput.updatedToolOutput | if type == "string" then . else "not withheld" end' "$WORK/hook4" 2>/dev/null || echo unreadable)
    case $withheld in *withheld*) ;; *) check_failed "(5) spawn mask-hook on $what did not withhold the response: [$(head -c 200 "$WORK/hook4")]" ;; esac
    if grep -qF -e "postgres://bb" -e "cat config" "$WORK/hook4" "$WORK/hook4.err"; then
        check_failed "(5) spawn mask-hook on $what printed some of its input"
    fi
done
echo "117: (5) checked: the PostToolUse hook" >&2

# --- (6) nothing installed, nothing printed --------------------------------------------------------
settings=$(find "$HOME" -name 'settings*.json' 2>/dev/null | head -n 3)
[ -z "$settings" ] || check_failed "(6) a run left a Claude Code settings file: $settings"
if grep -qF -f "$WORK/forms" "$WORK/printed"; then
    check_failed "(6) something a run printed holds a form of a value"
fi
echo "117: (6) checked: nothing installed, no value printed" >&2

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "117: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "117: the mask hides every form of a known value, on a stream and in a hook, and fails closed"
exit 0
