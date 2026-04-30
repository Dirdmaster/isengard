//! Isengard binary entry point. Parses subcommand, sets up tracing, dispatches
//! to either the controller or agent runner.

use std::io::IsTerminal;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[cfg(feature = "dev")]
mod dev_plugin;

#[derive(Debug, Parser)]
#[command(
    name = "isengard",
    version,
    about = "Isengard container management platform"
)]
struct Cli {
    /// Logging filter (e.g. "info", "debug,isengard=trace"). Read from
    /// `RUST_LOG` env var if not set.
    #[arg(long, global = true, env = "ISENGARD_LOG")]
    log: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run in controller mode: aggregates agent state, hosts the dashboard
    /// and notifier plugins, distributes config.
    Controller {
        /// HTTP/gRPC listen address.
        #[arg(long, env = "ISENGARD_LISTEN", default_value = "0.0.0.0:9417")]
        listen: String,
        /// Directory where the controller stores mutable state (SQLite db, etc).
        /// Created if missing.
        #[arg(long, env = "ISENGARD_STATE_DIR", default_value = "/var/lib/isengard")]
        state_dir: std::path::PathBuf,
    },
    /// Run in agent mode: registers with a controller, runs agent-side plugins
    /// (updater).
    Agent {
        /// URL of the controller, e.g. `http://controller.example.com:9417`.
        #[arg(long, env = "ISENGARD_CONTROLLER")]
        controller: String,
        /// Directory where the agent persists state (agent.json, etc).
        /// Created if missing.
        #[arg(long, env = "ISENGARD_STATE_DIR", default_value = "/var/lib/isengard")]
        state_dir: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = cli.log.as_deref().map(EnvFilter::new).unwrap_or_else(|| {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    });
    let use_ansi = std::io::stderr().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(use_ansi)
        .init();

    match cli.command {
        Command::Controller { listen, state_dir } => {
            let token = std::env::var("ISENGARD_TOKEN")
                .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;
            let _ = token; // unused in this task; Task 2 wires it into TokenAuthLayer

            let listen_addr: std::net::SocketAddr = listen
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid --listen address {listen:?}: {e}"))?;

            // Create state dir if missing (controller can't run without somewhere to put the db).
            std::fs::create_dir_all(&state_dir)
                .map_err(|e| anyhow::anyhow!("creating state dir {state_dir:?}: {e}"))?;

            tracing::info!(%listen_addr, ?state_dir, "controller mode");
            isengard_controller::run_controller(isengard_controller::ControllerOptions {
                listen: listen_addr,
                state_dir,
                config: serde_json::Value::Object(Default::default()),
            })
            .await
        }
        Command::Agent {
            controller,
            state_dir,
        } => {
            let _token = std::env::var("ISENGARD_TOKEN")
                .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;

            std::fs::create_dir_all(&state_dir)
                .map_err(|e| anyhow::anyhow!("creating state dir {state_dir:?}: {e}"))?;

            tracing::info!(%controller, ?state_dir, "agent mode");
            isengard_agent::run_agent(isengard_agent::AgentOptions {
                controller_url: controller,
                state_dir,
                config: serde_json::Value::Object(Default::default()),
            })
            .await
        }
    }
}
