#!/bin/sh
# Objective 115 [recoverability], two backup keys, part B1: a `spawn store
# put` run's record (`backup.stored`) may name the key that sealed its
# objects, `"sealed_with": "<name>"` (sync/design-backup-two-keys.md rev 2,
# split B1 by the lead 11-13). The field is optional: the live binary and a
# rolled-back controller write none, and their records stay accepted.
#
# Black box at the store's own interface, on a database of its own: the
# schema `spawn init-db` installs, then records offered through
# `spawn_backup_record` as the owner's login (as `spawn store put` offers
# them). Every value is made up; nothing here is a secret.
#   (1) A backup.stored record of today's shape, without sealed_with, is
#       accepted.
#   (2) The same with "sealed_with": "k2", and with "k12", is accepted.
#   (3) Each of these is refused, and nothing of it recorded: sealed_with
#       "x7", "k0", "k2 " (a space after), 2 (a number), null; and another
#       extra key ("key_file") beside a good sealed_with.
#   (4) The log holds exactly the three accepted backup.stored records,
#       the two with sealed_with naming k2 and k12.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "115: harness: $*" >&2
    exit 2
}
[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
controller=$(cd "$(dirname "$0")/../.." && pwd)
for tool in psql jq od; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -m 700 "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_115_stored_$run"
database_made=""
cleanup() {
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "115: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
export SPAWN_DATABASE_URL
tries=0
until "$SPAWN_BIN" init-db >"$WORK/init.out" 2>&1 </dev/null; do
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(tail -n 3 "$WORK/init.out")"
    sleep 1
done

check_failed() {
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
# offer <record json>: spawn_backup_record as the owner's login; status in $status
offer() {
    status=0
    printf "select spawn_backup_record('%s'::jsonb);\n" "$1" |
        psql -d "$SPAWN_DB_NAME" -qtA -v ON_ERROR_STOP=1 -f - >"$WORK/offer.out" 2>"$WORK/offer.err" || status=$?
}
base='{"type":"backup.stored","area":"transcripts","host":"bb115","account":"bb","manifest":"locktest/bb115/transcripts/manifests/bb115/bb/000000000001-20261113-120000-0a1b2c3d.json.gpg","files":2,"unchanged":0,"grown":1,"whole":1,"segments_written":2,"segments_found":0,"bytes_written":714}'
with() { printf '%s' "$base" | jq -c "$1"; }

# --- (1) today's shape, no sealed_with ------------------------------------------------
offer "$base"
[ "$status" -eq 0 ] || check_failed "(1) a backup.stored record without sealed_with was refused: $(head -c 300 "$WORK/offer.err")"
echo "115: (1) checked: a record without sealed_with" >&2

# --- (2) sealed_with k2, k12 ------------------------------------------------------------
for name in k2 k12; do
    offer "$(with ".sealed_with = \"$name\"")"
    [ "$status" -eq 0 ] || check_failed "(2) a backup.stored record with sealed_with $name was refused: $(head -c 300 "$WORK/offer.err")"
done
echo "115: (2) checked: sealed_with k2 and k12" >&2

# --- (3) refused ------------------------------------------------------------------------------
for change in '.sealed_with = "x7"' '.sealed_with = "k0"' '.sealed_with = "k2 "' '.sealed_with = 2' \
    '.sealed_with = null' '.sealed_with = "k2" | .key_file = "k2"'; do
    offer "$(with "$change")"
    [ "$status" -ne 0 ] || check_failed "(3) a backup.stored record with [$change] was accepted"
done
echo "115: (3) checked: a bad name or another key is refused" >&2

# --- (4) the log --------------------------------------------------------------------------------
got=$(psql -d "$SPAWN_DB_NAME" -tA -c "select count(*) || ' ' || coalesce(string_agg(coalesce(kind -> 'detail' -> 'record' ->> 'sealed_with', '-'), ',' order by id), '')
        from spawn_events where session_id = 'session_control' and event_name = 'control.recorded'
         and kind -> 'detail' -> 'record' ->> 'type' = 'backup.stored'") || harness "cannot read the log"
[ "$got" = "3 -,k2,k12" ] || check_failed "(4) the log's backup.stored records (count, sealed_with in order): expected [3 -,k2,k12], got [$got]"
echo "115: (4) checked: the log holds the three accepted records" >&2

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "115: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "115: a backup.stored record may name the key that sealed it, and needs not"
exit 0
