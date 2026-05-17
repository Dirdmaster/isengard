# Isengard production install

The supported way to put Isengard on a server. As of Track D (2026-05-17) the docker compose recipe is the documented default; the systemd-native flow stays around for one more release before being removed in v0.7.

## Quick start (controller host)

```sh
# 1. Fetch the compose recipe.
sudo mkdir -p /etc/isengard
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/compose.yaml \
  -o /etc/isengard/compose.yaml

# 2. Create the shared proxy network the controller and operator stacks share.
sudo docker network create isengard-proxy 2>/dev/null || true

# 3. Bring up the controller.
sudo docker compose -f /etc/isengard/compose.yaml up -d controller

# 4. (Optional) Mint a join token to enroll additional agent hosts.
sudo docker compose -f /etc/isengard/compose.yaml exec controller \
  isengard controller token mint --role agent
```

The controller container is labelled `io.isengard.role=controller` so the operator CLI (`isd`) discovers it automatically over a docker context: `isd context import <docker-context-name>` mirrors a docker context into `isd`'s credentials, then every `isd` verb works against that host. See [`crates/isd-runtime/src/discovery_labels.rs`](../crates/isd-runtime/src/discovery_labels.rs) for the discovery contract.

The legacy systemd flow (`install.sh` + `isengard init`) still works for one more release. See [Legacy systemd-native install](#legacy-systemd-native-install) below for the sunset path.

## Legacy systemd-native install

> **Deprecated as of Track D (2026-05-17).** Sunset in v0.7. The compose recipe above is the supported path. This section is kept for operators mid-migration.

Both flows share the same secrets model (master key on disk, encrypted SQLite for everything else), the same env file shape, and the same operator CLI (`isd`).

### Quick install (interactive)

Step 1: drop the binary on the host. Runs the same way whether the host will become a controller or an agent.

```sh
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh | sudo bash
```

`install.sh` downloads the binary to `/usr/local/bin/isengard`, verifies the sha256, and prints a usage hint. It does NOT pick `init` or `join` for you: that was a footgun pre-2026-05-11, where every host that ran the curl pipe silently spun up a fresh controller. Now you choose explicitly.

Step 2 (controller host only): run `isengard init`. It walks you through prompts:

```sh
sudo isengard init
```

- ACME contact email (Let's Encrypt account registration; blank skips)
- ACME domains (comma-separated; wildcards require DNS-01)
- ACME directory (staging | production | custom URL)
- Optional Cloudflare DNS API token (encrypted with the master key)
- Optional backup passphrase (encrypted with the master key)
- Auto-detected host IP confirmation (used as a SAN on the controller cert)
- Optional extra SANs for the controller cert

After the prompts, init does the install: dirs + master key + secrets + systemd units + env file + start controller + export CA + mint enrollment token + start agent + success banner. The banner includes a copy-pasteable `isengard join` command for the other hosts in the fleet.

### Quick install (non-interactive)

Bake the subcommand into the install pipe via `bash -s --`:

```sh
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh | \
  sudo bash -s -- \
    init \
    --non-interactive \
    --acme-email ops@example.com \
    --acme-domains "*.example.com,example.com" \
    --acme-directory production \
    --no-cf-dns-token \
    --no-backup
```

Note the explicit `init` after `--`: that is the new requirement. Pre-2026-05-11 the bootstrap auto-appended `init`, which is why every piped curl turned the host into a controller. Now any args after `--` are exec'd as-is: `init [flags]` for a controller, `join [flags] <url>` for an agent.

Every prompt has a matching flag. Missing required values fail with a clear error.

### Adding more hosts to the fleet

After `init` finishes, the success banner prints a join command. On each additional host:

```sh
# 1. Drop the binary (same one-liner the controller host used).
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh | sudo bash

# 2. Copy /etc/isengard/ca.pem from the controller host to this one first.
sudo isengard join \
  --token "<token-from-init-banner>" \
  --ca-pem-path /etc/isengard/ca.pem \
  https://<controller-host-ip>:9417
```

Or fold both steps into a piped one-liner:

```sh
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh | \
  sudo bash -s -- \
    join \
    --token "<token>" \
    --ca-pem-base64 "<base64-ca>" \
    https://<controller-host-ip>:9417
```

Or pipe the CA inline:

```sh
sudo isengard join \
  --token "<token>" \
  --ca-pem-base64 "$(base64 -w0 /etc/isengard/ca.pem)" \
  https://<controller-host-ip>:9417
```

Optional defense-in-depth pin via `--ca-fingerprint sha256:<hex>`: when set, the join flow refuses to proceed if the supplied CA's sha256 doesn't match.

The token is single-use with a 15-minute TTL by default. Mint a fresh one on the controller when needed:

```sh
isengard controller token mint --role agent
```

### `isengard init` flags

| Flag | Default | Purpose |
|---|---|---|
| `--acme-email <email>` | (prompt) | LE account contact. Blank for internal-only. |
| `--acme-domains <list>` | (prompt) | Comma-separated. Wildcards need DNS-01. |
| `--acme-directory <kind>` | `staging` | `staging` / `production` / raw URL. |
| `--cf-dns-token <token>` | (prompt) | Cloudflare DNS API token, encrypted at rest. |
| `--no-cf-dns-token` | unset | Skip the Cloudflare prompt entirely. |
| `--backup-passphrase-file <path>` | (prompt) | Read passphrase from file. |
| `--no-backup` | unset | Skip the backup prompt entirely. |
| `--extra-san <host>` | empty | Repeatable; adds to the controller cert SAN list. |
| `--listen-grpc <addr:port>` | `0.0.0.0:9417` | Controller gRPC + agent enrollment. |
| `--listen-dashboard <addr:port>` | `0.0.0.0:9418` | Dashboard HTTP. |
| `--state-dir <path>` | `/var/lib/isengard` | Top-level state dir. |
| `--etc-dir <path>` | `/etc/isengard` | Config dir (env files, master key, ca.pem). |
| `--non-interactive` | unset | Skip prompts. Implied when stdin isn't a TTY. |
| `--runtime <kind>` | `wisp` | Container runtime backend. |
| `--host-ip <ip>` | (auto-detect) | Override the auto-detected host IP. |

### `isengard join` flags

| Flag | Default | Purpose |
|---|---|---|
| `<controller>` | required | Controller URL, positional. |
| `--token <enrollment-token>` | required | Minted on the controller. |
| `--ca-pem-path <file>` | one of these two | Path to controller CA pem. |
| `--ca-pem-base64 <b64>` | required | Base64-encoded CA pem (single-line). |
| `--ca-fingerprint sha256:<hex>` | optional | Defense-in-depth CA pin. |
| `--state-dir <path>` | `/var/lib/isengard` | Same layout as init. |
| `--etc-dir <path>` | `/etc/isengard` | Same layout as init. |
| `--runtime <kind>` | `wisp` | Container runtime backend. |

### What you need before running

| Requirement | Why |
|---|---|
| Linux host with systemd | Service unit + journal. |
| Kernel with cgroup v2 + clone3 | Wisp's container runtime. Any kernel from 2021+ has both. |
| `curl` or `wget` | Fetches the binary + sha256 sidecar. |
| `sha256sum` | Verifies the download. |
| Ports 80, 443 free on the host | Pingora binds them for HTTP/HTTPS routing. |
| Ports 9417, 9418 free on host loopback | Controller gRPC + dashboard. |
| Root | Default install paths are `/etc/isengard`, `/var/lib/isengard`, `/usr/local/bin`. |

### Day-to-day operations

```sh
systemctl status iso-controller iso-agent
journalctl -u iso-controller -f
journalctl -u iso-agent -f
systemctl restart iso-controller    # cycles the agent automatically (PartOf=)
systemctl start iso-agent.target    # convenience wrapper for the whole stack
$EDITOR /etc/isengard/isengard.env  # flip ACME staging -> production etc.
systemctl restart iso-controller    # picks up env changes
```

The dashboard is at `http://localhost:9418` by default. The controller is reachable at `https://localhost:9417` (and `https://<host-ip>:9417` from peers, since the controller's cert now includes the auto-detected host IP in its SAN list).

### Permission model

| File | Mode | Group | Why |
|---|---|---|---|
| `/etc/isengard/isengard.env` | `0644` | root | non-secret config |
| `/etc/isengard/agent-token.env` | `0600` | root | transient enrollment token |
| `/etc/isengard/master-key.env` | `0600` | root | points the controller at the master key path |
| `/etc/isengard/ca.pem` | `0644` | root | public CA cert; readable but read-only |
| `/etc/isengard/master.key` | `0600` | root | gates the secrets store |
| `/usr/local/bin/isengard` | `0755` | root | the binary |

Secrets (Cloudflare API token, backup passphrase, etc.) live encrypted in the controller's SQLite, gated by `master.key`. Manage them via `isd secret put|list|rm`.

### Updating

```sh
# Re-run the bootstrap to fetch a new binary. Drops the new binary
# and exits; no init or join is triggered. Re-running `isengard init`
# afterward is safe (existing master key + secrets are preserved).
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh | sudo bash
```

The agent also self-updates: when the controller publishes a `host.update_binary` action with a release URL + expected digest, the agent downloads to `/tmp`, verifies, atomically renames into place, and triggers `systemctl restart iso-agent` on itself.

### Migrating from the legacy docker-compose flow

If you have an existing docker-compose-installed host:

1. Drain workload stacks first: `isd stack down <stack>` for everything you don't want orphaned.
2. Tear down the legacy install: `sudo bash /etc/isengard/uninstall.sh --purge` (or `bash install/uninstall.sh --purge` from a checkout).
3. Run the new `install/install.sh`. Master key + secrets DB do NOT carry over (the wipe in step 2 removed them); you'll re-enter the bootstrap secrets.
4. `isd stack up <stack>` for each stack you wanted; wisp pulls the images and creates the bundles fresh.

## Legacy docker-compose install

For operators not yet ready to move to systemd-native, `install/install-docker.sh` is the pre-0.8 flow. It pulls GHCR images, creates the `isengard-proxy` docker network, and brings up the controller + agent as containers via `docker compose`. The `ISENGARD_RUNTIME=wisp` opt-in (Phase 0.4-0.7) still works inside that container path.

```sh
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install-docker.sh -o install-docker.sh
less install-docker.sh
sudo bash install-docker.sh
```

This script is on a deprecation timer. Phase 0.11+ removes it; pin to a tag if you need to keep using it long-term.

## Threat model

- The master key is the single thing on the host filesystem that gates access to every stored secret. It's mode `0600 root:root` at `/etc/isengard/master.key`. Same threat profile as Docker Swarm's default raft unlock key.
- The encrypted DB at `/var/lib/isengard/isengard.db` holds ChaCha20-Poly1305 ciphertexts. Without the master key it's a pile of opaque bytes.
- Secret values that the operator types at the install prompt live in the kernel pipe and the bootstrap process's memory only. They never hit the host filesystem in plaintext form.
- The systemd-native install runs both services as root. Phase 0.10 calls this out explicitly: the agent needs broad caps (SYS_ADMIN, NET_ADMIN, SYS_PTRACE, SYS_RESOURCE, plus chown/setuid/setgid for workload bootstrap) and AppArmor profiles aren't shipped yet. Phase 0.11+ tightens to a dedicated user + AmbientCapabilities + an AppArmor profile.

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
