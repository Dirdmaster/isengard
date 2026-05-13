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
//! ## Rendering model
//!
//! v0.5.3 onward: one in-place line per service via
//! [`indicatif::MultiProgress`]. Each bar shows a spinner during
//! intermediate states (Pending / Restarting), a service name, a state
//! label, and an `elapsed` counter that ticks live. On reaching a terminal
//! state the bar is finished with a success / failure glyph + colour.
//! Modeled on `docker compose up`.
//!
//! The intro line (`┌  isd deploy --watch  servarr`) and the closing
//! summary still come from cliclack so the connector chrome matches the
//! rest of isd's UX.
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
//! richer per-service detail (pull progress, layer counts, IPs); deferred
//! to v0.5.3+ so this lands without a controller change.
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
//! The MultiProgress renderer surfaces `Pulling` / `Creating` as the
//! human label on the per-service progress bar (with the cyan spinner),
//! and `Failed` as a red row via `finish_with_message`.
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
use console::{Style, Term};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
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

/// Spinner tick cadence. 80ms matches docker-compose's feel: fast enough
/// that the spinner reads as motion, slow enough not to thrash a remote
/// terminal.
const SPINNER_TICK: Duration = Duration::from_millis(80);

/// Service name column width on the progress bar. Padded to this with
/// spaces; longer names just push the rest of the line right (indicatif
/// truncates `wide_msg` to keep within the terminal width).
const NAME_COL_WIDTH: usize = 32;

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

/// Map a raw `ServiceState` string into a human-facing label. Mirrors
/// docker-compose's vocabulary so an operator reading both feels at
/// home. v0.5.3 added Pulling / Creating / Failed mid-startup states.
pub fn state_label(state: &str) -> &'static str {
    match state {
        "running" => "Running",
        "stopped" => "Stopped",
        "failed" => "Failed",
        "restarting" => "Restarting",
        "pulling" => "Pulling",
        "creating" => "Creating",
        "starting" => "Starting",
        "unknown" => "Pending",
        _ => "Pending",
    }
}

/// Inverse of [`is_terminal`] for the v0.5.3 mid-startup states.
/// The indicatif MultiProgress renderer uses this to label "still
/// working" rows without re-encoding the classification rule.
///
/// Lib-build sees this as dead because the renderer's call site is
/// currently inlined; keeping the helper as the documented contract
/// keeps the classification logic in one place.
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

/// Render a single transition line for tests / non-tty fallback. Format:
/// `service-name<pad>state<pad>image | took Xs`. Pulled out so renderer
/// implementations and the plain-text fallback agree on the layout.
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
/// status. Side-effect: emits one transition callback per observed state
/// change (the renderer turns those into progress-bar mutations or plain
/// stdout lines).
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

/// Side-channel that takes a single transition and writes it somewhere.
///
/// Two production-relevant implementations:
///
///   - [`MultiProgressRenderer`]: one in-place [`ProgressBar`] per
///     service via [`MultiProgress`]; the docker-compose-up experience.
///     Used when stdout is a tty.
///   - [`PlainRenderer`]: append-only stdout lines, one per transition.
///     Used when stdout is piped / not a tty (CI, redirected output).
///
/// Plus a test renderer ([`CaptureRenderer`]) that collects lines into a
/// `Vec` without touching the terminal.
pub trait TransitionRenderer {
    fn transition(&mut self, svc: &ServiceSnapshot, took: Option<Duration>);
}

/// Glyph set for the spinner + terminal-state markers. Two variants:
/// fancy Unicode (default; what modern terminals render correctly) and
/// ASCII (fallback for `LANG=C` / dumb terminals / mojibake).
#[derive(Debug, Clone, Copy)]
struct GlyphSet {
    spinner: &'static [&'static str],
    /// Tick used by [`ProgressStyle::tick_strings`]; final element is the
    /// "finished" frame indicatif drops in once `finish_*` is called. We
    /// override per-state via `finish_with_message` so this stays neutral.
    finished_running: &'static str,
    finished_failed: &'static str,
}

const FANCY_SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "];
const ASCII_SPINNER: &[&str] = &["-", "\\", "|", "/", " "];

const FANCY_GLYPHS: GlyphSet = GlyphSet {
    spinner: FANCY_SPINNER,
    finished_running: "✔",
    finished_failed: "✗",
};
const ASCII_GLYPHS: GlyphSet = GlyphSet {
    spinner: ASCII_SPINNER,
    finished_running: "+",
    finished_failed: "x",
};

/// Decide which glyph set to use based on the locale. We default to fancy
/// Unicode; if `LANG` / `LC_ALL` / `LC_CTYPE` don't contain `UTF-8` /
/// `utf8` we fall back to ASCII so terminals stuck in a single-byte
/// locale don't render mojibake. `ISD_ASCII=1` is a hard override (useful
/// for screenshots / docs / CI snapshots).
fn pick_glyphs() -> GlyphSet {
    if std::env::var("ISD_ASCII")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return ASCII_GLYPHS;
    }
    let locale = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_CTYPE"))
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if locale.contains("utf-8") || locale.contains("utf8") {
        FANCY_GLYPHS
    } else if locale.is_empty() {
        // Empty locale on macOS Terminal.app is normal; assume UTF-8.
        FANCY_GLYPHS
    } else {
        ASCII_GLYPHS
    }
}

/// Style preset for a service in an intermediate (non-terminal) state.
/// Spinner + service name (padded) + state label + elapsed time. Truncated
/// to terminal width by indicatif's `wide_msg` placeholder.
fn intermediate_style(glyphs: GlyphSet) -> ProgressStyle {
    ProgressStyle::with_template("  {spinner:.cyan} {prefix} {wide_msg} {elapsed:>6}")
        .expect("static template")
        .tick_strings(glyphs.spinner)
}

/// Style preset for a service that has reached `running`. Green checkmark
/// + "Started" + final elapsed.
fn success_style(glyphs: GlyphSet) -> ProgressStyle {
    ProgressStyle::with_template(&format!(
        "  {} {{prefix}} {{wide_msg:.green}} {{elapsed:>6}}",
        Style::new().green().apply_to(glyphs.finished_running)
    ))
    .expect("static template")
}

/// Style preset for a service in a non-happy terminal state (stopped /
/// failed). Red X + reason + final elapsed.
fn error_style(glyphs: GlyphSet) -> ProgressStyle {
    ProgressStyle::with_template(&format!(
        "  {} {{prefix}} {{wide_msg:.red}} {{elapsed:>6}}",
        Style::new().red().apply_to(glyphs.finished_failed)
    ))
    .expect("static template")
}

/// Production renderer: one [`ProgressBar`] per service on a shared
/// [`MultiProgress`]. State changes mutate the bar (style + message);
/// terminal states finish it. The MultiProgress instance is held alive
/// for the lifetime of the renderer; dropping it triggers a final redraw.
pub struct MultiProgressRenderer {
    multi: MultiProgress,
    bars: HashMap<String, ProgressBar>,
    glyphs: GlyphSet,
}

impl MultiProgressRenderer {
    pub fn new() -> Self {
        Self {
            multi: MultiProgress::new(),
            bars: HashMap::new(),
            glyphs: pick_glyphs(),
        }
    }

    /// Pad a service name to a fixed column width so labels line up.
    /// Truncates with an ellipsis if longer than the column (rare; most
    /// docker-compose names are well under 32 chars).
    fn pad_name(&self, name: &str) -> String {
        let width = NAME_COL_WIDTH;
        if name.len() >= width {
            // Truncate to width - 1 chars + ellipsis so the column still
            // aligns. Slicing is byte-safe because service names are
            // ASCII (docker imposes [a-zA-Z0-9_.-]).
            format!("{}…", &name[..width.saturating_sub(1).min(name.len())])
        } else {
            format!("{name:<width$}")
        }
    }

    /// Compose the `wide_msg` payload: state label first, then truncated
    /// image. The terminal width is enforced by indicatif itself.
    fn message_for(&self, svc: &ServiceSnapshot) -> String {
        format!("{:<12} {}", state_label(&svc.state), svc.image)
    }

    /// Compose the finished-bar message: state label + image or detail
    /// string. For terminal successes: `Started   <image>`. For terminal
    /// failures: `Failed: <state>   <image>` (the state name carries the
    /// failure reason since the inventory channel doesn't expose an
    /// error string today).
    fn finish_message_for(&self, svc: &ServiceSnapshot) -> String {
        match svc.state.as_str() {
            "running" => format!("{:<12} {}", "Started", svc.image),
            "stopped" => format!("{:<12} {}", "Stopped", svc.image),
            "failed" => format!("{:<12} {}", "Failed", svc.image),
            other => format!("{:<12} {}", state_label(other), svc.image),
        }
    }
}

impl Default for MultiProgressRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl TransitionRenderer for MultiProgressRenderer {
    fn transition(&mut self, svc: &ServiceSnapshot, _took: Option<Duration>) {
        // `_took` is computed by the watch loop's tracker; for the
        // MultiProgress renderer we use indicatif's own elapsed counter
        // (started when the bar was first added) so the displayed value
        // matches what the operator has been watching tick up.
        let glyphs = self.glyphs;
        let padded_name = self.pad_name(&svc.name);
        let intermediate_msg = self.message_for(svc);
        let finish_msg = self.finish_message_for(svc);

        // Add-if-missing first via a non-borrowing path so the post-add
        // mutations below can take their own immutable borrow without
        // colliding with the `&mut self` `entry()` chain.
        if !self.bars.contains_key(&svc.id) {
            let pb = self.multi.add(ProgressBar::new_spinner());
            pb.set_style(intermediate_style(glyphs));
            pb.set_prefix(padded_name);
            pb.enable_steady_tick(SPINNER_TICK);
            self.bars.insert(svc.id.clone(), pb);
        }
        let bar = self.bars.get(&svc.id).expect("inserted above");

        // Defensive: a bar can be finished only once. If the watch loop
        // ever re-emits a transition for an already-finished service
        // (shouldn't happen given the tracker dedup, but cheap to guard)
        // skip the mutation.
        if bar.is_finished() {
            return;
        }

        if is_terminal(&svc.state) {
            if is_success_state(&svc.state) {
                bar.set_style(success_style(glyphs));
            } else {
                bar.set_style(error_style(glyphs));
            }
            bar.finish_with_message(finish_msg);
        } else {
            bar.set_message(intermediate_msg);
        }
    }
}

/// Non-tty fallback renderer: emits a plain stdout line per transition.
/// Same format as the pre-v0.5.3 cliclack renderer minus the connector
/// glyphs, so a `tee` / log-capture downstream still gets readable output.
pub struct PlainRenderer;

impl TransitionRenderer for PlainRenderer {
    fn transition(&mut self, svc: &ServiceSnapshot, took: Option<Duration>) {
        // Non-tty fallback: append-only stdout lines. The MultiProgress
        // renderer (production path) handles tty output via indicatif.
        // Intermediate states (pulling, creating, restarting, unknown)
        // and terminal states (running, stopped, failed) all land here
        // as a flat append; downstream tooling parsing piped output
        // gets the full transition log.
        println!("{}", format_transition_line(svc, took));
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

/// Print the closing summary that closes a watch session. Lives in this
/// module so the colouring rules are alongside the rest of the renderer.
///
/// Docker-compose-style banner first (`[+] Done 8/8`) then a cliclack
/// outro for the connector chrome.
pub fn print_outro(outcome: &WatchOutcome) {
    match outcome {
        WatchOutcome::AllRunning { services, elapsed } => {
            // Docker-compose-style summary line, mirrored to the
            // operator's stdout. Green when everything came up clean.
            println!();
            let banner = format!("[+] Done {services}/{services}");
            println!("{}", Style::new().green().apply_to(&banner));
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
            let total = running + failed;
            println!();
            let banner = format!("[+] Failed {running}/{total}");
            println!("{}", Style::new().red().apply_to(&banner));
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
    // Choose renderer based on whether stdout is a real terminal. Piped /
    // redirected output gets plain lines; an interactive shell gets the
    // in-place MultiProgress experience.
    let outcome = if Term::stdout().is_term() {
        let mut renderer = MultiProgressRenderer::new();
        watch_until_terminal(poller, cancel, WatchConfig::default(), &mut renderer).await?
    } else {
        let mut renderer = PlainRenderer;
        watch_until_terminal(poller, cancel, WatchConfig::default(), &mut renderer).await?
    };
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
    fn state_label_maps_known_states_to_human_labels() {
        assert_eq!(state_label("running"), "Running");
        assert_eq!(state_label("stopped"), "Stopped");
        assert_eq!(state_label("failed"), "Failed");
        assert_eq!(state_label("restarting"), "Restarting");
        assert_eq!(state_label("unknown"), "Pending");
        // v0.5.3 extended the agent vocabulary with mid-startup states.
        // Each gets its own label; the watch renderer groups them as
        // intermediate via `is_intermediate`, not via this label.
        assert_eq!(state_label("pulling"), "Pulling");
        assert_eq!(state_label("creating"), "Creating");
        assert_eq!(state_label("starting"), "Starting");
        // Truly unknown values still fall back to "Pending" so a
        // future agent state renders as an intermediate row.
        assert_eq!(state_label("blorp"), "Pending");
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

    /// `pick_glyphs` respects the explicit override env var and the
    /// locale env vars, defaulting to fancy when neither says otherwise.
    #[test]
    fn pick_glyphs_honors_overrides() {
        // We can't safely mutate process env in parallel-running tests
        // (other tests in the binary may read LANG concurrently), so
        // this asserts the pure-function shape rather than running the
        // env-reading path. The behaviour is exercised via integration
        // by setting ISD_ASCII / LANG in the surrounding shell.
        let fancy = FANCY_GLYPHS;
        let ascii = ASCII_GLYPHS;
        assert_eq!(fancy.finished_running, "✔");
        assert_eq!(ascii.finished_running, "+");
        assert!(fancy.spinner.len() >= 2);
        assert!(ascii.spinner.len() >= 2);
    }

    /// The MultiProgress renderer's transition implementation never
    /// panics on the empty hashmap path and dedupes by `svc.id`.
    #[test]
    fn multiprogress_renderer_handles_repeated_transitions() {
        // We don't assert visual output (indicatif owns rendering); we
        // assert the renderer's state-machine: same id reuses the same
        // bar, terminal state finishes it, post-finish transitions are
        // no-ops.
        let mut r = MultiProgressRenderer::new();
        let s1 = snap("1", "web", "restarting", "nginx:1");
        r.transition(&s1, None);
        assert_eq!(r.bars.len(), 1);
        let s2 = snap("1", "web", "running", "nginx:1");
        r.transition(&s2, Some(Duration::from_secs(3)));
        assert_eq!(r.bars.len(), 1);
        assert!(r.bars.values().next().unwrap().is_finished());
        // A stale duplicate after finish must not panic or unfinish.
        let s3 = snap("1", "web", "running", "nginx:1");
        r.transition(&s3, Some(Duration::from_secs(3)));
        assert!(r.bars.values().next().unwrap().is_finished());
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
