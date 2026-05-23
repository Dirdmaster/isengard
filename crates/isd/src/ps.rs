//! `isd ps`: docker-context view of the operator's containers.
//!
//! Resolves the active docker context via `crate::docker_context`, opens a
//! `DockerBackend` against the resolved URI, and renders the container
//! list (`#`, CONTAINER ID, IMAGE, STATUS, PORTS, NAMES) in the same
//! grouped/flat table the operator sees from `docker ps`. `--no-trunc`
//! widens CONTAINER ID; `--format json` dumps the raw container list.

use anyhow::{Context as _, Result};
use clap::Args;
use isd_runtime::discovery_labels::{ROLE_LABEL, is_protected_label_value};

use crate::index_cache::{IndexCache, IndexRow};
use crate::render::{Align, CellStyle, Column, Table, render, render_plain};

/// Width of the truncated CONTAINER ID column. Docker uses 12 chars
/// for the short id; `--no-trunc` widens to the full hex.
const ID_DISPLAY_WIDTH: usize = 12;

/// CLI flags for `isd ps`.
#[derive(Debug, Args, Default)]
pub struct PsArgs {
    /// Show every container (incl. stopped + healthy infrastructure).
    ///
    /// By default `isd ps` hides healthy Isengard infrastructure
    /// (controller, agent) to keep the table focused on the operator's
    /// own workloads. Errored or stopped infrastructure still surfaces
    /// because that is the case the operator needs to see.
    #[arg(short = 'a', long)]
    pub all: bool,

    /// Disable ID and command truncation.
    #[arg(long)]
    pub no_trunc: bool,

    /// Filter by key=value (repeatable). Known keys: host, stack, service, state.
    #[arg(long = "filter", value_name = "KEY=VALUE")]
    pub filters: Vec<String>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = crate::output::Format::Table)]
    pub format: crate::output::Format,

    /// Force flat output (no per-host grouping).
    ///
    /// Default groups when the controller has more than one enrolled host.
    #[arg(long)]
    pub no_group: bool,

    /// Filter to a single host by name.
    ///
    /// Implies flat output (grouping only makes sense across multiple hosts).
    #[arg(long, value_name = "NAME")]
    pub host: Option<String>,
}

/// List containers on the resolved docker context. Renders a boxed
/// table on a TTY, tab-separated plain text when piped, or pretty
/// JSON with `--format json`.
///
/// # Errors
///
/// Returns `Err` on docker connection or list failures.
pub async fn run(args: PsArgs, context: Option<&str>) -> Result<()> {
    // Every context is a docker context with a docker URI. The
    // controller-direct REST path is gone for `ps`: we always go through
    // the DockerBackend.
    let docker_uri = crate::docker_context::resolve_docker_uri(context)?;
    run_docker_backend(args, docker_uri, context).await
}

/// Returns `true` when a docker status string reads as a healthy
/// running container.
///
/// Docker's status field is a human string. The shapes we treat as
/// healthy:
///
/// - `Up <duration>` (running, no healthcheck)
/// - `Up <duration> (healthy)` (running, passing healthcheck)
///
/// Anything else (`Exited`, `Restarting`, `Dead`, `Created`, `Paused`,
/// or `Up <duration> (unhealthy)` / `(starting)`) is treated as not
/// healthy so the operator still sees infrastructure that needs
/// attention.
fn is_healthy_status(status: &str) -> bool {
    let trimmed = status.trim();
    if !trimmed.starts_with("Up ") && trimmed != "Up" {
        return false;
    }
    // `Up <duration> (<healthcheck-state>)`: only `(healthy)` counts as
    // healthy. `(unhealthy)`, `(starting)`, etc. are not.
    if let Some(paren) = trimmed.rfind('(') {
        let tail = &trimmed[paren..];
        return tail == "(healthy)";
    }
    true
}

/// Container names that identify Isengard infrastructure in the wild.
///
/// The current agent compose recipe does NOT stamp
/// `io.isengard.role=agent` on the runtime container (it inherits only
/// the `com.docker.compose.*` labels), so a pure-label check misses the
/// very container the operator wants hidden. Until the recipe is
/// updated to apply the role label, fall back to the canonical
/// container names. `iso-*` covers pre-rename fleets; `isd-*` covers
/// post-rename fleets (#218); both can coexist during a partial
/// migration.
const PROTECTED_NAMES: &[&str] = &["isd-controller", "isd-agent", "iso-controller", "iso-agent"];

/// Compose service values that identify Isengard infrastructure when
/// they sit inside the `isengard` compose project. The agent compose
/// recipe stamps `com.docker.compose.project=isengard` +
/// `com.docker.compose.service=agent` (or `controller`) on the
/// container; this is a more durable signal than the runtime name when
/// the operator renamed the container.
/// Label key carrying the compose project name. Set by docker compose
/// on every container it manages.
const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
/// Label key carrying the compose service name (the key under
/// `services:` in the compose file).
const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";
/// Compose project value that identifies the Isengard control-plane
/// stack (controller + agent share this project).
const COMPOSE_PROJECT_VALUE: &str = "isengard";
/// Compose service values that identify Isengard infrastructure inside
/// the `isengard` project. Matched against
/// `com.docker.compose.service` after the project name matches
/// [`COMPOSE_PROJECT_VALUE`].
const COMPOSE_SERVICE_VALUES_PROTECTED: &[&str] = &["controller", "agent"];

/// Returns `true` when the container is part of the protected Isengard
/// infrastructure set (controller / agent).
///
/// Detection is layered so the filter catches infrastructure across
/// every shape it appears in the wild:
///
/// 1. The canonical `io.isengard.role=controller|agent` label. Future
///    infrastructure roles added to
///    [`isd_runtime::discovery_labels::ROLE_VALUES_PROTECTED`]
///    auto-qualify here without touching this function.
/// 2. The `com.docker.compose.project=isengard` +
///    `com.docker.compose.service=controller|agent` label pair the
///    current compose recipe stamps. Covers fleets whose agent
///    container predates the role-label stamping.
/// 3. The canonical container names (`isd-controller`, `isd-agent`,
///    `iso-controller`, `iso-agent`). Covers pre-rename + post-rename
///    fleets during a partial migration, and any case where the
///    operator renamed the compose service but kept the container
///    name.
fn is_protected(c: &isd_runtime::ContainerSummary) -> bool {
    if let Some(role) = c.labels.get(ROLE_LABEL) {
        if is_protected_label_value(role) {
            return true;
        }
    }
    let project = c.labels.get(COMPOSE_PROJECT_LABEL).map(String::as_str);
    let service = c.labels.get(COMPOSE_SERVICE_LABEL).map(String::as_str);
    if project == Some(COMPOSE_PROJECT_VALUE)
        && service
            .map(|s| COMPOSE_SERVICE_VALUES_PROTECTED.contains(&s))
            .unwrap_or(false)
    {
        return true;
    }
    PROTECTED_NAMES.contains(&c.names.as_str())
}

/// Visibility filter for the docker-direct path.
///
/// Default (`all = false`): hide healthy Isengard infrastructure
/// (controller / agent). An errored or stopped infrastructure container
/// still surfaces because that is exactly the case the operator wants
/// to see.
///
/// With `all = true`: return the input unchanged so `-a` / `--all`
/// shows everything (every workload plus every infrastructure row,
/// healthy or not).
pub(crate) fn filter_visible(
    rows: Vec<isd_runtime::ContainerSummary>,
    all: bool,
) -> Vec<isd_runtime::ContainerSummary> {
    if all {
        return rows;
    }
    rows.into_iter()
        .filter(|c| !(is_protected(c) && is_healthy_status(&c.status)))
        .collect()
}

/// Open a DockerBackend for the resolved context. Used by the
/// lifecycle commands so they share `ps`'s context-resolution + docker
/// connection path. Resolution goes through
/// `crate::docker_context::resolve_docker_uri`, which reads
/// `~/.docker/contexts/` directly.
pub(crate) async fn open_docker_backend(
    context: Option<&str>,
) -> Result<isd_runtime::DockerBackend> {
    let uri = crate::docker_context::resolve_docker_uri(context)?;
    isd_runtime::DockerBackend::from_uri(&uri)
        .await
        .with_context(|| format!("opening docker backend at {uri}"))
}

/// Column layout for the docker-backend `isd ps` table. Order matches
/// the spec mockup: #, CONTAINER ID, IMAGE, STATUS, PORTS, NAMES.
fn docker_ps_columns() -> Vec<Column> {
    // shrink_priority: lower shrinks first when the table is wider than
    // the terminal. IMAGE is the noisiest column (registry path) and the
    // safest to truncate; PORTS is operationally critical so it gets a
    // higher priority + a generous min_width that fits a typical single
    // mapping like `8080->80`.
    vec![
        Column::new("#", Align::Right, CellStyle::Dim, 9, 1),
        Column::new("CONTAINER ID", Align::Left, CellStyle::Dim, 7, 12),
        Column::new("IMAGE", Align::Left, CellStyle::Plain, 1, 13),
        Column::new("STATUS", Align::Left, CellStyle::State, 8, 10),
        Column::new("PORTS", Align::Left, CellStyle::Plain, 4, 14),
        Column::new("NAMES", Align::Left, CellStyle::Emphasis, 6, 8),
    ]
}

/// Build the display rows: one `Vec<String>` per container, columns in
/// `docker_ps_columns` order. CONTAINER ID is truncated to the display
/// width; the index is the render-order position.
fn build_docker_rows(containers: &[isd_runtime::ContainerSummary]) -> Vec<Vec<String>> {
    containers
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let short_id: String = c.id.chars().take(ID_DISPLAY_WIDTH).collect();
            vec![
                i.to_string(),
                short_id,
                c.image.clone(),
                c.status.clone(),
                c.ports.clone(),
                c.names.clone(),
            ]
        })
        .collect()
}

/// Build the index-cache rows: keep the FULL container ID (never the
/// truncated display form) and stamp each with the context name.
fn build_index_rows(
    containers: &[isd_runtime::ContainerSummary],
    context_name: &str,
) -> Vec<IndexRow> {
    containers
        .iter()
        .enumerate()
        .map(|(i, c)| IndexRow {
            index: i,
            context: context_name.to_string(),
            container_id: c.id.clone(),
            name: c.names.clone(),
        })
        .collect()
}

/// Inner body of [`run`] once the docker URI is resolved.
async fn run_docker_backend(args: PsArgs, docker_uri: String, context: Option<&str>) -> Result<()> {
    use isd_runtime::DockerBackend;

    let context_name = crate::docker_context::resolve_context_name(context)?;
    let backend = DockerBackend::from_uri(&docker_uri)
        .await
        .with_context(|| format!("opening docker backend at {docker_uri}"))?;

    // Always query docker with `all = true`. We need every container in
    // the response so a stopped or errored protected container (the
    // exact case the operator wants to see) surfaces even when the
    // default `isd ps` is hiding healthy infrastructure. `filter_visible`
    // below applies the operator-facing default of hiding healthy
    // infrastructure when `--all` is not set.
    let containers = backend
        .list_containers(true)
        .await
        .context("listing containers")?;

    // Hide healthy Isengard infrastructure (io.isengard.role=controller
    // |agent in a running / Up state) unless `--all` is set. Applied
    // BEFORE index-cache write + render so the `#` column is dense
    // (0..N) over the visible rows and a downstream `isd stop <#>`
    // never targets a hidden infrastructure container by index. Errored
    // or stopped infrastructure stays visible without `--all`.
    let containers = filter_visible(containers, args.all);

    // JSON output: the raw DTO list, no index cache write (the `#`
    // column is a TTY affordance, not part of the JSON contract).
    if args.format == crate::output::Format::Json {
        // ContainerSummary is not Serialize; emit a stable hand-rolled
        // shape so `--format json` stays parseable.
        let json: Vec<serde_json::Value> = containers
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "image": c.image,
                    "status": c.status,
                    "ports": c.ports,
                    "names": c.names,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json)?);
        return Ok(());
    }

    // Write the index cache before rendering so a downstream
    // `isd stop <#>` always has the freshest row set.
    let cache = IndexCache {
        captured_at: chrono::Utc::now(),
        command: "ps".to_string(),
        rows: build_index_rows(&containers, &context_name),
    };
    if let Err(e) = crate::index_cache::write(&cache) {
        // A cache-write failure is not fatal to `isd ps`; warn and
        // carry on so the operator still sees their containers.
        eprintln!("isd: warning: could not write index cache: {e:#}");
    }

    let table = Table {
        columns: docker_ps_columns(),
        rows: build_docker_rows(&containers),
    };

    let term = console::Term::stdout();
    if term.is_term() {
        let width = term.size().1 as usize;
        println!("{}", render(&table, width, console::colors_enabled()));
    } else {
        println!("{}", render_plain(&table));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use isd_runtime::ContainerSummary;
    use std::collections::HashMap;

    fn sample_summaries() -> Vec<ContainerSummary> {
        vec![
            ContainerSummary {
                id: "a1b2c3d4e5f6deadbeefcafe".into(),
                image: "nginx:1.27".into(),
                status: "Up 2 hours".into(),
                ports: "0.0.0.0:80->80/tcp".into(),
                private_ports: vec![80],
                names: "web-proxy".into(),
                labels: HashMap::new(),
            },
            ContainerSummary {
                id: "7f8e9d0c1b2acafef00dbabe".into(),
                image: "postgres:16".into(),
                status: "Exited (0) 12 minutes ago".into(),
                ports: String::new(),
                private_ports: Vec::new(),
                names: "app-db".into(),
                labels: HashMap::new(),
            },
        ]
    }

    /// Helper for filter tests: build a `ContainerSummary` with a
    /// given name, an optional `io.isengard.role` label value, and a
    /// docker-style status string.
    fn make_row(name: &str, role: Option<&str>, status: &str) -> isd_runtime::ContainerSummary {
        let mut labels = HashMap::new();
        if let Some(r) = role {
            labels.insert(ROLE_LABEL.to_string(), r.to_string());
        }
        isd_runtime::ContainerSummary {
            id: format!("{name}-id"),
            image: "test:latest".into(),
            status: status.into(),
            ports: String::new(),
            private_ports: Vec::new(),
            names: name.into(),
            labels,
        }
    }

    /// `is_healthy_status` accepts the docker shapes that mean
    /// "container is running" and rejects everything else, including
    /// the `Up ... (unhealthy)` and `Up ... (starting)` healthcheck
    /// variants the operator needs to see.
    #[test]
    fn healthy_status_matrix() {
        assert!(is_healthy_status("Up 2 hours"));
        assert!(is_healthy_status("Up 5 seconds"));
        assert!(is_healthy_status("Up About a minute (healthy)"));
        assert!(is_healthy_status("Up"));
        assert!(!is_healthy_status("Up 3 minutes (unhealthy)"));
        assert!(!is_healthy_status("Up 1 second (health: starting)"));
        assert!(!is_healthy_status("Exited (0) 12 minutes ago"));
        assert!(!is_healthy_status("Restarting (1) 4 seconds ago"));
        assert!(!is_healthy_status("Created"));
        assert!(!is_healthy_status("Dead"));
        assert!(!is_healthy_status("Paused"));
        assert!(!is_healthy_status(""));
    }

    /// `isd ps` (default, no `--all`) hides healthy Isengard
    /// infrastructure (controller / agent in an `Up` state) but keeps
    /// errored infrastructure visible because that is the case the
    /// operator needs to see. Workload rows pass through regardless of
    /// status so the table preserves docker-style visibility for the
    /// operator's own containers.
    #[test]
    fn ps_hides_healthy_protected_but_keeps_errored_and_workloads() {
        let rows = vec![
            make_row("isd-controller", Some("controller"), "Up 2 hours"),
            make_row("isd-agent", Some("agent"), "Exited (1) 4 seconds ago"),
            make_row("bazarr", None, "Up 10 minutes"),
        ];
        let filtered = filter_visible(rows, false);
        let names: Vec<&str> = filtered.iter().map(|r| r.names.as_str()).collect();
        assert_eq!(names, vec!["isd-agent", "bazarr"]);
        // The `#` column is dense (0..N) over the visible rows so
        // downstream `isd stop <#>` never targets a hidden row by index.
        let display = build_docker_rows(&filtered);
        assert_eq!(display.len(), 2);
        assert_eq!(display[0][0], "0");
        assert_eq!(display[1][0], "1");
        let index_rows = build_index_rows(&filtered, "lausanne");
        assert_eq!(index_rows.len(), 2);
        assert_eq!(index_rows[0].index, 0);
        assert_eq!(index_rows[1].index, 1);
    }

    /// `--all` returns everything unfiltered: every workload row plus
    /// every infrastructure row, healthy or not. Restores the pre-filter
    /// behavior for operators who want the full list.
    #[test]
    fn ps_all_shows_everything() {
        let rows = vec![
            make_row("isd-controller", Some("controller"), "Up 2 hours"),
            make_row("bazarr", None, "Up 10 minutes"),
            make_row("isd-agent", Some("agent"), "Up 2 hours (healthy)"),
        ];
        let filtered = filter_visible(rows, true);
        assert_eq!(filtered.len(), 3);
        let names: Vec<&str> = filtered.iter().map(|r| r.names.as_str()).collect();
        assert_eq!(names, vec!["isd-controller", "bazarr", "isd-agent"]);
    }

    /// A healthy protected container with the `(healthy)` healthcheck
    /// suffix is still hidden by default. The healthcheck does not
    /// change visibility, it only confirms what `Up <duration>` already
    /// implies.
    #[test]
    fn ps_hides_healthy_protected_with_healthcheck() {
        let rows = vec![
            make_row("isd-controller", Some("controller"), "Up 2 hours (healthy)"),
            make_row("bazarr", None, "Up 10 minutes"),
        ];
        let filtered = filter_visible(rows, false);
        let names: Vec<&str> = filtered.iter().map(|r| r.names.as_str()).collect();
        assert_eq!(names, vec!["bazarr"]);
    }

    /// An `Up ... (unhealthy)` protected container is visible by
    /// default. This is the failure case the operator most needs to
    /// see: the container is technically up but the healthcheck is
    /// failing.
    #[test]
    fn ps_keeps_unhealthy_protected_visible() {
        let rows = vec![
            make_row("isd-agent", Some("agent"), "Up 3 minutes (unhealthy)"),
            make_row("bazarr", None, "Up 10 minutes"),
        ];
        let filtered = filter_visible(rows, false);
        let names: Vec<&str> = filtered.iter().map(|r| r.names.as_str()).collect();
        assert_eq!(names, vec!["isd-agent", "bazarr"]);
    }

    /// The three-row demo case the spec calls out: a healthy protected
    /// row, an erroring protected row, and a normal workload. Default
    /// filter keeps two rows (erroring protected + workload); `--all`
    /// keeps all three.
    #[test]
    fn ps_three_row_demo_case() {
        let rows = vec![
            make_row("isd-controller", Some("controller"), "Up 2 hours"),
            make_row("isd-agent", Some("agent"), "Exited (1) 4 seconds ago"),
            make_row("bazarr", None, "Up 10 minutes"),
        ];
        // Default: two rows visible.
        let default = filter_visible(rows.clone(), false);
        assert_eq!(default.len(), 2);
        let default_names: Vec<&str> = default.iter().map(|r| r.names.as_str()).collect();
        assert_eq!(default_names, vec!["isd-agent", "bazarr"]);
        // `--all`: all three rows visible.
        let all = filter_visible(rows, true);
        assert_eq!(all.len(), 3);
        let all_names: Vec<&str> = all.iter().map(|r| r.names.as_str()).collect();
        assert_eq!(all_names, vec!["isd-controller", "isd-agent", "bazarr"]);
    }

    /// `is_protected` catches the agent container even when the runtime
    /// only carries the compose labels (current shape on lausanne, where
    /// the `io.isengard.role=agent` stamp is missing). Without this
    /// fallback the very container the operator complained about would
    /// still leak through the default filter.
    #[test]
    fn protected_detects_compose_labels() {
        let mut labels = HashMap::new();
        labels.insert(COMPOSE_PROJECT_LABEL.to_string(), "isengard".to_string());
        labels.insert(COMPOSE_SERVICE_LABEL.to_string(), "agent".to_string());
        let c = isd_runtime::ContainerSummary {
            id: "agent-id".into(),
            image: "ghcr.io/weavers-engineering/isengard-agent:next".into(),
            status: "Up 36 hours".into(),
            ports: String::new(),
            private_ports: Vec::new(),
            names: "compose-only-agent".into(),
            labels,
        };
        assert!(is_protected(&c));
    }

    /// `is_protected` falls through to the canonical container names
    /// when neither the role label nor the compose labels are present.
    /// Covers pre-rename fleets that still use `iso-*` names and
    /// post-rename fleets on `isd-*`.
    #[test]
    fn protected_detects_canonical_names() {
        for name in ["isd-controller", "isd-agent", "iso-controller", "iso-agent"] {
            let c = isd_runtime::ContainerSummary {
                id: format!("{name}-id"),
                image: "test:latest".into(),
                status: "Up 1m".into(),
                ports: String::new(),
                private_ports: Vec::new(),
                names: name.into(),
                labels: HashMap::new(),
            };
            assert!(is_protected(&c), "expected {name} to be protected");
        }
    }

    /// A wildly different compose project / service combination does
    /// NOT match the protected predicate. Guard against accidentally
    /// hiding workloads whose compose service name happens to be
    /// `agent` outside the `isengard` project.
    #[test]
    fn protected_ignores_unrelated_compose_service() {
        let mut labels = HashMap::new();
        labels.insert(COMPOSE_PROJECT_LABEL.to_string(), "media-stack".to_string());
        labels.insert(COMPOSE_SERVICE_LABEL.to_string(), "agent".to_string());
        let c = isd_runtime::ContainerSummary {
            id: "id".into(),
            image: "test:latest".into(),
            status: "Up 1m".into(),
            ports: String::new(),
            private_ports: Vec::new(),
            names: "media-agent".into(),
            labels,
        };
        assert!(!is_protected(&c));
    }

    /// `isd ps --help` documents the `-a` / `--all` flag and explains
    /// the healthy-infrastructure hiding behavior so the operator can
    /// discover the override without reading the source.
    #[test]
    fn ps_help_surface_documents_all_flag() {
        // `PsArgs` is a clap `Args` (not a `Parser`), so it has no
        // standalone `command()`. Augment a fresh `clap::Command` with
        // the `PsArgs` shape to render its help text in isolation.
        let mut cmd = <PsArgs as clap::Args>::augment_args(clap::Command::new("ps"));
        let rendered = cmd.render_help().to_string();
        assert!(
            rendered.contains("--all"),
            "help should advertise --all: rendered={rendered}"
        );
        assert!(
            rendered.contains("-a"),
            "help should advertise the short -a alias: rendered={rendered}"
        );
        // The behavior callout is what the operator needs to recognise:
        // healthy infrastructure is hidden by default, errored stays
        // visible. The exact wording lives in the doc comment on the
        // field; the test only checks the load-bearing keywords.
        let lower = rendered.to_lowercase();
        assert!(
            lower.contains("infrastructure"),
            "help should mention infrastructure: rendered={rendered}"
        );
        assert!(
            lower.contains("healthy"),
            "help should mention the healthy gate: rendered={rendered}"
        );
    }

    #[test]
    fn docker_rows_truncate_id_and_assign_index() {
        let rows = build_docker_rows(&sample_summaries());
        assert_eq!(rows.len(), 2);
        // `#` column is the render-order index as a string.
        assert_eq!(rows[0][0], "0");
        assert_eq!(rows[1][0], "1");
        // CONTAINER ID truncated to 12 chars.
        assert_eq!(rows[0][1], "a1b2c3d4e5f6");
        // Columns are in the spec's order: #, ID, IMAGE, STATUS, PORTS, NAMES.
        assert_eq!(rows[0][2], "nginx:1.27");
        assert_eq!(rows[0][3], "Up 2 hours");
        assert_eq!(rows[0][4], "0.0.0.0:80->80/tcp");
        assert_eq!(rows[0][5], "web-proxy");
    }

    #[test]
    fn docker_index_cache_rows_keep_full_id_and_context() {
        let summaries = sample_summaries();
        let cache_rows = build_index_rows(&summaries, "lausanne");
        assert_eq!(cache_rows.len(), 2);
        assert_eq!(cache_rows[0].index, 0);
        assert_eq!(cache_rows[0].context, "lausanne");
        // Cache keeps the FULL id, not the truncated display form.
        assert_eq!(cache_rows[0].container_id, "a1b2c3d4e5f6deadbeefcafe");
        assert_eq!(cache_rows[1].name, "app-db");
    }

    /// Bare `isd` invokes `Ps` with the default arg shape.
    /// This guards against accidental regressions to the clap-default
    /// path that the main.rs match arm relies on.
    #[test]
    fn ps_args_default_has_no_filters_and_default_format() {
        let args = PsArgs::default();
        assert!(!args.all);
        assert!(!args.no_trunc);
        assert!(args.filters.is_empty());
        assert_eq!(args.format, crate::output::Format::Table);
        // grouping defaults to on (auto-enabled when
        // >1 host present); --host filter unset by default.
        assert!(!args.no_group);
        assert!(args.host.is_none());
    }

    /// End-to-end: a docker-backed context renders the boxed table and
    /// writes the index cache. Ignored because it needs a real docker
    /// daemon on the local socket. Run with
    /// `cargo test -p isd -- --ignored docker_backend`.
    #[tokio::test]
    #[ignore]
    async fn docker_backend_writes_index_cache() {
        let dir = tempfile::tempdir().unwrap();
        // `ISD_INDEX_CACHE` now names a directory; per-command files
        // (`last-ps.json` here) land inside it.
        let cache_path = dir.path().join("last-ps.json");
        // Use an empty $DOCKER_CONFIG so we resolve the synthetic
        // "default" context against $DOCKER_HOST / the local socket.
        // SAFETY: distinct temp paths per test; matches the existing env
        // pattern in this module.
        unsafe {
            std::env::set_var("DOCKER_CONFIG", dir.path());
            std::env::set_var("ISD_INDEX_CACHE", dir.path());
        }
        let args = PsArgs {
            all: true,
            format: crate::output::Format::Table,
            ..Default::default()
        };
        run(args, None)
            .await
            .expect("ps docker backend should succeed");
        unsafe {
            std::env::remove_var("DOCKER_CONFIG");
            std::env::remove_var("ISD_INDEX_CACHE");
        }
        assert!(cache_path.exists(), "index cache file was written");
        let body = std::fs::read_to_string(&cache_path).unwrap();
        assert!(body.contains("\"command\": \"ps\""));
    }
}
