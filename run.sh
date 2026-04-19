#!/usr/bin/env bash
# BlackHole Pool — Quick restart / rebuild
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
if [ -f "/home/umbrel/umbrel/umbrel.yaml" ] || \
   [ -d "/home/umbrel/umbrel/app-data" ]   || \
   command -v umbreld &>/dev/null 2>&1; then
  COMPOSE_FILES="-f docker-compose.yml"
else
  COMPOSE_FILES="-f docker-compose.standalone.yml"
fi

echo "[1/2] Building BlackHole…"
# shellcheck disable=SC2086
sudo -E "${DC[@]}" $COMPOSE_FILES build

echo "[2/2] Starting services…"
# shellcheck disable=SC2086
sudo -E "${DC[@]}" $COMPOSE_FILES up -d

echo ""
echo "Status:"
sleep 4
sudo docker ps --filter "name=blackhole" \
  --format "table {{.Names}}\t{{.Status}}\t{{.Ports}}"

echo ""
HOST_IP=$(hostname -I 2>/dev/null | awk '{print $1}' || echo localhost)
echo "  Dashboard : http://${HOST_IP}:3334"
echo "  API       : http://${HOST_IP}:8081/pool"
echo "  Stratum   : ${HOST_IP}:3333"
