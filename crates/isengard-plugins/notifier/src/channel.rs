//! Channel abstraction and shared rendering for the notifier.
//!
//! Each channel declares which event kinds it wants and how to send a
//! formatted event. [`format_event`] produces the plain-text body every
//! channel reuses. [`RateLimited`] wraps any channel with a token-bucket
//! limiter plus overflow batching: when the bucket runs dry, events
//! queue locally and flush as one `notifier.batch` event on the next
//! allowed send.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use isengard_core::Event;
use tracing::warn;

/// One sink the notifier can fan events into.
///
/// Implementations decide their filter (`matches_kind`) and their send
/// path. Errors bubbled out of `send` are logged at warn by the
/// dispatch loop and never break the loop.
#[async_trait]
pub trait NotifyChannel: Send + Sync {
    /// Stable channel name surfaced in logs.
    fn name(&self) -> &'static str;
    /// Returns true when this channel wants `kind`.
    fn matches_kind(&self, kind: &str) -> bool;
    /// Renders and sends `event`. May log internally; the caller still
    /// expects a `Result`.
    async fn send(&self, event: &Event) -> anyhow::Result<()>;
}

/// One button on an interactive message.
///
/// Channel-agnostic shape: Telegram serializes this as an inline
/// keyboard cell, Discord as a `Button` component in an action row.
/// `text` is what the user sees; `callback_data` is opaque payload
/// echoed back when the button is clicked. Telegram caps
/// `callback_data` at 64 bytes; Discord caps `custom_id` at 100.
/// Neither limit is enforced here, so keep payloads short at the
/// call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineButton {
    /// User-visible button label.
    pub text: String,
    /// Opaque payload the channel echoes back on click.
    pub callback_data: String,
}

impl InlineButton {
    /// Builds a button from any `Into<String>` pair.
    pub fn new(text: impl Into<String>, callback_data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: callback_data.into(),
        }
    }
}

/// Blanket impl: an `Arc`-shared channel is itself a [`NotifyChannel`].
///
/// Lets the notifier hold the same Telegram or Discord instance both
/// directly (for typed methods like
/// [`crate::telegram::TelegramChannel::send_inline_keyboard`]) and
/// behind a [`RateLimited`] wrapper for the fan-out loop.
#[async_trait]
impl<C: NotifyChannel + ?Sized> NotifyChannel for Arc<C> {
    fn name(&self) -> &'static str {
        (**self).name()
    }
    fn matches_kind(&self, kind: &str) -> bool {
        (**self).matches_kind(kind)
    }
    async fn send(&self, event: &Event) -> anyhow::Result<()> {
        (**self).send(event).await
    }
}

/// Renders an [`Event`] to multi-line plain text shared across channels.
///
/// Output begins with `[isengard] <kind>` then appends a line per
/// populated field (container, image, digest delta, error, summary).
/// Blank lines are dropped.
pub fn format_event(event: &Event) -> String {
    let mut lines = vec![format!("[isengard] {}", event.kind)];
    if let Some(c) = &event.container_name {
        lines.push(format!("container: {c}"));
    }
    if let Some(i) = &event.image {
        lines.push(format!("image: {i}"));
    }
    match (event.old_digest.as_deref(), event.new_digest.as_deref()) {
        (Some(o), Some(n)) => lines.push(format!("digest: {o} → {n}")),
        (None, Some(n)) => lines.push(format!("digest: {n}")),
        (Some(o), None) => lines.push(format!("digest (old): {o}")),
        (None, None) => {}
    }
    if let Some(e) = &event.error {
        lines.push(format!("error: {e}"));
    }
    if !event.summary.is_empty() {
        lines.push(event.summary.clone());
    }
    lines.join("\n")
}

/// Default token refill rate when the operator doesn't configure one.
const DEFAULT_TOKENS_PER_MINUTE: f64 = 10.0;

/// Token bucket backing [`RateLimited`].
///
/// Refill rate is `tokens_per_minute / 60` per second. Capacity equals
/// the configured `tokens_per_minute`. `try_acquire` returns true and
/// debits a token when one is available, false otherwise.
struct Bucket {
    /// Tokens currently available.
    tokens: f64,
    /// Maximum tokens this bucket holds.
    capacity: f64,
    /// Tokens added per real-time second.
    refill_per_sec: f64,
    /// Wall-clock instant of the last refill pass.
    last_refill: Instant,
}

impl Bucket {
    /// Creates a full bucket sized by `tokens_per_minute`.
    fn new(tokens_per_minute: f64) -> Self {
        Self {
            tokens: tokens_per_minute,
            capacity: tokens_per_minute,
            refill_per_sec: tokens_per_minute / 60.0,
            last_refill: Instant::now(),
        }
    }

    /// Tops the bucket up based on elapsed real time, capped at
    /// [`Self::capacity`].
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last_refill = now;
    }

    /// Returns true and consumes a token when one is available.
    ///
    /// Refills first, then checks.
    fn try_acquire(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Wraps a [`NotifyChannel`] with a token-bucket limiter and overflow
/// batching.
///
/// When tokens are available, `send` passes through. When the bucket
/// runs dry the event queues locally; the next allowed send flushes
/// the queue as one `notifier.batch` summary then delivers the new
/// event. Operators see one batched message instead of N missed
/// notifications.
pub struct RateLimited<C: NotifyChannel> {
    /// Underlying channel.
    inner: C,
    /// Token bucket guarding the channel.
    bucket: Mutex<Bucket>,
    /// Events held over until tokens free up.
    overflow: Mutex<Vec<Event>>,
}

impl<C: NotifyChannel> RateLimited<C> {
    /// Builds a limiter around `inner` with the supplied refill rate.
    ///
    /// `tokens_per_minute <= 0.0` falls back to
    /// [`DEFAULT_TOKENS_PER_MINUTE`].
    pub fn new(inner: C, tokens_per_minute: f64) -> Self {
        let tpm = if tokens_per_minute > 0.0 {
            tokens_per_minute
        } else {
            DEFAULT_TOKENS_PER_MINUTE
        };
        Self {
            inner,
            bucket: Mutex::new(Bucket::new(tpm)),
            overflow: Mutex::new(Vec::new()),
        }
    }

    /// Attempts to debit one token from the bucket.
    fn try_acquire(&self) -> bool {
        self.bucket.lock().unwrap().try_acquire()
    }

    /// Pushes one event onto the overflow queue.
    fn queue(&self, event: Event) {
        self.overflow.lock().unwrap().push(event);
    }

    /// Drains the overflow queue and returns its contents.
    fn take_queue(&self) -> Vec<Event> {
        std::mem::take(&mut *self.overflow.lock().unwrap())
    }
}

#[async_trait]
impl<C: NotifyChannel> NotifyChannel for RateLimited<C> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn matches_kind(&self, kind: &str) -> bool {
        self.inner.matches_kind(kind)
    }

    async fn send(&self, event: &Event) -> anyhow::Result<()> {
        if self.try_acquire() {
            let queue = self.take_queue();
            if !queue.is_empty() {
                let batched = batch_event(&queue);
                if let Err(e) = self.inner.send(&batched).await {
                    warn!(channel = self.inner.name(), error = %e, "batched flush send failed");
                }
            }
            self.inner.send(event).await
        } else {
            self.queue(event.clone());
            Ok(())
        }
    }
}

/// Collapses a queued batch of events into one synthetic
/// `notifier.batch` event.
///
/// The summary leads with the count, then one `- <kind> (<container>)`
/// line per event.
fn batch_event(events: &[Event]) -> Event {
    let mut summary_lines = vec![format!("{} events in last minute:", events.len())];
    for e in events {
        let line = if let Some(c) = &e.container_name {
            format!("- {} ({})", e.kind, c)
        } else {
            format!("- {}", e.kind)
        };
        summary_lines.push(line);
    }
    Event {
        kind: "notifier.batch".into(),
        occurred_at: chrono::Utc::now(),
        summary: summary_lines.join("\n"),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::Arc;

    fn ev(kind: &str) -> Event {
        Event {
            kind: kind.into(),
            occurred_at: Utc::now(),
            summary: kind.into(),
            container_name: Some("c".into()),
            ..Default::default()
        }
    }

    #[test]
    fn format_event_includes_kind_and_container() {
        let s = format_event(&ev("update.success"));
        assert!(s.contains("[isengard] update.success"));
        assert!(s.contains("container: c"));
    }

    #[test]
    fn bucket_starts_full() {
        let mut b = Bucket::new(10.0);
        for _ in 0..10 {
            assert!(b.try_acquire());
        }
        assert!(!b.try_acquire());
    }

    #[test]
    fn bucket_refills_over_time() {
        let mut b = Bucket::new(60.0);
        for _ in 0..60 {
            assert!(b.try_acquire());
        }
        assert!(!b.try_acquire());
        b.last_refill = Instant::now() - std::time::Duration::from_secs(2);
        assert!(b.try_acquire());
        assert!(b.try_acquire());
    }

    /// Test channel that records every event it sees.
    struct Recorder {
        sent: Mutex<Vec<Event>>,
    }
    impl Recorder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
            })
        }
        fn snapshot(&self) -> Vec<Event> {
            self.sent.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl NotifyChannel for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }
        fn matches_kind(&self, _kind: &str) -> bool {
            true
        }
        async fn send(&self, event: &Event) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn rate_limited_passes_through_under_limit() {
        let rec = Recorder::new();
        let limiter = RateLimited::new(rec.clone(), 10.0);
        for i in 0..5 {
            limiter.send(&ev(&format!("k{i}"))).await.unwrap();
        }
        assert_eq!(rec.snapshot().len(), 5);
    }

    #[tokio::test]
    async fn rate_limited_queues_then_batches() {
        let rec = Recorder::new();
        let limiter = RateLimited::new(rec.clone(), 2.0);
        for i in 0..7 {
            limiter.send(&ev(&format!("k{i}"))).await.unwrap();
        }
        let snap = rec.snapshot();
        assert_eq!(snap.len(), 2);
        {
            let mut b = limiter.bucket.lock().unwrap();
            b.last_refill = Instant::now() - std::time::Duration::from_secs(60);
        }
        limiter.send(&ev("trigger")).await.unwrap();
        let snap = rec.snapshot();
        assert_eq!(snap.len(), 4);
        let batched = &snap[2];
        assert_eq!(batched.kind, "notifier.batch");
        assert!(batched.summary.starts_with("5 events in last minute:"));
    }
}
