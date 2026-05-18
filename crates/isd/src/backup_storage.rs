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
        BackupDestination::S3 { .. } => {
            Err(anyhow!("s3 destination not implemented yet (Task 5.6)"))
        }
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
        BackupDestination::S3 { .. } => Err(anyhow!("s3 source not implemented yet (Task 5.6)")),
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
