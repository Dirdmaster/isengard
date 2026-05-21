//! `RoutingPusher.build_for_host` pulls `acme.contact_email` from the
//! configure dispatcher. Replaces the silent `String::new()` fallback
//! that PR 4 of the `isd configure` track retires.
//!
//! Three paths to exercise: dispatcher absent (back-compat for tests
//! that construct a bare pusher), dispatcher attached with the key
//! unset (still empty), and dispatcher attached with a value set
//! (echoed verbatim into the proto `ProxySettings`).

use std::sync::Arc;

use isengard_controller::ControllerHandles;
use isengard_controller::config::{ConfigDispatcher, Schema};
use isengard_controller::routing::RoutingPusher;
use isengard_controller::secrets::SecretsStore;
use isengard_storage::{EnrollHost, Inventory, SettingStore};
use serde_json::json;
use tempfile::tempdir;

fn fixed_master_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = i as u8;
    }
    k
}

async fn dispatcher_for(inv: Arc<Inventory>) -> Arc<ConfigDispatcher> {
    let secrets = Arc::new(SecretsStore::new(inv.clone(), fixed_master_key()));
    let settings = Arc::new(SettingStore::new(inv));
    Arc::new(ConfigDispatcher::new(Schema::v01(), secrets, settings))
}

async fn enroll_host(inv: &Inventory) -> isengard_storage::HostId {
    inv.enroll_host(EnrollHost {
        fingerprint: "fp".into(),
        hostname: "h".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0".into(),
        docker_version: "27".into(),
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn build_for_host_without_dispatcher_keeps_empty_email() {
    let dir = tempdir().unwrap();
    let inv = Arc::new(
        Inventory::open(&dir.path().join("isengard.db"))
            .await
            .unwrap(),
    );
    let host = enroll_host(&inv).await;

    let pusher = RoutingPusher::new(inv.clone());
    let cfg = pusher.build_for_host(host).await.unwrap();
    let settings = cfg.settings.expect("ProxyConfig.settings present");
    assert_eq!(
        settings.acme_contact_email, "",
        "without a dispatcher the pusher must fall back to the historical empty contact email"
    );
}

#[tokio::test]
async fn build_for_host_with_unset_email_returns_empty() {
    let dir = tempdir().unwrap();
    let inv = Arc::new(
        Inventory::open(&dir.path().join("isengard.db"))
            .await
            .unwrap(),
    );
    let host = enroll_host(&inv).await;

    let dispatcher = dispatcher_for(inv.clone()).await;
    let pusher = RoutingPusher::new(inv.clone()).with_config_dispatcher(dispatcher);
    let cfg = pusher.build_for_host(host).await.unwrap();
    let settings = cfg.settings.expect("ProxyConfig.settings present");
    assert_eq!(
        settings.acme_contact_email, "",
        "acme.contact_email has no schema default; unset must yield empty string"
    );
}

#[tokio::test]
async fn build_for_host_uses_dispatcher_acme_contact_email() {
    let dir = tempdir().unwrap();
    let inv = Arc::new(
        Inventory::open(&dir.path().join("isengard.db"))
            .await
            .unwrap(),
    );
    let host = enroll_host(&inv).await;

    let dispatcher = dispatcher_for(inv.clone()).await;
    dispatcher
        .put(
            "acme.contact_email",
            json!("ops@weavers.engineering"),
            Some("test"),
        )
        .await
        .expect("dispatcher accepts acme.contact_email");

    let pusher = RoutingPusher::new(inv.clone()).with_config_dispatcher(dispatcher);
    let cfg = pusher.build_for_host(host).await.unwrap();
    let settings = cfg.settings.expect("ProxyConfig.settings present");
    assert_eq!(settings.acme_contact_email, "ops@weavers.engineering");
}

#[tokio::test]
async fn test_config_dispatcher_helper_round_trips_through_handles() {
    // Belt-and-braces: confirm the `ControllerHandles::test_config_dispatcher`
    // helper still produces a dispatcher the pusher accepts, so other
    // test harnesses can borrow this wiring.
    let dir = tempdir().unwrap();
    let inv = Arc::new(
        Inventory::open(&dir.path().join("isengard.db"))
            .await
            .unwrap(),
    );
    let secrets = Arc::new(SecretsStore::new(inv.clone(), fixed_master_key()));
    let dispatcher = ControllerHandles::test_config_dispatcher(inv.clone(), secrets);
    dispatcher
        .put("acme.contact_email", json!("ops@example.test"), None)
        .await
        .unwrap();

    let host = enroll_host(&inv).await;
    let pusher = RoutingPusher::new(inv.clone()).with_config_dispatcher(dispatcher);
    let cfg = pusher.build_for_host(host).await.unwrap();
    assert_eq!(cfg.settings.unwrap().acme_contact_email, "ops@example.test");
}
