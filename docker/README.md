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

The agent owns the `isengard-proxy` ingress network. When a route targets a
Docker bridge-networked container, the agent creates the network if needed,
attaches the target container, and routes to the IP on that network.

Operators do not need to edit Compose files just to make a routed service
reachable. Host-networked containers are routed through the Docker host
gateway. Containers using Docker `none` networking cannot be routed and show
an unresolved route reason.

### Diagnose

```sh
# Inspect the ingress network when diagnosing routing reachability
docker network inspect isengard-proxy --format '{{range .Containers}}{{.Name}} {{.IPv4Address}}{{"\n"}}{{end}}'

# Hit the proxy from the host (assumes hello.local routing rule applied)
curl -sS -H "Host: hello.local" http://127.0.0.1/         # HTTP 200
curl -sSk -H "Host: hello.local" https://127.0.0.1/       # HTTP 200 (-k for self-signed)
```

If `curl` returns 503 with `no_route_for_host`, inspect the agent log for the
route's unresolved reason.

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

## Secrets (v0.3.6)

Isengard ships a Docker-Swarm-style managed secrets store: operator-supplied values are encrypted at rest in the controller's SQLite, ferried to agents over mTLS at container start, materialised on tmpfs, and bind-mounted at `/run/secrets/<name>` inside the workload container. Plaintext never lives on disk on the agent.

### Bootstrap

The controller reads a 32-byte master key from a bind-mounted file (default `/run/secrets/master.key`, override with `ISENGARD_MASTER_KEY_FILE`). The installer (`install/install.sh`) generates the key on first run via `openssl rand 32`, writes it to `/etc/isengard/master.key` mode 0600 root, and bind-mounts it into the controller container.

For dev:

```sh
mkdir -p docker/secrets
head -c 32 /dev/urandom > docker/secrets/master.key
chmod 600 docker/secrets/master.key
docker compose -f docker/compose.yaml up -d controller
```

The dev compose bind-mounts `docker/secrets/master.key` into the controller as `/run/secrets/master.key`. The controller refuses to start without it.

The operator never types the master key. Day-to-day secrets management uses `isd secret put|list|rm` against the running dashboard; the master key is needed only at install time and on every controller boot (to decrypt at-rest ciphertexts).

### Load a value

```sh
# From a file:
isd secret put cf_token --from-file ~/secrets/cloudflare.token

# From stdin (the operator's preferred path; nothing leaks to shell history):
printf '%s' "$CF_TOKEN" | isd secret put cf_token

# List names + timestamps. The CLI never prints values.
isd secret list

# Replace an existing value. `put` is upsert.
printf '%s' "$NEW_TOKEN" | isd secret put cf_token

# Delete.
isd secret rm cf_token
```

### Reference from compose

Top-level `secrets:` declares the names the agent should fetch; per-service `secrets:` mounts them into the workload container.

```yaml
services:
  cloudflared:
    image: cloudflare/cloudflared:latest
    command: ["tunnel", "--token-file", "/run/secrets/cf_token", "run"]
    secrets:
      - cf_token

# Long form with custom mount path:
#   secrets:
#     - source: cf_token
#       target: /etc/cloudflared/credentials.json

secrets:
  cf_token:
    external: true
```

`external: true` is the only supported source in v0.4. File-source secrets (`file: /path`) are intentionally rejected with a clear error pointing at `isd secret put`.

### Limitations (v0.3.6)

- macOS dev: the agent's tmpfs root is a regular tmpdir, not a real `tmpfs` mount (Linux containers run as root with `CAP_SYS_ADMIN` and use a real mem-backed mount). Plaintext on a Mac dev box is on the same filesystem as the rest of the agent's state. Production = Linux.
- Rotation: re-running `isd secret put` writes a new value but currently does NOT roll the dependent containers. Recreate them manually (`isd apply` after a no-op edit, or restart) for the new value to take effect.
- No `isd secret get`. Secrets are write-only from the operator side; the agent is the only consumer that ever sees plaintext. This is intentional.

## Tear down

```sh
docker compose -p hello -f docker/hello-stack.yaml down -v
docker compose -f docker/compose.yaml down -v
```

The `-v` removes named volumes; drop it to keep state across restarts.
