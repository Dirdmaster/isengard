//! `isd backup`: stream the `iso-controller-state` docker volume through
//! a one-shot `tar c -C /state .` container, encrypt with age, and write to
//! a destination (filesystem / docker volume / S3-API).
//!
//! The pipeline is:
//!   alpine `tar c -C /state .` stdout
//!     -> tokio AsyncRead (bollard attach_container)
//!     -> SyncIoBridge (blocking Read)
//!     -> age::Encryptor (blocking Read/Write)
//!     -> SyncIoBridge (blocking Write over tokio AsyncWrite)
//!     -> destination sink (file / volume container stdin / S3 multipart)

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use tokio::io::{AsyncRead, AsyncWriteExt};

use crate::backup_credentials;
use crate::backup_crypto;
use crate::backup_storage::{self, BackupDestination};

const HELPER_IMAGE: &str = "alpine:3.21";
const STATE_VOLUME: &str = "iso-controller-state";

#[derive(Debug, Args, Default)]
pub struct BackupArgs {
    /// Output path (filesystem). Defaults to `./iso-<ctx>-<date>.tgz.age`.
    #[arg(long, conflicts_with = "to")]
    pub out: Option<PathBuf>,
    /// Non-filesystem destination: `volume:<name>` or `s3://bucket/path`.
    #[arg(long, conflicts_with = "out")]
    pub to: Option<String>,
    /// Read passphrase from file (overrides env + stored).
    #[arg(long)]
    pub passphrase_file: Option<PathBuf>,
}

impl BackupArgs {
    /// Default args for the `isd uninit --backup-first` integration: just
    /// the fs destination with the default-generated filename in `cwd`.
    pub fn default_for_uninit() -> Self {
        Self::default()
    }
}

pub async fn run(args: BackupArgs, context: Option<&str>) -> Result<()> {
    let context_name = crate::docker_context::resolve_context_name(context)?;
    let docker_uri = crate::docker_context::resolve_docker_uri(context)?;

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

    let destination = if let Some(spec) = &args.to {
        BackupDestination::parse_to(spec, &docker_uri, &context_name, s3_creds)?
    } else {
        let path = args.out.clone().unwrap_or_else(|| {
            PathBuf::from(format!(
                "iso-{context_name}-{}.tgz.age",
                chrono::Local::now().format("%Y%m%d%H%M")
            ))
        });
        BackupDestination::Fs(path)
    };

    eprintln!(
        "isd backup: streaming {STATE_VOLUME} -> {}",
        destination.describe()
    );

    // 1. Spawn the alpine tar producer; collect its stdout as an AsyncRead.
    let docker = isd_runtime::DockerBackend::from_uri(&docker_uri).await?;
    let tar_stdout = spawn_tar_producer(&docker).await?;

    // 2. Open destination sink. The sink moves into the spawn_blocking
    //    closure so SyncIoBridge can drive it synchronously, then comes back
    //    out so we can call `shutdown().await` on the original tokio runtime.
    let sink = backup_storage::open_writer(&destination).await?;

    // 3. Pipeline: tar -> age::encrypt -> sink. SyncIoBridge wraps each end
    //    so age's blocking Read/Write loop works on top of tokio AsyncIO.
    //    Build the bridges *inside* the blocking task so `Handle::current()`
    //    resolves cleanly to the multi-thread runtime handle.
    let (bytes, mut sink) = tokio::task::spawn_blocking(move || {
        let sync_reader = tokio_util::io::SyncIoBridge::new(tar_stdout);
        let mut sink_owned = sink;
        let sync_writer = tokio_util::io::SyncIoBridge::new(&mut sink_owned);
        let bytes = backup_crypto::encrypt_stream(&passphrase, sync_reader, sync_writer)?;
        Ok::<_, anyhow::Error>((bytes, sink_owned))
    })
    .await
    .context("backup encrypt task panicked")?
    .context("encrypting backup stream")?;

    sink.shutdown()
        .await
        .context("closing backup destination")?;
    println!(
        "isd backup: wrote {bytes} plaintext bytes ({})",
        destination.describe()
    );
    Ok(())
}

/// Spawn a one-shot `alpine tar c -C /state .` container with the
/// `iso-controller-state` volume mounted and return its stdout as an
/// `AsyncRead`. The container auto-removes on exit.
async fn spawn_tar_producer(
    docker: &isd_runtime::DockerBackend,
) -> Result<Box<dyn AsyncRead + Unpin + Send>> {
    use bollard::container::{
        AttachContainerOptions, Config, CreateContainerOptions, StartContainerOptions,
    };
    use bollard::models::HostConfig;
    use futures_util::StreamExt;
    use tokio_util::bytes::Bytes;
    use tokio_util::io::StreamReader;

    let cmd = vec![
        "tar".to_string(),
        "c".to_string(),
        "-C".to_string(),
        "/state".to_string(),
        ".".to_string(),
    ];
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
        .context("creating alpine tar producer container")?;
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
        .context("attaching to alpine tar producer container")?;
    docker
        .client()
        .start_container(&created.id, None::<StartContainerOptions<String>>)
        .await
        .context("starting alpine tar producer container")?;

    let mapped = attach.output.map(|item| -> std::io::Result<Bytes> {
        item.map(|log| log.into_bytes())
            .map_err(|e| std::io::Error::other(format!("bollard tar attach: {e}")))
    });
    Ok(Box::new(StreamReader::new(mapped)))
}
