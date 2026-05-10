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
    loop {
        let s = rt.state(&handle.id)?;
        if s.state == wisp::ContainerState::Stopped {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    rt.delete(&handle.id, true)?;
    Ok(())
}
