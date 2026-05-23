//! In-process broadcast bus for proxy lifecycle events.
//!
//! The deployment driver (Task 8) subscribes here to react to upstream
//! health flips immediately after a blue/green switch, without waiting
//! for the controller round-trip via the journal.
//!
//! Semantics: tokio `broadcast` channel: late subscribers miss any
//! events published before they called `subscribe`. That's deliberate;
//! the driver subscribes during deploy planning, before the switch.

use tokio::sync::broadcast;

/// Lifecycle events the proxy emits to in-process subscribers. Currently
/// only `UpstreamHealthChanged`; more variants land as other subsystems
/// (cert renewal, eviction, config apply) need real-time fan-out.
#[derive(Debug, Clone)]
pub enum ProxyEvent {
    /// One upstream's healthcheck transitioned. Emitted exactly once
    /// per state change (not per probe tick).
    UpstreamHealthChanged {
        /// Hostname the upstream is registered under.
        public_hostname: String,
        /// Backing container id.
        container_id: String,
        /// New health state.
        healthy: bool,
    },
}

/// Channel capacity. 64 is plenty: events are produced by the
/// healthcheck loop (one per transition, not per tick) and consumed by
/// at most a handful of long-lived subscribers.
const CHANNEL_CAPACITY: usize = 64;

/// Thin wrapper around a `broadcast::Sender<ProxyEvent>`. Cheap to
/// clone: the inner sender is `Clone` and shared between all holders.
#[derive(Debug, Clone)]
pub struct ProxyEventBus {
    /// Sender half of the broadcast channel. Clones share state.
    tx: broadcast::Sender<ProxyEvent>,
}

impl ProxyEventBus {
    /// Construct a fresh bus with `CHANNEL_CAPACITY` slots.
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Best-effort publish. Returns `Err` only if there are zero live
    /// receivers; we ignore that because lifecycle events with no
    /// subscriber are uninteresting.
    pub fn publish(&self, ev: ProxyEvent) {
        let _ = self.tx.send(ev);
    }

    /// Subscribe to receive future events. The returned receiver only
    /// sees events published *after* this call (broadcast semantics).
    pub fn subscribe(&self) -> broadcast::Receiver<ProxyEvent> {
        self.tx.subscribe()
    }
}

impl Default for ProxyEventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// Subscriber that exists at publish time receives the event.
    #[tokio::test]
    async fn subscribe_then_publish_receives() {
        let bus = ProxyEventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(ProxyEvent::UpstreamHealthChanged {
            public_hostname: "app.example.com".into(),
            container_id: "abc123".into(),
            healthy: true,
        });
        let ev = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("recv ok");
        match ev {
            ProxyEvent::UpstreamHealthChanged {
                public_hostname,
                container_id,
                healthy,
            } => {
                assert_eq!(public_hostname, "app.example.com");
                assert_eq!(container_id, "abc123");
                assert!(healthy);
            }
        }
    }

    /// A late subscriber misses events published before it subscribed
    /// (broadcast semantics: confirms we're not accidentally a replay
    /// channel). The second publish, after subscription, is delivered.
    #[tokio::test]
    async fn publish_then_subscribe_misses() {
        let bus = ProxyEventBus::new();
        // Lost: no subscribers yet.
        bus.publish(ProxyEvent::UpstreamHealthChanged {
            public_hostname: "lost.example.com".into(),
            container_id: "lost".into(),
            healthy: false,
        });
        let mut rx = bus.subscribe();
        // Delivered: subscriber is live.
        bus.publish(ProxyEvent::UpstreamHealthChanged {
            public_hostname: "kept.example.com".into(),
            container_id: "kept".into(),
            healthy: true,
        });
        let ev = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("recv ok");
        match ev {
            ProxyEvent::UpstreamHealthChanged {
                public_hostname, ..
            } => {
                assert_eq!(
                    public_hostname, "kept.example.com",
                    "should only see the post-subscribe publish"
                );
            }
        }
        // No further events queued.
        let next = timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(next.is_err(), "expected timeout, got {next:?}");
    }
}
