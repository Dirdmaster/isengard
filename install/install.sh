#!/usr/bin/env bash
# Isengard install bootstrap (Phase 0.10, P1 footgun fix 2026-05-11).
#
# Downloads the isengard binary from GitHub Releases, verifies its
# sha256, drops it at /usr/local/bin/isengard, then hands off to a
# subcommand the operator chooses. All real install logic lives
# inside the binary (interactive cliclack TUI, or flag-driven for
# scripted setups). This script's job is to put the binary in place
# and hand off; nothing else.
#
# Subcommand handling (FOOTGUN FIX):
#   - With NO subcommand: drop the binary and print a usage hint.
#     Previously this exec'd `isengard init` unconditionally, which
#     meant every host that ran `curl | bash` became a controller,
#     including hosts intended to be agents. Now the operator must
#     explicitly choose `init` or `join`.
#   - With a subcommand: exec `isengard <subcommand> [args...]`.
#
# Usage:
#   # Drop the binary, then choose what to do next:
#   curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh | sudo bash
#   sudo isengard init                                   # become a controller
#   sudo isengard join --token <t> https://<ctrl>:9417   # enroll as an agent
#
#   # Or bake the subcommand into the curl pipe:
#   curl ... | sudo bash -s -- init --acme-email me@example.com
#   curl ... | sudo bash -s -- join --token <t> --ca-pem-path /etc/isengard/ca.pem https://<ctrl>:9417
#
# Env vars (all optional):
#   ISENGARD_VERSION             Release tag. Default "latest".
#   ISENGARD_TARGET              Binary target triple. Default detected from uname -m.
#   ISENGARD_RELEASE_BASE_REPO   GitHub repo. Default Weavers-Engineering/Isengard.
#   ISENGARD_BIN                 Final binary path. Default /usr/local/bin/isengard.
#
# This script intentionally does NOT detect existing installs, write
# config, generate keys, or talk to systemd. `isengard init` (and
# `isengard join`) do all of that and are idempotent.

set -euo pipefail

# Track D deprecation banner (2026-05-17). Print this first so the operator
# sees it before any download or install work begins. Plain ASCII; no em or
# en dashes (vault lefthook rule + downstream tooling friendliness).
cat >&2 <<'WARN'

  Heads up: systemd-native install is deprecated as of Track D (2026-05-17).
  The new flow ships the controller as a docker container:

      sudo mkdir -p /etc/isengard
      curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/compose.yaml \
        -o /etc/isengard/compose.yaml
      sudo docker compose -f /etc/isengard/compose.yaml up -d

  This script still works for one more release (sunset in v0.7). Open an
  issue if the new flow does not fit your setup.

WARN

ISENGARD_VERSION="${ISENGARD_VERSION:-latest}"
ISENGARD_RELEASE_BASE_REPO="${ISENGARD_RELEASE_BASE_REPO:-https://github.com/Weavers-Engineering/Isengard}"
ISENGARD_BIN="${ISENGARD_BIN:-/usr/local/bin/isengard}"

log()  { printf '[isengard] %s\n' "$*"; }
die()  { printf '[isengard] ERROR: %s\n' "$*" >&2; exit 1; }

# Resolve the release-asset target triple. aarch64 falls back to x86_64
# when qemu-x86_64 binfmt is registered (OrbStack on Apple Silicon),
# matching the Phase 7 musl build pipeline that ships only x86_64.
detect_target() {
  local arch
  arch="$(uname -m)"
  case "${arch}" in
    x86_64|amd64)
      printf 'x86_64-unknown-linux-musl'
      ;;
    aarch64|arm64)
      if [[ -e /proc/sys/fs/binfmt_misc/qemu-x86_64 ]] \
        && grep -q '^enabled' /proc/sys/fs/binfmt_misc/qemu-x86_64 2>/dev/null; then
        printf 'x86_64-unknown-linux-musl'
      else
        die "no aarch64 release artifact today; install qemu-user-static (apt install qemu-user-static) to enable x86_64 emulation, or set ISENGARD_TARGET=x86_64-unknown-linux-musl manually after registering binfmt"
      fi
      ;;
    *)
      die "unsupported arch: ${arch}"
      ;;
  esac
}

# GitHub Releases resolves "latest" via /releases/latest/download/<file>;
# explicit tags use /releases/download/<tag>/<file>.
release_url() {
  local file="$1"
  if [[ "${ISENGARD_VERSION}" == "latest" ]]; then
    printf '%s/releases/latest/download/%s' "${ISENGARD_RELEASE_BASE_REPO}" "${file}"
  else
    printf '%s/releases/download/%s/%s' "${ISENGARD_RELEASE_BASE_REPO}" "${ISENGARD_VERSION}" "${file}"
  fi
}

fetch() {
  local url="$1"
  local out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "${url}" -o "${out}" || die "fetch failed: ${url}"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "${out}" "${url}" || die "fetch failed: ${url}"
  else
    die "neither curl nor wget is installed; one is needed to fetch the binary"
  fi
}

# Linux check: systemd is Linux-only, so the bootstrap makes no sense
# elsewhere. Faster to fail here than after a 30 MB download.
kernel="$(uname -s)"
if [[ "${kernel}" != "Linux" ]]; then
  die "this script targets systemd on Linux; got ${kernel}. For dev on macOS see docker/README.md."
fi

# /usr/local/bin needs root.
if [[ "$(id -u)" -ne 0 ]]; then
  die "writing to ${ISENGARD_BIN} requires root. Re-run with sudo, or override ISENGARD_BIN to a user-writable path."
fi

target="${ISENGARD_TARGET:-$(detect_target)}"
asset="isengard-${target}"
url="$(release_url "${asset}")"
sha_url="$(release_url "${asset}.sha256")"

log "downloading ${url}"
tmp="$(mktemp)"
tmp_sha="$(mktemp)"
trap 'rm -f "${tmp}" "${tmp_sha}"' EXIT
fetch "${url}" "${tmp}"
fetch "${sha_url}" "${tmp_sha}"

got="$(sha256sum "${tmp}" | awk '{print $1}')"
expected="$(awk '{print $1}' <"${tmp_sha}")"
if [[ -z "${expected}" ]]; then
  die "empty sha256 sidecar at ${sha_url}"
fi
if [[ "${got}" != "${expected}" ]]; then
  die "sha256 mismatch (got ${got}, expected ${expected}); refusing to install"
fi
log "sha256 verified: ${expected}"

install -m 0755 "${tmp}" "${ISENGARD_BIN}"
log "installed ${ISENGARD_BIN}"

# Hand off. The operator picks the subcommand. We refuse to silently
# pick `init` for them: that's the v0.4.0 footgun where every host
# that ran `curl | bash` became a controller, including ones intended
# to be agents. See header comment for details.
#
# - No args: print a usage hint and exit clean. The binary is in
#   place; the operator runs `isengard init` or `isengard join` next.
# - With args: exec `isengard <args...>`. So `bash -s -- init ...`
#   and `bash -s -- join ...` both work for one-shot piped installs.
if [[ $# -eq 0 ]]; then
  cat <<EOF >&2
[isengard] binary installed at ${ISENGARD_BIN}.

What this host should be next determines the command to run:

  Create a new fleet (this host runs the controller):
    sudo ${ISENGARD_BIN} init

  Join an existing fleet (this host runs only an agent):
    sudo ${ISENGARD_BIN} join \\
      --token <token-from-controller> \\
      --ca-pem-path /etc/isengard/ca.pem \\
      https://<controller-host>:9417

To bake the choice into a one-liner, pass it through the install pipe:

  curl ... | sudo bash -s -- init [init flags...]
  curl ... | sudo bash -s -- join --token <t> https://<ctrl>:9417

EOF
  exit 0
fi

exec "${ISENGARD_BIN}" "$@"
