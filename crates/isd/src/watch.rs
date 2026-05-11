//! `isd deploy --watch`: stream a deploy's progress back to the operator's
//! terminal.
//!
//! Today `isd deploy` is fire-and-forget: the PUT or POST that ships the
//! compose returns the moment the controller has persisted the YAML, and
//! the operator stares at a blinking cursor while the agent pulls images,
//! creates containers, and reports state back. `--watch` keeps the isd
//! process attached after the write succeeds and renders per-service state
//! transitions until everything reaches a terminal state.
//!
//! ## Channel
//!
//! Polling, not server-sent events. We hit two endpoints already exposed by
//! every v0.5.x controller:
//!
//!   - `GET /api/v1/stacks` once at the start (to look up the host hostname
//!     for the watch header)
//!   - `GET /api/v1/services?stack_id=<id>` every 1s
//!
//! Real event streams (`/api/v1/deployments/<id>/events` SSE) would render
//! cleaner; deferred to v0.5.3+ so this lands without a controller change.
//!
//! ## Terminal states
//!
//! v0.5.3 extended `ServiceState` to surface mid-startup transitions
//! (`pulling`, `creating`, `restarting`) and the explicit `failed` terminal.
//! The spec's rule "Running, Failed, Stopped are terminal" maps cleanly:
//!
//!   - Terminal: `running`, `stopped`, `failed`
//!   - Intermediate: `pulling`, `creating`, `restarting`, `unknown`
//!
//! `unknown` stays intermediate so a service that the agent reports
//! before the heartbeat carries a recognisable state string doesn't
//! short-circuit the watch loop to a misleading "all terminal" outcome.
//!
//! ## Timeout + Ctrl+C
//!
//! - 10 minutes with no state transition: warn + exit 0 with a hint to
//!   re-check via `isd ps`.
//! - SIGINT: print "Detached." and exit 0. The deploy continues on the
//!   agent; this just stops the local watch loop.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::session::Session;

/// Poll cadence. 1s is the spec target; tests override via a builder so
/// they can fast-forward without `tokio::time::pause`.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Hard-stop after this long without observing any state transition. The
/// agent may still be reconciling (slow image pull, hook still running);
/// we surface the timeout so the operator isn't stuck and tell them to
/// re-check with `isd ps`.
pub const DEFAULT_NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// One row of `GET /api/v1/services?stack_id=...`. Subset of [`crate::ps`]'s
/// `ServiceDto`: we only care about identity, state, image, and the
/// `last_seen_at` heartbeat (for "took Xs" computation when the controller
/// first observes a service after we start watching).
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceSnapshot {
    /// Per-service primary key (rendered as a string in JSON for consistency
    /// with other DTOs). Used as the dedup key in the diff loop.
    pub id: String,
    pub name: String,
    pub state: String,
    pub image: String,
    #[allow(dead_code)] // surfaced in future "container_id" enrichment
    pub last_seen_at: DateTime<Utc>,
}

/// Stack header info: name + host hostname (or truncated ULID fallback).
/// Resolved once at the start of the watch, so the intro line can read
/// `Watching deployment of servarr on lausanne` instead of just an id.
#[derive(Debug, Clone)]
pub struct StackHeader {
    pub stack_id: String,
    pub stack_name: String,
    pub host_label: String,
}

/// Per-service progress tracker. Holds the state observed at the previous
/// poll plus a monotonic `Instant` of when we first saw the service so the
/// outro can render "took Xs" for terminal transitions.
#[derive(Debug, Clone)]
struct ServiceTracker {
    /// Service name as last observed. Carried for future "still pending"
    /// outro lines + debug traces; read only by tests today.
    #[allow(dead_code)]
    name: String,
    last_state: String,
    /// Image string as last observed; kept alongside `name` for the same
    /// future-use reasons.
    #[allow(dead_code)]
    image: String,
    first_seen: Instant,
    /// Cached at the moment the service first reaches a terminal state so
    /// the outro and the rendered transition line agree on the duration
    /// (the line is printed when we observe the transition; the outro
    /// reads the same value).
    terminal_at: Option<Instant>,
}

impl ServiceTracker {
    /// Wall-clock duration from first sighting to terminal state (or to
    /// `now` if still pending). Exposed via `cfg(test)` only: used by
    /// the tracker unit test to assert the behaviour, no production
    /// caller today (the renderer computes `took` directly from the
    /// transition timestamps).
    #[cfg(test)]
    fn took(&self) -> Duration {
        self.terminal_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.first_seen)
    }
}

/// Outcome of the watch loop. Used by the caller to set the process exit
/// status and by tests to assert behaviour without parsing stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchOutcome {
    /// Every service reached `Running`. Outro green.
    AllRunning { services: usize, elapsed: Duration },
    /// At least one service reached a non-Running terminal state. Outro red.
    SomeFailed {
        running: usize,
        failed: usize,
        elapsed: Duration,
    },
    /// 10-minute no-transition timer fired. Outro neutral; the agent may
    /// still be working.
    Timeout { last_progress: Duration },
    /// SIGINT. Outro neutral.
    Detached,
}

impl WatchOutcome {
    /// Map an outcome to an exit-status-like Result. `AllRunning` and
    /// `Detached` are successes; `SomeFailed` is the only Err. `Timeout`
    /// is treated as success because the deploy may still complete on
    /// the agent (failure here would be misleading).
    pub fn into_result(self) -> Result<()> {
        match self {
            WatchOutcome::AllRunning { .. }
            | WatchOutcome::Timeout { .. }
            | WatchOutcome::Detached => Ok(()),
            WatchOutcome::SomeFailed { failed, .. } => Err(anyhow::anyhow!(
                "{failed} service(s) did not reach Running before the watch loop exited"
            )),
        }
    }
}

/// Classify a service state string into "this is a terminal end-state".
///
/// v0.5.3: matches the extended `ServiceState` (running / stopped /
/// failed are terminal; pulling / creating / restarting / unknown are
/// intermediate). Unknown stays intermediate so the watch loop keeps
/// polling instead of short-circuiting on a service the agent hasn't
/// yet reported a recognisable state for.
pub fn is_terminal(state: &str) -> bool {
    matches!(state, "running" | "stopped" | "failed")
}

/// Is this a "happy" terminal state? Used to colour the outro.
pub fn is_success_state(state: &str) -> bool {
    state == "running"
}

/// Inverse of [`is_terminal`] for the new v0.5.3 mid-startup states.
/// Exposed alongside `is_terminal` so callers (and the indicatif
/// renderer landing under #89) can label "still working" rows without
/// re-encoding the classification rule.
///
/// `#[allow(dead_code)]`: today only the test suite consumes this;
/// the production caller arrives once the watch renderer migrates to
/// indicatif MultiProgress.
#[allow(dead_code)]
pub fn is_intermediate(state: &str) -> bool {
    matches!(
        state,
        "pulling" | "creating" | "starting" | "restarting" | "unknown"
    )
}

/// Pretty-format a [`Duration`] as `Xs` (rounded to seconds, minimum 1s).
fn fmt_secs(d: Duration) -> String {
    let s = d.as_secs().max(1);
    format!("{s}s")
}

/// Render the cliclack "transition" line for a service.
///
/// Format: `service-name<pad>state<pad>image | took Xs`.
///
/// Picks a glyph variant: `log::step` (◇) for intermediate / non-terminal
/// transitions, `log::success` (◆ green) for `running`, `log::error` (✗ red)
/// for `failed`, and `log::warning` for `stopped` (terminal but not happy).
///
/// Separated out so tests can compute the expected string without invoking
/// cliclack's stdout side-effect.
pub fn format_transition_line(svc: &ServiceSnapshot, took: Option<Duration>) -> String {
    let took_suffix = match took {
        Some(d) => format!("  (took {})", fmt_secs(d)),
        None => String::new(),
    };
    format!(
        "{:<28} {:<12} {}{}",
        svc.name, svc.state, svc.image, took_suffix
    )
}

/// The watch loop's polling source. Trait-ified so tests can substitute a
/// scripted in-memory implementation without spinning up wiremock.
#[async_trait::async_trait]
pub trait WatchPoller: Send + Sync {
    /// Fetch the current set of services for the stack. Returning an
    /// empty Vec means "no services yet observed by the controller"; the
    /// watch loop treats that as a non-terminal state and keeps polling.
    async fn poll_services(&self) -> Result<Vec<ServiceSnapshot>>;
}

/// HTTP polling implementation backed by [`Session`]. The default for
/// production.
pub struct HttpPoller<'a> {
    pub session: &'a Session,
    pub stack_id: String,
}

#[async_trait::async_trait]
impl<'a> WatchPoller for HttpPoller<'a> {
    async fn poll_services(&self) -> Result<Vec<ServiceSnapshot>> {
        // The dashboard's `list_services` accepts `?stack_id=<i64>`; the
        // ULID-shaped contexts can't reach here (StackId is i64) so we
        // pass the string straight through.
        let url = format!(
            "{}/api/v1/services?stack_id={}",
            self.session.controller_url(),
            self.stack_id
        );
        let resp = self
            .session
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let snapshots: Vec<ServiceSnapshot> = resp
            .error_for_status()
            .with_context(|| format!("GET {url}"))?
            .json()
            .await
            .context("decoding services JSON")?;
        Ok(snapshots)
    }
}

/// Resolve a stack id + name into [`StackHeader`] by walking the
/// `GET /api/v1/stacks` + `GET /api/v1/hosts` payloads. Best-effort: if
/// hosts can't be enumerated we fall back to the truncated ULID for the
/// host label so the watch banner still renders.
pub async fn fetch_stack_header(session: &Session, stack_id: &str) -> Result<StackHeader> {
    #[derive(Deserialize)]
    struct StackDto {
        id: String,
        host_id: String,
        name: String,
    }
    #[derive(Deserialize)]
    struct HostDto {
        id: String,
        hostname: String,
    }
    let stacks_url = format!("{}/api/v1/stacks", session.controller_url());
    let stacks: Vec<StackDto> = session
        .client
        .get(&stacks_url)
        .send()
        .await
        .with_context(|| format!("GET {stacks_url}"))?
        .error_for_status()?
        .json()
        .await
        .context("decoding stacks JSON")?;
    let stack = stacks
        .into_iter()
        .find(|s| s.id == stack_id)
        .with_context(|| format!("stack {stack_id} not found"))?;

    // Hostname lookup is best-effort; pre-0.5 controllers may 404 or
    // return an empty array on /api/v1/hosts. Fall back to a short
    // suffix of the ULID so the header is never empty.
    let hosts_url = format!("{}/api/v1/hosts", session.controller_url());
    let hostname = match session.client.get(&hosts_url).send().await {
        Ok(resp) => match resp.error_for_status() {
            Ok(ok) => ok
                .json::<Vec<HostDto>>()
                .await
                .ok()
                .and_then(|hs| hs.into_iter().find(|h| h.id == stack.host_id))
                .map(|h| h.hostname),
            Err(_) => None,
        },
        Err(_) => None,
    };
    let host_label = hostname.unwrap_or_else(|| {
        // ULID prefix is fine for the header; the full id is in `isd ps`.
        stack.host_id.chars().take(8).collect::<String>()
    });
    Ok(StackHeader {
        stack_id: stack.id,
        stack_name: stack.name,
        host_label,
    })
}

/// Configuration knob for [`watch_until_terminal`]. Tests inject a tight
/// `poll_interval` so they don't sit for 1s per tick; production uses the
/// defaults from this module.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    pub poll_interval: Duration,
    pub no_progress_timeout: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            no_progress_timeout: DEFAULT_NO_PROGRESS_TIMEOUT,
        }
    }
}

/// Drive the poll loop until every service is in a terminal state, the
/// no-progress timer fires, or `cancel` signals (Ctrl+C from the caller).
///
/// Returns the outcome so the caller can colour the outro + set the exit
/// status. Side-effect: emits one cliclack line per observed transition.
///
/// `cancel` is a future the caller resolves on SIGINT. Tests pass a
/// `std::future::pending()` to drive the timeout path explicitly.
pub async fn watch_until_terminal<P, C>(
    poller: P,
    cancel: C,
    config: WatchConfig,
    renderer: &mut dyn TransitionRenderer,
) -> Result<WatchOutcome>
where
    P: WatchPoller,
    C: std::future::Future<Output = ()> + Unpin,
{
    let start = Instant::now();
    let mut last_progress = Instant::now();
    let mut tracked: HashMap<String, ServiceTracker> = HashMap::new();
    tokio::pin!(cancel);

    loop {
        let services = poller.poll_services().await?;
        let mut any_change = false;

        // Detect new / changed services.
        for svc in &services {
            match tracked.get_mut(&svc.id) {
                None => {
                    // New service observed for the first time. Treat the
                    // first sighting as a transition (so we render its
                    // initial state) and record `first_seen` for "took".
                    let terminal_at = if is_terminal(&svc.state) {
                        Some(Instant::now())
                    } else {
                        None
                    };
                    let took_now = if terminal_at.is_some() {
                        Some(Duration::from_secs(0))
                    } else {
                        None
                    };
                    renderer.transition(svc, took_now);
                    tracked.insert(
                        svc.id.clone(),
                        ServiceTracker {
                            name: svc.name.clone(),
                            last_state: svc.state.clone(),
                            image: svc.image.clone(),
                            first_seen: Instant::now(),
                            terminal_at,
                        },
                    );
                    any_change = true;
                }
                Some(t) if t.last_state != svc.state => {
                    let now = Instant::now();
                    if is_terminal(&svc.state) && t.terminal_at.is_none() {
                        t.terminal_at = Some(now);
                    }
                    let took = if is_terminal(&svc.state) {
                        Some(now.saturating_duration_since(t.first_seen))
                    } else {
                        None
                    };
                    renderer.transition(svc, took);
                    t.last_state = svc.state.clone();
                    t.image = svc.image.clone();
                    any_change = true;
                }
                Some(_) => {} // no-op: same state as last tick
            }
        }

        if any_change {
            last_progress = Instant::now();
        }

        // Termination criteria: at least one service has been observed
        // AND every tracked service is in a terminal state.
        if !tracked.is_empty() && tracked.values().all(|t| is_terminal(&t.last_state)) {
            let elapsed = start.elapsed();
            let running = tracked
                .values()
                .filter(|t| is_success_state(&t.last_state))
                .count();
            let failed = tracked.len() - running;
            return Ok(if failed == 0 {
                WatchOutcome::AllRunning {
                    services: running,
                    elapsed,
                }
            } else {
                WatchOutcome::SomeFailed {
                    running,
                    failed,
                    elapsed,
                }
            });
        }

        // No-progress timeout. Bumps every time we observe a transition;
        // a stuck deploy that's been sitting in `restarting` for 10
        // minutes triggers the warning path.
        if last_progress.elapsed() >= config.no_progress_timeout {
            return Ok(WatchOutcome::Timeout {
                last_progress: last_progress.elapsed(),
            });
        }

        // Wait for either the next tick or Ctrl+C, whichever comes first.
        tokio::select! {
            _ = tokio::time::sleep(config.poll_interval) => {}
            _ = &mut cancel => {
                return Ok(WatchOutcome::Detached);
            }
        }
    }
}

/// Side-channel that takes a single transition and writes it somewhere
/// (production: cliclack glyph + stderr; tests: a Vec<String>). Pulled
/// out so the state-machine tests don't depend on cliclack's global
/// terminal state.
pub trait TransitionRenderer {
    fn transition(&mut self, svc: &ServiceSnapshot, took: Option<Duration>);
}

/// Production renderer: each transition lands on stderr through cliclack's
/// connector. Terminal-running -> `log::success` (green ◆), terminal-stopped
/// / -failed -> `log::error` (red ✗), pulling / creating / restarting /
/// unknown -> `log::step` (grey ◇).
pub struct CliclackRenderer;

impl TransitionRenderer for CliclackRenderer {
    fn transition(&mut self, svc: &ServiceSnapshot, took: Option<Duration>) {
        let line = format_transition_line(svc, took);
        let _ = match svc.state.as_str() {
            "running" => cliclack::log::success(line),
            "stopped" | "failed" => cliclack::log::error(line),
            // pulling / creating / restarting / unknown all land here:
            // intermediate transitions render as ◇ so the operator can
            // see the agent making progress without misreading them as
            // terminal failures.
            _ => cliclack::log::step(line),
        };
    }
}

/// Test renderer: capture transitions into a Vec without touching the
/// terminal. The Vec is shared via Arc<Mutex<...>> so the test can read
/// it after `watch_until_terminal` returns.
#[cfg(test)]
#[derive(Default)]
pub struct CaptureRenderer {
    pub lines: Vec<String>,
}

#[cfg(test)]
impl TransitionRenderer for CaptureRenderer {
    fn transition(&mut self, svc: &ServiceSnapshot, took: Option<Duration>) {
        self.lines.push(format_transition_line(svc, took));
    }
}

/// Print the outro line that closes a watch session. Lives in this module
/// so the colouring rules are alongside the rest of the renderer.
pub fn print_outro(outcome: &WatchOutcome) {
    match outcome {
        WatchOutcome::AllRunning { services, elapsed } => {
            let _ = cliclack::outro(format!(
                "All {services} service(s) running. Total: {}.",
                fmt_secs(*elapsed)
            ));
        }
        WatchOutcome::SomeFailed {
            running,
            failed,
            elapsed,
        } => {
            let _ = cliclack::outro_cancel(format!(
                "{running} running, {failed} did not reach Running. Total: {}.",
                fmt_secs(*elapsed)
            ));
        }
        WatchOutcome::Timeout { last_progress } => {
            let _ = cliclack::log::warning(format!(
                "no state transitions for {}; the agent may still be working: re-check with `isd ps`",
                fmt_secs(*last_progress)
            ));
            let _ = cliclack::outro("Watch detached on timeout.");
        }
        WatchOutcome::Detached => {
            let _ = cliclack::outro("Detached. Deploy continues on agent.");
        }
    }
}

/// Public entrypoint used by `compose_cmd.rs` after a successful deploy.
/// Resolves the stack header, renders the intro, runs the watch loop, then
/// prints the outro. Wraps the result so callers can `?` it directly.
pub async fn run_watch(session: &Session, stack_id: &str) -> Result<()> {
    let header = fetch_stack_header(session, stack_id).await?;
    cliclack::intro(format!("isd deploy --watch  {}", header.stack_name))?;
    cliclack::log::step(format!(
        "Watching deployment {} on {}",
        header.stack_id, header.host_label
    ))?;

    let poller = HttpPoller {
        session,
        stack_id: stack_id.to_string(),
    };
    let cancel = Box::pin(async {
        // tokio::signal::ctrl_c errors only when the runtime can't install
        // the handler; treat the error as "no cancel ever".
        let _ = tokio::signal::ctrl_c().await;
    });
    let mut renderer = CliclackRenderer;
    let outcome =
        watch_until_terminal(poller, cancel, WatchConfig::default(), &mut renderer).await?;
    print_outro(&outcome);
    outcome.into_result()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;
    use std::sync::{Arc, Mutex};
    use tokio::time::Duration as TDuration;

    fn snap(id: &str, name: &str, state: &str, image: &str) -> ServiceSnapshot {
        ServiceSnapshot {
            id: id.into(),
            name: name.into(),
            state: state.into(),
            image: image.into(),
            last_seen_at: Utc::now(),
        }
    }

    #[test]
    fn terminal_classifier_matches_spec() {
        // v0.5.3: ServiceState exposes pulling / creating / restarting
        // as intermediate; running / stopped / failed as terminal;
        // unknown is treated as intermediate so the watch loop keeps
        // polling instead of short-circuiting on a service the agent
        // hasn't yet reported a recognised state for.
        assert!(is_terminal("running"));
        assert!(is_terminal("stopped"));
        assert!(is_terminal("failed"));
        assert!(!is_terminal("pulling"));
        assert!(!is_terminal("creating"));
        assert!(!is_terminal("restarting"));
        assert!(!is_terminal("unknown"));
    }

    #[test]
    fn intermediate_classifier_covers_mid_startup_states() {
        // The v0.5.3 extension's mid-startup states must all classify
        // as intermediate so the watch loop keeps drawing progress.
        assert!(is_intermediate("pulling"));
        assert!(is_intermediate("creating"));
        assert!(is_intermediate("starting"));
        assert!(is_intermediate("restarting"));
        assert!(is_intermediate("unknown"));
        // Terminal states are NOT intermediate; the two classifiers
        // partition the state space (modulo any unrecognised string).
        assert!(!is_intermediate("running"));
        assert!(!is_intermediate("stopped"));
        assert!(!is_intermediate("failed"));
    }

    #[test]
    fn success_state_is_running_only() {
        assert!(is_success_state("running"));
        assert!(!is_success_state("stopped"));
        assert!(!is_success_state("failed"));
    }

    #[test]
    fn format_transition_line_renders_columns_and_optional_took() {
        let svc = snap(
            "1",
            "servarr-radarr",
            "pulling",
            "lscr.io/lsio/radarr:latest",
        );
        let line = format_transition_line(&svc, None);
        assert!(line.contains("servarr-radarr"));
        assert!(line.contains("pulling"));
        assert!(line.contains("radarr:latest"));
        assert!(!line.contains("took"));

        let took_line = format_transition_line(&svc, Some(Duration::from_secs(22)));
        assert!(took_line.contains("took 22s"));
    }

    #[test]
    fn outcome_into_result_maps_failure_to_err() {
        let ok = WatchOutcome::AllRunning {
            services: 3,
            elapsed: Duration::from_secs(10),
        };
        assert!(ok.into_result().is_ok());
        let fail = WatchOutcome::SomeFailed {
            running: 1,
            failed: 2,
            elapsed: Duration::from_secs(15),
        };
        assert!(fail.into_result().is_err());
        let timeout = WatchOutcome::Timeout {
            last_progress: Duration::from_secs(600),
        };
        assert!(timeout.into_result().is_ok());
        assert!(WatchOutcome::Detached.into_result().is_ok());
    }

    /// Scripted poller: returns a sequence of canned snapshots, one per
    /// tick. Replays the last entry forever once exhausted so the watch
    /// loop has something to read until its termination criteria kicks in.
    struct ScriptedPoller {
        ticks: Arc<Mutex<Vec<Vec<ServiceSnapshot>>>>,
        cursor: Arc<Mutex<usize>>,
    }

    #[async_trait::async_trait]
    impl WatchPoller for ScriptedPoller {
        async fn poll_services(&self) -> Result<Vec<ServiceSnapshot>> {
            let ticks = self.ticks.lock().unwrap();
            let mut cursor = self.cursor.lock().unwrap();
            let idx = (*cursor).min(ticks.len().saturating_sub(1));
            *cursor += 1;
            Ok(ticks.get(idx).cloned().unwrap_or_default())
        }
    }

    /// Walking the Pending -> Running transition for a single service
    /// emits one line per observed state and terminates with AllRunning.
    #[tokio::test(flavor = "current_thread")]
    async fn state_machine_walks_pending_to_running() {
        let ticks = vec![
            // tick 0: agent hasn't reported anything yet
            vec![],
            // tick 1: service appears in `restarting` (treated like
            // "pulling" intermediate)
            vec![snap("1", "web", "restarting", "nginx:1")],
            // tick 2: container up
            vec![snap("1", "web", "running", "nginx:1")],
            // tick 3: steady-state (loop should already have exited)
            vec![snap("1", "web", "running", "nginx:1")],
        ];
        let poller = ScriptedPoller {
            ticks: Arc::new(Mutex::new(ticks)),
            cursor: Arc::new(Mutex::new(0)),
        };
        let mut renderer = CaptureRenderer::default();
        let outcome = watch_until_terminal(
            poller,
            Box::pin(pending::<()>()),
            WatchConfig {
                poll_interval: TDuration::from_millis(10),
                no_progress_timeout: TDuration::from_secs(60),
            },
            &mut renderer,
        )
        .await
        .unwrap();

        // Two transitions: first sighting at `restarting`, then `running`.
        assert_eq!(renderer.lines.len(), 2, "lines: {:#?}", renderer.lines);
        assert!(renderer.lines[0].contains("restarting"));
        assert!(renderer.lines[1].contains("running"));
        assert!(renderer.lines[1].contains("took"));
        assert!(matches!(
            outcome,
            WatchOutcome::AllRunning { services: 1, .. }
        ));
    }

    /// v0.5.3: walks the full pulling -> creating -> running sequence
    /// the extended `ServiceState` enum now exposes. Every intermediate
    /// step renders a transition line; only `running` is terminal.
    #[tokio::test(flavor = "current_thread")]
    async fn state_machine_walks_pulling_creating_running() {
        let ticks = vec![
            vec![snap("1", "web", "pulling", "nginx:1")],
            vec![snap("1", "web", "creating", "nginx:1")],
            vec![snap("1", "web", "running", "nginx:1")],
        ];
        let poller = ScriptedPoller {
            ticks: Arc::new(Mutex::new(ticks)),
            cursor: Arc::new(Mutex::new(0)),
        };
        let mut renderer = CaptureRenderer::default();
        let outcome = watch_until_terminal(
            poller,
            Box::pin(pending::<()>()),
            WatchConfig {
                poll_interval: TDuration::from_millis(10),
                no_progress_timeout: TDuration::from_secs(60),
            },
            &mut renderer,
        )
        .await
        .unwrap();

        assert_eq!(renderer.lines.len(), 3, "lines: {:#?}", renderer.lines);
        assert!(renderer.lines[0].contains("pulling"));
        assert!(renderer.lines[1].contains("creating"));
        assert!(renderer.lines[2].contains("running"));
        assert!(renderer.lines[2].contains("took"));
        assert!(matches!(
            outcome,
            WatchOutcome::AllRunning { services: 1, .. }
        ));
    }

    /// Multi-service: mixed terminal states (one running, one stopped)
    /// must yield SomeFailed.
    #[tokio::test(flavor = "current_thread")]
    async fn mixed_terminal_states_yield_some_failed() {
        let ticks = vec![
            vec![
                snap("1", "web", "restarting", "nginx:1"),
                snap("2", "db", "restarting", "postgres:16"),
            ],
            vec![
                snap("1", "web", "running", "nginx:1"),
                snap("2", "db", "stopped", "postgres:16"),
            ],
        ];
        let poller = ScriptedPoller {
            ticks: Arc::new(Mutex::new(ticks)),
            cursor: Arc::new(Mutex::new(0)),
        };
        let mut renderer = CaptureRenderer::default();
        let outcome = watch_until_terminal(
            poller,
            Box::pin(pending::<()>()),
            WatchConfig {
                poll_interval: TDuration::from_millis(10),
                no_progress_timeout: TDuration::from_secs(60),
            },
            &mut renderer,
        )
        .await
        .unwrap();
        match outcome {
            WatchOutcome::SomeFailed {
                running, failed, ..
            } => {
                assert_eq!(running, 1);
                assert_eq!(failed, 1);
            }
            other => panic!("expected SomeFailed, got {other:?}"),
        }
    }

    /// SIGINT-equivalent: the cancel future resolves before the loop
    /// reaches a terminal state. Outcome must be `Detached`.
    #[tokio::test(flavor = "current_thread")]
    async fn cancel_future_yields_detached() {
        let ticks = vec![
            vec![snap("1", "web", "restarting", "nginx:1")],
            // never reaches a terminal state on its own
        ];
        let poller = ScriptedPoller {
            ticks: Arc::new(Mutex::new(ticks)),
            cursor: Arc::new(Mutex::new(0)),
        };
        let mut renderer = CaptureRenderer::default();
        let cancel = Box::pin(async {
            tokio::time::sleep(TDuration::from_millis(50)).await;
        });
        let outcome = watch_until_terminal(
            poller,
            cancel,
            WatchConfig {
                poll_interval: TDuration::from_millis(10),
                no_progress_timeout: TDuration::from_secs(60),
            },
            &mut renderer,
        )
        .await
        .unwrap();
        assert_eq!(outcome, WatchOutcome::Detached);
    }

    /// Timeout: the poller forever returns the same intermediate state;
    /// the `no_progress_timeout` knob fires and yields a `Timeout`
    /// outcome.
    #[tokio::test(flavor = "current_thread")]
    async fn no_progress_timeout_fires() {
        let ticks = vec![vec![snap("1", "web", "restarting", "nginx:1")]];
        let poller = ScriptedPoller {
            ticks: Arc::new(Mutex::new(ticks)),
            cursor: Arc::new(Mutex::new(0)),
        };
        let mut renderer = CaptureRenderer::default();
        let outcome = watch_until_terminal(
            poller,
            Box::pin(pending::<()>()),
            WatchConfig {
                poll_interval: TDuration::from_millis(5),
                no_progress_timeout: TDuration::from_millis(50),
            },
            &mut renderer,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, WatchOutcome::Timeout { .. }));
        // Exactly one line: the first-sighting of "restarting". No
        // subsequent transitions were observed.
        assert_eq!(renderer.lines.len(), 1);
    }

    /// `--watch` against a stack that the agent never reports for ends
    /// in `Timeout`, NOT `AllRunning`. This guards against the bug where
    /// the loop would exit "successfully" because `tracked.is_empty()`
    /// is true on tick 0.
    #[tokio::test(flavor = "current_thread")]
    async fn empty_service_list_does_not_short_circuit_to_success() {
        let ticks: Vec<Vec<ServiceSnapshot>> = vec![vec![]];
        let poller = ScriptedPoller {
            ticks: Arc::new(Mutex::new(ticks)),
            cursor: Arc::new(Mutex::new(0)),
        };
        let mut renderer = CaptureRenderer::default();
        let outcome = watch_until_terminal(
            poller,
            Box::pin(pending::<()>()),
            WatchConfig {
                poll_interval: TDuration::from_millis(5),
                no_progress_timeout: TDuration::from_millis(50),
            },
            &mut renderer,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, WatchOutcome::Timeout { .. }));
    }

    /// ServiceTracker.took() falls back to "now - first_seen" before the
    /// terminal_at timestamp is recorded. Used by the outro path in case
    /// we ever surface a "still pending" line.
    #[test]
    fn tracker_took_uses_terminal_at_when_set() {
        let mut t = ServiceTracker {
            name: "web".into(),
            last_state: "running".into(),
            image: "nginx".into(),
            first_seen: Instant::now() - Duration::from_secs(10),
            terminal_at: Some(Instant::now() - Duration::from_secs(2)),
        };
        let d = t.took();
        // ~8s: 10s ago first_seen, 2s ago terminal_at.
        assert!(d.as_secs() >= 7 && d.as_secs() <= 9, "got {:?}", d);
        t.terminal_at = None;
        let d2 = t.took();
        assert!(d2.as_secs() >= 9, "got {:?}", d2);
    }

    /// Integration test: stand up a wiremock controller that returns a
    /// canned `Vec<ServiceSnapshot>` and drive the [`HttpPoller`] end-to-
    /// end through [`watch_until_terminal`]. Verifies the JSON round-trip
    /// + the URL shape (`?stack_id=`). `#[ignore]` because it touches
    /// the process-global credentials file env var.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn integration_full_cycle_against_wiremock() {
        use crate::credentials::{Backend, ContextEntry};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

        let server = MockServer::start().await;

        /// Wiremock `Respond` impl that walks a state-machine: each call
        /// returns the next step's snapshot list. Once exhausted it
        /// keeps replaying the final step.
        struct StepResponder {
            counter: Arc<AtomicUsize>,
        }
        impl Respond for StepResponder {
            fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
                let step = self.counter.fetch_add(1, Ordering::SeqCst);
                let body = match step {
                    0 => serde_json::json!([
                        { "id": "1", "host_id": "01HX", "stack_id": "42",
                          "name": "web", "image": "nginx:1", "state": "restarting",
                          "last_seen_at": "2026-05-11T00:00:00Z" }
                    ]),
                    1 => serde_json::json!([
                        { "id": "1", "host_id": "01HX", "stack_id": "42",
                          "name": "web", "image": "nginx:1", "state": "running",
                          "last_seen_at": "2026-05-11T00:00:01Z" }
                    ]),
                    _ => serde_json::json!([
                        { "id": "1", "host_id": "01HX", "stack_id": "42",
                          "name": "web", "image": "nginx:1", "state": "running",
                          "last_seen_at": "2026-05-11T00:00:02Z" }
                    ]),
                };
                ResponseTemplate::new(200).set_body_json(body)
            }
        }

        let counter = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/api/v1/services"))
            .and(query_param("stack_id", "42"))
            .respond_with(StepResponder {
                counter: counter.clone(),
            })
            .mount(&server)
            .await;

        // Build a session pointing at the mock server directly. Skips
        // the credentials-file round-trip the higher-level entrypoint
        // does (we're testing the loop, not the credentials parser).
        let ctx = ContextEntry {
            name: "test".into(),
            backend: Backend::Http { url: server.uri() },
        };
        let session = Session::from_context(ctx).await.unwrap();

        let poller = HttpPoller {
            session: &session,
            stack_id: "42".into(),
        };
        let mut renderer = CaptureRenderer::default();
        let outcome = watch_until_terminal(
            poller,
            Box::pin(std::future::pending::<()>()),
            WatchConfig {
                poll_interval: Duration::from_millis(20),
                no_progress_timeout: Duration::from_secs(5),
            },
            &mut renderer,
        )
        .await
        .unwrap();

        assert!(
            matches!(outcome, WatchOutcome::AllRunning { services: 1, .. }),
            "got {outcome:?}"
        );
        // First-sighting (restarting) + transition to running.
        assert_eq!(renderer.lines.len(), 2, "{:#?}", renderer.lines);
        assert!(renderer.lines[0].contains("restarting"));
        assert!(renderer.lines[1].contains("running"));
        // Mock got at least two GETs (one per state). Wiremock doesn't
        // expose the count without an `.expect()`, but the
        // `counter` we hold a clone of does.
        assert!(counter.load(Ordering::SeqCst) >= 2);
    }
}
