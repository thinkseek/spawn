#!/bin/sh
# Check: the black-box CLI scenarios (scripts/blackbox), run from the
# controller's checkout against the candidate's binary.
set -eu
: "${SPAWN_TEST_DATABASE_URL:?the black-box scenarios need SPAWN_TEST_DATABASE_URL}"
controller=$(cd "$(dirname "$0")/../.." && pwd)
url=$SPAWN_TEST_DATABASE_URL
rest=${url#*://}
credentials=${rest%%@*}
address=${rest#*@}
address=${address%%/*}
PGUSER=${credentials%%:*}
PGPASSWORD=${credentials#*:}
PGHOST=${address%%:*}
PGPORT=${address#*:}
export PGUSER PGPASSWORD PGHOST PGPORT
SPAWN_PSQL=psql SPAWN_PG_BASE="${url%/*}" bash "$controller/scripts/blackbox/run-scenarios.sh"
