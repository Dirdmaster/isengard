//! Channel abstraction for the notifier. Each channel decides which event
//! kinds it wants and how to send them. v1 supports exact-string match;
//! glob patterns defer to v1.x.

use async_trait::async_trait;
use isengard_core::Event;

#[async_trait]
pub trait NotifyChannel: Send + Sync {
    /// Stable channel name for logging.
    fn name(&self) -> &'static str;

    /// True if this channel should receive an event of the given kind.
    fn matches_kind(&self, kind: &str) -> bool;

    /// Dispatch a single event. Errors are logged + dropped by the caller.
    async fn send(&self, event: &Event) -> anyhow::Result<()>;
}
