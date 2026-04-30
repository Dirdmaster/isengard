# Phase 4: Event Journal + Notifier Plugin Design

**Status:** Draft, 2026-04-30
**Parent spec:** `2026-04-29-platform-pivot-design.md` §9.3 + §11
**Author:** AI partner, in dialogue with engineer (off-session — best-judgment design, push back on any direction)

---

## 1. Why this exists

Phase 3 made the updater feature-complete with the legacy Go binary. But the value of the platform — the reason it's not just "watchtower in Rust" — is that updates become first-class observable events that something can act on. Phase 4 adds:

1. A controller-side **event journal** — append-only durable record of what happened across the fleet (which agent updated which container at what time, what the digests were, what failed).
2. An in-process **event bus** so plugins can react to events as they happen.
3. The **`notifier` plugin** — first consumer of the bus — sending those events to Telegram, Discord, or a generic HTTP POST endpoint.
4. The **`agent.disconnect_long` detection** in the controller — produced internally, not from agents.

Once Phase 4 lands, you can deploy the platform and get a Telegram ping every time the friend's prod fleet updates a container. That's the user-visible product proof. Telegram is the v1 default channel because BotFather + `/start` is a 5-minute one-person setup; Slack incoming webhooks require workspace admin rights and an org account, which we don't want to assume.

---

## 2. Architecture

```
┌────────────────── agent ──────────────────┐         ┌────────────── controller ──────────────┐
│                                            │         │                                          │
│  updater plugin                            │         │   gRPC service                          │
│      │                                     │         │      │                                   │
│      │ emit_event(Event)                   │         │      │ on AgentMessage::Event           │
│      ▼                                     │         │      ▼                                   │
│  PluginContext.events  ──────────────────► │  Sync   │  Journal::insert(event)                 │
│   (Arc<dyn EventEmitter>)                  │ stream  │      │                                   │
│                                            │  ────►  │      ▼                                   │
│  outbound msgs queued, multiplexed onto    │         │  EventBus::broadcast(event)             │
│  the existing Sync stream                  │         │      │                                   │
│                                            │         │      ▼                                   │
└────────────────────────────────────────────┘         │  notifier plugin (subscriber)           │
                                                       │      │                                   │
                                                       │      │ matches event_kinds()           │
                                                       │      ▼                                   │
                                                       │  channel.send(event)                    │
                                                       │   ├── slack webhook                      │
                                                       │   ├── discord webhook                    │
                                                       │   └── http POST                          │
                                                       │                                          │
                                                       │   internal:                              │
                                                       │   tokio task scans inventory every 60s   │
                                                       │   for hosts last_seen > 4h ago →         │
                                                       │   emit agent.disconnect_long             │
                                                       │                                          │
                                                       └──────────────────────────────────────────┘
```

**Key invariants:**
- Events are append-only. Never updated, never deleted (a future GC plugin can prune by age, but that's out of scope).
- Persistence happens BEFORE broadcast. If the journal write fails, no subscriber sees the event. (Better to drop than to notify on something we have no record of.)
- Subscribers are best-effort. A slow notifier can't backpressure the journal — broadcast's bounded channel will lag and drop for that subscriber, with a metric.
- The wire format is additive: new `AgentMessage::Event` variant, no changes to existing `Hello` / `Heartbeat`.

---

## 3. Wire format

Add to `crates/isengard-proto/proto/isengard.v1.proto`:

```proto
message Event {
  // Stable kind identifier, dotted: "update.success", "update.failed", "update.checked",
  //                                  "update.skipped", "agent.disconnect_long"
  string kind = 1;

  // ISO 8601 / RFC 3339 timestamp of when the event occurred on the agent.
  string occurred_at = 2;

  // One-line human summary, used directly in notifications.
  string summary = 3;

  // Optional structured fields (kept narrow on purpose to avoid schema drift).
  optional string container_name = 4;
  optional string image = 5;
  optional string old_digest = 6;
  optional string new_digest = 7;
  optional string error = 8;

  // Free-form additional context, JSON-serialised. Plugin-specific.
  optional string metadata_json = 9;
}

message AgentMessage {
  oneof body {
    SyncHello hello = 1;
    Heartbeat heartbeat = 2;
    Event event = 3;        // NEW
  }
}
```

Controller doesn't need to send events DOWN to agents in v1; events are agent-originated or controller-internal.

---

## 4. Storage schema

Migration `crates/isengard-storage/migrations/0002_events.sql`:

```sql
CREATE TABLE events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id         BLOB NOT NULL,    -- ULID bytes (16) of emitting host; NULL host_id reserved for controller-internal events
    kind            TEXT NOT NULL,
    container_name  TEXT,
    image           TEXT,
    old_digest      TEXT,
    new_digest      TEXT,
    error           TEXT,
    summary         TEXT NOT NULL,
    metadata_json   TEXT,
    occurred_at     TEXT NOT NULL,    -- ISO 8601 string from the agent
    received_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_events_host_id    ON events(host_id);
CREATE INDEX idx_events_kind       ON events(kind);
CREATE INDEX idx_events_occurred   ON events(occurred_at);
```

Controller-internal events (e.g., `agent.disconnect_long`) use the affected host's `host_id`; a future "platform health" event without a host could store NULL — schema allows it, but no producer in v1 emits one.

---

## 5. Core types

In `isengard-core`:

```rust
// Already stubbed in Phase 1; finalise the shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Event {
    pub kind: String,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    pub host_id: Option<HostId>,    // None = controller-internal
    pub summary: String,
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub old_digest: Option<String>,
    pub new_digest: Option<String>,
    pub error: Option<String>,
    pub metadata: serde_json::Value,
}

// New type, replaces the v0.1 stub:
#[async_trait::async_trait]
pub trait EventEmitter: Send + Sync + 'static {
    async fn emit(&self, event: Event);
}

// Existing trait (Phase 1 stub), refined:
#[async_trait::async_trait]
pub trait EventSubscriber: Plugin {
    /// Glob-style kind patterns this subscriber wants. "update.*" matches all update.X.
    fn event_kinds(&self) -> &[&'static str];
    async fn handle(&self, event: &Event) -> Result<()>;
}

// PluginContext gains:
pub struct PluginContext {
    pub mode: HostMode,
    pub config: serde_json::Value,
    pub events: Option<Arc<dyn EventEmitter>>,   // None on controller; Some on agent
}
```

---

## 6. EventBus

In `isengard-controller`:

```rust
pub struct EventBus {
    inner: tokio::sync::broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        // Capacity 1024 — generous; subscribers should keep up but lag-and-drop
        // is the correct semantics if they don't.
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        Self { inner: tx }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.inner.subscribe()
    }

    pub fn publish(&self, event: Event) {
        // Errors here mean no active subscribers — that's fine.
        let _ = self.inner.send(event);
    }
}
```

Plugin host on the controller: after persisting an event to the journal, calls `bus.publish(event)`. Subscribed plugins get a `Receiver` from `subscribe()` during their `init`.

---

## 7. agent.disconnect_long

Background task in the controller, started after `Server::serve`:

```rust
async fn disconnect_monitor(inv: Arc<Inventory>, bus: Arc<EventBus>) {
    let threshold = chrono::Duration::hours(4);
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    let mut already_emitted: HashSet<HostId> = HashSet::new();
    loop {
        ticker.tick().await;
        let now = chrono::Utc::now();
        let hosts = match inv.list_hosts().await {
            Ok(h) => h,
            Err(_) => continue,
        };
        for h in hosts {
            let stale = now - h.last_seen_at > threshold;
            if stale && !already_emitted.contains(&h.id) {
                bus.publish(Event {
                    kind: "agent.disconnect_long".into(),
                    occurred_at: now,
                    host_id: Some(h.id.clone()),
                    summary: format!("agent {} unreachable for over 4h", h.fingerprint),
                    ..Default::default()
                });
                already_emitted.insert(h.id);
            }
            if !stale {
                already_emitted.remove(&h.id);
            }
        }
    }
}
```

Trade-off: in-memory `HashSet` for "already emitted" so we don't spam every minute. Lost on controller restart (replay possible). Acceptable for v1.

---

## 8. Notifier plugin

Crate: `crates/isengard-plugins/notifier/` (already scaffolded empty in Phase 0).

Implements `Plugin + EventSubscriber`. On init:
1. Read config (Telegram bot token + chat IDs, Discord webhook URLs, generic POST URLs, channel-specific kind filters)
2. Subscribe to controller's EventBus
3. Spawn a per-channel worker task that consumes from a per-channel mpsc and dispatches with rate limiting

Channel abstraction:

```rust
#[async_trait::async_trait]
pub trait NotifyChannel: Send + Sync {
    fn name(&self) -> &'static str;
    /// Subset of kinds (glob pattern) this channel cares about.
    fn matches_kind(&self, kind: &str) -> bool;
    /// Send a single event (or batch).
    async fn send(&self, events: &[Event]) -> anyhow::Result<()>;
}
```

Implementations: `TelegramChannel` (v1 default), `DiscordChannel`, `HttpChannel`. Slack deferred to v1.x — workspace-admin gating makes it a worse fit for the v1 audience (solo / small-team operators), and the wire format is essentially `HttpChannel` + a JSON shape if anyone wants to wire it up.

**Telegram channel** — outbound-only in v1:
- Config: `bot_token` (from @BotFather), `chat_ids: Vec<String>` (one bot can post to many chats)
- One-time setup: user creates bot via @BotFather → gets token → adds bot to chat → sends `/start` → reads chat_id from `https://api.telegram.org/bot<TOKEN>/getUpdates`
- Send: `POST https://api.telegram.org/bot<TOKEN>/sendMessage` with `{ chat_id, text, parse_mode: "MarkdownV2" }` (or HTML; pick whichever escapes cleaner)
- No bot polling/getUpdates loop in v1 — we never receive from Telegram, only send
- Rate limit: Telegram's docs cap at 30 msg/sec/bot. Our 10/min default is comfortably below.

Rate limit: token bucket per channel, default 10/min. When token exhausted, events queued; on next refill, batch into one message ("3 events in last minute: ...").

---

## 9. Sub-phase plan

| Sub-phase | Scope |
|---|---|
| 4a | proto Event + AgentMessage variant; `events` table + `Journal` API in isengard-storage; EventBus type in isengard-controller; finalised core types (Event, EventEmitter, EventSubscriber). Pure plumbing — no new producers/consumers. |
| 4b | Agent's sync stream wires an `EventEmitter` impl that queues events onto the existing outbound stream. Controller's Sync handler matches the new variant, persists to journal, broadcasts on EventBus. EventBus passed to plugin host. |
| 4c | Updater emits `update.checked`, `update.success`, `update.failed`, `update.skipped` via the injected EventEmitter. Each event includes container_name, image, digests where applicable. |
| 4d | Notifier plugin scaffold: Plugin + EventSubscriber traits, channel abstraction, **Telegram channel** implementation. Subscribes to bus on init, dispatches matching events with no rate limit (4e adds it). |
| 4e | Discord + generic HTTP POST channels. Token-bucket rate limiter. Batched messages when over limit. |
| 4f | `agent.disconnect_long` background task in controller. Integration test: agent enrolls, controller waits, agent dies, after threshold the event flows to a test-mode notifier that captures into a Vec. |

Each sub-phase is independently shippable. After 4d, Telegram notifications work end-to-end. 4e + 4f are polish + monitoring.

---

## 10. Out of scope (deferred)

- Slack channel (workspace-admin friction, deferred to v1.x — `HttpChannel` covers anyone who really wants to wire an incoming webhook)
- Bot-receives-commands flow (Telegram inbound — would need polling getUpdates loop or webhook; v1.x with `dashboard` integration so users can `/list-hosts` from Telegram)
- Email channel (SMTP setup is hostile in 2026)
- Webhook signing (HMAC) — v1.x
- Per-channel templates (defaults only in v1; configurable templates in v1.x)
- Journal querying API for the dashboard (Phase 5 surfaces it)
- Event acknowledgement / dedup at notifier (rely on rate limit + batching)
- Replay on controller restart (in-memory `already_emitted` set is fine for v1)
- WebSocket live event feed (Phase 5 dashboard concern)
- Webhook DLQ on persistent failures (v1.x — log + count for now)

---

## 11. Open questions

None blocking. Choices documented above. If any direction is wrong, push back before 4a lands and the plumbing locks in.

---

## 12. Done condition for Phase 4

After 4f: Deploy the controller + an agent in Docker against a labelled container with an outdated tag. Within ~30s, the updater performs the recreate. Within ~5s of that, a Telegram message lands in the user's chat: `[isengard] update.success — web (nginx:1.25-alpine) on host-abc, digest sha256:xxx → sha256:yyy`. The journal contains the event. `journalctl -u isengard-controller` shows it received and broadcast.
