//! Bridge-scoped IPv4 allocator. Real implementation lands in commit 2.

use crate::error::WispNetError;
use std::collections::BTreeMap;
use std::net::Ipv4Addr;

/// Bridge-scoped IPv4 allocator. Each `network_name` has its own pool
/// scoped to a `subnet`; allocations are keyed by `bundle_id` so callers
/// can release by id and so `list` returns id -> ip pairs.
pub trait Ipam: Send {
    fn alloc(
        &mut self,
        network_name: &str,
        subnet: ipnet::Ipv4Net,
        gateway: Ipv4Addr,
        bundle_id: &str,
    ) -> Result<Ipv4Addr, WispNetError>;

    fn release(&mut self, network_name: &str, bundle_id: &str) -> Result<(), WispNetError>;

    fn list(&self, network_name: &str) -> Result<BTreeMap<String, Ipv4Addr>, WispNetError>;
}

/// Stub. Real implementation lands in commit 2 (`feat(wisp-net):
/// static-bitmap IPAM with persistence`).
pub struct StaticBitmapIpam {
    _placeholder: (),
}

impl StaticBitmapIpam {
    pub fn new(_state_dir: &std::path::Path) -> Self {
        Self { _placeholder: () }
    }
}
