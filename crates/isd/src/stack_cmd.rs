//! `isd stack ls` and `isd stack ps`: docker-parity stack subcommands.
//!
//! Phase 0.18 step 6. The pre-0.18 surface had no `isd stack` namespace:
//! stack enumeration was buried inside the joined `isd ps` view and the
//! verbs (`deploy`, `diff`, `edit`, `manifest`) lived at the top level
//! with no shared parent. This module gives stacks their own namespace.
//!
//! - `isd stack ls`: one row per stack, with services and hosts counts
//!   and an aggregate STATE column. Talks to `GET /api/v1/stacks` +
//!   `GET /api/v1/services` and joins client-side.
//! - `isd stack ps <name>`: services in the named stack. Mirrors
//!   `docker stack ps`.
//!
//! The deploy / diff / edit / manifest verbs also live under
//! `isd stack <verb>` (one-release deprecation window: top-level forms
//! keep working and print a hint).

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use comfy_table::{ContentArrangement, Table, presets::NOTHING};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::compose_cmd::{DeployArgs, DiffArgs, EditArgs};
use crate::manifest_cmd::ManifestCommand;
use crate::session::Session;

#[derive(Debug, Args)]
pub struct StackArgs {
    #[command(subcommand)]
    pub command: StackCommand,
}

#[derive(Debug, Subcommand)]
pub enum StackCommand {
    /// List stacks.
    Ls(LsArgs),
    /// List services in a stack.
    Ps(PsArgs),
    /// Deploy a stack from compose.yaml.
    Deploy(DeployArgs),
    /// Show the reconcile plan for a compose.yaml.
    Diff(DiffArgs),
    /// Open compose.yaml in $EDITOR and apply on save.
    Edit(EditArgs),
    /// View and edit a stack's stack.toml.
    #[command(subcommand)]
    Manifest(ManifestCommand),
}

#[derive(Debug, Args)]
pub struct LsArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = crate::output::Format::Table)]
    pub format: crate::output::Format,
}

#[derive(Debug, Args)]
pub struct PsArgs {
    /// Stack name.
    pub name: String,
    /// Output format.
    #[arg(long, value_enum, default_value_t = crate::output::Format::Table)]
    pub format: crate::output::Format,
}

/// Mirror of the dashboard's `StackDto`. Extra fields are ignored
/// (serde default tolerance); we keep only what we render.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StackApiRow {
    pub id: String,
    pub host_id: String,
    pub name: String,
    pub source: String,
    pub discovered_at: DateTime<Utc>,
}

/// Subset of `ServiceDto` used for stack aggregation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServiceApiRow {
    pub id: String,
    pub host_id: String,
    pub hostname: Option<String>,
    pub stack_id: Option<String>,
    pub name: String,
    pub image: String,
    pub state: String,
    pub last_seen_at: DateTime<Utc>,
}

/// One rendered row in `isd stack ls`.
#[derive(Debug, Clone, Serialize)]
pub struct StackLsRow {
    pub name: String,
    pub services: usize,
    pub hosts: usize,
    pub state: StackAggregateState,
    pub discovered_at: DateTime<Utc>,
    pub source: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StackAggregateState {
    /// Every service is running.
    Running,
    /// At least one service is running but not all are.
    Degraded,
    /// No services or every service is stopped/failed/dead/exited.
    Stopped,
    /// At least one service is in a mid-startup state (pulling,
    /// creating, starting, restarting) and none are stopped/failed.
    Pending,
}

impl StackAggregateState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Degraded => "degraded",
            Self::Stopped => "stopped",
            Self::Pending => "pending",
        }
    }
}

pub async fn run(args: StackArgs, context: Option<&str>) -> Result<()> {
    match args.command {
        StackCommand::Ls(a) => run_ls(a, context).await,
        StackCommand::Ps(a) => run_ps(a, context).await,
        StackCommand::Deploy(a) => crate::compose_cmd::run_deploy(a, context).await,
        StackCommand::Diff(a) => crate::compose_cmd::run_diff(a, context).await,
        StackCommand::Edit(a) => crate::compose_cmd::run_edit(a, context).await,
        StackCommand::Manifest(cmd) => {
            crate::manifest_cmd::run(crate::manifest_cmd::ManifestArgs { command: cmd }, context)
                .await
        }
    }
}

async fn run_ls(args: LsArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;

    let stacks: Vec<StackApiRow> = fetch_json(
        &session,
        &format!("{}/api/v1/stacks", session.controller_url()),
    )
    .await?;

    // Services for aggregation.
    let services: Vec<ServiceApiRow> = fetch_json(
        &session,
        &format!("{}/api/v1/services", session.controller_url()),
    )
    .await?;

    let rows = build_ls_rows(&stacks, &services);

    match args.format {
        crate::output::Format::Json => {
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        crate::output::Format::Table => {
            let out = render_ls_table(&rows);
            println!("{}", out.trim_end());
        }
    }
    Ok(())
}

async fn run_ps(args: PsArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;

    let stacks: Vec<StackApiRow> = fetch_json(
        &session,
        &format!("{}/api/v1/stacks", session.controller_url()),
    )
    .await?;

    let stack = stacks
        .iter()
        .find(|s| s.name == args.name)
        .ok_or_else(|| anyhow::anyhow!("stack {:?} not found in this context", args.name))?;

    // Stack IDs come back from the dashboard as stringified i64; the
    // services endpoint takes them back as `?stack_id=<i64>`.
    let stack_id: i64 = stack
        .id
        .parse()
        .with_context(|| format!("stack id {} is not an integer", stack.id))?;

    let services: Vec<ServiceApiRow> = fetch_json(
        &session,
        &format!(
            "{}/api/v1/services?stack_id={}",
            session.controller_url(),
            stack_id
        ),
    )
    .await?;

    match args.format {
        crate::output::Format::Json => {
            println!("{}", serde_json::to_string_pretty(&services)?);
        }
        crate::output::Format::Table => {
            let out = render_ps_table(&services);
            println!("{}", out.trim_end());
        }
    }
    Ok(())
}

async fn fetch_json<T: for<'a> Deserialize<'a>>(session: &Session, url: &str) -> Result<T> {
    let resp = session
        .client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("GET {url} returned HTTP {status}");
    }
    resp.json::<T>()
        .await
        .with_context(|| format!("decoding response from {url}"))
}

pub fn build_ls_rows(stacks: &[StackApiRow], services: &[ServiceApiRow]) -> Vec<StackLsRow> {
    let mut rows: Vec<StackLsRow> = stacks
        .iter()
        .map(|s| {
            let services_in_stack: Vec<&ServiceApiRow> = services
                .iter()
                .filter(|sv| sv.stack_id.as_deref() == Some(s.id.as_str()))
                .collect();
            let hosts: HashSet<&str> = services_in_stack
                .iter()
                .map(|sv| sv.host_id.as_str())
                .collect();
            StackLsRow {
                name: s.name.clone(),
                services: services_in_stack.len(),
                hosts: hosts.len(),
                state: aggregate_state(&services_in_stack),
                discovered_at: s.discovered_at,
                source: s.source.clone(),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn aggregate_state(services: &[&ServiceApiRow]) -> StackAggregateState {
    if services.is_empty() {
        return StackAggregateState::Stopped;
    }
    let mut running = 0;
    let mut stopped = 0;
    let mut pending = 0;
    for s in services {
        match s.state.as_str() {
            "running" => running += 1,
            "stopped" | "exited" | "dead" | "failed" => stopped += 1,
            "pulling" | "creating" | "starting" | "restarting" | "unknown" => pending += 1,
            _ => pending += 1,
        }
    }
    let total = services.len();
    if running == total {
        StackAggregateState::Running
    } else if stopped == total {
        StackAggregateState::Stopped
    } else if running > 0 && (stopped > 0 || pending == 0) {
        StackAggregateState::Degraded
    } else {
        StackAggregateState::Pending
    }
}

fn render_ls_table(rows: &[StackLsRow]) -> String {
    let mut t = Table::new();
    t.load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec![
            "NAME",
            "SERVICES",
            "HOSTS",
            "STATE",
            "SOURCE",
            "DISCOVERED",
        ]);
    for row in rows {
        t.add_row(vec![
            row.name.clone(),
            row.services.to_string(),
            row.hosts.to_string(),
            row.state.as_str().to_string(),
            row.source.clone(),
            row.discovered_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        ]);
    }
    t.to_string()
}

fn render_ps_table(services: &[ServiceApiRow]) -> String {
    let mut t = Table::new();
    t.load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec!["SERVICE", "HOST", "STATE", "IMAGE", "LAST SEEN"]);
    for s in services {
        t.add_row(vec![
            s.name.clone(),
            s.hostname.clone().unwrap_or_else(|| s.host_id.clone()),
            s.state.clone(),
            s.image.clone(),
            s.last_seen_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        ]);
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn stack(id: &str, name: &str) -> StackApiRow {
        StackApiRow {
            id: id.into(),
            host_id: "01H0".into(),
            name: name.into(),
            source: "compose".into(),
            discovered_at: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
        }
    }

    fn svc(name: &str, stack_id: &str, host: &str, state: &str) -> ServiceApiRow {
        ServiceApiRow {
            id: format!("svc-{name}"),
            host_id: host.into(),
            hostname: Some(host.into()),
            stack_id: Some(stack_id.into()),
            name: name.into(),
            image: "nginx:alpine".into(),
            state: state.into(),
            last_seen_at: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
        }
    }

    #[test]
    fn ls_rows_count_services_and_distinct_hosts() {
        let stacks = vec![stack("1", "hello"), stack("2", "lonely")];
        let services = vec![
            svc("web", "1", "h-a", "running"),
            svc("worker", "1", "h-b", "running"),
            svc("cache", "1", "h-a", "running"),
        ];
        let rows = build_ls_rows(&stacks, &services);
        assert_eq!(rows.len(), 2);
        let hello = rows.iter().find(|r| r.name == "hello").unwrap();
        assert_eq!(hello.services, 3);
        assert_eq!(hello.hosts, 2);
        let lonely = rows.iter().find(|r| r.name == "lonely").unwrap();
        assert_eq!(lonely.services, 0);
        assert_eq!(lonely.hosts, 0);
    }

    #[test]
    fn aggregate_state_classifies_running_degraded_stopped_pending() {
        let s_run = svc("a", "1", "h", "running");
        let s_stop = svc("b", "1", "h", "stopped");
        let s_pend = svc("c", "1", "h", "pulling");

        assert!(matches!(
            aggregate_state(&[&s_run, &s_run]),
            StackAggregateState::Running
        ));
        assert!(matches!(
            aggregate_state(&[&s_stop, &s_stop]),
            StackAggregateState::Stopped
        ));
        assert!(matches!(
            aggregate_state(&[&s_run, &s_stop]),
            StackAggregateState::Degraded
        ));
        assert!(matches!(
            aggregate_state(&[&s_pend, &s_pend]),
            StackAggregateState::Pending
        ));
        assert!(matches!(aggregate_state(&[]), StackAggregateState::Stopped));
    }

    #[test]
    fn ls_table_contains_header_and_each_row() {
        let stacks = vec![stack("1", "alpha"), stack("2", "beta")];
        let services = vec![
            svc("web", "1", "h-a", "running"),
            svc("web", "2", "h-b", "stopped"),
        ];
        let rows = build_ls_rows(&stacks, &services);
        let t = render_ls_table(&rows);
        assert!(t.contains("NAME"));
        assert!(t.contains("SERVICES"));
        assert!(t.contains("HOSTS"));
        assert!(t.contains("STATE"));
        assert!(t.contains("alpha"));
        assert!(t.contains("beta"));
        assert!(t.contains("running"));
        assert!(t.contains("stopped"));
    }

    #[test]
    fn ps_table_renders_service_rows() {
        let services = vec![
            svc("web", "1", "iso-1", "running"),
            svc("worker", "1", "iso-2", "running"),
        ];
        let t = render_ps_table(&services);
        assert!(t.contains("SERVICE"));
        assert!(t.contains("HOST"));
        assert!(t.contains("web"));
        assert!(t.contains("worker"));
        assert!(t.contains("iso-1"));
        assert!(t.contains("iso-2"));
    }

    #[test]
    fn ls_rows_sorted_alphabetically_by_name() {
        let stacks = vec![
            stack("1", "zebra"),
            stack("2", "alpha"),
            stack("3", "mango"),
        ];
        let services = vec![];
        let rows = build_ls_rows(&stacks, &services);
        assert_eq!(rows[0].name, "alpha");
        assert_eq!(rows[1].name, "mango");
        assert_eq!(rows[2].name, "zebra");
    }
}
