#!/usr/bin/env bash
# Prepare an OCI bundle at examples/busybox/ that wisp can run.
#
# Idempotent: re-running overwrites the rootfs + config.json cleanly.
#
# Source busybox, in priority order:
# 1. $WISP_BUSYBOX_BIN if set and points at a working file.
# 2. /usr/bin/busybox (Debian/Ubuntu's busybox-static package).
# 3. The multiarch musl static builds at
#    https://www.busybox.net/downloads/binaries/1.31.0-defconfig-multiarch-musl/
#    + https://www.busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/
#    (older multiarch only goes up to 32-bit ARM; 1.35 covers x86_64
#    standalone). If the upstream URL ever 404s, install
#    `busybox-static` and re-run, or drop a static busybox binary
#    into examples/busybox/rootfs/bin/busybox by hand.
#
# Run from the wisp crate root (so paths line up with run-busybox.rs):
#   cd crates/wisp && bash examples/prepare-busybox.sh

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUNDLE="${HERE}/busybox"
ROOTFS="${BUNDLE}/rootfs"

# Detect architecture. We map to busybox.net's filename conventions:
# - x86_64 has a standalone 1.35.0 musl static build
# - aarch64 only has the 32-bit `armv8l` build at busybox.net; we
#   prefer /usr/bin/busybox on this arch and fall back to the URL
#   only if the system busybox is missing.
ARCH="$(uname -m)"
case "${ARCH}" in
  aarch64|arm64)
    BB_URL="https://www.busybox.net/downloads/binaries/1.31.0-defconfig-multiarch-musl/busybox-armv8l"
    ;;
  x86_64|amd64)
    BB_URL="https://www.busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox"
    ;;
  *)
    echo "prepare-busybox.sh: unsupported arch ${ARCH}" >&2
    exit 1
    ;;
esac

mkdir -p "${ROOTFS}/bin" "${ROOTFS}/etc" "${ROOTFS}/proc" "${ROOTFS}/sys" \
         "${ROOTFS}/dev" "${ROOTFS}/dev/pts" "${ROOTFS}/dev/shm" "${ROOTFS}/dev/mqueue" \
         "${ROOTFS}/tmp"

BB_BIN="${ROOTFS}/bin/busybox"

# Source resolution: explicit -> system -> network.
SRC=""
if [ -n "${WISP_BUSYBOX_BIN:-}" ] && [ -s "${WISP_BUSYBOX_BIN}" ]; then
  SRC="${WISP_BUSYBOX_BIN}"
elif [ -s /usr/bin/busybox ]; then
  SRC="/usr/bin/busybox"
fi

if [ -z "${SRC}" ]; then
  echo "downloading ${BB_URL} -> ${BB_BIN}"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "${BB_BIN}" "${BB_URL}"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "${BB_BIN}" "${BB_URL}"
  else
    echo "prepare-busybox.sh: need curl, wget, or a system busybox-static" >&2
    exit 1
  fi
else
  echo "copying ${SRC} -> ${BB_BIN}"
  cp "${SRC}" "${BB_BIN}"
fi
chmod +x "${BB_BIN}"

# Symlink the applets we care about. busybox dispatches on argv[0].
for applet in sh echo hostname cat ls true false sleep; do
  ln -sf busybox "${ROOTFS}/bin/${applet}"
done

# Write config.json. We deliberately keep it small: minimal capability
# set, the five required namespaces, the standard /proc + /sys + /dev
# tmpfs + /dev/pts devpts + /dev/shm tmpfs + /dev/mqueue mqueue mounts.
cat > "${BUNDLE}/config.json" <<'JSON'
{
  "ociVersion": "1.0.2",
  "process": {
    "terminal": false,
    "user": { "uid": 0, "gid": 0 },
    "args": ["/bin/sh", "-c", "echo hello && hostname"],
    "env": [
      "PATH=/bin:/usr/bin",
      "TERM=xterm"
    ],
    "cwd": "/",
    "capabilities": {
      "bounding":    ["CAP_KILL", "CAP_NET_BIND_SERVICE"],
      "permitted":   ["CAP_KILL", "CAP_NET_BIND_SERVICE"],
      "effective":   ["CAP_KILL", "CAP_NET_BIND_SERVICE"],
      "inheritable": [],
      "ambient":     []
    },
    "rlimits": [],
    "noNewPrivileges": true
  },
  "root": {
    "path": "rootfs",
    "readonly": false
  },
  "hostname": "wisp-demo",
  "mounts": [
    {
      "destination": "/proc",
      "type": "proc",
      "source": "proc"
    },
    {
      "destination": "/dev",
      "type": "tmpfs",
      "source": "tmpfs",
      "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]
    },
    {
      "destination": "/dev/pts",
      "type": "devpts",
      "source": "devpts",
      "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"]
    },
    {
      "destination": "/dev/shm",
      "type": "tmpfs",
      "source": "shm",
      "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]
    },
    {
      "destination": "/dev/mqueue",
      "type": "mqueue",
      "source": "mqueue",
      "options": ["nosuid", "noexec", "nodev"]
    },
    {
      "destination": "/sys",
      "type": "sysfs",
      "source": "sysfs",
      "options": ["nosuid", "noexec", "nodev", "ro"]
    }
  ],
  "linux": {
    "namespaces": [
      { "type": "pid" },
      { "type": "mount" },
      { "type": "uts" },
      { "type": "ipc" },
      { "type": "network" }
    ],
    "maskedPaths": [
      "/proc/kcore",
      "/proc/keys",
      "/proc/latency_stats",
      "/proc/timer_list",
      "/proc/timer_stats",
      "/proc/sched_debug",
      "/sys/firmware",
      "/proc/scsi"
    ],
    "readonlyPaths": [
      "/proc/asound",
      "/proc/bus",
      "/proc/fs",
      "/proc/irq",
      "/proc/sys",
      "/proc/sysrq-trigger"
    ]
  }
}
JSON

echo "prepared bundle at ${BUNDLE}"
echo "  rootfs: ${ROOTFS}"
echo "  config: ${BUNDLE}/config.json"
echo
echo "run with: cargo run --example run-busybox"
