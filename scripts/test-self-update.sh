#!/usr/bin/env bash
#
# test-self-update.sh
#
# Builds Isengard locally, starts it via compose, then rebuilds with a
# code change so the image digest differs. Isengard should detect the
# mismatch and recreate its own container.
#
# Prerequisites: docker, docker compose
#
# Usage:
#   ./scripts/test-self-update.sh

set -euo pipefail

IMAGE="isengard-self-update-test"
COMPOSE_PROJECT="isengard-selftest"
COMPOSE_FILE="docker-compose.yml"

cleanup() {
  echo ""
  echo "==> Cleaning up..."
  docker compose -p "$COMPOSE_PROJECT" -f "$COMPOSE_FILE" down --remove-orphans 2>/dev/null || true
  docker rmi "$IMAGE:latest" 2>/dev/null || true
  echo "==> Done."
}
trap cleanup EXIT

echo "==> Step 1: Build initial image"
docker build -t "$IMAGE:latest" .

echo ""
echo "==> Step 2: Start Isengard + test containers"
COMPOSE_PROJECT_NAME="$COMPOSE_PROJECT" docker compose -f "$COMPOSE_FILE" up -d --build

echo ""
echo "==> Step 3: Verify Isengard is running"
sleep 3
CONTAINER_ID=$(docker ps --filter "name=${COMPOSE_PROJECT}-isengard" --format '{{.ID}}')
if [ -z "$CONTAINER_ID" ]; then
  echo "ERROR: Isengard container not found"
  exit 1
fi
echo "    Isengard container: $CONTAINER_ID"

echo ""
echo "==> Step 4: Show initial logs"
docker logs "$CONTAINER_ID" 2>&1 | tail -20

echo ""
echo "==> Step 5: Rebuild image (triggers digest change)"
echo "    Touching a file to invalidate the build cache..."
echo "// rebuild $(date +%s)" > .cachebust
docker build -t "$IMAGE:latest" --no-cache .
rm -f .cachebust

echo ""
echo "==> Step 6: Wait for Isengard to detect the change and self-update"
echo "    Watching logs for up to 120 seconds..."
echo "    (Isengard checks every 30s, so this may take a moment)"
echo ""

TIMEOUT=120
ELAPSED=0
while [ $ELAPSED -lt $TIMEOUT ]; do
  # Check if original container is gone (recreated)
  if ! docker ps -q --no-trunc | grep -q "$CONTAINER_ID"; then
    echo ""
    echo "==> Original container $CONTAINER_ID is gone (self-update triggered!)"
    sleep 2
    NEW_ID=$(docker ps --filter "name=${COMPOSE_PROJECT}-isengard" --format '{{.ID}}')
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
      echo "Check docker ps -a for details."
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
