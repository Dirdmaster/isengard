# Isengard production install

The supported way to put Isengard on a server. One command, no source checkout, no Justfile, no Rust toolchain. Pulls signed images from GHCR and brings up the controller + agent + shared proxy network with documented defaults.

## What this does

`install/install.sh` is a single bash script that:

1. Verifies docker + docker compose v2 are present.
2. Creates `/etc/isengard/` (config) and `/var/lib/isengard/{controller,agent,stacks}/` (state) on the host.
3. On first run, writes a commented env template to `/etc/isengard/isengard.env`, prints the variables you need to fill in, and exits. You edit the file in place.
4. On subsequent runs, drops `compose.yaml` into `/etc/isengard/`, creates the `isengard-proxy` docker network if missing, pulls the GHCR images, and brings up the stack via `docker compose up -d`.

Everything is idempotent. Re-running with all pieces in place is a no-op (apart from `compose pull` checking for image updates).

## What you need before running

| Requirement | Why |
|---|---|
| Linux host with docker engine 24+ | Container runtime. |
| `docker compose` v2 plugin | The compose file uses `env_file:` and external networks. |
| Ports 80, 443 free on the host | Pingora binds them for HTTP/HTTPS routing. |
| Ports 9417, 9418 free on host loopback | Controller gRPC + dashboard. |
| Root (or `sudo`) | Default install paths are `/etc/isengard` and `/var/lib/isengard`. Override `ISENGARD_PREFIX` + `ISENGARD_ETC` to install rootlessly. |
| (optional) Cloudflare DNS API token | Required only for DNS-01 wildcard certs. |

## Install (the one-liner)

```sh
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh | sudo bash
```

Or, if you'd rather review before running:

```sh
curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/next/install/install.sh -o install.sh
less install.sh
sudo bash install.sh
```

The first invocation writes `/etc/isengard/isengard.env`, prints the vars to set, and exits. Edit the file. Re-run the same command. The second invocation pulls images and starts the stack.

To pin to a specific tag instead of `next`:

```sh
ISENGARD_REF=v0.3.5 ISENGARD_IMAGE_TAG=v0.3.5 \
  curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/Isengard/v0.3.5/install/install.sh | sudo -E bash
```

## Post-install

Mint an enrollment token so the agent can talk to the controller:

```sh
docker exec iso-controller isengard controller token mint --role agent
```

Paste the token into `/etc/isengard/isengard.env` as `ISENGARD_ENROLL_TOKEN=...` and re-run `install.sh`, OR pass it inline:

```sh
ISENGARD_ENROLL_TOKEN=<token> sudo -E bash install.sh
```

The agent persists its mTLS cert after first successful enrollment; you can clear `ISENGARD_ENROLL_TOKEN` from the env file afterward.

Operator CLI (build once on a workstation, not on the server):

```sh
git clone https://github.com/Weavers-Engineering/Isengard
cd Isengard
just isd-build
target/release/isd login http://<your-server>:9418
target/release/isd ps
```

The dashboard is at `http://127.0.0.1:9418` by default. Front it with a Cloudflare Tunnel or reverse proxy for remote access; do not bind it directly to a public interface.

## Configurable env

Every variable is documented in `install/isengard.env.example`. The defaults are tuned for production: loopback binds for control plane, public binds for the proxy, conservative log levels, and Let's Encrypt staging until you flip to prod ACME explicitly.

The full list of script-level overrides (set before piping to bash):

| Variable | Default | Purpose |
|---|---|---|
| `ISENGARD_PREFIX` | `/var/lib/isengard` | Bind-mount root for state. |
| `ISENGARD_ETC` | `/etc/isengard` | Where env file + compose.yaml live. |
| `ISENGARD_ENV_FILE` | `${ISENGARD_ETC}/isengard.env` | Env file path. |
| `ISENGARD_COMPOSE_FILE` | `${ISENGARD_ETC}/compose.yaml` | Compose file path. |
| `ISENGARD_REF` | `next` | Git ref to fetch install assets from. |
| `ISENGARD_RAW_BASE` | computed from ref | Base URL for raw fetches; override for forks. |
| `ISENGARD_PROXY_NETWORK` | `isengard-proxy` | Shared external docker network name. |

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
| Compose file location | repo working tree | `/etc/isengard/compose.yaml` |
| Proxy port bind | `127.0.0.1:80` / `127.0.0.1:443` | `0.0.0.0:80` / `0.0.0.0:443` |
| Backup passphrase | hard-coded test value | env-supplied secret |
| Bring-up command | `just up` / `just dev` | `bash install.sh` |
