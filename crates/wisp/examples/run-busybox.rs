//! Run the busybox demo bundle prepared by
//! `crates/wisp/examples/prepare-busybox.sh`.
//!
//! Usage (from the wisp crate root, as root inside the OrbStack VM):
//!
//! ```bash
//! bash examples/prepare-busybox.sh
//! cargo run --example run-busybox
//! # or override the bundle path / state-dir:
//! WISP_STATE_DIR=/tmp/wisp-demo cargo run --example run-busybox -- /path/to/bundle
//! ```
//!
//! The example exercises the public [`wisp::Runtime`] API end to
//! end: create + start + state-poll-until-stopped + delete. It is
//! intentionally NOT what `wisp-cli run` does (that path uses
//! `waitpid`); this example demonstrates that the polling
//! `Runtime::state` flow also works for a non-CLI consumer.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

fn main() -> Result<()> {
    // First positional arg is the bundle path; default to the
    // sibling `examples/busybox` directory.
    let bundle: String = std::env::args().nth(1).unwrap_or_else(|| {
        // examples are invoked from the crate root by `cargo run
        // --example`, so a relative path here is fine.
        "examples/busybox".to_string()
    });

    let state_dir = std::env::var("WISP_STATE_DIR").unwrap_or_else(|_| "/var/lib/wisp".to_string());

    let rt = wisp::Runtime::new(Path::new(&state_dir))?;
    let handle = rt.create("demo", Path::new(&bundle))?;
    rt.start(&handle.id)?;

    // Poll state until Stopped. The runtime's state() lazily
    // transitions Running -> Stopped when /proc/<pid> goes away.
    let stopped = loop {
        let s = rt.state(&handle.id)?;
        if s.state == wisp::ContainerState::Stopped {
            break s;
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    // Phase 0.4 dispatch C: stdout / stderr now redirect into
    // <state_dir>/containers/<id>/{stdout,stderr}.log so the agent's
    // `stream_logs` can tail them. The demo used to inherit the
    // parent terminal's stdout, so to keep the visible behaviour
    // identical (still prints "hello\nwisp-demo") we read the log
    // files back here and dump them to the example's stdout / stderr.
    if let Some(stdout_path) = &stopped.stdout_log_path {
        if let Ok(bytes) = std::fs::read(stdout_path) {
            let _ = std::io::stdout().write_all(&bytes);
        }
    }
    if let Some(stderr_path) = &stopped.stderr_log_path {
        if let Ok(bytes) = std::fs::read(stderr_path) {
            let _ = std::io::stderr().write_all(&bytes);
        }
    }

    // Phase 0.5: the per-container reaper writes an exit_status file
    // when PID 1 is reaped. `Runtime::state` reads it back into
    // `handle.exit_code`. Print whatever we got: `Some(0)` for a
    // clean exit; `Some(-N)` for a signal kill (`-9` is SIGKILL).
    // `None` means the reaper hasn't reaped yet (the poll loop above
    // saw Stopped before the 500ms reaper tick fired); the demo
    // reads it again briefly to give the reaper time to land.
    let mut final_handle = stopped;
    for _ in 0..20 {
        if final_handle.exit_code.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        final_handle = rt.state(&handle.id)?;
    }
    println!("exit_code: {:?}", final_handle.exit_code);

    rt.delete(&handle.id, true)?;
    Ok(())
}
