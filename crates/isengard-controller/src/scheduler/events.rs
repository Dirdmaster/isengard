//! `placement.*` event kind strings.
//!
//! Helpers for the bus event kinds the scheduler emits. Subscribers
//! (journal, dashboard banner, future notifiers) match on these
//! strings; centralising them in one module keeps the producer and
//! consumer sides in lock-step.

/// `placement.*` event kind strings carried on `Event.kind`.
pub mod kind {
    /// A new replica was created on a host.
    pub const CREATED: &str = "placement.created";
    /// A replica was drained from a host.
    pub const REMOVED: &str = "placement.removed";
    /// A spread placement couldn't fill the requested count.
    pub const DEGRADED: &str = "placement.degraded";
    /// No host was eligible under the placement's `where:` selector.
    pub const NO_ELIGIBLE_HOSTS: &str = "placement.no_eligible_hosts";
    /// `on: <host>` named a host that isn't enrolled.
    pub const UNKNOWN_HOST: &str = "placement.unknown_host";
    /// `on: <host>` pointed at an enrolled host that is now
    /// disconnected.
    pub const HOST_GONE: &str = "placement.host_gone";
    /// Two placements claim the same replica slot.
    pub const DUPLICATE: &str = "placement.duplicate";
    /// A previously-Pending placement got auto-placed after a label
    /// or enrol event.
    pub const AUTO_PLACED: &str = "placement.auto_placed";
}
