# Blue-Green UI + Abort + Settings Override Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface Plan A's blue-green deployment driver in the dashboard (in-flight progress panel + abort button), add post-switch collapse recovery in the agent's Driver, and let users override the per-service deploy strategy via Settings.

**Architecture:** Three sub-systems wired through existing seams. (1) Agent → controller: Driver embeds the full `Deployment` row in `event.metadata.deployment` for every `deployment.*` event; controller's event bus subscriber upserts into its local `deployments` table. (2) Controller → agent abort: new `ControllerMessage::AbortDeployment` proto variant, sent via the `RoutingPusher`'s existing per-host sender registry; agent's Sync receiver dispatches to `Supervisor::handle_abort` which cancels a `tokio_util::sync::CancellationToken`. (3) Driver gets `tokio::select!` over (grace_period sleep, abort token, `proxy_events` broadcast) during draining, with a `recover_to_blue` helper that re-calls `swap_upstream` against the saved blue snapshot.

**Tech Stack:** Rust + sqlx (SQLite) + bollard (Docker) + tokio + `tokio_util::sync::CancellationToken` + tokio broadcast + tonic gRPC + Vue 3 / Nuxt 3 / Pinia / TypeScript.

**Spec:** `docs/superpowers/specs/2026-05-04-phase-10e-10g-blue-green-ui-design.md`

**Branch:** `feat/blue-green-ui` stacked on `feat/blue-green-core` (PR #21).

---

## File map

Create:
- `crates/isengard-storage/migrations/0012_services_deploy_strategy.sql`
- `crates/isengard-agent/src/proxy/events.rs` (proxy event broadcast)
- `crates/isengard-plugins/dashboard/src/deployments.rs` (REST endpoints)
- `crates/isengard-plugins/dashboard/web/composables/useDeployments.ts`
- `crates/isengard-plugins/dashboard/web/composables/useServiceDeployStrategy.ts`
- `crates/isengard-plugins/dashboard/web/components/DeploymentInProgressPanel.vue`
- `crates/isengard-plugins/dashboard/web/components/DeploymentAbortedPanel.vue`
- `crates/isengard-plugins/dashboard/web/components/DeploymentsSettings.vue`

Modify:
- `crates/isengard-storage/src/service.rs` (add `deploy_strategy_override` + setter + `get_service_by_name`)
- `crates/isengard-storage/src/deployment.rs` (add `upsert_deployment_from_remote`)
- `crates/isengard-proto/proto/isengard.v1.proto` (add `AbortDeployment` to ControllerMessage oneof)
- `crates/isengard-agent/src/proxy/healthcheck.rs` (extend health_changed event metadata + emit on broadcast channel)
- `crates/isengard-agent/src/proxy/mod.rs` (re-export `proxy_events`)
- `crates/isengard-agent/src/deployment/driver.rs` (Recovering state, abort_token, tokio::select!, recover_to_blue, blue_upstream snapshot, emit Deployment in metadata)
- `crates/isengard-agent/src/deployment/mod.rs` (Supervisor abort_tokens map + handle_abort, override consultation order, ProxyEventsRx wiring)
- `crates/isengard-agent/src/sync.rs` or wherever ControllerMessage is dispatched (route AbortDeployment)
- `crates/isengard-controller/src/routing.rs` (extend `RoutingPusher` with `send_message_to_host`)
- `crates/isengard-controller/src/event_handler.rs` or `lib.rs` (extend event bus subscriber to upsert deployment metadata)
- `crates/isengard-plugins/dashboard/src/lib.rs` (mount deployments router)
- `crates/isengard-plugins/dashboard/web/pages/stacks/[id].vue` (mount panels above services)
- `crates/isengard-plugins/dashboard/web/pages/settings/index.vue` (add Deployments tab)

---

## Task split

| Task | Sub-phase | Scope | New tests |
|---|---|---|---|
| 1 | 10g-storage | Migration 0012 + service column + getter/setter + lookup-by-name | 3 storage |
| 2 | 10e-driver-emit | Driver embeds Deployment in event.metadata + Supervisor consults service override | 2 (1 driver, 1 supervisor) |
| 3 | 10e-storage | `upsert_deployment_from_remote` storage method | 1 storage |
| 4 | 10e-controller | Event bus subscriber upserts deployment.* events into local table | 1 controller integration |
| 5 | 10e-rest | `GET /api/v1/deployments?stack_id=&state=` REST endpoint | 2 endpoint |
| 6 | 10e-frontend | `useDeployments` composable + DeploymentInProgressPanel + Stack detail wire-up | (manual smoke + bun build) |
| 7 | 10f-proxy-events | New `proxy/events.rs` broadcast + extend healthcheck.rs to emit on it with structured metadata | 1 broadcast |
| 8 | 10f-driver | Recovering state + abort_token + tokio::select! + recover_to_blue + blue snapshot | 4 driver |
| 9 | 10f-proto-sync | Proto AbortDeployment + RoutingPusher::send_message_to_host + agent Sync receiver dispatch | 1 sync |
| 10 | 10f-rest-frontend | `POST /api/v1/deployments/:id/abort` + DeploymentAbortedPanel + abort button wire-up + Retry → force-update | 2 endpoint |
| 11 | 10g-frontend | DeploymentsSettings.vue + Deployments tab + composable + GET/PUT REST | 2 endpoint |
| 12 | 10h | Final workspace gates + open PR #22 | (gates) |

---

## Critical type notes carried over from Plan A

These corrections were surfaced during Plan A execution and apply throughout:

- `Error::Decode { reason: String }` is a struct variant (NOT tuple).
- `inv.enroll_host(...)` returns `Result<HostId>` directly.
- `inv.insert_stack(...)` returns `Result<StackId>` directly.
- `EnrollHost` has 7 fields: `hostname, fingerprint, fleet, os, arch, agent_version, docker_version` (all `String`).
- `inventory.pool()` is `pub(crate)` — DO NOT call from outside isengard-storage.
- `isengard_core::HostId = ulid::Ulid` (type alias). `isengard_storage::HostId = struct HostId(pub Ulid)` (newtype). Convert with `.0` (storage→core) or `HostId(ulid)` (core→storage).
- `RoutingRule.healthcheck_path: Option<String>`, `RoutingRule.public_hostname: String`, `RoutingRule.container_port: u16`.
- `ProxyState` is `Clone` and exposes `pub upstreams: Arc<RwLock<UpstreamRegistry>>`.

---

## Task 1: services.deploy_strategy_override column

**Files:**
- Create: `crates/isengard-storage/migrations/0012_services_deploy_strategy.sql`
- Modify: `crates/isengard-storage/src/service.rs`

- [ ] **Step 1: Migration**

Create `crates/isengard-storage/migrations/0012_services_deploy_strategy.sql`:

```sql
-- Phase 10g: per-service deploy strategy override.
-- Values: NULL or 'auto' = auto-detect (default), 'blue-green' or 'in-place' = explicit override.

ALTER TABLE services ADD COLUMN deploy_strategy_override TEXT
    CHECK (deploy_strategy_override IS NULL OR deploy_strategy_override IN ('auto', 'blue-green', 'in-place'));
```

- [ ] **Step 2: Failing tests in service.rs**

Append to `crates/isengard-storage/src/service.rs` test module (or create one if absent):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::EnrollHost;
    use crate::inventory::Inventory;
    use crate::stack::{InsertStack, StackSource};

    async fn setup() -> (Inventory, ServiceId) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv.enroll_host(EnrollHost {
            hostname: "h".into(), fingerprint: "fp".into(), fleet: "default".into(),
            os: "linux".into(), arch: "x86_64".into(),
            agent_version: "0.1.0".into(), docker_version: "24.0".into(),
        }).await.unwrap();
        let stack_id = inv.insert_stack(InsertStack {
            host_id, name: "blog".into(), source: StackSource::Compose,
        }).await.unwrap();
        let svc = inv.insert_service(InsertService {
            host_id,
            stack_id: Some(stack_id),
            name: "web".into(),
            container_id: Some("c1".into()),
            image: Some("nginx:alpine".into()),
            state: ServiceState::Running,
        }).await.unwrap();
        (inv, svc.id)
    }

    #[tokio::test]
    async fn override_starts_null() {
        let (inv, svc_id) = setup().await;
        let svc = inv.get_service(svc_id).await.unwrap().unwrap();
        assert_eq!(svc.deploy_strategy_override, None);
    }

    #[tokio::test]
    async fn set_and_get_override() {
        let (inv, svc_id) = setup().await;
        inv.set_service_deploy_strategy_override(svc_id, Some("blue-green")).await.unwrap();
        let svc = inv.get_service(svc_id).await.unwrap().unwrap();
        assert_eq!(svc.deploy_strategy_override.as_deref(), Some("blue-green"));

        inv.set_service_deploy_strategy_override(svc_id, None).await.unwrap();
        let svc = inv.get_service(svc_id).await.unwrap().unwrap();
        assert_eq!(svc.deploy_strategy_override, None);
    }

    #[tokio::test]
    async fn lookup_by_name_returns_matching_service() {
        let (inv, svc_id) = setup().await;
        let svc = inv.get_service(svc_id).await.unwrap().unwrap();
        let by_name = inv
            .get_service_by_name(svc.host_id, svc.stack_id, "web")
            .await
            .unwrap()
            .expect("service found by name");
        assert_eq!(by_name.id, svc_id);

        let missing = inv
            .get_service_by_name(svc.host_id, svc.stack_id, "doesnotexist")
            .await
            .unwrap();
        assert!(missing.is_none());
    }
}
```

If `InsertService` has different field names than above, adapt to match the existing struct definition (read `crates/isengard-storage/src/service.rs` first). If `get_service` doesn't exist, you'll add it in Step 4.

- [ ] **Step 3: Run tests, expect fail**

Run: `cargo test -p isengard-storage --lib service::tests`
Expected: compile error — `deploy_strategy_override` field undefined; methods `set_service_deploy_strategy_override` and `get_service_by_name` undefined.

- [ ] **Step 4: Implement**

In `crates/isengard-storage/src/service.rs`:

1. Add field to `Service` struct:
```rust
pub struct Service {
    // ... existing fields ...
    pub deploy_strategy_override: Option<String>,
}
```

2. Add methods to `impl crate::inventory::Inventory`:

```rust
pub async fn set_service_deploy_strategy_override(
    &self,
    service_id: ServiceId,
    override_value: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE services SET deploy_strategy_override = ? WHERE id = ?"
    )
    .bind(override_value)
    .bind(service_id.0)
    .execute(self.pool())
    .await?;
    Ok(())
}

pub async fn get_service_by_name(
    &self,
    host_id: HostId,
    stack_id: Option<StackId>,
    service_name: &str,
) -> Result<Option<Service>> {
    use sqlx::Row;
    let host_bytes = host_id.0.to_bytes().to_vec();
    let row = match stack_id {
        Some(sid) => sqlx::query(
            "SELECT id, host_id, stack_id, name, container_id, image, state, deploy_strategy_override
             FROM services
             WHERE host_id = ? AND stack_id = ? AND name = ?"
        )
        .bind(&host_bytes)
        .bind(sid.0)
        .bind(service_name)
        .fetch_optional(self.pool())
        .await?,
        None => sqlx::query(
            "SELECT id, host_id, stack_id, name, container_id, image, state, deploy_strategy_override
             FROM services
             WHERE host_id = ? AND stack_id IS NULL AND name = ?"
        )
        .bind(&host_bytes)
        .bind(service_name)
        .fetch_optional(self.pool())
        .await?,
    };
    let Some(r) = row else { return Ok(None) };
    Ok(Some(row_to_service(&r)?))
}
```

Update `row_to_service` (or whichever helper deserializes Service rows) to also read the new column. Update existing SELECT statements that load Service rows to include `deploy_strategy_override` in the column list. Use `r.try_get("deploy_strategy_override").unwrap_or(None)` defensively if the column might be missing during migrations on older DBs.

If `get_service(id)` doesn't exist, add it:
```rust
pub async fn get_service(&self, id: ServiceId) -> Result<Option<Service>> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT id, host_id, stack_id, name, container_id, image, state, deploy_strategy_override
         FROM services WHERE id = ?"
    )
    .bind(id.0)
    .fetch_optional(self.pool())
    .await?;
    let Some(r) = row else { return Ok(None) };
    Ok(Some(row_to_service(&r)?))
}
```

- [ ] **Step 5: Run tests, expect pass**

Run: `cargo test -p isengard-storage --lib service::tests`
Expected: 3/3 pass.

Run: `cargo test -p isengard-storage`
Expected: full storage suite green.

- [ ] **Step 6: Commit**

```bash
cd /Users/dirdmaster/Projects/isengard/.worktrees/blue-green-ui
cargo fmt --all
git add crates/isengard-storage/migrations/0012_services_deploy_strategy.sql \
        crates/isengard-storage/src/service.rs
git commit -m "feat(storage): services.deploy_strategy_override column + setter + lookup-by-name"
```

---

## Task 2: Driver embeds Deployment in event metadata + Supervisor consults service override

**Files:**
- Modify: `crates/isengard-agent/src/deployment/driver.rs` (extend `emit`)
- Modify: `crates/isengard-agent/src/deployment/mod.rs` (Supervisor consults service override)

- [ ] **Step 1: Driver emit extension**

In `crates/isengard-agent/src/deployment/driver.rs`, find the `Driver::emit` method (added in Plan A). It currently builds an `Event` with `metadata: serde_json::json!({ "deployment_id": ..., "service_name": ..., ... })`. Replace the metadata builder with the full Deployment row plus a flat `kind` discriminator:

```rust
fn emit(&self, kind: &str, error: Option<String>) {
    let deployment_json = serde_json::to_value(&self.deployment)
        .unwrap_or_else(|_| serde_json::json!({}));
    let event = Event {
        kind: kind.to_string(),
        occurred_at: Utc::now(),
        host_id: Some(self.deployment.host_id.0),  // storage::HostId.0 → core::HostId (Ulid)
        summary: format!(
            "{} {} {}",
            self.deployment.service_name,
            self.deployment.green_digest,
            kind
        ),
        error,
        metadata: serde_json::json!({
            "deployment": deployment_json,
        }),
        ..Default::default()
    };
    let emitter = self.emitter.clone();
    tokio::spawn(async move {
        emitter.emit(event).await;
    });
}
```

Note the `.0` on `host_id` — Plan A's Driver had this same wrinkle (storage HostId is a newtype around Ulid, core HostId is the Ulid alias).

- [ ] **Step 2: Driver test for metadata shape**

Append to `driver.rs` test module:

```rust
#[tokio::test]
async fn emit_includes_full_deployment_in_metadata() {
    use isengard_core::Event;
    use std::sync::Mutex as StdMutex;

    // Capturing emitter:
    struct CaptureEmitter {
        events: Arc<StdMutex<Vec<Event>>>,
    }
    #[async_trait::async_trait]
    impl EventEmitter for CaptureEmitter {
        async fn emit(&self, event: Event) {
            self.events.lock().unwrap().push(event);
        }
    }

    let captured = Arc::new(StdMutex::new(Vec::new()));
    let emitter: Arc<dyn EventEmitter> = Arc::new(CaptureEmitter { events: captured.clone() });

    let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;

    // Use any minimal mock for DriverDeps (the test doesn't run the state machine,
    // it only exercises emit()):
    struct NoopDeps;
    #[async_trait::async_trait]
    impl DriverDeps for NoopDeps {
        async fn start_green(&self, _: &Deployment) -> Result<(String, SocketAddr)> {
            Ok(("g".into(), "127.0.0.1:0".parse().unwrap()))
        }
        async fn stop_and_remove(&self, _: &str) -> Result<()> { Ok(()) }
        fn build_health_checker(&self, _: &Deployment) -> HealthChecker {
            HealthChecker::tcp_only(Duration::from_millis(10))
        }
        async fn swap_upstream_to_green(&self, _: &Deployment, _: SocketAddr, _: &str, _: Duration) -> Result<()> { Ok(()) }
    }

    let driver = Driver::new(d.clone(), Arc::new(NoopDeps), inv, emitter);
    driver.emit("deployment.spinning_up", None);
    // emit() spawns; let it run.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let events = captured.lock().unwrap();
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(e.kind, "deployment.spinning_up");
    let dep = e.metadata.get("deployment").expect("deployment in metadata");
    assert_eq!(dep.get("id").and_then(|v| v.as_str()), Some(d.id.as_str()));
    assert_eq!(dep.get("state").and_then(|v| v.as_str()), Some("pending"));
}
```

- [ ] **Step 3: Run, expect pass**

Run: `cargo test -p isengard-agent --lib deployment::driver::tests::emit_includes_full_deployment_in_metadata`
Expected: pass.

Run: `cargo test -p isengard-agent --lib`
Expected: all green (existing 37+ tests untouched + 1 new).

- [ ] **Step 4: Supervisor consults service override**

In `crates/isengard-agent/src/deployment/mod.rs`, find `DeploymentSupervisor::handle_update_trigger`. It currently classifies via `eligibility::classify(&spec)` where `spec.label_strategy = trigger.label_strategy.as_deref()`.

Insert a service-override lookup BETWEEN the label override path and the auto-detect path. The classifier's existing label-priority logic handles step 1 (label override wins). Step 2 (service override) needs to be inserted manually since classify() doesn't know about services.

Refactor `handle_update_trigger` to:

```rust
pub async fn handle_update_trigger(&self, trigger: UpdateTrigger) -> Result<SupervisorOutcome> {
    // Dedupe (existing).
    let in_flight = self.inventory
        .list_in_flight_for_service(trigger.host_id, &trigger.service_name)
        .await?;
    if !in_flight.is_empty() {
        return Ok(SupervisorOutcome::AlreadyInFlight);
    }

    // Resolve effective label_strategy: container label wins; if absent,
    // fall back to the service's stored deploy_strategy_override.
    let effective_strategy: Option<String> = trigger.label_strategy.clone().or_else(|| {
        // Synchronous lookup is awkward; use a separate async call:
        None  // placeholder, replaced below
    });
    // Actual lookup (async):
    let stored_override = self.inventory
        .get_service_by_name(trigger.host_id, Some(trigger.stack_id), &trigger.service_name)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.deploy_strategy_override);
    let effective_strategy = trigger.label_strategy.clone().or(stored_override);

    let mounts: Vec<String> = trigger.rw_volume_mounts.clone();
    let spec = ContainerSpec {
        has_routing_rule: trigger.public_hostname.is_some(),
        has_healthcheck: trigger.has_healthcheck,
        rw_volume_mounts: &mounts,
        label_strategy: effective_strategy.as_deref(),
    };

    // Rest as before: classify(), insert_deployment, spawn driver, etc.
    match classify(&spec) {
        Decision::InPlace { .. } => Ok(SupervisorOutcome::InPlaceForUpdater),
        Decision::BlueGreen => {
            // ... existing insert + spawn logic ...
        }
    }
}
```

This passes the stored override through the same `label_strategy` field of the classifier — which means an "in-place" stored override classifies as InPlace with `LabelForced` reason. Acceptable: the reason is informational; the user understands it as "explicit override".

- [ ] **Step 5: Supervisor test**

Append to `supervisor_tests` module in `mod.rs`:

```rust
#[tokio::test]
async fn supervisor_consults_service_deploy_strategy_override() {
    let (sup, host, stack, inv) = setup().await;

    // Insert a service row with override = "in-place".
    let svc = inv.insert_service(isengard_storage::service::InsertService {
        host_id: host,
        stack_id: Some(stack),
        name: "web".into(),
        container_id: Some("c1".into()),
        image: Some("nginx:alpine".into()),
        state: isengard_storage::service::ServiceState::Running,
    }).await.unwrap();
    inv.set_service_deploy_strategy_override(svc.id, Some("in-place")).await.unwrap();

    // Trigger looks blue-green-eligible (has hostname, has healthcheck, no volumes,
    // no container label) but the stored override forces in-place.
    let mut t = trigger(host, stack, true);  // helper from existing tests
    t.label_strategy = None;  // no container label; relies on stored override
    let outcome = sup.handle_update_trigger(t).await.unwrap();
    assert_eq!(outcome, SupervisorOutcome::InPlaceForUpdater);
}
```

If the existing `trigger()` helper sets `label_strategy = Some("blue-green")`, override it to `None` for this test as shown.

- [ ] **Step 6: Run tests**

Run: `cargo test -p isengard-agent --lib deployment::supervisor_tests`
Expected: all pass (existing 3 + 1 new).

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/deployment/driver.rs \
        crates/isengard-agent/src/deployment/mod.rs
git commit -m "feat(agent): driver embeds Deployment in event metadata + supervisor honors service strategy override"
```

---

## Task 3: `upsert_deployment_from_remote` storage method

**Files:**
- Modify: `crates/isengard-storage/src/deployment.rs`

- [ ] **Step 1: Failing test**

Append to `deployment.rs` test module:

```rust
#[tokio::test]
async fn upsert_from_remote_inserts_or_replaces() {
    let (inv, host_id, stack_id) = setup().await;

    // Build a Deployment as if it arrived from a remote agent.
    let mut d = Deployment {
        id: ulid::Ulid::new().to_string(),
        host_id,
        stack_id,
        service_name: "web".into(),
        strategy: DeployStrategy::BlueGreen,
        state: DeploymentState::SpinningUp,
        blue_container: Some("c-blue".into()),
        green_container: None,
        blue_digest: "sha256:aaa".into(),
        green_digest: "sha256:bbb".into(),
        public_hostname: Some("blog.test".into()),
        health_path: Some("/healthz".into()),
        container_port: Some(8080),
        healthcheck_started_at: None,
        healthcheck_passed_at: None,
        switched_at: None,
        drained_at: None,
        finished_at: None,
        error: None,
        metadata_json: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    // First upsert: insert.
    inv.upsert_deployment_from_remote(&d).await.unwrap();
    let back = inv.get_deployment(&d.id).await.unwrap().unwrap();
    assert_eq!(back.state, DeploymentState::SpinningUp);

    // Second upsert with state change: replace.
    d.state = DeploymentState::Done;
    d.green_container = Some("c-green".into());
    inv.upsert_deployment_from_remote(&d).await.unwrap();
    let back = inv.get_deployment(&d.id).await.unwrap().unwrap();
    assert_eq!(back.state, DeploymentState::Done);
    assert_eq!(back.green_container.as_deref(), Some("c-green"));
}
```

- [ ] **Step 2: Implement**

Add to `impl crate::inventory::Inventory` in `crates/isengard-storage/src/deployment.rs`:

```rust
/// Upsert a Deployment row received from a remote source (controller-side use).
/// Idempotent INSERT OR REPLACE keyed on `id`. Each upsert carries the full row,
/// so latest-write-wins per field. Used by the controller's event subscriber.
pub async fn upsert_deployment_from_remote(&self, d: &Deployment) -> Result<()> {
    let host_bytes = d.host_id.0.to_bytes().to_vec();
    let to_rfc = |dt: Option<chrono::DateTime<chrono::Utc>>| dt.map(|t| t.to_rfc3339());
    sqlx::query(
        r#"
        INSERT OR REPLACE INTO deployments (
            id, host_id, stack_id, service_name, strategy, state,
            blue_container, green_container, blue_digest, green_digest,
            public_hostname, health_path, container_port,
            healthcheck_started_at, healthcheck_passed_at, switched_at,
            drained_at, finished_at, error, metadata_json,
            created_at, updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&d.id)
    .bind(&host_bytes)
    .bind(d.stack_id.0)
    .bind(&d.service_name)
    .bind(d.strategy.as_str())
    .bind(d.state.as_str())
    .bind(&d.blue_container)
    .bind(&d.green_container)
    .bind(&d.blue_digest)
    .bind(&d.green_digest)
    .bind(&d.public_hostname)
    .bind(&d.health_path)
    .bind(d.container_port)
    .bind(to_rfc(d.healthcheck_started_at))
    .bind(to_rfc(d.healthcheck_passed_at))
    .bind(to_rfc(d.switched_at))
    .bind(to_rfc(d.drained_at))
    .bind(to_rfc(d.finished_at))
    .bind(&d.error)
    .bind(&d.metadata_json)
    .bind(d.created_at.to_rfc3339())
    .bind(d.updated_at.to_rfc3339())
    .execute(self.pool())
    .await?;
    Ok(())
}
```

- [ ] **Step 3: Run, expect pass**

Run: `cargo test -p isengard-storage --lib deployment::tests::upsert_from_remote_inserts_or_replaces`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/isengard-storage/src/deployment.rs
git commit -m "feat(storage): upsert_deployment_from_remote (controller-side mirror)"
```

---

## Task 4: Controller event bus subscriber upserts deployment.* events

**Files:**
- Modify: `crates/isengard-controller/src/lib.rs` (or `event_handler.rs` if one exists)

- [ ] **Step 1: Locate the event handler site**

Run: `grep -rn "persist_and_broadcast\|bus.subscribe\|bus.send" crates/isengard-controller/src/`

The controller already has `persist_and_broadcast(journal, bus, event)` which both journals AND broadcasts. We need a separate consumer that subscribes to the bus and runs side-effects (upserts) on deployment events. This avoids coupling the journal write to the deployment upsert.

Find where the controller spawns long-running tasks during startup (likely `lib.rs`'s `serve` or `start` function). Add a new spawn:

```rust
// After bus is constructed:
let deployment_handler_inv = inventory.clone();
let mut deployment_rx = bus.subscribe();
tokio::spawn(async move {
    while let Ok(event) = deployment_rx.recv().await {
        if !event.kind.starts_with("deployment.") {
            continue;
        }
        let Some(dep_value) = event.metadata.get("deployment") else {
            tracing::warn!(kind = %event.kind, "deployment event missing metadata.deployment");
            continue;
        };
        let dep: isengard_storage::deployment::Deployment =
            match serde_json::from_value(dep_value.clone()) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(kind = %event.kind, error = %e, "deployment metadata parse failed");
                    continue;
                }
            };
        if let Err(e) = deployment_handler_inv.upsert_deployment_from_remote(&dep).await {
            tracing::warn!(kind = %event.kind, error = %e, "upsert_deployment_from_remote failed");
        }
    }
});
```

If `EventBus::subscribe()` returns a different type than `tokio::sync::broadcast::Receiver`, adapt the `recv` call accordingly. Read `crates/isengard-controller/src/bus.rs` first to confirm the API.

- [ ] **Step 2: Integration test**

Create `crates/isengard-controller/tests/deployment_event_upserts.rs`:

```rust
//! Verifies that emitting a `deployment.*` event with a serialized Deployment
//! in metadata results in an upsert into the controller's deployments table.

use isengard_controller::{ControllerHandles, persist_and_broadcast};
use isengard_core::Event;
use isengard_storage::deployment::{DeployStrategy, Deployment, DeploymentState};
use isengard_storage::host::EnrollHost;
use isengard_storage::inventory::Inventory;
use isengard_storage::stack::{InsertStack, StackSource};
use std::time::Duration;
use ulid::Ulid;

#[tokio::test]
async fn deployment_event_with_metadata_upserts_row() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let host_id = inv.enroll_host(EnrollHost {
        hostname: "h".into(), fingerprint: "fp".into(), fleet: "default".into(),
        os: "linux".into(), arch: "x86_64".into(),
        agent_version: "0.1.0".into(), docker_version: "24.0".into(),
    }).await.unwrap();
    let stack_id = inv.insert_stack(InsertStack {
        host_id, name: "blog".into(), source: StackSource::Compose,
    }).await.unwrap();

    // Construct ControllerHandles (or the minimum subset for this test).
    // The test imports persist_and_broadcast or the equivalent that fans out.
    // For this scaffold, we use a journal-less bus directly:
    let bus = std::sync::Arc::new(isengard_controller::bus::EventBus::new());
    let inv_for_handler = inv.clone();
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if !event.kind.starts_with("deployment.") { continue; }
            let Some(dep_v) = event.metadata.get("deployment") else { continue };
            if let Ok(d) = serde_json::from_value::<Deployment>(dep_v.clone()) {
                let _ = inv_for_handler.upsert_deployment_from_remote(&d).await;
            }
        }
    });

    let dep = Deployment {
        id: Ulid::new().to_string(),
        host_id,
        stack_id,
        service_name: "web".into(),
        strategy: DeployStrategy::BlueGreen,
        state: DeploymentState::Switching,
        blue_container: Some("c-blue".into()),
        green_container: Some("c-green".into()),
        blue_digest: "sha256:aaa".into(),
        green_digest: "sha256:bbb".into(),
        public_hostname: Some("blog.test".into()),
        health_path: Some("/".into()),
        container_port: Some(80),
        healthcheck_started_at: None,
        healthcheck_passed_at: None,
        switched_at: None,
        drained_at: None,
        finished_at: None,
        error: None,
        metadata_json: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let event = Event {
        kind: "deployment.switched".into(),
        occurred_at: chrono::Utc::now(),
        host_id: Some(host_id.0),
        summary: "test".into(),
        metadata: serde_json::json!({ "deployment": dep }),
        ..Default::default()
    };
    bus.send(event).expect("publish");

    // Wait for the handler to upsert.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let back = inv.get_deployment(&dep.id).await.unwrap().expect("deployment row exists");
    assert_eq!(back.state, DeploymentState::Switching);
}
```

If `EventBus` has a different API (e.g., it's not pub or is wrapped differently), use the actual public surface. The key assertions: (1) bus.send accepts the event, (2) the handler upserts.

- [ ] **Step 3: Run, expect pass**

Run: `cargo test -p isengard-controller --test deployment_event_upserts`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/isengard-controller/src/lib.rs \
        crates/isengard-controller/tests/deployment_event_upserts.rs
git commit -m "feat(controller): event bus subscriber upserts deployment.* metadata into local table"
```

---

## Task 5: GET /api/v1/deployments REST endpoint

**Files:**
- Create: `crates/isengard-plugins/dashboard/src/deployments.rs`
- Modify: `crates/isengard-plugins/dashboard/src/lib.rs`

- [ ] **Step 1: Module + router**

Create `crates/isengard-plugins/dashboard/src/deployments.rs`:

```rust
//! REST endpoints for blue-green deployments. See spec §10e-rest.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use isengard_controller::ControllerHandles;
use isengard_storage::deployment::{Deployment, DeploymentState};
use isengard_storage::stack::StackId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub stack_id: Option<i64>,
    pub state: Option<String>,         // "active" | "history"
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct DeploymentDto {
    pub id: String,
    pub host_id: String,
    pub stack_id: i64,
    pub service_name: String,
    pub strategy: String,
    pub state: String,
    pub blue_container: Option<String>,
    pub green_container: Option<String>,
    pub blue_digest: String,
    pub green_digest: String,
    pub public_hostname: Option<String>,
    pub healthcheck_passed_at: Option<String>,
    pub switched_at: Option<String>,
    pub drained_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<Deployment> for DeploymentDto {
    fn from(d: Deployment) -> Self {
        DeploymentDto {
            id: d.id,
            host_id: d.host_id.0.to_string(),
            stack_id: d.stack_id.0,
            service_name: d.service_name,
            strategy: d.strategy.as_str().to_string(),
            state: d.state.as_str().to_string(),
            blue_container: d.blue_container,
            green_container: d.green_container,
            blue_digest: d.blue_digest,
            green_digest: d.green_digest,
            public_hostname: d.public_hostname,
            healthcheck_passed_at: d.healthcheck_passed_at.map(|t| t.to_rfc3339()),
            switched_at: d.switched_at.map(|t| t.to_rfc3339()),
            drained_at: d.drained_at.map(|t| t.to_rfc3339()),
            finished_at: d.finished_at.map(|t| t.to_rfc3339()),
            error: d.error,
            created_at: d.created_at.to_rfc3339(),
            updated_at: d.updated_at.to_rfc3339(),
        }
    }
}

pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/deployments", get(list_deployments))
        // POST /:id/abort lands in Task 10
        .with_state(handles)
}

async fn list_deployments(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<DeploymentDto>>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(50).min(200);
    let state_filter = q.state.as_deref().unwrap_or("active");

    let deps = match (q.stack_id, state_filter) {
        (Some(sid), "active") => {
            // list_in_flight_deployments by host doesn't filter by stack;
            // do an in-memory filter for v1 (small lists).
            let stacks_in_flight = handles.inventory
                .list_deployments_by_stack(StackId(sid), limit)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
            stacks_in_flight.into_iter()
                .filter(|d| !d.state.is_terminal())
                .collect::<Vec<_>>()
        }
        (Some(sid), "history") => {
            let all = handles.inventory
                .list_deployments_by_stack(StackId(sid), limit)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
            all.into_iter().filter(|d| d.state.is_terminal()).collect::<Vec<_>>()
        }
        (Some(sid), _) => {
            return Err((StatusCode::BAD_REQUEST, format!("unknown state filter: {state_filter}")));
        }
        (None, _) => {
            return Err((StatusCode::BAD_REQUEST, "stack_id query param required".into()));
        }
    };

    Ok(Json(deps.into_iter().map(DeploymentDto::from).collect()))
}
```

- [ ] **Step 2: Mount in dashboard router**

In `crates/isengard-plugins/dashboard/src/lib.rs`, add the module and merge the router:

```rust
mod deployments;
// ...
let api_v1 = Router::new()
    .merge(api::router(handles.clone()))
    .merge(routing::router(handles.clone()))
    .merge(deployments::router(handles.clone()));   // NEW
```

(If the existing structure differs, copy the pattern used for `routing::router(handles)` from Plan C.)

- [ ] **Step 3: Endpoint tests**

Create `crates/isengard-plugins/dashboard/tests/deployments_endpoints.rs`:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use isengard_storage::deployment::{DeployStrategy, DeploymentState, InsertDeployment};
use isengard_storage::host::EnrollHost;
use isengard_storage::inventory::Inventory;
use isengard_storage::stack::{InsertStack, StackSource};
use tower::ServiceExt;
use ulid::Ulid;

// Test scaffolding: a helper that builds a minimal ControllerHandles wrapper
// + the dashboard's deployments router, and returns the axum app + an inventory
// reference for direct setup. Steal the pattern from existing dashboard tests
// (e.g., routing endpoints from Plan C: `crates/isengard-plugins/dashboard/tests/routing_*.rs`).
async fn setup_app() -> (axum::Router, Inventory, isengard_storage::host::HostId, isengard_storage::stack::StackId) {
    let inv = Inventory::open_in_memory().await.unwrap();
    let host_id = inv.enroll_host(EnrollHost {
        hostname: "h".into(), fingerprint: "fp".into(), fleet: "default".into(),
        os: "linux".into(), arch: "x86_64".into(),
        agent_version: "0.1.0".into(), docker_version: "24.0".into(),
    }).await.unwrap();
    let stack_id = inv.insert_stack(InsertStack {
        host_id, name: "blog".into(), source: StackSource::Compose,
    }).await.unwrap();
    // Build ControllerHandles with the in-memory inventory. Existing routing
    // tests do this — copy the pattern. If needed, expose a constructor like
    // `ControllerHandles::for_test(inv: Inventory)`.
    let handles = std::sync::Arc::new(
        isengard_controller::ControllerHandles::for_test(inv.clone())
    );
    let app = isengard_plugin_dashboard::deployments::router(handles);
    (app, inv, host_id, stack_id)
}

fn sample_dep(host_id: isengard_storage::host::HostId, stack_id: isengard_storage::stack::StackId, state: DeploymentState) -> InsertDeployment {
    InsertDeployment {
        id: Ulid::new().to_string(),
        host_id, stack_id,
        service_name: "web".into(),
        strategy: DeployStrategy::BlueGreen,
        state,
        blue_container: Some("c-blue".into()),
        green_container: None,
        blue_digest: "sha256:aaa".into(),
        green_digest: "sha256:bbb".into(),
        public_hostname: Some("blog.test".into()),
        health_path: Some("/".into()),
        container_port: Some(80),
        metadata_json: None,
    }
}

#[tokio::test]
async fn list_active_returns_only_non_terminal() {
    let (app, inv, host, stack) = setup_app().await;
    // Active row.
    inv.insert_deployment(sample_dep(host, stack, DeploymentState::SpinningUp)).await.unwrap();
    // Terminal row.
    let done = inv.insert_deployment(sample_dep(host, stack, DeploymentState::Pending)).await.unwrap();
    inv.update_deployment_state(&done.id, DeploymentState::Done).await.unwrap();

    let req = Request::builder()
        .uri(format!("/deployments?stack_id={}&state=active", stack.0))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
    let dtos: Vec<isengard_plugin_dashboard::deployments::DeploymentDto> =
        serde_json::from_slice(&body).unwrap();
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].state, "spinning_up");
}

#[tokio::test]
async fn list_history_returns_only_terminal() {
    let (app, inv, host, stack) = setup_app().await;
    inv.insert_deployment(sample_dep(host, stack, DeploymentState::SpinningUp)).await.unwrap();
    let done = inv.insert_deployment(sample_dep(host, stack, DeploymentState::Pending)).await.unwrap();
    inv.update_deployment_state(&done.id, DeploymentState::Done).await.unwrap();

    let req = Request::builder()
        .uri(format!("/deployments?stack_id={}&state=history", stack.0))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
    let dtos: Vec<isengard_plugin_dashboard::deployments::DeploymentDto> =
        serde_json::from_slice(&body).unwrap();
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].state, "done");
}
```

`ControllerHandles::for_test(inv)` may need to be added if it doesn't exist — check existing dashboard tests in `crates/isengard-plugins/dashboard/tests/` for the pattern. If those tests pass `ControllerHandles { inventory: inv, ... }` directly, follow that.

- [ ] **Step 4: Run tests**

Run: `cargo test -p isengard-plugin-dashboard --test deployments_endpoints`
Expected: 2/2 pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-plugins/dashboard/src/deployments.rs \
        crates/isengard-plugins/dashboard/src/lib.rs \
        crates/isengard-plugins/dashboard/tests/deployments_endpoints.rs
git commit -m "feat(dashboard): GET /api/v1/deployments?stack_id=&state= endpoint"
```

---

## Task 6: useDeployments composable + DeploymentInProgressPanel + Stack detail wire-up

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/composables/useDeployments.ts`
- Create: `crates/isengard-plugins/dashboard/web/components/DeploymentInProgressPanel.vue`
- Modify: `crates/isengard-plugins/dashboard/web/pages/stacks/[id].vue`

- [ ] **Step 1: Composable**

Create `crates/isengard-plugins/dashboard/web/composables/useDeployments.ts`:

```ts
import { ref, onMounted, onUnmounted } from 'vue'

export interface DeploymentDto {
  id: string
  host_id: string
  stack_id: number
  service_name: string
  strategy: 'blue-green' | 'in-place'
  state: 'pending' | 'spinning_up' | 'switching' | 'draining' | 'destroying_blue' | 'recovering' | 'done' | 'aborted' | 'failed'
  blue_container: string | null
  green_container: string | null
  blue_digest: string
  green_digest: string
  public_hostname: string | null
  healthcheck_passed_at: string | null
  switched_at: string | null
  drained_at: string | null
  finished_at: string | null
  error: string | null
  created_at: string
  updated_at: string
}

export function useDeployments(stackId: number) {
  const { get } = useApi()
  const active = ref<DeploymentDto[]>([])
  const history = ref<DeploymentDto[]>([])
  const loading = ref(false)
  let ws: WebSocket | null = null

  async function refresh() {
    loading.value = true
    try {
      active.value = await get<DeploymentDto[]>(
        `/deployments?stack_id=${stackId}&state=active`,
      )
      history.value = await get<DeploymentDto[]>(
        `/deployments?stack_id=${stackId}&state=history&limit=10`,
      )
    } finally {
      loading.value = false
    }
  }

  function connectWs() {
    if (ws) return
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
    ws = new WebSocket(`${proto}//${location.host}/ws/events`)
    ws.onmessage = (msg) => {
      try {
        const event = JSON.parse(msg.data)
        if (typeof event.kind === 'string' && event.kind.startsWith('deployment.')) {
          // Re-fetch on any deployment.* event for this stack.
          const meta = event.metadata?.deployment
          if (meta && meta.stack_id === stackId) {
            refresh()
          }
        }
      } catch {
        // ignore malformed
      }
    }
  }

  function disconnectWs() {
    if (ws) { ws.close(); ws = null }
  }

  onMounted(() => { refresh(); connectWs() })
  onUnmounted(disconnectWs)

  return { active, history, loading, refresh }
}
```

(Import path `~/composables/useApi` matches Plan C's pattern. If `useApi` doesn't expose a generic `get<T>`, use `request<T>(path)` or whatever Plan C added.)

- [ ] **Step 2: DeploymentInProgressPanel.vue**

Create `crates/isengard-plugins/dashboard/web/components/DeploymentInProgressPanel.vue`:

```vue
<template>
  <div class="border border-iso-border rounded-lg p-4 bg-iso-bg-elevated">
    <div class="flex items-center justify-between mb-3">
      <div>
        <span class="text-iso-info">⚡</span>
        <span class="text-sm font-semibold ml-2">Deployment in progress</span>
        <span class="ml-3 text-[10px] uppercase px-2 py-0.5 rounded bg-iso-info/20 text-iso-info">
          {{ deployment.state }}
        </span>
      </div>
      <div class="text-xs text-iso-text-muted">
        {{ deployment.service_name }} · {{ shortHash(deployment.blue_digest) }} → {{ shortHash(deployment.green_digest) }}
      </div>
    </div>

    <ol class="text-xs space-y-1 mb-3">
      <li :class="stepClass('pending')">▸ pending</li>
      <li :class="stepClass('spinning_up')">▸ spinning up green</li>
      <li :class="stepClass('switching')">▸ switching traffic</li>
      <li :class="stepClass('draining')">▸ draining blue</li>
      <li :class="stepClass('recovering')" v-if="deployment.state === 'recovering'">⚠ recovering (rolling back)</li>
      <li :class="stepClass('destroying_blue')">▸ destroying blue</li>
    </ol>

    <div class="flex items-center justify-end">
      <button
        class="px-3 py-1 text-xs font-medium border border-iso-error text-iso-error rounded hover:bg-iso-error/10"
        :disabled="aborting || isTerminal"
        @click="onAbort"
      >
        {{ aborting ? 'Aborting…' : 'Abort and rollback' }}
      </button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, ref } from 'vue'
import type { DeploymentDto } from '~/composables/useDeployments'

const props = defineProps<{ deployment: DeploymentDto }>()
const aborting = ref(false)
const { post } = useApi()

const STATE_ORDER: Array<DeploymentDto['state']> = [
  'pending', 'spinning_up', 'switching', 'draining', 'destroying_blue',
]

const isTerminal = computed(() =>
  ['done', 'aborted', 'failed'].includes(props.deployment.state),
)

function stepIndex(s: DeploymentDto['state']): number {
  const i = STATE_ORDER.indexOf(s)
  return i === -1 ? STATE_ORDER.length : i
}

function stepClass(s: DeploymentDto['state']): string {
  const cur = stepIndex(props.deployment.state)
  const idx = stepIndex(s)
  if (idx < cur) return 'text-iso-success'
  if (idx === cur) return 'text-iso-text-primary font-medium'
  return 'text-iso-text-faint'
}

function shortHash(d: string): string {
  return d.length > 14 ? d.slice(0, 14) + '…' : d
}

async function onAbort() {
  aborting.value = true
  try {
    // Endpoint lands in Task 10.
    await post(`/deployments/${props.deployment.id}/abort`, {})
  } catch (e) {
    aborting.value = false
    alert(`Abort failed: ${e}`)
  }
}
</script>
```

- [ ] **Step 3: Wire into Stack detail page**

In `crates/isengard-plugins/dashboard/web/pages/stacks/[id].vue`, add:

```vue
<script setup lang="ts">
// existing imports
import { useDeployments } from '~/composables/useDeployments'
import DeploymentInProgressPanel from '~/components/DeploymentInProgressPanel.vue'

// existing setup
const route = useRoute()
const stackIdParam = computed(() => Number(route.params.id))

const { active: activeDeployments } = useDeployments(stackIdParam.value)
const visibleDeployment = computed(() => {
  if (activeDeployments.value.length === 0) return null
  // Most recently created.
  return [...activeDeployments.value].sort((a, b) =>
    b.created_at.localeCompare(a.created_at),
  )[0]
})
</script>

<template>
  <!-- in the existing stack detail layout, ABOVE the services grid: -->
  <DeploymentInProgressPanel
    v-if="visibleDeployment"
    :deployment="visibleDeployment"
    class="mb-4"
  />
  <!-- existing services grid -->
</template>
```

If the route param is named differently or the stack id is fetched async, adapt the wiring. Confirm by reading the existing `pages/stacks/[id].vue` first.

- [ ] **Step 4: Build verify**

Run: `cd crates/isengard-plugins/dashboard/web && bun run build`
Expected: success.

Run: `cd /Users/dirdmaster/Projects/isengard/.worktrees/blue-green-ui && cargo build -p isengard-plugin-dashboard`
Expected: success.

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-plugins/dashboard/web/composables/useDeployments.ts \
        crates/isengard-plugins/dashboard/web/components/DeploymentInProgressPanel.vue \
        crates/isengard-plugins/dashboard/web/pages/stacks/\[id\].vue
git commit -m "feat(dashboard): useDeployments composable + DeploymentInProgressPanel + Stack detail mount"
```

---

## Task 7: proxy::events broadcast + extend health_changed metadata

**Files:**
- Create: `crates/isengard-agent/src/proxy/events.rs`
- Modify: `crates/isengard-agent/src/proxy/healthcheck.rs`
- Modify: `crates/isengard-agent/src/proxy/mod.rs`

- [ ] **Step 1: Define ProxyEvent + broadcast**

Create `crates/isengard-agent/src/proxy/events.rs`:

```rust
//! In-process broadcast of proxy lifecycle events.
//!
//! Plan A's healthcheck eviction code emits `routing.upstream.health_changed`
//! Events to the agent's `EventEmitter` (which forwards to the controller).
//! Plan B (10f) needs the SAME signal IN-PROCESS for the deployment Driver to
//! react to post-switch collapse without round-tripping through the controller.
//!
//! `ProxyEventBus` is a thin tokio broadcast wrapper. Receivers that lag are
//! dropped (broadcast semantics) — fine for our use case (Driver subscribes for
//! a few minutes during draining; lag is impossible at this volume).

use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum ProxyEvent {
    UpstreamHealthChanged {
        public_hostname: String,
        container_id: String,
        healthy: bool,
    },
}

#[derive(Debug, Clone)]
pub struct ProxyEventBus {
    tx: broadcast::Sender<ProxyEvent>,
}

impl ProxyEventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ProxyEvent> {
        self.tx.subscribe()
    }

    pub fn publish(&self, event: ProxyEvent) {
        // OK if no subscribers — broadcast just no-ops.
        let _ = self.tx.send(event);
    }
}

impl Default for ProxyEventBus {
    fn default() -> Self { Self::new() }
}
```

- [ ] **Step 2: Re-export from proxy/mod.rs**

Add to `crates/isengard-agent/src/proxy/mod.rs`:

```rust
pub mod events;
pub use events::{ProxyEvent, ProxyEventBus};
```

Add a `proxy_events: ProxyEventBus` field to `ProxyState`:

```rust
#[derive(Debug, Clone)]
pub struct ProxyState {
    pub upstreams: Arc<RwLock<UpstreamRegistry>>,
    pub proxy_events: ProxyEventBus,
}

impl ProxyState {
    pub fn new() -> Self {
        Self {
            upstreams: Arc::new(RwLock::new(UpstreamRegistry::new())),
            proxy_events: ProxyEventBus::new(),
        }
    }
}
```

(If `ProxyState` has additional fields beyond `upstreams`, preserve them.)

- [ ] **Step 3: Extend healthcheck emission**

In `crates/isengard-agent/src/proxy/healthcheck.rs`, find the emit block (around line 144-156):

```rust
// existing:
st.emit(isengard_core::Event {
    kind: "routing.upstream.health_changed".into(),
    occurred_at: chrono::Utc::now(),
    summary,
    container_name: Some(host.clone()),
    ..Default::default()
}).await;
```

Extend to (a) add structured metadata and (b) publish to the proxy event bus:

```rust
// Publish in-process.
st.proxy_state.proxy_events.publish(crate::proxy::ProxyEvent::UpstreamHealthChanged {
    public_hostname: host.clone(),
    container_id: container_id.clone(),
    healthy: now_healthy,
});

// Emit to controller (existing path, with structured metadata added).
st.emit(isengard_core::Event {
    kind: "routing.upstream.health_changed".into(),
    occurred_at: chrono::Utc::now(),
    summary,
    container_name: Some(host.clone()),
    metadata: serde_json::json!({
        "public_hostname": host,
        "container_id": container_id,
        "healthy": now_healthy,
    }),
    ..Default::default()
}).await;
```

The `st.proxy_state` field is whatever the healthcheck task uses to access upstream state — adapt to actual field name. If healthcheck doesn't currently have access to `ProxyState`, thread it through (the spawned task setup already takes the upstream registry; you'll add the broadcast too).

- [ ] **Step 4: Unit test for broadcast**

Append to `crates/isengard-agent/src/proxy/events.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_then_subscribe_misses() {
        // Broadcast semantics: subscribers only see events published AFTER
        // they subscribe. Document this with a test.
        let bus = ProxyEventBus::new();
        bus.publish(ProxyEvent::UpstreamHealthChanged {
            public_hostname: "blog.test".into(),
            container_id: "c1".into(),
            healthy: false,
        });

        let mut rx = bus.subscribe();
        // Publish another after subscribing.
        bus.publish(ProxyEvent::UpstreamHealthChanged {
            public_hostname: "blog.test".into(),
            container_id: "c1".into(),
            healthy: true,
        });

        let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await.expect("recv timed out").expect("recv ok");
        match received {
            ProxyEvent::UpstreamHealthChanged { healthy, .. } => assert!(healthy),
        }
    }

    #[tokio::test]
    async fn subscribe_then_publish_receives() {
        let bus = ProxyEventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(ProxyEvent::UpstreamHealthChanged {
            public_hostname: "blog.test".into(),
            container_id: "c1".into(),
            healthy: false,
        });
        let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await.expect("timeout").expect("recv");
        match received {
            ProxyEvent::UpstreamHealthChanged { healthy, .. } => assert!(!healthy),
        }
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p isengard-agent --lib proxy::events::tests`
Expected: 2/2 pass.

Run: `cargo test -p isengard-agent`
Expected: workspace green (existing tests intact).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/proxy/events.rs \
        crates/isengard-agent/src/proxy/mod.rs \
        crates/isengard-agent/src/proxy/healthcheck.rs
git commit -m "feat(agent): ProxyEventBus + extend health_changed metadata + publish in-process"
```

---

## Task 8: Driver Recovering state + abort_token + tokio::select! + recover_to_blue

**Files:**
- Modify: `crates/isengard-storage/src/deployment.rs` (add `Recovering` variant)
- Modify: `crates/isengard-storage/migrations/0011_deployments.sql` — NO, can't modify a past migration. Add a NEW migration to relax the CHECK constraint instead (Step 1).
- Modify: `crates/isengard-agent/src/deployment/driver.rs`

- [ ] **Step 1: Migration to extend state CHECK constraint**

Create `crates/isengard-storage/migrations/0013_deployment_state_recovering.sql`:

```sql
-- Phase 10f: add `recovering` to the deployments.state CHECK constraint.
-- SQLite doesn't support ALTER ... CHECK, so we recreate the table.
-- The data preserves: copy → drop → rename.

CREATE TABLE deployments_new (
    id                       TEXT PRIMARY KEY,
    host_id                  BLOB NOT NULL,
    stack_id                 INTEGER NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
    service_name             TEXT NOT NULL,
    strategy                 TEXT NOT NULL CHECK (strategy IN ('blue-green', 'in-place')),
    state                    TEXT NOT NULL CHECK (state IN (
        'pending', 'spinning_up', 'switching', 'draining',
        'destroying_blue', 'recovering', 'done', 'aborted', 'failed'
    )),
    blue_container           TEXT,
    green_container          TEXT,
    blue_digest              TEXT NOT NULL,
    green_digest             TEXT NOT NULL,
    public_hostname          TEXT,
    health_path              TEXT,
    container_port           INTEGER,
    healthcheck_started_at   TEXT,
    healthcheck_passed_at    TEXT,
    switched_at              TEXT,
    drained_at               TEXT,
    finished_at              TEXT,
    error                    TEXT,
    metadata_json            TEXT,
    created_at               TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at               TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO deployments_new SELECT * FROM deployments;
DROP TABLE deployments;
ALTER TABLE deployments_new RENAME TO deployments;

CREATE INDEX idx_deployments_state_active
    ON deployments(state)
    WHERE state NOT IN ('done', 'failed', 'aborted');

CREATE INDEX idx_deployments_stack_created
    ON deployments(stack_id, created_at DESC);

CREATE INDEX idx_deployments_host_service_active
    ON deployments(host_id, service_name)
    WHERE state NOT IN ('done', 'failed', 'aborted');
```

- [ ] **Step 2: Add Recovering variant to enum**

In `crates/isengard-storage/src/deployment.rs`, add `Recovering` to `DeploymentState`:

```rust
pub enum DeploymentState {
    Pending,
    SpinningUp,
    Switching,
    Draining,
    DestroyingBlue,
    Recovering,        // NEW (Plan B 10f)
    Done,
    Aborted,
    Failed,
}
```

Update `as_str` to include `Self::Recovering => "recovering"` and `FromStr` to parse `"recovering"` → `Self::Recovering`. `is_terminal` is unchanged (Recovering is non-terminal).

Update the existing test for `deployment_state_round_trips_through_str` — it iterates a list; add `DeploymentState::Recovering`.

- [ ] **Step 3: Driver: blue snapshot + abort_token + tokio::select**

In `crates/isengard-agent/src/deployment/driver.rs`, add fields to `Driver<D>`:

```rust
use tokio_util::sync::CancellationToken;
use crate::proxy::ProxyEvent;
use tokio::sync::broadcast;

pub struct Driver<D: DriverDeps> {
    pub deployment: Deployment,
    pub deps: Arc<D>,
    pub inventory: Inventory,
    pub emitter: Arc<dyn EventEmitter>,
    pub grace_period: Duration,
    pub drain_buffer: Duration,
    pub hc_interval: Duration,
    pub hc_success_threshold: u32,
    pub hc_deadline: Duration,
    pub abort_token: CancellationToken,                  // NEW
    pub proxy_events_rx: Option<broadcast::Receiver<ProxyEvent>>,  // NEW; Option for unit tests
    pub blue_upstream_snapshot: Option<crate::proxy::upstreams::Upstream>,  // NEW; populated before swap_upstream
}
```

Update `Driver::new` to:
- Accept abort_token (or default to `CancellationToken::new()`)
- Accept `proxy_events_rx: Option<broadcast::Receiver<ProxyEvent>>` (or default to None)
- Initialize `blue_upstream_snapshot: None`

```rust
impl<D: DriverDeps> Driver<D> {
    pub fn new(
        deployment: Deployment,
        deps: Arc<D>,
        inventory: Inventory,
        emitter: Arc<dyn EventEmitter>,
    ) -> Self {
        Self {
            deployment, deps, inventory, emitter,
            grace_period: Duration::from_secs(60),
            drain_buffer: Duration::from_secs(5),
            hc_interval: Duration::from_secs(5),
            hc_success_threshold: 2,
            hc_deadline: Duration::from_secs(120),
            abort_token: CancellationToken::new(),
            proxy_events_rx: None,
            blue_upstream_snapshot: None,
        }
    }

    pub fn with_abort_token(mut self, token: CancellationToken) -> Self {
        self.abort_token = token;
        self
    }

    pub fn with_proxy_events(mut self, rx: broadcast::Receiver<ProxyEvent>) -> Self {
        self.proxy_events_rx = Some(rx);
        self
    }
}
```

Modify `run_inner` to:
1. Wrap top-level with abort check
2. Snapshot blue's Upstream from registry before calling swap_upstream
3. Replace the `tokio::time::sleep(grace_period + buffer)` in the draining phase with a 3-way `tokio::select!`

Code (full updated `run_inner`):

```rust
async fn run_inner(&mut self) -> Result<()> {
    // Top-level abort check.
    if self.abort_token.is_cancelled() {
        self.set_error("aborted_by_user").await.ok();
        self.transition_to(DeploymentState::Aborted).await?;
        self.emit("deployment.aborted", Some("aborted_by_user".into()));
        return Ok(());
    }

    // pending → spinning_up
    self.transition_to(DeploymentState::SpinningUp).await?;
    self.emit("deployment.spinning_up", None);

    // Race spinup against abort.
    let (green_id, green_addr) = tokio::select! {
        _ = self.abort_token.cancelled() => {
            self.set_error("aborted_by_user").await.ok();
            self.transition_to(DeploymentState::Aborted).await?;
            self.emit("deployment.aborted", Some("aborted_by_user".into()));
            return Ok(());
        }
        result = self.deps.start_green(&self.deployment) => {
            match result {
                Ok(v) => v,
                Err(e) => {
                    let msg = format!("spinup_failed: {e:#}");
                    self.set_error(&msg).await.ok();
                    self.transition_to(DeploymentState::Aborted).await?;
                    self.emit("deployment.aborted", Some(msg));
                    return Ok(());
                }
            }
        }
    };

    self.deployment.green_container = Some(green_id.clone());
    self.inventory.set_deployment_green_container(&self.deployment.id, &green_id).await?;

    // Healthcheck loop, racing abort.
    let hc = DeploymentHealthcheck::new(self.deps.build_health_checker(&self.deployment))
        .with_interval(self.hc_interval)
        .with_success_threshold(self.hc_success_threshold)
        .with_deadline(self.hc_deadline);

    let hc_result = tokio::select! {
        _ = self.abort_token.cancelled() => {
            // Abort during healthcheck: cleanup green, mark Aborted.
            self.set_error("aborted_by_user").await.ok();
            self.deps.stop_and_remove(&green_id).await.ok();
            self.transition_to(DeploymentState::Aborted).await?;
            self.emit("deployment.aborted", Some("aborted_by_user".into()));
            return Ok(());
        }
        r = hc.wait_for_healthy(green_addr) => r,
    };
    match hc_result {
        Ok(passed_at) => {
            self.deployment.healthcheck_passed_at = Some(passed_at);
            self.inventory.set_deployment_healthcheck_passed(&self.deployment.id, passed_at).await?;
        }
        Err(timeout) => {
            let attempts_str = format_attempts(&timeout);
            let msg = format!("healthcheck_timeout: {attempts_str}");
            self.set_error(&msg).await.ok();
            self.deps.stop_and_remove(&green_id).await.ok();
            self.transition_to(DeploymentState::Aborted).await?;
            self.emit("deployment.aborted", Some(msg));
            return Ok(());
        }
    }

    // Snapshot blue's current upstream before we swap.
    self.blue_upstream_snapshot = self.read_blue_upstream_snapshot().await;

    // switching → draining
    self.transition_to(DeploymentState::Switching).await?;
    self.emit("deployment.switched", None);

    if let Err(e) = self.deps.swap_upstream_to_green(
        &self.deployment, green_addr, &green_id, self.grace_period,
    ).await {
        let msg = format!("swap_failed: {e:#}");
        self.set_error(&msg).await.ok();
        self.deps.stop_and_remove(&green_id).await.ok();
        self.transition_to(DeploymentState::Aborted).await?;
        self.emit("deployment.aborted", Some(msg));
        return Ok(());
    }
    let switched_at = Utc::now();
    self.inventory.set_deployment_switched(&self.deployment.id, switched_at).await?;

    self.transition_to(DeploymentState::Draining).await?;

    // 3-way race during the drain window.
    let drain_outcome = self.race_drain(&green_id).await;
    match drain_outcome {
        DrainOutcome::Completed => {
            // Happy path: proceed to destroying_blue.
            let drained_at = Utc::now();
            self.inventory.set_deployment_drained(&self.deployment.id, drained_at).await?;
            self.transition_to(DeploymentState::DestroyingBlue).await?;
            if let Some(blue) = self.deployment.blue_container.clone() {
                self.deps.stop_and_remove(&blue).await.ok();
            }
            let finished_at = Utc::now();
            self.inventory.set_deployment_finished(&self.deployment.id, finished_at).await?;
            self.transition_to(DeploymentState::Done).await?;
            self.emit("deployment.completed", None);
        }
        DrainOutcome::Aborted => {
            // User clicked abort. Recover blue + cleanup green.
            self.recover_to_blue("aborted_during_drain").await;
            self.deps.stop_and_remove(&green_id).await.ok();
            self.transition_to(DeploymentState::Aborted).await?;
            self.emit("deployment.aborted", Some("aborted_during_drain".into()));
        }
        DrainOutcome::GreenUnhealthy => {
            // Post-switch collapse. Try to recover.
            self.transition_to(DeploymentState::Recovering).await?;
            self.recover_to_blue("post_switch_collapse_recovered").await;
            self.deps.stop_and_remove(&green_id).await.ok();
            self.transition_to(DeploymentState::Failed).await?;
            self.emit("deployment.failed", Some("post_switch_collapse_recovered".into()));
        }
    }
    Ok(())
}

enum DrainOutcome { Completed, Aborted, GreenUnhealthy }

impl<D: DriverDeps> Driver<D> {
    async fn race_drain(&mut self, green_id: &str) -> DrainOutcome {
        let total_drain = self.grace_period + self.drain_buffer;
        let abort_token = self.abort_token.clone();
        let hostname = self.deployment.public_hostname.clone();

        // If we don't have a proxy_events_rx, skip the unhealthy listener.
        let mut rx = self.proxy_events_rx.take();

        loop {
            tokio::select! {
                biased;
                _ = abort_token.cancelled() => return DrainOutcome::Aborted,
                event = async {
                    if let Some(rx) = rx.as_mut() {
                        rx.recv().await.ok()
                    } else {
                        std::future::pending::<()>().await;
                        None
                    }
                }, if rx.is_some() => {
                    if let Some(ProxyEvent::UpstreamHealthChanged { public_hostname, healthy, container_id, .. }) = event {
                        // Only react if it's OUR hostname AND it's the GREEN container that went unhealthy.
                        if Some(public_hostname) == hostname && !healthy && Some(container_id) == self.deployment.green_container {
                            return DrainOutcome::GreenUnhealthy;
                        }
                        // Otherwise keep listening.
                        continue;
                    }
                    // Channel closed: stop listening, fall back to sleep-only.
                    rx = None;
                }
                _ = tokio::time::sleep(total_drain) => return DrainOutcome::Completed,
            }
        }
    }

    async fn read_blue_upstream_snapshot(&self) -> Option<crate::proxy::upstreams::Upstream> {
        // We need the ProxyState to read the registry. Add a method on DriverDeps
        // to expose it OR snapshot from a passed reference. For unit testability,
        // add a new DriverDeps method:
        self.deps.snapshot_current_upstream(&self.deployment).await
    }

    async fn recover_to_blue(&mut self, reason: &str) {
        // If we have a snapshot AND blue's container still exists, swap back.
        let Some(blue_up) = self.blue_upstream_snapshot.clone() else {
            self.set_error(&format!("{reason} (no_blue_snapshot)")).await.ok();
            return;
        };
        let hostname = match self.deployment.public_hostname.as_deref() {
            Some(h) => h.to_string(),
            None => {
                self.set_error(&format!("{reason} (no_public_hostname)")).await.ok();
                return;
            }
        };
        // Instant swap-back (grace=0).
        if let Err(e) = self.deps.swap_back_to_blue(&hostname, &blue_up).await {
            self.set_error(&format!("{reason}_unrecoverable: {e:#}")).await.ok();
            return;
        }
        self.set_error(reason).await.ok();
    }
}
```

Add the new DriverDeps trait methods. Update the trait:

```rust
#[async_trait::async_trait]
pub trait DriverDeps: Send + Sync {
    async fn start_green(&self, d: &Deployment) -> Result<(String, SocketAddr)>;
    async fn stop_and_remove(&self, container_id: &str) -> Result<()>;
    fn build_health_checker(&self, d: &Deployment) -> HealthChecker;
    async fn swap_upstream_to_green(
        &self, d: &Deployment, green_addr: SocketAddr, green_container_id: &str, grace: Duration,
    ) -> Result<()>;

    /// NEW for Plan B: snapshot the upstream currently routing for this deployment's hostname.
    async fn snapshot_current_upstream(&self, d: &Deployment) -> Option<crate::proxy::upstreams::Upstream>;

    /// NEW for Plan B: instant swap back to a previously-snapshotted blue upstream.
    async fn swap_back_to_blue(
        &self, hostname: &str, blue: &crate::proxy::upstreams::Upstream,
    ) -> Result<()>;
}
```

Update `RealDriverDeps`:

```rust
#[async_trait::async_trait]
impl DriverDeps for RealDriverDeps {
    // existing methods unchanged

    async fn snapshot_current_upstream(&self, d: &Deployment) -> Option<crate::proxy::upstreams::Upstream> {
        let host = d.public_hostname.as_deref()?;
        let r = self.proxy_state.upstreams.read().await;
        r.get(host).cloned()
    }

    async fn swap_back_to_blue(
        &self, hostname: &str, blue: &crate::proxy::upstreams::Upstream,
    ) -> Result<()> {
        crate::proxy::swap::swap_upstream(
            &self.proxy_state, hostname, blue.clone(), Duration::from_secs(0),
        ).await?;
        Ok(())
    }
}
```

`UpstreamRegistry::get` may return `&Upstream` (not Option<Upstream>). If `Upstream` is `Clone`, do `.cloned()`; if not, derive `#[derive(Clone)]` on `Upstream` (it's a small struct of strings + numbers).

- [ ] **Step 4: Driver tests for new behavior**

Append to `driver.rs` test module:

```rust
#[tokio::test]
async fn abort_during_pending_marks_aborted_without_starting_green() {
    let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
    let token = CancellationToken::new();
    token.cancel();   // Pre-cancelled.

    struct Deps;
    #[async_trait::async_trait]
    impl DriverDeps for Deps {
        async fn start_green(&self, _: &Deployment) -> Result<(String, SocketAddr)> {
            panic!("start_green should NOT be called when aborted before spinup");
        }
        async fn stop_and_remove(&self, _: &str) -> Result<()> { Ok(()) }
        fn build_health_checker(&self, _: &Deployment) -> HealthChecker { HealthChecker::tcp_only(Duration::from_millis(10)) }
        async fn swap_upstream_to_green(&self, _: &Deployment, _: SocketAddr, _: &str, _: Duration) -> Result<()> { Ok(()) }
        async fn snapshot_current_upstream(&self, _: &Deployment) -> Option<crate::proxy::upstreams::Upstream> { None }
        async fn swap_back_to_blue(&self, _: &str, _: &crate::proxy::upstreams::Upstream) -> Result<()> { Ok(()) }
    }

    let driver = Driver::new(d.clone(), Arc::new(Deps), inv.clone(), Arc::new(NoopEmitter))
        .with_abort_token(token);
    driver.run().await;

    let final_d = inv.get_deployment(&d.id).await.unwrap().unwrap();
    assert_eq!(final_d.state, DeploymentState::Aborted);
    assert!(final_d.error.unwrap_or_default().contains("aborted_by_user"));
}

#[tokio::test]
async fn abort_during_spinup_cleans_green_and_marks_aborted() {
    let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
    let token = CancellationToken::new();
    let token2 = token.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);    // Won't matter — start_green simulates a slow operation.

    struct Deps {
        token: CancellationToken,
        addr: SocketAddr,
        cleanup_called: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait::async_trait]
    impl DriverDeps for Deps {
        async fn start_green(&self, _: &Deployment) -> Result<(String, SocketAddr)> {
            // Simulate a long-running spinup.
            tokio::select! {
                _ = self.token.cancelled() => {
                    // Driver's tokio::select around start_green should have raced
                    // and won — we get here only if start_green doesn't actually
                    // get cancelled, but the driver picks up the cancel on its
                    // own. Return a value the driver will then immediately
                    // discard via post-spinup abort detection.
                    Ok(("g".into(), self.addr))
                }
                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                    Ok(("g".into(), self.addr))
                }
            }
        }
        async fn stop_and_remove(&self, _: &str) -> Result<()> {
            self.cleanup_called.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn build_health_checker(&self, _: &Deployment) -> HealthChecker { HealthChecker::tcp_only(Duration::from_millis(10)) }
        async fn swap_upstream_to_green(&self, _: &Deployment, _: SocketAddr, _: &str, _: Duration) -> Result<()> { Ok(()) }
        async fn snapshot_current_upstream(&self, _: &Deployment) -> Option<crate::proxy::upstreams::Upstream> { None }
        async fn swap_back_to_blue(&self, _: &str, _: &crate::proxy::upstreams::Upstream) -> Result<()> { Ok(()) }
    }

    let cleanup = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let deps = Arc::new(Deps { token: token2.clone(), addr, cleanup_called: cleanup.clone() });
    let driver = Driver::new(d.clone(), deps, inv.clone(), Arc::new(NoopEmitter))
        .with_abort_token(token);

    let driver_task = tokio::spawn(driver.run());
    tokio::time::sleep(Duration::from_millis(50)).await;
    token2.cancel();
    driver_task.await.unwrap();

    let final_d = inv.get_deployment(&d.id).await.unwrap().unwrap();
    assert_eq!(final_d.state, DeploymentState::Aborted);
}

#[tokio::test]
async fn abort_during_drain_swaps_back_and_marks_aborted() {
    let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
    let token = CancellationToken::new();
    let token2 = token.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { loop { let _ = listener.accept().await; } });

    struct Deps {
        addr: SocketAddr,
        swap_back_called: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait::async_trait]
    impl DriverDeps for Deps {
        async fn start_green(&self, _: &Deployment) -> Result<(String, SocketAddr)> {
            Ok(("green".into(), self.addr))
        }
        async fn stop_and_remove(&self, _: &str) -> Result<()> { Ok(()) }
        fn build_health_checker(&self, _: &Deployment) -> HealthChecker { HealthChecker::tcp_only(Duration::from_millis(50)) }
        async fn swap_upstream_to_green(&self, _: &Deployment, _: SocketAddr, _: &str, _: Duration) -> Result<()> { Ok(()) }
        async fn snapshot_current_upstream(&self, _: &Deployment) -> Option<crate::proxy::upstreams::Upstream> {
            // Return a synthetic blue upstream so recover_to_blue can use it.
            Some(crate::proxy::upstreams::Upstream {
                container_id: "blue".into(),
                addr: "127.0.0.1:9999".parse().unwrap(),
                healthy: true,
                health_path: None,
                health_interval: Duration::from_secs(5),
                consecutive_failures: 0,
                state: crate::proxy::upstreams::UpstreamState::Active,
            })
        }
        async fn swap_back_to_blue(&self, _: &str, _: &crate::proxy::upstreams::Upstream) -> Result<()> {
            self.swap_back_called.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    let swap_back = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let deps = Arc::new(Deps { addr, swap_back_called: swap_back.clone() });
    let mut driver = Driver::new(d.clone(), deps, inv.clone(), Arc::new(NoopEmitter))
        .with_abort_token(token);
    driver.grace_period = Duration::from_secs(60);
    driver.drain_buffer = Duration::from_secs(5);

    let driver_task = tokio::spawn(driver.run());
    // Wait for the driver to enter draining.
    tokio::time::sleep(Duration::from_millis(300)).await;
    token2.cancel();
    driver_task.await.unwrap();

    assert!(swap_back.load(std::sync::atomic::Ordering::SeqCst));
    let final_d = inv.get_deployment(&d.id).await.unwrap().unwrap();
    assert_eq!(final_d.state, DeploymentState::Aborted);
    assert!(final_d.error.unwrap_or_default().contains("aborted_during_drain"));
}

#[tokio::test]
async fn green_unhealthy_during_drain_recovers_and_marks_failed() {
    let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { loop { let _ = listener.accept().await; } });

    struct Deps {
        addr: SocketAddr,
        swap_back_called: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait::async_trait]
    impl DriverDeps for Deps {
        async fn start_green(&self, _: &Deployment) -> Result<(String, SocketAddr)> {
            Ok(("green".into(), self.addr))
        }
        async fn stop_and_remove(&self, _: &str) -> Result<()> { Ok(()) }
        fn build_health_checker(&self, _: &Deployment) -> HealthChecker { HealthChecker::tcp_only(Duration::from_millis(50)) }
        async fn swap_upstream_to_green(&self, _: &Deployment, _: SocketAddr, _: &str, _: Duration) -> Result<()> { Ok(()) }
        async fn snapshot_current_upstream(&self, _: &Deployment) -> Option<crate::proxy::upstreams::Upstream> {
            Some(crate::proxy::upstreams::Upstream {
                container_id: "blue".into(),
                addr: "127.0.0.1:9999".parse().unwrap(),
                healthy: true,
                health_path: None,
                health_interval: Duration::from_secs(5),
                consecutive_failures: 0,
                state: crate::proxy::upstreams::UpstreamState::Active,
            })
        }
        async fn swap_back_to_blue(&self, _: &str, _: &crate::proxy::upstreams::Upstream) -> Result<()> {
            self.swap_back_called.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    let bus = crate::proxy::ProxyEventBus::new();
    let rx = bus.subscribe();

    let swap_back = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let deps = Arc::new(Deps { addr, swap_back_called: swap_back.clone() });
    let mut driver = Driver::new(d.clone(), deps, inv.clone(), Arc::new(NoopEmitter))
        .with_proxy_events(rx);
    driver.grace_period = Duration::from_secs(60);
    driver.drain_buffer = Duration::from_secs(5);

    let driver_task = tokio::spawn(driver.run());
    // Wait for entry into draining.
    tokio::time::sleep(Duration::from_millis(300)).await;
    bus.publish(crate::proxy::ProxyEvent::UpstreamHealthChanged {
        public_hostname: "blog.test".into(),       // matches d.public_hostname
        container_id: "green".into(),               // matches our green
        healthy: false,
    });
    driver_task.await.unwrap();

    assert!(swap_back.load(std::sync::atomic::Ordering::SeqCst));
    let final_d = inv.get_deployment(&d.id).await.unwrap().unwrap();
    assert_eq!(final_d.state, DeploymentState::Failed);
    assert!(final_d.error.unwrap_or_default().contains("post_switch_collapse_recovered"));
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p isengard-agent --lib deployment::driver::tests`
Expected: all pass (existing 4 + 4 new = 8). The existing tests should still pass — `Driver::new` sets defaults, abort_token defaults to non-cancelled, proxy_events_rx defaults to None.

Add `tokio_util = { workspace = true, features = ["rt"] }` to `crates/isengard-agent/Cargo.toml` if not already present (CancellationToken comes from `tokio_util::sync`).

Run: `cargo test -p isengard-agent`
Expected: workspace agent tests green.

Run: `cargo test -p isengard-storage`
Expected: storage tests green (the migration-0013 doesn't break anything).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-storage/migrations/0013_deployment_state_recovering.sql \
        crates/isengard-storage/src/deployment.rs \
        crates/isengard-agent/src/deployment/driver.rs \
        crates/isengard-agent/Cargo.toml
git commit -m "feat(agent): driver Recovering state + abort token + tokio::select drain race + recover_to_blue"
```

---

## Task 9: Proto AbortDeployment + RoutingPusher::send_message_to_host + agent dispatch

**Files:**
- Modify: `crates/isengard-proto/proto/isengard.v1.proto`
- Modify: `crates/isengard-controller/src/routing.rs`
- Modify: `crates/isengard-agent/src/sync.rs` (or wherever ControllerMessage payload is matched)
- Modify: `crates/isengard-agent/src/deployment/mod.rs`

- [ ] **Step 1: Proto change**

In `crates/isengard-proto/proto/isengard.v1.proto`, find `ControllerMessage`:

```proto
message ControllerMessage {
  oneof payload {
    HeartbeatAck heartbeat_ack = 1;
    ProxyConfig proxy_config = 2;
    // ... existing fields, use existing numbers ...
    AbortDeployment abort_deployment = <next available>;
  }
}

message AbortDeployment {
  string deployment_id = 1;
}
```

Run `cargo build -p isengard-proto` to regenerate. The `pb` module in dependents will pick up the new variant automatically.

- [ ] **Step 2: Extend RoutingPusher with send_message_to_host**

In `crates/isengard-controller/src/routing.rs`, add to `impl RoutingPusher`:

```rust
/// Send an arbitrary `ControllerMessage` to a specific host. Returns Err if
/// the host isn't currently connected. Used by the dashboard's abort endpoint
/// (Plan B 10f) to deliver `AbortDeployment` immediately.
pub async fn send_message_to_host(
    &self,
    host_id: HostId,
    message: ControllerMessage,
) -> Result<()> {
    let senders = self.senders.lock().await;
    let sender = senders.by_host.get(&host_id)
        .ok_or_else(|| anyhow::anyhow!("host {host_id} not connected"))?;
    sender.try_send(Ok(message))
        .map_err(|e| anyhow::anyhow!("send_to_host {host_id} failed: {e}"))?;
    Ok(())
}
```

(Adjust `HostId`'s Display impl as needed; if it doesn't impl Display, use `format!("{:?}", host_id)`.)

- [ ] **Step 3: Agent dispatch**

In `crates/isengard-agent/src/sync.rs` (or wherever the agent reads ControllerMessage in its Sync stream loop), find the match on `controller_message::Payload` and add an arm:

```rust
use isengard_proto::pb::controller_message::Payload;
// ... existing matches ...
Some(Payload::AbortDeployment(abort)) => {
    if let Some(ref supervisor) = ctx.deployment_supervisor {
        let cancelled = supervisor.handle_abort(&abort.deployment_id).await;
        if !cancelled {
            tracing::warn!(deployment_id = %abort.deployment_id, "AbortDeployment received for unknown id");
        }
    } else {
        tracing::warn!("AbortDeployment received but supervisor not wired");
    }
}
```

`ctx.deployment_supervisor` may need to be added to whatever context struct the sync handler has. Plan A's main wiring already constructed the supervisor — thread it into the sync receiver too.

If the sync receiver doesn't currently have access to the Supervisor, add it via the existing PluginContext or via an Arc wired through the agent's task assembly (mirror how the agent wires `proxy_state` into multiple consumers).

- [ ] **Step 4: Supervisor handle_abort**

In `crates/isengard-agent/src/deployment/mod.rs`, extend `DeploymentSupervisor`:

```rust
use std::collections::HashMap;
use std::sync::RwLock as StdRwLock;
use tokio_util::sync::CancellationToken;

pub struct DeploymentSupervisor {
    inventory: Inventory,
    docker: Arc<bollard::Docker>,
    proxy_state: ProxyState,
    emitter: Arc<dyn EventEmitter>,
    abort_tokens: Arc<StdRwLock<HashMap<String, CancellationToken>>>,
}

impl DeploymentSupervisor {
    pub fn new(
        inventory: Inventory,
        docker: Arc<bollard::Docker>,
        proxy_state: ProxyState,
        emitter: Arc<dyn EventEmitter>,
    ) -> Self {
        Self {
            inventory, docker, proxy_state, emitter,
            abort_tokens: Arc::new(StdRwLock::new(HashMap::new())),
        }
    }

    /// Cancel the in-flight Driver for `deployment_id`. Returns true if
    /// a token was found + cancelled, false if the deployment isn't in flight.
    pub async fn handle_abort(&self, deployment_id: &str) -> bool {
        let tokens = self.abort_tokens.read().unwrap();
        if let Some(token) = tokens.get(deployment_id) {
            token.cancel();
            true
        } else {
            false
        }
    }
}
```

In `handle_update_trigger`, when spawning the Driver for the BlueGreen branch, also register the token:

```rust
let abort_token = CancellationToken::new();
{
    let mut tokens = self.abort_tokens.write().unwrap();
    tokens.insert(id.clone(), abort_token.clone());
}
let proxy_events_rx = self.proxy_state.proxy_events.subscribe();

let deps = Arc::new(RealDriverDeps {
    docker: self.docker.clone(),
    proxy_state: self.proxy_state.clone(),
});
let driver = Driver::new(row, deps, self.inventory.clone(), self.emitter.clone())
    .with_abort_token(abort_token)
    .with_proxy_events(proxy_events_rx);

let tokens_for_cleanup = self.abort_tokens.clone();
let id_for_cleanup = id.clone();
tokio::spawn(async move {
    driver.run().await;
    tokens_for_cleanup.write().unwrap().remove(&id_for_cleanup);
});
```

- [ ] **Step 5: Sync test**

Create `crates/isengard-agent/tests/abort_dispatch.rs`:

```rust
//! Verifies that calling `handle_abort` on the Supervisor cancels the registered token.

use isengard_agent::deployment::DeploymentSupervisor;
use isengard_agent::proxy::ProxyState;
use isengard_core::NoopEmitter;
use isengard_storage::inventory::Inventory;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn handle_abort_returns_true_when_token_registered_and_cancels_it() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let docker = Arc::new(bollard::Docker::connect_with_local_defaults().unwrap());
    let proxy_state = ProxyState::new();
    let sup = DeploymentSupervisor::new(inv, docker, proxy_state, Arc::new(NoopEmitter));

    // Cheat: register a token directly via a test-only setter.
    // (Add `pub fn _test_register_abort_token(&self, id: &str, token: CancellationToken)`
    // to DeploymentSupervisor for this test; gate behind cfg(test) if you prefer.)
    let token = CancellationToken::new();
    sup._test_register_abort_token("dep-1", token.clone());

    assert!(sup.handle_abort("dep-1").await);
    assert!(token.is_cancelled());

    // Unknown id returns false.
    assert!(!sup.handle_abort("unknown").await);
}
```

Add the test-only helper to Supervisor:

```rust
#[cfg(test)]
impl DeploymentSupervisor {
    pub fn _test_register_abort_token(&self, id: &str, token: CancellationToken) {
        self.abort_tokens.write().unwrap().insert(id.to_string(), token);
    }
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p isengard-agent --test abort_dispatch`
Expected: pass.

Run: `cargo test --workspace --lib`
Expected: full workspace lib green.

Run: `cargo build --workspace`
Expected: clean (proto regeneration succeeds).

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/isengard-proto/proto/isengard.v1.proto \
        crates/isengard-controller/src/routing.rs \
        crates/isengard-agent/src/sync.rs \
        crates/isengard-agent/src/deployment/mod.rs \
        crates/isengard-agent/tests/abort_dispatch.rs
git commit -m "feat(agent+controller): AbortDeployment proto + send_message_to_host + supervisor handle_abort"
```

---

## Task 10: POST /api/v1/deployments/:id/abort + DeploymentAbortedPanel + abort wire-up

**Files:**
- Modify: `crates/isengard-plugins/dashboard/src/deployments.rs`
- Modify: `crates/isengard-plugins/dashboard/tests/deployments_endpoints.rs`
- Create: `crates/isengard-plugins/dashboard/web/components/DeploymentAbortedPanel.vue`
- Modify: `crates/isengard-plugins/dashboard/web/pages/stacks/[id].vue`

- [ ] **Step 1: Add abort handler**

Append to `crates/isengard-plugins/dashboard/src/deployments.rs`:

```rust
pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/deployments", get(list_deployments))
        .route("/deployments/:id/abort", post(abort_deployment))     // NEW
        .with_state(handles)
}

#[derive(Debug, Serialize)]
pub struct AbortResponse {
    pub noop: bool,
    pub reason: Option<String>,
}

async fn abort_deployment(
    State(handles): State<Arc<ControllerHandles>>,
    Path(deployment_id): Path<String>,
) -> Result<(StatusCode, Json<AbortResponse>), (StatusCode, String)> {
    let dep = handles.inventory
        .get_deployment(&deployment_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or((StatusCode::NOT_FOUND, format!("deployment {deployment_id} not found")))?;

    if dep.state.is_terminal() {
        return Ok((StatusCode::OK, Json(AbortResponse {
            noop: true,
            reason: Some(format!("deployment_already_terminal: {}", dep.state.as_str())),
        })));
    }

    let msg = isengard_proto::pb::ControllerMessage {
        payload: Some(isengard_proto::pb::controller_message::Payload::AbortDeployment(
            isengard_proto::pb::AbortDeployment { deployment_id: deployment_id.clone() },
        )),
    };

    handles.routing
        .send_message_to_host(dep.host_id, msg)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("agent offline: {e}")))?;

    Ok((StatusCode::ACCEPTED, Json(AbortResponse { noop: false, reason: None })))
}
```

- [ ] **Step 2: Endpoint tests**

Append to `crates/isengard-plugins/dashboard/tests/deployments_endpoints.rs`:

```rust
#[tokio::test]
async fn abort_returns_noop_for_terminal_deployment() {
    let (app, inv, host, stack) = setup_app().await;
    let dep = inv.insert_deployment(sample_dep(host, stack, DeploymentState::Pending)).await.unwrap();
    inv.update_deployment_state(&dep.id, DeploymentState::Done).await.unwrap();

    let req = Request::builder()
        .method("POST")
        .uri(format!("/deployments/{}/abort", dep.id))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
    let r: isengard_plugin_dashboard::deployments::AbortResponse = serde_json::from_slice(&body).unwrap();
    assert!(r.noop);
}

#[tokio::test]
async fn abort_returns_404_for_unknown_deployment() {
    let (app, _inv, _host, _stack) = setup_app().await;

    let req = Request::builder()
        .method("POST")
        .uri("/deployments/unknown-id/abort")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
```

(The "agent offline" path returns 503 — testing it requires a mock host registry; defer to manual smoke.)

- [ ] **Step 3: DeploymentAbortedPanel.vue**

Create `crates/isengard-plugins/dashboard/web/components/DeploymentAbortedPanel.vue`:

```vue
<template>
  <div class="border border-iso-error rounded-lg p-4 bg-iso-bg-elevated">
    <div class="flex items-center justify-between mb-3">
      <div>
        <span class="text-iso-error">⚠</span>
        <span class="text-sm font-semibold ml-2">Deployment {{ deployment.state }}</span>
      </div>
      <div class="text-xs text-iso-text-muted">
        {{ deployment.service_name }} · {{ shortHash(deployment.blue_digest) }} → {{ shortHash(deployment.green_digest) }} (target)
      </div>
    </div>

    <div class="text-xs text-iso-text-muted mb-3">
      <strong>Reason:</strong> {{ deployment.error || 'unknown' }}
    </div>

    <div class="text-xs text-iso-text-muted mb-3">
      Blue ({{ shortHash(deployment.blue_digest) }}) is still serving traffic.
    </div>

    <div class="flex items-center justify-between">
      <button
        class="px-3 py-1 text-xs font-medium bg-iso-info text-iso-bg-base rounded"
        :disabled="retrying"
        @click="onRetry"
      >
        {{ retrying ? 'Retrying…' : 'Retry' }}
      </button>
      <button
        class="text-xs text-iso-text-muted hover:text-iso-text-primary"
        @click="emit('dismiss')"
      >
        Dismiss
      </button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import type { DeploymentDto } from '~/composables/useDeployments'

const props = defineProps<{ deployment: DeploymentDto }>()
const emit = defineEmits<{ (e: 'dismiss'): void }>()
const retrying = ref(false)
const { post } = useApi()

function shortHash(d: string): string {
  return d.length > 14 ? d.slice(0, 14) + '…' : d
}

async function onRetry() {
  retrying.value = true
  try {
    // Trigger a force-update via existing endpoint. The updater will detect
    // the still-stale image and re-enter the deployment flow (Plan A).
    await post(`/stacks/${props.deployment.stack_id}/force-update`, {})
    emit('dismiss')
  } catch (e) {
    retrying.value = false
    alert(`Retry failed: ${e}`)
  }
}
</script>
```

If `/stacks/:id/force-update` doesn't exist exactly, use whatever the existing path is for "force a stack to re-check for updates" — grep `crates/isengard-plugins/dashboard/src/` for `force` or `force_update`.

- [ ] **Step 4: Wire DeploymentAbortedPanel into Stack detail**

Update `crates/isengard-plugins/dashboard/web/pages/stacks/[id].vue`:

```vue
<script setup lang="ts">
// existing setup
import DeploymentInProgressPanel from '~/components/DeploymentInProgressPanel.vue'
import DeploymentAbortedPanel from '~/components/DeploymentAbortedPanel.vue'

const { active, history } = useDeployments(stackIdParam.value)
const dismissedIds = ref(new Set<string>())

const visibleActive = computed(() => {
  if (active.value.length === 0) return null
  return [...active.value].sort((a, b) => b.created_at.localeCompare(a.created_at))[0]
})

const visibleAborted = computed(() => {
  if (visibleActive.value) return null
  // Show the most recent terminal deployment that's NOT 'done' AND finished < 5 minutes ago AND not dismissed.
  const recent = history.value.find((d) => {
    if (dismissedIds.value.has(d.id)) return false
    if (d.state === 'done') return false
    const finishedAt = d.finished_at || d.updated_at
    const ageMs = Date.now() - new Date(finishedAt).getTime()
    return ageMs < 5 * 60 * 1000
  })
  return recent || null
})

function dismissAborted(id: string) {
  dismissedIds.value.add(id)
}
</script>

<template>
  <DeploymentInProgressPanel
    v-if="visibleActive"
    :deployment="visibleActive"
    class="mb-4"
  />
  <DeploymentAbortedPanel
    v-else-if="visibleAborted"
    :deployment="visibleAborted"
    class="mb-4"
    @dismiss="dismissAborted(visibleAborted.id)"
  />
  <!-- existing services grid -->
</template>
```

- [ ] **Step 5: Run tests + build verify**

Run: `cargo test -p isengard-plugin-dashboard --test deployments_endpoints`
Expected: 4/4 pass (2 from Task 5 + 2 new).

Run: `cd crates/isengard-plugins/dashboard/web && bun run build`
Expected: success.

Run: `cd /Users/dirdmaster/Projects/isengard/.worktrees/blue-green-ui && cargo build --workspace`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-plugins/dashboard/src/deployments.rs \
        crates/isengard-plugins/dashboard/tests/deployments_endpoints.rs \
        crates/isengard-plugins/dashboard/web/components/DeploymentAbortedPanel.vue \
        crates/isengard-plugins/dashboard/web/pages/stacks/\[id\].vue
git commit -m "feat(dashboard): POST /api/v1/deployments/:id/abort + DeploymentAbortedPanel + retry wire-up"
```

---

## Task 11: DeploymentsSettings.vue + Deployments tab + composable + GET/PUT REST

**Files:**
- Modify: `crates/isengard-plugins/dashboard/src/deployments.rs` (add settings endpoints)
- Modify: `crates/isengard-plugins/dashboard/tests/deployments_endpoints.rs`
- Create: `crates/isengard-plugins/dashboard/web/composables/useServiceDeployStrategy.ts`
- Create: `crates/isengard-plugins/dashboard/web/components/DeploymentsSettings.vue`
- Modify: `crates/isengard-plugins/dashboard/web/pages/settings/index.vue`

- [ ] **Step 1: REST: GET services with override + PUT override**

Append to `crates/isengard-plugins/dashboard/src/deployments.rs`:

```rust
#[derive(Debug, Serialize)]
pub struct ServiceDeployStrategyDto {
    pub service_id: i64,
    pub host_id: String,
    pub stack_id: Option<i64>,
    pub stack_name: Option<String>,
    pub service_name: String,
    pub override_value: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PutOverrideBody {
    pub override_value: Option<String>,
}

pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/deployments", get(list_deployments))
        .route("/deployments/:id/abort", post(abort_deployment))
        .route("/services/deploy-strategy", get(list_service_strategies))
        .route("/services/:id/deploy-strategy", axum::routing::put(put_service_strategy))
        .with_state(handles)
}

async fn list_service_strategies(
    State(handles): State<Arc<ControllerHandles>>,
) -> Result<Json<Vec<ServiceDeployStrategyDto>>, (StatusCode, String)> {
    let services = handles.inventory.list_all_services()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    let mut dtos = Vec::with_capacity(services.len());
    for s in services {
        let stack_name = match s.stack_id {
            Some(sid) => handles.inventory.get_stack(sid).await.ok().flatten().map(|st| st.name),
            None => None,
        };
        dtos.push(ServiceDeployStrategyDto {
            service_id: s.id.0,
            host_id: s.host_id.0.to_string(),
            stack_id: s.stack_id.map(|s| s.0),
            stack_name,
            service_name: s.name,
            override_value: s.deploy_strategy_override,
        });
    }
    Ok(Json(dtos))
}

async fn put_service_strategy(
    State(handles): State<Arc<ControllerHandles>>,
    Path(service_id): Path<i64>,
    Json(body): Json<PutOverrideBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Validate override value if provided.
    if let Some(v) = body.override_value.as_deref() {
        if !["auto", "blue-green", "in-place"].contains(&v) {
            return Err((StatusCode::BAD_REQUEST, format!("invalid override: {v}")));
        }
    }
    handles.inventory
        .set_service_deploy_strategy_override(
            isengard_storage::service::ServiceId(service_id),
            body.override_value.as_deref().filter(|v| *v != "auto"),
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(StatusCode::NO_CONTENT)
}
```

First grep for existing equivalents: `grep -n "list_services\|list_all\|fn get_stack" crates/isengard-storage/src/`. If the existing API only exposes `list_services_for_host(host_id)` or similar, add a fleet-wide variant. Reference shapes:

```rust
// in crates/isengard-storage/src/service.rs (or inventory.rs)
pub async fn list_all_services(&self) -> Result<Vec<Service>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT id, host_id, stack_id, name, container_id, image, state, deploy_strategy_override
         FROM services ORDER BY name"
    )
    .fetch_all(self.pool())
    .await?;
    rows.iter().map(row_to_service).collect()
}

// in crates/isengard-storage/src/stack.rs (or inventory.rs)
pub async fn get_stack(&self, id: StackId) -> Result<Option<Stack>> {
    use sqlx::Row;
    let row = sqlx::query("SELECT id, host_id, name, source FROM stacks WHERE id = ?")
        .bind(id.0)
        .fetch_optional(self.pool())
        .await?;
    let Some(r) = row else { return Ok(None) };
    Ok(Some(row_to_stack(&r)?))
}
```

If `row_to_stack` doesn't exist, inline the field reads. Include any added methods in the Task 11 commit's `git add -u`.

- [ ] **Step 2: Endpoint tests**

Append to `crates/isengard-plugins/dashboard/tests/deployments_endpoints.rs`:

```rust
#[tokio::test]
async fn put_service_strategy_persists() {
    let (app, inv, host, stack) = setup_app().await;
    let svc = inv.insert_service(isengard_storage::service::InsertService {
        host_id: host,
        stack_id: Some(stack),
        name: "web".into(),
        container_id: Some("c1".into()),
        image: Some("nginx:alpine".into()),
        state: isengard_storage::service::ServiceState::Running,
    }).await.unwrap();

    let req = Request::builder()
        .method("PUT")
        .uri(format!("/services/{}/deploy-strategy", svc.id.0))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"override_value":"blue-green"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let after = inv.get_service(svc.id).await.unwrap().unwrap();
    assert_eq!(after.deploy_strategy_override.as_deref(), Some("blue-green"));
}

#[tokio::test]
async fn put_service_strategy_rejects_unknown_value() {
    let (app, _inv, _host, _stack) = setup_app().await;

    let req = Request::builder()
        .method("PUT")
        .uri("/services/1/deploy-strategy")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"override_value":"bogus"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
```

- [ ] **Step 3: Composable**

Create `crates/isengard-plugins/dashboard/web/composables/useServiceDeployStrategy.ts`:

```ts
import { ref, onMounted } from 'vue'

export interface ServiceDeployStrategy {
  service_id: number
  host_id: string
  stack_id: number | null
  stack_name: string | null
  service_name: string
  override_value: 'blue-green' | 'in-place' | null
}

export function useServiceDeployStrategies() {
  const { get, put } = useApi()
  const services = ref<ServiceDeployStrategy[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function refresh() {
    loading.value = true
    error.value = null
    try {
      services.value = await get<ServiceDeployStrategy[]>('/services/deploy-strategy')
    } catch (e: any) {
      error.value = String(e)
    } finally {
      loading.value = false
    }
  }

  async function setOverride(serviceId: number, value: 'auto' | 'blue-green' | 'in-place') {
    await put(`/services/${serviceId}/deploy-strategy`, { override_value: value === 'auto' ? null : value })
    await refresh()
  }

  onMounted(refresh)
  return { services, loading, error, refresh, setOverride }
}
```

- [ ] **Step 4: DeploymentsSettings.vue**

Create `crates/isengard-plugins/dashboard/web/components/DeploymentsSettings.vue`:

```vue
<template>
  <div>
    <h3 class="text-sm font-semibold text-iso-text-primary mb-4">Deployment strategy per service</h3>
    <p class="text-xs text-iso-text-muted mb-4">
      Container labels override these settings. <code>isengard.deploy.strategy: blue-green | in-place | auto</code>.
    </p>

    <div v-if="loading" class="text-xs text-iso-text-muted">Loading…</div>
    <div v-else-if="error" class="text-xs text-iso-error">{{ error }}</div>
    <div v-else-if="services.length === 0" class="text-xs text-iso-text-muted">
      No services discovered yet. Run a deployment or wait for the next agent heartbeat.
    </div>
    <table v-else class="w-full text-xs">
      <thead class="text-iso-text-muted">
        <tr>
          <th class="text-left pb-2 font-medium">Stack</th>
          <th class="text-left pb-2 font-medium">Service</th>
          <th class="text-left pb-2 font-medium">Strategy</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="s in services" :key="s.service_id" class="border-t border-iso-border">
          <td class="py-2 font-mono">{{ s.stack_name || '-' }}</td>
          <td class="py-2 font-mono">{{ s.service_name }}</td>
          <td class="py-2">
            <label v-for="opt in ['auto', 'blue-green', 'in-place']" :key="opt"
                   class="inline-flex items-center gap-1 mr-3 cursor-pointer">
              <input
                type="radio"
                :name="`strategy-${s.service_id}`"
                :checked="(s.override_value || 'auto') === opt"
                @change="onChange(s.service_id, opt as 'auto' | 'blue-green' | 'in-place')"
              />
              {{ opt }}
            </label>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<script setup lang="ts">
import { useServiceDeployStrategies } from '~/composables/useServiceDeployStrategy'

const { services, loading, error, setOverride } = useServiceDeployStrategies()

async function onChange(serviceId: number, value: 'auto' | 'blue-green' | 'in-place') {
  try {
    await setOverride(serviceId, value)
  } catch (e: any) {
    alert(`Save failed: ${e}`)
  }
}
</script>
```

- [ ] **Step 5: Add Deployments tab in /settings**

Modify `crates/isengard-plugins/dashboard/web/pages/settings/index.vue` to add a fourth tab:

The page currently uses `<SettingsTabs>` with three tabs (general / networking / notifier — added in Plan C). Add a fourth tab `'deployments'`:

```vue
<template>
  <SettingsTabs :tabs="tabs" v-slot="{ activeTab }">
    <FleetsSettings v-if="activeTab === 'general'" />
    <EnrollmentSettings v-if="activeTab === 'general'" />
    <NetworkingSettings v-if="activeTab === 'networking'" />
    <DeploymentsSettings v-if="activeTab === 'deployments'" />
    <NotifierSettings v-if="activeTab === 'notifier'" />
  </SettingsTabs>
</template>

<script setup lang="ts">
// existing imports
import DeploymentsSettings from '~/components/DeploymentsSettings.vue'

const tabs = [
  { key: 'general', label: 'General' },
  { key: 'networking', label: 'Networking' },
  { key: 'deployments', label: 'Deployments' },
  { key: 'notifier', label: 'Notifier' },
]
</script>
```

(Tab order has 'deployments' between 'networking' and 'notifier' to keep policy-shaped tabs adjacent.)

- [ ] **Step 6: Run tests + build verify**

Run: `cargo test -p isengard-plugin-dashboard --test deployments_endpoints`
Expected: 6/6 pass.

Run: `cd crates/isengard-plugins/dashboard/web && bun run build`
Expected: success.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/isengard-plugins/dashboard/src/deployments.rs \
        crates/isengard-plugins/dashboard/tests/deployments_endpoints.rs \
        crates/isengard-plugins/dashboard/web/composables/useServiceDeployStrategy.ts \
        crates/isengard-plugins/dashboard/web/components/DeploymentsSettings.vue \
        crates/isengard-plugins/dashboard/web/pages/settings/index.vue
# Plus storage changes if you had to add list_all_services / get_stack:
git add -u
git commit -m "feat(dashboard): Deployments settings tab + per-service strategy override REST + UI"
```

---

## Task 12: Final workspace gates + open PR #22

- [ ] **Step 1: All four gates**

```bash
cd /Users/dirdmaster/Projects/isengard/.worktrees/blue-green-ui
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
(cd crates/isengard-plugins/dashboard/web && bun run build)
```

Expected: all clean.

- [ ] **Step 2: Push + open PR**

```bash
git push -u origin feat/blue-green-ui

gh pr create --base feat/blue-green-core \
  --title "Phase 10 Plan B: blue-green UI + abort + settings (10e-10g)" \
  --body "$(cat <<'EOF'
## Summary

Implements Plan B of Phase 10. Stacked on PR #21 (Plan A — blue-green core).

- **10e**: Agent's Driver embeds the full Deployment row in `event.metadata.deployment` for every state transition. Controller's event bus subscriber upserts into its local deployments table. New `GET /api/v1/deployments?stack_id=&state=` REST endpoint. New `useDeployments` composable + `DeploymentInProgressPanel` mounted on Stack detail.
- **10f**: Driver gains a `Recovering` state, an `abort_token`, and a 3-way `tokio::select!` during the draining window (grace_period sleep / abort / `routing.upstream.health_changed` event). New `recover_to_blue` helper does an instant swap-back if blue is still alive. New `proxy::ProxyEventBus` for in-process pub-sub of upstream lifecycle. New `ControllerMessage::AbortDeployment` proto variant. `RoutingPusher::send_message_to_host` for arbitrary controller→agent messages. `POST /api/v1/deployments/:id/abort` REST + `DeploymentAbortedPanel` + Retry button (force-update).
- **10g**: New `services.deploy_strategy_override TEXT` column. Supervisor consults it after the container-label override (label still wins). New Deployments tab in /settings + per-service radio override.

## Spec

`docs/superpowers/specs/2026-05-04-phase-10e-10g-blue-green-ui-design.md`

## Test plan

- [x] cargo build --workspace
- [x] cargo test --workspace (~13 new unit tests + 6 endpoint tests)
- [x] cargo clippy --workspace --all-targets -- -D warnings
- [x] cargo deny check
- [x] bun run build (dashboard frontend)
- [ ] Manual smoke: trigger a real deployment via Docker — DeploymentInProgressPanel renders + updates live
- [ ] Manual smoke: click Abort during spinning_up — cleanup happens within 1s
- [ ] Manual smoke: click Abort during draining — traffic swaps back to blue immediately
- [ ] Manual smoke: kill green container manually mid-drain — collapse detected via Pingora health, swap back, panel shows Failed with reason
- [ ] Manual smoke: set override "in-place" on a routed service in Settings — next update goes through recreate path

## Stacked PR merge order

#18 → #19 → #20 → #21 → **#22**. Will rebase as upstream PRs merge.
EOF
)"
```

- [ ] **Step 3: Return PR URL**

---

## Notes for the implementer

- Plan A's deviations (corrected in §"Critical type notes" above) apply throughout — particularly the `EnrollHost` 7-field shape and the storage/core HostId distinction.
- `tokio_util::sync::CancellationToken` may need to be added to `crates/isengard-agent/Cargo.toml` as `tokio-util = { workspace = true, features = ["rt"] }`. Check workspace `Cargo.toml` first.
- `Upstream` may need `#[derive(Clone)]` for the snapshot path. Check `crates/isengard-agent/src/proxy/upstreams.rs`; if Plan A made it `Clone`, this is a no-op. If not, add the derive — it's a small struct.
- The pre-existing `dispatch_helpers.rs` in the updater (Plan A 10d) extracts `label_strategy` from container labels. This is unchanged by Plan B — the supervisor's NEW logic in Task 2 reads the label first, then falls back to the stored override.
- The dashboard's existing tab-with-`v-if` pattern in `/settings` was set up by Plan C (PR #20). Mirror it.
- `useApi` exposes `get<T>`, `post<T>`, `put<T>` (Plan C added `put<T>`). Confirm by `grep "put<T>" crates/isengard-plugins/dashboard/web/composables/useApi.ts` first.
