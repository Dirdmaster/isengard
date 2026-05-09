//! `NetworkAttacher`: the trait the wisp runtime calls during
//! `start_container` (post-clone, pre-ready) to wire a container into
//! a bridge / veth / iptables setup.
//!
//! `wisp` deliberately does not depend on `wisp-net`: the runtime
//! owns the protocol (the [`NetworkSpec`] / [`NetworkAttachmentRecord`]
//! data shape), and `wisp-net` consumes it. The execution layer is
//! exposed through this trait so any concrete networking provider
//! (the production `wisp-net` impl, an integration-test fake, or a
//! future cross-host adapter) can be wired in without `wisp`
//! pulling in those dependencies.
//!
//! # Lifecycle
//!
//! - Parent calls [`NetworkAttacher::attach`] AFTER `clone3` returns
//!   a child PID, AFTER `cgroup.add_pid`, and BEFORE signalling the
//!   child to proceed with `pivot_root` / `execvpe`. The child is
//!   blocked on the ready pipe until the parent finishes attach;
//!   inside the child's network namespace, the attacher must move
//!   the veth, configure addressing, and bring `lo` + `eth0` up.
//! - Parent calls [`NetworkAttacher::detach`] during
//!   [`crate::runtime::Runtime::delete_with_attacher`]. The container
//!   process is already gone (or being killed) by then, so detach is
//!   purely host-side: revoke iptables rules, delete the host veth,
//!   release the IP.
//!
//! # Errors
//!
//! Failures bubble up as [`crate::error::WispError::Lifecycle`] with
//! the underlying error rendered into the message. `wisp` cannot
//! reach into a richer error type without depending on `wisp-net`,
//! and the `Lifecycle` variant is already the catch-all for
//! orchestration faults.

use std::path::Path;

use crate::error::WispError;
use crate::network_spec::{NetworkAttachmentRecord, NetworkSpec};

/// Pluggable network attach / detach hook for the wisp runtime.
///
/// Implementations live in `wisp-net` (production) or in test code
/// (`tests/runtime_with_network.rs`). `&mut self` is the contract:
/// implementations typically hold IPAM state and a state directory,
/// both of which need exclusive access during attach / detach.
pub trait NetworkAttacher {
    /// Attach the container to the network described by `spec`.
    ///
    /// Called by `start_container` after `clone3` + `cgroup.add_pid`
    /// but BEFORE `wait_ready`. Implementations are expected to:
    ///
    /// 1. Lazily ensure the bridge exists (idempotent).
    /// 2. Allocate an IPv4 address for `container_id`.
    /// 3. Create a veth pair, enslave the host side to the bridge,
    ///    move the container side into `container_pid`'s netns,
    ///    rename to `eth0`, configure addressing, route default via
    ///    the gateway.
    /// 4. Apply the iptables rules for `spec.ports`.
    /// 5. (Optional) Write `<rootfs>/etc/resolv.conf` and
    ///    `<rootfs>/etc/hosts` per `spec.resolv_source`. The rootfs
    ///    is on the host filesystem at this point because the child
    ///    has not yet `pivot_root`'d.
    ///
    /// The returned [`NetworkAttachmentRecord`] is persisted into
    /// `state.json` so that `delete` can reverse the operation.
    fn attach(
        &mut self,
        spec: &NetworkSpec,
        container_id: &str,
        container_pid: u32,
        rootfs: &Path,
    ) -> Result<NetworkAttachmentRecord, WispError>;

    /// Reverse [`attach`]: revoke iptables rules, delete the host
    /// veth, release the IP. Called by `Runtime::delete_with_attacher`.
    ///
    /// Should be tolerant of partial state (e.g. iptables rules
    /// already gone, veth already removed): a wisp container may have
    /// been delete-forced after a crash and the kernel may have
    /// reclaimed some of these resources already.
    fn detach(&mut self, record: &NetworkAttachmentRecord) -> Result<(), WispError>;
}
