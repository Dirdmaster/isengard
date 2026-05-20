Isengard agent: the per-host daemon.

One process per Docker host. The agent enrolls against the controller, holds an
mTLS-pinned long-lived Sync stream, drives compose reconciliation, materialises
secrets onto tmpfs, runs lifecycle hooks, fans out container logs, and runs the
Pingora reverse proxy. The controller never reaches into the host directly: it
sends `ControllerMessage`s down the stream and the agent reads back
`AgentMessage`s (heartbeat, container labels, journal events).

The crate factors into the following surfaces:

- `enroll`: bootstrap trust. Fingerprint-verified CA fetch (`GetCaPem`), then
  the authenticated `Enroll` RPC. The verified PEM becomes the trust anchor
  for every subsequent mTLS dial.
- `sync`: the bidi `Sync` stream. Heartbeats outbound; routing-rule pushes,
  `WriteCompose`, `AbortDeployment`, `Start/StopLogStream` inbound. Backoff +
  reconnect built on top of `backoff::Backoff`.
- `cert_renewal`: polls the on-disk cert TTL and calls `RenewCert` past 50%.
  Atomically swaps the shared `Endpoint` so the next reconnect uses the new
  identity without restarting the agent.
- `compose_apply` + `compose_reconciler` + `compose_watcher` + `compose_writer` + `compose_import` + `compose_export`: compose-as-truth. Operator edits a `compose.yaml` on disk; the agent reconciles the running containers to match. The reconciler also drives controller-pushed deploys.
- `secret_fetch`: the `FetchSecret` mTLS RPC + tmpfs materialisation at `/run/isengard-secrets/<container>/<name>`, bind-mounted into the workload at `/run/secrets/<name>`.
- `container_snapshot`: the per-heartbeat container view shipped to the controller. Backend-agnostic (drives off `runtime::RuntimeBackend`).
- `lifecycle_hooks`: pre-deploy / post-deploy / failure hook execution. Audit events fan out through the same outbound channel as the journal.
- `proxy`: the Pingora HTTP + HTTPS listener, routing registry, swap + drain semantics, container healthcheck loop, SNI cert callback.
- `tls`: ACME issuance, the in-memory cert store, the renewal scheduler, the HTTP-01 challenge state.
- `runtime`: the `RuntimeBackend` trait + the bollard implementation. Lets the rest of the crate stay backend-agnostic (room for wisp later) while keeping Docker the only shipped backend.
- `deployment`: blue/green driver + supervisor. Recreate at digest, swap the upstream, drain blue, fail back on healthcheck flip.
- `mdns`: `.local` advertisement for routed hostnames so a LAN client can hit `<host>.local` without DNS configuration.

# Lifecycle

`run_agent` is the entry point. On first boot it reads
`ISENGARD_ENROLL_TOKEN`, fetches + pins the controller CA via fingerprint,
runs `Enroll`, persists the cert bundle and `agent.json`, then starts the
sync stream. On every subsequent boot it loads the persisted state and
re-attaches to the controller without a fresh enroll.

# Not for you if

- You want k8s. Reach for k3s or k8s instead.
- You want a single-binary all-in-one. The controller and agent are separate processes by design: the controller can outlive a host reboot.
