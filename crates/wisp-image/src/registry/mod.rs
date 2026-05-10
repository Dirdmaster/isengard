//! Registry client for OCI distribution-spec endpoints.
//!
//! Layered the same way containerd's distribution client is:
//!
//!   - `auth`: parse a `WWW-Authenticate: Bearer ...` challenge and
//!     exchange it for a token at the realm. Anonymous; no client
//!     credentials are supported in 0.2.
//!   - `manifest`: deserialise a manifest or index response body using
//!     `oci_spec`, with arch selection for multi-arch indexes.
//!   - `blob`: stream a content-addressed blob from the registry into
//!     the on-disk content store with atomic-rename + sha256 verify.
//!   - `client`: orchestrates the four-step pull (manifest -> arch
//!     resolve -> config -> layers) and exposes the public API
//!     (`pull`, `lookup`, `list`, `gc`) that wisp-cli wires to.
//!
//! Public registries only. The docker.io alias maps to
//! `registry-1.docker.io` per Docker's long-standing convention; other
//! registries are used directly as `https://<host>/`.

pub mod auth;
pub mod blob;
mod client;
pub mod manifest;

pub use client::{Client, LayerRef, PulledImage};
