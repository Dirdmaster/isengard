//! `isd ps`: container-first view of the saved context's controller.
//!
//! One round-trip to `/containers` plus `--filter` query params.
//! `--no-trunc` widens CONTAINER ID + COMMAND to their full value;
//! `--format json` dumps the raw API rows.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::index_cache::{IndexCache, IndexRow};
use crate::render::{Align, CellStyle, Column, Table, render, render_plain};
use crate::session::Session;
use crate::table::{ContainerPsRow, render_container_json, render_container_table};

// Docker uses 12 chars for the short CONTAINER ID; --no-trunc widens to 16.
const ID_DISPLAY_WIDTH: usize = 12;
const COMMAND_TRUNC_WIDTH: usize = 40;

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
}

/// One row from `GET /api/v1/containers`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContainerApiDto {
    pub id: String,
    pub runtime_container_id: String,
    pub image: String,
    pub command: Option<String>,
    pub state: String,
    pub status_message: Option<String>,
    pub names: String,
    pub stack: Option<String>,
    pub service: Option<String>,
    pub host_id: String,
    pub host_name: Option<String>,
    pub host_offline: bool,
    pub host_offline_secs: i64,
    pub created_at: Option<DateTime<Utc>>,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub removed_at: Option<DateTime<Utc>>,
}

pub async fn run(args: PsArgs, context: Option<&str>) -> Result<()> {
    // Phase 0.20: container-level commands route by context shape, not
    // by a CLI flag. If the resolved context carries a `docker = "..."`
    // endpoint, `isd ps` is a docker-daemon round-trip; otherwise we
    // fall back to the Isengard controller's REST surface.
    if let Some(docker_uri) = resolve_docker_uri(context)? {
        return run_docker_backend(args, docker_uri, context).await;
    }

    let session = Session::open(context).await?;
    let url = build_url(session.controller_url(), &args)?;

    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;

    let rows: Vec<ContainerApiDto> = resp
        .error_for_status()
        .context("listing containers")?
        .json()
        .await
        .context("decoding containers JSON")?;

    match args.format {
        crate::output::Format::Json => {
            println!("{}", render_container_json(&rows)?);
        }
        crate::output::Format::Table => {
            let ps_rows = build_ps_rows(&rows, args.no_trunc);
            let out = render_container_table(&ps_rows);
            println!("{}", out.trim_end());
        }
    }
    Ok(())
}

/// Build a `ContainerPsRow` per row, applying ID + COMMAND truncation
/// when `no_trunc` is false. STATUS picks up the host-offline qualifier
/// when applicable.
pub fn build_ps_rows(rows: &[ContainerApiDto], no_trunc: bool) -> Vec<ContainerPsRow> {
    rows.iter()
        .map(|row| {
            let id = if no_trunc {
                row.id.clone()
            } else {
                row.id.chars().take(ID_DISPLAY_WIDTH).collect()
            };
            let command = match (&row.command, no_trunc) {
                (Some(c), false) if c.chars().count() > COMMAND_TRUNC_WIDTH => {
                    let truncated: String = c.chars().take(COMMAND_TRUNC_WIDTH).collect();
                    format!("{truncated}...")
                }
                (Some(c), _) => c.clone(),
                (None, _) => String::new(),
            };
            let status = build_status_column(row);
            let host = row
                .host_name
                .clone()
                .unwrap_or_else(|| row.host_id.chars().take(8).collect());
            ContainerPsRow {
                container_id: id,
                image: row.image.clone(),
                command,
                status,
                host,
                stack: row.stack.clone().unwrap_or_default(),
                names: row.names.clone(),
            }
        })
        .collect()
}

/// STATUS column: the row's `status_message` (rendered agent-side)
/// with an optional `(host offline 30s)` qualifier appended when the
/// controller's join flagged the host as offline.
fn build_status_column(row: &ContainerApiDto) -> String {
    let base = row
        .status_message
        .clone()
        .unwrap_or_else(|| row.state.clone());
    if row.host_offline {
        let qualifier = humanize_secs(row.host_offline_secs);
        format!("{base} (host offline {qualifier})")
    } else {
        base
    }
}

fn humanize_secs(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// Build the GET URL from base + filters. Unknown filter keys pass
/// through (let the controller decide whether to honour them).
pub fn build_url(base: &str, args: &PsArgs) -> Result<String> {
    let mut url = format!("{base}/api/v1/containers");
    let mut parts: Vec<String> = Vec::new();
    if args.all {
        parts.push("all=true".to_string());
    }
    for filter in &args.filters {
        let (k, v) = filter
            .split_once('=')
            .with_context(|| format!("invalid --filter (need key=value): {filter}"))?;
        let k = k.trim();
        let v = v.trim();
        if k.is_empty() || v.is_empty() {
            anyhow::bail!("invalid --filter (empty key or value): {filter}");
        }
        let encoded_k = urlencoded(k);
        let encoded_v = urlencoded(v);
        parts.push(format!("{encoded_k}={encoded_v}"));
    }
    if !parts.is_empty() {
        url.push('?');
        url.push_str(&parts.join("&"));
    }
    Ok(url)
}

/// Minimal percent-encoding for filter values: we only escape the
/// characters that would break the query-string contract (`&`, `=`,
/// `#`, `+`, ` `). Filter values are operator-supplied so we don't
/// need a general-purpose encoder.
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("%26"),
            '=' => out.push_str("%3D"),
            '#' => out.push_str("%23"),
            '+' => out.push_str("%2B"),
            ' ' => out.push_str("%20"),
            _ => out.push(ch),
        }
    }
    out
}

// ----- direct-bollard path (Phase 0.20) -----

/// Resolve the current context (default or `--context <name>` override)
/// and return its `docker = "..."` URI when set. Returns `Ok(None)` if
/// the context has no docker endpoint; the caller falls back to the
/// controller path. Returns `Ok(None)` for "no context selected at
/// all" too, so the controller path can produce its own canonical
/// "no context" error.
pub(crate) fn resolve_docker_uri(context: Option<&str>) -> Result<Option<String>> {
    let path = crate::credentials::default_credentials_path()?;
    let file = crate::credentials::load(&path)?;
    let target_name = context
        .map(str::to_string)
        .or_else(|| file.default_context.clone());
    let Some(name) = target_name else {
        return Ok(None);
    };
    let ctx = file
        .contexts
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| anyhow::anyhow!("context {name:?} not found"))?;
    Ok(ctx.docker.clone())
}

/// Resolve the context name actually in use (the `--context` override
/// or the file's `default_context`). Returns the name string so the
/// index cache can record which host each row lives on. Returns an
/// error only if no context is selected at all; a missing docker URI
/// is handled upstream by `resolve_docker_uri`.
pub(crate) fn resolve_docker_context(context: Option<&str>) -> Result<String> {
    let path = crate::credentials::default_credentials_path()?;
    let file = crate::credentials::load(&path)?;
    context
        .map(str::to_string)
        .or(file.default_context)
        .ok_or_else(|| anyhow::anyhow!("no context selected; pass --context <name>"))
}

/// Open a DockerBackend for the resolved context. Used by the
/// lifecycle commands so they share `ps`'s context-resolution + docker
/// connection path.
pub(crate) async fn open_docker_backend(
    context: Option<&str>,
) -> Result<isd_runtime::DockerBackend> {
    let uri = resolve_docker_uri(context)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no docker endpoint on the resolved context; \
             add one with `isd context create ... --docker ssh://...`"
        )
    })?;
    isd_runtime::DockerBackend::from_uri(&uri)
        .await
        .with_context(|| format!("opening docker backend at {uri}"))
}

/// Column layout for the docker-backend `isd ps` table. Order matches
/// the spec mockup: #, CONTAINER ID, IMAGE, STATUS, PORTS, NAMES.
fn docker_ps_columns() -> Vec<Column> {
    vec![
        Column::new("#", Align::Right, CellStyle::Dim, 9, 1),
        Column::new("CONTAINER ID", Align::Left, CellStyle::Dim, 7, 12),
        Column::new("IMAGE", Align::Left, CellStyle::Plain, 2, 10),
        Column::new("STATUS", Align::Left, CellStyle::State, 8, 10),
        Column::new("PORTS", Align::Left, CellStyle::Plain, 1, 6),
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

    let context_name = resolve_docker_context(context)?;
    let backend = DockerBackend::from_uri(&docker_uri)
        .await
        .with_context(|| format!("opening docker backend at {docker_uri}"))?;

    let containers = backend
        .list_containers(args.all)
        .await
        .context("listing containers")?;

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

    fn sample_summaries() -> Vec<ContainerSummary> {
        vec![
            ContainerSummary {
                id: "a1b2c3d4e5f6deadbeefcafe".into(),
                image: "nginx:1.27".into(),
                status: "Up 2 hours".into(),
                ports: "0.0.0.0:80->80/tcp".into(),
                names: "web-proxy".into(),
            },
            ContainerSummary {
                id: "7f8e9d0c1b2acafef00dbabe".into(),
                image: "postgres:16".into(),
                status: "Exited (0) 12 minutes ago".into(),
                ports: String::new(),
                names: "app-db".into(),
            },
        ]
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

    fn sample_dto(id: &str) -> ContainerApiDto {
        ContainerApiDto {
            id: id.into(),
            runtime_container_id: format!("rt-{id}"),
            image: "nginx:alpine".into(),
            command: Some("nginx -g 'daemon off;'".into()),
            state: "running".into(),
            status_message: Some("Up 5m".into()),
            names: format!("{id}-name"),
            stack: Some("hello".into()),
            service: Some("web".into()),
            host_id: "01HXABCDEFGHJKMNPQRSTVWXYZ".into(),
            host_name: Some("homelab-01".into()),
            host_offline: false,
            host_offline_secs: 0,
            created_at: Some(Utc::now()),
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
            removed_at: None,
        }
    }

    /// Phase 0.18: default `isd ps` truncates the CONTAINER ID column
    /// to 12 chars; `--no-trunc` leaves it full-width.
    #[test]
    fn container_id_truncation_applies_by_default() {
        let dto = sample_dto("0123456789abcdef");
        let rows = build_ps_rows(std::slice::from_ref(&dto), false);
        assert_eq!(rows[0].container_id, "0123456789ab");

        let rows = build_ps_rows(&[dto], true);
        assert_eq!(rows[0].container_id, "0123456789abcdef");
    }

    /// Phase 0.18: COMMAND truncates to 40 chars + ellipsis when
    /// default; `--no-trunc` keeps the full string.
    #[test]
    fn command_truncation_adds_ellipsis_by_default() {
        let mut dto = sample_dto("abc");
        dto.command = Some("a".repeat(80));
        let rows = build_ps_rows(std::slice::from_ref(&dto), false);
        assert!(rows[0].command.ends_with("..."));
        // 40 chars of `a` + the literal `...` suffix.
        assert_eq!(rows[0].command.len(), 43);

        let rows = build_ps_rows(&[dto], true);
        assert_eq!(rows[0].command.len(), 80);
    }

    /// Phase 0.18: when the controller flags the host as offline, the
    /// STATUS column carries a `(host offline N)` qualifier.
    #[test]
    fn status_column_appends_offline_qualifier() {
        let mut dto = sample_dto("abc");
        dto.host_offline = true;
        dto.host_offline_secs = 90;
        dto.status_message = Some("Up 1h".into());
        let rows = build_ps_rows(&[dto], false);
        assert_eq!(rows[0].status, "Up 1h (host offline 1m)");
    }

    /// Phase 0.18: JSON output is the raw API row array. Smoke-check
    /// that the structure is parseable round-trip.
    #[test]
    fn json_output_is_raw_api_array() {
        let rows = vec![sample_dto("abc"), sample_dto("def")];
        let json = render_container_json(&rows).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "abc");
        assert_eq!(parsed[1]["id"], "def");
    }

    /// Phase 0.18: `--filter k=v` round-trips through build_url with
    /// percent encoding for the special characters that would break
    /// query-string parsing.
    #[test]
    fn build_url_encodes_filters_and_combines_with_all() {
        let args = PsArgs {
            all: true,
            filters: vec!["stack=hello".into(), "host=01HXABC".into()],
            ..Default::default()
        };
        let url = build_url("http://controller.local:9418", &args).unwrap();
        assert!(url.contains("all=true"));
        assert!(url.contains("stack=hello"));
        assert!(url.contains("host=01HXABC"));

        // Bad filter shape errors with a helpful message.
        let args = PsArgs {
            filters: vec!["malformed".into()],
            ..Default::default()
        };
        let err = build_url("http://x", &args).unwrap_err().to_string();
        assert!(err.contains("invalid --filter"));
    }

    /// Phase 0.18: bare `isd` invokes `Ps` with the default arg shape.
    /// This guards against accidental regressions to the clap-default
    /// path that the main.rs match arm relies on.
    #[test]
    fn ps_args_default_has_no_filters_and_default_format() {
        let args = PsArgs::default();
        assert!(!args.all);
        assert!(!args.no_trunc);
        assert!(args.filters.is_empty());
        assert_eq!(args.format, crate::output::Format::Table);
    }

    /// Phase 0.18: end-to-end against a wiremock controller. The handler
    /// issues exactly one `GET /api/v1/containers` and renders the row
    /// to stdout. Marked `#[ignore]` so it doesn't race with other
    /// credentials-file-touching tests; run with
    /// `cargo test -p isd -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn ps_hits_containers_endpoint_against_wiremock() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/containers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "id": "a1b2c3d4e5f6a7b8",
                    "runtime_container_id": "rt-1",
                    "image": "nginx:alpine",
                    "command": "nginx -g 'daemon off;'",
                    "state": "running",
                    "status_message": "Up 5m",
                    "names": "hello-web.1",
                    "stack": "hello",
                    "service": "web",
                    "host_id": "01HXABCDEFGHJKMNPQRSTVWXYZ",
                    "host_name": "homelab-01",
                    "host_offline": false,
                    "host_offline_secs": 0,
                    "created_at": "2026-05-13T12:00:00Z",
                    "first_seen_at": "2026-05-13T12:00:01Z",
                    "last_seen_at": "2026-05-13T12:05:42Z",
                    "removed_at": null
                }
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join("credentials.toml");
        std::fs::write(
            &creds_path,
            format!(
                r#"default_context = "alice"

[[contexts]]
name = "alice"
kind = "http"
url = "{}"
"#,
                server.uri()
            ),
        )
        .unwrap();
        // SAFETY: matches manifest_cmd.rs's ignored-test pattern.
        unsafe {
            std::env::set_var("ISD_CREDENTIALS_FILE", &creds_path);
        }

        let args = PsArgs {
            format: crate::output::Format::Table,
            ..Default::default()
        };
        run(args, None).await.expect("ps should succeed");
    }

    /// Phase 0.18: `--filter` round-trips into query-string params on
    /// the GET to the controller. Marked `#[ignore]` for the same
    /// reason as the previous test.
    #[tokio::test]
    #[ignore]
    async fn ps_passes_filters_in_query_string() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/containers"))
            .and(query_param("stack", "hello"))
            .and(query_param("state", "running"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join("credentials.toml");
        std::fs::write(
            &creds_path,
            format!(
                r#"default_context = "alice"

[[contexts]]
name = "alice"
kind = "http"
url = "{}"
"#,
                server.uri()
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("ISD_CREDENTIALS_FILE", &creds_path);
        }

        let args = PsArgs {
            filters: vec!["stack=hello".into(), "state=running".into()],
            format: crate::output::Format::Table,
            ..Default::default()
        };
        run(args, None).await.expect("ps should succeed");
    }

    /// End-to-end: a docker-backed context renders the boxed table and
    /// writes the index cache. Ignored because it needs a real docker
    /// daemon on the local socket. Run with
    /// `cargo test -p isd -- --ignored docker_backend`.
    #[tokio::test]
    #[ignore]
    async fn docker_backend_writes_index_cache() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("credentials.toml");
        std::fs::write(
            &creds,
            r#"default_context = "local"

[[contexts]]
name = "local"
kind = "http"
url = "http://no-controller.invalid"
docker = "local"
"#,
        )
        .unwrap();
        let cache_path = dir.path().join("last-ps.json");
        // SAFETY: distinct temp paths per test; matches the existing
        // ISD_CREDENTIALS_FILE pattern in this module.
        unsafe {
            std::env::set_var("ISD_CREDENTIALS_FILE", &creds);
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
        assert!(cache_path.exists(), "index cache file was written");
        let body = std::fs::read_to_string(&cache_path).unwrap();
        assert!(body.contains("\"command\": \"ps\""));
    }
}
