//! Phase 11a: S3Destination tests using a wiremock S3-compatible endpoint.
//!
//! We don't aim to validate the SigV4 signature in tests (mock servers don't
//! verify it). Instead we verify request shape: PUT/GET/DELETE land on the
//! expected paths with the headers SigV4 always sets, and LIST parses a
//! representative ListObjectsV2 response.

use isengard_plugin_backup::destination::{BackupDestination, S3Config, S3Destination};
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cfg(endpoint: &str) -> S3Config {
    S3Config {
        endpoint: endpoint.to_string(),
        region: "auto".to_string(),
        bucket: "isengard-backups".to_string(),
        prefix: "controllers/prod".to_string(),
        access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
    }
}

#[tokio::test]
async fn upload_puts_to_signed_url_with_amz_headers() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/isengard-backups/controllers/prod/snap-a.db.age"))
        .and(header_exists("authorization"))
        .and(header_exists("x-amz-content-sha256"))
        .and(header_exists("x-amz-date"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let dest = S3Destination::new(cfg(&server.uri()));
    dest.upload("snap-a.db.age", b"contents").await.unwrap();
}

#[tokio::test]
async fn download_gets_object_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/isengard-backups/controllers/prod/snap-b.db.age"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"the bytes".as_slice()))
        .expect(1)
        .mount(&server)
        .await;

    let dest = S3Destination::new(cfg(&server.uri()));
    let got = dest.download("snap-b.db.age").await.unwrap();
    assert_eq!(got, b"the bytes");
}

#[tokio::test]
async fn delete_issues_delete_request() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/isengard-backups/controllers/prod/old.db.age"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let dest = S3Destination::new(cfg(&server.uri()));
    dest.delete("old.db.age").await.unwrap();
}

#[tokio::test]
async fn list_parses_v2_response() {
    let server = MockServer::start().await;
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult>
  <Name>isengard-backups</Name>
  <Prefix>controllers/prod/</Prefix>
  <Contents>
    <Key>controllers/prod/snap-1.db.age</Key>
    <Size>1024</Size>
  </Contents>
  <Contents>
    <Key>controllers/prod/snap-2.db.age</Key>
    <Size>2048</Size>
  </Contents>
</ListBucketResult>"#;
    Mock::given(method("GET"))
        .and(path("/isengard-backups/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(xml))
        .expect(1)
        .mount(&server)
        .await;

    let dest = S3Destination::new(cfg(&server.uri()));
    let listed = dest.list().await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].name, "snap-1.db.age");
    assert_eq!(listed[0].size, 1024);
    assert_eq!(listed[1].name, "snap-2.db.age");
    assert_eq!(listed[1].size, 2048);
}

#[tokio::test]
async fn upload_failure_returns_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_string("<?xml version='1.0'?><Error><Code>Forbidden</Code></Error>"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let dest = S3Destination::new(cfg(&server.uri()));
    let err = dest.upload("snap.db.age", b"x").await.unwrap_err();
    match err {
        isengard_plugin_backup::destination::DestinationError::Status { status, .. } => {
            assert_eq!(status, 403)
        }
        other => panic!("expected Status error, got {other:?}"),
    }
}
