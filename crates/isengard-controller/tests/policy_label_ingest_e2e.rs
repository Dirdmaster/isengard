//! End-to-end (no gRPC) integration tests for Phase 9b.1 container-scope
//! policy ingest. Drive `PolicyLabelIngest::ingest` directly, then verify
//! that the resolver picks the new container-scope row up.

use std::collections::HashMap;
use std::sync::Arc;

use isengard_controller::policy_ingest::PolicyLabelIngest;
use isengard_core::policy::{
    PolicyContext, PolicyOrigin, UpdateStrategy, resolve_policy,
};
use isengard_proto::pb::{ContainerLabelsRemoved, ContainerLabelsReport};
use isengard_storage::{
    EnrollHost, InsertPolicy, Inventory, policy::PolicyScopeType,
};
use tempfile::{TempDir, tempdir};

async fn setup() -> (TempDir, Arc<Inventory>, isengard_storage::HostId) {
    let dir = tempdir().unwrap();
    let inv = Arc::new(
        Inventory::open(&dir.path().join("isengard.db"))
            .await
            .unwrap(),
    );
    let host = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0".into(),
            docker_version: "27".into(),
            fleet: "default".into(),
        })
        .await
        .unwrap();
    (dir, inv, host)
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
    }
}

/// 1. Label-discovered policy persists as container scope.
#[tokio::test]
async fn label_discovered_policy_persists_as_container_scope() {
    let (_dir, inv, host) = setup().await;
    let ingest = PolicyLabelIngest::new(inv.clone());

    ingest
        .ingest(
            host,
            &report(
                "cid-1",
                "web",
                &[("isengard.policy.strategy", "pinned")],
            ),
        )
        .await
        .unwrap();

    let key = format!("{host}/web");
    let row = inv
        .get_policy(PolicyScopeType::Container, &key)
        .await
        .unwrap()
        .expect("container row should exist after ingest");
    assert_eq!(row.scope_type, PolicyScopeType::Container);
    assert_eq!(row.scope_key, key);
    assert_eq!(row.body.strategy, Some(UpdateStrategy::Pinned));
}

/// 2. Resolver uses container scope when it has labels. After ingest, the
///    resolver returns Pinned with `provenance.strategy == Container`.
#[tokio::test]
async fn resolver_uses_container_scope_after_label_ingest() {
    let (_dir, inv, host) = setup().await;
    let ingest = PolicyLabelIngest::new(inv.clone());

    // Pre-seed a less-specific service row so we can prove container wins.
    let mut svc_body = isengard_core::policy::Policy::default();
    svc_body.strategy = Some(UpdateStrategy::TagOnly);
    inv.insert_policy(InsertPolicy {
        scope_type: PolicyScopeType::Service,
        scope_key: "default/blog/web".into(),
        body: svc_body,
    })
    .await
    .unwrap();

    // Container label says: pinned. Most specific scope wins.
    ingest
        .ingest(
            host,
            &report(
                "cid-1",
                "web",
                &[("isengard.policy.strategy", "pinned")],
            ),
        )
        .await
        .unwrap();

    // Project all rows for the resolver.
    let rows = inv.list_policies().await.unwrap();
    let view: Vec<_> = rows
        .iter()
        .map(|r| (r.scope_type, r.scope_key.as_str(), &r.body))
        .collect();

    let host_str = host.to_string();
    let ctx = PolicyContext {
        fleet: Some("default"),
        stack: Some("blog"),
        service: Some("web"),
        host_id_hex: Some(host_str.as_str()),
        container_name: Some("web"),
    };
    let resolved = resolve_policy(&view, &ctx);
    assert_eq!(resolved.strategy, UpdateStrategy::Pinned);
    assert_eq!(resolved.provenance.strategy, PolicyOrigin::Container);
}

/// 3. Removed container's policy is cleaned up.
#[tokio::test]
async fn removed_container_drops_its_policy_row() {
    let (_dir, inv, host) = setup().await;
    let ingest = PolicyLabelIngest::new(inv.clone());

    ingest
        .ingest(
            host,
            &report(
                "cid-1",
                "web",
                &[("isengard.policy.strategy", "pinned")],
            ),
        )
        .await
        .unwrap();
    let key = format!("{host}/web");
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
        "row must be gone after the agent reports container removed"
    );
}

/// 4. Malformed label value logs a warn but does not crash the ingest, and
///    leaves any pre-existing row alone.
#[tokio::test]
async fn malformed_label_value_does_not_crash_ingest() {
    let (_dir, inv, host) = setup().await;
    let ingest = PolicyLabelIngest::new(inv.clone());

    // Seed a known-good row.
    let key = format!("{host}/web");
    let mut body = isengard_core::policy::Policy::default();
    body.strategy = Some(UpdateStrategy::Pinned);
    inv.insert_policy(InsertPolicy {
        scope_type: PolicyScopeType::Container,
        scope_key: key.clone(),
        body,
    })
    .await
    .unwrap();

    // Now ingest a report with garbage strategy. ingest must Ok(()) without
    // touching the row.
    let res = ingest
        .ingest(
            host,
            &report(
                "cid-1",
                "web",
                &[("isengard.policy.strategy", "obviously-bogus")],
            ),
        )
        .await;
    assert!(res.is_ok(), "ingest should succeed even on parse error");

    let row = inv
        .get_policy(PolicyScopeType::Container, &key)
        .await
        .unwrap()
        .expect("pre-existing row should be preserved");
    assert_eq!(row.body.strategy, Some(UpdateStrategy::Pinned));
}

/// 5. Label-set transitioning from "has policy labels" to "none" deletes
///    the row instead of leaving a stale entry.
#[tokio::test]
async fn dropping_policy_labels_deletes_existing_row() {
    let (_dir, inv, host) = setup().await;
    let ingest = PolicyLabelIngest::new(inv.clone());

    // First report carries a policy label; row gets created.
    ingest
        .ingest(
            host,
            &report(
                "cid-1",
                "web",
                &[("isengard.policy.strategy", "pinned")],
            ),
        )
        .await
        .unwrap();
    let key = format!("{host}/web");
    assert!(
        inv.get_policy(PolicyScopeType::Container, &key)
            .await
            .unwrap()
            .is_some()
    );

    // Operator removes the policy label from compose; agent re-emits the
    // report with the policy labels gone but the container still alive.
    ingest
        .ingest(
            host,
            &report("cid-1", "web", &[("isengard.expose", "x.test")]),
        )
        .await
        .unwrap();

    assert!(
        inv.get_policy(PolicyScopeType::Container, &key)
            .await
            .unwrap()
            .is_none(),
        "row must be deleted when policy labels are removed"
    );
}
