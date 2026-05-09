//! End-to-end whiteout test through the public `Client` + `BundleBuilder`
//! API.
//!
//! Builds two synthetic layers in a tempdir, serves them from a wiremock
//! `MockServer` posing as a registry, then drives a real `Client::pull`
//! followed by `BundleBuilder::assemble_rootfs`. The first layer creates
//! `etc/passwd`; the second carries an `etc/.wh.passwd` whiteout. After
//! assembly, `etc/passwd` must NOT be present in the rootfs and the
//! whiteout entry itself must NOT be written.
//!
//! Lives under `tests/` (not `src/`) so it runs against the public crate
//! surface only. No registry credentials, no real network.

use std::io::Write;

use sha2::{Digest, Sha256};
use tar::EntryType;
use tempfile::TempDir;
use wiremock::matchers::{method, path as wpath};
use wiremock::{Mock, MockServer, ResponseTemplate};
use wisp_image::registry::manifest::MT_OCI_MANIFEST;
use wisp_image::{BundleBuilder, Client, ConfigOverrides, ImageRef};

const MT_OCI_TAR_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

fn sha256_digest(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    format!("sha256:{}", hex::encode(h.finalize()))
}

/// Build a tar containing the supplied entries and gzip-encode it.
/// Each entry is `(path, content)` for a regular 0o644 file. Paths
/// ending in `/` are written as directories.
fn build_gzipped_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut raw = Vec::new();
    {
        let mut b = tar::Builder::new(&mut raw);
        for (path, content) in entries {
            let mut h = tar::Header::new_gnu();
            if path.ends_with('/') {
                h.set_path(path).unwrap();
                h.set_size(0);
                h.set_mode(0o755);
                h.set_entry_type(EntryType::Directory);
                h.set_cksum();
                b.append(&h, std::io::empty()).unwrap();
            } else {
                h.set_path(path).unwrap();
                h.set_size(content.len() as u64);
                h.set_mode(0o644);
                h.set_entry_type(EntryType::Regular);
                h.set_cksum();
                b.append(&h, *content).unwrap();
            }
        }
        b.finish().unwrap();
    }
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&raw).unwrap();
    gz.finish().unwrap()
}

fn make_config_bytes() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "architecture": "arm64",
        "os": "linux",
        "rootfs": {
            "type": "layers",
            "diff_ids": []
        }
    }))
    .unwrap()
}

fn make_manifest(config: &[u8], layers: &[Vec<u8>]) -> Vec<u8> {
    let layers_json: Vec<_> = layers
        .iter()
        .map(|l| {
            serde_json::json!({
                "mediaType": MT_OCI_TAR_GZIP,
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

#[tokio::test]
async fn pulled_image_assembles_rootfs_without_whitedout_files() {
    let server = MockServer::start().await;

    // Anonymous /v2/ probe.
    Mock::given(method("GET"))
        .and(wpath("/v2/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // Layer 1: writes etc/ + etc/passwd.
    let layer1 = build_gzipped_tar(&[
        ("etc/", b""),
        ("etc/passwd", b"root:x:0:0:root:/root:/bin/sh\n"),
    ]);
    // Layer 2: whiteout for etc/passwd. The whiteout entry's content is
    // ignored by the extractor; only the .wh.<name> path matters.
    let layer2 = build_gzipped_tar(&[("etc/.wh.passwd", b"")]);

    let config = make_config_bytes();
    let manifest = make_manifest(&config, &[layer1.clone(), layer2.clone()]);
    let manifest_digest = sha256_digest(&manifest);
    let config_digest = sha256_digest(&config);
    let layer1_digest = sha256_digest(&layer1);
    let layer2_digest = sha256_digest(&layer2);

    Mock::given(method("GET"))
        .and(wpath("/v2/library/whiteout-test/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest.clone())
                .insert_header("Content-Type", MT_OCI_MANIFEST)
                .insert_header("Docker-Content-Digest", manifest_digest.as_str()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(wpath(format!(
            "/v2/library/whiteout-test/blobs/{config_digest}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(config.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(wpath(format!(
            "/v2/library/whiteout-test/blobs/{layer1_digest}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(layer1.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(wpath(format!(
            "/v2/library/whiteout-test/blobs/{layer2_digest}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(layer2.clone()))
        .mount(&server)
        .await;

    let store_tmp = TempDir::new().unwrap();
    let bundle_tmp = TempDir::new().unwrap();
    let store_path = store_tmp.path().to_path_buf();
    let bundle_dir = bundle_tmp.path().join("whiteout-bundle");
    let endpoint = server.uri();

    let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let client = Client::new(&store_path)
            .map_err(|e| e.to_string())?
            .with_endpoint("docker.io", &endpoint);
        let r: ImageRef = "library/whiteout-test:v1"
            .parse()
            .map_err(|e: wisp_image::WispImageError| e.to_string())?;
        let pulled = client.pull(&r).map_err(|e| e.to_string())?;
        let builder = BundleBuilder::new(&pulled, client.store(), &bundle_dir);
        builder.assemble_rootfs().map_err(|e| e.to_string())?;
        builder
            .write_config(ConfigOverrides {
                args: Some(vec!["/bin/sh".to_string()]),
                ..Default::default()
            })
            .map_err(|e| e.to_string())?;

        let rootfs = bundle_dir.join("rootfs");
        // The base file from layer 1 must be gone after layer 2's whiteout.
        if rootfs.join("etc/passwd").exists() {
            return Err("etc/passwd should have been whited out".to_string());
        }
        // The whiteout entry itself must not have been written.
        if rootfs.join("etc/.wh.passwd").exists() {
            return Err("whiteout entry leaked into rootfs".to_string());
        }
        // The parent directory should still exist (only the file was whited out).
        if !rootfs.join("etc").is_dir() {
            return Err("etc dir should survive a per-file whiteout".to_string());
        }
        Ok(())
    })
    .await
    .expect("join");

    result.expect("whiteout pipeline");
}
