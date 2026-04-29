//! Errors emitted by the plugin host.

use thiserror::Error;

/// Errors at the host/plugin boundary. Plugins may also return their own error
/// types from operations not surfaced through this enum.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("plugin {name}: invalid config: {reason}")]
    InvalidConfig { name: String, reason: String },

    #[error("plugin {name}: init failed: {source}")]
    InitFailed {
        name: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("plugin {name}: start failed: {source}")]
    StartFailed {
        name: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("plugin {name}: stop failed: {source}")]
    StopFailed {
        name: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("plugin {name}: panicked")]
    Panicked { name: String },

    #[error("no plugin registered with name {name}")]
    UnknownPlugin { name: String },
}

/// Convenience alias used throughout the host code.
pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_invalid_config() {
        let err = CoreError::InvalidConfig {
            name: "updater".into(),
            reason: "missing 'interval' key".into(),
        };
        assert_eq!(
            err.to_string(),
            "plugin updater: invalid config: missing 'interval' key"
        );
    }

    #[test]
    fn display_unknown_plugin() {
        let err = CoreError::UnknownPlugin { name: "ghost".into() };
        assert_eq!(err.to_string(), "no plugin registered with name ghost");
    }
}
