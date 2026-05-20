REST + WebSocket dashboard for the Isengard controller.

The plugin runs controller-side. It boots an embedded Nuxt 4 SPA on
port 9418 (configurable via `bind_addr`) and serves the controller's
JSON API plus log-streaming WebSockets on the same axum router.

# Surface map

- `/` and `/_nuxt/*`: Nuxt 4 SPA bundle, baked in via `rust_embed`.
  Unknown routes fall back to `index.html` so Vue Router handles
  client-side navigation.
- `/api/v1/*`: REST handlers split per resource. See [`api`] for the
  core inventory / containers / services / ca / leaf surface;
  [`routing`], [`deployments`], [`deployment_groups`], [`policies`],
  [`approvals`], [`webhooks`], [`backup`], [`secrets`],
  [`enrollment`] each carry their own routers.
- `/ws/events`: bus event stream for the SPA. See [`ws`].
- `/api/v1/services/{stack_id}/{service_name}/logs/ws`:
  per-service log tail. Same module.
- `/install.sh`: one-liner installer payload. Handled by
  [`api::install_sh`].

# Why one binary

Embedding the SPA via `rust_embed` keeps deploy to a single
binary. The Nuxt build runs at compile time
(`dashboard/build.rs`) and the resulting `_nuxt/*.js` chunks land
on disk under `dashboard/web/.output/public`. The plugin reads
them from `WebAssets` at request time.

# Auth boundary

The controller's mTLS proxy sits in front of this listener. Every
request that lands here has already been authed by the controller;
the plugin treats `ControllerHandles` as the trust root and never
reads operator certificates directly.

# Failure mode

A missing `ControllerHandles` (test harness or pre-Phase-5b agent)
leaves the API routes unmounted but keeps the SPA reachable.
Operators see "controller not wired" surfaces; the binary stays
useful for static-site smoke tests.
