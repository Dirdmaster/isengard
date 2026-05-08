#!/usr/bin/env bash
# Isengard standalone install. Brings up controller + agent on this host
# from pre-built GHCR images. No source checkout required.
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
#   1. Generates a 32-byte random master key at /etc/isengard/master.key
#      (mode 0600 root). The operator never types or sees it.
#   2. Interactively prompts for individual secret values (Cloudflare
#      DNS API token, backup passphrase, ...). Each entered value is
#      piped to `isengard secret bootstrap <name>` which encrypts it
#      with the master key and writes the ciphertext to the controller's
#      SQLite. Plaintext NEVER touches a file on the host.
#   3. Prompts for non-secret config (ACME email, ACME domains, ACME
#      directory) and writes /etc/isengard/isengard.env.
#   4. Pulls images, creates the docker network, brings up the stack.
#
# Re-runs (master key already present): skips key generation and secret
# prompts; just brings up the stack. To re-prompt, delete
# /etc/isengard/master.key (rotates: every existing secret row becomes
# undecryptable, so don't do this on a populated stack).

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
ISENGARD_MASTER_KEY="${ISENGARD_MASTER_KEY:-${ISENGARD_ETC}/master.key}"

# Source ref for the install assets. Defaults to whatever branch the script
# was fetched from; override to pin to a tag (e.g. ISENGARD_REF=v0.3.5) once
# we ship one.
ISENGARD_REF="${ISENGARD_REF:-next}"
ISENGARD_RAW_BASE="${ISENGARD_RAW_BASE:-https://raw.githubusercontent.com/Weavers-Engineering/Isengard/${ISENGARD_REF}/install}"

# Shared docker network for the pingora proxy + every routed stack.
ISENGARD_PROXY_NETWORK="${ISENGARD_PROXY_NETWORK:-isengard-proxy}"

# Image used for one-shot bootstrap subcommands (`isengard secret bootstrap`).
# Same image as the controller; we run it with `--rm` and the same bind-mounts
# the controller will use, so the encrypted SQLite is written exactly where
# the running controller will read it.
ISENGARD_CONTROLLER_IMAGE="${ISENGARD_CONTROLLER_IMAGE:-ghcr.io/weavers-engineering/isengard-controller:${ISENGARD_IMAGE_TAG:-next}}"

# ---------------------------------------------------------------------------
# Logging helpers. Plain text, no colors (broken on some piped CI logs).
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

preflight() {
  log "preflight: checking dependencies"
  require_cmd openssl

  # Smoke-test mode (ISENGARD_LOCAL_BIN set, ISENGARD_SKIP_BRING_UP set)
  # exercises the master key + bootstrap + env path without needing
  # docker. Skip the docker checks in that mode so contributors can run
  # `bash install/install.sh` without a docker daemon.
  if [[ -z "${ISENGARD_LOCAL_BIN:-}" || -z "${ISENGARD_SKIP_BRING_UP:-}" ]]; then
    require_cmd docker
    if ! docker compose version >/dev/null 2>&1; then
      die "docker compose v2 plugin not found. Install via:
       https://docs.docker.com/compose/install/linux/"
    fi
  fi

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
  # Tighten the etc dir: it's about to hold the master key file.
  chmod 0750 "${ISENGARD_ETC}" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Step 2: master key. Generated once on first run; never overwritten.
# ---------------------------------------------------------------------------

# Returns 0 if the master key file exists and is the right size, 1 otherwise.
master_key_ready() {
  if [[ ! -f "${ISENGARD_MASTER_KEY}" ]]; then
    return 1
  fi
  # File must be exactly 32 bytes; the controller hard-rejects anything
  # else and refuses to boot.
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
    log "key: ${ISENGARD_MASTER_KEY} already present (${ISENGARD_MASTER_KEY##*/})"
    return 0
  fi
  log "key: generating fresh 32-byte master key at ${ISENGARD_MASTER_KEY}"
  openssl rand 32 >"${ISENGARD_MASTER_KEY}"
  chmod 0600 "${ISENGARD_MASTER_KEY}"
  # Match the controller container's UID. The ghcr controller image runs
  # as root (matches the agent recipe); root:root with 0600 is correct.
  chown 0:0 "${ISENGARD_MASTER_KEY}" 2>/dev/null || true
  log "key: master key created. Operator never sees the value; back up the file out of band."
}

# ---------------------------------------------------------------------------
# Step 3: interactive secret bootstrap. Prompts the operator for each
# named secret; pipes the value into the controller's bootstrap subcommand
# which encrypts it with the master key and writes ciphertext to SQLite.
# Plaintext NEVER hits the host filesystem.
# ---------------------------------------------------------------------------

# bootstrap_secret <name> <prompt> [<allow_empty>]
# Prompts the operator (hidden input). If they press Enter on an empty
# line and allow_empty is "yes", the secret is skipped. Otherwise the
# value is piped into `isengard secret bootstrap <name>`.
bootstrap_secret() {
  local name="$1"
  local prompt="$2"
  local allow_empty="${3:-yes}"

  # `read -s` (silent) keeps the value off the terminal. We do not echo
  # the value back, ever. The shell variable `value` is unset on the way
  # out so a stray `set` won't dump it.
  local value=""
  while :; do
    printf '  %s' "${prompt}"
    if [[ "${allow_empty}" == "yes" ]]; then
      printf ' (press Enter to skip)'
    fi
    printf ': '
    # When piped (curl | sudo bash) stdin is the pipe, not a TTY. Open
    # /dev/tty on FD 9 so the read loop can take hidden input from the
    # operator's actual terminal. Fall back to stdin (FD 0) only when
    # /dev/tty isn't available (truly headless).
    if exec 9</dev/tty 2>/dev/null; then
      :
    elif [[ -t 0 ]]; then
      exec 9<&0
    else
      die "no controlling terminal available for secret input; run 'sudo bash install.sh' from an interactive shell"
    fi
    # Read one character at a time so we can echo `*` per input
    # character (including pastes) and handle backspace. Plain `read -s`
    # gives no visual confirmation that paste actually landed.
    value=""
    local char
    while IFS= read -rsn1 -u 9 char; do
      if [[ -z "${char}" ]]; then
        # Enter pressed
        break
      fi
      # Backspace / DEL: remove last char + erase one star
      if [[ "${char}" == $'\x7f' || "${char}" == $'\b' ]]; then
        if [[ -n "${value}" ]]; then
          value="${value%?}"
          printf '\b \b'
        fi
        continue
      fi
      # Ignore other control characters (Ctrl-* etc.)
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
  # If the operator pre-built a local `isengard` binary (e.g. CI on a
  # source checkout) we use it directly; this is also how the on-host
  # smoke test in `install/README.md` validates the end-to-end flow
  # without needing the GHCR image pulled. Otherwise fall back to a
  # one-shot container that bind-mounts the master key + state dir.
  if [[ -n "${ISENGARD_LOCAL_BIN:-}" && -x "${ISENGARD_LOCAL_BIN}" ]]; then
    printf '%s' "${value}" | "${ISENGARD_LOCAL_BIN}" secret bootstrap "${name}" \
      --master-key-file "${ISENGARD_MASTER_KEY}" \
      --state-dir "${ISENGARD_PREFIX}/controller" >/dev/null
  else
    # --user 0:0: the distroless image defaults to nonroot, but the master
    # key is mode 0600 root on the host. The bind-mount carries those
    # perms inside the container, so a nonroot reader gets EACCES and the
    # bootstrap exits silently (set -e then kills the install with no
    # visible error). Run the one-shot as root for the seconds it takes
    # to encrypt + write. Same threat-model as the agent's user: "0:0"
    # for docker.sock access.
    printf '%s' "${value}" | docker run --rm -i --user 0:0 \
      -v "${ISENGARD_PREFIX}/controller:/var/lib/isengard" \
      -v "${ISENGARD_MASTER_KEY}:/run/secrets/master.key:ro" \
      "${ISENGARD_CONTROLLER_IMAGE}" \
      secret bootstrap "${name}" \
        --master-key-file /run/secrets/master.key \
        --state-dir /var/lib/isengard \
      >/dev/null
  fi

  # Wipe the value out of process memory before the next iteration.
  value=""
}

bootstrap_secrets_if_first_run() {
  if [[ -f "${ISENGARD_PREFIX}/controller/isengard.db" ]]; then
    log "secrets: existing controller DB at ${ISENGARD_PREFIX}/controller/isengard.db; skipping interactive prompt"
    log "secrets: manage with 'isd secret put|list|rm' against the running dashboard"
    return 0
  fi

  if [[ -n "${ISENGARD_LOCAL_BIN:-}" && -x "${ISENGARD_LOCAL_BIN}" ]]; then
    log "secrets: using local binary ${ISENGARD_LOCAL_BIN} for bootstrap"
  else
    log "secrets: pulling controller image so the bootstrap one-shots can run"
    docker pull "${ISENGARD_CONTROLLER_IMAGE}" >/dev/null
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
# Step 4: plain (non-secret) config. Prompts for ACME email / domains /
# directory and writes /etc/isengard/isengard.env. Re-runs leave the env
# file alone.
# ---------------------------------------------------------------------------

prompt_plain_config() {
  if [[ -f "${ISENGARD_ENV_FILE}" ]]; then
    log "env: ${ISENGARD_ENV_FILE} already exists; leaving in place"
    return 0
  fi

  log "env: prompting for non-secret config (visible input)"

  local acme_email=""
  local acme_domains=""
  local acme_directory="https://acme-staging-v02.api.letsencrypt.org/directory"

  # Same TTY-or-fallback dance as the secret prompts: prefer /dev/tty so
  # `curl | sudo bash` works, fall back to stdin only when there's a TTY
  # already on FD 0, otherwise skip with empty defaults.
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
    printf '  ACME directory URL [default: Let'\''s Encrypt staging]: '
    local input=""
    IFS= read -r -u "${input_fd}" input || input=""
    if [[ -n "${input}" ]]; then
      acme_directory="${input}"
    fi
  else
    warn "no controlling terminal; writing env file with empty defaults"
  fi

  log "env: writing template to ${ISENGARD_ENV_FILE}"
  # Pull the comment/structure from the example file but materialise the
  # operator-supplied values. We write it ourselves rather than `cat >>`
  # so the file always has a known shape.
  umask 0027
  cat >"${ISENGARD_ENV_FILE}" <<EOF
# Isengard non-secret config. Written by install/install.sh.
#
# Secrets (Cloudflare API token, backup passphrase, ...) are NEVER in
# this file. They live encrypted in the controller's SQLite, keyed by
# /etc/isengard/master.key. Manage them via:
#   isd secret put <name>       # while the stack is up
#   isengard secret bootstrap <name>  # at install time, see install.sh

# ACME contact email. Required only if you publish public HTTPS routes.
ISENGARD_ACME_EMAIL=${acme_email}

# Comma-separated hostnames the agent should pre-issue certs for.
# Wildcards (*.example.com) require DNS-01, which uses the
# 'cf_dns_api_token' bootstrapped secret.
ISENGARD_ACME_DOMAINS=${acme_domains}

# ACME directory URL. Staging by default until you confirm cert issue
# works; switch to https://acme-v02.api.letsencrypt.org/directory for
# production.
ISENGARD_ACME_DIRECTORY=${acme_directory}
EOF
  chmod 0640 "${ISENGARD_ENV_FILE}"
}

# ---------------------------------------------------------------------------
# Step 5: drop compose.yaml in place.
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
# Step 6: shared docker network.
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
# Step 7: pull images + bring the stack up.
# ---------------------------------------------------------------------------

bring_up() {
  export ISENGARD_PREFIX
  export ISENGARD_ENV_FILE
  export ISENGARD_COMPOSE_FILE
  export ISENGARD_MASTER_KEY

  log "images: pulling latest"
  docker compose --env-file "${ISENGARD_ENV_FILE}" -f "${ISENGARD_COMPOSE_FILE}" pull

  log "stack: bringing up via docker compose up -d"
  docker compose --env-file "${ISENGARD_ENV_FILE}" -f "${ISENGARD_COMPOSE_FILE}" up -d
}

# ---------------------------------------------------------------------------
# Step 8: print next steps.
# ---------------------------------------------------------------------------

post_install_hints() {
  cat <<EOF

  =====================================================================
  Isengard is up.

  Dashboard:  http://127.0.0.1:9418  (loopback by default)
  Logs:       docker logs -f iso-controller
              docker logs -f iso-agent

  Secrets:
    - Master key:     ${ISENGARD_MASTER_KEY} (mode 0600 root)
    - Encrypted DB:   ${ISENGARD_PREFIX}/controller/isengard.db
    - Bootstrap more: re-run install.sh after deleting isengard.db, OR
                      use 'isd secret put <name>' against the dashboard.

  To enroll the agent (first time only):
    1. Mint a token:
       docker exec iso-controller isengard controller token mint --role agent
    2. Paste the token at the prompt the agent shows in its logs, or
       export ISENGARD_ENROLL_TOKEN=<token> and 'docker compose up -d agent'.

  Operator CLI (\`isd\`): build once on a workstation, then 'isd login'.

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
  setup_master_key
  bootstrap_secrets_if_first_run
  prompt_plain_config
  setup_compose_file
  # ISENGARD_SKIP_BRING_UP=1 is for the on-host smoke test that
  # validates the master key + bootstrap + env path without needing
  # docker images pulled. Production installs always run the full path.
  if [[ -n "${ISENGARD_SKIP_BRING_UP:-}" ]]; then
    log "stack: ISENGARD_SKIP_BRING_UP set; skipping network + compose up"
    return 0
  fi
  setup_network
  bring_up
  post_install_hints
}

main "$@"
