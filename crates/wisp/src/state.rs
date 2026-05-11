//! On-disk state directory for wisp containers.
//!
//! Layout, per spec section "State directory layout":
//!
//! ```text
//! <state_dir>/
//!   containers/
//!     <id>/
//!       state.json    # serialised ContainerHandle
//!       config.json   # cached copy of the bundle's config.json
//!       bundle.path   # absolute path to bundle dir (so state.json stays portable)
//!       log           # combined wisp-internal log for this container
//! ```
//!
//! Writes are atomic: serialise to `state.json.tmp`, fsync, rename onto
//! `state.json`. Reads tolerate concurrent writers, so a half-written
//! `state.json` looks like a parse failure to the reader. `list` swallows
//! per-entry parse failures and emits a tracing warning so a single corrupt
//! entry never poisons the whole listing.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::{Result, WispError};
use crate::network_spec::{NetworkAttachmentRecord, NetworkSpec};

/// State a container is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContainerState {
    /// State directory allocated, spec validated, cgroup created. No process yet.
    Created,
    /// PID 1 is alive. `pid` should be `Some`.
    Running,
    /// PID 1 has exited. State-dir + cgroup not yet cleaned.
    Stopped,
}

/// Persisted handle for one container.
///
/// `bundle` is the absolute path to the bundle directory the operator passed
/// to `Runtime::create`. We keep it on disk because the operator may invoke
/// `wisp` from a different cwd later (e.g. via `wisp ps`), and the bundle
/// path needs to round-trip.
///
/// `network_specs` is the operator-supplied input (set by
/// `Runtime::create_with_network` / `create_with_networks`);
/// `network_attachments` is what the runtime actually attached at start
/// time. Both default to empty vecs for the no-network path so existing
/// `state.json` files (Phase 0.1) deserialize unchanged. They also accept
/// the legacy singular fields (`network_spec` / `network_attachment`) via
/// custom deserialize so state files written before Phase 0.18 round-trip
/// into the new shape transparently.
///
/// `stdout_log_path` / `stderr_log_path` point at the per-container
/// log files written by [`crate::lifecycle::start_container`] (the
/// child's stdout / stderr are dup2'd onto these files before exec).
/// Both default to `None` for back-compat with Phase 0.1 / 0.3 state
/// files; `start_container` populates them on every new run.
///
/// `exit_code` is set once the per-container reaper has reaped PID 1.
/// Encoding mirrors shell convention: positive `0..=255` for a clean
/// `_exit(N)`; negative `-N` when PID 1 was killed by signal `N` (so
/// `Some(-9)` is SIGKILL, `Some(-15)` is SIGTERM). `None` until reap
/// lands on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerHandle {
    pub id: String,
    pub bundle: PathBuf,
    pub state: ContainerState,
    pub pid: Option<u32>,
    pub created_at: SystemTime,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_specs: Vec<NetworkSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_attachments: Vec<NetworkAttachmentRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_log_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_log_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// Manual Deserialize: accepts both the new `network_specs` /
/// `network_attachments` plural form AND the legacy
/// `network_spec` / `network_attachment` singular form written by
/// pre-0.18 agents. On collision the plural form wins.
impl<'de> Deserialize<'de> for ContainerHandle {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            id: String,
            bundle: PathBuf,
            state: ContainerState,
            pid: Option<u32>,
            created_at: SystemTime,
            #[serde(default)]
            network_spec: Option<NetworkSpec>,
            #[serde(default)]
            network_specs: Option<Vec<NetworkSpec>>,
            #[serde(default)]
            network_attachment: Option<NetworkAttachmentRecord>,
            #[serde(default)]
            network_attachments: Option<Vec<NetworkAttachmentRecord>>,
            #[serde(default)]
            stdout_log_path: Option<PathBuf>,
            #[serde(default)]
            stderr_log_path: Option<PathBuf>,
            #[serde(default)]
            exit_code: Option<i32>,
        }

        let raw = Raw::deserialize(deserializer)?;
        // Plural wins. Fall back to singular when plural is absent.
        let network_specs = raw
            .network_specs
            .unwrap_or_else(|| raw.network_spec.into_iter().collect());
        let network_attachments = raw
            .network_attachments
            .unwrap_or_else(|| raw.network_attachment.into_iter().collect());
        Ok(ContainerHandle {
            id: raw.id,
            bundle: raw.bundle,
            state: raw.state,
            pid: raw.pid,
            created_at: raw.created_at,
            network_specs,
            network_attachments,
            stdout_log_path: raw.stdout_log_path,
            stderr_log_path: raw.stderr_log_path,
            exit_code: raw.exit_code,
        })
    }
}

/// Compute the per-container directory: `<state_dir>/containers/<id>/`.
fn container_dir(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join("containers").join(id)
}

/// Serialise `h` to `<state_dir>/containers/<h.id>/state.json` atomically.
///
/// The container directory is created if it does not exist. The write is
/// atomic via write-then-rename: we write to `state.json.tmp` (fsync the
/// data), then `rename` onto `state.json`. A reader that catches us mid-write
/// will see either the old contents or the new contents, never a torn file.
pub fn write(state_dir: &Path, h: &ContainerHandle) -> Result<()> {
    let dir = container_dir(state_dir, &h.id);
    fs::create_dir_all(&dir)?;

    let json = serde_json::to_vec_pretty(h)?;
    let target = dir.join("state.json");
    let tmp = dir.join("state.json.tmp");

    {
        let mut f = File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &target)?;
    Ok(())
}

/// Read `<state_dir>/containers/<id>/state.json` and deserialise.
pub fn read(state_dir: &Path, id: &str) -> Result<ContainerHandle> {
    let path = container_dir(state_dir, id).join("state.json");
    let data = fs::read(&path)?;
    let h: ContainerHandle = serde_json::from_slice(&data)?;
    Ok(h)
}

/// List every container handle under `<state_dir>/containers/`.
///
/// If a per-container `state.json` is missing, unreadable, or fails to parse
/// we log a warning and skip the entry. This keeps `wisp ps` usable in the
/// face of a single corrupt container directory; the caller can recover
/// individual broken entries with `wisp delete --force`.
pub fn list(state_dir: &Path) -> Result<Vec<ContainerHandle>> {
    let containers_dir = state_dir.join("containers");
    if !containers_dir.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for entry in fs::read_dir(&containers_dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to read containers/ directory entry, skipping"
                );
                continue;
            }
        };
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        match read(state_dir, &id) {
            Ok(h) => out.push(h),
            Err(err) => {
                tracing::warn!(
                    container_id = %id,
                    error = %err,
                    "failed to parse state.json for container, skipping"
                );
            }
        }
    }

    Ok(out)
}

/// Remove `<state_dir>/containers/<id>/` and everything in it.
///
/// Idempotent: removing a container that does not exist is a no-op. Errors
/// only on filesystem failures we cannot interpret as "already gone".
pub fn remove(state_dir: &Path, id: &str) -> Result<()> {
    let dir = container_dir(state_dir, id);
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(WispError::Io(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use tempfile::TempDir;

    fn make_handle(id: &str) -> ContainerHandle {
        ContainerHandle {
            id: id.to_string(),
            bundle: PathBuf::from(format!("/tmp/bundle-{id}")),
            state: ContainerState::Created,
            pid: None,
            // SystemTime serialised as a (sec, nsec) pair; pin to a known
            // offset so round-trip equality is deterministic.
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            network_specs: Vec::new(),
            network_attachments: Vec::new(),
            stdout_log_path: None,
            stderr_log_path: None,
            exit_code: None,
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = TempDir::new().unwrap();
        let h = make_handle("alpha");
        write(dir.path(), &h).unwrap();
        let got = read(dir.path(), "alpha").unwrap();
        assert_eq!(got, h);
    }

    #[test]
    fn list_empty_returns_empty() {
        let dir = TempDir::new().unwrap();
        let got = list(dir.path()).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn list_one_entry() {
        let dir = TempDir::new().unwrap();
        let h = make_handle("only");
        write(dir.path(), &h).unwrap();

        let got = list(dir.path()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], h);
    }

    #[test]
    fn list_many_entries() {
        let dir = TempDir::new().unwrap();
        let ids = ["one", "two", "three"];
        for id in ids.iter() {
            write(dir.path(), &make_handle(id)).unwrap();
        }

        let got = list(dir.path()).unwrap();
        assert_eq!(got.len(), 3);

        let mut got_ids: Vec<_> = got.iter().map(|h| h.id.clone()).collect();
        got_ids.sort();
        let mut want_ids: Vec<_> = ids.iter().map(|s| s.to_string()).collect();
        want_ids.sort();
        assert_eq!(got_ids, want_ids);
    }

    #[test]
    fn list_skips_corrupt_entry_without_failing() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), &make_handle("good")).unwrap();

        // Manually create a busted entry: directory present, state.json
        // truncated to an obviously-invalid prefix.
        let bad_dir = dir.path().join("containers").join("bad");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join("state.json"), b"{ this").unwrap();

        let got = list(dir.path()).unwrap();
        assert_eq!(got.len(), 1, "corrupt entry should be skipped");
        assert_eq!(got[0].id, "good");
    }

    #[test]
    fn list_skips_entry_missing_state_json() {
        let dir = TempDir::new().unwrap();
        // Container directory exists but never had its state.json written.
        fs::create_dir_all(dir.path().join("containers").join("orphan")).unwrap();

        let got = list(dir.path()).unwrap();
        assert!(
            got.is_empty(),
            "orphan dir without state.json should be skipped"
        );
    }

    #[test]
    fn remove_cleans_directory() {
        let dir = TempDir::new().unwrap();
        let h = make_handle("victim");
        write(dir.path(), &h).unwrap();
        let cdir = container_dir(dir.path(), "victim");
        assert!(cdir.exists());

        remove(dir.path(), "victim").unwrap();
        assert!(!cdir.exists(), "container dir should be gone");
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = TempDir::new().unwrap();
        // Never written: first remove must succeed.
        remove(dir.path(), "ghost").unwrap();
        // Second remove on a non-existent id must also succeed.
        remove(dir.path(), "ghost").unwrap();
    }

    #[test]
    fn handle_with_network_specs_round_trips() {
        use crate::network_spec::{NetworkSpec, PortProtocol, PortPublish, ResolvSource};
        let dir = TempDir::new().unwrap();
        let mut h = make_handle("net");
        h.network_specs = vec![NetworkSpec {
            network_name: "app".into(),
            ports: vec![PortPublish::v4_any(18080, 80, PortProtocol::Tcp)],
            resolv_source: ResolvSource::HostCopy,
        }];
        write(dir.path(), &h).unwrap();
        let back = read(dir.path(), "net").unwrap();
        assert_eq!(back.network_specs, h.network_specs);
        assert!(back.network_attachments.is_empty());
    }

    #[test]
    fn handle_with_multiple_network_specs_preserves_declaration_order() {
        use crate::network_spec::{NetworkSpec, PortProtocol, PortPublish, ResolvSource};
        let dir = TempDir::new().unwrap();
        let mut h = make_handle("multi");
        h.network_specs = vec![
            NetworkSpec {
                network_name: "isengard-proxy".into(),
                ports: vec![PortPublish::v4_any(80, 80, PortProtocol::Tcp)],
                resolv_source: ResolvSource::HostCopy,
            },
            NetworkSpec {
                network_name: "servarr".into(),
                ports: vec![],
                resolv_source: ResolvSource::HostCopy,
            },
        ];
        write(dir.path(), &h).unwrap();
        let back = read(dir.path(), "multi").unwrap();
        assert_eq!(back.network_specs.len(), 2);
        // Declaration order must round-trip: it picks the primary
        // network (eth0) for the attached container.
        assert_eq!(back.network_specs[0].network_name, "isengard-proxy");
        assert_eq!(back.network_specs[1].network_name, "servarr");
    }

    #[test]
    fn legacy_state_json_without_network_fields_round_trips() {
        // Phase 0.1 wrote handles without network_spec / network_attachment.
        // Reading such a file must succeed (serde(default) on the new
        // optional fields). We hand-craft a JSON blob that omits both.
        let dir = TempDir::new().unwrap();
        let cdir = dir.path().join("containers/legacy");
        std::fs::create_dir_all(&cdir).unwrap();
        // Pin the SystemTime fields to the same shape serde emits.
        let json = r#"{
  "id": "legacy",
  "bundle": "/tmp/bundle-legacy",
  "state": "Created",
  "pid": null,
  "created_at": { "secs_since_epoch": 1700000000, "nanos_since_epoch": 0 }
}"#;
        std::fs::write(cdir.join("state.json"), json).unwrap();

        let h = read(dir.path(), "legacy").unwrap();
        assert_eq!(h.id, "legacy");
        assert!(h.network_specs.is_empty());
        assert!(h.network_attachments.is_empty());
    }

    #[test]
    fn legacy_singular_network_fields_migrate_into_plural() {
        // Pre-0.18 agents wrote `network_spec` / `network_attachment`
        // singular. Reading such a file must surface them as the first
        // entry of the new plural Vec fields.
        use std::net::Ipv4Addr;
        let dir = TempDir::new().unwrap();
        let cdir = dir.path().join("containers/old");
        std::fs::create_dir_all(&cdir).unwrap();
        let json = r#"{
  "id": "old",
  "bundle": "/tmp/bundle-old",
  "state": "Running",
  "pid": 42,
  "created_at": { "secs_since_epoch": 1700000000, "nanos_since_epoch": 0 },
  "network_spec": {
    "network_name": "isengard-proxy",
    "ports": []
  },
  "network_attachment": {
    "container_id": "old",
    "network_name": "isengard-proxy",
    "bridge": "wbr-isengard-proxy",
    "ipv4": "10.83.0.5",
    "veth_host": "wveth-h-abc123",
    "veth_container": "wveth-c-abc123",
    "ports": []
  }
}"#;
        std::fs::write(cdir.join("state.json"), json).unwrap();

        let h = read(dir.path(), "old").unwrap();
        assert_eq!(h.network_specs.len(), 1);
        assert_eq!(h.network_specs[0].network_name, "isengard-proxy");
        assert_eq!(h.network_attachments.len(), 1);
        assert_eq!(h.network_attachments[0].ipv4, Ipv4Addr::new(10, 83, 0, 5));
    }
}
