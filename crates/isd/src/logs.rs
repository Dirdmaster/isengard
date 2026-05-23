//! `isd logs <selector>`: stream container logs directly from the docker
//! daemon on the resolved context.
//!
//! The selector contract mirrors `isd stop/start/restart/rm/kill`: a `#N`
//! from `isd ps`, a range like `0-3`, a literal container name, or a
//! literal container ID. Resolution flows through
//! [`crate::index_resolve`] so the index cache validates and refreshes
//! exactly like the lifecycle verbs.
//!
//! Streaming uses `bollard::container::logs` per target. When a range
//! resolves to multiple targets the streams are merged via
//! `futures_util::stream::select_all` and each output line is prefixed
//! `<container-name> | <line>` (docker-compose style). Single targets
//! print raw lines.
//!
//! Note: cross-host log streaming (resolved target lives on a
//! different host than the current context) currently errors with an
//! ssh hint. The controller-side WebSocket forwarder that the
//! version used was removed in this rewrite; a future track wires it
//! back in.

use anyhow::{Result, anyhow};
use clap::Args;
use futures_util::StreamExt;

use crate::index_resolve;
use crate::ps;

/// CLI flags for `isd logs`.
#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Container selector (#N from `isd ps`, name, ID, or range like `0-3`).
    pub target: String,
    /// Follow log output.
    #[arg(short = 'f', long)]
    pub follow: bool,
    /// Lines from the end (default 200).
    #[arg(long, default_value_t = 200)]
    pub tail: u32,
    /// Show timestamps.
    #[arg(short = 't', long)]
    pub timestamps: bool,
}

/// Tail or stream logs for one or more containers. Multi-target
/// streams are merged via `select_all` and each line prefixed with
/// the container name (docker-compose style).
///
/// # Errors
///
/// Returns `Err` when resolution yields no targets, when any target
/// lives on a different context (cross-host streaming is not wired
/// yet), or when the docker backend fails to attach.
pub async fn run(args: LogsArgs, context: Option<&str>) -> Result<()> {
    let targets = index_resolve::resolve(std::slice::from_ref(&args.target))?;
    if targets.is_empty() {
        return Err(anyhow!("no targets resolved for {:?}", args.target));
    }

    // Cross-host log streaming is not wired through the
    // controller WebSocket forwarder anymore. Detect any target whose
    // recorded context (the host stamped at `isd ps` time) does not
    // match the current resolved context, and error with an ssh hint.
    let current_context = crate::docker_context::resolve_context_name(context)?;
    for t in &targets {
        // Literal-only resolution stamps an empty context; skip that case
        // since we cannot prove cross-host without the index cache.
        if !t.context.is_empty() && t.context != current_context {
            return Err(anyhow!(
                "cross-host log streaming not yet supported; \
                 ssh to {} and run `isd logs <selector>` there",
                t.context
            ));
        }
    }

    let backend = ps::open_docker_backend(context).await?;
    let prefix = targets.len() > 1;

    let mut streams = Vec::with_capacity(targets.len());
    for t in &targets {
        let opts = bollard::container::LogsOptions::<String> {
            follow: args.follow,
            stdout: true,
            stderr: true,
            tail: args.tail.to_string(),
            timestamps: args.timestamps,
            ..Default::default()
        };
        let stream = backend.client().logs(&t.container_id, Some(opts));
        let name = t.name.clone();
        streams.push(stream.map(move |item| (name.clone(), item)));
    }

    let mut merged = futures_util::stream::select_all(streams);
    while let Some((name, item)) = merged.next().await {
        match item {
            Ok(chunk) => {
                let body = chunk.to_string();
                for raw in body.lines() {
                    if prefix {
                        println!("{name} | {raw}");
                    } else {
                        println!("{raw}");
                    }
                }
            }
            Err(e) => {
                eprintln!("isd logs: {name}: {e}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_cache::test_env_lock as env_lock;
    use crate::index_cache::{self, IndexCache, IndexRow};
    use chrono::Utc;

    fn with_cache(rows: Vec<IndexRow>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("ISD_INDEX_CACHE", dir.path());
        }
        let cache = IndexCache {
            captured_at: Utc::now(),
            command: "ps".into(),
            rows,
        };
        index_cache::write(&cache).unwrap();
        dir
    }

    fn sample_rows() -> Vec<IndexRow> {
        vec![
            IndexRow {
                index: 0,
                context: "lausanne".into(),
                container_id: "a1b2c3d4e5f6".into(),
                name: "web-proxy".into(),
            },
            IndexRow {
                index: 1,
                context: "lausanne".into(),
                container_id: "7f8e9d0c1b2a".into(),
                name: "app-db".into(),
            },
            IndexRow {
                index: 2,
                context: "lausanne".into(),
                container_id: "3c4d5e6f7a8b".into(),
                name: "isd-agent".into(),
            },
            IndexRow {
                index: 3,
                context: "lausanne".into(),
                container_id: "deadbeefcafe".into(),
                name: "isd-controller".into(),
            },
        ]
    }

    /// Selector `"0"` resolves to exactly one target, picking the right
    /// row from the cache. Mirrors the index_resolve single-index test
    /// but exercised through the LogsArgs surface so a future arg
    /// refactor catches drift.
    #[test]
    fn logs_parses_single_target() {
        let _lock = env_lock();
        let _dir = with_cache(sample_rows());
        let args = LogsArgs {
            target: "0".into(),
            follow: false,
            tail: 200,
            timestamps: false,
        };
        let resolved = index_resolve::resolve(std::slice::from_ref(&args.target)).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "web-proxy");
        assert_eq!(resolved[0].container_id, "a1b2c3d4e5f6");
        assert!(resolved[0].via_index);
    }

    /// Selector `"0-3"` resolves to four targets, ordered by index.
    #[test]
    fn logs_parses_range() {
        let _lock = env_lock();
        let _dir = with_cache(sample_rows());
        let args = LogsArgs {
            target: "0-3".into(),
            follow: false,
            tail: 200,
            timestamps: false,
        };
        let resolved = index_resolve::resolve(std::slice::from_ref(&args.target)).unwrap();
        assert_eq!(resolved.len(), 4);
        assert_eq!(resolved[0].name, "web-proxy");
        assert_eq!(resolved[3].name, "isd-controller");
    }
}
