# Wisp Phase 0.7: GitHub Actions release pipeline for static binaries

> Phase 0.7 of the wisp arc. Branch: `wisp/phase-0-7`. Stacked on `wisp/phase-0-6`. Sets up the artifact production that Phase 0.8 (drop docker on host) will consume; install.sh is unchanged in this phase.

## What this is

A new `.github/workflows/binaries.yml` workflow that builds `isengard` + `isd` as static binaries on tag push and uploads them to the GitHub Release alongside the existing docker images.

Targets:

| Binary | Target | Path |
|--------|--------|------|
| `isengard` | `x86_64-unknown-linux-musl` | container build (`messense/rust-musl-cross`) |
| `isengard` | `aarch64-unknown-linux-musl` | container build (`messense/rust-musl-cross`) |
| `isd` | `x86_64-unknown-linux-musl` | plain `apt-get` musl-tools |
| `isd` | `aarch64-unknown-linux-musl` | plain `apt-get` musl-tools + `gcc-aarch64-linux-gnu` |
| `isd` | `x86_64-apple-darwin` | native cargo on `macos-latest` |
| `isd` | `aarch64-apple-darwin` | native cargo on `macos-latest` |

`release.yml` now fans out to `docker.yml` + `binaries.yml` in parallel after the tag and Release are created.

## Why two Linux build paths

`isengard` depends on Pingora (`pingora-core`, `pingora-proxy`, `pingora-boringssl`), which compiles BoringSSL via `boring-sys`. That needs a real musl cross-toolchain (`x86_64-linux-musl-gcc`); Debian's `musl-tools` is a gcc wrapper that rejects `-m64` and breaks on the asm path zstd-sys uses. The Dockerfile already documents this and builds inside `messense/rust-musl-cross`. Phase 0.7 reuses the same image for the `isengard` binaries.

`isd` is rustls-only (no BoringSSL, no Pingora). Plain `apt-get install musl-tools cmake clang libclang-dev perl protobuf-compiler` is enough; aarch64 cross adds `gcc-aarch64-linux-gnu` and a `~/.cargo/config.toml` linker pin.

## Artifact shape

Each Linux artifact is a single static binary (`-` separator, target triple suffix), companion `<name>.sha256` file, and 30-day workflow-artifact copy for inspection. Mac binaries are unstripped (strip can break code-signed artifacts).

Naming examples:

```
isengard-x86_64-unknown-linux-musl
isengard-x86_64-unknown-linux-musl.sha256
isengard-aarch64-unknown-linux-musl
isd-aarch64-apple-darwin
```

`docs/RELEASES.md` documents the URL pattern, sha256 verify command, and one-shot operator install snippets.

## Local smoke

The `isd` musl build was validated locally on the OrbStack `wisp` VM (Ubuntu noble, arm64, rustc 1.95.0). `isengard` was not tested locally; it requires the BoringSSL cross-toolchain that isn't on the VM. CI exercises that path on push.

Verbatim output:

```text
$ ssh wisp@orb 'rustc --version && rustup target add aarch64-unknown-linux-musl && sudo apt-get install -y musl-tools'
rustc 1.95.0 (59807616e 2026-04-14)
info: downloading component rust-std
Setting up musl-tools (1.2.4-2) ...

$ cd /Users/dirdmaster/Projects/isengard/.worktrees/next
$ cargo build --release --target aarch64-unknown-linux-musl --bin isd
   Compiling tracing-serde v0.2.0
   Compiling chrono v0.4.44
   Compiling toml_edit v0.22.27
   ...
   Compiling reqwest v0.12.28
   Compiling tokio-tungstenite v0.24.0
   Compiling isd v0.1.0-alpha (/Users/dirdmaster/Projects/isengard/.worktrees/next/crates/isd)
    Finished `release` profile [optimized] target(s) in 41.80s

$ ldd target/aarch64-unknown-linux-musl/release/isd
	not a dynamic executable

$ ls -la target/aarch64-unknown-linux-musl/release/isd
-rwxr-xr-x 2 dirdmaster dirdmaster 6896056 May 10 13:18 target/aarch64-unknown-linux-musl/release/isd

$ ./target/aarch64-unknown-linux-musl/release/isd --version
isd 0.1.0-alpha
```

PASS: 6.9MB binary, statically linked, runs.

## What's NOT in 0.7

- **install.sh changes.** Stays on docker-image fetch in 0.7. Phase 0.8 swaps it to fetch the static binary based on `uname -m`.
- **Signed artifacts.** No cosign / minisign yet. SHA256 only. Signing is a 0.9+ decision once we know whether we want sigstore vs a key we control.
- **Universal Mac binary.** Two separate Mac assets (Intel + Apple Silicon). Operators install whichever matches `uname -m`. `lipo`-fusing them into a single fat binary is cosmetic and adds CI complexity.
- **`isengard` on Mac.** The agent links Linux-only syscalls (cgroup v2, clone3 in wisp). Mac users run `isd` only, against a Linux controller.
- **Trigger on every commit.** `binaries.yml` only runs on `workflow_call` (from release.yml), tag push (`v*`), or manual `workflow_dispatch`. Building 6 targets on every push to `next` is ~30 min of CI we don't need.

## Done bar

- `cargo check --workspace` still green on Mac (no surprise feature regressions).
- `binaries.yml` parses as valid YAML; both `release.yml` jobs (`docker`, `binaries`) declared with the right `needs:` and inputs.
- Local musl smoke proves the rustls-only `isd` path works end to end on aarch64 (CI does x86_64 too, plus `isengard` via the messense container).
- `docs/RELEASES.md` covers download URLs, sha256 verify, and operator install snippets so v0.4.0 release notes can link to it.

## Phase 0.8 hooks

Phase 0.8's `install.sh` should:

1. Resolve `ARCH=$(uname -m)` to `TARGET=x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`.
2. Pick the latest tag via the GitHub API (`/releases/latest` or pinned via env).
3. `curl -fsSL` the matching `isengard-$TARGET` and `isengard-$TARGET.sha256` to `/usr/local/bin/`.
4. Verify with `sha256sum -c`.
5. Bring up the controller / agent as a systemd unit (no docker-compose).

The artifact paths are stable: `https://github.com/Weavers-Engineering/Isengard/releases/download/$VERSION/isengard-$TARGET`.
