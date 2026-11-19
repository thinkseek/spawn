#!/usr/bin/env bash
# deploy/on-landing.sh (#54) against a fake live directory and the real
# binary on a throwaway database; the live fleet is never touched.
#
#   SPAWN_BIN=<a build of spawn> deploy/test-on-landing.sh
#
# The fake live directory ($SPAWN_LIVE) holds a bare missions repo, the
# controller symlink deploy.sh would leave, a spawn.env naming a database
# created on the test server (~/.config/spawn/test-database.url, or the file
# SPAWN_DEPLOY_TEST_URL_FILE names; never printed) and dropped at the end,
# and bin/spawn, the real build (SPAWN_BIN, default the workspace's
# target/debug/spawn). Every store answer the tick reads is the real
# binary's on the real schema: the mission, the task, its task.landed, the
# holds and the deploy records are written as the integrator and the CLI
# write them. Main's tree carries a stand-in deploy/deploy.sh (the real
# one's --main is deploy/test-deploy.sh's): it logs what it saw (its
# argument, its HEAD, the source's hold), then, as the real one does, moves
# the controller to HEAD, records deploy.finished and releases the hold;
# FAKE_DEPLOY=fail records a failure instead, and FAKE_DEPLOY=kill kills
# the tick and itself outright (SIGKILL: no trap runs).
#
# Covered: a live main deploys nothing; a main nothing gate-accepted landed
# deploys nothing; while another task of the source is judged the debounce
# waits for a quiet spell and the max wait bounds it, and with none judged
# main deploys at once (160); a held source, an owner's landing request with no outcome
# (S23), a pending deploy, a red nightly (until it is
# green), a busy deploy lock and the off switch each make the tick wait,
# and say so once; the deploy runs main's own deploy.sh --main from the
# checkout at main while the source is held with this script's reason; a
# failed deploy of a main is tried again only after the retry bound; a tick
# killed with its deploy leaves its hold, and the next tick releases it and
# deploys; a failure before deploy.sh runs is recorded as deploy.sh records
# its own (deploy.finished, by landing).
set -euo pipefail

repo=$(git rev-parse --show-toplevel)
PROG=test-on-landing
# shellcheck source=deploy/backup/lib.sh
. "$repo/deploy/backup/lib.sh"

failures=0
pass() { printf 'ok   %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; failures=$((failures + 1)); }
check() { # check <description> <command...>
  local what=$1; shift
  if "$@"; then pass "$what"; else fail "$what"; fi
}

spawn_bin=${SPAWN_BIN:-${CARGO_TARGET_DIR:-$repo/target}/debug/spawn}
[ -x "$spawn_bin" ] || die "no spawn build at $spawn_bin (cargo build -p spawn-cli, or SPAWN_BIN)"
test_url_file=${SPAWN_DEPLOY_TEST_URL_FILE:-$HOME/.config/spawn/test-database.url}
[ -f "$test_url_file" ] || die "no $test_url_file"
test_url=$(cat "$test_url_file")
case $test_url in *:15433/*) die "the test URL names port 15433, the live database" ;; esac
db=spawn_on_landing_test_$$
live_url=${test_url%/*}/$db

scratch=$(mktemp -d "${TMPDIR:-/tmp}/spawn-on-landing-test.XXXXXX")
# Never the real home (sync/cc-rules.md, after 2026-11-12): every tick, the
# binary and git run with HOME a directory of the scratch, and the test
# refuses (exit 2, nothing run) when that is empty, does not resolve, lies
# outside the scratch, or is, holds or lies in the real home.
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
fake_home=$scratch/home
mkdir -p "$fake_home"
guard_home() {
  local gh_scratch gh_real gh_path gh_name
  [ -n "$scratch" ] && [ -n "$real_home" ] && [ -n "${HOME-}" ] && [ -n "${live-}" ] || {
    echo "test-on-landing: refused: an empty home, scratch or live variable; nothing ran" >&2
    exit 2
  }
  gh_scratch=$(readlink -f -- "$scratch") && gh_real=$(readlink -f -- "$real_home") || {
    echo "test-on-landing: refused: a path that does not resolve; nothing ran" >&2
    exit 2
  }
  # HOME, the fake live directory, and every XDG root the test sets.
  for gh_name in HOME live XDG_CACHE_HOME XDG_CONFIG_HOME XDG_DATA_HOME; do
    eval "gh_path=\${$gh_name-}"
    [ -n "$gh_path" ] || {
      echo "test-on-landing: refused: $gh_name is empty; nothing ran" >&2
      exit 2
    }
    gh_path=$(readlink -m -- "$gh_path")
    case $gh_path/ in "$gh_scratch"/*) ;; *)
      echo "test-on-landing: refused: $gh_name $gh_path is not inside the scratch $gh_scratch; nothing ran" >&2
      exit 2
      ;;
    esac
    case $gh_path/ in "$gh_real"/*)
      echo "test-on-landing: refused: $gh_name $gh_path is in the real home; nothing ran" >&2
      exit 2
      ;;
    esac
    case $gh_real/ in "$gh_path"/*)
      echo "test-on-landing: refused: $gh_name $gh_path holds the real home; nothing ran" >&2
      exit 2
      ;;
    esac
  done
}
live=$scratch/live
export HOME=$fake_home XDG_CACHE_HOME=$fake_home/.cache XDG_CONFIG_HOME=$fake_home/.config \
  XDG_DATA_HOME=$fake_home/.local/share
guard_home
cleanup() {
  with_db "$test_url" psql -XAtq -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" >/dev/null 2>&1 || true
  rm -rf "$scratch"
}
trap cleanup EXIT
with_db "$test_url" psql -XAtq -v ON_ERROR_STOP=1 -c "CREATE DATABASE $db" >/dev/null

live=$scratch/live
missions=$live/missions/spawn.git
mkdir -p "$live/bin" "$live/controllers" "$scratch/ws/nightly"
printf 'SPAWN_DATABASE_URL=%q\nSPAWN_WORKSPACE_ROOT=%q\n' "$live_url" "$scratch/ws" >"$live/spawn.env"
chmod 600 "$live/spawn.env"
ln -s "$spawn_bin" "$live/bin/spawn"
spawn_live() { (set -a && . "$live/spawn.env" && set +a && "$live/bin/spawn" "$@"); }
spawn_live init-db >/dev/null
sql() { with_db "$live_url" psql -XAtq -v ON_ERROR_STOP=1 -c "$1"; }

# The source: a work tree whose commits are pushed to the bare missions
# repo, each with the stand-in deploy.sh.
work=$scratch/work
git init -q -b main "$work"
mkdir -p "$work/deploy"
cat >"$work/deploy/deploy.sh" <<'EOF'
#!/usr/bin/env bash
# The stand-in (test-on-landing.sh): what the real one's --main does to the
# world around it, nothing else.
set -euo pipefail
live=$SPAWN_LIVE
head=$(git rev-parse HEAD)
spawn() { "$live/bin/spawn" "$@"; }
hold=$(spawn mission landing --source "$live/missions/spawn.git" | jq -c '.hold')
echo "deploy $* at $head in $PWD, hold $hold" >>"$FAKE_LOG"
case ${FAKE_DEPLOY-} in
kill) kill -KILL "$PPID"; kill -KILL $$ ;;
fail)
  spawn mission deployed --head "$head" --ok false --by landing --reason "the stand-in failed" >/dev/null
  spawn mission release --source "$live/missions/spawn.git" >/dev/null
  exit 1
  ;;
esac
ln -sfn "spawn-$head" "$live/controllers/spawn"
spawn mission deployed --head "$head" --ok true --by landing --reason "" >/dev/null
spawn mission release --source "$live/missions/spawn.git" >/dev/null
EOF
commit_work() { # <message>: a new commit, pushed as the missions main; prints it
  echo "$1" >"$work/value"
  git -C "$work" add -A
  git -C "$work" -c user.name=test -c user.email=test@localhost commit -q -m "$1"
  git -C "$work" push -q "$missions" HEAD:refs/heads/main
  git -C "$work" rev-parse HEAD
}
git init -q --bare -b main "$missions"
c0=$(commit_work "the live commit")
ln -s "spawn-$c0" "$live/controllers/spawn"
mission=$(spawn_live mission add --text "test-on-landing: the Spawn mission" --source "$missions" | jq -r '.mission')
# Gated, as the live Spawn mission is: only a gated mission's tasks are
# judged (168: a review mission's never land). It accepts by its gate, so
# its reported task is the gate's input and is counted too.
mkdir -p "$scratch/scenarios"
spawn_live mission configure --id "$mission" --accept gate --gate "$scratch/scenarios" >/dev/null
# land <commit>: a task of the mission, landed at <commit> by the integrator.
land() {
  local task
  task=$(spawn_live task assign --mission "$mission" --kind ephemeral --worker w --brief "landed $1" | jq -r '.task')
  sql "select spawn_control_record(jsonb_build_object('type', 'task.landed', 'id', $task,
         'from', '$c0', 'to', '$1'))" >/dev/null
}
sql "select spawn_control_record(jsonb_build_object('type', 'source.moved', 'source', '$missions',
       'from', '$c0', 'to', '$c0'))" >/dev/null
# judged <state>: a task of the (gated) mission between its report and its
# landing (reported, accepted, landing), as `spawn mission landing`'s
# judging counts them; prints it. unjudge <task>: it landed.
judged() {
  local task
  task=$(spawn_live task assign --mission "$mission" --kind ephemeral --worker w --brief "judged" | jq -r '.task')
  sql "update mission_tasks set state = '$1' where id = $task" >/dev/null
  printf '%s\n' "$task"
}
unjudge() { sql "update mission_tasks set state = 'landed' where id = $1" >/dev/null; }
judging() { spawn_live mission landing --source "$missions" | jq -r '.judging'; }

export FAKE_LOG=$scratch/deploys.log
: >"$FAKE_LOG"
# The default quiet spell is a case of its own (235): a tick given no
# SPAWN_DEPLOY_QUIET_SECS uses on-landing.sh's, never the caller's.
unset SPAWN_DEPLOY_QUIET_SECS
# tick [ENV=VALUE...]: one tick, as the timer runs it; output in $out.
tick() {
  guard_home
  out=$scratch/tick.out
  set +e
  env SPAWN_LIVE="$live" "$@" bash "$repo/deploy/on-landing.sh" >"$out" 2>&1
  status=$?
  set -e
  sed 's/^/  | /' "$out"
}
deploys() { wc -l <"$FAKE_LOG"; }
live_commit() { readlink "$live/controllers/spawn"; }
holds() { spawn_live mission landing --source "$missions" | jq -c '.hold'; }
requests_pending() { spawn_live mission landing --source "$missions" | jq -c '[.owner_requests[].request]'; }
# owner_commit <message>: a commit on top of main that main does not hold,
# waiting at its owner ref as `spawn mission push` leaves one; prints it.
owner_commit() {
  local made
  made=$(git -C "$work" -c user.name=test -c user.email=test@localhost commit-tree \
    -p HEAD -m "$1" "HEAD^{tree}")
  git -C "$work" push -q "$missions" "$made:refs/spawn-owner/$made"
  printf '%s\n' "$made"
}
finished() {
  sql "select r ->> 'ok' || ' ' || (r ->> 'by') || ' ' || (r ->> 'head') || ' ' || (r ->> 'reason')
         from (select kind -> 'detail' -> 'record' as r, session_seq from spawn_events
                where session_id = 'session_control'
                  and kind -> 'detail' -> 'record' ->> 'type' = 'deploy.finished') d
        order by session_seq"
}

echo "== live at main"
tick
check "nothing deployed (status $status)" bash -c '[ "$1" -eq 0 ] && [ "$(wc -l <"$2")" -eq 0 ]' _ "$status" "$FAKE_LOG"
check "says it is live" grep -qF "live at main $c0" "$out"
tick
check "and says it once" [ ! -s "$out" ]

echo "== a main nothing gate-accepted landed"
c1=$(commit_work "pushed past the gate")
tick
check "not deployed" [ "$(deploys)" -eq 0 ]
check "says so" grep -qF "main $c1 has no gate-accepted landing in the store" "$out"

echo "== the debounce: while another task is judged, a landing a moment ago waits for a quiet spell"
land "$c1"
landed_s=$(date +%s)
pending=$(judged reported)
check "the store counts the task judged" [ "$(judging)" = 1 ]
tick SPAWN_DEPLOY_QUIET_SECS=600
check "not deployed yet" [ "$(deploys)" -eq 0 ]
check "says it waits for the quiet spell, and why" grep -qF "main $c1 waits: landed" "$out"
check "naming the task judged" grep -qF "and 1 task(s) of the source are judged or landing" "$out"
check "and names the bounds" grep -qF "deployed after 600 s without a landing (SPAWN_DEPLOY_QUIET_SECS), at the latest 1800 s" "$out"
tick SPAWN_DEPLOY_QUIET_SECS=600
check "said once" [ ! -s "$out" ]

echo "== the default quiet spell is 60 s (235): a landing a moment ago waits"
tick
check "not deployed yet" [ "$(deploys)" -eq 0 ]
check "names the default bounds" grep -qF "main $c1 waits: landed" "$out"
check "60 s without a landing, at the latest 1800 s" grep -qF "deployed after 60 s without a landing (SPAWN_DEPLOY_QUIET_SECS), at the latest 1800 s" "$out"

echo "== a held source waits for its holder"
spawn_live mission hold --source "$missions" --reason "owner gate $c1" >/dev/null
tick SPAWN_DEPLOY_QUIET_SECS=1
check "not deployed" [ "$(deploys)" -eq 0 ]
check "says who holds it" grep -qF "main $c1 waits: the source is held (owner gate $c1)" "$out"
spawn_live mission release --source "$missions" >/dev/null

echo "== an owner's landing request in judging (S23)"
# As spawn mission push records them, oldest first: one of main's own
# commit (its landing went out; its record follows at the integrator's
# next pass), then two that main does not hold.
held=$(sql "select spawn_source_push_request('$missions', '$c1', 'refs/spawn-owner/$c1')")
o1=$(owner_commit "the owner's first push")
o2=$(owner_commit "the owner's second push")
first=$(sql "select spawn_source_push_request('$missions', '$o1', 'refs/spawn-owner/$o1')")
second=$(sql "select spawn_source_push_request('$missions', '$o2', 'refs/spawn-owner/$o2')")
# Past the 1 s quiet spell, so nothing but the requests holds the deploy.
sleep 2
tick SPAWN_DEPLOY_QUIET_SECS=1
check "not deployed" [ "$(deploys)" -eq 0 ]
check "says the oldest request main does not hold waits, and how to withdraw it" \
  grep -qF "main $c1 waits: the owner's landing request $first ($o1) is waiting for its judging or in it; deployed once it has an outcome (spawn mission push --withdraw $first ends it)" "$out"
check "the request main holds already is not the one waited for" \
  bash -c '! grep -qF "landing request $1 " "$2"' _ "$held" "$out"
tick SPAWN_DEPLOY_QUIET_SECS=1
check "said once" [ ! -s "$out" ]
# Its outcome, as the integrator records one: refused names the request.
sql "select spawn_integrator_record(jsonb_build_object('type', 'source.push_refused',
       'request', $first, 'reason', 'test-on-landing: refused'))" >/dev/null
tick SPAWN_DEPLOY_QUIET_SECS=1
check "the next pending request still holds the deploy" \
  grep -qF "main $c1 waits: the owner's landing request $second ($o2)" "$out"
check "still not deployed" [ "$(deploys)" -eq 0 ]
sql "select spawn_integrator_record(jsonb_build_object('type', 'source.push_refused',
       'request', $second, 'reason', 'test-on-landing: refused'))" >/dev/null
check "the store still has main's own request pending" [ "$(requests_pending)" = "[$held]" ]
# Past the requests: a pending deploy is the next wait, so the request main
# holds does not hold the tick (and nothing deploys here).
echo "s23 $c1" >"$live/deploy.pending"
tick SPAWN_DEPLOY_QUIET_SECS=1
check "a request whose commit main holds holds nothing: the tick goes on to the next wait" \
  grep -qF "main $c1 waits: a deploy is pending" "$out"
rm "$live/deploy.pending"
sql "select spawn_integrator_record(jsonb_build_object('type', 'source.moved', 'source', '$missions',
       'from', '$c0', 'to', '$c1', 'request', $held, 'by', 'human'))" >/dev/null
check "no request left: the store says none" [ "$(requests_pending)" = "[]" ]

echo "== a pending deploy"
echo "$c0 $c1" >"$live/deploy.pending"
tick SPAWN_DEPLOY_QUIET_SECS=1
check "not deployed" [ "$(deploys)" -eq 0 ]
check "says so" grep -qF "main $c1 waits: a deploy is pending ($c0 $c1)" "$out"
rm "$live/deploy.pending"

echo "== a red nightly holds the deploy until a green night"
printf '{"v":1,"state":"red","failed":["023"],"red_since":"%s"}\n' "$c0" >"$scratch/ws/nightly/spawn.json"
tick SPAWN_DEPLOY_QUIET_SECS=1
check "not deployed" [ "$(deploys)" -eq 0 ]
check "says the nightly is red, and on what" grep -qF "main $c1 waits: the nightly is red since $c0 (failed: 023)" "$out"
printf '{"v":1,"state":"skipped","failed":["023"],"red_since":"%s"}\n' "$c0" >"$scratch/ws/nightly/spawn.json"
tick SPAWN_DEPLOY_QUIET_SECS=1
check "a skipped night keeps it red" [ "$(deploys)" -eq 0 ]

echo "not json" >"$scratch/ws/nightly/spawn.json"
tick SPAWN_DEPLOY_QUIET_SECS=1
check "a fact that does not read holds it too (closed, not open)" [ "$(deploys)" -eq 0 ]
check "says so" grep -qF "main $c1 waits: the nightly's fact $scratch/ws/nightly/spawn.json does not read as JSON" "$out"
echo '[]' >"$scratch/ws/nightly/spawn.json"
tick SPAWN_DEPLOY_QUIET_SECS=1
check "nor does a fact that is no object" [ "$(deploys)" -eq 0 ]

echo "== past the default 60 s, the quiet spell holds nothing (235): the tick goes on to the next wait"
printf '{"v":1,"state":"green","failed":[],"red_since":null}\n' >"$scratch/ws/nightly/spawn.json"
check "the store still counts the task judged" [ "$(judging)" = 1 ]
while [ $(($(date +%s) - landed_s)) -lt 62 ]; do sleep 1; done
flock -o "$live/deploy.lock" sleep 60 &
holder=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
  flock -n "$live/deploy.lock" true || break
  sleep 0.2
done
tick
kill "$holder" 2>/dev/null || true
wait "$holder" 2>/dev/null || true
check "not deployed while another deploy holds the lock" [ "$(deploys)" -eq 0 ]
check "the quiet spell is over: it waits for the lock, not for a landing" grep -qF "main $c1 waits: another deploy holds $live/deploy.lock" "$out"

echo "== the off switch and a busy deploy lock"
printf '{"v":1,"state":"green","failed":[],"red_since":null}\n' >"$scratch/ws/nightly/spawn.json"
tick SPAWN_DEPLOY_QUIET_SECS=1 SPAWN_DEPLOY_ON_LANDING=0
check "paused: not deployed" [ "$(deploys)" -eq 0 ]
check "says so" grep -qF "paused: SPAWN_DEPLOY_ON_LANDING=0" "$out"
sleep 1.2
flock -o "$live/deploy.lock" sleep 60 &
holder=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
  flock -n "$live/deploy.lock" true || break
  sleep 0.2
done
tick SPAWN_DEPLOY_QUIET_SECS=1
kill "$holder" 2>/dev/null || true
wait "$holder" 2>/dev/null || true
check "not deployed while another deploy holds the lock" [ "$(deploys)" -eq 0 ]
check "says so" grep -qF "main $c1 waits: another deploy holds $live/deploy.lock" "$out"
tick SPAWN_DEPLOY_QUIET_SECS=x
check "a bad bound is refused (status $status)" [ "$status" -ne 0 ]
check "naming it" grep -qF "SPAWN_DEPLOY_QUIET_SECS must be a whole number of seconds" "$out"

echo "== after the quiet spell: main's own deploy.sh --main, with the source held"
tick SPAWN_DEPLOY_QUIET_SECS=1
check "deployed (status $status)" [ "$status" -eq 0 ]
check "once" [ "$(deploys)" -eq 1 ]
check "main's deploy.sh, --main, in the checkout at main" \
  grep -qF "deploy --main at $c1 in $live/deploy-checkout, hold {" "$FAKE_LOG"
check "with the source held for this deploy" grep -qF "\"reason\":\"deploy-on-landing $c1\"" "$FAKE_LOG"
check "live at main now" [ "$(live_commit)" = "spawn-$c1" ]
check "the hold is released" [ "$(holds)" = null ]
tick SPAWN_DEPLOY_QUIET_SECS=1
check "the next tick deploys nothing" [ "$(deploys)" -eq 1 ]

echo "== the max wait: landings that keep coming are deployed within the bound"
c2=$(commit_work "first of a burst")
land "$c2"
sleep 2
c3=$(commit_work "second of a burst")
land "$c3"
tick SPAWN_DEPLOY_QUIET_SECS=600
check "inside the quiet spell and the max wait: not deployed" [ "$(deploys)" -eq 1 ]
tick SPAWN_DEPLOY_QUIET_SECS=600 SPAWN_DEPLOY_MAX_WAIT_SECS=2
check "past the max wait since the first landing not live (c2): deployed" [ "$(deploys)" -eq 2 ]
check "the burst's last main" [ "$(live_commit)" = "spawn-$c3" ]

echo "== a failed deploy is tried again after the retry bound"
c4=$(commit_work "a main whose deploy fails")
land "$c4"
sleep 1.2
tick SPAWN_DEPLOY_QUIET_SECS=1 SPAWN_DEPLOY_MAX_WAIT_SECS=1 FAKE_DEPLOY=fail
check "it ran and failed (status $status)" bash -c '[ "$1" -ne 0 ] && [ "$(wc -l <"$2")" -eq 3 ]' _ "$status" "$FAKE_LOG"
check "the failure is on the record" [ "$(finished | tail -n 1)" = "false landing $c4 the stand-in failed" ]
tick SPAWN_DEPLOY_QUIET_SECS=1 SPAWN_DEPLOY_MAX_WAIT_SECS=1
check "not tried again inside the bound" [ "$(deploys)" -eq 3 ]
check "says so" grep -qF "main $c4 waits: its deploy failed" "$out"
sleep 1
tick SPAWN_DEPLOY_QUIET_SECS=1 SPAWN_DEPLOY_MAX_WAIT_SECS=1 SPAWN_DEPLOY_RETRY_SECS=1
check "tried again after it, and deployed" bash -c '[ "$(wc -l <"$1")" -eq 4 ] && [ "$(readlink "$2")" = "spawn-$3" ]' _ "$FAKE_LOG" "$live/controllers/spawn" "$c4"

echo "== a tick killed with its deploy leaves its hold; the next tick releases it"
c5=$(commit_work "a deploy killed outright")
land "$c5"
sleep 1.2
tick SPAWN_DEPLOY_QUIET_SECS=1 SPAWN_DEPLOY_MAX_WAIT_SECS=1 FAKE_DEPLOY=kill
check "killed (status $status)" [ "$status" -eq 137 ]
check "its hold is left" [ "$(holds | jq -r .reason)" = "deploy-on-landing $c5" ]
check "and nothing went live" [ "$(live_commit)" = "spawn-$c4" ]
tick SPAWN_DEPLOY_QUIET_SECS=1 SPAWN_DEPLOY_MAX_WAIT_SECS=1
check "the next tick releases it" grep -qF "(deploy-on-landing $c5): its tick or deploy was killed before it could" "$out"
check "and deploys" [ "$(live_commit)" = "spawn-$c5" ]
check "the hold is gone" [ "$(holds)" = null ]
spawn_live mission hold --source "$missions" --reason "owner gate $c5" >/dev/null
tick
check "an owner's hold is never taken for a stale one" [ "$(holds | jq -r .reason)" = "owner gate $c5" ]
spawn_live mission release --source "$missions" >/dev/null

echo "== nothing judged: no burst can come, so a landing deploys at once (160)"
unjudge "$pending"
check "the store counts none judged" [ "$(judging)" = 0 ]
c8=$(commit_work "a landing with nothing else of the source judged")
land "$c8"
before=$(deploys)
tick SPAWN_DEPLOY_QUIET_SECS=600
check "deployed inside the quiet spell (status $status)" bash -c '[ "$1" -eq 0 ] && [ "$(wc -l <"$2")" -eq "$3" ]' _ "$status" "$FAKE_LOG" "$((before + 1))"
check "live at main now" [ "$(live_commit)" = "spawn-$c8" ]
check "and that 0 were judged" grep -qF "; 0 judged or landing" "$out"
for state in accepted landing; do
  pending=$(judged "$state")
  c9=$(commit_work "a landing beside a task $state")
  land "$c9"
  tick SPAWN_DEPLOY_QUIET_SECS=600
  check "a task $state keeps the quiet spell" grep -qF "main $c9 waits: landed" "$out"
  unjudge "$pending"
  tick SPAWN_DEPLOY_QUIET_SECS=600
  check "and once it landed, main deploys at once" [ "$(live_commit)" = "spawn-$c9" ]
done

echo "== an answer without judging (an older binary): the quiet holds (160's fallback; 168)"
nojudging=$scratch/spawn-nojudging
cat >"$nojudging" <<EOF
#!/usr/bin/env bash
if [ "\${1-}" = mission ] && [ "\${2-}" = landing ]; then
  "$spawn_bin" "\$@" | jq -c 'del(.judging)'
  exit "\${PIPESTATUS[0]}"
fi
exec "$spawn_bin" "\$@"
EOF
chmod +x "$nojudging"
ln -sfn "$nojudging" "$live/bin/spawn"
c10=$(commit_work "a landing read by a binary that does not say judging")
land "$c10"
tick SPAWN_DEPLOY_QUIET_SECS=600
check "with nothing judged, an answer without judging keeps the quiet spell" grep -qF "main $c10 waits: landed" "$out"
check "read as one judged" grep -qF "and 1 task(s) of the source are judged or landing" "$out"
ln -sfn "$spawn_bin" "$live/bin/spawn"
tick SPAWN_DEPLOY_QUIET_SECS=600
check "and with judging said again (0), main deploys at once" [ "$(live_commit)" = "spawn-$c10" ]

echo "== a failure before deploy.sh is recorded as deploy.sh records its own"
c6=$(commit_work "a main whose UI dependencies cannot be installed")
mkdir -p "$work/web" && echo "lockfileVersion: '9.0'" >"$work/web/pnpm-lock.yaml"
c6=$(commit_work "a lockfile")
land "$c6"
sleep 1.2
nopnpm=$scratch/nopnpm
mkdir -p "$nopnpm"
for tool in bash git jq flock true cat sed head tr mv rm cmp readlink basename dirname env mkdir wc sleep grep cut ls; do
  ln -sf "$(type -P "$tool")" "$nopnpm/$tool"
done
before=$(deploys)
tick PATH="$nopnpm" SPAWN_DEPLOY_QUIET_SECS=1 SPAWN_DEPLOY_MAX_WAIT_SECS=1
check "failed (status $status)" [ "$status" -ne 0 ]
check "deploy.sh did not run" [ "$(deploys)" -eq "$before" ]
check "recorded: by the landing, with the reason" \
  bash -c 'finished_last=$1; [ "$finished_last" = "false landing $2 pnpm is not on PATH: the UI'"'"'s dependencies for $2 cannot be installed" ]' _ "$(finished | tail -n 1)" "$c6"
check "and holds nothing" [ "$(holds)" = null ]

echo "== a checkout path that is no clone stops the tick; nothing is removed"
c7=$(commit_work "a main whose checkout path is taken")
land "$c7"
sleep 1.2
mv "$live/deploy-checkout" "$scratch/checkout-aside"
mkdir "$live/deploy-checkout"
echo mine >"$live/deploy-checkout/owners-file"
before=$(deploys)
tick SPAWN_DEPLOY_QUIET_SECS=1 SPAWN_DEPLOY_MAX_WAIT_SECS=1
check "failed (status $status)" [ "$status" -ne 0 ]
check "the path and what is in it are untouched" [ "$(cat "$live/deploy-checkout/owners-file")" = mine ]
check "deploy.sh did not run" [ "$(deploys)" -eq "$before" ]
check "recorded, with the reason" \
  grep -qF "false landing $c7 $live/deploy-checkout is there but is no clone" <<<"$(finished | tail -n 1)"

echo "== the home guard, and SPAWN_LIVE never defaulted"
guard_status=0
(HOME=$real_home; guard_home) 2>"$scratch/guard.err" || guard_status=$?
check "the real home is refused (status $guard_status)" \
  bash -c '[ "$1" -eq 2 ] && grep -q "HOME .* is not inside the scratch" "$2"' _ "$guard_status" "$scratch/guard.err"
guard_status=0
(live=$real_home/.local/share/spawn-rsi; guard_home) 2>"$scratch/guard.err" || guard_status=$?
check "a live directory outside the scratch is refused (status $guard_status)" \
  bash -c '[ "$1" -eq 2 ] && grep -q "live .* is not inside the scratch" "$2"' _ "$guard_status" "$scratch/guard.err"
guard_status=0
(fake_home=""; HOME=""; guard_home) 2>"$scratch/guard.err" || guard_status=$?
check "an empty HOME is refused (status $guard_status)" [ "$guard_status" -eq 2 ]
nolive_status=0
env -u SPAWN_LIVE bash "$repo/deploy/on-landing.sh" >"$scratch/nolive.out" 2>&1 || nolive_status=$?
check "on-landing.sh without SPAWN_LIVE refuses (status $nolive_status), touching nothing" \
  bash -c '[ "$1" -eq 2 ] && grep -q "SPAWN_LIVE is not set" "$2"' _ "$nolive_status" "$scratch/nolive.out"

echo
if [ "$failures" -eq 0 ]; then
  echo "on-landing: all cases pass"
else
  echo "on-landing: $failures case(s) failed"
  exit 1
fi
