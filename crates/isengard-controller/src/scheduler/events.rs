//! `placement.*` event helpers (created, removed, degraded, etc.).
//! Filled in at step 6.

/// Phase 0.14 placement event kinds. These are the strings the bus
/// carries on `Event.kind`; subscribers (journal, dashboard banner,
/// future notifiers) match on them.
pub mod kind {
    pub const CREATED: &str = "placement.created";
    pub const REMOVED: &str = "placement.removed";
    pub const DEGRADED: &str = "placement.degraded";
    pub const NO_ELIGIBLE_HOSTS: &str = "placement.no_eligible_hosts";
    pub const UNKNOWN_HOST: &str = "placement.unknown_host";
    pub const HOST_GONE: &str = "placement.host_gone";
    pub const DUPLICATE: &str = "placement.duplicate";
    pub const AUTO_PLACED: &str = "placement.auto_placed";
}
