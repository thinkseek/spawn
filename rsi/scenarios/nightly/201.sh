#!/bin/sh
# Black box (objective 201): the Jev relay takes its API key from systemd's
# credential, Keep's first move. Under `LoadCredential=typesafe.api-key:...`
# systemd gives the relay $CREDENTIALS_DIRECTORY with the file
# typesafe.api-key in it; the relay sends that key, and with a credentials
# directory it takes no other: a missing or empty credential stops it at its
# start, saying which, even when TYPESAFE_API_KEY is set. Without a
# credentials directory (the unit before its move) TYPESAFE_API_KEY is the
# key, as before.
#
# The relay is the checkout's scripts/jev-relay.py (the working directory is
# the checkout under test), run as the unit runs it, against a made-up hosted
# API on loopback that records each request's Authorization header. Every
# key is made up here, in scratch; nothing under /run/keep or
# /run/keep-test, ~/.config/spawn or the live directory is read.
#
# Checks:
#   (1) a credentials directory holding typesafe.api-key as Keep writes it
#       (the raw made-up value, no newline), and no TYPESAFE_API_KEY: the
#       relay answers a POST with the hosted API's answer, which saw
#       "Bearer <that value>"; the relay's log holds no key.
#   (2) the file ending with a newline (written by hand), TYPESAFE_API_KEY set
#       to another made-up value: the hosted API saw the credential's value
#       without the newline, never the variable's.
#   (3) a credentials directory without typesafe.api-key, TYPESAFE_API_KEY
#       set: the relay exits by itself within 10 s with status 78 (the status
#       the unit's RestartPreventExitStatus lists beside 243), saying
#       "the credential typesafe.api-key cannot be read"; neither value is in
#       its output.
#   (4) an empty typesafe.api-key, TYPESAFE_API_KEY set: the relay exits with
#       status 78, saying "the credential typesafe.api-key is empty".
#   (5) no credentials directory, TYPESAFE_API_KEY set: the hosted API saw
#       "Bearer <the variable's value>" (the unit before its move).
#   (6) a credential whose made-up value begins with a space (Keep's form, no
#       newline): the hosted API saw "Bearer  <value>", two spaces, the space
#       kept (only one trailing newline is ever taken off). A leading space,
#       because a server may trim a header value's end, never its inside.
#   (7) (R5, cc-astra A85/A86) a credential with spaces at both ends, and a
#       hosted API answering 502 with the key it was sent echoed in its body,
#       raw (the exact value, its spaces too), trimmed (the value without its
#       spaces, as a server that trims a header echoes it) and inside JSON;
#       then a credential with a double quote inside, echoed inside JSON
#       (escaped, \"), and one with a "/" inside, echoed inside JSON that
#       escapes it (\/): the caller gets status 502 and the relay's own reason
#       ("the hosted API answered 502"), and nothing of the key is in what the
#       caller gets or in the relay's log.
#   (8) (R6, rev-e's n1) the hosted API answering 301, 302, 303, 307 and 308
#       with Location at a second made-up host: the caller gets that status
#       and the relay's reason, and the second host gets no request (so never
#       the key).
# Each check is reported; the run fails if any failed.

set -eu

NAME=201
harness() {
    echo "$NAME: harness: $*" >&2
    exit 2
}
failures=0
check_failed() {
    echo "blackbox: $*" >&2
    failures=$((failures + 1))
}

tree=$(pwd)
relay_script=$tree/scripts/jev-relay.py
[ -f "$relay_script" ] || harness "no scripts/jev-relay.py in the working directory $tree"
for tool in python3 curl od; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
[ -d "$WORK" ] || harness "mktemp gave no directory"
upstream_pid=""
relay_pid=""
elsewhere_pid=""
# set +e first (rev-e, 2026-11-16): under set -e a kill that fails here (its
# child already gone and reaped) ended the trap with 1 before the rm, so a
# run that had passed every check exited 1 now and then and left WORK behind.
trap 'set +e; [ -z "$relay_pid" ] || kill "$relay_pid" 2>/dev/null; [ -z "$upstream_pid" ] || kill "$upstream_pid" 2>/dev/null; [ -z "$elsewhere_pid" ] || kill "$elsewhere_pid" 2>/dev/null; rm -rf "$WORK"' EXIT

made_up() { # a made-up key, new each run
    printf 'ts-201-%s-%s' "$1" "$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
}
CRED_KEY=$(made_up credential)
ENV_KEY=$(made_up variable)


# The made-up hosted API: answers each POST 200 {"answers":{}} and appends
# the request's Authorization header, one line each, to $WORK/seen. While
# $WORK/echo holds "raw", "trim", "json" or "jsonslash", it answers 502
# echoing the key it was sent (the header's value after "Bearer "): raw,
# without its spaces, inside JSON, or inside JSON with "/" written "\/".
# While it holds "redirect <code>", it answers <code> with Location at the
# URL in $WORK/redirect-to.
cat >"$WORK/upstream.py" <<'EOF'
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

seen, port_file = sys.argv[1], sys.argv[2]
echo = os.path.join(os.path.dirname(seen), "echo")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length") or 0))
        with open(seen, "a") as out:
            out.write((self.headers.get("Authorization") or "(none)") + "\n")
        status, body = 200, b'{"answers":{}}'
        mode = open(echo).read().strip() if os.path.exists(echo) else ""
        key = (self.headers.get("Authorization") or "")[len("Bearer "):]
        if mode == "raw":
            status, body = 502, key.encode()
        elif mode == "trim":
            status, body = 502, ("token=" + key.strip() + ";").encode()
        elif mode == "json":
            status, body = 502, json.dumps({"error": "unauthorized", "detail": "Bearer " + key, "key": key}).encode()
        elif mode == "jsonslash":
            status, body = 502, json.dumps({"error": "unauthorized", "key": key}).replace("/", "\\/").encode()
        elif mode.startswith("redirect "):
            to = open(os.path.join(os.path.dirname(seen), "redirect-to")).read().strip()
            self.send_response(int(mode.split()[1]))
            self.send_header("Location", to)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        with open(seen, "a") as out:
            out.write("GET " + (self.headers.get("Authorization") or "(none)") + "\n")
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *_):
        pass


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(port_file + ".tmp", "w") as out:
    out.write(str(server.server_address[1]))
os.rename(port_file + ".tmp", port_file)
server.serve_forever()
EOF
python3 -I "$WORK/upstream.py" "$WORK/seen" "$WORK/upstream.port" &
upstream_pid=$!
n=0
until [ -s "$WORK/upstream.port" ]; do
    n=$((n + 1))
    [ "$n" -lt 100 ] || harness "the made-up hosted API never started"
    sleep 0.1
done
UPSTREAM=http://127.0.0.1:$(cat "$WORK/upstream.port")

# relay <log> <environment assignments...>: the relay in the background, with
# only the environment given (as a unit has), on port 0: the system picks a
# free one as the relay binds, and the relay's first line says which (R6',
# the lead: a port picked here and released before the relay binds it could
# be taken by any other process on the host in between).
relay() {
    relay_log=$1
    shift
    RELAY_PORT=""
    env -i PATH="$PATH" TYPESAFE_BASE_URL="$UPSTREAM" JEV_RELAY_ADDRESS="127.0.0.1:0" "$@" \
        python3 -I "$relay_script" >"$relay_log" 2>&1 &
    relay_pid=$!
}

# relay_port: RELAY_PORT from the relay's line once it listens (within 10 s),
# else "" (it exited, or never said).
relay_port() {
    n=0
    while [ "$n" -lt 100 ]; do
        RELAY_PORT=$(sed -n 's|^jev-relay: http://127\.0\.0\.1:\([0-9][0-9]*\)/.*|\1|p' "$relay_log" | head -n 1)
        [ -z "$RELAY_PORT" ] || return 0
        kill -0 "$relay_pid" 2>/dev/null || return 0
        n=$((n + 1))
        sleep 0.1
    done
}

# post: one POST through the relay once it listens; the body, else "".
post() {
    relay_port
    [ -n "$RELAY_PORT" ] || return 0
    n=0
    while [ "$n" -lt 100 ]; do
        if curl -s --noproxy '*' -o "$WORK/answer" "http://127.0.0.1:$RELAY_PORT/healthz" 2>/dev/null; then
            curl -s --noproxy '*' --max-time 10 -H 'Content-Type: application/json' \
                -d '{"model":"jev","question":"state?"}' "http://127.0.0.1:$RELAY_PORT/v1/systemone" || true
            return 0
        fi
        kill -0 "$relay_pid" 2>/dev/null || return 0
        n=$((n + 1))
        sleep 0.1
    done
}

stop_relay() {
    kill "$relay_pid" 2>/dev/null || true
    wait "$relay_pid" 2>/dev/null || true
    relay_pid=""
}

# exits_alone <log> <check>: the relay ended by itself within 10 s, non-zero;
# its exit status in EXITED.
exits_alone() {
    n=0
    while kill -0 "$relay_pid" 2>/dev/null && [ "$n" -lt 100 ]; do
        n=$((n + 1))
        sleep 0.1
    done
    if kill -0 "$relay_pid" 2>/dev/null; then
        check_failed "$2 the relay was still running after 10 s: $(head -c 300 "$1")"
        stop_relay
        EXITED=running
        return 0
    fi
    EXITED=0
    wait "$relay_pid" || EXITED=$?
    relay_pid=""
    [ "$EXITED" -ne 0 ] || check_failed "$2 the relay exited 0: $(head -c 300 "$1")"
}

no_key_in() { # no_key_in <file> <check>
    if grep -qF -e "$CRED_KEY" -e "$ENV_KEY" "$1"; then
        check_failed "$2 a key is in the relay's output"
    fi
}

creds=$WORK/creds
mkdir -m 700 "$creds"

# --- (1) the credential is the key, as Keep writes it (no newline) ---------------
printf '%s' "$CRED_KEY" >"$creds/typesafe.api-key"
: >"$WORK/seen"
relay "$WORK/relay1.log" CREDENTIALS_DIRECTORY="$creds"
answer=$(post)
stop_relay
[ "$answer" = '{"answers":{}}' ] ||
    check_failed "(1) the relay with the credential did not pass the answer back: [$answer] $(head -c 300 "$WORK/relay1.log")"
[ "$(cat "$WORK/seen")" = "Bearer $CRED_KEY" ] ||
    check_failed "(1) the hosted API did not see the credential's key: [$(sed 's/ts-201-[a-z]*-[0-9a-f]*/<a made-up key>/g' "$WORK/seen" | tr '\n' '~')]"
no_key_in "$WORK/relay1.log" "(1)"

# --- (2) a newline at its end is not the key's; the credential wins over the variable
printf '%s\n' "$CRED_KEY" >"$creds/typesafe.api-key"
: >"$WORK/seen"
relay "$WORK/relay2.log" CREDENTIALS_DIRECTORY="$creds" TYPESAFE_API_KEY="$ENV_KEY"
answer=$(post)
stop_relay
if [ "$(cat "$WORK/seen")" != "Bearer $CRED_KEY" ]; then
    if grep -qF "$ENV_KEY" "$WORK/seen"; then
        check_failed "(2) with a credential, the hosted API saw TYPESAFE_API_KEY's value"
    else
        check_failed "(2) with a credential and TYPESAFE_API_KEY, the hosted API did not see the credential's key: [$answer] $(head -c 300 "$WORK/relay2.log")"
    fi
fi
no_key_in "$WORK/relay2.log" "(2)"

# --- (3) a missing credential stops the relay ----------------------------------
rm "$creds/typesafe.api-key"
: >"$WORK/seen"
relay "$WORK/relay3.log" CREDENTIALS_DIRECTORY="$creds" TYPESAFE_API_KEY="$ENV_KEY"
exits_alone "$WORK/relay3.log" "(3) with no typesafe.api-key in the credentials directory,"
[ "$EXITED" = running ] || [ "$EXITED" = 78 ] ||
    check_failed "(3) with no typesafe.api-key in the credentials directory, the relay exited $EXITED, not 78 (a status the unit restarts every 3 s)"
grep -qF "the credential typesafe.api-key cannot be read" "$WORK/relay3.log" ||
    check_failed "(3) the relay does not say the credential typesafe.api-key cannot be read: $(head -c 300 "$WORK/relay3.log")"
[ ! -s "$WORK/seen" ] || check_failed "(3) the relay with no credential reached the hosted API"
no_key_in "$WORK/relay3.log" "(3)"

# --- (4) an empty credential stops the relay -----------------------------------
: >"$creds/typesafe.api-key"
relay "$WORK/relay4.log" CREDENTIALS_DIRECTORY="$creds" TYPESAFE_API_KEY="$ENV_KEY"
exits_alone "$WORK/relay4.log" "(4) with an empty typesafe.api-key,"
[ "$EXITED" = running ] || [ "$EXITED" = 78 ] ||
    check_failed "(4) with an empty typesafe.api-key, the relay exited $EXITED, not 78 (a status the unit restarts every 3 s)"
grep -qF "the credential typesafe.api-key is empty" "$WORK/relay4.log" ||
    check_failed "(4) the relay does not say the credential typesafe.api-key is empty: $(head -c 300 "$WORK/relay4.log")"
no_key_in "$WORK/relay4.log" "(4)"

# --- (5) no credentials directory: the variable, as before ---------------------
: >"$WORK/seen"
relay "$WORK/relay5.log" TYPESAFE_API_KEY="$ENV_KEY"
answer=$(post)
stop_relay
[ "$(cat "$WORK/seen")" = "Bearer $ENV_KEY" ] ||
    check_failed "(5) without a credentials directory, the hosted API did not see TYPESAFE_API_KEY's value: [$answer] $(head -c 300 "$WORK/relay5.log")"
no_key_in "$WORK/relay5.log" "(5)"

# --- (6) a space the value begins with is the value's ------------------------------
SPACE_KEY=" $CRED_KEY"
printf '%s' "$SPACE_KEY" >"$creds/typesafe.api-key"
: >"$WORK/seen"
relay "$WORK/relay6.log" CREDENTIALS_DIRECTORY="$creds"
answer=$(post)
stop_relay
[ "$(cat "$WORK/seen")" = "Bearer $SPACE_KEY" ] ||
    check_failed "(6) a credential beginning with a space: the hosted API did not see it with the space kept (\"Bearer  <the value>\"): [$(sed 's/ts-201-[a-z]*-[0-9a-f]*/<a made-up key>/g' "$WORK/seen" | tr '\n' '~')] [$answer]"
no_key_in "$WORK/relay6.log" "(6)"

# --- (7) an error echoing the key: nothing of it goes back or into the log ----
QUOTE_KEY="ts-201-quo\"te-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
SLASH_KEY="ts-201-sla/sh-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
for case in raw trim json quote slash; do
    mode=$case PAD_KEY=" $CRED_KEY " FIND=$CRED_KEY
    if [ "$case" = quote ]; then
        mode=json PAD_KEY=$QUOTE_KEY FIND=${QUOTE_KEY#*\"}
    elif [ "$case" = slash ]; then
        mode=jsonslash PAD_KEY=$SLASH_KEY FIND=${SLASH_KEY#*/}
    fi
    printf '%s' "$PAD_KEY" >"$creds/typesafe.api-key"
    printf '%s' "$mode" >"$WORK/echo"
    : >"$WORK/seen"
    relay "$WORK/relay7$case.log" CREDENTIALS_DIRECTORY="$creds"
    post >/dev/null
    code=$(curl -s --noproxy '*' --max-time 10 -o "$WORK/answer7$case" -w '%{http_code}' -H 'Content-Type: application/json' \
        -d '{"model":"jev","question":"state?"}' "http://127.0.0.1:$RELAY_PORT/v1/systemone" || true)
    stop_relay
    [ "$(head -n 1 "$WORK/seen")" = "Bearer $PAD_KEY" ] ||
        check_failed "(7 $case) the hosted API did not see the credential with its spaces"
    [ "$code" = 502 ] || check_failed "(7 $case) the caller got status $code, not the hosted API's 502"
    grep -qF "the hosted API answered 502" "$WORK/answer7$case" ||
        check_failed "(7 $case) the caller did not get the relay's reason: [$(sed -e 's/ts-201-[a-z]*-[0-9a-f]*/<a made-up key>/g' -e 's/te-[0-9a-f]\{16\}/<a made-up key>/g' -e 's/sh-[0-9a-f]\{16\}/<a made-up key>/g' "$WORK/answer7$case" | head -c 200)]"
    if grep -qF "$FIND" "$WORK/answer7$case"; then
        check_failed "(7 $case) an error echoing the key: the key went back to the caller: [$(sed -e 's/ts-201-[a-z]*-[0-9a-f]*/<a made-up key>/g' -e 's/te-[0-9a-f]\{16\}/<a made-up key>/g' -e 's/sh-[0-9a-f]\{16\}/<a made-up key>/g' "$WORK/answer7$case" | head -c 200)]"
    fi
    if grep -qF "$FIND" "$WORK/relay7$case.log"; then
        check_failed "(7 $case) an error echoing the key: the key is in the relay's log: [$(sed -e 's/ts-201-[a-z]*-[0-9a-f]*/<a made-up key>/g' -e 's/te-[0-9a-f]\{16\}/<a made-up key>/g' -e 's/sh-[0-9a-f]\{16\}/<a made-up key>/g' "$WORK/relay7$case.log" | head -c 200)]"
    fi
done
rm -f "$WORK/echo"

# --- (8) a redirect is not followed: the key goes to no other host --------------
mkdir "$WORK/elsewhere"
python3 -I "$WORK/upstream.py" "$WORK/elsewhere/seen" "$WORK/elsewhere/port" &
elsewhere_pid=$!
n=0
until [ -s "$WORK/elsewhere/port" ]; do
    n=$((n + 1))
    [ "$n" -lt 100 ] || harness "the second made-up host never started"
    sleep 0.1
done
printf 'http://127.0.0.1:%s/v1/systemone' "$(cat "$WORK/elsewhere/port")" >"$WORK/redirect-to"
printf '%s' "$CRED_KEY" >"$creds/typesafe.api-key"
for code in 301 302 303 307 308; do
    printf 'redirect %s' "$code" >"$WORK/echo"
    : >"$WORK/seen"
    : >"$WORK/elsewhere/seen"
    relay "$WORK/relay8$code.log" CREDENTIALS_DIRECTORY="$creds"
    post >/dev/null
    got=$(curl -s --noproxy '*' --max-time 10 -o "$WORK/answer8$code" -w '%{http_code}' -H 'Content-Type: application/json' \
        -d '{"model":"jev","question":"state?"}' "http://127.0.0.1:$RELAY_PORT/v1/systemone" || true)
    stop_relay
    [ "$got" = "$code" ] || check_failed "(8 $code) the caller got status $got, not the hosted API's $code"
    grep -qF "the hosted API answered $code" "$WORK/answer8$code" ||
        check_failed "(8 $code) the caller did not get the relay's reason: [$(head -c 200 "$WORK/answer8$code")]"
    if [ -s "$WORK/elsewhere/seen" ]; then
        if grep -qF "$CRED_KEY" "$WORK/elsewhere/seen"; then
            check_failed "(8 $code) a redirect to another host: that host got the key"
        else
            check_failed "(8 $code) a redirect to another host: that host got a request: [$(sed 's/ts-201-[a-z]*-[0-9a-f]*/<a made-up key>/g' "$WORK/elsewhere/seen" | tr '\n' '~')]"
        fi
    fi
done
rm -f "$WORK/echo"
kill "$elsewhere_pid" 2>/dev/null || true
wait "$elsewhere_pid" 2>/dev/null || true
elsewhere_pid=""

if [ "$failures" -gt 0 ]; then
    echo "$NAME: $failures check(s) failed" >&2
    exit 1
fi
echo "$NAME: the Jev relay takes its key from systemd's credential typesafe.api-key, and only from it when there is one; a missing or empty one stops it, saying which"
