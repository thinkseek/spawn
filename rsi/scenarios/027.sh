#!/bin/sh
# Black box: `spawn mission configure` changes only what it is given. Today
# it stores the whole configuration from the flags of that one call: an
# omitted --accept becomes `review`, and an omitted --gate, budget or
# backlog is dropped, so a configure meant to change one number switches a
# gate-landing objective mission to review-landing and forgets its gate,
# its budget and its backlog. It merges the settings given into the current
# ones instead; a setting is cleared only by an explicit flag (--no-gate,
# --no-budget, --no-objectives); it prints the configuration before and
# after; and the `mission.configured` record holds the whole resulting
# configuration.
#
# No loop and no integrator: the mission's row (mission_missions: gate,
# accept, budget, objectives) and its control records are read after each
# call. One mission on a source, configured in full first: --gate <dir>
# --accept gate --minutes 45 --tool-calls 60 --backlog <dir> --resident rsi
# --workers 3. Checked:
#   (1) `configure --id N --tool-calls 200`: budget.tool_calls is 200, and
#       gate, accept, budget.minutes and objectives (backlog, resident,
#       workers) are what they were.
#   (2) its output is JSON naming `before` and `after`, each with gate,
#       accept, budget and objectives: before.budget.tool_calls 60,
#       after.budget.tool_calls 200, the rest equal.
#   (3) the newest `mission.configured` record holds the whole resulting
#       configuration: gate, accept gate, budget {minutes 45, tool_calls 200},
#       objectives {backlog, resident rsi, workers 3}.
#   (4) `configure --id N --workers 2` changes objectives.workers alone (the
#       backlog and the resident stay); `--minutes 90` changes
#       budget.minutes alone.
#   (5) `--no-budget` clears the budget and nothing else; `--no-objectives`
#       clears the objectives and nothing else; `--no-gate` while objectives
#       are set is refused and changes nothing (objectives need a gate);
#       `--no-gate --accept review` then clears the gate.
#   (6) a call that sets nothing (`configure --id N`) changes nothing.
#
# Measured on the live main (49b92be, the deployed binary built from it),
# 2026-11-06: (1) leaves gate null, accept `review`, budget {tool_calls: 200}
# and objectives null; the output is {"mission", "gate": null, "accept":
# "review", "objectives": null}.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main: a configure of one number drops the
# gate, the backlog and the minutes and turns accept to review. It passes
# once configure merges what it is given into the mission's current
# configuration, clears only on an explicit --no-* flag, prints before and
# after, and records the whole result.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, no network.
set -eu

asserted=""
harness() {
    echo "027: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_027_configure_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
finish() {
    rc=$?
    trap - EXIT
    if [ -n "$database_made" ]; then
        if [ "$rc" -ne 0 ]; then
            echo "027: the mission's configuration records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select kind -> 'detail' -> 'record' from spawn_events
                where kind -> 'detail' -> 'record' ->> 'type' = 'mission.configured' order by id" >&2 2>/dev/null || true
        fi
        psql -d postgres -tA -c "drop database if exists $SPAWN_DB_NAME with (force)" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "027: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"
fail() {
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}

unset SPAWN_GATE SPAWN_JUDGE_URL SPAWN_SUPERVISOR 2>/dev/null || true
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
export SPAWN_DATABASE_URL
tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

cat >"$WORK/gitconfig" <<'EOF'
[user]
	name = bb
	email = bb@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
EOF
GIT_CONFIG_GLOBAL=$WORK/gitconfig
export GIT_CONFIG_GLOBAL
source=$WORK/source.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git init -q --bare --initial-branch=main "$source"
git -C "$WORK/seed" push -q "$source" main:main
gate=$WORK/gate
backlog=$WORK/backlog
mkdir -p "$gate" "$backlog"
printf '# nothing protected\n' >"$gate/protected"
gate=$(cd "$gate" && pwd)
backlog=$(cd "$backlog" && pwd)

q() { scalar "$1"; }
# The mission's configuration as its row holds it, one canonical JSON line.
row() {
    q "select jsonb_build_object('gate', gate, 'accept', accept, 'budget', budget,
                                 'objectives', objectives)::text
         from mission_missions where id = $mission"
}
want() { # <gate|null> <accept> <budget json|null> <objectives json|null>
    q "select jsonb_build_object('gate', $1, 'accept', '$2', 'budget', $3::jsonb,
                                 'objectives', $4::jsonb)::text"
}
configure() {
    capture "$SPAWN_BIN" mission configure --id "$mission" "$@"
}
OBJ="'{\"backlog\": \"$backlog\", \"resident\": \"rsi\", \"workers\": 3}'"

capture "$SPAWN_BIN" mission add --text "027 configure" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
configure --gate "$gate" --accept gate --minutes 45 --tool-calls 60 --backlog "$backlog" \
    --resident rsi --workers 3
[ "$status" -eq 0 ] || harness "the first, full configure exited $status: $(err)"
full=$(want "'$gate'" gate "'{\"minutes\": 45, \"tool_calls\": 60}'" "$OBJ")
[ "$(row)" = "$full" ] || harness "the full configure stored $(row), not $full"

# --- (1) one number ------------------------------------------------------------------

configure --tool-calls 200
[ "$status" -eq 0 ] || fail "(1) \`spawn mission configure --id $mission --tool-calls 200\` exited $status: $(err | head -n 2)"
printed=$(out)
expected=$(want "'$gate'" gate "'{\"minutes\": 45, \"tool_calls\": 200}'" "$OBJ")
got=$(row)
[ "$got" = "$expected" ] ||
    fail "(1) \`spawn mission configure --id $mission --tool-calls 200\` changed more than the tool-call budget: the mission is now $got, was $full (wanted $expected)"

# --- (2) before and after --------------------------------------------------------------

side() { # <before|after>: the printed side, canonical
    printf '%s' "$printed" | jq -c --arg s "$1" '.[$s] | {gate, accept, budget, objectives}' 2>/dev/null |
        while IFS= read -r json; do
            q "select '$(printf '%s' "$json" | sed "s/'/''/g")'::jsonb::text"
        done
}
[ "$(side before)" = "$full" ] ||
    fail "(2) the output's \`before\` is [$(side before)], not the configuration before the call, $full (output: $(printf '%s' "$printed" | tr -d '\n' | head -c 300))"
[ "$(side after)" = "$expected" ] ||
    fail "(2) the output's \`after\` is [$(side after)], not the configuration after the call, $expected"

# --- (3) the record holds the whole result -------------------------------------------------

record=$(q "select (kind -> 'detail' -> 'record') - 'type' - 'id' from spawn_events
             where kind -> 'detail' -> 'record' ->> 'type' = 'mission.configured'
               and kind -> 'detail' -> 'record' ->> 'id' = '$mission'
             order by id desc limit 1")
recorded=$(q "select jsonb_build_object('gate', r -> 'gate', 'accept', r -> 'accept',
                                        'budget', r -> 'budget', 'objectives', r -> 'objectives')::text
                from (select '$(printf '%s' "$record" | sed "s/'/''/g")'::jsonb as r) x")
[ "$recorded" = "$expected" ] ||
    fail "(3) the newest mission.configured record holds $recorded, not the whole resulting configuration $expected"

# --- (4) one setting of a group --------------------------------------------------------------

configure --workers 2
[ "$status" -eq 0 ] || fail "(4) \`configure --workers 2\` exited $status: $(err | head -n 1)"
expected=$(want "'$gate'" gate "'{\"minutes\": 45, \"tool_calls\": 200}'" \
    "'{\"backlog\": \"$backlog\", \"resident\": \"rsi\", \"workers\": 2}'")
[ "$(row)" = "$expected" ] || fail "(4) \`configure --workers 2\` left the mission $(row), not $expected"
configure --minutes 90
[ "$status" -eq 0 ] || fail "(4) \`configure --minutes 90\` exited $status: $(err | head -n 1)"
expected=$(want "'$gate'" gate "'{\"minutes\": 90, \"tool_calls\": 200}'" \
    "'{\"backlog\": \"$backlog\", \"resident\": \"rsi\", \"workers\": 2}'")
[ "$(row)" = "$expected" ] || fail "(4) \`configure --minutes 90\` left the mission $(row), not $expected"

# --- (5) clearing is explicit -------------------------------------------------------------------

configure --no-budget
[ "$status" -eq 0 ] || fail "(5) \`configure --no-budget\` exited $status: $(err | head -n 1)"
expected=$(want "'$gate'" gate null "'{\"backlog\": \"$backlog\", \"resident\": \"rsi\", \"workers\": 2}'")
[ "$(row)" = "$expected" ] || fail "(5) \`configure --no-budget\` left the mission $(row), not $expected"
before=$(row)
configure --no-gate
[ "$status" -ne 0 ] ||
    fail "(5) \`configure --no-gate\` on a mission with objectives succeeded (objectives need a gate): the mission is now $(row)"
[ "$(row)" = "$before" ] || fail "(5) a refused \`configure --no-gate\` changed the mission to $(row)"
configure --no-objectives
[ "$status" -eq 0 ] || fail "(5) \`configure --no-objectives\` exited $status: $(err | head -n 1)"
expected=$(want "'$gate'" gate null null)
[ "$(row)" = "$expected" ] || fail "(5) \`configure --no-objectives\` left the mission $(row), not $expected"
configure --no-gate --accept review
[ "$status" -eq 0 ] || fail "(5) \`configure --no-gate --accept review\` exited $status: $(err | head -n 1)"
expected=$(want null review null null)
[ "$(row)" = "$expected" ] || fail "(5) \`configure --no-gate --accept review\` left the mission $(row), not $expected"

# --- (6) nothing given, nothing changed ------------------------------------------------------------

configure --gate "$gate" --accept gate --tool-calls 7
[ "$status" -eq 0 ] || fail "(6) \`configure --gate <dir> --accept gate --tool-calls 7\` exited $status: $(err | head -n 1)"
before=$(row)
configure
[ "$(row)" = "$before" ] || fail "(6) \`configure --id $mission\` with no setting changed the mission from $before to $(row)"

echo "027: each configure changed only what it was given, cleared only on an explicit flag, printed before and after, and recorded the whole result"
exit 0
