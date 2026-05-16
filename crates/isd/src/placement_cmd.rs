//! `isd placement show`: print the controller's placement grid.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use comfy_table::{ContentArrangement, Table, presets::NOTHING};
use serde::{Deserialize, Serialize};

use crate::session::Session;

#[derive(Debug, Args)]
pub struct PlacementArgs {
    #[command(subcommand)]
    pub command: PlacementCommand,
}

#[derive(Debug, Subcommand)]
pub enum PlacementCommand {
    /// Print the placement grid (one row per replica).
    Show(ShowArgs),
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Filter by stack id.
    #[arg(long)]
    pub stack_id: Option<i64>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = crate::output::Format::Table)]
    pub format: crate::output::Format,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlacementRow {
    pub service_id: i64,
    pub service_name: Option<String>,
    pub stack_id: Option<i64>,
    pub host_id: String,
    pub hostname: Option<String>,
    pub replica_index: u32,
    pub state: String,
    pub assigned_at: DateTime<Utc>,
    pub last_event: Option<String>,
}

pub async fn run(args: PlacementArgs, context: Option<&str>) -> Result<()> {
    match args.command {
        PlacementCommand::Show(a) => run_show(a, context).await,
    }
}

async fn run_show(args: ShowArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let url = match args.stack_id {
        Some(id) => format!(
            "{}/api/v1/placements/by-stack/{id}",
            session.controller_url()
        ),
        None => format!("{}/api/v1/placements", session.controller_url()),
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
