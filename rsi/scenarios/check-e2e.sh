#!/bin/sh
# Check: the decision ladder end to end (scripts/e2e.sh), run from the
# controller's checkout against the candidate's binary.
set -eu
: "${SPAWN_TEST_DATABASE_URL:?the e2e needs SPAWN_TEST_DATABASE_URL}"
controller=$(cd "$(dirname "$0")/../.." && pwd)
SPAWN_DATABASE_URL=$SPAWN_TEST_DATABASE_URL bash "$controller/scripts/e2e.sh"
