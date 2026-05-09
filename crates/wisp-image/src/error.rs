//! Error type for the wisp-image crate.
//!
//! Phase 0.2 grows variants per dispatch as the surface area expands:
//! Dispatch A added Io / Parse / Json / NotFound; Dispatch B added
//! Network / Auth / Manifest for registry-touching code paths;
//! Dispatch C adds Tar / Whiteout / PathTraversal for layer extraction.

use std::path::PathBuf;

#[derive(thiserror::Error, Debug)]
pub enum WispImageError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    /// Network or HTTP transport failure: connection refused, TLS
    /// handshake failure, non-2xx response from a registry endpoint
    /// (other than auth-related 401), etc. The string carries the
    /// caller-relevant detail (URL, status, server message).
    #[error("network: {0}")]
    Network(String),
    /// Authentication / token-exchange failure: malformed
    /// WWW-Authenticate header, realm rejected the token request,
    /// realm returned a body without a `token` field.
    #[error("auth: {0}")]
    Auth(String),
    /// Manifest or index document is malformed or carries an
    /// unexpected mediaType. Distinct from `Json` because the bytes
    /// may parse as JSON yet violate OCI shape constraints.
    #[error("manifest: {0}")]
    Manifest(String),
    /// Tarball is malformed: corrupt header, truncated body,
    /// unsupported entry type, or unsupported layer mediaType.
    #[error("tar: {0}")]
    Tar(String),
    /// Whiteout entry could not be applied to the underlying rootfs:
    /// the entry's dirname resolves outside target, the name segment
    /// is empty, or the OCI whiteout shape is otherwise violated.
    /// Plain "file to delete is missing" is tolerated and does NOT
    /// produce this error.
    #[error("whiteout: {0}")]
    Whiteout(String),
    /// A tar entry path, symlink target, or hardlink target would
    /// resolve outside the extraction root. Carries the offending
    /// would-be path (after lexical resolution against target) so the
    /// operator can diagnose which entry tripped the check.
    #[error("path traversal: {0:?}")]
    PathTraversal(PathBuf),
}
