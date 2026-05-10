//! Bootstrap progress events streamed from `Plan::execute_with_progress`
//! into the wizard's TUI. The non-interactive path is event-free (the
//! sender is `None`) so existing CLI behavior is unchanged.

use std::time::{Duration, Instant};

/// One discrete substep of the install. Order matches `Plan::execute`'s
/// numbered phases. The wizard renders one row per step with a status
/// icon (pending / spinner / check / cross).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BootstrapStep {
    SetupDirs,
    GenerateMasterKey,
    WriteMasterKeyEnv,
    BootstrapSecrets,
    WriteEnvFile,
    InstallSystemdUnits,
    StartController,
    WaitControllerHealthy,
    ExportCa,
    MintToken,
    WriteTokenFile,
    StartAgent,
}

impl BootstrapStep {
    /// Ordered list rendered in the bootstrap step list. New steps must
    /// be appended in execution order; the wizard relies on this order.
    pub fn ordered() -> &'static [BootstrapStep] {
        &[
            BootstrapStep::SetupDirs,
            BootstrapStep::GenerateMasterKey,
            BootstrapStep::WriteMasterKeyEnv,
            BootstrapStep::BootstrapSecrets,
            BootstrapStep::WriteEnvFile,
            BootstrapStep::InstallSystemdUnits,
            BootstrapStep::StartController,
            BootstrapStep::WaitControllerHealthy,
            BootstrapStep::ExportCa,
            BootstrapStep::MintToken,
            BootstrapStep::WriteTokenFile,
            BootstrapStep::StartAgent,
        ]
    }

    /// Short human label shown in the wizard's bootstrap row.
    pub fn label(self) -> &'static str {
        match self {
            BootstrapStep::SetupDirs => "Create state and config directories",
            BootstrapStep::GenerateMasterKey => "Generate master key",
            BootstrapStep::WriteMasterKeyEnv => "Write master-key.env",
            BootstrapStep::BootstrapSecrets => "Encrypt secrets to controller store",
            BootstrapStep::WriteEnvFile => "Write isengard.env",
            BootstrapStep::InstallSystemdUnits => "Install systemd units",
            BootstrapStep::StartController => "Start iso-controller.service",
            BootstrapStep::WaitControllerHealthy => "Wait for controller health",
            BootstrapStep::ExportCa => "Export controller CA",
            BootstrapStep::MintToken => "Mint enrollment token",
            BootstrapStep::WriteTokenFile => "Persist agent-token.env",
            BootstrapStep::StartAgent => "Start iso-agent.service",
        }
    }
}

/// Per-step status. `Done` carries an optional human detail (e.g. the
/// path written, the count of secrets, the controller URL).
#[derive(Debug, Clone)]
pub enum StepStatus {
    Pending,
    Running {
        started: Instant,
    },
    Done {
        detail: Option<String>,
        took: Duration,
    },
    Failed {
        error: String,
        // `took` is captured so the wizard can render duration on the
        // failed row in a future iteration; not consumed today.
        #[allow(dead_code)]
        took: Duration,
    },
}

/// Events emitted by the install pipeline. The wizard merges these into
/// its per-step status table; the non-interactive caller drops them.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    Started(BootstrapStep),
    Finished {
        step: BootstrapStep,
        detail: Option<String>,
    },
    Failed {
        step: BootstrapStep,
        error: String,
    },
    /// Tick from a long-running step (e.g. controller health poll). The
    /// wizard re-renders the spinner+elapsed counter without changing
    /// status. The step field is informational; today the wizard only
    /// consumes the implicit redraw, not the step value.
    Tick(#[allow(dead_code)] BootstrapStep),
    /// Completion summary, with the data the Done screen renders.
    Summary(SummaryData),
}

/// Data shown on the wizard's Done screen. Mirrors the plain-text banner
/// the legacy non-interactive flow prints: controller URLs, dashboard,
/// the join command for additional hosts.
#[derive(Debug, Clone)]
pub struct SummaryData {
    pub controller_url_local: String,
    pub controller_url_remote: String,
    pub dashboard_url: String,
    /// Stored for future "copy host IP" affordance; not surfaced today.
    #[allow(dead_code)]
    pub host_ip: String,
    pub token: String,
    pub ca_pem_path: String,
    pub state_dir: String,
}
