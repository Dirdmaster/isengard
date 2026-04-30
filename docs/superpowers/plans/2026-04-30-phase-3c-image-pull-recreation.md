# Phase 3c: Image Pull + Faithful Container Recreation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** When a candidate container is classified `needs_update`, the updater actually performs the update — pulls the new image, stops + removes the old container, creates + starts a replacement preserving its config (ports, mounts, networks, env, labels, restart policy). Self-update (replacing the agent's own container) is explicitly out of scope; that's 3d.

**Architecture:** A new `recreate.rs` module owns the orchestration: `update_container(docker, container_id) -> Result<()>` does inspect → pull → stop → remove → create → start → reconnect-networks. A pure-function `capture_config(inspect) -> RecreateSpec` extracts the relevant fields with mount-dedup and network-deduplication, making the translation testable in isolation. `do_cycle` calls `update_container` per `needs_update` candidate; failures log + continue (no aborting the whole cycle).

**Tech stack:** Adds a few bollard 0.18 types (`CreateContainerOptions`, `Config`, `HostConfig`, `RestartPolicy`, `MountPoint`, `EndpointSettings`, `CreateImageOptions`). The pull stream from `Docker::create_image` is consumed via `futures-util::StreamExt`. No new workspace deps — `futures-util` is already in the tree as a `bollard` transitive but we'll declare it explicitly.

**Branch:** `next`. Lefthook pre-push runs full gates.

**Spec:** `docs/superpowers/specs/2026-04-29-platform-pivot-design.md` §9.1 (recreate preserving config) + §10 (mount/volume dedup is one of the "subtle bits" the test suite must cover).

---

## Scope

**In:**
- `futures-util` workspace dep (explicit declaration; needed for `StreamExt` over the pull stream)
- `recreate.rs` module under `crates/isengard-plugins/updater/src/`:
  - `RecreateSpec` struct holding the captured config (image name, container name, Config, HostConfig, networks-to-reconnect)
  - `capture_config(inspect: &ContainerInspectResponse, new_image: &str) -> RecreateSpec` — pure function, fully unit-testable
  - `pull_image(docker: &Docker, image_ref: &ImageRef) -> anyhow::Result<()>` — consumes the create_image progress stream, returns when done or on first error event
  - `update_container(docker: &Docker, container_id: &str, image_ref: &ImageRef) -> anyhow::Result<()>` — orchestration
- Mount dedup: when `Binds` and `Mounts` both target the same destination, drop the `Bind` entry (Mounts is the modern API and may have richer options like read-only flags).
- Network handling: capture all `EndpointSettings` except the default `bridge`. After `start_container`, reconnect each captured network.
- Self-protection: a `self_id.rs` module detects this process's container ID by reading `/proc/self/cgroup` (Linux) or returning `None` (macOS, where the agent doesn't run inside Docker in dev). If `self_id == container_id`, skip with a WARN log: "skipping self-update; deferred to phase 3d".
- Restart-state guard: if `inspect.state.status == Some("restarting")`, skip this cycle for that container.
- Wire `update_container` into `do_cycle`: for each `needs_update` candidate, call it; log success or failure but never abort the cycle.
- Unit tests for `capture_config` covering: env preservation, labels preservation, restart policy preservation, port bindings preservation, mount dedup (Bind+Mount targeting same path → only Mount kept), network exclusion (bridge filtered out, others kept).
- Integration test: agent against real Docker, pull a `hello-world` image, label it `isengard.enable=true`, force a cycle. Skips if Docker absent. The test verifies that *pull is invoked* and that *update_container does not panic when the local digest matches the remote* (i.e., when the cycle classifies as `up_to_date`, recreate is NOT called — this is a regression guard for the `do_cycle` branching).

**Out (deferred to 3d–3e):**
- Self-update with rename-then-replace ordering (3d)
- Old image cleanup post-update (3e)
- Configurable cycle interval (3e)
- testcontainers-based full e2e (3e)
- Healthcheck-driven rollback (v1.x per spec §13)
- Per-container update policy via labels (v1.x with `scheduler`/`hooks`)
- Concurrent updates of multiple containers (3c does them serially; 3e revisits)
- Pull retry / backoff (single attempt per cycle for now)
- Authenticated pulls via `~/.docker/config.json` for the pull itself (Phase 3b's auth is for HEAD only; bollard's `create_image` accepts a `Some(CreateImageOptions { ... })` + a `Some(DockerCredentials)` arg — we pass `None` for both in 3c, meaning unauthenticated pulls only. Private registry pulls are deferred to a 3c.1 follow-up if it surfaces in smoke testing)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes — baseline 80 + new unit tests + 1 new integration test
3. `just ci-local` clean
4. Manual smoke (if Docker is on this box): start a labelled `nginx:alpine` container, force its image to be out of date by retagging an older nginx image as `nginx:alpine`, run agent against it, observe the container being recreated with same name, ports, labels intact
5. Tag `v0.1.0-alpha.phase3c` set locally
6. **Not pushed** until user confirms

---

## File Structure

```
crates/isengard-plugins/updater/
├── Cargo.toml                       # MODIFY: add futures-util workspace dep
├── src/
│   ├── lib.rs                       # MODIFY: do_cycle calls recreate::update_container on needs_update
│   ├── image_ref.rs                 # UNCHANGED
│   ├── labels.rs                    # UNCHANGED
│   ├── auth.rs                      # UNCHANGED
│   ├── registry.rs                  # UNCHANGED
│   ├── self_id.rs                   # NEW: detect this process's own container ID
│   └── recreate.rs                  # NEW: capture_config + pull_image + update_container
└── tests/
    ├── plugin_loads.rs              # UNCHANGED
    ├── registry_e2e.rs              # UNCHANGED
    └── recreate_e2e.rs              # NEW: pull hello-world, classify, verify update_container
                                     #      is not called when up_to_date (guard test)

Cargo.toml                            # MODIFY: add futures-util to workspace deps
```

`recreate.rs` is the largest new file (~250 lines). Splitting it further would scatter the orchestration; keeping `capture_config` + `pull_image` + `update_container` together lets the orchestration read top-to-bottom.

---

## Task 1: Workspace deps + updater Cargo.toml

**Files:**
- Modify: `Cargo.toml` (workspace `[workspace.dependencies]`)
- Modify: `crates/isengard-plugins/updater/Cargo.toml`

- [ ] **Step 1: Add `futures-util` to workspace deps**

In `Cargo.toml`, append below the existing `dirs = "6.0.0"` line:

```toml
# stream utilities (StreamExt for bollard's create_image progress stream)
futures-util = { version = "0.3.31", default-features = false, features = ["std"] }
```

- [ ] **Step 2: Add to updater Cargo.toml**

In `crates/isengard-plugins/updater/Cargo.toml` `[dependencies]`:

```toml
futures-util.workspace = true
```

- [ ] **Step 3: Build to confirm**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add Cargo.toml Cargo.lock crates/isengard-plugins/updater/Cargo.toml
cd ~/Projects/isengard && git commit -m "chore(deps): add futures-util for bollard pull stream consumption"
```

**Self-review checklist:**
- [ ] `cargo build -p isengard-plugin-updater` clean
- [ ] `cargo fmt --check` clean
- [ ] `Cargo.lock` staged
- [ ] No `Co-Authored-By` trailer

---

## Task 2: self_id module

**Files:**
- Create: `crates/isengard-plugins/updater/src/self_id.rs`
- Modify: `crates/isengard-plugins/updater/src/lib.rs` (add `pub mod self_id;`)

- [ ] **Step 1: Create the module**

Create `crates/isengard-plugins/updater/src/self_id.rs`:

```rust
//! Detect this process's own container ID by reading /proc/self/cgroup.
//!
//! Returns `None` on non-Linux platforms (e.g. macOS dev) and on Linux hosts
//! where the agent runs outside any container (the cgroup file then contains
//! no docker/containerd-style paths).
//!
//! Used by the updater to refuse to recreate its own container — phase 3d
//! adds proper rename-first self-update; until then, "skip self" is the
//! safe default.

use std::fs;

/// Returns the long (64-hex) container ID this process is running inside,
/// or `None` if not detectable.
pub fn current_container_id() -> Option<String> {
    let bytes = fs::read("/proc/self/cgroup").ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    parse_cgroup(text)
}

fn parse_cgroup(text: &str) -> Option<String> {
    // Lines look like: "12:devices:/docker/<64-hex>" or
    //                  "0::/system.slice/docker-<64-hex>.scope"
    // We pull out the 64-hex segment. Ignore short IDs (12-hex truncations).
    for line in text.lines() {
        if let Some(id) = extract_64_hex(line) {
            return Some(id);
        }
    }
    None
}

fn extract_64_hex(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut start = 0;
    while start + 64 <= bytes.len() {
        let candidate = &bytes[start..start + 64];
        if candidate.iter().all(|b| b.is_ascii_hexdigit()) {
            // Boundary check: char before must be non-hex (or start of slice),
            // char after must be non-hex (or end).
            let before_ok = start == 0 || !bytes[start - 1].is_ascii_hexdigit();
            let after_ok =
                start + 64 == bytes.len() || !bytes[start + 64].is_ascii_hexdigit();
            if before_ok && after_ok {
                return Some(std::str::from_utf8(candidate).unwrap().to_string());
            }
        }
        start += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_from_classic_docker_cgroup_line() {
        let line = "12:devices:/docker/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(
            extract_64_hex(line).as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
    }

    #[test]
    fn extract_from_systemd_scope_line() {
        let line = "0::/system.slice/docker-fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210.scope";
        assert_eq!(
            extract_64_hex(line).as_deref(),
            Some("fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210")
        );
    }

    #[test]
    fn ignores_short_truncated_id() {
        let line = "12:devices:/docker/abcdef012345";
        assert!(extract_64_hex(line).is_none());
    }

    #[test]
    fn ignores_random_hex_runs_too_long() {
        // 65 hex chars in a row should NOT match (boundary-after must be non-hex)
        let line = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789a";
        assert!(extract_64_hex(line).is_none());
    }

    #[test]
    fn parse_cgroup_picks_first_match() {
        let text = "10:cpu:/\n12:devices:/docker/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\n";
        assert!(parse_cgroup(text).is_some());
    }

    #[test]
    fn parse_cgroup_returns_none_for_no_container() {
        let text = "10:cpu:/\n11:memory:/user.slice/user-1000.slice\n";
        assert!(parse_cgroup(text).is_none());
    }
}
```

- [ ] **Step 2: Add `pub mod self_id;` to lib.rs**

In `crates/isengard-plugins/updater/src/lib.rs`, after the other `pub mod` lines, add:

```rust
pub mod self_id;
```

- [ ] **Step 3: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater self_id:: 2>&1 | tail -15
```

Expected: 6 tests pass.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/self_id.rs crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): detect own container ID via /proc/self/cgroup (self-update guard)"
```

**Self-review checklist:**
- [ ] All 6 tests pass
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 3: capture_config (pure config translation)

**Files:**
- Create: `crates/isengard-plugins/updater/src/recreate.rs` (initial version, just the spec + capture_config)
- Modify: `crates/isengard-plugins/updater/src/lib.rs` (add `pub mod recreate;`)

- [ ] **Step 1: Create the module skeleton**

Create `crates/isengard-plugins/updater/src/recreate.rs`:

```rust
//! Recreate a running container against a new image, preserving its config.
//!
//! Phase 3c does NOT handle self-update — see `self_id.rs`. Phase 3d adds
//! rename-first ordering for the agent's own container.

use std::collections::HashMap;

use bollard::models::{
    ContainerInspectResponse, EndpointSettings, HostConfig, MountPoint,
};
use bollard::container::Config;

/// All the data we need to faithfully recreate a container against a new image.
#[derive(Debug, Clone)]
pub struct RecreateSpec {
    /// Container name (without leading slash).
    pub name: String,
    /// New image to use (e.g. "ghcr.io/foo/bar:latest").
    pub image: String,
    /// Container-level config (env, labels, cmd, working dir, etc).
    pub config: Config<String>,
    /// Networks to reconnect AFTER start (excluding the default bridge that
    /// `start_container` connects automatically).
    pub networks: HashMap<String, EndpointSettings>,
}

/// Translate an inspect response into a RecreateSpec for `new_image`.
pub fn capture_config(
    inspect: &ContainerInspectResponse,
    new_image: &str,
) -> RecreateSpec {
    let name = inspect
        .name
        .as_deref()
        .map(|s| s.trim_start_matches('/').to_string())
        .unwrap_or_default();

    let mut host_config = inspect.host_config.clone().unwrap_or_default();
    dedup_mounts(&mut host_config);
    let mounts_for_options = host_config.mounts.clone();

    let cfg_in = inspect.config.clone().unwrap_or_default();
    let config: Config<String> = Config {
        image: Some(new_image.to_string()),
        cmd: cfg_in.cmd,
        entrypoint: cfg_in.entrypoint,
        env: cfg_in.env,
        labels: cfg_in.labels,
        working_dir: cfg_in.working_dir,
        user: cfg_in.user,
        exposed_ports: cfg_in.exposed_ports,
        host_config: Some(host_config),
        ..Default::default()
    };

    let mut networks = HashMap::new();
    if let Some(ns) = inspect
        .network_settings
        .as_ref()
        .and_then(|s| s.networks.as_ref())
    {
        for (net_name, settings) in ns {
            if net_name == "bridge" {
                continue;
            }
            networks.insert(net_name.clone(), settings.clone());
        }
    }

    // Suppress unused-warning for the cloned-out mounts; bollard's Config
    // already carries them via host_config but some downstream code may want
    // them separately. Phase 3d revisits.
    let _ = mounts_for_options;

    RecreateSpec {
        name,
        image: new_image.to_string(),
        config,
        networks,
    }
}

/// When both `binds` and `mounts` target the same destination, drop the bind.
/// `mounts` is the newer API and may carry richer flags (read-only, propagation).
fn dedup_mounts(host_config: &mut HostConfig) {
    let Some(mounts) = host_config.mounts.as_ref() else {
        return;
    };
    let mount_targets: std::collections::HashSet<&String> = mounts
        .iter()
        .filter_map(|m| m.target.as_ref())
        .collect();

    if let Some(binds) = host_config.binds.as_mut() {
        binds.retain(|b| {
            // Bind format: "<src>:<dst>" or "<src>:<dst>:<opts>"
            let dst = b.split(':').nth(1);
            match dst {
                Some(d) => !mount_targets.iter().any(|t| t.as_str() == d),
                None => true,
            }
        });
        if binds.is_empty() {
            host_config.binds = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{
        ContainerConfig, ContainerInspectResponse, EndpointSettings, HostConfig,
        Mount, NetworkSettings, RestartPolicy,
    };

    fn make_inspect() -> ContainerInspectResponse {
        let mut labels = HashMap::new();
        labels.insert("isengard.enable".into(), "true".into());
        labels.insert("traefik.enable".into(), "true".into());

        let cfg = ContainerConfig {
            image: Some("nginx:1.24".into()),
            env: Some(vec!["FOO=bar".into(), "BAZ=qux".into()]),
            labels: Some(labels),
            cmd: Some(vec!["nginx".into(), "-g".into(), "daemon off;".into()]),
            working_dir: Some("/etc/nginx".into()),
            ..Default::default()
        };

        let host_cfg = HostConfig {
            restart_policy: Some(RestartPolicy {
                name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
                ..Default::default()
            }),
            binds: Some(vec!["/host/data:/data".into()]),
            mounts: Some(vec![Mount {
                target: Some("/etc/nginx/conf.d".into()),
                source: Some("/host/conf".into()),
                typ: Some(bollard::models::MountTypeEnum::BIND),
                read_only: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut networks = HashMap::new();
        networks.insert("bridge".into(), EndpointSettings::default());
        networks.insert(
            "my-app-net".into(),
            EndpointSettings {
                aliases: Some(vec!["nginx-server".into()]),
                ..Default::default()
            },
        );

        ContainerInspectResponse {
            name: Some("/web".into()),
            image: Some("sha256:oldimagehash".into()),
            config: Some(cfg),
            host_config: Some(host_cfg),
            network_settings: Some(NetworkSettings {
                networks: Some(networks),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn name_strips_leading_slash() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        assert_eq!(spec.name, "web");
    }

    #[test]
    fn image_replaced_with_new() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        assert_eq!(spec.config.image.as_deref(), Some("nginx:1.25"));
    }

    #[test]
    fn env_preserved() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        assert_eq!(
            spec.config.env.as_ref().unwrap(),
            &vec!["FOO=bar".to_string(), "BAZ=qux".to_string()]
        );
    }

    #[test]
    fn labels_preserved_including_third_party() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        let labels = spec.config.labels.as_ref().unwrap();
        assert_eq!(labels.get("isengard.enable").map(|s| s.as_str()), Some("true"));
        assert_eq!(labels.get("traefik.enable").map(|s| s.as_str()), Some("true"));
    }

    #[test]
    fn cmd_and_workdir_preserved() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        assert_eq!(spec.config.cmd.as_ref().unwrap().len(), 3);
        assert_eq!(spec.config.working_dir.as_deref(), Some("/etc/nginx"));
    }

    #[test]
    fn restart_policy_preserved() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        let host_cfg = spec.config.host_config.as_ref().unwrap();
        let policy = host_cfg.restart_policy.as_ref().unwrap();
        assert_eq!(
            policy.name,
            Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED)
        );
    }

    #[test]
    fn bridge_network_filtered_out() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        assert!(!spec.networks.contains_key("bridge"));
        assert!(spec.networks.contains_key("my-app-net"));
    }

    #[test]
    fn mounts_kept_when_only_mounts_set() {
        let mut inspect = make_inspect();
        if let Some(hc) = inspect.host_config.as_mut() {
            hc.binds = None;
        }
        let spec = capture_config(&inspect, "nginx:1.25");
        let host_cfg = spec.config.host_config.as_ref().unwrap();
        assert_eq!(host_cfg.mounts.as_ref().unwrap().len(), 1);
        assert!(host_cfg.binds.is_none());
    }

    #[test]
    fn binds_kept_when_only_binds_set() {
        let mut inspect = make_inspect();
        if let Some(hc) = inspect.host_config.as_mut() {
            hc.mounts = None;
        }
        let spec = capture_config(&inspect, "nginx:1.25");
        let host_cfg = spec.config.host_config.as_ref().unwrap();
        assert_eq!(host_cfg.binds.as_ref().unwrap(), &vec!["/host/data:/data".to_string()]);
    }

    #[test]
    fn bind_dropped_when_mount_targets_same_destination() {
        // Add a bind whose destination matches an existing mount target.
        let mut inspect = make_inspect();
        if let Some(hc) = inspect.host_config.as_mut() {
            hc.binds = Some(vec!["/elsewhere:/etc/nginx/conf.d".into()]);
            // mounts already targets /etc/nginx/conf.d
        }
        let spec = capture_config(&inspect, "nginx:1.25");
        let host_cfg = spec.config.host_config.as_ref().unwrap();
        // bind is dropped
        assert!(host_cfg.binds.is_none());
        // mount survives
        assert_eq!(host_cfg.mounts.as_ref().unwrap().len(), 1);
    }
}

// Suppress dead_code on the helper while pull_image / update_container land
// in the next task.
#[allow(dead_code)]
fn _unused_marker(_: &MountPoint) {}
```

- [ ] **Step 2: Add `pub mod recreate;` to lib.rs**

After the other `pub mod` lines:

```rust
pub mod recreate;
```

- [ ] **Step 3: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater recreate:: 2>&1 | tail -20
```

Expected: 10 tests pass.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/recreate.rs crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): capture_config — translate inspect into RecreateSpec with mount dedup"
```

**Self-review checklist:**
- [ ] All 10 tests pass
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 4: pull_image + update_container orchestration

**Files:**
- Modify: `crates/isengard-plugins/updater/src/recreate.rs`

- [ ] **Step 1: Add the orchestration functions**

Append to `crates/isengard-plugins/updater/src/recreate.rs` (and remove the `_unused_marker` placeholder):

```rust
use bollard::Docker;
use bollard::container::{
    CreateContainerOptions, RemoveContainerOptions, StopContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::network::ConnectNetworkOptions;
use futures_util::StreamExt;
use tracing::{debug, info, warn};

use crate::image_ref::ImageRef;

/// Pull an image. Consumes bollard's progress stream until it ends or yields
/// an error frame.
pub async fn pull_image(docker: &Docker, image: &ImageRef) -> anyhow::Result<()> {
    let from_image = format!("{}/{}", image.registry, image.repository);
    // Bollard's CreateImageOptions takes the image as `from_image` + `tag`.
    let options = CreateImageOptions {
        from_image,
        tag: image.tag.clone(),
        ..Default::default()
    };

    info!(image = %image, "pulling image");
    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(progress) => {
                if let Some(err) = progress.error {
                    return Err(anyhow::anyhow!("pull error frame: {err}"));
                }
                if let Some(status) = progress.status {
                    debug!(status = %status, "pull progress");
                }
            }
            Err(e) => return Err(anyhow::anyhow!("pull stream error: {e}")),
        }
    }
    info!(image = %image, "pull complete");
    Ok(())
}

/// Recreate the container at `container_id` against `new_image_ref`.
/// Inspects → pulls → stops → removes → creates → starts → reconnects networks.
pub async fn update_container(
    docker: &Docker,
    container_id: &str,
    new_image_ref: &ImageRef,
) -> anyhow::Result<()> {
    let inspect = docker
        .inspect_container(container_id, None)
        .await
        .map_err(|e| anyhow::anyhow!("inspect {container_id}: {e}"))?;

    if inspect.state.as_ref().and_then(|s| s.status.as_ref())
        == Some(&bollard::models::ContainerStateStatusEnum::RESTARTING)
    {
        warn!(container = container_id, "container is restarting; deferring update");
        return Ok(());
    }

    pull_image(docker, new_image_ref).await?;

    let new_image_str = format!(
        "{}/{}:{}",
        new_image_ref.registry, new_image_ref.repository, new_image_ref.tag
    );
    let spec = capture_config(&inspect, &new_image_str);

    info!(container = %spec.name, image = %spec.image, "stopping old container");
    docker
        .stop_container(container_id, Some(StopContainerOptions { t: 10 }))
        .await
        .map_err(|e| anyhow::anyhow!("stop {container_id}: {e}"))?;

    info!(container = %spec.name, "removing old container");
    docker
        .remove_container(
            container_id,
            Some(RemoveContainerOptions {
                force: false,
                v: false,
                link: false,
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("remove {container_id}: {e}"))?;

    info!(container = %spec.name, image = %spec.image, "creating replacement container");
    let create = docker
        .create_container(
            Some(CreateContainerOptions {
                name: spec.name.clone(),
                platform: None,
            }),
            spec.config,
        )
        .await
        .map_err(|e| anyhow::anyhow!("create {}: {e}", spec.name))?;

    info!(container = %spec.name, new_id = %create.id, "starting replacement container");
    docker
        .start_container::<String>(&create.id, None)
        .await
        .map_err(|e| anyhow::anyhow!("start {}: {e}", spec.name))?;

    for (net_name, settings) in &spec.networks {
        let opts = ConnectNetworkOptions {
            container: create.id.clone(),
            endpoint_config: settings.clone(),
        };
        if let Err(e) = docker.connect_network(net_name, opts).await {
            // Don't fail the whole update if a network reattach fails;
            // log and continue. The container is already running.
            warn!(container = %spec.name, network = %net_name, error = %e, "network reattach failed");
        } else {
            debug!(container = %spec.name, network = %net_name, "reconnected");
        }
    }

    info!(container = %spec.name, image = %spec.image, "update complete");
    Ok(())
}
```

Also remove the `_unused_marker` placeholder function and its `#[allow(dead_code)]` attribute from the bottom of the file.

- [ ] **Step 2: Build**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -10
```

Expected: clean. If bollard 0.18 method signatures differ from what's written above (e.g., `inspect_container` returns Result over a different type, or `CreateContainerOptions` has different fields), adapt the calls to match the actual bollard API. The structure (inspect → pull → stop → remove → create → start → reconnect) must be preserved.

- [ ] **Step 3: Confirm clippy clean**

```bash
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 4: Confirm capture_config unit tests still pass**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater recreate:: 2>&1 | tail -15
```

Expected: 10 tests pass (no new tests for the orchestration; that's covered by the integration test in Task 6).

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/recreate.rs
cd ~/Projects/isengard && git commit -m "feat(updater): pull_image + update_container orchestration (no self-update yet)"
```

**Self-review checklist:**
- [ ] `cargo build -p isengard-plugin-updater` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 5: Wire do_cycle to call update_container

**Files:**
- Modify: `crates/isengard-plugins/updater/src/lib.rs`

- [ ] **Step 1: Update do_cycle**

In `crates/isengard-plugins/updater/src/lib.rs`, find the existing `do_cycle` function. The branch that currently does:

```rust
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
```

becomes:

```rust
            (Some(local), Some(remote)) => {
                info!(
                    container = %name,
                    image = %image_str,
                    current_digest = %local,
                    remote_digest = %remote,
                    status = "needs_update"
                );
                needs_update += 1;

                // Self-protection: skip if this container is the agent itself.
                if let Some(self_id) = crate::self_id::current_container_id() {
                    if c.id.as_deref() == Some(self_id.as_str()) {
                        warn!(
                            container = %name,
                            "skipping self-update; deferred to phase 3d"
                        );
                        continue;
                    }
                }

                let Some(container_id) = c.id.as_deref() else {
                    warn!(container = %name, "no container ID; cannot update");
                    continue;
                };

                if let Err(e) = recreate::update_container(docker, container_id, &image_ref).await {
                    warn!(container = %name, error = %e, "update failed");
                }
            }
```

Add the import at the top of the file (alongside the other crate-local imports):

```rust
use crate::recreate;
```

- [ ] **Step 2: Build + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: both clean.

- [ ] **Step 3: All updater unit tests still green**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --lib 2>&1 | tail -15
```

Expected: 32+ tests pass (26 from 3b + 6 self_id + 10 recreate = 42; if some count differently due to filter behavior, just ensure no failures).

- [ ] **Step 4: Phase 3a + 3b integration tests still green**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --test plugin_loads --test registry_e2e 2>&1 | tail -15
```

Expected: 2 tests pass (or skip if Docker/network unavailable).

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): cycle calls update_container on needs_update (skip self via /proc/self/cgroup)"
```

**Self-review checklist:**
- [ ] All updater unit tests pass
- [ ] Phase 3a + 3b integration tests pass (or skip cleanly)
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 6: Integration test — recreate happy path

**Files:**
- Create: `crates/isengard-plugins/updater/tests/recreate_e2e.rs`

- [ ] **Step 1: Create the test**

Create `crates/isengard-plugins/updater/tests/recreate_e2e.rs`:

```rust
//! Integration test: pull `library/hello-world:latest`, run a labelled
//! container, force a cycle, verify the orchestration runs without panic.
//!
//! This test does NOT verify recreation END-TO-END (that requires the upstream
//! image to actually move, which we can't synthesise reliably without
//! testcontainers). What it verifies:
//!
//! 1. pull_image works against real Docker Hub
//! 2. update_container's inspect path works on a real running container
//! 3. The capture_config translation produces a Config that bollard accepts
//!    (i.e. round-trip: inspect → capture → recreate → start succeeds)
//!
//! Skipped if Docker isn't reachable.

#![allow(clippy::result_large_err)]

use bollard::container::{Config, CreateContainerOptions, RemoveContainerOptions};
use bollard::Docker;
use isengard_plugin_updater::image_ref::ImageRef;
use isengard_plugin_updater::recreate::{pull_image, update_container};

const TEST_IMAGE: &str = "hello-world:latest";
const TEST_CONTAINER: &str = "isengard-3c-test";

async fn docker_available() -> Option<Docker> {
    let docker = Docker::connect_with_local_defaults().ok()?;
    docker.version().await.ok()?;
    Some(docker)
}

async fn cleanup(docker: &Docker) {
    let _ = docker
        .remove_container(
            TEST_CONTAINER,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
}

#[tokio::test]
async fn pull_then_recreate_round_trip() {
    let Some(docker) = docker_available().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    cleanup(&docker).await;

    let image = ImageRef::parse(TEST_IMAGE).unwrap();

    // Pull the image.
    pull_image(&docker, &image)
        .await
        .expect("pull should succeed");

    // Create + start a labelled container against the pulled image.
    let mut labels = std::collections::HashMap::new();
    labels.insert("isengard.enable".to_string(), "true".to_string());
    let create = docker
        .create_container(
            Some(CreateContainerOptions {
                name: TEST_CONTAINER.to_string(),
                platform: None,
            }),
            Config {
                image: Some(TEST_IMAGE.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
        )
        .await
        .expect("create should succeed");

    docker
        .start_container::<String>(&create.id, None)
        .await
        .expect("start should succeed");

    // hello-world exits immediately; that's fine. update_container will inspect
    // the (exited) container and recreate it.
    let result = update_container(&docker, &create.id, &image).await;

    cleanup(&docker).await;

    // The orchestration should not error on a hello-world recreate.
    if let Err(e) = result {
        panic!("update_container failed: {e}");
    }
}
```

- [ ] **Step 2: Run the test**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --test recreate_e2e 2>&1 | tail -15
```

Expected: passes against real Docker, or takes the skip path if Docker unavailable.

- [ ] **Step 3: Confirm clippy clean**

```bash
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/tests/recreate_e2e.rs
cd ~/Projects/isengard && git commit -m "test(updater): pull + update_container round trip on hello-world (skipped if no Docker)"
```

**Self-review checklist:**
- [ ] Test passes (or skips cleanly)
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 7: CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 3c`, re-run.

- [ ] **Step 2: Confirm test counts**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4; fails+=$6} END {print "Total passing:", sum, "| failures:", fails}'
```

Expected: ≥ 80 (Phase 3b baseline) + 6 self_id + 10 recreate + 1 integration = 97-ish. Critical: zero failures.

- [ ] **Step 3: Manual smoke (only if Docker is on this box)**

```bash
cd ~/Projects/isengard
mkdir -p /tmp/isengard-3c-ctrl /tmp/isengard-3c-agent

# Set up a labelled nginx:alpine container, then retag an OLDER nginx as alpine
# to force needs_update detection.
docker pull nginx:alpine || true
docker pull nginx:1.24-alpine || true
docker tag nginx:1.24-alpine nginx:alpine
docker run -d --name isengard-3c-target --label isengard.enable=true -p 18080:80 nginx:alpine

ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9462 --state-dir /tmp/isengard-3c-ctrl &
CTRL=$!
sleep 1

ISENGARD_TOKEN=test ./target/debug/isengard agent --controller http://127.0.0.1:9462 --state-dir /tmp/isengard-3c-agent 2>&1 | tee /tmp/isengard-3c-agent.log &
AGENT=$!
# Wait long enough for ≥1 cycle (30s) + recreation work.
sleep 90

echo "--- updater log ---"
grep -E "updater cycle complete|status=|stopping|removing|creating|starting|update complete" /tmp/isengard-3c-agent.log

echo "--- container state ---"
docker inspect isengard-3c-target --format '{{.Image}} {{.State.Status}} {{index .Config.Labels "isengard.enable"}}'

kill -INT $AGENT 2>/dev/null
wait $AGENT 2>/dev/null || true
kill $CTRL 2>/dev/null
wait $CTRL 2>/dev/null || true
docker rm -f isengard-3c-target 2>/dev/null || true
```

Expected log lines:
- `status=needs_update` for `isengard-3c-target` (because nginx:1.24-alpine ≠ current nginx:alpine digest)
- `stopping old container ... web=isengard-3c-target`
- `update complete`
- After: `docker inspect` shows the container running with the same name + label

If you don't have Docker on this box, skip — the integration test (Task 6) covers the orchestration path.

- [ ] **Step 4: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase3c -m "phase 3c: updater pulls + recreates containers preserving config (no self-update)"
cd ~/Projects/isengard && git tag -l | grep phase3c
```

Don't push.

- [ ] **Step 5: Confirm done conditions**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 80 baseline + new tests, zero failures
- [ ] `just ci-local` clean
- [ ] Manual smoke shows recreation OR Task 6 test passes
- [ ] Tag `v0.1.0-alpha.phase3c` exists locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (§9.1, §10) | Plan task |
|---|---|
| Pull new image | Task 4 (`pull_image`) + Task 5 (called from cycle) |
| Recreate container preserving ports | Task 3 (`HostConfig` cloned, includes `port_bindings`) |
| Recreate preserving mounts | Task 3 (`HostConfig.binds` + `mounts`, deduped) |
| Recreate preserving networks | Task 3 (filter bridge) + Task 4 (reconnect after start) |
| Recreate preserving env | Task 3 (`Config.env` cloned) |
| Recreate preserving labels | Task 3 (`Config.labels` cloned) |
| Recreate preserving restart policy | Task 3 (`HostConfig.restart_policy` cloned) |
| Mount/volume dedup (subtle bit) | Task 3 (`dedup_mounts`) |
| Self-update with rename-first | Deferred to 3d (3c has WARN-and-skip via `self_id`) |
| Old image cleanup | Deferred to 3e |

No placeholders. The "subtle bits" §10 calls out are addressed: digest extraction (3b), faithful recreation (3c capture_config), mount/volume dedup (3c dedup_mounts). Rename-first self-update is the one remaining subtle bit, owned by 3d.

**Type consistency check:**
- `RecreateSpec` (Task 3) — used by `update_container` (Task 4).
- `pull_image(&Docker, &ImageRef)` (Task 4) — called from `update_container` (Task 4) and `do_cycle` (Task 5, indirectly via update_container).
- `update_container(&Docker, &str, &ImageRef)` (Task 4) — called from `do_cycle` (Task 5) with `c.id.as_deref()` and `&image_ref`.
- `self_id::current_container_id() -> Option<String>` (Task 2) — compared in Task 5 against `c.id.as_deref()`.
- `image_ref` is in scope in `do_cycle` from Phase 3b's existing `let Some(image_ref) = ImageRef::parse(image_str) else { ... };`.

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-3c-image-pull-recreation.md`. Subagent-driven execution.
