//! `isd stack ls` and `isd stack ps`: docker-parity stack subcommands.
//!
//! Step 6. The pre-0.18 surface had no `isd stack` namespace:
//! stack enumeration was buried inside the joined `isd ps` view and the
//! verbs (`deploy`, `diff`, `edit`, `manifest`) lived at the top level
//! with no shared parent. This module gives stacks their own namespace.
//!
//! - `isd stack ls`: one row per stack, with services and hosts counts
//!   and an aggregate STATE column. Talks to `GET /api/v1/stacks` +
//!   `GET /api/v1/services` and joins client-side.
//! - `isd stack ps <name>`: services in the named stack. Mirrors
//!   `docker stack ps`.
//! - `isd stack deploy` (alias `up`): bring a stack up from compose.yaml.
//! - `isd stack diff`: print the reconcile plan without applying.
//! - `isd stack edit`: open compose.yaml in $EDITOR, apply on save.
//! - `isd stack manifest`: view / edit a stack's stack.toml.

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::compose_cmd::{DeployArgs, DiffArgs, EditArgs};
use crate::manifest_cmd::ManifestCommand;
use crate::render::{Align, CellStyle, Column, StatusColor, Table, render, render_plain};
use crate::session::Session;

/// CLI flags for `isd stack`.
#[derive(Debug, Args)]
pub struct StackArgs {
    /// Resolved sub-verb.
    #[command(subcommand)]
    pub command: StackCommand,
}

/// Sub-verbs under `isd stack`.
#[derive(Debug, Subcommand)]
pub enum StackCommand {
    /// List stacks.
    Ls(LsArgs),
    /// List services in a stack.
    Ps(PsArgs),
    /// Deploy a stack from compose.yaml. `up` is a hidden alias.
    #[command(alias = "up")]
    Deploy(DeployArgs),
    /// Show the reconcile plan for a compose.yaml.
    Diff(DiffArgs),
    /// Open compose.yaml in $EDITOR and apply on save.
    Edit(EditArgs),
    /// View and edit a stack's stack.toml.
    #[command(subcommand)]
    Manifest(ManifestCommand),
}

/// CLI flags for `isd stack ls`.
#[derive(Debug, Args)]
pub struct LsArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = crate::output::Format::Table)]
    pub format: crate::output::Format,
}

/// CLI flags for `isd stack ps <name>`.
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
    /// Stringified surrogate key.
    pub id: String,
    /// Owning host ULID.
    pub host_id: String,
    /// Operator-facing stack name.
    pub name: String,
    /// Origin tag (`compose`, `imported`, ...).
    pub source: String,
    /// When the controller first observed this stack.
    pub discovered_at: DateTime<Utc>,
}

/// Subset of `ServiceDto` used for stack aggregation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServiceApiRow {
    /// Service surrogate key.
    pub id: String,
    /// Owning host ULID.
    pub host_id: String,
    /// Reported hostname; falls back to host_id in render.
    pub hostname: Option<String>,
    /// Owning stack id; `None` for unstacked services.
    pub stack_id: Option<String>,
    /// Service name.
    pub name: String,
    /// Image reference.
    pub image: String,
    /// Operational state.
    pub state: String,
    /// Last heartbeat timestamp.
    pub last_seen_at: DateTime<Utc>,
}

/// One rendered row in `isd stack ls`.
#[derive(Debug, Clone, Serialize)]
pub struct StackLsRow {
    /// Stack name.
    pub name: String,
    /// Number of services in the stack.
    pub services: usize,
    /// Number of distinct hosts the stack runs on.
    pub hosts: usize,
    /// Aggregate state computed by [`aggregate_state`].
    pub state: StackAggregateState,
    /// When the controller first observed the stack.
    pub discovered_at: DateTime<Utc>,
    /// Origin tag (`compose`, `imported`, ...).
    pub source: String,
}

/// Coarse stack-level aggregate of every service's state. Drives the
/// STATE column on `isd stack ls`.
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
    /// Lowercase string representation used by the table renderer
    /// and the JSON output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Degraded => "degraded",
            Self::Stopped => "stopped",
            Self::Pending => "pending",
        }
    }
}

/// Dispatch to the matching `stack` sub-verb.
///
/// # Errors
///
/// Propagates the sub-verb's error.
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

/// `isd stack ls`: list every stack with aggregated service / host
/// counts and a coarse STATE column.
async fn run_ls(args: LsArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;

    let stacks: Vec<StackApiRow> =
        fetch_json(&session, &format!("{controller_url}/api/v1/stacks")).await?;

    // Services for aggregation.
    let services: Vec<ServiceApiRow> =
        fetch_json(&session, &format!("{controller_url}/api/v1/services")).await?;

    let rows = build_ls_rows(&stacks, &services);

    match args.format {
        crate::output::Format::Json => {
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        crate::output::Format::Table => {
            print_ls_table(&rows);
        }
    }
    Ok(())
}

/// `isd stack ps <name>`: list every service inside the named stack.
async fn run_ps(args: PsArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;

    let stacks: Vec<StackApiRow> =
        fetch_json(&session, &format!("{controller_url}/api/v1/stacks")).await?;

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
        &format!("{controller_url}/api/v1/services?stack_id={stack_id}"),
    )
    .await?;

    match args.format {
        crate::output::Format::Json => {
            println!("{}", serde_json::to_string_pretty(&services)?);
        }
        crate::output::Format::Table => {
            print_ps_table(&services);
        }
    }
    Ok(())
}

/// GET `url` and decode the JSON body as `T`. Bail on any non-2xx.
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

/// Join services to stacks, compute aggregate state, return one row
/// per stack sorted alphabetically by name.
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

/// Compute the coarse stack state from per-service states. Empty
/// inputs collapse to `Stopped`. Classification rules:
///
/// - every service `running` -> `Running`
/// - every service `stopped`/`exited`/`dead`/`failed` -> `Stopped`
/// - some running, some stopped, no pending -> `Degraded`
/// - mid-transition state present -> `Pending`
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

/// Column layout for `isd stack ls`. Columns in spec order:
/// `NAME`, `SERVICES`, `HOSTS`, `STATE`, `SOURCE`, `DISCOVERED`.
fn ls_columns() -> Vec<Column> {
    vec![
        Column::new("NAME", Align::Left, CellStyle::Emphasis, 8, 6),
        Column::new("SERVICES", Align::Right, CellStyle::Plain, 5, 4),
        Column::new("HOSTS", Align::Right, CellStyle::Plain, 5, 4),
        Column::new("STATE", Align::Left, CellStyle::Plain, 7, 8),
        Column::new("SOURCE", Align::Left, CellStyle::Plain, 3, 6),
        Column::new("DISCOVERED", Align::Left, CellStyle::Plain, 1, 20),
    ]
}

/// Classify a stack-aggregate or service-state string into a STATE
/// color bucket. Independent of `render::classify_status`, which keys
/// off docker's "Up ..." strings.
///
///   - `running`                        -> green
///   - `pending`, `restarting`,
///     `pulling`, `creating`, `starting` -> yellow
///   - `failed`, `degraded`, `dead`,
///     `unhealthy`                       -> red
///   - everything else (`stopped`,
///     `exited`, `paused`, `unknown`, "") -> grey
fn classify_stack_state(s: &str) -> StatusColor {
    match s {
        "running" => StatusColor::Green,
        "pending" | "restarting" | "pulling" | "creating" | "starting" => StatusColor::Yellow,
        "failed" | "degraded" | "dead" | "unhealthy" => StatusColor::Red,
        _ => StatusColor::Grey,
    }
}

/// Wrap `text` in ANSI styling matching its [`StatusColor`] bucket
/// when `color` is true. Otherwise pass through. Used to pre-color
/// `STATE` cells before they reach the renderer: the column itself
/// is declared [`CellStyle::Plain`] so the renderer does not run a
/// (docker-shape) classifier over the plain stack-state vocabulary.
fn color_state_text(text: &str, color: bool) -> String {
    if !color {
        return text.to_string();
    }
    let mk = || console::Style::new().force_styling(true);
    let styled = match classify_stack_state(text) {
        StatusColor::Green => mk().green(),
        StatusColor::Yellow => mk().yellow(),
        StatusColor::Red => mk().red(),
        StatusColor::Grey => mk().dim(),
    };
    styled.apply_to(text).to_string()
}

/// Build the row matrix for `isd stack ls`. STATE cells are colored
/// per [`classify_stack_state`] when `color` is true.
pub fn build_ls_row_cells(rows: &[StackLsRow], color: bool) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            vec![
                row.name.clone(),
                row.services.to_string(),
                row.hosts.to_string(),
                color_state_text(row.state.as_str(), color),
                row.source.clone(),
                row.discovered_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            ]
        })
        .collect()
}

/// Render `stack ls` to stdout: boxed table on a TTY (with colored
/// STATE), tab-separated plain on a pipe.
fn print_ls_table(rows: &[StackLsRow]) {
    let term = console::Term::stdout();
    if term.is_term() {
        let width = term.size().1 as usize;
        let color = console::colors_enabled();
        let table = Table {
            columns: ls_columns(),
            rows: build_ls_row_cells(rows, color),
        };
        println!("{}", render(&table, width, color));
    } else {
        let table = Table {
            columns: ls_columns(),
            rows: build_ls_row_cells(rows, false),
        };
        println!("{}", render_plain(&table));
    }
}

/// Column layout for `isd stack ps <name>`. Columns in spec order:
/// `SERVICE`, `HOST`, `STATE`, `IMAGE`, `LAST SEEN`.
fn ps_columns() -> Vec<Column> {
    vec![
        Column::new("SERVICE", Align::Left, CellStyle::Emphasis, 8, 6),
        Column::new("HOST", Align::Left, CellStyle::Plain, 5, 6),
        Column::new("STATE", Align::Left, CellStyle::Plain, 7, 8),
        Column::new("IMAGE", Align::Left, CellStyle::Plain, 1, 13),
        Column::new("LAST SEEN", Align::Left, CellStyle::Plain, 2, 20),
    ]
}

/// Build the row matrix for `isd stack ps`. STATE cells are colored
/// per [`classify_stack_state`] when `color` is true.
pub fn build_ps_row_cells(services: &[ServiceApiRow], color: bool) -> Vec<Vec<String>> {
    services
        .iter()
        .map(|s| {
            vec![
                s.name.clone(),
                s.hostname.clone().unwrap_or_else(|| s.host_id.clone()),
                color_state_text(&s.state, color),
                s.image.clone(),
                s.last_seen_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            ]
        })
        .collect()
}

/// Render `stack ps` to stdout: boxed table on a TTY (with colored
/// STATE), tab-separated plain on a pipe.
fn print_ps_table(services: &[ServiceApiRow]) {
    let term = console::Term::stdout();
    if term.is_term() {
        let width = term.size().1 as usize;
        let color = console::colors_enabled();
        let table = Table {
            columns: ps_columns(),
            rows: build_ps_row_cells(services, color),
        };
        println!("{}", render(&table, width, color));
    } else {
        let table = Table {
            columns: ps_columns(),
            rows: build_ps_row_cells(services, false),
        };
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

    /// `stack ls` rows render through the unified boxed renderer:
    /// the plain (non-TTY) path emits ALL CAPS headers in spec order
    /// plus every row's text.
    #[test]
    fn ls_table_contains_header_and_each_row() {
        let stacks = vec![stack("1", "alpha"), stack("2", "beta")];
        let services = vec![
            svc("web", "1", "h-a", "running"),
            svc("web", "2", "h-b", "stopped"),
        ];
        let rows = build_ls_rows(&stacks, &services);
        let table = Table {
            columns: ls_columns(),
            rows: build_ls_row_cells(&rows, false),
        };
        let t = render_plain(&table);
        // Headers in spec order on the first line.
        let header = t.lines().next().unwrap();
        assert_eq!(header, "NAME\tSERVICES\tHOSTS\tSTATE\tSOURCE\tDISCOVERED");
        // Row values present.
        assert!(t.contains("alpha"));
        assert!(t.contains("beta"));
        assert!(t.contains("running"));
        assert!(t.contains("stopped"));
        // Boxed-TTY render emits the rounded-corner glyphs.
        let boxed = render(&table, 200, false);
        assert!(boxed.contains('╭'));
        assert!(boxed.contains('╰'));
    }

    /// Empty input still renders the header row so pipeline consumers
    /// (`wc -l`, `cut -f`) keep a stable shape.
    #[test]
    fn ls_table_renders_header_on_empty_input() {
        let table = Table {
            columns: ls_columns(),
            rows: build_ls_row_cells(&[], false),
        };
        let t = render_plain(&table);
        assert_eq!(
            t.lines().next().unwrap(),
            "NAME\tSERVICES\tHOSTS\tSTATE\tSOURCE\tDISCOVERED"
        );
    }

    /// `stack ps` rows render through the unified boxed renderer:
    /// headers in spec order, service / host / state values present.
    #[test]
    fn ps_table_renders_service_rows() {
        let services = vec![
            svc("web", "1", "iso-1", "running"),
            svc("worker", "1", "iso-2", "running"),
        ];
        let table = Table {
            columns: ps_columns(),
            rows: build_ps_row_cells(&services, false),
        };
        let t = render_plain(&table);
        let header = t.lines().next().unwrap();
        assert_eq!(header, "SERVICE\tHOST\tSTATE\tIMAGE\tLAST SEEN");
        assert!(t.contains("web"));
        assert!(t.contains("worker"));
        assert!(t.contains("iso-1"));
        assert!(t.contains("iso-2"));
        // Boxed render: rounded corners present.
        let boxed = render(&table, 200, false);
        assert!(boxed.contains('╭'));
    }

    /// Empty service list still renders the header row.
    #[test]
    fn ps_table_renders_header_on_empty_input() {
        let table = Table {
            columns: ps_columns(),
            rows: build_ps_row_cells(&[], false),
        };
        let t = render_plain(&table);
        assert_eq!(
            t.lines().next().unwrap(),
            "SERVICE\tHOST\tSTATE\tIMAGE\tLAST SEEN"
        );
    }

    /// STATE coloring maps the stack vocabulary to the renderer's
    /// [`StatusColor`] buckets: green for running, yellow for mid-
    /// transition states, red for failure shapes, grey for everything
    /// else.
    #[test]
    fn classify_stack_state_buckets() {
        assert_eq!(classify_stack_state("running"), StatusColor::Green);
        assert_eq!(classify_stack_state("pending"), StatusColor::Yellow);
        assert_eq!(classify_stack_state("restarting"), StatusColor::Yellow);
        assert_eq!(classify_stack_state("failed"), StatusColor::Red);
        assert_eq!(classify_stack_state("degraded"), StatusColor::Red);
        assert_eq!(classify_stack_state("stopped"), StatusColor::Grey);
        assert_eq!(classify_stack_state("unknown"), StatusColor::Grey);
        assert_eq!(classify_stack_state(""), StatusColor::Grey);
    }

    /// STATE pre-coloring is a no-op when `color = false`, so the
    /// non-TTY decay path emits clean tab-separated text.
    #[test]
    fn color_state_text_passthrough_when_color_disabled() {
        assert_eq!(color_state_text("running", false), "running");
        assert_eq!(color_state_text("failed", false), "failed");
    }

    /// STATE pre-coloring emits ANSI escape bytes when `color = true`.
    #[test]
    fn color_state_text_emits_ansi_when_color_enabled() {
        let s = color_state_text("running", true);
        assert!(s.contains('\u{1b}'), "expected ANSI: {s:?}");
        // The visible width is unchanged: ANSI is zero-width.
        assert_eq!(console::measure_text_width(&s), "running".len());
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
