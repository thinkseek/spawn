#!/bin/sh
# test-pg-url.sh: what pg_url_export (scripts/pg-url.sh) exports for psql
# (S21: the URL's database among it). No server: the variables only.
set -u
here=$(cd "$(dirname "$0")" && pwd)
failed=0
expect() { # <what> <got> <want>
    if [ "$2" = "$3" ]; then
        echo "ok   $1"
    else
        echo "FAIL $1: got [$2], want [$3]"
        failed=$((failed + 1))
    fi
}
# exported <url>: what a child process of pg_url_export sees, in a fresh
# shell that starts with stale values of every variable.
exported() {
    PGUSER=stale PGPASSWORD=stale PGHOST=stale PGPORT=1 PGDATABASE=stale sh -c '
        . "$1"
        pg_url_export "$2" || { printf "not a URL"; exit 0; }
        sh -c '\''printf "%s|%s|%s|%s|%s" "${PGUSER-unset}" "${PGPASSWORD-unset}" \
            "${PGHOST-unset}" "${PGPORT-unset}" "${PGDATABASE-unset}"'\''
    ' exported "$here/pg-url.sh" "$1"
}
expect "the URL's database is exported" \
    "$(exported 'postgres://u:p@h:6000/spawn_test')" 'u|p|h|6000|spawn_test'
expect "a percent-encoded database is decoded" \
    "$(exported 'postgres://u@h/my%20db')" 'u|unset|h|5432|my db'
expect "a dbname parameter names it too" \
    "$(exported 'postgres://u@h/?dbname=other')" 'u|unset|h|5432|other'
expect "a URL with no database leaves libpq's default (no stale value)" \
    "$(exported 'postgres://u@h')" 'u|unset|h|5432|unset'
expect "a socket URL" \
    "$(exported 'postgres://u@%2Ftmp%2Fs/db')" 'u|unset|/tmp/s|5432|db'
expect "no URL exports nothing" "$(exported 'mysql://u@h/db')" 'not a URL'
[ "$failed" -eq 0 ] && echo "test-pg-url: all passed" || echo "test-pg-url: $failed FAIL"
[ "$failed" -eq 0 ]
