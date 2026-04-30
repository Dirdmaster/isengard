# Phase 3b: Registry Digest Check + Docker Config Auth

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** The `updater` plugin can tell which of its candidate containers are out of date. End state: every cycle logs `updater cycle: candidates=N up_to_date=M needs_update=K` and per-candidate `image=foo current_digest=sha256:abc remote_digest=sha256:def status=needs_update`. No pulls yet — that's 3c.

**Architecture:** Three new modules under `crates/isengard-plugins/updater/src/`. `image_ref.rs` parses `registry/repo:tag` into normalised parts. `auth.rs` reads `~/.docker/config.json` (`auths` map: base64 `user:pass`) and exposes per-registry credential lookup. `registry.rs` does the HTTP HEAD against the registry's `/v2/<repo>/manifests/<tag>` endpoint, handling both bearer token flows (Docker Hub `auth.docker.io`, generic `WWW-Authenticate` realm) and basic auth (private registries). `lib.rs` orchestrates: filter containers by `isengard.enable=true`, parse the image ref, fetch local digest via bollard `inspect_image`, fetch remote digest via the registry client, compare, log.

**Tech stack:** Adds `reqwest` 0.12 (rustls-tls only, no native-tls), `base64` 0.22 (decode docker config auth blobs), `dirs` 6 (find `~/.docker/config.json` cross-platform). Bearer flow follows the OCI distribution spec; private-registry basic auth follows Docker's standard `~/.docker/config.json` schema.

**Branch:** `next`. Lefthook pre-push runs full gates.

**Spec:** `docs/superpowers/specs/2026-04-29-platform-pivot-design.md` §9.1 (digest check + private registry auth).

---

## Scope

**In:**
- `reqwest` + `base64` + `dirs` workspace deps (with pinned versions)
- `image_ref.rs` — parse `nginx`, `nginx:1.25`, `ghcr.io/foo/bar:latest`, `localhost:5000/baz` into `(registry, repository, tag)` with normalisation
- `labels.rs` — filter helper: container has `isengard.enable` label set to `true` (case-insensitive)
- `auth.rs` — `DockerConfig::load_default()` reads `~/.docker/config.json` if present, exposes `credentials_for(registry: &str) -> Option<(user, pass)>`
- `registry.rs` — `RegistryClient::head_digest(image_ref, creds) -> anyhow::Result<Option<String>>`. Returns `Some(sha256:...)` on success, `None` on 404 (image/tag genuinely doesn't exist), `Err` on transport errors. Supports both bearer + basic auth.
- `lib.rs` — `do_cycle` rewritten: filter by label, for each candidate fetch local digest via `inspect_image`, fetch remote digest via registry client, classify as `up_to_date | needs_update | unknown`, aggregate counts, log.
- Unit tests per module (image_ref, labels, auth)
- Integration test: against real Docker Hub, query `library/hello-world:latest`, expect `Some(sha256:...)`. Skipped if offline.

**Out (deferred to 3c–3e):**
- Image pull (3c)
- Container recreation (3c)
- Self-update (3d)
- Old-image cleanup (3e)
- Configurable label name (hardcoded to `isengard.enable` for 3b; 3e configurable)
- Per-container update policy via labels (3e+ with `scheduler`/`hooks`)
- Cosign signature verification (v1.x or v2 per spec §13)
- Trusted-registry allowlist (v1.x per spec §13)
- Watch-all default (3b is opt-in only — `isengard.enable=true` required)
- Concurrency tuning (3b processes candidates serially; 3c can parallelise once pulls land)
- Configurable cycle interval (still hardcoded 30s; 3e moves to plugin config)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` ≥ 53 baseline + new unit tests + 1 new integration test
3. `just ci-local` clean
4. Manual smoke (if Docker is on this box): a labelled container against Docker Hub logs `status=up_to_date`; same labelled container with `:latest` after a fresh pull elsewhere shows `status=needs_update` if upstream moved
5. Tag `v0.1.0-alpha.phase3b` set locally
6. **Not pushed** until user confirms

---

## File Structure

```
crates/isengard-plugins/updater/
├── Cargo.toml                       # MODIFY: add reqwest + base64 + dirs deps
├── src/
│   ├── lib.rs                       # MODIFY: do_cycle rewritten to use new modules
│   ├── image_ref.rs                 # NEW: ImageRef parser + normalisation
│   ├── labels.rs                    # NEW: isengard_enabled() filter
│   ├── auth.rs                      # NEW: DockerConfig + credentials_for
│   └── registry.rs                  # NEW: RegistryClient + head_digest
└── tests/
    ├── plugin_loads.rs              # UNCHANGED: phase 3a lifecycle test
    └── registry_e2e.rs              # NEW: hit Docker Hub for hello-world digest, skip if offline

Cargo.toml                            # MODIFY: add reqwest + base64 + dirs to workspace deps
```

`lib.rs` was 174 lines after Phase 3a. Splitting into 4 focused modules keeps each under ~150 lines and lets each be tested independently.

---

## Task 1: Workspace deps + updater Cargo.toml

**Files:**
- Modify: `Cargo.toml` (workspace `[workspace.dependencies]`)
- Modify: `crates/isengard-plugins/updater/Cargo.toml`

- [ ] **Step 1: Add three deps to workspace `[workspace.dependencies]`**

In `Cargo.toml`, append below the existing `bollard` line (around line 68):

```toml
# HTTP client (used by updater to query registries)
reqwest = { version = "0.12.9", default-features = false, features = ["rustls-tls", "json"] }

# base64 (decode docker config auth blobs)
base64 = "0.22.1"

# user dirs (locate ~/.docker/config.json cross-platform)
dirs = "6.0.0"
```

- [ ] **Step 2: Add deps to updater Cargo.toml**

Modify `crates/isengard-plugins/updater/Cargo.toml`. Add to `[dependencies]`:

```toml
reqwest.workspace = true
base64.workspace = true
dirs.workspace = true
```

Add to (or create) `[dev-dependencies]`:

```toml
[dev-dependencies]
tokio = { workspace = true }
```

(`tokio` may already be there; this is for the dev side. If `[dev-dependencies]` already exists, just ensure tokio is in it.)

- [ ] **Step 3: Build to confirm deps resolve**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -10
```

Expected: clean build, no errors. (No new code yet — just deps available.)

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add Cargo.toml Cargo.lock crates/isengard-plugins/updater/Cargo.toml
cd ~/Projects/isengard && git commit -m "chore(deps): add reqwest, base64, dirs for registry digest checks"
```

**Self-review checklist:**
- [ ] `cargo build -p isengard-plugin-updater` clean
- [ ] `cargo fmt --check` clean
- [ ] `Cargo.lock` staged
- [ ] No `Co-Authored-By` trailer in commit

---

## Task 2: ImageRef parser

**Files:**
- Create: `crates/isengard-plugins/updater/src/image_ref.rs`
- Modify: `crates/isengard-plugins/updater/src/lib.rs` (add `mod image_ref;`)

- [ ] **Step 1: Create the module file**

Create `crates/isengard-plugins/updater/src/image_ref.rs`:

```rust
//! Parse Docker image references into (registry, repository, tag).
//!
//! Handles the common forms:
//!   nginx                          → docker.io / library/nginx / latest
//!   nginx:1.25                     → docker.io / library/nginx / 1.25
//!   library/nginx:1.25             → docker.io / library/nginx / 1.25
//!   ghcr.io/foo/bar:latest         → ghcr.io   / foo/bar       / latest
//!   localhost:5000/baz             → localhost:5000 / baz      / latest
//!
//! A "registry" is detected as the first path component if it contains a `.`,
//! a `:`, or is exactly `localhost`. Otherwise the registry is `docker.io` and
//! single-component repos are prefixed with `library/`.
//!
//! Digest references (`name@sha256:...`) are not parsed here — for the
//! updater's purposes we only need the tag form, since digest-pinned images
//! never need an update.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub registry: String,
    pub repository: String,
    pub tag: String,
}

impl ImageRef {
    /// Parse a reference string. Returns `None` for digest-pinned refs
    /// (`name@sha256:...`) since those are never out-of-date.
    pub fn parse(input: &str) -> Option<Self> {
        if input.contains('@') {
            return None;
        }

        let (head, tag) = match input.rsplit_once(':') {
            // If the part after `:` contains `/`, it was a port number, not a tag.
            Some((h, t)) if !t.contains('/') => (h, t.to_string()),
            _ => (input, "latest".to_string()),
        };

        let (registry, repo_path) = match head.split_once('/') {
            Some((maybe_registry, rest))
                if maybe_registry.contains('.')
                    || maybe_registry.contains(':')
                    || maybe_registry == "localhost" =>
            {
                (maybe_registry.to_string(), rest.to_string())
            }
            _ => ("docker.io".to_string(), head.to_string()),
        };

        // On docker.io, single-component names live under `library/`.
        let repository = if registry == "docker.io" && !repo_path.contains('/') {
            format!("library/{repo_path}")
        } else {
            repo_path
        };

        Some(Self {
            registry,
            repository,
            tag,
        })
    }

    /// Manifest URL for a HEAD request: `https://<registry>/v2/<repo>/manifests/<tag>`.
    /// `docker.io` is rewritten to `registry-1.docker.io` (the actual host).
    pub fn manifest_url(&self) -> String {
        let host = if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        };
        format!(
            "https://{host}/v2/{repo}/manifests/{tag}",
            repo = self.repository,
            tag = self.tag
        )
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}:{}", self.registry, self.repository, self.tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_defaults_to_dockerhub_library_latest() {
        let r = ImageRef::parse("nginx").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn name_with_tag() {
        let r = ImageRef::parse("nginx:1.25").unwrap();
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.tag, "1.25");
    }

    #[test]
    fn dockerhub_user_repo() {
        let r = ImageRef::parse("foo/bar").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "foo/bar");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn ghcr_with_tag() {
        let r = ImageRef::parse("ghcr.io/foo/bar:v2").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "foo/bar");
        assert_eq!(r.tag, "v2");
    }

    #[test]
    fn localhost_with_port_is_a_registry_not_a_tag() {
        let r = ImageRef::parse("localhost:5000/baz").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "baz");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn digest_pinned_returns_none() {
        assert!(ImageRef::parse("nginx@sha256:abc123").is_none());
    }

    #[test]
    fn manifest_url_rewrites_dockerhub() {
        let r = ImageRef::parse("nginx:1.25").unwrap();
        assert_eq!(
            r.manifest_url(),
            "https://registry-1.docker.io/v2/library/nginx/manifests/1.25"
        );
    }

    #[test]
    fn manifest_url_passes_other_registries_through() {
        let r = ImageRef::parse("ghcr.io/foo/bar:v2").unwrap();
        assert_eq!(
            r.manifest_url(),
            "https://ghcr.io/v2/foo/bar/manifests/v2"
        );
    }

    #[test]
    fn display_round_trips() {
        let r = ImageRef::parse("ghcr.io/foo/bar:v2").unwrap();
        assert_eq!(r.to_string(), "ghcr.io/foo/bar:v2");
    }
}
```

- [ ] **Step 2: Add `mod image_ref;` declaration to lib.rs**

In `crates/isengard-plugins/updater/src/lib.rs`, add right after the file-level `#![allow(...)]` line:

```rust
mod image_ref;
```

(Don't `pub use` anything yet — `do_cycle` will use it directly in Task 7.)

- [ ] **Step 3: Run the new tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater image_ref:: 2>&1 | tail -15
```

Expected: 9 tests pass.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/image_ref.rs crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): ImageRef parser with dockerhub library/ + manifest URL"
```

**Self-review checklist:**
- [ ] All 9 tests pass
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer in commit

---

## Task 3: Label filter

**Files:**
- Create: `crates/isengard-plugins/updater/src/labels.rs`
- Modify: `crates/isengard-plugins/updater/src/lib.rs` (add `mod labels;`)

- [ ] **Step 1: Create the module**

Create `crates/isengard-plugins/updater/src/labels.rs`:

```rust
//! Label-based filter. v1 is opt-in: only containers with
//! `isengard.enable=true` are considered candidates for update.
//!
//! Why opt-in: the friend's fleet runs containers we don't own (host metrics,
//! a tunneling daemon, system services). A watch-all default would update
//! those out from under their owner. v1.x can revisit with a per-host
//! `--watch-all` flag if real demand surfaces.

use std::collections::HashMap;

pub const ENABLE_LABEL: &str = "isengard.enable";

/// Returns true if the labels indicate the container opted in.
/// Accepts `true`, `True`, `TRUE` (case-insensitive). Anything else → false.
pub fn isengard_enabled(labels: Option<&HashMap<String, String>>) -> bool {
    labels
        .and_then(|m| m.get(ENABLE_LABEL))
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(k: &str, v: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(k.into(), v.into());
        m
    }

    #[test]
    fn missing_labels_means_disabled() {
        assert!(!isengard_enabled(None));
    }

    #[test]
    fn empty_labels_means_disabled() {
        assert!(!isengard_enabled(Some(&HashMap::new())));
    }

    #[test]
    fn no_isengard_label_means_disabled() {
        let l = label("other.label", "true");
        assert!(!isengard_enabled(Some(&l)));
    }

    #[test]
    fn label_true_lowercase_enables() {
        let l = label(ENABLE_LABEL, "true");
        assert!(isengard_enabled(Some(&l)));
    }

    #[test]
    fn label_true_uppercase_enables() {
        let l = label(ENABLE_LABEL, "TRUE");
        assert!(isengard_enabled(Some(&l)));
    }

    #[test]
    fn label_false_disables() {
        let l = label(ENABLE_LABEL, "false");
        assert!(!isengard_enabled(Some(&l)));
    }

    #[test]
    fn label_random_value_disables() {
        let l = label(ENABLE_LABEL, "yes");
        assert!(!isengard_enabled(Some(&l)));
    }
}
```

- [ ] **Step 2: Add `mod labels;` declaration to lib.rs**

In `crates/isengard-plugins/updater/src/lib.rs`, add right after the existing `mod image_ref;`:

```rust
mod labels;
```

- [ ] **Step 3: Run the tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater labels:: 2>&1 | tail -15
```

Expected: 7 tests pass.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/labels.rs crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): isengard.enable label filter (opt-in)"
```

**Self-review checklist:**
- [ ] All 7 tests pass
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer in commit

---

## Task 4: Docker config auth loader

**Files:**
- Create: `crates/isengard-plugins/updater/src/auth.rs`
- Modify: `crates/isengard-plugins/updater/src/lib.rs` (add `mod auth;`)

- [ ] **Step 1: Create the auth module**

Create `crates/isengard-plugins/updater/src/auth.rs`:

```rust
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
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
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
    let s = std::str::from_utf8(&decoded)
        .map_err(|e| anyhow::anyhow!("auth blob not utf-8: {e}"))?;
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
        let f = write_config(
            r#"{"auths": {"ghcr.io": {"auth": "YWxpY2U6c2VjcmV0"}}}"#,
        );
        let cfg = DockerConfig::load_from(&f.path().to_path_buf()).unwrap();
        let (u, p) = cfg.credentials_for("ghcr.io").unwrap();
        assert_eq!(u, "alice");
        assert_eq!(p, "secret");
    }

    #[test]
    fn explicit_username_password_used_when_present() {
        let f = write_config(
            r#"{"auths": {"ghcr.io": {"username": "bob", "password": "pw"}}}"#,
        );
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
        let f = write_config(
            r#"{"auths": {"ghcr.io": {"auth": "!!!not-base64!!!"}}}"#,
        );
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
```

- [ ] **Step 2: Add `tempfile` dev-dep + `mod auth;`**

In `crates/isengard-plugins/updater/Cargo.toml`, ensure `[dev-dependencies]` contains:

```toml
[dev-dependencies]
tokio = { workspace = true }
tempfile = "3.14.0"
```

Add `tempfile` to the workspace `[workspace.dependencies]` if not already there. Place near `serde_json`:

```toml
tempfile = "3.14.0"
```

(then change updater dev-dep to `tempfile.workspace = true`)

In `crates/isengard-plugins/updater/src/lib.rs` add after `mod labels;`:

```rust
mod auth;
```

- [ ] **Step 3: Run the auth tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater auth:: 2>&1 | tail -20
```

Expected: 7 tests pass.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add Cargo.toml Cargo.lock crates/isengard-plugins/updater/Cargo.toml crates/isengard-plugins/updater/src/auth.rs crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): DockerConfig auth loader (~/.docker/config.json)"
```

**Self-review checklist:**
- [ ] All 7 tests pass
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] `Cargo.lock` staged
- [ ] No `Co-Authored-By` trailer in commit

---

## Task 5: Registry HEAD client

**Files:**
- Create: `crates/isengard-plugins/updater/src/registry.rs`
- Modify: `crates/isengard-plugins/updater/src/lib.rs` (add `mod registry;`)

- [ ] **Step 1: Create the registry module**

Create `crates/isengard-plugins/updater/src/registry.rs`:

```rust
//! HTTP HEAD against a registry's manifest endpoint, returns the
//! `Docker-Content-Digest` header.
//!
//! Auth flow:
//!   1. HEAD the manifest URL with no auth.
//!   2a. 200 → done, read `Docker-Content-Digest`.
//!   2b. 401 → parse `WWW-Authenticate`. Two flavours:
//!       - `Basic ...` → retry with Basic creds from DockerConfig.
//!       - `Bearer realm=...,service=...,scope=...` → fetch a bearer token
//!         from the realm (with optional Basic creds for private repos),
//!         retry HEAD with `Authorization: Bearer <token>`.
//!   2c. 404 → `Ok(None)` (image/tag genuinely doesn't exist).
//!   2d. anything else → Err.
//!
//! Manifest media types we accept (newest first; the registry picks):
//!   - application/vnd.oci.image.index.v1+json
//!   - application/vnd.oci.image.manifest.v1+json
//!   - application/vnd.docker.distribution.manifest.list.v2+json
//!   - application/vnd.docker.distribution.manifest.v2+json

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue, WWW_AUTHENTICATE};
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::auth::DockerConfig;
use crate::image_ref::ImageRef;

const ACCEPT_MANIFESTS: &str = concat!(
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.docker.distribution.manifest.v2+json"
);

pub struct RegistryClient {
    http: Client,
    config: DockerConfig,
}

impl RegistryClient {
    pub fn new(config: DockerConfig) -> anyhow::Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("isengard/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| anyhow::anyhow!("building http client: {e}"))?;
        Ok(Self { http, config })
    }

    /// Returns the remote manifest digest, or `None` if the tag doesn't exist.
    pub async fn head_digest(&self, image: &ImageRef) -> anyhow::Result<Option<String>> {
        let url = image.manifest_url();

        // Initial unauthenticated HEAD.
        let resp = self
            .http
            .head(&url)
            .header(ACCEPT, ACCEPT_MANIFESTS)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("HEAD {url}: {e}"))?;

        match resp.status() {
            StatusCode::OK => return Ok(extract_digest(resp.headers())),
            StatusCode::NOT_FOUND => return Ok(None),
            StatusCode::UNAUTHORIZED => { /* fall through to auth */ }
            other => {
                return Err(anyhow::anyhow!(
                    "unexpected status from registry: {other} for {url}"
                ));
            }
        }

        let www_auth = resp
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| {
                anyhow::anyhow!("registry returned 401 without WWW-Authenticate for {url}")
            })?;

        let creds = self.config.credentials_for(&image.registry);

        let auth_header = if let Some(challenge) = parse_bearer(www_auth) {
            let token = self
                .fetch_bearer_token(&challenge, creds.as_ref())
                .await?;
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| anyhow::anyhow!("bad bearer token: {e}"))?
        } else if www_auth.to_ascii_lowercase().starts_with("basic") {
            let (u, p) = creds.as_ref().ok_or_else(|| {
                anyhow::anyhow!("registry requires Basic auth but no credentials in docker config")
            })?;
            let blob = B64.encode(format!("{u}:{p}"));
            HeaderValue::from_str(&format!("Basic {blob}"))
                .map_err(|e| anyhow::anyhow!("bad basic header: {e}"))?
        } else {
            return Err(anyhow::anyhow!(
                "unsupported WWW-Authenticate scheme: {www_auth}"
            ));
        };

        let resp = self
            .http
            .head(&url)
            .header(ACCEPT, ACCEPT_MANIFESTS)
            .header(AUTHORIZATION, auth_header)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("authed HEAD {url}: {e}"))?;

        match resp.status() {
            StatusCode::OK => Ok(extract_digest(resp.headers())),
            StatusCode::NOT_FOUND => Ok(None),
            other => Err(anyhow::anyhow!(
                "registry returned {other} after auth for {url}"
            )),
        }
    }

    async fn fetch_bearer_token(
        &self,
        challenge: &BearerChallenge,
        creds: Option<&(String, String)>,
    ) -> anyhow::Result<String> {
        let mut req = self.http.get(&challenge.realm);
        if let Some(service) = &challenge.service {
            req = req.query(&[("service", service.as_str())]);
        }
        if let Some(scope) = &challenge.scope {
            req = req.query(&[("scope", scope.as_str())]);
        }
        if let Some((u, p)) = creds {
            req = req.basic_auth(u, Some(p));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("bearer token fetch from {}: {e}", challenge.realm))?;

        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "bearer endpoint {} returned {}",
                challenge.realm,
                resp.status()
            ));
        }

        let body: TokenResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("parsing bearer token response: {e}"))?;

        // Some registries return `token`, some `access_token`.
        body.token
            .or(body.access_token)
            .ok_or_else(|| anyhow::anyhow!("bearer token response had neither token nor access_token"))
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

fn parse_bearer(www_auth: &str) -> Option<BearerChallenge> {
    let trimmed = www_auth.trim();
    if !trimmed.to_ascii_lowercase().starts_with("bearer ") {
        return None;
    }
    let rest = &trimmed["bearer ".len()..];

    let mut realm = None;
    let mut service = None;
    let mut scope = None;

    for part in split_kv(rest) {
        match part {
            ("realm", v) => realm = Some(v.to_string()),
            ("service", v) => service = Some(v.to_string()),
            ("scope", v) => scope = Some(v.to_string()),
            _ => {}
        }
    }

    realm.map(|r| BearerChallenge {
        realm: r,
        service,
        scope,
    })
}

/// Split `realm="x",service="y",scope="z"` honoring quoted values.
fn split_kv(input: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c.is_ascii_whitespace() || c == ',' {
            continue;
        }
        // Find '='
        let key_start = i;
        while let Some(&(_, ch)) = chars.peek() {
            if ch == '=' {
                break;
            }
            chars.next();
        }
        let key_end = chars.peek().map(|&(j, _)| j).unwrap_or(input.len());
        let key = &input[key_start..key_end];
        // Consume '='
        chars.next();
        // Value: quoted or until ','
        let val = if let Some(&(_, '"')) = chars.peek() {
            chars.next();
            let val_start = chars.peek().map(|&(j, _)| j).unwrap_or(input.len());
            let mut val_end = val_start;
            while let Some((j, ch)) = chars.next() {
                if ch == '"' {
                    val_end = j;
                    break;
                }
            }
            &input[val_start..val_end]
        } else {
            let val_start = chars.peek().map(|&(j, _)| j).unwrap_or(input.len());
            let mut val_end = input.len();
            while let Some(&(j, ch)) = chars.peek() {
                if ch == ',' {
                    val_end = j;
                    break;
                }
                chars.next();
            }
            &input[val_start..val_end]
        };
        out.push((key, val));
    }
    out
}

fn extract_digest(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("docker-content-digest")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dockerhub_bearer_challenge() {
        let h = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/hello-world:pull""#;
        let c = parse_bearer(h).unwrap();
        assert_eq!(c.realm, "https://auth.docker.io/token");
        assert_eq!(c.service.as_deref(), Some("registry.docker.io"));
        assert_eq!(
            c.scope.as_deref(),
            Some("repository:library/hello-world:pull")
        );
    }

    #[test]
    fn parses_realm_only_bearer() {
        let h = r#"Bearer realm="https://example.com/token""#;
        let c = parse_bearer(h).unwrap();
        assert_eq!(c.realm, "https://example.com/token");
        assert!(c.service.is_none());
        assert!(c.scope.is_none());
    }

    #[test]
    fn returns_none_for_non_bearer_scheme() {
        assert!(parse_bearer("Basic realm=\"x\"").is_none());
    }
}
```

- [ ] **Step 2: Add `mod registry;` declaration**

In `crates/isengard-plugins/updater/src/lib.rs` after `mod auth;`:

```rust
mod registry;
```

- [ ] **Step 3: Run unit tests for registry**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater registry:: 2>&1 | tail -15
```

Expected: 3 tests pass.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/registry.rs crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): RegistryClient — HEAD manifest with bearer + basic auth"
```

**Self-review checklist:**
- [ ] All 3 unit tests pass
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer in commit

---

## Task 6: Wire cycle to use registry + label filter

**Files:**
- Modify: `crates/isengard-plugins/updater/src/lib.rs` (rewrite `do_cycle`)

- [ ] **Step 1: Rewrite `do_cycle` and update `Updater` struct**

In `crates/isengard-plugins/updater/src/lib.rs`, change the imports and `do_cycle` function. Final state of the relevant sections:

Add these imports at the top (alongside existing ones):

```rust
use bollard::image::InspectImageOptions;

use crate::auth::DockerConfig;
use crate::image_ref::ImageRef;
use crate::labels::isengard_enabled;
use crate::registry::RegistryClient;
```

Update the `Updater` struct to hold a `RegistryClient`:

```rust
pub struct Updater {
    docker: Option<Docker>,
    registry: Option<Arc<RegistryClient>>,
    cancel: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl Updater {
    pub fn new() -> Self {
        Self {
            docker: None,
            registry: None,
            cancel: Arc::new(Notify::new()),
            task: None,
        }
    }
}
```

Add registry construction in `init` (after the docker version probe):

```rust
        let docker_config = DockerConfig::load_default().unwrap_or_else(|e| {
            warn!(error = %e, "failed to read ~/.docker/config.json — proceeding without registry creds");
            DockerConfig::default()
        });
        let registry =
            RegistryClient::new(docker_config).map_err(|e| init_err(format!("registry client: {e}")))?;
        self.registry = Some(Arc::new(registry));
```

Pass the registry into `start`'s spawned task by cloning the Arc:

```rust
    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> {
        let docker = self
            .docker
            .clone()
            .ok_or_else(|| start_err("updater started before init"))?;
        let registry = self
            .registry
            .clone()
            .ok_or_else(|| start_err("updater started before init"))?;
        let cancel = self.cancel.clone();

        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(CYCLE_INTERVAL);
            loop {
                tokio::select! {
                    _ = cancel.notified() => {
                        debug!("updater cycle task cancelled");
                        break;
                    }
                    _ = ticker.tick() => {
                        if let Err(e) = do_cycle(&docker, &registry).await {
                            warn!(error = %e, "updater cycle failed");
                        }
                    }
                }
            }
        });

        self.task = Some(task);
        info!("updater started");
        Ok(())
    }
```

Update `AgentPlugin::run_cycle` to take both:

```rust
#[async_trait]
impl AgentPlugin for Updater {
    async fn run_cycle(&self, _ctx: &PluginContext) -> Result<()> {
        let docker = self
            .docker
            .as_ref()
            .ok_or_else(|| init_err("run_cycle before init"))?;
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| init_err("run_cycle before init"))?;
        do_cycle(docker, registry)
            .await
            .map_err(|e| init_err(format!("cycle failed: {e}")))
    }
}
```

Replace `do_cycle` with the new version:

```rust
/// One cycle of work. Filter candidates by `isengard.enable=true`, compare
/// each one's local digest against its remote registry digest, classify, log.
async fn do_cycle(docker: &Docker, registry: &RegistryClient) -> anyhow::Result<()> {
    let opts = ListContainersOptions::<String> {
        all: false,
        ..Default::default()
    };
    let containers = docker
        .list_containers(Some(opts))
        .await
        .map_err(|e| anyhow::anyhow!("listing containers: {e}"))?;

    let candidates: Vec<_> = containers
        .iter()
        .filter(|c| isengard_enabled(c.labels.as_ref()))
        .collect();

    let mut up_to_date = 0usize;
    let mut needs_update = 0usize;
    let mut unknown = 0usize;

    for c in &candidates {
        let name = c
            .names
            .as_ref()
            .and_then(|ns| ns.first())
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| "<unknown>".into());
        let image_str = c.image.as_deref().unwrap_or("");

        let Some(image_ref) = ImageRef::parse(image_str) else {
            debug!(container = %name, image = %image_str, "skipping digest-pinned or unparseable image");
            continue;
        };

        let local_digest = match docker
            .inspect_image_with_options(image_str, InspectImageOptions::default())
            .await
        {
            Ok(i) => i
                .repo_digests
                .as_ref()
                .and_then(|v| v.first())
                .and_then(|d| d.split_once('@'))
                .map(|(_, dig)| dig.to_string()),
            Err(e) => {
                warn!(container = %name, image = %image_str, error = %e, "inspect_image failed");
                None
            }
        };

        let remote_digest = match registry.head_digest(&image_ref).await {
            Ok(d) => d,
            Err(e) => {
                warn!(container = %name, image = %image_str, error = %e, "registry HEAD failed");
                unknown += 1;
                continue;
            }
        };

        match (local_digest.as_deref(), remote_digest.as_deref()) {
            (Some(local), Some(remote)) if local == remote => {
                info!(container = %name, image = %image_str, status = "up_to_date");
                up_to_date += 1;
            }
            (Some(local), Some(remote)) => {
                info!(
                    container = %name,
                    image = %image_str,
                    current_digest = %local,
                    remote_digest = %remote,
                    status = "needs_update"
                );
                needs_update += 1;
            }
            _ => {
                debug!(container = %name, image = %image_str, "could not classify (missing local or remote digest)");
                unknown += 1;
            }
        }
    }

    info!(
        candidates = candidates.len(),
        up_to_date,
        needs_update,
        unknown,
        "updater cycle complete"
    );
    Ok(())
}
```

Note: `inspect_image_with_options` is the correct bollard method name in 0.18.

- [ ] **Step 2: Confirm clippy + build are clean**

```bash
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: clean.

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 3: Run all updater unit tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --lib 2>&1 | tail -15
```

Expected: 17 tests pass (9 image_ref + 7 labels + 7 auth + 3 registry — minus 9 = wait, count the right way: 9+7+7+3 = 26).

(If the count differs, the tests are still right; just verify no failures. The plan doesn't depend on a specific count beyond "no failures, more than the previous baseline".)

- [ ] **Step 4: Confirm phase 3a integration test still passes**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --test plugin_loads 2>&1 | tail -10
```

Expected: pass (or skip if Docker is not running).

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): cycle classifies candidates as up_to_date / needs_update via registry digest"
```

**Self-review checklist:**
- [ ] All updater unit tests pass
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] phase 3a `plugin_loads` test still passes (or skips)
- [ ] No `Co-Authored-By` trailer in commit

---

## Task 7: Integration test — real Docker Hub HEAD

**Files:**
- Create: `crates/isengard-plugins/updater/tests/registry_e2e.rs`

- [ ] **Step 1: Create the test**

Create `crates/isengard-plugins/updater/tests/registry_e2e.rs`:

```rust
//! Integration test: hit the real Docker Hub registry for `library/hello-world:latest`,
//! expect a sha256 digest. Skips if the network is unreachable.
//!
//! `hello-world` is the canonical "tiniest public image" and is extremely
//! unlikely to disappear, so this test is a stable smoke for the bearer-token
//! flow against `auth.docker.io`.

#![allow(clippy::result_large_err)]

use isengard_plugin_updater::{
    auth::DockerConfig, image_ref::ImageRef, registry::RegistryClient,
};

async fn network_reachable() -> bool {
    // Try a tiny request against auth.docker.io. If DNS fails or we can't
    // reach the host, treat as offline and skip.
    match reqwest::Client::new()
        .head("https://auth.docker.io")
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
    {
        Ok(_) => true,
        Err(_) => false,
    }
}

#[tokio::test]
async fn head_digest_for_hello_world_returns_sha256() {
    if !network_reachable().await {
        eprintln!("skipping: network not reachable");
        return;
    }

    let client = RegistryClient::new(DockerConfig::default()).unwrap();
    let image = ImageRef::parse("library/hello-world:latest").unwrap();

    let digest = client
        .head_digest(&image)
        .await
        .expect("registry HEAD should succeed");

    let digest = digest.expect("hello-world:latest should exist");
    assert!(
        digest.starts_with("sha256:"),
        "expected sha256 digest, got: {digest}"
    );
    assert!(
        digest.len() > "sha256:".len() + 32,
        "digest looks truncated: {digest}"
    );
}
```

- [ ] **Step 2: Make `image_ref`, `auth`, `registry` modules accessible to integration tests**

In `crates/isengard-plugins/updater/src/lib.rs`, change the three module declarations from private to `pub`:

```rust
pub mod auth;
pub mod image_ref;
pub mod labels;
pub mod registry;
```

(Integration tests live in a separate crate compilation; private modules aren't visible. Making them `pub` in the library is the standard pattern.)

- [ ] **Step 3: Run the test**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --test registry_e2e 2>&1 | tail -15
```

Expected: passes against real Docker Hub (digest starts with `sha256:`), or takes the skip path if offline.

- [ ] **Step 4: Confirm clippy still clean (with the now-public modules)**

```bash
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/tests/registry_e2e.rs crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "test(updater): registry HEAD against Docker Hub for hello-world (skipped if offline)"
```

**Self-review checklist:**
- [ ] Test passes (or skips cleanly)
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer in commit

---

## Task 8: CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 3b`, re-run.

- [ ] **Step 2: Confirm test counts**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4} END {print "Total passing:", sum}'
```

Expected: ≥ 53 (Phase 3a baseline) + 26 unit + 1 integration = 80, give or take. Anything above the 53 baseline + new tests count is fine. **Critical:** zero failures.

- [ ] **Step 3: Manual smoke (only if Docker is on this box)**

```bash
cd ~/Projects/isengard
mkdir -p /tmp/isengard-3b-ctrl /tmp/isengard-3b-agent

# Run a labelled hello-world container so the updater has something to chew on.
docker run -d --name isengard-3b-target --label isengard.enable=true hello-world || true

ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9461 --state-dir /tmp/isengard-3b-ctrl &
CTRL=$!
sleep 1

ISENGARD_TOKEN=test ./target/debug/isengard agent --controller http://127.0.0.1:9461 --state-dir /tmp/isengard-3b-agent 2>&1 | tee /tmp/isengard-3b-agent.log &
AGENT=$!
sleep 35

echo "--- updater cycle log ---"
grep -E "updater cycle complete|status=|connected to docker" /tmp/isengard-3b-agent.log

kill -INT $AGENT 2>/dev/null
wait $AGENT 2>/dev/null || true
kill $CTRL 2>/dev/null
wait $CTRL 2>/dev/null || true
docker rm -f isengard-3b-target 2>/dev/null || true
```

Expected log lines:
- `updater connected to docker daemon ...`
- For the hello-world container: `status=up_to_date` (or `status=needs_update` if upstream moved since the local pull)
- `updater cycle complete candidates=N up_to_date=M needs_update=K unknown=L`

If you don't have Docker on this box, skip — the integration tests cover the same paths.

- [ ] **Step 4: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase3b -m "phase 3b: updater compares local vs registry digests, classifies candidates"
cd ~/Projects/isengard && git tag -l | grep phase3b
```

Don't push. User confirms before push.

- [ ] **Step 5: Confirm done conditions**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 53 baseline + new tests, zero failures
- [ ] `just ci-local` clean
- [ ] Manual smoke shows status= classifications (or test in Task 7 covers it if no local Docker)
- [ ] Tag `v0.1.0-alpha.phase3b` exists locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (§9.1) | Plan task |
|---|---|
| List running containers (filter by `isengard.enable` label) | Task 3 (filter) + Task 6 (wired into cycle) |
| Check remote registry digest via HEAD | Task 5 (RegistryClient) + Task 6 (called per candidate) |
| Compare against local `RepoDigests` | Task 6 (`inspect_image` + first `repo_digests` entry) |
| Private registry auth via `~/.docker/config.json` | Task 4 (DockerConfig) + Task 5 (used for bearer/basic flows) |

3c (image pull), 3d (self-update), 3e (cleanup + scheduling) explicitly deferred. No placeholders. No silent failures: every error path either logs+continues, classifies as `unknown`, or returns Err to bubble up to the cycle's `warn!`.

**Type consistency check:**
- `RegistryClient::new` takes `DockerConfig` (Task 5) — used in Task 6 init.
- `RegistryClient::head_digest(&ImageRef) -> anyhow::Result<Option<String>>` (Task 5) — called in Task 6 with `Ok(Some)/Ok(None)/Err` all handled.
- `ImageRef::parse(&str) -> Option<Self>` (Task 2) — Task 6 uses `let Some(image_ref) = ... else { continue; }`.
- `isengard_enabled(Option<&HashMap<String,String>>) -> bool` (Task 3) — Task 6 calls with `c.labels.as_ref()`. Bollard's `ContainerSummary.labels` is `Option<HashMap<String, String>>` ✓.
- All four modules `pub` after Task 7 so the integration test can reach them.

**Workspace dep additions:** `reqwest`, `base64`, `dirs`, `tempfile`. All pinned. License compatibility (MIT/Apache-2.0/BSD) — `reqwest` brings `webpki-roots` (CDLA-Permissive-2.0, already allowed in deny.toml from Phase 2c).

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-3b-registry-digest.md`. Subagent-driven execution.
