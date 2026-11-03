#!/usr/bin/env bash
# Spawn gate. Runs every check: fmt, clippy, tests, e2e, black box.
# - network (crate downloads, docker pull) goes through the corporate proxy
# - compilation uses kache as RUSTC_WRAPPER with a cache under $HOME
# - PostgreSQL integration tests use an ephemeral docker container
set -euo pipefail

# Every machine-specific value has a default that matches the digest host and
# can be overridden, so the same script runs on any host.
REPO_DIR="${SPAWN_GATE_REPO_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
TARGET_DIR="${SPAWN_GATE_TARGET_DIR:-$HOME/.cache/spawn-gate-target}"
KACHE_DIR="${SPAWN_GATE_KACHE_DIR:-$HOME/.cache/kache}"
LOG_DIR="${SPAWN_GATE_LOG_DIR:-$HOME/.cache/spawn-gate/logs}"
PG_CONTAINER="${SPAWN_GATE_PG_CONTAINER:-spawn-gate-pg}"
PG_PORT="${SPAWN_GATE_PG_PORT:-15432}"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
export http_proxy="${http_proxy:-http://sys-proxy-rd-relay.byted.org:8118}"
export https_proxy="${https_proxy:-$http_proxy}"
export HTTP_PROXY="$http_proxy"
export HTTPS_PROXY="$https_proxy"
export no_proxy="${no_proxy:-localhost,127.0.0.1,.byted.org,.bytedance.net}"
export NO_PROXY="$no_proxy"
export CARGO_TARGET_DIR="$TARGET_DIR"
export CARGO_INCREMENTAL=0
# kache links cache entries into the target directory read-only, so a second
# run cannot overwrite them. Restoring the write bit is cheaper than a rebuild.
chmod -R u+w "$TARGET_DIR" 2>/dev/null || true
# kache is optional: a host without it compiles directly.
if [ -z "${RUSTC_WRAPPER:-}" ]; then
  for candidate in "$HOME/.local/bin/kache" "$HOME/.cargo/bin/kache" "$HOME/bin/kache"; do
    if [ -x "$candidate" ]; then
      RUSTC_WRAPPER="$candidate"
      break
    fi
  done
fi
if [ -z "${RUSTC_WRAPPER:-}" ] && command -v kache >/dev/null 2>&1; then
  RUSTC_WRAPPER="$(command -v kache)"
fi
if [ -n "${RUSTC_WRAPPER:-}" ]; then
  export RUSTC_WRAPPER KACHE_CACHE_DIR="$KACHE_DIR"
fi

mkdir -p "$LOG_DIR"
cd "$REPO_DIR"

echo "== toolchain =="
cargo --version
if [ -n "${RUSTC_WRAPPER:-}" ]; then "$RUSTC_WRAPPER" --version; fi

cleanup() {
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

start_postgres() {
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$PG_CONTAINER" \
    -e POSTGRES_PASSWORD=spawn -e POSTGRES_DB=spawn \
    -p "127.0.0.1:${PG_PORT}:5432" postgres:16-alpine >/dev/null
  for _ in $(seq 1 60); do
    if docker exec "$PG_CONTAINER" pg_isready -U postgres -d spawn >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "postgres did not become ready" >&2
  docker logs "$PG_CONTAINER" >&2 || true
  return 1
}

echo "== postgres =="
start_postgres
export SPAWN_TEST_DATABASE_URL="postgres://postgres:PASSWORD@127.0.0.1:${PG_PORT}/spawn"

echo "== fmt =="
cargo fmt --all -- --check 2>&1 | tee "$LOG_DIR/fmt.log"

echo "== clippy =="
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tee "$LOG_DIR/clippy.log"

echo "== test =="
cargo test --workspace --no-fail-fast 2>&1 | tee "$LOG_DIR/test.log"

echo "== e2e =="
SPAWN_PSQL="docker exec $PG_CONTAINER psql -U postgres -d spawn -c" \
  SPAWN_DATABASE_URL="$SPAWN_TEST_DATABASE_URL" \
  bash "$REPO_DIR/scripts/e2e-smoke.sh" 2>&1 | tee "$LOG_DIR/e2e.log"

echo "== blackbox =="
cargo build -q -p spawn-cli
SPAWN_PSQL="docker exec $PG_CONTAINER psql -U postgres" \
  SPAWN_PG_BASE="postgres://postgres:PASSWORD@127.0.0.1:${PG_PORT}" \
  SPAWN_BIN="${CARGO_TARGET_DIR}/debug/spawn" \
  bash "$REPO_DIR/scripts/blackbox/run-scenarios.sh" 2>&1 | tee "$LOG_DIR/blackbox.log"

echo "== kache stats =="
if [ -n "${RUSTC_WRAPPER:-}" ]; then
  "$RUSTC_WRAPPER" stats 2>&1 | tee "$LOG_DIR/kache.log" || true
fi
echo "gate ok"
