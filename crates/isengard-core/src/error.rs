//! Errors emitted by the plugin host.
//!
//! Every variant of [`CoreError`] tags the offending plugin by `name` so the
//! host can surface "which plugin broke" without parsing display strings.
//! Plugins are free to return their own error types from operations not
//! surfaced through this enum: this type is the lingua franca at the
//! lifecycle boundary only.

use thiserror::Error;

/// Errors at the host/plugin boundary.
///
/// Plugins may also return their own error types from operations not surfaced
/// through this enum (e.g. an adapter's HTTP call failing). The variants here
/// are the ones the host itself raises while orchestrating plugin lifecycle.
#[derive(Debug, Error)]
pub enum CoreError {
    /// A plugin's configuration slice failed validation during [`crate::Plugin::init`].
    #[error("plugin {name}: invalid config: {reason}")]
    InvalidConfig {
        /// Plugin name (matches [`crate::Plugin::name`]).
        name: String,
        /// Short, human-readable reason the validation failed.
        reason: String,
    },

    /// A plugin's [`crate::Plugin::init`] returned `Err`.
    #[error("plugin {name}: init failed: {source}")]
    InitFailed {
        /// Plugin name (matches [`crate::Plugin::name`]).
        name: String,
        /// Underlying error returned by the plugin.
        #[source]
        source: anyhow::Error,
    },

    /// A plugin's [`crate::Plugin::start`] returned `Err`.
    #[error("plugin {name}: start failed: {source}")]
    StartFailed {
        /// Plugin name (matches [`crate::Plugin::name`]).
        name: String,
        /// Underlying error returned by the plugin.
        #[source]
        source: anyhow::Error,
    },

    /// A plugin's [`crate::Plugin::stop`] returned `Err`.
    #[error("plugin {name}: stop failed: {source}")]
    StopFailed {
        /// Plugin name (matches [`crate::Plugin::name`]).
        name: String,
        /// Underlying error returned by the plugin.
        #[source]
        source: anyhow::Error,
    },

    /// A plugin panicked during a lifecycle call. The host catches the
    /// unwind and reports this variant so other plugins keep running.
    #[error("plugin {name}: panicked")]
    Panicked {
        /// Plugin name (matches [`crate::Plugin::name`]).
        name: String,
    },

    /// The host was asked for a plugin by name and none was registered.
    #[error("no plugin registered with name {name}")]
    UnknownPlugin {
        /// Plugin name the caller asked for.
        name: String,
    },

    /// Catch-all for plugin-boundary errors that don't fit a more specific
    /// variant.
    ///
    /// Used by adapter implementations that want to return a free-form
    /// message (e.g. "no adapter configured") without each defining their
    /// own error type.
    #[error("{0}")]
    Other(String),
}

/// Convenience alias used throughout the host code.
///
/// Defaults `E` to [`CoreError`] so most call sites can write `Result<T>`.
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
        let err = CoreError::UnknownPlugin {
            name: "ghost".into(),
        };
        assert_eq!(err.to_string(), "no plugin registered with name ghost");
    }
}
