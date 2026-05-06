# Phase 13B: Logs streaming

The placeholder card on the service detail page is gone. Open `/stacks/<id>/services/<name>` and the right column now streams `docker logs -f` from each host running the service in real time.

## What works

- New WebSocket endpoint: `GET /api/v1/services/:stack_id/:service_name/logs/ws`. The dashboard SPA opens it on page mount and tears it down on unmount.
- Agent-side bollard tail with `follow=true`, `stdout=true`, `stderr=true`, `timestamps=true`, configurable `tail` (default 200). One `LogChunk` per line, multi-line bodies split.
- Backpressure: per-subscription bounded queue at the controller. On overflow we emit a `dropped` frame with the count and reason, never block the agent.
- Multi-host: when the same service runs on more than one enrolled host, `StartLogStream` fans out to every host. Lines from all hosts feed a single subscription queue and reach the client interleaved.
- Cancellation: client disconnect triggers a `StopLogStream` ControllerMessage to every involved host so the agent's tail tasks abort cleanly.
- UI: host tabs (one per host plus an Aggregate tab), level dropdown (info / warn / error inferred from line text), substring + regex filter, pause / resume button, last-N slider (50 / 200 / 1000), hard cap 5000 lines client-side, "Jump to live" pill when scrolled up.
- Heartbeat: server pings every 15s; the connection closes after 30s of no traffic.

## Wire protocol

Server -> client (JSON text frames):

```json
{"type":"backfill","host":"abc12345","lines":[{"ts":"...","stream":"stdout","msg":"starting"}]}
{"type":"line","host":"abc12345","ts":"...","stream":"stdout","msg":"req GET /"}
{"type":"dropped","host":"abc12345","count":47,"reason":"backpressure"}
{"type":"unavailable","host":"abc12345","reason":"container_not_found"}
{"type":"closed","reason":"no_agents"}
```

Client -> server: `pause`, `resume`, `seek_tail`, `set_level`. All JSON text.

## gRPC additions

The `Controller` service gains three new payloads on the existing Sync stream rather than a separate RPC. This keeps the connection topology stable (one mTLS stream per agent) and avoids minting new server-side endpoints on the agent.

- `ControllerMessage::StartLogStream { subscription_id, container_name, tail, follow }`
- `ControllerMessage::StopLogStream { subscription_id }`
- `AgentMessage::LogChunk { subscription_id, kind, occurred_at, stream, line, dropped, reason }`

`Kind` is one of `BACKFILL`, `LINE`, `DROPPED`, `UNAVAILABLE`. Wire numbers are append-only on the existing `oneof` payloads; old agents and old controllers are still mutually compatible at the protocol level (they just ignore the new variants).

## Tests

- 6 dashboard integration tests (`crates/isengard-plugins/dashboard/tests/logs_ws.rs`): handshake, backfill + live, dropped, unavailable, multi-host aggregation, client disconnect cleanup.
- 4 agent unit tests (`crates/isengard-agent/src/logs.rs`): backfill + lines, abort signal, multi-line decode, error -> unavailable.
- 4 controller unit tests (`crates/isengard-controller/src/log_fanout.rs`): register/route, unregister drops, unknown subscription, full queue drops.
- 5 proto round-trip tests for `LogChunk` / `StartLogStream` / `StopLogStream`.

## Deferred

- ANSI escape rendering, in-buffer regex highlight, log download (Phase 13J).
- Restart and Exec shell quick actions (Phase 13H/13I).
- Persistence: out of scope by design. Operators who need it use Docker log drivers + Loki / Vector / Fluentd.
- Cloudflare Access auth gating: still env-gated alongside the rest of the dashboard API; no new auth surface lands here.

## Operator notes

- The new path replaces the old `/api/v1/services/{id}/logs` placeholder. Old clients hitting that path get 404 and should refresh.
- A single Aggregate tab with 10 hosts producing 1K lines/sec each will hit the per-subscription drop counter quickly. The UI surfaces `Dropped: host=N` counts in the panel footer so the operator can see when this is happening.
- The agent's tail subscription is keyed by `subscription_id`; restarting the agent or losing the Sync stream cancels every active subscription on that host. The dashboard auto-reconnects on the next page mount.
