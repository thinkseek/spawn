# shellcheck shell=bash
# Shared by pg-backup.sh and pg-restore-drill.sh. Sourced, not run.
#
# Secrets (database passwords, R2 keys, the encryption key) are read from
# mode-600 files and never echoed, logged or put on a command line: PostgreSQL
# passwords go to libpq through PGPASSWORD, R2 keys to curl through a config
# on a file descriptor.

SPAWN_ENV_FILE=${SPAWN_ENV_FILE:-$HOME/.local/share/spawn-rsi/spawn.env}
SPAWN_R2_ENV_FILE=${SPAWN_R2_ENV_FILE:-$HOME/.config/spawn/r2.env}
SPAWN_BACKUP_KEY_FILE=${SPAWN_BACKUP_KEY_FILE:-$HOME/.config/spawn/backup.key}
SPAWN_BACKUP_DIR=${SPAWN_BACKUP_DIR:-$HOME/.local/share/spawn-rsi/backups}
SPAWN_TEST_DATABASE_URL_FILE=${SPAWN_TEST_DATABASE_URL_FILE:-$HOME/.config/spawn/test-database.url}

# Object keys: postgres/spawn-YYYYMMDD-HHMMSS.dump.gpg, timestamp in UTC.
R2_PREFIX=postgres/
R2_KEY_RE='^postgres/spawn-[0-9]{8}-[0-9]{6}\.dump\.gpg$'

die() { echo "$PROG: error: $*" >&2; exit 1; }
log() { echo "$PROG: $*"; }

need_tools() {
  local t
  for t in "$@"; do
    command -v "$t" >/dev/null 2>&1 || die "required tool not found: $t"
  done
}

# check_private FILE: the file exists and only its owner can read it.
check_private() {
  local mode
  [ -f "$1" ] || die "missing $1"
  mode=$(stat -c %a "$1") || die "cannot stat $1"
  case $mode in
    [4-7]00) ;;
    *) die "$1 has mode $mode; it holds a secret, chmod 600 it" ;;
  esac
}

# env_value FILE KEY: prints the value of KEY=VALUE in FILE (quotes stripped).
# Returns 1 when the key is absent. The file is parsed, not sourced.
env_value() {
  local line
  line=$(grep -m1 "^$2=" "$1") || return 1
  line=${line#*=}
  case $line in
    \"*\") line=${line#\"}; line=${line%\"} ;;
    \'*\') line=${line#\'}; line=${line%\'} ;;
  esac
  printf '%s' "$line"
}

load_r2() {
  check_private "$SPAWN_R2_ENV_FILE"
  R2_ENDPOINT=$(env_value "$SPAWN_R2_ENV_FILE" SPAWN_R2_ENDPOINT) || die "SPAWN_R2_ENDPOINT not in $SPAWN_R2_ENV_FILE"
  R2_BUCKET=$(env_value "$SPAWN_R2_ENV_FILE" SPAWN_R2_BUCKET) || die "SPAWN_R2_BUCKET not in $SPAWN_R2_ENV_FILE"
  R2_ACCESS_KEY_ID=$(env_value "$SPAWN_R2_ENV_FILE" SPAWN_R2_ACCESS_KEY_ID) || die "SPAWN_R2_ACCESS_KEY_ID not in $SPAWN_R2_ENV_FILE"
  R2_SECRET_ACCESS_KEY=$(env_value "$SPAWN_R2_ENV_FILE" SPAWN_R2_SECRET_ACCESS_KEY) || die "SPAWN_R2_SECRET_ACCESS_KEY not in $SPAWN_R2_ENV_FILE"
  R2_ENDPOINT=${R2_ENDPOINT%/}
}

load_live_url() {
  check_private "$SPAWN_ENV_FILE"
  LIVE_URL=$(env_value "$SPAWN_ENV_FILE" SPAWN_DATABASE_URL) || die "SPAWN_DATABASE_URL not in $SPAWN_ENV_FILE"
  [ -n "$LIVE_URL" ] || die "SPAWN_DATABASE_URL is empty in $SPAWN_ENV_FILE"
}

# pg_split URL: sets PG_URL (the URL without its password) and PG_PASS (the
# password, percent-decoded; empty when the URL has none).
pg_split() {
  local re='^(postgres(ql)?://)([^:@/]*):([^@]*)@(.*)$'
  if [[ $1 =~ $re ]]; then
    PG_URL="${BASH_REMATCH[1]}${BASH_REMATCH[3]}@${BASH_REMATCH[5]}"
    PG_PASS=$(printf '%b' "${BASH_REMATCH[4]//%/\\x}")
  else
    PG_URL=$1
    PG_PASS=
  fi
}

# with_db URL CMD ARGS...: runs CMD ARGS... -w -d <URL without password>,
# with the password in PGPASSWORD (environment, not argv).
with_db() {
  local url=$1; shift
  pg_split "$url"
  PGPASSWORD=$PG_PASS PGCONNECT_TIMEOUT=10 "$@" -w -d "$PG_URL"
}

# url_with_dbname URL NAME: the same server and credentials, another database.
url_with_dbname() {
  local re='^(postgres(ql)?://[^/]*/)([^?]*)(\?.*)?$'
  [[ $1 =~ $re ]] || die "cannot parse a database name out of the test URL"
  printf '%s%s%s' "${BASH_REMATCH[1]}" "$2" "${BASH_REMATCH[4]}"
}

# s3 CURL-ARGS...: one signed request to R2. Credentials go through a curl
# config on a file descriptor so they never show in ps.
s3() {
  curl --silent --show-error --fail-with-body --retry 3 --connect-timeout 20 --max-time 900 \
    --aws-sigv4 "aws:amz:auto:s3" \
    -K <(printf 'user = "%s:%s"\n' "$R2_ACCESS_KEY_ID" "$R2_SECRET_ACCESS_KEY") \
    "$@"
}

object_url() { printf '%s/%s/%s' "$R2_ENDPOINT" "$R2_BUCKET" "$1"; }

# list_backups: one ListObjectsV2 call; prints "KEY SIZE" for every object
# under the prefix, oldest first (keys sort by their UTC timestamp).
list_backups() {
  local xml
  xml=$(s3 "$R2_ENDPOINT/$R2_BUCKET?list-type=2&max-keys=1000&prefix=postgres%2F") \
    || die "listing s3://$R2_BUCKET/$R2_PREFIX failed: $xml"
  case $xml in
    *"<IsTruncated>true</IsTruncated>"*) die "more than 1000 objects under $R2_PREFIX; refusing to prune from a partial list" ;;
    *"<ListBucketResult"*) ;;
    *) die "unexpected ListObjectsV2 response" ;;
  esac
  printf '%s' "$xml" | sed 's#<Contents>#\n<Contents>#g' | grep '^<Contents>' |
    sed -E 's#.*<Key>([^<]*)</Key>.*<Size>([0-9]+)</Size>.*#\1 \2#' | sort
}

# retention_keep DAILY WEEKLY: reads "KEY SIZE" lines (a listing) on stdin
# and prints the backup keys to keep: the newest backup of each of the newest
# DAILY days, and of each of the newest WEEKLY ISO weeks (UTC). Keys that do
# not match R2_KEY_RE are neither kept nor printed: callers leave them alone.
retention_keep() {
  local daily=$1 weekly=$2 days=0 weeks=0 k stamp day week keep
  local -A seen_day=() seen_week=()
  while read -r k _; do
    [[ $k =~ $R2_KEY_RE ]] || continue
    stamp=${k#"${R2_PREFIX}"spawn-}
    day=${stamp%%-*}
    week=$(date -u -d "$day" +%G-W%V) || die "bad date in $k"
    keep=
    if [ -z "${seen_day[$day]-}" ]; then
      seen_day[$day]=1
      if [ "$days" -lt "$daily" ]; then keep=1; days=$((days + 1)); fi
    fi
    if [ -z "${seen_week[$week]-}" ]; then
      seen_week[$week]=1
      if [ "$weeks" -lt "$weekly" ]; then keep=1; weeks=$((weeks + 1)); fi
    fi
    [ -z "$keep" ] || printf '%s\n' "$k"
  done < <(sort -r)
}

gpg_common=(--batch --quiet --no-tty --pinentry-mode loopback --no-symkey-cache)
