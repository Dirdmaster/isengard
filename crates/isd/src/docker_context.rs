//! `isd` reads docker's context store directly. No parallel
//! credentials.toml. The single source of truth is `~/.docker/contexts/`,
//! same files docker itself reads.
//!
//! Default-context resolution chain (for `--context <name>`):
//!   1. Caller-supplied name (`--context` flag)
//!   2. `DOCKER_CONTEXT` env var
//!   3. `currentContext` field in `~/.docker/config.json`
//!   4. `default` (docker's literal fallback name)

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// Subset of docker's `meta.json` we read. Forward-compatible: extra
/// fields are ignored by serde's default tolerance.
#[derive(Debug, Clone, Deserialize)]
pub struct DockerContextMeta {
    /// Context name (`docker context ls` first column).
    #[serde(rename = "Name")]
    pub name: String,
    /// Docker's free-form metadata blob (e.g. description / additional
    /// fields the operator set on `docker context create`). Read so we
    /// can forward-compat the shape; not consumed by isd today.
    #[serde(rename = "Metadata", default)]
    #[allow(dead_code)]
    pub metadata: serde_json::Value,
    /// Endpoint URLs keyed by service name.
    #[serde(rename = "Endpoints")]
    pub endpoints: DockerEndpoints,
}

/// Container for the `docker` endpoint inside `meta.json`. Docker's
/// own format key allows other services (kubernetes); we only read the
/// docker one.
#[derive(Debug, Clone, Deserialize)]
pub struct DockerEndpoints {
    /// The docker daemon endpoint.
    #[serde(rename = "docker")]
    pub docker: DockerEndpoint,
}

/// One docker endpoint entry (e.g. `ssh://op@host`).
#[derive(Debug, Clone, Deserialize)]
pub struct DockerEndpoint {
    /// Endpoint URI (`ssh://...`, `tcp://...`, `unix://...`).
    #[serde(rename = "Host")]
    pub host: String,
}

/// Operator-facing summary used by `isd context list / show`.
#[derive(Debug, Clone, Serialize)]
pub struct DockerContextSummary {
    /// Context name.
    pub name: String,
    /// Endpoint kind. Always `"docker"` today; the field exists so a
    /// future Kubernetes or Podman variant can land without a wire
    /// break.
    pub kind: &'static str,
    /// Docker host URI (the value of `Endpoints.docker.Host`).
    pub target: String,
    /// True when this context is currently active.
    pub current: bool,
}

/// Compute the contexts/meta directory entry for a given name. Docker stores
/// each context under `<docker_config>/contexts/meta/<sha256(name)>/`.
fn context_dir_for(name: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(name.as_bytes());
    hex::encode(digest)
}

/// Return `$DOCKER_CONFIG` if set, else `~/.docker/`. Matches docker's behavior.
fn docker_config_dir() -> Result<PathBuf> {
    if let Ok(env) = std::env::var("DOCKER_CONFIG") {
        return Ok(PathBuf::from(env));
    }
    Ok(dirs::home_dir()
        .context("home dir not available")?
        .join(".docker"))
}

/// Read the `currentContext` field from `~/.docker/config.json`. Honors
/// `DOCKER_CONTEXT` env first (matches docker's own precedence). Falls
/// back to `default` when neither the env nor the JSON field is set.
pub fn current_context_name() -> Result<String> {
    if let Ok(env) = std::env::var("DOCKER_CONTEXT") {
        if !env.is_empty() {
            return Ok(env);
        }
    }
    let path = docker_config_dir()?.join("config.json");
    if !path.exists() {
        return Ok("default".to_string());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(v.get("currentContext")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string())
}

/// Read a docker context's meta. Returns Err with an actionable
/// "no docker context named X; create with `docker context create X --docker host=...`" message
/// on miss.
pub fn read_context_meta(name: &str) -> Result<DockerContextMeta> {
    let path = docker_config_dir()?
        .join("contexts")
        .join("meta")
        .join(context_dir_for(name))
        .join("meta.json");
    if !path.exists() {
        // Special-case the "default" docker context (no meta.json on disk).
        // Default points at $DOCKER_HOST or the local Unix socket.
        if name == "default" {
            return Ok(DockerContextMeta {
                name: "default".into(),
                metadata: serde_json::Value::Null,
                endpoints: DockerEndpoints {
                    docker: DockerEndpoint {
                        host: std::env::var("DOCKER_HOST")
                            .unwrap_or_else(|_| "unix:///var/run/docker.sock".into()),
                    },
                },
            });
        }
        return Err(anyhow!(
            "no docker context named {name:?}; create with `docker context create {name} --docker host=...`"
        ));
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Returns the docker host URI for the resolved context (caller-supplied
/// name OR the default-context chain). Single entry point for every
/// `isd` verb that needs to talk to docker.
pub fn resolve_docker_uri(context: Option<&str>) -> Result<String> {
    let name = match context {
        Some(n) => n.to_string(),
        None => current_context_name()?,
    };
    let meta = read_context_meta(&name)?;
    Ok(meta.endpoints.docker.host)
}

/// Project a docker host URI into an operator-typeable SSH dial target.
///
/// docker's URL shapes that map to a dialable SSH target:
///
/// - `ssh://user@host`            -> `Some("user@host")`
/// - `ssh://user@host:port`       -> `Some("user@host:port")`
/// - `ssh://user@host/some/path`  -> `Some("user@host")` (strip path)
/// - `ssh://host`                 -> `Some("host")` (no user)
///
/// Non-SSH docker URIs (`tcp://`, `unix://`, anything else) return
/// `None`: there is no operator-meaningful SSH target to capture. The
/// PR B contract treats `None` as "do not PATCH": the host row keeps
/// its existing dial_target (which may have been set on an earlier
/// enroll from an SSH context, or by an explicit
/// `isd ssh hosts set <agent> --dial`).
pub fn dial_target_from_docker_uri(docker_uri: &str) -> Option<String> {
    let rest = docker_uri.strip_prefix("ssh://")?;
    // Strip any trailing path component: docker's context URL form
    // technically does not carry one, but be tolerant of an operator
    // who pasted `ssh://user@host/` from a shell history.
    let target = rest.split('/').next().unwrap_or(rest);
    if target.is_empty() {
        return None;
    }
    Some(target.to_string())
}

/// Returns the active context's name (post-resolution, for diagnostics).
pub fn resolve_context_name(context: Option<&str>) -> Result<String> {
    match context {
        Some(n) => Ok(n.to_string()),
        None => current_context_name(),
    }
}

/// List all docker contexts. Walks `~/.docker/contexts/meta/*/meta.json`.
/// Always includes the synthetic "default" context first.
pub fn list_contexts() -> Result<Vec<DockerContextSummary>> {
    let current = current_context_name().unwrap_or_else(|_| "default".into());
    let mut out: Vec<DockerContextSummary> = Vec::new();

    // Synthetic default first
    let default_host =
        std::env::var("DOCKER_HOST").unwrap_or_else(|_| "unix:///var/run/docker.sock".into());
    out.push(DockerContextSummary {
        name: "default".into(),
        kind: "docker",
        target: default_host,
        current: current == "default",
    });

    let meta_dir = docker_config_dir()?.join("contexts").join("meta");
    if meta_dir.exists() {
        for entry in std::fs::read_dir(&meta_dir)
            .with_context(|| format!("reading {}", meta_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path().join("meta.json");
            if !path.exists() {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let meta: DockerContextMeta = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(_) => continue,
            };
            out.push(DockerContextSummary {
                name: meta.name.clone(),
                kind: "docker",
                target: meta.endpoints.docker.host.clone(),
                current: current == meta.name,
            });
        }
    }
    Ok(out)
}

/// Write `currentContext` field in `~/.docker/config.json`. Read-modify-write
/// the JSON so other fields (auths, plugins, etc) survive untouched.
pub fn set_current_context(name: &str) -> Result<()> {
    // Validate the name exists first (special-case "default").
    if name != "default" {
        read_context_meta(name).with_context(|| format!("setting current context to {name:?}"))?;
    }

    let path = docker_config_dir()?.join("config.json");
    let mut value: serde_json::Value = if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
    } else {
        serde_json::json!({})
    };
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("docker config.json root is not an object"))?;
    if name == "default" {
        obj.remove("currentContext");
    } else {
        obj.insert(
            "currentContext".into(),
            serde_json::Value::String(name.into()),
        );
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let pretty = serde_json::to_string_pretty(&value)?;
    std::fs::write(&path, pretty).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_dir_matches_docker_format() {
        // sha256("default") = c2b8...0bf3 ; just verify it's 64 hex chars.
        let d = context_dir_for("default");
        assert_eq!(d.len(), 64);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Process-wide guard for tests that touch DOCKER_CONFIG /
    /// DOCKER_CONTEXT / DOCKER_HOST. These are process-global env vars;
    /// parallel tests fighting over them see each other's writes. Mirrors
    /// the `ISD_INDEX_CACHE` lock pattern in `index_cache.rs`.
    fn docker_env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn write_docker_meta(docker_config: &std::path::Path, name: &str, host: &str) {
        let entry = docker_config
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
    fn resolve_uri_reads_named_context() {
        let _lock = docker_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        write_docker_meta(tmp.path(), "lausanne", "ssh://dirdmaster@10.17.0.125");
        // SAFETY: serialized via docker_env_lock.
        unsafe {
            std::env::set_var("DOCKER_CONFIG", tmp.path());
            std::env::remove_var("DOCKER_CONTEXT");
            std::env::remove_var("DOCKER_HOST");
        }
        let uri = resolve_docker_uri(Some("lausanne")).unwrap();
        unsafe {
            std::env::remove_var("DOCKER_CONFIG");
        }
        assert_eq!(uri, "ssh://dirdmaster@10.17.0.125");
    }

    #[test]
    fn resolve_uri_follows_current_context_in_config_json() {
        let _lock = docker_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        write_docker_meta(tmp.path(), "lausanne", "ssh://op@host");
        std::fs::write(
            tmp.path().join("config.json"),
            r#"{"currentContext": "lausanne"}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("DOCKER_CONFIG", tmp.path());
            std::env::remove_var("DOCKER_CONTEXT");
            std::env::remove_var("DOCKER_HOST");
        }
        let uri = resolve_docker_uri(None).unwrap();
        unsafe {
            std::env::remove_var("DOCKER_CONFIG");
        }
        assert_eq!(uri, "ssh://op@host");
    }

    #[test]
    fn resolve_uri_falls_back_to_default_when_unset() {
        let _lock = docker_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("DOCKER_CONFIG", tmp.path());
            std::env::remove_var("DOCKER_CONTEXT");
            std::env::set_var("DOCKER_HOST", "tcp://1.2.3.4:2375");
        }
        let uri = resolve_docker_uri(None).unwrap();
        unsafe {
            std::env::remove_var("DOCKER_CONFIG");
            std::env::remove_var("DOCKER_HOST");
        }
        assert_eq!(uri, "tcp://1.2.3.4:2375");
    }

    #[test]
    fn resolve_uri_default_uses_unix_socket_when_no_docker_host() {
        let _lock = docker_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("DOCKER_CONFIG", tmp.path());
            std::env::remove_var("DOCKER_CONTEXT");
            std::env::remove_var("DOCKER_HOST");
        }
        let uri = resolve_docker_uri(None).unwrap();
        unsafe {
            std::env::remove_var("DOCKER_CONFIG");
        }
        assert_eq!(uri, "unix:///var/run/docker.sock");
    }

    #[test]
    fn read_context_meta_unknown_name_actionable_error() {
        let _lock = docker_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("DOCKER_CONFIG", tmp.path());
        }
        let err = read_context_meta("does-not-exist").unwrap_err();
        unsafe {
            std::env::remove_var("DOCKER_CONFIG");
        }
        let msg = format!("{err}");
        assert!(
            msg.contains("docker context create"),
            "missing actionable hint: {msg}"
        );
    }

    #[test]
    fn current_context_name_honors_env_first() {
        let _lock = docker_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.json"),
            r#"{"currentContext": "from-json"}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("DOCKER_CONFIG", tmp.path());
            std::env::set_var("DOCKER_CONTEXT", "from-env");
        }
        let got = current_context_name().unwrap();
        unsafe {
            std::env::remove_var("DOCKER_CONFIG");
            std::env::remove_var("DOCKER_CONTEXT");
        }
        assert_eq!(got, "from-env");
    }

    #[test]
    fn list_contexts_includes_synthetic_default_and_named() {
        let _lock = docker_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        write_docker_meta(tmp.path(), "lausanne", "ssh://op@host");
        unsafe {
            std::env::set_var("DOCKER_CONFIG", tmp.path());
            std::env::remove_var("DOCKER_CONTEXT");
            std::env::remove_var("DOCKER_HOST");
        }
        let list = list_contexts().unwrap();
        unsafe {
            std::env::remove_var("DOCKER_CONFIG");
        }
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "default");
        assert!(list[0].current);
        assert_eq!(list[1].name, "lausanne");
        assert!(!list[1].current);
    }

    #[test]
    fn set_current_context_writes_field_preserves_others() {
        let _lock = docker_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        write_docker_meta(tmp.path(), "lausanne", "ssh://op@host");
        std::fs::write(
            tmp.path().join("config.json"),
            r#"{"auths": {"registry.example": {"auth": "abc"}}, "currentContext": "old"}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("DOCKER_CONFIG", tmp.path());
        }
        set_current_context("lausanne").unwrap();
        let body = std::fs::read_to_string(tmp.path().join("config.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        unsafe {
            std::env::remove_var("DOCKER_CONFIG");
        }
        assert_eq!(v["currentContext"], "lausanne");
        // Sibling fields untouched.
        assert_eq!(v["auths"]["registry.example"]["auth"], "abc");
    }

    #[test]
    fn set_current_context_default_clears_field() {
        let _lock = docker_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.json"),
            r#"{"currentContext": "old"}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("DOCKER_CONFIG", tmp.path());
        }
        set_current_context("default").unwrap();
        let body = std::fs::read_to_string(tmp.path().join("config.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        unsafe {
            std::env::remove_var("DOCKER_CONFIG");
        }
        assert!(v.get("currentContext").is_none());
    }

    #[test]
    fn set_current_context_rejects_unknown_name() {
        let _lock = docker_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("DOCKER_CONFIG", tmp.path());
        }
        let err = set_current_context("does-not-exist").unwrap_err();
        unsafe {
            std::env::remove_var("DOCKER_CONFIG");
        }
        let msg = format!("{err:#}");
        assert!(msg.contains("docker context create"), "{msg}");
    }

    /// `dial_target_from_docker_uri` returns the SSH target an operator
    /// would type into `ssh`. Non-SSH docker URIs return `None` so the
    /// enroll-time PATCH skips the call.
    #[test]
    fn dial_target_for_ssh_user_host() {
        assert_eq!(
            dial_target_from_docker_uri("ssh://dirdmaster@10.17.0.125"),
            Some("dirdmaster@10.17.0.125".to_string())
        );
    }

    #[test]
    fn dial_target_for_ssh_user_host_port() {
        assert_eq!(
            dial_target_from_docker_uri("ssh://op@host.example:2222"),
            Some("op@host.example:2222".to_string())
        );
    }

    #[test]
    fn dial_target_strips_trailing_path() {
        assert_eq!(
            dial_target_from_docker_uri("ssh://op@host/some/path"),
            Some("op@host".to_string())
        );
    }

    #[test]
    fn dial_target_for_ssh_bare_host() {
        // No `user@`: docker falls back to the local username at
        // connect time. We return the bare host so the operator's
        // `ssh` invocation honors their `~/.ssh/config` default user.
        assert_eq!(
            dial_target_from_docker_uri("ssh://lausanne.local"),
            Some("lausanne.local".to_string())
        );
    }

    #[test]
    fn dial_target_for_non_ssh_uri_is_none() {
        assert_eq!(dial_target_from_docker_uri("tcp://10.0.0.1:2375"), None);
        assert_eq!(
            dial_target_from_docker_uri("unix:///var/run/docker.sock"),
            None
        );
        assert_eq!(dial_target_from_docker_uri("garbage"), None);
    }

    #[test]
    fn dial_target_empty_ssh_uri_is_none() {
        // `ssh://` with no host: refuse to PATCH an empty target.
        assert_eq!(dial_target_from_docker_uri("ssh://"), None);
    }
}
