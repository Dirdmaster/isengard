//! ratatui install wizard for `isengard init`.
//!
//! Replaces the legacy `inquire`-driven prompts with a polished, full-screen
//! TUI: side-panel layout, persistent step list, live validation, async
//! bootstrap progress feed.
//!
//! Design: see `crates/isengard/src/init/mod.rs` for the brief. Five steps:
//!
//!   1. Welcome (banner + system detection)
//!   2. Network identity (hostnames, ACME directory, ACME email)
//!   3. Secrets (Cloudflare DNS token, optional Verify; backup passphrase)
//!   4. Bootstrap (live install progress streamed from `Plan::execute_with_progress`)
//!   5. Done (summary + join command)
//!
//! TTY handling: stdin may be a curl pipe (`curl ... | bash`). We open
//! `/dev/tty` directly for crossterm in/out so the wizard works under both
//! a normal interactive shell and the install-from-pipe pattern.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tokio::sync::mpsc;
use tui_big_text::{BigText, PixelSize};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::progress::{BootstrapStep, ProgressEvent, StepStatus, SummaryData};
use super::theme;
use super::{InitArgs, Plan, read_passphrase_file};

/// Top-level entry: take over the screen, run the wizard, restore.
pub(super) async fn run(args: InitArgs, host_ip: String) -> Result<()> {
    // Open `/dev/tty` for both reading keys and drawing. Falls through to
    // stdio when stdin is a terminal already (most local invocations).
    let mut term = TtyTerminal::open()
        .context("open /dev/tty for the install wizard (try --non-interactive)")?;

    // Run the wizard. We always restore the terminal even on panic via the
    // RAII guard inside TtyTerminal; the explicit drop here flushes
    // alternate screen + raw mode before any post-wizard prints.
    let outcome = drive_wizard(&mut term, args, host_ip).await;
    drop(term);

    match outcome {
        Ok(WizardOutcome::Completed(summary)) => {
            // After the wizard exits, dump the summary to plain stdout so the
            // operator has a copy in their scrollback for later reference.
            print_summary(&summary);
            Ok(())
        }
        Ok(WizardOutcome::Aborted) => {
            println!(
                "isengard init: aborted by operator (no changes committed if any unfinished step was running)."
            );
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// What the wizard returns to `run`.
enum WizardOutcome {
    Completed(SummaryData),
    Aborted,
}

// =====================================================================
// Terminal setup / RAII teardown
// =====================================================================

/// Wraps the active ratatui terminal. Drops cleanly: leaves alternate
/// screen, disables raw mode, removes mouse capture, even on panic.
struct TtyTerminal {
    term: Terminal<CrosstermBackend<TtyWriter>>,
    raw: bool,
}

/// Writer half: either the inherited stdout (when stdin is a TTY and we
/// trust the calling pipe) or `/dev/tty` opened for write. Both implement
/// `io::Write`; ratatui needs no more than that.
enum TtyWriter {
    Stdout(io::Stdout),
    DevTty(std::fs::File),
}

impl io::Write for TtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TtyWriter::Stdout(s) => s.write(buf),
            TtyWriter::DevTty(f) => f.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            TtyWriter::Stdout(s) => s.flush(),
            TtyWriter::DevTty(f) => f.flush(),
        }
    }
}

impl TtyTerminal {
    fn open() -> Result<Self> {
        // Pick the writer. Prefer stdout when it's a TTY (preserves any
        // user-configured terminal settings the inherited stdio carries);
        // fall back to /dev/tty when stdout was redirected.
        let writer = if io::stdout().is_terminal() {
            TtyWriter::Stdout(io::stdout())
        } else if Path::new("/dev/tty").exists() {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .context("open /dev/tty for writing")?;
            TtyWriter::DevTty(f)
        } else {
            bail!("no TTY available for the install wizard; pass --non-interactive")
        };

        let mut backend = CrosstermBackend::new(writer);
        // crossterm's enable_raw_mode flips the controlling terminal,
        // which is what we want even when stdout is /dev/tty: the kernel
        // routes raw key reads from the controlling tty.
        enable_raw_mode().context("enable raw mode")?;
        execute!(backend, EnterAlternateScreen, EnableMouseCapture)
            .context("enter alternate screen")?;
        let term = Terminal::new(backend).context("create ratatui terminal")?;
        Ok(TtyTerminal { term, raw: true })
    }
}

impl Drop for TtyTerminal {
    fn drop(&mut self) {
        if self.raw {
            let _ = execute!(
                self.term.backend_mut(),
                LeaveAlternateScreen,
                DisableMouseCapture
            );
            let _ = disable_raw_mode();
        }
    }
}

// =====================================================================
// Wizard state machine
// =====================================================================

/// Which step the wizard is currently on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Network,
    Secrets,
    Bootstrap,
    Done,
}

impl Step {
    fn ordered() -> &'static [Step] {
        &[
            Step::Welcome,
            Step::Network,
            Step::Secrets,
            Step::Bootstrap,
            Step::Done,
        ]
    }
    fn title(self) -> &'static str {
        match self {
            Step::Welcome => "Welcome",
            Step::Network => "Network identity",
            Step::Secrets => "Secrets",
            Step::Bootstrap => "Bootstrap",
            Step::Done => "Done",
        }
    }
    fn prev(self) -> Option<Step> {
        match self {
            Step::Welcome => None,
            Step::Network => Some(Step::Welcome),
            Step::Secrets => Some(Step::Network),
            Step::Bootstrap => None, // can't navigate back from a running install
            Step::Done => None,
        }
    }
}

/// ACME directory selection.
#[derive(Debug, Clone)]
enum AcmeChoice {
    Staging,
    Production,
    Custom(Input),
}

/// Which field on the Network step is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetworkFocus {
    Hostnames,
    AcmeRadio,
    AcmeCustomUrl,
    Email,
}

impl NetworkFocus {
    fn next(self, custom_acme: bool) -> NetworkFocus {
        match self {
            NetworkFocus::Hostnames => NetworkFocus::AcmeRadio,
            NetworkFocus::AcmeRadio if custom_acme => NetworkFocus::AcmeCustomUrl,
            NetworkFocus::AcmeRadio => NetworkFocus::Email,
            NetworkFocus::AcmeCustomUrl => NetworkFocus::Email,
            NetworkFocus::Email => NetworkFocus::Hostnames,
        }
    }
    fn prev(self, custom_acme: bool) -> NetworkFocus {
        match self {
            NetworkFocus::Hostnames => NetworkFocus::Email,
            NetworkFocus::AcmeRadio => NetworkFocus::Hostnames,
            NetworkFocus::AcmeCustomUrl => NetworkFocus::AcmeRadio,
            NetworkFocus::Email if custom_acme => NetworkFocus::AcmeCustomUrl,
            NetworkFocus::Email => NetworkFocus::AcmeRadio,
        }
    }
}

#[derive(Debug)]
struct NetworkState {
    hostnames: Input,
    acme: AcmeChoice,
    email: Input,
    focus: NetworkFocus,
}

/// Which field on the Secrets step is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretsFocus {
    CfRadio,
    CfTokenField,
    BackupRadio,
    BackupField,
    BackupConfirmField,
}

#[derive(Debug)]
struct SecretsState {
    cf_enabled: bool,
    cf_token: Input,
    cf_token_revealed: bool,
    backup_enabled: bool,
    backup: Input,
    backup_confirm: Input,
    backup_revealed: bool,
    focus: SecretsFocus,
}

impl SecretsState {
    fn focus_after(&self, focus: SecretsFocus, forward: bool) -> SecretsFocus {
        // Field order depends on the radios. Build a Vec of the active
        // focusables, then advance.
        let mut order = vec![SecretsFocus::CfRadio];
        if self.cf_enabled {
            order.push(SecretsFocus::CfTokenField);
        }
        order.push(SecretsFocus::BackupRadio);
        if self.backup_enabled {
            order.push(SecretsFocus::BackupField);
            order.push(SecretsFocus::BackupConfirmField);
        }
        let idx = order.iter().position(|f| *f == focus).unwrap_or(0);
        let n = order.len();
        let next_idx = if forward {
            (idx + 1) % n
        } else {
            (idx + n - 1) % n
        };
        order[next_idx]
    }
}

#[derive(Debug)]
struct BootstrapState {
    statuses: HashMap<BootstrapStep, StepStatus>,
    /// Last error: when set, the bootstrap step shows the failure footer
    /// and offers retry/abort.
    failed_step: Option<BootstrapStep>,
    /// Set once the install task has completed (success or failure).
    finished: bool,
}

impl BootstrapState {
    fn new() -> Self {
        let mut statuses = HashMap::new();
        for step in BootstrapStep::ordered() {
            statuses.insert(*step, StepStatus::Pending);
        }
        BootstrapState {
            statuses,
            failed_step: None,
            finished: false,
        }
    }

    fn apply(&mut self, evt: &ProgressEvent) {
        match evt {
            ProgressEvent::Started(s) => {
                self.statuses.insert(
                    *s,
                    StepStatus::Running {
                        started: Instant::now(),
                    },
                );
            }
            ProgressEvent::Finished { step, detail } => {
                let took = match self.statuses.get(step) {
                    Some(StepStatus::Running { started }) => started.elapsed(),
                    _ => Duration::ZERO,
                };
                self.statuses.insert(
                    *step,
                    StepStatus::Done {
                        detail: detail.clone(),
                        took,
                    },
                );
            }
            ProgressEvent::Failed { step, error } => {
                let took = match self.statuses.get(step) {
                    Some(StepStatus::Running { started }) => started.elapsed(),
                    _ => Duration::ZERO,
                };
                self.statuses.insert(
                    *step,
                    StepStatus::Failed {
                        error: error.clone(),
                        took,
                    },
                );
                self.failed_step = Some(*step);
            }
            ProgressEvent::Tick(_) => {}
            ProgressEvent::Summary(_) => {}
        }
    }
}

struct Wizard {
    args: InitArgs,
    host_ip: String,
    sysinfo: SystemInfo,

    step: Step,
    network: NetworkState,
    secrets: SecretsState,
    bootstrap: BootstrapState,
    summary: Option<SummaryData>,

    /// Animation tick counter (drives the spinner glyph index).
    tick: u64,

    /// One-line status message shown in the footer (e.g. "Token verified",
    /// "Hostnames invalid"). Auto-clears on next state change.
    flash: Option<(String, FlashKind)>,

    aborted: bool,
}

#[derive(Debug, Clone, Copy)]
enum FlashKind {
    /// Reserved for future "verified token" green pulse and other
    /// neutral / positive transient banners.
    #[allow(dead_code)]
    Info,
    #[allow(dead_code)]
    Ok,
    Error,
}

#[derive(Debug, Clone)]
struct SystemInfo {
    hostname: String,
    kernel: String,
    cgroup: String,
    systemd: String,
    cpu: String,
    mem: String,
}

impl SystemInfo {
    fn detect(host_ip: &str) -> Self {
        let hostname = std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());

        let kernel = std::fs::read_to_string("/proc/version")
            .map(|s| s.split_whitespace().nth(2).unwrap_or("unknown").to_string())
            .unwrap_or_else(|_| std::env::consts::OS.to_string());

        let cgroup = if Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
            "cgroup v2".to_string()
        } else if Path::new("/sys/fs/cgroup").exists() {
            "cgroup v1".to_string()
        } else {
            "no cgroup".to_string()
        };

        let systemd = std::process::Command::new("systemctl")
            .arg("--version")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| {
                s.lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1).map(|n| format!("systemd {n}")))
            })
            .unwrap_or_else(|| "systemd ?".to_string());

        // Cheap CPU/mem read; non-Linux gets "n/a".
        let cpu = std::fs::read_to_string("/proc/cpuinfo")
            .map(|s| {
                let cores = s.lines().filter(|l| l.starts_with("processor")).count();
                format!("{cores} cores")
            })
            .unwrap_or_else(|_| "cpu n/a".to_string());

        let mem = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("MemTotal:"))
                    .and_then(|l| {
                        let kb: u64 = l
                            .split_whitespace()
                            .nth(1)
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(0);
                        if kb == 0 {
                            None
                        } else {
                            Some(format!("{:.1} GB RAM", (kb as f64) / 1024.0 / 1024.0))
                        }
                    })
            })
            .unwrap_or_else(|| "mem n/a".to_string());

        let _ = host_ip; // included in footer separately
        SystemInfo {
            hostname,
            kernel,
            cgroup,
            systemd,
            cpu,
            mem,
        }
    }
}

// =====================================================================
// Wizard driver
// =====================================================================

async fn drive_wizard(
    term: &mut TtyTerminal,
    args: InitArgs,
    host_ip: String,
) -> Result<WizardOutcome> {
    let sysinfo = SystemInfo::detect(&host_ip);

    let mut wiz = Wizard {
        network: NetworkState {
            hostnames: Input::new(args.acme_domains.clone().unwrap_or_default()),
            acme: match args.acme_directory.as_deref() {
                Some("production") => AcmeChoice::Production,
                Some("staging") | None => AcmeChoice::Staging,
                Some(other) => AcmeChoice::Custom(Input::new(other.to_string())),
            },
            email: Input::new(args.acme_email.clone().unwrap_or_default()),
            focus: NetworkFocus::Hostnames,
        },
        secrets: SecretsState {
            cf_enabled: args.cf_dns_token.is_some(),
            cf_token: Input::new(args.cf_dns_token.clone().unwrap_or_default()),
            cf_token_revealed: false,
            backup_enabled: args.backup_passphrase_file.is_some(),
            backup: Input::new(
                args.backup_passphrase_file
                    .as_deref()
                    .and_then(|p| read_passphrase_file(p).ok())
                    .unwrap_or_default(),
            ),
            backup_confirm: Input::new(String::new()),
            backup_revealed: false,
            focus: SecretsFocus::CfRadio,
        },
        bootstrap: BootstrapState::new(),
        summary: None,
        tick: 0,
        flash: None,
        aborted: false,
        args,
        host_ip,
        sysinfo,
        step: Step::Welcome,
    };

    let mut events = EventStream::new();
    let mut redraw_interval = tokio::time::interval(Duration::from_millis(100));

    // Bootstrap channel + task. `None` until the operator hits Enter on
    // the Secrets step. Re-spawned on retry.
    let mut progress_rx: Option<mpsc::Receiver<ProgressEvent>> = None;
    let mut bootstrap_handle: Option<tokio::task::JoinHandle<Result<SummaryData>>> = None;

    use futures_util::StreamExt;
    loop {
        // Drain progress events without blocking.
        if let Some(rx) = progress_rx.as_mut() {
            while let Ok(evt) = rx.try_recv() {
                if let ProgressEvent::Summary(s) = &evt {
                    wiz.summary = Some(s.clone());
                }
                wiz.bootstrap.apply(&evt);
            }
        }

        // If the bootstrap task finished, surface its outcome.
        if let Some(handle) = &mut bootstrap_handle
            && handle.is_finished()
        {
            let h = bootstrap_handle.take().expect("checked");
            wiz.bootstrap.finished = true;
            // Drain any remaining events that landed before the task
            // future resolved (Summary fires last, so this catches it).
            if let Some(rx) = progress_rx.as_mut() {
                while let Ok(evt) = rx.try_recv() {
                    if let ProgressEvent::Summary(s) = &evt {
                        wiz.summary = Some(s.clone());
                    }
                    wiz.bootstrap.apply(&evt);
                }
            }
            match h.await {
                Ok(Ok(summary)) => {
                    wiz.summary = Some(summary);
                    wiz.step = Step::Done;
                }
                Ok(Err(e)) => {
                    wiz.flash = Some((format!("install failed: {e:#}"), FlashKind::Error));
                }
                Err(join_err) => {
                    wiz.flash = Some((
                        format!("bootstrap task panicked: {join_err}"),
                        FlashKind::Error,
                    ));
                }
            }
        }

        // Render.
        term.term.draw(|f| render(f, &wiz))?;

        // Wait for either an input event or a redraw tick.
        tokio::select! {
            _ = redraw_interval.tick() => {
                wiz.tick = wiz.tick.wrapping_add(1);
            }
            maybe_evt = events.next() => {
                match maybe_evt {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        let action = handle_key(&mut wiz, key);
                        match action {
                            KeyAction::Quit => break,
                            KeyAction::Continue => {}
                            KeyAction::SpawnBootstrap => {
                                let (tx, rx) = mpsc::channel(64);
                                progress_rx = Some(rx);
                                let plan = build_plan(&wiz);
                                bootstrap_handle = Some(tokio::spawn(async move {
                                    plan.execute_with_progress(Some(tx)).await
                                }));
                            }
                            KeyAction::RetryBootstrap => {
                                wiz.bootstrap = BootstrapState::new();
                                let (tx, rx) = mpsc::channel(64);
                                progress_rx = Some(rx);
                                let plan = build_plan(&wiz);
                                bootstrap_handle = Some(tokio::spawn(async move {
                                    plan.execute_with_progress(Some(tx)).await
                                }));
                            }
                        }
                    }
                    Some(Ok(_)) => {} // resize, mouse, paste: render handles them next tick
                    Some(Err(e)) => {
                        wiz.flash = Some((format!("input error: {e}"), FlashKind::Error));
                    }
                    None => break, // event stream closed
                }
            }
        }

        if wiz.aborted {
            break;
        }
    }

    if wiz.aborted {
        // Cancel the bootstrap task if it's still running. We don't try to
        // unwind partial state: that's the operator's job to clean up
        // (matches the legacy bash bootstrap's behavior).
        if let Some(h) = bootstrap_handle {
            h.abort();
        }
        return Ok(WizardOutcome::Aborted);
    }

    match wiz.summary {
        Some(s) => Ok(WizardOutcome::Completed(s)),
        None => Ok(WizardOutcome::Aborted),
    }
}

// =====================================================================
// Key handling
// =====================================================================

/// Action the main loop should take after dispatching a key.
#[derive(Debug, Clone, Copy)]
enum KeyAction {
    Continue,
    Quit,
    SpawnBootstrap,
    RetryBootstrap,
}

fn handle_key(wiz: &mut Wizard, key: KeyEvent) -> KeyAction {
    // Universal: Ctrl-C aborts immediately.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        wiz.aborted = true;
        return KeyAction::Quit;
    }
    match wiz.step {
        Step::Welcome => match key.code {
            KeyCode::Enter => {
                wiz.step = Step::Network;
                KeyAction::Continue
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                wiz.aborted = true;
                KeyAction::Quit
            }
            _ => KeyAction::Continue,
        },
        Step::Network => {
            handle_network_key(wiz, key);
            KeyAction::Continue
        }
        Step::Secrets => handle_secrets_key(wiz, key),
        Step::Bootstrap => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if wiz.bootstrap.failed_step.is_some() || wiz.bootstrap.finished {
                    wiz.aborted = true;
                    KeyAction::Quit
                } else {
                    // Install in flight: ignore. Ctrl-C is the escape hatch.
                    KeyAction::Continue
                }
            }
            KeyCode::Char('r') if wiz.bootstrap.failed_step.is_some() => KeyAction::RetryBootstrap,
            _ => KeyAction::Continue,
        },
        Step::Done => match key.code {
            KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc => KeyAction::Quit,
            _ => KeyAction::Continue,
        },
    }
}

fn handle_network_key(wiz: &mut Wizard, key: KeyEvent) {
    let custom_acme = matches!(wiz.network.acme, AcmeChoice::Custom(_));
    match key.code {
        KeyCode::Esc => {
            if let Some(prev) = Step::Network.prev() {
                wiz.step = prev;
            }
        }
        KeyCode::Tab => {
            wiz.network.focus = wiz.network.focus.next(custom_acme);
        }
        KeyCode::BackTab => {
            wiz.network.focus = wiz.network.focus.prev(custom_acme);
        }
        KeyCode::Up => match wiz.network.focus {
            NetworkFocus::AcmeRadio => {
                wiz.network.acme = match wiz.network.acme {
                    AcmeChoice::Staging => AcmeChoice::Custom(Input::new(String::new())),
                    AcmeChoice::Production => AcmeChoice::Staging,
                    AcmeChoice::Custom(_) => AcmeChoice::Production,
                };
            }
            _ => wiz.network.focus = wiz.network.focus.prev(custom_acme),
        },
        KeyCode::Down => match wiz.network.focus {
            NetworkFocus::AcmeRadio => {
                wiz.network.acme = match wiz.network.acme {
                    AcmeChoice::Staging => AcmeChoice::Production,
                    AcmeChoice::Production => AcmeChoice::Custom(Input::new(String::new())),
                    AcmeChoice::Custom(_) => AcmeChoice::Staging,
                };
            }
            _ => wiz.network.focus = wiz.network.focus.next(custom_acme),
        },
        KeyCode::Enter => {
            // Validate, then advance.
            if validate_network(wiz).is_ok() {
                wiz.step = Step::Secrets;
                wiz.flash = None;
            } else {
                wiz.flash = Some((
                    "fix the highlighted fields before continuing".to_string(),
                    FlashKind::Error,
                ));
            }
        }
        _ => {
            // Route to the focused input.
            match wiz.network.focus {
                NetworkFocus::Hostnames => {
                    wiz.network.hostnames.handle_event(&Event::Key(key));
                }
                NetworkFocus::AcmeRadio => {
                    // Number-key shortcuts: 1=staging, 2=prod, 3=custom.
                    if let KeyCode::Char(c) = key.code {
                        match c {
                            '1' => wiz.network.acme = AcmeChoice::Staging,
                            '2' => wiz.network.acme = AcmeChoice::Production,
                            '3' => {
                                if !custom_acme {
                                    wiz.network.acme =
                                        AcmeChoice::Custom(Input::new(String::new()));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                NetworkFocus::AcmeCustomUrl => {
                    if let AcmeChoice::Custom(ref mut inp) = wiz.network.acme {
                        inp.handle_event(&Event::Key(key));
                    }
                }
                NetworkFocus::Email => {
                    wiz.network.email.handle_event(&Event::Key(key));
                }
            }
        }
    }
}

fn handle_secrets_key(wiz: &mut Wizard, key: KeyEvent) -> KeyAction {
    match key.code {
        KeyCode::Esc => {
            if let Some(prev) = Step::Secrets.prev() {
                wiz.step = prev;
            }
            KeyAction::Continue
        }
        KeyCode::Tab => {
            wiz.secrets.focus = wiz.secrets.focus_after(wiz.secrets.focus, true);
            KeyAction::Continue
        }
        KeyCode::BackTab => {
            wiz.secrets.focus = wiz.secrets.focus_after(wiz.secrets.focus, false);
            KeyAction::Continue
        }
        KeyCode::Up | KeyCode::Down => {
            match wiz.secrets.focus {
                SecretsFocus::CfRadio => {
                    wiz.secrets.cf_enabled = !wiz.secrets.cf_enabled;
                }
                SecretsFocus::BackupRadio => {
                    wiz.secrets.backup_enabled = !wiz.secrets.backup_enabled;
                }
                _ => {
                    let forward = matches!(key.code, KeyCode::Down);
                    wiz.secrets.focus = wiz.secrets.focus_after(wiz.secrets.focus, forward);
                }
            }
            KeyAction::Continue
        }
        KeyCode::Char(' ') => {
            match wiz.secrets.focus {
                SecretsFocus::CfRadio => wiz.secrets.cf_enabled = !wiz.secrets.cf_enabled,
                SecretsFocus::BackupRadio => {
                    wiz.secrets.backup_enabled = !wiz.secrets.backup_enabled
                }
                _ => route_char_to_input(wiz, ' '),
            }
            KeyAction::Continue
        }
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match wiz.secrets.focus {
                SecretsFocus::CfTokenField => {
                    wiz.secrets.cf_token_revealed = !wiz.secrets.cf_token_revealed;
                }
                SecretsFocus::BackupField | SecretsFocus::BackupConfirmField => {
                    wiz.secrets.backup_revealed = !wiz.secrets.backup_revealed;
                }
                _ => {}
            }
            KeyAction::Continue
        }
        KeyCode::Enter => {
            if validate_secrets(wiz).is_ok() {
                wiz.step = Step::Bootstrap;
                wiz.bootstrap = BootstrapState::new();
                wiz.flash = None;
                KeyAction::SpawnBootstrap
            } else {
                wiz.flash = Some((
                    "fix the highlighted fields before continuing".to_string(),
                    FlashKind::Error,
                ));
                KeyAction::Continue
            }
        }
        _ => {
            match wiz.secrets.focus {
                SecretsFocus::CfTokenField => {
                    wiz.secrets.cf_token.handle_event(&Event::Key(key));
                }
                SecretsFocus::BackupField => {
                    wiz.secrets.backup.handle_event(&Event::Key(key));
                }
                SecretsFocus::BackupConfirmField => {
                    wiz.secrets.backup_confirm.handle_event(&Event::Key(key));
                }
                _ => {}
            }
            KeyAction::Continue
        }
    }
}

fn route_char_to_input(wiz: &mut Wizard, c: char) {
    let ch_event = Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    match wiz.secrets.focus {
        SecretsFocus::CfTokenField => {
            wiz.secrets.cf_token.handle_event(&ch_event);
        }
        SecretsFocus::BackupField => {
            wiz.secrets.backup.handle_event(&ch_event);
        }
        SecretsFocus::BackupConfirmField => {
            wiz.secrets.backup_confirm.handle_event(&ch_event);
        }
        _ => {}
    }
}

// =====================================================================
// Validation
// =====================================================================

fn validate_network(wiz: &Wizard) -> Result<()> {
    // Hostnames: comma-split, each must match the DNS label regex.
    let raw = wiz.network.hostnames.value().trim();
    if !raw.is_empty() {
        for part in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if !is_valid_hostname(part) {
                return Err(anyhow!("invalid hostname: {part}"));
            }
        }
    }
    // Email: blank is allowed (internal-only deploy), otherwise must look
    // vaguely like an email.
    let email = wiz.network.email.value().trim();
    if !email.is_empty() && !is_valid_email(email) {
        return Err(anyhow!("invalid email: {email}"));
    }
    // Custom ACME URL must parse as https://...
    if let AcmeChoice::Custom(inp) = &wiz.network.acme {
        let v = inp.value().trim();
        if v.is_empty() || !v.starts_with("https://") {
            return Err(anyhow!("custom ACME URL must start with https://"));
        }
    }
    Ok(())
}

fn validate_secrets(wiz: &Wizard) -> Result<()> {
    if wiz.secrets.cf_enabled {
        let v = wiz.secrets.cf_token.value().trim();
        if v.is_empty() {
            return Err(anyhow!("Cloudflare token field is empty"));
        }
    }
    if wiz.secrets.backup_enabled {
        let p = wiz.secrets.backup.value();
        let c = wiz.secrets.backup_confirm.value();
        if p.is_empty() {
            return Err(anyhow!("backup passphrase is empty"));
        }
        if p != c {
            return Err(anyhow!("backup passphrase confirmation does not match"));
        }
    }
    Ok(())
}

fn is_valid_hostname(s: &str) -> bool {
    // RFC1123 (relaxed): 1..253 chars, labels 1..63, alnum + '-' + '*' + '.'
    // (we allow leading "*." for wildcard certs).
    let s = s.trim_end_matches('.');
    if s.is_empty() || s.len() > 253 {
        return false;
    }
    for label in s.split('.') {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        // Wildcard label is the literal "*".
        if label == "*" {
            continue;
        }
        if label.starts_with('-') || label.ends_with('-') {
            return false;
        }
        for c in label.chars() {
            if !(c.is_ascii_alphanumeric() || c == '-') {
                return false;
            }
        }
    }
    true
}

fn is_valid_email(s: &str) -> bool {
    // Cheap check: exactly one '@', at least one '.' after '@', no
    // whitespace. Good enough for live UI feedback; real validation
    // happens at ACME registration time.
    if s.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let mut parts = s.split('@');
    let local = parts.next().unwrap_or("");
    let domain = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return false;
    }
    if local.is_empty() || domain.is_empty() {
        return false;
    }
    domain.contains('.')
}

// =====================================================================
// Plan construction
// =====================================================================

fn build_plan(wiz: &Wizard) -> Plan {
    let acme_directory = match &wiz.network.acme {
        AcmeChoice::Staging => "staging".to_string(),
        AcmeChoice::Production => "production".to_string(),
        AcmeChoice::Custom(inp) => inp.value().to_string(),
    };
    let extra_sans: Vec<String> = wiz
        .network
        .hostnames
        .value()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Plan {
        acme_email: wiz.network.email.value().to_string(),
        acme_domains: wiz.network.hostnames.value().to_string(),
        acme_directory,
        cf_dns_token: if wiz.secrets.cf_enabled && !wiz.secrets.cf_token.value().is_empty() {
            Some(wiz.secrets.cf_token.value().to_string())
        } else {
            None
        },
        backup_passphrase: if wiz.secrets.backup_enabled && !wiz.secrets.backup.value().is_empty() {
            Some(wiz.secrets.backup.value().to_string())
        } else {
            None
        },
        extra_sans,
        listen_grpc: wiz.args.listen_grpc.clone(),
        listen_dashboard: wiz.args.listen_dashboard.clone(),
        state_dir: wiz.args.state_dir.clone(),
        etc_dir: wiz.args.etc_dir.clone(),
        runtime: wiz.args.runtime.clone(),
        force: wiz.args.force,
        host_ip: wiz.host_ip.clone(),
    }
}

// =====================================================================
// Rendering
// =====================================================================

fn render(f: &mut ratatui::Frame, wiz: &Wizard) {
    let area = f.area();
    if area.width < 70 || area.height < 18 {
        render_too_small(f, area);
        return;
    }

    // Header (1 line) + body + footer (2 lines).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(10),   // body
            Constraint::Length(4), // footer (info + keys + flash)
        ])
        .split(area);

    render_header(f, chunks[0], wiz);

    // Body: sidebar + content.
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(24), Constraint::Min(40)])
        .split(chunks[1]);

    render_sidebar(f, body[0], wiz);
    render_content(f, body[1], wiz);
    render_footer(f, chunks[2], wiz);
}

fn render_too_small(f: &mut ratatui::Frame, area: Rect) {
    let p = Paragraph::new(Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            "Terminal too small",
            Style::default()
                .fg(theme::FAILURE)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Resize to at least 70 cols x 18 rows."),
    ]))
    .alignment(Alignment::Center);
    f.render_widget(Clear, area);
    f.render_widget(p, area);
}

fn render_header(f: &mut ratatui::Frame, area: Rect, _wiz: &Wizard) {
    let title = Span::styled(
        format!(" Isengard installer · v{} ", env!("CARGO_PKG_VERSION")),
        Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border_unfocused())
        .title(title);
    f.render_widget(block, area);
}

fn render_sidebar(f: &mut ratatui::Frame, area: Rect, wiz: &Wizard) {
    let mut lines: Vec<Line> = Vec::with_capacity(Step::ordered().len() * 2 + 4);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Steps",
        Style::default()
            .fg(theme::HINT)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    let active_idx = Step::ordered()
        .iter()
        .position(|s| *s == wiz.step)
        .unwrap_or(0);

    for (i, step) in Step::ordered().iter().enumerate() {
        let (icon, style) = if i < active_idx {
            ("✓", theme::completed_step())
        } else if i == active_idx {
            ("▸", theme::active_step())
        } else {
            ("○", theme::pending_step())
        };
        // Bootstrap may have failed; flag it red even though it's the active step.
        let (icon, style) = if *step == Step::Bootstrap && wiz.bootstrap.failed_step.is_some() {
            ("✗", theme::failed_step())
        } else {
            (icon, style)
        };
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(icon, style),
            Span::raw(" "),
            Span::styled(step.title(), style),
        ]));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border_unfocused());
    let p = Paragraph::new(Text::from(lines)).block(block);
    f.render_widget(p, area);
}

fn render_content(f: &mut ratatui::Frame, area: Rect, wiz: &Wizard) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border_unfocused())
        .title(Span::styled(
            format!(" {} ", wiz.step.title()),
            theme::title(),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    match wiz.step {
        Step::Welcome => render_welcome(f, inner, wiz),
        Step::Network => render_network(f, inner, wiz),
        Step::Secrets => render_secrets(f, inner, wiz),
        Step::Bootstrap => render_bootstrap(f, inner, wiz),
        Step::Done => render_done(f, inner, wiz),
    }
}

fn render_welcome(f: &mut ratatui::Frame, area: Rect, wiz: &Wizard) {
    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Length(1),
            Constraint::Length(7),
            Constraint::Min(1),
        ])
        .split(inner);

    // Banner. tui-big-text returns a widget; we render with the default
    // pixel size (Quadrant), centered horizontally by the layout.
    let banner = BigText::builder()
        .pixel_size(PixelSize::Quadrant)
        .alignment(Alignment::Left)
        .style(Style::default().fg(theme::ACCENT))
        .lines(vec![Line::from("ISENGARD")])
        .build();
    f.render_widget(banner, layout[0]);

    let intro = Paragraph::new(Text::from(vec![
        Line::from(Span::styled(
            "Single-binary container fleet manager: controller + agent + dashboard + ACME, all in one box.",
            theme::body(),
        )),
    ]))
    .wrap(Wrap { trim: true });
    f.render_widget(intro, layout[2]);

    // System info table.
    let mut info = Vec::new();
    info.push(detail_row("Host", &wiz.sysinfo.hostname));
    info.push(detail_row(
        "IP",
        if wiz.host_ip.is_empty() {
            "(none)"
        } else {
            &wiz.host_ip
        },
    ));
    info.push(detail_row("Kernel", &wiz.sysinfo.kernel));
    info.push(detail_row("Init", &wiz.sysinfo.systemd));
    info.push(detail_row("CGroup", &wiz.sysinfo.cgroup));
    info.push(detail_row("CPU", &wiz.sysinfo.cpu));
    info.push(detail_row("Mem", &wiz.sysinfo.mem));
    let info_p = Paragraph::new(Text::from(info));
    f.render_widget(info_p, layout[4]);
}

fn detail_row<'a>(label: &'a str, value: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {label:<8}"), theme::hint()),
        Span::styled(value.to_string(), theme::body()),
    ])
}

fn render_network(f: &mut ratatui::Frame, area: Rect, wiz: &Wizard) {
    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    // Layout: hostnames (3) + acme (5) + custom-url (3 if shown) + email (3).
    let custom = matches!(wiz.network.acme, AcmeChoice::Custom(_));
    let constraints: Vec<Constraint> = if custom {
        vec![
            Constraint::Length(4), // hostnames
            Constraint::Length(1),
            Constraint::Length(5), // acme radio
            Constraint::Length(1),
            Constraint::Length(4), // custom url
            Constraint::Length(1),
            Constraint::Length(4), // email
            Constraint::Min(0),
        ]
    } else {
        vec![
            Constraint::Length(4),
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Min(0),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    // Hostnames field.
    let hostnames_valid = {
        let raw = wiz.network.hostnames.value().trim();
        raw.is_empty()
            || raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .all(is_valid_hostname)
    };
    render_input_field(
        f,
        chunks[0],
        "Public hostnames (cert SANs)",
        &wiz.network.hostnames,
        wiz.network.focus == NetworkFocus::Hostnames,
        false,
        hostnames_valid,
        Some("Comma-separated. localhost + the host IP are added automatically."),
    );

    // ACME radio.
    let acme_focused = wiz.network.focus == NetworkFocus::AcmeRadio;
    render_radio_block(
        f,
        chunks[2],
        "ACME directory",
        &[
            (
                "Let's Encrypt staging",
                matches!(wiz.network.acme, AcmeChoice::Staging),
            ),
            (
                "Let's Encrypt production",
                matches!(wiz.network.acme, AcmeChoice::Production),
            ),
            (
                "Custom directory URL",
                matches!(wiz.network.acme, AcmeChoice::Custom(_)),
            ),
        ],
        acme_focused,
    );

    let mut email_idx = 4;
    if custom {
        if let AcmeChoice::Custom(ref inp) = wiz.network.acme {
            let valid = inp.value().trim().starts_with("https://");
            render_input_field(
                f,
                chunks[4],
                "Custom ACME directory URL",
                inp,
                wiz.network.focus == NetworkFocus::AcmeCustomUrl,
                false,
                valid || inp.value().is_empty(),
                Some("Full https:// URL of the ACME directory endpoint."),
            );
        }
        email_idx = 6;
    }

    let email_valid = {
        let v = wiz.network.email.value().trim();
        v.is_empty() || is_valid_email(v)
    };
    render_input_field(
        f,
        chunks[email_idx],
        "ACME contact email",
        &wiz.network.email,
        wiz.network.focus == NetworkFocus::Email,
        false,
        email_valid,
        Some("Optional. Required only for public Let's Encrypt issuance."),
    );
}

fn render_secrets(f: &mut ratatui::Frame, area: Rect, wiz: &Wizard) {
    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    // Layout: cf yes/no, [cf field], gap, backup yes/no, [backup, confirm].
    let mut constraints: Vec<Constraint> = vec![Constraint::Length(4)]; // cf radio
    if wiz.secrets.cf_enabled {
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(4));
    }
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Length(4)); // backup radio
    if wiz.secrets.backup_enabled {
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(4));
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(4));
    }
    constraints.push(Constraint::Min(0));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let mut idx = 0;
    render_radio_block(
        f,
        chunks[idx],
        "Cloudflare DNS API token",
        &[
            ("Yes, I have one", wiz.secrets.cf_enabled),
            ("No, set later", !wiz.secrets.cf_enabled),
        ],
        wiz.secrets.focus == SecretsFocus::CfRadio,
    );
    idx += 1;

    if wiz.secrets.cf_enabled {
        idx += 1; // gap
        let valid = !wiz.secrets.cf_token.value().is_empty();
        render_input_field(
            f,
            chunks[idx],
            "Cloudflare DNS API token",
            &wiz.secrets.cf_token,
            wiz.secrets.focus == SecretsFocus::CfTokenField,
            !wiz.secrets.cf_token_revealed,
            valid,
            Some("Hidden. Encrypted with the master key. [Ctrl-V] reveals."),
        );
        idx += 1;
    }

    idx += 1; // gap before backup radio

    render_radio_block(
        f,
        chunks[idx],
        "Backup passphrase (encrypted snapshots)",
        &[
            ("Yes, set one now", wiz.secrets.backup_enabled),
            ("No, skip", !wiz.secrets.backup_enabled),
        ],
        wiz.secrets.focus == SecretsFocus::BackupRadio,
    );
    idx += 1;

    if wiz.secrets.backup_enabled {
        idx += 1; // gap
        let valid = !wiz.secrets.backup.value().is_empty();
        render_input_field(
            f,
            chunks[idx],
            "Backup passphrase",
            &wiz.secrets.backup,
            wiz.secrets.focus == SecretsFocus::BackupField,
            !wiz.secrets.backup_revealed,
            valid,
            Some("Hidden. [Ctrl-V] reveals."),
        );
        idx += 1;
        idx += 1; // gap
        let confirm_valid = wiz.secrets.backup.value() == wiz.secrets.backup_confirm.value()
            && !wiz.secrets.backup_confirm.value().is_empty();
        render_input_field(
            f,
            chunks[idx],
            "Confirm backup passphrase",
            &wiz.secrets.backup_confirm,
            wiz.secrets.focus == SecretsFocus::BackupConfirmField,
            !wiz.secrets.backup_revealed,
            confirm_valid,
            Some("Matches the field above when both are set."),
        );
    }
}

fn render_bootstrap(f: &mut ratatui::Frame, area: Rect, wiz: &Wizard) {
    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Installing Isengard…",
        theme::title(),
    )));
    lines.push(Line::from(""));

    for step in BootstrapStep::ordered() {
        let status = wiz
            .bootstrap
            .statuses
            .get(step)
            .cloned()
            .unwrap_or(StepStatus::Pending);
        let (icon, color) = match &status {
            StepStatus::Pending => ("○", theme::HINT),
            StepStatus::Running { .. } => (spinner_glyph(wiz.tick), theme::PENDING),
            StepStatus::Done { .. } => ("✓", theme::SUCCESS),
            StepStatus::Failed { .. } => ("✗", theme::FAILURE),
        };
        let label = step.label();

        let mut spans = vec![
            Span::styled(
                format!(" {icon} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(label, Style::default().fg(theme::TEXT)),
        ];

        match &status {
            StepStatus::Done { detail, took } => {
                if let Some(d) = detail {
                    spans.push(Span::styled(format!("  · {d}"), theme::hint()));
                }
                spans.push(Span::styled(
                    format!("  ({:.1}s)", took.as_secs_f64()),
                    theme::hint(),
                ));
            }
            StepStatus::Running { started } => {
                spans.push(Span::styled(
                    format!("  ({:.1}s)", started.elapsed().as_secs_f64()),
                    theme::hint(),
                ));
            }
            StepStatus::Failed { error, .. } => {
                spans.push(Span::styled(
                    format!("  · {error}"),
                    Style::default().fg(theme::FAILURE),
                ));
            }
            StepStatus::Pending => {}
        }

        lines.push(Line::from(spans));
    }

    if let Some(failed_step) = wiz.bootstrap.failed_step {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "Step '{}' failed. Press [r] to retry, [q] to abort.",
                failed_step.label()
            ),
            Style::default()
                .fg(theme::FAILURE)
                .add_modifier(Modifier::BOLD),
        )));
    }

    let p = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

fn render_done(f: &mut ratatui::Frame, area: Rect, wiz: &Wizard) {
    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8), // banner
            Constraint::Length(1),
            Constraint::Min(8), // summary
        ])
        .split(inner);

    let banner = BigText::builder()
        .pixel_size(PixelSize::Quadrant)
        .alignment(Alignment::Left)
        .style(Style::default().fg(theme::SUCCESS))
        .lines(vec![Line::from("READY")])
        .build();
    f.render_widget(banner, layout[0]);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(s) = &wiz.summary {
        lines.push(detail_row("Local", &s.controller_url_local));
        lines.push(detail_row("Remote", &s.controller_url_remote));
        lines.push(detail_row("Dashboard", &s.dashboard_url));
        lines.push(detail_row("State", &s.state_dir));
        lines.push(detail_row("CA pem", &s.ca_pem_path));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Add another host (token expires in 15 minutes):",
            theme::hint(),
        )));
        lines.push(Line::from(Span::styled(
            format!(
                "    isengard join --token {} --ca-pem-path {} {}",
                s.token, s.ca_pem_path, s.controller_url_remote
            ),
            theme::body(),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "(no summary available)",
            theme::hint(),
        )));
    }
    let p = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    f.render_widget(p, layout[2]);
}

// =====================================================================
// Field widgets
// =====================================================================

fn render_input_field(
    f: &mut ratatui::Frame,
    area: Rect,
    label: &str,
    input: &Input,
    focused: bool,
    masked: bool,
    valid: bool,
    hint: Option<&str>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);

    // Label row.
    let label_line = Line::from(vec![Span::styled(
        format!("  {label}"),
        if focused {
            theme::title()
        } else {
            Style::default().fg(theme::TEXT)
        },
    )]);
    f.render_widget(Paragraph::new(label_line), chunks[0]);

    // Input row: cursor caret + value (masked or plain).
    let display_value = if masked {
        "•".repeat(input.value().chars().count())
    } else {
        input.value().to_string()
    };

    let prefix = if focused {
        Span::styled(
            "  ▶ ",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("    ", Style::default())
    };

    let underline_style = if !valid {
        Style::default()
            .fg(theme::FAILURE)
            .add_modifier(Modifier::UNDERLINED)
    } else if focused {
        Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
            .fg(theme::HINT)
            .add_modifier(Modifier::UNDERLINED)
    };

    let value_span = if display_value.is_empty() {
        Span::styled(
            " (empty)",
            Style::default()
                .fg(theme::HINT)
                .add_modifier(Modifier::ITALIC),
        )
    } else {
        Span::styled(display_value, Style::default().fg(theme::TEXT))
    };
    let value_line = Line::from(vec![prefix, value_span]);
    f.render_widget(Paragraph::new(value_line), chunks[1]);

    // Hint row.
    if let Some(h) = hint {
        let hint_line = Line::from(vec![Span::styled(format!("    {h}"), theme::hint())]);
        let _ = underline_style; // styling baked into value_line border below if we add it
        f.render_widget(Paragraph::new(hint_line), chunks[2]);
    }

    // Set the cursor position when this field is focused so the operator
    // sees a caret in their terminal. tui-input gives a 0-based index in
    // the visible value; offset for the prefix.
    if focused {
        let offset_x = chunks[1].x + 4 + input.visual_cursor() as u16;
        if offset_x < area.x + area.width {
            f.set_cursor_position((offset_x, chunks[1].y));
        }
    }
}

fn render_radio_block(
    f: &mut ratatui::Frame,
    area: Rect,
    label: &str,
    options: &[(&str, bool)],
    focused: bool,
) {
    let mut lines: Vec<Line> = Vec::with_capacity(options.len() + 1);
    lines.push(Line::from(vec![Span::styled(
        format!("  {label}"),
        if focused {
            theme::title()
        } else {
            Style::default().fg(theme::TEXT)
        },
    )]));
    for (i, (opt, selected)) in options.iter().enumerate() {
        let icon = if *selected { "●" } else { "○" };
        let style = if focused && *selected {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else if *selected {
            Style::default().fg(theme::ACCENT)
        } else {
            Style::default().fg(theme::TEXT)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("    {} ", i + 1), theme::hint()),
            Span::styled(format!("{icon} "), style),
            Span::styled(opt.to_string(), style),
        ]));
    }
    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn render_footer(f: &mut ratatui::Frame, area: Rect, wiz: &Wizard) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);

    // System info line.
    let info = format!(
        " Detected: {} ({}) · {} · {} · {} ",
        wiz.sysinfo.hostname,
        if wiz.host_ip.is_empty() {
            "no IP"
        } else {
            wiz.host_ip.as_str()
        },
        wiz.sysinfo.cgroup,
        wiz.sysinfo.systemd,
        wiz.sysinfo.cpu,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(info, theme::hint()))),
        chunks[0],
    );

    // Key bindings.
    let keys = match wiz.step {
        Step::Welcome => "[Enter] start  [q/Esc] quit",
        Step::Network | Step::Secrets => {
            "[Tab] next field  [↑↓] navigate  [Space] toggle  [Enter] continue  [Esc] back  [Ctrl-C] abort"
        }
        Step::Bootstrap => {
            if wiz.bootstrap.failed_step.is_some() {
                "[r] retry  [q] abort"
            } else {
                "(install in progress)  [Ctrl-C] force-abort"
            }
        }
        Step::Done => "[Enter/q] exit",
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {keys}"), theme::hint()))),
        chunks[1],
    );

    // Flash row.
    if let Some((msg, kind)) = &wiz.flash {
        let style = match kind {
            FlashKind::Info => theme::body(),
            FlashKind::Ok => Style::default().fg(theme::SUCCESS),
            FlashKind::Error => Style::default()
                .fg(theme::FAILURE)
                .add_modifier(Modifier::BOLD),
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(msg.clone(), style),
            ])),
            chunks[2],
        );
    }
}

// Animation helpers.
fn spinner_glyph(tick: u64) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[(tick as usize) % FRAMES.len()]
}

// =====================================================================
// Post-wizard summary print (plain stdout)
// =====================================================================

fn print_summary(s: &SummaryData) {
    println!();
    println!("=================================================================");
    println!("Isengard is up.");
    println!();
    println!("Controller (local):  {}", s.controller_url_local);
    println!("Controller (remote): {}", s.controller_url_remote);
    println!("Dashboard:           {}", s.dashboard_url);
    println!();
    println!("To enroll another host, run on that host:");
    println!();
    println!(
        "  isengard join \\\n    --token {} \\\n    --ca-pem-path {} \\\n    {}",
        s.token, s.ca_pem_path, s.controller_url_remote
    );
    println!();
    println!(
        "(Copy {} from this host to the joining host first,",
        s.ca_pem_path
    );
    println!(" or paste it via --ca-pem-base64. Token expires in 15 minutes.)");
    println!();
    println!("Logs:    journalctl -u iso-controller -f");
    println!("         journalctl -u iso-agent -f");
    println!("Status:  systemctl status iso-controller iso-agent");
    println!("=================================================================");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_accepts_simple_labels() {
        assert!(is_valid_hostname("example.com"));
        assert!(is_valid_hostname("foo"));
        assert!(is_valid_hostname("a.b.c.d"));
        assert!(is_valid_hostname("dashboard.lab.example.org"));
    }

    #[test]
    fn hostname_accepts_wildcard_prefix() {
        assert!(is_valid_hostname("*.example.com"));
    }

    #[test]
    fn hostname_rejects_invalid_chars() {
        assert!(!is_valid_hostname("under_score.com"));
        assert!(!is_valid_hostname("space inside.com"));
        assert!(!is_valid_hostname("-leading.com"));
        assert!(!is_valid_hostname("trailing-.com"));
        assert!(!is_valid_hostname(""));
    }

    #[test]
    fn hostname_rejects_oversize_labels() {
        let too_long: String = "a".repeat(64);
        let host = format!("{too_long}.com");
        assert!(!is_valid_hostname(&host));
    }

    #[test]
    fn email_accepts_basic_addresses() {
        assert!(is_valid_email("a@b.c"));
        assert!(is_valid_email("operator@example.com"));
        assert!(is_valid_email("a.b+tag@sub.example.org"));
    }

    #[test]
    fn email_rejects_obvious_garbage() {
        assert!(!is_valid_email(""));
        assert!(!is_valid_email("no-at-sign"));
        assert!(!is_valid_email("@only.right"));
        assert!(!is_valid_email("only.left@"));
        assert!(!is_valid_email("two@@signs.com"));
        assert!(!is_valid_email("space @inside.com"));
        assert!(!is_valid_email("no-tld@example"));
    }

    #[test]
    fn step_prev_chain_terminates_at_welcome() {
        assert_eq!(Step::Welcome.prev(), None);
        assert_eq!(Step::Network.prev(), Some(Step::Welcome));
        assert_eq!(Step::Secrets.prev(), Some(Step::Network));
        assert_eq!(Step::Bootstrap.prev(), None);
        assert_eq!(Step::Done.prev(), None);
    }

    #[test]
    fn spinner_glyph_cycles() {
        let a = spinner_glyph(0);
        let b = spinner_glyph(1);
        assert_ne!(a, b);
        // Wraparound consistency: tick % FRAMES.len() lands back on the
        // first frame after a full cycle.
        assert_eq!(spinner_glyph(0), spinner_glyph(10));
    }
}
