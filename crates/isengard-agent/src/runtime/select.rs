//! Phase 0.4 dispatch A2: env-driven [`RuntimeBackend`] factory.
//!
//! Operators pick the backend per host via `ISENGARD_RUNTIME=docker|wisp`.
//! Unset / empty defaults to docker so existing fleets see no change. The
//! wisp arm currently errors at construction; dispatch B fills it in.

use std::path::Path;
use std::sync::Arc;

use super::{RuntimeBackend, RuntimeError};

/// Parsed `ISENGARD_RUNTIME` choice. Public so unit tests can hit the
/// parsing without constructing real backends (which would touch the
/// docker socket).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendChoice {
    Docker,
    Wisp,
    Unknown(String),
}

impl BackendChoice {
    /// Parse the `ISENGARD_RUNTIME` value as the factory will. Empty,
    /// missing, or `"docker"` (case-insensitive) all map to `Docker`.
    pub fn from_env_value(raw: Option<&str>) -> Self {
        match raw.map(str::trim).map(str::to_lowercase).as_deref() {
            None | Some("") | Some("docker") => Self::Docker,
            Some("wisp") => Self::Wisp,
            Some(other) => Self::Unknown(other.to_string()),
        }
    }

    pub fn from_env() -> Self {
        Self::from_env_value(std::env::var("ISENGARD_RUNTIME").ok().as_deref())
    }
}

/// Build the [`RuntimeBackend`] selected by `ISENGARD_RUNTIME`.
///
/// Logged once at the call site in `lib.rs`; tests can call this with an
/// explicit `state_dir` against a tempdir.
pub async fn select_backend(state_dir: &Path) -> Result<Arc<dyn RuntimeBackend>, RuntimeError> {
    match BackendChoice::from_env() {
        BackendChoice::Wisp => {
            tracing::info!("runtime backend: wisp");
            let backend = super::wisp_backend::WispBackend::from_env(state_dir).await?;
            Ok(Arc::new(backend))
        }
        BackendChoice::Docker => {
            tracing::info!("runtime backend: docker (bollard)");
            let backend = super::bollard_backend::BollardBackend::from_env(state_dir).await?;
            Ok(Arc::new(backend))
        }
        BackendChoice::Unknown(other) => Err(RuntimeError::UnknownBackend(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_defaults_to_docker() {
        assert_eq!(BackendChoice::from_env_value(None), BackendChoice::Docker);
    }

    #[test]
    fn empty_defaults_to_docker() {
        assert_eq!(
            BackendChoice::from_env_value(Some("")),
            BackendChoice::Docker
        );
    }

    #[test]
    fn explicit_docker_lowercase() {
        assert_eq!(
            BackendChoice::from_env_value(Some("docker")),
            BackendChoice::Docker
        );
    }

    #[test]
    fn explicit_docker_uppercase_normalises() {
        assert_eq!(
            BackendChoice::from_env_value(Some("DOCKER")),
            BackendChoice::Docker
        );
    }

    #[test]
    fn explicit_wisp_selected() {
        assert_eq!(
            BackendChoice::from_env_value(Some("wisp")),
            BackendChoice::Wisp
        );
    }

    #[test]
    fn explicit_wisp_mixed_case() {
        assert_eq!(
            BackendChoice::from_env_value(Some("WiSp")),
            BackendChoice::Wisp
        );
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(
            BackendChoice::from_env_value(Some("  docker  ")),
            BackendChoice::Docker
        );
        assert_eq!(
            BackendChoice::from_env_value(Some(" wisp ")),
            BackendChoice::Wisp
        );
    }

    #[test]
    fn garbage_value_is_unknown() {
        assert_eq!(
            BackendChoice::from_env_value(Some("podman")),
            BackendChoice::Unknown("podman".into()),
        );
    }
}
