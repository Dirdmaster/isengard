//! Backup destination dispatch: filesystem, docker named volume, S3 API.
//!
//! Each variant exposes a writer (sink for the encrypted ciphertext stream)
//! and a reader (source for restore). `backup_cmd` / `restore_cmd` construct
//! the destination from the operator's CLI args and thread the writer or
//! reader through the streaming pipeline (`age::encrypt_stream` /
//! `age::decrypt_stream`).
//!
//! The Volume + S3 arms are filled in by Tasks 5.5 + 5.6. The Fs arm is
//! complete in this scaffold so the round-trip integration test in Task 5.9
//! has a working path even before remote destinations land.

#![allow(dead_code)]

use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::backup_credentials::S3Creds;

/// Image used for the tar / cat one-shot containers that bridge the
/// iso-controller-state volume into a stdin / stdout byte stream.
/// Same tag as `init_cmd::BOOTSTRAP_IMAGE` so cached layers stay shared.
const HELPER_IMAGE: &str = "alpine:3.21";

#[derive(Debug, Clone)]
pub enum BackupDestination {
    Fs(PathBuf),
    Volume {
        docker_uri: String,
        volume: String,
        filename: String,
    },
    S3 {
        creds: S3Creds,
        bucket: String,
        key: String,
    },
}

impl BackupDestination {
    /// Parse `--to <spec>`:
    ///   `volume:<name>` -> Volume (filename auto-generated)
    ///   `s3://bucket/path` -> S3 (path is the object key; trailing `/`
    ///     means "directory, append auto-generated filename")
    ///
    /// The Fs variant is built directly by the caller from `--out <path>`
    /// (the default destination when neither `--out` nor `--to` is given).
    pub fn parse_to(
        spec: &str,
        docker_uri: &str,
        context_name: &str,
        creds: Option<S3Creds>,
    ) -> Result<Self> {
        if let Some(rest) = spec.strip_prefix("volume:") {
            if rest.is_empty() {
                return Err(anyhow!(
                    "destination 'volume:' is missing a volume name; use volume:<name>"
                ));
            }
            let filename = format!(
                "iso-{context_name}-{}.tgz.age",
                chrono::Local::now().format("%Y%m%d%H%M")
            );
            return Ok(Self::Volume {
                docker_uri: docker_uri.to_string(),
                volume: rest.to_string(),
                filename,
            });
        }
        if let Some(rest) = spec.strip_prefix("s3://") {
            let (bucket, key) = rest
                .split_once('/')
                .ok_or_else(|| anyhow!("s3 destination must be s3://bucket/key"))?;
            if bucket.is_empty() {
                return Err(anyhow!("s3 destination must be s3://bucket/key"));
            }
            let creds = creds.ok_or_else(|| {
                anyhow!(
                    "s3 destination requires S3 creds in ~/.config/isd/backup.toml \
                     (contexts.<ctx>.s3.{{endpoint,access_key_id,secret_access_key,region}})"
                )
            })?;
            let key = if key.is_empty() || key.ends_with('/') {
                format!(
                    "{key}iso-{context_name}-{}.tgz.age",
                    chrono::Local::now().format("%Y%m%d%H%M")
                )
            } else {
                key.to_string()
            };
            return Ok(Self::S3 {
                creds,
                bucket: bucket.to_string(),
                key,
            });
        }
        Err(anyhow!(
            "unknown destination scheme: {spec:?}. Use --out <path> for filesystem, \
             --to volume:<name>, or --to s3://bucket/path"
        ))
    }

    /// Operator-friendly one-line description: avoids dumping secrets
    /// like the S3 access key into stdout / logs.
    pub fn describe(&self) -> String {
        match self {
            Self::Fs(path) => format!("fs:{}", path.display()),
            Self::Volume {
                volume, filename, ..
            } => format!("volume:{volume}/{filename}"),
            Self::S3 { bucket, key, .. } => format!("s3://{bucket}/{key}"),
        }
    }
}

/// Open a writer sink for the destination. The streaming pipeline pipes
/// `age` ciphertext into the returned `AsyncWrite`. The caller is
/// responsible for `shutdown().await` once writing is complete.
pub async fn open_writer(dest: &BackupDestination) -> Result<Box<dyn AsyncWrite + Unpin + Send>> {
    match dest {
        BackupDestination::Fs(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
            }
            let f = tokio::fs::File::create(path)
                .await
                .with_context(|| format!("creating {}", path.display()))?;
            Ok(Box::new(f))
        }
        BackupDestination::Volume {
            docker_uri,
            volume,
            filename,
        } => open_volume_writer(docker_uri, volume, filename).await,
        BackupDestination::S3 { creds, bucket, key } => open_s3_writer(creds, bucket, key).await,
    }
}

/// Open a reader source for restore. The streaming pipeline reads age
/// ciphertext from the returned `AsyncRead` and decrypts into the tar
/// extract container.
pub async fn open_reader(dest: &BackupDestination) -> Result<Box<dyn AsyncRead + Unpin + Send>> {
    match dest {
        BackupDestination::Fs(path) => {
            let f = tokio::fs::File::open(path)
                .await
                .with_context(|| format!("opening {}", path.display()))?;
            Ok(Box::new(f))
        }
        BackupDestination::Volume {
            docker_uri,
            volume,
            filename,
        } => open_volume_reader(docker_uri, volume, filename).await,
        BackupDestination::S3 { creds, bucket, key } => open_s3_reader(creds, bucket, key).await,
    }
}

// === Volume destination (Task 5.5) ============================================
//
// Each side spawns a one-shot alpine container with the iso-controller-state
// volume mounted at /dst and attaches to its stdin (writer) or stdout (reader).
// `auto_remove: true` means the container vanishes after exit; we don't need to
// track its ID after attach. The bollard image-pull is skipped here on the
// assumption that `isd init` (which uses the same image tag) has already pulled
// it. If not, the create_container call will fail with a clear error and the
// operator can `docker pull alpine:3.21` once on their context's docker host.

/// Wrap bollard's `input: Pin<Box<dyn AsyncWrite + Send>>` so we can return
/// it as the concrete `Box<dyn AsyncWrite + Unpin + Send>` the pipeline
/// expects. The inner `Pin<Box<...>>` is already `Unpin` at the outer
/// pointer level (a pinned box is itself unpin-able).
struct VolumeSink {
    inner: Pin<Box<dyn AsyncWrite + Send>>,
}

impl AsyncWrite for VolumeSink {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.inner.as_mut().poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_shutdown(cx)
    }
}

async fn open_volume_writer(
    docker_uri: &str,
    volume: &str,
    filename: &str,
) -> Result<Box<dyn AsyncWrite + Unpin + Send>> {
    use bollard::container::{
        AttachContainerOptions, Config, CreateContainerOptions, StartContainerOptions,
    };
    use bollard::models::HostConfig;

    let docker = isd_runtime::DockerBackend::from_uri(docker_uri).await?;
    let cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("cat > /dst/{filename}"),
    ];
    let config = Config::<String> {
        image: Some(HELPER_IMAGE.into()),
        cmd: Some(cmd),
        attach_stdin: Some(true),
        open_stdin: Some(true),
        stdin_once: Some(true),
        host_config: Some(HostConfig {
            binds: Some(vec![format!("{volume}:/dst")]),
            auto_remove: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let created = docker
        .client()
        .create_container(None::<CreateContainerOptions<String>>, config)
        .await
        .with_context(|| format!("creating alpine writer container for volume {volume:?}"))?;
    let attach = docker
        .client()
        .attach_container(
            &created.id,
            Some(AttachContainerOptions::<String> {
                stdin: Some(true),
                stream: Some(true),
                ..Default::default()
            }),
        )
        .await
        .with_context(|| format!("attaching to alpine writer container for volume {volume:?}"))?;
    docker
        .client()
        .start_container(&created.id, None::<StartContainerOptions<String>>)
        .await
        .with_context(|| format!("starting alpine writer container for volume {volume:?}"))?;
    Ok(Box::new(VolumeSink {
        inner: attach.input,
    }))
}

async fn open_volume_reader(
    docker_uri: &str,
    volume: &str,
    filename: &str,
) -> Result<Box<dyn AsyncRead + Unpin + Send>> {
    use bollard::container::{
        AttachContainerOptions, Config, CreateContainerOptions, StartContainerOptions,
    };
    use bollard::models::HostConfig;
    use futures_util::StreamExt;
    use tokio_util::bytes::Bytes;
    use tokio_util::io::StreamReader;

    let docker = isd_runtime::DockerBackend::from_uri(docker_uri).await?;
    let cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("cat /dst/{filename}"),
    ];
    let config = Config::<String> {
        image: Some(HELPER_IMAGE.into()),
        cmd: Some(cmd),
        attach_stdout: Some(true),
        host_config: Some(HostConfig {
            binds: Some(vec![format!("{volume}:/dst")]),
            auto_remove: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let created = docker
        .client()
        .create_container(None::<CreateContainerOptions<String>>, config)
        .await
        .with_context(|| format!("creating alpine reader container for volume {volume:?}"))?;
    let attach = docker
        .client()
        .attach_container(
            &created.id,
            Some(AttachContainerOptions::<String> {
                stdout: Some(true),
                stream: Some(true),
                ..Default::default()
            }),
        )
        .await
        .with_context(|| format!("attaching to alpine reader container for volume {volume:?}"))?;
    docker
        .client()
        .start_container(&created.id, None::<StartContainerOptions<String>>)
        .await
        .with_context(|| format!("starting alpine reader container for volume {volume:?}"))?;

    // Map bollard's Stream<LogOutput> into a Stream<Bytes> for StreamReader.
    // Stderr is folded into the same byte stream: a non-zero `cat` (file
    // missing) emits its error to stderr, and the upstream age::decrypt
    // catches a malformed header and surfaces it. We still surface bollard
    // transport errors as io::Errors so StreamReader can propagate them.
    let mapped = attach.output.map(|item| -> std::io::Result<Bytes> {
        item.map(|log| log.into_bytes())
            .map_err(|e| std::io::Error::other(format!("bollard volume attach: {e}")))
    });
    Ok(Box::new(StreamReader::new(mapped)))
}

// === S3 destination (Task 5.6) ================================================
//
// `S3MultipartSink` buffers up to 5 MiB locally, then ships each chunk as an
// S3 `upload_part`. On `poll_shutdown` it flushes the trailing buffer (any
// remaining bytes, even <5MiB, since the last part is allowed to be small),
// calls `complete_multipart_upload`, and surfaces any error. If the writer is
// shut down before any 5MiB part has been sent, the *entire* upload is one
// (small) part: S3 accepts this for the very last part of a multipart upload.
//
// The reader side uses `get_object`, whose response body is an
// `aws_smithy_types::byte_stream::ByteStream` that exposes an
// `into_async_read()` returning `impl AsyncRead`.

const S3_PART_SIZE: usize = 5 * 1024 * 1024;

fn s3_client(creds: &S3Creds) -> aws_sdk_s3::Client {
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};

    let config = aws_sdk_s3::config::Builder::new()
        .endpoint_url(&creds.endpoint)
        .region(Region::new(creds.region.clone()))
        .credentials_provider(Credentials::new(
            &creds.access_key_id,
            &creds.secret_access_key,
            None,
            None,
            "isd-backup",
        ))
        .behavior_version(BehaviorVersion::latest())
        // Required for path-style addressing some S3-compatibles use (MinIO
        // default). AWS / R2 / B2 accept it too; virtual-hosted style would
        // need the bucket name to be a valid subdomain, which user-picked
        // bucket names often aren't.
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(config)
}

struct S3MultipartSink {
    /// Channel to the background uploader task. `None` means the sink has
    /// been shut down and the task was joined.
    tx: Option<tokio::sync::mpsc::Sender<tokio_util::bytes::Bytes>>,
    /// Local buffer building up the next part. Flushed when it crosses
    /// `S3_PART_SIZE`.
    buf: tokio_util::bytes::BytesMut,
    /// Background uploader task. `take`n on first call to `poll_shutdown`.
    join: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl AsyncWrite for S3MultipartSink {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        use tokio_util::bytes::BufMut;

        let Some(tx) = self.tx.as_ref().cloned() else {
            return Poll::Ready(Err(std::io::Error::other("s3 sink already shut down")));
        };
        self.buf.put_slice(data);
        // Flush any full parts the buffer now contains. Use try_send for
        // backpressure: if the channel is full, ask the runtime to retry.
        while self.buf.len() >= S3_PART_SIZE {
            let part = self.buf.split_to(S3_PART_SIZE).freeze();
            match tx.try_send(part) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(part)) => {
                    // Channel full: put the bytes back at the head of buf,
                    // wake the task, return Pending.
                    let mut new_buf =
                        tokio_util::bytes::BytesMut::with_capacity(part.len() + self.buf.len());
                    new_buf.extend_from_slice(&part);
                    new_buf.extend_from_slice(&self.buf);
                    self.buf = new_buf;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    return Poll::Ready(Err(std::io::Error::other(
                        "s3 uploader task closed unexpectedly",
                    )));
                }
            }
        }
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        // S3 multipart parts must be ≥5 MiB except the last; we cannot
        // flush mid-stream. The pipeline's only meaningful flush is
        // shutdown, which is handled by poll_shutdown.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        // Drain remaining buffer to the channel (final part, any size).
        if let Some(tx) = self.tx.take() {
            if !self.buf.is_empty() {
                let final_part = std::mem::take(&mut self.buf).freeze();
                // try_send with retry; the channel is now closed for new
                // senders once we drop `tx`, but the receiver still drains.
                if let Err(e) = tx.try_send(final_part) {
                    return Poll::Ready(Err(std::io::Error::other(format!(
                        "s3 sink final part: {e}"
                    ))));
                }
            }
            drop(tx); // close the channel; the uploader sees end-of-stream
        }
        // Drive the join handle to completion.
        let Some(handle) = self.join.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        let handle = Pin::new(handle);
        match handle.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(inner)) => {
                self.join = None;
                Poll::Ready(inner)
            }
            Poll::Ready(Err(e)) => {
                self.join = None;
                Poll::Ready(Err(std::io::Error::other(format!("s3 uploader join: {e}"))))
            }
        }
    }
}

async fn open_s3_writer(
    creds: &S3Creds,
    bucket: &str,
    key: &str,
) -> Result<Box<dyn AsyncWrite + Unpin + Send>> {
    use aws_sdk_s3::primitives::ByteStream;

    let client = s3_client(creds);
    let create = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .content_type("application/octet-stream")
        .send()
        .await
        .with_context(|| format!("starting multipart upload for s3://{bucket}/{key}"))?;
    let upload_id = create
        .upload_id
        .ok_or_else(|| anyhow!("s3 returned no upload_id for s3://{bucket}/{key}"))?;

    // Bounded channel. Cap is small (parts are large, ~5 MiB each).
    let (tx, mut rx) = tokio::sync::mpsc::channel::<tokio_util::bytes::Bytes>(4);

    let bucket_owned = bucket.to_string();
    let key_owned = key.to_string();
    let upload_id_owned = upload_id.clone();
    let join = tokio::spawn(async move {
        let mut completed_parts: Vec<aws_sdk_s3::types::CompletedPart> = Vec::new();
        let mut part_number: i32 = 1;
        let mut upload_err: Option<String> = None;

        while let Some(part_bytes) = rx.recv().await {
            let body = ByteStream::from(part_bytes.to_vec());
            match client
                .upload_part()
                .bucket(&bucket_owned)
                .key(&key_owned)
                .upload_id(&upload_id_owned)
                .part_number(part_number)
                .body(body)
                .send()
                .await
            {
                Ok(resp) => {
                    completed_parts.push(
                        aws_sdk_s3::types::CompletedPart::builder()
                            .part_number(part_number)
                            .set_e_tag(resp.e_tag)
                            .build(),
                    );
                    part_number += 1;
                }
                Err(e) => {
                    upload_err = Some(format!("upload_part {part_number}: {e}"));
                    break;
                }
            }
        }

        if let Some(msg) = upload_err {
            // Abort the upload so we don't leak partial state on the bucket.
            let _ = client
                .abort_multipart_upload()
                .bucket(&bucket_owned)
                .key(&key_owned)
                .upload_id(&upload_id_owned)
                .send()
                .await;
            return Err(std::io::Error::other(msg));
        }

        let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();
        client
            .complete_multipart_upload()
            .bucket(&bucket_owned)
            .key(&key_owned)
            .upload_id(&upload_id_owned)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(|e| std::io::Error::other(format!("complete_multipart_upload: {e}")))?;
        Ok(())
    });

    Ok(Box::new(S3MultipartSink {
        tx: Some(tx),
        buf: tokio_util::bytes::BytesMut::with_capacity(S3_PART_SIZE),
        join: Some(join),
    }))
}

async fn open_s3_reader(
    creds: &S3Creds,
    bucket: &str,
    key: &str,
) -> Result<Box<dyn AsyncRead + Unpin + Send>> {
    let client = s3_client(creds);
    let resp = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .with_context(|| format!("get_object s3://{bucket}/{key}"))?;
    Ok(Box::new(resp.body.into_async_read()))
}

// Pull `Future` into scope for the JoinHandle poll in S3MultipartSink.
use std::future::Future;

#[cfg(test)]
mod tests {
    use super::*;

    fn s3_creds() -> S3Creds {
        S3Creds {
            endpoint: "https://r2.cf.com".into(),
            access_key_id: "AKID".into(),
            secret_access_key: "SECRET".into(),
            region: "auto".into(),
        }
    }

    #[test]
    fn parse_to_volume_basic() {
        let dest = BackupDestination::parse_to(
            "volume:iso-backups",
            "unix:///var/run/docker.sock",
            "lausanne",
            None,
        )
        .unwrap();
        match dest {
            BackupDestination::Volume {
                volume, filename, ..
            } => {
                assert_eq!(volume, "iso-backups");
                assert!(filename.starts_with("iso-lausanne-"));
                assert!(filename.ends_with(".tgz.age"));
            }
            other => panic!("expected Volume, got {other:?}"),
        }
    }

    #[test]
    fn parse_to_volume_missing_name_errors() {
        let err = BackupDestination::parse_to("volume:", "unix:///x", "ctx", None).unwrap_err();
        assert!(format!("{err}").contains("missing a volume name"));
    }

    #[test]
    fn parse_to_s3_with_explicit_key() {
        let dest = BackupDestination::parse_to(
            "s3://my-bucket/backups/iso-2026.tgz.age",
            "unix:///x",
            "lausanne",
            Some(s3_creds()),
        )
        .unwrap();
        match dest {
            BackupDestination::S3 { bucket, key, .. } => {
                assert_eq!(bucket, "my-bucket");
                assert_eq!(key, "backups/iso-2026.tgz.age");
            }
            other => panic!("expected S3, got {other:?}"),
        }
    }

    #[test]
    fn parse_to_s3_with_directory_prefix_auto_appends_filename() {
        let dest = BackupDestination::parse_to(
            "s3://my-bucket/backups/",
            "unix:///x",
            "lausanne",
            Some(s3_creds()),
        )
        .unwrap();
        match dest {
            BackupDestination::S3 { bucket, key, .. } => {
                assert_eq!(bucket, "my-bucket");
                assert!(key.starts_with("backups/iso-lausanne-"));
                assert!(key.ends_with(".tgz.age"));
            }
            other => panic!("expected S3, got {other:?}"),
        }
    }

    #[test]
    fn parse_to_s3_without_creds_errors() {
        let err = BackupDestination::parse_to("s3://b/k", "unix:///x", "ctx", None).unwrap_err();
        assert!(format!("{err}").contains("requires S3 creds"));
    }

    #[test]
    fn parse_to_unknown_scheme_errors() {
        let err = BackupDestination::parse_to("ftp://b/k", "unix:///x", "ctx", None).unwrap_err();
        assert!(format!("{err}").contains("unknown destination scheme"));
    }

    #[test]
    fn describe_does_not_leak_s3_credentials() {
        let dest = BackupDestination::S3 {
            creds: s3_creds(),
            bucket: "b".into(),
            key: "k".into(),
        };
        let described = dest.describe();
        assert!(!described.contains("AKID"));
        assert!(!described.contains("SECRET"));
        assert_eq!(described, "s3://b/k");
    }

    #[tokio::test]
    async fn open_writer_fs_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub/dir/out.age");
        let dest = BackupDestination::Fs(path.clone());
        let mut w = open_writer(&dest).await.unwrap();
        use tokio::io::AsyncWriteExt;
        w.write_all(b"hello").await.unwrap();
        w.shutdown().await.unwrap();
        let read = tokio::fs::read(&path).await.unwrap();
        assert_eq!(read, b"hello");
    }
}
