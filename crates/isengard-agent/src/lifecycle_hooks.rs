//! Agent-side execution of stack lifecycle hooks.
//!
//! Wave 2.A shipped the data path: the proto carries hook lists on
//! [`isengard_proto::pb::WriteCompose`], the dashboard persists them, the
//! agent persists the verbatim `stack.toml` next to `compose.yaml`. This
//! module ships the execution side: spawn each hook on the host, capture
//! stdout/stderr, enforce timeouts, and surface results to the controller
//! via the same [`isengard_core::EventEmitter`] channel the rest of the
//! agent uses.
//!
//! ## Execution semantics (matching the spec)
//!
//! **cwd** is `<stack_dir>` (the directory holding `compose.yaml` +
//! `stack.toml` for this stack on disk, typically
//! `/etc/isengard/stacks/<name>/`).
//!
//! **env** starts EMPTY (deny-by-default) and is then layered with the
//! whitelisted parent variables ([`AGENT_ENV_WHITELIST`]) plus the
//! [`HookContext`] vars (`ISENGARD_STACK`, `ISENGARD_HOST_ID`,
//! `ISENGARD_DEPLOYMENT_ID`, `ISENGARD_HOOK_EVENT`,
//! `ISENGARD_MANIFEST_PATH`; failure adds `ISENGARD_FAILURE_REASON` +
//! `ISENGARD_FAILURE_DETAIL`), and finally the hook spec's own `env`
//! map (last writer wins on collision so hook authors can override
//! anything the agent set).
//!
//! **timeout** is per-hook, default 60s. Enforced via
//! `tokio::time::timeout` around `child.wait_with_output()`. On
//! expiry we SIGTERM the child and, after 5s, SIGKILL.
//!
//! **failure**: [`OnFailure::Abort`] (default) returns
//! [`HookOutcome::Aborted`] to the caller, which the WriteCompose
//! handler turns into a deploy abort. [`OnFailure::Warn`] /
//! [`OnFailure::Ignore`] keep going.
//!
//! **audit**: every hook execution emits a structured tracing event
//! AND a `lifecycle_hook.*` [`isengard_core::Event`] over the emitter
//! so the controller's event stream captures the run.
//!
//! ## Security
//!
//! Hooks run as the agent's own user. On the systemd install
//! that's `root`. A dedicated `isengard-hooks` user + AppArmor profile
//! is a future tightening (out of scope here). To reduce blast
//! radius today, the parent environment is whitelisted (see
//! [`AGENT_ENV_WHITELIST`]); `ISENGARD_*` and other agent-private
//! variables are NOT exposed to hooks by default.
//!
//! The hook command is invoked as `sh -c "<command>"` so operators can
//! use pipes / redirects / shell builtins, but the agent never
//! interpolates user input into a format string: the spec's `command`
//! is passed verbatim as a single argv entry, sidestepping the classic
//! "$variable expanded into a printf string" footgun.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use isengard_core::{Event, EventEmitter};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{debug, info, warn};

/// Variables inherited from the agent's own environment into every
/// hook's process. Anything not on this list is dropped before the
/// child is spawned, even if it was set in the agent's parent shell.
///
/// Deny-by-default keeps `ISENGARD_*` (which may carry the controller
/// CA, master-key paths, or other operator-private state) out of hook
/// processes. Operators who explicitly want to pass a value through
/// can set it on the [`HookSpec::env`] map per hook.
pub const AGENT_ENV_WHITELIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TZ"];

/// The four lifecycle phases the agent runs.
///
/// `pre-deploy` and `post-deploy` map 1:1 to the spec's manifest enum.
/// `failure` (the spec's third event) fires when the deploy itself
/// fails (e.g. WriteCompose conflicts, reconcile errors). `PreStop` +
/// `PostStop` are reserved for the future `isd stack stop` path; not
/// yet wired into [`crate::sync`] but the executor handles them the
/// same way so the proto can grow without an executor rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPhase {
    PreDeploy,
    PostDeploy,
    Failure,
    /// Reserved: not yet emitted by [`crate::sync`]. Wired for symmetry.
    PreStop,
    /// Reserved: not yet emitted by [`crate::sync`]. Wired for symmetry.
    PostStop,
}

impl HookPhase {
    /// Stringification matches the proto's `on` field shape.
    pub fn as_str(self) -> &'static str {
        match self {
            HookPhase::PreDeploy => "pre-deploy",
            HookPhase::PostDeploy => "post-deploy",
            HookPhase::Failure => "failure",
            HookPhase::PreStop => "pre-stop",
            HookPhase::PostStop => "post-stop",
        }
    }
}

/// What to do when a hook exits non-zero (or times out).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum OnFailure {
    /// Stop running further hooks; signal abort to the caller.
    /// This is the default per the manifest spec.
    #[default]
    Abort,
    /// Log a WARN, continue with the next hook.
    Warn,
    /// Log a DEBUG, continue with the next hook.
    Ignore,
}

impl OnFailure {
    /// Parse the proto's `on_error` string. Unknown values fall back to
    /// `Abort` (the safest behavior: surface the surprise).
    pub fn parse(s: &str) -> Self {
        match s {
            "continue" | "warn" => Self::Warn,
            "ignore" => Self::Ignore,
            _ => Self::Abort,
        }
    }
}

/// One hook the agent will run. The proto carries a `repeated string cmd`;
/// we accept that argv form via [`HookSpec::from_argv`].
#[derive(Debug, Clone)]
pub struct HookSpec {
    /// Shell command line. Passed to `sh -c "<command>"` so operators
    /// can use pipes / redirects / shell builtins. Never formatted
    /// into a format string by the agent.
    pub command: String,
    /// Hook-specific env, layered on top of the whitelist + context
    /// vars. Spec values win on collision.
    pub env: HashMap<String, String>,
    /// Wall-clock budget for the child process. 0 means "use the
    /// default" (60s, see [`DEFAULT_HOOK_TIMEOUT`]).
    pub timeout: Duration,
    /// What to do on non-zero exit or timeout.
    pub on_failure: OnFailure,
}

/// Default per-hook timeout when the spec doesn't pin one.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

/// Grace period between SIGTERM and SIGKILL when a hook times out.
const HOOK_TERM_GRACE: Duration = Duration::from_secs(5);

impl HookSpec {
    /// Build a [`HookSpec`] from the proto's argv-shaped command. We
    /// join the argv with spaces because the spec's example uses
    /// `cmd = ["./scripts/backup-config.sh"]` (single program) AND
    /// `cmd = ["./scripts/notify-discord.sh", "stack deployed"]`
    /// (program + literal args). For the multi-argv case we shell-quote
    /// each component so spaces / globs / quotes in args don't get
    /// re-parsed by the shell. Plain alphanumeric + safe punctuation
    /// pass through unquoted to keep the audit trail readable.
    pub fn from_argv(argv: &[String], timeout_ms: u64, on_error: &str) -> Self {
        let command = if argv.len() == 1 {
            argv[0].clone()
        } else {
            argv.iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let timeout = if timeout_ms == 0 {
            DEFAULT_HOOK_TIMEOUT
        } else {
            Duration::from_millis(timeout_ms)
        };
        Self {
            command,
            env: HashMap::new(),
            timeout,
            on_failure: OnFailure::parse(on_error),
        }
    }
}

/// POSIX shell-quote `s` for safe re-parsing inside `sh -c`. Identifier-y
/// inputs pass through unchanged; anything else gets wrapped in single
/// quotes with embedded `'` escaped as `'\''`. Mirrors the conservative
/// shell-quote rule from `shlex` / `python's shlex.quote`.
fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '='));
    if safe {
        s.to_string()
    } else {
        let escaped = s.replace('\'', "'\\''");
        format!("'{}'", escaped)
    }
}

/// Per-deploy context passed to every hook in a sweep. The fields land
/// in the child process's environment via the
/// [`HookContext::env_for_phase`] helper.
#[derive(Debug, Clone)]
pub struct HookContext {
    pub stack: String,
    pub host_id: String,
    pub deployment_id: String,
    /// Directory the child process should `cd` into before exec. Usually
    /// `/etc/isengard/stacks/<stack>/`.
    pub stack_dir: PathBuf,
    /// Optional failure-detail string. Set only when running
    /// [`HookPhase::Failure`] hooks.
    pub failure_reason: Option<String>,
    pub failure_detail: Option<String>,
}

impl HookContext {
    /// Build the environment map for a hook of `phase`. Starts with the
    /// whitelisted parent variables, layers in the ISENGARD_* context
    /// vars, then the failure detail (if any).
    fn env_for_phase(&self, phase: HookPhase) -> HashMap<String, String> {
        let mut env = HashMap::new();
        for &key in AGENT_ENV_WHITELIST {
            if let Ok(val) = std::env::var(key) {
                env.insert(key.to_string(), val);
            }
        }
        env.insert("ISENGARD_STACK".into(), self.stack.clone());
        env.insert("ISENGARD_HOST_ID".into(), self.host_id.clone());
        env.insert("ISENGARD_DEPLOYMENT_ID".into(), self.deployment_id.clone());
        env.insert("ISENGARD_HOOK_EVENT".into(), phase.as_str().into());
        let manifest_path = self.stack_dir.join("stack.toml");
        env.insert(
            "ISENGARD_MANIFEST_PATH".into(),
            manifest_path.to_string_lossy().into_owned(),
        );
        if phase == HookPhase::Failure {
            if let Some(r) = &self.failure_reason {
                env.insert("ISENGARD_FAILURE_REASON".into(), r.clone());
            }
            if let Some(d) = &self.failure_detail {
                env.insert("ISENGARD_FAILURE_DETAIL".into(), d.clone());
            }
        }
        env
    }
}

/// Outcome of one hook execution.
#[derive(Debug, Clone)]
pub struct HookRunReport {
    pub command: String,
    /// `Some(code)` on clean exit, `None` on signal / timeout.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
    pub timed_out: bool,
}

/// Outcome of running an entire sweep of hooks for one phase.
#[derive(Debug)]
pub enum HookOutcome {
    /// Every hook either succeeded or its `on_failure` policy let us
    /// continue.
    AllOk { reports: Vec<HookRunReport> },
    /// A hook with `on_failure = Abort` failed; further hooks were
    /// skipped. `index` is the position of the failing hook in the
    /// sweep (0-based); `reason` is the short audit-friendly string
    /// also emitted as the event's `error` field.
    Aborted {
        index: usize,
        reason: String,
        reports: Vec<HookRunReport>,
    },
}

impl HookOutcome {
    pub fn is_aborted(&self) -> bool {
        matches!(self, HookOutcome::Aborted { .. })
    }
    pub fn reports(&self) -> &[HookRunReport] {
        match self {
            HookOutcome::AllOk { reports } => reports,
            HookOutcome::Aborted { reports, .. } => reports,
        }
    }
}

/// Run every hook in `hooks` for `phase` in declared order. Emits one
/// [`Event`] per hook via `emitter`:
/// - `lifecycle_hook.started` before spawn
/// - `lifecycle_hook.completed` on clean exit
/// - `lifecycle_hook.failed` on non-zero exit or timeout
///
/// Empty `hooks` short-circuits to `AllOk` without emitting any events
/// (no hooks, nothing to audit).
pub async fn run_hooks(
    phase: HookPhase,
    hooks: &[HookSpec],
    ctx: &HookContext,
    emitter: &dyn EventEmitter,
) -> HookOutcome {
    if hooks.is_empty() {
        return HookOutcome::AllOk {
            reports: Vec::new(),
        };
    }

    let mut reports: Vec<HookRunReport> = Vec::with_capacity(hooks.len());

    for (idx, hook) in hooks.iter().enumerate() {
        emit_started(emitter, phase, ctx, hook, idx).await;

        let report = run_single_hook(hook, phase, ctx).await;

        let ok = report.exit_code == Some(0) && !report.timed_out;
        if ok {
            emit_completed(emitter, phase, ctx, &report, idx).await;
            info!(
                stack = %ctx.stack,
                phase = phase.as_str(),
                idx,
                duration_ms = report.duration.as_millis() as u64,
                "lifecycle hook ok",
            );
            reports.push(report);
            continue;
        }

        // The hook failed (non-zero exit, signal, or timeout).
        let reason = if report.timed_out {
            format!(
                "hook[{}] timed out after {}ms",
                idx,
                hook.timeout.as_millis() as u64
            )
        } else {
            format!("hook[{}] exited with code {:?}", idx, report.exit_code)
        };
        emit_failed(emitter, phase, ctx, &report, idx, &reason).await;

        match hook.on_failure {
            OnFailure::Abort => {
                warn!(
                    stack = %ctx.stack,
                    phase = phase.as_str(),
                    idx,
                    reason = %reason,
                    "lifecycle hook failed; aborting sweep",
                );
                reports.push(report);
                return HookOutcome::Aborted {
                    index: idx,
                    reason,
                    reports,
                };
            }
            OnFailure::Warn => {
                warn!(
                    stack = %ctx.stack,
                    phase = phase.as_str(),
                    idx,
                    reason = %reason,
                    "lifecycle hook failed (on_failure=warn); continuing",
                );
            }
            OnFailure::Ignore => {
                debug!(
                    stack = %ctx.stack,
                    phase = phase.as_str(),
                    idx,
                    reason = %reason,
                    "lifecycle hook failed (on_failure=ignore); continuing",
                );
            }
        }
        reports.push(report);
    }

    HookOutcome::AllOk { reports }
}

async fn run_single_hook(hook: &HookSpec, phase: HookPhase, ctx: &HookContext) -> HookRunReport {
    let started = std::time::Instant::now();
    let mut env = ctx.env_for_phase(phase);
    for (k, v) in &hook.env {
        env.insert(k.clone(), v.clone());
    }

    // Ensure the cwd exists. We DON'T create the stack_dir here: that's
    // the compose_writer's job during the deploy proper. If the dir
    // doesn't exist (pre-deploy on a brand-new stack with no prior
    // compose), the hook can still run from "/" which is harmless.
    let cwd: &Path = if ctx.stack_dir.is_dir() {
        ctx.stack_dir.as_path()
    } else {
        Path::new("/")
    };

    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&hook.command)
        .current_dir(cwd)
        .env_clear()
        .envs(&env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return HookRunReport {
                command: hook.command.clone(),
                exit_code: None,
                stdout: String::new(),
                stderr: format!("spawn failed: {e}"),
                duration: started.elapsed(),
                timed_out: false,
            };
        }
    };

    // Take stdout/stderr OUT of the child handle so we can read them
    // concurrently with wait(). Without this the child can deadlock if
    // it fills its pipe before exiting.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let pid = child.id();

    let wait_fut = async {
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let read_out = async {
            if let Some(ref mut s) = stdout_pipe {
                let _ = s.read_to_end(&mut stdout_buf).await;
            }
        };
        let read_err = async {
            if let Some(ref mut s) = stderr_pipe {
                let _ = s.read_to_end(&mut stderr_buf).await;
            }
        };
        let (_, _, status) = tokio::join!(read_out, read_err, child.wait());
        (status, stdout_buf, stderr_buf)
    };

    let outcome = tokio::time::timeout(hook.timeout, wait_fut).await;

    match outcome {
        Ok((Ok(status), out_buf, err_buf)) => HookRunReport {
            command: hook.command.clone(),
            exit_code: status.code(),
            stdout: String::from_utf8_lossy(&out_buf).into_owned(),
            stderr: String::from_utf8_lossy(&err_buf).into_owned(),
            duration: started.elapsed(),
            timed_out: false,
        },
        Ok((Err(e), out_buf, err_buf)) => HookRunReport {
            command: hook.command.clone(),
            exit_code: None,
            stdout: String::from_utf8_lossy(&out_buf).into_owned(),
            stderr: format!("{}\nwait failed: {e}", String::from_utf8_lossy(&err_buf)),
            duration: started.elapsed(),
            timed_out: false,
        },
        Err(_elapsed) => {
            // Timeout: SIGTERM the child, wait `HOOK_TERM_GRACE`, then SIGKILL.
            if let Some(pid) = pid {
                send_sigterm(pid as i32);
            }
            let _ = tokio::time::timeout(HOOK_TERM_GRACE, async {
                // best-effort wait for graceful exit
                let _ = child.wait().await;
            })
            .await;
            // Hard kill if still alive. tokio's `start_kill` sends SIGKILL.
            let _ = child.start_kill();
            let _ = child.wait().await;
            HookRunReport {
                command: hook.command.clone(),
                exit_code: None,
                stdout: String::new(),
                stderr: "timeout".into(),
                duration: started.elapsed(),
                timed_out: true,
            }
        }
    }
}

/// Send SIGTERM to `pid`. Best-effort; errors (already-exited child,
/// EPERM after a uid drop, ESRCH on race) are swallowed because the
/// caller's next step is a hard SIGKILL via `tokio::process::Child::start_kill`
/// anyway.
fn send_sigterm(pid: i32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
}

async fn emit_started(
    emitter: &dyn EventEmitter,
    phase: HookPhase,
    ctx: &HookContext,
    hook: &HookSpec,
    idx: usize,
) {
    let metadata = serde_json::json!({
        "stack": ctx.stack,
        "host_id": ctx.host_id,
        "deployment_id": ctx.deployment_id,
        "phase": phase.as_str(),
        "hook_index": idx,
        "command": hook.command,
        "timeout_ms": hook.timeout.as_millis() as u64,
    });
    let ev = Event {
        kind: "lifecycle_hook.started".into(),
        occurred_at: chrono::Utc::now(),
        summary: format!(
            "{} hook[{}] starting for stack {}",
            phase.as_str(),
            idx,
            ctx.stack,
        ),
        metadata,
        ..Default::default()
    };
    emitter.emit(ev).await;
}

async fn emit_completed(
    emitter: &dyn EventEmitter,
    phase: HookPhase,
    ctx: &HookContext,
    report: &HookRunReport,
    idx: usize,
) {
    let metadata = serde_json::json!({
        "stack": ctx.stack,
        "host_id": ctx.host_id,
        "deployment_id": ctx.deployment_id,
        "phase": phase.as_str(),
        "hook_index": idx,
        "exit_code": report.exit_code,
        "duration_ms": report.duration.as_millis() as u64,
        "stdout": truncate(&report.stdout, 4096),
        "stderr": truncate(&report.stderr, 4096),
    });
    let ev = Event {
        kind: "lifecycle_hook.completed".into(),
        occurred_at: chrono::Utc::now(),
        summary: format!(
            "{} hook[{}] completed for stack {}",
            phase.as_str(),
            idx,
            ctx.stack,
        ),
        metadata,
        ..Default::default()
    };
    emitter.emit(ev).await;
}

async fn emit_failed(
    emitter: &dyn EventEmitter,
    phase: HookPhase,
    ctx: &HookContext,
    report: &HookRunReport,
    idx: usize,
    reason: &str,
) {
    let metadata = serde_json::json!({
        "stack": ctx.stack,
        "host_id": ctx.host_id,
        "deployment_id": ctx.deployment_id,
        "phase": phase.as_str(),
        "hook_index": idx,
        "exit_code": report.exit_code,
        "duration_ms": report.duration.as_millis() as u64,
        "timed_out": report.timed_out,
        "stdout": truncate(&report.stdout, 4096),
        "stderr": truncate(&report.stderr, 4096),
    });
    let ev = Event {
        kind: "lifecycle_hook.failed".into(),
        occurred_at: chrono::Utc::now(),
        summary: format!(
            "{} hook[{}] failed for stack {}: {}",
            phase.as_str(),
            idx,
            ctx.stack,
            reason,
        ),
        error: Some(reason.to_string()),
        metadata,
        ..Default::default()
    };
    emitter.emit(ev).await;
}

/// Truncate a string to at most `n` UTF-8 bytes, appending an ellipsis
/// marker so the consumer can tell it was clipped. Used to cap the
/// stdout/stderr payload size in the audit metadata; a hook that
/// dumps a 50MB build log shouldn't flood the controller's event
/// stream.
fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    // Walk back to a char boundary so we don't slice mid-codepoint.
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... <truncated {} bytes>", &s[..end], s.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[derive(Default, Clone)]
    struct CapturingEmitter {
        events: Arc<Mutex<Vec<Event>>>,
    }

    #[async_trait]
    impl EventEmitter for CapturingEmitter {
        async fn emit(&self, event: Event) {
            self.events.lock().await.push(event);
        }
    }

    fn ctx_in(dir: &Path) -> HookContext {
        HookContext {
            stack: "alpha".into(),
            host_id: "host-01".into(),
            deployment_id: "dep-01".into(),
            stack_dir: dir.to_path_buf(),
            failure_reason: None,
            failure_detail: None,
        }
    }

    fn spec(cmd: &str) -> HookSpec {
        HookSpec {
            command: cmd.into(),
            env: HashMap::new(),
            timeout: Duration::from_secs(5),
            on_failure: OnFailure::Abort,
        }
    }

    #[tokio::test]
    async fn hook_with_echo_succeeds_and_captures_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let emitter = CapturingEmitter::default();
        let hooks = vec![spec("echo hello-from-hook")];
        let outcome = run_hooks(HookPhase::PreDeploy, &hooks, &ctx, &emitter).await;
        assert!(matches!(outcome, HookOutcome::AllOk { .. }));
        let reports = outcome.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].exit_code, Some(0));
        assert!(reports[0].stdout.contains("hello-from-hook"));
        assert!(!reports[0].timed_out);

        // Two events emitted (started + completed); no failures.
        let evs = emitter.events.lock().await.clone();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].kind, "lifecycle_hook.started");
        assert_eq!(evs[1].kind, "lifecycle_hook.completed");
    }

    #[tokio::test]
    async fn hook_exit_1_with_abort_aborts_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let emitter = CapturingEmitter::default();
        let mut h = spec("exit 1");
        h.on_failure = OnFailure::Abort;
        let hooks = vec![h, spec("echo never-runs")];
        let outcome = run_hooks(HookPhase::PreDeploy, &hooks, &ctx, &emitter).await;
        match outcome {
            HookOutcome::Aborted { index, reports, .. } => {
                assert_eq!(index, 0);
                // Only the failing hook ran (length = 1, second skipped).
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].exit_code, Some(1));
            }
            HookOutcome::AllOk { .. } => panic!("expected Aborted"),
        }
        let evs = emitter.events.lock().await.clone();
        // started + failed (but NOT completed for the first hook; second
        // hook NEVER started).
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].kind, "lifecycle_hook.started");
        assert_eq!(evs[1].kind, "lifecycle_hook.failed");
    }

    #[tokio::test]
    async fn hook_exit_1_with_warn_continues() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let emitter = CapturingEmitter::default();
        let mut h = spec("exit 1");
        h.on_failure = OnFailure::Warn;
        let hooks = vec![h, spec("echo did-run")];
        let outcome = run_hooks(HookPhase::PostDeploy, &hooks, &ctx, &emitter).await;
        let reports = match outcome {
            HookOutcome::AllOk { reports } => reports,
            HookOutcome::Aborted { .. } => panic!("expected AllOk"),
        };
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].exit_code, Some(1));
        assert_eq!(reports[1].exit_code, Some(0));
        assert!(reports[1].stdout.contains("did-run"));
    }

    #[tokio::test]
    async fn hook_exit_1_with_ignore_continues() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let emitter = CapturingEmitter::default();
        let mut h = spec("exit 7");
        h.on_failure = OnFailure::Ignore;
        let hooks = vec![h];
        let outcome = run_hooks(HookPhase::PostDeploy, &hooks, &ctx, &emitter).await;
        assert!(matches!(outcome, HookOutcome::AllOk { .. }));
    }

    #[tokio::test]
    async fn hook_with_long_sleep_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let emitter = CapturingEmitter::default();
        let mut h = spec("sleep 30");
        h.timeout = Duration::from_millis(300);
        let hooks = vec![h];
        let outcome = run_hooks(HookPhase::PreDeploy, &hooks, &ctx, &emitter).await;
        let reports = outcome.reports();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].timed_out);
        // Should have aborted by default.
        assert!(outcome.is_aborted());
        // Duration is bounded by timeout + grace (under 7s).
        assert!(reports[0].duration < Duration::from_secs(7));
    }

    #[tokio::test]
    async fn cwd_is_stack_dir_when_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join("probe.txt");
        let ctx = ctx_in(dir.path());
        let emitter = CapturingEmitter::default();
        // `pwd` resolves the dev-machine path; we tee it into a known
        // file inside the dir so we can read it back without parsing.
        let cmd = format!("pwd > {}", probe.display());
        let hooks = vec![spec(&cmd)];
        let outcome = run_hooks(HookPhase::PreDeploy, &hooks, &ctx, &emitter).await;
        assert!(matches!(outcome, HookOutcome::AllOk { .. }));
        let body = std::fs::read_to_string(&probe).unwrap();
        // pwd may resolve symlinks (e.g. /var -> /private/var on macOS);
        // accept any trailing component match.
        assert!(
            body.trim()
                .ends_with(dir.path().file_name().unwrap().to_str().unwrap()),
            "pwd output {body:?} did not end with {:?}",
            dir.path()
        );
    }

    #[tokio::test]
    async fn env_layering_spec_wins_over_agent() {
        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join("env.txt");
        let mut ctx = ctx_in(dir.path());
        ctx.stack = "envstack".into();
        let emitter = CapturingEmitter::default();
        let mut h = spec(&format!(
            "printf '%s|%s|%s' \"$ISENGARD_STACK\" \"$MY_HOOK_VAR\" \"$ISENGARD_HOOK_EVENT\" > {}",
            probe.display(),
        ));
        h.env.insert("MY_HOOK_VAR".into(), "from-spec".into());
        // Hook-spec can override an ISENGARD_* var if it really wants
        // to. We don't try to lock those down: hook authors run as the
        // agent user already.
        h.env.insert("ISENGARD_STACK".into(), "spec-wins".into());
        let hooks = vec![h];
        let outcome = run_hooks(HookPhase::PostDeploy, &hooks, &ctx, &emitter).await;
        assert!(matches!(outcome, HookOutcome::AllOk { .. }));
        let body = std::fs::read_to_string(&probe).unwrap();
        let parts: Vec<&str> = body.split('|').collect();
        assert_eq!(
            parts[0], "spec-wins",
            "ISENGARD_STACK should be overridden by spec env",
        );
        assert_eq!(parts[1], "from-spec");
        assert_eq!(parts[2], "post-deploy");
    }

    #[tokio::test]
    async fn agent_env_outside_whitelist_is_dropped() {
        // Set a non-whitelisted var in the parent; assert the hook
        // doesn't see it.
        // SAFETY: std::env::set_var is unsafe in Rust 2024 because envvar
        // mutation is not thread-safe across the process. The other test
        // arms in this module only read `std::env::var` for whitelisted
        // names, so the local set + clear scope is the safest reachable
        // call site.
        unsafe { std::env::set_var("ISENGARD_SECRET_DO_NOT_LEAK", "TOPSECRET") };
        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join("leak.txt");
        let ctx = ctx_in(dir.path());
        let emitter = CapturingEmitter::default();
        let hooks = vec![spec(&format!(
            "printf '%s' \"${{ISENGARD_SECRET_DO_NOT_LEAK:-MISSING}}\" > {}",
            probe.display()
        ))];
        let outcome = run_hooks(HookPhase::PreDeploy, &hooks, &ctx, &emitter).await;
        assert!(matches!(outcome, HookOutcome::AllOk { .. }));
        let body = std::fs::read_to_string(&probe).unwrap();
        assert_eq!(
            body, "MISSING",
            "non-whitelisted agent env must not leak into hooks",
        );
        unsafe { std::env::remove_var("ISENGARD_SECRET_DO_NOT_LEAK") };
    }

    #[test]
    fn from_argv_single_passes_through_verbatim() {
        let h = HookSpec::from_argv(&["./scripts/backup.sh".into()], 60_000, "abort");
        assert_eq!(h.command, "./scripts/backup.sh");
        assert_eq!(h.timeout, Duration::from_secs(60));
        assert_eq!(h.on_failure, OnFailure::Abort);
    }

    #[test]
    fn from_argv_multi_shell_quotes_args() {
        let argv = vec![
            "./scripts/notify.sh".into(),
            "stack deployed".into(),
            "ok".into(),
        ];
        let h = HookSpec::from_argv(&argv, 0, "continue");
        // The first arg is plain (no quoting needed); the second has a
        // space so it gets single-quoted; "ok" stays plain.
        assert_eq!(h.command, "./scripts/notify.sh 'stack deployed' ok");
        // timeout_ms=0 -> default
        assert_eq!(h.timeout, DEFAULT_HOOK_TIMEOUT);
        // "continue" maps to Warn
        assert_eq!(h.on_failure, OnFailure::Warn);
    }

    #[test]
    fn from_argv_quote_handles_embedded_single_quote() {
        let argv = vec!["echo".into(), "it's-fine".into()];
        let h = HookSpec::from_argv(&argv, 0, "abort");
        // POSIX single-quote escape: 'it'\''s-fine'
        assert_eq!(h.command, "echo 'it'\\''s-fine'");
    }

    #[test]
    fn on_failure_parse_table() {
        assert_eq!(OnFailure::parse("abort"), OnFailure::Abort);
        assert_eq!(OnFailure::parse("continue"), OnFailure::Warn);
        assert_eq!(OnFailure::parse("warn"), OnFailure::Warn);
        assert_eq!(OnFailure::parse("ignore"), OnFailure::Ignore);
        // Unknown values default to Abort (safest).
        assert_eq!(OnFailure::parse(""), OnFailure::Abort);
        assert_eq!(OnFailure::parse("nope"), OnFailure::Abort);
    }

    #[test]
    fn truncate_clips_long_strings_at_char_boundary() {
        let s = "a".repeat(5000);
        let t = truncate(&s, 100);
        assert!(t.starts_with(&"a".repeat(100)));
        assert!(t.contains("truncated"));
        // Multi-byte safety: a string of repeated 2-byte chars truncated
        // mid-codepoint should walk back to the prior boundary.
        let multi = "é".repeat(50); // each char is 2 bytes
        let t = truncate(&multi, 21); // odd byte limit
        assert!(t.contains("truncated"));
    }

    #[test]
    fn hook_phase_as_str_round_trip() {
        assert_eq!(HookPhase::PreDeploy.as_str(), "pre-deploy");
        assert_eq!(HookPhase::PostDeploy.as_str(), "post-deploy");
        assert_eq!(HookPhase::Failure.as_str(), "failure");
        assert_eq!(HookPhase::PreStop.as_str(), "pre-stop");
        assert_eq!(HookPhase::PostStop.as_str(), "post-stop");
    }

    #[test]
    fn shell_quote_passes_safe_chars_through() {
        assert_eq!(shell_quote("./scripts/foo.sh"), "./scripts/foo.sh");
        assert_eq!(shell_quote("KEY=value"), "KEY=value");
        assert_eq!(shell_quote("foo-bar_1.2"), "foo-bar_1.2");
    }

    #[test]
    fn shell_quote_quotes_unsafe_chars() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }
}
