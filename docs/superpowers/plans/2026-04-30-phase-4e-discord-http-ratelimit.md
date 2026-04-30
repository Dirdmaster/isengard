# Phase 4e: Discord + HTTP POST Channels + Rate Limiting + Batching

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Two more channels (Discord webhooks, generic HTTP POST) and a rate limiter wrapping every channel. End state: configure any combination of Telegram + Discord + HTTP, receive notifications on each, and never spam more than `tokens_per_minute` events per channel — overflow gets batched into a single message ("3 events in last minute: ...").

**Architecture:** Two new channel impls (`DiscordChannel`, `HttpChannel`) alongside `TelegramChannel`. A `RateLimited<C: NotifyChannel>` decorator wraps any channel — token bucket per channel, refill rate from config, queue overflow into an internal `Vec<Event>` flushed on next refill. The notifier `Notifier::init` builds raw channels from config, then wraps each in `RateLimited` with per-channel limits.

**Tech stack:** No new workspace deps (`reqwest` for HTTP, `serde_json` for the body, both already present).

**Branch:** `next`. Lefthook pre-push runs full gates (cargo-deny mandatory).

**Spec:** `docs/superpowers/specs/2026-04-30-phase-4-events-journal-design.md` §8 (per-channel rate limit + batched messages).

---

## Scope

**In:**
- `DiscordChannel`:
  - Config: `webhook_url: String` + `kinds: Vec<String>`
  - Send: `POST <webhook_url>` with body `{"content": "<formatted text>"}`. Discord webhook accepts plain text in `content`; max 2000 chars (truncate with `... [truncated]` if exceeded).
  - Format: same plain-text format as Telegram's `format_event` — extract that helper into `channel.rs` so all channels share it.
- `HttpChannel`:
  - Config: `url: String` + `headers: HashMap<String, String>` (optional, default empty) + `body_template: Option<String>` (optional; default `{"text": "<formatted text>"}` JSON) + `kinds: Vec<String>`
  - Send: `POST <url>` with configured headers + body. v1 just sends `{"text": format_event(event)}`; templating is deferred unless the user provides `body_template` containing `{{text}}` and `{{kind}}` placeholders (use simple `str::replace`, no full template engine).
- `RateLimited<C: NotifyChannel>` decorator:
  - Wraps any channel + a per-channel `tokens_per_minute` (default 10).
  - Internally: `Mutex<TokenBucket>` (current tokens f64, last_refill Instant) + `Mutex<Vec<Event>>` (pending overflow queue).
  - On `send(event)`: try to acquire 1 token. If success → forward to inner. If not → push event to overflow queue. After every send attempt, check if any tokens are now available AND queue is non-empty → drain queue into one batched event (`Event { kind: "notifier.batch", summary: "N events in last minute: ...", ... }`) and forward.
  - Spawned background flush task NOT needed — flush happens lazily on next event arrival or on a periodic ticker. v1 uses lazy flush; document trade-off.
- Move `format_event` from `telegram.rs` to `channel.rs` (it's channel-agnostic). Telegram + Discord + HTTP all use it.
- Notifier `init`: build channels per config, wrap each in `RateLimited`. Expose `tokens_per_minute` on each channel's config (default 10).
- Unit tests:
  - `format_event` (already covered, just moved)
  - `DiscordChannel`: matches_kind, format truncation at 2000 chars
  - `HttpChannel`: matches_kind, default body shape, custom body_template substitution
  - `RateLimited`: under-limit forwards, over-limit queues, batched flush on refill
- Integration tests via wiremock:
  - `discord_e2e`: POST to mock webhook URL, verify body shape `{"content": "..."}`
  - `http_e2e`: POST to mock URL, verify body + custom header pass-through

**Out (deferred to v1.x):**
- Persistent overflow queue across restarts (in-memory only)
- Per-event priority (some events bypass rate limit)
- Discord embeds with rich formatting
- Webhook signing on outbound HTTP
- HMAC validation on the receiving side
- Slack channel (still in v1.x backlog)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes — baseline 127 + new tests
3. `just ci-local` clean (cargo-deny mandatory)
4. Tag `v0.1.0-alpha.phase4e` set locally
5. **Not pushed** until user confirms

---

## File Structure

```
crates/isengard-plugins/notifier/
└── src/
    ├── lib.rs              # MODIFY: NotifierConfig grows (discord, http); init wires RateLimited around each channel
    ├── channel.rs          # MODIFY: + format_event (moved from telegram); + RateLimited<C> decorator
    ├── telegram.rs         # MODIFY: drop local format_event (use channel::format_event); per-channel tokens_per_minute config
    ├── discord.rs          # NEW: DiscordChannel impl
    └── http.rs             # NEW: HttpChannel impl

crates/isengard-plugins/notifier/tests/
├── telegram_e2e.rs         # UNCHANGED (still passes)
├── discord_e2e.rs          # NEW: wiremock test
└── http_e2e.rs             # NEW: wiremock test
```

---

## Task 1: Move format_event to channel.rs + add RateLimited decorator

**Files:**
- Modify: `crates/isengard-plugins/notifier/src/channel.rs`
- Modify: `crates/isengard-plugins/notifier/src/telegram.rs` (remove local format_event, import from channel)

- [ ] **Step 1: Add format_event + RateLimited to channel.rs**

In `crates/isengard-plugins/notifier/src/channel.rs`, replace the file content with:

```rust
//! Channel abstraction for the notifier. Each channel decides which event
//! kinds it wants and how to send them. Shared `format_event` produces the
//! plain-text message body all channels send. `RateLimited<C>` wraps any
//! channel with a token-bucket limiter + overflow batching.

use std::sync::Mutex;
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
        let mut b = Bucket::new(60.0);  // 1 token / sec
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
            Arc::new(Self { sent: Mutex::new(Vec::new()) })
        }
        fn snapshot(&self) -> Vec<Event> {
            self.sent.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl NotifyChannel for Arc<Recorder> {
        fn name(&self) -> &'static str { "recorder" }
        fn matches_kind(&self, _kind: &str) -> bool { true }
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
        let limiter = RateLimited::new(rec.clone(), 2.0);  // 2/min — basically nothing left after 2
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
```

- [ ] **Step 2: Update telegram.rs to import format_event from channel**

In `crates/isengard-plugins/notifier/src/telegram.rs`:

1. Remove the local `pub fn format_event` definition.
2. Add `use crate::channel::format_event;` at the top.
3. The `send` method already calls `format_event(event)` — the import change makes it use the moved version. No body changes needed.
4. The unit tests in telegram.rs that exercise `format_event` directly should now `use crate::channel::format_event;` instead of `use super::format_event;`.

If `tokens_per_minute` is in `TelegramConfig`, add it as `Option<f64>` with default None (channel.rs decides). For now, leave TelegramConfig as-is — we add per-channel rate-limit config in Task 4 when wiring `RateLimited` in Notifier::init.

Actually, to keep this task minimal: just move the `format_event` declaration. Per-channel rate-limit config goes in Task 4.

- [ ] **Step 3: Build + tests**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-notifier 2>&1 | tail -5
cd ~/Projects/isengard && cargo test -p isengard-plugin-notifier 2>&1 | tail -15
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-notifier --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean. Test count grows: 1 (notifier_new) + 6 (telegram, with `format_event` re-imported) + ~6 (channel.rs new tests) = ~13.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/notifier/src/channel.rs crates/isengard-plugins/notifier/src/telegram.rs
cd ~/Projects/isengard && git commit -m "feat(notifier): RateLimited channel decorator + shared format_event"
```

**Self-review checklist:**
- [ ] All notifier tests pass
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 2: DiscordChannel

**Files:**
- Create: `crates/isengard-plugins/notifier/src/discord.rs`
- Modify: `crates/isengard-plugins/notifier/src/lib.rs` (`pub mod discord;`)

- [ ] **Step 1: Create discord.rs**

Create `crates/isengard-plugins/notifier/src/discord.rs`:

```rust
//! Discord webhook channel. Outbound-only via incoming webhook URL.
//!
//! Setup: server admin creates a webhook in channel settings → Discord gives
//! a URL like `https://discord.com/api/webhooks/<id>/<token>`. POST `{"content": "<text>"}`
//! to that URL; max 2000 chars in `content`.

use async_trait::async_trait;
use isengard_core::Event;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::channel::{format_event, NotifyChannel};

const DISCORD_CONTENT_MAX: usize = 2000;
const TRUNCATED_SUFFIX: &str = "\n... [truncated]";

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DiscordConfig {
    pub webhook_url: String,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub tokens_per_minute: Option<f64>,
}

pub struct DiscordChannel {
    webhook_url: String,
    kinds: Vec<String>,
    http: Client,
}

#[derive(Debug, Serialize)]
struct DiscordBody<'a> {
    content: &'a str,
}

impl DiscordChannel {
    pub fn from_config(cfg: DiscordConfig) -> anyhow::Result<Self> {
        if cfg.webhook_url.is_empty() {
            return Err(anyhow::anyhow!("discord channel: webhook_url required"));
        }
        Ok(Self {
            webhook_url: cfg.webhook_url,
            kinds: cfg.kinds,
            http: Client::builder()
                .user_agent(concat!("isengard-notifier/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| anyhow::anyhow!("building http client: {e}"))?,
        })
    }
}

fn truncate_for_discord(s: String) -> String {
    if s.len() <= DISCORD_CONTENT_MAX {
        return s;
    }
    let cap = DISCORD_CONTENT_MAX - TRUNCATED_SUFFIX.len();
    let mut t: String = s.chars().take(cap).collect();
    t.push_str(TRUNCATED_SUFFIX);
    t
}

#[async_trait]
impl NotifyChannel for DiscordChannel {
    fn name(&self) -> &'static str {
        "discord"
    }

    fn matches_kind(&self, kind: &str) -> bool {
        self.kinds.iter().any(|k| k == kind)
    }

    async fn send(&self, event: &Event) -> anyhow::Result<()> {
        let content = truncate_for_discord(format_event(event));
        let body = DiscordBody { content: &content };
        match self.http.post(&self.webhook_url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => {
                warn!(status = %resp.status(), "discord webhook non-success");
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "discord webhook failed");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_errors_on_empty_url() {
        let r = DiscordChannel::from_config(DiscordConfig::default());
        assert!(r.is_err());
    }

    #[test]
    fn matches_kind_exact() {
        let cfg = DiscordConfig {
            webhook_url: "https://discord.com/api/webhooks/1/x".into(),
            kinds: vec!["update.success".into()],
            tokens_per_minute: None,
        };
        let ch = DiscordChannel::from_config(cfg).unwrap();
        assert!(ch.matches_kind("update.success"));
        assert!(!ch.matches_kind("update.failed"));
    }

    #[test]
    fn truncate_keeps_short_strings() {
        let s = "hello".to_string();
        let t = truncate_for_discord(s.clone());
        assert_eq!(t, s);
    }

    #[test]
    fn truncate_caps_long_strings_and_appends_marker() {
        let s = "x".repeat(3000);
        let t = truncate_for_discord(s);
        assert!(t.len() <= DISCORD_CONTENT_MAX);
        assert!(t.ends_with(TRUNCATED_SUFFIX));
    }
}
```

- [ ] **Step 2: Add `pub mod discord;` to lib.rs**

After the existing `pub mod telegram;`:

```rust
pub mod discord;
```

- [ ] **Step 3: Build + tests**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-notifier 2>&1 | tail -5
cd ~/Projects/isengard && cargo test -p isengard-plugin-notifier discord:: 2>&1 | tail -10
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-notifier --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: 4 discord tests pass. Clippy clean (the `pub mod discord;` exposing public items shouldn't trigger dead_code).

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/notifier/src/discord.rs crates/isengard-plugins/notifier/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(notifier): DiscordChannel — webhook POST with 2000-char content cap"
```

---

## Task 3: HttpChannel

**Files:**
- Create: `crates/isengard-plugins/notifier/src/http.rs`
- Modify: `crates/isengard-plugins/notifier/src/lib.rs` (`pub mod http;`)

- [ ] **Step 1: Create http.rs**

Create `crates/isengard-plugins/notifier/src/http.rs`:

```rust
//! Generic HTTP POST channel. Send events as JSON to any URL with optional
//! custom headers. Default body shape: `{"text": "<formatted event>"}`.
//! Custom `body_template` supports `{{text}}` and `{{kind}}` placeholders.

use std::collections::HashMap;

use async_trait::async_trait;
use isengard_core::Event;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Client;
use serde::Deserialize;
use tracing::warn;

use crate::channel::{format_event, NotifyChannel};

#[derive(Debug, Clone, Deserialize, Default)]
pub struct HttpConfig {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Body template with `{{text}}` and `{{kind}}` placeholders.
    /// Default: `{"text": "<text>"}` JSON.
    #[serde(default)]
    pub body_template: Option<String>,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub tokens_per_minute: Option<f64>,
}

pub struct HttpChannel {
    url: String,
    headers: HeaderMap,
    body_template: Option<String>,
    kinds: Vec<String>,
    http: Client,
}

impl HttpChannel {
    pub fn from_config(cfg: HttpConfig) -> anyhow::Result<Self> {
        if cfg.url.is_empty() {
            return Err(anyhow::anyhow!("http channel: url required"));
        }
        let mut headers = HeaderMap::new();
        for (k, v) in &cfg.headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| anyhow::anyhow!("bad header name {k}: {e}"))?;
            let value = HeaderValue::from_str(v)
                .map_err(|e| anyhow::anyhow!("bad header value for {k}: {e}"))?;
            headers.insert(name, value);
        }
        Ok(Self {
            url: cfg.url,
            headers,
            body_template: cfg.body_template,
            kinds: cfg.kinds,
            http: Client::builder()
                .user_agent(concat!("isengard-notifier/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| anyhow::anyhow!("building http client: {e}"))?,
        })
    }

    fn build_body(&self, event: &Event) -> String {
        let text = format_event(event);
        match &self.body_template {
            Some(tpl) => tpl
                .replace("{{text}}", &json_escape(&text))
                .replace("{{kind}}", &json_escape(&event.kind)),
            None => serde_json::to_string(&serde_json::json!({ "text": text }))
                .unwrap_or_else(|_| String::from("{}")),
        }
    }
}

/// Minimal JSON string-content escaping (quote, backslash, newline).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

#[async_trait]
impl NotifyChannel for HttpChannel {
    fn name(&self) -> &'static str {
        "http"
    }

    fn matches_kind(&self, kind: &str) -> bool {
        self.kinds.iter().any(|k| k == kind)
    }

    async fn send(&self, event: &Event) -> anyhow::Result<()> {
        let body = self.build_body(event);
        let mut req = self.http.post(&self.url).body(body);
        for (k, v) in self.headers.iter() {
            req = req.header(k, v);
        }
        // Default Content-Type to application/json if user didn't override
        if !self.headers.contains_key(reqwest::header::CONTENT_TYPE) {
            req = req.header(reqwest::header::CONTENT_TYPE, "application/json");
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => {
                warn!(status = %resp.status(), "http channel non-success");
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "http channel send failed");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_errors_on_empty_url() {
        let r = HttpChannel::from_config(HttpConfig::default());
        assert!(r.is_err());
    }

    #[test]
    fn from_config_errors_on_bad_header_name() {
        let mut h = HashMap::new();
        h.insert("invalid header".into(), "x".into());
        let cfg = HttpConfig {
            url: "https://example.com".into(),
            headers: h,
            ..Default::default()
        };
        let r = HttpChannel::from_config(cfg);
        assert!(r.is_err());
    }

    #[test]
    fn default_body_is_text_field_json() {
        let ch = HttpChannel::from_config(HttpConfig {
            url: "https://example.com".into(),
            ..Default::default()
        })
        .unwrap();
        let ev = Event {
            kind: "k".into(),
            occurred_at: chrono::Utc::now(),
            summary: "s".into(),
            ..Default::default()
        };
        let body = ch.build_body(&ev);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parsed["text"].is_string());
        assert!(parsed["text"].as_str().unwrap().contains("[isengard] k"));
    }

    #[test]
    fn custom_body_template_substitutes_placeholders() {
        let ch = HttpChannel::from_config(HttpConfig {
            url: "https://example.com".into(),
            body_template: Some(r#"{"kind":"{{kind}}","msg":"{{text}}"}"#.into()),
            ..Default::default()
        })
        .unwrap();
        let ev = Event {
            kind: "test.kind".into(),
            occurred_at: chrono::Utc::now(),
            summary: "hi".into(),
            ..Default::default()
        };
        let body = ch.build_body(&ev);
        assert!(body.contains(r#""kind":"test.kind""#));
        assert!(body.contains(r#""msg":"[isengard] test.kind\nhi""#));
    }
}
```

- [ ] **Step 2: Add `pub mod http;`**

In `crates/isengard-plugins/notifier/src/lib.rs`, after `pub mod discord;`:

```rust
pub mod http;
```

- [ ] **Step 3: Build + tests + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-notifier 2>&1 | tail -5
cd ~/Projects/isengard && cargo test -p isengard-plugin-notifier http:: 2>&1 | tail -10
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-notifier --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: 4 http tests pass, clippy clean.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/notifier/src/http.rs crates/isengard-plugins/notifier/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(notifier): HttpChannel — POST with optional custom headers + body template"
```

---

## Task 4: NotifierConfig grows + init wires RateLimited around each channel

**Files:**
- Modify: `crates/isengard-plugins/notifier/src/lib.rs` (NotifierConfig + init)

- [ ] **Step 1: Update NotifierConfig + Notifier::init**

In `crates/isengard-plugins/notifier/src/lib.rs`, update:

1. Add the per-channel configs to `NotifierConfig`:

```rust
use crate::channel::{NotifyChannel, RateLimited};
use crate::discord::{DiscordChannel, DiscordConfig};
use crate::http::{HttpChannel, HttpConfig};
use crate::telegram::{TelegramChannel, TelegramConfig};

#[derive(Debug, Clone, Deserialize, Default)]
struct NotifierConfig {
    #[serde(default)]
    telegram: Option<TelegramConfig>,
    #[serde(default)]
    discord: Option<DiscordConfig>,
    #[serde(default)]
    http: Option<HttpConfig>,
}
```

2. Add `tokens_per_minute: Option<f64>` to `TelegramConfig` if not already there. (Plan should have added it; verify and add if missing.)

3. Update `Notifier::init` to wrap each constructed channel in `RateLimited`:

```rust
    async fn init(&mut self, ctx: &PluginContext) -> Result<()> {
        let cfg: NotifierConfig = serde_json::from_value(ctx.config.clone())
            .map_err(|e| init_err(format!("parsing notifier config: {e}")))?;

        if let Some(tg_cfg) = cfg.telegram {
            let tpm = tg_cfg.tokens_per_minute.unwrap_or(10.0);
            let tg = TelegramChannel::from_config(tg_cfg)
                .map_err(|e| init_err(format!("telegram channel: {e}")))?;
            self.channels.push(Box::new(RateLimited::new(tg, tpm)));
        }

        if let Some(dc_cfg) = cfg.discord {
            let tpm = dc_cfg.tokens_per_minute.unwrap_or(10.0);
            let dc = DiscordChannel::from_config(dc_cfg)
                .map_err(|e| init_err(format!("discord channel: {e}")))?;
            self.channels.push(Box::new(RateLimited::new(dc, tpm)));
        }

        if let Some(http_cfg) = cfg.http {
            let tpm = http_cfg.tokens_per_minute.unwrap_or(10.0);
            let hc = HttpChannel::from_config(http_cfg)
                .map_err(|e| init_err(format!("http channel: {e}")))?;
            self.channels.push(Box::new(RateLimited::new(hc, tpm)));
        }

        self.kinds = vec![
            "update.success".into(),
            "update.failed".into(),
            "update.checked".into(),
            "agent.disconnect_long".into(),
        ];

        info!(channels = self.channels.len(), "notifier initialised");
        Ok(())
    }
```

4. In `crates/isengard-plugins/notifier/src/telegram.rs`, add `tokens_per_minute: Option<f64>` to `TelegramConfig`:

```rust
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TelegramConfig {
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub chat_ids: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub tokens_per_minute: Option<f64>,
}
```

(Existing tests should still pass — `Default::default()` keeps `tokens_per_minute: None`.)

- [ ] **Step 2: Build + tests + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-notifier 2>&1 | tail -5
cd ~/Projects/isengard && cargo test -p isengard-plugin-notifier 2>&1 | tail -15
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-notifier --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean. All notifier tests pass.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/notifier/src/lib.rs crates/isengard-plugins/notifier/src/telegram.rs
cd ~/Projects/isengard && git commit -m "feat(notifier): NotifierConfig accepts discord+http; init wraps each channel in RateLimited"
```

---

## Task 5: Integration tests for Discord + HTTP

**Files:**
- Create: `crates/isengard-plugins/notifier/tests/discord_e2e.rs`
- Create: `crates/isengard-plugins/notifier/tests/http_e2e.rs`

- [ ] **Step 1: Create discord_e2e.rs**

Create `crates/isengard-plugins/notifier/tests/discord_e2e.rs`:

```rust
//! Integration test: DiscordChannel POSTs to mock webhook URL.

#![allow(clippy::result_large_err)]

use chrono::Utc;
use isengard_core::Event;
use isengard_plugin_notifier::channel::NotifyChannel;
use isengard_plugin_notifier::discord::{DiscordChannel, DiscordConfig};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn discord_send_posts_to_webhook_with_content_field() {
    let server = MockServer::start().await;
    let webhook_path = "/api/webhooks/123/abc";

    Mock::given(method("POST"))
        .and(path(webhook_path))
        .and(body_json(serde_json::json!({
            "content": "[isengard] update.success\ncontainer: web\nimage: nginx:1.25\nupdated"
        })))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let cfg = DiscordConfig {
        webhook_url: format!("{}{}", server.uri(), webhook_path),
        kinds: vec!["update.success".into()],
        tokens_per_minute: None,
    };
    let ch = DiscordChannel::from_config(cfg).unwrap();

    let ev = Event {
        kind: "update.success".into(),
        occurred_at: Utc::now(),
        summary: "updated".into(),
        container_name: Some("web".into()),
        image: Some("nginx:1.25".into()),
        ..Default::default()
    };
    ch.send(&ev).await.expect("discord send should succeed");
}
```

- [ ] **Step 2: Create http_e2e.rs**

Create `crates/isengard-plugins/notifier/tests/http_e2e.rs`:

```rust
//! Integration test: HttpChannel POSTs to mock URL with custom header.

#![allow(clippy::result_large_err)]

use std::collections::HashMap;

use chrono::Utc;
use isengard_core::Event;
use isengard_plugin_notifier::channel::NotifyChannel;
use isengard_plugin_notifier::http::{HttpChannel, HttpConfig};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn http_send_posts_with_default_body_and_custom_header() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("x-isengard-test", "yes"))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let mut headers = HashMap::new();
    headers.insert("x-isengard-test".into(), "yes".into());

    let cfg = HttpConfig {
        url: format!("{}/hook", server.uri()),
        headers,
        body_template: None,
        kinds: vec!["update.success".into()],
        tokens_per_minute: None,
    };
    let ch = HttpChannel::from_config(cfg).unwrap();

    let ev = Event {
        kind: "update.success".into(),
        occurred_at: Utc::now(),
        summary: "ok".into(),
        ..Default::default()
    };
    ch.send(&ev).await.expect("http send should succeed");
}
```

- [ ] **Step 3: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-notifier --tests 2>&1 | tail -15
```

Expected: 3 integration tests pass (telegram_e2e + discord_e2e + http_e2e).

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/notifier/tests/discord_e2e.rs crates/isengard-plugins/notifier/tests/http_e2e.rs
cd ~/Projects/isengard && git commit -m "test(notifier): wiremock integration tests for Discord + HTTP channels"
```

---

## Task 6: CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 4e`, re-run.

- [ ] **Step 2: Confirm test counts**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4; fails+=$6} END {print "Total passing:", sum, "| failures:", fails}'
```

Expected: ≥ 127 baseline + ~13 new tests (channel+discord+http unit + 2 integration) = 140+. Critical: zero failures.

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase4e -m "phase 4e: Discord + HTTP channels + token-bucket rate limit + batched overflow"
cd ~/Projects/isengard && git tag -l | grep phase4e
```

Don't push.

- [ ] **Step 4: Confirm done**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 127 baseline + new tests, zero failures
- [ ] `just ci-local` clean (cargo-deny mandatory)
- [ ] Tag `v0.1.0-alpha.phase4e` exists locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (sub §8) | Plan task |
|---|---|
| Discord channel | Task 2 (impl) + Task 5 (e2e) |
| Generic HTTP POST | Task 3 (impl) + Task 5 (e2e) |
| Token-bucket rate limit per channel | Task 1 (RateLimited) |
| Batched message on overflow | Task 1 (batch_event helper, called from RateLimited::send) |
| Per-channel kind filter | Task 2 + Task 3 (matches_kind on each impl) |
| Wired through Notifier::init | Task 4 |

`agent.disconnect_long` producer (4f) deferred. Slack still v1.x.

**Type consistency check:**
- `RateLimited<C: NotifyChannel>` wraps any channel; outer `Box<dyn NotifyChannel>` works because `RateLimited<C>` is itself a NotifyChannel.
- `format_event(&Event) -> String` moved to `channel::format_event`; telegram/discord/http all import + use it.
- `tokens_per_minute: Option<f64>` added to TelegramConfig + DiscordConfig + HttpConfig — defaults to 10/min if None.
- `batch_event(&[Event]) -> Event` produces a synthetic `notifier.batch` kind that the RateLimited decorator forwards through `inner.send`.

**No new workspace deps.**

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-4e-discord-http-ratelimit.md`. Subagent-driven execution.
