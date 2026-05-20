//! v0.3d compose watcher: spawn an inotify (Linux) / FSEvents (macOS)
//! watcher on `/etc/isengard/stacks/` and emit a debounced "stack
//! compose changed" event whenever a file under it is written.
//!
//! Why this lives in the agent: the agent is the only process with a
//! bind-mount onto the host's `/etc/isengard/stacks/` directory, so it's
//! the authoritative observer of operator edits via `vim`, `git pull`,
//! `tee`, etc. The watcher does not interpret the change: it shoves a
//! `StackComposeChanged { stack_name }` event onto an in-process channel
//! and lets the caller decide what to do (parse, plan, apply).
//!
//! Debounce: file editors typically write atomically by `unlink + create`
//! or via a `<file>.swp` -> `mv -f` rename. Both produce 2-3 events
//! within ~50ms. We collapse the burst by waiting `DEBOUNCE` (default
//! 500ms) of quiescence before emitting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, warn};

/// Default debounce window. The watcher collapses multiple events for
/// the same file inside this window into one outbound notification.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// Outbound notification: a stack's `compose.yaml` has settled into a
/// new state on disk. The receiver decides whether to re-read, hash,
/// and reconcile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackComposeChanged {
    /// Bare stack name (the directory name under the watched root).
    pub stack_name: String,
    /// Absolute path to the `compose.yaml` that changed.
    pub path: PathBuf,
}

/// Spawn the watcher. Listens on `root` recursively. Outbound events
/// land on the returned receiver; the caller owns the lifetime.
///
/// Returns `(receiver, watcher)`. The `Watcher` value must be kept
/// alive as long as you want events; dropping it stops the watch.
pub fn spawn(
    root: PathBuf,
) -> anyhow::Result<(
    mpsc::Receiver<StackComposeChanged>,
    notify::RecommendedWatcher,
)> {
    spawn_with_debounce(root, DEBOUNCE)
}

/// Same as [`spawn`], with an explicit debounce window. Used by the
/// integration tests so they can run faster than the production 500ms.
pub fn spawn_with_debounce(
    root: PathBuf,
    debounce: Duration,
) -> anyhow::Result<(
    mpsc::Receiver<StackComposeChanged>,
    notify::RecommendedWatcher,
)> {
    // Make the directory exist so the watcher doesn't error out on a
    // fresh agent boot. v0.3c's compose_import already creates this
    // path on first write, but the watcher must come up before the
    // first import.
    if !root.exists() {
        std::fs::create_dir_all(&root).map_err(|e| {
            anyhow::anyhow!("creating compose watcher root {}: {e}", root.display())
        })?;
    }
    // Canonicalize so the path the watcher emits matches the path we
    // compare against. macOS FSEvents reports `/private/var/folders/...`
    // even when we watch `/var/folders/...`; on Linux the symlink is
    // typically a noop. Failures here are not fatal: fall back to the
    // input path.
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    // Bridge: notify uses a sync callback; we want a tokio receiver.
    // The raw channel is bounded so a flaky filesystem can't grow
    // unbounded queues; the consumer drains as fast as it reconciles.
    let (raw_tx, mut raw_rx) = mpsc::channel::<RawEvent>(256);
    let (out_tx, out_rx) = mpsc::channel::<StackComposeChanged>(64);

    let raw_tx_for_cb = raw_tx.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(ev) => ev,
            Err(e) => {
                warn!(error = %e, "compose_watcher: notify error");
                return;
            }
        };
        // Only care about creates / writes / renames. Removes are followed
        // by creates for atomic-rename editors; we react to the create.
        if !matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            return;
        }
        for path in event.paths {
            if path.file_name().and_then(|s| s.to_str()) != Some("compose.yaml") {
                continue;
            }
            let raw = RawEvent {
                path,
                kind: event.kind,
            };
            // Best-effort: the consumer is faster than the filesystem
            // event rate by orders of magnitude, but if the receiver
            // hung up the event drops.
            match raw_tx_for_cb.try_send(raw) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => warn!("compose_watcher: raw channel full, dropping"),
                Err(TrySendError::Closed(_)) => debug!("compose_watcher: raw channel closed"),
            }
        }
    })
    .map_err(|e| anyhow::anyhow!("create notify watcher: {e}"))?;

    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| anyhow::anyhow!("watch {}: {e}", root.display()))?;

    let root_for_task = root.clone();
    tokio::spawn(async move {
        // Per-path coalesce: store last-event time keyed by `compose.yaml`
        // path. A periodic flush ticker scans for entries whose deadline
        // has passed and emits one outbound event per path.
        let mut pending: HashMap<PathBuf, tokio::time::Instant> = HashMap::new();
        let tick_period = (debounce / 5).max(Duration::from_millis(20));
        let mut ticker = tokio::time::interval(tick_period);
        loop {
            tokio::select! {
                maybe_raw = raw_rx.recv() => {
                    let Some(raw) = maybe_raw else { break };
                    pending.insert(raw.path, tokio::time::Instant::now() + debounce);
                }
                _ = ticker.tick() => {
                    let now = tokio::time::Instant::now();
                    let ready: Vec<PathBuf> = pending
                        .iter()
                        .filter_map(|(p, deadline)| if *deadline <= now { Some(p.clone()) } else { None })
                        .collect();
                    for path in ready {
                        pending.remove(&path);
                        let Some(stack) = stack_name_for(&root_for_task, &path) else {
                            debug!(path = %path.display(), "compose_watcher: ignoring path outside root");
                            continue;
                        };
                        if !path.exists() {
                            // File deleted (the editor's atomic rename
                            // raced ahead of us). The next create will
                            // produce its own event; skip this one.
                            continue;
                        }
                        let evt = StackComposeChanged { stack_name: stack, path };
                        match out_tx.try_send(evt) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => warn!("compose_watcher: out channel full, dropping"),
                            Err(TrySendError::Closed(_)) => return,
                        }
                    }
                }
            }
        }
    });

    Ok((out_rx, watcher))
}

#[derive(Debug)]
/// Internal struct: RawEvent.
struct RawEvent {
    /// `path` field.
    path: PathBuf,
    #[allow(dead_code)]
    /// `kind` field.
    kind: EventKind,
}

/// Given a watched root and a child path, derive the stack name (the
/// directory immediately under the root). Returns `None` for paths that
/// don't live directly under `<root>/<stack>/compose.yaml`.
fn stack_name_for(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut comps = rel.components();
    let stack = comps.next()?.as_os_str().to_str()?.to_string();
    // Must be exactly `<stack>/compose.yaml`; deeper paths (e.g.
    // `<stack>/.git/HEAD`) are ignored.
    let file = comps.next()?.as_os_str().to_str()?;
    if comps.next().is_some() || file != "compose.yaml" {
        return None;
    }
    Some(stack)
}

/// Convenience: spawn the watcher and a reconcile-driving task that
/// invokes `on_change` for each debounced event. Used by `run_agent`.
pub fn spawn_with_callback<F, Fut>(
    root: PathBuf,
    on_change: F,
) -> anyhow::Result<notify::RecommendedWatcher>
where
    F: Fn(StackComposeChanged) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let (mut rx, watcher) = spawn(root)?;
    let cb = Arc::new(on_change);
    tokio::spawn(async move {
        while let Some(evt) = rx.recv().await {
            cb(evt).await;
        }
    });
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stack_name_extracted_from_compose_path() {
        let root = Path::new("/etc/isengard/stacks");
        let p = Path::new("/etc/isengard/stacks/hello/compose.yaml");
        assert_eq!(stack_name_for(root, p).as_deref(), Some("hello"));
    }

    #[test]
    fn stack_name_rejects_subdir_files() {
        let root = Path::new("/etc/isengard/stacks");
        let p = Path::new("/etc/isengard/stacks/hello/.git/HEAD");
        assert_eq!(stack_name_for(root, p), None);
    }

    #[test]
    fn stack_name_rejects_non_compose_filename() {
        let root = Path::new("/etc/isengard/stacks");
        let p = Path::new("/etc/isengard/stacks/hello/meta.toml");
        assert_eq!(stack_name_for(root, p), None);
    }

    #[tokio::test]
    async fn watcher_emits_event_after_write() {
        let dir = tempfile::tempdir().unwrap();
        let stack_dir = dir.path().join("hello");
        std::fs::create_dir_all(&stack_dir).unwrap();

        let (mut rx, _w) =
            spawn_with_debounce(dir.path().to_path_buf(), Duration::from_millis(50)).unwrap();

        let compose = stack_dir.join("compose.yaml");
        std::fs::write(&compose, "services: {}\n").unwrap();

        let evt = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("watcher emitted no event in time")
            .expect("channel closed");
        assert_eq!(evt.stack_name, "hello");
        // Canonicalize both sides: macOS FSEvents reports
        // `/private/var/folders/...` even when we watch `/var/folders/...`.
        let canonical_compose = std::fs::canonicalize(&compose).unwrap();
        assert_eq!(evt.path, canonical_compose);
    }

    #[tokio::test]
    async fn watcher_debounces_burst_writes() {
        let dir = tempfile::tempdir().unwrap();
        let stack_dir = dir.path().join("hello");
        std::fs::create_dir_all(&stack_dir).unwrap();

        let (mut rx, _w) =
            spawn_with_debounce(dir.path().to_path_buf(), Duration::from_millis(80)).unwrap();

        let compose = stack_dir.join("compose.yaml");
        // Burst of writes inside the debounce window.
        for i in 0..5 {
            std::fs::write(&compose, format!("services: {{ x: {i} }}\n")).unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // First event arrives after the debounce settles.
        let _ = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("watcher emitted no event")
            .expect("channel closed");

        // Verify at least one event landed; the channel may have a
        // second event if FSEvents coalesced unevenly. We only assert
        // that the burst didn't produce 5 events.
        let mut extras = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(40), rx.recv()).await {
                Ok(Some(_)) => extras += 1,
                _ => break,
            }
        }
        assert!(
            extras < 5,
            "expected debounce to collapse the burst; saw {} extras",
            extras
        );
    }
}
