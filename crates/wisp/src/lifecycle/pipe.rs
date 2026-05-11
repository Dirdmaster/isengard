//! Parent / child synchronisation pipes used during container start.
//!
//! Two one-shot pipes coordinate the parent and the cloned child:
//!
//! 1. ReadyPipe (child -> parent): the child finishes its post-clone
//!    setup (drop caps, mount, pivot, sethostname, set permitted /
//!    effective / inheritable caps), then writes the five ASCII bytes
//!    `ready` to the writer end. The parent reads exactly those five
//!    bytes and knows the child has pivoted.
//!
//! 2. GoPipe (parent -> child, Phase 0.17): after the parent has
//!    finished cgroup attach + network attach (which shells out
//!    `nsenter -t <child_pid> -n ip ...` and races against the
//!    child's exec), the parent writes the two ASCII bytes `go` to
//!    the child. Only then does the child raise ambient + execvpe.
//!
//! This second pipe closes the nsenter race: without it, a
//! short-lived workload (e.g. `sh -c 'exit 0'`) could exec, run, and
//! exit before the parent's nsenter calls completed, causing the
//! `/proc/<pid>/ns/net` lookup to ENOENT mid-attach.
//!
//! Each protocol is deliberately tiny: there's nothing to negotiate.
//! If a peer dies before signalling, the other side observes EOF
//! (zero-byte read) and surfaces a clear lifecycle error. If the
//! peer writes the wrong bytes, we surface a separate diagnostic so
//! a regression doesn't masquerade as a hang.
//!
//! Portable: Linux uses `nix::unistd::pipe2(O_CLOEXEC)` for an atomic
//! CLOEXEC set; macOS (where `pipe2` isn't exposed by `nix`) falls
//! back to `pipe()` + per-fd `fcntl(F_SETFD, FD_CLOEXEC)`. Mac unit
//! tests exercise the full pair / signal / wait flow.

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use crate::error::{Result, WispError};

/// The exact bytes the child writes to signal "post-clone setup is
/// done; record me as Running."
const READY: &[u8; 5] = b"ready";

/// The exact bytes the parent writes (Phase 0.17) to release the
/// child from its pre-exec hold. Sent only after cgroup + network
/// attach complete.
const GO: &[u8; 2] = b"go";

/// Parent / child synchronisation pipe.
///
/// Owns both ends until the caller splits them: the child closes
/// `reader` (via Drop) and writes to `writer`; the parent closes
/// `writer` and reads from `reader`. Both ends carry `O_CLOEXEC`
/// so an accidental `exec` doesn't leak a dangling fd into the
/// container's process table.
pub struct ReadyPipe {
    pub reader: OwnedFd,
    pub writer: OwnedFd,
}

/// Open an `O_CLOEXEC` pipe for the parent / child handshake.
///
/// On Linux we use `pipe2(O_CLOEXEC)` for an atomic CLOEXEC set. On
/// macOS (where `pipe2` is absent from the libc `nix` ships), we fall
/// back to `pipe()` followed by per-fd `fcntl(FD_CLOEXEC)`. The
/// fallback is portability glue for the Mac dev loop only; the real
/// runtime path is Linux.
#[cfg(target_os = "linux")]
pub fn pair() -> Result<ReadyPipe> {
    use nix::fcntl::OFlag;
    use nix::unistd::pipe2;
    let (reader, writer) = pipe2(OFlag::O_CLOEXEC)
        .map_err(|err| WispError::Lifecycle(format!("pipe2(O_CLOEXEC): {err}")))?;
    Ok(ReadyPipe { reader, writer })
}

#[cfg(not(target_os = "linux"))]
pub fn pair() -> Result<ReadyPipe> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    let (reader, writer) =
        nix::unistd::pipe().map_err(|err| WispError::Lifecycle(format!("pipe(): {err}")))?;
    // Set FD_CLOEXEC on each end so the test harness behaves like
    // the linux path (CLOEXEC means an accidental exec doesn't leak
    // the pipe to a child process).
    fcntl(&reader, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .map_err(|err| WispError::Lifecycle(format!("fcntl(reader, FD_CLOEXEC): {err}")))?;
    fcntl(&writer, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .map_err(|err| WispError::Lifecycle(format!("fcntl(writer, FD_CLOEXEC): {err}")))?;
    Ok(ReadyPipe { reader, writer })
}

/// Write the literal `ready` token down the writer. Called from the
/// child once post-clone setup has completed.
///
/// The fd is borrowed (not consumed) so the caller stays in control
/// of when the writer is closed: the child must close the writer end
/// before `execvpe` to avoid leaking it into the container.
pub fn signal_ready(writer: &OwnedFd) -> Result<()> {
    // SAFETY: we wrap the borrowed fd in a transient `File` so the
    // standard `Write` API is available. `into_raw_fd` would consume
    // the fd, so we duplicate via `as_raw_fd`. The transient `File`
    // is leaked into a `ManuallyDrop`-equivalent by `mem::forget` so
    // `Drop` doesn't close the fd: the caller still owns it.
    let raw = writer.as_fd().as_raw_fd();
    let mut f = unsafe { std::fs::File::from_raw_fd(raw) };
    let res = f.write_all(READY);
    // Don't run `Drop` on `f`: that would close the fd we don't own.
    std::mem::forget(f);
    res.map_err(|err| WispError::Lifecycle(format!("signal_ready: write \"ready\": {err}")))
}

/// Read the literal `ready` token from the reader. Called from the
/// parent after `clone3` returns.
///
/// Three failure modes:
///
/// - EOF (the child died before signalling): error mentions
///   "premature close".
/// - Wrong bytes: error names the bytes we got.
/// - Underlying I/O error from the kernel: the error chain bubbles
///   up via [`WispError::Lifecycle`].
pub fn wait_ready(reader: &OwnedFd) -> Result<()> {
    // Same borrowed-fd pattern as `signal_ready`: we wrap, read, then
    // forget so Drop doesn't close the fd.
    let raw = reader.as_fd().as_raw_fd();
    let mut f = unsafe { std::fs::File::from_raw_fd(raw) };

    let mut buf = [0u8; READY.len()];
    let read_res = f.read_exact(&mut buf);
    std::mem::forget(f);

    match read_res {
        Ok(()) => {
            if buf == *READY {
                Ok(())
            } else {
                Err(WispError::Lifecycle(format!(
                    "wait_ready: expected {:?}, got {:?}",
                    String::from_utf8_lossy(READY),
                    String::from_utf8_lossy(&buf)
                )))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Err(WispError::Lifecycle(
            "wait_ready: child closed pipe without signalling (premature close / EOF)".to_string(),
        )),
        Err(err) => Err(WispError::Lifecycle(format!("wait_ready: read: {err}"))),
    }
}

/// Phase 0.17: parent / child go pipe.
///
/// Same shape as [`ReadyPipe`] but reversed direction: parent writes,
/// child reads. The parent only writes `go` after the per-container
/// cgroup is populated AND (if a network spec is set) `nsenter`-based
/// veth attach completes. This eliminates the race where a short-lived
/// child execs and exits before the parent's nsenter calls finish.
pub struct GoPipe {
    pub reader: OwnedFd,
    pub writer: OwnedFd,
}

/// Open an `O_CLOEXEC` pipe for the parent -> child `go` handshake.
/// Same CLOEXEC discipline as [`pair`].
#[cfg(target_os = "linux")]
pub fn pair_go() -> Result<GoPipe> {
    use nix::fcntl::OFlag;
    use nix::unistd::pipe2;
    let (reader, writer) = pipe2(OFlag::O_CLOEXEC)
        .map_err(|err| WispError::Lifecycle(format!("pipe2(O_CLOEXEC) (go): {err}")))?;
    Ok(GoPipe { reader, writer })
}

#[cfg(not(target_os = "linux"))]
pub fn pair_go() -> Result<GoPipe> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    let (reader, writer) =
        nix::unistd::pipe().map_err(|err| WispError::Lifecycle(format!("pipe() (go): {err}")))?;
    fcntl(&reader, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .map_err(|err| WispError::Lifecycle(format!("fcntl(go reader, FD_CLOEXEC): {err}")))?;
    fcntl(&writer, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .map_err(|err| WispError::Lifecycle(format!("fcntl(go writer, FD_CLOEXEC): {err}")))?;
    Ok(GoPipe { reader, writer })
}

/// Phase 0.17: write the literal `go` token down the writer. Called
/// from the parent once cgroup + network attach have completed and
/// the child may safely raise ambient + execvpe.
///
/// Same borrowed-fd discipline as [`signal_ready`].
pub fn signal_go(writer: &OwnedFd) -> Result<()> {
    let raw = writer.as_fd().as_raw_fd();
    let mut f = unsafe { std::fs::File::from_raw_fd(raw) };
    let res = f.write_all(GO);
    std::mem::forget(f);
    res.map_err(|err| WispError::Lifecycle(format!("signal_go: write \"go\": {err}")))
}

/// Phase 0.17: read the literal `go` token from the reader. Called
/// from the child after `signal_ready`, before raising ambient + exec.
///
/// Failure modes mirror [`wait_ready`]:
///
/// - EOF (parent dropped the writer without signalling): the child
///   exits with a clear "parent died before signalling go"
///   diagnostic. The reaper sees a normal `Die`.
/// - Wrong bytes: error names the bytes we got.
/// - Kernel I/O error: bubbles up via [`WispError::Lifecycle`].
pub fn wait_go(reader: &OwnedFd) -> Result<()> {
    let raw = reader.as_fd().as_raw_fd();
    let mut f = unsafe { std::fs::File::from_raw_fd(raw) };

    let mut buf = [0u8; GO.len()];
    let read_res = f.read_exact(&mut buf);
    std::mem::forget(f);

    match read_res {
        Ok(()) => {
            if buf == *GO {
                Ok(())
            } else {
                Err(WispError::Lifecycle(format!(
                    "wait_go: expected {:?}, got {:?}",
                    String::from_utf8_lossy(GO),
                    String::from_utf8_lossy(&buf)
                )))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Err(WispError::Lifecycle(
            "wait_go: parent died before signalling go (premature close / EOF)".to_string(),
        )),
        Err(err) => Err(WispError::Lifecycle(format!("wait_go: read: {err}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::fd::AsRawFd;
    use std::thread;

    #[test]
    fn pair_returns_two_fds() {
        let pipe = pair().expect("pipe pair");
        // The two fds must be distinct numerically; otherwise the
        // kernel handed us a malformed pair (or our wrapping is
        // wrong).
        assert_ne!(
            pipe.reader.as_fd().as_raw_fd(),
            pipe.writer.as_fd().as_raw_fd(),
            "reader and writer should be distinct fds"
        );
    }

    #[test]
    fn signal_then_wait_round_trips() {
        let pipe = pair().expect("pipe pair");
        let ReadyPipe { reader, writer } = pipe;

        // Spawn a thread that signals; main thread waits. The thread
        // owns the writer so dropping it after signal closes the
        // write end and unblocks any subsequent read.
        let writer_thread = thread::spawn(move || {
            signal_ready(&writer).expect("signal_ready");
            // Drop happens at scope exit; that closes the write end.
        });

        wait_ready(&reader).expect("wait_ready should succeed");
        writer_thread.join().expect("writer thread");
    }

    #[test]
    fn wait_ready_errors_on_eof() {
        let pipe = pair().expect("pipe pair");
        let ReadyPipe { reader, writer } = pipe;

        // Drop the writer without signalling. The reader observes EOF.
        drop(writer);

        let err = wait_ready(&reader).expect_err("EOF should surface as an error");
        let msg = err.to_string();
        assert!(
            msg.contains("EOF") || msg.contains("premature close"),
            "expected EOF-flavoured error, got: {msg}"
        );
    }

    // ----- Phase 0.17: GoPipe (parent -> child) tests -----

    #[test]
    fn pair_go_returns_two_fds() {
        let pipe = pair_go().expect("go pipe pair");
        assert_ne!(
            pipe.reader.as_fd().as_raw_fd(),
            pipe.writer.as_fd().as_raw_fd(),
            "go reader and writer should be distinct fds"
        );
    }

    #[test]
    fn signal_go_then_wait_go_round_trips() {
        let pipe = pair_go().expect("go pipe pair");
        let GoPipe { reader, writer } = pipe;
        let writer_thread = thread::spawn(move || {
            signal_go(&writer).expect("signal_go");
        });
        wait_go(&reader).expect("wait_go should succeed");
        writer_thread.join().expect("writer thread");
    }

    #[test]
    fn wait_go_errors_on_eof() {
        // Parent crashes between wait_ready and signal_go: child
        // observes EOF and exits cleanly rather than hanging.
        let pipe = pair_go().expect("go pipe pair");
        let GoPipe { reader, writer } = pipe;
        drop(writer);
        let err = wait_go(&reader).expect_err("EOF should surface as an error");
        let msg = err.to_string();
        assert!(
            msg.contains("EOF") || msg.contains("premature close") || msg.contains("parent died"),
            "expected EOF-flavoured error, got: {msg}"
        );
    }

    #[test]
    fn wait_ready_errors_on_garbage() {
        let pipe = pair().expect("pipe pair");
        let ReadyPipe { reader, writer } = pipe;

        // Spawn a thread that writes the wrong bytes. We use a
        // transient File the same way `signal_ready` does so we stay
        // owners of the fd (not strictly necessary here since we
        // drop it; just keeping symmetry with the prod code).
        let writer_thread = thread::spawn(move || {
            let raw = writer.as_fd().as_raw_fd();
            let mut f = unsafe { std::fs::File::from_raw_fd(raw) };
            f.write_all(b"nope!").expect("write garbage");
            std::mem::forget(f);
            // Now drop the OwnedFd to close the write end so the
            // reader doesn't block forever.
            drop(writer);
        });

        let err = wait_ready(&reader).expect_err("garbage bytes should surface");
        let msg = err.to_string();
        assert!(
            msg.contains("expected") && msg.contains("ready"),
            "expected mismatch error mentioning the expected token, got: {msg}"
        );
        writer_thread.join().expect("writer thread");
    }
}
