#!/usr/bin/env bash
# scripts/blackbox/test-engine-read.sh: bb_engine and the bb_read_line it
# writes into a fake engine (scripts/blackbox/assertions.sh), on a probe
# engine fed by timed writes.
#
#   scripts/blackbox/test-engine-read.sh      # about 20 s; touches nothing outside its scratch
#
# Cases:
# 1. a whole line is the line;
# 2. a line cut by the timeout ("BB:S", 1.5 s of quiet, "AY x"), 070's gate
#    failure: one whole line "BB:SAY x", and no quiet in between (a turn does
#    not end mid-paste);
# 3. a second with nothing typed is quiet (read's timeout status);
# 4. a part held 10 s with no end of line is returned as the line, after
#    about 10 s, with no quiet before it;
# 5. the end of input is read's status 1;
# 6. the engine's own variables (part, held, status) are untouched;
# 7. bb_engine refuses an engine that is not bash, and no engine.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
PROG=test-engine-read
scratch=$(mktemp -d "${TMPDIR:-/tmp}/engine-read-test.XXXXXX")
trap 'rm -f "$scratch"/engine "$scratch"/out "$scratch"/err; rmdir "$scratch"' EXIT

failures=0
check() { # check <description> <command...>
  local what=$1
  shift
  if "$@"; then printf 'ok   %s\n' "$what"; else printf 'FAIL %s\n' "$what"; failures=$((failures + 1)); fi
}

# The probe: one output line per call, "got[<line>] <seconds>" or
# "quiet(<status>) <seconds>", seconds since it started; it stops at the end
# of input. Its own part/held/status must survive every call.
sh -c '. "$1/assertions.sh"; bb_engine' sh "$here" >"$scratch/engine" <<'ENGINE'
#!/usr/bin/env bash
part=mine held=mine status=mine
start=$(date +%s.%N)
at() { awk -v now="$(date +%s.%N)" -v start="$start" 'BEGIN { printf "%.1f", now - start }'; }
while :; do
  if bb_read_line typed; then
    echo "got[$typed] $(at)"
  else
    code=$?
    echo "quiet($code) $(at)"
    [ "$code" -le 128 ] && break
  fi
done
echo "vars $part $held $status"
ENGINE
chmod 700 "$scratch/engine"
feed() { "$scratch/engine" >"$scratch/out"; }
has() { grep -qxE "$1" "$scratch/out"; }
lines() { grep -c "$1" "$scratch/out" || true; }

echo "== 1-3, 5, 6: a whole line, a cut line, quiet, the end of input"
{ printf 'one\n'; printf 'BB:S'; sleep 1.5; printf 'AY x\n'; sleep 1.3; } | feed
sed 's/^/  | /' "$scratch/out"
check "1. a whole line is the line" has 'got\[one\] [0-9.]+'
check "2. the cut line is one whole line" has 'got\[BB:SAY x\] [0-9.]+'
check "2. no quiet between one and BB:SAY x" \
  [ "$(sed -n '2p' "$scratch/out" | cut -d' ' -f1)" = 'got[BB:SAY' ]
check "3. a second with nothing typed is quiet (timeout status)" has 'quiet\(1(29|[3-9][0-9]|[0-9]{3})\) [0-9.]+'
check "5. the end of input is status 1" has 'quiet\(1\) [0-9.]+'
check "6. the engine's own variables are untouched" has 'vars mine mine mine'

echo "== 4: a part with no end of line"
{ printf 'never ends'; sleep 12.5; } | feed
sed 's/^/  | /' "$scratch/out"
check "4. returned as the line" has 'got\[never ends\] [0-9.]+'
check "4. after about 10 s (9.5-12)" \
  awk '/^got\[never ends\]/ { exit !($NF >= 9.5 && $NF <= 12) }' "$scratch/out"
check "4. no quiet before it" [ "$(head -n 1 "$scratch/out" | cut -d' ' -f1)" = 'got[never' ]

echo "== 7: bb_engine refuses"
set +e
printf '#!/bin/sh\nx\n' | sh -c '. "$1/assertions.sh"; bb_engine' sh "$here" >/dev/null 2>"$scratch/err"
status=$?
set -e
check "a #!/bin/sh engine: refused (status $status)" [ "$status" -ne 0 ]
check "says it is not a bash #! line" grep -q "not a bash #! line" "$scratch/err"
set +e
sh -c '. "$1/assertions.sh"; bb_engine' sh "$here" </dev/null >/dev/null 2>"$scratch/err"
status=$?
set -e
check "no engine: refused (status $status)" [ "$status" -ne 0 ]
check "says there is no engine" grep -q "no engine on standard input" "$scratch/err"

if [ "$failures" -gt 0 ]; then
  echo "$PROG: $failures check(s) failed"
  exit 1
fi
echo "$PROG: all cases pass"
