//! Integration tests for the compose auto-adoption pass.
//!
//! Exercises [`run_auto_adoption_pass`] against a real in-memory
//! storage so the trifecta (lookup_stack_id_by_name, get_stack_compose
//! short-circuit, tracker observe → synthesize → write) is wired
//! correctly.
//!
//! The rich-data lookup and synthesis closures are test doubles
//! because the parallel agent + synthesizer PRs land alongside this
//! one. The closure signatures the tests use here are the exact
//! shape the heartbeat handler in `service.rs` will use once those
//! PRs merge.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use isengard_controller::compose_autoadopt::{
    AutoAdoptionTracker, Decision, run_auto_adoption_pass,
};
use isengard_storage::host::HostId;
use isengard_storage::{ComposeSource, EnrollHost, InsertStack, Inventory, StackSource};

async fn fixture() -> (Arc<Inventory>, HostId) {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp-iso-1".into(),
            hostname: "iso-1".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.6.0".into(),
            docker_version: "27.0".into(),
        })
        .await
        .unwrap();
    (inv, host_id)
}

fn ids(slice: &[&str]) -> Vec<String> {
    slice.iter().map(|s| (*s).to_string()).collect()
}

/// Walks the example sequence from the spec: 3 services arriving
/// across 4 heartbeats, with synthesis firing on heartbeat #4 (the
/// second-consecutive-stable read). Asserts the stored compose row
/// is tagged `auto_synthesized` after the fire.
#[tokio::test]
async fn three_services_four_heartbeats_fires_synthesis_and_writes_tagged_row() {
    let (inv, host_id) = fixture().await;
    inv.insert_stack(InsertStack {
        host_id,
        name: "servarr".into(),
        source: StackSource::Compose,
    })
    .await
    .unwrap();

    let tracker = AutoAdoptionTracker::new();

    // Test double: rich-data lookup. Mirror the input list so the
    // rich-data gate passes (every container ID has rich data).
    let rich_lookup = |slice: &[String]| {
        let v: Vec<String> = slice.to_vec();
        async move { v }
    };

    // Test double: synthesizer + writer. Records the call count and
    // writes a sentinel compose body so the storage round-trip can
    // be verified.
    let synth_calls = Arc::new(AtomicUsize::new(0));
    let synth_calls_clone = synth_calls.clone();
    let inv_for_writer = inv.clone();
    let host_id_for_writer = host_id;
    let synth_and_write = move |stack_id, stack_name: String, rich_ids: Vec<String>| {
        let counter = synth_calls_clone.clone();
        let inv = inv_for_writer.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            let yaml = format!(
                "# auto-synthesized for {stack_name} ({n} services)\nservices:\n",
                n = rich_ids.len(),
            );
            let written = inv
                .set_stack_compose(
                    host_id_for_writer,
                    &stack_name,
                    &yaml,
                    "auto-fixture-sha",
                    "2026-05-23T12:00:00Z",
                    ComposeSource::AutoSynthesized,
                )
                .await
                .map_err(|e| e.to_string())?;
            assert!(written, "set_stack_compose returned false");
            let _ = stack_id;
            Ok::<_, String>(())
        }
    };

    // HB1: only `web` running.
    let hb1 = vec![("servarr".to_string(), ids(&["web"]))];
    let d1 = run_auto_adoption_pass(
        &tracker,
        &inv,
        host_id,
        &hb1,
        rich_lookup,
        synth_and_write.clone(),
    )
    .await;
    assert_eq!(d1, vec![("servarr".into(), Decision::NewlyTracked)]);
    assert_eq!(synth_calls.load(Ordering::SeqCst), 0);

    // HB2: web + worker.
    let hb2 = vec![("servarr".to_string(), ids(&["web", "worker"]))];
    let d2 = run_auto_adoption_pass(
        &tracker,
        &inv,
        host_id,
        &hb2,
        rich_lookup,
        synth_and_write.clone(),
    )
    .await;
    assert_eq!(d2, vec![("servarr".into(), Decision::SetChanged)]);
    assert_eq!(synth_calls.load(Ordering::SeqCst), 0);

    // HB3: web + worker + cache (first stable observation).
    let hb3 = vec![("servarr".to_string(), ids(&["web", "worker", "cache"]))];
    let d3 = run_auto_adoption_pass(
        &tracker,
        &inv,
        host_id,
        &hb3,
        rich_lookup,
        synth_and_write.clone(),
    )
    .await;
    assert_eq!(d3, vec![("servarr".into(), Decision::SetChanged)]);
    assert_eq!(synth_calls.load(Ordering::SeqCst), 0);

    // HB4: same set as HB3 -> stable for 2 -> synthesize.
    let hb4 = vec![("servarr".to_string(), ids(&["web", "worker", "cache"]))];
    let d4 = run_auto_adoption_pass(
        &tracker,
        &inv,
        host_id,
        &hb4,
        rich_lookup,
        synth_and_write.clone(),
    )
    .await;
    assert_eq!(d4, vec![("servarr".into(), Decision::Synthesize)]);
    assert_eq!(synth_calls.load(Ordering::SeqCst), 1);

    // Storage row is now populated and tagged correctly.
    let stack_id = inv
        .list_stacks(Some(host_id))
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.name == "servarr")
        .unwrap()
        .id;
    let row = inv.get_stack_compose(stack_id).await.unwrap().unwrap();
    assert_eq!(row.source, ComposeSource::AutoSynthesized);
    assert!(row.yaml.contains("auto-synthesized for servarr"));
    assert_eq!(row.sha256, "auto-fixture-sha");

    // HB5: same set again. The stored compose now exists, so
    // run_auto_adoption_pass's pre-tracker check sees
    // has_stored_compose=true and short-circuits with
    // AlreadyHasCompose (the in-memory tracker's `Adopted` stamp is
    // never reached because the storage check fires first). Either
    // way the synthesizer is not called again.
    let hb5 = vec![("servarr".to_string(), ids(&["web", "worker", "cache"]))];
    let d5 = run_auto_adoption_pass(
        &tracker,
        &inv,
        host_id,
        &hb5,
        rich_lookup,
        synth_and_write.clone(),
    )
    .await;
    assert_eq!(d5, vec![("servarr".into(), Decision::AlreadyHasCompose)]);
    assert_eq!(synth_calls.load(Ordering::SeqCst), 1);
}

/// A stack that already has stored compose (operator-owned) is
/// never re-adopted, even on a perfectly stable heartbeat sequence.
/// The tracker stamps `Adopted` on the first observation so even
/// later heartbeats short-circuit.
#[tokio::test]
async fn operator_written_stack_is_never_auto_adopted() {
    let (inv, host_id) = fixture().await;
    inv.insert_stack(InsertStack {
        host_id,
        name: "owned".into(),
        source: StackSource::Compose,
    })
    .await
    .unwrap();

    inv.set_stack_compose(
        host_id,
        "owned",
        "services:\n  web:\n    image: nginx\n",
        "op-sha",
        "2026-05-23T00:00:00Z",
        ComposeSource::OperatorWritten,
    )
    .await
    .unwrap();

    let tracker = AutoAdoptionTracker::new();
    let rich_lookup = |s: &[String]| {
        let v: Vec<String> = s.to_vec();
        async move { v }
    };

    let synth_calls = Arc::new(AtomicUsize::new(0));
    let synth_calls_clone = synth_calls.clone();
    let synth_and_write = move |_, _: String, _: Vec<String>| {
        let counter = synth_calls_clone.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(())
        }
    };

    let hb = vec![("owned".to_string(), ids(&["c1"]))];
    for _ in 0..4 {
        let _ = run_auto_adoption_pass(
            &tracker,
            &inv,
            host_id,
            &hb,
            rich_lookup,
            synth_and_write.clone(),
        )
        .await;
    }
    assert_eq!(
        synth_calls.load(Ordering::SeqCst),
        0,
        "operator-written stack must not trigger synthesis",
    );

    let stack_id = inv
        .list_stacks(Some(host_id))
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.name == "owned")
        .unwrap()
        .id;
    let row = inv.get_stack_compose(stack_id).await.unwrap().unwrap();
    assert_eq!(row.source, ComposeSource::OperatorWritten);
    assert_eq!(row.sha256, "op-sha");
}

/// Rich-data gate: when the rich-data lookup returns a shorter list
/// than the container set, no observation advances and the
/// synthesizer is never called. The stack remains unstored.
#[tokio::test]
async fn missing_rich_data_blocks_synthesis_indefinitely() {
    let (inv, host_id) = fixture().await;
    inv.insert_stack(InsertStack {
        host_id,
        name: "incomplete".into(),
        source: StackSource::Compose,
    })
    .await
    .unwrap();

    let tracker = AutoAdoptionTracker::new();
    // Test double: rich lookup returns nothing, simulating an older
    // agent that doesn't ship the rich snapshot fields.
    let rich_lookup = |_s: &[String]| async { Vec::<String>::new() };
    let synth_calls = Arc::new(AtomicUsize::new(0));
    let synth_calls_clone = synth_calls.clone();
    let synth_and_write = move |_, _: String, _: Vec<String>| {
        let counter = synth_calls_clone.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(())
        }
    };

    let hb = vec![("incomplete".to_string(), ids(&["c1", "c2"]))];
    for _ in 0..5 {
        let decisions = run_auto_adoption_pass(
            &tracker,
            &inv,
            host_id,
            &hb,
            rich_lookup,
            synth_and_write.clone(),
        )
        .await;
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].1, Decision::MissingRichData);
    }
    assert_eq!(synth_calls.load(Ordering::SeqCst), 0);

    // No stored compose was written.
    let stack_id = inv
        .list_stacks(Some(host_id))
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.name == "incomplete")
        .unwrap()
        .id;
    assert!(inv.get_stack_compose(stack_id).await.unwrap().is_none());
}

/// Stack rows the agent reports that aren't yet in the inventory are
/// silently skipped (the sync_stacks pass that inserts them runs
/// just before the auto-adopt pass; a missed insert would log a
/// warning but the auto-adopt pass must not panic).
#[tokio::test]
async fn unknown_stack_is_skipped_without_error() {
    let (inv, host_id) = fixture().await;
    let tracker = AutoAdoptionTracker::new();
    let rich_lookup = |s: &[String]| {
        let v: Vec<String> = s.to_vec();
        async move { v }
    };
    let synth_calls = Arc::new(AtomicUsize::new(0));
    let synth_calls_clone = synth_calls.clone();
    let synth_and_write = move |_, _: String, _: Vec<String>| {
        let counter = synth_calls_clone.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(())
        }
    };
    let hb = vec![("ghost".to_string(), ids(&["c1"]))];
    let decisions =
        run_auto_adoption_pass(&tracker, &inv, host_id, &hb, rich_lookup, synth_and_write).await;
    assert!(
        decisions.is_empty(),
        "expected zero per-stack decisions, got {decisions:?}"
    );
    assert_eq!(synth_calls.load(Ordering::SeqCst), 0);
}
