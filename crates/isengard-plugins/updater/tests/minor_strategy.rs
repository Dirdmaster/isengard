//! End-to-end: `Minor` strategy reaches into the registry's
//! tags-list, picks the highest patch+minor on the same major, and the
//! cycle's bump path produces the bumped image_ref + digest pair that the
//! recreate path consumes.
//!
//! This is the registry-facing seam of `maybe_minor_bump`. We don't drive
//! the full `do_cycle` (that requires bollard + a real Docker daemon, and
//! the existing `cycle_e2e.rs` test already owns that surface). Instead we
//! exercise the constituent pieces end to end via `wiremock`:
//!
//! 1. `RegistryClient::list_tags_with_scheme` returns all advertised tags.
//! 2. `tag_cache::pick_highest_minor` picks the highest qualifying tag.
//! 3. `RegistryClient::head_digest_with_scheme` for the bumped tag returns
//!    a sha256 digest header.
//!
//! The result is what the cycle's `maybe_minor_bump` builds and threads
//! into `image_ref`/`image_str`/`remote_digest`. The override -> recreate
//! pipeline downstream is structurally tested by the existing
//! `policy_approval.rs` / `cycle_e2e.rs` cases (they do not gate on the
//! tag string the cycle picked).

#![allow(clippy::result_large_err)]

use isengard_plugin_updater::{
    auth::DockerConfig,
    image_ref::ImageRef,
    registry::RegistryClient,
    tag_cache::{TagCache, parse_tag, pick_highest_minor},
};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn imageref_for(host: &str, tag: &str) -> ImageRef {
    ImageRef {
        registry: host.to_string(),
        repository: "foo/bar".into(),
        tag: tag.to_string(),
    }
}

#[tokio::test]
async fn minor_strategy_picks_higher_minor_and_resolves_digest() {
    let server = MockServer::start().await;

    // Registry advertises 1.2.3, 1.2.4, 1.3.0, 2.0.0. The running
    // container is at 1.2.3, so the bump should land on 1.3.0.
    Mock::given(method("GET"))
        .and(path("/v2/foo/bar/tags/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "foo/bar",
            "tags": ["1.2.3", "1.2.4", "1.3.0", "2.0.0"],
        })))
        .mount(&server)
        .await;

    // HEAD on the bumped tag returns a different digest than the original.
    Mock::given(method("HEAD"))
        .and(path("/v2/foo/bar/manifests/1.3.0"))
        .respond_with(ResponseTemplate::new(200).insert_header(
            "docker-content-digest",
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ))
        .mount(&server)
        .await;

    let client = RegistryClient::new(DockerConfig::default()).unwrap();
    let host = server.address().to_string();
    let current = imageref_for(&host, "1.2.3");

    // 1. List tags via the cache, then 2. pick the highest minor.
    let cache = Arc::new(TagCache::new(Duration::from_secs(60)));
    let tags = cache
        .get_or_fetch(&current, || async {
            client.list_tags_with_scheme(&current, "http").await
        })
        .await
        .expect("list_tags");

    let current_v = parse_tag(&current.tag).expect("parse current");
    let bumped = pick_highest_minor(&tags, &current_v).expect("bump exists");
    assert_eq!(bumped.to_string(), "1.3.0");

    // 3. HEAD the bumped tag.
    let bumped_ref = current.with_tag(bumped.to_string());
    let bumped_digest = client
        .head_digest_with_scheme(&bumped_ref, "http")
        .await
        .expect("HEAD bumped tag")
        .expect("digest present");
    assert!(bumped_digest.starts_with("sha256:"));
    assert_ne!(
        bumped_digest, "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bumped digest must differ from any prior placeholder"
    );

    // The cache memoizes: a second list_tags call must not hit the
    // registry (the wiremock above didn't set `expect`, so we assert
    // structurally by checking that the second call returns the same
    // Arc-shared body).
    let tags2 = cache
        .get_or_fetch(&current, || async {
            // If this branch runs, the cache is broken.
            panic!("cache miss on the second call within TTL");
        })
        .await
        .expect("second list_tags");
    assert_eq!(tags2.as_slice(), tags.as_slice());
}

/// When the running tag has no higher minor on the same major (only a
/// major bump exists in the registry), `pick_highest_minor` returns
/// `None` and the cycle falls back to the `TagOnly` digest-only path.
#[tokio::test]
async fn minor_strategy_no_bump_when_only_major_higher_exists() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/foo/bar/tags/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "foo/bar",
            "tags": ["1.2.3", "2.0.0", "2.1.0"],
        })))
        .mount(&server)
        .await;

    let client = RegistryClient::new(DockerConfig::default()).unwrap();
    let host = server.address().to_string();
    let current = imageref_for(&host, "1.2.3");
    let cache = Arc::new(TagCache::new(Duration::from_secs(60)));
    let tags = cache
        .get_or_fetch(&current, || async {
            client.list_tags_with_scheme(&current, "http").await
        })
        .await
        .expect("list_tags");
    let current_v = parse_tag(&current.tag).expect("parse current");
    assert!(pick_highest_minor(&tags, &current_v).is_none());
}
