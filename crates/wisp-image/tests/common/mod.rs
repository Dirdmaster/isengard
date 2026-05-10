//! Shared helpers for wisp-image integration tests.
//!
//! Lives under `tests/common/mod.rs` (a sub-module path) rather than
//! `tests/common.rs` so cargo doesn't treat it as a separate test
//! binary. Each integration test file declares `mod common;` to pull
//! it in.

#![allow(dead_code)] // helpers used selectively by each test target

use std::path::{Path, PathBuf};

/// Returns true if the current process is uid 0. Used by the
/// roundtrip test to skip when not running as root.
pub fn is_root() -> bool {
    // SAFETY: geteuid is infallible per POSIX; a libc call with no
    // arguments and a stable return type is sound to FFI.
    let euid = unsafe { libc::geteuid() };
    euid == 0
}

/// True iff `WISP_OFFLINE=1` is set in the environment. Lets CI runs
/// keep the network-required tests skipped while still running the rest.
pub fn offline() -> bool {
    std::env::var("WISP_OFFLINE").is_ok()
}

/// Build an isolated cgroup root path for one test invocation. Mirrors
/// the wisp crate's `tests/common::unique_cgroup_root` so per-test
/// roots don't collide with each other or with parallel wisp tests.
pub fn unique_cgroup_root(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        "/sys/fs/cgroup/wisp-test-image/{label}-{pid}-{nanos}-{seq}"
    ))
}

/// Best-effort: enable cpu/memory/pids on the parent slice's
/// subtree_control so a freshly-mkdir'd child can inherit them.
/// Mirrors what the wisp crate's `tests/common::isolated_runtime` does.
pub fn prime_cgroup_parent(cgroup_root: &Path) {
    if let Some(parent) = cgroup_root.parent() {
        let _ = std::fs::create_dir_all(parent);
        let parent_subtree = parent.join("cgroup.subtree_control");
        if parent_subtree.exists() {
            let _ = std::fs::write(&parent_subtree, b"+cpu +memory +pids");
        }
    }
}
