//! Error type for the wisp-image crate.
//!
//! Phase 0.2 grows variants per dispatch as the surface area expands:
//! Dispatch A added Io / Parse / Json / NotFound; Dispatch B adds
//! Network and Auth for registry-touching code paths.

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
}
