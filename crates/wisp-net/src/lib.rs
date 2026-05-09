//! Wisp networking: bridge + veth + IPAM + iptables for wisp-runtime
//! containers. Local fabric only; cross-host overlay is the agent's
//! existing adapter layer.
//!
//! Phase 0.3 dispatch A lands the side-effect-free skeleton: error type,
//! IPAM with persistence, and pure planners for `ip` and `iptables`. The
//! actual syscalls (real `ip link add`, `iptables -A ...`) land in
//! dispatch B alongside Linux-only integration tests.

pub mod bridge;
pub mod error;
pub mod ipam;
pub mod iptables;

pub use bridge::{IpCommand, Network, plan_create, plan_delete};
pub use error::WispNetError;
pub use ipam::{Ipam, StaticBitmapIpam};
pub use iptables::{
    PortProtocol, PortPublish, Rule, RuleSet, Table, plan_for_attachment, plan_for_network,
};
