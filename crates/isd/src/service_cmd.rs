//! `isd service ls`: list every service across every stack.
//! Talks to `GET /api/v1/services`.

use crate::render::{Align, CellStyle, Column, Table, render, render_plain};
use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::session::Session;
use crate::stack_cmd::{ServiceApiRow, StackApiRow};

/// CLI flags for `isd service`.
#[derive(Debug, Args)]
pub struct ServiceArgs {
    /// Resolved sub-verb.
    #[command(subcommand)]
    pub command: ServiceCommand,
}

/// Sub-verbs under `isd service`.
#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// List services across stacks.
    Ls(LsArgs),
}

/// CLI flags for `isd service ls`.
#[derive(Debug, Args)]
pub struct LsArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = crate::output::Format::Table)]
    pub format: crate::output::Format,
}

/// One row in the rendered `service ls` table.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceLsRow {
    /// Owning stack name; `None` for unstacked services.
    pub stack: Option<String>,
    /// Service name.
    pub name: String,
    /// Host the service runs on (hostname when known, ULID fallback).
    pub host: String,
    /// Operational state.
    pub state: String,
    /// Image reference.
    pub image: String,
    /// Last heartbeat timestamp.
    pub last_seen_at: DateTime<Utc>,
}

/// Dispatch to the matching `service` sub-verb.
///
/// # Errors
///
/// Propagates the sub-verb's error.
pub async fn run(args: ServiceArgs, context: Option<&str>) -> Result<()> {
    match args.command {
        ServiceCommand::Ls(a) => run_ls(a, context).await,
    }
}

/// Fetch stacks + services and render one joined row per service.
async fn run_ls(args: LsArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;

    let stacks: Vec<StackApiRow> =
        fetch(&session, &format!("{controller_url}/api/v1/stacks")).await?;
    let services: Vec<ServiceApiRow> =
        fetch(&session, &format!("{controller_url}/api/v1/services")).await?;

    let rows = build_rows(&stacks, &services);

    match args.format {
        crate::output::Format::Json => {
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        crate::output::Format::Table => print_table(&rows),
    }
    Ok(())
}

/// GET `url` and deserialize the JSON body as `T`. Used by `run_ls` to
/// pull the two parallel endpoints with one error-surface shape.
async fn fetch<T: for<'a> Deserialize<'a>>(session: &Session, url: &str) -> Result<T> {
    use anyhow::Context as _;
    let resp = session
        .client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET {url} returned HTTP {status}");
    }
    resp.json::<T>()
        .await
        .with_context(|| format!("decoding response from {url}"))
}

/// Join services to stacks by `stack_id` and sort by (stack, name).
/// Unstacked services land with `stack = None`.
pub fn build_rows(stacks: &[StackApiRow], services: &[ServiceApiRow]) -> Vec<ServiceLsRow> {
    let mut rows: Vec<ServiceLsRow> = services
        .iter()
        .map(|s| ServiceLsRow {
            stack: s
                .stack_id
                .as_ref()
                .and_then(|sid| stacks.iter().find(|st| &st.id == sid))
                .map(|st| st.name.clone()),
            name: s.name.clone(),
            host: s.hostname.clone().unwrap_or_else(|| s.host_id.clone()),
            state: s.state.clone(),
            image: s.image.clone(),
            last_seen_at: s.last_seen_at,
        })
        .collect();
    rows.sort_by(|a, b| {
        a.stack
            .as_deref()
            .unwrap_or("")
            .cmp(b.stack.as_deref().unwrap_or(""))
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

/// Build the boxed-renderer table for `service ls`.
fn build_table(rows: &[ServiceLsRow]) -> Table {
    Table {
        columns: vec![
            Column::new("#", Align::Right, CellStyle::Dim, 9, 1),
            Column::new("STACK", Align::Left, CellStyle::Dim, 5, 6),
            Column::new("SERVICE", Align::Left, CellStyle::Emphasis, 1, 8),
            Column::new("HOST", Align::Left, CellStyle::Cyan, 4, 6),
            Column::new("STATE", Align::Left, CellStyle::State, 6, 5),
            Column::new("IMAGE", Align::Left, CellStyle::Plain, 2, 12),
            Column::new("LAST SEEN", Align::Left, CellStyle::Dim, 3, 20),
        ],
        rows: rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                vec![
                    i.to_string(),
                    row.stack.clone().unwrap_or_else(|| "-".into()),
                    row.name.clone(),
                    row.host.clone(),
                    row.state.clone(),
                    row.image.clone(),
                    row.last_seen_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                ]
            })
            .collect(),
    }
}

/// Print the table: boxed rounded-corners on a TTY, tab-separated plain
/// when stdout is redirected so scripts stay clean.
fn print_table(rows: &[ServiceLsRow]) {
    let table = build_table(rows);
    let term = console::Term::stdout();
    if term.is_term() {
        let width = term.size().1 as usize;
        println!("{}", render(&table, width, console::colors_enabled()));
    } else {
        println!("{}", render_plain(&table));
    }
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

    fn svc(name: &str, stack_id: Option<&str>, host: &str, state: &str) -> ServiceApiRow {
        ServiceApiRow {
            id: format!("svc-{name}"),
            host_id: host.into(),
            hostname: Some(host.into()),
            stack_id: stack_id.map(|s| s.into()),
            name: name.into(),
            image: "nginx:alpine".into(),
            state: state.into(),
            last_seen_at: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
        }
    }

    #[test]
    fn rows_join_stack_name_via_stack_id() {
        let stacks = vec![stack("1", "hello")];
        let services = vec![svc("web", Some("1"), "iso-1", "running")];
        let rows = build_rows(&stacks, &services);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stack.as_deref(), Some("hello"));
        assert_eq!(rows[0].name, "web");
    }

    #[test]
    fn rows_handle_unstacked_services() {
        let stacks = vec![];
        let services = vec![svc("orphan", None, "iso-1", "running")];
        let rows = build_rows(&stacks, &services);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].stack.is_none());
    }

    #[test]
    fn rows_sorted_by_stack_then_name() {
        let stacks = vec![stack("1", "zebra"), stack("2", "alpha")];
        let services = vec![
            svc("b", Some("1"), "h", "running"),
            svc("a", Some("1"), "h", "running"),
            svc("c", Some("2"), "h", "running"),
        ];
        let rows = build_rows(&stacks, &services);
        // alpha sorts first, then zebra; within zebra, a before b.
        assert_eq!(rows[0].stack.as_deref(), Some("alpha"));
        assert_eq!(rows[0].name, "c");
        assert_eq!(rows[1].stack.as_deref(), Some("zebra"));
        assert_eq!(rows[1].name, "a");
        assert_eq!(rows[2].name, "b");
    }

    #[test]
    fn table_carries_dash_for_unstacked_rows() {
        let stacks = vec![];
        let services = vec![svc("orphan", None, "iso-1", "running")];
        let rows = build_rows(&stacks, &services);
        let t = render_plain(&build_table(&rows));
        assert!(t.contains("STACK"));
        assert!(t.contains("SERVICE"));
        assert!(t.contains("orphan"));
        // The dash placeholder appears in the STACK cell.
        assert!(t.lines().any(|l| l.contains("orphan") && l.contains("-")));
    }
}
