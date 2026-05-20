//! Crate-wide error and result types for `isd-runtime`.

use std::io;

/// Errors surfaced by [`crate::DockerBackend`] and [`crate::SshTunnel`].
///
/// Variants fall into three buckets: SSH tunnel lifecycle failures
/// ([`Error::SshTunnel`]), docker daemon round-trip failures
/// ([`Error::Docker`], wrapping `bollard`'s error type), and operator
/// input failures ([`Error::InvalidEndpoint`]). [`Error::Io`] catches
/// the kernel-level errors that fall outside those three (e.g. binding
/// a local port for the tunnel).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SSH tunnel lifecycle failed. The payload names the failure point
    /// (spawn, readiness probe, etc.) in operator vocabulary.
    #[error("ssh tunnel: {0}")]
    SshTunnel(String),
    /// Docker Engine API round-trip failed. Forwarded verbatim from
    /// `bollard` so the operator sees the daemon's own error text.
    #[error("docker api: {0}")]
    Docker(#[from] bollard::errors::Error),
    /// Kernel-level I/O failed. Today this fires when port acquisition
    /// for the SSH tunnel cannot bind 127.0.0.1:0.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// The docker endpoint URI passed to [`crate::DockerBackend::from_uri`]
    /// did not match any supported scheme. The payload echoes what the
    /// operator passed so they can spot a typo.
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
}

/// Result alias that pins the error type to [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
