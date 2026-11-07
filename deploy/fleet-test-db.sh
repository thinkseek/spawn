#!/bin/sh
# The fleet's own test server: a PostgreSQL 16 cluster the engines' tests
# run on, never the gates' shared server (the system cluster 16-test that
# SPAWN_TEST_DATABASE_URL of spawn-loop and spawn-integrate names) and
# never the live one.
#
#   sh deploy/fleet-test-db.sh            # create it once; then the unit runs it
#
# Owned by this user, beside the live directory, under `$FLEET_PG`
# (default ~/.local/share/spawn-rsi/fleet-pg, denied to engines):
#
#   data/   the cluster (initdb; fsync off: test data, rebuilt if lost)
#   sock/   its only listener, a Unix socket (mode 0700, no TCP)
#
# Two logins, each with a random password (scram-sha-256, local only):
#
#   spawn_fleet_admin  the cluster's superuser (initdb's). Its password goes
#                      to ~/.config/spawn/fleet-test-admin.pw (0600), for
#                      the owner only.
#   spawn_fleet        what engines get: NOSUPERUSER, CREATEDB, CREATEROLE
#                      (the spawn-postgres tests create the restricted
#                      logins), owner of the database `spawn_fleet_test`.
#
# Not a superuser, so a test cannot read or run anything on the host
# through the server (pg_read_file, COPY ... PROGRAM, lo_import): those are
# the superuser's, and pg_read_server_files, pg_write_server_files and
# pg_execute_server_program are roles a CREATEROLE login cannot grant
# itself (PostgreSQL 16: that needs ADMIN OPTION on them).
#
# The cost: 15 of Spawn's own spawn-postgres tests need a superuser (they
# act as the restricted logins with SET SESSION AUTHORIZATION, because the
# schema names its caller by session_user; one reads pg_authid), and fail
# on this login with "permission denied to set session authorization"; the
# gate's login is a superuser, so the gate still runs them. Measured
# 2026-11-08: 129 of 144 pass on spawn_fleet, 144 of 144 with superuser.
# FLEET_PG_SUPERUSER=1 makes spawn_fleet a superuser instead, for a host
# whose workers run those tests; the owner decides. The engines'
# URL goes to ~/.config/spawn/fleet-test-database.url (mode 0600), never
# to the screen. Then, by hand, once (deploy/systemd/README.md, "Fleet
# test database", has the whole order):
#
#   printf 'SPAWN_FLEET_TEST_DATABASE_URL=%s\n' "$(cat ~/.config/spawn/fleet-test-database.url)" \
#       >> ~/.local/share/spawn-rsi/spawn.env
#   cp deploy/systemd/spawn-fleet-test-db.service ~/.config/systemd/user/
#   systemctl --user daemon-reload
#   systemctl --user enable --now spawn-fleet-test-db
#   systemctl --user restart spawn-loop     # agents keep running
#
# spawn-loop wants the unit and starts after it; the Bridge gives an engine
# that URL as SPAWN_TEST_DATABASE_URL only once the server answers and is
# neither the gates' server nor the live database.
#
# Run again, it changes nothing that exists: an existing cluster is left
# alone (its files too). It starts the server only for its own setup, and
# stops it again.
set -eu

pg_bin=${PG_BIN:-/usr/lib/postgresql/16/bin}
root=${FLEET_PG:-$HOME/.local/share/spawn-rsi/fleet-pg}
url_file=${FLEET_PG_URL_FILE:-$HOME/.config/spawn/fleet-test-database.url}
admin_file=${FLEET_PG_ADMIN_FILE:-$HOME/.config/spawn/fleet-test-admin.pw}
# Only names the socket file: the server listens on no TCP port.
port=${FLEET_PG_PORT:-5442}
case ${FLEET_PG_SUPERUSER:-0} in
1) superuser=superuser ;;
0) superuser=nosuperuser ;;
*) echo "fleet-test-db: FLEET_PG_SUPERUSER is 0 or 1" >&2 && exit 1 ;;
esac

die() {
    echo "fleet-test-db: $*" >&2
    exit 1
}

[ -x "$pg_bin/initdb" ] || die "no PostgreSQL 16 at $pg_bin (PG_BIN)"
umask 077
mkdir -p "$root/sock" "$(dirname "$url_file")" "$(dirname "$admin_file")"
chmod 700 "$root" "$root/sock"

if [ -f "$root/data/PG_VERSION" ]; then
    [ -f "$url_file" ] || die "$root/data exists but $url_file does not; remove $root to start over"
    echo "fleet-test-db: $root/data exists; nothing to do"
    exit 0
fi
[ ! -e "$admin_file" ] || die "$admin_file exists but $root/data does not; remove it to start over"

# Hex: nothing to escape in a URL or in SQL.
password() { od -An -N24 -tx1 /dev/urandom | tr -d ' \n'; }
password >"$admin_file"
fleet_password=$(password)

"$pg_bin/initdb" -D "$root/data" -U spawn_fleet_admin --pwfile="$admin_file" \
    --auth-local=scram-sha-256 --auth-host=reject -E UTF8 --locale=C.UTF-8 >/dev/null
cat >>"$root/data/postgresql.conf" <<EOF

# deploy/fleet-test-db.sh: the fleet's test server.
listen_addresses = ''
port = $port
unix_socket_directories = '$root/sock'
unix_socket_permissions = 0700
fsync = off
synchronous_commit = off
full_page_writes = off
max_connections = 200
shared_buffers = 256MB
EOF

"$pg_bin/pg_ctl" -D "$root/data" -l "$root/setup.log" -w start >/dev/null
stop() { "$pg_bin/pg_ctl" -D "$root/data" -m fast -w stop >/dev/null; }
# The SQL, with the engines' password, on stdin: never on argv.
if ! PGPASSWORD=$(cat "$admin_file") "$pg_bin/psql" -X -q -v ON_ERROR_STOP=1 \
    -h "$root/sock" -p "$port" -U spawn_fleet_admin -d postgres <<EOF
create role spawn_fleet login $superuser createdb createrole noreplication nobypassrls
    password '$fleet_password';
create database spawn_fleet_test owner spawn_fleet;
EOF
then
    stop
    die "creating the login and the database failed (above)"
fi
stop

socket=$(printf '%s' "$root/sock" | sed 's|%|%25|g; s|/|%2F|g')
printf 'postgres://spawn_fleet:%s@%s:%s/spawn_fleet_test?sslmode=disable\n' \
    "$fleet_password" "$socket" "$port" >"$url_file"
chmod 600 "$url_file" "$admin_file"
echo "fleet-test-db: cluster in $root/data, URL in $url_file; install the unit (see this script's header)"
