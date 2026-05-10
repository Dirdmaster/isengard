# Isengard task runner. Run `just` to see available commands.

set shell := ["bash", "-cu"]

# Default: list available commands
default:
    @just --list

# === Build ===

# Build all workspace crates (debug)
build:
    cargo build --workspace

# Build the release binary
release:
    cargo build --release -p isengard

# Build the operator CLI (`isd`) in release mode (v0.3a)
isd-build:
    cargo build --release -p isd

# === Test ===

# Run all tests with cargo-nextest if available, fallback to cargo test
test:
    @if command -v cargo-nextest >/dev/null 2>&1; then \
        cargo nextest run --workspace; \
    else \
        echo "(install cargo-nextest for faster runs: cargo install cargo-nextest)"; \
        cargo test --workspace; \
    fi

# === Lint / format ===

# Run clippy with -D warnings
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Check formatting (no writes)
fmt-check:
    cargo fmt --check

# Apply formatting
fmt:
    cargo fmt

# Run the EXACT CI gates inside the OrbStack `wisp` Linux VM. Catches
# Mac-vs-Linux divergence (cfg-gated unused imports, clippy lint
# differences, dashboard build env). Same checks the pre-push
# `linux-mirror` hook runs.
ci-linux:
    @if ! command -v orb >/dev/null 2>&1; then \
        echo "ERROR: OrbStack not installed; install from https://orbstack.dev"; exit 1; \
    fi
    @if ! orbctl list 2>/dev/null | awk '{print $1}' | grep -qx 'wisp'; then \
        echo "ERROR: no 'wisp' OrbStack machine; create with: orb create ubuntu:noble wisp"; exit 1; \
    fi
    orb -m wisp bash -lc "set -euo pipefail; source ~/.cargo/env; cd '$(pwd)'; export RUSTFLAGS='-D warnings'; cargo fmt --check; cargo clippy --workspace --all-targets -- -D warnings; if command -v cargo-nextest >/dev/null 2>&1; then cargo nextest run --workspace; else cargo test --workspace; fi"

# === Local dev ===

# Run the binary in agent mode
agent *ARGS:
    cargo run -p isengard -- agent {{ARGS}}

# Run the binary in controller mode
controller *ARGS:
    cargo run -p isengard -- controller {{ARGS}}

# === Marketing site ===

# Run the Nuxt landing site dev server (uses bun)
www:
    cd www && bun run dev

# Build the marketing site
www-build:
    cd www && bun run build

# === Local dev (Docker compose stack) ===

# Compose file pair used by every recipe below. Layered: base + dev override.
# Note: OrbStack / Colima / Rancher Desktop users must export DOCKER_SOCK
# in the same shell before `just dev`. See docker/README.md.
compose_args := "-f docker/compose.yaml -f docker/compose.dev.yaml"

# Idempotently create the shared `isengard-proxy` external network. Both the
# control-plane stack and any routed operator stacks attach to it so pingora
# and backends share an L3 fabric (Traefik recipe). `2>/dev/null || true`
# keeps re-runs silent when the network already exists.
net-up:
    @docker network create isengard-proxy 2>/dev/null || true
    @docker network inspect isengard-proxy >/dev/null 2>&1 \
        && echo "✓ network isengard-proxy ready" \
        || { echo "ERROR: failed to create or inspect isengard-proxy network"; exit 1; }

# Build local images + bring up controller + agent (compose dev override).
# Depends on `net-up` so a fresh clone doesn't fail at compose-up time on a
# missing external network.
dev: net-up
    docker compose {{compose_args}} up -d --build
    @echo ""
    @echo "Dashboard: http://127.0.0.1:9418"
    @echo "If this is a fresh stack, mint a token: just mint-token"

# Bring up with current images (no rebuild). Same net-up dep as `dev`.
up: net-up
    docker compose {{compose_args}} up -d

# Stop everything (keeps volumes + enrollment state)
down:
    -docker compose -p hello -f docker/hello-stack.yaml down 2>/dev/null
    docker compose {{compose_args}} down

# Full reset: down + remove volumes (loses enrollment + state)
nuke:
    @read -r -p "This will delete all enrollment + state + volumes. Type 'nuke' to confirm: " confirm; \
        [ "$confirm" = "nuke" ] || { echo "Aborted."; exit 1; }
    -docker compose -p hello -f docker/hello-stack.yaml down -v 2>/dev/null
    docker compose {{compose_args}} down -v

# Mint an enrollment token; renders the docker run join command
mint-token:
    docker exec iso-controller isengard controller token mint --role agent --public-addr controller.local:9417

# Bring up the example managed stack (separate Compose project so the agent
# sees it via the host docker socket). Depends on `net-up` so the stack's
# `isengard-proxy: external: true` reference resolves on a fresh host.
hello: net-up
    docker compose -p hello -f docker/hello-stack.yaml up -d

# Tail all logs (Ctrl+C exits)
logs:
    docker compose {{compose_args}} logs -f

# Tail just the controller
logs-controller:
    docker logs -f iso-controller

# Tail just the agent
logs-agent:
    docker logs -f iso-agent

# Force-rebuild local images without bringing them up
build-images:
    docker compose {{compose_args}} build

# Switch back to GHCR :next images. Useful for "is this a regression in my
# local build, or is it broken on next too?"
prod:
    docker compose -f docker/compose.yaml pull controller agent
    docker compose -f docker/compose.yaml up -d --no-build

# === Smoke / demo (full controller + agent end-to-end on Docker) ===

ctrl := "isengard-controller"
agent := "isengard-agent"
ca_pem := "/tmp/isengard-ca.pem"
http_port := "9418"
grpc_port := "9417"

# Wipe any previous smoke run (containers + named volumes + extracted CA)
smoke-clean:
    @echo "→ stopping + removing containers"
    -docker rm -f {{ctrl}} {{agent}} 2>/dev/null
    @echo "→ removing volumes"
    -docker volume rm {{ctrl}}-data {{agent}}-data 2>/dev/null
    @echo "→ removing extracted CA"
    -rm -f {{ca_pem}}
    @echo "✓ clean"

# Pull both :next images from GHCR (use this for the published flow)
smoke-pull:
    docker pull --platform linux/amd64 ghcr.io/dirdmaster/isengard-controller:next
    docker pull --platform linux/amd64 ghcr.io/dirdmaster/isengard-agent:next

# Build both images locally from the working-tree Dockerfile (use when iterating
# on uncommitted changes — no GHA round-trip)
smoke-build:
    @echo "→ building isengard-controller:local"
    docker build --platform linux/amd64 --target controller -t isengard-controller:local .
    @echo "→ building isengard-agent:local"
    docker build --platform linux/amd64 --target agent -t isengard-agent:local .

# Internal: bring up controller + agent given image refs
[private]
_smoke-up ctrl_img agent_img:
    #!/usr/bin/env bash
    set -e
    echo "→ starting controller ({{ctrl_img}})"
    docker run -d --name {{ctrl}} --restart=always \
      --platform linux/amd64 \
      -p {{grpc_port}}:9417 -p {{http_port}}:9418 \
      -v {{ctrl}}-data:/var/lib/isengard \
      "{{ctrl_img}}" >/dev/null
    echo "→ waiting for controller HTTP"
    for i in $(seq 1 30); do
      curl -fsS -o /dev/null "http://localhost:{{http_port}}/" && break
      sleep 1
      [ "$i" = "30" ] && { echo "controller didn't start; logs:"; docker logs {{ctrl}}; exit 1; }
    done
    echo "→ extracting CA"
    docker exec {{ctrl}} isengard controller ca export > {{ca_pem}}
    echo "→ minting enrollment token (15m)"
    TOKEN=$(docker exec {{ctrl}} isengard controller token mint --ttl 15m | tr -d '[:space:]')
    echo "→ starting agent ({{agent_img}})"
    docker run -d --name {{agent}} --restart=always \
      --platform linux/amd64 \
      --add-host controller.local:host-gateway \
      -v /var/run/docker.sock:/var/run/docker.sock \
      -v {{agent}}-data:/var/lib/isengard \
      -v {{ca_pem}}:/etc/isengard/ca.pem:ro \
      -e ISENGARD_CONTROLLER=https://controller.local:9417 \
      -e ISENGARD_ENROLL_TOKEN="$TOKEN" \
      -e ISENGARD_CONTROLLER_CA_PEM_PATH=/etc/isengard/ca.pem \
      --group-add $(stat -f %g /var/run/docker.sock) \
      "{{agent_img}}" >/dev/null
    echo "→ waiting for agent enrollment"
    for i in $(seq 1 30); do
      docker exec {{ctrl}} isengard controller agent list 2>/dev/null | grep -q . && { echo "✓ agent enrolled"; break; }
      sleep 1
      [ "$i" = "30" ] && { echo "agent didn't enroll; logs:"; docker logs {{agent}}; exit 1; }
    done
    echo ""
    echo "✓ smoke ready"
    echo "  dashboard:    http://localhost:{{http_port}}/"
    echo "  controller:   docker logs -f {{ctrl}}"
    echo "  agent:        docker logs -f {{agent}}"
    echo "  list agents:  docker exec {{ctrl}} isengard controller agent list"
    echo "  revoke agent: docker exec {{ctrl}} isengard controller agent revoke <host-id>"
    echo "  teardown:     just smoke-clean"

# Smoke test using published :next images from GHCR (most common path)
smoke: smoke-clean smoke-pull (_smoke-up "ghcr.io/dirdmaster/isengard-controller:next" "ghcr.io/dirdmaster/isengard-agent:next")

# Smoke test using locally-built images (use when iterating on uncommitted code)
smoke-local: smoke-clean smoke-build (_smoke-up "isengard-controller:local" "isengard-agent:local")

# === Maintenance ===

clean:
    cargo clean
    rm -rf www/.nuxt www/.output

# Pre-commit gate: fmt + lint + test + cargo-deny (mirrors CI exactly).
# cargo-deny is required — it's the gate that catches advisories CI blocks on.
ci-local: fmt-check lint test
    @if ! command -v cargo-deny >/dev/null 2>&1; then \
        echo "ERROR: cargo-deny is not installed. Run: cargo install cargo-deny"; \
        echo "       (or: just install-hooks  — bootstraps it)"; \
        exit 1; \
    fi
    cargo deny check
    @echo "✓ ci-local passed"

# Install lefthook git hooks AND bootstrap cargo-deny so the local gate
# matches CI exactly. Run this once after cloning.
install-hooks:
    @if ! command -v lefthook >/dev/null 2>&1; then \
        echo "lefthook not installed. install with: brew install lefthook  (or: cargo install lefthook)"; \
        exit 1; \
    fi
    @if ! command -v cargo-deny >/dev/null 2>&1; then \
        echo "Installing cargo-deny (one-time, ~1 min)..."; \
        cargo install cargo-deny; \
    fi
    lefthook install
    @echo "✓ pre-push hook installed (fmt-check + clippy + test + cargo-deny)"

# === Design ===

# Open the design concepts index in your default browser
design:
    @bash design/regen-index.sh
    @open design/concepts/_index.html

# Regenerate design/concepts/_index.html (after adding/removing a concept)
design-index:
    @bash design/regen-index.sh

# Scaffold a new dated concept HTML file. Usage: just concept hosts
concept name:
    #!/usr/bin/env bash
    DATE=$(date +%Y-%m-%d)
    FILE="design/concepts/${DATE}-{{name}}-v1.html"
    if [ -f "$FILE" ]; then
        # If v1 exists, find next available version
        N=2
        while [ -f "design/concepts/${DATE}-{{name}}-v${N}.html" ]; do N=$((N+1)); done
        FILE="design/concepts/${DATE}-{{name}}-v${N}.html"
    fi
    cp design/concepts/_shell.html "$FILE"
    sed -i.bak "s/{{{{TITLE}}}}/{{name}}/g" "$FILE" && rm "$FILE.bak"
    echo "Created $FILE"

# Scaffold a new dated decision (ADR) markdown file. Usage: just decision bottom-bar
decision name:
    #!/usr/bin/env bash
    DATE=$(date +%Y-%m-%d)
    FILE="design/decisions/${DATE}-{{name}}.md"
    if [ -f "$FILE" ]; then
        echo "Already exists: $FILE"
        exit 1
    fi
    cat > "$FILE" <<'TPL'
    ---
    type: decision
    status: draft
    date: PLACEHOLDER_DATE
    tags:
      - design
      - decision
    ---

    # {{name}}

    ## Context

    ## Options considered

    ## Decision

    ## Consequences
    TPL
    sed -i.bak "s/PLACEHOLDER_DATE/${DATE}/" "$FILE" && rm "$FILE.bak"
    echo "Created $FILE"
