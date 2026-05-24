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
        }
    }
}

/// Resolve a stack manifest to its only compose file.
///
/// # Errors
///
/// Returns `Err` when the manifest cannot be read or parsed, or when
/// the manifest does not identify exactly one compose file.
pub fn resolve_manifest_compose_path(stack_toml: &Path) -> Result<PathBuf> {
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
    if manifest.compose.len() != 1 {
        return Err(manifest_compose_count_error(manifest.compose.len()));
    }
    Ok(root.join(&manifest.compose[0]))
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
