#!/usr/bin/env bash
# Launch Spawn in Docker while inheriting verified secret-free host toolchains read-only.
set -euo pipefail

usage() {
    cat <<'EOF'
usage: deploy/docker/run-inherit.sh --data-dir <dir> [--config <file>] [--repo <dir>] [--image <image>] [--dry-run] [-- <spawn args...>]
EOF
}

DATA_DIR=""
CONFIG_FILE=""
REPO_DIR=""
IMAGE="spawn:latest"
DRY_RUN=0
SPAWN_ARGS=()

while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --data-dir)
            [ "$#" -ge 2 ] || { echo "error: --data-dir requires a value" >&2; exit 2; }
            DATA_DIR="$2"
            shift 2
            ;;
        --config)
            [ "$#" -ge 2 ] || { echo "error: --config requires a value" >&2; exit 2; }
            CONFIG_FILE="$2"
            shift 2
            ;;
        --repo)
            [ "$#" -ge 2 ] || { echo "error: --repo requires a value" >&2; exit 2; }
            REPO_DIR="$2"
            shift 2
            ;;
        --image)
            [ "$#" -ge 2 ] || { echo "error: --image requires a value" >&2; exit 2; }
            IMAGE="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            SPAWN_ARGS=("$@")
            break
            ;;
        *)
            echo "error: unknown flag: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ -z "$DATA_DIR" ]; then
    echo "error: --data-dir is required" >&2
    usage >&2
    exit 2
fi

HOST_HOME="${HOME:-}"
CONTAINER_HOME="/home/spawn"

gitconfig_is_safe() {
    local file="$1"
    python3 - "$file" <<'PY'
import re, sys

try:
    text = open(sys.argv[1], "r", encoding="utf-8", errors="ignore").read()
except OSError:
    sys.exit(1)

for raw in text.splitlines():
    line = raw.strip()
    if not line or line.startswith("#") or line.startswith(";"):
        continue
    low = line.lower()
    if any(k in low for k in ("extraheader", "authorization:", "bearer ", "ghp_", "github_pat_", "sk-ant-", "sk-proj-")):
        sys.exit(1)
    for m in re.finditer(r"://([^/?#\"'\s\\]+)", low):
        if "@" in m.group(1):
            sys.exit(1)
    if "=" in low:
        k, v = [p.strip() for p in low.split("=", 1)]
        if any(w in k for w in ("password", "token", "secret", "apikey", "api_key")):
            sys.exit(1)
        if (k == "helper" or k.endswith(".helper")) and v.startswith("!"):
            sys.exit(1)
sys.exit(0)
PY
}

CMD=(
    docker run --rm -i
    -e SPAWN_ENGINE_VIEW=container
    -e SPAWN_SANDBOX_INHERIT_HOST=auto
    -e SPAWN_TERMINAL_BACKEND=herdr
    -v "${DATA_DIR}:/var/lib/spawn:rw"
)

if [ -n "$CONFIG_FILE" ]; then
    CMD+=(-v "${CONFIG_FILE}:/etc/spawn/spawn.toml:ro" -e "SPAWN_CONFIG=/etc/spawn/spawn.toml")
fi
if [ -n "$REPO_DIR" ]; then
    CMD+=(-v "${REPO_DIR}:/repo:rw" -e "SPAWN_GIT_REPO=/repo")
fi

if [ -n "$HOST_HOME" ] && [ -d "$HOST_HOME" ]; then
    for rel in .rustup .cargo/bin .nvm .pyenv .asdf .mise .local/opt .local/share/mise; do
        src="${HOST_HOME}/${rel}"
        if [ -d "$src" ]; then
            CMD+=(-v "${src}:${CONTAINER_HOME}/${rel}:ro")
        fi
    done
    for rel in .cargo/config.toml .cargo/config; do
        src="${HOST_HOME}/${rel}"
        if [ -f "$src" ]; then
            CMD+=(-v "${src}:${CONTAINER_HOME}/${rel}:ro")
        fi
    done
    if [ -f "${HOST_HOME}/.gitconfig" ]; then
        if gitconfig_is_safe "${HOST_HOME}/.gitconfig"; then
            CMD+=(-v "${HOST_HOME}/.gitconfig:${CONTAINER_HOME}/.gitconfig:ro")
        else
            echo "warning: skipping host .gitconfig (contains inline credentials or auth headers)" >&2
        fi
    fi
fi

for var in http_proxy https_proxy no_proxy HTTP_PROXY HTTPS_PROXY NO_PROXY; do
    val="${!var-}"
    if [ -n "$val" ] && [[ "$val" != *"@"* ]]; then
        CMD+=(-e "${var}=${val}")
    fi
done

CMD+=("$IMAGE")
if [ "${#SPAWN_ARGS[@]}" -gt 0 ]; then
    CMD+=("${SPAWN_ARGS[@]}")
else
    CMD+=(doctor --json)
fi

if [ "$DRY_RUN" -eq 1 ]; then
    printf '%q ' "${CMD[@]}"
    printf '\n'
    exit 0
fi

exec "${CMD[@]}"
