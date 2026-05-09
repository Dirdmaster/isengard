//! Host-global networking sysctls the runtime nudges before bringing
//! up a wisp network.
//!
//! Phase 0.3 ships a single helper, [`ensure_global_ip_forward`],
//! which writes `/proc/sys/net/ipv4/ip_forward = 1`. Without that the
//! kernel silently drops packets that traverse the bridge -> wider
//! network direction even though the iptables FORWARD rules accept
//! them; the symptom is a hung curl on `--port` traffic.
//!
//! Other host-wide knobs (`net.bridge.bridge-nf-call-iptables`,
//! `route_localnet` per-bridge) are handled inside `wisp-net::bridge`
//! at bridge-create time; this module only owns the truly global ones
//! that don't naturally belong to a single bridge's lifecycle.

use crate::error::{Result, WispError};

/// Write `/proc/sys/net/ipv4/ip_forward = 1`.
///
/// Idempotent: if the value is already `1` the kernel accepts the
/// write as a no-op. On a host where `/proc` is missing (Mac, certain
/// containerised CIs) this returns a `WispError::Lifecycle` that the
/// caller can choose to log-and-continue: `wisp-net`'s
/// integration-test harness sets the sysctl manually anyway, so
/// `Runtime::start_with_attacher` only invokes this best-effort.
pub fn ensure_global_ip_forward() -> Result<()> {
    let path = "/proc/sys/net/ipv4/ip_forward";
    std::fs::write(path, "1")
        .map_err(|err| WispError::Lifecycle(format!("write {path}=1 failed: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On Mac `/proc` does not exist: the helper should return an
    /// error rather than panicking. Linux call-sites can choose to
    /// ignore the result.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn errors_cleanly_on_non_linux() {
        let res = ensure_global_ip_forward();
        assert!(res.is_err(), "expected error on Mac, got {res:?}");
    }
}
