#!/bin/sh
# The read-only HTTP API. The server must start against an installed schema,
# answer the two read endpoints with JSON, and serve a static file. It never
# writes, so the store is unchanged after a full round of requests. Off
# loopback it refuses to start without SPAWN_SERVE_TOKEN, and with a token
# every API request must carry it.

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"

# `tick` below talks to tmux; pin it to the scenario's private server.
cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"
export SPAWN_TMUX_PROGRAM="$WORK/tmux"

# A port derived from the scenario pid keeps parallel scenarios apart.
port=$((20000 + ($$ % 20000)))
guarded_port=$((port + 20000))
web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"

"$SPAWN_BIN" serve --address "127.0.0.1:$port" --web "$web" \
    >"$WORK/serve.log" 2>&1 &
serve_pid=$!
guarded_pid=
trap 'kill $serve_pid $guarded_pid 2>/dev/null' EXIT

# Wait for the listener. Bounded: 50 tries, 0.1s apart.
ready=0
i=0
while [ "$i" -lt 50 ]; do
    i=$((i + 1))
    if curl -fsS "http://127.0.0.1:$port/api/overview" >"$WORK/overview.json" 2>/dev/null; then
        ready=1
        break
    fi
    sleep 0.1
done
[ "$ready" -eq 1 ] || fail "serve did not answer on port $port"

# Empty store: no session. (The retired RSI controller's block is not
# pinned: RSI is an objective mission, docs/fleet-architecture.md §10.4.)
assert_contains "$(cat "$WORK/overview.json")" '"sessions":[]' \
    "overview lists no session on an empty store"

code=$(curl -s -o "$WORK/session.json" -w '%{http_code}' \
    "http://127.0.0.1:$port/api/session/session_absent")
assert_eq "$code" 200 "session endpoint status for an unknown session"
assert_contains "$(cat "$WORK/session.json")" '"session_id":"session_absent"' \
    "session endpoint echoes the requested id"
assert_contains "$(cat "$WORK/session.json")" '"timeline":[]' \
    "unknown session has an empty timeline"

# One real event (the command.accepted a submit appends), then the overview
# must show the session the Observer judged. A stop starts no engine.
capture "$SPAWN_BIN" submit --agent agent_serve --session session_serve \
    --key serve-1 --action '{"type":"stop"}'
assert_eq "$status" 0 "submit exit"
capture "$SPAWN_BIN" tick
assert_eq "$status" 0 "tick exit"

# Snapshot after the only writers (submit, tick). Every request below is a
# read and must leave this count alone.
before=$(scalar 'select count(*) from spawn_events')

curl -fsS "http://127.0.0.1:$port/api/overview" >"$WORK/overview.json" 2>/dev/null
assert_contains "$(cat "$WORK/overview.json")" '"session_id":"session_serve"' \
    "overview shows the judged session"

curl -fsS "http://127.0.0.1:$port/api/session/session_serve" >"$WORK/session.json" 2>/dev/null
assert_contains "$(cat "$WORK/session.json")" '"event_name":"command.accepted"' \
    "session endpoint shows the timeline event"
assert_contains "$(cat "$WORK/session.json")" '"event_count":' \
    "session endpoint reports freshness"

# Static files come from the configured directory.
code=$(curl -s -o "$WORK/index.html" -w '%{http_code}' "http://127.0.0.1:$port/")
assert_eq "$code" 200 "index status"
assert_contains "$(cat "$WORK/index.html")" "spawn" "index served from the web directory"

# A request path never escapes the web directory.
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/../Cargo.toml")
[ "$code" = 404 ] || fail "path traversal returned $code, expected 404"

# --- off loopback: no token, no server --------------------------------------
# Bounded, so a server that wrongly starts cannot hang the scenario.
capture env -u SPAWN_SERVE_TOKEN SPAWN_SERVE_ADDRESS="0.0.0.0:$guarded_port" \
    timeout 5 "$SPAWN_BIN" serve --web "$web"
assert_eq "$status" 1 "serve on 0.0.0.0 without a token exit"
assert_contains "$(err)" "without SPAWN_SERVE_TOKEN" "the refusal names the token"

# --- with a token, every API request carries it ------------------------------
token=bb-serve-token-$$
SPAWN_SERVE_TOKEN=$token "$SPAWN_BIN" serve --address "0.0.0.0:$guarded_port" \
    --web "$web" >"$WORK/guarded.log" 2>&1 &
guarded_pid=$!
guarded="http://127.0.0.1:$guarded_port"
wait_for 40 curl -fsS -H "Authorization: Bearer $token" "$guarded/api/overview" ||
    fail "guarded serve did not answer on port $guarded_port: $(cat "$WORK/guarded.log")"

code=$(curl -s -o "$WORK/denied.json" -w '%{http_code}' "$guarded/api/overview")
assert_eq "$code" 401 "API request without a token"
assert_contains "$(cat "$WORK/denied.json")" '"error":"unauthorized"' "401 body"
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer wrong-$token" \
    "$guarded/api/overview")
assert_eq "$code" 401 "API request with a wrong bearer token"
code=$(curl -s -o /dev/null -w '%{http_code}' "$guarded/api/pane/session_serve?token=wrong")
assert_eq "$code" 401 "pane request with a wrong ?token="
code=$(curl -s -o /dev/null -w '%{http_code}' "$guarded/api/pane/session_serve")
assert_eq "$code" 401 "pane request without a token"

code=$(curl -s -o "$WORK/overview.json" -w '%{http_code}' \
    -H "Authorization: Bearer $token" "$guarded/api/overview")
assert_eq "$code" 200 "API request with the bearer token"
assert_contains "$(cat "$WORK/overview.json")" '"session_id":"session_serve"' \
    "the guarded overview shows the session"
code=$(curl -s -o "$WORK/session.json" -w '%{http_code}' \
    "$guarded/api/session/session_serve?after=0&token=$token")
assert_eq "$code" 200 "API request with ?token="
assert_contains "$(cat "$WORK/session.json")" '"event_name":"command.accepted"' \
    "?token= sits beside the endpoint's own parameters"

# The built UI is public code with no data: the browser loads it, then sends
# the token on its API reads.
code=$(curl -s -o /dev/null -w '%{http_code}' "$guarded/")
assert_eq "$code" 200 "static index without a token"

assert_eq "$(scalar 'select count(*) from spawn_events')" "$before" \
    "serving appended no event"
