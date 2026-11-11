#!/usr/bin/env bash
# scripts/rsi/nightly.sh, against a stub gate and the real store: green,
# red (once alerted, the same red again silent), flaky (clears), a host
# fault on main (red), a gate with no room (skipped, the alert kept), a busy
# host (skipped, no gate run), a second run of the same source (does
# nothing), the host slots it holds, and what its gate is given (main
# against itself, the one scenario as a check, the integrator's gate
# environment and not the owner's database).
#
#   SPAWN_TEST_DATABASE_URL=… SPAWN_BIN=target/debug/spawn bash scripts/rsi/test-nightly.sh
#
# A database of its own on that server (made, `spawn init-db`, dropped);
# everything else in a scratch directory.
set -uo pipefail
here=$(cd "$(dirname "$0")/../.." && pwd)
: "${SPAWN_TEST_DATABASE_URL:?test-nightly needs SPAWN_TEST_DATABASE_URL}"
: "${SPAWN_BIN:?test-nightly needs SPAWN_BIN (a built spawn, for init-db)}"
spawn_bin=$(readlink -f "$SPAWN_BIN")
fails=0 oks=0
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

scratch=$(mktemp -d "${TMPDIR:-/tmp}/test-nightly.XXXXXX")
# shellcheck source=scripts/pg-url.sh
. "$here/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" || {
  echo "SPAWN_TEST_DATABASE_URL is not one postgres URL" >&2
  exit 2
}
db="nightly_test_$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
made=""
cleanup() {
  [ -z "$made" ] || sh "$here/scripts/blackbox/scenario-db.sh" drop "$db" >/dev/null 2>&1 || true
  rm -rf "$scratch"
}
trap cleanup EXIT
psql -X -q -d postgres -c "create database $db" >/dev/null || exit 2
made=1
# The test database's URL with this database: built here, kept in a file
# and the environment, never an argument.
url="$(pg_url_server "$SPAWN_TEST_DATABASE_URL")/$db"
SPAWN_DATABASE_URL="$url" "$spawn_bin" init-db >"$scratch/init-db.log" 2>&1 || {
  cat "$scratch/init-db.log" >&2
  exit 2
}

# The live layout: a controller (the real scripts and deploy/gate-env.sh,
# gate.sh a stub), its scenarios (one the gate runs, two nightly), the
# missions repository.
live=$scratch/live
ctl=$live/controllers/spawn-abc
mkdir -p "$ctl/rsi/scenarios/nightly" "$ctl/deploy" "$scratch/home" "$scratch/ws" "$scratch/bin" "$scratch/behave"
ln -s spawn-abc "$live/controllers/spawn"
cp -a "$here/scripts" "$ctl/scripts"
cp "$here/deploy/gate-env.sh" "$ctl/deploy/"
printf 'crates/\n' >"$ctl/rsi/scenarios/protected"
printf '#!/bin/sh\ntrue\n' >"$ctl/rsi/scenarios/build"
printf '011.sh\n023.sh\n' >"$ctl/rsi/scenarios/serial"
printf '#!/bin/sh\necho the gate runs me, the nightly does not\n' >"$ctl/rsi/scenarios/011.sh"
for name in 023 025; do printf '#!/bin/sh\n# nightly %s\nexit 0\n' "$name" >"$ctl/rsi/scenarios/nightly/$name.sh"; done
# The stub gate: what it was given goes to $scratch/calls; what it answers
# comes from $scratch/behave/<scenario>: pass, flaky, fail (rejected),
# main (failed twice on the base with the same tests), asserts (exit 1 on
# every run, no failing tests to compare: what the real gate says of a
# check main always fails, judged against itself), harness (exit 2 on every
# run), noroom.
cat >"$ctl/scripts/rsi/gate.sh" <<'EOF'
#!/bin/sh
# The gate gets the integrator's gate environment alone: its HOME is the
# scratch's home.
NIGHTLY_TEST_SCRATCH=$(dirname "$HOME")
calls=$NIGHTLY_TEST_SCRATCH/calls
dir=$4
name=$(cd "$dir" && ls ./*.sh | sed 's|^\./||' | tr '\n' ' ')
scenario=$(cd "$dir" && ls check-nightly-*.sh 2>/dev/null | head -n 1)
plain=${scenario#check-nightly-}
plain=${plain%-run.sh}.sh
{
  echo "call repo=$1 base=$2 candidate=$3 scripts=$name serial=$(tr '\n' ' ' <"$dir/serial")"
  echo "env jobs=${SPAWN_GATE_JOBS-} host=${SPAWN_GATE_HOST_JOBS-} unit=${NIGHTLY_UNIT_ONLY-} db=${SPAWN_DATABASE_URL:+set} pghost=${PGHOST:+set} pgpassword=${PGPASSWORD:+set}"
  # The gate hides its verdict's directory from the sandbox: the scenarios
  # and scripts it runs must lie elsewhere.
  case $dir/ in "$(dirname "$SPAWN_GATE_VERDICT")"/*) echo "verdict beside the scenarios" ;; *) echo "verdict apart" ;; esac
  for i in 1 2 3; do
    if flock -n "$HOME/.cache/spawn/gate-slots/slot.$i" true 2>/dev/null; then echo "slot.$i free"; else echo "slot.$i held"; fi
  done
} >>"$calls"
echo "gate (stub): $scenario on $3"
case $(cat "$NIGHTLY_TEST_SCRATCH/behave/$plain" 2>/dev/null || echo pass) in
pass) printf '{"verdict":"accepted"}\n' >"$SPAWN_GATE_VERDICT" ;;
flaky) printf '{"verdict":"accepted","flakes":["%s"]}\n' "$scenario" >"$SPAWN_GATE_VERDICT" ;;
sleep)
  sleep 30
  printf '{"verdict":"accepted"}\n' >"$SPAWN_GATE_VERDICT"
  ;;
fail) printf '{"verdict":"rejected","reason":"regression: %s must still pass"}\n' "$scenario" >"$SPAWN_GATE_VERDICT" ;;
main)
  printf '{"host_fault":true,"on":"main","scenario":"%s","reason":"main is red on it"}\n' "$scenario" >"$SPAWN_GATE_VERDICT"
  exit 6
  ;;
asserts | harness)
  case $(cat "$NIGHTLY_TEST_SCRATCH/behave/$plain") in asserts) e=1 ;; *) e=2 ;; esac
  printf '{"v":1,"host_fault":true,"scenario":"%s","on":"candidate and base","runs":"%s, then %s on the candidate; %s, then %s on the base","reason":"failed twice on the candidate and on the base: a host or harness fault"}\n' \
    "$scenario" "$e" "$e" "$e" "$e" >"$SPAWN_GATE_VERDICT"
  exit 6
  ;;
noroom)
  echo "gate: no host scenario slot within 1800 s"
  printf '{"no_room":true,"check":"host scenario slot","reason":"no slot"}\n' >"$SPAWN_GATE_VERDICT"
  exit 5
  ;;
esac
exit 0
EOF
repo=$live/missions/spawn.git
git init -q --bare --initial-branch=main "$repo"
seed=$scratch/seed
git init -q --initial-branch=main "$seed"
commit() { # commit <subject>: a new main, its sha printed
  echo "$1" >>"$seed/log.txt"
  git -C "$seed" add log.txt
  git -C "$seed" -c user.name=t -c user.email=t@t commit -q -m "$1"
  git -C "$seed" push -q "$repo" HEAD:main
  git -C "$seed" rev-parse HEAD
}
# The integrator's gate environment (gate.rs write_gate_env's format).
printf 'SPAWN_INTEGRATE_PID=1\0SPAWN_GATE_HOST_JOBS=3\0PATH=%s\0HOME=%s\0' \
  "$scratch/bin:$PATH" "$scratch/home" >"$scratch/gate-env"
# systemctl and pgrep, as the host shows its gates: a busy host while
# $scratch/busy exists.
cat >"$scratch/bin/systemctl" <<'EOF'
#!/bin/sh
case "$*" in
*LastTriggerUSec*) [ -e "$NIGHTLY_TEST_SCRATCH/fired" ] && echo "Tue 2026-11-10 02:31:04 CST" ;;
*list-units*) [ -e "$NIGHTLY_TEST_SCRATCH/busy" ] && echo "spawn-gate-1-abc-0.service loaded active running spawn gate" ;;
esac
exit 0
EOF
printf '#!/bin/sh\nexit 1\n' >"$scratch/bin/pgrep"
chmod +x "$scratch/bin/systemctl" "$scratch/bin/pgrep"

# A busy host: another gate's scenario run holding slot 2 (and its unit
# listed), until busy_off.
slots=$scratch/home/.cache/spawn/gate-slots
holder=""
busy_on() {
  mkdir -p "$slots" && chmod 700 "$slots"
  : >>"$slots/slot.2"
  touch "$scratch/busy"
  flock "$slots/slot.2" sleep "${1:-600}" &
  holder=$!
  sleep 0.3
}
busy_off() {
  [ -z "$holder" ] || kill "$holder" 2>/dev/null
  [ -z "$holder" ] || wait "$holder" 2>/dev/null
  pkill -f "^sleep ${1:-600}\$" 2>/dev/null
  holder=""
  rm -f "$scratch/busy"
}
fact=$scratch/ws/nightly/spawn.json
nightly() { # the nightly as its unit runs it: spawn.env's variables
  : >"$scratch/calls"
  HOME="$scratch/home" PATH="$scratch/bin:$PATH" SPAWN_LIVE="$live" SPAWN_WORKSPACE_ROOT="$scratch/ws" \
    SPAWN_GATE_ENV_FILE="$scratch/gate-env" NIGHTLY_TEST_SCRATCH="$scratch" NIGHTLY_UNIT_ONLY=1 SPAWN_DATABASE_URL="$url" \
    SPAWN_NIGHTLY_WAIT_SECS="${WAIT:-60}" SPAWN_NIGHTLY_POLL_SECS=1 SPAWN_NIGHTLY_SAMPLE_SECS=1 \
    SPAWN_NIGHTLY_LOCK_WAIT_SECS="${LOCKWAIT:-120}" \
    bash "$ctl/scripts/rsi/nightly.sh" spawn "$@" >"$scratch/out" 2>&1
}
field() { jq -r "$1" "$fact"; }
store() { psql -X -At -q -d "$db" -c "$1"; }
alerts_fired() { store "select count(*) from spawn_alerts where kind = 'nightly_red'"; }
stale_keys() { store "select coalesce(string_agg(key, ' '), '') from spawn_alerts where kind = 'nightly_stale' and state = 'open'"; }
open_key() { store "select coalesce(string_agg(key, ' '), '') from spawn_alerts where kind = 'nightly_red' and state = 'open'"; }

echo "== green"
m1=$(commit "one")
nightly
rc=$?
check "exit 0 ($(tail -n 1 "$scratch/out"))" [ "$rc" -eq 0 ]
check "state green" [ "$(field .state)" = green ]
check "the fact file has the design's keys" \
  [ "$(jq -c '[has("v"), .v, has("main"), has("at"), has("state"), has("failed"), has("last_green")]' "$fact")" = '[true,1,true,true,true,true,true]' ]
check "main and last_green are main" [ "$(field .main) $(field .last_green)" = "$m1 $m1" ]
check "failed is empty" [ "$(field '.failed | length')" = 0 ]
check "two gates, one per nightly scenario" [ "$(grep -c '^call' "$scratch/calls")" = 2 ]
check "main judged against itself, in the missions repository" grep -q "^call repo=$repo base=$m1 candidate=$m1 " "$scratch/calls"
check "each gate holds its scenario alone, as a check, and on the serial list" \
  grep -qx "call repo=$repo base=$m1 candidate=$m1 scripts=check-nightly-023-run.sh  serial=check-nightly-023-run.sh " "$scratch/calls"
check "the gate's environment is the integrator's (one job), not the unit's nor its database" \
  grep -qx 'env jobs=1 host=3 unit= db= pghost= pgpassword=' "$scratch/calls"
check "slots 2 and 3 held for the run, slot 1 left to its gate" \
  [ "$(grep -c '^slot\.1 free' "$scratch/calls") $(grep -c '^slot\.[23] held' "$scratch/calls")" = "2 4" ]
check "the verdict's directory holds neither the scenarios nor the scripts" \
  [ "$(grep -c '^verdict apart' "$scratch/calls") $(grep -c '^verdict beside' "$scratch/calls")" = "2 0" ]
check "no alert" [ "$(alerts_fired)" = 0 ]

echo "== red"
m2=$(commit "two: breaks 023")
echo asserts >"$scratch/behave/023.sh"
nightly
check "state red, 023 failed" [ "$(field .state) $(field '.failed | join(",")')" = "red 023.sh" ]
check "red since this main, last green the one before" [ "$(field .red_since) $(field .last_green)" = "$m2 $m1" ]
check "one alert, keyed by the red's first main" [ "$(open_key)" = "nightly_red:spawn:$m2" ]
check "its evidence lists the landing since the last green" \
  [ "$(store "select kind -> 'detail' -> 'record' -> 'evidence' ->> 'landings' from spawn_events where event_name = 'control.recorded' and kind -> 'detail' -> 'record' ->> 'type' = 'alert.fired'")" = "[\"$(git -C "$seed" rev-parse --short "$m2") two: breaks 023\"]" ]
check "the gate's log kept beside the fact file" grep -q "gate (stub): check-nightly-023-run.sh on $m2" "$scratch/ws/nightly/spawn.023.sh.log"

echo "== a busy night during the red"
busy_on
WAIT=1 nightly
busy_off
check "skipped, the red kept: since $m2, 023 failed" \
  [ "$(field .state) $(field .red_since) $(field '.failed | join(",")')" = "skipped $m2 023.sh" ]
check "its alert still open, alone" [ "$(open_key) $(alerts_fired)" = "nightly_red:spawn:$m2 1" ]

echo "== the same red the next night (rejected now: failed twice, then passed on the base)"
m3=$(commit "three")
echo fail >"$scratch/behave/023.sh"
nightly
check "still red since the first" [ "$(field .state) $(field .red_since)" = "red $m2" ]
check "no second alert" [ "$(alerts_fired)" = 1 ]

echo "== a harness failing on every run"
echo harness >"$scratch/behave/025.sh"
nightly
check "red still (023), 025 skipped, not failed" \
  [ "$(field .state) $(field '.failed | join(",")') $(field '.skipped | join(",")')" = "red 023.sh 025.sh" ]
rm -f "$scratch/behave/025.sh"

echo "== flaky"
echo flaky >"$scratch/behave/023.sh"
nightly
check "state flaky, nothing failed" [ "$(field .state) $(field '.failed | length') $(field '.flaky | join(",")')" = "flaky 0 023.sh" ]
check "last green moves to main" [ "$(field .last_green)" = "$m3" ]
check "the alert is cleared" [ "$(open_key)" = "" ]

echo "== a host fault on main"
m4=$(commit "four")
echo main >"$scratch/behave/025.sh"
echo pass >"$scratch/behave/023.sh"
nightly
check "state red, 025 failed" [ "$(field .state) $(field '.failed | join(",")')" = "red 025.sh" ]
check "a new alert for this red" [ "$(open_key)" = "nightly_red:spawn:$m4" ]

echo "== red beside a skipped one"
echo noroom >"$scratch/behave/023.sh"
nightly
check "red wins over skipped" [ "$(field .state) $(field '.failed | join(",")') $(field '.skipped | join(",")')" = "red 025.sh 023.sh" ]
check "the same red: still one alert open" [ "$(open_key)" = "nightly_red:spawn:$m4" ]
echo pass >"$scratch/behave/023.sh"

echo "== no room"
echo noroom >"$scratch/behave/025.sh"
nightly
check "state skipped, 025 named, the reason said" \
  [ "$(field .state) $(field '.skipped | join(",")')" = "skipped 025.sh" ]
check "the reason names the exit" grep -q 'exited 5' <<<"$(field .reason)"
check "the alert stays open" [ "$(open_key)" = "nightly_red:spawn:$m4" ]

echo "== green again"
rm -f "$scratch/behave/025.sh"
nightly
check "state green" [ "$(field .state)" = green ]
check "the alert is cleared" [ "$(open_key)" = "" ]
check "two alerts in all" [ "$(alerts_fired)" = 2 ]
check "red_since is null" [ "$(field .red_since)" = null ]
check "each scenario's log is this run's" \
  grep -q "gate (stub): check-nightly-023-run.sh on $(field .main)" "$scratch/ws/nightly/spawn.023.sh.log"

echo "== a busy host"
busy_on
WAIT=2 nightly
check "skipped: a slot stayed held ($(field .reason))" \
  [ "$(field .state) $(field .gates_beside) $(field .skipped_runs)" = "skipped 1 1" ]
check "the reason says the slots were not free" grep -q 'slots were not all free within 2s' <<<"$(field .reason)"
check "no gate run" [ ! -s "$scratch/calls" ]
check "no stale alert after one skipped run" [ "$(stale_keys)" = "" ]

echo "== the second skipped run in a row"
judged=$(field .judged_at)
WAIT=1 nightly
busy_off
check "two in a row" [ "$(field .skipped_runs)" = 2 ]
check "one nightly_stale alert, keyed by the last run that judged" [ "$(stale_keys)" = "nightly_stale:spawn:$judged" ]
busy_on
WAIT=1 nightly
busy_off
check "still one stale alert after a third" [ "$(store "select count(*) from spawn_alerts where kind = 'nightly_stale'")" = 1 ]

echo "== the host drains: a slot held for 3 s more"
busy_on 3
WAIT=30 nightly
busy_off 3
check "it waited for the run holding the slot, then judged ($(grep -o 'drained in [0-9]*s' "$scratch/out"))" \
  bash -c '[ "$(jq -r .state "$1")" = green ] && grep -qE "drained in [2-9]s" "$2"' _ "$fact" "$scratch/out"
check "the judging run clears the stale alert" [ "$(stale_keys)" = "" ]
check "skipped_runs back to 0" [ "$(field .skipped_runs)" = 0 ]

echo "== no fact younger than 26 h (the hourly --stale check)"
jq '.at = (.at - 100000000)' "$fact" >"$fact.old" && mv "$fact.old" "$fact"
: >"$scratch/calls"
HOME="$scratch/home" PATH="$scratch/bin:$PATH" SPAWN_LIVE="$live" SPAWN_WORKSPACE_ROOT="$scratch/ws" \
  NIGHTLY_TEST_SCRATCH="$scratch" SPAWN_DATABASE_URL="$url" \
  bash "$ctl/scripts/rsi/nightly.sh" --stale spawn >"$scratch/out" 2>&1
check "--stale fires one alert ($(tail -n 1 "$scratch/out"))" [ "$(stale_keys)" = "nightly_stale:spawn:$(field .judged_at)" ]
exec 7>>"$scratch/ws/nightly/.spawn.lock"
flock 7
HOME="$scratch/home" PATH="$scratch/bin:$PATH" SPAWN_LIVE="$live" SPAWN_WORKSPACE_ROOT="$scratch/ws" \
  NIGHTLY_TEST_SCRATCH="$scratch" SPAWN_DATABASE_URL="$url" \
  bash "$ctl/scripts/rsi/nightly.sh" --stale spawn >"$scratch/out.locked" 2>&1
exec 7>&-
check "--stale beside a run in progress does nothing" grep -q 'another nightly of spawn runs' "$scratch/out.locked"
check "--stale runs no gate and writes no fact" bash -c '[ ! -s "$1" ] && [ "$(jq .at "$2")" -lt "$(($(date +%s) * 1000 - 90000000))" ]' _ "$scratch/calls" "$fact"
nightly
check "a run clears it" [ "$(stale_keys)" = "" ]

echo "== no fact at all, though the timer fired"
mv "$fact" "$fact.kept"
touch "$scratch/fired"
HOME="$scratch/home" PATH="$scratch/bin:$PATH" SPAWN_LIVE="$live" SPAWN_WORKSPACE_ROOT="$scratch/ws" \
  NIGHTLY_TEST_SCRATCH="$scratch" SPAWN_DATABASE_URL="$url" \
  bash "$ctl/scripts/rsi/nightly.sh" --stale spawn >"$scratch/out" 2>&1
check "stale, never judged ($(tail -n 1 "$scratch/out"))" [ "$(stale_keys)" = "nightly_stale:spawn:never" ]
rm -f "$scratch/fired"
mv "$fact.kept" "$fact"
nightly
check "a run clears it" [ "$(stale_keys)" = "" ]

echo "== a run stopped by a signal (the unit's timeout)"
echo sleep >"$scratch/behave/023.sh"
: >"$scratch/calls"
HOME="$scratch/home" PATH="$scratch/bin:$PATH" SPAWN_LIVE="$live" SPAWN_WORKSPACE_ROOT="$scratch/ws" \
  SPAWN_GATE_ENV_FILE="$scratch/gate-env" NIGHTLY_TEST_SCRATCH="$scratch" SPAWN_DATABASE_URL="$url" \
  SPAWN_NIGHTLY_SAMPLE_SECS=1 setsid bash "$ctl/scripts/rsi/nightly.sh" spawn >"$scratch/out" 2>&1 &
stopped=$!
for _ in $(seq 1 100); do grep -q '^call' "$scratch/calls" && break; sleep 0.1; done
kill -TERM -- "-$stopped" 2>/dev/null
wait "$stopped" 2>/dev/null
check "the fact says it was stopped ($(field .reason))" \
  bash -c '[ "$(jq -r .state "$1")" = skipped ] && jq -r .reason "$1" | grep -q "stopped by a signal"' _ "$fact"
check "its work directory is gone" bash -c '! ls -d "$1"/.run-spawn.* 2>/dev/null | grep -q .' _ "$scratch/ws/nightly"
rm -f "$scratch/behave/023.sh"
nightly

echo "== a run beside a short holder of the lock (an hourly --stale) still judges"
flock "$scratch/ws/nightly/.spawn.lock" sleep 2 &
short=$!
sleep 0.3
before=$(field .at)
LOCKWAIT=30 nightly
wait "$short"
check "it waited for the lock and judged ($(tail -n 1 "$scratch/out"))" \
  bash -c '[ "$(jq .at "$1")" != "$2" ] && [ "$(jq -r .state "$1")" = green ]' _ "$fact" "$before"

echo "== another nightly of the source runs"
exec 7>>"$scratch/ws/nightly/.spawn.lock"
flock 7
before=$(field .at)
LOCKWAIT=1 nightly
check "this one does nothing ($(tail -n 1 "$scratch/out"))" grep -q 'another nightly of spawn runs' "$scratch/out"
check "the fact file is untouched" [ "$(field .at)" = "$before" ]
exec 7>&-

echo "== the journal and the fact file hold no secret"
nightly
pg_url_parse "$SPAWN_TEST_DATABASE_URL"
check "no password in the output or the fact file" \
  bash -c '! grep -rqF -- "$1" "$2" "$3"' _ "${pg_url_password:-no-password-in-the-url}" "$scratch/out" "$scratch/ws/nightly"
check "no URL either" bash -c '! grep -rq "postgres://" "$1" "$2"' _ "$scratch/out" "$scratch/ws/nightly"

echo "== gate.sh's words for a check's runs are the ones nightly.sh reads"
# A check main fails on every run is red only by these words (nightly.sh's
# judge): gate.sh's printf of a check's runs, rendered with every exit 1,
# must be the string nightly.sh compares, or a rewording turns reds into
# skipped runs.
said=$(grep -F '>"$job/runs"' "$here/scripts/rsi/gate.sh" | grep -F '$on_base2' |
  sed -n 's/^ *printf .%s\\n. "\(.*\)" >"$job\/runs"$/\1/p' |
  sed 's/\$run_status/1/; s/\$again/1/; s/\$on_base2/1/; s/\$on_base/1/')
read_words=$(grep -o '\[ "$runs" = "[^"]*" \]' "$here/scripts/rsi/nightly.sh" | sed 's/^\[ "$runs" = "//; s/" \]$//')
check "gate.sh says '$said', nightly.sh reads '$read_words'" test -n "$said" -a "$said" = "$read_words"

echo "test-nightly: $oks ok, $fails FAIL"
[ "$fails" -eq 0 ]
