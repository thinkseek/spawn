#!/usr/bin/env bash
# Safely initialize a new, EMPTY Spawn PostgreSQL database after verified backups.
# No argument is needed for the offline, non-destructive dry run. Execution needs:
#   SPAWN_CLEAN_DATABASE_URL      owner URL, read from the environment only
#   SPAWN_CLEAN_EXPECT_DATABASE  exact database name expected at that URL
#   --archive-dir ABSOLUTE_DIR   new/empty directory for the node summary
#   --execute --confirm-clean-remigrate=EXPECTED_DATABASE
# The target URL is passed through PGDATABASE/SPAWN_DATABASE_URL, never argv/logs.
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
WRITER_UNITS=(
  spawn-loop.service spawn-integrate.service spawn-serve.service
  spawn-core.service spawn-node-gateway.service
)

usage() {
  cat <<'EOF'
usage: deploy/clean-remigrate-db.sh [--dry-run] [--archive-dir ABSOLUTE_DIR]
       deploy/clean-remigrate-db.sh --execute --archive-dir ABSOLUTE_DIR \
         --confirm-clean-remigrate=EXPECTED_DATABASE [--schema NAME]

Environment required only for --execute:
  SPAWN_CLEAN_DATABASE_URL, SPAWN_CLEAN_EXPECT_DATABASE
The target schema must already exist, be selected by the target URL, and be empty.
EOF
}

die() { printf '%s: error: %s\n' "$PROG" "$*" >&2; exit 1; }
log() { printf '%s: %s\n' "$PROG" "$*"; }
need_tools() {
  local tool
  for tool in "$@"; do command -v "$tool" >/dev/null 2>&1 || die "required tool not found: $tool"; done
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
$PROG: 3. Archive a mode-0600 'spawn node list --json' summary.
$PROG: 4. Stop and prove inactive: ${WRITER_UNITS[*]}.
$PROG: 5. Require the selected '$SCHEMA' schema in the expected target database to be empty.
$PROG: 6. Run spawn init-db against the target through its environment.
$PROG: 7. Verify xid8, D6/provenance tables, triggers, and restricted login roles.
$PROG: Execution additionally requires an absolute archive directory and database-bound confirmation.
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
need_tools "$SYSTEMCTL" "$SPAWN_BIN" "$PSQL" mkdir chmod

start_unit() {
  log "starting $1"
  "$SYSTEMCTL" --user start --wait "$1" || die "$1 did not complete successfully; refusing remigration"
}

# The verify unit fails unless the newest encrypted database object restores AND
# the newest transcript/source bundle opens. No previous/stale backup is accepted.
start_unit spawn-backup.service
start_unit spawn-backup-bundle.service
start_unit spawn-backup-verify.service

[ ! -L "$ARCHIVE_DIR" ] || die "archive directory must not be a symbolic link"
[ ! -e "$ARCHIVE_DIR" ] || [ -d "$ARCHIVE_DIR" ] || die "archive path exists and is not a directory"
if [ -d "$ARCHIVE_DIR" ] && [ -n "$(find "$ARCHIVE_DIR" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
  die "archive directory is not empty"
fi
mkdir -p "$ARCHIVE_DIR"
chmod 700 "$ARCHIVE_DIR"
log "archiving node metadata without credentials"
"$SPAWN_BIN" node list --json > "$ARCHIVE_DIR/nodes.json" || die "node summary export failed"
[ -s "$ARCHIVE_DIR/nodes.json" ] || die "node summary export is empty"
chmod 600 "$ARCHIVE_DIR/nodes.json"

for unit in "${WRITER_UNITS[@]}"; do
  log "stopping writer $unit"
  "$SYSTEMCTL" --user stop "$unit" || die "could not stop $unit; leaving target untouched"
done
for unit in "${WRITER_UNITS[@]}"; do
  state=$("$SYSTEMCTL" --user is-active "$unit" 2>/dev/null || true)
  [ "$state" = inactive ] || die "$unit is '$state', not inactive; refusing target initialization"
done

export PGDATABASE=$TARGET_URL
export PGCONNECT_TIMEOUT=10
psql_scalar() {
  "$PSQL" -XAtq -w -v ON_ERROR_STOP=1 --set=schema="$SCHEMA" "$@"
}

actual=$(psql_scalar -c "select current_database() || E'\\n' || current_schema()") || die "cannot identify target"
[ "$actual" = "$EXPECTED_DB"$'\n'"$SCHEMA" ] || die "target identity/schema mismatch; refusing initialization"
objects=$(psql_scalar -c "select (select count(*) from pg_class c join pg_namespace n on n.oid=c.relnamespace where n.nspname=:'schema') + (select count(*) from pg_proc p join pg_namespace n on n.oid=p.pronamespace where n.nspname=:'schema')") || die "cannot prove target is empty"
[ "$objects" = 0 ] || die "target schema is not empty; refusing initialization"

log "initializing clean target"
SPAWN_DATABASE_URL=$TARGET_URL "$SPAWN_BIN" init-db || die "spawn init-db failed"

column_type=$(psql_scalar -c "select format_type(a.atttypid,a.atttypmod) from pg_attribute a join pg_class c on c.oid=a.attrelid join pg_namespace n on n.oid=c.relnamespace where n.nspname=:'schema' and c.relname='spawn_events' and a.attname='commit_xid' and a.attnum>0") || die "cannot inspect commit_xid"
[ "$column_type" = xid8 ] || die "spawn_events.commit_xid is not xid8"
missing_tables=$(psql_scalar -c "with wanted(name) as (values ('spawn_events'),('spawn_snapshots'),('spawn_refs'),('spawn_ref_log'),('prov_changes'),('prov_entries'),('prov_edges')) select count(*) from wanted w where to_regclass(format('%I.%I',:'schema',w.name)) is null") || die "table verification failed"
[ "$missing_tables" = 0 ] || die "D6/provenance table verification failed"
missing_triggers=$(psql_scalar -c "with wanted(name) as (values ('prov_changes_unchanged'),('prov_changes_untruncated'),('prov_entries_unchanged'),('prov_entries_untruncated'),('prov_edges_unchanged'),('prov_edges_untruncated'),('spawn_snapshots_guard_row'),('spawn_snapshots_guard_truncate'),('spawn_ref_log_unchanged'),('spawn_ref_log_untruncated')) select count(*) from wanted w where not exists (select 1 from pg_trigger t join pg_class c on c.oid=t.tgrelid join pg_namespace n on n.oid=c.relnamespace where n.nspname=:'schema' and t.tgname=w.name and not t.tgisinternal)") || die "trigger verification failed"
[ "$missing_triggers" = 0 ] || die "D6/provenance trigger verification failed"
missing_roles=$(psql_scalar -c "with wanted(base) as (values ('spawn_supervisor'),('spawn_reader'),('spawn_human'),('spawn_node_gateway')) select count(*) from wanted w where not exists (select 1 from pg_roles r where r.rolname=spawn_login_role(w.base) and r.rolcanlogin)") || die "restricted role verification failed"
[ "$missing_roles" = 0 ] || die "restricted login role verification failed"

log "SUCCESS: clean target initialized and verified; writers remain STOPPED"
log "Switch unit credentials/configuration to the target through the normal secret store, then restore writers deliberately."
