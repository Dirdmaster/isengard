//! DAO-level coverage for `deployment_groups` and stack parallelism.
//! Phase 10c (T1).

use isengard_storage::deployment::{DeployStrategy, DeploymentState, InsertDeployment};
use isengard_storage::{
    DeploymentGroup, DeploymentGroupState, EnrollHost, HostId, InsertDeploymentGroup, InsertStack,
    Inventory, StackId, StackSource,
};
use ulid::Ulid;

async fn fresh_inv() -> Inventory {
    Inventory::open_in_memory().await.expect("open inventory")
}

async fn enroll(inv: &Inventory, hostname: &str) -> HostId {
    inv.enroll_host(EnrollHost {
        fingerprint: format!("fp-{hostname}"),
        hostname: hostname.into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.1.0".into(),
        docker_version: "27.0".into(),
    })
    .await
    .expect("enroll host")
}

async fn make_stack(inv: &Inventory, host_id: HostId, name: &str) -> StackId {
    inv.insert_stack(InsertStack {
        host_id,
        name: name.into(),
        source: StackSource::Compose,
    })
    .await
    .expect("insert stack")
}

fn sample_group(stack_id: StackId, hosts: Vec<HostId>) -> InsertDeploymentGroup {
    InsertDeploymentGroup {
        id: Ulid::new().to_string(),
        stack_id,
        service_name: "web".into(),
        parallelism: "1".into(),
        state: DeploymentGroupState::Pending,
        target_hosts: hosts,
    }
}

fn sample_deployment(host_id: HostId, stack_id: StackId, service: &str) -> InsertDeployment {
    InsertDeployment {
        id: Ulid::new().to_string(),
        host_id,
        stack_id,
        service_name: service.into(),
        strategy: DeployStrategy::BlueGreen,
        state: DeploymentState::Pending,
        blue_container: Some("c-blue".into()),
        green_container: None,
        blue_digest: "sha256:aaa".into(),
        green_digest: "sha256:bbb".into(),
        public_hostname: Some("blog.test".into()),
        health_path: Some("/healthz".into()),
        container_port: Some(8080),
        metadata_json: None,
        previous_digest: None,
    }
}

#[tokio::test]
async fn insert_returns_row_with_pending_state_and_target_hosts() {
    let inv = fresh_inv().await;
    let h1 = enroll(&inv, "h1").await;
    let h2 = enroll(&inv, "h2").await;
    let stack = make_stack(&inv, h1, "blog").await;

    let g = inv
        .insert_deployment_group(sample_group(stack, vec![h1, h2]))
        .await
        .expect("insert group");

    assert_eq!(g.state, DeploymentGroupState::Pending);
    assert_eq!(g.parallelism, "1");
    assert_eq!(g.service_name, "web");
    assert_eq!(g.target_hosts, vec![h1, h2]);
    assert!(g.finished_at.is_none());
    assert!(g.error.is_none());
}

#[tokio::test]
async fn get_returns_none_for_unknown_id() {
    let inv = fresh_inv().await;
    let g = inv.get_deployment_group("nope").await.expect("get");
    assert!(g.is_none());
}

#[tokio::test]
async fn get_returns_inserted_row() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let stack = make_stack(&inv, h, "blog").await;
    let ins = sample_group(stack, vec![h]);
    let id = ins.id.clone();
    inv.insert_deployment_group(ins).await.unwrap();

    let got = inv.get_deployment_group(&id).await.unwrap().unwrap();
    assert_eq!(got.id, id);
    assert_eq!(got.target_hosts, vec![h]);
}

#[tokio::test]
async fn list_groups_orders_by_started_at_desc() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let stack = make_stack(&inv, h, "blog").await;

    let first = sample_group(stack, vec![h]);
    let first_id = first.id.clone();
    inv.insert_deployment_group(first).await.unwrap();

    // Tick CURRENT_TIMESTAMP by sleeping past one full second.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let second = sample_group(stack, vec![h]);
    let second_id = second.id.clone();
    inv.insert_deployment_group(second).await.unwrap();

    let listed = inv.list_deployment_groups(stack, 10).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, second_id, "newest first");
    assert_eq!(listed[1].id, first_id);
}

#[tokio::test]
async fn list_groups_respects_limit() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let stack = make_stack(&inv, h, "blog").await;
    for _ in 0..3 {
        inv.insert_deployment_group(sample_group(stack, vec![h]))
            .await
            .unwrap();
    }
    let listed = inv.list_deployment_groups(stack, 2).await.unwrap();
    assert_eq!(listed.len(), 2);
}

#[tokio::test]
async fn list_groups_scopes_to_stack() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let s1 = make_stack(&inv, h, "blog").await;
    let s2 = make_stack(&inv, h, "shop").await;

    inv.insert_deployment_group(sample_group(s1, vec![h]))
        .await
        .unwrap();
    inv.insert_deployment_group(sample_group(s2, vec![h]))
        .await
        .unwrap();

    let blog = inv.list_deployment_groups(s1, 10).await.unwrap();
    let shop = inv.list_deployment_groups(s2, 10).await.unwrap();
    assert_eq!(blog.len(), 1);
    assert_eq!(shop.len(), 1);
    assert_ne!(blog[0].id, shop[0].id);
}

#[tokio::test]
async fn update_state_to_rolling_keeps_finished_at_null() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let stack = make_stack(&inv, h, "blog").await;
    let ins = sample_group(stack, vec![h]);
    let id = ins.id.clone();
    inv.insert_deployment_group(ins).await.unwrap();

    inv.update_deployment_group_state(&id, DeploymentGroupState::Rolling, None)
        .await
        .unwrap();

    let g = inv.get_deployment_group(&id).await.unwrap().unwrap();
    assert_eq!(g.state, DeploymentGroupState::Rolling);
    assert!(g.finished_at.is_none());
}

#[tokio::test]
async fn update_state_to_done_sets_finished_at() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let stack = make_stack(&inv, h, "blog").await;
    let ins = sample_group(stack, vec![h]);
    let id = ins.id.clone();
    inv.insert_deployment_group(ins).await.unwrap();

    inv.update_deployment_group_state(&id, DeploymentGroupState::Done, None)
        .await
        .unwrap();

    let g = inv.get_deployment_group(&id).await.unwrap().unwrap();
    assert_eq!(g.state, DeploymentGroupState::Done);
    assert!(g.finished_at.is_some());
}

#[tokio::test]
async fn update_state_to_failed_records_error() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let stack = make_stack(&inv, h, "blog").await;
    let ins = sample_group(stack, vec![h]);
    let id = ins.id.clone();
    inv.insert_deployment_group(ins).await.unwrap();

    inv.update_deployment_group_state(&id, DeploymentGroupState::Failed, Some("wave 1 aborted"))
        .await
        .unwrap();

    let g = inv.get_deployment_group(&id).await.unwrap().unwrap();
    assert_eq!(g.state, DeploymentGroupState::Failed);
    assert_eq!(g.error.as_deref(), Some("wave 1 aborted"));
    assert!(g.finished_at.is_some());
}

#[tokio::test]
async fn cascade_delete_drops_groups_when_stack_removed() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let stack = make_stack(&inv, h, "blog").await;
    let ins = sample_group(stack, vec![h]);
    let id = ins.id.clone();
    inv.insert_deployment_group(ins).await.unwrap();

    inv.delete_stack(stack).await.unwrap();

    assert!(inv.get_deployment_group(&id).await.unwrap().is_none());
}

#[tokio::test]
async fn set_deployment_group_links_existing_deployment() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let stack = make_stack(&inv, h, "blog").await;

    let dep = inv
        .insert_deployment(sample_deployment(h, stack, "web"))
        .await
        .unwrap();

    let group = inv
        .insert_deployment_group(sample_group(stack, vec![h]))
        .await
        .unwrap();

    inv.set_deployment_group(&dep.id, &group.id).await.unwrap();

    let in_group = inv.list_deployments_by_group(&group.id).await.unwrap();
    assert_eq!(in_group.len(), 1);
    assert_eq!(in_group[0].id, dep.id);
}

#[tokio::test]
async fn list_deployments_by_group_orders_by_created_asc_and_filters() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let stack = make_stack(&inv, h, "blog").await;
    let group: DeploymentGroup = inv
        .insert_deployment_group(sample_group(stack, vec![h]))
        .await
        .unwrap();

    // Two deployments are part of the group, one is not.
    let a = inv
        .insert_deployment(sample_deployment(h, stack, "svc-a"))
        .await
        .unwrap();
    inv.set_deployment_group(&a.id, &group.id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let b = inv
        .insert_deployment(sample_deployment(h, stack, "svc-b"))
        .await
        .unwrap();
    inv.set_deployment_group(&b.id, &group.id).await.unwrap();
    let c = inv
        .insert_deployment(sample_deployment(h, stack, "svc-c"))
        .await
        .unwrap();
    // c is intentionally not linked.

    let listed = inv.list_deployments_by_group(&group.id).await.unwrap();
    let ids: Vec<_> = listed.iter().map(|d| d.id.clone()).collect();
    assert_eq!(ids, vec![a.id.clone(), b.id.clone()]);
    assert!(!ids.contains(&c.id));
}

#[tokio::test]
async fn list_deployments_by_group_returns_empty_for_unknown_group() {
    let inv = fresh_inv().await;
    let listed = inv.list_deployments_by_group("nope").await.unwrap();
    assert!(listed.is_empty());
}

#[tokio::test]
async fn set_stack_parallelism_round_trips() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let stack = make_stack(&inv, h, "blog").await;

    // Default (NULL) until explicitly set.
    let initial = inv.get_stack_parallelism(stack).await.unwrap();
    assert!(initial.is_none());

    inv.set_stack_parallelism(stack, Some("all")).await.unwrap();
    let v = inv.get_stack_parallelism(stack).await.unwrap();
    assert_eq!(v.as_deref(), Some("all"));

    inv.set_stack_parallelism(stack, Some("3")).await.unwrap();
    let v = inv.get_stack_parallelism(stack).await.unwrap();
    assert_eq!(v.as_deref(), Some("3"));

    // Clear back to NULL.
    inv.set_stack_parallelism(stack, None).await.unwrap();
    let v = inv.get_stack_parallelism(stack).await.unwrap();
    assert!(v.is_none());
}

#[tokio::test]
async fn get_stack_parallelism_returns_none_for_unknown_stack() {
    let inv = fresh_inv().await;
    let v = inv.get_stack_parallelism(StackId(9999)).await.unwrap();
    assert!(v.is_none());
}

#[tokio::test]
async fn group_survives_round_trip_with_multiple_target_hosts() {
    let inv = fresh_inv().await;
    let h1 = enroll(&inv, "h1").await;
    let h2 = enroll(&inv, "h2").await;
    let h3 = enroll(&inv, "h3").await;
    let stack = make_stack(&inv, h1, "fleet-stack").await;

    let mut ins = sample_group(stack, vec![h1, h2, h3]);
    ins.parallelism = "all".into();
    ins.state = DeploymentGroupState::Rolling;

    let g = inv.insert_deployment_group(ins).await.unwrap();
    assert_eq!(g.target_hosts.len(), 3);
    assert_eq!(g.parallelism, "all");
    assert_eq!(g.state, DeploymentGroupState::Rolling);

    // Read-back via get returns identical hosts in order.
    let got = inv.get_deployment_group(&g.id).await.unwrap().unwrap();
    assert_eq!(got.target_hosts, vec![h1, h2, h3]);
}
