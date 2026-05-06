# Phase 13 Plan B: Logs streaming

Replace the placeholder logs panel on the service detail page with a real WebSocket-driven log surface. Closes issue #57.

Source design: `1 Projects/Isengard/Service Detail & Logs Streaming.md` (logs streaming architecture section). Phase 13A landed the route and placeholder; this slab fills the right-column logs surface.

## Scope

In:
- New gRPC streaming RPC `StreamLogs` on the controller-side `Controller` service. Implementation lives on the agent: bollard tail of `docker logs -f`, decode + line-split, send back a stream of `LogChunk`.
- New controller WebSocket endpoint `GET /api/v1/services/:stack_id/:service_name/logs/ws`. Resolves the host(s), opens an agent stream per host, fans lines into the WebSocket as JSON text frames.
- Client-side `<LogsPanel>` Vue component: backfill, live tail, pause/resume, host tabs, level filter, regex filter, last-N slider. Replaces the placeholder card on `[name].vue`.
- Backpressure: bounded mpsc per client. On overflow we emit a `dropped` frame and continue, never blocking the producer.
- Cancellation: client disconnect aborts the per-host streams cleanly.

Out (deferred):
- ANSI rendering, in-buffer regex highlight, file download (Phase 13J).
- Restart / Exec shell / Force update per-service action wiring (Phase 13H/13I).
- Persistence on the controller (out of scope per design).
- Cloudflare Access auth: env-gated bypass for now, same as the rest of the dashboard API.

## Wire protocol (browser <-> controller)

JSON-encoded text frames. No binary frames.

Server -> client:

```json
{"type":"backfill","host":"prod-04","lines":[{"ts":"2026-05-06T14:32:01Z","stream":"stdout","msg":"starting"}]}
{"type":"line","host":"prod-04","ts":"2026-05-06T14:32:02Z","stream":"stdout","msg":"listening on 8080"}
{"type":"dropped","host":"prod-04","count":47,"reason":"backpressure"}
{"type":"unavailable","host":"prod-04","reason":"container_not_found"}
{"type":"closed","reason":"client_disconnect"}
```

Client -> server:

```json
{"type":"pause"}
{"type":"resume"}
{"type":"seek_tail","tail":1000}
{"type":"set_level","level":"warn"}
```

Heartbeat: server pings every 15s; if no traffic for 30s the server closes. Client pongs are handled by the browser automatically.

`set_level` is best-effort: the controller forwards it to the line-classification step (info / warn / error inferred from the line contents). Empty / unknown level passes everything.

## gRPC: `StreamLogs`

Added to the `Controller` service in `isengard.v1.proto`:

```proto
rpc StreamLogs(StreamLogsRequest) returns (stream LogChunk);

message StreamLogsRequest {
  string container_name = 1;
  uint32 tail            = 2; // initial backfill size; 0 = none
  bool   follow          = 3; // true = continuous stream
}

message LogChunk {
  enum Kind { KIND_UNSPECIFIED = 0; KIND_BACKFILL = 1; KIND_LINE = 2; KIND_DROPPED = 3; KIND_UNAVAILABLE = 4; }
  Kind   kind       = 1;
  string occurred_at = 2; // RFC3339
  string stream     = 3; // "stdout" or "stderr"
  string line       = 4; // utf-8, no trailing newline
  uint32 dropped    = 5; // count when kind == KIND_DROPPED
  string reason     = 6; // populated for unavailable / dropped
}
```

The controller is the gRPC server: it receives the agent's outbound stream when the client (a controller-side task) calls `StreamLogs`. **Important**: the existing wire only has `Sync` as a long-lived stream; agents register their inbound channel via the cert. For 13B we introduce a piggyback: agents subscribe to a controller-issued log subscription via a new `ControllerMessage::StartLogStream` / `StopLogStream` payload, and stream `LogChunk` messages back through a new `AgentMessage::LogChunk` payload. This keeps the connection topology stable (single Sync stream per agent) and avoids minting new server-side endpoints on the agent.

In effect, the new "RPC" is **logical**: it lives on top of the already-mTLS-authenticated Sync stream. Subscriptions are keyed by an opaque subscription_id the controller generates per WebSocket client.

## Server-side fanout

Per WebSocket client:

1. Resolve the service: load the stack, the primary service, and walk `other_instances` to discover replicas on other hosts.
2. Generate a `subscription_id` (ULID).
3. For each host, send `ControllerMessage::StartLogStream { subscription_id, container_name, tail, follow }` over that host's Sync sender. The agent opens a bollard `containers.logs(...)` stream and writes back `AgentMessage::LogChunk { subscription_id, ... }` frames.
4. Controller has a per-subscription `tokio::sync::mpsc<LogChunk>(256)` queue. The Sync handler routes incoming `LogChunk` frames into the matching queue.
5. WebSocket task drains the queue, prefixes each line with the host id, JSON-encodes, and sends as text. On `try_send` overflow it emits a `dropped` frame.
6. On client disconnect: the WS task drops the queue. The controller sends `StopLogStream { subscription_id }` to every involved host so agents stop tailing. The agent aborts its `select!` loop.

The fanout is single-process; the controller does not need to broadcast across instances. If the operator runs multiple controllers behind a load balancer (out of scope for v1), each controller fans out independently to the agents it owns.

## Agent-side bollard tail

```rust
let opts = bollard::container::LogsOptions::<String> {
    follow: true,
    stdout: true,
    stderr: true,
    timestamps: true,
    tail: tail.to_string(),
    ..Default::default()
};
let mut stream = docker.logs(&container_name, Some(opts));
loop {
    tokio::select! {
        chunk = stream.next() => match chunk { Some(Ok(c)) => emit_line(c), ... },
        _ = abort_rx.changed() => break,
    }
}
```

Each `LogOutput` carries the stream tag (stdout vs stderr) and the bytes, prefixed with the timestamp the daemon stamps when `timestamps=true`. We strip the timestamp prefix from the bytes, parse it as RFC3339, and emit one `LogChunk` per line. Multi-line bodies are split on `\n`. Non-UTF8 bytes are lossily decoded.

If the container disappears mid-stream, bollard returns `Err`; we emit a `KIND_UNAVAILABLE` chunk with reason and exit the loop. The controller forwards as `unavailable` to the client.

## UI: `<LogsPanel>`

Replaces the dashed-border placeholder card on `pages/stacks/[id]/services/[name].vue`:

```
┌─────────────────────────────────────────────────────┐
│ [aggregate] [prod-04] [prod-05]   tail: 200 ▾  ▶ pause │
│  filter: ____________________   level: all  ▾        │
├─────────────────────────────────────────────────────┤
│ 14:32:01 [info ] starting                           │
│ 14:32:02 [info ] listening on :8080                 │
│ 14:33:11 [warn ] slow query 384ms                   │
│  ▼ live tail                                         │
└─────────────────────────────────────────────────────┘
```

- Single scroll container (CSS `overflow-y:auto`). Auto-scroll to bottom unless the user scrolls up; once scrolled up we surface a "Jump to live" pill.
- Hard cap 5000 lines; older lines drop oldest-first when over.
- `useLogStream` composable: replaces the existing placeholder; opens the WebSocket, parses frames, exposes reactive `lines` / `state` / `dropped`.
- Filter input: case-insensitive substring (regex if the input begins with `/` and ends with `/`).
- Empty: "No logs yet."
- Error: "Stream unavailable: <reason>" with a Retry button.

## Authentication

Same surface as the rest of `/api/v1`: a thin Cloudflare-Access env-gate is on the roadmap. For 13B we accept any local connection; the WebSocket is mounted under the same router as the REST handlers and inherits whatever middleware the dashboard plugin attaches. No new auth surface lands here.

## Tests

Server-side (dashboard):
1. WebSocket handshake upgrades.
2. Backfill frame is delivered first.
3. Live `line` frames flow.
4. `dropped` frame emitted on simulated overflow.
5. Multi-host aggregation interleaves frames from two hosts.
6. Client disconnect cancels the agent stream (verified by checking the cancel flag flips).

Agent-side: bollard interactions are mocked via a thin `LogSource` trait so the tests do not need a live Docker daemon (4 tests: line splitting, abort on cancel, multi-line decode, error -> unavailable).

Proto: round-trip `LogChunk` and the new `AgentMessage` / `ControllerMessage` variants.

UI: the placeholder is gone, the new panel renders without console errors, `bun run build` is clean.

## Decisions

- **WebSocket library**: axum's built-in `axum::extract::ws`. It is already used by the events endpoint. Pulling in `tokio-tungstenite` directly buys nothing here.
- **Backpressure semantics**: bounded mpsc with `try_send`. On overflow we drop the line and increment a per-host counter; the next successful send flushes a `dropped` frame ahead of the next `line`. Never block the producer.
- **Multi-host aggregation**: per-host bounded queues feed a single fan-out task that round-robins across hosts. No global timestamp sort; lines are emitted in the order they arrive at the controller. Lines carry the host id so the client can color-code or filter.
- **Subscription lifecycle**: piggybacked on the existing Sync stream via two new `ControllerMessage` payloads (`StartLogStream`, `StopLogStream`) and one new `AgentMessage` payload (`LogChunk`). Avoids minting a second long-lived agent endpoint.
- **No persistence**: matches the source design's recommendation. Operators who care use Docker log drivers + Loki/Vector; we do not try to be a log aggregator.
