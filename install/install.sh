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
# Re-runs (existing install detected): present a reinstall menu so the
# operator can refresh compose.yaml (preserving secrets), refresh
# compose + non-secret env (preserving secrets), wipe everything and
# reinstall, or abort. Set ISENGARD_REINSTALL_MODE to one of
# refresh-compose|refresh-config|wipe|abort to skip the prompt.
#
# Legacy behaviour (delete /etc/isengard/master.key + re-run) still
# works as a manual escape hatch but is no longer the recommended path:
# it rotates the master key, which makes every existing secret row
# undecryptable.

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
  # World-traversable so non-sudo `docker compose -f /etc/isengard/...`
  # works for any operator in the docker group. The directory itself
  # leaks no secrets — the master key inside is mode 0600 root, only
  # ever read by the controller container as uid 0 via bind-mount.
  chmod 0755 "${ISENGARD_ETC}" 2>/dev/null || true
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
  # `force=1` (or `--force`) re-prompts even when the env file already
  # exists; used by the refresh-config action handler.
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
  # Default to staging until the operator confirms a cert is issued. The
  # controller resolves 'staging' / 'production' aliases to LE URLs at
  # boot (see acme/mod.rs::resolve_directory).
  local acme_directory="staging"

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
    printf "  ACME environment [staging/production] (default staging): "
    local input=""
    IFS= read -r -u "${input_fd}" input || input=""
    case "${input,,}" in
      "" | staging | stage) acme_directory="staging" ;;
      production | prod) acme_directory="production" ;;
      *) acme_directory="${input}" ;;  # treat as raw URL
    esac
  else
    warn "no controlling terminal; writing env file with empty defaults"
  fi

  log "env: writing template to ${ISENGARD_ENV_FILE}"
  # Pull the comment/structure from the example file but materialise the
  # operator-supplied values. We write it ourselves rather than `cat >>`
  # so the file always has a known shape.
  #
  # umask 0022 + chmod 0644: this file holds NO secrets (CF tokens, etc
  # live encrypted in the SQLite secrets store, keyed by master.key).
  # World-readable lets the operator's user run `docker compose` without
  # sudo, since compose reads the env file at parse time.
  umask 0022
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

# ACME environment. Accepts:
#   staging | stage              -> LE staging (default; un-trusted by browsers)
#   production | prod | <empty>  -> LE production (real browser-trusted certs)
#   <URL>                        -> raw directory URL (custom CA, Pebble, etc.)
# Resolved by the controller at boot (acme/mod.rs::resolve_directory).
ISENGARD_ACME_DIRECTORY=${acme_directory}
EOF
  _apply_editable_config_perms "${ISENGARD_ENV_FILE}"
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

# Write compose.yaml to ${out}. Prefers a sibling install/compose.yaml when
# the script is run from a checkout (smoke tests, dev installs); falls back
# to a raw GitHub fetch keyed on ISENGARD_REF / ISENGARD_RAW_BASE.
write_compose_to() {
  local out="$1"
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || echo "")"
  local local_compose="${script_dir}/compose.yaml"

  if [[ -f "${local_compose}" ]]; then
    cp "${local_compose}" "${out}"
  else
    fetch "${ISENGARD_RAW_BASE}/compose.yaml" "${out}"
  fi
  _apply_editable_config_perms "${out}"
}

# Set permissions for an editable, non-secret config file under
# /etc/isengard. Group-writable to the `docker` group when present so any
# operator already in that group can `vi /etc/isengard/<file>` without
# sudo. Falls back to mode 0644 (world-readable, root-only-write) when
# the docker group is missing — surfaces a warn so the operator knows.
#
# Master key, secrets DB, and other genuinely-secret material take a
# different code path and stay 0600 root.
_apply_editable_config_perms() {
  local path="$1"
  if getent group docker >/dev/null 2>&1; then
    chgrp docker "${path}" 2>/dev/null || true
    chmod 0664 "${path}" 2>/dev/null || true
  else
    chmod 0644 "${path}" 2>/dev/null || true
    warn "perms: docker group not found; ${path} is world-readable but only root can edit it. Run as a member of the docker group to enable sudoless config edits."
  fi
}

setup_compose_file() {
  if [[ -f "${ISENGARD_COMPOSE_FILE}" ]]; then
    log "compose: ${ISENGARD_COMPOSE_FILE} already present; leaving in place"
    return 0
  fi
  log "compose: writing ${ISENGARD_COMPOSE_FILE}"
  write_compose_to "${ISENGARD_COMPOSE_FILE}"
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

# bring_up_stack [--force-recreate]
#   Pulls images and runs `docker compose up -d` against the prod compose
#   file. With --force-recreate, recreates containers even if their image
#   digest is unchanged: needed after a compose.yaml refresh that fixes a
#   service definition (e.g. #107 which added `user: "0:0"`).
bring_up_stack() {
  local force_recreate=0
  case "${1:-}" in
    --force-recreate) force_recreate=1 ;;
  esac

  export ISENGARD_PREFIX
  export ISENGARD_ENV_FILE
  export ISENGARD_COMPOSE_FILE
  export ISENGARD_MASTER_KEY

  # Phase 14 mTLS: agent needs the controller's CA cert to verify the
  # bootstrap TLS handshake on first enroll. Make sure the file exists
  # BEFORE compose tries to bind-mount it (docker would auto-create it
  # as a directory otherwise, breaking the agent on the next start).
  ensure_ca_placeholder

  log "images: pulling latest"
  docker compose --env-file "${ISENGARD_ENV_FILE}" -f "${ISENGARD_COMPOSE_FILE}" pull

  # Bring up the controller first so we can export its CA before the
  # agent tries to enroll. Compose's depends_on already enforces ordering
  # but we want the CA file written ASAP.
  log "stack: starting controller"
  docker compose --env-file "${ISENGARD_ENV_FILE}" -f "${ISENGARD_COMPOSE_FILE}" up -d controller

  bootstrap_ca_export

  # Mint a one-shot enrollment token and pass it to the agent via env var
  # at compose-up time. Without this the agent boots with
  # ISENGARD_ENROLL_TOKEN="" (compose default), tries to enroll, gets
  # `Unauthenticated`, and crashloops until the operator manually mints
  # a token and recreates. Idempotent in spirit: if the agent is already
  # enrolled (its agent.json + cert bundle exist in the volume), the
  # token is ignored and no harm done.
  local enroll_token
  enroll_token="$(mint_agent_enroll_token)" || enroll_token=""

  if [[ "${force_recreate}" -eq 1 ]]; then
    log "stack: bringing up agent via docker compose up -d --force-recreate"
    ISENGARD_ENROLL_TOKEN="${enroll_token}" \
      docker compose --env-file "${ISENGARD_ENV_FILE}" -f "${ISENGARD_COMPOSE_FILE}" up -d --force-recreate agent
  else
    log "stack: bringing up agent via docker compose up -d"
    ISENGARD_ENROLL_TOKEN="${enroll_token}" \
      docker compose --env-file "${ISENGARD_ENV_FILE}" -f "${ISENGARD_COMPOSE_FILE}" up -d agent
  fi
}

# Mint a fresh agent enrollment token via the controller's `token mint`
# subcommand and echo the raw token to stdout. Falls back to empty
# string on failure (logs a warn) so the caller can still bring the
# agent up; the agent will simply crashloop on enroll and the operator
# can mint+restart by hand.
#
# `--format token` returns the bare token on stdout (one line); the
# default human-readable format prints "Token expires at ..." as the
# last line, which would be the wrong thing to bind.
mint_agent_enroll_token() {
  local out
  if ! out="$(docker exec iso-controller \
        isengard controller token mint --role agent --format token 2>/dev/null \
        | tail -1)"; then
    warn "enroll: failed to mint agent token; agent will need a manual mint"
    return 0
  fi
  if [[ -z "${out}" ]]; then
    warn "enroll: token mint returned empty; agent will need a manual mint"
    return 0
  fi
  log "enroll: minted one-shot agent enrollment token (${#out} chars)"
  printf '%s' "${out}"
}

# Place a 0-byte ca.pem at the host path the agent's compose entry
# bind-mounts. If we let docker create the missing path it'll be a
# DIRECTORY instead of a file, and the eventual real cert can never
# replace it without manual rm. Writing an empty placeholder forces
# docker to bind-mount the file shape.
ensure_ca_placeholder() {
  local ca_path="${ISENGARD_CA_FILE:-${ISENGARD_ETC}/ca.pem}"
  if [[ ! -e "${ca_path}" ]]; then
    : > "${ca_path}"
    chmod 0644 "${ca_path}"
    log "ca: created placeholder at ${ca_path} (will be populated after controller starts)"
  fi
}

# Wait briefly for the controller to be ready, export its CA via the
# `isengard controller ca export` subcommand, write to ISENGARD_CA_FILE.
# Idempotent: if the cert already on disk matches the controller's, no-op.
bootstrap_ca_export() {
  local ca_path="${ISENGARD_CA_FILE:-${ISENGARD_ETC}/ca.pem}"

  log "ca: waiting for controller readiness (up to 30s)"
  # Probe via `docker inspect` rather than `docker exec ... true`: the runtime
  # image is `FROM scratch`, so there is no `true` binary (or any binary) to
  # exec against. State-based detection works for any base image and avoids
  # spawning a process inside the container just to check if it is alive.
  local i state
  for i in $(seq 1 30); do
    state="$(docker inspect iso-controller --format '{{.State.Status}}' 2>/dev/null || echo missing)"
    if [[ "${state}" == "running" ]]; then
      break
    fi
    sleep 1
  done

  log "ca: exporting controller CA to ${ca_path}"
  local tmp
  tmp="$(mktemp)"
  if docker exec iso-controller isengard controller ca export 2>/dev/null > "${tmp}"; then
    if grep -q "BEGIN CERTIFICATE" "${tmp}"; then
      mv "${tmp}" "${ca_path}"
      chmod 0644 "${ca_path}"
      log "ca: wrote $(wc -l < "${ca_path}") lines to ${ca_path}"
    else
      rm -f "${tmp}"
      warn "ca: controller exported empty / non-PEM output; agent will fail to verify"
    fi
  else
    rm -f "${tmp}"
    warn "ca: failed to exec ca export against iso-controller; agent will fail to verify"
  fi
}

# Backwards-compatible alias: `bring_up` was the original name.
bring_up() {
  bring_up_stack "$@"
}

# ---------------------------------------------------------------------------
# Reinstall menu: detect, report, prompt, dispatch.
#
# When `curl ... | sudo bash install.sh` is re-run on a host that already
# has /etc/isengard/master.key + compose.yaml + isengard.env, the script
# would otherwise short-circuit every step and miss compose.yaml fixes
# (e.g. #107). The menu surfaces three explicit choices: refresh just
# compose, refresh compose + non-secret env, or wipe everything and
# reinstall from scratch. ISENGARD_REINSTALL_MODE pre-answers the prompt
# for CI / scripted reinstalls.
# ---------------------------------------------------------------------------

# sha256 of a file, or empty string if not readable. Quietly tolerates
# missing files / missing tools so the "drift" check downgrades to
# "unknown" instead of failing the install.
_sha256_of() {
  local path="$1"
  if [[ ! -r "${path}" ]]; then
    printf ''
    return 0
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${path}" 2>/dev/null | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "${path}" 2>/dev/null | awk '{print $1}'
  else
    printf ''
  fi
}

# Compose drift status: "in sync" / "drifted" / "unknown".
# Compares the local ${ISENGARD_COMPOSE_FILE} to upstream:
#   1. sibling install/compose.yaml when this script lives in a checkout
#   2. ${ISENGARD_RAW_BASE}/compose.yaml otherwise
compose_sync_status() {
  if [[ ! -f "${ISENGARD_COMPOSE_FILE}" ]]; then
    printf 'absent'
    return 0
  fi
  local local_hash upstream_hash
  local_hash="$(_sha256_of "${ISENGARD_COMPOSE_FILE}")"
  if [[ -z "${local_hash}" ]]; then
    printf 'unknown'
    return 0
  fi

  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || echo "")"
  local sibling="${script_dir}/compose.yaml"

  local tmp_upstream=""
  if [[ -f "${sibling}" ]]; then
    upstream_hash="$(_sha256_of "${sibling}")"
  else
    tmp_upstream="$(mktemp 2>/dev/null || echo "")"
    if [[ -z "${tmp_upstream}" ]]; then
      printf 'unknown'
      return 0
    fi
    if command -v curl >/dev/null 2>&1; then
      curl -fsSL "${ISENGARD_RAW_BASE}/compose.yaml" -o "${tmp_upstream}" 2>/dev/null || true
    elif command -v wget >/dev/null 2>&1; then
      wget -qO "${tmp_upstream}" "${ISENGARD_RAW_BASE}/compose.yaml" 2>/dev/null || true
    fi
    upstream_hash="$(_sha256_of "${tmp_upstream}")"
    rm -f "${tmp_upstream}"
  fi

  if [[ -z "${upstream_hash}" ]]; then
    printf 'unknown'
    return 0
  fi
  if [[ "${local_hash}" == "${upstream_hash}" ]]; then
    printf 'in sync'
  else
    printf 'drifted'
  fi
}

# Reports the runtime state of a named container as one of:
#   Up | Restarting | Stopped | absent
container_state() {
  local name="$1"
  if ! command -v docker >/dev/null 2>&1; then
    printf 'unknown'
    return 0
  fi
  local out
  out="$(docker ps -a --filter "name=^${name}$" --format '{{.State}}' 2>/dev/null | head -n 1)"
  if [[ -z "${out}" ]]; then
    printf 'absent'
    return 0
  fi
  case "${out}" in
    running)         printf 'Up' ;;
    restarting)      printf 'Restarting' ;;
    exited|created|paused|dead) printf 'Stopped' ;;
    *)               printf '%s' "${out}" ;;
  esac
}

# Best-effort secret count via the local isengard binary. Returns "?" when
# we can't run the command (no local bin, list-bootstrap missing, etc.).
secret_count() {
  if [[ -z "${ISENGARD_LOCAL_BIN:-}" || ! -x "${ISENGARD_LOCAL_BIN:-}" ]]; then
    printf '?'
    return 0
  fi
  if [[ ! -f "${ISENGARD_PREFIX}/controller/isengard.db" ]]; then
    printf '0'
    return 0
  fi
  local out
  out="$("${ISENGARD_LOCAL_BIN}" secret list-bootstrap \
    --master-key-file "${ISENGARD_MASTER_KEY}" \
    --state-dir "${ISENGARD_PREFIX}/controller" 2>/dev/null | wc -l | tr -d '[:space:]')"
  if [[ -z "${out}" ]]; then
    printf '?'
  else
    printf '%s' "${out}"
  fi
}

# Returns one of: none | partial | complete
detect_existing() {
  # Migration: bring legacy installs up to the current permission model.
  # Older versions wrote /etc/isengard at 0750 root + isengard.env at
  # 0640 root + compose.yaml at 0644 root, forcing operators to `sudo`
  # every `docker compose` invocation and config edit. None of those
  # paths hold secrets (those live encrypted in SQLite, gated by
  # master.key, which keeps its 0600 root file mode regardless of the
  # parent dir's mode). We set the dir world-traversable and the
  # editable configs group-writable to the docker group. Idempotent and
  # runs on every install.sh invocation so a second curl-bash brings
  # legacy installs current without a reinstall menu trip.
  if [[ -d "${ISENGARD_ETC}" ]]; then
    chmod 0755 "${ISENGARD_ETC}" 2>/dev/null || true
  fi
  for f in "${ISENGARD_ENV_FILE}" "${ISENGARD_COMPOSE_FILE}"; do
    [[ -f "${f}" ]] || continue
    _apply_editable_config_perms "${f}" || true
  done

  local key=0 compose=0 env=0 db=0
  [[ -f "${ISENGARD_MASTER_KEY}" ]] && key=1
  [[ -f "${ISENGARD_COMPOSE_FILE}" ]] && compose=1
  [[ -f "${ISENGARD_ENV_FILE}" ]] && env=1
  [[ -f "${ISENGARD_PREFIX}/controller/isengard.db" ]] && db=1

  local total=$((key + compose + env + db))
  if [[ "${total}" -eq 0 ]]; then
    printf 'none'
  elif [[ "${key}" -eq 1 && "${compose}" -eq 1 && "${env}" -eq 1 ]]; then
    # DB may legitimately be missing on a freshly bootstrapped install
    # that never had its container brought up (ISENGARD_SKIP_BRING_UP).
    printf 'complete'
  else
    printf 'partial'
  fi
}

print_existing_report() {
  local sync_status="$(compose_sync_status)"
  local controller_state="$(container_state iso-controller)"
  local agent_state="$(container_state iso-agent)"
  local key_size="absent"
  if [[ -f "${ISENGARD_MASTER_KEY}" ]]; then
    key_size="$(wc -c <"${ISENGARD_MASTER_KEY}" 2>/dev/null | tr -d '[:space:]') bytes"
  fi
  local compose_state="${sync_status}"
  if [[ ! -f "${ISENGARD_COMPOSE_FILE}" ]]; then
    compose_state="absent"
  fi
  local env_state="present"
  if [[ ! -f "${ISENGARD_ENV_FILE}" ]]; then
    env_state="absent"
  fi
  local db_state="absent"
  if [[ -f "${ISENGARD_PREFIX}/controller/isengard.db" ]]; then
    local count
    count="$(secret_count)"
    db_state="present (${count} secrets)"
  fi

  cat <<EOF

[isengard] existing install detected:
  master.key:    ${ISENGARD_MASTER_KEY} (${key_size}, mode 0600)
  compose.yaml:  ${ISENGARD_COMPOSE_FILE} (${compose_state})
  isengard.env:  ${ISENGARD_ENV_FILE} (${env_state})
  secrets DB:    ${ISENGARD_PREFIX}/controller/isengard.db (${db_state})
  controllers:   iso-controller (${controller_state})
  agents:        iso-agent (${agent_state})

EOF
}

# Maps ISENGARD_REINSTALL_MODE / a numeric menu choice / a default to a
# canonical action name: refresh-compose | refresh-config | wipe | abort.
# Empty input = canonical empty.
_normalize_action() {
  case "${1:-}" in
    1|refresh-compose|compose) printf 'refresh-compose' ;;
    2|refresh-config|env|config) printf 'refresh-config' ;;
    3|wipe|reinstall) printf 'wipe' ;;
    4|abort|cancel|exit) printf 'abort' ;;
    "") printf '' ;;
    *) printf 'invalid' ;;
  esac
}

# Reads a single menu choice from /dev/tty (curl | sudo bash safe) or
# stdin (interactive shell), with a default of "1" on Enter. Echoes the
# canonical action name; bails to abort on missing TTY.
_prompt_menu_choice() {
  local input_fd=""
  # Brace-group wrapping silences the "Device not configured" stderr leak
  # bash emits when /dev/tty is unavailable (CI sandboxes, headless docker
  # exec). Plain `exec 9</dev/tty 2>/dev/null` lets the redirection error
  # through before the redirect takes effect.
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
  1) Refresh compose.yaml only             (preserves secrets, env, master.key)
  2) Refresh compose.yaml + isengard.env   (keeps secrets + master.key)
  3) Wipe everything and reinstall         (DESTRUCTIVE: deletes secrets + master.key + volumes)
  4) Abort

EOF

  local action=""
  if [[ -n "${ISENGARD_REINSTALL_MODE:-}" ]]; then
    action="$(_normalize_action "${ISENGARD_REINSTALL_MODE}")"
    log "reinstall: ISENGARD_REINSTALL_MODE=${ISENGARD_REINSTALL_MODE} -> ${action}"
    if [[ "${action}" == "invalid" || -z "${action}" ]]; then
      die "ISENGARD_REINSTALL_MODE must be one of: refresh-compose, refresh-config, wipe, abort"
    fi
  else
    while :; do
      action="$(_prompt_menu_choice)"
      if [[ "${action}" == "invalid" ]]; then
        warn "unknown choice; pick 1, 2, 3, or 4"
        continue
      fi
      if [[ -z "${action}" ]]; then
        action="refresh-compose"
      fi
      break
    done
    log "reinstall: choice=${action}"
  fi

  case "${action}" in
    refresh-compose) action_refresh_compose ;;
    refresh-config)  action_refresh_config ;;
    wipe)            action_wipe ;;
    abort)           log "reinstall: aborted, no changes made"; exit 0 ;;
    *)               die "internal error: unknown action ${action}" ;;
  esac
}

# Action 1: refresh compose.yaml; recreate containers; leave everything
# else (master.key, env, secrets DB) untouched.
action_refresh_compose() {
  log "action: refresh compose.yaml only"
  setup_dirs
  log "compose: re-fetching ${ISENGARD_COMPOSE_FILE}"
  write_compose_to "${ISENGARD_COMPOSE_FILE}"

  if [[ -n "${ISENGARD_SKIP_BRING_UP:-}" ]]; then
    log "stack: ISENGARD_SKIP_BRING_UP set; skipping recreate"
    return 0
  fi
  setup_network
  bring_up_stack --force-recreate
  install_isd_binary
  log "action: refresh compose complete"
}

# Action 2: refresh compose.yaml + non-secret env (re-prompt for ACME);
# keep master.key + secrets DB; recreate containers.
action_refresh_config() {
  log "action: refresh compose.yaml + isengard.env"
  setup_dirs
  log "compose: re-fetching ${ISENGARD_COMPOSE_FILE}"
  write_compose_to "${ISENGARD_COMPOSE_FILE}"

  # Force re-prompt: the existing env file is preserved as a .bak so the
  # operator can crib their old values back if they fat-finger a prompt.
  if [[ -f "${ISENGARD_ENV_FILE}" ]]; then
    cp "${ISENGARD_ENV_FILE}" "${ISENGARD_ENV_FILE}.bak"
    log "env: backed up old env to ${ISENGARD_ENV_FILE}.bak"
  fi
  prompt_plain_config --force

  if [[ -n "${ISENGARD_SKIP_BRING_UP:-}" ]]; then
    log "stack: ISENGARD_SKIP_BRING_UP set; skipping recreate"
    return 0
  fi
  setup_network
  bring_up_stack --force-recreate
  install_isd_binary
  log "action: refresh config complete"
}

# Action 3: nuke everything (containers + volumes + on-disk state +
# master.key) and run the full first-time install path. Requires the
# operator to type "WIPE" verbatim, OR ISENGARD_WIPE_YES=1 / --yes from
# scripted callers.
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

  # Safety: refuse to rm a path that didn't end up as an absolute path
  # under at least three characters of root. Catches the classic
  # "ISENGARD_PREFIX is empty -> rm -rf /" footgun even though `set -u`
  # already covers the strict-unset case. Defence in depth.
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

  # Take the stack down + remove volumes if compose is present and docker
  # is available. Any errors here are warnings: we still want to proceed
  # to the rm step, which is what the operator actually asked for.
  if [[ -f "${ISENGARD_COMPOSE_FILE}" ]] && command -v docker >/dev/null 2>&1; then
    log "wipe: docker compose down -v"
    if [[ -f "${ISENGARD_ENV_FILE}" ]]; then
      docker compose --env-file "${ISENGARD_ENV_FILE}" -f "${ISENGARD_COMPOSE_FILE}" down -v --remove-orphans 2>&1 | sed 's/^/    /' || \
        warn "compose down reported errors; continuing wipe"
    else
      docker compose -f "${ISENGARD_COMPOSE_FILE}" down -v --remove-orphans 2>&1 | sed 's/^/    /' || \
        warn "compose down reported errors; continuing wipe"
    fi
  else
    log "wipe: no compose / docker; skipping compose down"
  fi

  log "wipe: removing ${ISENGARD_ETC}"
  _safe_rm_rf "${ISENGARD_ETC}"
  log "wipe: removing ${ISENGARD_PREFIX}"
  _safe_rm_rf "${ISENGARD_PREFIX}"

  log "wipe: complete; running first-time install"
  fresh_install
}

# Fresh-install path, factored out so action_wipe can reuse it after
# nuking state. main() also calls this when detect_existing returns
# "none".
fresh_install() {
  setup_dirs
  setup_master_key
  bootstrap_secrets_if_first_run
  prompt_plain_config
  setup_compose_file
  if [[ -n "${ISENGARD_SKIP_BRING_UP:-}" ]]; then
    log "stack: ISENGARD_SKIP_BRING_UP set; skipping network + compose up"
    return 0
  fi
  setup_network
  bring_up_stack
  install_isd_binary
  post_install_hints
}

# ---------------------------------------------------------------------------
# Extract the `isd` operator CLI from the controller image into
# /usr/local/bin/isd on the host. The CLI is statically linked (musl) and
# bundled into the same scratch image as the daemon binary, so we don't
# need a separate release pipeline. Idempotent: a second run overwrites
# with whatever version is in the currently-cached image.
# ---------------------------------------------------------------------------
install_isd_binary() {
  local image="${ISENGARD_CONTROLLER_IMAGE:-ghcr.io/weavers-engineering/isengard-controller:${ISENGARD_REF}}"
  local target="/usr/local/bin/isd"
  log "isd: extracting ${target} from ${image}"
  local cid
  if ! cid="$(docker create "${image}" 2>/dev/null)"; then
    warn "isd: docker create failed for ${image}; skipping isd install"
    return 0
  fi
  # Always clean up the throwaway container, even on copy failure.
  if ! docker cp "${cid}:/usr/local/bin/isd" "${target}" 2>/dev/null; then
    warn "isd: docker cp failed (image may not bundle /usr/local/bin/isd yet); skipping"
    docker rm "${cid}" >/dev/null 2>&1 || true
    return 0
  fi
  docker rm "${cid}" >/dev/null 2>&1 || true
  chmod 0755 "${target}" 2>/dev/null || true
  log "isd: installed ${target} ($(${target} --version 2>/dev/null || echo unknown))"
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

  Operator CLI:
    - On this server: /usr/local/bin/isd (extracted from the image)
    - On your workstation: same binary, login to http://<this-host>:9418
    Try: isd login http://127.0.0.1:9418
         isd ps; isd route list; isd secret list

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

  local state
  state="$(detect_existing)"
  log "detect: existing install state = ${state}"

  case "${state}" in
    none)
      # Pure first-time path. ISENGARD_SKIP_BRING_UP=1 still applies
      # inside fresh_install so the on-host smoke test that exercises
      # master key + bootstrap + env without docker continues to work.
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
