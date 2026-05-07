# Running Isengard with Docker

The recommended way to run Isengard in any environment: development, homelab, production. The native binary is available for contributors but is not a supported install path.

## Files

| File | Purpose |
|---|---|
| `compose.yaml` | The control plane: one controller + one agent on a Compose bridge network. The starting point for any deployment. |
| `hello-stack.yaml` | A minimal example of a stack the agent will manage. Start here, then replace with your own services. |

Every recipe pins to `:next` images on GHCR. For a tagged release, swap `:next` for the version (e.g. `:v0.2.0`).

## Quick start

```sh
# 0. Create the shared proxy network (one-time, idempotent). Pingora and
#    every routed container share this network so they have L3 reachability.
#    See "Shared proxy network" below for the rationale (Traefik recipe).
docker network create isengard-proxy 2>/dev/null || true

# 1. Start the controller (it generates a self-signed CA on first boot)
docker compose -f docker/compose.yaml up -d controller

# 2. Export the controller's CA so the agent can validate the TLS handshake
docker exec iso-controller isengard controller ca export > docker/ca.pem

# 3. Mint an enrollment token (15-minute TTL by default)
TOKEN=$(docker exec iso-controller isengard controller token mint --role agent)
echo "Token: $TOKEN"

# 4. Start the agent with the token
ISENGARD_ENROLL_TOKEN=$TOKEN \
  docker compose -f docker/compose.yaml up -d agent

# 5. Open the dashboard
open http://127.0.0.1:9418

# 6. (Optional) Save controller credentials for the `isd` operator CLI:
#    cargo build -p isd --release
#    target/release/isd login https://127.0.0.1:9417
#    target/release/isd ps

# 7. Bring up the example stack so the agent has something to manage
docker compose -p hello -f docker/hello-stack.yaml up -d
```

Note (v0.3): Isengard does not run DNS. Pingora (the agent's reverse proxy) is published on host loopback at `127.0.0.1:80` and `127.0.0.1:443`; you bring your own DNS pointing at the host. Two common patterns:

- `/etc/hosts` line: `127.0.0.1 hello.local` (or whatever `public_hostname` you set on the routing rule). Per-host, zero infra.
- A real DNS / mDNS / Tailscale MagicDNS / corporate split-horizon entry pointing the public hostname at the host running the agent. This is the Traefik / Nginx Proxy Manager model.

The CA export step is a current rough edge — Phase 14's mTLS makes it unavoidable today. The pending `swarm-style enrollment join command` PR rolls these steps into a single `docker run …` line that bundles the token + base64-encoded CA + URL.

## Local dev (no GHCR wait)

If you've cloned the repo and want to iterate on backend / dashboard / Dockerfile changes without waiting for GHCR rebuilds:

```sh
just dev
```

Builds local images tagged `iso-controller:dev` / `iso-agent:dev` and brings up the stack with the dev compose override (`docker/compose.dev.yaml` layered on top of `docker/compose.yaml`). Roughly 1-2 min on a warm cargo cache, 5+ min cold.

Useful follow-ups:

```sh
just mint-token       # render the docker run join command
just hello            # bring up the example managed stack
just logs-agent       # tail agent logs
just logs-controller  # tail controller logs
just down             # stop (keeps volumes + enrollment state)
just nuke             # full reset (deletes enrollment + state + volumes)
just prod             # switch back to GHCR :next images
```

OrbStack / Colima / Rancher Desktop users: export `DOCKER_SOCK=...` in the same shell before `just dev` (see the table below).

## Shared proxy network

The agent's pingora reaches operator stacks over a single shared external network: `isengard-proxy`. This is the same recipe Traefik documents for its `proxy: external: true` model.

Why: by default the agent runs on its own compose-project bridge (`isengard_default`) and operator stacks on theirs (`hello_default`). Different bridges, no L3 reachability. Pingora gets a routing rule with a container IP it cannot reach, the healthcheck evicts the upstream, and clients see 503.

Joining a single shared network solves it: agent + every routed container sit on the same L3 fabric, the agent's container-IP discovery prefers that network's IP, and pingora has a path.

### One-time setup (per host)

```sh
docker network create isengard-proxy
```

Or run `just net-up` (idempotent; safe to re-run). `just dev`, `just up`, and `just hello` all depend on this recipe so a fresh clone bootstraps the network automatically.

### Opting a stack in

Add a top-level `networks:` block declaring `isengard-proxy: external: true`, then attach each routed service to it. The `hello-stack.yaml` in this directory is a worked example:

```yaml
networks:
  default:
  isengard-proxy:
    external: true
    name: isengard-proxy

services:
  hello:
    networks:
      - default          # intra-stack talk
      - isengard-proxy   # shared fabric for pingora
```

Stacks that don't join `isengard-proxy` are still discovered via `isengard.expose*` labels, but pingora can't reach them across bridges. The agent's discovery falls back to the first non-driver network IP and the healthcheck evicts the rule. The clear next step in that case is "join `isengard-proxy`."

### Verify

```sh
# Confirm the network exists and the agent + routed containers are on it
docker network inspect isengard-proxy --format '{{range .Containers}}{{.Name}} {{.IPv4Address}}{{"\n"}}{{end}}'

# Hit the proxy from the host (assumes hello.local routing rule applied)
curl -sS -H "Host: hello.local" http://127.0.0.1/         # HTTP 200
curl -sSk -H "Host: hello.local" https://127.0.0.1/       # HTTP 200 (-k for self-signed)
```

If `curl` returns 503 with `no_route_for_host`, inspect the agent log: a line like `proxy: ProxyConfig rule has empty container_ip; falling back to 127.0.0.1` means discovery couldn't find an IP and the operator stack is probably not on `isengard-proxy`.

## Conventions used

- Controller listens on `0.0.0.0:9417` (gRPC) + `0.0.0.0:9418` (dashboard). Bound to `127.0.0.1` on the host so the dashboard is reachable at `http://127.0.0.1:9418` without exposing it publicly.
- Agent reaches the controller over the Compose network DNS name (`controller:9417`), not via host loopback.
- Agent mounts `/var/run/docker.sock` from the host so it can manage containers running outside its own Compose project. The example stack runs as a separate Compose project (`-p hello`) so the agent sees it through the host socket.
- State lives on named Docker volumes. Replace with bind mounts to a backed-up host path for production.

## Docker socket path (non-Desktop runtimes)

The agent bind-mounts the host Docker socket so it can manage containers running outside its own Compose project. `compose.yaml` defaults to `/var/run/docker.sock`, which works for Docker Desktop and vanilla Linux. For other Mac runtimes, export `DOCKER_SOCK` before bringing up the agent:

| Runtime | Socket path |
|---|---|
| Docker Desktop / Linux | `/var/run/docker.sock` (default, no override needed) |
| OrbStack | `$HOME/.orbstack/run/docker.sock` |
| Colima | `$HOME/.colima/default/docker.sock` |
| Rancher Desktop | `$HOME/.rd/docker.sock` |

```sh
export DOCKER_SOCK=$HOME/.orbstack/run/docker.sock
docker compose -f docker/compose.yaml up -d agent
```

Confirm what your context uses with `docker context inspect | grep -i host`.

## Architecture

Images currently ship `linux/amd64` only. Apple Silicon (arm64) Macs need to pull the amd64 manifest and run under Rosetta — `docker/compose.yaml` already pins `platform: linux/amd64` for both services so this works out of the box. Linux/amd64 hosts ignore the line and run native.

The earlier multi-arch attempt produced corrupt arm64 manifests because the Dockerfile pinned the build stage to `--platform=$BUILDPLATFORM`, so cargo always emitted amd64 binaries inside the arm64 manifest entry. Restored to amd64-only at `2085d55`. Proper arm64 (option B: drop the BUILDPLATFORM pin and let buildx run under QEMU) is on the v0.3 list — slow CI (~30min cold) but multi-arch correct.

## Adapting for production

The shape stays the same; the differences are typically:

- Bind-mount state-dirs to a backed-up host path instead of named volumes
- Put the controller behind Cloudflare Tunnel / reverse proxy instead of binding 9418 to host
- Set `RUST_LOG=warn` instead of `info`
- Source the backup passphrase, notifier tokens, webhook secrets, etc. from a secrets manager
- Pin to a tagged release (`:v0.2.0`), not `:next`
- Run multiple agents on different hosts pointing at the same controller (each gets its own enrollment token)

## Tear down

```sh
docker compose -p hello -f docker/hello-stack.yaml down -v
docker compose -f docker/compose.yaml down -v
```

The `-v` removes named volumes; drop it to keep state across restarts.
