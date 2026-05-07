#!/usr/bin/env bash
# Isengard standalone install. Brings up controller + agent on this host
# from pre-built GHCR images. No source checkout required.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh | bash
#
# Or for the cautious path (recommended for first-time installs):
#   curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh -o install.sh
#   less install.sh
#   bash install.sh
#
# Re-running this script is idempotent: existing dirs, network, env file,
# and compose.yaml are left in place; only missing pieces are created. The
# stack is brought up via `docker compose up -d`, which is itself idempotent.

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration: every path / ref the script touches is overridable via env.
# ---------------------------------------------------------------------------

# Where bind-mounted runtime state lives on the host. The controller's CA,
# sqlite, certs, and the agent's enrollment cert all live under here.
ISENGARD_PREFIX="${ISENGARD_PREFIX:-/var/lib/isengard}"

# Where the env file + compose.yaml live. Operators edit isengard.env in place.
ISENGARD_ETC="${ISENGARD_ETC:-/etc/isengard}"
ISENGARD_ENV_FILE="${ISENGARD_ENV_FILE:-${ISENGARD_ETC}/isengard.env}"
ISENGARD_COMPOSE_FILE="${ISENGARD_COMPOSE_FILE:-${ISENGARD_ETC}/compose.yaml}"

# Source ref for the install assets. Defaults to whatever branch the script
# was fetched from; override to pin to a tag (e.g. ISENGARD_REF=v0.3.5) once
# we ship one.
ISENGARD_REF="${ISENGARD_REF:-next}"
ISENGARD_RAW_BASE="${ISENGARD_RAW_BASE:-https://raw.githubusercontent.com/Weavers-Engineering/Isengard/${ISENGARD_REF}/install}"

# Shared docker network for the pingora proxy + every routed stack.
ISENGARD_PROXY_NETWORK="${ISENGARD_PROXY_NETWORK:-isengard-proxy}"

# ---------------------------------------------------------------------------
# Logging helpers. Plain text, no colors (broken on some piped CI logs).
# ---------------------------------------------------------------------------

log()  { printf '[isengard] %s\n' "$*"; }
warn() { printf '[isengard] WARN: %s\n' "$*" >&2; }
die()  { printf '[isengard] ERROR: %s\n' "$*" >&2; exit 1; }

# Trap unhandled errors so the operator sees the offending line instead of a
# silent `set -e` exit.
on_err() {
  local exit_code=$?
  local line=${1:-?}
  warn "install failed at line ${line} (exit ${exit_code})"
  warn "re-run with: bash -x ${BASH_SOURCE[0]:-install.sh}  for a verbose trace"
  exit "${exit_code}"
}
trap 'on_err ${LINENO}' ERR

# ---------------------------------------------------------------------------
# Preflight: every external dependency the script needs.
# ---------------------------------------------------------------------------

require_cmd() {
  local cmd="$1"
  command -v "${cmd}" >/dev/null 2>&1 || die "missing required command: ${cmd}"
}

preflight() {
  log "preflight: checking dependencies"
  require_cmd docker

  # `docker compose` (v2 plugin) vs the legacy `docker-compose` shim. We need
  # the v2 plugin for env_file + bind-mount semantics this compose file uses.
  if ! docker compose version >/dev/null 2>&1; then
    die "docker compose v2 plugin not found. Install via:
       https://docs.docker.com/compose/install/linux/"
  fi

  # Need either curl or wget to fetch compose.yaml from the source ref.
  if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
    die "neither curl nor wget is installed; one is needed to fetch compose.yaml"
  fi

  # Permission check. Default paths require root; override the defaults if you
  # want to run the whole stack as a non-privileged user.
  if [[ "${ISENGARD_PREFIX}" == /var/* || "${ISENGARD_ETC}" == /etc/* ]] && [[ "$(id -u)" -ne 0 ]]; then
    die "writing to ${ISENGARD_ETC} and ${ISENGARD_PREFIX} requires root.
       re-run with sudo, or override ISENGARD_PREFIX + ISENGARD_ETC to user-writable paths."
  fi

  log "preflight: docker $(docker version -f '{{.Server.Version}}' 2>/dev/null || echo unknown), compose $(docker compose version --short 2>/dev/null || echo unknown)"
}

# ---------------------------------------------------------------------------
# Step 1: scaffold host directories.
# ---------------------------------------------------------------------------

setup_dirs() {
  log "setup: ensuring ${ISENGARD_ETC} and ${ISENGARD_PREFIX} exist"
  mkdir -p "${ISENGARD_ETC}"
  mkdir -p "${ISENGARD_PREFIX}/controller"
  mkdir -p "${ISENGARD_PREFIX}/agent"
  mkdir -p "${ISENGARD_PREFIX}/stacks"
  # Tighten the env file's parent dir: it's about to hold passphrases.
  chmod 0750 "${ISENGARD_ETC}" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Step 2: write the env template if it doesn't exist.
# ---------------------------------------------------------------------------

# `fetch URL OUTPATH` writes the URL to OUTPATH using whichever of curl/wget
# is available. Refuses to overwrite an existing file (caller checks first).
fetch() {
  local url="$1"
  local out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "${url}" -o "${out}" || die "fetch failed: ${url}"
  else
    wget -qO "${out}" "${url}" || die "fetch failed: ${url}"
  fi
}

setup_env_file() {
  if [[ -f "${ISENGARD_ENV_FILE}" ]]; then
    log "env: ${ISENGARD_ENV_FILE} already exists; leaving in place"
    return 0
  fi

  log "env: writing template to ${ISENGARD_ENV_FILE}"
  # If we're being piped through `bash` from curl, isengard.env.example is not
  # on disk: fetch it from the ref. If we were invoked with the source tree
  # nearby (operator did a `git clone` or downloaded the install/ dir), prefer
  # the local copy so offline installs work.
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || echo "")"
  local local_template="${script_dir}/isengard.env.example"

  if [[ -f "${local_template}" ]]; then
    cp "${local_template}" "${ISENGARD_ENV_FILE}"
  else
    fetch "${ISENGARD_RAW_BASE}/isengard.env.example" "${ISENGARD_ENV_FILE}"
  fi
  chmod 0640 "${ISENGARD_ENV_FILE}"

  cat <<EOF

  =====================================================================
  Wrote env template to ${ISENGARD_ENV_FILE}
  Edit it now to set:
    - ISENGARD_ACME_EMAIL          (if you'll publish HTTPS routes)
    - ISENGARD_SECRETS_PASSPHRASE  (if you'll store stack secrets)
    - ISENGARD_BACKUP_PASSPHRASE   (if you'll enable backups)
    - ISENGARD_CF_DNS_API_TOKEN    (if you'll use DNS-01 wildcards)

  Then re-run this script to bring the stack up.
  =====================================================================

EOF
  # Exit 0 on first run: forcing the operator to fill in the env file before
  # we start anything is the safer default than starting with all-empty
  # passphrases.
  exit 0
}

# ---------------------------------------------------------------------------
# Step 3: drop compose.yaml in place.
# ---------------------------------------------------------------------------

setup_compose_file() {
  if [[ -f "${ISENGARD_COMPOSE_FILE}" ]]; then
    log "compose: ${ISENGARD_COMPOSE_FILE} already present; leaving in place"
    return 0
  fi

  log "compose: writing ${ISENGARD_COMPOSE_FILE}"
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || echo "")"
  local local_compose="${script_dir}/compose.yaml"

  if [[ -f "${local_compose}" ]]; then
    cp "${local_compose}" "${ISENGARD_COMPOSE_FILE}"
  else
    fetch "${ISENGARD_RAW_BASE}/compose.yaml" "${ISENGARD_COMPOSE_FILE}"
  fi
  chmod 0644 "${ISENGARD_COMPOSE_FILE}"
}

# ---------------------------------------------------------------------------
# Step 4: shared docker network.
# ---------------------------------------------------------------------------

setup_network() {
  if docker network inspect "${ISENGARD_PROXY_NETWORK}" >/dev/null 2>&1; then
    log "network: ${ISENGARD_PROXY_NETWORK} already exists"
    return 0
  fi
  log "network: creating ${ISENGARD_PROXY_NETWORK}"
  docker network create "${ISENGARD_PROXY_NETWORK}" >/dev/null
}

# ---------------------------------------------------------------------------
# Step 5: pull images + bring the stack up.
# ---------------------------------------------------------------------------

bring_up() {
  # Export the path overrides so compose's `${VAR:-default}` substitution
  # picks them up. Compose reads substitution variables from the shell env
  # (in addition to --env-file), so an export here overrides any defaults
  # baked into compose.yaml.
  export ISENGARD_PREFIX
  export ISENGARD_ENV_FILE
  export ISENGARD_COMPOSE_FILE

  log "images: pulling latest"
  docker compose --env-file "${ISENGARD_ENV_FILE}" -f "${ISENGARD_COMPOSE_FILE}" pull

  log "stack: bringing up via docker compose up -d"
  docker compose --env-file "${ISENGARD_ENV_FILE}" -f "${ISENGARD_COMPOSE_FILE}" up -d
}

# ---------------------------------------------------------------------------
# Step 6: print next steps.
# ---------------------------------------------------------------------------

post_install_hints() {
  cat <<EOF

  =====================================================================
  Isengard is up.

  Dashboard:  http://127.0.0.1:9418  (loopback by default)
  Logs:       docker logs -f iso-controller
              docker logs -f iso-agent

  To enroll the agent (first time only):
    1. Mint a token:
       docker exec iso-controller isengard controller token mint --role agent
    2. Paste the token into ${ISENGARD_ENV_FILE} as
       ISENGARD_ENROLL_TOKEN=<token>
       and re-run this script. Or pass it inline:
         ISENGARD_ENROLL_TOKEN=<token> bash install.sh

  Operator CLI (\`isd\`):
    Build once on a workstation:  cargo build -p isd --release
    Then:  isd login http://127.0.0.1:9418

  Docs:
    install/README.md  (this directory)
    docker/README.md   (background and conventions)
  =====================================================================

EOF
}

# ---------------------------------------------------------------------------
# Main.
# ---------------------------------------------------------------------------

main() {
  log "Isengard install starting (ref=${ISENGARD_REF}, prefix=${ISENGARD_PREFIX})"
  preflight
  setup_dirs
  # On first run setup_env_file writes the template and exits 0; the operator
  # fills it in and re-runs. On subsequent runs the existing file is left
  # alone and we proceed to the rest of the steps.
  setup_env_file
  setup_compose_file
  setup_network
  bring_up
  post_install_hints
}

main "$@"
