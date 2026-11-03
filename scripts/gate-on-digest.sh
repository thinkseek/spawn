#!/usr/bin/env bash
# Spawn remote gate. Runs exclusively on ssh host "digest".
# - network (crate downloads, docker pull) goes through the corporate proxy
# - compilation uses kache as RUSTC_WRAPPER with a fixed target/cache on /data00
# - PostgreSQL integration tests use an ephemeral docker container
set -euo pipefail

REPO_DIR="/data00/home/chenkailun.c/spawn-workspace"
TARGET_DIR="/data00/home/chenkailun.c/spawn-target"
KACHE_DIR="/data00/home/chenkailun.c/.cache/kache"
LOG_DIR="${REPO_DIR}/gate-logs"
PG_CONTAINER="spawn-gate-pg"
PG_PORT="15432"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
export http_proxy="http://sys-proxy-rd-relay.byted.org:8118"
export https_proxy="http://sys-proxy-rd-relay.byted.org:8118"
export HTTP_PROXY="$http_proxy"
export HTTPS_PROXY="$https_proxy"
export no_proxy="localhost,127.0.0.1,.byted.org,.bytedance.net"
export NO_PROXY="$no_proxy"
export CARGO_TARGET_DIR="$TARGET_DIR"
export CARGO_INCREMENTAL=0
export RUSTC_WRAPPER="$HOME/.local/bin/kache"
export KACHE_CACHE_DIR="$KACHE_DIR"

mkdir -p "$LOG_DIR"
cd "$REPO_DIR"

echo "== toolchain =="
cargo --version
"$RUSTC_WRAPPER" --version

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

echo "== kache stats =="
"$RUSTC_WRAPPER" stats 2>&1 | tee "$LOG_DIR/kache.log" || true
echo "gate ok"
