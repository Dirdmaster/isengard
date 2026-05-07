#!/usr/bin/env bash
# Isengard standalone uninstall. Stops and removes the controller + agent
# containers and (optionally) the on-disk state. Reverse of install.sh.
#
# Usage:
#   bash uninstall.sh                     # stop + remove containers; keep state
#   bash uninstall.sh --purge             # also remove ${ISENGARD_PREFIX} state
#   bash uninstall.sh --purge --network   # also remove the shared proxy network
#                                          (rejected if other containers use it)
#   bash uninstall.sh --yes               # skip the confirmation prompt
#
# Re-running is idempotent: missing pieces are skipped with a warning.

set -euo pipefail

ISENGARD_PREFIX="${ISENGARD_PREFIX:-/var/lib/isengard}"
ISENGARD_ETC="${ISENGARD_ETC:-/etc/isengard}"
ISENGARD_ENV_FILE="${ISENGARD_ENV_FILE:-${ISENGARD_ETC}/isengard.env}"
ISENGARD_COMPOSE_FILE="${ISENGARD_COMPOSE_FILE:-${ISENGARD_ETC}/compose.yaml}"
ISENGARD_PROXY_NETWORK="${ISENGARD_PROXY_NETWORK:-isengard-proxy}"

PURGE=0
REMOVE_NETWORK=0
ASSUME_YES=0

for arg in "$@"; do
  case "${arg}" in
    --purge)   PURGE=1 ;;
    --network) REMOVE_NETWORK=1 ;;
    --yes|-y)  ASSUME_YES=1 ;;
    --help|-h)
      sed -n '2,12p' "${BASH_SOURCE[0]}"
      exit 0
      ;;
    *)
      printf '[isengard] ERROR: unknown flag: %s\n' "${arg}" >&2
      exit 64
      ;;
  esac
done

log()  { printf '[isengard] %s\n' "$*"; }
warn() { printf '[isengard] WARN: %s\n' "$*" >&2; }
die()  { printf '[isengard] ERROR: %s\n' "$*" >&2; exit 1; }

confirm() {
  if [[ "${ASSUME_YES}" -eq 1 ]]; then
    return 0
  fi
  local prompt="$1"
  printf '%s [y/N] ' "${prompt}"
  read -r reply || reply=""
  [[ "${reply}" == "y" || "${reply}" == "Y" ]]
}

stop_stack() {
  if [[ ! -f "${ISENGARD_COMPOSE_FILE}" ]]; then
    warn "compose file ${ISENGARD_COMPOSE_FILE} not found; skipping compose down"
    return 0
  fi

  log "stopping stack via docker compose down"
  if [[ -f "${ISENGARD_ENV_FILE}" ]]; then
    docker compose --env-file "${ISENGARD_ENV_FILE}" -f "${ISENGARD_COMPOSE_FILE}" down --remove-orphans || \
      warn "compose down reported errors; continuing"
  else
    docker compose -f "${ISENGARD_COMPOSE_FILE}" down --remove-orphans || \
      warn "compose down reported errors; continuing"
  fi
}

purge_state() {
  if [[ ! -d "${ISENGARD_PREFIX}" ]]; then
    warn "${ISENGARD_PREFIX} not found; nothing to purge"
    return 0
  fi
  if ! confirm "Delete all state under ${ISENGARD_PREFIX}? This is unrecoverable."; then
    log "skipping state purge"
    return 0
  fi
  log "removing ${ISENGARD_PREFIX}"
  rm -rf "${ISENGARD_PREFIX}"

  if confirm "Also remove ${ISENGARD_ETC} (env file + compose.yaml)?"; then
    rm -rf "${ISENGARD_ETC}"
  fi
}

remove_network() {
  if ! docker network inspect "${ISENGARD_PROXY_NETWORK}" >/dev/null 2>&1; then
    warn "network ${ISENGARD_PROXY_NETWORK} does not exist; skipping"
    return 0
  fi
  # Compose's `down` should have detached our containers. If anything else is
  # still on the network (operator stacks, manual `docker run`s), refuse so
  # we don't break their routing.
  local attached
  attached="$(docker network inspect "${ISENGARD_PROXY_NETWORK}" \
    --format '{{range .Containers}}{{.Name}} {{end}}' | tr -s ' ')"
  if [[ -n "${attached// /}" ]]; then
    warn "containers still attached to ${ISENGARD_PROXY_NETWORK}: ${attached}"
    warn "leave the network in place or detach those containers first"
    return 0
  fi
  log "removing network ${ISENGARD_PROXY_NETWORK}"
  docker network rm "${ISENGARD_PROXY_NETWORK}" >/dev/null
}

main() {
  log "Isengard uninstall starting (purge=${PURGE}, network=${REMOVE_NETWORK})"
  if ! confirm "Stop iso-controller + iso-agent?"; then
    die "aborted"
  fi
  stop_stack
  if [[ "${PURGE}" -eq 1 ]]; then
    purge_state
  fi
  if [[ "${REMOVE_NETWORK}" -eq 1 ]]; then
    remove_network
  fi
  log "uninstall complete"
}

main "$@"
