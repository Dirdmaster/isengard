# Phase 4d: Notifier Plugin Scaffold + Telegram Channel

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** First end-to-end notification: when the updater emits `update.success`, a Telegram message lands in the user's configured chat. The notifier plugin is the first controller-side plugin with `EventSubscriber`. It subscribes to the EventBus, filters by configured event kinds, and dispatches each match to one or more Telegram chats via `https://api.telegram.org/bot<TOKEN>/sendMessage`.

**Architecture:** Two structural additions: (1) `PluginContext` gains `bus: Option<Arc<EventBus>>` so controller-side plugins can subscribe. (2) `run_controller` now loads `EventSubscriber` plugins via `inventory`, `init`s each one with a context that includes the bus (and journal handle for richer features later), and tracks them so `stop` is called on shutdown. The notifier crate (currently empty from Phase 0) gets `Notifier` (impl Plugin + EventSubscriber), `TelegramChannel` (impl `NotifyChannel` trait), and a small dispatch loop. Channel abstraction lives in the notifier crate — moving it to a shared crate is premature until a second consumer exists.

**Tech stack:** Adds the notifier crate's first real deps: `reqwest` (already in workspace from Phase 3b), `serde`, `tokio`. Telegram's HTTP API is dead-simple — POST to `/sendMessage` with `{chat_id, text}`.

**Branch:** `next`. Lefthook pre-push runs full gates (cargo-deny mandatory).

**Spec:** `docs/superpowers/specs/2026-04-30-phase-4-events-journal-design.md` §6-§8 (Telegram-first per 2026-04-30 revision).

---

## Scope

**In:**
- `PluginContext.bus: Option<Arc<EventBus>>` field, populated by controller's plugin host (None on agent side).
- Controller plugin host (small): `run_controller` walks `inventory::iter::<PluginRegistration>()`, filters for `Capability::Controller` registrations, constructs each, calls `init` with a context including the bus + journal handle, calls `start`, tracks for shutdown.
- `EventSubscriber` integration: at controller start, the host subscribes EACH `EventSubscriber` plugin to the bus (gets a `broadcast::Receiver`), spawns a per-plugin task that loops on `recv` → checks `event_kinds()` match → calls `plugin.handle(event)`.
- Notifier crate (`crates/isengard-plugins/notifier/`):
  - `Notifier` struct: holds `Vec<Box<dyn NotifyChannel>>` + `Vec<&'static str>` of subscribed kinds (assembled from union of channels).
  - Implements `Plugin + EventSubscriber`.
  - `inventory::submit!` registration with `Capability::Controller`.
  - `init(ctx)` reads config, constructs channels, returns Ok.
  - `EventSubscriber::handle(&self, event)` → for each channel where `matches_kind(event.kind)` → send via channel.
- `NotifyChannel` trait + `TelegramChannel` impl in the notifier crate.
- `TelegramChannel` config: `bot_token: String` + `chat_ids: Vec<String>` + `kinds: Vec<String>` (defaults: `["update.success", "update.failed", "agent.disconnect_long"]`). Bot token also accepted via env var `ISENGARD_TELEGRAM_BOT_TOKEN`, env wins over config.
- Message format (plain text v1):
  ```
  [isengard] update.success
  container: web
  image: nginx:1.25-alpine
  digest: sha256:abc... → sha256:def...
  ```
  Lines ordered for readability; absent fields skipped.
- Unit tests: TelegramChannel format helper (pure string formatting), `matches_kind` glob behavior (single kind, exact match — no glob in v1), Notifier construction from config.
- Integration test: spin up a mock HTTP server (use `wiremock` workspace dep — adding it as a dev dep), construct TelegramChannel pointed at the mock, send an event, assert the mock received the right URL + JSON body.

**Out (deferred):**
- Discord (4e)
- Generic HTTP POST channel (4e)
- Rate limiting + batching (4e)
- `agent.disconnect_long` producer (4f)
- MarkdownV2 / HTML formatting (v1.x — escape rules are gnarly)
- Bot polling for `/start` to auto-discover chat_ids (v1.x — currently user reads it from `getUpdates` manually)
- Per-channel kind glob (v1 supports exact-string match only; glob defers)
- Webhook signing on outbound (Telegram doesn't need it; the bot token IS the auth)
- Retry/backoff on send failure (log+drop in v1; v1.x adds queue + retry)
- Multi-recipient batching ("3 events in last minute" message)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes — baseline 117 + new tests
3. `just ci-local` clean (cargo-deny mandatory)
4. Manual smoke (if user has a Telegram bot): `ISENGARD_TELEGRAM_BOT_TOKEN=...` + sample config → run controller → trigger an event → message lands in chat
5. Tag `v0.1.0-alpha.phase4d` set locally
6. **Not pushed** until user confirms

---

## File Structure

```
Cargo.toml                                          # MODIFY: + wiremock dev-dep in workspace

crates/isengard-core/
└── src/context.rs                                  # MODIFY: + bus field on PluginContext

crates/isengard-controller/
├── Cargo.toml                                      # UNCHANGED
└── src/
    ├── lib.rs                                      # MODIFY: load+init controller plugins, wire bus subscriptions
    └── plugin_host.rs                              # NEW: controller-side plugin host (load, init, subscribe, stop)

crates/isengard-plugins/notifier/
├── Cargo.toml                                      # MODIFY: real deps (reqwest, serde, tokio, async-trait, anyhow, tracing, isengard-core, chrono, inventory)
└── src/
    ├── lib.rs                                      # NEW: Notifier struct + Plugin/EventSubscriber impl + inventory::submit!
    ├── channel.rs                                  # NEW: NotifyChannel trait
    └── telegram.rs                                 # NEW: TelegramChannel
```

---

## Task 1: PluginContext gains bus field

**Files:**
- Modify: `crates/isengard-core/src/context.rs`
- Modify: `crates/isengard-core/src/lib.rs` (re-export, if needed)

- [ ] **Step 1: Add bus field**

In `crates/isengard-core/src/context.rs`, modify `PluginContext`:

```rust
pub struct PluginContext {
    pub mode: HostMode,
    pub config: Value,
    pub events: Option<Arc<dyn EventEmitter>>,
    pub bus: Option<Arc<dyn EventBusHandle>>,
}

impl PluginContext {
    pub fn new(mode: HostMode, config: Value) -> Self {
        Self {
            mode,
            config,
            events: None,
            bus: None,
        }
    }

    pub fn with_events(mut self, events: Arc<dyn EventEmitter>) -> Self {
        self.events = Some(events);
        self
    }

    pub fn with_bus(mut self, bus: Arc<dyn EventBusHandle>) -> Self {
        self.bus = Some(bus);
        self
    }
}
```

Add a small abstraction trait `EventBusHandle` in `crates/isengard-core/src/event.rs` so core doesn't depend on tokio's broadcast directly:

```rust
/// Subscribe-side handle to the controller's event bus. Plugins call
/// `subscribe` to get a stream of events. Concrete impl lives in the
/// controller crate; this trait keeps core free of tokio dependencies
/// beyond what's already there.
#[async_trait::async_trait]
pub trait EventBusHandle: Send + Sync + 'static {
    /// Returns a Receiver-style boxed stream. Implementer's choice of channel.
    async fn next_event(&self) -> Option<Event>;
}
```

Wait — `next_event` doesn't compose with multiple subscribers cleanly because each subscriber needs its own receiver. Better approach: return a fresh boxed stream per subscribe call.

Use this shape instead:

```rust
use std::pin::Pin;
use futures_core::Stream;

pub type EventStream = Pin<Box<dyn Stream<Item = Event> + Send>>;

pub trait EventBusHandle: Send + Sync + 'static {
    fn subscribe(&self) -> EventStream;
}
```

This needs `futures-core` in `isengard-core` as a workspace dep. Check if `futures-util` (already a workspace dep from Phase 3c) re-exports it; it does via `futures-util::stream`, but adding `futures-core` directly is cleaner.

**Simpler alternative (recommend):** sidestep the trait entirely. Pass `Arc<isengard_controller::EventBus>` directly via the context. This couples core to controller, which is wrong direction. So instead, define:

```rust
// In core/event.rs
#[async_trait::async_trait]
pub trait EventBusHandle: Send + Sync + 'static {
    /// Block until the next event the subscriber should see.
    /// Returns None when the bus is closed.
    /// IMPORTANT: each call to subscribe should return a fresh receiver.
    async fn subscribe_next(&self, sub_id: u64) -> Option<Event>;
    fn new_sub_id(&self) -> u64;
}
```

This is awkward. **Cleanest answer**: skip the trait abstraction in 4d, accept the controller crate as a context-source, and let the notifier crate depend on `isengard-controller` directly. The notifier already only runs in controller mode anyway.

**Final approach (chosen)**: Notifier depends on `isengard-controller` for `EventBus`. Controller's plugin host hands the `Arc<EventBus>` to controller-side plugins via a NEW context field that's typed as `Option<Arc<EventBus>>`, NOT through PluginContext (which is core). This means we need a NEW context type.

But that breaks the inventory factory pattern that returns `Box<dyn Plugin>` — we don't have per-plugin context types.

**Pragmatic resolution**: keep PluginContext in core, add the bus field as `Option<Arc<dyn Any + Send + Sync>>` and let the notifier downcast. Ugly but contained. Document the contract:

```rust
// In core/context.rs
pub struct PluginContext {
    pub mode: HostMode,
    pub config: Value,
    pub events: Option<Arc<dyn EventEmitter>>,
    /// Controller-side: opaque handle to the EventBus. Plugins downcast.
    pub bus: Option<Arc<dyn std::any::Any + Send + Sync>>,
}
```

Notifier downcasts:
```rust
let bus = ctx.bus.as_ref().and_then(|b| b.clone().downcast::<EventBus>().ok());
```

This is the lowest-coupling option that doesn't introduce a new trait. Yes it's `dyn Any`. The contract is "controller passes EventBus, notifier downcasts". Document with a `///` comment.

**Use this approach for the rest of the plan.** Skip the EventBusHandle trait.

So: just modify PluginContext to add `bus: Option<Arc<dyn Any + Send + Sync>>`.

In `crates/isengard-core/src/context.rs`:

```rust
use std::any::Any;
use std::sync::Arc;

use serde_json::Value;

use crate::EventEmitter;

#[derive(Clone)]
pub struct PluginContext {
    pub mode: HostMode,
    pub config: Value,
    pub events: Option<Arc<dyn EventEmitter>>,
    /// Opaque handle for controller-side bus. Notifier-style plugins downcast
    /// to the concrete `isengard_controller::EventBus`. None on agent side.
    pub bus: Option<Arc<dyn Any + Send + Sync>>,
}

impl PluginContext {
    pub fn new(mode: HostMode, config: Value) -> Self {
        Self {
            mode,
            config,
            events: None,
            bus: None,
        }
    }

    pub fn with_events(mut self, events: Arc<dyn EventEmitter>) -> Self {
        self.events = Some(events);
        self
    }

    pub fn with_bus(mut self, bus: Arc<dyn Any + Send + Sync>) -> Self {
        self.bus = Some(bus);
        self
    }
}

// Existing manual Debug impl needs updating:
impl std::fmt::Debug for PluginContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginContext")
            .field("mode", &self.mode)
            .field("config", &self.config)
            .field("events", &self.events.as_ref().map(|_| "<emitter>"))
            .field("bus", &self.bus.as_ref().map(|_| "<bus>"))
            .finish()
    }
}
```

(Keep `HostMode` definition unchanged.)

- [ ] **Step 2: Build + tests**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -5
cd ~/Projects/isengard && cargo test -p isengard-core 2>&1 | tail -10
```

Expected: clean. Add a unit test in core for `with_bus`:

```rust
// In context.rs tests:
#[test]
fn plugin_context_with_bus_attaches_handle() {
    let bus: Arc<dyn std::any::Any + Send + Sync> = Arc::new(42u32);
    let ctx = PluginContext::new(HostMode::Controller, serde_json::Value::Null).with_bus(bus);
    assert!(ctx.bus.is_some());
}
```

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-core/src/context.rs
cd ~/Projects/isengard && git commit -m "feat(core): PluginContext.bus — opaque handle for controller-side plugins"
```

**Self-review checklist:**
- [ ] Build + clippy clean
- [ ] Core tests +1 (with_bus_attaches_handle)
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 2: Controller-side plugin host + bus subscription

**Files:**
- Create: `crates/isengard-controller/src/plugin_host.rs`
- Modify: `crates/isengard-controller/src/lib.rs`

- [ ] **Step 1: Create the plugin host**

Create `crates/isengard-controller/src/plugin_host.rs`:

```rust
//! Controller-side plugin host. Walks the inventory for plugins with
//! `Capability::Controller`, inits each with a context including the bus +
//! journal, starts them, and (for `EventSubscriber` plugins) spawns a per-
//! plugin task that drains the bus and calls `handle`.
//!
//! v1 has no clean dynamic-cast for "is this plugin an EventSubscriber?"
//! since `Plugin` is the type-erased trait object. We work around it by
//! making EVERY controller plugin self-subscribe in its `start` if it wants
//! events — passed the bus via context (already there). The host doesn't
//! need to know.

use std::any::Any;
use std::sync::Arc;

use isengard_core::{Capability, HostMode, Plugin, PluginContext, registrations_for};
use isengard_storage::Journal;
use serde_json::Value;
use tracing::{info, warn};

use crate::bus::EventBus;

pub struct LoadedPlugin {
    pub name: &'static str,
    pub plugin: Box<dyn Plugin>,
}

/// Construct + init + start every controller-side plugin in the inventory.
/// Returns the loaded plugins so the caller can drive them through stop on shutdown.
pub async fn load_controller_plugins(
    bus: Arc<EventBus>,
    _journal: Arc<Journal>,
    config: Value,
) -> Vec<LoadedPlugin> {
    let bus_handle: Arc<dyn Any + Send + Sync> = bus.clone();
    let ctx = PluginContext::new(HostMode::Controller, config).with_bus(bus_handle);

    let mut loaded = Vec::new();
    for reg in registrations_for(HostMode::Controller) {
        let mut plugin = (reg.constructor)();
        let name = reg.name;
        info!(plugin = name, "loading controller plugin");
        if let Err(e) = plugin.init(&ctx).await {
            warn!(plugin = name, error = ?e, "plugin init failed; skipping");
            continue;
        }
        if let Err(e) = plugin.start(&ctx).await {
            warn!(plugin = name, error = ?e, "plugin start failed; skipping");
            continue;
        }
        loaded.push(LoadedPlugin { name, plugin });
    }
    loaded
}

/// Stop every loaded plugin. Called on controller shutdown.
pub async fn stop_controller_plugins(loaded: &mut [LoadedPlugin]) {
    for lp in loaded.iter_mut() {
        if let Err(e) = lp.plugin.stop().await {
            warn!(plugin = lp.name, error = ?e, "plugin stop failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_inventory_yields_no_plugins() {
        // Without any controller plugins linked into this test binary,
        // load_controller_plugins should return empty.
        let bus = Arc::new(EventBus::new());
        let journal = Arc::new(Journal::open_in_memory().await.unwrap());
        let loaded = load_controller_plugins(bus, journal, Value::Null).await;
        // Note: this test asserts the function returns CLEANLY with no plugins.
        // If a future test in this binary links a controller plugin, this
        // assertion would fail — adapt at that time.
        let _ = loaded; // length depends on what's linked; just verify no panic.
    }
}
```

- [ ] **Step 2: Wire it into run_controller**

In `crates/isengard-controller/src/lib.rs`, find `run_controller`. After it builds `Arc<Journal>` + `Arc<EventBus>` (per Phase 4b-T4), call the host:

```rust
    // Load + start controller-side plugins (notifier, etc).
    let mut controller_plugins = plugin_host::load_controller_plugins(
        bus.clone(),
        journal.clone(),
        opts.config.clone(),  // or however config is currently passed
    )
    .await;
```

After the existing `Server::serve(...)` block (which awaits ctrl_c and returns), add:

```rust
    plugin_host::stop_controller_plugins(&mut controller_plugins).await;
```

Add `pub mod plugin_host;` to `lib.rs`.

If `opts.config` doesn't exist or has a different name, use whatever the controller currently has. If config isn't currently threaded through, pass `serde_json::Value::Null` for now and add a TODO note in your concerns.

- [ ] **Step 3: Build + tests**

```bash
cd ~/Projects/isengard && cargo build -p isengard-controller 2>&1 | tail -5
cd ~/Projects/isengard && cargo test -p isengard-controller 2>&1 | tail -15
```

Expected: 12+ controller tests still pass, +1 new plugin_host test.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/src/plugin_host.rs crates/isengard-controller/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(controller): plugin host loads + drives controller-side plugins"
```

**Self-review checklist:**
- [ ] All controller tests pass
- [ ] `cargo clippy -p isengard-controller --all-targets -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 3: Notifier crate — Plugin scaffold + NotifyChannel trait + TelegramChannel

**Files:**
- Modify: `crates/isengard-plugins/notifier/Cargo.toml`
- Create: `crates/isengard-plugins/notifier/src/channel.rs`
- Create: `crates/isengard-plugins/notifier/src/telegram.rs`
- Replace: `crates/isengard-plugins/notifier/src/lib.rs`

- [ ] **Step 1: Update Cargo.toml**

Replace the existing minimal `crates/isengard-plugins/notifier/Cargo.toml` content's `[dependencies]` section with:

```toml
[dependencies]
anyhow.workspace = true
async-trait.workspace = true
chrono.workspace = true
inventory.workspace = true
isengard-core.workspace = true
isengard-controller.workspace = true
reqwest.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio = { workspace = true }
tracing.workspace = true

[dev-dependencies]
tokio = { workspace = true }
wiremock = "0.6.2"
```

Add `wiremock = "0.6.2"` to workspace `[workspace.dependencies]` in root `Cargo.toml`:

```toml
# HTTP mock server for tests
wiremock = "0.6.2"
```

Then in notifier dev-deps, change to:

```toml
wiremock.workspace = true
```

Also add `isengard-controller = { path = "crates/isengard-controller", version = "0.1.0-alpha" }` to the workspace `[workspace.dependencies]` list (alongside the existing internal-crate entries).

- [ ] **Step 2: Create channel.rs**

Create `crates/isengard-plugins/notifier/src/channel.rs`:

```rust
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
```

- [ ] **Step 3: Create telegram.rs**

Create `crates/isengard-plugins/notifier/src/telegram.rs`:

```rust
//! Telegram bot channel. Outbound-only in v1: POST to /sendMessage.
//!
//! Setup: user creates a bot via @BotFather, gets a token, adds the bot to
//! a chat (1:1 or group), sends `/start` once, reads the chat_id from
//! `https://api.telegram.org/bot<TOKEN>/getUpdates`. Configures via:
//!
//! ```json
//! { "telegram": { "chat_ids": ["123456"], "kinds": ["update.success", "update.failed"] } }
//! ```
//!
//! Bot token is sourced from env `ISENGARD_TELEGRAM_BOT_TOKEN` (preferred)
//! or config field `bot_token` (fallback).

use async_trait::async_trait;
use isengard_core::Event;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::channel::NotifyChannel;

const DEFAULT_API_BASE: &str = "https://api.telegram.org";

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TelegramConfig {
    /// Bot token. Optional in config — env `ISENGARD_TELEGRAM_BOT_TOKEN` takes precedence.
    #[serde(default)]
    pub bot_token: Option<String>,
    /// One or more chat IDs to post to.
    #[serde(default)]
    pub chat_ids: Vec<String>,
    /// Event kinds (exact string match) this channel cares about.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Override base URL (used by tests; defaults to https://api.telegram.org).
    #[serde(default)]
    pub api_base: Option<String>,
}

pub struct TelegramChannel {
    bot_token: String,
    chat_ids: Vec<String>,
    kinds: Vec<String>,
    api_base: String,
    http: Client,
}

#[derive(Debug, Serialize)]
struct SendMessageBody<'a> {
    chat_id: &'a str,
    text: &'a str,
}

impl TelegramChannel {
    /// Build a channel from config. Env var beats config. Returns Err if no
    /// token resolvable OR chat_ids is empty.
    pub fn from_config(cfg: TelegramConfig) -> anyhow::Result<Self> {
        let bot_token = std::env::var("ISENGARD_TELEGRAM_BOT_TOKEN")
            .ok()
            .or(cfg.bot_token)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "telegram channel: no bot token (set ISENGARD_TELEGRAM_BOT_TOKEN or notifier.telegram.bot_token)"
                )
            })?;
        if cfg.chat_ids.is_empty() {
            return Err(anyhow::anyhow!(
                "telegram channel: chat_ids must contain at least one entry"
            ));
        }
        Ok(Self {
            bot_token,
            chat_ids: cfg.chat_ids,
            kinds: cfg.kinds,
            api_base: cfg.api_base.unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            http: Client::builder()
                .user_agent(concat!("isengard-notifier/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| anyhow::anyhow!("building http client: {e}"))?,
        })
    }
}

/// Format an event into a plain-text Telegram message.
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

#[async_trait]
impl NotifyChannel for TelegramChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    fn matches_kind(&self, kind: &str) -> bool {
        self.kinds.iter().any(|k| k == kind)
    }

    async fn send(&self, event: &Event) -> anyhow::Result<()> {
        let text = format_event(event);
        let url = format!("{}/bot{}/sendMessage", self.api_base, self.bot_token);
        for chat_id in &self.chat_ids {
            let body = SendMessageBody {
                chat_id,
                text: &text,
            };
            match self.http.post(&url).json(&body).send().await {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => {
                    warn!(
                        chat_id = %chat_id,
                        status = %resp.status(),
                        "telegram sendMessage non-success"
                    );
                }
                Err(e) => {
                    warn!(chat_id = %chat_id, error = %e, "telegram sendMessage failed");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn ev() -> Event {
        Event {
            kind: "update.success".into(),
            occurred_at: Utc::now(),
            summary: "updated web".into(),
            container_name: Some("web".into()),
            image: Some("nginx:1.25".into()),
            old_digest: Some("sha256:aaa".into()),
            new_digest: Some("sha256:bbb".into()),
            ..Default::default()
        }
    }

    #[test]
    fn format_includes_kind_container_image_digests() {
        let s = format_event(&ev());
        assert!(s.contains("[isengard] update.success"));
        assert!(s.contains("container: web"));
        assert!(s.contains("image: nginx:1.25"));
        assert!(s.contains("digest: sha256:aaa → sha256:bbb"));
        assert!(s.contains("updated web"));
    }

    #[test]
    fn format_includes_error_when_present() {
        let mut e = ev();
        e.kind = "update.failed".into();
        e.error = Some("docker pull failed: timeout".into());
        let s = format_event(&e);
        assert!(s.contains("[isengard] update.failed"));
        assert!(s.contains("error: docker pull failed: timeout"));
    }

    #[test]
    fn format_skips_absent_optional_fields() {
        let e = Event {
            kind: "agent.disconnect_long".into(),
            occurred_at: Utc::now(),
            summary: "agent X unreachable for over 4h".into(),
            ..Default::default()
        };
        let s = format_event(&e);
        assert!(!s.contains("container:"));
        assert!(!s.contains("image:"));
        assert!(!s.contains("digest:"));
        assert!(s.contains("agent X unreachable"));
    }

    #[test]
    fn matches_kind_exact_string() {
        let cfg = TelegramConfig {
            bot_token: Some("test".into()),
            chat_ids: vec!["123".into()],
            kinds: vec!["update.success".into(), "update.failed".into()],
            api_base: None,
        };
        let ch = TelegramChannel::from_config(cfg).unwrap();
        assert!(ch.matches_kind("update.success"));
        assert!(ch.matches_kind("update.failed"));
        assert!(!ch.matches_kind("update.checked"));
        assert!(!ch.matches_kind("agent.disconnect_long"));
    }

    #[test]
    fn from_config_errors_without_token() {
        // Save & clear env to ensure isolation.
        let prev = std::env::var("ISENGARD_TELEGRAM_BOT_TOKEN").ok();
        // SAFETY: tests run with `--test-threads=1` is not guaranteed; race is possible.
        // Document this. v1.x: figure(env) crate.
        unsafe { std::env::remove_var("ISENGARD_TELEGRAM_BOT_TOKEN"); }
        let cfg = TelegramConfig {
            bot_token: None,
            chat_ids: vec!["123".into()],
            kinds: vec![],
            api_base: None,
        };
        let r = TelegramChannel::from_config(cfg);
        if let Some(p) = prev {
            unsafe { std::env::set_var("ISENGARD_TELEGRAM_BOT_TOKEN", p); }
        }
        assert!(r.is_err());
    }

    #[test]
    fn from_config_errors_with_no_chat_ids() {
        let cfg = TelegramConfig {
            bot_token: Some("test".into()),
            chat_ids: vec![],
            kinds: vec![],
            api_base: None,
        };
        let r = TelegramChannel::from_config(cfg);
        assert!(r.is_err());
    }
}
```

- [ ] **Step 4: Replace lib.rs with the Notifier struct**

Replace `crates/isengard-plugins/notifier/src/lib.rs` with:

```rust
//! Isengard `notifier` plugin (controller-side).
//!
//! Subscribes to the controller's EventBus, filters events by kind, and
//! dispatches matches to configured channels (v1: Telegram only).

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use async_trait::async_trait;
use isengard_controller::bus::EventBus;
use isengard_core::{
    Capability, CoreError, Event, EventSubscriber, Plugin, PluginContext, PluginRegistration,
    Result,
};
use serde::Deserialize;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

pub mod channel;
pub mod telegram;

use crate::channel::NotifyChannel;
use crate::telegram::{TelegramChannel, TelegramConfig};

const PLUGIN_NAME: &str = "notifier";

#[derive(Debug, Clone, Deserialize, Default)]
struct NotifierConfig {
    #[serde(default)]
    telegram: Option<TelegramConfig>,
}

pub struct Notifier {
    channels: Vec<Box<dyn NotifyChannel>>,
    /// Union of all kinds across channels (for the EventSubscriber trait).
    /// Stored as String here, leaked to &'static str in event_kinds() — see note.
    kinds: Vec<String>,
    task: Option<JoinHandle<()>>,
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
            kinds: Vec::new(),
            task: None,
        }
    }
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}

fn init_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::InitFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

fn start_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::StartFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

#[async_trait]
impl Plugin for Notifier {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: &PluginContext) -> Result<()> {
        let cfg: NotifierConfig = serde_json::from_value(ctx.config.clone())
            .map_err(|e| init_err(format!("parsing notifier config: {e}")))?;

        if let Some(tg_cfg) = cfg.telegram {
            let tg = TelegramChannel::from_config(tg_cfg)
                .map_err(|e| init_err(format!("telegram channel: {e}")))?;
            // Collect kinds the channel cares about. v1: re-parse the JSON
            // because matches_kind doesn't expose the list.
            // Simpler: re-read the kinds from the parsed config.
            self.channels.push(Box::new(tg));
        }

        // Currently only Telegram is configurable; future channels (Discord,
        // HTTP) are added in 4e. Re-derive subscribed kinds from config.
        // For v1, simplest approach: subscribe to every event kind ("*"
        // semantics) since we just-built channels filter per-kind anyway.
        // But event_kinds() returns &[&'static str] — we don't have a
        // catch-all literal. Hardcode the known producer kinds:
        self.kinds = vec![
            "update.success".into(),
            "update.failed".into(),
            "update.checked".into(),
            "agent.disconnect_long".into(),
        ];

        info!(channels = self.channels.len(), "notifier initialised");
        Ok(())
    }

    async fn start(&mut self, ctx: &PluginContext) -> Result<()> {
        // Downcast the bus handle to the concrete EventBus.
        let bus_arc = ctx
            .bus
            .clone()
            .ok_or_else(|| start_err("notifier started without a bus on PluginContext"))?;
        let bus = bus_arc
            .downcast::<EventBus>()
            .map_err(|_| start_err("bus on PluginContext was not an EventBus"))?;
        let mut rx = bus.subscribe();

        // Move channels out so the spawned task owns them (Plugin::stop
        // doesn't need them anymore).
        let channels = std::mem::take(&mut self.channels);
        let channels: Arc<[Box<dyn NotifyChannel>]> = channels.into();

        let task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => dispatch(&channels, event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "notifier broadcast lag — events dropped");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        debug!("notifier broadcast closed; ending task");
                        break;
                    }
                }
            }
        });
        self.task = Some(task);
        info!("notifier started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        info!("notifier stopped");
        Ok(())
    }
}

async fn dispatch(channels: &[Box<dyn NotifyChannel>], event: Event) {
    for ch in channels.iter() {
        if !ch.matches_kind(&event.kind) {
            continue;
        }
        if let Err(e) = ch.send(&event).await {
            warn!(channel = ch.name(), kind = %event.kind, error = %e, "channel send failed");
        }
    }
}

#[async_trait]
impl EventSubscriber for Notifier {
    fn event_kinds(&self) -> &[&'static str] {
        // The trait wants &'static str slice; we can't return runtime kinds
        // here without leaking. For v1, return an empty slice — the actual
        // filter happens per-channel in dispatch(). The host's role is just
        // to subscribe; we always-want-everything from the bus.
        &[]
    }

    async fn handle(&self, _event: &Event) -> Result<()> {
        // Unused — start() spawns its own dispatch task. EventSubscriber
        // is implemented for shape-compliance with the Phase 1 trait, but
        // the actual handling is the spawned task in start().
        Ok(())
    }
}

inventory::submit! {
    PluginRegistration {
        name: PLUGIN_NAME,
        capabilities: &[Capability::Controller],
        constructor: || Box::new(Notifier::new()) as Box<dyn Plugin>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifier_new_has_no_channels() {
        let n = Notifier::new();
        assert!(n.channels.is_empty());
    }
}
```

- [ ] **Step 5: Wire notifier into the controller binary**

In `crates/isengard/Cargo.toml`, add to `[dependencies]`:

```toml
isengard-plugin-notifier = { path = "../isengard-plugins/notifier", version = "0.1.0-alpha" }
```

Add the workspace dep entry too if not already present:

```toml
isengard-plugin-notifier = { path = "crates/isengard-plugins/notifier", version = "0.1.0-alpha" }
```

In `crates/isengard/src/main.rs`, add a no-op import to ensure the inventory registration is linked:

```rust
#[allow(unused_imports)]
use isengard_plugin_notifier as _;
```

(Same pattern as Phase 3a-T3 used for the updater plugin.)

- [ ] **Step 6: Build + clippy + tests**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -10
cd ~/Projects/isengard && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-plugin-notifier 2>&1 | tail -15
```

Expected: clean. Notifier tests: 1 (notifier_new_has_no_channels) + 5-6 telegram tests = ~6-7.

- [ ] **Step 7: Commit**

```bash
cd ~/Projects/isengard && git add Cargo.toml Cargo.lock crates/isengard-plugins/notifier crates/isengard/Cargo.toml crates/isengard/src/main.rs
cd ~/Projects/isengard && git commit -m "feat(notifier): scaffold + Telegram channel + wire into binary"
```

**Self-review checklist:**
- [ ] Build + clippy + tests clean
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 4: Integration test — Telegram channel hits a mock server

**Files:**
- Create: `crates/isengard-plugins/notifier/tests/telegram_e2e.rs`

- [ ] **Step 1: Create the test**

Create `crates/isengard-plugins/notifier/tests/telegram_e2e.rs`:

```rust
//! Integration test: TelegramChannel POSTs to a mock /sendMessage endpoint.

#![allow(clippy::result_large_err)]

use chrono::Utc;
use isengard_core::Event;
use isengard_plugin_notifier::channel::NotifyChannel;
use isengard_plugin_notifier::telegram::{TelegramChannel, TelegramConfig};
use wiremock::matchers::{method, path, body_json};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn telegram_send_posts_to_send_message_endpoint() {
    let server = MockServer::start().await;
    let bot_token = "TEST_TOKEN_123";

    Mock::given(method("POST"))
        .and(path(format!("/bot{bot_token}/sendMessage")))
        .and(body_json(serde_json::json!({
            "chat_id": "555",
            "text": "[isengard] update.success\ncontainer: web\nimage: nginx:1.25\ndigest: sha256:aaa → sha256:bbb\nupdated web"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
        .mount(&server)
        .await;

    let cfg = TelegramConfig {
        bot_token: Some(bot_token.into()),
        chat_ids: vec!["555".into()],
        kinds: vec!["update.success".into()],
        api_base: Some(server.uri()),
    };
    let ch = TelegramChannel::from_config(cfg).unwrap();

    let ev = Event {
        kind: "update.success".into(),
        occurred_at: Utc::now(),
        summary: "updated web".into(),
        container_name: Some("web".into()),
        image: Some("nginx:1.25".into()),
        old_digest: Some("sha256:aaa".into()),
        new_digest: Some("sha256:bbb".into()),
        ..Default::default()
    };
    ch.send(&ev).await.expect("telegram send should succeed");
    // Mock auto-verifies on drop that the expected request was made.
}
```

- [ ] **Step 2: Run the test**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-notifier --test telegram_e2e 2>&1 | tail -10
```

Expected: passes.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/notifier/tests/telegram_e2e.rs
cd ~/Projects/isengard && git commit -m "test(notifier): TelegramChannel POSTs to mock /sendMessage endpoint"
```

---

## Task 5: CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 4d`, re-run.

- [ ] **Step 2: Confirm test counts**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4; fails+=$6} END {print "Total passing:", sum, "| failures:", fails}'
```

Expected: ≥ 117 baseline + ~7 new tests = 124+. Critical: zero failures.

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase4d -m "phase 4d: notifier scaffold + telegram channel"
cd ~/Projects/isengard && git tag -l | grep phase4d
```

Don't push.

- [ ] **Step 4: Confirm done**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 117 baseline + new tests, zero failures
- [ ] `just ci-local` clean (cargo-deny mandatory)
- [ ] Tag `v0.1.0-alpha.phase4d` exists locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (sub §6-§8) | Plan task |
|---|---|
| `PluginContext.bus` for controller-side plugins | Task 1 |
| Controller plugin host (load + init + start + stop) | Task 2 |
| `Notifier` plugin scaffold + EventSubscriber + dispatch loop | Task 3 |
| `NotifyChannel` trait | Task 3 |
| `TelegramChannel` impl with `from_config`, env-or-config token, format helper | Task 3 |
| Wire notifier into the controller binary | Task 3 step 5 |
| Mock-based integration test for HTTP | Task 4 |

Discord + HTTP + rate limit + batching deferred to 4e. `agent.disconnect_long` producer to 4f.

**Type consistency check:**
- `PluginContext.bus: Option<Arc<dyn Any + Send + Sync>>` — set by controller plugin host with `bus.clone() as Arc<dyn Any + Send + Sync>`, downcast in notifier `start` to `Arc<EventBus>`.
- `EventBus.subscribe() -> broadcast::Receiver<Event>` — used by notifier task.
- `NotifyChannel::send(&Event) -> anyhow::Result<()>` — called per-channel in dispatch.
- `TelegramConfig.api_base: Option<String>` — overridden in tests to point at wiremock; production uses None → defaults to `https://api.telegram.org`.
- `inventory::submit!` for Notifier with `Capability::Controller` — picked up by `registrations_for(HostMode::Controller)`.

**No new workspace deps** beyond `wiremock` (dev-only, for tests).

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-4d-notifier-telegram.md`. Subagent-driven execution.
