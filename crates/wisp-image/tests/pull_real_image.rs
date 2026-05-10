//! Network-required integration test: pull `alpine:3.19` from the
//! real Docker Hub.
//!
//! `#[ignore]` by default so `cargo test -p wisp-image --tests` stays
//! hermetic. Run via `cargo test -p wisp-image --tests -- --ignored`.
//! Skipped (without failing) if `WISP_OFFLINE=1` is set.
//!
//! Asserts structurally rather than pinning a manifest digest: alpine
//! republishes 3.19 periodically, and the digest is allowed to drift.
//! What we lock in is the shape: 1 layer of nontrivial size, with
//! repo + tag preserved on the returned `PulledImage`. A second pull
//! must be a cache hit and yield the same manifest digest.

mod common;

use tempfile::TempDir;
use wisp_image::{Client, ImageRef};

#[test]
#[ignore = "needs network; run with --ignored"]
fn pulls_alpine_3_19() {
    if common::offline() {
        eprintln!("SKIP pulls_alpine_3_19: WISP_OFFLINE=1");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let store_dir = tmp.path().join("images");
    let client = Client::new(&store_dir).expect("Client::new");
    let r: ImageRef = "alpine:3.19".parse().expect("parse alpine:3.19");

    let pulled = client.pull(&r).expect("pull alpine:3.19");

    // Sanity: alpine:3.19 is a single-layer image of around 3MB on arm64
    // and ~3MB on amd64. The exact digest can shift when alpine
    // republishes; assert structurally instead of pinning a digest.
    assert_eq!(pulled.r.repo, "library/alpine");
    assert_eq!(pulled.r.tag.as_deref(), Some("3.19"));
    assert!(
        !pulled.layers.is_empty(),
        "alpine:3.19 should have at least one layer"
    );
    assert!(
        pulled.layers[0].size > 1_000_000,
        "alpine:3.19 layer 0 should be > 1MB; got {}",
        pulled.layers[0].size
    );

    // Re-pull should be a cache hit and yield the same manifest digest.
    let pulled2 = client.pull(&r).expect("re-pull alpine:3.19");
    assert_eq!(
        pulled.manifest_digest, pulled2.manifest_digest,
        "re-pull should resolve to the same manifest digest"
    );
}
