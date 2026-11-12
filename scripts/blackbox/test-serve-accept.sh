#!/bin/sh
# scripts/blackbox/test-serve-accept.sh: `spawn serve` survives a failed
# accept(). One failed accept ends tiny_http's accept thread; serve used to
# return Ok(0) then, and exit without a word on a refused port (046's step
# (5) "answered 000000"). Now it says the listener stopped and listens on the
# same port again.
#
#   SPAWN_BIN=target/debug/spawn scripts/blackbox/test-serve-accept.sh   # about 15 s; no database; touches nothing outside its scratch
#
# serve runs under `ulimit -n 64`; 120 idle connections exhaust its files,
# so accept() fails with EMFILE. Once they are closed:
#   1. serve still runs;
#   2. its log says the listener stopped and that it listens again;
#   3. a GET / on the port it named at start answers 200.
set -eu
[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || { echo "test-serve-accept: SPAWN_BIN is not an executable" >&2; exit 2; }
command -v python3 >/dev/null 2>&1 && command -v curl >/dev/null 2>&1 || { echo "test-serve-accept: needs python3 and curl" >&2; exit 2; }
scratch=$(mktemp -d "${TMPDIR:-/tmp}/serve-accept-test.XXXXXX")
spid=""
trap '[ -z "$spid" ] || kill "$spid" 2>/dev/null || true; rm -f "$scratch"/web/index.html "$scratch"/serve.log; rmdir "$scratch"/web "$scratch"' EXIT
mkdir "$scratch/web"
printf '<!doctype html>\n' >"$scratch/web/index.html"
: >"$scratch/serve.log"
(ulimit -n 64 && exec "$SPAWN_BIN" serve --address 127.0.0.1:0 --web "$scratch/web") >>"$scratch/serve.log" 2>&1 &
spid=$!
i=0
port=""
while [ "$i" -lt 100 ] && [ -z "$port" ]; do
    i=$((i + 1))
    port=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$scratch/serve.log" | head -n 1)
    [ -n "$port" ] || sleep 0.1
done
[ -n "$port" ] || { echo "test-serve-accept: serve did not start: $(head -c 300 "$scratch/serve.log")" >&2; exit 2; }
# 120 idle connections, held 3 s, then closed.
python3 -I - "$port" <<'EOF'
import socket, sys, time
port = int(sys.argv[1])
held = []
for _ in range(120):
    try:
        held.append(socket.create_connection(("127.0.0.1", port), timeout=1))
    except OSError:
        pass
    time.sleep(0.01)
time.sleep(3)
for s in held:
    s.close()
EOF
failures=0
code=000
i=0
while [ "$i" -lt 50 ] && [ "$code" != 200 ]; do
    i=$((i + 1))
    code=$(curl -sS --noproxy '*' -o /dev/null -w '%{http_code}' --max-time 2 "http://127.0.0.1:$port/" 2>/dev/null) || code=000
    [ "$code" = 200 ] || sleep 0.2
done
if kill -0 "$spid" 2>/dev/null; then echo "ok   serve still runs"; else
    status=0; wait "$spid" || status=$?; spid=""
    echo "FAIL serve exited (status $status)"; failures=$((failures + 1)); fi
if grep -q 'serve: the listener stopped: ' "$scratch/serve.log" && grep -q 'serve: listening again on ' "$scratch/serve.log"; then
    echo "ok   the log says the listener stopped and serve listens again"
else
    echo "FAIL the log does not say the listener stopped and was listened on again"; failures=$((failures + 1))
fi
if [ "$code" = 200 ]; then echo "ok   GET / on port $port answers 200 after the flood"; else
    echo "FAIL GET / on port $port answered $code after the flood"; failures=$((failures + 1)); fi
if [ "$failures" -ne 0 ]; then
    echo "serve's log:"; sed 's/^/  /' "$scratch/serve.log" | head -n 20
    exit 1
fi
