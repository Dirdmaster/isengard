//! Phase 12b T2 acceptance tests for the container_hooks DAO.

use isengard_storage::{ContainerHooks, EnrollHost, HostId, Inventory, UpsertContainerHooks};

async fn fresh() -> (Inventory, HostId) {
    let inv = Inventory::open_in_memory().await.expect("open");
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
        .expect("enroll");
    (inv, host)
}

fn upsert(host: HostId, name: &str, pre: Option<&str>, post: Option<&str>) -> UpsertContainerHooks {
    UpsertContainerHooks {
        host_id: host,
        container_id: format!("cid-{name}"),
        container_name: name.to_string(),
        pre_deploy_url: pre.map(str::to_string),
        post_deploy_url: post.map(str::to_string),
        on_failure_url: None,
        secret: None,
    }
}

#[tokio::test]
async fn insert_then_get_round_trips_fields() {
    let (inv, host) = fresh().await;
    let row: ContainerHooks = inv
        .upsert_container_hooks(upsert(
            host,
            "web",
            Some("https://h.example/pre"),
            Some("https://h.example/post"),
        ))
        .await
        .expect("upsert");
    assert_eq!(row.container_name, "web");
    assert_eq!(row.pre_deploy_url.as_deref(), Some("https://h.example/pre"));
    assert_eq!(
        row.post_deploy_url.as_deref(),
        Some("https://h.example/post")
    );
    assert!(row.on_failure_url.is_none());

    let got = inv
        .get_container_hooks(host, "web")
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got, row);
}

#[tokio::test]
async fn upsert_overwrites_existing_row() {
    let (inv, host) = fresh().await;
    inv.upsert_container_hooks(upsert(host, "web", Some("https://old"), None))
        .await
        .unwrap();

    let updated = inv
        .upsert_container_hooks(upsert(
            host,
            "web",
            Some("https://new"),
            Some("https://post"),
        ))
        .await
        .unwrap();
    assert_eq!(updated.pre_deploy_url.as_deref(), Some("https://new"));
    assert_eq!(updated.post_deploy_url.as_deref(), Some("https://post"));

    let all = inv.list_container_hooks_by_host(host).await.unwrap();
    assert_eq!(all.len(), 1);
}

#[tokio::test]
async fn delete_by_name_returns_true_when_row_present() {
    let (inv, host) = fresh().await;
    inv.upsert_container_hooks(upsert(host, "web", Some("https://h"), None))
        .await
        .unwrap();
    let deleted = inv
        .delete_container_hooks_by_name(host, "web")
        .await
        .unwrap();
    assert!(deleted);
    assert!(
        inv.get_container_hooks(host, "web")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn delete_by_name_returns_false_when_missing() {
    let (inv, host) = fresh().await;
    let deleted = inv
        .delete_container_hooks_by_name(host, "ghost")
        .await
        .unwrap();
    assert!(!deleted);
}

#[tokio::test]
async fn delete_by_container_id_uses_id_match() {
    let (inv, host) = fresh().await;
    inv.upsert_container_hooks(upsert(host, "web", Some("https://h"), None))
        .await
        .unwrap();
    let deleted = inv
        .delete_container_hooks_by_id(host, "cid-web")
        .await
        .unwrap();
    assert!(deleted);
}

#[tokio::test]
async fn list_by_host_returns_alphabetical() {
    let (inv, host) = fresh().await;
    inv.upsert_container_hooks(upsert(host, "zeta", Some("https://z"), None))
        .await
        .unwrap();
    inv.upsert_container_hooks(upsert(host, "alpha", Some("https://a"), None))
        .await
        .unwrap();
    let rows = inv.list_container_hooks_by_host(host).await.unwrap();
    let names: Vec<_> = rows.iter().map(|r| r.container_name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "zeta"]);
}

#[tokio::test]
async fn upsert_with_no_urls_has_any_url_returns_false() {
    let no_urls = UpsertContainerHooks {
        host_id: HostId::from_bytes([0u8; 16]),
        container_id: "cid".into(),
        container_name: "name".into(),
        pre_deploy_url: None,
        post_deploy_url: None,
        on_failure_url: None,
        secret: None,
    };
    assert!(!no_urls.has_any_url());

    let with_pre = UpsertContainerHooks {
        pre_deploy_url: Some("https://h".into()),
        ..no_urls
    };
    assert!(with_pre.has_any_url());
}
