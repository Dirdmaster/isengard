//! Error type for the wisp-image crate.
//!
//! Phase 0.2 starts minimal; later dispatches add `Network`,
//! `DigestMismatch`, `Tar`, `Whiteout`, etc. as the surface area grows.

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
}
