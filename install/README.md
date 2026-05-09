# Isengard production install

The supported way to put Isengard on a server. One command, no source checkout, no Justfile, no Rust toolchain. Pulls signed images from GHCR and brings up the controller + agent + shared proxy network with documented defaults.

## What this does

`install/install.sh` is a single bash script that runs to completion in one pass:

1. Verifies docker + docker compose v2 + openssl are present.
2. Creates `/etc/isengard/` (config) and `/var/lib/isengard/{controller,agent,stacks}/` (state) on the host.
3. **Generates a 32-byte random master key** at `/etc/isengard/master.key` mode 0600 root. The operator never types or sees the key value. The compose recipe bind-mounts this file into the controller container at `/run/secrets/master.key`.
4. **Interactively prompts for individual secrets** (Cloudflare DNS API token, backup passphrase). Each entered value is hidden (`read -s`) and piped into a one-shot `isengard secret bootstrap <name>` container that encrypts with the master key and writes the ciphertext to `/var/lib/isengard/controller/isengard.db`. **Plaintext never touches a file on the host.**
5. **Prompts for non-secret config** (ACME email, ACME domains, ACME directory) and writes `/etc/isengard/isengard.env`. Plain values only; the file is mode 0640.
6. Drops `compose.yaml` into `/etc/isengard/`, creates the `isengard-proxy` docker network if missing, pulls the GHCR images, and brings up the stack via `docker compose up -d`.

Re-running the script when an install is already on disk drops into a reinstall menu instead of silently short-circuiting. Choices:

1. **Refresh compose.yaml only** (default). Re-fetches `/etc/isengard/compose.yaml` from the requested ref and recreates containers with `--force-recreate`. Keeps `master.key`, `isengard.env`, and the secrets DB. Use this whenever a compose-level fix ships (e.g. #107).
2. **Refresh compose.yaml + isengard.env**. Same as 1, plus re-prompts for the non-secret ACME values. Old env file is backed up to `isengard.env.bak`. Master key + secrets DB are still preserved.
3. **Wipe everything and reinstall** (DESTRUCTIVE). Confirms with a literal `WIPE` prompt, runs `docker compose down -v`, removes `${ISENGARD_PREFIX}` and `${ISENGARD_ETC}`, then runs the full first-time path. Erases secrets and master key.
4. **Abort**. Exits with no changes.

For CI / scripted reinstalls, set `ISENGARD_REINSTALL_MODE` to one of `refresh-compose`, `refresh-config`, `wipe`, `abort` to skip the prompt. The wipe path additionally requires `ISENGARD_WIPE_YES=1` to bypass the literal-WIPE confirmation.

Day-to-day secret changes happen via `isd secret put <name>` against the running dashboard, not by re-running the install script.

## What you need before running

| Requirement | Why |
|---|---|
| Linux host with docker engine 24+ | Container runtime. |
| `docker compose` v2 plugin | The compose file uses `env_file:` and external networks. |
| `openssl` | Generates the 32-byte master key on first run. |
| Ports 80, 443 free on the host | Pingora binds them for HTTP/HTTPS routing. |
| Ports 9417, 9418 free on host loopback | Controller gRPC + dashboard. |
| Root (or `sudo`) | Default install paths are `/etc/isengard` and `/var/lib/isengard`. Override `ISENGARD_PREFIX` + `ISENGARD_ETC` to install rootlessly. |
| Interactive TTY | The script prompts for secret values with `read -s`; not pipe-safe past the first run. |

## Install

Pull the script down with `curl`, eyeball it, then run with `sudo bash`:

```sh
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh -o install.sh
less install.sh
sudo bash install.sh
```

The one-liner (`curl ... | sudo bash`) is documented but not recommended for first-time installs: piping past `sudo` confuses the TTY check that secret prompts depend on.

To pin to a specific tag:

```sh
ISENGARD_REF=v0.3.5 ISENGARD_IMAGE_TAG=v0.3.5 \
  sudo -E bash install.sh
```

### Transcript a fresh user sees

```
[isengard] Isengard install starting (ref=next, prefix=/var/lib/isengard)
[isengard] preflight: checking dependencies
[isengard] preflight: docker 27.4.0, compose v2.30.3
[isengard] setup: ensuring /etc/isengard and /var/lib/isengard exist
[isengard] key: generating fresh 32-byte master key at /etc/isengard/master.key
[isengard] key: master key created. Operator never sees the value; back up the file out of band.
[isengard] secrets: pulling controller image so the bootstrap one-shots can run

  =====================================================================
  Bootstrapping secrets. Each value is encrypted with the master key
  and written to the controller's SQLite. Plaintext is NEVER stored
  on disk; values you enter here are not echoed and not logged.

  Press Enter on an empty line to skip any optional secret.
  =====================================================================

  Cloudflare DNS API token (DNS-01 wildcards) (press Enter to skip):
[isengard]   bootstrap: cf_dns_api_token
  Backup passphrase (encrypted snapshots) (press Enter to skip):
[isengard]   bootstrap: backup_passphrase
[isengard] secrets: done
[isengard] env: prompting for non-secret config (visible input)
  ACME contact email (leave blank for internal-only deploys): ops@example.com
  ACME pre-issue domains, comma-separated (e.g. *.example.com,foo.example.com): *.example.com
  ACME directory URL [default: Let's Encrypt staging]:
[isengard] env: writing template to /etc/isengard/isengard.env
[isengard] compose: writing /etc/isengard/compose.yaml
[isengard] network: creating isengard-proxy
[isengard] images: pulling latest
[isengard] stack: bringing up via docker compose up -d
  =====================================================================
  Isengard is up.
  ...
```

## Post-install

Day-to-day operations do **not** require `sudo`. The operator's user (in the `docker` group) can run `docker compose` and edit the editable configs directly:

```sh
docker compose -f /etc/isengard/compose.yaml ps
docker compose -f /etc/isengard/compose.yaml pull
docker compose -f /etc/isengard/compose.yaml up -d --force-recreate controller
$EDITOR /etc/isengard/isengard.env       # flip ACME staging -> production etc.
```

Permission model (set by `install.sh`):

| File | Mode | Group | Why |
|---|---|---|---|
| `/etc/isengard/isengard.env` | `0664` | `docker` | non-secret config; operator edits without sudo |
| `/etc/isengard/compose.yaml` | `0664` | `docker` | non-secret; same as above |
| `/etc/isengard/ca.pem` | `0644` | `root` | public CA cert; readable but read-only |
| `/etc/isengard/master.key` | `0600` | `root` | gates the secrets store; only read by the container as uid 0 via bind-mount |

Secrets (Cloudflare API token, backup passphrase, etc.) live encrypted in the controller's SQLite, gated by `master.key`. Manage them via `isd secret put|list|rm`.

Mint an enrollment token so the agent can talk to the controller:

```sh
docker exec iso-controller isengard controller token mint --role agent
```

The dashboard is at `http://127.0.0.1:9418` by default. Front it with a Cloudflare Tunnel or reverse proxy for remote access.

To add or rotate a secret after install:

```sh
# Day-to-day path: against the running stack.
isd login http://127.0.0.1:9418
printf '%s' "$NEW_TOKEN" | isd secret put cf_dns_api_token
```

To rotate the master key: there is no in-place rotation in v0.3.6. The encrypted ciphertexts are bound to the current key; a new key would invalidate every row. Rotation is a follow-up; until then, treat the master key the way you treat a Swarm cluster's raft unlock key.

## Threat model

- The master key is the single thing on the host filesystem that gates access to every stored secret. It's mode `0600 root:root` at `/etc/isengard/master.key`. Same threat profile as Docker Swarm's default raft unlock key: any process running as root on the host can read it, any operator with `sudo` can read it, but it doesn't appear in env vars, in `docker inspect`, or in `ps` output.
- The encrypted DB at `/var/lib/isengard/controller/isengard.db` holds ChaCha20-Poly1305 ciphertexts. Without the master key it's a pile of opaque bytes.
- Secret values that the operator types at the install prompt live in the kernel pipe and the bootstrap one-shot's process memory only. They never hit the host filesystem in plaintext form.
- **Improvement path:** an autolock mode where the master key is operator-typed on every controller restart is a follow-up flag. With autolock enabled, the host filesystem holds no secret material at rest; the trade-off is an interactive unlock on every boot.

## Configurable env

Every variable is documented in `install/isengard.env.example`. The defaults are tuned for production: loopback binds for control plane, public binds for the proxy, conservative log levels, and Let's Encrypt staging until you flip to prod ACME explicitly.

The full list of script-level overrides (set before piping to bash):

| Variable | Default | Purpose |
|---|---|---|
| `ISENGARD_PREFIX` | `/var/lib/isengard` | Bind-mount root for state. |
| `ISENGARD_ETC` | `/etc/isengard` | Where env file + compose.yaml live. |
| `ISENGARD_ENV_FILE` | `${ISENGARD_ETC}/isengard.env` | Env file path. |
| `ISENGARD_COMPOSE_FILE` | `${ISENGARD_ETC}/compose.yaml` | Compose file path. |
| `ISENGARD_MASTER_KEY` | `${ISENGARD_ETC}/master.key` | Master key file path. |
| `ISENGARD_REF` | `next` | Git ref to fetch install assets from. |
| `ISENGARD_RAW_BASE` | computed from ref | Base URL for raw fetches; override for forks. |
| `ISENGARD_PROXY_NETWORK` | `isengard-proxy` | Shared external docker network name. |
| `ISENGARD_CONTROLLER_IMAGE` | `ghcr.io/weavers-engineering/isengard-controller:next` | Image used for the bootstrap one-shots. |
| `ISENGARD_REINSTALL_MODE` | unset | When set, pre-answers the reinstall menu. One of: `refresh-compose`, `refresh-config`, `wipe`, `abort`. |
| `ISENGARD_WIPE_YES` | unset | When `1`, bypasses the literal-`WIPE` confirmation in the wipe action. Required for non-interactive `ISENGARD_REINSTALL_MODE=wipe`. |

## Updating

Re-run `install.sh`. It calls `docker compose pull` followed by `up -d`, which is the canonical update path: containers are recreated only if their image digest changed.

## Uninstalling

```sh
sudo bash /etc/isengard/uninstall.sh           # stop containers, keep state
sudo bash /etc/isengard/uninstall.sh --purge   # also delete /var/lib/isengard
sudo bash /etc/isengard/uninstall.sh --purge --network  # also drop proxy net
```

If you didn't keep the install/ directory around, fetch the script the same way as install.sh:

```sh
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/uninstall.sh | sudo bash
```

## Differences from the dev recipe

`docker/compose.yaml` is the development recipe. It's still the right thing for contributors iterating on the codebase. The production recipe at `install/compose.yaml` differs:

| Concern | Dev (`docker/`) | Prod (`install/`) |
|---|---|---|
| Image source | GHCR `:next` or local `:dev` build | GHCR pinned tag (default `:next`) |
| `build:` directive | yes (via `compose.dev.yaml`) | no |
| State storage | named volumes | absolute bind-mounts under `${ISENGARD_PREFIX}` |
| Env source | `compose.yaml` defaults + shell exports | `/etc/isengard/isengard.env` |
| Master key | `docker/secrets/master.key` (operator generates manually) | `/etc/isengard/master.key` (install.sh generates) |
| Compose file location | repo working tree | `/etc/isengard/compose.yaml` |
| Proxy port bind | `127.0.0.1:80` / `127.0.0.1:443` | `0.0.0.0:80` / `0.0.0.0:443` |
| Bring-up command | `just up` / `just dev` | `bash install.sh` |
