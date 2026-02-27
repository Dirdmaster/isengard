#!/usr/bin/env bash
#
# test-self-update.sh
#
# Spins up a local registry, pushes Isengard to it, starts Isengard
# pointing at the local registry image, then pushes a rebuilt image.
# Isengard should detect the digest mismatch and recreate itself.
#
# Prerequisites: docker, docker compose
#
# Usage:
#   ./scripts/test-self-update.sh

set -euo pipefail

REGISTRY="localhost:5111"
IMAGE="$REGISTRY/isengard"
REGISTRY_CONTAINER="isengard-test-registry"

cleanup() {
  echo ""
  echo "==> Cleaning up..."
  docker compose -p isengard-selftest down --remove-orphans 2>/dev/null || true
  docker rm -f "$REGISTRY_CONTAINER" 2>/dev/null || true
  docker rmi "$IMAGE:latest" 2>/dev/null || true
  echo "==> Done."
}
trap cleanup EXIT

echo "==> Step 1: Start local registry on port 5111"
docker rm -f "$REGISTRY_CONTAINER" 2>/dev/null || true
docker run -d --name "$REGISTRY_CONTAINER" -p 5111:5000 registry:2
sleep 2

echo ""
echo "==> Step 2: Build and push initial image"
docker build -t "$IMAGE:latest" .
docker push "$IMAGE:latest"

echo ""
echo "==> Step 3: Start Isengard pointing at the local registry image"
# Override the compose service to use our registry image
docker compose -p isengard-selftest -f docker-compose.yml -f - up -d <<'OVERRIDE'
services:
  isengard:
    build: !reset null
    image: localhost:5111/isengard:latest
OVERRIDE

sleep 3
CONTAINER_ID=$(docker ps --filter "name=isengard-selftest-isengard" --format '{{.ID}}')
if [ -z "$CONTAINER_ID" ]; then
  echo "ERROR: Isengard container not found"
  docker ps -a --filter "name=isengard-selftest"
  exit 1
fi
echo "    Isengard container: $CONTAINER_ID"

echo ""
echo "==> Step 4: Show initial logs"
docker logs "$CONTAINER_ID" 2>&1 | tail -10

echo ""
echo "==> Step 5: Rebuild and push updated image"
echo "// rebuild $(date +%s)" > .cachebust
docker build -t "$IMAGE:latest" --no-cache .
rm -f .cachebust
docker push "$IMAGE:latest"
echo "    New image pushed to $IMAGE:latest"

echo ""
echo "==> Step 6: Wait for Isengard to detect the change and self-update"
echo "    Watching for up to 120 seconds (interval is 30s)..."
echo ""

TIMEOUT=120
ELAPSED=0
while [ $ELAPSED -lt $TIMEOUT ]; do
  if ! docker ps -q --no-trunc | grep -q "$CONTAINER_ID"; then
    echo ""
    echo "==> Original container $CONTAINER_ID is gone (self-update triggered!)"
    sleep 3
    NEW_ID=$(docker ps --filter "name=isengard-selftest-isengard" --format '{{.ID}}')
    if [ -n "$NEW_ID" ]; then
      echo "==> New Isengard container: $NEW_ID"
      echo ""
      echo "==> New container logs:"
      docker logs "$NEW_ID" 2>&1 | tail -10
      echo ""
      echo "SUCCESS: Self-update worked!"
      exit 0
    else
      echo "WARNING: Original container stopped but no replacement found."
      echo "Check: docker ps -a --filter name=isengard-selftest"
      exit 1
    fi
  fi
  sleep 5
  ELAPSED=$((ELAPSED + 5))
  printf "    %ds / %ds...\r" "$ELAPSED" "$TIMEOUT"
done

echo ""
echo "TIMEOUT: Isengard did not self-update within ${TIMEOUT}s"
echo ""
echo "==> Final logs:"
docker logs "$CONTAINER_ID" 2>&1 | tail -30
exit 1
