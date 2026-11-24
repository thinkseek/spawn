#!/bin/sh
# Black box: free text that the SQL functions write into control records is
# stored masked, and a source URL carrying credentials is refused (objective
# 171). 170 masks the records the Rust side builds. These records are built
# by SQL from a command's arguments: a mission's text, a task's brief, a
# pause's reason, and so on.
#
# The world: a store of its own; commands as a person runs them.
#   (1) `spawn mission add --text "<text with a GitHub token and a
#       PGPASSWORD= value>" --source <dir>` exits 0, and neither the
#       mission.added record's text nor the mission's text holds them; each
#       holds [redacted].
#   (2) `spawn task assign --brief "<the same>"`: the task.assigned record's
#       brief and the task's brief hold neither value.
#   (3) `spawn mission pause --id N --reason <500 chars ending " PWD=x">`
#       exits 0 (masking never makes the store refuse it), and the stored
#       reason is at most 500 chars and holds no "PWD=x".
#   (4) `spawn mission add --source https://user:<token>@host/r.git` exits
#       non-zero, its error says the URL "carries credentials" and does not
#       repeat the token, and no mission names that source.
#   (5) (R3, rev-g's F2) A worker's report: a resident under a real `spawn
#       loop` (the fake engine of scripts/blackbox/assertions.sh's
#       bb_engine, as j04's) is given a task and writes .spawn/report.md
#       holding the same token and PGPASSWORD= value. Once the task is
#       reported (within 90 s), neither the task.reported record's report
#       nor the task's report holds them, and each holds [redacted].
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "171: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od git sed grep curl cut tmux bash; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_171_text_$run"
SPAWN_TMUX_SOCKET="bb-171-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL
HOME=$WORK/home
SPAWN_CLAUDE_PROJECTS=$WORK/home/projects
export HOME SPAWN_CLAUDE_PROJECTS
mkdir -p "$SPAWN_CLAUDE_PROJECTS" || harness "mkdir $SPAWN_CLAUDE_PROJECTS failed"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$TMUX_TMPDIR"
export TMUX_TMPDIR
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
: >"$WORK/pids"
cleanup() {
    while read -r pid; do
        kill -KILL "$pid" 2>/dev/null || true
    done <"$WORK/pids"
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        echo "171: the gate runs:" >&2
        cat "$WORK/judged.log" >&2 2>/dev/null || true
        echo "171: the integrator's last lines:" >&2
        grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 20 >&2 || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "171: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

: >"$WORK/failures"
check_failed() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL \
    SPAWN_GATE_MIN_FREE_GB SPAWN_GATE_JOBS SPAWN_GATE_WORKER_CORES SPAWN_GATE_MIN_FREE_MEMORY_MB \
    SPAWN_INTEGRATE_GATES SPAWN_TEST_DATABASE_LOG SPAWN_INTEGRATE_DRAIN_SECS SPAWN_GATE_ENV_FILE \
    SPAWN_INTEGRATE_WARM SPAWN_INTEGRATE_SHADOW SPAWN_GATE_RUNNER; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_WORKSPACE_ROOT=$WORK/ws
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT
mkdir -p "$SPAWN_WORKSPACE_ROOT"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

note() { echo "171: $*" >&2; }
q() { scalar "$1"; }

TOKEN=ghp_171madeup0123456789abcdefghijklmnopq
PASSWORD=hunter171hunter171
SECRET="curl -H 'Authorization: Bearer $TOKEN' and PGPASSWORD=$PASSWORD psql"
leaks() { case $1 in *"$TOKEN"* | *"$PASSWORD"*) return 0 ;; esac; return 1; }
masked_ok() { # <what> <text>
    if leaks "$2"; then check_failed "$1 holds a credential unmasked: [$(printf '%s' "$2" | head -c 200 | tr '\n' ' ')]"
    elif [ "${2#*\[redacted\]}" = "$2" ]; then check_failed "$1 holds no [redacted]: [$(printf '%s' "$2" | head -c 200 | tr '\n' ' ')]"
    fi
}
newest() { # <type> <key>
    q "select coalesce(kind -> 'detail' -> 'record' ->> '$2', '') from spawn_events
        where kind -> 'detail' -> 'record' ->> 'type' = '$1' order by id desc limit 1"
}
src=$WORK/src.git
git init -q --bare --initial-branch=main "$src"

capture "$SPAWN_BIN" mission add --text "171 mission: $SECRET" --source "$src"
[ "$status" -eq 0 ] || harness "mission add failed: $(err | tail -n 2 | tr '\n' ' ')"
mission=$(jq -r .mission "$WORK/out.txt")
masked_ok "(1) the mission.added record's text" "$(newest mission.added text)"
masked_ok "(1) the mission's text" "$(q "select text from mission_missions where id = $mission")"
note "(1) checked: a mission's text is masked"

capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker w171 --brief "171 brief: $SECRET"
[ "$status" -eq 0 ] || harness "task assign failed: $(err | tail -n 2 | tr '\n' ' ')"
task=$(jq -r .task "$WORK/out.txt")
masked_ok "(2) the task.assigned record's brief" "$(newest task.assigned brief)"
masked_ok "(2) the task's brief" "$(q "select brief from mission_tasks where id = $task")"
note "(2) checked: a task's brief is masked"

long_reason="$(printf '%494s' '' | tr ' ' r) PWD=x"
capture "$SPAWN_BIN" mission pause --id "$mission" --reason "$long_reason"
if [ "$status" -ne 0 ]; then
    check_failed "(3) a 500-char pause reason with a short secret was refused, exit $status: $(err | tail -n 2 | tr '\n' ' ')"
else
    got=$(newest mission.paused reason)
    length=$(q "select coalesce(length(kind -> 'detail' -> 'record' ->> 'reason'), 0) from spawn_events
                 where kind -> 'detail' -> 'record' ->> 'type' = 'mission.paused' order by id desc limit 1")
    [ "$length" -le 500 ] || check_failed "(3) the stored pause reason is $length chars, over 500"
    case $got in *"PWD=x"*) check_failed "(3) the stored pause reason holds the secret" ;; esac
fi
note "(3) checked: a reason at its limit is taken, masked, within it"

url="https://user:$TOKEN@git.example.org/acme/r.git"
capture "$SPAWN_BIN" mission add --text "171 remote" --source "$url"
said="$(cat "$WORK/out.txt" "$WORK/err.txt")"
[ "$status" -ne 0 ] || check_failed "(4) a source URL carrying a token was taken (exit 0)"
case $said in *"carries credentials"*) ;; *) check_failed "(4) the refusal does not say the URL carries credentials: [$(printf '%s' "$said" | head -c 300 | tr '\n' ' ')]" ;; esac
if leaks "$said"; then check_failed "(4) the refusal repeats the token: [$(printf '%s' "$said" | head -c 300 | tr '\n' ' ')]"; fi
[ "$(q "select count(*) from mission_missions where sources::text like '%git.example.org%'")" = 0 ] ||
    check_failed "(4) a mission names the source with the token"
# cc-astra A76: no refusal repeats a credential, whatever its form and
# whichever check refuses it (made-up values): an opaque token as the user
# (no known prefix), an all-digit password, one inside an overlength URL,
# and one in a source refused as malformed (another scheme, or none); R5
# (rev-g F1): a :// that is no scheme, a / before the @ (the last one also
# names a port that is not digits, which no git can fetch: refused); R6
# (rev-g F2): a password written where the port goes (no @ to cut at), all
# digits too: refused, and the refusal shows the scheme alone; and a text
# that is neither a path nor a URL (a token pasted alone) is not shown; R7
# (rev-g F3): nor is any text but a path or an https:// or ssh:// URL (a
# password where the port goes under http://, HTTPS://, ftp://, git://);
# R8 (cc-astra A83): a credential before a second colon in the authority,
# refused and never shown: no refusal repeats any URL-shaped text. R9
# (rev-g's R8 review, n3 and four unit-only mutants): an ssh:// URL with a
# password (opaque or all digits) is refused (171's own rule), a text port
# after a bracketed IPv6 host is refused, port 0 is refused, and a path
# with an @ refused for a control character is not shown.
OPAQUE=Zq81mPW04xLrT7vKjHc2NdYb
DIGITS=8675309123456
long=$(printf 'a%.0s' $(seq 1 4200))
for case in "https://$OPAQUE@git.example.org/acme/r.git|$OPAQUE|an opaque token as the user" \
    "https://deploy:$DIGITS@git.example.org/acme/r.git|$DIGITS|an all-digit password" \
    "https://u:$OPAQUE@git.example.org/$long|$OPAQUE|a credential in an overlength URL" \
    "ftp://u:$OPAQUE@git.example.org/acme/r.git|$OPAQUE|a credential in a malformed source" \
    "u:$DIGITS@git.example.org|$DIGITS|a credential in a source with no scheme" \
    "u:$OPAQUE@git.example.org/x://y|$OPAQUE|a credential before a :// that is no scheme" \
    "https://u:abc/$OPAQUE@git.example.org/r b.git|$OPAQUE|a credential behind a / in a malformed URL" \
    "ftp://u:abc/$OPAQUE@git.example.org/r.git|$OPAQUE|a credential behind a / in another scheme" \
    "https://u:abc/$OPAQUE@git.example.org/r.git|$OPAQUE|a credential behind a / and a port that is not digits" \
    "https://user:$OPAQUE/r.git|$OPAQUE|a password where the port goes, no host" \
    "ssh://git@git.example.org:$OPAQUE/r|$OPAQUE|a password where an ssh port goes" \
    "https://user:$DIGITS/r.git|$DIGITS|an all-digit password where the port goes" \
    "oauth2:$OPAQUE|$OPAQUE|a user and token pasted alone" \
    "$OPAQUE|$OPAQUE|a token pasted alone" \
    "http://oauth2:$OPAQUE/r.git|$OPAQUE|a password where the port goes under http://" \
    "HTTPS://user:$OPAQUE/r.git|$OPAQUE|a password where the port goes under HTTPS://" \
    "Https://user:$OPAQUE/r.git|$OPAQUE|a password where the port goes under Https://" \
    "SSH://git@git.example.org:$OPAQUE/r|$OPAQUE|a password where the port goes under SSH://" \
    "ftp://user:$OPAQUE/r|$OPAQUE|a password where the port goes under ftp://" \
    "git://git.example.org:$OPAQUE/r|$OPAQUE|a password where the port goes under git://" \
    "https://u/$OPAQUE@git.example.org/r b.git|$OPAQUE|a credential behind a / with no port, in a malformed URL" \
    "https://user:$OPAQUE:443/r b.git|$OPAQUE|a credential before a second colon (https, malformed)" \
    "ssh://git@git.example.org:$OPAQUE:22/r b.git|$OPAQUE|a credential before a second colon (ssh, malformed)" \
    "https://user:$OPAQUE:443/r.git|$OPAQUE|a credential before a second colon and a port in digits" \
    "ssh://git@git.example.org:$DIGITS:22/r|$DIGITS|an all-digit password before a second colon (ssh)" \
    "/srv/u:$OPAQUE@git.example.org$(printf '\001')|$OPAQUE|a credential in a path refused for a control character" \
    "ssh://git:$OPAQUE@git.example.org/acme/r.git|$OPAQUE|a password in an ssh:// URL" \
    "ssh://deploy:$DIGITS@git.example.org/acme/r.git|$DIGITS|an all-digit password in an ssh:// URL" \
    "https://[2001:db8::1]:$OPAQUE/acme/r.git|$OPAQUE|a password where the port goes after a bracketed host" \
    "https://git.example.org:0/acme/r.git|git.example.org:0|port 0" \
    "/srv/$OPAQUE@git.example.org$(printf '\001')|$OPAQUE|a credential before an @ in a path refused for a control character"; do
    src=${case%%|*}
    rest=${case#*|}
    secret=${rest%%|*}
    what=${rest#*|}
    capture "$SPAWN_BIN" mission add --text "171 remote A76" --source "$src"
    said="$(cat "$WORK/out.txt" "$WORK/err.txt")"
    [ "$status" -ne 0 ] || check_failed "(4) a source with $what was taken (exit 0)"
    case $said in *"$secret"*) check_failed "(4) the refusal of a source with $what repeats it: [$(printf '%s' "$said" | head -c 300 | tr '\n' ' ')]" ;; esac
done
[ "$(q "select count(*) from mission_missions where sources::text like '%git.example.org%'")" = 0 ] ||
    check_failed "(4) a mission names a source with a credential"
note "(4) checked: a source URL carrying credentials is refused by name"

# --- (5) a worker's report, through the deliver loop (R3, rev-g's F2) --------
repo=$WORK/repo.git
git init -q --bare --initial-branch=main "$repo"
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" -c user.name=bb -c user.email=bb@localhost commit -qm base
git -C "$WORK/seed" push -q "$repo" main:main
# The fake engine: a turn whose text names BB:WORK writes the report (the
# made-up token and password in it, as a worker that pasted a log line
# would), then ends as Claude Code's turn does: the transcript's
# turn_duration line, then the prompt.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=$2 ;; esac
  shift
done
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
work=
while :; do
  if bb_read_line typed; then
    printf 'you: %s\n' "$typed"
    [ "$pending" = 1 ] || line '"type":"user","message":{"role":"user","content":"prompt"}'
    pending=1
    case "$typed" in *"BB:WORK"*) work=1 ;; esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$work" ]; then
      mkdir -p .spawn
      printf '## Outcome\ndone; the deploy log said: %s\n## What changed\nnothing\n## Open issues\nnone\n' \
        "$BB_SECRET" >.spawn/report.md
      printf 'wrote %s\n' "$(date +%s)" >>"$BB_WORK/engine.log"
      work=
    fi
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"
cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"
: >"$WORK/engine.log"
capture "$SPAWN_BIN" resident propose --name core --charter "Owns the 171 report."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name core
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
# A mission of its own: (3) paused the first.
capture "$SPAWN_BIN" mission add --text "171 report"
[ "$status" -eq 0 ] || harness "mission add (report) failed: $(err)"
rmission=$(jq -r .mission "$WORK/out.txt")
capture "$SPAWN_BIN" task assign --mission "$rmission" --kind resident --worker core --brief "BB:WORK report it"
[ "$status" -eq 0 ] || harness "task assign (report) failed: $(err)"
rtask=$(jq -r .task "$WORK/out.txt")
[ -n "$rtask" ] && [ "$rtask" != null ] || harness "task assign (report) printed no id"
capture "$SPAWN_BIN" submit --principal bb-harness --agent agent_resident_core --session session_resident_core \
    --key bb-start-core --action '{"type":"start","engine":"claude","prompt":""}'
[ "$status" -eq 0 ] || harness "submit of the resident's start failed: $(err)"
(
    export SPAWN_GIT_REPO="$repo" SPAWN_CLAUDE_PROGRAM="$WORK/claude" SPAWN_CLAUDE_PROJECTS="$WORK/projects" \
        BB_PROJECTS="$WORK/projects" BB_WORK="$WORK" BB_SECRET="$SECRET" SPAWN_TMUX_PROGRAM="$WORK/tmux" \
        SPAWN_WORKER=worker-bb SPAWN_LOOP_INTERVAL_MS=300 SPAWN_DISK_WARN_GB=0 SPAWN_DISK_FLOOR_GB=0
    exec "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT"
) >>"$WORK/loop.log" 2>&1 &
echo "$!" >>"$WORK/pids"
reported() { [ "$(q "select state from mission_tasks where id = $rtask")" = reported ]; }
waited=0
until reported; do
    waited=$((waited + 1))
    [ "$waited" -lt 360 ] ||
        harness "(5) task $rtask was not reported within 90 s (state $(q "select state from mission_tasks where id = $rtask"); engine: $(tr '\n' ' ' <"$WORK/engine.log"); loop: $(tail -n 5 "$WORK/loop.log" | tr '\n' ' ' | cut -c1-400))"
    sleep 0.25
done
grep -q '^wrote' "$WORK/engine.log" || harness "(5) the engine never wrote the report"
# One line each (the report is markdown): newlines read as spaces.
rec=$(q "select replace(coalesce(kind -> 'detail' -> 'record' ->> 'report', ''), E'\\n', ' ') from spawn_events
          where kind -> 'detail' -> 'record' ->> 'type' = 'task.reported'
            and (kind -> 'detail' -> 'record' ->> 'id')::bigint = $rtask order by id desc limit 1")
row=$(q "select replace(coalesce(report, ''), E'\\n', ' ') from mission_tasks where id = $rtask")
for got in "task.reported record's report:$rec" "task's report:$row"; do
    what=${got%%:*}
    text=${got#*:}
    [ -n "$text" ] || { check_failed "(5) the $what is empty"; continue; }
    if leaks "$text"; then check_failed "(5) the $what holds a credential unmasked: [$(printf '%s' "$text" | head -c 200)]"; fi
    case $text in *"[redacted]"*) ;; *) check_failed "(5) the $what holds no [redacted]: [$(printf '%s' "$text" | head -c 200)]" ;; esac
done
note "(5) checked: a worker's report is stored masked"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "171: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "171: SQL-built records keep no credential, and a source URL carrying one is refused"
exit 0
