//! `isd restore`: inverse of `isd backup`. Decrypts ciphertext from
//! filesystem / docker named volume / S3-API and extracts the tar into the
//! `iso-controller-state` docker volume.
//!
//! Pipeline:
//!   source reader (file / alpine cat / S3 get_object)
//!     -> SyncIoBridge (blocking Read)
//!     -> age::decrypt_stream (blocking Read/Write)
//!     -> SyncIoBridge (blocking Write over tokio AsyncWrite)
//!     -> alpine `tar x -C /state` stdin
//!
//! Refuses to overwrite a populated `iso-controller-state` unless
//! `--overwrite` is set: protects an operator from blowing away a live
//! controller's state on a typo.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::Args;
use tokio::io::AsyncWrite;

use crate::backup_credentials;
use crate::backup_crypto;
use crate::backup_storage::{self, BackupDestination};

const HELPER_IMAGE: &str = "alpine:3.21";
const STATE_VOLUME: &str = "iso-controller-state";

#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Backup source: filesystem path, `volume:<name>/<filename>`, or
    /// `s3://bucket/key`.
    pub source: String,
    /// Decrypt with passphrase from file (overrides env + stored).
    #[arg(long)]
    pub passphrase_file: Option<PathBuf>,
    /// Refuse if iso-controller-state already has content; --overwrite
    /// is required to clobber a populated volume.
    #[arg(long)]
    pub overwrite: bool,
}

pub async fn run(args: RestoreArgs, context: Option<&str>) -> Result<()> {
    let context_name = crate::ps::resolve_docker_context(context)?;
    let docker_uri = crate::ps::resolve_docker_uri(context)?.ok_or_else(|| {
        anyhow!(
            "context has no docker endpoint; add one with `isd context create ... --docker ...`"
        )
    })?;

    let creds_path = backup_credentials::default_path()?;
    let creds_file = backup_credentials::load(&creds_path)?;
    let passphrase = backup_credentials::resolve_passphrase(
        &context_name,
        &creds_file,
        args.passphrase_file.as_deref(),
    )?;

    let s3_creds = creds_file
        .contexts
        .get(&context_name)
        .and_then(|c| c.s3.clone());

    let source = parse_source(&args.source, &docker_uri, s3_creds)?;
    eprintln!("isd restore: reading from {}", source.describe());

    let docker = isd_runtime::DockerBackend::from_uri(&docker_uri).await?;

    // Refuse to clobber a populated volume on a typo. We list entries inside
    // /state via a one-shot ls; non-empty -> bail unless --overwrite.
    if !args.overwrite && state_volume_populated(&docker).await? {
        return Err(anyhow!(
            "{STATE_VOLUME} already has content; pass --overwrite to restore on top of it"
        ));
    }

    // 1. Open source reader.
    let reader = backup_storage::open_reader(&source).await?;

    // 2. Spawn the alpine tar-extract container; attach to its stdin.
    let tar_stdin = spawn_tar_extractor(&docker).await?;

    // 3. Pipeline: source -> age::decrypt -> tar stdin. Same SyncIoBridge
    //    + spawn_blocking shape as backup_cmd.
    let bytes = tokio::task::spawn_blocking(move || {
        let sync_reader = tokio_util::io::SyncIoBridge::new(reader);
        let mut sink_owned = tar_stdin;
        let sync_writer = tokio_util::io::SyncIoBridge::new(&mut sink_owned);
        let bytes = backup_crypto::decrypt_stream(&passphrase, sync_reader, sync_writer)?;
        // Drop sink_owned so tar sees EOF on stdin. Returning it is unnecessary
        // because the caller doesn't need to await its shutdown: closing the
        // bollard attach.input is what flushes the final bytes to the
        // container, and bollard does that on drop.
        drop(sink_owned);
        Ok::<_, anyhow::Error>(bytes)
    })
    .await
    .context("restore decrypt task panicked")?
    .context("decrypting restore stream")?;

    println!("isd restore: extracted {bytes} plaintext bytes into {STATE_VOLUME}");
    Ok(())
}

/// Parse the operator's source spec:
///   `s3://bucket/key`           -> S3
///   `volume:<name>/<filename>`  -> Volume (filename is part of the spec
///                                  because restore needs to know which
///                                  archive to read, not auto-generate one)
///   anything else                -> Fs path
fn parse_source(
    spec: &str,
    docker_uri: &str,
    creds: Option<crate::backup_credentials::S3Creds>,
) -> Result<BackupDestination> {
    if let Some(rest) = spec.strip_prefix("s3://") {
        let (bucket, key) = rest
            .split_once('/')
            .ok_or_else(|| anyhow!("s3 source must be s3://bucket/key"))?;
        if bucket.is_empty() || key.is_empty() {
            return Err(anyhow!("s3 source must be s3://bucket/key"));
        }
        let creds = creds.ok_or_else(|| {
            anyhow!(
                "s3 source requires S3 creds in ~/.config/isd/backup.toml \
                 (contexts.<ctx>.s3.{{endpoint,access_key_id,secret_access_key,region}})"
            )
        })?;
        return Ok(BackupDestination::S3 {
            creds,
            bucket: bucket.to_string(),
            key: key.to_string(),
        });
    }
    if let Some(rest) = spec.strip_prefix("volume:") {
        let (volume, filename) = rest.split_once('/').ok_or_else(|| {
            anyhow!("volume source must be volume:<name>/<filename> (e.g. volume:iso-backups/iso-lausanne-202605181200.tgz.age)")
        })?;
        if volume.is_empty() || filename.is_empty() {
            return Err(anyhow!("volume source must be volume:<name>/<filename>"));
        }
        return Ok(BackupDestination::Volume {
            docker_uri: docker_uri.to_string(),
            volume: volume.to_string(),
            filename: filename.to_string(),
        });
    }
    Ok(BackupDestination::Fs(PathBuf::from(spec)))
}

/// Ask docker whether `iso-controller-state` has any entries. Uses a
/// one-shot alpine `ls -A /state` whose exit code is 0 iff the directory
/// is readable; we treat any captured stdout line as "non-empty". Volume
/// not yet created -> the bind mount creates it empty, so we still see
/// "empty" + return false.
async fn state_volume_populated(docker: &isd_runtime::DockerBackend) -> Result<bool> {
    use bollard::container::{
        AttachContainerOptions, Config, CreateContainerOptions, StartContainerOptions,
        WaitContainerOptions,
    };
    use bollard::models::HostConfig;
    use futures_util::StreamExt;

    let cmd = vec!["sh".into(), "-c".into(), "ls -A /state | head -n1".into()];
    let config = Config::<String> {
        image: Some(HELPER_IMAGE.into()),
        cmd: Some(cmd),
        attach_stdout: Some(true),
        host_config: Some(HostConfig {
            binds: Some(vec![format!("{STATE_VOLUME}:/state:ro")]),
            auto_remove: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let created = docker
        .client()
        .create_container(None::<CreateContainerOptions<String>>, config)
        .await
        .context("creating ls probe container")?;
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
        .context("attaching to ls probe container")?;
    docker
        .client()
        .start_container(&created.id, None::<StartContainerOptions<String>>)
        .await
        .context("starting ls probe container")?;

    let mut buf = String::new();
    let mut stream = attach.output;
    while let Some(item) = stream.next().await {
        let chunk = item.context("reading ls probe stdout")?;
        buf.push_str(&chunk.to_string());
    }

    // Wait for the container to exit so its auto-remove fires before we
    // return. We can't wait_container after auto-remove fires; that's
    // a race the bollard side already handles by returning a terminal
    // wait response, but in practice the stream draining above blocks
    // until exit, so we usually don't need an explicit wait.
    let mut wait = docker
        .client()
        .wait_container(&created.id, None::<WaitContainerOptions<String>>);
    while let Some(item) = wait.next().await {
        // Ignore: container may already be gone (auto-removed).
        let _ = item;
    }

    Ok(!buf.trim().is_empty())
}

/// Spawn a one-shot `alpine tar x -C /state` container with `iso-controller-state`
/// mounted read-write, attached to its stdin. Returns the stdin sink so the
/// pipeline can write age-decrypted tar bytes into it.
async fn spawn_tar_extractor(
    docker: &isd_runtime::DockerBackend,
) -> Result<Box<dyn AsyncWrite + Unpin + Send>> {
    use bollard::container::{
        AttachContainerOptions, Config, CreateContainerOptions, StartContainerOptions,
    };
    use bollard::models::HostConfig;

    let cmd = vec!["tar".into(), "x".into(), "-C".into(), "/state".into()];
    let config = Config::<String> {
        image: Some(HELPER_IMAGE.into()),
        cmd: Some(cmd),
        attach_stdin: Some(true),
        open_stdin: Some(true),
        stdin_once: Some(true),
        host_config: Some(HostConfig {
            binds: Some(vec![format!("{STATE_VOLUME}:/state")]),
            auto_remove: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let created = docker
        .client()
        .create_container(None::<CreateContainerOptions<String>>, config)
        .await
        .context("creating alpine tar extractor container")?;
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
        .context("attaching to alpine tar extractor container")?;
    docker
        .client()
        .start_container(&created.id, None::<StartContainerOptions<String>>)
        .await
        .context("starting alpine tar extractor container")?;
    Ok(Box::new(TarStdinSink {
        inner: attach.input,
    }))
}

/// Thin wrapper so we can hand back `Box<dyn AsyncWrite + Unpin + Send>`
/// from bollard's `Pin<Box<dyn AsyncWrite + Send>>`.
struct TarStdinSink {
    inner: std::pin::Pin<Box<dyn AsyncWrite + Send>>,
}

impl AsyncWrite for TarStdinSink {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.inner.as_mut().poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_source_fs() {
        let dest = parse_source("/tmp/back.tgz.age", "unix:///x", None).unwrap();
        match dest {
            BackupDestination::Fs(path) => assert_eq!(path, PathBuf::from("/tmp/back.tgz.age")),
            other => panic!("expected Fs, got {other:?}"),
        }
    }

    #[test]
    fn parse_source_volume_requires_filename() {
        let err = parse_source("volume:iso-backups", "unix:///x", None).unwrap_err();
        assert!(format!("{err}").contains("volume:<name>/<filename>"));
    }

    #[test]
    fn parse_source_volume_basic() {
        let dest = parse_source(
            "volume:iso-backups/iso-lausanne-202605181200.tgz.age",
            "unix:///x",
            None,
        )
        .unwrap();
        match dest {
            BackupDestination::Volume {
                volume, filename, ..
            } => {
                assert_eq!(volume, "iso-backups");
                assert_eq!(filename, "iso-lausanne-202605181200.tgz.age");
            }
            other => panic!("expected Volume, got {other:?}"),
        }
    }

    #[test]
    fn parse_source_s3_basic() {
        let creds = crate::backup_credentials::S3Creds {
            endpoint: "https://r2".into(),
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
            region: "auto".into(),
        };
        let dest = parse_source("s3://b/k", "unix:///x", Some(creds)).unwrap();
        match dest {
            BackupDestination::S3 { bucket, key, .. } => {
                assert_eq!(bucket, "b");
                assert_eq!(key, "k");
            }
            other => panic!("expected S3, got {other:?}"),
        }
    }

    #[test]
    fn parse_source_s3_without_creds_errors() {
        let err = parse_source("s3://b/k", "unix:///x", None).unwrap_err();
        assert!(format!("{err}").contains("requires S3 creds"));
    }
}
