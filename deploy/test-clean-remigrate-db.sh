#!/usr/bin/env bash
# Offline and optional real PostgreSQL 16 tests for deploy/clean-remigrate-db.sh.
set -euo pipefail
umask 077
here=$(cd "$(dirname "$0")" && pwd)
script=${CLEAN_REMIGRATE_SCRIPT:-$here/clean-remigrate-db.sh}
work=$(mktemp -d)
pg_data=""
stop_pg() {
  if [ -n "$pg_data" ] && [ -f "$pg_data/postmaster.pid" ]; then
    pg_ctl -D "$pg_data" -m immediate stop >/dev/null 2>&1 || true
  fi
}
cleanup() {
  stop_pg
  rm -rf "$work"
}
trap cleanup EXIT

failed=0
ok() { printf 'ok: %s\n' "$*"; }
bad() { printf 'FAILED: %s\n' "$*" >&2; failed=1; }

file_mode() {
  stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"
}

mkdir -p "$work/bin"
cat > "$work/bin/systemctl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'systemctl' >> "$MOCK_LOG"
printf ' <%s>' "$@" >> "$MOCK_LOG"
printf '\n' >> "$MOCK_LOG"
state_dir=${MOCK_STATE_DIR:?}
mkdir -p "$state_dir"

case "${1-} ${2-}" in
  "--user start")
    if [ "${3-}" = "--wait" ] && [ "${4-}" = "${MOCK_FAIL_UNIT-}" ]; then
      exit 1
    fi
    exit 0
    ;;
  "--user stop")
    unit=${3-}
    if [ "$unit" != "${MOCK_STUCK_UNIT-}" ]; then
      printf 'inactive\n' > "$state_dir/$unit"
    fi
    exit 0
    ;;
  "--user is-active")
    unit=${3-}
    if [ -f "$state_dir/$unit" ]; then
      cat "$state_dir/$unit"
      exit 0
    fi
    case $unit in
      *.timer|*.path)
        printf '%s\n' "${MOCK_TRIGGER_ACTIVE-inactive}"
        ;;
      *)
        printf '%s\n' "${MOCK_ACTIVE-active}"
        ;;
    esac
    exit 0
    ;;
esac
EOF

cat > "$work/bin/spawn" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'spawn' >> "$MOCK_LOG"
printf ' <%s>' "$@" >> "$MOCK_LOG"
printf '\n' >> "$MOCK_LOG"
for arg in "$@"; do
  case $arg in
    *secret-value*|*://*)
      printf 'unexpected secret or URL in spawn argv\n' >&2
      exit 91
      ;;
  esac
done
if [ "${1-} ${2-}" = "node list" ]; then
  if [ "${MOCK_NODE_LIST_FAIL-0}" = "1" ]; then
    exit 1
  fi
  if [ "${MOCK_NODE_LIST_EMPTY-0}" = "1" ]; then
    exit 0
  fi
  printf '{"nodes":[],"credentials":"redacted"}\n'
  exit 0
fi
if [ "${1-}" = init-db ]; then
  exit "${MOCK_INIT_STATUS-0}"
fi
EOF

cat > "$work/bin/psql" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
file_mode() {
  stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"
}

if [ -n "${PGDATABASE-}" ]; then
  printf 'forbidden PGDATABASE env var set in psql invocation\n' >&2
  exit 90
fi

for arg in "$@"; do
  case $arg in
    *://*|*postgres:*|*secret-value*)
      printf 'forbidden credential or URI in psql argv\n' >&2
      exit 91
      ;;
  esac
done

svc_file=${PGSERVICEFILE-}
if [ "${PGSERVICE-}" != "remigrate" ] || [ -z "$svc_file" ] || [ ! -f "$svc_file" ] || [ -L "$svc_file" ]; then
  printf 'missing or unsafe PGSERVICEFILE/PGSERVICE\n' >&2
  exit 92
fi
if [ "$(file_mode "$svc_file")" != "600" ]; then
  printf 'PGSERVICEFILE mode must be 600\n' >&2
  exit 93
fi
if ! grep -Fqx '[remigrate]' "$svc_file" || \
   ! grep -Fqx 'dbname=clean_target' "$svc_file" || \
   ! grep -Fqx 'user=owner' "$svc_file" || \
   ! grep -Fqx 'host=127.0.0.1' "$svc_file" || \
   ! grep -Fq 'password=' "$svc_file"; then
  printf 'PGSERVICEFILE missing required libpq fields\n' >&2
  exit 94
fi

vars=" "
has_c=0
sql_c=""
while [ "$#" -gt 0 ]; do
  case $1 in
    -v)
      vars+="${2%%=*} "
      shift 2
      ;;
    --set=*)
      kv=${1#--set=}
      vars+="${kv%%=*} "
      shift
      ;;
    -c)
      has_c=1
      sql_c=${2-}
      shift 2
      ;;
    --command=*)
      has_c=1
      sql_c=${1#--command=}
      shift
      ;;
    *)
      shift
      ;;
  esac
done

if [ "$has_c" = "1" ]; then
  if [[ $sql_c == *":'"* ]]; then
    printf 'B2 regression: -c passed with psql variable interpolation\n' >&2
    exit 95
  fi
  sql=$sql_c
else
  sql=$(cat)
fi
if [ -z "$sql" ]; then
  printf 'empty SQL passed to psql\n' >&2
  exit 96
fi

# Verify every :'var' interpolation variable referenced in SQL was passed cleanly.
sq="'"
var_re=":${sq}([A-Za-z_][A-Za-z0-9_]*)${sq}"
rest=$sql
while [[ $rest =~ $var_re ]]; do
  varname=${BASH_REMATCH[1]}
  if [[ $vars != *" $varname "* ]]; then
    printf 'undefined psql variable :%s in SQL\n' "$varname" >&2
    exit 97
  fi
  rest=${rest#*"${sq}$varname${sq}"}
done

if [ "${MOCK_PSQL_FAIL-0}" = "1" ]; then
  printf 'psql <failed>\n' >> "$MOCK_LOG"
  printf 'psql: error: connection to server at "127.0.0.1", port 5432 failed\n' >&2
  exit 2
fi

case $sql in
  *current_database*current_schema*)
    printf 'psql <identity>\n' >> "$MOCK_LOG"
    printf '%s\n%s\n' "${MOCK_DB-clean_target}" "${MOCK_SCHEMA-public}"
    ;;
  *pg_class*pg_proc*pg_type*pg_extension*)
    printf 'psql <empty>\n' >> "$MOCK_LOG"
    count_file=$MOCK_STATE_DIR/empty_calls
    calls=0
    [ ! -f "$count_file" ] || calls=$(cat "$count_file")
    calls=$((calls + 1))
    printf '%s\n' "$calls" > "$count_file"
    if [ "$calls" -ge 2 ] && [ -n "${MOCK_OBJECTS_SECOND-}" ]; then
      printf '%s\n' "$MOCK_OBJECTS_SECOND"
    else
      printf '%s\n' "${MOCK_OBJECTS-0}"
    fi
    ;;
  *format_type*spawn_events*commit_xid*)
    printf 'psql <xid>\n' >> "$MOCK_LOG"
    printf '%s\n' "${MOCK_XID-xid8}"
    ;;
  *spawn_snapshots*prov_edges*to_regclass*)
    printf 'psql <tables>\n' >> "$MOCK_LOG"
    printf '%s\n' "${MOCK_TABLES-0}"
    ;;
  *spawn_snapshots_guard_row*pg_trigger*)
    printf 'psql <triggers>\n' >> "$MOCK_LOG"
    printf '%s\n' "${MOCK_TRIGGERS-0}"
    ;;
  *spawn_login_role*rolcanlogin*)
    printf 'psql <roles>\n' >> "$MOCK_LOG"
    printf '%s\n' "${MOCK_ROLES-0}"
    ;;
  *)
    printf 'unexpected SQL in mock psql\n' >&2
    exit 98
    ;;
esac
EOF

chmod 700 "$work/bin/systemctl" "$work/bin/spawn" "$work/bin/psql"

run_case() {
  local name=$1; shift
  local dir=$work/$name
  local -a vars=()
  while [ "$1" != -- ]; do vars+=("$1"); shift; done
  shift
  mkdir -p "$dir/state" "$dir/home"
  MOCK_LOG=$dir/calls
  MOCK_STATE_DIR=$dir/state
  export MOCK_LOG MOCK_STATE_DIR
  : > "$MOCK_LOG"
  set +e
  env -i PATH="$work/bin:/usr/bin:/bin" HOME="$dir/home" \
    MOCK_LOG="$MOCK_LOG" MOCK_STATE_DIR="$MOCK_STATE_DIR" \
    SPAWN_CLEAN_DATABASE_URL='postgres://owner:secret-value@127.0.0.1/clean_target' \
    SPAWN_CLEAN_EXPECT_DATABASE=clean_target \
    ${vars[@]+"${vars[@]}"} \
    bash "$script" "$@" > "$dir/out" 2>&1
  status=$?
  set -e
  OUT=$dir/out
  CALLS=$dir/calls
  DIR=$dir
  if grep -Rqs 'secret-value' "$dir/out" "$dir/calls" || grep -Rqs 'postgres://' "$dir/out" "$dir/calls"; then
    bad "$name leaked credential or database URI"
  fi
}

# 1. Dry-run & argument safety
run_case default-dry --
if [ "$status" = 0 ] && grep -q 'DRY RUN' "$OUT" && [ ! -s "$CALLS" ]; then
  ok "default dry run executes no command"
else
  bad "default dry run"
fi

run_case both -- --execute --dry-run
if [ "$status" != 0 ]; then ok "mutually exclusive modes rejected"; else bad "both modes accepted"; fi

run_case no-confirm -- --execute --archive-dir "$work/no-confirm/archive"
if [ "$status" != 0 ] && [ ! -s "$CALLS" ]; then ok "execute without confirmation stops before commands"; else bad "missing confirmation"; fi

run_case no-password-url 'SPAWN_CLEAN_DATABASE_URL=postgres://owner@127.0.0.1/clean_target' -- --execute --archive-dir "$work/no-password-url/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && [ ! -s "$CALLS" ] && grep -qi 'password' "$OUT"; then
  ok "owner URL without password fails before commands"
else
  bad "owner URL without password accepted"
fi

# 2. Backup & verify hard gates
run_case backup-fail MOCK_FAIL_UNIT=spawn-backup.service -- --execute --archive-dir "$work/backup-fail/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && grep -q spawn-backup.service "$OUT" && ! grep -q spawn-backup-bundle.service "$CALLS"; then
  ok "backup failure is a hard stop"
else
  bad "backup failure gate"
fi

run_case bundle-fail MOCK_FAIL_UNIT=spawn-backup-bundle.service -- --execute --archive-dir "$work/bundle-fail/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && grep -q spawn-backup-bundle.service "$OUT" && ! grep -q spawn-backup-verify.service "$CALLS"; then
  ok "bundle failure is a hard stop"
else
  bad "bundle failure gate"
fi

run_case verify-fail MOCK_FAIL_UNIT=spawn-backup-verify.service -- --execute --archive-dir "$work/verify-fail/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && [ ! -e "$DIR/archive/nodes.json" ] && ! grep -q '<stop>' "$CALLS"; then
  ok "object verification failure stops before archive/writers"
else
  bad "verify gate"
fi

# 3. Archive path & export failures
mkdir -p "$work/symlink-archive" "$work/existing-target"
ln -s "$work/existing-target" "$work/symlink-archive/archive"
run_case symlink-archive -- --execute --archive-dir "$work/symlink-archive/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && grep -q 'symbolic link' "$OUT" && ! grep -q '<node> <list>' "$CALLS" && ! grep -q '<stop>' "$CALLS"; then
  ok "symlinked archive path rejected before export"
else
  bad "symlink archive gate"
fi

mkdir -p "$work/dangling-symlink"
ln -s "$work/does-not-exist-anywhere" "$work/dangling-symlink/archive"
run_case dangling-symlink -- --execute --archive-dir "$work/dangling-symlink/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && grep -q 'symbolic link' "$OUT" && ! grep -q '<node> <list>' "$CALLS" && ! grep -q '<stop>' "$CALLS"; then
  ok "dangling symlink archive path rejected before export"
else
  bad "dangling symlink archive gate"
fi

mkdir -p "$work/nonempty-archive/archive"
printf 'existing\n' > "$work/nonempty-archive/archive/existing"
run_case nonempty-archive -- --execute --archive-dir "$work/nonempty-archive/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q '<node> <list>' "$CALLS" && ! grep -q '<stop>' "$CALLS"; then
  ok "existing archive path rejected before export"
else
  bad "existing archive gate"
fi

# 4. Pre-stop fast-fail checks (identity, schema, nonempty, psql error) MUST stop 0 writers
run_case wrong-identity MOCK_DB=other_db -- --execute --archive-dir "$work/wrong-identity/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q '<stop>' "$CALLS" && ! grep -q '<init-db>' "$CALLS"; then
  ok "target DB mismatch fails before stopping writers"
else
  bad "wrong-identity gate"
fi

run_case wrong-schema MOCK_SCHEMA=other_schema -- --execute --archive-dir "$work/wrong-schema/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q '<stop>' "$CALLS" && ! grep -q '<init-db>' "$CALLS"; then
  ok "target schema mismatch fails before stopping writers"
else
  bad "wrong-schema gate"
fi

run_case nonempty MOCK_OBJECTS=1 -- --execute --archive-dir "$work/nonempty/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q '<stop>' "$CALLS" && ! grep -q '<init-db>' "$CALLS"; then
  ok "nonempty target blocks before stopping writers and init-db"
else
  bad "nonempty target gate"
fi

run_case psql-error-no-leak MOCK_PSQL_FAIL=1 -- --execute --archive-dir "$work/psql-error-no-leak/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q '<stop>' "$CALLS" && ! grep -q '<init-db>' "$CALLS"; then
  ok "psql connection error fails before stopping writers without credential leaks"
else
  bad "psql error leak/gate"
fi

# 5. Stop triggers & writers enforcement
run_case trigger-stuck MOCK_TRIGGER_ACTIVE=active MOCK_STUCK_UNIT=spawn-deploy-on-landing.timer -- --execute --archive-dir "$work/trigger-stuck/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q '<init-db>' "$CALLS"; then
  ok "stuck auto-restart trigger blocks initialization"
else
  bad "trigger-stuck gate"
fi

run_case writer-active MOCK_STUCK_UNIT=spawn-loop.service -- --execute --archive-dir "$work/writer-active/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q '<init-db>' "$CALLS"; then
  ok "active writer blocks initialization"
else
  bad "active writer gate"
fi

run_case nonempty-after-stop MOCK_OBJECTS=0 MOCK_OBJECTS_SECOND=1 -- --execute --archive-dir "$work/nonempty-after-stop/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && grep -q '<stop>' "$CALLS" && ! grep -q '<init-db>' "$CALLS"; then
  ok "post-stop re-verification blocks non-empty target before init-db"
else
  bad "post-stop nonempty recheck"
fi

# 6. Post-init verification gates
run_case bad-xid MOCK_XID=xid -- --execute --archive-dir "$work/bad-xid/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q SUCCESS "$OUT"; then ok "wrong commit_xid type blocks success"; else bad "xid8 verification"; fi

run_case bad-tables MOCK_TABLES=1 -- --execute --archive-dir "$work/bad-tables/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q SUCCESS "$OUT"; then ok "missing D6/provenance table blocks success"; else bad "table verification"; fi

run_case bad-triggers MOCK_TRIGGERS=1 -- --execute --archive-dir "$work/bad-triggers/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q SUCCESS "$OUT"; then ok "missing guard trigger blocks success"; else bad "trigger verification"; fi

run_case bad-roles MOCK_ROLES=1 -- --execute --archive-dir "$work/bad-roles/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q SUCCESS "$OUT"; then ok "missing restricted login blocks success"; else bad "role verification"; fi

# 7. Exact end-to-end ordering on success (triggers active -> stopped alongside 5 writers)
run_case success MOCK_TRIGGER_ACTIVE=active -- --execute --archive-dir "$work/success/archive" --confirm-clean-remigrate=clean_target
expected_calls=$(cat <<'EOF'
systemctl <--user> <start> <--wait> <spawn-backup.service>
systemctl <--user> <start> <--wait> <spawn-backup-bundle.service>
systemctl <--user> <start> <--wait> <spawn-backup-verify.service>
spawn <node> <list> <--json>
psql <identity>
psql <empty>
systemctl <--user> <is-active> <spawn-deploy-on-landing.timer>
systemctl <--user> <stop> <spawn-deploy-on-landing.timer>
systemctl <--user> <is-active> <spawn-deploy-on-landing.timer>
systemctl <--user> <is-active> <spawn-node-gateway.path>
systemctl <--user> <stop> <spawn-node-gateway.path>
systemctl <--user> <is-active> <spawn-node-gateway.path>
systemctl <--user> <stop> <spawn-loop.service>
systemctl <--user> <stop> <spawn-integrate.service>
systemctl <--user> <stop> <spawn-serve.service>
systemctl <--user> <stop> <spawn-core.service>
systemctl <--user> <stop> <spawn-node-gateway.service>
systemctl <--user> <is-active> <spawn-loop.service>
systemctl <--user> <is-active> <spawn-integrate.service>
systemctl <--user> <is-active> <spawn-serve.service>
systemctl <--user> <is-active> <spawn-core.service>
systemctl <--user> <is-active> <spawn-node-gateway.service>
psql <identity>
psql <empty>
spawn <init-db>
psql <xid>
psql <tables>
psql <triggers>
psql <roles>
EOF
)
if [ "$status" = 0 ] && grep -q SUCCESS "$OUT" && [ -s "$DIR/archive/nodes.json" ] && \
   [ "$(file_mode "$DIR/archive/nodes.json")" = 600 ] && \
   [ "$(cat "$CALLS")" = "$expected_calls" ]; then
  ok "verified exact ordering, PGSERVICEFILE 0600, trigger+writer stop, and schema checks"
else
  bad "successful execution order (status $status)"
  printf '%s\n' "--- OUT ---" "$(cat "$OUT")" "--- ACTUAL CALLS ---" "$(cat "$CALLS")" >&2
fi

# 8. Optional real PostgreSQL 16 + real spawn binary E2E check
run_real_pg16_e2e() {
  local real_bin=${SPAWN_REAL_BIN-}
  local pg_root=$work/pg
  local real_work=$work/real-e2e
  local pg_pw pg_port uri_enum uri_clean pg_log

  command -v initdb >/dev/null 2>&1 || { bad "real PG16 requested but initdb not found"; return; }
  command -v pg_ctl >/dev/null 2>&1 || { bad "real PG16 requested but pg_ctl not found"; return; }
  command -v psql >/dev/null 2>&1 || { bad "real PG16 requested but psql not found"; return; }
  [ -x "$real_bin" ] || { bad "real PG16 requested but SPAWN_REAL_BIN='$real_bin' is not executable"; return; }

  pg_pw=$(od -An -tx1 -N12 /dev/urandom | tr -d ' \n')
  pg_data=$pg_root/data
  pg_log=$pg_root/server.log
  mkdir -p "$pg_root/sock" "$real_work/bin" "$real_work/state" "$real_work/home"
  chmod 700 "$pg_root" "$pg_root/sock"

  initdb -D "$pg_data" -U pgowner -A trust --encoding=UTF8 --locale=C >/dev/null 2>&1

  # Pick an unused local TCP port safely without hardcoded collisions.
  if command -v python3 >/dev/null 2>&1; then
    pg_port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
  else
    pg_port=65489
  fi

  cat >> "$pg_data/postgresql.conf" <<EOF
listen_addresses = '127.0.0.1'
port = $pg_port
unix_socket_directories = '$pg_root/sock'
log_connections = off
log_disconnections = off
EOF
  cat > "$pg_data/pg_hba.conf" <<'EOF'
local   all   all                 trust
host    all   all   127.0.0.1/32  scram-sha-256
EOF

  pg_ctl -D "$pg_data" -l "$pg_log" -w start >/dev/null
  psql -XAtq -h "$pg_root/sock" -p "$pg_port" -U pgowner -d postgres -v ON_ERROR_STOP=1 -v pw="$pg_pw" <<'SQL'
ALTER ROLE pgowner WITH SUPERUSER PASSWORD :'pw';
CREATE DATABASE enum_only OWNER pgowner;
CREATE DATABASE real_clean OWNER pgowner;
SQL
  psql -XAtq -h "$pg_root/sock" -p "$pg_port" -U pgowner -d enum_only -v ON_ERROR_STOP=1 <<'SQL'
CREATE TYPE cutover_enum AS ENUM ('alpha', 'beta');
CREATE DOMAIN cutover_dom AS integer CHECK (VALUE > 0);
SQL

  # Symlink mock systemctl (from $work/bin) + real spawn wrapper (init-db uses SPAWN_REAL_BIN).
  ln -s "$work/bin/systemctl" "$real_work/bin/systemctl"
  cat > "$real_work/bin/spawn" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'spawn' >> "$MOCK_LOG"
printf ' <%s>' "$@" >> "$MOCK_LOG"
printf '\n' >> "$MOCK_LOG"
if [ "${1-} ${2-}" = "node list" ]; then
  printf '{"nodes":[],"credentials":"redacted"}\n'
  exit 0
fi
exec "${SPAWN_REAL_BIN:?}" "$@"
EOF
  chmod 700 "$real_work/bin/spawn"

  real_psql=$(command -v psql)
  uri_enum="postgres://pgowner:$pg_pw@127.0.0.1:$pg_port/enum_only?sslmode=disable"
  uri_clean="postgres://pgowner:$pg_pw@127.0.0.1:$pg_port/real_clean?sslmode=disable"

  # R1: enum + domain-only schema must be rejected BEFORE stopping writers.
  mkdir -p "$real_work/r1/state" "$real_work/r1/home"
  : > "$real_work/r1/calls"
  set +e
  env -i PATH="$real_work/bin:/usr/bin:/bin" HOME="$real_work/r1/home" \
    MOCK_LOG="$real_work/r1/calls" MOCK_STATE_DIR="$real_work/r1/state" \
    SPAWN_REAL_BIN="$real_bin" PSQL="$real_psql" MOCK_TRIGGER_ACTIVE=active \
    SPAWN_CLEAN_DATABASE_URL="$uri_enum" SPAWN_CLEAN_EXPECT_DATABASE=enum_only \
    bash "$script" --execute --archive-dir "$real_work/r1/archive" --confirm-clean-remigrate=enum_only > "$real_work/r1/out" 2>&1
  r1_rc=$?
  set -e
  if [ "$r1_rc" != 0 ] && ! grep -q '<stop>' "$real_work/r1/calls" && ! grep -q '<init-db>' "$real_work/r1/calls"; then
    ok "real PG16: enum/domain-only schema refused before stopping writers"
  else
    bad "real PG16 R1 failed (rc=$r1_rc)"
  fi

  # R2: real empty PG16 database succeeds end-to-end with real psql + real spawn init-db.
  mkdir -p "$real_work/r2/state" "$real_work/r2/home"
  : > "$real_work/r2/calls"
  set +e
  env -i PATH="$real_work/bin:/usr/bin:/bin" HOME="$real_work/r2/home" \
    MOCK_LOG="$real_work/r2/calls" MOCK_STATE_DIR="$real_work/r2/state" \
    SPAWN_REAL_BIN="$real_bin" PSQL="$real_psql" MOCK_TRIGGER_ACTIVE=active \
    SPAWN_CLEAN_DATABASE_URL="$uri_clean" SPAWN_CLEAN_EXPECT_DATABASE=real_clean \
    bash "$script" --execute --archive-dir "$real_work/r2/archive" --confirm-clean-remigrate=real_clean > "$real_work/r2/out" 2>&1
  r2_rc=$?
  set -e
  if [ "$r2_rc" = 0 ] && grep -q SUCCESS "$real_work/r2/out" && \
     [ "$(grep -c '<stop>' "$real_work/r2/calls")" = 7 ] && \
     [ -s "$real_work/r2/archive/nodes.json" ] && \
     [ "$(file_mode "$real_work/r2/archive/nodes.json")" = 600 ]; then
    ok "real PG16: empty target init-db, xid8, 7 tables, 10 triggers, and 4 roles succeeded"
  else
    bad "real PG16 R2 failed (rc=$r2_rc): $(cat "$real_work/r2/out")"
  fi

  # R3: repeated run against the same initialized PG16 DB must fail closed before stop & init-db.
  mkdir -p "$real_work/r3/state" "$real_work/r3/home"
  : > "$real_work/r3/calls"
  set +e
  env -i PATH="$real_work/bin:/usr/bin:/bin" HOME="$real_work/r3/home" \
    MOCK_LOG="$real_work/r3/calls" MOCK_STATE_DIR="$real_work/r3/state" \
    SPAWN_REAL_BIN="$real_bin" PSQL="$real_psql" MOCK_TRIGGER_ACTIVE=active \
    SPAWN_CLEAN_DATABASE_URL="$uri_clean" SPAWN_CLEAN_EXPECT_DATABASE=real_clean \
    bash "$script" --execute --archive-dir "$real_work/r3/archive" --confirm-clean-remigrate=real_clean > "$real_work/r3/out" 2>&1
  r3_rc=$?
  set -e
  if [ "$r3_rc" != 0 ] && ! grep -q '<stop>' "$real_work/r3/calls" && ! grep -q '<init-db>' "$real_work/r3/calls"; then
    ok "real PG16: repeat execute on initialized database refused fail-closed"
  else
    bad "real PG16 R3 failed (rc=$r3_rc)"
  fi

  # R4: zero leakage of test password or URI in script outputs, call logs, or PG16 server log.
  if grep -RqsF "$pg_pw" "$real_work" "$pg_log" || grep -RqsF 'postgres://' "$real_work" "$pg_log"; then
    bad "real PG16 leaked test credential or database URI into logs/output"
  else
    ok "real PG16: zero password or URI occurrences across stdout, stderr, calls, and PG16 log"
  fi
}

if [ "${SPAWN_TEST_REAL_PG-0}" = "1" ] || { [ -n "${SPAWN_REAL_BIN-}" ] && command -v initdb >/dev/null 2>&1 && command -v pg_ctl >/dev/null 2>&1; }; then
  run_real_pg16_e2e
fi

[ "$failed" = 0 ] || exit 1
printf 'all clean-remigrate-db tests passed\n'
