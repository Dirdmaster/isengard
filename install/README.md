# Isengard production install

The supported way to put Isengard on a server. Two flows:

1. **Phase 0.8 default: systemd-native.** `install/install.sh` downloads a static musl binary from GitHub Releases, drops in systemd unit files, and brings up the controller + agent as services. No docker, no compose, no socket. The agent uses wisp (clone3 + cgroup v2 + iptables) to manage workload containers.
2. **Legacy: docker compose.** `install/install-docker.sh` is the pre-0.8 flow. Pulls images from GHCR and brings up controller + agent as containers via `docker compose`. Stays around for operators who haven't migrated. Phase 0.10+ removes it.

Both flows share the same secrets model (master key on disk, encrypted SQLite for everything else), the same env file shape, and the same operator CLI (`isd`).

## Phase 0.8 systemd-native install

`install/install.sh` is a single bash script that runs to completion in one pass:

1. Verifies systemd + openssl + curl (or wget) + sha256sum are present, detects the host arch.
2. Creates `/etc/isengard/` (config) and `/var/lib/isengard/{controller,agent,stacks}/` + `/var/lib/wisp/` (state) on the host.
3. **Downloads the static musl binary** from GitHub Releases for the requested version, verifies the sha256 sidecar, installs to `/usr/local/bin/isengard` (mode 0755).
4. **Generates a 32-byte random master key** at `/etc/isengard/master.key` mode 0600 root. Operator never types or sees the value.
5. **Interactively prompts for individual secrets** (Cloudflare DNS API token, backup passphrase). Each entered value is hidden (`read -s`) and piped into `isengard secret bootstrap <name>` which encrypts with the master key and writes ciphertext to `/var/lib/isengard/controller/isengard.db`. **Plaintext never touches a file on the host.**
6. **Prompts for non-secret config** (ACME email, ACME domains, ACME directory) and writes `/etc/isengard/isengard.env`.
7. **Installs systemd units** to `/etc/systemd/system/`: `iso-controller.service`, `iso-agent.service`, and `iso-agent.target` (a convenience wrapper for `systemctl start/stop` of the whole stack).
8. `systemctl enable --now iso-controller.service`, waits for the CA to initialize, exports it to `/etc/isengard/ca.pem`, mints a 15-minute enrollment token to `/etc/isengard/agent-token.env`, and starts `iso-agent.service`.

Re-running the script when an install is already on disk drops into a refresh menu. Choices:

1. **Refresh binary only** (default). Re-downloads the binary at the requested version, restarts the services. Keeps `master.key`, `isengard.env`, and the secrets DB. Use this for routine updates.
2. **Refresh binary + isengard.env**. Same as 1, plus re-prompts for the non-secret ACME values. Old env file is backed up to `isengard.env.bak`.
3. **Wipe everything and reinstall** (DESTRUCTIVE). Confirms with a literal `WIPE` prompt, stops + disables the units, removes `${ISENGARD_PREFIX}`, `${ISENGARD_ETC}`, `/var/lib/wisp`, and `${ISENGARD_BIN}`, then runs the full first-time path.
4. **Abort**. Exits with no changes.

For CI / scripted reinstalls, set `ISENGARD_REINSTALL_MODE` to one of `refresh-binary`, `refresh-config`, `wipe`, `abort` to skip the prompt. The wipe path additionally requires `ISENGARD_WIPE_YES=1` to bypass the literal-`WIPE` confirmation.

### What you need before running (systemd flow)

| Requirement | Why |
|---|---|
| Linux host with systemd | Service unit + journal. |
| Kernel with cgroup v2 + clone3 | Wisp's container runtime. Any kernel from 2021+ has both. |
| `openssl` | Generates the 32-byte master key on first run. |
| `curl` or `wget` | Fetches the binary + sha256 sidecar. |
| `sha256sum` | Verifies the download. |
| Ports 80, 443 free on the host | Pingora binds them for HTTP/HTTPS routing. |
| Ports 9417, 9418 free on host loopback | Controller gRPC + dashboard. |
| Root | Default install paths are `/etc/isengard`, `/var/lib/isengard`, `/usr/local/bin`. Override `ISENGARD_PREFIX` + `ISENGARD_ETC` + `ISENGARD_BIN_DIR` to install rootlessly (rare; the agent needs root caps for wisp anyway). |
| Interactive TTY | The script prompts for secret values with `read -s`. |

### Install (systemd flow)

```sh
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh -o install.sh
less install.sh
sudo bash install.sh
```

To pin to a specific tag:

```sh
ISENGARD_REF=v0.4.0 ISENGARD_VERSION=v0.4.0 \
  sudo -E bash install.sh
```

### Configurable env (systemd flow)

| Variable | Default | Purpose |
|---|---|---|
| `ISENGARD_PREFIX` | `/var/lib/isengard` | State path. |
| `ISENGARD_ETC` | `/etc/isengard` | Config path (env files, master key, CA). |
| `ISENGARD_BIN_DIR` | `/usr/local/bin` | Where the `isengard` binary lands. |
| `ISENGARD_VERSION` | `latest` | GitHub Release tag for the binary download. |
| `ISENGARD_RELEASE_BASE` | `https://github.com/Weavers-Engineering/Isengard/releases/download` | Base URL for binary + sha256 fetch. |
| `ISENGARD_REF` | `next` | Git ref to fetch the systemd unit files from. |
| `ISENGARD_RAW_BASE` | computed from ref | Base URL for unit-file raw fetches; override for forks. |
| `ISENGARD_LOCAL_BIN` | unset | When set to an executable path, skips the download and copies that file to `${ISENGARD_BIN}`. Used by smoke tests against unreleased builds. |
| `ISENGARD_REINSTALL_MODE` | unset | When set, pre-answers the refresh menu. One of: `refresh-binary`, `refresh-config`, `wipe`, `abort`. |
| `ISENGARD_WIPE_YES` | unset | When `1`, bypasses the literal-`WIPE` confirmation. |
| `ISENGARD_SKIP_BRING_UP` | unset | When set, skips `systemctl enable --now`. Used by CI smoke tests. |

### Day-to-day operations (systemd flow)

```sh
systemctl status iso-controller iso-agent
journalctl -u iso-controller -f
journalctl -u iso-agent -f
systemctl restart iso-controller    # cycles the agent automatically (PartOf=)
systemctl start iso-agent.target    # convenience wrapper for the whole stack
$EDITOR /etc/isengard/isengard.env  # flip ACME staging -> production etc.
systemctl restart iso-controller    # picks up env changes
```

The dashboard is at `http://127.0.0.1:9418` by default.

### Permission model (systemd flow)

| File | Mode | Group | Why |
|---|---|---|---|
| `/etc/isengard/isengard.env` | `0644` | root | non-secret config |
| `/etc/isengard/agent-token.env` | `0600` | root | transient enrollment token |
| `/etc/isengard/master-key.env` | `0600` | root | points the controller at the master key path |
| `/etc/isengard/ca.pem` | `0644` | root | public CA cert; readable but read-only |
| `/etc/isengard/master.key` | `0600` | root | gates the secrets store |
| `/usr/local/bin/isengard` | `0755` | root | the binary |

Secrets (Cloudflare API token, backup passphrase, etc.) live encrypted in the controller's SQLite, gated by `master.key`. Manage them via `isd secret put|list|rm`.

### Updating (systemd flow)

Re-run `install.sh` and pick `Refresh binary only`. The script downloads the new binary, atomically replaces `/usr/local/bin/isengard`, and runs `systemctl restart iso-controller iso-agent`.

The agent also self-updates via the same path: when the controller publishes a `host.update_binary` action with a release URL + expected digest, the agent downloads to `/tmp`, verifies, atomically renames into place, and triggers `systemctl restart iso-agent` on itself. systemd brings up the new binary on the next ExecStart cycle.

### Migrating from the legacy docker-compose flow

If you have an existing docker-compose-installed host:

1. Drain workload stacks first: `isd stack down <stack>` for everything you don't want orphaned.
2. Tear down the legacy install: `sudo bash /etc/isengard/uninstall.sh --purge` (or `bash install/uninstall.sh --purge` from a checkout).
3. Run the new `install/install.sh`. Master key + secrets DB do NOT carry over (the wipe in step 2 removed them); you'll re-enter the bootstrap secrets.
4. `isd stack up <stack>` for each stack you wanted; wisp pulls the images and creates the bundles fresh.

Going back to docker is the inverse: stop the systemd units, run `install/install-docker.sh`, accept that wisp-managed bundles won't carry over.

## Legacy docker-compose install

For operators not yet ready to move to systemd-native, `install/install-docker.sh` is the pre-0.8 flow. It pulls GHCR images, creates the `isengard-proxy` docker network, and brings up the controller + agent as containers via `docker compose`. The `ISENGARD_RUNTIME=wisp` opt-in (Phase 0.4-0.7) still works inside that container path.

```sh
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install-docker.sh -o install-docker.sh
less install-docker.sh
sudo bash install-docker.sh
```

This script is on a deprecation timer. Phase 0.10+ removes it; pin to a tag if you need to keep using it long-term.

## Threat model

- The master key is the single thing on the host filesystem that gates access to every stored secret. It's mode `0600 root:root` at `/etc/isengard/master.key`. Same threat profile as Docker Swarm's default raft unlock key.
- The encrypted DB at `/var/lib/isengard/controller/isengard.db` holds ChaCha20-Poly1305 ciphertexts. Without the master key it's a pile of opaque bytes.
- Secret values that the operator types at the install prompt live in the kernel pipe and the bootstrap process's memory only. They never hit the host filesystem in plaintext form.
- The systemd-native install runs both services as root. Phase 0.8 calls this out explicitly: the agent needs broad caps (SYS_ADMIN, NET_ADMIN, SYS_PTRACE, SYS_RESOURCE, plus chown/setuid/setgid for workload bootstrap) and AppArmor profiles aren't shipped yet. Phase 0.10+ tightens to a dedicated user + AmbientCapabilities + an AppArmor profile.

## Uninstalling

```sh
sudo systemctl stop iso-agent.service iso-controller.service
sudo systemctl disable iso-agent.service iso-controller.service
sudo rm /etc/systemd/system/iso-controller.service /etc/systemd/system/iso-agent.service /etc/systemd/system/iso-agent.target
sudo systemctl daemon-reload

# Optional: also remove state.
sudo rm -rf /var/lib/isengard /var/lib/wisp /etc/isengard /usr/local/bin/isengard
```

For the legacy docker-compose flow:

```sh
sudo bash /etc/isengard/uninstall.sh           # stop containers, keep state
sudo bash /etc/isengard/uninstall.sh --purge   # also delete /var/lib/isengard
sudo bash /etc/isengard/uninstall.sh --purge --network  # also drop proxy net
```
