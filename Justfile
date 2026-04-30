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

# Pre-commit gate: fmt + lint + test
ci-local: fmt-check lint test
    @echo "✓ ci-local passed"

# Install lefthook git hooks (pre-push runs fmt-check + clippy + test + cargo-deny).
# After running this once, `git push` will run the gates locally.
install-hooks:
    @if ! command -v lefthook >/dev/null 2>&1; then \
        echo "lefthook not installed. install with: brew install lefthook  (or: cargo install lefthook)"; \
        exit 1; \
    fi
    lefthook install
    @echo "✓ pre-push hook installed"
