//! Errors emitted by the storage layer.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("decoding host row: {reason}")]
    Decode { reason: String },

    #[error("invalid HostId byte length: expected 16, got {0}")]
    InvalidHostId(usize),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_host_id_renders_clearly() {
        let err = Error::InvalidHostId(8);
        assert_eq!(err.to_string(), "invalid HostId byte length: expected 16, got 8");
    }

    #[test]
    fn decode_renders_clearly() {
        let err = Error::Decode { reason: "missing column".into() };
        assert_eq!(err.to_string(), "decoding host row: missing column");
    }
}
