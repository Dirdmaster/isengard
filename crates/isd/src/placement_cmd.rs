//! `isd placement show`: print the controller's placement grid.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use comfy_table::{ContentArrangement, Table, presets::NOTHING};
use serde::{Deserialize, Serialize};

use crate::session::Session;

/// CLI flags for `isd placement`.
#[derive(Debug, Args)]
pub struct PlacementArgs {
    /// Resolved sub-verb.
    #[command(subcommand)]
    pub command: PlacementCommand,
}

/// Sub-verbs under `isd placement`. Canonical: `ls` (alias: `show`).
#[derive(Debug, Subcommand)]
pub enum PlacementCommand {
    /// Print the placement grid (one row per replica).
    #[command(alias = "show")]
    Ls(ShowArgs),
}

/// CLI flags for `isd placement show`.
#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Filter by stack id.
    #[arg(long)]
    pub stack_id: Option<i64>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = crate::output::Format::Table)]
    pub format: crate::output::Format,
}

/// One row from the controller's `GET /api/v1/placements` endpoint:
/// one replica's assignment to a host.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlacementRow {
    /// Service surrogate key.
    pub service_id: i64,
    /// Operator-facing service name; `None` when the controller hasn't
    /// joined the placement back to a service row.
    pub service_name: Option<String>,
    /// Owning stack surrogate key; `None` for unstacked services.
    pub stack_id: Option<i64>,
    /// Host ULID rendered as the canonical 26-char base32.
    pub host_id: String,
    /// Reported hostname; falls back to host_id in the table render
    /// when absent.
    pub hostname: Option<String>,
    /// Zero-based replica index within the service.
    pub replica_index: u32,
    /// Placement state (`assigned`, `running`, `failed`, ...). Free-form
    /// string from the controller; not enum-validated client-side.
    pub state: String,
    /// When the assignment was first made.
    pub assigned_at: DateTime<Utc>,
    /// Optional last-event marker for diagnostics. Currently rendered
    /// only as part of `--format json` output.
    pub last_event: Option<String>,
}

/// Dispatch to the matching `placement` sub-verb.
///
/// # Errors
///
/// Propagates the sub-verb's error: HTTP failures, JSON decode errors,
/// or the controller-less actionable error from `require_controller`.
pub async fn run(args: PlacementArgs, context: Option<&str>) -> Result<()> {
    match args.command {
        PlacementCommand::Ls(a) => run_show(a, context).await,
    }
}

/// Fetch placements from the controller and render the grid.
///
/// Hits `GET /api/v1/placements/by-stack/<id>` when `--stack-id` is set,
/// otherwise the full `GET /api/v1/placements`. Empty results render as
/// `no placements` on stderr (table mode) or an empty JSON array.
///
/// # Errors
///
/// Returns `Err` on HTTP failures or response decode errors.
async fn run_show(args: ShowArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;
    let url = match args.stack_id {
        Some(id) => format!("{controller_url}/api/v1/placements/by-stack/{id}"),
        None => format!("{controller_url}/api/v1/placements"),
    };
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let rows: Vec<PlacementRow> = resp
        .error_for_status()
        .context("listing placements")?
        .json()
        .await
        .context("decoding placements JSON")?;

    match args.format {
        crate::output::Format::Json => {
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        crate::output::Format::Table => {
            if rows.is_empty() {
                eprintln!("no placements");
                return Ok(());
            }
            let mut table = Table::new();
            table
                .load_preset(NOTHING)
                .set_content_arrangement(ContentArrangement::Dynamic);
            table.set_header(vec![
                "STACK", "SERVICE", "REPLICA", "HOST", "STATE", "ASSIGNED",
            ]);
            for row in &rows {
                table.add_row(vec![
                    row.stack_id
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "-".into()),
                    row.service_name
                        .clone()
                        .unwrap_or_else(|| row.service_id.to_string()),
                    row.replica_index.to_string(),
                    row.hostname.clone().unwrap_or_else(|| row.host_id.clone()),
                    row.state.clone(),
                    row.assigned_at.to_rfc3339(),
                ]);
            }
            println!("{table}");
        }
    }
    Ok(())
}
