#!/usr/bin/env bash
# scripts/rsi/check-credential-prints.py (objective 174, D8): each way a
# script can show a credential is a finding, each way of reading one without
# showing it is not, an accepted line needs its why, a finding never repeats
# the line, and a missing or broken value collector refuses.
#
#   bash scripts/rsi/test-check-credential-prints.sh
#
# Against a made-up collector of the real one's shape (its file lists and its
# name rule), so the cases do not depend on this host; then the guard over
# scripts/ and deploy/ with the collector it reads by default, when this host
# has it. Everything in a scratch directory, under a scratch HOME.
set -uo pipefail
here=$(cd "$(dirname "$0")/../.." && pwd)
check_py=$here/scripts/rsi/check-credential-prints.py
fails=0 oks=0
t() { eval "$1"; }
check() {
  local what=$1
  shift
  if "$@"; then
    echo "ok   $what"
    oks=$((oks + 1))
  else
    echo "FAIL $what"
    fails=$((fails + 1))
  fi
}

scratch=$(mktemp -d "${TMPDIR:-/tmp}/test-check-credential-prints.XXXXXX")
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
export HOME=$scratch/home
case $HOME/ in "${real_home:-/nonexistent}"/*)
  echo "test-check-credential-prints: refused: HOME $HOME is in the real home; nothing ran" >&2
  rmdir "$scratch"
  exit 2
  ;;
esac
mkdir -p "$HOME" "$scratch/cases"
# Removes only its own mktemp directory.
trap 'rm -rf -- "$scratch"' EXIT

# The made-up collector: the real one's names, made-up sources, and its name
# rule as objective 175 has it (a name ending in DATABASE_URL counts). The
# guard adds no name of its own: without that rule, $SPAWN_DATABASE_URL is not
# a finding.
cat >"$scratch/collector.py" <<'PY'
import re
KV = [".local/share/spawn-rsi/spawn.env", ".config/spawn/r2.env", ".hermes/.env"]
MIXED = [".config/spawn/gate-env"]
WHOLE = [".config/spawn/test-database.url", ".config/spawn/backup.key"]
WHOLE_ABSOLUTE = ["/data/tmp/ccd-pg/url"]
JSON_LEAVES = [".claude/.credentials.json"]
RETIRED = ".config/spawn/retired-values.env"
RENAMED = {".config/spawn/backup.key": ".config/spawn/backup.key.k1"}
CREDENTIAL_WORDS = {"TOKEN", "KEY", "SECRET", "PASSWORD", "PWD", "SESSION"}
CREDENTIAL_ENDINGS = ("PASSWORD", "PASSWD", "APIKEY", "SECRET", "TOKEN")
NAME_WORDS = re.compile(r"[_.\-]+|(?<=[a-z0-9])(?=[A-Z])")
CREDENTIAL_NAME_ENDINGS = (("DATABASE", "URL"),)
def credential_name(name):
    words = [w.upper() for w in NAME_WORDS.split(name) if w]
    return (any(w in CREDENTIAL_WORDS or w.endswith(CREDENTIAL_ENDINGS) for w in words)
            or any(tuple(words[-len(e):]) == e for e in CREDENTIAL_NAME_ENDINGS))
PY
export SPAWN_SECRET_VALUES=$scratch/collector.py

# run <file>: the guard over one file; its stdout in $out, stderr in $err, exit in $rc.
run() {
  out=$(python3 -I "$check_py" "$@" 2>"$scratch/err")
  rc=$?
  err=$(cat "$scratch/err")
}
case_file() { # case_file <name> <<'SH' ... SH
  local f=$scratch/cases/$1.sh
  { echo '#!/usr/bin/env bash'; cat; } >"$f"
  echo "$f"
}

echo "== shows a credential: one finding each"
while IFS='|' read -r name line; do
  f=$(printf '%s\n' "$line" | case_file "bad-$name")
  run "$f"
  check "$name is a finding (exit 1, one line naming it)" t ' [ "$rc" = 1 ] && [ "$(grep -c "^$f:2: " <<<"$out")" = 1 ]'
done <<'CASES'
grep-lines|grep -rn 'SPAWN_BACKUP_KEY_FILE\|SPAWN_BACKUP_SEAL_KEY' ~/.config/systemd/user ~/.local/share/spawn-rsi/spawn.env
cat|cat ~/.config/spawn/backup.key
renamed|xxd "$HOME/.config/spawn/backup.key.k1"
older-copy|head -1 "$HOME/.config/spawn/r2.env.bak-relay"
sed-n-p|sed -n 1p /data/tmp/ccd-pg/url
sed-whole|sed 's/^/  /' ~/.config/spawn/retired-values.env
echo-cat|echo "url: $(cat ~/.config/spawn/test-database.url)"
backticks|printf '%s\n' `cat ~/.hermes/.env`
echo-var|echo "url: $SPAWN_DATABASE_URL"
echo-brace|printf 'k=%s\n' "${R2_SECRET}"
echo-lower-password|echo "$pg_password"
env-grep|env | grep TOKEN
printenv|printenv SPAWN_BACKUP_SEAL_KEY
to-stderr|cat ~/.config/spawn/backup.key >&2
stderr-and-out|cat ~/.config/spawn/backup.key 2>&1 >&2
stdin|tail -n 2 < ~/.config/spawn/gate-env
later-stage|cat ~/.local/share/spawn-rsi/spawn.env | cut -d= -f2
wrapped|sudo nice -n 19 timeout 5 cat /data/tmp/ccd-pg/url
env-prefix|LC_ALL=C sort ~/.config/spawn/r2.env
jq|jq . ~/.claude/.credentials.json
after-and|cd /x && less ~/.config/spawn/backup.key
CASES

f=$(case_file bad-continued <<'SH'
grep \
  KEY ~/.hermes/.env
SH
)
run "$f"
check "a command over two lines is a finding, at its first line" t '[ "$rc" = 1 ] && grep -q "^$f:2: grep: shows .hermes/.env" <<<"$out"'

f=$(case_file bad-value <<'SH'
cat ~/.config/spawn/backup.key; echo hunter174hunter174
SH
)
run "$f"
check "a finding names the command and the file, never the line" t ' [ "$rc" = 1 ] && grep -q ": cat: shows backup.key$" <<<"$out" && ! grep -q hunter174 <<<"$out$err"'

f=$(case_file bad-unreasoned <<'SH'
cat ~/.config/spawn/backup.key # credential-print: ok
SH
)
run "$f"
check "an acceptance with no why accepts nothing" [ "$rc" = 1 ]

echo "== reads a credential without showing it: no finding"
while IFS='|' read -r name line; do
  f=$(printf '%s\n' "$line" | case_file "good-$name")
  run "$f"
  check "$name is not a finding" t '[ "$rc" = 0 ] && [ -z "$out" ]'
done <<'CASES'
grep-count|grep -c 'SPAWN_BACKUP_SEAL_KEY' ~/.local/share/spawn-rsi/spawn.env
grep-count-files|grep -rc 'SPAWN_BACKUP_KEY_FILE\|SPAWN_BACKUP_SEAL_KEY' ~/.config/systemd/user ~/.local/share/spawn-rsi/spawn.env | grep -v ':0$'
grep-quiet|grep -q '^SPAWN_X=' ~/.local/share/spawn-rsi/spawn.env && echo set
grep-files|grep -l KEY ~/.config/spawn/r2.env
grep-combined|grep -qs KEY ~/.config/spawn/r2.env
into-variable|url=$(cat /data/tmp/ccd-pg/url)
compared|[ -z "$(tail -c1 ~/.local/share/spawn-rsi/spawn.env)" ] || echo no newline
hashed|cat ~/.config/spawn/backup.key | sha256sum
counted|cut -d= -f1 ~/.config/spawn/r2.env | wc -l
to-file|cat ~/.config/spawn/backup.key > "$scratch/copy"
stdout-to-file|sed -n 1p ~/.config/spawn/r2.env 1>/dev/null
both-to-file|cat ~/.config/spawn/backup.key >& "$scratch/copy"
script-not-file|cat deploy/gate-env.sh
sourced|. ~/.local/share/spawn-rsi/spawn.env
stat|stat -c %s ~/.config/spawn/backup.key; ls -l ~/.config/spawn/r2.env
lower-locals|echo "-- pane $session"; printf '%s\n' "$gate_key" "$binds_key"
env-grep-excluding|env | grep -vE '^(SPAWN_TEST_DATABASE_URL|PWD)=' | sort > "$scratch/env"
env-names|env | sed 's/=.*//'
child-env|SPAWN_DATABASE_URL=$url spawn alert record < "$scratch/a.json"
accepted|cat ~/.config/spawn/backup.key # credential-print: ok the made-up key of a test
CASES

f=$(case_file good-written <<'SH'
cat >"$scratch/spawn.env" <<EOF
SPAWN_R2_SECRET=made-up
EOF
cat > "$scratch/r2.env" <<-'EOF'
	cat ~/.config/spawn/backup.key
	EOF
# cat ~/.config/spawn/backup.key
# credential-print: ok the made-up key the test wrote above
cat "$scratch/backup.key"
SH
)
run "$f"
check "a written file, a heredoc's body, a comment and an accepted line are no findings" t '[ "$rc" = 0 ] && [ -z "$out" ]'
check "and the accepted line is counted" grep -q "0 finding(s), 1 accepted" <<<"$err"

echo "== heredocs, here-strings and /dev outputs (rev-f's review of R1: F1-F3, n1, n2, n4-n6)"
# finding <name> <line> <what> <<'SH' ... SH: the file is one finding, at that line, showing that.
finding() {
  local f want_at=$2 want_shown=$3
  f=$(case_file "bad-$1")
  run "$f"
  check "$1 is one finding at line $want_at, showing $want_shown" \
    t '[ "$rc" = 1 ] && [ "$(grep -c . <<<"$out")" = 1 ] && grep -qF -- "$f:$want_at: " <<<"$out" && grep -qF -- ": shows $want_shown" <<<"$out"'
}
# clean <name> <<'SH' ... SH: the file is no finding.
clean() {
  local f
  f=$(case_file "good-$1")
  run "$f"
  check "$1 is not a finding" t '[ "$rc" = 0 ] && [ -z "$out" ]'
}
finding arith-shift 3 r2.env <<'SH'
x=$((1 << SHIFT))
cat ~/.config/spawn/r2.env
SH
finding herestring-word 3 r2.env <<'SH'
tr a b <<<word
cat ~/.config/spawn/r2.env
SH
finding quoted-heredoc-text 3 r2.env <<'SH'
echo "use <<EOF to start"
cat ~/.config/spawn/r2.env
SH
finding comment-heredoc 3 r2.env <<'SH'
# bb_engine >FILE <<'X' ... X: a comment, as assertions.sh:185 has one
cat ~/.config/spawn/r2.env
SH
finding trailing-comment-heredoc 3 r2.env <<'SH'
echo start # then <<'X' opens nothing: a comment
cat ~/.config/spawn/r2.env
SH
finding dev-stderr 2 r2.env <<'SH'
cat ~/.config/spawn/r2.env >/dev/stderr
SH
finding dev-tty 2 r2.env <<'SH'
cat ~/.config/spawn/r2.env > /dev/tty
SH
finding dev-fd 2 r2.env <<'SH'
sed -n 1p ~/.config/spawn/r2.env 1>/dev/fd/2
SH
finding tee-stderr 2 r2.env <<'SH'
cat ~/.config/spawn/r2.env | tee /dev/stderr >/dev/null
SH
finding herestring-var 2 '$API_TOKEN' <<'SH'
cat <<<"$API_TOKEN"
SH
finding heredoc-body-var 2 '$API_TOKEN' <<'SH'
cat <<EOF
key: $API_TOKEN
EOF
SH
finding heredoc-body-cat 2 backup.key <<'SH'
cat <<-EOF
	key: $(cat ~/.config/spawn/backup.key)
	EOF
SH
finding after-heredoc 5 r2.env <<'SH'
cat <<'EOF'
text
EOF
cat ~/.config/spawn/r2.env
SH
finding env-sort-grep 2 TOKEN <<'SH'
env | sort | grep TOKEN
SH
finding declare-grep 2 SECRET <<'SH'
declare -p | grep SECRET
SH
finding logger 2 '$API_TOKEN' <<'SH'
logger "$API_TOKEN"
SH
finding env-grep-word 2 '<a credential name>' <<'SH'
env | grep -i token
SH
clean quoted-heredoc-body <<'SH'
cat <<'EOF'
key: $API_TOKEN $(cat ~/.config/spawn/backup.key)
EOF
SH
clean escaped-heredoc-body <<'SH'
cat <<EOF
key: \$API_TOKEN and \$PWD/verdict.json
EOF
SH
clean pwd <<'SH'
echo "in $PWD, was $OLDPWD"
SH
clean length <<'SH'
echo "${#API_TOKEN}"
SH
clean sed-in-place <<'SH'
sed -i 's/a/b/' ~/.config/spawn/r2.env; sed --in-place=.bak 's/a/b/' ~/.config/spawn/r2.env
SH
clean heredoc-to-psql <<'SH'
psql <<EOF
select '$API_TOKEN';
EOF
SH
f=$(case_file open-heredoc <<'SH'
echo start
cat <<EOF
never ended
SH
)
run "$f"
check "a heredoc with no delimiter line refuses (2), naming path:line" t '[ "$rc" = 2 ] && grep -qF "$f:3: a heredoc with no delimiter line: refused" <<<"$err"'
f=$(case_file counted <<'SH'
cat <<EOF
one
two
EOF
SH
)
run "$f"
check "the summary counts the lines and the heredoc body lines" grep -q "(5 line(s), 2 of them heredoc bodies)" <<<"$err"

echo "== a tree"
mkdir -p "$scratch/tree/deploy" "$scratch/tree/tools"
printf '#!/bin/sh\ncat ~/.config/spawn/backup.key\n' >"$scratch/tree/tools/precheck"
printf 'cat ~/.config/spawn/backup.key\n' >"$scratch/tree/tools/notes.md"
printf '#!/usr/bin/env python3\nprint(open("backup.key").read())\n' >"$scratch/tree/tools/tool.py"
printf 'echo "$SPAWN_DATABASE_URL"\n' >"$scratch/tree/deploy/a.sh"
printf '#!/bin/dash\nlogger "$SPAWN_DATABASE_URL"\n' >"$scratch/tree/tools/dash-script"
run "$scratch/tree"
check "a sh or dash script with no suffix and a .sh are read; markdown and Python are not" t ' [ "$rc" = 1 ] && [ "$(grep -c . <<<"$out")" = 3 ] && grep -q "tools/dash-script:2: logger" <<<"$out" && grep -q "tools/precheck:2: cat" <<<"$out" && grep -q "deploy/a.sh:1: echo: shows \$SPAWN_DATABASE_URL" <<<"$out" && grep -q "^check-credential-prints: 3 script(s)" <<<"$err"'

echo "== the collector"
good=$scratch/cases/good-stat.sh
SPAWN_SECRET_VALUES=$scratch/none.py run "$good"
check "no collector: refused (2), naming it" t '[ "$rc" = 2 ] && grep -q "no value collector at $scratch/none.py: refused" <<<"$err"'
printf 'raise SystemExit(3)\n' >"$scratch/broken.py"
SPAWN_SECRET_VALUES=$scratch/broken.py run "$good"
check "a collector that does not load: refused (2)" t '[ "$rc" = 2 ] && grep -q "cannot be read" <<<"$err"'
grep -v '^MIXED' "$scratch/collector.py" >"$scratch/partial.py"
SPAWN_SECRET_VALUES=$scratch/partial.py run "$good"
check "a collector missing a list: refused (2), naming the list" t '[ "$rc" = 2 ] && grep -q "has no MIXED" <<<"$err"'
# The same collector with the name rule before 175 (no DATABASE_URL ending): a later def wins.
{ cat "$scratch/collector.py"; cat <<'PY'
def credential_name(name):
    words = [w.upper() for w in NAME_WORDS.split(name) if w]
    return any(w in CREDENTIAL_WORDS or w.endswith(CREDENTIAL_ENDINGS) for w in words)
PY
} >"$scratch/older.py"
SPAWN_SECRET_VALUES=$scratch/older.py run "$scratch/cases/bad-echo-var.sh"
check "the name rule is the collector's alone: without its DATABASE_URL ending, \$SPAWN_DATABASE_URL is no finding" t '[ "$rc" = 0 ]'
run "$scratch/no-such-root"
check "a root that does not exist: refused (2)" [ "$rc" = 2 ]

# The host's own collector, when it has one: the guard reads it, and scripts/ and deploy/ are clean.
default=/usr/local/lib/spawn/redact/secret-values.py
# Before objective 175 the host's collector has no credential_name, and the guard refuses
# under it (exit 2, by design: 174 lands after 175); said, not judged here.
if [ -f "$default" ] && ! grep -q '^def credential_name(' "$default"; then
  echo "-- $default predates 175 (no credential_name): the guard refuses under it; not checked here"
elif [ -f "$default" ]; then
  out=$(env -u SPAWN_SECRET_VALUES python3 -I "$check_py" "$here/scripts" "$here/deploy" 2>"$scratch/err")
  rc=$?
  check "with $default: scripts/ and deploy/ show no credential" [ "$rc" = 0 ]
  check "and a case shown under the made-up list is shown under it too" \
    sh -c "env -u SPAWN_SECRET_VALUES python3 -I '$check_py' '$scratch/cases/bad-cat.sh' >/dev/null 2>&1; [ \$? = 1 ]"
else
  echo "-- $default is not on this host: the cases above ran against the made-up list only"
fi

echo "test-check-credential-prints: $oks ok, $fails FAIL"
[ "$fails" = 0 ]
