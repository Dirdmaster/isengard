Holds the bidirectional Sync stream open for an agent's lifetime.

The first inbound frame must be a `SyncHello`. The `agent_id` carried
there is logged but not trusted: the authoritative host id comes from
the client cert CN, which [`crate::ca::Authority::sign_agent_leaf`]
sets to `host_id.to_string()`. This is the Bl-2 fix. Pre-fix an agent
holding cert A could attribute heartbeats, container reports, and
label reports to host B by sending B's id in the Hello payload.

# Initial side-effects

- Caches the agent's remote IP in `hosts.lan_ip` so the controller's
  DNS resolver can map `<host>.<zone>` to it.
- Registers the outbound sender with [`crate::routing::RoutingPusher`].
- Pushes the current `ProxyConfig` immediately so a reconnecting agent
  catches up without waiting for the next rule change.

# Inbound frames

| Frame                       | Path                                                                                                 |
|-----------------------------|------------------------------------------------------------------------------------------------------|
| `Heartbeat`                 | `touch_host`, persist runtime backend, run `sync_stacks` / `sync_services` / `sync_containers`, ingest labels, feed the scheduler, build a `HeartbeatAck` carrying any pending host actions. |
| `Event`                     | Convert proto to core, stamp `host_id` from the cert CN, journal and broadcast.                       |
| `ContainerLabelsReport`     | Run [`crate::policy_ingest`] + [`crate::hook_ingest`] in parallel, then [`crate::routing::RoutingPusher::ingest_labels`], then push the new config. |
| `ContainerLabelsRemoved`    | Mirror image of the report path.                                                                      |
| `LogChunk`                  | Hand to [`crate::log_fanout::LogFanout::route`].                                                      |
| `StackComposeReport`        | Persist the agent's reverse-engineered compose YAML for v0.3c.                                        |
| `WriteComposeAck`           | Hand to [`crate::compose_broker::ComposeBroker::resolve`].                                            |

# Shutdown

When the stream closes, the spawned task unregisters the outbound
sender from the routing pusher so subsequent `push_to_host` calls for
this host become no-ops until it reconnects.
