//! Errors emitted by the storage layer.
//!
//! Every public DAO method returns [`Result<T>`] which threads through
//! [`enum@Error`]. The variants split along the lines of "the database said
//! no" (`Db`, `Migrate`), "we couldn't read what was stored" (`Decode`,
//! `InvalidHostId`), "the caller asked for the impossible" (`Conflict`,
//! `UnknownSecrets`), and a transparent passthrough for filesystem I/O.

use thiserror::Error;

/// Failure modes for the storage crate.
#[derive(Debug, Error)]
pub enum Error {
    /// Underlying sqlx error: connection lost, constraint hit, bad SQL.
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    /// A migration in `migrations/` failed to apply.
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// A row decoded out of SQLite did not match the expected shape.
    /// Reasons include a missing column, malformed JSON, an unknown enum
    /// string, or a timestamp the parser refused.
    #[error("decoding host row: {reason}")]
    Decode {
        /// Operator-readable reason; safe to surface to the dashboard.
        reason: String,
    },

    /// A `host_id` BLOB column held a length other than 16 bytes.
    #[error("invalid HostId byte length: expected 16, got {0}")]
    InvalidHostId(usize),

    /// Filesystem error opening or creating the SQLite file.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// State machine conflict. Used by callers that need to distinguish
    /// "you asked for an illegal transition" from "the database is
    /// broken" (e.g. revoking an already-revoked cert, consuming an
    /// already-consumed enrollment token).
    #[error("conflict: {0}")]
    Conflict(String),

    /// Returned by `set_stack_secrets` when one or more requested secret
    /// names do not exist in the `secrets` table. The dashboard surfaces
    /// this as a 422 with the missing-name list.
    #[error("unknown secret names: {0:?}")]
    UnknownSecrets(Vec<String>),
}

/// Convenience alias used across the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_host_id_renders_clearly() {
        let err = Error::InvalidHostId(8);
        assert_eq!(
            err.to_string(),
            "invalid HostId byte length: expected 16, got 8"
        );
    }

    #[test]
    fn decode_renders_clearly() {
        let err = Error::Decode {
            reason: "missing column".into(),
        };
        assert_eq!(err.to_string(), "decoding host row: missing column");
    }
}
