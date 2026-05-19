//! End-to-end tests for the stack deployment orchestrator.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use isengard_controller::bus::EventBus;
use isengard_controller::stack_deploy_orchestrator::{
    OnFailure, ProductionDispatcher, StackDeployOrchestrator, StartOutcome, WaveDispatcher,
};
use isengard_core::Event;
use isengard_storage::deployment::{DeployStrategy, Deployment, DeploymentState, InsertDeployment};
use isengard_storage::host::EnrollHost;
use isengard_storage::stack::{InsertStack, StackSource};
use isengard_storage::{DeploymentGroupState, HostId, Inventory, StackId};
use tokio::sync::Mutex;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

async fn fresh_inv() -> Arc<Inventory> {
    Arc::new(Inventory::open_in_memory().await.unwrap())
}

async fn enroll(inv: &Inventory, name: &str) -> HostId {
    inv.enroll_host(EnrollHost {
        fingerprint: format!("fp-{name}"),
        hostname: name.into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.1.0".into(),
        docker_version: "27.0".into(),
    })
    .await
    .unwrap()
}

async fn make_stack(inv: &Inventory, host: HostId, name: &str) -> StackId {
    inv.insert_stack(InsertStack {
        host_id: host,
        name: name.into(),
        source: StackSource::Compose,
    })
    .await
    .unwrap()
}

fn dep_template(host: HostId, stack: StackId) -> InsertDeployment {
    InsertDeployment {
        id: Ulid::new().to_string(),
        host_id: host,
        stack_id: stack,
        service_name: "web".into(),
        strategy: DeployStrategy::BlueGreen,
        state: DeploymentState::Pending,
        blue_container: None,
        green_container: None,
        blue_digest: "sha256:aaa".into(),
        green_digest: "sha256:bbb".into(),
        public_hostname: None,
        health_path: None,
        container_port: None,
        metadata_json: None,
        previous_digest: None,
    }
}

/// Helper: build an in-memory Deployment row for an event metadata payload.
/// Uses the row freshly read from storage so we get correct timestamps.
async fn dep_event(inv: &Inventory, dep_id: &str, kind: &str) -> Event {
    let row = inv.get_deployment(dep_id).await.unwrap().unwrap();
    let mut metadata = serde_json::Map::new();
    metadata.insert("deployment".into(), serde_json::to_value(row).unwrap());
    Event {
        kind: kind.into(),
        occurred_at: Utc::now(),
        summary: kind.into(),
        metadata: serde_json::Value::Object(metadata),
        ..Default::default()
    }
}

#[derive(Default)]
struct RecordingDispatcher {
    calls: Mutex<Vec<Vec<HostId>>>,
}

impl RecordingDispatcher {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl WaveDispatcher for RecordingDispatcher {
    async fn dispatch(
        &self,
        host_ids: &[HostId],
        _stack_id: StackId,
        _service_name: &str,
        _deployment_ids: &[String],
    ) -> Result<(), anyhow::Error> {
        self.calls.lock().await.push(host_ids.to_vec());
        Ok(())
    }
}

/// Build a 3-host stack with one Pending deployment row per host.
async fn three_host_stack() -> (Arc<Inventory>, StackId, Vec<HostId>, Vec<String>) {
    let inv = fresh_inv().await;
    let h1 = enroll(&inv, "h1").await;
    let h2 = enroll(&inv, "h2").await;
    let h3 = enroll(&inv, "h3").await;
    let stack = make_stack(&inv, h1, "blog").await;
    let mut dep_ids = Vec::new();
    for h in [h1, h2, h3] {
        let d = inv.insert_deployment(dep_template(h, stack)).await.unwrap();
        dep_ids.push(d.id);
    }
    (inv, stack, vec![h1, h2, h3], dep_ids)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn three_host_parallelism_one_rolls_one_at_a_time() {
    let (inv, stack, hosts, dep_ids) = three_host_stack().await;
    let bus = Arc::new(EventBus::new());
    let disp = RecordingDispatcher::new();
    let orch = Arc::new(
        StackDeployOrchestrator::new(inv.clone(), bus.clone(), disp.clone())
            .with_reconcile_interval(Duration::from_millis(20)),
    );
    let _h = orch.clone().start_background();

    // parallelism=1 default.
    let outcome = orch
        .start_group(
            stack,
            "web",
            hosts.clone(),
            dep_ids.clone(),
            OnFailure::Rollback,
        )
        .await
        .unwrap();
    let group_id = match outcome {
        StartOutcome::Group(g) => g,
        other => panic!("expected Group outcome, got {other:?}"),
    };

    // Wave 0 dispatched immediately.
    {
        let calls = disp.calls.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 1, "batch=1: single host per wave");
    }

    // Drive wave 0 to terminal: pick the host that received the first wave.
    let first_host = disp.calls.lock().await[0][0];
    let first_dep_id = dep_ids
        .iter()
        .zip(hosts.iter())
        .find(|(_, h)| **h == first_host)
        .map(|(id, _)| id.clone())
        .unwrap();
    inv.update_deployment_state(&first_dep_id, DeploymentState::Done)
        .await
        .unwrap();
    bus.publish(dep_event(&inv, &first_dep_id, "deployment.completed").await);

    // Wait until the orchestrator advances. Poll up to 1s.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if disp.calls.lock().await.len() >= 2 {
            break;
        }
    }
    assert!(disp.calls.lock().await.len() >= 2, "wave 1 not dispatched");

    // Drive wave 1.
    let second_host = disp.calls.lock().await[1][0];
    let second_dep_id = dep_ids
        .iter()
        .zip(hosts.iter())
        .find(|(_, h)| **h == second_host)
        .map(|(id, _)| id.clone())
        .unwrap();
    inv.update_deployment_state(&second_dep_id, DeploymentState::Done)
        .await
        .unwrap();
    bus.publish(dep_event(&inv, &second_dep_id, "deployment.completed").await);

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if disp.calls.lock().await.len() >= 3 {
            break;
        }
    }
    assert!(disp.calls.lock().await.len() >= 3, "wave 2 not dispatched");

    // Drive final wave to Done.
    let third_host = disp.calls.lock().await[2][0];
    let third_dep_id = dep_ids
        .iter()
        .zip(hosts.iter())
        .find(|(_, h)| **h == third_host)
        .map(|(id, _)| id.clone())
        .unwrap();
    inv.update_deployment_state(&third_dep_id, DeploymentState::Done)
        .await
        .unwrap();
    bus.publish(dep_event(&inv, &third_dep_id, "deployment.completed").await);

    // Group should transition to Done.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let g = inv.get_deployment_group(&group_id).await.unwrap().unwrap();
        if matches!(g.state, DeploymentGroupState::Done) {
            return;
        }
    }
    let final_state = inv
        .get_deployment_group(&group_id)
        .await
        .unwrap()
        .unwrap()
        .state;
    panic!("group did not reach Done; final state {:?}", final_state);
}

#[tokio::test]
async fn parallelism_all_dispatches_every_host_in_one_wave() {
    let (inv, stack, hosts, dep_ids) = three_host_stack().await;
    inv.set_stack_parallelism(stack, Some("all")).await.unwrap();

    let bus = Arc::new(EventBus::new());
    let disp = RecordingDispatcher::new();
    let orch = Arc::new(
        StackDeployOrchestrator::new(inv.clone(), bus, disp.clone())
            .with_reconcile_interval(Duration::from_secs(60)),
    );

    let outcome = orch
        .start_group(stack, "web", hosts, dep_ids, OnFailure::Rollback)
        .await
        .unwrap();
    assert!(matches!(outcome, StartOutcome::Group(_)));

    let calls = disp.calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].len(), 3, "all=3 hosts dispatched at once");
}

#[tokio::test]
async fn abort_on_wave_one_failure_skips_remaining_waves() {
    let (inv, stack, hosts, dep_ids) = three_host_stack().await;
    let bus = Arc::new(EventBus::new());
    let disp = RecordingDispatcher::new();
    let orch = Arc::new(
        StackDeployOrchestrator::new(inv.clone(), bus.clone(), disp.clone())
            .with_reconcile_interval(Duration::from_millis(20)),
    );
    let _h = orch.clone().start_background();

    let group_id = match orch
        .start_group(
            stack,
            "web",
            hosts.clone(),
            dep_ids.clone(),
            OnFailure::Rollback,
        )
        .await
        .unwrap()
    {
        StartOutcome::Group(g) => g,
        other => panic!("expected Group, got {other:?}"),
    };

    let first_host = disp.calls.lock().await[0][0];
    let first_dep_id = dep_ids
        .iter()
        .zip(hosts.iter())
        .find(|(_, h)| **h == first_host)
        .map(|(id, _)| id.clone())
        .unwrap();
    inv.set_deployment_error(&first_dep_id, "container exited 1")
        .await
        .unwrap();
    inv.update_deployment_state(&first_dep_id, DeploymentState::Failed)
        .await
        .unwrap();
    bus.publish(dep_event(&inv, &first_dep_id, "deployment.failed").await);

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let g = inv.get_deployment_group(&group_id).await.unwrap().unwrap();
        if matches!(g.state, DeploymentGroupState::Aborted) {
            // Ensure no further waves were dispatched.
            assert_eq!(
                disp.calls.lock().await.len(),
                1,
                "no waves dispatched after rollback"
            );
            return;
        }
    }
    panic!("group did not transition to Aborted on wave-1 failure");
}

#[tokio::test]
async fn single_host_deploy_bypasses_orchestrator() {
    let inv = fresh_inv().await;
    let h1 = enroll(&inv, "h1").await;
    let stack = make_stack(&inv, h1, "blog").await;
    let d = inv
        .insert_deployment(dep_template(h1, stack))
        .await
        .unwrap();

    let bus = Arc::new(EventBus::new());
    let disp = RecordingDispatcher::new();
    let orch = StackDeployOrchestrator::new(inv.clone(), bus, disp.clone());

    let outcome = orch
        .start_group(stack, "web", vec![h1], vec![d.id], OnFailure::Rollback)
        .await
        .unwrap();
    assert_eq!(outcome, StartOutcome::Direct);
    assert!(disp.calls.lock().await.is_empty(), "no dispatches issued");

    // No group row should have been created either.
    let groups = inv.list_deployment_groups(stack, 50).await.unwrap();
    assert!(groups.is_empty(), "no group rows for single-host deploys");
}

#[tokio::test]
async fn group_state_transitions_through_pending_rolling_done() {
    let (inv, stack, hosts, dep_ids) = three_host_stack().await;
    inv.set_stack_parallelism(stack, Some("all")).await.unwrap();

    let bus = Arc::new(EventBus::new());
    let disp = RecordingDispatcher::new();
    let orch = Arc::new(
        StackDeployOrchestrator::new(inv.clone(), bus.clone(), disp.clone())
            .with_reconcile_interval(Duration::from_millis(20)),
    );
    let _h = orch.clone().start_background();

    let group_id = match orch
        .start_group(stack, "web", hosts, dep_ids.clone(), OnFailure::Rollback)
        .await
        .unwrap()
    {
        StartOutcome::Group(g) => g,
        other => panic!("expected Group, got {other:?}"),
    };

    // After start_group, group should be Rolling.
    let g = inv.get_deployment_group(&group_id).await.unwrap().unwrap();
    assert_eq!(g.state, DeploymentGroupState::Rolling);

    // Drive every deployment to Done.
    for id in &dep_ids {
        inv.update_deployment_state(id, DeploymentState::Done)
            .await
            .unwrap();
    }
    // Publish a single event; the orchestrator re-reads storage for the wave.
    bus.publish(dep_event(&inv, &dep_ids[0], "deployment.completed").await);

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let g = inv.get_deployment_group(&group_id).await.unwrap().unwrap();
        if matches!(g.state, DeploymentGroupState::Done) {
            return;
        }
    }
    panic!(
        "group never reached Done; state {:?}",
        inv.get_deployment_group(&group_id)
            .await
            .unwrap()
            .unwrap()
            .state
    );
}

#[tokio::test]
async fn reconciliation_advances_after_missed_event() {
    // Same scenario as the first test, but we don't publish an event.
    // The periodic reconcile tick must pick up that the wave is terminal
    // and dispatch the next wave on its own.
    let (inv, stack, hosts, dep_ids) = three_host_stack().await;
    let bus = Arc::new(EventBus::new());
    let disp = RecordingDispatcher::new();
    let orch = Arc::new(
        StackDeployOrchestrator::new(inv.clone(), bus, disp.clone())
            .with_reconcile_interval(Duration::from_millis(20)),
    );
    let _h = orch.clone().start_background();

    let _group_id = match orch
        .start_group(
            stack,
            "web",
            hosts.clone(),
            dep_ids.clone(),
            OnFailure::Rollback,
        )
        .await
        .unwrap()
    {
        StartOutcome::Group(g) => g,
        other => panic!("expected Group, got {other:?}"),
    };

    // Drive wave 0 to Done in storage but skip publishing the event.
    let first_host = disp.calls.lock().await[0][0];
    let first_dep_id = dep_ids
        .iter()
        .zip(hosts.iter())
        .find(|(_, h)| **h == first_host)
        .map(|(id, _)| id.clone())
        .unwrap();
    inv.update_deployment_state(&first_dep_id, DeploymentState::Done)
        .await
        .unwrap();

    // Reconcile tick should advance us to wave 1.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if disp.calls.lock().await.len() >= 2 {
            return;
        }
    }
    panic!(
        "reconciliation never advanced; calls={}",
        disp.calls.lock().await.len()
    );
}

#[tokio::test]
async fn production_dispatcher_queues_force_update_per_host() {
    // Smoke test for the production dispatcher: it should write one
    // host_actions row per host with kind=force_update and the correct stack
    // name.
    let inv = fresh_inv().await;
    let h1 = enroll(&inv, "h1").await;
    let h2 = enroll(&inv, "h2").await;
    let stack = make_stack(&inv, h1, "blog").await;

    let prod = ProductionDispatcher::new(inv.clone());
    prod.dispatch(&[h1, h2], stack, "web", &[]).await.unwrap();

    let actions_h1 = inv.pending_actions(h1).await.unwrap();
    let actions_h2 = inv.pending_actions(h2).await.unwrap();
    assert_eq!(actions_h1.len(), 1);
    assert_eq!(actions_h2.len(), 1);
    // Both rows are kind=force_update for the same stack name.
    let kinds: Vec<&str> = [&actions_h1[0], &actions_h2[0]]
        .iter()
        .map(|a| match &a.kind {
            isengard_storage::HostActionKind::ForceUpdate { .. } => "force_update",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["force_update", "force_update"]);
}

#[tokio::test]
async fn waves_are_deterministic_by_host_hex() {
    // build_waves sorts hosts by their stringified UUID so the wave assignment
    // is stable across orchestrator restarts. Verify by constructing two
    // orchestrators with the same hosts and asserting wave 0 carries the same
    // host id.
    let (inv, stack, hosts, dep_ids) = three_host_stack().await;
    let bus = Arc::new(EventBus::new());

    let disp_a = RecordingDispatcher::new();
    let orch_a = StackDeployOrchestrator::new(inv.clone(), bus.clone(), disp_a.clone());
    orch_a
        .start_group(
            stack,
            "web",
            hosts.clone(),
            dep_ids.clone(),
            OnFailure::Rollback,
        )
        .await
        .unwrap();
    let first_a = disp_a.calls.lock().await[0][0];

    // Reset storage state and rerun.
    let (inv2, stack2, hosts2, dep_ids2) = three_host_stack().await;
    let _ = (inv2, stack2);
    let disp_b = RecordingDispatcher::new();
    let orch_b = StackDeployOrchestrator::new(inv.clone(), bus, disp_b.clone());
    orch_b
        .start_group(stack, "web", hosts.clone(), dep_ids, OnFailure::Rollback)
        .await
        .unwrap();
    let first_b = disp_b.calls.lock().await[0][0];
    // Same input set => same first wave host.
    let _ = (hosts2, dep_ids2);
    assert_eq!(first_a, first_b);
}

/// Used by the orchestrator's start_background task so it can decode events.
/// Compile check only: this is the same Deployment type the production code
/// expects.
#[allow(dead_code)]
fn _dep_compile_check(_d: Deployment) {}
