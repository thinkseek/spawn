#!/usr/bin/env bash
# Offline tests for clean-remigrate-db.sh. All systemctl/spawn/psql calls are mocked.
set -euo pipefail
umask 077
here=$(cd "$(dirname "$0")" && pwd)
script=$here/clean-remigrate-db.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failed=0
ok() { printf 'ok: %s\n' "$*"; }
bad() { printf 'FAILED: %s\n' "$*" >&2; failed=1; }

mkdir -p "$work/bin"
cat > "$work/bin/systemctl" <<'EOF'
#!/usr/bin/env bash
printf 'systemctl' >> "$MOCK_LOG"
printf ' <%s>' "$@" >> "$MOCK_LOG"
printf '\n' >> "$MOCK_LOG"
if [ "$1 $2 $3" = "--user start --wait" ] && [ "$4" = "${MOCK_FAIL_UNIT-}" ]; then exit 1; fi
if [ "$1 $2" = "--user is-active" ]; then printf '%s\n' "${MOCK_ACTIVE-inactive}"; fi
EOF
cat > "$work/bin/spawn" <<'EOF'
#!/usr/bin/env bash
printf 'spawn' >> "$MOCK_LOG"; printf ' <%s>' "$@" >> "$MOCK_LOG"; printf '\n' >> "$MOCK_LOG"
if [ "${1-} ${2-}" = "node list" ]; then printf '{"nodes":[],"credentials":"redacted"}\n'; fi
if [ "${1-}" = init-db ]; then exit "${MOCK_INIT_STATUS-0}"; fi
EOF
cat > "$work/bin/psql" <<'EOF'
#!/usr/bin/env bash
printf 'psql' >> "$MOCK_LOG"; printf ' <%s>' "$@" >> "$MOCK_LOG"; printf '\n' >> "$MOCK_LOG"
sql=${!#}
case $sql in
  *current_database*) printf '%s\n%s\n' "${MOCK_DB-clean_target}" "${MOCK_SCHEMA-public}" ;;
  *'select (select count(*) from pg_class'*) printf '%s\n' "${MOCK_OBJECTS-0}" ;;
  *format_type*) printf '%s\n' "${MOCK_XID-xid8}" ;;
  *spawn_snapshots_guard_row*) printf '%s\n' "${MOCK_TRIGGERS-0}" ;;
  *"with wanted(name)"*) printf '%s\n' "${MOCK_TABLES-0}" ;;
  *"with wanted(base)"*) printf '%s\n' "${MOCK_ROLES-0}" ;;
  *) printf 'unexpected SQL\n' >&2; exit 2 ;;
esac
EOF
chmod 700 "$work/bin/systemctl" "$work/bin/spawn" "$work/bin/psql"

run_case() {
  local name=$1; shift
  local dir=$work/$name
  local -a vars=()
  while [ "$1" != -- ]; do vars+=("$1"); shift; done
  shift
  mkdir -p "$dir"
  MOCK_LOG=$dir/calls
  export MOCK_LOG
  : > "$MOCK_LOG"
  set +e
  env -i PATH="$work/bin:/usr/bin:/bin" HOME="$dir/home" MOCK_LOG="$MOCK_LOG" \
    SPAWN_CLEAN_DATABASE_URL='postgres://owner:secret-value@127.0.0.1/clean_target' \
    SPAWN_CLEAN_EXPECT_DATABASE=clean_target "${vars[@]}" \
    bash "$script" "$@" > "$dir/out" 2>&1
  status=$?
  set -e
  OUT=$dir/out
  CALLS=$dir/calls
  DIR=$dir
  if grep -Rqs 'secret-value' "$dir/out" "$dir/calls"; then bad "$name leaked a credential"; fi
}

# No mode means the completely offline dry run.
run_case default-dry --
if [ "$status" = 0 ] && grep -q 'DRY RUN' "$OUT" && [ ! -s "$CALLS" ]; then ok "default dry run executes no command"; else bad "default dry run"; fi
run_case both -- --execute --dry-run
if [ "$status" != 0 ]; then ok "mutually exclusive modes rejected"; else bad "both modes accepted"; fi
run_case no-confirm -- --execute --archive-dir "$work/no-confirm/archive"
if [ "$status" != 0 ] && [ ! -s "$CALLS" ]; then ok "execute without confirmation stops before commands"; else bad "missing confirmation"; fi
run_case backup-fail MOCK_FAIL_UNIT=spawn-backup.service -- --execute --archive-dir "$work/backup-fail/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && grep -q spawn-backup.service "$OUT" && ! grep -q spawn-backup-bundle.service "$CALLS"; then ok "backup failure is a hard stop"; else bad "backup failure gate"; fi
run_case verify-fail MOCK_FAIL_UNIT=spawn-backup-verify.service -- --execute --archive-dir "$work/verify-fail/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && [ ! -e "$DIR/archive/nodes.json" ] && ! grep -q '<stop>' "$CALLS"; then ok "object verification failure stops before archive/writers"; else bad "verify gate"; fi
run_case writer-active MOCK_ACTIVE=active -- --execute --archive-dir "$work/writer-active/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q '<init-db>' "$CALLS"; then ok "active writer blocks initialization"; else bad "active writer gate"; fi
run_case nonempty MOCK_OBJECTS=1 -- --execute --archive-dir "$work/nonempty/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q '<init-db>' "$CALLS"; then ok "nonempty target blocks init-db"; else bad "nonempty target gate"; fi
run_case bad-xid MOCK_XID=xid -- --execute --archive-dir "$work/bad-xid/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q SUCCESS "$OUT"; then ok "wrong commit_xid type blocks success"; else bad "xid8 verification"; fi
run_case bad-roles MOCK_ROLES=1 -- --execute --archive-dir "$work/bad-roles/archive" --confirm-clean-remigrate=clean_target
if [ "$status" != 0 ] && ! grep -q SUCCESS "$OUT"; then ok "missing restricted login blocks success"; else bad "role verification"; fi
run_case success -- --execute --archive-dir "$work/success/archive" --confirm-clean-remigrate=clean_target
if [ "$status" = 0 ] && grep -q SUCCESS "$OUT" && [ -s "$DIR/archive/nodes.json" ] && \
   [ "$(stat -c %a "$DIR/archive/nodes.json")" = 600 ] && grep -q '<init-db>' "$CALLS" && \
   [ "$(grep -c '<stop>' "$CALLS")" = 5 ]; then
  ok "verified backup, quiesce, empty target, init and schema checks run in order"
else
  bad "successful execution (status $status)"
fi

[ "$failed" = 0 ] || exit 1
printf 'all clean-remigrate-db tests passed\n'
