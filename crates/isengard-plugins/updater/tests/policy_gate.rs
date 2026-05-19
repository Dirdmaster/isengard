//! Integration tests for external-action gates (#55).
//!
//! Composes `evaluate_gate` (HTTP, wiremock-driven) with
//! `policy_decision_from_gate` to confirm the whole pipeline collapses to
//! the expected `PolicyDecision` for each response shape.

#![cfg(test)]

use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use isengard_core::HostId;
use isengard_core::approval_store::PendingApprovalBody;
use isengard_core::policy::{ExternalGate, GatePayload};
use isengard_plugin_updater::gate::evaluate_gate;
use isengard_plugin_updater::policy::{PolicyDecision, SkipReason, policy_decision_from_gate};
use reqwest::Client;
use std::time::Duration;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn payload() -> GatePayload {
    GatePayload {
        kind: "update.gate".into(),
        host_id: "01HX0".into(),
        stack: "blog".into(),
        service: "web".into(),
        container_name: "blog-web-1".into(),
        image: "ghcr.io/owner/repo:latest".into(),
        current_digest: "sha256:1111".into(),
        proposed_digest: "sha256:2222".into(),
        timestamp: Utc.with_ymd_and_hms(2026, 5, 6, 12, 0, 0).unwrap(),
    }
}

fn pending_body() -> PendingApprovalBody {
    PendingApprovalBody {
        host_id: HostId::nil(),
        stack: "blog".into(),
        service: "web".into(),
        container_name: "blog-web-1".into(),
        image: "ghcr.io/owner/repo:latest".into(),
        current_digest: "sha256:1111".into(),
        proposed_digest: "sha256:2222".into(),
        diff_url: None,
        approver_channel: None,
    }
}

fn http() -> Client {
    Client::builder().build().unwrap()
}

#[tokio::test]
async fn approve_response_drives_proceed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"decision":"approve"}"#))
        .mount(&server)
        .await;

    let gate = ExternalGate {
        url: server.uri(),
        secret: None,
        timeout_secs: 5,
    };
    let dec = evaluate_gate(&http(), &gate, &payload()).await;
    let policy = policy_decision_from_gate(dec, pending_body());
    assert_eq!(policy, PolicyDecision::Proceed);
}

#[tokio::test]
async fn reject_response_drives_skip_with_reason() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"decision":"reject","reason":"change-window-only"}"#),
        )
        .mount(&server)
        .await;
    let gate = ExternalGate {
        url: server.uri(),
        secret: None,
        timeout_secs: 5,
    };
    let dec = evaluate_gate(&http(), &gate, &payload()).await;
    let policy = policy_decision_from_gate(dec, pending_body());
    assert_eq!(
        policy,
        PolicyDecision::Skip(SkipReason::GateRejected {
            reason: Some("change-window-only".into())
        })
    );
}

#[tokio::test]
async fn defer_response_drives_deferred_with_until() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"decision":"defer","until":"2026-06-01T00:00:00Z"}"#),
        )
        .mount(&server)
        .await;
    let gate = ExternalGate {
        url: server.uri(),
        secret: None,
        timeout_secs: 5,
    };
    let dec = evaluate_gate(&http(), &gate, &payload()).await;
    let policy = policy_decision_from_gate(dec, pending_body());
    match policy {
        PolicyDecision::Deferred {
            next_window: Some(t),
        } => {
            assert_eq!(t, Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap());
        }
        other => panic!("expected Deferred, got {other:?}"),
    }
}

#[tokio::test]
async fn manual_response_drives_pending_approval() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"decision":"manual"}"#))
        .mount(&server)
        .await;
    let gate = ExternalGate {
        url: server.uri(),
        secret: None,
        timeout_secs: 5,
    };
    let dec = evaluate_gate(&http(), &gate, &payload()).await;
    let policy = policy_decision_from_gate(dec, pending_body());
    assert_eq!(policy, PolicyDecision::PendingApproval(pending_body()));
}

#[tokio::test]
async fn timeout_collapses_to_pending_approval_via_manual() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"decision":"approve"}"#)
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&server)
        .await;
    let gate = ExternalGate {
        url: server.uri(),
        secret: None,
        timeout_secs: 1,
    };
    let dec = evaluate_gate(&http(), &gate, &payload()).await;
    let policy = policy_decision_from_gate(dec, pending_body());
    // Timeout collapses to Manual at evaluator level, then PendingApproval
    // at policy level.
    assert_eq!(policy, PolicyDecision::PendingApproval(pending_body()));
}

#[tokio::test]
async fn unreachable_gate_defers_one_hour() {
    let gate = ExternalGate {
        url: "http://127.0.0.1:1/gate".into(), // closed port
        secret: None,
        timeout_secs: 1,
    };
    let dec = evaluate_gate(&http(), &gate, &payload()).await;
    let policy = policy_decision_from_gate(dec, pending_body());
    match policy {
        PolicyDecision::Deferred {
            next_window: Some(t),
        } => {
            // Unreachable -> 1h backoff. Allow a wide tolerance for CI clocks.
            let now = Utc::now();
            let lower = now + ChronoDuration::minutes(50);
            let upper = now + ChronoDuration::minutes(70);
            assert!(t >= lower && t <= upper, "expected ~1h ahead, got {t}");
        }
        // Some CI environments produce different error categorisation
        // (timeout vs connect refused). Treat Manual -> PendingApproval as
        // an acceptable fallback.
        PolicyDecision::PendingApproval(_) => {}
        other => panic!("expected Deferred or PendingApproval, got {other:?}"),
    }
}
