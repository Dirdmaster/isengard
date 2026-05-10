#!/usr/bin/env bash
# Isengard systemd-native install (Phase 0.8 default).
#
# Brings up the controller + agent on this host as systemd services from
# pre-built musl static binaries on GitHub Releases. NO docker, NO compose.
# The agent uses wisp (clone3 + cgroup v2 + iptables) to manage workload
# containers; the host runs only systemd and the isengard binary.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh | sudo bash
#
# Or for the cautious path (recommended for first-time installs):
#   curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh -o install.sh
#   less install.sh
#   sudo bash install.sh
#
# First run:
#   1. Detects host arch (x86_64 / aarch64) and downloads
#      isengard-<arch>-unknown-linux-musl from the requested release tag.
#      Verifies the sha256 sidecar before installing to /usr/local/bin.
#   2. Generates a 32-byte random master key at /etc/isengard/master.key
#      (mode 0600 root). Operator never sees the value.
#   3. Interactively prompts for individual secret values (Cloudflare DNS
#      API token, backup passphrase, ...). Each value is piped through
#      `isengard secret bootstrap <name>` which encrypts with the master
#      key and writes ciphertext to the controller's SQLite. Plaintext
#      NEVER touches a file on the host.
#   4. Prompts for non-secret config (ACME email, domains, directory) and
#      writes /etc/isengard/isengard.env.
#   5. Installs the systemd units, enables iso-controller, mints an
#      enrollment token, exports the controller CA, hands the token to
#      iso-agent via /etc/isengard/agent-token.env.
#   6. systemctl enable --now iso-controller.service iso-agent.service
#
# Re-runs (existing install detected): present a refresh menu so the
# operator can refresh the binary, refresh non-secret config, wipe
# everything, or abort. Set ISENGARD_REINSTALL_MODE to one of
# refresh-binary|refresh-config|wipe|abort to skip the prompt.
#
# For the legacy docker-compose flow, run install/install-docker.sh.

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration: every path / ref the script touches is overridable via env.
# ---------------------------------------------------------------------------

# Where bind-style runtime state lives on the host. The controller's CA,
# sqlite, certs, and the agent's enrollment cert all live under here.
ISENGARD_PREFIX="${ISENGARD_PREFIX:-/var/lib/isengard}"

# Where the env files + systemd units live. Operators edit isengard.env
# in place; secret values stay encrypted in SQLite.
ISENGARD_ETC="${ISENGARD_ETC:-/etc/isengard}"
ISENGARD_ENV_FILE="${ISENGARD_ENV_FILE:-${ISENGARD_ETC}/isengard.env}"
ISENGARD_TOKEN_FILE="${ISENGARD_TOKEN_FILE:-${ISENGARD_ETC}/agent-token.env}"
ISENGARD_MASTER_KEY="${ISENGARD_MASTER_KEY:-${ISENGARD_ETC}/master.key}"
ISENGARD_CA_FILE="${ISENGARD_CA_FILE:-${ISENGARD_ETC}/ca.pem}"

# Source ref for the install assets (systemd units, README). Defaults to
# whatever branch this script was fetched from; override to pin a tag.
ISENGARD_REF="${ISENGARD_REF:-next}"
ISENGARD_RAW_BASE="${ISENGARD_RAW_BASE:-https://raw.githubusercontent.com/Weavers-Engineering/Isengard/${ISENGARD_REF}/install}"

# GitHub Release tag to fetch the static binary from. `latest` picks up
# the newest release. Pin to e.g. v0.4.0 once we ship a tagged release.
ISENGARD_VERSION="${ISENGARD_VERSION:-latest}"
ISENGARD_RELEASE_BASE="${ISENGARD_RELEASE_BASE:-https://github.com/Weavers-Engineering/Isengard/releases/download}"

# Where the binary lands. `install -m 0755` overwrites if present (the
# refresh-binary action drops a new build in here).
ISENGARD_BIN_DIR="${ISENGARD_BIN_DIR:-/usr/local/bin}"
ISENGARD_BIN="${ISENGARD_BIN_DIR}/isengard"

# systemd unit install path. /etc/systemd/system/ is the operator-owned
# tier (preferred over /lib/systemd/system, which is reserved for the
# distro package manager).
SYSTEMD_DIR="${SYSTEMD_DIR:-/etc/systemd/system}"

# ---------------------------------------------------------------------------
# Logging helpers. Plain text, no colors.
# ---------------------------------------------------------------------------

log()  { printf '[isengard] %s\n' "$*"; }
warn() { printf '[isengard] WARN: %s\n' "$*" >&2; }
die()  { printf '[isengard] ERROR: %s\n' "$*" >&2; exit 1; }

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

# Resolve the release-asset target triple for this host's CPU.
# Echoes the triple to stdout; dies on unsupported arch.
detect_target() {
  local arch
  arch="$(uname -m)"
  case "${arch}" in
    x86_64|amd64)
      printf 'x86_64-unknown-linux-musl'
      ;;
    aarch64|arm64)
      printf 'aarch64-unknown-linux-musl'
      ;;
    *)
      die "unsupported arch: ${arch} (only x86_64 and aarch64 are released)"
      ;;
  esac
}

preflight() {
  log "preflight: checking dependencies"
  require_cmd openssl
  require_cmd systemctl
  require_cmd install
  require_cmd sha256sum
  if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
    die "neither curl nor wget is installed; one is needed to fetch the binary"
  fi

  # Default paths require root.
  if [[ "${ISENGARD_PREFIX}" == /var/* || "${ISENGARD_ETC}" == /etc/* || "${ISENGARD_BIN_DIR}" == /usr/* ]] \
    && [[ "$(id -u)" -ne 0 ]]; then
    die "writing to ${ISENGARD_ETC}, ${ISENGARD_PREFIX}, ${ISENGARD_BIN_DIR} requires root.
       re-run with sudo, or override ISENGARD_PREFIX + ISENGARD_ETC + ISENGARD_BIN_DIR to user-writable paths."
  fi

  # Linux check: systemd is Linux-only, so the script makes no sense on
  # macOS / *BSD. Better to fail early than chase weird errors later.
  local kernel
  kernel="$(uname -s)"
  if [[ "${kernel}" != "Linux" ]]; then
    die "this script targets systemd on Linux; got ${kernel}. For dev on macOS see docker/README.md."
  fi

  log "preflight: target=$(detect_target), version=${ISENGARD_VERSION}"
}

# ---------------------------------------------------------------------------
# fetch URL OUTPATH writes URL to OUTPATH using whichever of curl / wget
# is available. Refuses to overwrite an existing file (caller checks first).
# ---------------------------------------------------------------------------

fetch() {
  local url="$1"
  local out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "${url}" -o "${out}" || die "fetch failed: ${url}"
  else
    wget -qO "${out}" "${url}" || die "fetch failed: ${url}"
  fi
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
  # Wisp host-side state. The agent writes pulled images, bundles, and
  # network IPAM data under here.
  mkdir -p /var/lib/wisp
  # Tighten the etc dir: it's about to hold the master key file.
  chmod 0750 "${ISENGARD_ETC}" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Step 2: download + install the isengard binary.
# ---------------------------------------------------------------------------

# Download isengard-<target> and isengard-<target>.sha256, verify, install
# to ISENGARD_BIN. Idempotent: a second call overwrites with whatever is
# at the requested release tag (refresh-binary action calls this directly).
install_binary() {
  local target url sha_url tmp tmp_sha
  target="$(detect_target)"
  url="${ISENGARD_RELEASE_BASE}/${ISENGARD_VERSION}/isengard-${target}"
  sha_url="${url}.sha256"

  # When the operator is testing against an unreleased build, ISENGARD_LOCAL_BIN
  # short-circuits the download. Same shape as the legacy install-docker flow.
  if [[ -n "${ISENGARD_LOCAL_BIN:-}" ]]; then
    if [[ ! -x "${ISENGARD_LOCAL_BIN}" ]]; then
      die "ISENGARD_LOCAL_BIN=${ISENGARD_LOCAL_BIN} is not executable"
    fi
    log "binary: ISENGARD_LOCAL_BIN=${ISENGARD_LOCAL_BIN}; copying to ${ISENGARD_BIN}"
    install -m 0755 "${ISENGARD_LOCAL_BIN}" "${ISENGARD_BIN}"
    return 0
  fi

  log "binary: fetching ${url}"
  tmp="$(mktemp)"
  tmp_sha="$(mktemp)"
  # Best-effort cleanup on any path out of this function.
  trap 'rm -f "${tmp}" "${tmp_sha}"' RETURN
  fetch "${url}" "${tmp}"
  fetch "${sha_url}" "${tmp_sha}"

  # The published .sha256 sidecar's checksum line names a path relative
  # to wherever the workflow generated it (e.g. `dist/isengard-<target>`).
  # Recompute the digest of what we just downloaded and compare against
  # the first whitespace-delimited field in the sidecar.
  local got expected
  got="$(sha256sum "${tmp}" | awk '{print $1}')"
  expected="$(awk '{print $1}' <"${tmp_sha}")"
  if [[ -z "${expected}" ]]; then
    die "binary: empty sha256 sidecar at ${sha_url}"
  fi
  if [[ "${got}" != "${expected}" ]]; then
    die "binary: sha256 mismatch (got ${got}, expected ${expected}); refusing to install"
  fi
  log "binary: sha256 verified (${expected})"

  install -m 0755 "${tmp}" "${ISENGARD_BIN}"
  log "binary: installed ${ISENGARD_BIN} ($(${ISENGARD_BIN} --version 2>/dev/null || echo unknown))"
}

# ---------------------------------------------------------------------------
# Step 3: master key (generated once on first run, never overwritten).
# ---------------------------------------------------------------------------

master_key_ready() {
  if [[ ! -f "${ISENGARD_MASTER_KEY}" ]]; then
    return 1
  fi
  local size
  size=$(wc -c <"${ISENGARD_MASTER_KEY}" | tr -d '[:space:]')
  if [[ "${size}" != "32" ]]; then
    warn "${ISENGARD_MASTER_KEY} exists but is ${size} bytes (expected 32). Refusing to overwrite."
    die  "delete ${ISENGARD_MASTER_KEY} manually, then re-run install.sh."
  fi
  return 0
}

setup_master_key() {
  if master_key_ready; then
    log "key: ${ISENGARD_MASTER_KEY} already present"
    return 0
  fi
  log "key: generating fresh 32-byte master key at ${ISENGARD_MASTER_KEY}"
  openssl rand 32 >"${ISENGARD_MASTER_KEY}"
  chmod 0600 "${ISENGARD_MASTER_KEY}"
  chown 0:0 "${ISENGARD_MASTER_KEY}" 2>/dev/null || true
  log "key: master key created. Operator never sees the value; back up the file out of band."
}

# Write /etc/isengard/master-key.env so systemd's EnvironmentFile= picks
# up the controller's master key path. Optional; the binary also accepts
# --master-key-file directly when invoked outside systemd.
write_master_key_env() {
  local out="${ISENGARD_ETC}/master-key.env"
  if [[ -f "${out}" ]]; then
    return 0
  fi
  log "key: writing ${out}"
  umask 0077
  cat >"${out}" <<EOF
# Auto-generated by install.sh. Sourced by iso-controller.service via
# EnvironmentFile=. The leading -EnvironmentFile in the unit makes it
# tolerant of this file being absent (older installs lacked it).
ISENGARD_MASTER_KEY_FILE=${ISENGARD_MASTER_KEY}
EOF
  chmod 0600 "${out}"
}

# ---------------------------------------------------------------------------
# Step 4: interactive secret bootstrap.
# Prompts the operator for each named secret; pipes the value into
# `isengard secret bootstrap <name>` which encrypts with the master key
# and writes ciphertext to SQLite.
# ---------------------------------------------------------------------------

# bootstrap_secret <name> <prompt> [<allow_empty>]
bootstrap_secret() {
  local name="$1"
  local prompt="$2"
  local allow_empty="${3:-yes}"

  local value=""
  while :; do
    printf '  %s' "${prompt}"
    if [[ "${allow_empty}" == "yes" ]]; then
      printf ' (press Enter to skip)'
    fi
    printf ': '
    # /dev/tty hand-off so curl | sudo bash works.
    if exec 9</dev/tty 2>/dev/null; then
      :
    elif [[ -t 0 ]]; then
      exec 9<&0
    else
      die "no controlling terminal available for secret input; run 'sudo bash install.sh' from an interactive shell"
    fi
    value=""
    local char
    while IFS= read -rsn1 -u 9 char; do
      if [[ -z "${char}" ]]; then
        break
      fi
      if [[ "${char}" == $'\x7f' || "${char}" == $'\b' ]]; then
        if [[ -n "${value}" ]]; then
          value="${value%?}"
          printf '\b \b'
        fi
        continue
      fi
      if [[ "${char}" < ' ' ]]; then
        continue
      fi
      value="${value}${char}"
      printf '*'
    done
    printf '\n'

    if [[ -z "${value}" && "${allow_empty}" != "yes" ]]; then
      warn "value cannot be empty"
      continue
    fi
    if [[ -n "${value}" ]]; then
      printf '    (%d characters captured)\n' "${#value}"
    else
      printf '    (skipped)\n'
    fi
    break
  done

  if [[ -z "${value}" ]]; then
    log "  skip: ${name}"
    return 0
  fi

  log "  bootstrap: ${name}"
  printf '%s' "${value}" | "${ISENGARD_BIN}" secret bootstrap "${name}" \
    --master-key-file "${ISENGARD_MASTER_KEY}" \
    --state-dir "${ISENGARD_PREFIX}/controller" >/dev/null

  value=""
}

bootstrap_secrets_if_first_run() {
  if [[ -f "${ISENGARD_PREFIX}/controller/isengard.db" ]]; then
    log "secrets: existing controller DB at ${ISENGARD_PREFIX}/controller/isengard.db; skipping interactive prompt"
    log "secrets: manage with 'isd secret put|list|rm' against the running dashboard"
    return 0
  fi

  cat <<EOF

  =====================================================================
  Bootstrapping secrets. Each value is encrypted with the master key
  and written to the controller's SQLite. Plaintext is NEVER stored
  on disk; values you enter here are not echoed and not logged.

  Press Enter on an empty line to skip any optional secret.
  =====================================================================

EOF
  bootstrap_secret cf_dns_api_token "Cloudflare DNS API token (DNS-01 wildcards)" yes
  bootstrap_secret backup_passphrase "Backup passphrase (encrypted snapshots)" yes
  log "secrets: done"
}

# ---------------------------------------------------------------------------
# Step 5: non-secret config -> /etc/isengard/isengard.env.
# ---------------------------------------------------------------------------

prompt_plain_config() {
  local force=0
  case "${1:-}" in
    1|--force|force) force=1 ;;
  esac

  if [[ -f "${ISENGARD_ENV_FILE}" && "${force}" -eq 0 ]]; then
    log "env: ${ISENGARD_ENV_FILE} already exists; leaving in place"
    return 0
  fi
  if [[ "${force}" -eq 1 && -f "${ISENGARD_ENV_FILE}" ]]; then
    log "env: refreshing ${ISENGARD_ENV_FILE} (re-prompting for plain config; secrets untouched)"
  fi

  log "env: prompting for non-secret config (visible input)"

  local acme_email=""
  local acme_domains=""
  local acme_directory="staging"

  local input_fd=""
  if exec 9</dev/tty 2>/dev/null; then
    input_fd=9
  elif [[ -t 0 ]]; then
    input_fd=0
  fi

  if [[ -n "${input_fd}" ]]; then
    printf '  ACME contact email (leave blank for internal-only deploys): '
    IFS= read -r -u "${input_fd}" acme_email || acme_email=""
    printf '  ACME pre-issue domains, comma-separated (e.g. *.example.com,foo.example.com): '
    IFS= read -r -u "${input_fd}" acme_domains || acme_domains=""
    printf "  ACME environment [staging/production] (default staging): "
    local input=""
    IFS= read -r -u "${input_fd}" input || input=""
    case "${input,,}" in
      "" | staging | stage) acme_directory="staging" ;;
      production | prod) acme_directory="production" ;;
      *) acme_directory="${input}" ;;
    esac
  else
    warn "no controlling terminal; writing env file with empty defaults"
  fi

  log "env: writing template to ${ISENGARD_ENV_FILE}"
  umask 0022
  cat >"${ISENGARD_ENV_FILE}" <<EOF
# Isengard non-secret config (Phase 0.8 systemd-native install).
#
# Sourced by iso-controller.service + iso-agent.service via systemd's
# EnvironmentFile=. Secrets (Cloudflare API token, backup passphrase,
# ...) live encrypted in the controller's SQLite, keyed by
# /etc/isengard/master.key. Manage them via:
#   isd secret put <name>             # while the stack is up
#   isengard secret bootstrap <name>  # at install time, see install.sh

# ACME contact email. Required if you publish public HTTPS routes.
ISENGARD_ACME_EMAIL=${acme_email}

# Comma-separated hostnames the agent should pre-issue certs for.
# Wildcards (*.example.com) require DNS-01 (which uses the
# 'cf_dns_api_token' bootstrapped secret).
ISENGARD_ACME_DOMAINS=${acme_domains}

# ACME environment. Accepts:
#   staging | stage             -> LE staging (default; un-trusted by browsers)
#   production | prod | <empty> -> LE production (real browser-trusted certs)
#   <URL>                       -> raw directory URL (custom CA, Pebble, etc.)
ISENGARD_ACME_DIRECTORY=${acme_directory}

# Wisp is the only supported runtime under the systemd-native install.
# Setting this here so anything that reads the env file (operator
# scripts, debug tooling) can see the active backend.
ISENGARD_RUNTIME=wisp

# Default log level. Override to debug for short-term troubleshooting.
RUST_LOG=info
EOF
  chmod 0644 "${ISENGARD_ENV_FILE}"
}

# ---------------------------------------------------------------------------
# Step 6: install systemd units.
# ---------------------------------------------------------------------------

# Source-or-fetch a single unit file, write to ${SYSTEMD_DIR}/<name>.
# Prefers a sibling install/systemd/<name> when the script lives in a
# checkout (smoke tests, dev installs); falls back to a raw GitHub fetch.
install_unit() {
  local name="$1"
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || echo "")"
  local local_unit="${script_dir}/systemd/${name}"
  local target="${SYSTEMD_DIR}/${name}"
  if [[ -f "${local_unit}" ]]; then
    install -m 0644 "${local_unit}" "${target}"
  else
    fetch "${ISENGARD_RAW_BASE}/systemd/${name}" "${target}"
    chmod 0644 "${target}"
  fi
  log "systemd: installed ${target}"
}

setup_systemd_units() {
  install_unit iso-controller.service
  install_unit iso-agent.service
  install_unit iso-agent.target
  systemctl daemon-reload
}

# ---------------------------------------------------------------------------
# Step 7: bring up controller, mint enrollment token, export CA, bring up agent.
# ---------------------------------------------------------------------------

# Start iso-controller.service via systemctl. Retries the readiness
# probe (an `isengard controller ca export` succeeds once the CA is
# initialized) for up to 30s before giving up.
start_controller() {
  log "controller: starting iso-controller.service"
  systemctl enable iso-controller.service >/dev/null 2>&1 || true
  systemctl start iso-controller.service

  log "controller: waiting for CA initialization (up to 30s)"
  local i
  for i in $(seq 1 30); do
    if "${ISENGARD_BIN}" controller --state-dir "${ISENGARD_PREFIX}/controller" ca export >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  die "controller: timed out waiting for CA. Check 'systemctl status iso-controller' and 'journalctl -u iso-controller'."
}

# Export the controller's root CA to /etc/isengard/ca.pem. The agent
# pins this for the bootstrap mTLS handshake on first enroll.
export_controller_ca() {
  log "ca: exporting controller CA to ${ISENGARD_CA_FILE}"
  local tmp
  tmp="$(mktemp)"
  if "${ISENGARD_BIN}" controller --state-dir "${ISENGARD_PREFIX}/controller" ca export >"${tmp}" 2>/dev/null; then
    if grep -q "BEGIN CERTIFICATE" "${tmp}"; then
      install -m 0644 "${tmp}" "${ISENGARD_CA_FILE}"
      log "ca: wrote $(wc -l < "${ISENGARD_CA_FILE}") lines to ${ISENGARD_CA_FILE}"
      rm -f "${tmp}"
    else
      rm -f "${tmp}"
      die "ca: controller exported empty / non-PEM output"
    fi
  else
    rm -f "${tmp}"
    die "ca: failed to export CA from controller; check the controller logs"
  fi
}

# Mint a fresh enrollment token and write it to /etc/isengard/agent-token.env
# so iso-agent.service picks it up via EnvironmentFile=. The token has a
# 15-minute TTL by default; the agent consumes it on first start, persists
# its mTLS cert, and subsequent restarts ignore the env value.
mint_agent_token() {
  if [[ -f "${ISENGARD_PREFIX}/agent/agent.json" ]]; then
    log "token: agent already enrolled (${ISENGARD_PREFIX}/agent/agent.json present); skipping mint"
    return 0
  fi

  log "token: minting enrollment token (--format token, 15m ttl)"
  local token
  if ! token="$("${ISENGARD_BIN}" controller --state-dir "${ISENGARD_PREFIX}/controller" \
      token mint --role agent --ttl 15m --format token 2>/dev/null)"; then
    die "token: mint failed; check controller status"
  fi
  if [[ -z "${token}" ]]; then
    die "token: mint produced no output"
  fi

  log "token: writing ${ISENGARD_TOKEN_FILE}"
  umask 0077
  cat >"${ISENGARD_TOKEN_FILE}" <<EOF
# Auto-generated by install.sh. The agent consumes this on first start,
# persists its mTLS cert under ${ISENGARD_PREFIX}/agent, and ignores
# this file on subsequent restarts. Safe to delete after enrollment.
ISENGARD_ENROLL_TOKEN=${token}
ISENGARD_CONTROLLER_CA_PEM_PATH=${ISENGARD_CA_FILE}
EOF
  chmod 0600 "${ISENGARD_TOKEN_FILE}"
}

start_agent() {
  log "agent: starting iso-agent.service"
  systemctl enable iso-agent.service >/dev/null 2>&1 || true
  systemctl start iso-agent.service
}

bring_up_stack() {
  start_controller
  export_controller_ca
  mint_agent_token
  start_agent
}

# ---------------------------------------------------------------------------
# Reinstall / refresh menu.
# ---------------------------------------------------------------------------

# Returns one of: none | partial | complete
detect_existing() {
  local key=0 env=0 db=0 binary=0 unit=0
  [[ -f "${ISENGARD_MASTER_KEY}" ]] && key=1
  [[ -f "${ISENGARD_ENV_FILE}" ]] && env=1
  [[ -f "${ISENGARD_PREFIX}/controller/isengard.db" ]] && db=1
  [[ -x "${ISENGARD_BIN}" ]] && binary=1
  [[ -f "${SYSTEMD_DIR}/iso-controller.service" ]] && unit=1

  local total=$((key + env + db + binary + unit))
  if [[ "${total}" -eq 0 ]]; then
    printf 'none'
  elif [[ "${key}" -eq 1 && "${env}" -eq 1 && "${binary}" -eq 1 && "${unit}" -eq 1 ]]; then
    printf 'complete'
  else
    printf 'partial'
  fi
}

# Reports the status of a named systemd unit.
unit_state() {
  local name="$1"
  if ! command -v systemctl >/dev/null 2>&1; then
    printf 'unknown'
    return 0
  fi
  local state
  state="$(systemctl is-active "${name}" 2>/dev/null || true)"
  if [[ -z "${state}" ]]; then
    printf 'absent'
  else
    printf '%s' "${state}"
  fi
}

print_existing_report() {
  local key_size="absent"
  if [[ -f "${ISENGARD_MASTER_KEY}" ]]; then
    key_size="$(wc -c <"${ISENGARD_MASTER_KEY}" 2>/dev/null | tr -d '[:space:]') bytes"
  fi
  local env_state="present"
  [[ -f "${ISENGARD_ENV_FILE}" ]] || env_state="absent"
  local db_state="absent"
  [[ -f "${ISENGARD_PREFIX}/controller/isengard.db" ]] && db_state="present"
  local binary_version="absent"
  if [[ -x "${ISENGARD_BIN}" ]]; then
    binary_version="$(${ISENGARD_BIN} --version 2>/dev/null || echo unknown)"
  fi
  local controller_state agent_state
  controller_state="$(unit_state iso-controller.service)"
  agent_state="$(unit_state iso-agent.service)"

  cat <<EOF

[isengard] existing install detected:
  binary:        ${ISENGARD_BIN} (${binary_version})
  master.key:    ${ISENGARD_MASTER_KEY} (${key_size}, mode 0600)
  isengard.env:  ${ISENGARD_ENV_FILE} (${env_state})
  secrets DB:    ${ISENGARD_PREFIX}/controller/isengard.db (${db_state})
  controller:    iso-controller.service (${controller_state})
  agent:         iso-agent.service (${agent_state})

EOF
}

_normalize_action() {
  case "${1:-}" in
    1|refresh-binary|binary) printf 'refresh-binary' ;;
    2|refresh-config|env|config) printf 'refresh-config' ;;
    3|wipe|reinstall) printf 'wipe' ;;
    4|abort|cancel|exit) printf 'abort' ;;
    "") printf '' ;;
    *) printf 'invalid' ;;
  esac
}

_prompt_menu_choice() {
  local input_fd=""
  if { exec 9</dev/tty; } 2>/dev/null; then
    input_fd=9
  elif [[ -t 0 ]]; then
    input_fd=0
  fi

  if [[ -z "${input_fd}" ]]; then
    warn "no controlling terminal; aborting reinstall (set ISENGARD_REINSTALL_MODE for non-interactive)"
    printf 'abort'
    return 0
  fi

  local raw=""
  printf '  Choice [1]: ' >&2
  IFS= read -r -u "${input_fd}" raw || raw=""
  if [[ -z "${raw}" ]]; then
    raw="1"
  fi
  local action
  action="$(_normalize_action "${raw}")"
  printf '%s' "${action}"
}

reinstall_menu() {
  print_existing_report

  cat <<EOF
  1) Refresh binary only                   (re-downloads ${ISENGARD_BIN}, restarts services)
  2) Refresh binary + isengard.env         (re-prompts ACME values; secrets + master.key untouched)
  3) Wipe everything and reinstall         (DESTRUCTIVE: deletes secrets + master.key + state)
  4) Abort

EOF

  local action=""
  if [[ -n "${ISENGARD_REINSTALL_MODE:-}" ]]; then
    action="$(_normalize_action "${ISENGARD_REINSTALL_MODE}")"
    log "reinstall: ISENGARD_REINSTALL_MODE=${ISENGARD_REINSTALL_MODE} -> ${action}"
    if [[ "${action}" == "invalid" || -z "${action}" ]]; then
      die "ISENGARD_REINSTALL_MODE must be one of: refresh-binary, refresh-config, wipe, abort"
    fi
  else
    while :; do
      action="$(_prompt_menu_choice)"
      if [[ "${action}" == "invalid" ]]; then
        warn "unknown choice; pick 1, 2, 3, or 4"
        continue
      fi
      if [[ -z "${action}" ]]; then
        action="refresh-binary"
      fi
      break
    done
    log "reinstall: choice=${action}"
  fi

  case "${action}" in
    refresh-binary)  action_refresh_binary ;;
    refresh-config)  action_refresh_config ;;
    wipe)            action_wipe ;;
    abort)           log "reinstall: aborted, no changes made"; exit 0 ;;
    *)               die "internal error: unknown action ${action}" ;;
  esac
}

action_refresh_binary() {
  log "action: refresh binary only"
  install_binary
  setup_systemd_units
  if [[ -n "${ISENGARD_SKIP_BRING_UP:-}" ]]; then
    log "stack: ISENGARD_SKIP_BRING_UP set; skipping restart"
    return 0
  fi
  log "stack: restarting services"
  systemctl daemon-reload
  systemctl restart iso-controller.service
  systemctl restart iso-agent.service 2>/dev/null || true
  log "action: refresh binary complete"
}

action_refresh_config() {
  log "action: refresh binary + isengard.env"
  install_binary
  setup_systemd_units

  if [[ -f "${ISENGARD_ENV_FILE}" ]]; then
    cp "${ISENGARD_ENV_FILE}" "${ISENGARD_ENV_FILE}.bak"
    log "env: backed up old env to ${ISENGARD_ENV_FILE}.bak"
  fi
  prompt_plain_config --force

  if [[ -n "${ISENGARD_SKIP_BRING_UP:-}" ]]; then
    log "stack: ISENGARD_SKIP_BRING_UP set; skipping restart"
    return 0
  fi
  systemctl daemon-reload
  systemctl restart iso-controller.service
  systemctl restart iso-agent.service 2>/dev/null || true
  log "action: refresh config complete"
}

action_wipe() {
  log "action: wipe everything and reinstall (DESTRUCTIVE)"

  if [[ "${ISENGARD_WIPE_YES:-0}" != "1" ]]; then
    local input_fd=""
    if { exec 9</dev/tty; } 2>/dev/null; then
      input_fd=9
    elif [[ -t 0 ]]; then
      input_fd=0
    fi
    if [[ -z "${input_fd}" ]]; then
      die "wipe requires a TTY for confirmation; set ISENGARD_WIPE_YES=1 to bypass"
    fi
    local confirm=""
    printf '  type WIPE to confirm: ' >&2
    IFS= read -r -u "${input_fd}" confirm || confirm=""
    if [[ "${confirm}" != "WIPE" ]]; then
      log "reinstall: wipe cancelled (got '${confirm}')"
      exit 0
    fi
  else
    log "reinstall: ISENGARD_WIPE_YES=1, skipping interactive WIPE confirmation"
  fi

  _safe_rm_rf() {
    local target="$1"
    if [[ -z "${target}" || "${target}" == "/" || "${#target}" -lt 4 ]]; then
      die "refusing rm -rf on suspicious path: '${target}'"
    fi
    if [[ "${target:0:1}" != "/" ]]; then
      die "refusing rm -rf on non-absolute path: '${target}'"
    fi
    if [[ -e "${target}" ]]; then
      rm -rf -- "${target}"
    fi
  }

  log "wipe: stopping units"
  systemctl stop iso-agent.service 2>/dev/null || true
  systemctl stop iso-controller.service 2>/dev/null || true
  systemctl disable iso-agent.service 2>/dev/null || true
  systemctl disable iso-controller.service 2>/dev/null || true

  log "wipe: removing systemd units"
  rm -f "${SYSTEMD_DIR}/iso-controller.service"
  rm -f "${SYSTEMD_DIR}/iso-agent.service"
  rm -f "${SYSTEMD_DIR}/iso-agent.target"
  systemctl daemon-reload

  log "wipe: removing ${ISENGARD_ETC}"
  _safe_rm_rf "${ISENGARD_ETC}"
  log "wipe: removing ${ISENGARD_PREFIX}"
  _safe_rm_rf "${ISENGARD_PREFIX}"
  log "wipe: removing /var/lib/wisp"
  _safe_rm_rf /var/lib/wisp
  log "wipe: removing ${ISENGARD_BIN}"
  rm -f "${ISENGARD_BIN}"

  log "wipe: complete; running first-time install"
  fresh_install
}

# ---------------------------------------------------------------------------
# Fresh-install path.
# ---------------------------------------------------------------------------

fresh_install() {
  setup_dirs
  install_binary
  setup_master_key
  write_master_key_env
  bootstrap_secrets_if_first_run
  prompt_plain_config
  setup_systemd_units
  if [[ -n "${ISENGARD_SKIP_BRING_UP:-}" ]]; then
    log "stack: ISENGARD_SKIP_BRING_UP set; skipping bring-up"
    return 0
  fi
  bring_up_stack
  post_install_hints
}

# ---------------------------------------------------------------------------
# Step 8: print next steps.
# ---------------------------------------------------------------------------

post_install_hints() {
  cat <<EOF

  =====================================================================
  Isengard is up.

  Dashboard:  http://127.0.0.1:9418  (loopback by default)
  Logs:       journalctl -u iso-controller -f
              journalctl -u iso-agent -f

  Status:     systemctl status iso-controller iso-agent

  Secrets:
    - Master key:     ${ISENGARD_MASTER_KEY} (mode 0600 root)
    - Encrypted DB:   ${ISENGARD_PREFIX}/controller/isengard.db
    - Add more:       isd secret put <name>     (against running dashboard)

  To enroll an additional agent on another host:
    1. Mint a token here:
       isengard controller token mint --state-dir ${ISENGARD_PREFIX}/controller --role agent
    2. On the other host, run install.sh and paste the token when prompted
       (or write it to ${ISENGARD_TOKEN_FILE} before first start).

  Operator CLI:
    - On this server: install isd from the same release tag:
        VERSION=${ISENGARD_VERSION}
        TARGET=$(detect_target)
        curl -fsSL "${ISENGARD_RELEASE_BASE}/\${VERSION}/isd-\${TARGET}" \\
          -o /usr/local/bin/isd && chmod +x /usr/local/bin/isd
    - Then: isd login http://127.0.0.1:9418
            isd ps; isd route list; isd secret list

  Docs:
    install/README.md
  =====================================================================

EOF
}

# ---------------------------------------------------------------------------
# Main.
# ---------------------------------------------------------------------------

main() {
  log "Isengard install starting (ref=${ISENGARD_REF}, version=${ISENGARD_VERSION}, prefix=${ISENGARD_PREFIX})"
  preflight

  local state
  state="$(detect_existing)"
  log "detect: existing install state = ${state}"

  case "${state}" in
    none)
      fresh_install
      ;;
    partial|complete)
      reinstall_menu
      ;;
    *)
      die "internal error: detect_existing returned '${state}'"
      ;;
  esac
}

main "$@"
