# shellcheck shell=bash
# Shared by pg-backup.sh and pg-restore-drill.sh. Sourced, not run.
#
# Secrets (database passwords, R2 keys, the encryption key) are read from
# mode-600 files and never echoed, logged or put on a command line: PostgreSQL
# passwords go to libpq through PGPASSWORD (or a docker --env-file on a file
# descriptor), R2 keys to curl through a config on a file descriptor.

SPAWN_ENV_FILE=${SPAWN_ENV_FILE:-$HOME/.local/share/spawn-rsi/spawn.env}
SPAWN_R2_ENV_FILE=${SPAWN_R2_ENV_FILE:-$HOME/.config/spawn/r2.env}
SPAWN_BACKUP_KEY_FILE=${SPAWN_BACKUP_KEY_FILE:-$HOME/.config/spawn/backup.key}
SPAWN_BACKUP_DIR=${SPAWN_BACKUP_DIR:-$HOME/.local/share/spawn-rsi/backups}
# The live server's container: its image supplies a pg_dump/pg_restore of the
# server's own major version (dumps stay readable by that version's tools).
SPAWN_LIVE_PG_CONTAINER=${SPAWN_LIVE_PG_CONTAINER:-spawn-rsi-pg}

# Object keys, timestamp in UTC. Retention is not done here: R2 lifecycle
# rules expire each prefix, R2 bucket locks stop anything deleting or
# overwriting it earlier (r2-lifecycle-rules.json, r2-lock-rules.json).
#   postgres/daily/    every good backup          lock 14 d,  expire 15 d
#   postgres/weekly/   first good one of a week   lock 56 d,  expire 60 d
#   postgres/monthly/  first good one of a month  lock 365 d, expire 400 d
#   postgres/suspect/  failed the plausibility check, no lock, expire 30 d
R2_KEY_RE='^postgres/(daily|weekly|monthly)/spawn-[0-9]{8}-[0-9]{6}\.dump\.gpg$'

die() { echo "$PROG: error: $*" >&2; exit 1; }
log() { echo "$PROG: $*"; }
warn() { echo "$PROG: warning: $*" >&2; }

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

# pct_decode S: decodes %XX escapes and nothing else (backslashes stay literal).
pct_decode() {
  local s=$1 out="" ch
  while [[ $s =~ ^([^%]*)%(.*)$ ]]; do
    out+=${BASH_REMATCH[1]}
    s=${BASH_REMATCH[2]}
    if [[ $s =~ ^([0-9A-Fa-f]{2})(.*)$ ]]; then
      printf -v ch '%b' "\\x${BASH_REMATCH[1]}"
      out+=$ch
      s=${BASH_REMATCH[2]}
    else
      out+=%
    fi
  done
  printf '%s' "$out$s"
}

# pg_split URL: sets PG_URL (the URL without its password) and PG_PASS (the
# password, percent-decoded; empty when the URL has none).
pg_split() {
  local re='^(postgres(ql)?://)([^:@/]*):([^@]*)@(.*)$'
  if [[ $1 =~ $re ]]; then
    PG_URL="${BASH_REMATCH[1]}${BASH_REMATCH[3]}@${BASH_REMATCH[5]}"
    PG_PASS=$(pct_decode "${BASH_REMATCH[4]}")
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

# live_sql ARGS...: psql on the live database in read-only transactions.
live_sql() {
  with_db "$LIVE_URL" env PGOPTIONS='-c default_transaction_read_only=on' \
    psql -XAtq -v ON_ERROR_STOP=1 "$@"
}

# pg_image: the image of the live server's container (same major version),
# or SPAWN_PG_IMAGE when set.
pg_image() {
  if [ -n "${SPAWN_PG_IMAGE-}" ]; then
    printf '%s' "$SPAWN_PG_IMAGE"
  else
    docker inspect -f '{{.Config.Image}}' "$SPAWN_LIVE_PG_CONTAINER" 2>/dev/null \
      || die "cannot read the image of container $SPAWN_LIVE_PG_CONTAINER (set SPAWN_PG_IMAGE)"
  fi
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

# list_objects PREFIX: one ListObjectsV2 call; prints "KEY SIZE" for every
# object under PREFIX, sorted (backup keys sort by their UTC timestamp).
list_objects() {
  local xml prefix=${1//\//%2F}
  xml=$(s3 "$R2_ENDPOINT/$R2_BUCKET?list-type=2&max-keys=1000&prefix=$prefix") \
    || die "listing s3://$R2_BUCKET/$1 failed: $xml"
  case $xml in
    *"<ListBucketResult"*) ;;
    *) die "unexpected ListObjectsV2 response" ;;
  esac
  [[ $xml != *"<IsTruncated>true</IsTruncated>"* ]] || warn "more than 1000 objects under $1; listing is partial"
  printf '%s' "$xml" | sed 's#<Contents>#\n<Contents>#g' | grep '^<Contents>' |
    sed -E 's#.*<Key>([^<]*)</Key>.*<Size>([0-9]+)</Size>.*#\1 \2#' | sort
}

gpg_common=(--batch --quiet --no-tty --pinentry-mode loopback --no-symkey-cache)
