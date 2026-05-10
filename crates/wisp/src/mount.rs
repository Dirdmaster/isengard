//! Rootfs mount setup for wisp containers.
//!
//! Per spec section "Mounts", wisp drives the in-container filesystem
//! tree in this order:
//!
//! 1. Make `/` slave so our churn doesn't propagate back to the host
//!    (`mount("none", "/", null, MS_REC | MS_SLAVE, null)`).
//! 2. Bind-mount the bundle's rootfs onto itself with `MS_BIND |
//!    MS_REC`. `pivot_root` requires the new root to be a separate
//!    mount.
//! 3. For each `mount` entry in `spec.mounts`, translate to a
//!    `PendingMount` and execute. Common types: `proc`, `tmpfs`,
//!    `devpts`, `sysfs`, `mqueue`, `cgroup` / `cgroup2`, plus
//!    arbitrary bind mounts.
//! 4. Bind-mount the standard device nodes (`/dev/null`, `zero`,
//!    `full`, `random`, `urandom`, `tty`) from the host. Creating
//!    them with `mknod` would need `CAP_MKNOD` which we may have
//!    dropped: bind-mounts side-step that.
//! 5. After `pivot_root`: bind-mount `/dev/null` over each
//!    `spec.linux.maskedPaths`, then remount each
//!    `spec.linux.readonlyPaths` with `MS_RDONLY`.
//!
//! [`plan_mounts`] is the pure-logic planner: it builds the ordered
//! `PendingMount` list without touching syscalls so unit tests can
//! assert the plan on Mac. [`setup_rootfs`] (Linux only) iterates the
//! plan and calls `nix::mount::mount`.

use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use crate::error::Result;
#[cfg(target_os = "linux")]
use crate::error::WispError;

/// Cross-platform alias for the mount-syscall flag bitset. On Linux
/// it's `nix::mount::MsFlags` (the real flags consumed by
/// [`nix::mount::mount`]). On macOS we mirror the Linux constants
/// with a thin bitflags type so [`plan_mounts`] stays portable.
#[cfg(target_os = "linux")]
pub type MountFlags = nix::mount::MsFlags;

#[cfg(not(target_os = "linux"))]
pub use stub_flags::MountFlags;

#[cfg(not(target_os = "linux"))]
mod stub_flags {
    //! Mac mirror of `nix::mount::MsFlags`. Numeric values match the
    //! Linux kernel ABI so the bitset prints identically across
    //! platforms; nothing on Mac actually consumes them.

    use std::ops::{BitAnd, BitOr, BitOrAssign};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct MountFlags(pub u64);

    impl MountFlags {
        pub const fn empty() -> Self {
            MountFlags(0)
        }

        pub const fn bits(self) -> u64 {
            self.0
        }

        pub const fn contains(self, other: Self) -> bool {
            (self.0 & other.0) == other.0
        }

        // Linux MS_* numeric values, kept verbatim from `bits/mount.h`
        // so that any future debugging/log-printing matches the kernel.
        pub const MS_RDONLY: Self = MountFlags(1);
        pub const MS_NOSUID: Self = MountFlags(2);
        pub const MS_NODEV: Self = MountFlags(4);
        pub const MS_NOEXEC: Self = MountFlags(8);
        pub const MS_SYNCHRONOUS: Self = MountFlags(16);
        pub const MS_REMOUNT: Self = MountFlags(32);
        pub const MS_MANDLOCK: Self = MountFlags(64);
        pub const MS_DIRSYNC: Self = MountFlags(128);
        pub const MS_NOATIME: Self = MountFlags(1024);
        pub const MS_NODIRATIME: Self = MountFlags(2048);
        pub const MS_BIND: Self = MountFlags(4096);
        pub const MS_REC: Self = MountFlags(16384);
        pub const MS_SLAVE: Self = MountFlags(1 << 19);
        pub const MS_PRIVATE: Self = MountFlags(1 << 18);
        pub const MS_SHARED: Self = MountFlags(1 << 20);
        pub const MS_RELATIME: Self = MountFlags(1 << 21);
        pub const MS_STRICTATIME: Self = MountFlags(1 << 24);
    }

    impl BitOr for MountFlags {
        type Output = Self;
        fn bitor(self, rhs: Self) -> Self {
            MountFlags(self.0 | rhs.0)
        }
    }

    impl BitOrAssign for MountFlags {
        fn bitor_assign(&mut self, rhs: Self) {
            self.0 |= rhs.0;
        }
    }

    impl BitAnd for MountFlags {
        type Output = Self;
        fn bitand(self, rhs: Self) -> Self {
            MountFlags(self.0 & rhs.0)
        }
    }
}

/// Convenience accessors so `plan_mounts` can write `flags::MS_BIND`
/// instead of branching on cfg(linux). On Linux these are direct
/// re-exports of the bitflags constants from `nix::mount::MsFlags`.
pub mod flags {
    use super::MountFlags;

    #[cfg(target_os = "linux")]
    pub const MS_RDONLY: MountFlags = nix::mount::MsFlags::MS_RDONLY;
    #[cfg(target_os = "linux")]
    pub const MS_NOSUID: MountFlags = nix::mount::MsFlags::MS_NOSUID;
    #[cfg(target_os = "linux")]
    pub const MS_NODEV: MountFlags = nix::mount::MsFlags::MS_NODEV;
    #[cfg(target_os = "linux")]
    pub const MS_NOEXEC: MountFlags = nix::mount::MsFlags::MS_NOEXEC;
    #[cfg(target_os = "linux")]
    pub const MS_BIND: MountFlags = nix::mount::MsFlags::MS_BIND;
    #[cfg(target_os = "linux")]
    pub const MS_REC: MountFlags = nix::mount::MsFlags::MS_REC;
    #[cfg(target_os = "linux")]
    pub const MS_SLAVE: MountFlags = nix::mount::MsFlags::MS_SLAVE;
    #[cfg(target_os = "linux")]
    pub const MS_RELATIME: MountFlags = nix::mount::MsFlags::MS_RELATIME;
    #[cfg(target_os = "linux")]
    pub const MS_REMOUNT: MountFlags = nix::mount::MsFlags::MS_REMOUNT;
    #[cfg(target_os = "linux")]
    pub const MS_NOATIME: MountFlags = nix::mount::MsFlags::MS_NOATIME;
    #[cfg(target_os = "linux")]
    pub const MS_NODIRATIME: MountFlags = nix::mount::MsFlags::MS_NODIRATIME;
    #[cfg(target_os = "linux")]
    pub const MS_STRICTATIME: MountFlags = nix::mount::MsFlags::MS_STRICTATIME;

    #[cfg(not(target_os = "linux"))]
    pub const MS_RDONLY: MountFlags = MountFlags::MS_RDONLY;
    #[cfg(not(target_os = "linux"))]
    pub const MS_NOSUID: MountFlags = MountFlags::MS_NOSUID;
    #[cfg(not(target_os = "linux"))]
    pub const MS_NODEV: MountFlags = MountFlags::MS_NODEV;
    #[cfg(not(target_os = "linux"))]
    pub const MS_NOEXEC: MountFlags = MountFlags::MS_NOEXEC;
    #[cfg(not(target_os = "linux"))]
    pub const MS_BIND: MountFlags = MountFlags::MS_BIND;
    #[cfg(not(target_os = "linux"))]
    pub const MS_REC: MountFlags = MountFlags::MS_REC;
    #[cfg(not(target_os = "linux"))]
    pub const MS_SLAVE: MountFlags = MountFlags::MS_SLAVE;
    #[cfg(not(target_os = "linux"))]
    pub const MS_RELATIME: MountFlags = MountFlags::MS_RELATIME;
    #[cfg(not(target_os = "linux"))]
    pub const MS_REMOUNT: MountFlags = MountFlags::MS_REMOUNT;
    #[cfg(not(target_os = "linux"))]
    pub const MS_NOATIME: MountFlags = MountFlags::MS_NOATIME;
    #[cfg(not(target_os = "linux"))]
    pub const MS_NODIRATIME: MountFlags = MountFlags::MS_NODIRATIME;
    #[cfg(not(target_os = "linux"))]
    pub const MS_STRICTATIME: MountFlags = MountFlags::MS_STRICTATIME;
}

/// One step in the rootfs setup plan. Holds borrowed `&str` slices
/// rather than `String` so the planner doesn't allocate per-entry
/// strings the executor would have to free; the lifetime is bound to
/// the spec slice the planner walks.
#[derive(Debug)]
pub struct PendingMount<'a> {
    /// Source of the mount (e.g. `"proc"` for proc, the host path for
    /// bind mounts, or `None` for the slave-root step).
    pub source: Option<&'a str>,
    /// Absolute target inside the container's resolved rootfs (or the
    /// literal `/` for the first slave-root entry).
    pub target: PathBuf,
    /// Filesystem type (`"proc"`, `"tmpfs"`, `"devpts"`, etc.). `None`
    /// for bind mounts.
    pub fs_type: Option<&'a str>,
    /// Mount syscall flags.
    pub flags: MountFlags,
    /// `data` argument: comma-joined OCI option string for fs types
    /// that take it (tmpfs's `size=`, devpts's `newinstance`, etc.).
    pub data: Option<String>,
}

/// OCI mount options that map to bits in `MsFlags` rather than to
/// the `data` string. We strip these from the option list before
/// joining the rest into `data`. Keeping the table short and
/// non-exhaustive is intentional: we only translate the flags that
/// the spec's documented mount table actually uses.
fn flag_for_option(opt: &str) -> Option<MountFlags> {
    match opt {
        "ro" => Some(flags::MS_RDONLY),
        "nosuid" => Some(flags::MS_NOSUID),
        "nodev" => Some(flags::MS_NODEV),
        "noexec" => Some(flags::MS_NOEXEC),
        "bind" => Some(flags::MS_BIND),
        "rbind" => Some(flags::MS_BIND | flags::MS_REC),
        "relatime" => Some(flags::MS_RELATIME),
        // atime modifiers: tmpfs / devpts options frequently include
        // these. They're flags, not data options: leaving them in
        // `data` makes the kernel return EINVAL.
        "noatime" => Some(flags::MS_NOATIME),
        "nodiratime" => Some(flags::MS_NODIRATIME),
        "strictatime" => Some(flags::MS_STRICTATIME),
        _ => None,
    }
}

/// Translate one OCI `Mount` into a `PendingMount`, resolving its
/// destination relative to `rootfs` and splitting the option list
/// into (flags, data string).
fn translate_spec_mount<'a>(
    rootfs: &Path,
    mount: &'a oci_spec::runtime::Mount,
) -> PendingMount<'a> {
    // OCI dest is absolute, e.g. "/proc". Strip the leading slash
    // before joining so we land at `<rootfs>/proc`, not `/proc`.
    let dest = mount.destination();
    let target = match dest.strip_prefix("/") {
        Ok(rel) => rootfs.join(rel),
        Err(_) => rootfs.join(dest),
    };

    let fs_type = mount.typ().as_deref();

    // Bind mounts use the source as-is. Type-specific mounts (proc,
    // tmpfs, ...) take the source string from the spec verbatim
    // (kernel ignores it for proc-style filesystems but we stay
    // honest to whatever the bundle declared).
    let source = mount.source().as_ref().and_then(|p| p.to_str());

    let mut flags_acc = MountFlags::empty();
    let mut data_parts: Vec<&str> = Vec::new();
    if let Some(opts) = mount.options() {
        for opt in opts {
            if let Some(bit) = flag_for_option(opt) {
                flags_acc |= bit;
            } else {
                data_parts.push(opt.as_str());
            }
        }
    }
    let data = if data_parts.is_empty() {
        None
    } else {
        Some(data_parts.join(","))
    };

    PendingMount {
        source,
        target,
        fs_type,
        flags: flags_acc,
        data,
    }
}

/// Standard device nodes wisp bind-mounts from host into the rootfs.
/// `mknod` would need `CAP_MKNOD` which the spec usually drops; bind
/// mounts work without that capability.
const STANDARD_DEVICES: &[&str] = &["null", "zero", "full", "random", "urandom", "tty"];

/// Build the ordered mount-execution plan for a container rootfs.
///
/// This is pure logic: it doesn't touch any syscall. Tests assert the
/// plan and the linux-only [`setup_rootfs`] iterates it.
pub fn plan_mounts<'a>(
    rootfs: &'a Path,
    spec_mounts: &'a [oci_spec::runtime::Mount],
) -> Vec<PendingMount<'a>> {
    let mut plan: Vec<PendingMount<'a>> =
        Vec::with_capacity(spec_mounts.len() + STANDARD_DEVICES.len() + 2);

    // 1. Make `/` slave so child mounts don't propagate to host. The
    //    target is the literal "/", not anything under rootfs: this
    //    runs before pivot_root.
    plan.push(PendingMount {
        source: Some("none"),
        target: PathBuf::from("/"),
        fs_type: None,
        flags: flags::MS_REC | flags::MS_SLAVE,
        data: None,
    });

    // 2. Bind-mount the bundle rootfs onto itself. pivot_root requires
    //    the new root to be a separate mount entry; this is the
    //    standard runc/crun trick to satisfy that.
    plan.push(PendingMount {
        source: rootfs.to_str(),
        target: rootfs.to_path_buf(),
        fs_type: None,
        flags: flags::MS_BIND | flags::MS_REC,
        data: None,
    });

    // 3. Per-spec mounts.
    for spec_mount in spec_mounts {
        plan.push(translate_spec_mount(rootfs, spec_mount));
    }

    // 4. Standard device bind mounts. Each one points host
    //    `/dev/<name>` -> rootfs `/dev/<name>` with MS_BIND. The
    //    source string lives in a static array so the lifetime
    //    elides cleanly.
    for name in STANDARD_DEVICES {
        let host_path = match *name {
            "null" => "/dev/null",
            "zero" => "/dev/zero",
            "full" => "/dev/full",
            "random" => "/dev/random",
            "urandom" => "/dev/urandom",
            "tty" => "/dev/tty",
            _ => unreachable!("unknown standard device {name}"),
        };
        plan.push(PendingMount {
            source: Some(host_path),
            target: rootfs.join("dev").join(name),
            fs_type: None,
            flags: flags::MS_BIND,
            data: None,
        });
    }

    plan
}

/// Execute the mount plan and apply masked / readonly path policies.
///
/// Linux only: walks [`plan_mounts`] and calls `nix::mount::mount` for
/// each entry, then iterates the masked-paths list (bind-mounting
/// `/dev/null` over each) and the readonly-paths list (remounting
/// each with `MS_RDONLY`). All errors fold into [`WispError::Mount`]
/// with a useful path-bearing message.
#[cfg(target_os = "linux")]
pub fn setup_rootfs(
    bundle_rootfs: &Path,
    spec_mounts: &[oci_spec::runtime::Mount],
    masked: &[String],
    readonly: &[String],
) -> Result<()> {
    let plan = plan_mounts(bundle_rootfs, spec_mounts);
    for entry in &plan {
        execute_mount(entry)?;
    }
    apply_masked_paths(bundle_rootfs, masked)?;
    apply_readonly_paths(bundle_rootfs, readonly)?;
    Ok(())
}

/// Linux: invoke `nix::mount::mount` for one `PendingMount`. Errors
/// are wrapped in `WispError::Mount` with the target path so the log
/// pinpoints the failing entry.
///
/// Before mounting we ensure the target directory exists. This is
/// necessary for layered mounts: e.g. mounting `tmpfs` on
/// `<rootfs>/dev` discards our pre-created `dev/pts`, `dev/shm`,
/// `dev/mqueue` subdirs (the new tmpfs is empty), so the subsequent
/// devpts / tmpfs / mqueue mounts on those paths fail with ENOENT.
/// runc / crun do the same dance.
#[cfg(target_os = "linux")]
fn execute_mount(entry: &PendingMount<'_>) -> Result<()> {
    // Ensure the target exists. Bind-mounts onto file paths (like the
    // standard device nodes /dev/null etc.) need a regular file, not
    // a directory; for those entries we touch a zero-byte file at
    // the target. The heuristic: if the source is an existing file
    // on the host, the bind-mount target should be a file too.
    if let Some(parent) = entry.target.parent() {
        if !parent.exists() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    // "Is the host source something other than a directory?" covers
    // regular files (rare) and character / block / socket / fifo
    // device nodes (the standard /dev/* bind sources). We use
    // `metadata` so `is_file()` reports correctly, and explicitly
    // treat character devices as non-directories. `is_dir` returns
    // false for both files and devices.
    let host_source_is_file = entry
        .source
        .map(Path::new)
        .filter(|p| p.is_absolute())
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| !m.is_dir())
        .unwrap_or(false);
    if host_source_is_file {
        if !entry.target.exists() {
            // Create a zero-byte target file for the bind to land on.
            if let Err(err) = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&entry.target)
            {
                return Err(WispError::Mount(format!(
                    "create bind target file {}: {}",
                    entry.target.display(),
                    err
                )));
            }
        }
    } else if !entry.target.exists() {
        if let Err(err) = std::fs::create_dir_all(&entry.target) {
            return Err(WispError::Mount(format!(
                "create mount target {}: {}",
                entry.target.display(),
                err
            )));
        }
    }

    nix::mount::mount(
        entry.source.map(Path::new),
        &entry.target,
        entry.fs_type.map(Path::new),
        entry.flags,
        entry.data.as_deref().map(str::as_bytes),
    )
    .map_err(|err| {
        WispError::Mount(format!(
            "mount(source={:?}, target={}, type={:?}, flags={:#x}): {}",
            entry.source,
            entry.target.display(),
            entry.fs_type,
            entry.flags.bits(),
            err
        ))
    })
}

/// Mask each path inside the rootfs. The OCI spec resolves these
/// inside the container; we resolve them against the bundle rootfs so
/// the runtime can apply them before pivot_root.
///
/// File-typed targets get bind-mounted from `/dev/null` (read returns
/// EOF). Directory-typed targets get a fresh empty `tmpfs` mounted
/// over them (read sees an empty dir): bind-mounting a file over a
/// directory fails with ENOTDIR. Paths that don't exist on this
/// kernel are skipped silently: a missing path is already unreadable.
#[cfg(target_os = "linux")]
fn apply_masked_paths(rootfs: &Path, paths: &[String]) -> Result<()> {
    for raw in paths {
        let target = match raw.strip_prefix('/') {
            Some(rel) => rootfs.join(rel),
            None => rootfs.join(raw),
        };
        let meta = match std::fs::metadata(&target) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            nix::mount::mount(
                Some("tmpfs"),
                &target,
                Some(Path::new("tmpfs")),
                flags::MS_RDONLY,
                Some(b"size=0,mode=755".as_slice()),
            )
            .map_err(|err| {
                WispError::Mount(format!(
                    "mask {} via tmpfs overlay: {}",
                    target.display(),
                    err
                ))
            })?;
        } else {
            nix::mount::mount(
                Some("/dev/null"),
                &target,
                None::<&Path>,
                flags::MS_BIND,
                None::<&[u8]>,
            )
            .map_err(|err| {
                WispError::Mount(format!(
                    "mask {} via /dev/null bind: {}",
                    target.display(),
                    err
                ))
            })?;
        }
    }
    Ok(())
}

/// Make each spec'd path read-only inside the rootfs.
///
/// Kernel quirk: `MS_REMOUNT | MS_RDONLY` on a sub-path of a
/// non-bind-mounted filesystem (like `/proc/bus`, where the parent
/// mount is procfs) returns EINVAL. The runc/crun trick is to
/// FIRST bind-mount the path onto itself (creating a separate mount
/// entry the kernel can remount), THEN remount that bind as
/// readonly. We do the same.
///
/// Paths that don't exist are skipped silently: per OCI semantics
/// the spec is "guarantee this path is read-only", which a missing
/// path satisfies vacuously.
#[cfg(target_os = "linux")]
fn apply_readonly_paths(rootfs: &Path, paths: &[String]) -> Result<()> {
    for raw in paths {
        let target = match raw.strip_prefix('/') {
            Some(rel) => rootfs.join(rel),
            None => rootfs.join(raw),
        };
        if !target.exists() {
            continue;
        }
        // Step 1: bind-mount the target onto itself. MS_REC matters
        // when the target is a directory tree (e.g. /proc/sys); we
        // need every nested mount to also be a bind, otherwise the
        // remount-readonly only catches the top-level dentry.
        nix::mount::mount(
            Some(&target),
            &target,
            None::<&Path>,
            flags::MS_BIND | flags::MS_REC,
            None::<&[u8]>,
        )
        .map_err(|err| {
            WispError::Mount(format!(
                "bind {} onto self for ro remount: {}",
                target.display(),
                err
            ))
        })?;
        // Step 2: remount the new bind as readonly.
        nix::mount::mount(
            None::<&Path>,
            &target,
            None::<&Path>,
            flags::MS_REMOUNT | flags::MS_BIND | flags::MS_RDONLY,
            None::<&[u8]>,
        )
        .map_err(|err| {
            WispError::Mount(format!("remount {} read-only: {}", target.display(), err))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use oci_spec::runtime::MountBuilder;

    fn rootfs_path() -> PathBuf {
        PathBuf::from("/var/lib/wisp/containers/demo/rootfs")
    }

    #[test]
    fn plan_mounts_starts_with_slave_root() {
        let rootfs = rootfs_path();
        let plan = plan_mounts(&rootfs, &[]);

        let first = &plan[0];
        assert_eq!(first.target, PathBuf::from("/"));
        assert!(
            first.flags.contains(flags::MS_REC),
            "first entry should set MS_REC"
        );
        assert!(
            first.flags.contains(flags::MS_SLAVE),
            "first entry should set MS_SLAVE"
        );
        assert_eq!(first.source, Some("none"));
        assert!(first.fs_type.is_none());
    }

    #[test]
    fn plan_mounts_includes_bundle_rootfs_bind() {
        let rootfs = rootfs_path();
        let plan = plan_mounts(&rootfs, &[]);

        let second = &plan[1];
        assert_eq!(second.target, rootfs);
        assert_eq!(second.source.map(PathBuf::from), Some(rootfs.clone()));
        assert!(
            second.flags.contains(flags::MS_BIND),
            "rootfs bind should set MS_BIND"
        );
        assert!(
            second.flags.contains(flags::MS_REC),
            "rootfs bind should set MS_REC"
        );
    }

    #[test]
    fn plan_mounts_translates_spec_proc_entry() {
        let rootfs = rootfs_path();
        let proc_mount = MountBuilder::default()
            .destination(PathBuf::from("/proc"))
            .typ("proc")
            .source(PathBuf::from("proc"))
            .build()
            .unwrap();
        let spec_mounts = vec![proc_mount];

        let plan = plan_mounts(&rootfs, &spec_mounts);

        // The spec mount lands at index 2 (after slave-root + rootfs
        // bind).
        let entry = plan
            .iter()
            .find(|p| p.target == rootfs.join("proc"))
            .expect("plan should contain a mount targeting <rootfs>/proc");
        assert_eq!(entry.source, Some("proc"));
        assert_eq!(entry.fs_type, Some("proc"));
    }

    #[test]
    fn plan_mounts_translates_tmpfs_with_data() {
        let rootfs = rootfs_path();
        let tmpfs_mount = MountBuilder::default()
            .destination(PathBuf::from("/dev"))
            .typ("tmpfs")
            .source(PathBuf::from("tmpfs"))
            .options(vec!["size=65536k".to_string(), "mode=755".to_string()])
            .build()
            .unwrap();

        let plan = plan_mounts(&rootfs, std::slice::from_ref(&tmpfs_mount));
        let entry = plan
            .iter()
            .find(|p| p.fs_type == Some("tmpfs"))
            .expect("plan should contain the tmpfs entry");

        assert_eq!(entry.target, rootfs.join("dev"));
        assert_eq!(
            entry.data.as_deref(),
            Some("size=65536k,mode=755"),
            "tmpfs data should be the comma-joined option list"
        );
    }

    #[test]
    fn plan_mounts_appends_standard_devices() {
        let rootfs = rootfs_path();
        let plan = plan_mounts(&rootfs, &[]);

        // The last 6 entries should be the standard device bind
        // mounts, in the documented order.
        let expected_tail = [
            (rootfs.join("dev/null"), "/dev/null"),
            (rootfs.join("dev/zero"), "/dev/zero"),
            (rootfs.join("dev/full"), "/dev/full"),
            (rootfs.join("dev/random"), "/dev/random"),
            (rootfs.join("dev/urandom"), "/dev/urandom"),
            (rootfs.join("dev/tty"), "/dev/tty"),
        ];

        let tail = &plan[plan.len() - expected_tail.len()..];
        assert_eq!(tail.len(), expected_tail.len());
        for (entry, (expected_target, expected_source)) in tail.iter().zip(expected_tail.iter()) {
            assert_eq!(&entry.target, expected_target);
            assert_eq!(entry.source, Some(*expected_source));
            assert!(
                entry.flags.contains(flags::MS_BIND),
                "standard device {} should be a bind mount",
                expected_target.display()
            );
            assert!(
                entry.fs_type.is_none(),
                "standard device {} should have no fs_type (bind)",
                expected_target.display()
            );
        }
    }

    #[test]
    fn plan_mounts_handles_devpts_with_options() {
        let rootfs = rootfs_path();
        let devpts_mount = MountBuilder::default()
            .destination(PathBuf::from("/dev/pts"))
            .typ("devpts")
            .source(PathBuf::from("devpts"))
            .options(vec![
                "newinstance".to_string(),
                "ptmxmode=0666".to_string(),
                "nosuid".to_string(),
            ])
            .build()
            .unwrap();

        let plan = plan_mounts(&rootfs, std::slice::from_ref(&devpts_mount));
        let entry = plan
            .iter()
            .find(|p| p.fs_type == Some("devpts"))
            .expect("plan should contain the devpts entry");

        assert_eq!(entry.target, rootfs.join("dev/pts"));
        assert_eq!(
            entry.data.as_deref(),
            Some("newinstance,ptmxmode=0666"),
            "non-flag devpts options should land in data, comma-joined"
        );
        assert!(
            entry.flags.contains(flags::MS_NOSUID),
            "the `nosuid` option should be promoted to MS_NOSUID"
        );
    }
}
