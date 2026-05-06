//! Channel abstraction for the notifier. Each channel decides which event
//! kinds it wants and how to send them. Shared `format_event` produces the
//! plain-text message body all channels send. `RateLimited<C>` wraps any
//! channel with a token-bucket limiter + overflow batching.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use isengard_core::Event;
use tracing::warn;

#[async_trait]
pub trait NotifyChannel: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches_kind(&self, kind: &str) -> bool;
    async fn send(&self, event: &Event) -> anyhow::Result<()>;
}

/// One button on an interactive message (Telegram inline keyboard, Discord
/// component button in 9g). The exact wire serialization is per-channel; this
/// is the channel-agnostic shape consumers build.
///
/// `text` is what the user sees; `callback_data` is opaque payload echoed
/// back when the button is clicked. Telegram caps `callback_data` at 64
/// bytes; keep it short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineButton {
    pub text: String,
    pub callback_data: String,
}

impl InlineButton {
    pub fn new(text: impl Into<String>, callback_data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: callback_data.into(),
        }
    }
}

/// Blanket: an `Arc`-shared channel is itself a `NotifyChannel`. Lets the
/// notifier hold the same instance both directly (for typed methods, e.g.
/// `TelegramChannel::send_inline_keyboard`) and inside a `RateLimited<...>`
/// wrapper for the existing one-way fan-out path.
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

/// Plain-text format used by every channel. Multi-line, blank-line free.
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

const DEFAULT_TOKENS_PER_MINUTE: f64 = 10.0;

struct Bucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last_refill: Instant,
}

impl Bucket {
    fn new(tokens_per_minute: f64) -> Self {
        Self {
            tokens: tokens_per_minute,
            capacity: tokens_per_minute,
            refill_per_sec: tokens_per_minute / 60.0,
            last_refill: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last_refill = now;
    }

    /// Returns true if a token was available + consumed.
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

/// Wraps any NotifyChannel with a token-bucket limiter. Overflow events
/// queue locally; on the next send attempt where tokens are available, the
/// queue drains as one batched message.
pub struct RateLimited<C: NotifyChannel> {
    inner: C,
    bucket: Mutex<Bucket>,
    overflow: Mutex<Vec<Event>>,
}

impl<C: NotifyChannel> RateLimited<C> {
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

    fn try_acquire(&self) -> bool {
        self.bucket.lock().unwrap().try_acquire()
    }

    fn queue(&self, event: Event) {
        self.overflow.lock().unwrap().push(event);
    }

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
            // Token available: send this event AND flush any queued overflow as a batched message.
            let queue = self.take_queue();
            if !queue.is_empty() {
                let batched = batch_event(&queue);
                if let Err(e) = self.inner.send(&batched).await {
                    warn!(channel = self.inner.name(), error = %e, "batched flush send failed");
                }
            }
            self.inner.send(event).await
        } else {
            // No token: queue this event, return Ok (caller doesn't need to know it was deferred).
            self.queue(event.clone());
            Ok(())
        }
    }
}

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
        let mut b = Bucket::new(60.0); // 1 token / sec
        for _ in 0..60 {
            assert!(b.try_acquire());
        }
        assert!(!b.try_acquire());
        // Force time forward by mutating last_refill
        b.last_refill = Instant::now() - std::time::Duration::from_secs(2);
        assert!(b.try_acquire());
        assert!(b.try_acquire());
    }

    /// Test channel that records calls.
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
        let limiter = RateLimited::new(rec.clone(), 2.0); // 2/min — basically nothing left after 2
        // First 2 go through, next 5 queue.
        for i in 0..7 {
            limiter.send(&ev(&format!("k{i}"))).await.unwrap();
        }
        // After 7 sends with capacity 2, only the 2 initial + nothing else (queue grew, no token to flush).
        let snap = rec.snapshot();
        assert_eq!(snap.len(), 2);
        // Force a refill burst by sleeping (1 second at 2/min = ~0.033 token, not enough).
        // Instead: poke the bucket directly via a fresh acquire after manipulating time.
        // Simpler assertion: queue length is now 5.
        // Internal state is private; assert behaviour via another send after refill.
        // Mutate: set last_refill backward to allow 1 token + flush.
        {
            let mut b = limiter.bucket.lock().unwrap();
            b.last_refill = Instant::now() - std::time::Duration::from_secs(60);
        }
        // One more send should trigger the flush of overflow queue + this event.
        limiter.send(&ev("trigger")).await.unwrap();
        let snap = rec.snapshot();
        // Expect: 2 initial + 1 batched + 1 "trigger" = 4 entries on recorder.
        assert_eq!(snap.len(), 4);
        let batched = &snap[2];
        assert_eq!(batched.kind, "notifier.batch");
        assert!(batched.summary.starts_with("5 events in last minute:"));
    }
}
