# Isengard Platform Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the Cargo workspace, tooling, plugin trait surface, and binary entry point so subsequent phases (gRPC, updater port, notifier, dashboard) have somewhere to land.

**Architecture:** Single repository, Cargo workspace at root. Single `isengard` binary with two subcommands (`agent`, `controller`) that dispatch to library crates. Plugins are Rust crates that register via the `inventory` crate at compile time and implement traits from `isengard-core`. Phase 0+1 ends with a no-op plugin loaded at startup by both modes — proving the host wiring works before any real feature lands.

**Tech Stack:** Rust 2024 edition (1.85+), tokio, async-trait, inventory (compile-time plugin registration), clap (CLI), serde, anyhow + thiserror, tracing, cargo-nextest, just, GitHub Actions.

**Branch:** `feat/platform-rewrite` (already created). Do NOT push without explicit approval — the repo is public.

**Spec:** `docs/superpowers/specs/2026-04-29-platform-pivot-design.md`

---

## Scope

This plan covers **Phase 0 (workspace skeleton + CI)** and **Phase 1 (plugin trait + host)** from the spec. It does not include:

- gRPC services (Phase 2 — separate plan)
- The `updater` plugin port (Phase 3 — separate plan)
- `notifier` (Phase 4), `dashboard` (Phase 5), prod soak (Phase 6), main-merge (Phase 7) — each gets its own plan when its predecessor lands

**Done when:**

1. `cargo build --workspace` succeeds, no warnings under `-D warnings`
2. `cargo nextest run --workspace` runs all tests green
3. `just build`, `just test`, `just lint`, `just fmt-check` all succeed
4. `cargo run -p isengard -- agent --help` and `cargo run -p isengard -- controller --help` print usage
5. Both modes load and instantiate a no-op plugin compiled into the binary (validated via integration test)
6. CI green on branch push (when eventually pushed)
7. ~10–12 commits, each green on its own

---

## File Structure

Files this plan creates or modifies. Each file has one clear responsibility.

```
isengard/
├── Cargo.toml                                    # CREATE: [workspace] root, shared deps
├── Cargo.lock                                    # auto, do not hand-edit
├── Justfile                                      # CREATE: cross-stack task runner
├── deny.toml                                     # CREATE: cargo-deny config
├── rustfmt.toml                                  # CREATE: formatter config
├── clippy.toml                                   # CREATE: lint config
├── rust-toolchain.toml                           # CREATE: pin toolchain
├── .gitignore                                    # MODIFY: add /target, /.idea, etc
├── README.md                                     # MODIFY: add platform-pivot banner
├── .github/workflows/ci.yml                      # CREATE: build, test, clippy, fmt
├── crates/
│   ├── isengard/
│   │   ├── Cargo.toml                            # CREATE: binary crate manifest
│   │   └── src/main.rs                           # CREATE: clap subcommand entry
│   ├── isengard-core/
│   │   ├── Cargo.toml                            # CREATE: lib crate manifest
│   │   └── src/
│   │       ├── lib.rs                            # CREATE: re-exports
│   │       ├── plugin.rs                         # CREATE: Plugin trait + capability sub-traits
│   │       ├── registration.rs                   # CREATE: PluginRegistration + inventory glue
│   │       ├── context.rs                        # CREATE: PluginContext (host services)
│   │       ├── event.rs                          # CREATE: Event/EventKind stub types
│   │       └── error.rs                          # CREATE: CoreError enum (thiserror)
│   ├── isengard-controller/
│   │   ├── Cargo.toml                            # CREATE: lib crate manifest
│   │   └── src/lib.rs                            # CREATE: run_controller(opts) entry
│   ├── isengard-agent/
│   │   ├── Cargo.toml                            # CREATE: lib crate manifest
│   │   └── src/lib.rs                            # CREATE: run_agent(opts) entry
│   ├── isengard-proto/
│   │   ├── Cargo.toml                            # CREATE: lib crate (placeholder for Phase 2)
│   │   ├── build.rs                              # CREATE: tonic-build invoker (no .protos yet)
│   │   └── src/lib.rs                            # CREATE: placeholder
│   └── isengard-plugins/
│       ├── updater/
│       │   ├── Cargo.toml                        # CREATE: placeholder for Phase 3
│       │   └── src/lib.rs                        # CREATE: placeholder
│       ├── dashboard/
│       │   ├── Cargo.toml                        # CREATE: placeholder for Phase 5
│       │   └── src/lib.rs                        # CREATE: placeholder
│       └── notifier/
│           ├── Cargo.toml                        # CREATE: placeholder for Phase 4
│           └── src/lib.rs                        # CREATE: placeholder
└── crates/isengard/tests/
    └── plugin_loading.rs                         # CREATE: integration test for plugin host
```

---

## Phase 0 · Workspace skeleton + tooling

### Task 1: Stash existing Go layout out of the way

**Files:**
- Modify (move): `main.go`, `go.mod`, `go.sum`, `Dockerfile`, `docker-compose.yml`, `internal/`, `Makefile`, `.golangci.yml`, `lefthook.yml`, `package.json`, `scripts/`
- Keep at root: `README.md`, `LICENSE`, `.github/`, `www/`, `.gitignore`

The Go code stays accessible until v1 ships (per the migration approach in the spec) but must not interfere with the new Cargo workspace. Move it to `legacy-go/`.

- [ ] **Step 1: Verify branch and clean state**

```bash
cd ~/Projects/isengard
git branch --show-current
git status
```

Expected: on `feat/platform-rewrite`, working tree clean (only the spec already committed).

- [ ] **Step 2: Move Go code**

```bash
mkdir -p legacy-go
git mv main.go go.mod go.sum Dockerfile docker-compose.yml internal Makefile .golangci.yml lefthook.yml package.json scripts legacy-go/
```

Note: `www/` (Nuxt landing site) stays at the root — it's the marketing site and remains active. `bin/` artifacts and `data.db` if any are gitignored already.

- [ ] **Step 3: Add a one-line README at the legacy directory**

```bash
cat > legacy-go/README.md <<'EOF'
# legacy-go

Original Go implementation of Isengard (Watchtower replacement). Kept here on the
`feat/platform-rewrite` branch as a reference during the Rust rewrite. Will be
removed in the merge commit when v1 ships. See `../docs/superpowers/specs/`.
EOF
```

- [ ] **Step 4: Commit**

```bash
git add legacy-go README.md 2>/dev/null || true
git add -A
git commit -m "chore: move legacy Go layout to legacy-go/ ahead of rust rewrite"
```

Verify:
```bash
ls
```
Expected at root: `LICENSE  README.md  docs  legacy-go  www  .git  .github  .gitignore`

---

### Task 2: Pin Rust toolchain

**Files:**
- Create: `rust-toolchain.toml`

- [ ] **Step 1: Write toolchain pin**

```bash
cat > rust-toolchain.toml <<'EOF'
[toolchain]
channel = "1.85"
components = ["rustfmt", "clippy", "rust-src"]
profile = "minimal"
EOF
```

- [ ] **Step 2: Verify toolchain installs**

```bash
rustup show
```

Expected: stable 1.85+ active, rustfmt + clippy components present. If a different version is shown, run `rustup update stable` first. Edition 2024 requires Rust 1.85+.

- [ ] **Step 3: Commit**

```bash
git add rust-toolchain.toml
git commit -m "chore: pin rust 1.85 with rustfmt + clippy + rust-src"
```

---

### Task 3: Initialize Cargo workspace root

**Files:**
- Create: `Cargo.toml`

- [ ] **Step 1: Write workspace manifest**

```bash
cat > Cargo.toml <<'EOF'
[workspace]
resolver = "2"
members = [
    "crates/isengard",
    "crates/isengard-core",
    "crates/isengard-controller",
    "crates/isengard-agent",
    "crates/isengard-proto",
    "crates/isengard-plugins/updater",
    "crates/isengard-plugins/dashboard",
    "crates/isengard-plugins/notifier",
]

[workspace.package]
version = "0.1.0-alpha"
edition = "2024"
rust-version = "1.85"
license = "MIT"
authors = ["Dirdmaster"]
repository = "https://github.com/Dirdmaster/isengard"
homepage = "https://isengard.app"

[workspace.dependencies]
# async runtime
tokio = { version = "1.42", features = ["full"] }
async-trait = "0.1.83"

# plugin registration
inventory = "0.3.15"

# serialization
serde = { version = "1.0.215", features = ["derive"] }
serde_json = "1.0.133"

# errors
anyhow = "1.0.94"
thiserror = "2.0.6"

# CLI
clap = { version = "4.5.23", features = ["derive", "env"] }

# logging
tracing = "0.1.41"
tracing-subscriber = { version = "0.3.19", features = ["env-filter", "fmt"] }

# internal
isengard-core = { path = "crates/isengard-core" }
isengard-controller = { path = "crates/isengard-controller" }
isengard-agent = { path = "crates/isengard-agent" }
isengard-proto = { path = "crates/isengard-proto" }

[profile.release]
lto = "thin"
codegen-units = 1
strip = true

[profile.dev]
opt-level = 0
debug = true
EOF
```

- [ ] **Step 2: Commit before adding member crates so the workspace is empty-but-valid**

Note: this manifest references members that do not yet exist; `cargo build` will fail until Task 4. That is expected — we'll commit them together.

```bash
git add Cargo.toml
```

(Defer commit until Task 4, where the empty member crates exist.)

---

### Task 4: Create empty member crate skeletons

**Files:**
- Create: `crates/isengard/Cargo.toml`, `crates/isengard/src/main.rs`
- Create: `crates/isengard-core/Cargo.toml`, `crates/isengard-core/src/lib.rs`
- Create: `crates/isengard-controller/Cargo.toml`, `crates/isengard-controller/src/lib.rs`
- Create: `crates/isengard-agent/Cargo.toml`, `crates/isengard-agent/src/lib.rs`
- Create: `crates/isengard-proto/Cargo.toml`, `crates/isengard-proto/src/lib.rs`
- Create: `crates/isengard-plugins/updater/Cargo.toml`, `crates/isengard-plugins/updater/src/lib.rs`
- Create: `crates/isengard-plugins/dashboard/Cargo.toml`, `crates/isengard-plugins/dashboard/src/lib.rs`
- Create: `crates/isengard-plugins/notifier/Cargo.toml`, `crates/isengard-plugins/notifier/src/lib.rs`

- [ ] **Step 1: Create `isengard` binary crate**

```bash
mkdir -p crates/isengard/src
cat > crates/isengard/Cargo.toml <<'EOF'
[package]
name = "isengard"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Isengard container management platform — single binary, controller and agent modes"

[[bin]]
name = "isengard"
path = "src/main.rs"

[dependencies]
EOF
cat > crates/isengard/src/main.rs <<'EOF'
fn main() {
    println!("isengard: not yet wired up — see Task 9");
}
EOF
```

- [ ] **Step 2: Create the five library crates with placeholder `lib.rs`**

Run all of these in sequence:

```bash
# isengard-core
mkdir -p crates/isengard-core/src
cat > crates/isengard-core/Cargo.toml <<'EOF'
[package]
name = "isengard-core"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Isengard core: plugin trait, host services, event types"

[dependencies]
EOF
cat > crates/isengard-core/src/lib.rs <<'EOF'
//! Isengard core types: plugin trait, host services, event journal types.
//! Populated in Task 5 onwards.
EOF

# isengard-controller
mkdir -p crates/isengard-controller/src
cat > crates/isengard-controller/Cargo.toml <<'EOF'
[package]
name = "isengard-controller"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Controller-mode runtime"

[dependencies]
EOF
cat > crates/isengard-controller/src/lib.rs <<'EOF'
//! Isengard controller-mode runtime. Populated in Task 7.
EOF

# isengard-agent
mkdir -p crates/isengard-agent/src
cat > crates/isengard-agent/Cargo.toml <<'EOF'
[package]
name = "isengard-agent"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Agent-mode runtime"

[dependencies]
EOF
cat > crates/isengard-agent/src/lib.rs <<'EOF'
//! Isengard agent-mode runtime. Populated in Task 8.
EOF

# isengard-proto
mkdir -p crates/isengard-proto/src
cat > crates/isengard-proto/Cargo.toml <<'EOF'
[package]
name = "isengard-proto"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "gRPC service definitions and generated code (populated in Phase 2)"

[dependencies]
EOF
cat > crates/isengard-proto/src/lib.rs <<'EOF'
//! gRPC service definitions and tonic-generated code. Populated in Phase 2.
EOF

# isengard-plugins/{updater,dashboard,notifier}
for plugin in updater dashboard notifier; do
  mkdir -p crates/isengard-plugins/$plugin/src
  cat > crates/isengard-plugins/$plugin/Cargo.toml <<EOF
[package]
name = "isengard-plugin-$plugin"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Isengard $plugin plugin (populated in a later phase)"

[dependencies]
EOF
  cat > crates/isengard-plugins/$plugin/src/lib.rs <<EOF
//! Isengard \`$plugin\` plugin. Populated in a later phase.
EOF
done
```

- [ ] **Step 3: Verify workspace builds**

```bash
cargo build --workspace
```

Expected: clean build, eight crates compiled. No warnings beyond the lib.rs being mostly empty.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock crates/
git commit -m "chore: cargo workspace skeleton — 8 member crates, edition 2024"
```

---

### Task 5: Add formatter, lint, deny, gitignore configs

**Files:**
- Create: `rustfmt.toml`, `clippy.toml`, `deny.toml`
- Modify: `.gitignore`

- [ ] **Step 1: rustfmt config**

```bash
cat > rustfmt.toml <<'EOF'
edition = "2024"
max_width = 100
use_field_init_shorthand = true
use_try_shorthand = true
EOF
```

- [ ] **Step 2: clippy config**

```bash
cat > clippy.toml <<'EOF'
# Allow up to 7 function arguments before warning (default is 7; explicit for visibility)
too-many-arguments-threshold = 8
# Allow Vec<u8> for byte buffers without warning
allow-expect-in-tests = true
EOF
```

- [ ] **Step 3: cargo-deny config**

```bash
cat > deny.toml <<'EOF'
[graph]
all-features = true

[advisories]
db-path = "~/.cargo/advisory-db"
db-urls = ["https://github.com/rustsec/advisory-db"]
yanked = "deny"

[licenses]
unlicensed = "deny"
allow = [
    "MIT",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Unicode-DFS-2016",
    "Unicode-3.0",
    "Zlib",
    "MPL-2.0",
    "CC0-1.0",
]
confidence-threshold = 0.93

[bans]
multiple-versions = "warn"
wildcards = "deny"

[sources]
unknown-registry = "deny"
unknown-git = "deny"
allow-registry = ["https://github.com/rust-lang/crates.io-index"]
EOF
```

- [ ] **Step 4: extend .gitignore**

```bash
cat >> .gitignore <<'EOF'

# Rust
/target/
**/*.rs.bk

# IDE
/.idea/
/.vscode/
*.iml

# Local env
.env
.env.local
EOF
```

- [ ] **Step 5: Verify configs are valid**

```bash
cargo fmt --check    # should print nothing (no Rust files to format yet) and exit 0
cargo clippy --workspace -- -D warnings    # should pass
```

If `cargo-deny` is installed, run `cargo deny check`; otherwise note that it'll run in CI.

- [ ] **Step 6: Commit**

```bash
git add rustfmt.toml clippy.toml deny.toml .gitignore
git commit -m "chore: rustfmt, clippy, cargo-deny, gitignore configs"
```

---

### Task 6: Justfile (cross-stack task runner)

**Files:**
- Create: `Justfile`

- [ ] **Step 1: Write Justfile**

```bash
cat > Justfile <<'EOF'
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
EOF
```

- [ ] **Step 2: Verify Justfile parses and lists tasks**

```bash
just --list
```

Expected: lists `agent`, `build`, `clean`, `controller`, `fmt`, `fmt-check`, `lint`, `release`, `test`, `www`, `www-build`, `ci-local`.

If `just` is not installed, instruct: `brew install just` (macOS) or `cargo install just` (cross-platform).

- [ ] **Step 3: Run the local CI gate**

```bash
just ci-local
```

Expected: fmt-check passes (nothing to format yet), lint passes, test passes (no tests yet but cargo runs cleanly).

- [ ] **Step 4: Commit**

```bash
git add Justfile
git commit -m "chore: justfile with build/test/lint/fmt/dev/www/ci-local tasks"
```

---

### Task 7: GitHub Actions CI

**Files:**
- Create: `.github/workflows/ci.yml`

This replaces the existing Go CI workflow. The Go workflow stays in `.github/workflows/` only if it still references the legacy code; otherwise it's removed in a follow-up. For now, just add the Rust workflow.

- [ ] **Step 1: List existing workflows**

```bash
ls .github/workflows/
```

Note any existing workflows. The Go workflow file (likely `ci.yml` or `go.yml`) needs to either be replaced or renamed. If a `ci.yml` already exists, rename it first:

```bash
if [ -f .github/workflows/ci.yml ]; then
  git mv .github/workflows/ci.yml .github/workflows/ci-go-legacy.yml
fi
```

- [ ] **Step 2: Write the new Rust CI workflow**

```bash
cat > .github/workflows/ci.yml <<'EOF'
name: CI

on:
  push:
    branches: [main, "feat/**"]
  pull_request:

env:
  CARGO_TERM_COLOR: always
  RUSTFLAGS: "-D warnings"

jobs:
  build-test:
    name: build + test
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - name: Cache cargo registry + target
        uses: Swatinem/rust-cache@v2
        with:
          shared-key: build

      - name: Install cargo-nextest
        uses: taiki-e/install-action@v2
        with:
          tool: cargo-nextest

      - name: cargo fmt --check
        run: cargo fmt --check

      - name: cargo clippy
        run: cargo clippy --workspace --all-targets -- -D warnings

      - name: cargo build
        run: cargo build --workspace

      - name: cargo nextest
        run: cargo nextest run --workspace

  deny:
    name: cargo-deny
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2
EOF
```

- [ ] **Step 3: Verify YAML is well-formed**

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))"
```

Expected: no output (success).

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/
git commit -m "ci: rust workflow — fmt-check, clippy, build, nextest, cargo-deny"
```

---

### Task 8: README banner

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Read the current README**

```bash
head -20 README.md
```

- [ ] **Step 2: Prepend a platform-pivot banner**

Replace the current README opening with this. Keep all existing content below the banner — that's still accurate for the legacy Go binary while the rewrite is in flight.

```bash
# Use a temp file approach to prepend
cat > /tmp/banner.md <<'EOF'
# Isengard

> **Note (2026-04-29):** Isengard is being rewritten from the ground up as a container management platform — single binary with controller and agent modes, plugin model, multi-host support, web dashboard. The Rust rewrite is in progress on the `feat/platform-rewrite` branch. The Go implementation below remains the current stable release; it stays in [`legacy-go/`](./legacy-go/) on the rewrite branch as a reference. See [`docs/superpowers/specs/2026-04-29-platform-pivot-design.md`](./docs/superpowers/specs/2026-04-29-platform-pivot-design.md) for the design.

---

EOF
cat /tmp/banner.md README.md > /tmp/readme.md && mv /tmp/readme.md README.md
rm /tmp/banner.md
```

- [ ] **Step 3: Verify the README has the banner at the top and the original content still follows**

```bash
head -10 README.md
```

Expected: banner visible, "Lightweight Docker container auto-updater" line still present a few lines down.

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs(readme): banner pointing to platform pivot design"
```

---

## Phase 1 · Plugin trait + host

### Task 9: `isengard-core` — error type

**Files:**
- Create: `crates/isengard-core/src/error.rs`
- Modify: `crates/isengard-core/src/lib.rs`
- Modify: `crates/isengard-core/Cargo.toml`

- [ ] **Step 1: Add `thiserror` dependency to `isengard-core`**

```bash
cat > crates/isengard-core/Cargo.toml <<'EOF'
[package]
name = "isengard-core"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Isengard core: plugin trait, host services, event types"

[dependencies]
async-trait.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
inventory.workspace = true
EOF
```

- [ ] **Step 2: Write the error type**

```bash
cat > crates/isengard-core/src/error.rs <<'EOF'
//! Errors emitted by the plugin host.

use thiserror::Error;

/// Errors at the host/plugin boundary. Plugins may also return their own error
/// types from operations not surfaced through this enum.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("plugin {name}: invalid config: {reason}")]
    InvalidConfig { name: String, reason: String },

    #[error("plugin {name}: init failed: {source}")]
    InitFailed {
        name: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("plugin {name}: start failed: {source}")]
    StartFailed {
        name: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("plugin {name}: stop failed: {source}")]
    StopFailed {
        name: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("plugin {name}: panicked")]
    Panicked { name: String },

    #[error("no plugin registered with name {name}")]
    UnknownPlugin { name: String },
}

/// Convenience alias used throughout the host code.
pub type Result<T, E = CoreError> = std::result::Result<T, E>;
EOF
```

Note: this references `anyhow::Error` as a source. Add `anyhow` to the deps:

```bash
# Append anyhow to isengard-core Cargo.toml
sed -i.bak 's|^\[dependencies\]$|[dependencies]\nanyhow.workspace = true|' crates/isengard-core/Cargo.toml
rm crates/isengard-core/Cargo.toml.bak
```

- [ ] **Step 3: Write the failing test**

```bash
mkdir -p crates/isengard-core/src
# Tests live alongside code in #[cfg(test)] modules; for this one, inline in error.rs
cat >> crates/isengard-core/src/error.rs <<'EOF'

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_invalid_config() {
        let err = CoreError::InvalidConfig {
            name: "updater".into(),
            reason: "missing 'interval' key".into(),
        };
        assert_eq!(
            err.to_string(),
            "plugin updater: invalid config: missing 'interval' key"
        );
    }

    #[test]
    fn display_unknown_plugin() {
        let err = CoreError::UnknownPlugin { name: "ghost".into() };
        assert_eq!(err.to_string(), "no plugin registered with name ghost");
    }
}
EOF
```

- [ ] **Step 4: Wire error.rs into lib.rs**

```bash
cat > crates/isengard-core/src/lib.rs <<'EOF'
//! Isengard core types: plugin trait, host services, event journal types.

pub mod error;

pub use error::{CoreError, Result};
EOF
```

- [ ] **Step 5: Run the tests**

```bash
cargo nextest run -p isengard-core
```

Expected: 2 tests pass (`display_invalid_config`, `display_unknown_plugin`). Build succeeds.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-core/
git commit -m "feat(core): CoreError enum + Result alias"
```

---

### Task 10: `isengard-core` — Event types (stubs)

**Files:**
- Create: `crates/isengard-core/src/event.rs`
- Modify: `crates/isengard-core/src/lib.rs`

These are minimal types so plugins compile against them. Full event-stream wiring lands in Phase 4 (notifier + journal).

- [ ] **Step 1: Write the failing test**

```bash
cat > crates/isengard-core/src/event.rs <<'EOF'
//! Event types emitted by plugins and consumed by the journal + subscribers.
//!
//! Phase 1 contains only the minimal shape used by the plugin trait. The
//! journal/subscriber wiring lands in Phase 4.

use serde::{Deserialize, Serialize};

/// Stable identifier for the kind of event. Used by `EventSubscriber` to filter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    UpdateChecked,
    UpdateSuccess,
    UpdateFailed,
    UpdateSkipped,
    AgentConnect,
    AgentDisconnect,
    PluginCrashed,
}

/// A journal event. The payload is plugin-defined JSON; concrete schemas are
/// owned by each plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub kind: EventKind,
    pub ts_millis: u64,
    pub host_id: Option<String>,
    pub container_id: Option<String>,
    pub plugin: Option<String>,
    pub payload: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kind_round_trips_through_json() {
        let kind = EventKind::UpdateSuccess;
        let s = serde_json::to_string(&kind).unwrap();
        assert_eq!(s, "\"update_success\"");
        let back: EventKind = serde_json::from_str(&s).unwrap();
        assert_eq!(back, EventKind::UpdateSuccess);
    }

    #[test]
    fn event_serialises_with_optionals() {
        let evt = Event {
            kind: EventKind::AgentConnect,
            ts_millis: 1_700_000_000_000,
            host_id: Some("01J...".into()),
            container_id: None,
            plugin: Some("agent".into()),
            payload: serde_json::json!({"version": "0.1.0-alpha"}),
        };
        let s = serde_json::to_string(&evt).unwrap();
        assert!(s.contains("\"kind\":\"agent_connect\""));
        assert!(s.contains("\"host_id\":\"01J...\""));
        assert!(s.contains("\"container_id\":null"));
    }
}
EOF
```

- [ ] **Step 2: Re-export from lib.rs**

```bash
cat > crates/isengard-core/src/lib.rs <<'EOF'
//! Isengard core types: plugin trait, host services, event journal types.

pub mod error;
pub mod event;

pub use error::{CoreError, Result};
pub use event::{Event, EventKind};
EOF
```

- [ ] **Step 3: Run the tests**

```bash
cargo nextest run -p isengard-core
```

Expected: 4 tests pass (2 from error, 2 from event). Build succeeds.

- [ ] **Step 4: Commit**

```bash
git add crates/isengard-core/src/event.rs crates/isengard-core/src/lib.rs
git commit -m "feat(core): event + event_kind stub types with serde"
```

---

### Task 11: `isengard-core` — `PluginContext`

**Files:**
- Create: `crates/isengard-core/src/context.rs`
- Modify: `crates/isengard-core/src/lib.rs`

`PluginContext` is what the host passes into a plugin's lifecycle hooks. In Phase 1 it's intentionally minimal — just enough to identify which mode the host is running in and read configuration. Real services (logger, journal writer, gRPC handles) get added when their underlying systems exist (Phase 2 onward).

- [ ] **Step 1: Write the failing test**

```bash
cat > crates/isengard-core/src/context.rs <<'EOF'
//! `PluginContext`: services the host exposes to plugins via their lifecycle hooks.
//!
//! Phase 1 minimum: host mode + plugin's slice of the merged config. Subsequent
//! phases will add: logger handle, journal writer, gRPC clients, secret store.

use serde::{Deserialize, Serialize};

/// Which mode the host is running in. Affects which capability sub-traits a
/// plugin's lifecycle hooks are called through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostMode {
    Controller,
    Agent,
}

/// Context handed to a plugin during `init`/`start`. Cheap to clone (Arc-backed
/// fields will land in later phases).
#[derive(Debug, Clone)]
pub struct PluginContext {
    pub mode: HostMode,
    /// The plugin's slice of the merged configuration tree. Empty `Value::Null`
    /// when the plugin has no configuration.
    pub config: serde_json::Value,
}

impl PluginContext {
    pub fn new(mode: HostMode, config: serde_json::Value) -> Self {
        Self { mode, config }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_mode_serialises_lowercase() {
        let s = serde_json::to_string(&HostMode::Controller).unwrap();
        assert_eq!(s, "\"controller\"");
        let s = serde_json::to_string(&HostMode::Agent).unwrap();
        assert_eq!(s, "\"agent\"");
    }

    #[test]
    fn plugin_context_constructs_with_null_config() {
        let ctx = PluginContext::new(HostMode::Agent, serde_json::Value::Null);
        assert_eq!(ctx.mode, HostMode::Agent);
        assert!(ctx.config.is_null());
    }

    #[test]
    fn plugin_context_carries_arbitrary_config() {
        let cfg = serde_json::json!({"interval": "30m", "watch_all": true});
        let ctx = PluginContext::new(HostMode::Controller, cfg.clone());
        assert_eq!(ctx.config["interval"], "30m");
    }
}
EOF
```

- [ ] **Step 2: Re-export from lib.rs**

```bash
cat > crates/isengard-core/src/lib.rs <<'EOF'
//! Isengard core types: plugin trait, host services, event journal types.

pub mod context;
pub mod error;
pub mod event;

pub use context::{HostMode, PluginContext};
pub use error::{CoreError, Result};
pub use event::{Event, EventKind};
EOF
```

- [ ] **Step 3: Run the tests**

```bash
cargo nextest run -p isengard-core
```

Expected: 7 tests pass (2 error + 2 event + 3 context).

- [ ] **Step 4: Commit**

```bash
git add crates/isengard-core/src/context.rs crates/isengard-core/src/lib.rs
git commit -m "feat(core): PluginContext + HostMode"
```

---

### Task 12: `isengard-core` — `Plugin` trait + capabilities

**Files:**
- Create: `crates/isengard-core/src/plugin.rs`
- Modify: `crates/isengard-core/src/lib.rs`

This task introduces the trait surface plugins implement. It includes a dummy `NoopPlugin` *inside the test module* that exercises the trait shape and is reused in later registration tests.

- [ ] **Step 1: Write the trait surface and test**

```bash
cat > crates/isengard-core/src/plugin.rs <<'EOF'
//! Plugin trait surface.
//!
//! Every plugin implements [`Plugin`] (lifecycle). It then opts into one or more
//! capability sub-traits to declare what it does and where it runs:
//!
//! - [`AgentPlugin`] — runs on agents, runs work cycles
//! - [`ControllerPlugin`] — marker; runs on the controller (no extra methods yet)
//! - [`EventSubscriber`] — reacts to journal events on the controller
//! - [`HttpHandler`] — mounts HTTP routes on the controller's axum router (Phase 5)
//!
//! Inputs and outputs that cross the plugin boundary use serde-clean types so
//! the same trait works when plugins are loaded out-of-process (Phase 2+) or
//! sandboxed via WASM (later).

use crate::context::PluginContext;
use crate::error::Result;
use crate::event::{Event, EventKind};
use async_trait::async_trait;

/// Lifecycle. Every plugin implements this.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// Stable plugin identifier (e.g. "updater", "notifier"). Used for config
    /// namespacing and journal events. Must match the directory name under
    /// `crates/isengard-plugins/`.
    fn name(&self) -> &'static str;

    /// Semantic version of this plugin.
    fn version(&self) -> &'static str;

    /// Called once at host startup. The plugin reads its slice of the config,
    /// returns `Err` if it's invalid. The host aborts startup on `Err`.
    async fn init(&mut self, ctx: &PluginContext) -> Result<()>;

    /// Called after `init` succeeds. Plugin spawns its tasks here.
    async fn start(&mut self, ctx: &PluginContext) -> Result<()>;

    /// Called on graceful shutdown.
    async fn stop(&mut self) -> Result<()>;
}

/// Capability: this plugin runs on agents and exposes a work cycle the agent
/// triggers (e.g. the updater plugin's check-and-recreate loop).
#[async_trait]
pub trait AgentPlugin: Plugin {
    async fn run_cycle(&self, ctx: &PluginContext) -> Result<()>;
}

/// Capability: this plugin runs on the controller. Marker trait — concrete
/// behavior is provided by [`EventSubscriber`] and [`HttpHandler`].
pub trait ControllerPlugin: Plugin {}

/// Capability: this plugin reacts to journal events on the controller.
#[async_trait]
pub trait EventSubscriber: Plugin {
    /// Filter — which event kinds this subscriber wants. Empty = all.
    fn subscribed_events(&self) -> &[EventKind];

    /// Called on every matching event. Errors are logged; they do not stop
    /// the journal.
    async fn handle(&self, event: &Event, ctx: &PluginContext) -> Result<()>;
}

/// Capability: this plugin mounts HTTP routes onto the controller's axum
/// router. Real signature lands in Phase 5 once axum is added; for Phase 1 it
/// is a marker only.
pub trait HttpHandler: Plugin {}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::context::HostMode;

    /// Reusable no-op plugin for testing the trait surface and registration.
    pub struct NoopPlugin;

    #[async_trait]
    impl Plugin for NoopPlugin {
        fn name(&self) -> &'static str { "noop" }
        fn version(&self) -> &'static str { "0.0.0" }
        async fn init(&mut self, _ctx: &PluginContext) -> Result<()> { Ok(()) }
        async fn start(&mut self, _ctx: &PluginContext) -> Result<()> { Ok(()) }
        async fn stop(&mut self) -> Result<()> { Ok(()) }
    }

    #[async_trait]
    impl AgentPlugin for NoopPlugin {
        async fn run_cycle(&self, _ctx: &PluginContext) -> Result<()> { Ok(()) }
    }

    impl ControllerPlugin for NoopPlugin {}

    #[tokio::test]
    async fn noop_lifecycle_runs_clean() {
        let mut p = NoopPlugin;
        let ctx = PluginContext::new(HostMode::Agent, serde_json::Value::Null);
        assert_eq!(p.name(), "noop");
        assert_eq!(p.version(), "0.0.0");
        p.init(&ctx).await.unwrap();
        p.start(&ctx).await.unwrap();
        p.run_cycle(&ctx).await.unwrap();
        p.stop().await.unwrap();
    }

    #[tokio::test]
    async fn noop_carries_correct_mode_in_context() {
        let p = NoopPlugin;
        let ctx = PluginContext::new(HostMode::Controller, serde_json::json!({"k": "v"}));
        p.run_cycle(&ctx).await.unwrap();
        assert_eq!(ctx.mode, HostMode::Controller);
    }
}
EOF
```

- [ ] **Step 2: Add `tokio` (test-only) to `isengard-core`**

```bash
# isengard-core needs tokio for #[tokio::test], dev-only.
cat > crates/isengard-core/Cargo.toml <<'EOF'
[package]
name = "isengard-core"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Isengard core: plugin trait, host services, event types"

[dependencies]
anyhow.workspace = true
async-trait.workspace = true
inventory.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true

[dev-dependencies]
tokio = { workspace = true }
EOF
```

- [ ] **Step 3: Re-export from lib.rs**

```bash
cat > crates/isengard-core/src/lib.rs <<'EOF'
//! Isengard core types: plugin trait, host services, event journal types.

pub mod context;
pub mod error;
pub mod event;
pub mod plugin;

pub use context::{HostMode, PluginContext};
pub use error::{CoreError, Result};
pub use event::{Event, EventKind};
pub use plugin::{AgentPlugin, ControllerPlugin, EventSubscriber, HttpHandler, Plugin};
EOF
```

- [ ] **Step 4: Run the tests**

```bash
cargo nextest run -p isengard-core
```

Expected: 9 tests pass (2 error + 2 event + 3 context + 2 plugin).

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-core/
git commit -m "feat(core): Plugin trait + AgentPlugin + ControllerPlugin + EventSubscriber + HttpHandler"
```

---

### Task 13: `isengard-core` — registration via `inventory`

**Files:**
- Create: `crates/isengard-core/src/registration.rs`
- Modify: `crates/isengard-core/src/lib.rs`

The `inventory` crate provides compile-time plugin registration: each plugin crate calls `inventory::submit!` at module scope, the host calls `inventory::iter::<PluginRegistration>()` to enumerate them.

- [ ] **Step 1: Write registration module + test**

```bash
cat > crates/isengard-core/src/registration.rs <<'EOF'
//! Compile-time plugin registration via the [`inventory`] crate.
//!
//! Each plugin crate calls `inventory::submit!(PluginRegistration { ... })` at
//! module scope. The host enumerates them at startup by mode.

use crate::context::HostMode;
use crate::plugin::Plugin;

/// Capabilities a plugin advertises. Used by the host to skip plugins that
/// don't apply to its current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Agent,
    Controller,
}

/// Compile-time plugin registration entry. Plugin crates submit one of these
/// per plugin via [`inventory::submit!`].
pub struct PluginRegistration {
    pub name: &'static str,
    pub capabilities: &'static [Capability],
    /// Boxed-ed factory. Returns `Plugin` so callers don't need to know the
    /// concrete type. The host downcasts when calling capability sub-traits.
    pub constructor: fn() -> Box<dyn Plugin>,
}

inventory::collect!(PluginRegistration);

/// Enumerate every registered plugin that advertises a capability matching the
/// given mode.
pub fn registrations_for(mode: HostMode) -> Vec<&'static PluginRegistration> {
    let want = match mode {
        HostMode::Agent => Capability::Agent,
        HostMode::Controller => Capability::Controller,
    };
    inventory::iter::<PluginRegistration>()
        .into_iter()
        .filter(|r| r.capabilities.contains(&want))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::tests::NoopPlugin;
    use crate::plugin::Plugin;

    inventory::submit! {
        PluginRegistration {
            name: "noop",
            capabilities: &[Capability::Agent, Capability::Controller],
            constructor: || Box::new(NoopPlugin) as Box<dyn Plugin>,
        }
    }

    #[test]
    fn noop_is_visible_to_agent_mode() {
        let regs = registrations_for(HostMode::Agent);
        assert!(regs.iter().any(|r| r.name == "noop"));
    }

    #[test]
    fn noop_is_visible_to_controller_mode() {
        let regs = registrations_for(HostMode::Controller);
        assert!(regs.iter().any(|r| r.name == "noop"));
    }

    #[test]
    fn registration_constructor_yields_a_working_plugin() {
        let regs = registrations_for(HostMode::Agent);
        let noop = regs.iter().find(|r| r.name == "noop").unwrap();
        let plugin = (noop.constructor)();
        assert_eq!(plugin.name(), "noop");
        assert_eq!(plugin.version(), "0.0.0");
    }
}
EOF
```

- [ ] **Step 2: Re-export from lib.rs**

```bash
cat > crates/isengard-core/src/lib.rs <<'EOF'
//! Isengard core types: plugin trait, host services, event journal types,
//! compile-time plugin registration.

pub mod context;
pub mod error;
pub mod event;
pub mod plugin;
pub mod registration;

pub use context::{HostMode, PluginContext};
pub use error::{CoreError, Result};
pub use event::{Event, EventKind};
pub use plugin::{AgentPlugin, ControllerPlugin, EventSubscriber, HttpHandler, Plugin};
pub use registration::{registrations_for, Capability, PluginRegistration};
EOF
```

- [ ] **Step 3: Run the tests**

```bash
cargo nextest run -p isengard-core
```

Expected: 12 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/isengard-core/
git commit -m "feat(core): inventory-based plugin registration with mode filter"
```

---

### Task 14: `isengard-controller` — minimal runner

**Files:**
- Modify: `crates/isengard-controller/Cargo.toml`
- Modify: `crates/isengard-controller/src/lib.rs`

The controller runner discovers plugins matching `Capability::Controller`, calls their lifecycle hooks, and waits for shutdown. In Phase 1 there's no gRPC server, no inventory store — just the plugin lifecycle.

- [ ] **Step 1: Wire dependencies**

```bash
cat > crates/isengard-controller/Cargo.toml <<'EOF'
[package]
name = "isengard-controller"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Controller-mode runtime"

[dependencies]
anyhow.workspace = true
isengard-core.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio = { workspace = true }
tracing.workspace = true
EOF
```

- [ ] **Step 2: Write a failing test**

```bash
mkdir -p crates/isengard-controller/src
cat > crates/isengard-controller/src/lib.rs <<'EOF'
//! Controller-mode runtime: load plugins, run their lifecycle hooks, wait for
//! shutdown. Phase 1 minimum — no gRPC server, no inventory store.

use anyhow::Result;
use isengard_core::{registrations_for, HostMode, Plugin, PluginContext};
use tracing::{info, instrument};

/// Options for running the controller.
#[derive(Debug, Clone)]
pub struct ControllerOptions {
    /// Optional config tree (per-plugin slices keyed by plugin name).
    pub config: serde_json::Value,
}

impl Default for ControllerOptions {
    fn default() -> Self {
        Self { config: serde_json::Value::Object(Default::default()) }
    }
}

/// Discover and instantiate every plugin that advertises `Capability::Controller`.
pub fn load_plugins() -> Vec<Box<dyn Plugin>> {
    registrations_for(HostMode::Controller)
        .into_iter()
        .map(|r| (r.constructor)())
        .collect()
}

/// Run controller-mode lifecycle: init → start every plugin, then wait. Stop
/// every plugin on `ctx_token` cancellation. Phase 1 returns immediately
/// (no event loop yet) — subsequent phases hold the runner open on tokio
/// signal.
#[instrument(skip(opts))]
pub async fn run_controller(opts: ControllerOptions) -> Result<()> {
    info!("starting controller");
    let plugins = load_plugins();
    info!(plugin_count = plugins.len(), "plugins discovered");

    let mut started = Vec::with_capacity(plugins.len());

    for mut plugin in plugins {
        let plugin_name = plugin.name().to_string();
        let plugin_config = opts
            .config
            .get(&plugin_name)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let ctx = PluginContext::new(HostMode::Controller, plugin_config);
        plugin.init(&ctx).await?;
        plugin.start(&ctx).await?;
        info!(plugin = %plugin_name, "plugin started");
        started.push(plugin);
    }

    // Phase 1: stop everything immediately and return. Phase 2+ replaces this
    // with a tokio::signal::ctrl_c().await + per-plugin task supervision.
    for mut plugin in started {
        plugin.stop().await?;
    }

    info!("controller exited cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_controller_loads_zero_or_more_plugins_and_returns_ok() {
        // No plugins are registered against Capability::Controller in the
        // controller crate's own test cfg — that's fine, this asserts the
        // runner doesn't blow up on an empty plugin set or any plugin set.
        let res = run_controller(ControllerOptions::default()).await;
        assert!(res.is_ok(), "run_controller failed: {:?}", res);
    }
}
EOF
```

- [ ] **Step 3: Run the test**

```bash
cargo nextest run -p isengard-controller
```

Expected: 1 test passes (`run_controller_loads_zero_or_more_plugins_and_returns_ok`).

- [ ] **Step 4: Commit**

```bash
git add crates/isengard-controller/
git commit -m "feat(controller): minimal runner — load plugins, lifecycle, return"
```

---

### Task 15: `isengard-agent` — minimal runner

**Files:**
- Modify: `crates/isengard-agent/Cargo.toml`
- Modify: `crates/isengard-agent/src/lib.rs`

Mirror of the controller runner, filtered to `Capability::Agent`.

- [ ] **Step 1: Wire dependencies**

```bash
cat > crates/isengard-agent/Cargo.toml <<'EOF'
[package]
name = "isengard-agent"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Agent-mode runtime"

[dependencies]
anyhow.workspace = true
isengard-core.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio = { workspace = true }
tracing.workspace = true
EOF
```

- [ ] **Step 2: Write the runner + test**

```bash
mkdir -p crates/isengard-agent/src
cat > crates/isengard-agent/src/lib.rs <<'EOF'
//! Agent-mode runtime: load plugins, run their lifecycle hooks, wait for
//! shutdown. Phase 1 minimum — no gRPC client, no docker integration.

use anyhow::Result;
use isengard_core::{registrations_for, HostMode, Plugin, PluginContext};
use tracing::{info, instrument};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// URL of the controller (`https://host:port`). Unused in Phase 1; gRPC
    /// client lands in Phase 2.
    pub controller_url: Option<String>,
    pub config: serde_json::Value,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            controller_url: None,
            config: serde_json::Value::Object(Default::default()),
        }
    }
}

pub fn load_plugins() -> Vec<Box<dyn Plugin>> {
    registrations_for(HostMode::Agent)
        .into_iter()
        .map(|r| (r.constructor)())
        .collect()
}

#[instrument(skip(opts))]
pub async fn run_agent(opts: AgentOptions) -> Result<()> {
    info!(controller = ?opts.controller_url, "starting agent");
    let plugins = load_plugins();
    info!(plugin_count = plugins.len(), "plugins discovered");

    let mut started = Vec::with_capacity(plugins.len());

    for mut plugin in plugins {
        let plugin_name = plugin.name().to_string();
        let plugin_config = opts
            .config
            .get(&plugin_name)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let ctx = PluginContext::new(HostMode::Agent, plugin_config);
        plugin.init(&ctx).await?;
        plugin.start(&ctx).await?;
        info!(plugin = %plugin_name, "plugin started");
        started.push(plugin);
    }

    for mut plugin in started {
        plugin.stop().await?;
    }

    info!("agent exited cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_agent_returns_ok_with_default_options() {
        let res = run_agent(AgentOptions::default()).await;
        assert!(res.is_ok(), "run_agent failed: {:?}", res);
    }
}
EOF
```

- [ ] **Step 3: Run the test**

```bash
cargo nextest run -p isengard-agent
```

Expected: 1 test passes.

- [ ] **Step 4: Commit**

```bash
git add crates/isengard-agent/
git commit -m "feat(agent): minimal runner — load plugins, lifecycle, return"
```

---

### Task 16: `isengard` — clap subcommand entry point

**Files:**
- Modify: `crates/isengard/Cargo.toml`
- Modify: `crates/isengard/src/main.rs`

- [ ] **Step 1: Wire dependencies**

```bash
cat > crates/isengard/Cargo.toml <<'EOF'
[package]
name = "isengard"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Isengard container management platform — single binary, controller and agent modes"

[[bin]]
name = "isengard"
path = "src/main.rs"

[dependencies]
anyhow.workspace = true
clap.workspace = true
isengard-agent.workspace = true
isengard-controller.workspace = true
tokio = { workspace = true }
tracing.workspace = true
tracing-subscriber.workspace = true
EOF
```

- [ ] **Step 2: Write the entry point**

```bash
cat > crates/isengard/src/main.rs <<'EOF'
//! Isengard binary entry point. Parses subcommand, sets up tracing, dispatches
//! to either the controller or agent runner.

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "isengard", version, about = "Isengard container management platform")]
struct Cli {
    /// Logging filter (e.g. "info", "debug,isengard=trace"). Read from
    /// `RUST_LOG` env var if not set.
    #[arg(long, global = true, env = "ISENGARD_LOG")]
    log: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run in controller mode: aggregates agent state, hosts the dashboard
    /// and notifier plugins, distributes config.
    Controller {
        /// HTTP/gRPC listen address.
        #[arg(long, env = "ISENGARD_LISTEN", default_value = "0.0.0.0:9417")]
        listen: String,
    },
    /// Run in agent mode: registers with a controller, runs agent-side plugins
    /// (updater).
    Agent {
        /// URL of the controller, e.g. `https://controller.example.com:9417`.
        #[arg(long, env = "ISENGARD_CONTROLLER")]
        controller: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = cli
        .log
        .as_deref()
        .map(EnvFilter::new)
        .unwrap_or_else(|| EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    match cli.command {
        Command::Controller { listen } => {
            tracing::info!(%listen, "controller mode");
            isengard_controller::run_controller(isengard_controller::ControllerOptions::default()).await
        }
        Command::Agent { controller } => {
            tracing::info!(?controller, "agent mode");
            isengard_agent::run_agent(isengard_agent::AgentOptions {
                controller_url: controller,
                ..Default::default()
            }).await
        }
    }
}
EOF
```

- [ ] **Step 3: Verify build and CLI usage**

```bash
cargo build -p isengard
./target/debug/isengard --help
./target/debug/isengard controller --help
./target/debug/isengard agent --help
```

Expected:
- `isengard --help` shows two subcommands
- `isengard controller --help` shows `--listen`
- `isengard agent --help` shows `--controller`

- [ ] **Step 4: Smoke-run each mode**

```bash
./target/debug/isengard controller
./target/debug/isengard agent
```

Expected: each prints "starting controller" / "starting agent", "plugins discovered" with count 0 (no plugins registered against Agent or Controller capability in the binary's compilation unit yet — that comes in Task 17), then "exited cleanly", and exits 0.

- [ ] **Step 5: Commit**

```bash
git add crates/isengard/
git commit -m "feat(bin): clap subcommand entry — agent and controller modes"
```

---

### Task 17: Wire a `dev` plugin into the binary, write integration test

**Files:**
- Create: `crates/isengard/tests/plugin_loading.rs`
- Modify: `crates/isengard/Cargo.toml`
- Modify: `crates/isengard/src/main.rs`

The binary needs at least one plugin registered against `Capability::Agent` and `Capability::Controller` so we can prove the loading path end-to-end. We add a `dev` feature that compiles in a no-op plugin defined in the binary crate.

- [ ] **Step 1: Add a `dev` feature with a registered no-op plugin**

```bash
cat > crates/isengard/Cargo.toml <<'EOF'
[package]
name = "isengard"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Isengard container management platform — single binary, controller and agent modes"

[[bin]]
name = "isengard"
path = "src/main.rs"

[features]
default = ["dev"]
# `dev`: compiles in a no-op plugin so the host has something to load while
# real plugins (updater, dashboard, notifier) are not yet implemented. Removed
# from the default feature set when those land.
dev = []

[dependencies]
anyhow.workspace = true
async-trait.workspace = true
clap.workspace = true
inventory.workspace = true
isengard-agent.workspace = true
isengard-controller.workspace = true
isengard-core.workspace = true
serde_json.workspace = true
tokio = { workspace = true }
tracing.workspace = true
tracing-subscriber.workspace = true

[dev-dependencies]
assert_cmd = "2.0.16"
predicates = "3.1.3"
EOF
```

- [ ] **Step 2: Add the `dev` plugin module and main wiring**

```bash
cat > crates/isengard/src/main.rs <<'EOF'
//! Isengard binary entry point. Parses subcommand, sets up tracing, dispatches
//! to either the controller or agent runner.

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[cfg(feature = "dev")]
mod dev_plugin;

#[derive(Debug, Parser)]
#[command(name = "isengard", version, about = "Isengard container management platform")]
struct Cli {
    /// Logging filter (e.g. "info", "debug,isengard=trace"). Read from
    /// `RUST_LOG` env var if not set.
    #[arg(long, global = true, env = "ISENGARD_LOG")]
    log: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run in controller mode: aggregates agent state, hosts the dashboard
    /// and notifier plugins, distributes config.
    Controller {
        /// HTTP/gRPC listen address.
        #[arg(long, env = "ISENGARD_LISTEN", default_value = "0.0.0.0:9417")]
        listen: String,
    },
    /// Run in agent mode: registers with a controller, runs agent-side plugins
    /// (updater).
    Agent {
        /// URL of the controller, e.g. `https://controller.example.com:9417`.
        #[arg(long, env = "ISENGARD_CONTROLLER")]
        controller: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = cli
        .log
        .as_deref()
        .map(EnvFilter::new)
        .unwrap_or_else(|| EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    match cli.command {
        Command::Controller { listen } => {
            tracing::info!(%listen, "controller mode");
            isengard_controller::run_controller(isengard_controller::ControllerOptions::default()).await
        }
        Command::Agent { controller } => {
            tracing::info!(?controller, "agent mode");
            isengard_agent::run_agent(isengard_agent::AgentOptions {
                controller_url: controller,
                ..Default::default()
            }).await
        }
    }
}
EOF
```

```bash
cat > crates/isengard/src/dev_plugin.rs <<'EOF'
//! No-op `dev` plugin compiled in under the `dev` feature flag. Validates the
//! plugin host wiring while the real plugins (updater, dashboard, notifier)
//! are not yet implemented.

use anyhow::anyhow;
use async_trait::async_trait;
use isengard_core::{
    AgentPlugin, Capability, ControllerPlugin, Plugin, PluginContext, PluginRegistration, Result,
};

pub struct DevPlugin;

#[async_trait]
impl Plugin for DevPlugin {
    fn name(&self) -> &'static str { "dev" }
    fn version(&self) -> &'static str { "0.1.0-alpha" }

    async fn init(&mut self, _ctx: &PluginContext) -> Result<()> {
        tracing::info!(plugin = "dev", "init");
        Ok(())
    }

    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> {
        tracing::info!(plugin = "dev", "start");
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        tracing::info!(plugin = "dev", "stop");
        Ok(())
    }
}

#[async_trait]
impl AgentPlugin for DevPlugin {
    async fn run_cycle(&self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }
}

impl ControllerPlugin for DevPlugin {}

inventory::submit! {
    PluginRegistration {
        name: "dev",
        capabilities: &[Capability::Agent, Capability::Controller],
        constructor: || Box::new(DevPlugin) as Box<dyn Plugin>,
    }
}

// `anyhow` import is kept available for future expansion of dev plugin behavior;
// silence unused-import warning under #[allow] without dropping the import.
#[allow(dead_code)]
fn _keep_anyhow_imported() -> anyhow::Result<()> {
    Err(anyhow!("never called"))
}
EOF
```

- [ ] **Step 3: Build and smoke-test that the dev plugin loads**

```bash
cargo build -p isengard
./target/debug/isengard --log=info controller 2>&1 | grep -E "plugin_count|plugin =|exited cleanly"
./target/debug/isengard --log=info agent 2>&1 | grep -E "plugin_count|plugin =|exited cleanly"
```

Expected for both: `plugin_count=1` followed by `plugin = "dev"` lines (init, start, stop) and `exited cleanly`.

- [ ] **Step 4: Write the integration test**

```bash
mkdir -p crates/isengard/tests
cat > crates/isengard/tests/plugin_loading.rs <<'EOF'
//! Integration test: spawns the `isengard` binary in each mode, asserts the
//! `dev` plugin is loaded and lifecycled.

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn agent_mode_loads_dev_plugin() {
    let output = Command::cargo_bin("isengard")
        .unwrap()
        .args(["--log=info", "agent"])
        .assert()
        .success();

    output.stderr(predicate::str::contains("plugin_count=1"));
}

#[test]
fn controller_mode_loads_dev_plugin() {
    let output = Command::cargo_bin("isengard")
        .unwrap()
        .args(["--log=info", "controller"])
        .assert()
        .success();

    output.stderr(predicate::str::contains("plugin_count=1"));
}

#[test]
fn agent_help_lists_controller_flag() {
    Command::cargo_bin("isengard")
        .unwrap()
        .args(["agent", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--controller"));
}

#[test]
fn controller_help_lists_listen_flag() {
    Command::cargo_bin("isengard")
        .unwrap()
        .args(["controller", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--listen"));
}
EOF
```

- [ ] **Step 5: Run the integration tests**

```bash
cargo nextest run -p isengard
```

Expected: 4 tests pass.

If `tracing-subscriber` writes to stderr and `assert_cmd` looks at stderr correctly: the `plugin_count=1` predicate should match. If it fails, check that `tracing_subscriber::fmt()` is the default writer (it is — that's stderr). Adjust the predicate to look at `.stderr` rather than `.stdout` (it already does in the test above).

- [ ] **Step 6: Commit**

```bash
git add crates/isengard/
git commit -m "feat(bin): dev plugin under feature flag + integration tests"
```

---

### Task 18: Run the full local CI gate

- [ ] **Step 1: Run everything**

```bash
just ci-local
```

Expected:
- `cargo fmt --check` passes
- `cargo clippy --workspace --all-targets -- -D warnings` passes
- `cargo nextest run --workspace` passes (12 unit + 4 integration = 16 tests, all green)

- [ ] **Step 2: Verify the binary still runs end-to-end**

```bash
just controller
just agent
```

Expected: each spins up, logs "plugin_count=1", logs "dev" plugin lifecycle, exits cleanly.

- [ ] **Step 3: Confirm Phase 0+1 done conditions**

Tick off each (eyeball, no extra commit):

- [ ] `cargo build --workspace` succeeds
- [ ] `cargo nextest run --workspace` runs all tests green
- [ ] `just build`, `just test`, `just lint`, `just fmt-check` all succeed
- [ ] `cargo run -p isengard -- agent --help` and `... controller --help` print usage
- [ ] Both modes load and instantiate the `dev` plugin
- [ ] No commits pushed to remote

- [ ] **Step 4: Tag the foundation commit**

```bash
git log --oneline -1
git tag -a v0.1.0-alpha.foundation -m "phase 0+1: cargo workspace + plugin host wired up"
```

(The tag stays local along with the branch.)

---

## Self-review check

Before this plan is handed to an executor, the following spec requirements must be addressed:

| Spec section | Plan task |
|---|---|
| §3 Architecture overview (single binary, two modes) | Task 16 (clap entry), Task 14 (controller), Task 15 (agent) |
| §4 Repo structure (Cargo workspace, all 8 crates) | Tasks 3–4 |
| §4 Tooling: Justfile, cargo-nextest, sccache, cargo-deny | Tasks 5, 6 (sccache install left to dev environment, used in CI) |
| §5.1 Plugin trait + capability sub-traits | Task 12 |
| §5.2 Inventory-based registration | Task 13 |
| §5.3 Contract discipline (serde-clean) | Task 12 (trait signatures) |
| §6.1 gRPC service surface | Phase 2 (separate plan) |
| §6.2 Auth | Phase 2 |
| §7 Storage | Phase 2/4 |
| §8 Dashboard | Phase 5 |
| §9 v1 plugin responsibilities | Phases 3, 4, 5 |
| §10 Migration approach | Phase 3 |
| §11 Error handling philosophy | Task 9 (CoreError); recovery patterns land with networking in Phase 2 |
| §12 Testing strategy | Tasks 9–17 (unit per crate + integration test); testcontainers harness lands in Phase 2 |
| §13 Out of scope | (no work) |
| §14 Phases | Phase 0+1 implemented here, others future |

No remaining placeholders. All file paths are exact. Every code step contains code, every command step contains a command, every commit step contains the message.

---

## Execution Handoff

Plan complete and saved to `~/Projects/isengard/docs/superpowers/plans/2026-04-29-platform-foundation.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration. Each subagent gets one task, completes it, returns. I verify before claiming done.

**2. Inline Execution** — Execute tasks in this session using the executing-plans skill, batched with checkpoints for review.

Which approach?
