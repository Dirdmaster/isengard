# Releases

Where to find Isengard release artifacts and how to verify them.

## Where releases live

GitHub Releases:
[https://github.com/Weavers-Engineering/Isengard/releases](https://github.com/Weavers-Engineering/Isengard/releases)

Each tag has its own page:
`https://github.com/Weavers-Engineering/Isengard/releases/tag/v0.X.Y`

Two kinds of artifacts ship per release:

| Kind | Where | Used by |
|------|-------|---------|
| Docker images | `ghcr.io/weavers-engineering/isengard-agent`, `ghcr.io/weavers-engineering/isengard-controller` | docker-mode installs (current `install.sh`) |
| Static binaries | GitHub Release assets, see naming below | native installs (Phase 0.8 `install.sh`) |

## Artifact naming

Static binaries are named `<binary>-<rust-target-triple>`:

| Asset | Target | Notes |
|-------|--------|-------|
| `isengard-x86_64-unknown-linux-musl` | Linux x86_64 | static, scratch-runnable |
| `isengard-aarch64-unknown-linux-musl` | Linux aarch64 | static, scratch-runnable |
| `isd-x86_64-unknown-linux-musl` | Linux x86_64 | operator CLI |
| `isd-aarch64-unknown-linux-musl` | Linux aarch64 | operator CLI |
| `isd-x86_64-apple-darwin` | macOS Intel | operator CLI |
| `isd-aarch64-apple-darwin` | macOS Apple Silicon | operator CLI |

Each binary has a companion `<name>.sha256` file with a single-line SHA256
checksum in `sha256sum` format (`<hex>  <name>`).

`isengard` does not ship for macOS: the agent links Linux-only syscalls.
Run it inside the docker images or on a Linux host.

## Verifying a download

```bash
# Download the binary and its checksum next to each other.
TARGET=x86_64-unknown-linux-musl
VERSION=v0.4.0
curl -fsSL "https://github.com/Weavers-Engineering/Isengard/releases/download/$VERSION/isengard-$TARGET" -o isengard-$TARGET
curl -fsSL "https://github.com/Weavers-Engineering/Isengard/releases/download/$VERSION/isengard-$TARGET.sha256" -o isengard-$TARGET.sha256

# Verify.
sha256sum -c isengard-$TARGET.sha256       # Linux
shasum -a 256 -c isengard-$TARGET.sha256   # macOS
```

A passing check prints `OK`; a tampered file prints `FAILED` and exits
non-zero.

## One-shot install (operator)

For installing `isd` on a Mac:

```bash
ARCH=$(uname -m)
case "$ARCH" in
  x86_64)  TARGET=x86_64-apple-darwin ;;
  arm64)   TARGET=aarch64-apple-darwin ;;
esac
VERSION=v0.4.0   # or fetch latest via the API
curl -fsSL "https://github.com/Weavers-Engineering/Isengard/releases/download/$VERSION/isd-$TARGET" \
  -o /usr/local/bin/isd
chmod +x /usr/local/bin/isd
isd --version
```

For installing `isengard` on a Linux host (Phase 0.8 `install.sh` will do
this for you):

```bash
ARCH=$(uname -m)
case "$ARCH" in
  x86_64)  TARGET=x86_64-unknown-linux-musl ;;
  aarch64) TARGET=aarch64-unknown-linux-musl ;;
esac
VERSION=v0.4.0
curl -fsSL "https://github.com/Weavers-Engineering/Isengard/releases/download/$VERSION/isengard-$TARGET" \
  -o /usr/local/bin/isengard
chmod +x /usr/local/bin/isengard
isengard --version
```

Resolving `latest`: the GitHub Releases API returns the latest tag at
`https://api.github.com/repos/Weavers-Engineering/Isengard/releases/latest`.
Use `jq -r .tag_name` on the response to plug a real `VERSION` into the URLs
above.

## What's where

- **Docker images on GHCR**: today's `install.sh` uses these. Pull
  `ghcr.io/weavers-engineering/isengard-agent:latest` (or pin to a tag).
  Built from `Dockerfile` in CI on every tag.
- **Static binaries on GitHub Releases**: Phase 0.8's `install.sh` will use
  these. No docker dependency on the host: the binary runs against the host's
  `containerd` (or the wisp runtime in agentless mode) directly.

Both are produced by the same release pipeline (`release.yml`) on tag push.
The docker job and the binaries job run in parallel; failure in one does not
block the other.

## Local builds

If you'd rather build from source, the Dockerfile is the canonical recipe for
the Linux binaries. Mac builds are a plain `cargo build --release --bin isd`
with `protoc` from Homebrew on `PATH`.
