//! Network-namespace helpers wrapping `nsenter`.
//!
//! The runtime keeps the container PID in state.json (added in dispatch
//! C); both the veth attach path and the resolv.conf write path need to
//! run a handful of `ip ...` commands inside that PID's netns. Doing
//! this from Rust without `nsenter` would require `setns(2)` plumbing +
//! a child fork; `nsenter` is the cheap, debug-friendly alternative
//! that matches what an operator would type by hand.

use crate::error::WispNetError;
use std::process::{Command, Stdio};

/// Run a command inside the target PID's network namespace.
///
/// Wraps `nsenter -t <pid> -n <args...>`. Returns stdout on success,
/// [`WispNetError::Cmd`] on non-zero exit. The first arg should be the
/// program name (e.g. `"ip"`); subsequent args are passed verbatim.
pub fn run_in_netns(pid: u32, args: &[&str]) -> Result<String, WispNetError> {
    let pid_str = pid.to_string();
    let mut full: Vec<&str> = vec!["-t", &pid_str, "-n"];
    full.extend_from_slice(args);

    let child = Command::new("nsenter")
        .args(&full)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| WispNetError::Cmd {
            command: joined("nsenter", &full),
            exit_code: -1,
            stderr: format!("spawn failed: {e}"),
        })?;

    let out = child.wait_with_output().map_err(|e| WispNetError::Cmd {
        command: joined("nsenter", &full),
        exit_code: -1,
        stderr: format!("wait failed: {e}"),
    })?;

    if !out.status.success() {
        return Err(WispNetError::Cmd {
            command: joined("nsenter", &full),
            exit_code: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn joined(program: &str, args: &[&str]) -> String {
    let mut s = String::from(program);
    for a in args {
        s.push(' ');
        s.push_str(a);
    }
    s
}
