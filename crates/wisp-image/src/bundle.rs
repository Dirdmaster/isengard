//! Bundle synthesis: image -> OCI runtime bundle.
//!
//! A `BundleBuilder` materialises a `<bundle_dir>` that wisp-runtime can
//! `create`/`start`. Two outputs:
//!
//!   - `<bundle_dir>/rootfs/`: layered filesystem assembled from the
//!     image's layer blobs (delegated to `store::layer::assemble_rootfs`).
//!   - `<bundle_dir>/config.json`: an `oci_spec::runtime::Spec` derived
//!     from the image config plus operator-supplied overrides.
//!
//! The baseline Spec mirrors `crates/wisp/examples/busybox/config.json`:
//! five namespaces (PID, mount, UTS, IPC, network), the canonical mount
//! set (`/proc`, `/dev`, `/dev/pts`, `/dev/shm`, `/dev/mqueue`, `/sys`),
//! the runc-style default capabilities (`CAP_KILL`, `CAP_NET_BIND_SERVICE`),
//! the default masked / readonly proc paths, and `noNewPrivileges: true`.
//! User and cgroup namespaces are deliberately absent; both are deferred
//! to wisp 0.3 per `crates/wisp/src/spec.rs`'s validator.
//!
//! Override semantics (Docker-compatible):
//!
//!   - `args`: replaces the image config's `Cmd`.
//!   - `entrypoint`: replaces the image config's `Entrypoint`.
//!   - `env`: APPENDED to the image config's `Env` (override-after-image
//!     means the operator's late entry wins for `KEY=VALUE` duplicates,
//!     because container runtimes consume the last occurrence).
//!   - `cwd`: replaces the image config's `WorkingDir`.
//!   - `hostname`: replaces the default (first 12 chars of bundle dir's
//!     basename, matching Docker's container-id-as-hostname convention).
//!   - `mounts`: APPENDED to the baseline mount set.
//!   - `linux_resources`: replaces the baseline (no resources).
//!   - `capabilities`: REPLACES the baseline cap allow-list when
//!     `Some`. Otherwise the synthesised Spec keeps `CAP_KILL` +
//!     `CAP_NET_BIND_SERVICE` (the busybox demo allow-list).

use std::fs;
use std::path::{Path, PathBuf};

use std::collections::HashSet;

use oci_spec::runtime::{
    Capabilities, Capability, LinuxBuilder, LinuxCapabilitiesBuilder, LinuxNamespaceBuilder,
    LinuxNamespaceType, LinuxResources, Mount, MountBuilder, ProcessBuilder, RootBuilder, Spec,
    SpecBuilder,
};

use crate::error::WispImageError;
use crate::registry::PulledImage;
use crate::store::{ContentStore, layer};

/// Operator-supplied overrides applied on top of the image config when
/// synthesising a runtime Spec. All fields are optional / additive; an
/// empty `ConfigOverrides::default()` produces a Spec that runs the
/// image's own entrypoint with its own env in its own working dir.
#[derive(Debug, Default, Clone)]
pub struct ConfigOverrides {
    /// Replaces the image config's `Cmd`. `None` keeps the image value.
    pub args: Option<Vec<String>>,
    /// Replaces the image config's `Entrypoint`. `None` keeps the image
    /// value.
    pub entrypoint: Option<Vec<String>>,
    /// APPENDED to the image config's `Env`. Container runtimes consume
    /// the last occurrence of a `KEY=VALUE` pair, so a late entry here
    /// effectively overrides an earlier image-supplied one.
    pub env: Vec<String>,
    /// Replaces the image config's `WorkingDir`. `None` falls back to
    /// the image value, then to `/`.
    pub cwd: Option<String>,
    /// Replaces the default hostname (the bundle dir basename).
    pub hostname: Option<String>,
    /// APPENDED to the baseline mounts.
    pub mounts: Vec<Mount>,
    /// Replaces the baseline (no resources). Set to apply cgroup limits.
    pub linux_resources: Option<LinuxResources>,
    /// REPLACES the baseline capability set when `Some`. The baseline
    /// is `CAP_KILL` + `CAP_NET_BIND_SERVICE` (matching the busybox
    /// demo); operators that need to run images whose entrypoint drops
    /// privilege from root to a service user (nginx, postgres, ...)
    /// supply a richer set here. The five OCI capability sets
    /// (bounding / effective / permitted / inheritable / ambient) are
    /// each populated independently.
    pub capabilities: Option<CapabilityOverride>,
}

/// Operator-supplied capability sets for `process.capabilities` in the
/// synthesised runtime Spec. Each field carries a list of OCI cap
/// names (`CAP_KILL`, `CAP_NET_BIND_SERVICE`, ...). Names with or
/// without the `CAP_` prefix are accepted; both forms map to the same
/// underlying [`oci_spec::runtime::Capability`].
///
/// All five fields default to empty so the override can be partial:
/// e.g. populate only `bounding` to drop everything-but-bounding to a
/// narrower allow-list while keeping the other sets at OCI defaults.
/// In practice the wisp-cli `--cap-add` flag populates all five sets
/// to the same list (matching docker `--cap-add` semantics).
#[derive(Debug, Default, Clone)]
pub struct CapabilityOverride {
    pub bounding: Vec<String>,
    pub effective: Vec<String>,
    pub permitted: Vec<String>,
    pub inheritable: Vec<String>,
    pub ambient: Vec<String>,
}

/// Materialises a runtime bundle from a pulled image.
pub struct BundleBuilder<'a> {
    image: &'a PulledImage,
    store: &'a ContentStore,
    bundle_dir: PathBuf,
}

impl<'a> BundleBuilder<'a> {
    /// Bind a builder to the given image, store, and target bundle dir.
    /// Does not touch the filesystem; use `assemble_rootfs` and
    /// `write_config` to materialise.
    pub fn new(image: &'a PulledImage, store: &'a ContentStore, bundle_dir: &Path) -> Self {
        Self {
            image,
            store,
            bundle_dir: bundle_dir.to_path_buf(),
        }
    }

    /// Borrow the bundle directory.
    pub fn bundle_dir(&self) -> &Path {
        &self.bundle_dir
    }

    /// Layer the image's rootfs into `<bundle_dir>/rootfs`.
    ///
    /// Idempotent: if `<bundle_dir>/rootfs` already exists, returns
    /// without touching disk. Errors if a partial extraction left
    /// `<bundle_dir>/rootfs.partial` behind from a prior crashed run;
    /// the caller must remove it before retrying.
    pub fn assemble_rootfs(&self) -> Result<(), WispImageError> {
        let rootfs = self.bundle_dir.join("rootfs");
        if rootfs.exists() {
            return Ok(());
        }
        if let Some(parent) = rootfs.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent)?;
        }
        layer::assemble_rootfs(self.store, &self.image.layers, &rootfs)
    }

    /// Build a runtime `Spec` from the image config + overrides.
    /// Does NOT write to disk; use [`Self::write_config`] for that.
    pub fn synthesise_config(&self, overrides: ConfigOverrides) -> Result<Spec, WispImageError> {
        let image_cfg = self.image.config.config().as_ref();

        // Step 1: compose entrypoint + cmd.
        let final_args = compose_args(image_cfg, &overrides)?;

        // Step 2: env = image_env ++ override_env.
        let mut env = image_cfg.and_then(|c| c.env().clone()).unwrap_or_default();
        env.extend(overrides.env.clone());

        // Step 3: cwd = override else image else "/".
        let cwd = overrides
            .cwd
            .clone()
            .or_else(|| image_cfg.and_then(|c| c.working_dir().clone()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/".to_string());

        // Step 4: hostname = override else first 12 chars of bundle
        // dir basename (Docker's "short container id" convention).
        let hostname = overrides
            .hostname
            .clone()
            .unwrap_or_else(|| default_hostname(&self.bundle_dir));

        // Step 5: process. Default capabilities mirror the busybox
        // demo's `CAP_KILL` + `CAP_NET_BIND_SERVICE` allow-list. When
        // `overrides.capabilities` is `Some`, replace each of the five
        // sets (bounding / effective / permitted / inheritable / ambient)
        // with the operator-supplied list.
        let caps = if let Some(cap_override) = overrides.capabilities.as_ref() {
            let bounding = parse_caps(&cap_override.bounding)?;
            let effective = parse_caps(&cap_override.effective)?;
            let permitted = parse_caps(&cap_override.permitted)?;
            let inheritable = parse_caps(&cap_override.inheritable)?;
            let ambient = parse_caps(&cap_override.ambient)?;
            LinuxCapabilitiesBuilder::default()
                .bounding(bounding)
                .effective(effective)
                .permitted(permitted)
                .inheritable(inheritable)
                .ambient(ambient)
                .build()
                .map_err(|e| WispImageError::Manifest(format!("LinuxCapabilities build: {e}")))?
        } else {
            LinuxCapabilitiesBuilder::default()
                .bounding(default_caps())
                .effective(default_caps())
                .permitted(default_caps())
                .inheritable(Capabilities::new())
                .ambient(Capabilities::new())
                .build()
                .map_err(|e| WispImageError::Manifest(format!("LinuxCapabilities build: {e}")))?
        };

        let process = ProcessBuilder::default()
            .terminal(false)
            .args(final_args)
            .env(env)
            .cwd(PathBuf::from(&cwd))
            .capabilities(caps)
            .no_new_privileges(true)
            .build()
            .map_err(|e| WispImageError::Manifest(format!("Process build: {e}")))?;

        let root = RootBuilder::default()
            .path(PathBuf::from("rootfs"))
            .readonly(false)
            .build()
            .map_err(|e| WispImageError::Manifest(format!("Root build: {e}")))?;

        // Step 6: mounts = baseline ++ override.
        let mut mounts = baseline_mounts()?;
        mounts.extend(overrides.mounts.clone());

        // Step 7: linux: required namespaces + masked/readonly paths
        // (+ optional resources).
        let namespaces: Vec<_> = [
            LinuxNamespaceType::Pid,
            LinuxNamespaceType::Mount,
            LinuxNamespaceType::Uts,
            LinuxNamespaceType::Ipc,
            LinuxNamespaceType::Network,
        ]
        .iter()
        .map(|t| {
            LinuxNamespaceBuilder::default()
                .typ(*t)
                .build()
                .map_err(|e| WispImageError::Manifest(format!("LinuxNamespace build: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

        let mut linux_builder = LinuxBuilder::default();
        linux_builder = linux_builder
            .namespaces(namespaces)
            .masked_paths(default_masked_paths())
            .readonly_paths(default_readonly_paths());
        if let Some(resources) = overrides.linux_resources.clone() {
            linux_builder = linux_builder.resources(resources);
        }
        let linux = linux_builder
            .build()
            .map_err(|e| WispImageError::Manifest(format!("Linux build: {e}")))?;

        let spec = SpecBuilder::default()
            .version("1.0.2")
            .hostname(hostname)
            .root(root)
            .mounts(mounts)
            .process(process)
            .linux(linux)
            .build()
            .map_err(|e| WispImageError::Manifest(format!("Spec build: {e}")))?;

        Ok(spec)
    }

    /// Convenience: synthesise + write `<bundle_dir>/config.json`.
    /// Returns the Spec (in case the caller wants to inspect it).
    pub fn write_config(&self, overrides: ConfigOverrides) -> Result<Spec, WispImageError> {
        let spec = self.synthesise_config(overrides)?;
        if !self.bundle_dir.exists() {
            fs::create_dir_all(&self.bundle_dir)?;
        }
        let path = self.bundle_dir.join("config.json");
        spec.save(&path)
            .map_err(|e| WispImageError::Manifest(format!("save spec: {e}")))?;
        Ok(spec)
    }

    /// Remove `<bundle_dir>/rootfs`. Atomic via rename to
    /// `rootfs.deleting` then recursive remove. Layer blobs in the
    /// store are NOT touched; GC handles those separately.
    pub fn cleanup(&self) -> Result<(), WispImageError> {
        let rootfs = self.bundle_dir.join("rootfs");
        if !rootfs.exists() {
            return Ok(());
        }
        let deleting = self.bundle_dir.join("rootfs.deleting");
        // Best-effort: if a prior crashed cleanup left a stale
        // rootfs.deleting, blow it away before claiming the name.
        if deleting.exists() {
            fs::remove_dir_all(&deleting)?;
        }
        fs::rename(&rootfs, &deleting)?;
        fs::remove_dir_all(&deleting)?;
        Ok(())
    }
}

/// Compose the final `process.args` from the image config + overrides.
///
/// Resolution order matches Docker / OCI conventions:
///
/// 1. If `overrides.entrypoint` is set, use it; else use image
///    `Entrypoint` (may be `None`).
/// 2. Append `overrides.args` if set, else image `Cmd` (may be `None`).
/// 3. If both halves are empty, error: the spec needs a non-empty args.
fn compose_args(
    image_cfg: Option<&oci_spec::image::Config>,
    overrides: &ConfigOverrides,
) -> Result<Vec<String>, WispImageError> {
    let entrypoint = overrides
        .entrypoint
        .clone()
        .or_else(|| image_cfg.and_then(|c| c.entrypoint().clone()));
    let cmd = overrides
        .args
        .clone()
        .or_else(|| image_cfg.and_then(|c| c.cmd().clone()));

    let mut out = entrypoint.unwrap_or_default();
    if let Some(c) = cmd {
        out.extend(c);
    }
    if out.is_empty() {
        return Err(WispImageError::Manifest(
            "image has no entrypoint or cmd; pass --command or override args".to_string(),
        ));
    }
    Ok(out)
}

/// First 12 chars of `bundle_dir`'s basename. Falls back to `"wisp"`
/// for empty or `..`-style paths. Matches Docker's hostname-from-cid
/// convention closely enough for our purposes.
fn default_hostname(bundle_dir: &Path) -> String {
    let base = bundle_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty() && s != "." && s != "..")
        .unwrap_or_else(|| "wisp".to_string());
    base.chars().take(12).collect()
}

/// Runc's default capability allow-list, scoped to what the busybox
/// demo proves out: `CAP_KILL` (signal pid 1) + `CAP_NET_BIND_SERVICE`
/// (services on ports < 1024). Everything else is dropped.
fn default_caps() -> Capabilities {
    let mut s: HashSet<Capability> = HashSet::new();
    s.insert(Capability::Kill);
    s.insert(Capability::NetBindService);
    s
}

/// Parse a list of OCI capability names into a [`Capabilities`] set.
/// Accepts both the canonical `CAP_KILL` form and the prefix-stripped
/// `KILL` form (oci_spec's enum strum-derives FromStr in
/// SCREAMING_SNAKE_CASE without the prefix). Empty input yields an
/// empty set; an unknown name surfaces a `Manifest` error naming the
/// offender.
fn parse_caps(names: &[String]) -> Result<Capabilities, WispImageError> {
    let mut out: HashSet<Capability> = HashSet::new();
    for raw in names {
        let bare = raw.strip_prefix("CAP_").unwrap_or(raw.as_str());
        let cap: Capability = bare.parse().map_err(|_| {
            WispImageError::Manifest(format!(
                "unknown OCI capability {raw:?}; expected something like CAP_NET_BIND_SERVICE"
            ))
        })?;
        out.insert(cap);
    }
    Ok(out)
}

/// The six standard mounts. Order matches the busybox demo to keep
/// behaviour comparable; the runtime doesn't care about ordering, but
/// the demo + integration tests do byte-for-byte diffs of generated
/// config.json against a checked-in expected.
fn baseline_mounts() -> Result<Vec<Mount>, WispImageError> {
    Ok(vec![
        MountBuilder::default()
            .destination(PathBuf::from("/proc"))
            .typ("proc".to_string())
            .source(PathBuf::from("proc"))
            .build()
            .map_err(|e| WispImageError::Manifest(format!("mount /proc: {e}")))?,
        MountBuilder::default()
            .destination(PathBuf::from("/dev"))
            .typ("tmpfs".to_string())
            .source(PathBuf::from("tmpfs"))
            .options(vec![
                "nosuid".to_string(),
                "strictatime".to_string(),
                "mode=755".to_string(),
                "size=65536k".to_string(),
            ])
            .build()
            .map_err(|e| WispImageError::Manifest(format!("mount /dev: {e}")))?,
        MountBuilder::default()
            .destination(PathBuf::from("/dev/pts"))
            .typ("devpts".to_string())
            .source(PathBuf::from("devpts"))
            .options(vec![
                "nosuid".to_string(),
                "noexec".to_string(),
                "newinstance".to_string(),
                "ptmxmode=0666".to_string(),
                "mode=0620".to_string(),
            ])
            .build()
            .map_err(|e| WispImageError::Manifest(format!("mount /dev/pts: {e}")))?,
        MountBuilder::default()
            .destination(PathBuf::from("/dev/shm"))
            .typ("tmpfs".to_string())
            .source(PathBuf::from("shm"))
            .options(vec![
                "nosuid".to_string(),
                "noexec".to_string(),
                "nodev".to_string(),
                "mode=1777".to_string(),
                "size=65536k".to_string(),
            ])
            .build()
            .map_err(|e| WispImageError::Manifest(format!("mount /dev/shm: {e}")))?,
        MountBuilder::default()
            .destination(PathBuf::from("/dev/mqueue"))
            .typ("mqueue".to_string())
            .source(PathBuf::from("mqueue"))
            .options(vec![
                "nosuid".to_string(),
                "noexec".to_string(),
                "nodev".to_string(),
            ])
            .build()
            .map_err(|e| WispImageError::Manifest(format!("mount /dev/mqueue: {e}")))?,
        MountBuilder::default()
            .destination(PathBuf::from("/sys"))
            .typ("sysfs".to_string())
            .source(PathBuf::from("sysfs"))
            .options(vec![
                "nosuid".to_string(),
                "noexec".to_string(),
                "nodev".to_string(),
                "ro".to_string(),
            ])
            .build()
            .map_err(|e| WispImageError::Manifest(format!("mount /sys: {e}")))?,
    ])
}

/// `linux.maskedPaths`: covered files whose existence in `/proc` /
/// `/sys` would leak host kernel detail to the container. Identical
/// to runc's default.
fn default_masked_paths() -> Vec<String> {
    vec![
        "/proc/kcore".to_string(),
        "/proc/keys".to_string(),
        "/proc/latency_stats".to_string(),
        "/proc/timer_list".to_string(),
        "/proc/timer_stats".to_string(),
        "/proc/sched_debug".to_string(),
        "/sys/firmware".to_string(),
        "/proc/scsi".to_string(),
    ]
}

/// `linux.readonlyPaths`: bind-remounts inside `/proc` that the
/// container can read but not write. Identical to runc's default.
fn default_readonly_paths() -> Vec<String> {
    vec![
        "/proc/asound".to_string(),
        "/proc/bus".to_string(),
        "/proc/fs".to_string(),
        "/proc/irq".to_string(),
        "/proc/sys".to_string(),
        "/proc/sysrq-trigger".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reference::ImageRef;
    use crate::registry::LayerRef;
    use oci_spec::image::{ConfigBuilder, ImageConfigurationBuilder, RootFsBuilder};
    use tempfile::TempDir;

    fn pulled_with_config(config: oci_spec::image::Config) -> PulledImage {
        let rootfs = RootFsBuilder::default()
            .typ("layers".to_string())
            .diff_ids(Vec::<String>::new())
            .build()
            .unwrap();
        let image_cfg = ImageConfigurationBuilder::default()
            .architecture(oci_spec::image::Arch::ARM64)
            .os(oci_spec::image::Os::Linux)
            .config(config)
            .rootfs(rootfs)
            .build()
            .unwrap();
        PulledImage {
            r: "alpine:3.19".parse::<ImageRef>().unwrap(),
            manifest_digest: "sha256:dead".to_string(),
            config: image_cfg,
            layers: Vec::new(),
        }
    }

    fn pulled_no_config() -> PulledImage {
        let rootfs = RootFsBuilder::default()
            .typ("layers".to_string())
            .diff_ids(Vec::<String>::new())
            .build()
            .unwrap();
        let image_cfg = ImageConfigurationBuilder::default()
            .architecture(oci_spec::image::Arch::ARM64)
            .os(oci_spec::image::Os::Linux)
            .rootfs(rootfs)
            .build()
            .unwrap();
        PulledImage {
            r: "alpine:3.19".parse::<ImageRef>().unwrap(),
            manifest_digest: "sha256:dead".to_string(),
            config: image_cfg,
            layers: Vec::new(),
        }
    }

    fn store_and_dir() -> (TempDir, ContentStore, PathBuf) {
        let store_tmp = tempfile::tempdir().unwrap();
        let bundle_tmp = tempfile::tempdir().unwrap();
        let store = ContentStore::new(store_tmp.path()).unwrap();
        let bundle_dir = bundle_tmp.path().join("my-bundle-id");
        // Leak the bundle tmp via forgetting; tests are short-lived.
        std::mem::forget(bundle_tmp);
        (store_tmp, store, bundle_dir)
    }

    #[test]
    fn synthesise_config_uses_image_entrypoint_when_no_override() {
        let cfg = ConfigBuilder::default()
            .entrypoint(vec!["/bin/myentry".to_string()])
            .cmd(vec!["arg1".to_string(), "arg2".to_string()])
            .build()
            .unwrap();
        let pulled = pulled_with_config(cfg);
        let (_st_tmp, store, bundle_dir) = store_and_dir();
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);
        let spec = builder
            .synthesise_config(ConfigOverrides::default())
            .unwrap();

        let args = spec.process().as_ref().unwrap().args().clone().unwrap();
        assert_eq!(
            args,
            vec![
                "/bin/myentry".to_string(),
                "arg1".to_string(),
                "arg2".to_string()
            ]
        );
    }

    #[test]
    fn synthesise_config_args_override_takes_precedence() {
        let cfg = ConfigBuilder::default()
            .entrypoint(vec!["/bin/myentry".to_string()])
            .cmd(vec!["image-arg".to_string()])
            .build()
            .unwrap();
        let pulled = pulled_with_config(cfg);
        let (_st_tmp, store, bundle_dir) = store_and_dir();
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);
        let overrides = ConfigOverrides {
            args: Some(vec!["override-arg".to_string()]),
            ..Default::default()
        };
        let spec = builder.synthesise_config(overrides).unwrap();

        let args = spec.process().as_ref().unwrap().args().clone().unwrap();
        assert_eq!(
            args,
            vec!["/bin/myentry".to_string(), "override-arg".to_string()]
        );
    }

    #[test]
    fn synthesise_config_appends_override_env_to_image_env() {
        let cfg = ConfigBuilder::default()
            .entrypoint(vec!["/bin/sh".to_string()])
            .env(vec!["PATH=/usr/bin".to_string(), "TERM=xterm".to_string()])
            .build()
            .unwrap();
        let pulled = pulled_with_config(cfg);
        let (_st_tmp, store, bundle_dir) = store_and_dir();
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);
        let overrides = ConfigOverrides {
            env: vec!["FOO=bar".to_string()],
            ..Default::default()
        };
        let spec = builder.synthesise_config(overrides).unwrap();
        let env = spec.process().as_ref().unwrap().env().clone().unwrap();
        assert_eq!(
            env,
            vec![
                "PATH=/usr/bin".to_string(),
                "TERM=xterm".to_string(),
                "FOO=bar".to_string()
            ]
        );
    }

    #[test]
    fn synthesise_config_resolves_cwd_default_to_slash() {
        let cfg = ConfigBuilder::default()
            .entrypoint(vec!["/bin/sh".to_string()])
            .build()
            .unwrap();
        let pulled = pulled_with_config(cfg);
        let (_st_tmp, store, bundle_dir) = store_and_dir();
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);
        let spec = builder
            .synthesise_config(ConfigOverrides::default())
            .unwrap();
        let cwd = spec.process().as_ref().unwrap().cwd().clone();
        assert_eq!(cwd, PathBuf::from("/"));
    }

    #[test]
    fn synthesise_config_uses_image_workingdir_when_no_override() {
        let cfg = ConfigBuilder::default()
            .entrypoint(vec!["/bin/sh".to_string()])
            .working_dir("/app".to_string())
            .build()
            .unwrap();
        let pulled = pulled_with_config(cfg);
        let (_st_tmp, store, bundle_dir) = store_and_dir();
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);
        let spec = builder
            .synthesise_config(ConfigOverrides::default())
            .unwrap();
        let cwd = spec.process().as_ref().unwrap().cwd().clone();
        assert_eq!(cwd, PathBuf::from("/app"));
    }

    #[test]
    fn synthesise_config_default_hostname_truncates_bundle_id() {
        let cfg = ConfigBuilder::default()
            .entrypoint(vec!["/bin/sh".to_string()])
            .build()
            .unwrap();
        let pulled = pulled_with_config(cfg);
        let store_tmp = tempfile::tempdir().unwrap();
        let store = ContentStore::new(store_tmp.path()).unwrap();
        // Bundle dir basename longer than 12 chars: hostname must
        // truncate to 12.
        let long_dir = std::path::PathBuf::from("/tmp/abcdefghijklmnop-extra");
        let builder = BundleBuilder::new(&pulled, &store, &long_dir);
        let spec = builder
            .synthesise_config(ConfigOverrides::default())
            .unwrap();
        let hostname = spec.hostname().clone().unwrap();
        assert_eq!(hostname, "abcdefghijkl");
        assert_eq!(hostname.len(), 12);
    }

    #[test]
    fn synthesise_config_errors_when_no_entrypoint_or_args() {
        // Image config with neither entrypoint nor cmd, and no override.
        let cfg = ConfigBuilder::default().build().unwrap();
        let pulled = pulled_with_config(cfg);
        let (_st_tmp, store, bundle_dir) = store_and_dir();
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);
        let err = builder
            .synthesise_config(ConfigOverrides::default())
            .expect_err("should fail with no entrypoint or args");
        let msg = err.to_string();
        assert!(
            msg.contains("entrypoint") || msg.contains("cmd") || msg.contains("args"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn synthesise_config_handles_image_with_no_config_block() {
        // Image with no .config block at all (legal per OCI spec):
        // overrides.args alone should suffice.
        let pulled = pulled_no_config();
        let (_st_tmp, store, bundle_dir) = store_and_dir();
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);
        let overrides = ConfigOverrides {
            args: Some(vec!["/bin/sh".to_string()]),
            ..Default::default()
        };
        let spec = builder.synthesise_config(overrides).unwrap();
        let args = spec.process().as_ref().unwrap().args().clone().unwrap();
        assert_eq!(args, vec!["/bin/sh".to_string()]);
    }

    #[test]
    fn synthesise_config_appends_override_mounts_to_baseline() {
        let cfg = ConfigBuilder::default()
            .entrypoint(vec!["/bin/sh".to_string()])
            .build()
            .unwrap();
        let pulled = pulled_with_config(cfg);
        let (_st_tmp, store, bundle_dir) = store_and_dir();
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);
        let extra = MountBuilder::default()
            .destination(PathBuf::from("/data"))
            .typ("bind".to_string())
            .source(PathBuf::from("/host/data"))
            .options(vec!["bind".to_string(), "ro".to_string()])
            .build()
            .unwrap();
        let overrides = ConfigOverrides {
            mounts: vec![extra.clone()],
            ..Default::default()
        };
        let spec = builder.synthesise_config(overrides).unwrap();
        let mounts = spec.mounts().clone().unwrap();
        assert_eq!(mounts.len(), 7); // 6 baseline + 1 override.
        let last = mounts.last().unwrap();
        assert_eq!(last.destination(), &PathBuf::from("/data"));
    }

    #[test]
    fn assemble_rootfs_is_idempotent() {
        // Synthesise an image with zero layers; assemble_rootfs creates
        // an empty rootfs, second call should be a no-op.
        let pulled = pulled_no_config();
        // Override layers vec to confirm the no-layer case still works.
        let _ = LayerRef {
            digest: "sha256:zero".to_string(),
            size: 0,
            media_type: "application/vnd.oci.image.layer.v1.tar".to_string(),
        };
        let store_tmp = tempfile::tempdir().unwrap();
        let bundle_tmp = tempfile::tempdir().unwrap();
        let store = ContentStore::new(store_tmp.path()).unwrap();
        let bundle_dir = bundle_tmp.path().join("b1");
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);

        builder.assemble_rootfs().expect("first assemble");
        let rootfs = bundle_dir.join("rootfs");
        assert!(rootfs.exists(), "rootfs should exist after first assemble");

        // Second call must be a no-op; assemble_rootfs in the layer
        // module would error on existing dest, so the idempotent guard
        // in BundleBuilder must short-circuit before calling it.
        builder
            .assemble_rootfs()
            .expect("second assemble must be no-op");
        assert!(rootfs.exists());
    }

    #[test]
    fn cleanup_removes_rootfs() {
        let pulled = pulled_no_config();
        let store_tmp = tempfile::tempdir().unwrap();
        let bundle_tmp = tempfile::tempdir().unwrap();
        let store = ContentStore::new(store_tmp.path()).unwrap();
        let bundle_dir = bundle_tmp.path().join("b1");
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);
        builder.assemble_rootfs().unwrap();
        let rootfs = bundle_dir.join("rootfs");
        assert!(rootfs.exists());
        builder.cleanup().unwrap();
        assert!(!rootfs.exists());
        // Cleanup is idempotent: second call on a missing rootfs is fine.
        builder.cleanup().unwrap();
    }

    #[test]
    fn synthesise_config_uses_default_caps_when_override_none() {
        // Phase 0.5: with `capabilities: None`, the synthesised Spec
        // matches the pre-0.5 default of `CAP_KILL` + `CAP_NET_BIND_SERVICE`
        // in bounding / effective / permitted, and empty inheritable +
        // ambient. The busybox demo + every prior phase relies on this.
        let cfg = ConfigBuilder::default()
            .entrypoint(vec!["/bin/sh".to_string()])
            .build()
            .unwrap();
        let pulled = pulled_with_config(cfg);
        let (_st_tmp, store, bundle_dir) = store_and_dir();
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);
        let spec = builder
            .synthesise_config(ConfigOverrides::default())
            .unwrap();

        let caps = spec
            .process()
            .as_ref()
            .unwrap()
            .capabilities()
            .clone()
            .unwrap();
        let bounding = caps.bounding().clone().unwrap_or_default();
        assert_eq!(
            bounding.len(),
            2,
            "default bounding has Kill + NetBindService"
        );
        assert!(bounding.contains(&Capability::Kill));
        assert!(bounding.contains(&Capability::NetBindService));
        let inheritable = caps.inheritable().clone().unwrap_or_default();
        assert!(inheritable.is_empty(), "default inheritable is empty");
        let ambient = caps.ambient().clone().unwrap_or_default();
        assert!(ambient.is_empty(), "default ambient is empty");
    }

    #[test]
    fn synthesise_config_applies_cap_override() {
        // Phase 0.5: `CapabilityOverride` replaces the default cap set
        // across all five OCI sets. Each name accepts both `CAP_KILL`
        // and `KILL` forms.
        let cfg = ConfigBuilder::default()
            .entrypoint(vec!["/bin/sh".to_string()])
            .build()
            .unwrap();
        let pulled = pulled_with_config(cfg);
        let (_st_tmp, store, bundle_dir) = store_and_dir();
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);

        let cap_set = vec![
            "CAP_CHOWN".to_string(),
            "CAP_SETUID".to_string(),
            "CAP_SETGID".to_string(),
            "CAP_DAC_OVERRIDE".to_string(),
            "CAP_FOWNER".to_string(),
            "CAP_SETPCAP".to_string(),
        ];
        let overrides = ConfigOverrides {
            capabilities: Some(CapabilityOverride {
                bounding: cap_set.clone(),
                effective: cap_set.clone(),
                permitted: cap_set.clone(),
                inheritable: cap_set.clone(),
                ambient: cap_set.clone(),
            }),
            ..Default::default()
        };
        let spec = builder.synthesise_config(overrides).unwrap();
        let caps = spec
            .process()
            .as_ref()
            .unwrap()
            .capabilities()
            .clone()
            .unwrap();
        for set_name in [
            "bounding",
            "effective",
            "permitted",
            "inheritable",
            "ambient",
        ] {
            let set: HashSet<Capability> = match set_name {
                "bounding" => caps.bounding().clone().unwrap_or_default(),
                "effective" => caps.effective().clone().unwrap_or_default(),
                "permitted" => caps.permitted().clone().unwrap_or_default(),
                "inheritable" => caps.inheritable().clone().unwrap_or_default(),
                "ambient" => caps.ambient().clone().unwrap_or_default(),
                _ => unreachable!(),
            };
            assert_eq!(set.len(), 6, "{set_name} has 6 entries");
            assert!(
                set.contains(&Capability::Chown),
                "{set_name} contains Chown"
            );
            assert!(
                set.contains(&Capability::Setuid),
                "{set_name} contains Setuid"
            );
            assert!(
                set.contains(&Capability::Setpcap),
                "{set_name} contains Setpcap"
            );
        }
    }

    #[test]
    fn synthesise_config_cap_override_rejects_unknown_name() {
        // Garbage cap names surface a Manifest error so the operator
        // sees the offender, not a panic deep inside the OCI builder.
        let cfg = ConfigBuilder::default()
            .entrypoint(vec!["/bin/sh".to_string()])
            .build()
            .unwrap();
        let pulled = pulled_with_config(cfg);
        let (_st_tmp, store, bundle_dir) = store_and_dir();
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);

        let overrides = ConfigOverrides {
            capabilities: Some(CapabilityOverride {
                bounding: vec!["CAP_NOT_A_REAL_CAP".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = builder
            .synthesise_config(overrides)
            .expect_err("unknown cap should error");
        let msg = err.to_string();
        assert!(
            msg.contains("CAP_NOT_A_REAL_CAP") || msg.contains("unknown"),
            "expected error to name the bad cap, got: {msg}"
        );
    }

    #[test]
    fn write_config_creates_config_json() {
        let cfg = ConfigBuilder::default()
            .entrypoint(vec!["/bin/sh".to_string()])
            .build()
            .unwrap();
        let pulled = pulled_with_config(cfg);
        let bundle_tmp = tempfile::tempdir().unwrap();
        let store_tmp = tempfile::tempdir().unwrap();
        let store = ContentStore::new(store_tmp.path()).unwrap();
        let bundle_dir = bundle_tmp.path().join("written");
        let builder = BundleBuilder::new(&pulled, &store, &bundle_dir);
        builder.write_config(ConfigOverrides::default()).unwrap();
        let config_path = bundle_dir.join("config.json");
        assert!(config_path.exists());
        // Re-load the config and round-trip through Spec::load to
        // confirm we wrote a syntactically valid OCI runtime spec.
        let reloaded = Spec::load(&config_path).expect("spec re-load");
        assert!(reloaded.process().is_some());
    }
}
