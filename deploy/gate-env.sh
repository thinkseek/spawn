# shellcheck shell=bash
# gate_env_read <file>: the integrator's gate environment as deploy.sh's
# base-only gate takes it, from the file the integrator writes at its start
# (SPAWN_GATE_ENV_FILE; gate.rs write_gate_env: NAME=value records ended by
# a NUL, its pid first). Sets the caller's `keep` (an array of the
# assignments the gate gets, the test URL aside), `own` (the gate's own
# server, SPAWN_GATE_PG_IMAGE or SPAWN_GATE_PG_BIN, if any), `test_url` and
# `home`. Sourced by deploy.sh; gate.rs's tests check it against gate_env.
gate_env_read() {
  local assignment name
  while IFS= read -r -d '' assignment; do
    name=${assignment%%=*}
    case $name in
    PATH | HOME | LANG | TMPDIR | CARGO_HOME | RUSTUP_HOME | SPAWN_GATE_MIN_FREE_GB | SPAWN_GATE_JOBS | SPAWN_GATE_HOST_JOBS)
      keep+=("$assignment") ;;
    SPAWN_GATE_PG_IMAGE | SPAWN_GATE_PG_BIN)
      own=${own:-${assignment#*=}}
      keep+=("$assignment")
      ;;
    SPAWN_TEST_DATABASE_URL) test_url=${assignment#*=} ;;
    esac
    [ "$name" != HOME ] || home=${assignment#*=}
  done <"$1"
}
