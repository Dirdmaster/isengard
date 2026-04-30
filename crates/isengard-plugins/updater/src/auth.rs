//! Reads ~/.docker/config.json and exposes per-registry credentials.
//!
//! Schema (subset we care about):
//!   {
//!     "auths": {
//!       "https://index.docker.io/v1/": { "auth": "<base64 user:pass>" },
//!       "ghcr.io": { "auth": "<base64 user:pass>" }
//!     }
//!   }
//!
//! `auth` is base64(user:pass). `username`/`password` fields also exist in
//! the wild as alternatives — we accept either.
//!
//! Credential helpers (`credsStore`, `credHelpers`) are NOT supported in 3b.
//! v1.x can wire `docker-credential-helpers` if needed.

use std::collections::HashMap;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;

#[derive(Debug, Clone, Default)]
pub struct DockerConfig {
    /// Map of registry-host → (username, password).
    by_host: HashMap<String, (String, String)>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    auths: HashMap<String, RawAuth>,
}

#[derive(Debug, Deserialize)]
struct RawAuth {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

impl DockerConfig {
    /// Attempt to load `~/.docker/config.json`. Missing file → empty config.
    /// Malformed file → empty config + warning logged by the caller.
    pub fn load_default() -> anyhow::Result<Self> {
        let path = match dirs::home_dir() {
            Some(h) => h.join(".docker").join("config.json"),
            None => return Ok(Self::default()),
        };
        Self::load_from(&path)
    }

    pub fn load_from(path: &PathBuf) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes =
            std::fs::read(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let raw: RawConfig = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;

        let mut by_host = HashMap::new();
        for (registry_url, raw_auth) in raw.auths {
            let host = normalise_host(&registry_url);
            let creds = decode_auth(&raw_auth)?;
            if let Some(c) = creds {
                by_host.insert(host, c);
            }
        }
        Ok(Self { by_host })
    }

    /// Look up credentials for a registry host (e.g. "docker.io", "ghcr.io").
    pub fn credentials_for(&self, registry: &str) -> Option<(String, String)> {
        // Direct hit.
        if let Some(c) = self.by_host.get(registry) {
            return Some(c.clone());
        }
        // Docker stores Hub creds under "index.docker.io"; we look up by "docker.io".
        if registry == "docker.io" {
            if let Some(c) = self.by_host.get("index.docker.io") {
                return Some(c.clone());
            }
        }
        None
    }
}

fn normalise_host(registry_url: &str) -> String {
    // Strip scheme.
    let without_scheme = registry_url
        .strip_prefix("https://")
        .or_else(|| registry_url.strip_prefix("http://"))
        .unwrap_or(registry_url);
    // Strip path component (Docker's classic key is "index.docker.io/v1/").
    let host = without_scheme.split('/').next().unwrap_or(without_scheme);
    host.to_string()
}

fn decode_auth(raw: &RawAuth) -> anyhow::Result<Option<(String, String)>> {
    // Prefer explicit username/password if provided.
    if let (Some(u), Some(p)) = (raw.username.as_ref(), raw.password.as_ref()) {
        return Ok(Some((u.clone(), p.clone())));
    }
    let Some(b64) = raw.auth.as_deref() else {
        return Ok(None);
    };
    let decoded = B64
        .decode(b64.trim())
        .map_err(|e| anyhow::anyhow!("base64-decoding auth: {e}"))?;
    let s =
        std::str::from_utf8(&decoded).map_err(|e| anyhow::anyhow!("auth blob not utf-8: {e}"))?;
    let (user, pass) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("auth blob missing ':' separator"))?;
    Ok(Some((user.to_string(), pass.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(json: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn missing_file_returns_empty_config() {
        let path = PathBuf::from("/nonexistent/.docker/config.json");
        let cfg = DockerConfig::load_from(&path).unwrap();
        assert!(cfg.credentials_for("docker.io").is_none());
    }

    #[test]
    fn empty_auths_returns_empty_config() {
        let f = write_config(r#"{"auths": {}}"#);
        let cfg = DockerConfig::load_from(&f.path().to_path_buf()).unwrap();
        assert!(cfg.credentials_for("docker.io").is_none());
    }

    #[test]
    fn base64_auth_decodes() {
        // base64("alice:secret") = "YWxpY2U6c2VjcmV0"
        let f = write_config(r#"{"auths": {"ghcr.io": {"auth": "YWxpY2U6c2VjcmV0"}}}"#);
        let cfg = DockerConfig::load_from(&f.path().to_path_buf()).unwrap();
        let (u, p) = cfg.credentials_for("ghcr.io").unwrap();
        assert_eq!(u, "alice");
        assert_eq!(p, "secret");
    }

    #[test]
    fn explicit_username_password_used_when_present() {
        let f = write_config(r#"{"auths": {"ghcr.io": {"username": "bob", "password": "pw"}}}"#);
        let cfg = DockerConfig::load_from(&f.path().to_path_buf()).unwrap();
        let (u, p) = cfg.credentials_for("ghcr.io").unwrap();
        assert_eq!(u, "bob");
        assert_eq!(p, "pw");
    }

    #[test]
    fn dockerhub_index_key_resolves_for_docker_io_lookup() {
        // base64("alice:secret")
        let f = write_config(
            r#"{"auths": {"https://index.docker.io/v1/": {"auth": "YWxpY2U6c2VjcmV0"}}}"#,
        );
        let cfg = DockerConfig::load_from(&f.path().to_path_buf()).unwrap();
        let (u, _) = cfg.credentials_for("docker.io").unwrap();
        assert_eq!(u, "alice");
    }

    #[test]
    fn malformed_base64_returns_error() {
        let f = write_config(r#"{"auths": {"ghcr.io": {"auth": "!!!not-base64!!!"}}}"#);
        let err = DockerConfig::load_from(&f.path().to_path_buf()).unwrap_err();
        assert!(err.to_string().contains("base64"));
    }

    #[test]
    fn lookup_unknown_registry_returns_none() {
        let f = write_config(r#"{"auths": {}}"#);
        let cfg = DockerConfig::load_from(&f.path().to_path_buf()).unwrap();
        assert!(cfg.credentials_for("ghcr.io").is_none());
    }
}
