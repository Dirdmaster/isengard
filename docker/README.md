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

Note (v0.3a): the agent advertises any routing rule whose `public_hostname` ends in `.local` over mDNS. macOS Bonjour resolves these natively; Linux clients need `avahi-daemon` (or `systemd-resolved` with `MulticastDNS=yes`) and Windows clients need Bonjour Print Services.

Note (v0.3.5): on macOS, mDNS-in-Docker is broken by design (the agent's responder broadcasts on the Docker bridge inside the OrbStack/Docker Desktop VM; macOS Bonjour listens on real network interfaces; the packets never cross). Use `isd gateway` instead. See "Mac dev gateway" below.

The CA export step is a current rough edge — Phase 14's mTLS makes it unavoidable today. The pending `swarm-style enrollment join command` PR rolls these steps into a single `docker run …` line that bundles the token + base64-encoded CA + URL.

## Mac dev gateway (v0.3.5)

`isd gateway` runs a small DNS resolver + reverse proxy on your Mac that bridges browser traffic to containerized stacks. Single foreground command, Ctrl+C tears down. This replaces v0.3a mDNS for Mac dev (mDNS still works on Linux deployments where Docker is native).

One-time setup:

```sh
# 1. Build the operator CLI
just isd-build

# 2. Save controller credentials (HTTP for dev, prompts for token)
target/release/isd login http://127.0.0.1:9418

# 3. Tell macOS to route the `.isd` zone to the gateway. Idempotent;
#    rewrites the file if you re-run with a different --dns-port.
sudo target/release/isd gateway --install-resolver
```

Run the gateway:

```sh
target/release/isd gateway
# DNS:   listening on 127.0.0.1:5300 for *.isd
# HTTP:  listening on 127.0.0.1:8080
# HTTPS: listening on 127.0.0.1:8443 (self-signed; expect a browser warning)
```

Verify end to end (in another shell):

```sh
dig @127.0.0.1 -p 5300 hello.isd +short          # -> 127.0.0.1
curl -H 'Host: hello.isd' http://127.0.0.1:8080/  # -> hello-web body
open http://hello.isd:8080                        # browser, after install-resolver
```

CLI reference:

| Flag | Default | Notes |
|---|---|---|
| `--zone <name>` | `isd` | Override the zone the gateway serves. |
| `--dns-port <n>` | `5300` | Unprivileged. macOS resolver uses the matching port. |
| `--port <n>` | `8080` | HTTP listener. `--port 80` needs sudo. |
| `--tls-port <n>` | `8443` | HTTPS listener. `--tls-port 443` needs sudo. |
| `--no-tls` | off | Disable the HTTPS listener. |
| `--install-resolver` | off | Write `/etc/resolver/<zone>` and exit (sudo). |
| `--uninstall-resolver` | off | Remove `/etc/resolver/<zone>` and exit (sudo). |
| `--backend-host <host>` | `127.0.0.1` | Backend address the proxy forwards to. Set to e.g. an OrbStack container IP, or to your Docker bridge IP, when ports are not published 1:1 to localhost. |

Limitations (v0.3.5):
- The controller's `/api/v1/routing/rules` endpoint does NOT carry the agent's per-container IP, so the gateway forwards to `<backend-host>:<container_port>` (default `127.0.0.1`). For most dev compose files this means publishing `<container_port>:<container_port>` 1:1 to localhost. With OrbStack, set `--backend-host` to the container's bridge IP.
- TLS certs are self-signed; the browser will warn the first time. Use `mkcert` or accept the warning.
- Live routing refresh polls `/api/v1/routing/rules` every 30s. WS-driven refresh is a follow-up.

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

## DNS resolver (v0.3b)

mDNS (v0.3a, agent-side) handles `.local`. For a custom zone (e.g. `.iso`, `.weavers`, `.lan`), the controller can host a small embedded DNS server that resolves `<public_hostname>.<zone>` to the LAN IP of the agent that owns the matching routing rule.

### Enable

Set `ISENGARD_DNS_ZONE` in the environment (or pass `--dns-zone <name>` on the controller command). Empty string disables the resolver entirely (default).

```sh
ISENGARD_DNS_ZONE=iso docker compose -f docker/compose.yaml up -d controller
```

The compose recipe binds `127.0.0.1:5300/udp` on the host so the resolver is reachable locally without exposing it on a public interface. To bind UDP 53 instead, swap the port and add `cap_add: [NET_BIND_SERVICE]` to the controller service.

### macOS conditional forwarding

Tell the OS to send queries for `*.iso` to the controller's resolver, and everything else to its normal upstreams:

```sh
sudo mkdir -p /etc/resolver
sudo tee /etc/resolver/iso > /dev/null <<'EOF'
nameserver 127.0.0.1
port 5300
EOF
```

The directory-style resolver config takes effect immediately; no restart required. `scutil --dns | grep -A3 iso` confirms the routing.

### Linux conditional forwarding

`systemd-resolved` (per-link domain routing) or `dnsmasq` (server-by-domain) both work. Example dnsmasq snippet:

```conf
server=/iso/127.0.0.1#5300
```

### Verify

```sh
dig @127.0.0.1 -p 5300 +short hello.iso        # returns the agent's LAN IP
dig @127.0.0.1 -p 5300 nonexistent.iso         # NXDOMAIN
dig @127.0.0.1 -p 5300 google.com              # REFUSED (we don't recurse)
```

For names ending in `.local`, the agent's mDNS responder answers; the controller filters those out of its DNS table so the two paths don't collide.

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
