//! In-process event bus. Publishers (the gRPC handler, internal tasks) call
//! `publish`; subscribers (controller-side plugins) call `subscribe` to get
//! a `broadcast::Receiver`.
//!
//! Capacity is generous (1024). Slow subscribers will Lag-and-drop, which
//! is the right semantics — a stuck notifier mustn't backpressure the
//! journal write path.

use std::sync::Arc;

use isengard_core::Event;
use tokio::sync::broadcast;

const BUS_CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct EventBus {
    inner: Arc<broadcast::Sender<Event>>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BUS_CAPACITY);
        Self {
            inner: Arc::new(tx),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.subscribe()
    }

    /// Publish to all current subscribers. Errors mean nobody is listening
    /// — that's normal at startup and we don't surface it.
    pub fn publish(&self, event: Event) {
        let _ = self.inner.send(event);
    }

    pub fn subscriber_count(&self) -> usize {
        self.inner.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn ev(kind: &str) -> Event {
        Event {
            kind: kind.into(),
            occurred_at: Utc::now(),
            summary: kind.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_does_not_error() {
        let bus = EventBus::new();
        bus.publish(ev("update.checked"));
        // No assertion — just exercising the path.
    }

    #[tokio::test]
    async fn one_subscriber_receives_published_event() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(ev("update.success"));
        let received = rx.recv().await.unwrap();
        assert_eq!(received.kind, "update.success");
    }

    #[tokio::test]
    async fn multiple_subscribers_each_receive_event() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        bus.publish(ev("update.failed"));
        assert_eq!(rx1.recv().await.unwrap().kind, "update.failed");
        assert_eq!(rx2.recv().await.unwrap().kind, "update.failed");
    }

    #[tokio::test]
    async fn subscriber_count_reflects_active_receivers() {
        let bus = EventBus::new();
        assert_eq!(bus.subscriber_count(), 0);
        let _r1 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);
        let _r2 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);
        drop(_r1);
        assert_eq!(bus.subscriber_count(), 1);
    }
}
