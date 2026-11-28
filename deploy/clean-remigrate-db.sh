#!/usr/bin/env bash
# Safely initialize a new, EMPTY Spawn PostgreSQL database after verified backups.
# No argument is needed for the offline, non-destructive dry run. Execution needs:
#   SPAWN_CLEAN_DATABASE_URL      owner URL with password, read from the environment only
#   SPAWN_CLEAN_EXPECT_DATABASE  exact database name expected at that URL
#   --archive-dir ABSOLUTE_DIR   non-existent path for mode-0600 node summary export
#   --execute --confirm-clean-remigrate=EXPECTED_DATABASE
# Connection credentials reach psql only via a 0600 temp PGSERVICEFILE, never argv/logs.
set -euo pipefail
set +x
umask 077
PROG=clean-remigrate-db
MODE=dry-run
MODE_SET=
ARCHIVE_DIR=
CONFIRM=
SCHEMA=public
SYSTEMCTL=${SYSTEMCTL:-systemctl}
SPAWN_BIN=${SPAWN_BIN:-spawn}
PSQL=${PSQL:-psql}
TRIGGER_UNITS=(
  spawn-deploy-on-landing.timer
  spawn-node-gateway.path
)
WRITER_UNITS=(
  spawn-loop.service spawn-integrate.service spawn-serve.service
  spawn-core.service spawn-node-gateway.service
)
PG_TEMP_DIR=""
PG_SERVICE_FILE=""

cleanup() {
  if [ -n "$PG_TEMP_DIR" ] && [ -d "$PG_TEMP_DIR" ]; then
    rm -rf "$PG_TEMP_DIR"
  fi
}
trap cleanup EXIT

usage() {
  cat <<'EOF'
usage: deploy/clean-remigrate-db.sh [--dry-run] [--archive-dir ABSOLUTE_DIR]
       deploy/clean-remigrate-db.sh --execute --archive-dir ABSOLUTE_DIR \
         --confirm-clean-remigrate=EXPECTED_DATABASE [--schema NAME]

Environment required only for --execute:
  SPAWN_CLEAN_DATABASE_URL (owner URI with non-empty password),
  SPAWN_CLEAN_EXPECT_DATABASE (exact target database name).
Set SPAWN_DATABASE_URL in the environment to the live DB so `spawn node list --json`
exports live node metadata before initialization switches to SPAWN_CLEAN_DATABASE_URL.
The target schema must already exist, be selected by the target URL, and be empty.
EOF
}

die() { printf '%s: error: %s\n' "$PROG" "$*" >&2; exit 1; }
log() { printf '%s: %s\n' "$PROG" "$*"; }
need_tools() {
  local tool
  for tool in "$@"; do command -v "$tool" >/dev/null 2>&1 || die "required tool not found: $tool"; done
}

urldecode() {
  local encoded=$1 out="" ch hex byte
  while [ -n "$encoded" ]; do
    ch=${encoded:0:1}
    if [ "$ch" = "%" ]; then
      [ "${#encoded}" -ge 3 ] || die "malformed percent-encoding in target URL"
      hex=${encoded:1:2}
      [[ $hex =~ ^[0-9A-Fa-f]{2}$ ]] || die "malformed percent-encoding in target URL"
      case $hex in
        00|0[AaDd]) die "control characters are not allowed in target URL" ;;
      esac
      printf -v byte '%b' "\\x$hex"
      out+=$byte
      encoded=${encoded:3}
    else
      out+=$ch
      encoded=${encoded:1}
    fi
  done
  printf '%s' "$out"
}

write_pg_kv() {
  local LC_ALL=C
  local key=$1 val=$2
  [[ $key =~ ^[a-z_][a-z0-9_]*$ ]] || die "unsupported libpq parameter in target URL: $key"
  [[ $val != *$'\n'* && $val != *$'\r'* ]] || die "newline not allowed in target URL parameter: $key"
  if [ -n "$val" ]; then
    case ${val: -1} in
      ' '|$'\t'|$'\v'|$'\f')
        die "trailing whitespace is not supported in target URL parameter: $key"
        ;;
    esac
  fi
  if [ $(( ${#key} + 1 + ${#val} )) -gt 1021 ]; then
    die "target URL parameter exceeds libpq service-file line limit: $key"
  fi
  printf '%s=%s\n' "$key" "$val" >> "$PG_SERVICE_FILE"
}

write_pg_service_file() {
  local raw_uri=$1 rest base query authority path userinfo hostport
  local raw_user="" raw_pw="" raw_host="" pg_user="" pg_pw="" pg_host="" pg_port="" pg_db=""
  local pair raw_k raw_v k v
  local -a extra_kvs=()
  local -a query_pairs=()

  [[ $raw_uri != *$'\n'* && $raw_uri != *$'\r'* ]] || die "newlines are not allowed in SPAWN_CLEAN_DATABASE_URL"
  case $raw_uri in
    postgres://*) rest=${raw_uri#postgres://} ;;
    postgresql://*) rest=${raw_uri#postgresql://} ;;
    *) die "SPAWN_CLEAN_DATABASE_URL must be a postgres:// or postgresql:// URI" ;;
  esac

  base=${rest%%\?*}
  query=""
  [ "$rest" = "$base" ] || query=${rest#*\?}

  authority=${base%%/*}
  path=""
  [ "$base" = "$authority" ] || path=${base#*/}
  [[ $path != */* ]] || die "target URL path must name a single database"

  if [[ $authority == *@* ]]; then
    userinfo=${authority%@*}
    hostport=${authority##*@}
    if [[ $userinfo == *:* ]]; then
      raw_user=${userinfo%%:*}
      raw_pw=${userinfo#*:}
    else
      raw_user=$userinfo
      raw_pw=""
    fi
    pg_user=$(urldecode "$raw_user")
    pg_pw=$(urldecode "$raw_pw")
  else
    hostport=$authority
  fi

  if [ -n "$hostport" ]; then
    if [[ $hostport == \[* ]]; then
      [[ $hostport =~ ^\[([^]]+)\](:([0-9]+))?$ ]] || die "invalid IPv6 host/port in target URL"
      pg_host=$(urldecode "${BASH_REMATCH[1]}")
      pg_port=${BASH_REMATCH[3]-}
    elif [[ $hostport == *:* ]]; then
      raw_host=${hostport%%:*}
      pg_port=${hostport#*:}
      [[ $pg_port =~ ^[0-9]+$ ]] || die "invalid port in target URL"
      pg_host=$(urldecode "$raw_host")
    else
      pg_host=$(urldecode "$hostport")
    fi
  fi

  if [ -n "$path" ]; then
    pg_db=$(urldecode "$path")
  fi

  if [ -n "$query" ]; then
    IFS='&' read -r -a query_pairs <<< "$query"
    for pair in ${query_pairs[@]+"${query_pairs[@]}"}; do
      [ -n "$pair" ] || continue
      if [[ $pair == *=* ]]; then
        raw_k=${pair%%=*}
        raw_v=${pair#*=}
        k=$(urldecode "$raw_k")
        v=$(urldecode "$raw_v")
      else
        k=$(urldecode "$pair")
        v=""
      fi
      case $k in
        host|hostaddr|port|dbname|user|password|service|servicefile)
          die "target URL query must not override $k"
          ;;
        sslmode|sslrootcert|sslcert|sslkey|sslpassword|channel_binding|connect_timeout|options|target_session_attrs|application_name|gssencmode|krbsrvname|requiressl)
          extra_kvs+=("$k=$v")
          ;;
        *)
          die "unsupported libpq query parameter in target URL: $k"
          ;;
      esac
    done
  fi

  [ -n "$pg_host" ] || die "target URL must specify a host"
  [[ $pg_host != *,* ]] || die "target URL must specify a single host"
  [ -n "$pg_db" ] || die "target URL must name the target database"
  [ -n "$pg_user" ] || die "target URL must specify an owner username"
  [ -n "$pg_pw" ] || die "target URL must include a non-empty owner password"

  PG_TEMP_DIR=$(mktemp -d)
  chmod 700 "$PG_TEMP_DIR"
  PG_SERVICE_FILE=$PG_TEMP_DIR/pg_service.conf
  : > "$PG_SERVICE_FILE"
  chmod 600 "$PG_SERVICE_FILE"
  printf '[remigrate]\n' >> "$PG_SERVICE_FILE"
  write_pg_kv dbname "$pg_db"
  write_pg_kv user "$pg_user"
  write_pg_kv password "$pg_pw"
  write_pg_kv host "$pg_host"
  [ -z "$pg_port" ] || write_pg_kv port "$pg_port"
  for pair in ${extra_kvs[@]+"${extra_kvs[@]}"}; do
    write_pg_kv "${pair%%=*}" "${pair#*=}"
  done
  chmod 600 "$PG_SERVICE_FILE"
}

while [ "$#" -gt 0 ]; do
  case $1 in
    --dry-run) [ -z "$MODE_SET" ] || die "execution modes must be unique and mutually exclusive"; MODE=dry-run; MODE_SET=1 ;;
    --execute) [ -z "$MODE_SET" ] || die "execution modes must be unique and mutually exclusive"; MODE=execute; MODE_SET=1 ;;
    --archive-dir)
      [ "$#" -ge 2 ] || die "--archive-dir needs a value"
      [ -z "$ARCHIVE_DIR" ] || die "duplicate --archive-dir"
      ARCHIVE_DIR=$2; shift ;;
    --archive-dir=*) [ -z "$ARCHIVE_DIR" ] || die "duplicate --archive-dir"; ARCHIVE_DIR=${1#*=} ;;
    --confirm-clean-remigrate=*) [ -z "$CONFIRM" ] || die "duplicate confirmation"; CONFIRM=${1#*=} ;;
    --schema)
      [ "$#" -ge 2 ] || die "--schema needs a value"
      SCHEMA=$2; shift ;;
    --schema=*) SCHEMA=${1#*=} ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; die "unknown argument: $1" ;;
  esac
  shift
done

[[ $SCHEMA =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || die "invalid schema identifier"
if [ -n "$ARCHIVE_DIR" ]; then
  [[ $ARCHIVE_DIR = /* ]] || die "--archive-dir must be absolute"
  [[ $ARCHIVE_DIR != / && $ARCHIVE_DIR != *$'\n'* && $ARCHIVE_DIR != *$'\r'* ]] || die "unsafe archive directory"
fi
[ -z "$CONFIRM" ] || [ "$MODE" = execute ] || die "confirmation is valid only with --execute"

if [ "$MODE" = dry-run ]; then
  cat <<EOF
$PROG: DRY RUN; no command is executed and no file is created.
$PROG: 1. Complete spawn-backup.service and spawn-backup-bundle.service.
$PROG: 2. Complete spawn-backup-verify.service (latest encrypted dump and transcript bundle).
$PROG: 3. Prove target DB/schema identity and empty target BEFORE archiving or stopping any writer (fast-fail).
$PROG: 4. Archive a mode-0600 'spawn node list --json' summary under a new archive directory.
$PROG: 5. Stop auto-restart triggers (${TRIGGER_UNITS[*]}) if active, and stop/prove inactive: ${WRITER_UNITS[*]}.
$PROG: 6. Re-verify empty target schema, then run spawn init-db via SPAWN_DATABASE_URL.
$PROG: 7. Verify xid8, D6/provenance tables, triggers, and restricted login roles via 0600 PGSERVICEFILE.
$PROG: Execution additionally requires a non-existent absolute archive directory and database-bound confirmation.
EOF
  exit 0
fi

[ -n "$ARCHIVE_DIR" ] || die "--execute requires --archive-dir"
TARGET_URL=${SPAWN_CLEAN_DATABASE_URL-}
EXPECTED_DB=${SPAWN_CLEAN_EXPECT_DATABASE-}
[ -n "$TARGET_URL" ] || die "SPAWN_CLEAN_DATABASE_URL is not set"
[ -n "$EXPECTED_DB" ] || die "SPAWN_CLEAN_EXPECT_DATABASE is not set"
[[ $EXPECTED_DB =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || die "invalid expected database identifier"
[ "$CONFIRM" = "$EXPECTED_DB" ] || die "confirmation must exactly name SPAWN_CLEAN_EXPECT_DATABASE"
need_tools "$SYSTEMCTL" "$SPAWN_BIN" "$PSQL" mkdir chmod mktemp dirname rm mv env

write_pg_service_file "$TARGET_URL"

psql_scalar() {
  env -u PGDATABASE -u PGHOST -u PGHOSTADDR -u PGPORT -u PGUSER \
      -u PGPASSWORD -u PGPASSFILE -u PGOPTIONS -u PGSSLMODE \
      -u PGSSLROOTCERT -u PGSSLCERT -u PGSSLKEY -u PGCHANNELBINDING \
      -u PGTARGETSESSIONATTRS \
      PGSERVICEFILE="$PG_SERVICE_FILE" PGSERVICE=remigrate PGCONNECT_TIMEOUT=10 \
      "$PSQL" -XAtq -w -v ON_ERROR_STOP=1 -v schema="$SCHEMA"
}

verify_target_empty() {
  local actual objects
  actual=$(psql_scalar <<'SQL'
select current_database() || E'\n' || current_schema();
SQL
  ) || die "cannot identify target"
  [ "$actual" = "$EXPECTED_DB"$'\n'"$SCHEMA" ] || die "target identity/schema mismatch; refusing initialization"

  objects=$(psql_scalar <<'SQL'
select
  (select count(*) from pg_class c join pg_namespace n on n.oid = c.relnamespace where n.nspname = :'schema') +
  (select count(*) from pg_proc p join pg_namespace n on n.oid = p.pronamespace where n.nspname = :'schema') +
  (select count(*) from pg_type t join pg_namespace n on n.oid = t.typnamespace
   where n.nspname = :'schema'
     and t.typtype in ('c', 'd', 'e', 'r', 'm')
     and (t.typrelid = 0 or exists (select 1 from pg_class c where c.oid = t.typrelid and c.relkind = 'c'))) +
  (select count(*) from pg_extension e join pg_namespace n on n.oid = e.extnamespace where n.nspname = :'schema');
SQL
  ) || die "cannot prove target is empty"
  [ "$objects" = 0 ] || die "target schema is not empty; refusing initialization"
}

start_unit() {
  log "starting $1"
  "$SYSTEMCTL" --user start --wait "$1" || die "$1 did not complete successfully; refusing remigration"
}

# Note: backup units snapshot the live DB before writers stop; any writes occurring
# between backup completion and writer shutdown belong only to the old database.
# The verify unit fails unless the newest encrypted database object restores AND
# the newest transcript/source bundle opens. No previous/stale backup is accepted.
start_unit spawn-backup.service
start_unit spawn-backup-bundle.service
start_unit spawn-backup-verify.service

log "pre-flight verifying empty target before archiving or stopping writers"
verify_target_empty

ARCHIVE_PARENT=$(dirname "$ARCHIVE_DIR")
[ ! -L "$ARCHIVE_PARENT" ] || die "archive parent directory must not be a symbolic link"
[ ! -L "$ARCHIVE_DIR" ] || die "archive directory must not be a symbolic link"
[ ! -e "$ARCHIVE_DIR" ] || die "archive directory already exists; provide a new path"
mkdir -p "$ARCHIVE_DIR"
chmod 700 "$ARCHIVE_DIR"
log "archiving node metadata without credentials"
tmp_nodes=$ARCHIVE_DIR/.nodes.json.tmp
if ! "$SPAWN_BIN" node list --json > "$tmp_nodes"; then
  rm -f "$tmp_nodes"
  die "node summary export failed"
fi
if [ ! -s "$tmp_nodes" ]; then
  rm -f "$tmp_nodes"
  die "node summary export is empty"
fi
chmod 600 "$tmp_nodes"
mv -f "$tmp_nodes" "$ARCHIVE_DIR/nodes.json"

for unit in "${TRIGGER_UNITS[@]}"; do
  state=$("$SYSTEMCTL" --user is-active "$unit" 2>/dev/null || true)
  case $state in
    active|activating|reloading)
      log "stopping trigger $unit"
      "$SYSTEMCTL" --user stop "$unit" || die "could not stop $unit; leaving target untouched"
      post_state=$("$SYSTEMCTL" --user is-active "$unit" 2>/dev/null || true)
      case $post_state in
        inactive|failed|"") ;;
        *) die "$unit is '$post_state' after stop; refusing target initialization" ;;
      esac
      ;;
    inactive|failed|unknown|"") ;;
    *) die "unexpected state '$state' for trigger $unit" ;;
  esac
done

for unit in "${WRITER_UNITS[@]}"; do
  log "stopping writer $unit"
  "$SYSTEMCTL" --user stop "$unit" || die "could not stop $unit; leaving target untouched"
done
for unit in "${WRITER_UNITS[@]}"; do
  state=$("$SYSTEMCTL" --user is-active "$unit" 2>/dev/null || true)
  [ "$state" = inactive ] || die "$unit is '$state', not inactive; refusing target initialization"
done

# Re-check emptiness immediately before init-db in case another actor touched the target.
verify_target_empty

log "initializing clean target"
SPAWN_DATABASE_URL=$TARGET_URL "$SPAWN_BIN" init-db || die "spawn init-db failed"

column_type=$(psql_scalar <<'SQL'
select format_type(a.atttypid, a.atttypmod)
from pg_attribute a
join pg_class c on c.oid = a.attrelid
join pg_namespace n on n.oid = c.relnamespace
where n.nspname = :'schema'
  and c.relname = 'spawn_events'
  and a.attname = 'commit_xid'
  and a.attnum > 0;
SQL
) || die "cannot inspect commit_xid"
[ "$column_type" = xid8 ] || die "spawn_events.commit_xid is not xid8"

missing_tables=$(psql_scalar <<'SQL'
with wanted(name) as (
  values
    ('spawn_events'),
    ('spawn_snapshots'),
    ('spawn_refs'),
    ('spawn_ref_log'),
    ('prov_changes'),
    ('prov_entries'),
    ('prov_edges')
)
select count(*)
from wanted w
where to_regclass(format('%I.%I', :'schema', w.name)) is null;
SQL
) || die "table verification failed"
[ "$missing_tables" = 0 ] || die "D6/provenance table verification failed"

missing_triggers=$(psql_scalar <<'SQL'
with wanted(name) as (
  values
    ('prov_changes_unchanged'),
    ('prov_changes_untruncated'),
    ('prov_entries_unchanged'),
    ('prov_entries_untruncated'),
    ('prov_edges_unchanged'),
    ('prov_edges_untruncated'),
    ('spawn_snapshots_guard_row'),
    ('spawn_snapshots_guard_truncate'),
    ('spawn_ref_log_unchanged'),
    ('spawn_ref_log_untruncated')
)
select count(*)
from wanted w
where not exists (
  select 1
  from pg_trigger t
  join pg_class c on c.oid = t.tgrelid
  join pg_namespace n on n.oid = c.relnamespace
  where n.nspname = :'schema'
    and t.tgname = w.name
    and not t.tgisinternal
);
SQL
) || die "trigger verification failed"
[ "$missing_triggers" = 0 ] || die "D6/provenance trigger verification failed"

missing_roles=$(psql_scalar <<'SQL'
with wanted(base) as (
  values
    ('spawn_supervisor'),
    ('spawn_reader'),
    ('spawn_human'),
    ('spawn_node_gateway')
)
select count(*)
from wanted w
where not exists (
  select 1
  from pg_roles r
  where r.rolname = spawn_login_role(w.base)
    and r.rolcanlogin
);
SQL
) || die "restricted role verification failed"
[ "$missing_roles" = 0 ] || die "restricted login role verification failed"

log "SUCCESS: clean target initialized and verified; writers and restart triggers remain STOPPED"
log "Switch unit credentials/configuration to the target through the normal secret store, then restore writers and triggers deliberately."
