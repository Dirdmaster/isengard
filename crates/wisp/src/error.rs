//! Error types for the wisp runtime.
//!
//! `WispError` is the single error returned by the public API. It wraps the
//! `oci-spec` error for parse failures, std I/O for filesystem operations,
//! and string-payload variants for the runtime subsystems (cgroup, mount,
//! capability, lifecycle, state-dir). Subsystem variants stay strings for
//! now: phase 0.1 trades structured payloads for fast iteration. We can
//! tighten them once the call sites stabilise.

use thiserror::Error;

/// Result alias used across the wisp public API.
pub type Result<T> = std::result::Result<T, WispError>;

/// Error type returned by all `wisp` public APIs.
#[derive(Debug, Error)]
pub enum WispError {
    /// OCI runtime spec parse, validation, or serialisation failure.
    #[error("spec error: {0}")]
    Spec(#[from] oci_spec::OciSpecError),

    /// Underlying I/O failure (state-dir, bundle reads, cgroup writes).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON (de)serialisation failure for state.json or cached config.json.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Cgroup v2 setup or teardown failed.
    #[error("cgroup error: {0}")]
    Cgroup(String),

    /// Mount, bind-mount, or pivot_root operation failed.
    #[error("mount error: {0}")]
    Mount(String),

    /// Capability set application failed (drop/permitted/effective/ambient).
    #[error("capability error: {0}")]
    Capability(String),

    /// Lifecycle orchestration error (clone3, sync pipe, exec, signal).
    #[error("lifecycle error: {0}")]
    Lifecycle(String),

    /// On-disk state-dir layout, persistence, or recovery error.
    #[error("state error: {0}")]
    State(String),
}
