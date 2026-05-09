//! `pivot_root` orchestration for wisp containers.
//!
//! Per spec, after the rootfs has been mounted (see [`crate::mount`])
//! and the new root bind-mounted onto itself, the container child
//! pivots into it:
//!
//! 1. `mkdir(new_root.join(".old"))` for the old-root holding spot.
//! 2. `pivot_root(new_root, new_root.join(".old"))`
//! 3. `chdir("/")` so the working directory is inside the new root.
//! 4. `umount2(".old", MNT_DETACH)` to detach the old root.
//! 5. `rmdir(".old")` to remove the now-empty mount point.
//!
//! Linux only. macOS lacks `pivot_root`; the function is gated and
//! Mac builds skip the entire module body.

#![cfg(target_os = "linux")]

use std::path::Path;

use crate::error::{Result, WispError};

/// Perform the full `pivot_root` sequence on `new_root`.
///
/// Each step's failure is mapped to [`WispError::Mount`] (the spec
/// classes pivot under "mount, bind-mount, or pivot_root operation
/// failed") with the failing step's name embedded so logs pinpoint
/// where the lifecycle stalled.
pub fn pivot_root(new_root: &Path) -> Result<()> {
    let old = new_root.join(".old");

    // Step 1: mkdir new_root/.old. EEXIST is tolerated: the directory
    // may persist across container restarts; we just want a working
    // pivot target.
    match std::fs::create_dir(&old) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(WispError::Mount(format!(
                "pivot_root mkdir {}: {}",
                old.display(),
                err
            )));
        }
    }

    // Step 2: the actual pivot_root syscall.
    nix::unistd::pivot_root(new_root, &old).map_err(|err| {
        WispError::Mount(format!(
            "pivot_root({}, {}): {}",
            new_root.display(),
            old.display(),
            err
        ))
    })?;

    // Step 3: chdir into the new root. The pivot leaves cwd at the
    // old root's mount point; we move into "/" so subsequent path
    // resolution happens against the new root.
    nix::unistd::chdir("/")
        .map_err(|err| WispError::Mount(format!("pivot_root chdir(\"/\"): {err}")))?;

    // Step 4: detach the old root. After pivot_root the old root is
    // mounted at `/.old` (the new mount table reads back from the
    // new root); MNT_DETACH lets the kernel unmount lazily so any
    // open files keep working.
    nix::mount::umount2("/.old", nix::mount::MntFlags::MNT_DETACH)
        .map_err(|err| WispError::Mount(format!("pivot_root umount2(/.old): {err}")))?;

    // Step 5: rmdir the now-empty mount point. ENOENT is tolerated:
    // some kernels reap the directory eagerly on detach; either way
    // the post-condition we care about is "the dir is gone".
    match std::fs::remove_dir("/.old") {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(WispError::Mount(format!("pivot_root rmdir(/.old): {err}")));
        }
    }

    Ok(())
}
