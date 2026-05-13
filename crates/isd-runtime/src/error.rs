use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ssh tunnel: {0}")]
    SshTunnel(String),
    #[error("docker api: {0}")]
    Docker(#[from] bollard::errors::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
}

pub type Result<T> = std::result::Result<T, Error>;
