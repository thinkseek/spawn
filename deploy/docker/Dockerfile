# Runtime image for Spawn with inherited host toolchains (non-root, container fallback aware).
# Build from the repository root:
#   docker build -f deploy/docker/Dockerfile -t spawn:latest .

ARG RUST_IMAGE=rust:1.86-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

FROM ${RUST_IMAGE} AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --locked \
    -p spawn-cli --bin spawn \
    -p spawn-herdr-core --bin spawn-hc

FROM ${RUNTIME_IMAGE} AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    bash \
    bubblewrap \
    ca-certificates \
    curl \
    git \
    postgresql-client \
    procps \
    python3 \
    zstd \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 -s /bin/bash spawn \
    && mkdir -p /usr/local/lib/spawn/redact /var/lib/spawn /workspace \
    && chown -R spawn:spawn /var/lib/spawn /workspace

COPY --from=builder /src/target/release/spawn /usr/local/bin/spawn
COPY --from=builder /src/target/release/spawn-hc /usr/local/bin/spawn-hc
COPY scripts/redact/ /usr/local/lib/spawn/redact/
RUN chmod 0755 /usr/local/lib/spawn/redact/*.py

ENV HOME=/home/spawn \
    PATH="/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
    SPAWN_TERMINAL_BACKEND=herdr \
    SPAWN_ENGINE_VIEW=container \
    SPAWN_SANDBOX_INHERIT_HOST=auto \
    SPAWN_WORKSPACE_ROOT=/var/lib/spawn/workspaces

USER spawn
WORKDIR /var/lib/spawn
ENTRYPOINT ["/usr/local/bin/spawn"]
