#!/bin/sh
set -eu

interval="${AUTOHEAL_INTERVAL_SECS:-30}"

while :; do
  for cid in $(docker ps -q --filter label=autoheal=true); do
    status="$(docker inspect -f '{{.State.Health.Status}}' "$cid" 2>/dev/null || echo none)"
    if [ "$status" = "unhealthy" ]; then
      echo "autoheal: restarting $cid"
      docker restart "$cid" >/dev/null 2>&1 || true
    fi
  done

  sleep "$interval"
done
