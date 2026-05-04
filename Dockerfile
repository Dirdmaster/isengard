# syntax=docker/dockerfile:1.7
# Builds the single `isengard` binary. The same image powers both the agent
# and the controller. CI publishes two image tags by targeting different
# build stages: `agent` and `controller`. Users get clean one-liners:
#   docker run ghcr.io/dirdmaster/isengard-agent:latest
#   docker run ghcr.io/dirdmaster/isengard-controller:latest
# instead of having to remember a subcommand.

ARG RUST_VERSION=1.90

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-slim-bookworm AS chef
WORKDIR /build
RUN cargo install cargo-chef --locked --version 0.1.71

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG TARGETPLATFORM
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        pkg-config \
        protobuf-compiler \
        ca-certificates \
        build-essential \
        cmake \
        golang-go \
        clang \
        libclang-dev \
        perl \
        git \
 && rm -rf /var/lib/apt/lists/*

COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json --bin isengard

COPY . .
RUN cargo build --release --bin isengard \
 && strip target/release/isengard

# ---------------------------------------------------------------------------
# Common runtime base. Both agent + controller stages extend this. Distroless
# cc-debian12 gives us libgcc/glibc + ca-certs without a shell or pkg manager.
# ---------------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
LABEL org.opencontainers.image.source="https://github.com/Dirdmaster/isengard"
LABEL org.opencontainers.image.description="Isengard: a container and fleet manager for your servers"
LABEL org.opencontainers.image.licenses="MIT"

COPY --from=builder /build/target/release/isengard /usr/local/bin/isengard

USER nonroot:nonroot
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
