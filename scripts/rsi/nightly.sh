#!/usr/bin/env bash
# The nightly (gate v2 step 5a; sync/design-nightly-min.md): the scenarios
# that left the gate, run once a night on a source's live main.
#
#   scripts/rsi/nightly.sh <source> [<scenarios-dir>]
#
# <source> names the missions repository ($SPAWN_LIVE/missions/<source>.git,
# its `main`) and the fact file; <scenarios-dir> is the gate's scenario
# directory (default $SPAWN_LIVE/controllers/<source>/rsi/scenarios, taken
# physically once, as the integrator pins it). The scenarios it runs are
# the files in <scenarios-dir>/nightly/*.sh: moved there from the gate's
# directory, not rewritten (the gate runs only <scenarios-dir>/*.sh, so a
# file leaves the gate by the move). Run by spawn-nightly@<source>.timer
# (deploy/systemd), or by hand after a fix.
#
# 1. One nightly per source at a time (flock on <facts>/.<source>.lock; a
#    second exits 0 and says so).
# 2. A quiet host, made rather than waited for (the fleet's gates run
#    around the clock): it takes every slot of the budget all gates take
#    (scripts/rsi/gate.sh, "Host scenario slots"; $HOME/.cache/spawn/
#    gate-slots, 1..N) at once, each as the scenario run holding it ends,
#    up to SPAWN_NIGHTLY_WAIT_SECS (default 7200); past that it writes
#    `skipped` and says so. A gate that wants a slot meanwhile waits, so
#    the host drains. Once all are held it lets slot 1 go to its own gate
#    and keeps 2..N for its whole run: no other gate's scenario runs beside
#    it (one waits, up to its SPAWN_GATE_SLOT_WAIT_SECS, and then has no
#    room: judged again after the backoff). Without a budget (no flock, no
#    host_jobs) it waits instead for no gate to run (no active
#    spawn-gate-* unit, no scripts/rsi/gate.sh process), checked each
#    SPAWN_NIGHTLY_POLL_SECS (60), up to the same. Builds and test listings
#    take no slot: a gate may still build beside it, so the gates seen
#    running beside it are counted (`gates_beside` in the fact file,
#    sampled each SPAWN_NIGHTLY_SAMPLE_SECS, 15) and a flaky or red result
#    says how quiet the host was. A run stopped before its end (the unit's
#    TimeoutStartSec, a stop) writes `skipped`, saying so.
# 3. Each scenario is judged by the gate itself, main against itself
#    (gate.sh <repo> <main> <main> <dir>), with the integrator's gate
#    environment (SPAWN_GATE_ENV_FILE, as deploy.sh's base-cache warm-up
#    reads it) and SPAWN_GATE_JOBS=1. <dir> is a copy of the controller's
#    scenario directory without its *.sh, holding the one scenario as
#    `check-nightly-<name>-run.sh`: a check (its name ends in a letter, so
#    the base phase does not take it for an objective and run it there
#    too), so a failure is run again on the candidate and then on the base, and a pass on the second run is a
#    flake (an objective that fails on the base would be read as unsolved
#    and pass). The gate's build cache builds main once; the tests listing
#    is skipped, as a commit against itself drops no test.
#    Per scenario: accepted = pass, accepted with `flakes` = flaky (failed
#    once, passed again); failed (fail, fail): rejected (failed twice, then
#    passed on the base), a host fault `on` `main` (failed on the base twice
#    with the same failing tests), or one `on` `candidate and base` whose
#    runs all asserted ("1, then 1 on the candidate; 1, then 1 on the
#    base": the gate cannot tell that from a harness failing both trees,
#    but both trees are main); anything else (no room, a harness error, a
#    host fault with another exit among its runs, an abort) = skipped, with
#    the gate's reason.
# 4. The fact file, <facts>/<source>.json (<facts> = $SPAWN_WORKSPACE_ROOT/
#    nightly), written whole: {"v":1,"main":<sha>,"at":<ms>,"state":
#    "green|red|flaky|skipped","failed":[<names>],"last_green":<sha or
#    null>}, and beside those `red_since` (the main of the first red run of
#    this red, null otherwise), `flaky`, `skipped` (names), `gates_beside`,
#    `reason` (for skipped), `skipped_runs` (skipped runs in a row, this
#    one included) and `judged_at` (the last run that judged, not skipped). State: red if any failed, else skipped if
#    any was skipped, else flaky if any was, else green. last_green moves to
#    main on green or flaky, which end a red (red_since null). A skipped run
#    during a red keeps it: red_since and the red's failed list stay, as
#    nothing ran that showed main fixed (5b holds task landings while
#    red_since is set), and the next red is the same alert.
#    The gate's log of each scenario's last run is kept beside it, at
#    <facts>/<source>.<scenario>.log.
# 5. Red raises ONE owner alert (spawn_alert_record, the alerts' own path:
#    `spawn alerts`, /api/alerts, the home page): kind `nightly_red`,
#    subject <source>, key nightly_red:<source>:<red_since>, its evidence
#    the failed scenarios, main, last_green and the landings in
#    last_green..main (at most 30). The same red the next night fires
#    nothing (same key). The first green or flaky run clears it.
#    The database is the owner's (SPAWN_DATABASE_URL, from the unit's
#    EnvironmentFile), reached through PG* variables (scripts/pg-url.sh),
#    never an argument; nothing here prints it.
#
# 6. A nightly that covers nothing raises ONE owner alert too,
#    `nightly_stale` (check_stale below: two skipped runs in a row, or no
#    fact younger than 26 h), cleared by the first run that judges.
#    `nightly.sh --stale <source>` makes that check alone (no run), hourly
#    from spawn-nightly-stale@<source>.timer, so a nightly that never runs
#    is told as well.
#
# Exit: 0 when the fact file was written (whatever the state) or another
# run holds the source, 2 when it could not be (no main, no scenario
# directory, a fact file that cannot be written).
set -uo pipefail

say() { printf 'nightly: %s\n' "$*"; }
die() {
  printf 'nightly: %s\n' "$*" >&2
  exit 2
}

stale_only=""
if [ "${1-}" = --stale ]; then
  stale_only=1
  shift
fi
[ $# -ge 1 ] && [ $# -le 2 ] || die "usage: nightly.sh [--stale] <source> [<scenarios-dir>]"
source_name=$1
case $source_name in '' | *[!A-Za-z0-9._-]*) die "a source is a name ([A-Za-z0-9._-]), not '$source_name'" ;; esac
live=${SPAWN_LIVE:-$HOME/.local/share/spawn-rsi}
scenarios=$(readlink -f "${2:-$live/controllers/$source_name/rsi/scenarios}") ||
  die "no scenario directory ${2:-$live/controllers/$source_name/rsi/scenarios}"
[ -d "$scenarios" ] || die "no scenario directory $scenarios"
controller=$(cd "$scenarios/../.." && pwd)
repo=${SPAWN_NIGHTLY_REPO:-$live/missions/$source_name.git}
root=${SPAWN_WORKSPACE_ROOT:?nightly needs SPAWN_WORKSPACE_ROOT (spawn.env)}
facts=$root/nightly
fact=$facts/$source_name.json
gate_env_file=${SPAWN_GATE_ENV_FILE:-$HOME/.config/spawn/gate-env}
wait_secs=${SPAWN_NIGHTLY_WAIT_SECS:-7200}
poll_secs=${SPAWN_NIGHTLY_POLL_SECS:-60}
sample_secs=${SPAWN_NIGHTLY_SAMPLE_SECS:-15}
stale_secs=${SPAWN_NIGHTLY_STALE_SECS:-93600}
slot_dir=$HOME/.cache/spawn/gate-slots

mkdir -p "$facts" || die "cannot make $facts"
if [ -z "$stale_only" ]; then
  exec 9>>"$facts/.$source_name.lock"
  if ! flock -n 9; then
    say "another nightly of $source_name runs; this one does nothing"
    exit 0
  fi
fi

now_ms() { echo $(($(date +%s%N) / 1000000)); }

main=""
if [ -z "$stale_only" ]; then
  main=$(git --git-dir="$repo" rev-parse --verify --quiet refs/heads/main) || die "no main in $repo"
fi
previous='{}'
if [ -s "$fact" ]; then
  previous=$(jq -c 'if type == "object" then . else {} end' "$fact" 2>/dev/null) || previous='{}'
fi
last_green=$(jq -r '.last_green // empty' <<<"$previous")
# The red this run may continue: the previous run's, red or skipped while
# red (a skipped run shows nothing fixed), with what failed then.
red_since=$(jq -r 'if .state == "red" then (.red_since // .main // empty) else (.red_since // empty) end' <<<"$previous")
red_failed=""
[ -z "$red_since" ] || red_failed=$(jq -r '(.failed // [])[]' <<<"$previous")
# Skipped runs in a row, and when a run last judged anything (not skipped).
skipped_runs=$(jq -r '.skipped_runs // 0' <<<"$previous")
judged_at=$(jq -r '.judged_at // empty' <<<"$previous")
fact_written=""

# write_fact <state> <failed> <flaky> <skipped> <gates_beside> <reason>: the
# fact file, whole (the lists newline-separated names).
write_fact() {
  local state=$1 at
  at=$(now_ms)
  local failed=$2
  case $state in
  green | flaky)
    last_green=$main
    red_since=""
    ;;
  red) red_since=${red_since:-$main} ;;
  skipped)
    # A skipped run during a red keeps it, and what failed: nothing ran
    # that showed main fixed (5b holds landings while red_since is set).
    [ -z "$red_since" ] || failed=$red_failed
    ;;
  esac
  if [ "$state" = skipped ]; then
    skipped_runs=$((skipped_runs + 1))
  else
    skipped_runs=0
    judged_at=$at
  fi
  jq -n --arg main "$main" --argjson at "$at" --arg state "$state" \
    --arg failed "$failed" --arg flaky "$3" --arg skipped "$4" --argjson beside "$5" \
    --arg reason "$6" --arg last_green "$last_green" --arg red_since "$red_since" \
    --argjson skipped_runs "$skipped_runs" --arg judged_at "$judged_at" '
    def names: split("\n") | map(select(. != ""));
    {v: 1, main: $main, at: $at, state: $state, failed: ($failed | names),
     last_green: (if $last_green == "" then null else $last_green end),
     red_since: (if $red_since == "" then null else $red_since end),
     flaky: ($flaky | names), skipped: ($skipped | names), gates_beside: $beside,
     skipped_runs: $skipped_runs,
     judged_at: (if $judged_at == "" then null else ($judged_at | tonumber) end)}
    + (if $reason == "" then {} else {reason: $reason} end)' >"$fact.tmp" &&
    mv -f "$fact.tmp" "$fact" || die "cannot write $fact"
  fact_written=1
  say "$source_name at $main: $state${red_since:+ (red since $red_since)}${2:+ (failed: $(tr '\n' ' ' <<<"$2" | sed 's/ $//'))}${3:+ (flaky: $(tr '\n' ' ' <<<"$3" | sed 's/ $//'))}${4:+ (skipped: $(tr '\n' ' ' <<<"$4" | sed 's/ $//'))}${6:+: $6}; gates beside it: $5; $fact"
}

# The owner database, for the alert: PG* from SPAWN_DATABASE_URL, the URL
# itself then unset. 1 when there is none (said; the fact file stands).
db_ready=""
db_open() {
  [ -z "$db_ready" ] || return 0
  if [ -z "${SPAWN_DATABASE_URL-}" ] || [ ! -f "$controller/scripts/pg-url.sh" ]; then
    say "no SPAWN_DATABASE_URL (or no $controller/scripts/pg-url.sh): no alert raised or cleared"
    return 1
  fi
  # shellcheck source=scripts/pg-url.sh
  . "$controller/scripts/pg-url.sh"
  if ! pg_url_export "$SPAWN_DATABASE_URL"; then
    say "SPAWN_DATABASE_URL is not one postgres URL: no alert raised or cleared"
    return 1
  fi
  PGDATABASE=$pg_url_database
  export PGDATABASE
  unset SPAWN_DATABASE_URL
  db_ready=1
}

# alert_record <json>: one alert.fired or alert.cleared, through the store's
# owner-only spawn_alert_record. The record is no secret; the URL is not in it.
alert_record() {
  psql -X -At -q -v ON_ERROR_STOP=1 -v r="$1" <<'SQL'
select spawn_alert_record(:'r'::jsonb);
SQL
}

# open_alerts [<kind>]: the keys of this source's open alerts of the kind
# (nightly_red by default).
open_alerts() {
  psql -X -At -q -v ON_ERROR_STOP=1 -v s="$source_name" -v k="${1:-nightly_red}" <<'SQL'
select key from spawn_alerts where kind = :'k' and subject = :'s' and state = 'open' order by key;
SQL
}

raise_red() { # raise_red <failed names>
  db_open || return 0
  local key open landings evidence text
  key="nightly_red:$source_name:$red_since"
  if ! open=$(open_alerts); then
    say "the alerts could not be read: no alert raised"
    return 0
  fi
  if grep -qxF -- "$key" <<<"$open"; then
    say "alert $key is open already: nothing raised"
    return 0
  fi
  if [ -n "$last_green" ] && git --git-dir="$repo" cat-file -e "$last_green^{commit}" 2>/dev/null; then
    landings=$(git --git-dir="$repo" log --first-parent --format='%h %s' -n 30 "$last_green..$main" 2>/dev/null)
  else
    landings=""
  fi
  evidence=$(jq -n --arg main "$main" --arg last_green "$last_green" --arg red_since "$red_since" \
    --arg failed "$1" --arg landings "$landings" '
    def names: split("\n") | map(select(. != ""));
    {main: $main, red_since: $red_since,
     last_green: (if $last_green == "" then null else $last_green end),
     failed: ($failed | names), landings: ($landings | names)}')
  text="the nightly of $source_name fails $(tr '\n' ' ' <<<"$1" | sed 's/ $//') on main $(cut -c1-12 <<<"$main")"
  if [ -n "$last_green" ]; then
    text="$text; last green $(cut -c1-12 <<<"$last_green"), $(grep -c . <<<"$landings") landing(s) since"
  else
    text="$text; no green run yet"
  fi
  if alert_record "$(jq -cn --arg key "$key" --arg subject "$source_name" --argjson evidence "$evidence" \
    --arg text "$text" '{type: "alert.fired", key: $key, kind: "nightly_red", subject: $subject,
      evidence: $evidence, text: $text, notice: false}')" >/dev/null; then
    say "alert fired: $key"
  else
    say "alert $key could not be recorded (above)"
  fi
}

clear_red() { # clear_red [<kind>]: every open alert of the kind (nightly_red)
  db_open || return 0
  local open key kind=${1:-nightly_red}
  if ! open=$(open_alerts "$kind"); then
    say "the alerts could not be read: none cleared"
    return 0
  fi
  while IFS= read -r key; do
    [ -n "$key" ] || continue
    if alert_record "$(jq -cn --arg key "$key" --arg kind "$kind" --arg subject "$source_name" \
      --arg main "$(jq -r '.main // empty' "$fact" 2>/dev/null)" --arg state "$(jq -r '.state // empty' "$fact" 2>/dev/null)" \
      '{type: "alert.cleared", key: $key, kind: $kind, subject: $subject,
        evidence: {main: $main, state: $state}}')" >/dev/null; then
      say "alert cleared: $key"
    else
      say "alert $key could not be cleared (above)"
    fi
  done <<<"$open"
}

# check_stale: the nightly's coverage. Stale when no run judged anything
# lately: the newest fact older than SPAWN_NIGHTLY_STALE_SECS (93600, 26 h),
# SPAWN_NIGHTLY_STALE_RUNS skipped runs in a row (default 2), or no fact
# at all though spawn-nightly@<source>.timer has fired. Stale raises ONE
# owner alert, kind `nightly_stale`, key nightly_stale:<source>:<judged_at
# of the last run that judged, or never>; the first run that judges clears
# it. Run at the end of every nightly, and on its own (`--stale`) by
# spawn-nightly-stale@<source>.timer hourly, so a nightly that never runs is
# told too.
check_stale() {
  local why="" age runs triggered key open evidence
  if [ ! -s "$fact" ]; then
    triggered=$(systemctl --user show -p LastTriggerUSec --value "spawn-nightly@$source_name.timer" 2>/dev/null)
    case $triggered in '' | 0 | n/a) ;; *) why="no fact file, though spawn-nightly@$source_name.timer last fired at $triggered" ;; esac
  else
    age=$((($(now_ms) - $(jq -r '.at // 0' "$fact")) / 1000))
    runs=$(jq -r '.skipped_runs // 0' "$fact")
    if [ "$age" -gt "$stale_secs" ]; then
      why="the newest nightly of $source_name is ${age}s old (over ${stale_secs}s)"
    elif [ "$runs" -ge "${SPAWN_NIGHTLY_STALE_RUNS:-2}" ]; then
      why="$runs nightly runs of $source_name in a row were skipped, the last: $(jq -r '.reason // "no reason"' "$fact")"
    fi
  fi
  if [ -z "$why" ]; then
    # Cleared only by a fact of a run that judged (a missing file, or one
    # whose last run was skipped, is not coverage).
    if [ -s "$fact" ] && [ "$(jq -r '.skipped_runs // 0' "$fact")" = 0 ]; then
      clear_red nightly_stale
    fi
    return 0
  fi
  db_open || return 0
  key="nightly_stale:$source_name:$( (jq -r '.judged_at // "never"' "$fact" 2>/dev/null) || echo never)"
  [ "$key" != "nightly_stale:$source_name:" ] || key="nightly_stale:$source_name:never"
  if ! open=$(open_alerts nightly_stale); then
    say "the alerts could not be read: no alert raised"
    return 0
  fi
  if grep -qxF -- "$key" <<<"$open"; then
    say "stale: $why; alert $key is open already"
    return 0
  fi
  evidence=$(jq -cn --arg why "$why" --arg fact "$fact" \
    --argjson last "$( (jq -c '{state, main, at, judged_at, skipped_runs, reason}' "$fact" 2>/dev/null) || echo null)" \
    '{why: $why, fact: $fact, last: $last}')
  if alert_record "$(jq -cn --arg key "$key" --arg subject "$source_name" --argjson evidence "$evidence" \
    --arg text "the nightly of $source_name covers nothing: $why" '{type: "alert.fired", key: $key,
      kind: "nightly_stale", subject: $subject, evidence: $evidence, text: $text, notice: false}')" >/dev/null; then
    say "stale: $why; alert fired: $key"
  else
    say "alert $key could not be recorded (above)"
  fi
}

# gates_running [<own work dir>]: how many gates run on this host: active
# spawn-gate-* units, and gate.sh processes not of this nightly's own gate
# (whose command line names its work directory).
gates_running() {
  local units procs=0 pid cmd parent nested pids=" " own=" "
  units=$(systemctl --user list-units 'spawn-gate-*' --state=active --no-legend --plain 2>/dev/null | grep -c . || true)
  for pid in $(pgrep -f 'scripts/rsi/gate\.sh ' 2>/dev/null); do
    cmd=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null) || continue
    case $cmd in *scripts/rsi/gate.sh*) ;; *) continue ;; esac
    pids="$pids$pid "
    if [ -n "${1-}" ]; then case $cmd in *"$1"*) own="$own$pid " ;; esac; fi
  done
  # A gate's own subshells carry its command line, and a gate's scenario
  # may run gate.sh itself (check-test-gate): one gate is a gate.sh process
  # with no gate.sh among its ancestors, and this nightly's own is not
  # counted.
  for pid in $pids; do
    case $own in *" $pid "*) continue ;; esac
    parent=$pid nested=""
    while :; do
      parent=$(awk '/^PPid:/ { print $2 }' "/proc/$parent/status" 2>/dev/null)
      case $parent in '' | 0 | 1) break ;; esac
      case $pids in *" $parent "*) nested=1 && break ;; esac
    done
    [ -n "$nested" ] || procs=$((procs + 1))
  done
  # A unit's gate is also a gate.sh process: count the larger.
  [ "$units" -ge "$procs" ] && echo "$units" || echo "$procs"
}

if [ -n "$stale_only" ]; then
  check_stale
  exit 0
fi

# A run stopped before its fact (the unit's TimeoutStartSec, a stop, a
# kill of its gate) says so: skipped, and why.
started_s=$(date +%s)
on_signal() {
  if [ -z "$fact_written" ]; then
    local beside
    beside=$(cat "${work:-/nonexistent}/beside" 2>/dev/null)
    write_fact skipped "" "" "" "${beside:-0}" \
      "stopped by a signal after $(($(date +%s) - started_s))s (the unit's timeout, or a stop): not judged"
    check_stale
  fi
  exit 2
}
trap on_signal INT TERM HUP

list=()
for file in "$scenarios"/nightly/*.sh; do
  [ -f "$file" ] && list+=("$file")
done
if [ ${#list[@]} -eq 0 ]; then
  write_fact skipped "" "" "" 0 "no scenario in $scenarios/nightly"
  check_stale
  exit 0
fi

# The integrator's gate environment (deploy/gate-env.sh), as deploy.sh's
# warm-up takes it.
keep=()
own="" test_url="" home=$HOME
if [ -r "$gate_env_file" ] && [ -f "$controller/deploy/gate-env.sh" ]; then
  # shellcheck source=deploy/gate-env.sh
  . "$controller/deploy/gate-env.sh"
  gate_env_read "$gate_env_file"
  if [ -z "$own" ] && [ -n "$test_url" ]; then
    . "$controller/scripts/pg-url.sh"
    owner_server="" test_server=""
    pg_url_parse "${SPAWN_DATABASE_URL-}" && owner_server="$pg_url_host:$pg_url_port"
    pg_url_parse "$test_url" && test_server="$pg_url_host:$pg_url_port"
    if [ -n "$test_server" ] && [ "$test_server" != "$owner_server" ]; then
      keep+=("SPAWN_TEST_DATABASE_URL=$test_url")
    fi
  fi
else
  say "no gate environment at $gate_env_file: the gate runs with this unit's own"
fi
host_jobs=""
[ ! -f "$slot_dir/host_jobs" ] || host_jobs=$(tr -d ' \t\n' <"$slot_dir/host_jobs" 2>/dev/null)
if [ -z "$host_jobs" ]; then
  for assignment in "${keep[@]}"; do
    case $assignment in SPAWN_GATE_HOST_JOBS=*) host_jobs=${assignment#*=} ;; esac
  done
fi
# The host's budget: the file every gate reads, else the integrator's
# SPAWN_GATE_HOST_JOBS (above). drain_slots <host_jobs>: every slot taken,
# 1..N, waiting (until the deadline) for the scenario runs that hold them to
# end, so the host drains towards quiet rather than waiting for it: a gate
# that wants one meanwhile waits, the integrator's next gates too. Slot 1
# is let go once all are held, for the nightly's own gate. 1 when the
# deadline passed first (all let go again).
held=()
drain_slots() {
  local n=$1 i fd left
  for ((i = 1; i <= n; i++)); do
    (umask 077 && : >>"$slot_dir/slot.$i") 2>/dev/null || continue
    exec {fd}>>"$slot_dir/slot.$i"
    left=$((deadline - $(date +%s)))
    [ "$left" -gt 0 ] || left=0
    if flock -w "$left" "$fd"; then
      held+=("$fd")
    else
      exec {fd}>&-
      for fd in "${held[@]}"; do eval "exec $fd>&-"; done
      held=()
      return 1
    fi
  done
  eval "exec ${held[0]}>&-"
  held=("${held[@]:1}")
  say "holding ${#held[@]} of $n host scenario slots, drained in $(($(date +%s) - started_s))s (slot 1 is its own gate's)"
}

# 2. A quiet host. With a slot budget, the slots drained (above), up to
# SPAWN_NIGHTLY_WAIT_SECS; without one (no flock, no budget), no gate
# running, checked each SPAWN_NIGHTLY_POLL_SECS, up to the same.
deadline=$(($(date +%s) + wait_secs))
if [[ ${host_jobs:-} =~ ^[1-9][0-9]*$ ]] && command -v flock >/dev/null 2>&1 &&
  mkdir -p "$slot_dir" && chmod 700 "$slot_dir" 2>/dev/null; then
  if ! drain_slots "$host_jobs"; then
    busy=$(gates_running)
    write_fact skipped "" "" "" "$busy" "the host's scenario slots were not all free within ${wait_secs}s ($busy gate(s) running): not run"
    check_stale
    exit 0
  fi
else
  while :; do
    busy=$(gates_running)
    [ "$busy" -gt 0 ] || break
    if [ "$(date +%s)" -ge "$deadline" ]; then
      write_fact skipped "" "" "" "$busy" "$busy gate(s) still running after ${wait_secs}s, and no slot budget to drain: not run"
      check_stale
      exit 0
    fi
    sleep "$poll_secs"
  done
fi

work=$(mktemp -d "$facts/.run-$source_name.XXXXXX") || die "cannot make a work directory under $facts"
sampler=""
cleanup() {
  [ -z "$sampler" ] || kill "$sampler" 2>/dev/null
  rm -rf "$work"
}
trap cleanup EXIT


# The gates beside it, sampled for the whole run (the largest count seen).
: >"$work/beside"
(
  exec 9>&-
  for fd in "${held[@]}"; do eval "exec $fd>&-"; done
  most=0
  while :; do
    n=$(gates_running "$work")
    [ "$n" -le "$most" ] || {
      most=$n
      echo "$most" >"$work/beside"
    }
    sleep "$sample_secs"
  done
) &
sampler=$!

# judge <scenario file>: the gate on main with that scenario alone. Prints
# pass, flaky, failed or skipped, then a reason line.
judge() {
  local file=$1 name dir ctl checked status verdict on runs
  name=$(basename "$file")
  # $dir/ctl the controller the gate runs (its scripts, scenarios), and the
  # verdict apart, in a directory of its own: the gate hides the verdict's
  # directory from its sandbox.
  dir=$work/$name
  ctl=$dir/ctl
  mkdir -p "$ctl/rsi" "$dir/verdict"
  cp -a "$controller/scripts" "$ctl/scripts"
  [ ! -d "$controller/deploy" ] || cp -a "$controller/deploy" "$ctl/deploy"
  mkdir "$ctl/rsi/scenarios"
  local entry
  for entry in "$scenarios"/* "$scenarios"/.[!.]*; do
    [ -e "$entry" ] || continue
    case $(basename "$entry") in *.sh | nightly | serial | quarantined) continue ;; esac
    cp -a "$entry" "$ctl/rsi/scenarios/"
  done
  # A check's name that ends in a letter: a name ending in a digit
  # (`check-nightly-023.sh`) is an objective's to the gate's base phase too,
  # which would run it on the base as well, a second whole run.
  checked="check-nightly-${name%.sh}-run.sh"
  cp -a "$file" "$ctl/rsi/scenarios/$checked"
  printf '%s\n' "$checked" >"$ctl/rsi/scenarios/serial"
  (
    exec 9>&- </dev/null >"$dir/gate.log" 2>&1
    for fd in "${held[@]}"; do eval "exec $fd>&-"; done
    # The integrator's gate environment and nothing else of this unit's
    # (its database URL above all); without one, this unit's PATH, HOME,
    # LANG, TMPDIR and toolchain homes.
    for var in $(compgen -e); do
      if [ ${#keep[@]} -gt 0 ]; then
        unset "$var" 2>/dev/null || true
      else
        case $var in PATH | HOME | LANG | TMPDIR | CARGO_HOME | RUSTUP_HOME) ;; *) unset "$var" 2>/dev/null || true ;; esac
      fi
    done
    for assignment in "${keep[@]}"; do export "$assignment"; done
    export SPAWN_GATE_JOBS=1 SPAWN_GATE_VERDICT="$dir/verdict/verdict.json"
    exec sh "$ctl/scripts/rsi/gate.sh" "$repo" "$main" "$main" "$ctl/rsi/scenarios"
  )
  status=$?
  verdict=$(cat "$dir/verdict/verdict.json" 2>/dev/null)
  if [ "$status" = 0 ] && [ "$(jq -r '.verdict // empty' <<<"$verdict" 2>/dev/null)" = accepted ]; then
    if [ "$(jq -r '(.flakes // []) | length' <<<"$verdict")" -gt 0 ]; then
      printf 'flaky\nfailed once and passed again\n'
    else
      printf 'pass\n\n'
    fi
    return
  fi
  if [ "$status" = 0 ] && [ "$(jq -r '.verdict // empty' <<<"$verdict" 2>/dev/null)" = rejected ]; then
    printf 'failed\n%s\n' "$(jq -r '.reason // "rejected"' <<<"$verdict")"
    return
  fi
  on=$(jq -r '.on // empty' <<<"$verdict" 2>/dev/null)
  if [ "$status" = 6 ] && [ "$on" = main ]; then
    printf 'failed\n%s\n' "$(jq -r '.reason // "main is red on it"' <<<"$verdict")"
    return
  fi
  # Main judged against itself: a check that asserts (exit 1) on every run,
  # twice on the candidate and twice on the base, with no `FAILED <test>:`
  # lines to compare, is a host fault `on` `candidate and base` to the gate
  # (it cannot tell a red main from a harness that fails both trees). Here
  # both trees are main and every run asserted: main fails it. Any other
  # exit among the runs (2, a harness error) stays the host's.
  runs=$(jq -r '.runs // empty' <<<"$verdict" 2>/dev/null)
  if [ "$status" = 6 ] && [ "$on" = "candidate and base" ] &&
    [ "$runs" = "1, then 1 on the candidate; 1, then 1 on the base" ]; then
    printf 'failed\nit asserted on every run (%s)\n' "$runs"
    return
  fi
  printf 'skipped\nthe gate exited %s%s: %s\n' "$status" "${on:+ (on $on)}" \
    "$( (jq -r '.reason // empty' <<<"$verdict" 2>/dev/null) || true)$(grep -v '^$' "$dir/gate.log" | tail -n 1)"
}

failed="" flaky="" skipped="" reasons=""
for file in "${list[@]}"; do
  name=$(basename "$file")
  started=$(date +%s)
  result=$(judge "$file")
  outcome=$(head -n 1 <<<"$result")
  why=$(sed -n 2p <<<"$result")
  say "$name: $outcome in $(($(date +%s) - started))s${why:+ ($why)}"
  # The gate's log of the scenario's last run, whatever it found.
  cp "$work/$name/gate.log" "$facts/$source_name.$name.log" 2>/dev/null ||
    rm -f "$facts/$source_name.$name.log"
  case $outcome in
  failed) failed="$failed$name"$'\n' ;;
  flaky) flaky="$flaky$name"$'\n' ;;
  skipped)
    skipped="$skipped$name"$'\n'
    reasons="${reasons:+$reasons; }$name: $why"
    ;;
  esac
done
kill "$sampler" 2>/dev/null
wait "$sampler" 2>/dev/null
sampler=""
beside=$(cat "$work/beside" 2>/dev/null)
beside=${beside:-0}

if [ -n "$failed" ]; then
  state=red
elif [ -n "$skipped" ]; then
  state=skipped
elif [ -n "$flaky" ]; then
  state=flaky
else
  state=green
fi
write_fact "$state" "$failed" "$flaky" "$skipped" "$beside" "$reasons"
case $state in
red) raise_red "$failed" ;;
green | flaky) clear_red ;;
esac
check_stale
exit 0
