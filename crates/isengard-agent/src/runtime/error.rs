//! Error type returned by every [`super::RuntimeBackend`] method. The
//! BollardBackend translates bollard's native errors into one of these
//! variants.

/// Error returned by every [`super::RuntimeBackend`] method.
#[derive(thiserror::Error, Debug)]
pub enum RuntimeError {
    /// Filesystem or other local IO error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Caller asked for a backend that isn't compiled into the agent.
    #[error("backend `{0}` not found")]
    UnknownBackend(String),
    /// Generic dockerd error path. The string carries the bollard error chain.
    #[error("docker: {0}")]
    Docker(String),
    /// Wisp backend error. Reserved for the wisp shipping path.
    #[error("wisp: {0}")]
    Wisp(String),
    /// Image pull / digest resolution failure.
    #[error("image: {0}")]
    Image(String),
    /// Container create / start / stop / remove failure.
    #[error("container: {0}")]
    Container(String),
    /// Network attach / detach failure.
    #[error("network: {0}")]
    Network(String),
    /// Healthcheck probe error.
    #[error("healthcheck: {0}")]
    Healthcheck(String),
    /// Requested object did not exist on the runtime side.
    #[error("not found: {0}")]
    NotFound(String),
}
