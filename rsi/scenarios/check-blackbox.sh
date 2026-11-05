#!/bin/sh
# Check: the black-box CLI scenarios (scripts/blackbox), run from the
# controller's checkout against the candidate's binary.
set -eu
: "${SPAWN_TEST_DATABASE_URL:?the black-box scenarios need SPAWN_TEST_DATABASE_URL}"
controller=$(cd "$(dirname "$0")/../.." && pwd)
# psql's parameters and the server's URL, read from the test URL as libpq
# reads it (scripts/pg-url.sh): a socket directory or an address (a name,
# 127.0.0.1, [::1]), an optional port, a percent-encoded password.
# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
if ! pg_url_export "$SPAWN_TEST_DATABASE_URL"; then
    echo "check-blackbox: SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL" >&2
    exit 2
fi
SPAWN_PSQL=psql SPAWN_PG_BASE=$(pg_url_server "$SPAWN_TEST_DATABASE_URL") \
    bash "$controller/scripts/blackbox/run-scenarios.sh"
