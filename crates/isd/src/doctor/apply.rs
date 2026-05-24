//! Apply targets and path resolution for doctor fixes.

use anyhow::Result;
use isengard_manifest::{ManifestError, StackManifest};
use std::path::Path;
use std::path::PathBuf;

/// Destination that a doctor fixer can write an updated compose body to.
#[allow(dead_code)]
pub enum ApplyTarget {
    /// A compose file on the local filesystem.
    LocalFile {
        /// Path to overwrite with the fixed compose body.
        path: PathBuf,
    },
    /// A compose document stored by the controller.
    ControllerStack {
        /// Controller stack id.
        stack_id: String,
        /// Operator-facing stack name.
        stack_name: String,
        /// SHA-256 fetched before mutation.
        sha256: String,
    },
}

#[allow(dead_code)]
impl ApplyTarget {
    /// Write an updated compose body to this target.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the target cannot be written.
    pub async fn apply(&self, body: &str, _force: bool) -> Result<()> {
        match self {
            ApplyTarget::LocalFile { path } => {
                std::fs::write(path, body)
                    .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
                Ok(())
            }
            ApplyTarget::ControllerStack { .. } => {
                anyhow::bail!("controller apply requires a session")
            }
        }
    }

    /// Write an updated compose body to this target, using `session` for
    /// controller-backed stacks.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the target cannot be written or when a controller
    /// target is applied without a session.
    pub async fn apply_with_session(
        &self,
        session: Option<&crate::session::Session>,
        body: &str,
        force: bool,
    ) -> Result<()> {
        match self {
            ApplyTarget::LocalFile { .. } => self.apply(body, force).await,
            ApplyTarget::ControllerStack {
                stack_id, sha256, ..
            } => {
                let session = session
                    .ok_or_else(|| anyhow::anyhow!("controller apply requires a session"))?;
                crate::compose_cmd::put_compose(session, stack_id, body, sha256, force).await?;
                Ok(())
            }
        }
    }
}

/// Resolve a stack manifest to its only compose file.
///
/// # Errors
///
/// Returns `Err` when the manifest cannot be read or parsed, or when
/// the manifest does not identify exactly one compose file.
#[allow(dead_code)]
pub fn resolve_manifest_compose_path(stack_toml: &Path) -> Result<PathBuf> {
    let paths = resolve_manifest_compose_paths(stack_toml)?;
    if paths.len() != 1 {
        return Err(manifest_compose_count_error(paths.len()));
    }
    Ok(paths[0].clone())
}

/// Resolve a stack manifest to its declared compose files.
///
/// # Errors
///
/// Returns `Err` when the manifest cannot be read or parsed, or when
/// the manifest has no compose files.
pub fn resolve_manifest_compose_paths(stack_toml: &Path) -> Result<Vec<PathBuf>> {
    let text = std::fs::read_to_string(stack_toml)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", stack_toml.display()))?;
    let root = stack_toml
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    let manifest = match StackManifest::from_str(&text, root.clone()) {
        Ok(manifest) => manifest,
        Err(ManifestError::EmptyCompose) => {
            return Err(manifest_compose_count_error(0));
        }
        Err(err) => return Err(anyhow::anyhow!("parsing {}: {err}", stack_toml.display())),
    };
    Ok(manifest
        .compose
        .iter()
        .map(|compose| root.join(compose))
        .collect())
}

/// Build the operator guidance used when a manifest has ambiguous compose files.
fn manifest_compose_count_error(count: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "stack.toml has {} compose entries; pass the specific compose file to isd stack doctor",
        count
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_file_apply_overwrites_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("compose.yaml");
        std::fs::write(&path, "old").unwrap();
        let target = ApplyTarget::LocalFile { path: path.clone() };
        target.apply("new", false).await.unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new");
    }

    #[tokio::test]
    async fn controller_stack_apply_requires_session() {
        let target = ApplyTarget::ControllerStack {
            stack_id: "42".to_string(),
            stack_name: "demo".to_string(),
            sha256: "abc".to_string(),
        };

        let err = target
            .apply_with_session(None, "services: {}\n", false)
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(err, "controller apply requires a session");
    }

    #[test]
    fn manifest_with_one_compose_resolves_path() {
        let tmp = tempfile::tempdir().unwrap();
        let stack = tmp.path().join("stack.toml");
        std::fs::write(&stack, "name = \"demo\"\ncompose = [\"compose.toml\"]\n").unwrap();
        let resolved = resolve_manifest_compose_path(&stack).unwrap();
        assert_eq!(resolved, tmp.path().join("compose.toml"));
    }

    #[test]
    fn manifest_with_multiple_compose_files_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stack.toml");
        std::fs::write(
            &path,
            "name = \"demo\"\ncompose = [\"base.toml\", \"prod.toml\"]\n",
        )
        .unwrap();
        let err = resolve_manifest_compose_path(&path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("pass the specific compose file"), "{err}");
    }
}
