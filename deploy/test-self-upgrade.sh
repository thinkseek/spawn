#!/usr/bin/env bash
# Offline zero-side-effect tests for deploy/self-upgrade.sh,
# deploy/upgrade-watchdog.sh, and deploy/systemd/spawn-upgrade-watchdog.*.
#
# Covers:
#   (a) Fail-closed SHA256SUMS + layout + permission refusal before symlink touch
#   (b) Control-plane upgrade with web.tar.gz permission normalization,
#       pre-backup hook, init-db, sequential gateway -> serve restart, secret
#       redaction, and manual --rollback
#   (c) Control-plane canary probe failure (HTTP 503 & NRestarts>0) auto-rollback
#   (d) Worker-node Epoch Drain (`spawn integrate --drain`), detached `spawn-hc`
#       process survival across upgrade & rollback
#   (e) Independent external `upgrade-watchdog.sh` health probe failure
#       threshold, rollback without calling `bin/spawn`, recovery reset,
#       window expiry, and anti-loop debounce
#   (f) R1: Honest `init-db` contract + `--db-blind` hard refusal on
#       spawn-loop.service, pass on spawn-node.service, and ExecStartPre init-db
#       auto-rollback on fleet worker
#   (g) R2 + #8: `web.tar.gz` symlinks (abs/rel/root), hardlinks, setuid/setgid
#       modes, and pre-existing unlisted `web/dist` coexistence refusals
#   (h) R3: First-upgrade snapshot (`pre-upgrade-<ts>`) from legacy non-symlink
#       live bin/* + web/ layout with auto-rollback and manual --rollback
#   (i) #1: Watchdog coexistence with `deploy.sh install_build` (`superseded`)
#   (j) #2: Worker crash-loop detection inside `--worker-settle-secs` window
#   (k) #3 & #6: Active-only unit filtering + fail-closed on 0 active units + honest 0-unit status
#   (l) #4: Full secret redaction across state, audit, stdout, stderr, and captured hook/init-db output
#   (m) #5: Flock FD closed before running hooks (background child never wedges lock)
#   (n) R4: Owner clean-stop vs crash distinction in watchdog and manual --rollback

set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
upgrade_sh=$repo/deploy/self-upgrade.sh
watchdog_sh=$repo/deploy/upgrade-watchdog.sh
unit_service=$repo/deploy/systemd/spawn-upgrade-watchdog.service
unit_timer=$repo/deploy/systemd/spawn-upgrade-watchdog.timer

failures=0
passed=0
pass() {
  printf 'ok   %s\n' "$*"
  passed=$((passed + 1))
}
fail() {
  printf 'FAIL %s\n' "$*"
  failures=$((failures + 1))
}
check() {
  local what=$1
  shift
  if "$@"; then
    pass "$what"
  else
    fail "$what"
  fi
}
jq_ok() {
  jq -e "$1" "$2" >/dev/null
}

real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/spawn-self-upgrade-test.XXXXXX")
hc_pid=""
bg_hook_pid=""
cleanup() {
  local pid
  for pid in "${hc_pid-}" "${bg_hook_pid-}"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  chmod -R u+rwX "$scratch" 2>/dev/null || true
  rm -rf -- "$scratch"
}
trap cleanup EXIT

fake_home=$scratch/home
mkdir -p "$fake_home/.local/bin"
export HOME="$fake_home"

# Guard: HOME must stay strictly inside scratch, never real_home.
[ "$(readlink -f -- "$HOME")" != "$(readlink -f -- "$real_home")" ] || {
  echo "test-self-upgrade: HOME leaked to real home" >&2
  exit 2
}

mock_bin=$scratch/mocks
mkdir -p "$mock_bin"

cat >"$mock_bin/systemctl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
state_dir=${MOCK_SYSCTL_STATE:?}
log_file=${MOCK_EVENTS_LOG:?}
job_mode=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --user) shift ;;
    --job-mode=*) job_mode=${1#--job-mode=}; shift ;;
    --job-mode) job_mode=${2-}; shift 2 ;;
    *) break ;;
  esac
done
cmd=${1-}
shift || true

unit_is_loaded_and_active() {
  local unit=$1
  if [ -f "$state_dir/loadstate.$unit" ] && [ "$(cat "$state_dir/loadstate.$unit")" = "not-found" ]; then
    return 1
  fi
  if [ -f "$state_dir/activestate.$unit" ] && [ "$(cat "$state_dir/activestate.$unit")" != "active" ]; then
    return 1
  fi
  if [ -f "$state_dir/active.$unit" ] && [ "$(cat "$state_dir/active.$unit")" != "active" ]; then
    return 1
  fi
  return 0
}

activate_unit() {
  local action=$1 unit=$2
  if [ -f "$state_dir/fail_restart.$unit" ]; then
    return 1
  fi
  if [ -f "$state_dir/execstartpre.$unit" ]; then
    local pre_cmd
    pre_cmd=$(cat "$state_dir/execstartpre.$unit")
    pre_cmd=${pre_cmd//%h/$HOME}
    if [ -n "$pre_cmd" ]; then
      if ! bash -c "$pre_cmd"; then
        echo "failed" >"$state_dir/activestate.$unit"
        echo "failed" >"$state_dir/substate.$unit"
        echo "exit-code" >"$state_dir/result.$unit"
        echo 1 >"$state_dir/nrestarts.$unit"
        return 1
      fi
    fi
  fi
  if [ -n "${MOCK_RSI_DIR-}" ] && [ -L "$MOCK_RSI_DIR/current" ]; then
    readlink -- "$MOCK_RSI_DIR/current" >"$state_dir/unitver.$unit"
  fi
  echo "active" >"$state_dir/active.$unit"
  echo "active" >"$state_dir/activestate.$unit"
  echo "running" >"$state_dir/substate.$unit"
  echo "success" >"$state_dir/result.$unit"
  echo 0 >"$state_dir/pollcount.$unit"

  local cur_tag="" bad_tag=""
  if [ -f "$state_dir/unitver.$unit" ]; then
    cur_tag=$(basename "$(cat "$state_dir/unitver.$unit")")
  fi
  if [ "$action" = "restart" ] && [ -f "$state_dir/bump_nrestarts_on.$unit" ]; then
    bad_tag=$(cat "$state_dir/bump_nrestarts_on.$unit")
    if [ "$cur_tag" = "$bad_tag" ]; then
      echo 2 >"$state_dir/nrestarts.$unit"
    else
      echo 0 >"$state_dir/nrestarts.$unit"
    fi
  else
    echo 0 >"$state_dir/nrestarts.$unit"
  fi
  if [ "$action" = "restart" ] && [ -f "$state_dir/crash_after_polls_on.$unit" ]; then
    bad_tag=$(cat "$state_dir/crash_after_polls_on.$unit")
    if [ "$cur_tag" = "$bad_tag" ]; then
      cat "$state_dir/crash_after_polls_limit.$unit" >"$state_dir/crash_after_polls.$unit"
    else
      rm -f "$state_dir/crash_after_polls.$unit"
    fi
  fi
  return 0
}

maybe_trigger_crash_poll() {
  local unit=$1
  if [ -f "$state_dir/crash_after_polls.$unit" ]; then
    local limit cnt=0
    limit=$(cat "$state_dir/crash_after_polls.$unit")
    if [ -f "$state_dir/pollcount.$unit" ]; then
      cnt=$(cat "$state_dir/pollcount.$unit")
    fi
    cnt=$((cnt + 1))
    echo "$cnt" >"$state_dir/pollcount.$unit"
    if [ "$cnt" -ge "$limit" ]; then
      echo "activating" >"$state_dir/activestate.$unit"
      echo "auto-restart" >"$state_dir/substate.$unit"
      echo "exit-code" >"$state_dir/result.$unit"
      echo "1" >"$state_dir/nrestarts.$unit"
    fi
  fi
}

case "$cmd" in
  show)
    prop="" unit=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        -p) prop=$2; shift 2 ;;
        --value) shift ;;
        *) unit=$1; shift ;;
      esac
    done
    case "$prop" in
      ActiveState)
        maybe_trigger_crash_poll "$unit"
        if [ -f "$state_dir/activestate.$unit" ]; then
          cat "$state_dir/activestate.$unit"
        elif unit_is_loaded_and_active "$unit"; then
          echo "active"
        else
          echo "inactive"
        fi
        ;;
      SubState)
        if [ -f "$state_dir/substate.$unit" ]; then
          cat "$state_dir/substate.$unit"
        elif unit_is_loaded_and_active "$unit"; then
          echo "running"
        else
          echo "dead"
        fi
        ;;
      Result)
        if [ -f "$state_dir/result.$unit" ]; then
          cat "$state_dir/result.$unit"
        else
          echo "success"
        fi
        ;;
      NRestarts)
        if [ -f "$state_dir/nrestarts.$unit" ]; then
          cat "$state_dir/nrestarts.$unit"
        else
          echo 0
        fi
        ;;
      LoadState)
        if [ -f "$state_dir/loadstate.$unit" ]; then
          cat "$state_dir/loadstate.$unit"
        else
          echo loaded
        fi
        ;;
      ExecStartPre)
        if [ -f "$state_dir/execstartpre.$unit" ]; then
          cat "$state_dir/execstartpre.$unit"
        else
          echo ""
        fi
        ;;
      FragmentPath)
        if [ -f "$state_dir/fragmentpath.$unit" ]; then
          cat "$state_dir/fragmentpath.$unit"
        else
          echo ""
        fi
        ;;
      DropInPaths)
        if [ -f "$state_dir/dropinpaths.$unit" ]; then
          cat "$state_dir/dropinpaths.$unit"
        else
          echo ""
        fi
        ;;
      *) echo "" ;;
    esac
    ;;
  is-active)
    [ "${1-}" = "--quiet" ] && shift || true
    unit=${1-}
    unit_is_loaded_and_active "$unit"
    ;;
  stop)
    unit=${1-}
    printf 'systemctl:stop:%s\n' "$unit" >>"$log_file"
    echo "inactive" >"$state_dir/active.$unit"
    echo "inactive" >"$state_dir/activestate.$unit"
    echo "dead" >"$state_dir/substate.$unit"
    echo "success" >"$state_dir/result.$unit"
    echo 0 >"$state_dir/nrestarts.$unit"
    rm -f "$state_dir/crash_after_polls.$unit"
    ;;
  restart | start)
    unit=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --job-mode=*) job_mode=${1#--job-mode=}; shift ;;
        --job-mode) job_mode=${2-}; shift 2 ;;
        *) unit=$1; shift ;;
      esac
    done
    printf 'systemctl:%s:%s\n' "$cmd" "$unit" >>"$log_file"
    activate_unit "$cmd" "$unit" || exit 1
    # Simulate real systemd Wants=spawn-loop.service on spawn-integrate.service unless --job-mode=ignore-requirements is passed.
    if [ "$unit" = "spawn-integrate.service" ] && [ "$job_mode" != "ignore-requirements" ]; then
      if [ ! -f "$state_dir/loadstate.spawn-loop.service" ] || [ "$(cat "$state_dir/loadstate.spawn-loop.service")" != "not-found" ]; then
        if ! unit_is_loaded_and_active "spawn-loop.service"; then
          printf 'systemctl:wants-start:%s\n' "spawn-loop.service" >>"$log_file"
          activate_unit "start" "spawn-loop.service" || true
        fi
      fi
    fi
    ;;
  *)
    printf 'systemctl:%s:%s\n' "$cmd" "$*" >>"$log_file"
    ;;
esac
EOF
chmod 755 "$mock_bin/systemctl"

cat >"$mock_bin/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
state_dir=${MOCK_SYSCTL_STATE:?}
log_file=${MOCK_EVENTS_LOG:?}
url=""
for arg in "$@"; do
  url=$arg
done
printf 'curl:%s\n' "$url" >>"$log_file"
code=200
case "$url" in
  *":18096"*)
    if [ -f "$state_dir/gateway_code" ]; then
      code=$(cat "$state_dir/gateway_code")
    else
      code=404
    fi
    ;;
  *":8787"*)
    if [ -f "$state_dir/serve_code_by_unitver" ] && [ -f "$state_dir/unitver.spawn-serve.service" ]; then
      cur_tag=$(basename "$(cat "$state_dir/unitver.spawn-serve.service")")
      bad_tag=$(cat "$state_dir/serve_code_by_unitver")
      if [ "$cur_tag" = "$bad_tag" ]; then
        code=503
      else
        code=200
      fi
    elif [ -f "$state_dir/serve_code" ]; then
      code=$(cat "$state_dir/serve_code")
    else
      code=200
    fi
    ;;
esac
printf '%s' "$code"
if [ "$code" = "000" ]; then
  exit 7
fi
EOF
chmod 755 "$mock_bin/curl"

make_release() {
  local rsi=$1 tag=$2 role_kind=$3 web_mode=${4:-dir} fail_initdb=${5:-0}
  local rdir="$rsi/releases/$tag"
  mkdir -p "$rdir"
  cat >"$rdir/spawn" <<EOF
#!/usr/bin/env bash
set -euo pipefail
echo "spawn:$tag:\$*" >>"\${MOCK_EVENTS_LOG:?}"
if [ "\${SPAWN_STUB_FORBID_CALL:-0}" = "1" ]; then
  echo "FORBIDDEN_BIN_SPAWN_CALL" >>"\${MOCK_EVENTS_LOG:?}"
  exit 99
fi
if [ "\${1-}" = "init-db" ] && [ "$fail_initdb" = "1" ]; then
  echo "init-db failed intentionally for $tag" >&2
  exit 1
fi
if [ "\${1-}" = "integrate" ] && [ "\${2-}" = "--drain" ]; then
  cur=\$(readlink -- "$rsi/current" || true)
  echo "drain_saw_current=\$cur" >>"\${MOCK_EVENTS_LOG:?}"
fi
exit 0
EOF
  chmod 755 "$rdir/spawn"

  cat >"$rdir/spawn-hc" <<EOF
#!/usr/bin/env bash
echo "hc:$tag"
EOF
  chmod 755 "$rdir/spawn-hc"

  if [ "$role_kind" = "control-plane" ] || [ "$role_kind" = "both" ]; then
    cat >"$rdir/spawn-node-gateway" <<EOF
#!/usr/bin/env bash
echo "gateway:$tag"
EOF
    chmod 755 "$rdir/spawn-node-gateway"
    if [ "$web_mode" = "dir" ]; then
      mkdir -p "$rdir/web/dist/assets"
      printf '<html>%s</html>\n' "$tag" >"$rdir/web/dist/index.html"
      printf 'console.log("%s");\n' "$tag" >"$rdir/web/dist/assets/app.js"
    elif [ "$web_mode" = "tar-restrictive" ]; then
      local pack_dir="$scratch/pack-$tag"
      rm -rf -- "$pack_dir"
      mkdir -p "$pack_dir/dist/locked"
      printf '<html>%s</html>\n' "$tag" >"$pack_dir/dist/index.html"
      printf 'body{}\n' >"$pack_dir/dist/locked/style.css"
      # Intentionally pack with 0600 file and 0500 dir to test chmod normalization.
      chmod 0600 "$pack_dir/dist/locked/style.css"
      chmod 0500 "$pack_dir/dist/locked"
      tar -czf "$rdir/web.tar.gz" -C "$pack_dir" dist
      chmod 0755 "$pack_dir/dist/locked"
      rm -rf -- "$pack_dir"
    fi
  fi

  if [ "$role_kind" = "worker-node" ] || [ "$role_kind" = "both" ]; then
    cat >"$rdir/spawn-node" <<EOF
#!/usr/bin/env bash
echo "node:$tag"
EOF
    chmod 755 "$rdir/spawn-node"
  fi

  (
    cd "$rdir"
    find . -maxdepth 4 -type f ! -name 'SHA256SUMS' | sed 's#^\./##' | LC_ALL=C sort |
      xargs sha256sum >SHA256SUMS
  )
}

rehash_release() {
  local rdir=$1
  (
    cd "$rdir"
    find . -maxdepth 4 -type f ! -name 'SHA256SUMS' | sed 's#^\./##' | LC_ALL=C sort |
      xargs sha256sum >SHA256SUMS
  )
}

seed_initial_v1() {
  local rsi=$1 role_kind=$2
  mkdir -p "$rsi/releases" "$rsi/bin"
  make_release "$rsi" "v1" "$role_kind" "dir"
  ln -s "releases/v1" "$rsi/current"
  ln -s "releases/v1" "$rsi/previous"
  for b in spawn spawn-node-gateway spawn-hc spawn-node; do
    if [ -x "$rsi/releases/v1/$b" ]; then
      ln -s "../current/$b" "$rsi/bin/$b"
    fi
  done
  if [ -d "$rsi/releases/v1/web/dist" ]; then
    ln -s "current/web/dist" "$rsi/web"
  fi
}

export MOCK_SYSCTL_STATE="$scratch/sysctl-state"
export MOCK_EVENTS_LOG="$scratch/events.log"
mkdir -p "$MOCK_SYSCTL_STATE"

# -----------------------------------------------------------------------------
# Scenario (a): SHA256SUMS tampering & fail-closed pre-switch checks
# -----------------------------------------------------------------------------
rsi_a="$scratch/rsi-a"
seed_initial_v1 "$rsi_a" "control-plane"
export MOCK_RSI_DIR="$rsi_a"

# Tampered binary checksum
make_release "$rsi_a" "v2-tamper-bin" "control-plane" "dir"
printf '#!/bin/sh\necho EVIL\n' >"$rsi_a/releases/v2-tamper-bin/spawn"
chmod 755 "$rsi_a/releases/v2-tamper-bin/spawn"
: >"$MOCK_EVENTS_LOG"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_a" --role control-plane --version v2-tamper-bin \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/a1.out" 2>&1 || rc=$?
check "(a1) tampered spawn exits 1 (got $rc)" [ "$rc" -eq 1 ]
check "(a1) current symlink stayed releases/v1" [ "$(readlink "$rsi_a/current")" = "releases/v1" ]
check "(a1) zero systemctl calls on refusal" [ ! -s "$MOCK_EVENTS_LOG" ]
check "(a1) upgrade-state.json untouched/absent" [ ! -e "$rsi_a/upgrade-state.json" ]

# Tampered nested web/dist file checksum
make_release "$rsi_a" "v2-tamper-web" "control-plane" "dir"
printf '<html>PWNED</html>\n' >"$rsi_a/releases/v2-tamper-web/web/dist/index.html"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_a" --role control-plane --version v2-tamper-web \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/a2.out" 2>&1 || rc=$?
check "(a2) tampered web/dist exits 1 (got $rc)" [ "$rc" -eq 1 ]
check "(a2) current symlink stayed releases/v1" [ "$(readlink "$rsi_a/current")" = "releases/v1" ]

# Missing binary checksum coverage
make_release "$rsi_a" "v2-missing-gw-sum" "control-plane" "dir"
grep -v 'spawn-node-gateway' "$rsi_a/releases/v2-missing-gw-sum/SHA256SUMS" >"$scratch/tmp.sums"
mv "$scratch/tmp.sums" "$rsi_a/releases/v2-missing-gw-sum/SHA256SUMS"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_a" --role control-plane --version v2-missing-gw-sum \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/a3.out" 2>&1 || rc=$?
check "(a3) missing spawn-node-gateway SHA256SUMS entry exits 1 (got $rc)" [ "$rc" -eq 1 ]

# Path traversal inside SHA256SUMS
make_release "$rsi_a" "v2-traversal" "control-plane" "dir"
printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  ../escape\n' >>"$rsi_a/releases/v2-traversal/SHA256SUMS"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_a" --role control-plane --version v2-traversal \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/a4.out" 2>&1 || rc=$?
check "(a4) SHA256SUMS path traversal rejected exits 1 (got $rc)" [ "$rc" -eq 1 ]

# Non-executable spawn binary
make_release "$rsi_a" "v2-noexec" "control-plane" "dir"
chmod 0644 "$rsi_a/releases/v2-noexec/spawn"
rehash_release "$rsi_a/releases/v2-noexec"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_a" --role control-plane --version v2-noexec \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/a5.out" 2>&1 || rc=$?
check "(a5) non-executable spawn rejected exits 1 (got $rc)" [ "$rc" -eq 1 ]

# Valid --dry-run succeeds without touching symlinks or systemctl
make_release "$rsi_a" "v2-dry" "control-plane" "dir"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_a" --role control-plane --version v2-dry --dry-run \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/a6.out" 2>&1 || rc=$?
check "(a6) --dry-run exits 0 (got $rc)" [ "$rc" -eq 0 ]
check "(a6) --dry-run leaves current at releases/v1" [ "$(readlink "$rsi_a/current")" = "releases/v1" ]
check "(a6) --dry-run leaves systemctl untouched" [ ! -s "$MOCK_EVENTS_LOG" ]

# Unlisted executable in release root must be rejected and never linked into bin/
make_release "$rsi_a" "v2-unlisted-bin" "control-plane" "dir"
printf '#!/usr/bin/env bash\necho EXTRA\n' >"$rsi_a/releases/v2-unlisted-bin/extra-tool"
chmod 755 "$rsi_a/releases/v2-unlisted-bin/extra-tool"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_a" --role control-plane --version v2-unlisted-bin \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/a7.out" 2>&1 || rc=$?
check "(a7) unlisted executable extra-tool rejected exits 1 (got $rc)" [ "$rc" -eq 1 ]
check "(a7) unlisted executable NEVER linked into bin/extra-tool" \
  bash -c '[ ! -e "$1/bin/extra-tool" ] && [ ! -L "$1/bin/extra-tool" ] && [ "$(readlink "$1/current")" = "releases/v1" ]' _ "$rsi_a"

# Defense in depth: even if an unlisted executable exists in previous release at rollback time,
# activate_release_links links strictly SHA256SUMS-verified binaries.
rsi_a_rb="$scratch/rsi-a-rb"
seed_initial_v1 "$rsi_a_rb" "control-plane"
make_release "$rsi_a_rb" "v2" "control-plane" "dir"
export MOCK_RSI_DIR="$rsi_a_rb"
rm -f "$MOCK_SYSCTL_STATE"/*
SPAWN_UPGRADE_NOW_EPOCH=100 \
  bash "$upgrade_sh" --rsi-dir "$rsi_a_rb" --role control-plane --version v2 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null
printf '#!/usr/bin/env bash\necho SNEAKY\n' >"$rsi_a_rb/releases/v1/sneaky-tool"
chmod 755 "$rsi_a_rb/releases/v1/sneaky-tool"
echo "v2" >"$MOCK_SYSCTL_STATE/serve_code_by_unitver"
SPAWN_UPGRADE_NOW_EPOCH=110 \
  bash "$watchdog_sh" --rsi-dir "$rsi_a_rb" --failure-threshold 1 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null 2>&1
check "(a8) watchdog rollback to v1 NEVER links unlisted sneaky-tool into bin/" \
  bash -c '[ "$(readlink "$1/current")" = "releases/v1" ] && [ ! -e "$1/bin/sneaky-tool" ] && [ ! -L "$1/bin/sneaky-tool" ]' _ "$rsi_a_rb"

# -----------------------------------------------------------------------------
# Scenario (b): Control-plane happy path + web.tar.gz permissions + pre-backup + manual rollback
# -----------------------------------------------------------------------------
rsi_b="$scratch/rsi-b"
seed_initial_v1 "$rsi_b" "control-plane"
make_release "$rsi_b" "v2" "control-plane" "tar-restrictive"
export MOCK_RSI_DIR="$rsi_b"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "releases/v1" >"$MOCK_SYSCTL_STATE/unitver.spawn-serve.service"
echo "releases/v1" >"$MOCK_SYSCTL_STATE/unitver.spawn-node-gateway.service"
: >"$MOCK_EVENTS_LOG"

rc=0
SPAWN_UPGRADE_NOW_EPOCH=1000 \
SPAWN_UPGRADE_PRE_BACKUP_CMD="printf 'backup:current=%s\n' \"\$(readlink '$rsi_b/current')\" >>'$MOCK_EVENTS_LOG'" \
  bash "$upgrade_sh" --rsi-dir "$rsi_b" --role control-plane --version v2 \
  --probe-retries 2 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/b1.out" 2>&1 || rc=$?
check "(b) control-plane v1->v2 exits 0 (got $rc)" [ "$rc" -eq 0 ]
check "(b) current -> releases/v2" [ "$(readlink "$rsi_b/current")" = "releases/v2" ]
check "(b) previous -> releases/v1" [ "$(readlink "$rsi_b/previous")" = "releases/v1" ]
check "(b) bin/spawn -> ../current/spawn" [ "$(readlink "$rsi_b/bin/spawn")" = "../current/spawn" ]
check "(b) bin/spawn-node-gateway -> ../current/spawn-node-gateway" [ "$(readlink "$rsi_b/bin/spawn-node-gateway")" = "../current/spawn-node-gateway" ]
check "(b) web -> web-extracted/v2 and releases/v2 stays immutable (no releases/v2/web)" \
  bash -c '[ "$(readlink "$1/web")" = "web-extracted/v2" ] && [ ! -e "$1/releases/v2/web" ]' _ "$rsi_b"
check "(b) extracted 0500 dir normalized to writable 0755" [ "$(stat -c %a "$rsi_b/web-extracted/v2/locked")" = "755" ]
check "(b) extracted 0600 asset normalized to world-readable 0644" [ "$(stat -c %a "$rsi_b/web-extracted/v2/locked/style.css")" = "644" ]
check "(b) pre-backup ran while current was still releases/v1" grep -qx "backup:current=releases/v1" "$MOCK_EVENTS_LOG"
check "(b) init-db ran on new v2 binary" grep -qx "spawn:v2:init-db" "$MOCK_EVENTS_LOG"
check "(b) upgrade-state.json reports status=active current=v2 previous=v1" \
  jq_ok '.status == "active" and .role == "control-plane" and .current_version == "v2" and .previous_version == "v1" and .watch_window_until == 1600 and .consecutive_failures == 0 and (.units | length) == 2' "$rsi_b/upgrade-state.json"
check "(b) spawn-node-gateway restarted before spawn-serve" \
  bash -c 'awk "/^systemctl:restart:spawn-node-gateway\.service$/{g=NR} /^systemctl:restart:spawn-serve\.service$/{s=NR} END{exit !(g && s && g < s)}" "$1"' _ "$MOCK_EVENTS_LOG"

# Explicit --rollback with credential URL sanitization check
: >"$MOCK_EVENTS_LOG"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_b" --role control-plane --rollback \
  --reason "postgres://dbadmin:TOPSECRET_TOKEN_XYZ@db.internal:5432/spawn manual drill" \
  --probe-retries 2 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/b2.out" 2>&1 || rc=$?
check "(b) manual --rollback exits 0 (got $rc)" [ "$rc" -eq 0 ]
check "(b) manual --rollback restored current -> releases/v1" [ "$(readlink "$rsi_b/current")" = "releases/v1" ]
check "(b) manual --rollback sets status=rolled_back" \
  jq_ok '.status == "rolled_back" and .current_version == "v1" and .rolled_back_from == "v2"' "$rsi_b/upgrade-state.json"
check "(b) credential URL redacted in state, audit, and stdout/stderr" \
  bash -c '! grep -q "TOPSECRET_TOKEN_XYZ" "$1/upgrade-state.json" "$1/upgrade-audit.jsonl" "$2" && grep -q "<url>" "$1/upgrade-state.json"' _ "$rsi_b" "$scratch/b2.out"

# -----------------------------------------------------------------------------
# Scenario (c): Control-plane canary probe failure auto-rollback (HTTP 503 & NRestarts>0)
# -----------------------------------------------------------------------------
rm -f "$MOCK_SYSCTL_STATE"/*
bash "$upgrade_sh" --rsi-dir "$rsi_b" --role control-plane --version v2 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null

# c1: Serve returns HTTP 503 when running v3-bad-http
make_release "$rsi_b" "v3-bad-http" "control-plane" "dir"
echo "v3-bad-http" >"$MOCK_SYSCTL_STATE/serve_code_by_unitver"
: >"$MOCK_EVENTS_LOG"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_b" --role control-plane --version v3-bad-http \
  --probe-retries 2 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/c1.out" 2>&1 || rc=$?
check "(c1) serve 503 probe failure triggers auto-rollback exit 3 (got $rc)" [ "$rc" -eq 3 ]
check "(c1) current rolled back to releases/v2" [ "$(readlink "$rsi_b/current")" = "releases/v2" ]
check "(c1) spawn-serve restarted back on releases/v2" [ "$(cat "$MOCK_SYSCTL_STATE/unitver.spawn-serve.service")" = "releases/v2" ]
check "(c1) spawn-node-gateway restarted back on releases/v2" [ "$(cat "$MOCK_SYSCTL_STATE/unitver.spawn-node-gateway.service")" = "releases/v2" ]
check "(c1) state status=rolled_back rolled_back_from=v3-bad-http" \
  jq_ok '.status == "rolled_back" and .current_version == "v2" and .rolled_back_from == "v3-bad-http"' "$rsi_b/upgrade-state.json"

# c2: NRestarts > 0 on spawn-serve triggers auto-rollback
rm -f "$MOCK_SYSCTL_STATE/serve_code_by_unitver"
make_release "$rsi_b" "v3-bad-restarts" "control-plane" "dir"
echo "v3-bad-restarts" >"$MOCK_SYSCTL_STATE/bump_nrestarts_on.spawn-serve.service"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_b" --role control-plane --version v3-bad-restarts \
  --probe-retries 2 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/c2.out" 2>&1 || rc=$?
check "(c2) NRestarts=2 triggers auto-rollback exit 3 (got $rc)" [ "$rc" -eq 3 ]
check "(c2) current rolled back to releases/v2" [ "$(readlink "$rsi_b/current")" = "releases/v2" ]
check "(c2) NRestarts reset to 0 after rollback restart" [ "$(cat "$MOCK_SYSCTL_STATE/nrestarts.spawn-serve.service")" = "0" ]

# -----------------------------------------------------------------------------
# Scenario (d): Worker-node Epoch Drain & spawn-hc PID survival
# -----------------------------------------------------------------------------
rsi_d="$scratch/rsi-d"
seed_initial_v1 "$rsi_d" "worker-node"
make_release "$rsi_d" "v2" "worker-node" "none"
make_release "$rsi_d" "v3-fail" "worker-node" "none"
export MOCK_RSI_DIR="$rsi_d"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
: >"$MOCK_EVENTS_LOG"

setsid sleep 300 </dev/null >/dev/null 2>&1 &
hc_pid=$!
check "(d) simulated spawn-hc background PID $hc_pid is alive before upgrade" kill -0 "$hc_pid"

rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_d" --role worker-node --version v2 \
  --probe-retries 2 --probe-interval 0 --worker-settle-secs 1 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/d1.out" 2>&1 || rc=$?
check "(d) worker-node upgrade v1->v2 exits 0 (got $rc)" [ "$rc" -eq 0 ]
check "(d) drain ran before symlink switch (saw current=releases/v1)" grep -qx "drain_saw_current=releases/v1" "$MOCK_EVENTS_LOG"
check "(d) drain ran before spawn-loop restart" \
  bash -c 'awk "/^spawn:v1:integrate --drain$/{d=NR} /^systemctl:restart:spawn-loop\.service$/{l=NR} END{exit !(d && l && d < l)}" "$1"' _ "$MOCK_EVENTS_LOG"
check "(d) worker-node restarted loop, integrate, and node in order" \
  bash -c 'awk "/^systemctl:restart:spawn-loop\.service$/{a=NR} /^systemctl:restart:spawn-integrate\.service$/{b=NR} /^systemctl:restart:spawn-node\.service$/{c=NR} END{exit !(a && b && c && a < b && b < c)}" "$1"' _ "$MOCK_EVENTS_LOG"
check "(d) detached spawn-hc PID $hc_pid survived worker-node upgrade" kill -0 "$hc_pid"

# Inject probe failure during worker-node v3-fail -> verify rollback preserves spawn-hc PID too
: >"$MOCK_EVENTS_LOG"
echo "v3-fail" >"$MOCK_SYSCTL_STATE/bump_nrestarts_on.spawn-loop.service"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_d" --role worker-node --version v3-fail \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 1 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/d2.out" 2>&1 || rc=$?
check "(d2) worker-node failed probe triggers auto-rollback exit 3 (got $rc)" [ "$rc" -eq 3 ]
check "(d2) current rolled back to releases/v2" [ "$(readlink "$rsi_d/current")" = "releases/v2" ]
check "(d2) detached spawn-hc PID $hc_pid STILL alive after worker rollback" kill -0 "$hc_pid"
kill "$hc_pid" 2>/dev/null || true
wait "$hc_pid" 2>/dev/null || true
hc_pid=""

# -----------------------------------------------------------------------------
# Scenario (e): Watchdog threshold, reset, expiry, & debounce
# -----------------------------------------------------------------------------
rsi_e="$scratch/rsi-e"
seed_initial_v1 "$rsi_e" "control-plane"
make_release "$rsi_e" "v2" "control-plane" "dir"
export MOCK_RSI_DIR="$rsi_e"
rm -f "$MOCK_SYSCTL_STATE"/*

rc=0
SPAWN_UPGRADE_NOW_EPOCH=1000 \
  bash "$upgrade_sh" --rsi-dir "$rsi_e" --role control-plane --version v2 \
  --watch-window-secs 600 --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/e0.out" 2>&1 || rc=$?
check "(e) baseline v1->v2 active setup exits 0 (got $rc)" [ "$rc" -eq 0 ]

: >"$MOCK_EVENTS_LOG"
export SPAWN_STUB_FORBID_CALL=1

echo "503" >"$MOCK_SYSCTL_STATE/serve_code"
SPAWN_UPGRADE_NOW_EPOCH=1010 bash "$watchdog_sh" --rsi-dir "$rsi_e" --failure-threshold 3 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/e1.out" 2>&1
check "(e1) 1st failure increments consecutive_failures=1 without rollback" \
  jq_ok '.status == "active" and .consecutive_failures == 1 and .current_version == "v2"' "$rsi_e/upgrade-state.json"

echo "200" >"$MOCK_SYSCTL_STATE/serve_code"
SPAWN_UPGRADE_NOW_EPOCH=1015 bash "$watchdog_sh" --rsi-dir "$rsi_e" --failure-threshold 3 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/e2.out" 2>&1
check "(e2) healthy probe resets consecutive_failures=0" \
  jq_ok '.status == "active" and .consecutive_failures == 0 and .current_version == "v2"' "$rsi_e/upgrade-state.json"

echo "503" >"$MOCK_SYSCTL_STATE/serve_code"
SPAWN_UPGRADE_NOW_EPOCH=1700 bash "$watchdog_sh" --rsi-dir "$rsi_e" --failure-threshold 1 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/e-exp.out" 2>&1
check "(e3) expired watch window ignores probe failure (current stays v2)" \
  jq_ok '.status == "active" and .consecutive_failures == 0 and .current_version == "v2"' "$rsi_e/upgrade-state.json"

echo "v2" >"$MOCK_SYSCTL_STATE/serve_code_by_unitver"
rm -f "$MOCK_SYSCTL_STATE/serve_code"
SPAWN_UPGRADE_NOW_EPOCH=1020 bash "$watchdog_sh" --rsi-dir "$rsi_e" --failure-threshold 3 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/e4a.out" 2>&1
SPAWN_UPGRADE_NOW_EPOCH=1050 bash "$watchdog_sh" --rsi-dir "$rsi_e" --failure-threshold 3 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/e4b.out" 2>&1
check "(e4) 2 consecutive failures stay active (consecutive_failures=2)" \
  jq_ok '.status == "active" and .consecutive_failures == 2 and .current_version == "v2"' "$rsi_e/upgrade-state.json"

SPAWN_UPGRADE_NOW_EPOCH=1080 bash "$watchdog_sh" --rsi-dir "$rsi_e" --failure-threshold 3 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/e4c.out" 2>&1
check "(e4) 3rd consecutive failure triggers watchdog rollback to v1" \
  jq_ok '.status == "rolled_back" and .current_version == "v1" and .rolled_back_from == "v2"' "$rsi_e/upgrade-state.json"
check "(e4) current -> releases/v1 after watchdog rollback" [ "$(readlink "$rsi_e/current")" = "releases/v1" ]
check "(e4) audit logs watchdog rolled_back event" \
  bash -c 'grep -q "\"actor\":\"watchdog\",\"event\":\"rolled_back\"" "$1/upgrade-audit.jsonl"' _ "$rsi_e"
check "(e4) watchdog NEVER invoked bin/spawn" bash -c '! grep -q "FORBIDDEN_BIN_SPAWN_CALL" "$1"' _ "$MOCK_EVENTS_LOG"

restart_count_before=$(grep -c '^systemctl:restart:' "$MOCK_EVENTS_LOG" || true)
echo "503" >"$MOCK_SYSCTL_STATE/serve_code"
SPAWN_UPGRADE_NOW_EPOCH=1110 bash "$watchdog_sh" --rsi-dir "$rsi_e" --failure-threshold 3 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/e5a.out" 2>&1
SPAWN_UPGRADE_NOW_EPOCH=1140 bash "$watchdog_sh" --rsi-dir "$rsi_e" --failure-threshold 3 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/e5b.out" 2>&1
restart_count_after=$(grep -c '^systemctl:restart:' "$MOCK_EVENTS_LOG" || true)
check "(e5) watchdog debounced after rollback (no extra restarts: $restart_count_before == $restart_count_after)" \
  [ "$restart_count_before" = "$restart_count_after" ]
unset SPAWN_STUB_FORBID_CALL

# -----------------------------------------------------------------------------
# Scenario (f) R1: Honest init-db contract + --db-blind hard check + ExecStartPre init-db
# -----------------------------------------------------------------------------
rsi_f="$scratch/rsi-f"
seed_initial_v1 "$rsi_f" "worker-node"
make_release "$rsi_f" "v2" "worker-node" "none" 0
make_release "$rsi_f" "v3-bad-initdb" "worker-node" "none" 1
export MOCK_RSI_DIR="$rsi_f"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "$rsi_f/bin/spawn init-db" >"$MOCK_SYSCTL_STATE/execstartpre.spawn-loop.service"
echo "$repo/deploy/systemd/spawn-loop.service" >"$MOCK_SYSCTL_STATE/fragmentpath.spawn-loop.service"
: >"$MOCK_EVENTS_LOG"

# f1: --db-blind rejects spawn-loop.service before any symlink switch or restart.
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_f" --role worker-node --db-blind --version v2 \
  --worker-settle-secs 1 --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/f1.out" 2>&1 || rc=$?
check "(f1) --db-blind refuses spawn-loop.service (ExecStartPre init-db) exit 1 (got $rc)" [ "$rc" -eq 1 ]
check "(f1) --db-blind left current -> releases/v1" [ "$(readlink "$rsi_f/current")" = "releases/v1" ]
check "(f1) --db-blind performed 0 systemctl restarts" bash -c '! grep -q "^systemctl:restart:" "$1"' _ "$MOCK_EVENTS_LOG"

# f2: --db-blind even detects init-db from unit FragmentPath alone (when ExecStartPre property is unset).
rm -f "$MOCK_SYSCTL_STATE/execstartpre.spawn-loop.service"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_f" --role worker-node --db-blind --version v2 \
  --worker-settle-secs 1 --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/f2.out" 2>&1 || rc=$?
check "(f2) --db-blind detects init-db via real spawn-loop.service FragmentPath exit 1 (got $rc)" [ "$rc" -eq 1 ]

# f3: Stateless DB-blind canary node (spawn-node.service alone) passes --db-blind cleanly.
: >"$MOCK_EVENTS_LOG"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_f" --role worker-node --db-blind --units spawn-node.service --version v2 \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 1 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/f3.out" 2>&1 || rc=$?
check "(f3) --db-blind succeeds on spawn-node.service exit 0 (got $rc)" [ "$rc" -eq 0 ]
check "(f3) current -> releases/v2 after db-blind node upgrade" [ "$(readlink "$rsi_f/current")" = "releases/v2" ]

# f4: Restore spawn-loop ExecStartPre=init-db; v1 -> v2 runs v2 init-db cleanly and v3-bad-initdb auto-rolls back.
echo "$rsi_f/bin/spawn init-db" >"$MOCK_SYSCTL_STATE/execstartpre.spawn-loop.service"
: >"$MOCK_EVENTS_LOG"
bash "$upgrade_sh" --rsi-dir "$rsi_f" --role worker-node --version v2 \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 1 \
  --pre-backup-cmd "echo worker_backup >>'$MOCK_EVENTS_LOG'" \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null 2>&1 || true
: >"$MOCK_EVENTS_LOG"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_f" --role worker-node --version v3-bad-initdb \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 1 \
  --pre-backup-cmd "echo worker_backup_before_bad_initdb >>'$MOCK_EVENTS_LOG'" \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/f4.out" 2>&1 || rc=$?
check "(f4) failing ExecStartPre init-db on v3-bad-initdb triggers worker rollback exit 3 (got $rc)" [ "$rc" -eq 3 ]
check "(f4) pre-backup ran on worker-node before bad init-db" grep -qx "worker_backup_before_bad_initdb" "$MOCK_EVENTS_LOG"
check "(f4) spawn:v3-bad-initdb:init-db executed via spawn-loop ExecStartPre" grep -qx "spawn:v3-bad-initdb:init-db" "$MOCK_EVENTS_LOG"
check "(f4) current rolled back to releases/v2 cleanly" [ "$(readlink "$rsi_f/current")" = "releases/v2" ]

# -----------------------------------------------------------------------------
# Scenario (g) R2 + #8: web.tar.gz symlinks, hardlinks, setuid/setgid, unlisted web/dist
# -----------------------------------------------------------------------------
rsi_g="$scratch/rsi-g"
seed_initial_v1 "$rsi_g" "control-plane"
export MOCK_RSI_DIR="$rsi_g"
rm -f "$MOCK_SYSCTL_STATE"/*

build_evil_tar_release() {
  local tag=$1 kind=$2
  make_release "$rsi_g" "$tag" "control-plane" "none"
  local rdir="$rsi_g/releases/$tag"
  python3 - "$rdir" "$kind" "$scratch/outside-secret.txt" <<'PY'
import io, os, sys, tarfile

rdir, kind, outside = sys.argv[1:4]
tar_path = os.path.join(rdir, "web.tar.gz")
with tarfile.open(tar_path, "w:gz") as tf:
    base = tarfile.TarInfo("dist/index.html")
    body = b"<html>ok</html>\n"
    base.size = len(body)
    base.mode = 0o644
    tf.addfile(base, io.BytesIO(body))

    if kind == "abs-sym":
        ti = tarfile.TarInfo("dist/leak.txt")
        ti.type = tarfile.SYMTYPE
        ti.linkname = outside
        tf.addfile(ti)
    elif kind == "rel-sym":
        ti = tarfile.TarInfo("dist/leak.txt")
        ti.type = tarfile.SYMTYPE
        ti.linkname = "../../../outside-secret.txt"
        tf.addfile(ti)
    elif kind == "root-sym":
        ti = tarfile.TarInfo("dist/root")
        ti.type = tarfile.SYMTYPE
        ti.linkname = "/"
        tf.addfile(ti)
    elif kind == "hardlink":
        ti = tarfile.TarInfo("dist/hard.html")
        ti.type = tarfile.LNKTYPE
        ti.linkname = "dist/index.html"
        tf.addfile(ti)
    elif kind == "suid":
        ti = tarfile.TarInfo("dist/suid.bin")
        payload = b"#!/bin/sh\necho suid\n"
        ti.size = len(payload)
        ti.mode = 0o4755
        tf.addfile(ti, io.BytesIO(payload))
    elif kind == "sgid":
        ti = tarfile.TarInfo("dist/sgid.bin")
        payload = b"#!/bin/sh\necho sgid\n"
        ti.size = len(payload)
        ti.mode = 0o2755
        tf.addfile(ti, io.BytesIO(payload))
PY
  rehash_release "$rdir"
}

printf 'OUTSIDE_SECRET\n' >"$scratch/outside-secret.txt"
for evil_case in abs-sym rel-sym root-sym hardlink suid sgid; do
  etag="v2-tar-$evil_case"
  build_evil_tar_release "$etag" "$evil_case"
  rc=0
  bash "$upgrade_sh" --rsi-dir "$rsi_g" --role control-plane --version "$etag" \
    --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/g-$evil_case.out" 2>&1 || rc=$?
  check "(g) web.tar.gz rejects $evil_case pre-switch exit 1 (got $rc)" [ "$rc" -eq 1 ]
done

# Unlisted web/dist alongside listed web.tar.gz must be rejected
build_evil_tar_release "v2-unlisted-dir" "clean"
mkdir -p "$rsi_g/releases/v2-unlisted-dir/web/dist"
printf '<html>SMUGGLED</html>\n' >"$rsi_g/releases/v2-unlisted-dir/web/dist/index.html"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_g" --role control-plane --version "v2-unlisted-dir" \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/g-coexist.out" 2>&1 || rc=$?
check "(g) listed web.tar.gz + unlisted web/dist fails closed exit 1 (got $rc)" [ "$rc" -eq 1 ]
check "(g) all R2/#8 attacks left current at releases/v1" [ "$(readlink "$rsi_g/current")" = "releases/v1" ]

# -----------------------------------------------------------------------------
# Scenario (h) R3: Legacy non-symlink live layout adoption + rollback + manual --rollback
# -----------------------------------------------------------------------------
rsi_h="$scratch/rsi-h"
mkdir -p "$rsi_h/bin" "$rsi_h/web/assets"
for bin_name in spawn spawn-node-gateway spawn-hc; do
  cat >"$rsi_h/bin/$bin_name" <<EOF
#!/usr/bin/env bash
echo "legacy-$bin_name"
EOF
  chmod 755 "$rsi_h/bin/$bin_name"
done
printf '<html>LEGACY_V1_UI</html>\n' >"$rsi_h/web/index.html"
printf '/* legacy */\n' >"$rsi_h/web/assets/app.css"
make_release "$rsi_h" "v2-bad-503" "control-plane" "dir"
export MOCK_RSI_DIR="$rsi_h"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "v2-bad-503" >"$MOCK_SYSCTL_STATE/serve_code_by_unitver"

rc=0
SPAWN_UPGRADE_NOW_EPOCH=2000 \
  bash "$upgrade_sh" --rsi-dir "$rsi_h" --role control-plane --version v2-bad-503 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/h1.out" 2>&1 || rc=$?
check "(h1) legacy live host unhealthy v2 triggers rollback exit 3 (got $rc)" [ "$rc" -eq 3 ]
check "(h1) snapshot release pre-upgrade-* created and active as current" \
  bash -c 'tag=$(basename "$(readlink "$1/current")") && [[ "$tag" == pre-upgrade-* ]] && [ -x "$1/releases/$tag/spawn-hc" ] && [ -x "$1/releases/$tag/spawn-node-gateway" ]' _ "$rsi_h"
check "(h1) all 3 legacy binaries restored in bin/ after rollback" \
  bash -c '[ "$("$1/bin/spawn")" = "legacy-spawn" ] && [ "$("$1/bin/spawn-node-gateway")" = "legacy-spawn-node-gateway" ] && [ "$("$1/bin/spawn-hc")" = "legacy-spawn-hc" ]' _ "$rsi_h"
check "(h1) legacy web UI preserved through rollback" grep -q "LEGACY_V1_UI" "$rsi_h/web/index.html"

# Legacy healthy upgrade -> manual --rollback restores pre-upgrade-* binaries and UI
rsi_h2="$scratch/rsi-h2"
mkdir -p "$rsi_h2/bin" "$rsi_h2/web"
for bin_name in spawn spawn-node-gateway spawn-hc; do
  printf '#!/usr/bin/env bash\necho "legacy2-%s"\n' "$bin_name" >"$rsi_h2/bin/$bin_name"
  chmod 755 "$rsi_h2/bin/$bin_name"
done
printf '<html>LEGACY2_UI</html>\n' >"$rsi_h2/web/index.html"
make_release "$rsi_h2" "v2" "control-plane" "dir"
export MOCK_RSI_DIR="$rsi_h2"
rm -f "$MOCK_SYSCTL_STATE"/*
rc=0
SPAWN_UPGRADE_NOW_EPOCH=2100 \
  bash "$upgrade_sh" --rsi-dir "$rsi_h2" --role control-plane --version v2 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/h2a.out" 2>&1 || rc=$?
check "(h2) legacy host healthy upgrade v2 exits 0 (got $rc)" [ "$rc" -eq 0 ]
check "(h2) previous points to pre-upgrade-* snapshot" \
  bash -c '[[ "$(basename "$(readlink "$1/previous")")" == pre-upgrade-* ]]' _ "$rsi_h2"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_h2" --role control-plane --rollback \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/h2b.out" 2>&1 || rc=$?
check "(h2) manual --rollback to legacy snapshot exits 0 (got $rc)" [ "$rc" -eq 0 ]
check "(h2) manual --rollback restored legacy2 spawn-hc and web UI" \
  bash -c '[ "$("$1/bin/spawn-hc")" = "legacy2-spawn-hc" ] && grep -q "LEGACY2_UI" "$1/web/index.html"' _ "$rsi_h2"

# -----------------------------------------------------------------------------
# Scenario (i) #1: Watchdog yields (superseded) when install_build replaces symlinks
# -----------------------------------------------------------------------------
rsi_i="$scratch/rsi-i"
seed_initial_v1 "$rsi_i" "control-plane"
make_release "$rsi_i" "v2" "control-plane" "dir"
export MOCK_RSI_DIR="$rsi_i"
rm -f "$MOCK_SYSCTL_STATE"/*
SPAWN_UPGRADE_NOW_EPOCH=3000 \
  bash "$upgrade_sh" --rsi-dir "$rsi_i" --role control-plane --version v2 \
  --watch-window-secs 600 --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null

# Simulate deploy.sh install_build replacing symlinks with regular files/dirs.
rm -f "$rsi_i/bin/spawn" "$rsi_i/web"
printf '#!/usr/bin/env bash\necho "LANDED_MAIN_BUILD"\n' >"$rsi_i/bin/spawn"
chmod 755 "$rsi_i/bin/spawn"
mkdir -p "$rsi_i/web"
printf '<html>LANDED_MAIN_UI</html>\n' >"$rsi_i/web/index.html"
echo "503" >"$MOCK_SYSCTL_STATE/serve_code"
: >"$MOCK_EVENTS_LOG"

rc=0
SPAWN_UPGRADE_NOW_EPOCH=3030 \
  bash "$watchdog_sh" --rsi-dir "$rsi_i" --failure-threshold 1 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/i1.out" 2>&1 || rc=$?
check "(i) superseded watchdog exits 0 (got $rc)" [ "$rc" -eq 0 ]
check "(i) state transitioned to status=superseded" jq_ok '.status == "superseded"' "$rsi_i/upgrade-state.json"
check "(i) audit logged superseded event" grep -q '"event":"superseded"' "$rsi_i/upgrade-audit.jsonl"
check "(i) landed regular bin/spawn and web/ preserved untouched" \
  bash -c '[ ! -L "$1/bin/spawn" ] && [ "$("$1/bin/spawn")" = "LANDED_MAIN_BUILD" ] && [ ! -L "$1/web" ] && grep -q "LANDED_MAIN_UI" "$1/web/index.html"' _ "$rsi_i"
check "(i) zero systemctl restarts triggered on superseded" bash -c '! grep -q "^systemctl:restart:" "$1"' _ "$MOCK_EVENTS_LOG"

# -----------------------------------------------------------------------------
# Scenario (j) #2: Worker crash-loop caught inside --worker-settle-secs
# -----------------------------------------------------------------------------
rsi_j="$scratch/rsi-j"
seed_initial_v1 "$rsi_j" "worker-node"
make_release "$rsi_j" "v2" "worker-node" "none"
make_release "$rsi_j" "v3-crashloop" "worker-node" "none"
export MOCK_RSI_DIR="$rsi_j"
rm -f "$MOCK_SYSCTL_STATE"/*
bash "$upgrade_sh" --rsi-dir "$rsi_j" --role worker-node --version v2 \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 1 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null

# Initial poll passes (poll #1 active/running), poll #2 (during settle) simulates crash loop.
echo "v3-crashloop" >"$MOCK_SYSCTL_STATE/crash_after_polls_on.spawn-loop.service"
echo "2" >"$MOCK_SYSCTL_STATE/crash_after_polls_limit.spawn-loop.service"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_j" --role worker-node --version v3-crashloop \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 2 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/j1.out" 2>&1 || rc=$?
check "(j) crash-looping worker caught during settle window exit 3 (got $rc)" [ "$rc" -eq 3 ]
check "(j) current rolled back to releases/v2" [ "$(readlink "$rsi_j/current")" = "releases/v2" ]
check "(j) crashloop release NEVER recorded active / verified healthy" \
  bash -c '! grep -q "\"to_version\":\"v3-crashloop\".*\"verified healthy\"" "$1/upgrade-audit.jsonl"' _ "$rsi_j"

# -----------------------------------------------------------------------------
# Scenario (k) #3 & #6: Stopped units respected + 0 active units fail closed
# -----------------------------------------------------------------------------
rsi_k="$scratch/rsi-k"
seed_initial_v1 "$rsi_k" "worker-node"
make_release "$rsi_k" "v2" "worker-node" "none"
export MOCK_RSI_DIR="$rsi_k"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-loop.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-node.service"
: >"$MOCK_EVENTS_LOG"

rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_k" --role worker-node --version v2 \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 1 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/k1.out" 2>&1 || rc=$?
check "(k1) partial active worker upgrade exits 0 (got $rc)" [ "$rc" -eq 0 ]
check "(k1) stopped spawn-integrate/spawn-node were NEVER restarted" \
  bash -c '! grep -qE "^systemctl:restart:(spawn-integrate|spawn-node)\.service$" "$1" && grep -qx "systemctl:restart:spawn-loop.service" "$1"' _ "$MOCK_EVENTS_LOG"
check "(k1) stopped units stayed inactive" \
  bash -c '[ "$(cat "$1/active.spawn-integrate.service")" = "inactive" ] && [ "$(cat "$1/active.spawn-node.service")" = "inactive" ]' _ "$MOCK_SYSCTL_STATE"

# 0 active units must fail closed unless --allow-no-active-units is set
rsi_k2="$scratch/rsi-k2"
seed_initial_v1 "$rsi_k2" "worker-node"
make_release "$rsi_k2" "v2" "worker-node" "none"
export MOCK_RSI_DIR="$rsi_k2"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-loop.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-node.service"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_k2" --role worker-node --version v2 \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 1 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/k2a.out" 2>&1 || rc=$?
check "(k2) 0 active units fails closed exit 1 (got $rc)" [ "$rc" -eq 1 ]
check "(k2) current stayed releases/v1 and state file absent" \
  bash -c '[ "$(readlink "$1/current")" = "releases/v1" ] && [ ! -e "$1/upgrade-state.json" ]' _ "$rsi_k2"

rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_k2" --role worker-node --version v2 --allow-no-active-units \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/k2b.out" 2>&1 || rc=$?
check "(k2) --allow-no-active-units permits 0-unit upgrade exit 0 (got $rc)" [ "$rc" -eq 0 ]
check "(k2) --allow-no-active-units does NOT falsely claim 'verified healthy'" \
  bash -c '! grep -q "verified healthy" "$1/upgrade-audit.jsonl" "$1/upgrade-state.json" "$2"' _ "$rsi_k2" "$scratch/k2b.out"
check "(k2) --allow-no-active-units records honest staged 0-unit status in state and audit" \
  bash -c 'jq -e ".status == \"active\" and (.reason | contains(\"staged (0 active units\"))" "$1/upgrade-state.json" >/dev/null && grep -q "staged (0 active units" "$1/upgrade-audit.jsonl"' _ "$rsi_k2"

# -----------------------------------------------------------------------------
# Scenario (l) #4: Full secret redaction across state, audit, stdout, and stderr
# -----------------------------------------------------------------------------
rsi_l="$scratch/rsi-l"
seed_initial_v1 "$rsi_l" "control-plane"
make_release "$rsi_l" "v2" "control-plane" "dir"
export MOCK_RSI_DIR="$rsi_l"
rm -f "$MOCK_SYSCTL_STATE"/*
SPAWN_UPGRADE_NOW_EPOCH=4000 \
  bash "$upgrade_sh" --rsi-dir "$rsi_l" --role control-plane --version v2 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null 2>&1

# Watchdog failure tick with credentialed serve-url must redact both state/audit and stderr.
echo "503" >"$MOCK_SYSCTL_STATE/serve_code"
SPAWN_UPGRADE_NOW_EPOCH=4010 \
  bash "$watchdog_sh" --rsi-dir "$rsi_l" --failure-threshold 3 \
  --serve-url "https://wdadmin:CANARY_WD_SECRET@127.0.0.1:8787/" \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/lwd.stdout" 2>"$scratch/lwd.stderr"

# Run explicit --rollback with multiple secret token forms + credentialed probe URL failing stderr.
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_l" --role control-plane --rollback \
  --serve-url "https://admin:CANARY_URL_SECRET@127.0.0.1:8787/" \
  --reason 'Bearer CANARY_BEARER; Basic CANARY_BASIC; password=CANARY_PASS token="CANARY_TOK" secret:CANARY_SEC api_key=CANARY_KEY Authorization: CANARY_AUTH {"password":"CANARY_JSON_PW"} --password CANARY_FLAG_PW passphrase=CANARY_PASSPHRASE Cookie: session=CANARY_COOKIE key=CANARY_RAW_KEY sk-CANARY_SK_TOKEN_999 keep_task-1234 disk-check' \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/l.stdout" 2>"$scratch/l.stderr" || rc=$?
check "(l) secret canary rollback failed health as simulated (rc=$rc)" [ "$rc" -eq 4 ]
check "(l) zero canary secret leaks across state, audit, stdout, and stderr" \
  bash -c '! grep -qE "CANARY_WD_SECRET|CANARY_URL_SECRET|CANARY_BEARER|CANARY_BASIC|CANARY_PASS|CANARY_TOK|CANARY_SEC|CANARY_KEY|CANARY_AUTH|CANARY_JSON_PW|CANARY_FLAG_PW|CANARY_PASSPHRASE|CANARY_COOKIE|CANARY_RAW_KEY|CANARY_SK_TOKEN_999" "$1/upgrade-state.json" "$1/upgrade-audit.jsonl" "$2" "$3" "$4" "$5"' \
  _ "$rsi_l" "$scratch/l.stdout" "$scratch/l.stderr" "$scratch/lwd.stdout" "$scratch/lwd.stderr"
check "(l) benign task-1234 and disk-check preserved unmangled in state" \
  bash -c 'grep -q "keep_task-1234" "$1/upgrade-state.json" && grep -q "disk-check" "$1/upgrade-state.json"' _ "$rsi_l"
check "(l) redacted markers written to state and both stderr logs" \
  bash -c 'grep -q "<redacted>" "$1/upgrade-state.json" && grep -q "<url>" "$2" && grep -q "<url>" "$3"' _ "$rsi_l" "$scratch/l.stderr" "$scratch/lwd.stderr"

# Failing pre-backup hook & failing init-db stderr must be redacted before reaching stderr.
rsi_l2="$scratch/rsi-l2"
seed_initial_v1 "$rsi_l2" "control-plane"
make_release "$rsi_l2" "v2" "control-plane" "dir"
export MOCK_RSI_DIR="$rsi_l2"
rm -f "$MOCK_SYSCTL_STATE"/*
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_l2" --role control-plane --version v2 \
  --pre-backup-cmd 'echo "hookout https://bk:CANARY_HOOK_URL@127.0.0.1/db sk-CANARY_HOOK_SK_1234"; echo "hookerr postgres://db:CANARY_HOOK_ERR_PW@127.0.0.1:5432/spawn Cookie: session=CANARY_HOOK_COOKIE" >&2; exit 1' \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/lh.stdout" 2>"$scratch/lh.stderr" || rc=$?
check "(l2) failing pre-backup hook exits 1 (got $rc)" [ "$rc" -eq 1 ]
check "(l2) zero hook secret leaks in stdout or stderr" \
  bash -c '! grep -qE "CANARY_HOOK_URL|CANARY_HOOK_SK_1234|CANARY_HOOK_ERR_PW|CANARY_HOOK_COOKIE" "$1" "$2" && grep -q "<url>" "$2" && grep -q "<redacted>" "$2"' _ "$scratch/lh.stdout" "$scratch/lh.stderr"

cat >"$rsi_l2/releases/v2/spawn" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [ "${1-}" = "init-db" ]; then
  echo 'initdb failed: postgres://m:CANARY_INITDB_PW@127.0.0.1/spawn {"password":"CANARY_INITDB_JSON"} --password CANARY_INITDB_FLAG key=CANARY_INITDB_KEY sk-CANARY_INITDB_SK_1234' >&2
  exit 1
fi
exit 0
EOF
chmod 755 "$rsi_l2/releases/v2/spawn"
rehash_release "$rsi_l2/releases/v2"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_l2" --role control-plane --version v2 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/li.stdout" 2>"$scratch/li.stderr" || rc=$?
check "(l3) failing init-db triggers auto-rollback exit 3 (got $rc)" [ "$rc" -eq 3 ]
check "(l3) zero init-db secret leaks across stdout, stderr, state, and audit" \
  bash -c '! grep -qE "CANARY_INITDB_PW|CANARY_INITDB_JSON|CANARY_INITDB_FLAG|CANARY_INITDB_KEY|CANARY_INITDB_SK_1234" "$1" "$2" "$3/upgrade-state.json" "$3/upgrade-audit.jsonl" && grep -q "<url>" "$2" && grep -q "<redacted>" "$2"' _ "$scratch/li.stdout" "$scratch/li.stderr" "$rsi_l2"

# -----------------------------------------------------------------------------
# Scenario (m) #5: Hook closes flock FD so background child never holds lock
# -----------------------------------------------------------------------------
rsi_m="$scratch/rsi-m"
seed_initial_v1 "$rsi_m" "control-plane"
make_release "$rsi_m" "v2" "control-plane" "dir"
make_release "$rsi_m" "v3" "control-plane" "dir"
export MOCK_RSI_DIR="$rsi_m"
rm -f "$MOCK_SYSCTL_STATE"/*

rc=0
SPAWN_UPGRADE_NOW_EPOCH=5000 \
  bash "$upgrade_sh" --rsi-dir "$rsi_m" --role control-plane --version v2 \
  --pre-backup-cmd "sleep 20 </dev/null >/dev/null 2>&1 & echo \$! >'$scratch/bg_hook.pid'" \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/m1.out" 2>&1 || rc=$?
bg_hook_pid=$(cat "$scratch/bg_hook.pid")
check "(m) hook spawned background child $bg_hook_pid while upgrade exited 0 (got $rc)" \
  bash -c '[ "$1" -eq 0 ] && kill -0 "$2"' _ "$rc" "$bg_hook_pid"

rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_m" --role control-plane --version v3 --dry-run \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/m2.out" 2>&1 || rc=$?
check "(m) subsequent self-upgrade --dry-run acquired lock immediately (got $rc)" [ "$rc" -eq 0 ]

rc=0
SPAWN_UPGRADE_NOW_EPOCH=5010 \
  bash "$watchdog_sh" --rsi-dir "$rsi_m" --failure-threshold 3 \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/m3.out" 2>&1 || rc=$?
check "(m) subsequent watchdog tick acquired lock cleanly without 'lock held' (got $rc)" \
  bash -c '[ "$1" -eq 0 ] && ! grep -q "lock held" "$2"' _ "$rc" "$scratch/m3.out"
kill "$bg_hook_pid" 2>/dev/null || true
wait "$bg_hook_pid" 2>/dev/null || true
bg_hook_pid=""

# -----------------------------------------------------------------------------
# Scenario (n) R4: Owner clean-stop vs crash distinction (watchdog & --rollback)
# -----------------------------------------------------------------------------
# (n0) Mock Wants=self-check: bare `systemctl restart spawn-integrate.service` (without --job-mode=ignore-requirements) pulls up inactive spawn-loop.service.
rsi_n0="$scratch/rsi-n0"
seed_initial_v1 "$rsi_n0" "worker-node"
export MOCK_RSI_DIR="$rsi_n0"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
"$mock_bin/systemctl" --user stop spawn-loop.service
echo "$rsi_n0/bin/spawn init-db" >"$MOCK_SYSCTL_STATE/execstartpre.spawn-loop.service"
: >"$MOCK_EVENTS_LOG"
"$mock_bin/systemctl" --user restart spawn-integrate.service
check "(n0) mock Wants= pulls up stopped spawn-loop when --job-mode=ignore-requirements is omitted" \
  bash -c '[ "$(cat "$1/activestate.spawn-loop.service")" = "active" ] && grep -qx "systemctl:wants-start:spawn-loop.service" "$2" && grep -qx "spawn:v1:init-db" "$2"' \
  _ "$MOCK_SYSCTL_STATE" "$MOCK_EVENTS_LOG"

# (n1) Both spawn-loop and spawn-integrate active -> owner stops spawn-loop -> 4 watchdog ticks exit 0, no rollback, loop stays inactive.
rsi_n1="$scratch/rsi-n1"
seed_initial_v1 "$rsi_n1" "worker-node"
make_release "$rsi_n1" "v2" "worker-node" "none"
export MOCK_RSI_DIR="$rsi_n1"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-loop.service"
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-node.service"
SPAWN_UPGRADE_NOW_EPOCH=6000 \
  bash "$upgrade_sh" --rsi-dir "$rsi_n1" --role worker-node --version v2 \
  --watch-window-secs 600 --probe-retries 1 --probe-interval 0 --worker-settle-secs 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null
"$mock_bin/systemctl" --user stop spawn-loop.service
: >"$MOCK_EVENTS_LOG"
n1_ok=1
for tick in 1 2 3 4; do
  rc=0
  SPAWN_UPGRADE_NOW_EPOCH=$((6000 + tick * 30)) \
    bash "$watchdog_sh" --rsi-dir "$rsi_n1" --failure-threshold 3 \
    --probe-retries 1 --probe-interval 0 \
    --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/n1-$tick.out" 2>&1 || rc=$?
  [ "$rc" -eq 0 ] || n1_ok=0
done
check "(n1) owner stops spawn-loop: 4 watchdog ticks exit 0 without rollback" \
  bash -c '[ "$1" -eq 1 ] && jq -e ".status == \"active\" and .consecutive_failures == 0 and .current_version == \"v2\"" "$2/upgrade-state.json" >/dev/null && [ "$(readlink "$2/current")" = "releases/v2" ] && ! grep -q "^systemctl:restart:" "$3" && [ "$(cat "$4/activestate.spawn-loop.service")" = "inactive" ]' \
  _ "$n1_ok" "$rsi_n1" "$MOCK_EVENTS_LOG" "$MOCK_SYSCTL_STATE"

# (n2) Both spawn-loop and spawn-integrate active -> owner stops spawn-integrate only -> 4 watchdog ticks exit 0, no rollback, integrate stays inactive.
rsi_n2="$scratch/rsi-n2"
seed_initial_v1 "$rsi_n2" "worker-node"
make_release "$rsi_n2" "v2" "worker-node" "none"
export MOCK_RSI_DIR="$rsi_n2"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-loop.service"
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-node.service"
SPAWN_UPGRADE_NOW_EPOCH=6200 \
  bash "$upgrade_sh" --rsi-dir "$rsi_n2" --role worker-node --version v2 \
  --watch-window-secs 600 --probe-retries 1 --probe-interval 0 --worker-settle-secs 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null
"$mock_bin/systemctl" --user stop spawn-integrate.service
: >"$MOCK_EVENTS_LOG"
n2_ok=1
for tick in 1 2 3 4; do
  rc=0
  SPAWN_UPGRADE_NOW_EPOCH=$((6200 + tick * 30)) \
    bash "$watchdog_sh" --rsi-dir "$rsi_n2" --failure-threshold 3 \
    --probe-retries 1 --probe-interval 0 \
    --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/n2-$tick.out" 2>&1 || rc=$?
  [ "$rc" -eq 0 ] || n2_ok=0
done
check "(n2) owner stops spawn-integrate only: 4 watchdog ticks exit 0 without rollback" \
  bash -c '[ "$1" -eq 1 ] && jq -e ".status == \"active\" and .consecutive_failures == 0 and .current_version == \"v2\"" "$2/upgrade-state.json" >/dev/null && [ "$(readlink "$2/current")" = "releases/v2" ] && ! grep -q "^systemctl:restart:" "$3" && [ "$(cat "$4/activestate.spawn-integrate.service")" = "inactive" ]' \
  _ "$n2_ok" "$rsi_n2" "$MOCK_EVENTS_LOG" "$MOCK_SYSCTL_STATE"

# (n3) Only spawn-loop active -> owner stops spawn-loop (0 remaining units) -> 4 watchdog ticks exit 0, no rollback.
rsi_n3="$scratch/rsi-n3"
seed_initial_v1 "$rsi_n3" "worker-node"
make_release "$rsi_n3" "v2" "worker-node" "none"
export MOCK_RSI_DIR="$rsi_n3"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-loop.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-node.service"
SPAWN_UPGRADE_NOW_EPOCH=6400 \
  bash "$upgrade_sh" --rsi-dir "$rsi_n3" --role worker-node --version v2 \
  --watch-window-secs 600 --probe-retries 1 --probe-interval 0 --worker-settle-secs 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null
"$mock_bin/systemctl" --user stop spawn-loop.service
: >"$MOCK_EVENTS_LOG"
n3_ok=1
for tick in 1 2 3 4; do
  rc=0
  SPAWN_UPGRADE_NOW_EPOCH=$((6400 + tick * 30)) \
    bash "$watchdog_sh" --rsi-dir "$rsi_n3" --failure-threshold 3 \
    --probe-retries 1 --probe-interval 0 \
    --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/n3-$tick.out" 2>&1 || rc=$?
  [ "$rc" -eq 0 ] || n3_ok=0
done
check "(n3) only unit cleanly stopped: 4 watchdog ticks exit 0 without rollback" \
  bash -c '[ "$1" -eq 1 ] && jq -e ".status == \"active\" and .consecutive_failures == 0 and .current_version == \"v2\"" "$2/upgrade-state.json" >/dev/null && [ "$(readlink "$2/current")" = "releases/v2" ] && ! grep -q "^systemctl:restart:" "$3" && [ "$(cat "$4/activestate.spawn-loop.service")" = "inactive" ]' \
  _ "$n3_ok" "$rsi_n3" "$MOCK_EVENTS_LOG" "$MOCK_SYSCTL_STATE"

# (n4) Crash vs clean stop: spawn-integrate cleanly stopped (Result=success), spawn-loop crashed (Result=exit-code) -> watchdog rolls back & restarts ONLY spawn-loop.
"$mock_bin/systemctl" --user stop spawn-integrate.service >/dev/null 2>&1 || true
rsi_n4="$scratch/rsi-n4"
seed_initial_v1 "$rsi_n4" "worker-node"
make_release "$rsi_n4" "v2" "worker-node" "none"
export MOCK_RSI_DIR="$rsi_n4"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-loop.service"
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-node.service"
SPAWN_UPGRADE_NOW_EPOCH=6600 \
  bash "$upgrade_sh" --rsi-dir "$rsi_n4" --role worker-node --version v2 \
  --watch-window-secs 600 --probe-retries 1 --probe-interval 0 --worker-settle-secs 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null
"$mock_bin/systemctl" --user stop spawn-integrate.service
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-loop.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/activestate.spawn-loop.service"
echo "dead" >"$MOCK_SYSCTL_STATE/substate.spawn-loop.service"
echo "exit-code" >"$MOCK_SYSCTL_STATE/result.spawn-loop.service"
: >"$MOCK_EVENTS_LOG"
for tick in 1 2 3; do
  SPAWN_UPGRADE_NOW_EPOCH=$((6600 + tick * 30)) \
    bash "$watchdog_sh" --rsi-dir "$rsi_n4" --failure-threshold 3 \
    --probe-retries 1 --probe-interval 0 \
    --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/n4-$tick.out" 2>&1
done
check "(n4) Result=exit-code crash STILL triggers watchdog rollback while cleanly stopped unit stays inactive" \
  bash -c 'jq -e ".status == \"rolled_back\" and .current_version == \"v1\"" "$1/upgrade-state.json" >/dev/null && [ "$(readlink "$1/current")" = "releases/v1" ] && grep -qx "systemctl:restart:spawn-loop.service" "$2" && ! grep -q "^systemctl:restart:spawn-integrate\.service$" "$2" && [ "$(cat "$3/activestate.spawn-integrate.service")" = "inactive" ] && [ "$(cat "$3/activestate.spawn-loop.service")" = "active" ]' \
  _ "$rsi_n4" "$MOCK_EVENTS_LOG" "$MOCK_SYSCTL_STATE"

# (n5a) Scenario D: Owner stops spawn-loop (while spawn-integrate remains active) -> manual --rollback exits 0, restarts integrate with --job-mode=ignore-requirements, keeps loop inactive (no Wants= start or init-db).
rsi_n5a="$scratch/rsi-n5a"
seed_initial_v1 "$rsi_n5a" "worker-node"
make_release "$rsi_n5a" "v2" "worker-node" "none"
export MOCK_RSI_DIR="$rsi_n5a"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-loop.service"
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-node.service"
echo "$rsi_n5a/bin/spawn init-db" >"$MOCK_SYSCTL_STATE/execstartpre.spawn-loop.service"
bash "$upgrade_sh" --rsi-dir "$rsi_n5a" --role worker-node --version v2 \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null
"$mock_bin/systemctl" --user stop spawn-loop.service
: >"$MOCK_EVENTS_LOG"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_n5a" --role worker-node --rollback \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/n5a.out" 2>&1 || rc=$?
check "(n5a) Scenario D: manual --rollback with stopped spawn-loop exits 0, restarts integrate without Wants= starting loop (got $rc)" \
  bash -c '[ "$1" -eq 0 ] && jq -e ".status == \"rolled_back\" and .current_version == \"v1\"" "$2/upgrade-state.json" >/dev/null && [ "$(readlink "$2/current")" = "releases/v1" ] && ! grep -q "^systemctl:restart:spawn-loop\.service$" "$3" && ! grep -q "^systemctl:wants-start:spawn-loop\.service$" "$3" && ! grep -q "^spawn:v1:init-db$" "$3" && grep -qx "systemctl:restart:spawn-integrate.service" "$3" && [ "$(cat "$4/activestate.spawn-loop.service")" = "inactive" ] && [ "$(cat "$4/substate.spawn-loop.service")" = "dead" ] && [ "$(cat "$4/activestate.spawn-integrate.service")" = "active" ]' \
  _ "$rc" "$rsi_n5a" "$MOCK_EVENTS_LOG" "$MOCK_SYSCTL_STATE"

# (n5b) Owner stops sole unit spawn-loop -> manual --rollback exits 0, restores symlinks, 0 restarts, loop stays inactive.
rsi_n5b="$scratch/rsi-n5b"
seed_initial_v1 "$rsi_n5b" "worker-node"
make_release "$rsi_n5b" "v2" "worker-node" "none"
export MOCK_RSI_DIR="$rsi_n5b"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-loop.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-node.service"
bash "$upgrade_sh" --rsi-dir "$rsi_n5b" --role worker-node --version v2 \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null
"$mock_bin/systemctl" --user stop spawn-loop.service
: >"$MOCK_EVENTS_LOG"
rc=0
bash "$upgrade_sh" --rsi-dir "$rsi_n5b" --role worker-node --rollback \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/n5b.out" 2>&1 || rc=$?
check "(n5b) manual --rollback when sole unit is cleanly stopped exits 0 with 0 restarts (got $rc)" \
  bash -c '[ "$1" -eq 0 ] && jq -e ".status == \"rolled_back\" and .current_version == \"v1\"" "$2/upgrade-state.json" >/dev/null && [ "$(readlink "$2/current")" = "releases/v1" ] && ! grep -q "^systemctl:restart:" "$3" && [ "$(cat "$4/activestate.spawn-loop.service")" = "inactive" ]' \
  _ "$rc" "$rsi_n5b" "$MOCK_EVENTS_LOG" "$MOCK_SYSCTL_STATE"

# (n6) Scenario G: Owner stops spawn-loop BEFORE upgrade while spawn-integrate is active -> upgrade v1->v2 restarts integrate without Wants= starting loop or running v2 init-db.
rsi_n6="$scratch/rsi-n6"
seed_initial_v1 "$rsi_n6" "worker-node"
make_release "$rsi_n6" "v2" "worker-node" "none"
export MOCK_RSI_DIR="$rsi_n6"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-node.service"
echo "$rsi_n6/bin/spawn init-db" >"$MOCK_SYSCTL_STATE/execstartpre.spawn-loop.service"
"$mock_bin/systemctl" --user stop spawn-loop.service
: >"$MOCK_EVENTS_LOG"
rc=0
SPAWN_UPGRADE_NOW_EPOCH=6800 \
  bash "$upgrade_sh" --rsi-dir "$rsi_n6" --role worker-node --version v2 \
  --probe-retries 1 --probe-interval 0 --worker-settle-secs 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/n6.out" 2>&1 || rc=$?
check "(n6) Scenario G: upgrade with pre-stopped spawn-loop restarts integrate without Wants= starting loop or running init-db (got $rc)" \
  bash -c '[ "$1" -eq 0 ] && [ "$(readlink "$2/current")" = "releases/v2" ] && grep -qx "systemctl:restart:spawn-integrate.service" "$3" && ! grep -q "^systemctl:restart:spawn-loop\.service$" "$3" && ! grep -q "^systemctl:wants-start:spawn-loop\.service$" "$3" && ! grep -q "^spawn:v2:init-db$" "$3" && [ "$(cat "$4/activestate.spawn-loop.service")" = "inactive" ] && [ "$(cat "$4/substate.spawn-loop.service")" = "dead" ] && [ "$(cat "$4/activestate.spawn-integrate.service")" = "active" ]' \
  _ "$rc" "$rsi_n6" "$MOCK_EVENTS_LOG" "$MOCK_SYSCTL_STATE"

# (n7) Scenario C: Owner stops spawn-loop cleanly, spawn-integrate crashes -> 3 watchdog ticks roll back and restart integrate without Wants= starting loop.
rsi_n7="$scratch/rsi-n7"
seed_initial_v1 "$rsi_n7" "worker-node"
make_release "$rsi_n7" "v2" "worker-node" "none"
export MOCK_RSI_DIR="$rsi_n7"
rm -f "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-loop.service"
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-node.service"
echo "$rsi_n7/bin/spawn init-db" >"$MOCK_SYSCTL_STATE/execstartpre.spawn-loop.service"
SPAWN_UPGRADE_NOW_EPOCH=7000 \
  bash "$upgrade_sh" --rsi-dir "$rsi_n7" --role worker-node --version v2 \
  --watch-window-secs 600 --probe-retries 1 --probe-interval 0 --worker-settle-secs 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null
"$mock_bin/systemctl" --user stop spawn-loop.service
echo "activating" >"$MOCK_SYSCTL_STATE/activestate.spawn-integrate.service"
echo "auto-restart" >"$MOCK_SYSCTL_STATE/substate.spawn-integrate.service"
echo "exit-code" >"$MOCK_SYSCTL_STATE/result.spawn-integrate.service"
echo "1" >"$MOCK_SYSCTL_STATE/nrestarts.spawn-integrate.service"
: >"$MOCK_EVENTS_LOG"
n7_ok=1
for tick in 1 2 3; do
  rc=0
  SPAWN_UPGRADE_NOW_EPOCH=$((7000 + tick * 30)) \
    bash "$watchdog_sh" --rsi-dir "$rsi_n7" --failure-threshold 3 \
    --probe-retries 1 --probe-interval 0 \
    --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/n7-$tick.out" 2>&1 || rc=$?
  [ "$rc" -eq 0 ] || n7_ok=0
done
check "(n7) Scenario C: stopped spawn-loop + crashing spawn-integrate -> watchdog rollback restarts integrate without Wants= starting loop" \
  bash -c '[ "$1" -eq 1 ] && jq -e ".status == \"rolled_back\" and .current_version == \"v1\"" "$2/upgrade-state.json" >/dev/null && [ "$(readlink "$2/current")" = "releases/v1" ] && grep -qx "systemctl:restart:spawn-integrate.service" "$3" && ! grep -q "^systemctl:restart:spawn-loop\.service$" "$3" && ! grep -q "^systemctl:wants-start:spawn-loop\.service$" "$3" && ! grep -q "^spawn:v1:init-db$" "$3" && [ "$(cat "$4/activestate.spawn-loop.service")" = "inactive" ] && [ "$(cat "$4/substate.spawn-loop.service")" = "dead" ] && [ "$(cat "$4/activestate.spawn-integrate.service")" = "active" ]' \
  _ "$n7_ok" "$rsi_n7" "$MOCK_EVENTS_LOG" "$MOCK_SYSCTL_STATE"

# Unit sanity checks matching systemd/test-sync-units invariants
check "(units) spawn-upgrade-watchdog.service is regular file without spawn.env/DATABASE_URL" \
  bash -c '[ -f "$1" ] && [ ! -L "$1" ] && ! grep -qiE "spawn\.env|DATABASE_URL" "$1"' _ "$unit_service"
check "(units) spawn-upgrade-watchdog.timer is regular file under timers.target" \
  bash -c '[ -f "$1" ] && [ ! -L "$1" ] && grep -q "WantedBy=timers.target" "$1"' _ "$unit_timer"

# -----------------------------------------------------------------------------
# Scenario (o) PR #281 Blocker 1: control-plane snapshot drop-ins (legacy + Go/UDS)
# -----------------------------------------------------------------------------
add_snapshot_artifacts() {
  local rdir=$1 cov=${2:-1} reg=${3:-1}
  if [ "$cov" -eq 1 ]; then
    printf '{"root":".","registry_path":"docs/feature-map/registry.json","registry_writable":false,"read_only_reason":"precomputed","report":{"schema_version":1},"features":[]}\n' >"$rdir/feature-coverage.json"
    chmod 0644 "$rdir/feature-coverage.json"
  fi
  if [ "$reg" -eq 1 ]; then
    printf '{"schema_version":1,"initiative_id":"spawn","boundary":{"project_name":"Spawn","description":"d","included_categories":[],"exclusions":[]},"features":[]}\n' >"$rdir/registry.json"
    chmod 0644 "$rdir/registry.json"
  fi
  rehash_release "$rdir"
}

cp_units_o="$scratch/units-o"
mkdir -p "$cp_units_o"

# (o1) Release without snapshot artifacts -> no drop-ins, no daemon-reload
rsi_o1="$scratch/rsi-o1"
seed_initial_v1 "$rsi_o1" "control-plane"
make_release "$rsi_o1" "v2-live" "control-plane" "dir"
export MOCK_RSI_DIR="$rsi_o1"
rm -rf "$cp_units_o"/* "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-serve.service"
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-node-gateway.service"
: >"$MOCK_EVENTS_LOG"
rc=0
SPAWN_UNIT_DIR="$cp_units_o" SPAWN_UPGRADE_NOW_EPOCH=7100 \
  bash "$upgrade_sh" --rsi-dir "$rsi_o1" --role control-plane --version v2-live \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/o1.out" 2>&1 || rc=$?
check "(o1) control-plane without snapshot artifacts exits 0, no drop-in, no daemon-reload (got $rc)" \
  bash -c '[ "$1" -eq 0 ] && [ ! -e "$2/spawn-serve.service.d/50-feature-coverage-snapshot.conf" ] && [ ! -e "$2/spawn-core.service.d/50-feature-coverage-snapshot.conf" ] && ! grep -q "daemon-reload" "$3"' \
  _ "$rc" "$cp_units_o" "$MOCK_EVENTS_LOG"

# (o2) Missing one of the two snapshot artifacts -> no drop-ins
for partial_mode in "cov-only" "reg-only"; do
  rsi_op="$scratch/rsi-o2-$partial_mode"
  seed_initial_v1 "$rsi_op" "control-plane"
  make_release "$rsi_op" "v2-partial" "control-plane" "dir"
  if [ "$partial_mode" = "cov-only" ]; then
    add_snapshot_artifacts "$rsi_op/releases/v2-partial" 1 0
  else
    add_snapshot_artifacts "$rsi_op/releases/v2-partial" 0 1
  fi
  export MOCK_RSI_DIR="$rsi_op"
  rm -rf "$cp_units_o"/* "$MOCK_SYSCTL_STATE"/*
  echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-serve.service"
  echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-node-gateway.service"
  : >"$MOCK_EVENTS_LOG"
  rc=0
  SPAWN_UNIT_DIR="$cp_units_o" SPAWN_UPGRADE_NOW_EPOCH=7150 \
    bash "$upgrade_sh" --rsi-dir "$rsi_op" --role control-plane --version v2-partial \
    --probe-retries 1 --probe-interval 0 \
    --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/o2-$partial_mode.out" 2>&1 || rc=$?
  check "(o2:$partial_mode) partial snapshot artifacts leave drop-ins absent (got $rc)" \
    bash -c '[ "$1" -eq 0 ] && [ ! -e "$2/spawn-serve.service.d/50-feature-coverage-snapshot.conf" ] && [ ! -e "$2/spawn-core.service.d/50-feature-coverage-snapshot.conf" ] && ! grep -q "daemon-reload" "$3"' \
    _ "$rc" "$cp_units_o" "$MOCK_EVENTS_LOG"
done

# (o3) Release with both verified snapshot artifacts -> creates 0644 drop-ins for serve+core and daemon-reloads before restart
rsi_o3="$scratch/rsi-o3"
seed_initial_v1 "$rsi_o3" "control-plane"
make_release "$rsi_o3" "v2-snap" "control-plane" "dir"
add_snapshot_artifacts "$rsi_o3/releases/v2-snap" 1 1
export MOCK_RSI_DIR="$rsi_o3"
rm -rf "$cp_units_o"/* "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-serve.service"
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-node-gateway.service"
: >"$MOCK_EVENTS_LOG"
rc=0
SPAWN_UNIT_DIR="$cp_units_o" SPAWN_UPGRADE_NOW_EPOCH=7200 \
  bash "$upgrade_sh" --rsi-dir "$rsi_o3" --role control-plane --version v2-snap \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/o3.out" 2>&1 || rc=$?
serve_dropin="$cp_units_o/spawn-serve.service.d/50-feature-coverage-snapshot.conf"
core_dropin="$cp_units_o/spawn-core.service.d/50-feature-coverage-snapshot.conf"
check "(o3) verified snapshot release installs 0644 drop-ins and daemon-reloads before restarting serve (got $rc)" \
  bash -c '
    [ "$1" -eq 0 ] &&
    [ -f "$2" ] && [ "$(stat -c %a "$2")" = 644 ] &&
    [ -f "$3" ] && [ "$(stat -c %a "$3")" = 644 ] &&
    grep -qxF "Environment=SPAWN_FEATURE_COVERAGE_SNAPSHOT=$4/current/feature-coverage.json" "$2" &&
    grep -qxF "Environment=SPAWN_FEATURE_REGISTRY=$4/current/registry.json" "$2" &&
    grep -qxF "Environment=SPAWN_FEATURE_COVERAGE_SNAPSHOT=$4/current/feature-coverage.json" "$3" &&
    grep -qxF "Environment=SPAWN_FEATURE_REGISTRY=$4/current/registry.json" "$3" &&
    awk "/^systemctl:daemon-reload:/{d=NR} /^systemctl:restart:spawn-serve\\.service$/{r=NR} END{exit !(d && r && d < r)}" "$5"
  ' _ "$rc" "$serve_dropin" "$core_dropin" "$rsi_o3" "$MOCK_EVENTS_LOG"

# (o4) Go/UDS control-plane (--units spawn-core.service) with verified snapshots enables spawn-core drop-in before restart
cp_units_go="$scratch/units-o-go"
mkdir -p "$cp_units_go"
rm -rf "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-core.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-serve.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-node-gateway.service"
make_release "$rsi_o3" "v3-snap-go" "control-plane" "dir"
add_snapshot_artifacts "$rsi_o3/releases/v3-snap-go" 1 1
: >"$MOCK_EVENTS_LOG"
rc=0
SPAWN_UPGRADE_NOW_EPOCH=7250 \
  bash "$upgrade_sh" --rsi-dir "$rsi_o3" --unit-dir "$cp_units_go" --role control-plane --units spawn-core.service --version v3-snap-go \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/o4.out" 2>&1 || rc=$?
check "(o4) Go/UDS control-plane enables spawn-core drop-in and daemon-reloads before restarting spawn-core (got $rc)" \
  bash -c '
    [ "$1" -eq 0 ] &&
    [ -f "$2/spawn-core.service.d/50-feature-coverage-snapshot.conf" ] &&
    grep -qxF "Environment=SPAWN_FEATURE_COVERAGE_SNAPSHOT=$3/current/feature-coverage.json" "$2/spawn-core.service.d/50-feature-coverage-snapshot.conf" &&
    awk "/^systemctl:daemon-reload:/{d=NR} /^systemctl:restart:spawn-core\\.service$/{r=NR} END{exit !(d && r && d < r)}" "$4"
  ' _ "$rc" "$cp_units_go" "$rsi_o3" "$MOCK_EVENTS_LOG"

# (o5a) Manual --rollback from snapshot v2-snap back to live v1 removes drop-ins and daemon-reloads before restart
rm -rf "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-serve.service"
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-node-gateway.service"
# Re-upgrade rsi_o3 to v2-snap cleanly so current=v2-snap, previous=v1, drop-ins active in $cp_units_o
rsi_o5="$scratch/rsi-o5"
cp_units_o5="$scratch/units-o5"
mkdir -p "$cp_units_o5"
seed_initial_v1 "$rsi_o5" "control-plane"
make_release "$rsi_o5" "v2-snap" "control-plane" "dir"
add_snapshot_artifacts "$rsi_o5/releases/v2-snap" 1 1
export MOCK_RSI_DIR="$rsi_o5"
SPAWN_UNIT_DIR="$cp_units_o5" SPAWN_UPGRADE_NOW_EPOCH=7300 \
  bash "$upgrade_sh" --rsi-dir "$rsi_o5" --role control-plane --version v2-snap \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >/dev/null
: >"$MOCK_EVENTS_LOG"
rc=0
SPAWN_UNIT_DIR="$cp_units_o5" SPAWN_UPGRADE_NOW_EPOCH=7320 \
  bash "$upgrade_sh" --rsi-dir "$rsi_o5" --role control-plane --rollback \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/o5a.out" 2>&1 || rc=$?
check "(o5a) manual --rollback to live v1 removes snapshot drop-ins and daemon-reloads before restart (got $rc)" \
  bash -c '
    [ "$1" -eq 0 ] &&
    [ ! -e "$2/spawn-serve.service.d/50-feature-coverage-snapshot.conf" ] &&
    [ ! -e "$2/spawn-core.service.d/50-feature-coverage-snapshot.conf" ] &&
    awk "/^systemctl:daemon-reload:/{d=NR} /^systemctl:restart:spawn-serve\\.service$/{r=NR} END{exit !(d && r && d < r)}" "$3"
  ' _ "$rc" "$cp_units_o5" "$MOCK_EVENTS_LOG"

# (o5b) Probe-failure auto-rollback from snapshot v2-snap-bad back to live v1 cleans up newly written drop-ins
rsi_o5b="$scratch/rsi-o5b"
cp_units_o5b="$scratch/units-o5b"
mkdir -p "$cp_units_o5b"
seed_initial_v1 "$rsi_o5b" "control-plane"
make_release "$rsi_o5b" "v2-snap-bad" "control-plane" "dir"
add_snapshot_artifacts "$rsi_o5b/releases/v2-snap-bad" 1 1
export MOCK_RSI_DIR="$rsi_o5b"
rm -rf "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-serve.service"
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-node-gateway.service"
echo "v2-snap-bad" >"$MOCK_SYSCTL_STATE/serve_code_by_unitver"
: >"$MOCK_EVENTS_LOG"
rc=0
SPAWN_UNIT_DIR="$cp_units_o5b" SPAWN_UPGRADE_NOW_EPOCH=7350 \
  bash "$upgrade_sh" --rsi-dir "$rsi_o5b" --role control-plane --version v2-snap-bad \
  --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/o5b.out" 2>&1 || rc=$?
check "(o5b) auto-rollback on failed snapshot release removes drop-ins cleanly (got $rc)" \
  bash -c '[ "$1" -eq 3 ] && [ "$(readlink "$2/current")" = "releases/v1" ] && [ ! -e "$3/spawn-serve.service.d/50-feature-coverage-snapshot.conf" ] && [ ! -e "$3/spawn-core.service.d/50-feature-coverage-snapshot.conf" ]' \
  _ "$rc" "$rsi_o5b" "$cp_units_o5b"

# (o6) --role worker-node NEVER creates snapshot drop-ins even if snapshot artifacts exist
rsi_o6="$scratch/rsi-o6"
cp_units_w="$scratch/units-o6-worker"
mkdir -p "$cp_units_w"
seed_initial_v1 "$rsi_o6" "worker-node"
make_release "$rsi_o6" "v2-worker" "worker-node" "none"
add_snapshot_artifacts "$rsi_o6/releases/v2-worker" 1 1
export MOCK_RSI_DIR="$rsi_o6"
rm -rf "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-loop.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-integrate.service"
echo "inactive" >"$MOCK_SYSCTL_STATE/active.spawn-node.service"
: >"$MOCK_EVENTS_LOG"
rc=0
SPAWN_UNIT_DIR="$cp_units_w" SPAWN_UPGRADE_NOW_EPOCH=7400 \
  bash "$upgrade_sh" --rsi-dir "$rsi_o6" --role worker-node --version v2-worker \
  --worker-settle-secs 0 --probe-retries 1 --probe-interval 0 \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/o6.out" 2>&1 || rc=$?
check "(o6) worker-node upgrade never creates snapshot drop-ins (got $rc)" \
  bash -c '[ "$1" -eq 0 ] && [ ! -e "$2/spawn-serve.service.d/50-feature-coverage-snapshot.conf" ] && [ ! -e "$2/spawn-core.service.d/50-feature-coverage-snapshot.conf" ] && ! grep -q "daemon-reload" "$3"' \
  _ "$rc" "$cp_units_w" "$MOCK_EVENTS_LOG"

# (o7) --dry-run on snapshot release creates no drop-ins and runs no daemon-reload
cp_units_dry="$scratch/units-o7-dry"
mkdir -p "$cp_units_dry"
export MOCK_RSI_DIR="$rsi_o3"
rm -rf "$MOCK_SYSCTL_STATE"/*
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-serve.service"
echo "active" >"$MOCK_SYSCTL_STATE/active.spawn-node-gateway.service"
: >"$MOCK_EVENTS_LOG"
rc=0
SPAWN_UNIT_DIR="$cp_units_dry" \
  bash "$upgrade_sh" --rsi-dir "$rsi_o3" --role control-plane --version v3-snap-go --dry-run \
  --systemctl "$mock_bin/systemctl --user" --curl "$mock_bin/curl" >"$scratch/o7.out" 2>&1 || rc=$?
check "(o7) --dry-run on snapshot release leaves drop-ins untouched and runs no systemctl (got $rc)" \
  bash -c '[ "$1" -eq 0 ] && [ ! -e "$2/spawn-serve.service.d/50-feature-coverage-snapshot.conf" ] && [ ! -e "$2/spawn-core.service.d/50-feature-coverage-snapshot.conf" ] && [ ! -s "$3" ]' \
  _ "$rc" "$cp_units_dry" "$MOCK_EVENTS_LOG"

echo "test-self-upgrade: $passed passed, $failures failed"
[ "$failures" -eq 0 ]
