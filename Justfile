# Isengard task runner. Run `just` to see available commands.

set shell := ["bash", "-cu"]

min_free_gb := "50"

# Default: list available commands
default:
    @just --list

# === Build ===

# Build all workspace crates (debug)
build: (_disk-preflight min_free_gb)
    cargo build --workspace

# Build the release binary
release: (_disk-preflight min_free_gb)
    cargo build --release -p isengard

# Build the operator CLI (`isd`) in release mode (v0.3a)
isd-build: (_disk-preflight min_free_gb)
    cargo build --release -p isd

# Install `isd` to ~/.cargo/bin/ from the current checkout. Use after a
# pull on `next` or while iterating on a feature branch. `--force`
# overwrites any existing isd binary; `--path` makes cargo build from
# THIS checkout (not crates.io). Same warm cache as `isd-build`.
isd-install: (_disk-preflight min_free_gb)
    cargo install --path crates/isd --force

# Watch mode for local-first design iteration: auto-reinstall `isd` on
# every source change in `crates/isd/`. Requires `cargo-watch`
# (`cargo install cargo-watch`). Leave running in a side terminal; any
# edit you save triggers a rebuild + install. The operator's running
# `isd` invocations will pick up the new binary on the next launch.
isd-dev: (_disk-preflight min_free_gb)
    @if ! command -v cargo-watch >/dev/null 2>&1; then \
        echo "ERROR: cargo-watch not installed. Run: cargo install cargo-watch"; \
        exit 1; \
    fi
    cargo watch -w crates/isd -x 'install --path crates/isd --force'

# === Test ===

# Run all tests with cargo-nextest if available, fallback to cargo test
test: (_disk-preflight min_free_gb)
    @if command -v cargo-nextest >/dev/null 2>&1; then \
        cargo nextest run --workspace; \
    else \
        echo "(install cargo-nextest for faster runs: cargo install cargo-nextest)"; \
        cargo test --workspace; \
    fi

# Fast PR-loop checks. Mirrors the default pre-push hook.
ci-fast: (_disk-preflight min_free_gb)
    cargo fmt --check
    cargo check --workspace --all-targets
    @if ! command -v cargo-deny >/dev/null 2>&1; then \
        echo "ERROR: cargo-deny is not installed. Run: cargo install cargo-deny"; \
        exit 1; \
    fi
    cargo deny check

# Full native confidence gate. Use before risky merges or live upgrades.
ci-full: (_disk-preflight min_free_gb)
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items
    @if command -v cargo-nextest >/dev/null 2>&1; then \
        cargo nextest run --workspace; \
    else \
        echo "(install cargo-nextest for faster runs: cargo install cargo-nextest)"; \
        cargo test --workspace; \
    fi
    @if ! command -v cargo-deny >/dev/null 2>&1; then \
        echo "ERROR: cargo-deny is not installed. Run: cargo install cargo-deny"; \
        exit 1; \
    fi
    cargo deny check

# === Lint / format ===

# Run clippy with -D warnings
lint: (_disk-preflight min_free_gb)
    cargo clippy --workspace --all-targets -- -D warnings

# Check formatting (no writes)
fmt-check:
    cargo fmt --check

# Check current public docs for stale operator vocabulary
docs-check:
    bash scripts/ci/check_public_docs_vocabulary.sh

# Apply formatting
fmt:
    cargo fmt

# Full native confidence gate. Use before release-grade work.
ci-release: ci-full

# === Local dev ===

# Run the binary in agent mode
agent *ARGS:
    cargo run -p isengard -- agent {{ARGS}}

# Run the binary in controller mode
controller *ARGS:
    cargo run -p isengard -- controller {{ARGS}}

# === Documentation site ===

# Run the current public Docus site dev server (uses bun)
site:
    cd website && bun run dev

# Generate the current public Docus site
site-build:
    cd website && bun run generate

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
dev: (_disk-preflight min_free_gb) net-up
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
build-images: (_disk-preflight min_free_gb)
    docker compose {{compose_args}} build

# Switch back to GHCR :next images. Useful for "is this a regression in my
# local build, or is it broken on next too?"
prod:
    docker compose -f docker/compose.yaml pull controller agent
    docker compose -f docker/compose.yaml up -d --no-build

# === Examples (stack.toml fixture) ===

# Deploy just the `hello` stack from examples/stack-toml/. Assumes a
# controller is reachable and an `isd context` is selected.
example-deploy-hello:
    cd examples/stack-toml && isd stack deploy ./hello

# Deploy every stack in examples/stack-toml/ (currently hello + monitoring).
# Note: the monitoring stack binds `grafana_admin_password`; create it
# first with `isd secret set grafana_admin_password` or POST /stacks
# returns 422.
example-deploy:
    cd examples/stack-toml && isd stack deploy --all

# === Smoke / demo (full controller + agent end-to-end on Docker) ===

ctrl := "isengard-controller"
agent := "isengard-agent"
ca_pem := "/tmp/isengard-ca.pem"
http_port := "19418"
grpc_port := "19417"
showcase_file := "examples/showcase/compose.yaml"
showcase_host := "whoami.isengard.app"
showcase_container := "showcase-whoami"

# Wipe any previous smoke run (containers + named volumes + extracted CA)
smoke-clean:
    @echo "→ stopping + removing containers"
    -docker rm -f {{ctrl}} {{agent}} {{showcase_container}} 2>/dev/null
    @echo "→ removing volumes"
    -docker volume rm {{ctrl}}-data {{agent}}-data 2>/dev/null
    @echo "→ removing extracted CA"
    -rm -f {{ca_pem}}
    @echo "✓ clean"

# Pull both :next images from GHCR (use this for the published flow)
smoke-pull:
    docker pull --platform linux/amd64 ghcr.io/weavers-engineering/isengard-controller:next
    docker pull --platform linux/amd64 ghcr.io/weavers-engineering/isengard-agent:next

# Check prerequisites before the showcase demo claims fixed Docker ports.
[private]
_demo-preflight:
    #!/usr/bin/env bash
    set -euo pipefail
    for cmd in docker curl isd; do
      if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "ERROR: $cmd is required for just demo."
        exit 1
      fi
    done
    published_ports=$(docker ps --format '{{"{{"}}.Names{{"}}"}} {{"{{"}}.Ports{{"}}"}}')
    for port in 80 443 {{grpc_port}} {{http_port}}; do
      if [[ "$published_ports" == *":$port->"* ]]; then
        echo "ERROR: port $port is already published by a container on this Docker context."
        echo "$published_ports"
        exit 1
      fi
    done

# Build both images locally from the working-tree Dockerfile (use when iterating
# on uncommitted changes; no GHA round-trip)
smoke-build: (_disk-preflight min_free_gb)
    @echo "→ building isengard-controller:local"
    docker build --platform linux/amd64 --target controller -t isengard-controller:local .
    @echo "→ building isengard-agent:local"
    docker build --platform linux/amd64 --target agent -t isengard-agent:local .

# Internal: bring up controller + agent given image refs
[private]
_smoke-up ctrl_img agent_img:
    #!/usr/bin/env bash
    set -euo pipefail
    cleanup_on_error() {
      status=$?
      if [ "$status" -ne 0 ]; then
        echo "ERROR: smoke startup failed; cleaning partial containers."
        docker rm -f {{ctrl}} {{agent}} >/dev/null 2>&1 || true
      fi
      exit "$status"
    }
    trap cleanup_on_error EXIT
    DOCKER_SOCK="${DOCKER_SOCK:-/var/run/docker.sock}"
    docker network create isengard-proxy >/dev/null 2>&1 || true
    echo "→ starting controller ({{ctrl_img}})"
    docker run -d --name {{ctrl}} --restart=always \
      --platform linux/amd64 \
      --label io.isengard.role=controller \
      --label io.isengard.api.version=1 \
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
    TOKEN=$(docker exec {{ctrl}} isengard controller token mint --role agent --ttl 15m --format token | tr -d '[:space:]')
    echo "→ starting agent ({{agent_img}})"
    docker run -d --name {{agent}} --restart=always \
      --platform linux/amd64 \
      --network isengard-proxy \
      --add-host controller.local:host-gateway \
      --label io.isengard.role=agent \
      -p 127.0.0.1:80:8080 -p 127.0.0.1:443:8443 \
      -v "$DOCKER_SOCK":/var/run/docker.sock \
      -v {{agent}}-data:/var/lib/isengard \
      -v {{ca_pem}}:/etc/isengard/ca.pem:ro \
      -e ISENGARD_CONTROLLER=https://controller.local:{{grpc_port}} \
      -e ISENGARD_ENROLL_TOKEN="$TOKEN" \
      -e ISENGARD_CONTROLLER_CA_PEM_PATH=/etc/isengard/ca.pem \
      "{{agent_img}}" >/dev/null
    echo "→ waiting for agent enrollment"
    for i in $(seq 1 30); do
      docker exec {{ctrl}} isengard controller agent list 2>/dev/null | grep -q . && { echo "✓ agent enrolled"; break; }
      sleep 1
      [ "$i" = "30" ] && { echo "agent didn't enroll; logs:"; docker logs {{agent}}; exit 1; }
    done
    trap - EXIT
    echo ""
    echo "✓ smoke ready"
    echo "  dashboard:    http://localhost:{{http_port}}/"
    echo "  controller:   docker logs -f {{ctrl}}"
    echo "  agent:        docker logs -f {{agent}}"
    echo "  list agents:  docker exec {{ctrl}} isengard controller agent list"
    echo "  revoke agent: docker exec {{ctrl}} isengard controller agent revoke <host-id>"
    echo "  teardown:     just smoke-clean"

# Smoke test using published :next images from GHCR (most common path)
smoke: smoke-clean smoke-pull (_smoke-up "ghcr.io/weavers-engineering/isengard-controller:next" "ghcr.io/weavers-engineering/isengard-agent:next")

# Smoke test using locally-built images (use when iterating on uncommitted code)
smoke-local: smoke-clean smoke-build (_smoke-up "isengard-controller:local" "isengard-agent:local")

# Run the local showcase: controller + agent + routed whoami stack.
demo: demo-clean _demo-preflight smoke-pull (_smoke-up "ghcr.io/weavers-engineering/isengard-controller:next" "ghcr.io/weavers-engineering/isengard-agent:next")
    #!/usr/bin/env bash
    set -euo pipefail
    echo "==> deploying showcase stack"
    isd stack deploy --yes --detach {{showcase_file}}
    echo "==> waiting for route {{showcase_host}}"
    for i in $(seq 1 60); do
      routes=$(isd route ls 2>/dev/null || true)
      if [[ "$routes" == *"{{showcase_host}}"* ]]; then
        echo "OK route registered"
        break
      fi
      sleep 1
      if [ "$i" = "60" ]; then
        echo "ERROR: route {{showcase_host}} did not appear."
        isd route ls || true
        docker logs {{agent}} || true
        exit 1
      fi
    done
    echo "==> waiting for local proxy response"
    for i in $(seq 1 60); do
      if curl -fsS -H 'Host: {{showcase_host}}' http://127.0.0.1/ >/dev/null; then
        echo "OK proxy responded"
        echo ""
        echo "Showcase demo ready:"
        echo "  dashboard: http://127.0.0.1:{{http_port}}/"
        echo "  route:     curl -H 'Host: {{showcase_host}}' http://127.0.0.1/"
        echo "  stacks:    isd stack ls"
        echo "  routes:    isd route ls"
        echo "  logs:      docker logs -f {{agent}}"
        echo "  cleanup:   just demo-clean"
        exit 0
      fi
      sleep 1
    done
    echo "ERROR: local proxy did not serve {{showcase_host}}."
    isd route ls || true
    docker logs {{agent}} || true
    echo "Retry manually: curl -H 'Host: {{showcase_host}}' http://127.0.0.1/"
    exit 1

# Remove the showcase stack and smoke control-plane state.
demo-clean: smoke-clean

# === Maintenance ===

# Show disk usage for Rust, Docker, and local generated outputs.
disk:
    #!/usr/bin/env bash
    set -euo pipefail
    target_dir=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
    echo "==> filesystem free space"
    df -h .
    df -h "$target_dir" 2>/dev/null || df -h "$(dirname "$target_dir")" 2>/dev/null || true
    echo ""
    echo "==> build output sizes"
    du -sh "$target_dir" .cache website/.nuxt website/.output 2>/dev/null || true
    echo ""
    echo "==> docker usage"
    docker system df 2>/dev/null || echo "docker unavailable"

# Prune stale Rust build artifacts without deleting the whole target dir.
rust-prune days="30":
    @if ! command -v cargo-sweep >/dev/null 2>&1; then \
        echo "ERROR: cargo-sweep is not installed."; \
        echo "Install via: cargo install cargo-sweep"; \
        exit 1; \
    fi
    cargo sweep --dry-run --time {{days}} .
    @echo ""
    @echo "Preview only. To actually prune: cargo sweep --time {{days}} ."

[private]
_disk-preflight min_gb:
    #!/usr/bin/env bash
    set -euo pipefail
    target=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
    check_path="$target"
    if [ ! -e "$check_path" ]; then
      check_path="$(dirname "$target")"
    fi
    free_kb=$(df -Pk "$check_path" | awk 'NR == 2 { print $4 }')
    min_kb=$(({{min_gb}} * 1024 * 1024))
    if [ "$free_kb" -lt "$min_kb" ]; then
      echo "ERROR: low disk space for Rust builds."
      echo "Need at least {{min_gb}} GiB free at $check_path."
      echo "Current free: $((free_kb / 1024 / 1024)) GiB."
      echo "Run: just disk"
      echo "Then: just rust-prune"
      exit 1
    fi

clean:
    cargo clean
    rm -rf website/.nuxt website/.output

# Pre-commit gate: fmt + lint + test + cargo-deny (mirrors CI exactly).
# cargo-deny is required: it catches advisories CI blocks on.
ci-local: (_disk-preflight min_free_gb) fmt-check lint test
    @if ! command -v cargo-deny >/dev/null 2>&1; then \
        echo "ERROR: cargo-deny is not installed. Run: cargo install cargo-deny"; \
        echo "       (or: just install-hooks: bootstraps it)"; \
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
