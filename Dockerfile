# syntax=docker/dockerfile:1.7
# Builds the single `isengard` binary. The same image powers both the agent
# and the controller. CI publishes two image tags by targeting different
# build stages: `agent` and `controller`.
#
# Final stage: FROM scratch. The binary is statically linked against musl
# libc (no runtime libc / libgcc dependency). Image size: binary + CA certs.
# No shell, no package manager, no `true` binary (matters for install.sh's
# probe). Builder uses clux/muslrust which ships a real musl cross-toolchain
# (`x86_64-linux-musl-gcc`); Debian's `musl-tools` is a wrapper around
# system gcc that rejects `-m64` and the asm path zstd-sys uses.

ARG RUST_VERSION=1.90

# ---------------------------------------------------------------------------
# Builder. clux/muslrust ships rust + a real musl cross-toolchain
# (x86_64-linux-musl-gcc). We add bun + protobuf-compiler on top.
# ---------------------------------------------------------------------------
FROM --platform=linux/amd64 messense/rust-musl-cross:x86_64-musl AS chef
WORKDIR /build
RUN cargo install cargo-chef --locked --version 0.1.71

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG TARGETPLATFORM
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        build-essential \
        g++ \
        pkg-config \
        ca-certificates \
        cmake \
        golang-go \
        clang \
        libclang-dev \
        perl \
        git \
        curl \
        unzip \
 && rm -rf /var/lib/apt/lists/*

# Install protoc 27.x from upstream. Debian bookworm ships protobuf-compiler
# 3.21.x which doesn't support proto3 `optional` natively (it requires the
# --experimental_allow_proto3_optional flag, which prost-build doesn't pass).
# Our isengard.v1.proto uses proto3 optional, so we need >= 3.20 with that
# treated as stable. 27.x is the current LTS-ish release.
ARG PROTOC_VERSION=27.3
RUN curl -fsSL -o /tmp/protoc.zip \
        "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-x86_64.zip" \
 && unzip -q /tmp/protoc.zip -d /usr/local \
 && rm /tmp/protoc.zip \
 && protoc --version

# Install bun: the dashboard plugin's build.rs runs `bun install` + `bun run
# build` to generate the Nuxt static bundle that gets embedded via rust-embed
# into the binary. Bun runs at host arch (the builder), not in the final
# image, so it does not affect the runtime stage.
RUN curl -fsSL https://bun.sh/install | bash \
 && ln -s /root/.bun/bin/bun /usr/local/bin/bun

# clux/muslrust pre-configures: TARGET=x86_64-unknown-linux-musl, the right
# CC + AR + LINKER for the musl target, and PKG_CONFIG_ALLOW_CROSS=1.
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json \
        --target x86_64-unknown-linux-musl \
        --bin isengard

COPY . .
RUN cargo build --release \
        --target x86_64-unknown-linux-musl \
        --bin isengard \
 && strip target/x86_64-unknown-linux-musl/release/isengard

# ---------------------------------------------------------------------------
# Final stage: FROM scratch. Just the binary + CA certs. No /etc/passwd
# (numeric UIDs only); install/compose.yaml pins user: "0:0" on both
# services anyway because the controller reads /etc/isengard/master.key
# (mode 0600 root) and the agent needs root for the host docker.sock.
# ---------------------------------------------------------------------------
FROM scratch AS runtime
LABEL org.opencontainers.image.source="https://github.com/Dirdmaster/isengard"
LABEL org.opencontainers.image.description="Isengard: a container and fleet manager for your servers"
LABEL org.opencontainers.image.licenses="MIT"

COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/isengard /usr/local/bin/isengard

WORKDIR /var/lib/isengard
VOLUME ["/var/lib/isengard"]

ENTRYPOINT ["/usr/local/bin/isengard"]

# ---------------------------------------------------------------------------
# Agent variant. Default CMD: `agent` subcommand. Reads CONTROLLER_URL +
# ENROLLMENT_TOKEN from env. Mount /var/run/docker.sock to read containers.
# ---------------------------------------------------------------------------
FROM runtime AS agent
CMD ["agent"]

# ---------------------------------------------------------------------------
# Controller variant. Default CMD: `controller` with state-dir on the
# persistent volume. Listens on :9417 (gRPC, agents) + :9418 (HTTP, dashboard).
# Override with --listen / --dashboard-listen for non-default binds.
# ---------------------------------------------------------------------------
FROM runtime AS controller
EXPOSE 9417 9418
CMD ["controller", "--listen", "0.0.0.0:9417", "--state-dir", "/var/lib/isengard"]
