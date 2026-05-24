//! Container-scope policy ingest from Docker labels.
//!
//! Spec: `docs/superpowers/specs/2026-05-06-phase-9b1-container-label-discovery-design.md`
//! (`Discovery path` and `Cleanup`).
//!
//! The agent's label watcher ships `ContainerLabelsReport` and
//! `ContainerLabelsRemoved` for any container that carries
//! `isengard.policy.*` or `isengard.expose*`. This module consumes
//! those payloads on the controller side and turns them into rows in
//! the `policies` table at `(scope_type=container,
//! scope_key=<host_id>/<name>)`.
//!
//! Cleanup is two-layered:
//!
//! 1. Event-driven: `ContainerLabelsRemoved` deletes the row
//!    immediately, using an in-memory
//!    `(host_id, container_id) -> scope_key` map filled at upsert
//!    time (the removed event only carries `container_id`).
//! 2. Periodic reaper: [`reap_orphaned_container_policies`] deletes
//!    container-scope rows older than 24h. Belt-and-braces against
//!    missed `destroy` events (e.g. agent down during the event).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use isengard_core::policy::labels::{ParseLabelError, parse_policy_labels};
use isengard_proto::pb::{ContainerLabelsRemoved, ContainerLabelsReport};
use isengard_storage::{HostId, Inventory, policy::PolicyScopeType};
use tokio::sync::Mutex;

/// Builds the canonical container-scope `scope_key` the resolver expects.
///
/// Format: `<host_id>/<container_name>`. `host_id` is the ULID's display
/// representation (matches `HostId::Display` and `to_string`).
fn container_scope_key(host_id: HostId, container_name: &str) -> String {
    format!("{host_id}/{container_name}")
}

/// Owns the controller-side ingest of container-scope policy labels.
///
/// Holds an in-memory `(host_id, container_id) -> scope_key` map so
/// the remove path can find the row to delete (the wire payload only
/// carries `container_id`, not name).
pub struct PolicyLabelIngest {
    /// Shared inventory for `policies` writes.
    inv: Arc<Inventory>,
    /// `(host_id, container_id) -> scope_key`. Populated on upsert;
    /// drained on remove.
    by_container: Mutex<HashMap<(HostId, String), String>>,
}

impl PolicyLabelIngest {
    /// Builds an ingest worker over the shared inventory.
    pub fn new(inv: Arc<Inventory>) -> Self {
        Self {
            inv,
            by_container: Mutex::new(HashMap::new()),
        }
    }

    /// Ingests a `ContainerLabelsReport` into the `policies` table.
    ///
    /// - Parses `isengard.policy.*` labels into a `Policy`. On parse
    ///   error, logs at `warn` and returns `Ok(())` without touching
    ///   the row (the container's other ingest paths still run).
    /// - When the parsed body has any field set, upserts a
    ///   container-scope row at `<host_id>/<container_name>`.
    /// - When the parsed body is `Policy::default()` (no policy
    ///   labels present), deletes any row that previously existed for
    ///   this container. Lets a container drop its policy by removing
    ///   the labels.
    ///
    /// Records the `(host_id, container_id) -> scope_key` association
    /// so the remove path can resolve the `scope_key`.
    ///
    /// # Errors
    ///
    /// Returns `Err` on storage failure during the upsert or delete.
    pub async fn ingest(&self, host_id: HostId, report: &ContainerLabelsReport) -> Result<()> {
        let scope_key = container_scope_key(host_id, &report.container_name);

        let body = match parse_policy_labels(&report.labels) {
            Ok(b) => b,
            Err(e @ ParseLabelError { .. }) => {
                tracing::warn!(
                    host_id = %host_id,
                    container_id = %report.container_id,
                    container_name = %report.container_name,
                    error = %e,
                    "policy labels: parse failed; skipping upsert (existing row, if any, preserved)"
                );
                return Ok(());
            }
        };

        // Track the (host, container_id) -> scope_key mapping so the remove
        // event can find the right row. Done unconditionally: even if the
        // parsed body is empty, a future label add on the same container
        // should still hit the same scope_key.
        {
            let mut guard = self.by_container.lock().await;
            guard.insert((host_id, report.container_id.clone()), scope_key.clone());
        }

        let any_field_set = body.strategy.is_some()
            || body.gate.is_some()
            || body.paused_until.is_some()
            || body.on_failure.is_some()
            || body.approver_channel.is_some();

        if any_field_set {
            self.inv
                .upsert_policy(PolicyScopeType::Container, &scope_key, &body)
                .await?;
            tracing::debug!(
                host_id = %host_id,
                container_name = %report.container_name,
                "policy labels: upserted container-scope row"
            );
        } else {
            // No policy labels; drop any existing row so the resolver falls
            // back to less-specific scopes for this container.
            let deleted = self
                .inv
                .delete_policy(PolicyScopeType::Container, &scope_key)
                .await?;
            if deleted {
                tracing::debug!(
                    host_id = %host_id,
                    container_name = %report.container_name,
                    "policy labels: deleted container-scope row (labels removed)"
                );
            }
        }

        Ok(())
    }

    /// Drops the container-scope policy row for a removed container.
    ///
    /// `ContainerLabelsRemoved` only carries `container_id`, so the
    /// path looks up the `scope_key` from the in-memory map. When the
    /// map has no entry (the remove arrived without a prior report on
    /// this controller boot), the periodic reaper catches the row by
    /// `updated_at` age.
    ///
    /// # Errors
    ///
    /// Returns `Err` on storage failure.
    pub async fn ingest_removed(&self, host_id: HostId, ev: &ContainerLabelsRemoved) -> Result<()> {
        let scope_key = {
            let mut guard = self.by_container.lock().await;
            guard.remove(&(host_id, ev.container_id.clone()))
        };
        let Some(scope_key) = scope_key else {
            // No mapping; nothing to do here. Reaper covers the gap.
            return Ok(());
        };
        let deleted = self
            .inv
            .delete_policy(PolicyScopeType::Container, &scope_key)
            .await?;
        if deleted {
            tracing::debug!(
                host_id = %host_id,
                container_id = %ev.container_id,
                "policy labels: deleted container-scope row (container removed)"
            );
        }
        Ok(())
    }
}

/// Reaps container-scope policy rows whose `updated_at` is older
/// than `max_age`. Returns the number of rows deleted.
///
/// The agent re-emits a fresh `ContainerLabelsReport` for every live
/// labeled container at every sync-stream open (initial scan) and on
/// every `start`/`update` Docker event, which causes `upsert_policy`
/// to bump `updated_at`. A row whose `updated_at` is more than
/// `max_age` old has almost certainly been orphaned (container
/// destroyed during agent downtime, or the container's host
/// disconnected long enough that the destroy event went missing).
///
/// Conservative default for production: 24h. Pure function over the
/// inventory and a `now` clock so unit tests don't need to wait.
///
/// # Errors
///
/// Returns `Err` on storage failure during the list or any delete.
pub async fn reap_orphaned_container_policies(
    inv: &Inventory,
    now: DateTime<Utc>,
    max_age: Duration,
) -> Result<usize> {
    let cutoff = now - max_age;
    let rows = inv.list_policies().await?;
    let mut reaped = 0;
    for r in rows {
        if r.scope_type != PolicyScopeType::Container {
            continue;
        }
        if r.updated_at < cutoff {
            let deleted = inv
                .delete_policy(PolicyScopeType::Container, &r.scope_key)
                .await?;
            if deleted {
                reaped += 1;
                tracing::info!(
                    scope_key = %r.scope_key,
                    age_hours = (now - r.updated_at).num_hours(),
                    "policy labels: reaped orphaned container-scope row"
                );
            }
        }
    }
    Ok(reaped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_core::policy::{Policy, UpdateStrategy};
    use isengard_storage::{EnrollHost, InsertPolicy};
    use std::collections::HashMap;

    async fn setup() -> (Arc<Inventory>, HostId) {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let host = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp".into(),
                hostname: "h".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0".into(),
                docker_version: "27".into(),
            })
            .await
            .unwrap();
        (inv, host)
    }

    fn report(cid: &str, name: &str, pairs: &[(&str, &str)]) -> ContainerLabelsReport {
        let labels: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        ContainerLabelsReport {
            container_id: cid.into(),
            container_name: name.into(),
            image: "test".into(),
            labels,
            label_route_intents: Vec::new(),
        }
    }

    #[tokio::test]
    async fn ingest_with_strategy_pinned_upserts_container_row() {
        let (inv, host) = setup().await;
        let ingest = PolicyLabelIngest::new(inv.clone());

        let r = report("cid-1", "web", &[("isengard.policy.strategy", "pinned")]);
        ingest.ingest(host, &r).await.unwrap();

        let key = container_scope_key(host, "web");
        let row = inv
            .get_policy(PolicyScopeType::Container, &key)
            .await
            .unwrap()
            .expect("row should exist");
        assert_eq!(row.body.strategy, Some(UpdateStrategy::Pinned));
    }

    #[tokio::test]
    async fn ingest_with_no_policy_labels_deletes_existing_row() {
        let (inv, host) = setup().await;
        let ingest = PolicyLabelIngest::new(inv.clone());

        // Pre-seed a container-scope row.
        let key = container_scope_key(host, "api");
        let body = Policy {
            strategy: Some(UpdateStrategy::Pinned),
            ..Policy::default()
        };
        inv.insert_policy(InsertPolicy {
            scope_type: PolicyScopeType::Container,
            scope_key: key.clone(),
            body,
        })
        .await
        .unwrap();

        // Now ingest a report with NO policy labels (e.g. operator removed
        // them from the compose file).
        let r = report("cid-1", "api", &[("isengard.expose", "x.test")]);
        ingest.ingest(host, &r).await.unwrap();

        let row = inv
            .get_policy(PolicyScopeType::Container, &key)
            .await
            .unwrap();
        assert!(row.is_none(), "row should have been deleted");
    }

    #[tokio::test]
    async fn ingest_removed_deletes_row_via_in_memory_map() {
        let (inv, host) = setup().await;
        let ingest = PolicyLabelIngest::new(inv.clone());

        let r = report("cid-1", "web", &[("isengard.policy.strategy", "pinned")]);
        ingest.ingest(host, &r).await.unwrap();
        let key = container_scope_key(host, "web");
        assert!(
            inv.get_policy(PolicyScopeType::Container, &key)
                .await
                .unwrap()
                .is_some()
        );

        ingest
            .ingest_removed(
                host,
                &ContainerLabelsRemoved {
                    container_id: "cid-1".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            inv.get_policy(PolicyScopeType::Container, &key)
                .await
                .unwrap()
                .is_none(),
            "row should have been deleted by remove event"
        );
    }

    #[tokio::test]
    async fn malformed_label_value_does_not_crash_or_overwrite() {
        let (inv, host) = setup().await;
        let ingest = PolicyLabelIngest::new(inv.clone());

        // Pre-seed a known-good row.
        let key = container_scope_key(host, "web");
        let body = Policy {
            strategy: Some(UpdateStrategy::Pinned),
            ..Policy::default()
        };
        inv.insert_policy(InsertPolicy {
            scope_type: PolicyScopeType::Container,
            scope_key: key.clone(),
            body: body.clone(),
        })
        .await
        .unwrap();

        // Now ingest a report whose strategy label is garbage. Expect: no
        // panic, no row mutation.
        let r = report("cid-1", "web", &[("isengard.policy.strategy", "pinneded")]);
        ingest.ingest(host, &r).await.unwrap();

        let row = inv
            .get_policy(PolicyScopeType::Container, &key)
            .await
            .unwrap()
            .expect("pre-existing row should be preserved");
        assert_eq!(row.body.strategy, Some(UpdateStrategy::Pinned));
    }

    #[tokio::test]
    async fn reaper_with_now_within_max_age_keeps_rows() {
        let (inv, _host) = setup().await;
        let body = Policy {
            strategy: Some(UpdateStrategy::Pinned),
            ..Policy::default()
        };
        inv.insert_policy(InsertPolicy {
            scope_type: PolicyScopeType::Container,
            scope_key: "hosta/web".into(),
            body,
        })
        .await
        .unwrap();

        // Wall-clock now. The just-inserted row's updated_at is moments
        // ago; max_age=24h keeps it.
        let now = Utc::now();
        let reaped = reap_orphaned_container_policies(&inv, now, Duration::hours(24))
            .await
            .unwrap();
        assert_eq!(reaped, 0);
        assert!(
            inv.get_policy(PolicyScopeType::Container, "hosta/web")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn reaper_with_now_far_in_future_reaps_only_container_rows() {
        let (inv, _host) = setup().await;

        // One container row.
        let body = Policy {
            strategy: Some(UpdateStrategy::Pinned),
            ..Policy::default()
        };
        inv.insert_policy(InsertPolicy {
            scope_type: PolicyScopeType::Container,
            scope_key: "hosta/old".into(),
            body: body.clone(),
        })
        .await
        .unwrap();

        // One global row to confirm scope filtering.
        inv.insert_policy(InsertPolicy {
            scope_type: PolicyScopeType::Global,
            scope_key: "".into(),
            body,
        })
        .await
        .unwrap();

        // Pretend "now" is 100 days from now. With max_age=24h every row
        // looks orphaned by age, but the reaper must only touch container
        // scope.
        let now = Utc::now() + Duration::days(100);
        let reaped = reap_orphaned_container_policies(&inv, now, Duration::hours(24))
            .await
            .unwrap();
        assert_eq!(reaped, 1);
        assert!(
            inv.get_policy(PolicyScopeType::Container, "hosta/old")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            inv.get_policy(PolicyScopeType::Global, "")
                .await
                .unwrap()
                .is_some()
        );
    }
}
