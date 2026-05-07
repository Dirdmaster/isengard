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

# 6. Bring up the example stack so the agent has something to manage
docker compose -p hello -f docker/hello-stack.yaml up -d
```

The CA export step is a current rough edge — Phase 14's mTLS makes it unavoidable today. The pending `swarm-style enrollment join command` PR rolls these steps into a single `docker run …` line that bundles the token + base64-encoded CA + URL.

## Conventions used

- Controller listens on `0.0.0.0:9417` (gRPC) + `0.0.0.0:9418` (dashboard). Bound to `127.0.0.1` on the host so the dashboard is reachable at `http://127.0.0.1:9418` without exposing it publicly.
- Agent reaches the controller over the Compose network DNS name (`controller:9417`), not via host loopback.
- Agent mounts `/var/run/docker.sock` from the host so it can manage containers running outside its own Compose project. The example stack runs as a separate Compose project (`-p hello`) so the agent sees it through the host socket.
- State lives on named Docker volumes. Replace with bind mounts to a backed-up host path for production.

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
