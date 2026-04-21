#!/usr/bin/env bash
# BlockFinder Pool — Quick restart / rebuild
# Injects git provenance and selects the correct compose overlay automatically.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

# Prefer modern `docker compose`, but fall back to legacy `docker-compose`
# when the plugin is not available on the target server.
if docker compose version >/dev/null 2>&1; then
  DC=(docker compose)
elif docker-compose version >/dev/null 2>&1; then
  DC=(docker-compose)
else
  echo "docker compose / docker-compose not found" >&2
  exit 1
fi

# ── Git provenance ────────────────────────────────────────────────────────────
if git rev-parse --git-dir &>/dev/null 2>&1; then
  export BUILD_GIT_SHA="$(git rev-parse --verify HEAD 2>/dev/null || echo unknown)"
  export BUILD_GIT_DIRTY="$([ -n "$(git status --porcelain 2>/dev/null)" ] && echo true || echo false)"
  export BUILD_SOURCE="local-docker-compose"
  export BUILD_TIME_UTC="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
fi

# ── Compose overlay: Umbrel vs standalone ────────────────────────────────────
# Allow explicit override so a non-Umbrel server never accidentally tries to
# attach to an Umbrel-only external network.
MODE="${BLACKHOLE_COMPOSE_MODE:-auto}"
if [ "$MODE" = "umbrel" ]; then
  COMPOSE_FILES=(-f docker-compose.yml)
elif [ "$MODE" = "standalone" ]; then
  COMPOSE_FILES=(-f docker-compose.standalone.yml)
elif docker network inspect umbrel_main_network >/dev/null 2>&1; then
  COMPOSE_FILES=(-f docker-compose.yml)
else
  COMPOSE_FILES=(-f docker-compose.standalone.yml)
fi

PROJECT_NAME="$(basename "$PWD" | tr '[:upper:]' '[:lower:]' | tr -cd 'a-z0-9')"
COMPOSE_PROJECT_ARGS=(-p "$PROJECT_NAME")

echo "[1/2] Building BlockFinder…"
sudo -E "${DC[@]}" "${COMPOSE_PROJECT_ARGS[@]}" "${COMPOSE_FILES[@]}" build

EXISTING_CONTAINERS=$(
  {
    docker ps -aq \
      --filter "label=com.docker.compose.project=${PROJECT_NAME}" \
      2>/dev/null || true
    docker ps -aq \
      --filter "name=${PROJECT_NAME}_" \
      2>/dev/null || true
  } | awk 'NF && !seen[$0]++'
)
if [ -n "$EXISTING_CONTAINERS" ]; then
  echo "$EXISTING_CONTAINERS" | xargs -r docker rm -f >/dev/null 2>&1 || true
fi

echo "[2/2] Starting services…"
sudo -E "${DC[@]}" "${COMPOSE_PROJECT_ARGS[@]}" "${COMPOSE_FILES[@]}" up -d --remove-orphans

echo ""
echo "Status:"
sleep 4
sudo docker ps --filter "name=blackhole" \
  --format "table {{.Names}}\t{{.Status}}\t{{.Ports}}"

echo ""
HOST_IP=$(hostname -I 2>/dev/null | awk '{print $1}' || echo localhost)
echo "  Dashboard : http://${HOST_IP}:3334"
if [ "$MODE" = "umbrel" ] || { [ "$MODE" = "auto" ] && docker network inspect umbrel_main_network >/dev/null 2>&1; }; then
  API_PORT=8081
else
  API_PORT=8080
fi
echo "  API       : http://${HOST_IP}:${API_PORT}/pool"
echo "  Stratum   : ${HOST_IP}:3333"
