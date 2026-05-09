//! Subprocess runners for `ip`, `iptables`, and `iptables-restore`.
//!
//! All runners capture both stdout and stderr; on non-zero exit, the
//! stderr stream is folded into [`WispNetError::Cmd`] alongside the
//! joined command line, so dispatch B + C call sites can blame the
//! exact invocation in their error chain.
//!
//! These are deliberately thin wrappers around `std::process::Command`:
//! the planner modules ([`crate::bridge`], [`crate::iptables`]) hold all
//! the rule-shape logic; cmd.rs is just the bridge to the real syscall
//! frontends.

use crate::error::WispNetError;
use std::io::Write;
use std::process::{Command, Stdio};

/// Run `ip <args>`, returning stdout on success.
///
/// Captures stderr into [`WispNetError::Cmd`] on non-zero exit. The
/// joined command line in the error includes `ip` as the program name
/// so it's copy-pasteable when debugging from a daily note.
pub fn run_ip(args: &[&str]) -> Result<String, WispNetError> {
    run("ip", args, None)
}

/// Run `iptables <args>`, returning stdout on success.
///
/// Same shape as [`run_ip`]: stdout on success, [`WispNetError::Cmd`]
/// on non-zero exit with stderr captured.
pub fn run_iptables(args: &[&str]) -> Result<String, WispNetError> {
    run("iptables", args, None)
}

/// Run `iptables-restore --noflush`, feeding `rules` on stdin.
///
/// `--noflush` keeps any pre-existing rules in place; we only ADD the
/// wisp rules. iptables-restore is preferred over per-rule `iptables -A`
/// for atomicity, but dispatch A's planner emits per-rule shape so the
/// `iptables::apply` path uses [`run_iptables`] directly. This helper
/// is kept for future batch-apply paths.
pub fn run_iptables_restore(rules: &str) -> Result<(), WispNetError> {
    run("iptables-restore", &["--noflush"], Some(rules)).map(|_| ())
}

fn run(program: &str, args: &[&str], stdin_text: Option<&str>) -> Result<String, WispNetError> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if stdin_text.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    let mut child = cmd.spawn().map_err(|e| WispNetError::Cmd {
        command: joined(program, args),
        exit_code: -1,
        stderr: format!("spawn failed: {e}"),
    })?;

    if let Some(text) = stdin_text {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| WispNetError::Cmd {
                    command: joined(program, args),
                    exit_code: -1,
                    stderr: format!("stdin write failed: {e}"),
                })?;
        }
    }

    let out = child.wait_with_output().map_err(|e| WispNetError::Cmd {
        command: joined(program, args),
        exit_code: -1,
        stderr: format!("wait failed: {e}"),
    })?;

    if !out.status.success() {
        let exit_code = out.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(WispNetError::Cmd {
            command: joined(program, args),
            exit_code,
            stderr,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joined_includes_program_and_args() {
        assert_eq!(joined("ip", &["link", "show"]), "ip link show");
        assert_eq!(joined("iptables", &[]), "iptables");
    }

    #[test]
    #[cfg(unix)]
    fn run_captures_nonzero_exit() {
        // `false` is POSIX's "always exit 1". Used here as a portable
        // way to assert the error path without depending on iproute2.
        let err = run("false", &[], None).unwrap_err();
        match err {
            WispNetError::Cmd {
                command, exit_code, ..
            } => {
                assert_eq!(command, "false");
                assert_eq!(exit_code, 1);
            }
            other => panic!("expected Cmd error, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn run_returns_stdout_on_success() {
        let out = run("/bin/echo", &["hello"], None).unwrap();
        assert_eq!(out.trim(), "hello");
    }
}
