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

// Docker uses 12 chars for the short CONTAINER ID; --no-trunc widens to 16.
const ID_DISPLAY_WIDTH: usize = 12;

#[derive(Debug, Args, Default)]
pub struct PsArgs {
    /// Show all containers, not just running.
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

    /// Show system containers (io.isengard.role=controller|agent). Hidden by default.
    #[arg(long)]
    pub all_system: bool,

    /// Force flat output (no per-host grouping). Default groups when
    /// the controller has more than one enrolled host.
    #[arg(long)]
    pub no_group: bool,

    /// Filter to a single host by name. Implies flat output (grouping
    /// only makes sense across multiple hosts).
    #[arg(long, value_name = "NAME")]
    pub host: Option<String>,
}

pub async fn run(args: PsArgs, context: Option<&str>) -> Result<()> {
    // Every context is a docker context with a docker URI. The
    // controller-direct REST path is gone for `ps`: we always go through
    // the DockerBackend.
    let docker_uri = crate::docker_context::resolve_docker_uri(context)?;
    run_docker_backend(args, docker_uri, context).await
}

/// Protection filter for the docker-direct path. Drops any
/// `ContainerSummary` whose `io.isengard.role` label is in the protected
/// set when `all_system` is false. With `all_system=true` returns the
/// input unchanged so `--all-system` shows everything.
pub(crate) fn filter_system(
    rows: Vec<isd_runtime::ContainerSummary>,
    all_system: bool,
) -> Vec<isd_runtime::ContainerSummary> {
    if all_system {
        return rows;
    }
    rows.into_iter()
        .filter(|c| {
            c.labels
                .get(ROLE_LABEL)
                .map(|role| !is_protected_label_value(role))
                .unwrap_or(true)
        })
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

async fn run_docker_backend(args: PsArgs, docker_uri: String, context: Option<&str>) -> Result<()> {
    use isd_runtime::DockerBackend;

    let context_name = crate::docker_context::resolve_context_name(context)?;
    let backend = DockerBackend::from_uri(&docker_uri)
        .await
        .with_context(|| format!("opening docker backend at {docker_uri}"))?;

    let containers = backend
        .list_containers(args.all)
        .await
        .context("listing containers")?;

    // Hide system containers (io.isengard.role=controller|agent)
    // unless `--all-system`. Applied BEFORE index-cache write + render so
    // the `#` column is dense (0..N) over the visible rows and a
    // downstream `isd stop <#>` never targets a system container by
    // index.
    let containers = filter_system(containers, args.all_system);

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
                names: "web-proxy".into(),
                labels: HashMap::new(),
            },
            ContainerSummary {
                id: "7f8e9d0c1b2acafef00dbabe".into(),
                image: "postgres:16".into(),
                status: "Exited (0) 12 minutes ago".into(),
                ports: String::new(),
                names: "app-db".into(),
                labels: HashMap::new(),
            },
        ]
    }

    /// Helper for filter tests: build a `ContainerSummary` with
    /// a given name and an optional `io.isengard.role` label value.
    fn make_row(name: &str, role: Option<&str>) -> isd_runtime::ContainerSummary {
        let mut labels = HashMap::new();
        if let Some(r) = role {
            labels.insert(ROLE_LABEL.to_string(), r.to_string());
        }
        isd_runtime::ContainerSummary {
            id: format!("{name}-id"),
            image: "test:latest".into(),
            status: "Up 1m".into(),
            ports: String::new(),
            names: name.into(),
            labels,
        }
    }

    /// `isd ps` filters out containers labelled
    /// `io.isengard.role=controller|agent` unless `--all-system` is set.
    /// The index column re-numbers over the visible rows so
    /// `isd stop <#>` never targets a system container by index.
    #[test]
    fn ps_hides_system_containers_by_default() {
        let rows = vec![
            make_row("iso-controller", Some("controller")),
            make_row("bazarr", None),
            make_row("iso-agent", Some("agent")),
            make_row("plex", None),
        ];
        let filtered = filter_system(rows, false);
        let names: Vec<&str> = filtered.iter().map(|r| r.names.as_str()).collect();
        assert_eq!(names, vec!["bazarr", "plex"]);
        // Build the display rows + index cache rows. The `#` column is
        // the render-order index of the *visible* rows, so it is dense
        // (0..N) over the user-visible set.
        let display = build_docker_rows(&filtered);
        assert_eq!(display.len(), 2);
        assert_eq!(display[0][0], "0");
        assert_eq!(display[1][0], "1");
        let index_rows = build_index_rows(&filtered, "lausanne");
        assert_eq!(index_rows.len(), 2);
        assert_eq!(index_rows[0].index, 0);
        assert_eq!(index_rows[1].index, 1);
    }

    /// `--all-system` returns everything unfiltered so the
    /// operator can still see the controller/agent rows when they ask.
    #[test]
    fn ps_all_system_shows_everything() {
        let rows = vec![
            make_row("iso-controller", Some("controller")),
            make_row("bazarr", None),
            make_row("iso-agent", Some("agent")),
        ];
        let filtered = filter_system(rows, true);
        assert_eq!(filtered.len(), 3);
        let names: Vec<&str> = filtered.iter().map(|r| r.names.as_str()).collect();
        assert_eq!(names, vec!["iso-controller", "bazarr", "iso-agent"]);
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
        let cache_path = dir.path().join("last-ps.json");
        // Use an empty $DOCKER_CONFIG so we resolve the synthetic
        // "default" context against $DOCKER_HOST / the local socket.
        // SAFETY: distinct temp paths per test; matches the existing env
        // pattern in this module.
        unsafe {
            std::env::set_var("DOCKER_CONFIG", dir.path());
            std::env::set_var("ISD_INDEX_CACHE", &cache_path);
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
