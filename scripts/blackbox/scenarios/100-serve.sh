#!/bin/sh
# The read-only HTTP API. The server must start against an installed schema,
# answer the two read endpoints with JSON, and serve a static file. It never
# writes, so the store is unchanged after a full round of requests.

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"

# A random port keeps parallel scenarios apart (not the pid: in the gate's
# PID namespace it is the same every run).
port=$((20000 + $(od -An -N2 -tu2 /dev/urandom) % 20000))
web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"

"$SPAWN_BIN" serve --address "127.0.0.1:$port" --web "$web" \
    >"$WORK/serve.log" 2>&1 &
serve_pid=$!
trap 'kill "$serve_pid" 2>/dev/null' EXIT

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

# One real event, then the overview must show the session the Observer judged.
printf '%s' '{"hook_event_name":"UserPromptSubmit"}' | \
    SPAWN_AGENT_ID=agent_serve SPAWN_SESSION_ID=session_serve "$SPAWN_BIN" hook
capture "$SPAWN_BIN" tick
assert_eq "$status" 0 "tick exit"

# Snapshot after the only writers (hook, tick). Every request below is a read
# and must leave this count alone.
before=$(scalar 'select count(*) from spawn_events')

curl -fsS "http://127.0.0.1:$port/api/overview" >"$WORK/overview.json" 2>/dev/null
assert_contains "$(cat "$WORK/overview.json")" '"session_id":"session_serve"' \
    "overview shows the judged session"

curl -fsS "http://127.0.0.1:$port/api/session/session_serve" >"$WORK/session.json" 2>/dev/null
assert_contains "$(cat "$WORK/session.json")" '"event_name":"message.received"' \
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

assert_eq "$(scalar 'select count(*) from spawn_events')" "$before" \
    "serving appended no event"
