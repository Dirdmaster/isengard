//! Raw `clone3` syscall wrapper for wisp container start.
//!
//! `nix::sched::clone3` exists in 0.30+, but the API has changed shape
//! every minor release; we drop one layer down and hit the syscall
//! directly via [`libc::syscall`] for stability. The kernel's
//! `struct clone_args` layout is documented in `clone3(2)`; the
//! relevant fields for phase 0.1 are `flags` (the bitfield of
//! `CLONE_NEW*` namespace flags) and `exit_signal` (`SIGCHLD` so the
//! child shows up in `waitpid`).
//!
//! ## Single-threaded invariant
//!
//! `clone3` from a multi-threaded process is a footgun: glibc's
//! malloc holds locks across thread boundaries, and the cloned child
//! inherits mutexes that no other thread will ever release.
//! Lifecycle::start_container is documented to run only on the
//! single-threaded library entry point. The CLI binary calls it
//! before spawning any tokio runtime / tracing-subscriber thread.
//!
//! ## Linux-only
//!
//! This module is gated `#[cfg(target_os = "linux")]`. Mac builds
//! still compile because the parent module re-exports the public
//! types behind a matching cfg gate.

#![cfg(target_os = "linux")]

use crate::error::{Result, WispError};

/// Bitset of `CLONE_NEW*` namespace flags + the `exit_signal` byte.
///
/// We expose only the fields phase 0.1 actually needs: flags and the
/// exit-signal that `waitpid` keys off. Stack / parent_tid / pidfd /
/// cgroup-fd live in the kernel struct but we leave them zeroed.
#[derive(Debug, Clone, Copy)]
pub struct CloneArgs {
    /// `CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET`.
    /// `u64` because the kernel struct stores this as a wide bitfield;
    /// Rust's `libc::CLONE_NEW*` are `i32` so the caller casts.
    pub flags: u64,
    /// Signal delivered to the parent when the child exits. Wisp
    /// always uses `SIGCHLD` so `waitpid` reaps cleanly.
    pub exit_signal: i32,
}

/// Result of a [`clone3`] call.
///
/// Linux returns `0` to the child and the child PID to the parent;
/// we surface that as a typed enum so the caller can match instead of
/// reading the integer.
#[derive(Debug)]
pub enum CloneResult {
    /// We are running in the parent process. `child_pid` is the new
    /// child's PID in the parent's PID namespace.
    Parent { child_pid: u32 },
    /// We are running in the cloned child. PID is `1` in the new PID
    /// namespace; the caller proceeds with the post-clone setup
    /// sequence.
    Child,
}

/// The kernel `struct clone_args` from `linux/sched.h`.
///
/// 11 `u64` fields, each one matching the kernel ABI verbatim. We
/// keep this `repr(C)` and zero-initialise via `Default` so unset
/// fields stay at the kernel's "no thanks" sentinel.
#[repr(C)]
#[derive(Default)]
struct KernelCloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

/// Invoke `clone3(2)` with the given args.
///
/// Returns [`CloneResult::Parent`] in the original process and
/// [`CloneResult::Child`] in the new namespace-isolated process.
///
/// # Safety
///
/// The caller must:
///
/// - Be in a single-threaded process. Multi-threaded `clone3` is the
///   classic glibc-malloc deadlock; phase 0.1 explicitly forbids it.
/// - Be running on Linux 5.3+ (the kernel that introduced `clone3`).
/// - Hold whatever capabilities the requested namespaces need
///   (`CAP_SYS_ADMIN` for most of `CLONE_NEW*`, plus root or a user
///   namespace context if `CLONE_NEWUSER` is added later).
///
/// On `clone3` returning `-1`, the function maps `errno` into a
/// [`WispError::Lifecycle`] with the kernel error message.
pub unsafe fn clone3(args: &CloneArgs) -> Result<CloneResult> {
    let kargs = KernelCloneArgs {
        flags: args.flags,
        exit_signal: args.exit_signal as u64,
        ..KernelCloneArgs::default()
    };

    let size = core::mem::size_of::<KernelCloneArgs>() as libc::size_t;

    // SAFETY: SYS_clone3 takes a pointer to `struct clone_args` and a
    // size; the kernel checks the size against its own ABI version
    // and accepts shorter / longer buffers. Our buffer is the
    // canonical 11-u64 shape from clone3(2). The pointer is valid
    // for the lifetime of the call: `kargs` is on the stack and
    // outlives the syscall.
    let ret = unsafe { libc::syscall(libc::SYS_clone3, &kargs as *const KernelCloneArgs, size) };

    if ret < 0 {
        let err = std::io::Error::last_os_error();
        return Err(WispError::Lifecycle(format!(
            "clone3(flags={:#x}, exit_signal={}): {}",
            args.flags, args.exit_signal, err
        )));
    }
    if ret == 0 {
        Ok(CloneResult::Child)
    } else {
        Ok(CloneResult::Parent {
            child_pid: ret as u32,
        })
    }
}

/// `CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET`
/// folded into a single `u64`. The libc constants are `i32`, so we
/// widen on the way in.
pub fn five_namespace_flags() -> u64 {
    (libc::CLONE_NEWPID
        | libc::CLONE_NEWNS
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWNET) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_namespace_flags_includes_each_ns() {
        let flags = five_namespace_flags();
        assert_ne!(flags & libc::CLONE_NEWPID as u64, 0);
        assert_ne!(flags & libc::CLONE_NEWNS as u64, 0);
        assert_ne!(flags & libc::CLONE_NEWUTS as u64, 0);
        assert_ne!(flags & libc::CLONE_NEWIPC as u64, 0);
        assert_ne!(flags & libc::CLONE_NEWNET as u64, 0);
    }

    /// Smoke test: actually call clone3 with the five namespace flags
    /// and verify we get a positive child PID and the child sees PID 1.
    /// Requires root inside a Linux VM; gated `#[ignore]` so the
    /// default `cargo test` skips it. Run via:
    /// `orb -m wisp -u root bash -lc 'cargo test -p wisp -- --ignored clone3_smoke'`.
    #[test]
    #[ignore]
    fn clone3_smoke_in_vm() {
        let args = CloneArgs {
            flags: five_namespace_flags(),
            exit_signal: libc::SIGCHLD,
        };
        // SAFETY: integration smoke test, single-threaded test
        // harness invocation; root required so the kernel accepts
        // CLONE_NEW{NS,UTS,IPC,NET,PID}.
        let result = unsafe { clone3(&args) }.expect("clone3 should succeed");
        match result {
            CloneResult::Child => {
                // Inner pid must be 1 in the new pidns.
                let inner = unsafe { libc::getpid() };
                if inner != 1 {
                    unsafe { libc::_exit(64) };
                }
                unsafe { libc::_exit(0) };
            }
            CloneResult::Parent { child_pid } => {
                assert!(child_pid > 0, "child pid should be positive");
                let mut status: libc::c_int = 0;
                let waited = unsafe { libc::waitpid(child_pid as libc::pid_t, &mut status, 0) };
                assert_eq!(
                    waited, child_pid as libc::pid_t,
                    "waitpid should reap child"
                );
                assert!(libc::WIFEXITED(status), "child should exit normally");
                assert_eq!(
                    libc::WEXITSTATUS(status),
                    0,
                    "child should report inner pid == 1"
                );
            }
        }
    }
}
