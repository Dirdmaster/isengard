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
