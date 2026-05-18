//! `isd context import <name>`: mirror a docker context into our
//! credentials.toml as a `Backend::Docker` entry. Reads
//! `~/.docker/contexts/meta/<sha256-of-name>/meta.json` directly.
//!
//! The mirror is one-shot: subsequent `docker context update` edits do
//! not auto-sync. Operator re-runs `isd context import <name>` to
//! refresh. This matches docker's own behavior where contexts are
//! pinned snapshots once consumed.

use std::path::Path;

use anyhow::{Context as _, Result, anyhow};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::credentials::{Backend, ContextEntry};

/// Docker's `meta.json` shape (subset we read).
#[derive(Debug, Deserialize)]
struct DockerMeta {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Endpoints")]
    endpoints: DockerEndpoints,
}

#[derive(Debug, Deserialize)]
struct DockerEndpoints {
    #[serde(rename = "docker")]
    docker: DockerEndpoint,
}

#[derive(Debug, Deserialize)]
struct DockerEndpoint {
    #[serde(rename = "Host")]
    host: String,
}

/// Compute the contexts/ directory entry name for a docker context. Docker
/// stores each context under `~/.docker/contexts/meta/<sha256-of-name>/`.
fn context_dir_for(name: &str) -> String {
    let digest = Sha256::digest(name.as_bytes());
    hex::encode(digest)
}

/// Read a docker context by name and return a ContextEntry ready to insert
/// into our credentials.toml.
pub fn import_from_docker(name: &str, docker_config_dir: &Path) -> Result<ContextEntry> {
    let meta_path = docker_config_dir
        .join("contexts")
        .join("meta")
        .join(context_dir_for(name))
        .join("meta.json");
    let raw = std::fs::read_to_string(&meta_path)
        .with_context(|| format!("reading docker context meta at {}", meta_path.display()))?;
    let meta: DockerMeta = serde_json::from_str(&raw)
        .with_context(|| format!("parsing docker context meta at {}", meta_path.display()))?;
    if meta.name != name {
        return Err(anyhow!(
            "docker context name mismatch: expected {name:?}, got {:?} (corrupt store?)",
            meta.name
        ));
    }
    Ok(ContextEntry {
        name: meta.name,
        backend: Backend::Docker {
            url: meta.endpoints.docker.host,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_docker_meta(dir: &Path, name: &str, host: &str) {
        let entry = dir
            .join("contexts")
            .join("meta")
            .join(context_dir_for(name));
        std::fs::create_dir_all(&entry).unwrap();
        let meta = serde_json::json!({
            "Name": name,
            "Endpoints": { "docker": { "Host": host } }
        });
        std::fs::write(
            entry.join("meta.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn import_reads_ssh_endpoint() {
        let tmp = TempDir::new().unwrap();
        write_docker_meta(tmp.path(), "lausanne", "ssh://dirdmaster@10.17.0.125");
        let entry = import_from_docker("lausanne", tmp.path()).unwrap();
        assert_eq!(entry.name, "lausanne");
        let Backend::Docker { url } = entry.backend;
        assert_eq!(url, "ssh://dirdmaster@10.17.0.125");
    }

    #[test]
    fn import_errors_on_missing_context() {
        let tmp = TempDir::new().unwrap();
        let err = import_from_docker("does-not-exist", tmp.path()).unwrap_err();
        assert!(format!("{err}").contains("reading docker context meta"));
    }
}
