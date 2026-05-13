//! Error type returned by every [`super::RuntimeBackend`] method. The
//! BollardBackend translates bollard's native errors into one of these
//! variants.

#[derive(thiserror::Error, Debug)]
pub enum RuntimeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend `{0}` not found")]
    UnknownBackend(String),
    #[error("docker: {0}")]
    Docker(String),
    #[error("wisp: {0}")]
    Wisp(String),
    #[error("image: {0}")]
    Image(String),
    #[error("container: {0}")]
    Container(String),
    #[error("network: {0}")]
    Network(String),
    #[error("healthcheck: {0}")]
    Healthcheck(String),
    #[error("not found: {0}")]
    NotFound(String),
}
