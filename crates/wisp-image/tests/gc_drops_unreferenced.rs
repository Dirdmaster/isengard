//! Hermetic GC test: two images sharing two of three layers.
//!
//! Pre-populates a `ContentStore` directly via `write_blob` + `put_tag`
//! (no registry needed). Image A has layers `{L1, L2, Lshared}`; Image B
//! has layers `{L1, L2, B-only}`. After dropping image B's tag and
//! running `Client::gc`, the test asserts:
//!
//!   * `L1` and `L2` survive (still referenced by image A's manifest +
//!     an active bundle's `add_ref`).
//!   * `B-only` is gone.
//!   * Image A's manifest blob survives (referenced via the index).
//!   * Image B's manifest blob is gone.

use std::fs;

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use wisp_image::Client;
use wisp_image::registry::manifest::MT_OCI_MANIFEST;
use wisp_image::store::ContentStore;
use wisp_image::store::layout;

fn sha256_digest(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    format!("sha256:{}", hex::encode(h.finalize()))
}

fn make_config_bytes(label: &str) -> Vec<u8> {
    // Distinct labels keep the two images' config digests disjoint so a
    // bundle holding image A's config-as-layer doesn't bleed into B's
    // refcount. (We don't actually treat configs as layers in this test;
    // the label just keeps us from accidentally sharing a digest across
    // images and obscuring the GC contract.)
    serde_json::to_vec(&serde_json::json!({
        "architecture": "arm64",
        "os": "linux",
        "rootfs": { "type": "layers", "diff_ids": [] },
        "comment": label,
    }))
    .unwrap()
}

fn make_manifest(config: &[u8], layer_blobs: &[&[u8]]) -> Vec<u8> {
    let layers_json: Vec<_> = layer_blobs
        .iter()
        .map(|l| {
            serde_json::json!({
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": sha256_digest(l),
                "size": l.len(),
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MT_OCI_MANIFEST,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": sha256_digest(config),
            "size": config.len(),
        },
        "layers": layers_json,
    }))
    .unwrap()
}

#[test]
fn gc_drops_only_unique_layer_of_removed_image() {
    let store_tmp = TempDir::new().unwrap();
    let store = ContentStore::new(store_tmp.path()).unwrap();

    // Three layer payloads. Bytes are arbitrary; the store treats them
    // as opaque content-addressable blobs.
    let l1: &[u8] = b"layer-shared-1";
    let l2: &[u8] = b"layer-shared-2";
    let l_only_b: &[u8] = b"layer-only-in-b";

    let config_a = make_config_bytes("a");
    let config_b = make_config_bytes("b");

    // Image A: L1 + L2 (shared with B; survives).
    let manifest_a = make_manifest(&config_a, &[l1, l2]);
    // Image B: L1 + L2 + B-only.
    let manifest_b = make_manifest(&config_b, &[l1, l2, l_only_b]);

    // Persist all blobs. Image-config blobs are not refcount-tracked in
    // 0.2 (no bundle "uses" the config after assembly), so we don't
    // assert their fate here; the test only asserts the layer + manifest
    // contract.
    let l1_d = store.write_blob(l1).unwrap();
    let l2_d = store.write_blob(l2).unwrap();
    let l_only_b_d = store.write_blob(l_only_b).unwrap();
    let _config_a_d = store.write_blob(&config_a).unwrap();
    let _config_b_d = store.write_blob(&config_b).unwrap();
    let manifest_a_d = store.write_blob(&manifest_a).unwrap();
    let manifest_b_d = store.write_blob(&manifest_b).unwrap();

    // Tag each manifest digest as a separate image.
    store
        .put_tag("docker.io", "library/img-a", "v1", &manifest_a_d)
        .unwrap();
    store
        .put_tag("docker.io", "library/img-b", "v1", &manifest_b_d)
        .unwrap();

    // Active bundle keeps image A's layers referenced via add_ref. This
    // covers the "running bundle pins layers" path that the spec mentions.
    store
        .add_ref("bundle-running-a", &[l1_d.clone(), l2_d.clone()])
        .unwrap();

    let client = Client::new(store_tmp.path()).unwrap();

    // Sanity: list returns both images.
    let listed = client.list().unwrap();
    assert_eq!(
        listed.len(),
        2,
        "expected both images listed before drop, got {listed:?}"
    );

    // Drop image B by removing its tag file. (No public "untag" API in
    // 0.2; this is the same effect the future `wisp image rm` will have.)
    let tag_path = layout::tag_path(store_tmp.path(), "docker.io", "library/img-b", "v1");
    assert!(tag_path.exists(), "tag pointer should exist");
    fs::remove_file(&tag_path).unwrap();

    // After untagging, only image A is listed.
    let listed = client.list().unwrap();
    assert_eq!(listed.len(), 1, "expected only image A after drop");
    assert_eq!(listed[0].r.repo, "library/img-a");

    // Run GC.
    let report = client.gc().unwrap();

    // Shared layers survive (image A's manifest still points at them +
    // bundle-running-a holds a ref).
    assert!(store.has_blob(&l1_d), "L1 should survive (referenced by A)");
    assert!(store.has_blob(&l2_d), "L2 should survive (referenced by A)");
    // B's unique layer is gone.
    assert!(!store.has_blob(&l_only_b_d), "B-only layer must be removed");
    // Image A's manifest blob is referenced by the index and survives.
    assert!(
        store.has_blob(&manifest_a_d),
        "manifest A should survive (still in index)"
    );
    // Image B's manifest is no longer referenced (we removed the tag) and
    // also isn't held by a bundle, so it must be removed.
    assert!(
        !store.has_blob(&manifest_b_d),
        "manifest B should be removed"
    );

    // Cross-check the report: removed contains B's manifest + B's
    // unique layer (it may also contain orphaned config blobs, which is
    // expected and a separately-tracked limitation; we only assert the
    // layer + manifest contract here).
    assert!(
        report.removed.contains(&l_only_b_d),
        "B-only layer should be in gc.removed: {:?}",
        report.removed
    );
    assert!(
        report.removed.contains(&manifest_b_d),
        "manifest B should be in gc.removed: {:?}",
        report.removed
    );
    assert!(
        !report.removed.contains(&l1_d),
        "L1 must NOT be in gc.removed: {:?}",
        report.removed
    );
    assert!(
        !report.removed.contains(&l2_d),
        "L2 must NOT be in gc.removed: {:?}",
        report.removed
    );
    assert!(
        !report.removed.contains(&manifest_a_d),
        "manifest A must NOT be in gc.removed: {:?}",
        report.removed
    );
}
