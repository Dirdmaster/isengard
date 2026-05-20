Controller-mode runtime: the daemon every Isengard agent talks to.

The controller is one binary serving one gRPC endpoint over mTLS. Agents
enroll, heartbeat, ship container reports, and pull config; operators
push routing rules and secrets. This crate owns the boot path, the
service handlers, and every primitive an agent transaction touches.

# The boot path

[`run_controller`] is the entry point. Order matters: inventory and
journal open against the same SQLite file, the secrets store unlocks
against the master key file, the internal CA loads-or-mints, the
enrollment service composes against the CA, the routing pusher
composes against inventory, plugins start, the placement scheduler and
disconnect monitor spawn, ACME spawns when configured, then the gRPC
server binds and serves until SIGINT.

Every long-lived service the boot path constructs is also exposed to
controller-side plugins through [`ControllerHandles`]: the dashboard
plugin reads inventory and enrollment from there, the backup plugin
reads the db path, the notifier plugin subscribes to [`bus::EventBus`].

# The trust boundary

[`auth::CertAuthInterceptor`] sits on every RPC. Two methods are public
by design (`auth::PUBLIC_METHODS`): `GetCaPem` returns the CA root
PEM over a skip-verify TLS channel so a fresh agent can pin it, and
`Enroll` redeems a one-time token for a leaf cert. Everything else
requires a valid agent cert whose serial is not in
[`revocation::RevocationSet`]. The set is hydrated from `agent_certs`
on boot and mutated through [`revocation::revoke_agent`] which writes
the DB row and the in-memory set in one call.

# The CA

[`ca::Authority`] is the root the controller signs with. One self-signed
ECDSA P-256 root persisted in the single-row `ca` table, 10-year TTL,
loaded or generated on boot. Two minting entry points:
[`ca::Authority::sign_agent_leaf`] (ClientAuth-only, used by enrollment)
and [`ca::Authority::sign_server_leaf_with_sans`] (ClientAuth +
ServerAuth, used by the controller for its own gRPC server cert). The
EKU split closes the Bl-1 horizontal-escalation hole where an agent
leaf could have been presented as a server cert.

# Enrollment

[`enrollment::EnrollmentService`] owns mint and redeem. Mint stores
only the SHA-256 of the token; the plaintext is shown to the operator
once. Redeem looks the token up by hash, signs a leaf, enrolls the
host row, persists the cert, and consumes the token last. The
"consume last" order means the loser of a redeem race produces a
dangling cert (acceptable for an internal CA) rather than a half-
enrolled host.

# The agent stream

The `Controller::sync` impl on [`ControllerService`] holds the
bidirectional Sync stream open for an agent's lifetime. The first
frame must be a `SyncHello`; the `agent_id` it carries is logged but
not trusted, because the authoritative host id is read from the
client cert's CN (set to `host_id.to_string()` by
[`ca::Authority::sign_agent_leaf`]). This is the Bl-2 fix.

Inbound frames fan out to: [`sync_stacks`], [`sync_services`],
[`sync_containers`] for heartbeat payloads; [`policy_ingest`] and
[`hook_ingest`] for container-label reports; [`routing::RoutingPusher`]
for label-driven routing rules; [`log_fanout::LogFanout`] for
WebSocket log streams; [`compose_broker::ComposeBroker`] for the
v0.3d compose-write request/ack matcher.

# Routing

[`routing::RoutingPusher`] is the agent-facing reconciler. It owns a
per-host generation counter that increments only when the rule set or
the wildcard-cert set hashes to a new value. Pushes are best-effort:
a full sender queue drops the message rather than blocking the caller.
The same sender registry doubles as the channel for arbitrary
`ControllerMessage` payloads (start-log-stream, abort-deployment,
write-compose).

# DNS

[`dns::DnsResolver`] serves a single operator-configured zone (e.g.
`iso`, `weavers`) by polling `routing_rules` every 5 seconds and
deriving `<public_hostname>.<zone>` A records from each rule's host
LAN IP. Queries outside the zone return `REFUSED` so the OS falls
through to its upstream resolver; `.local` is filtered out because
mDNS already owns that namespace.

# Deployment

[`stack_deploy_orchestrator::StackDeployOrchestrator`] runs above the
per-host deployment supervisor. When a stack runs on more than one
host, it computes a wave plan from the stack's `parallelism` setting
and walks waves event-by-event, subscribing to `deployment.*` on the
bus. The orchestrator never actuates directly: the
[`stack_deploy_orchestrator::WaveDispatcher`] trait is the seam, and
the production impl queues `ForceUpdate` host actions.

# Placement

[`scheduler::Scheduler`] resolves a service's compose `placement` verb
into concrete `(host_id, replica_index)` rows in the `placements`
table. Three triggers feed it: [`scheduler::Scheduler::on_host_enroll`],
[`scheduler::Scheduler::on_heartbeat_labels`], and
[`scheduler::Scheduler::on_host_disconnect_long`]. A periodic
reconcile timer covers the missed-event case. 0.14 scope: the
scheduler is the planner; the existing per-host apply path stays the
actuator.

# Secrets

[`secrets::SecretsStore`] is the encrypt-on-write side of the v0.3.6
secrets pipeline. The master key is read from a bind-mounted file on
boot (default `/run/secrets/master.key`, override with
`ISENGARD_MASTER_KEY_FILE`); ciphertext is ChaCha20-Poly1305 with the
secret name as AAD. Plaintext never touches disk inside the controller
and never reaches a log line at any level.

# ACME

[`acme`] is the controller-side DNS-01 path for wildcard certs. HTTP-01
covers per-host names from the agent side; wildcards cannot use HTTP-01
because the validator has no host to talk to. The scheduler ticks every
six hours, re-issues thirty days before expiry, persists into
`tls_wildcard_certs`, and kicks a routing push so every connected agent
installs the new cert without waiting for its next reconnect.

# Plugins

[`plugin_host`] walks the `Capability::Controller` plugin registry and
runs each plugin's init then start. The plugin context carries
[`ControllerHandles`] downcast through `Arc<dyn Any>`, so a plugin can
reach into inventory, the bus, the routing pusher, or the secrets
store without a separate dependency on this crate's internals.

# Eventing

[`bus::EventBus`] is the in-process broadcast surface. Publishers
(`sync`, [`disconnect_monitor`], the scheduler) call `publish`;
subscribers (plugins, the orchestrator) call `subscribe`. The
[`persist_and_broadcast`] helper writes to the journal before
publishing so a downstream notifier never reacts to an event we have
no record of.
