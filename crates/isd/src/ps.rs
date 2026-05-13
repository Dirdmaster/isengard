//! `isd ps`: container-first view of the saved context's controller.
//!
//! Phase 0.18 rewrite. The handler now hits `GET /api/v1/containers`
//! and renders one row per container, matching the column layout
//! `docker ps` operators expect (CONTAINER ID, IMAGE, COMMAND, STATUS,
//! HOST, STACK, NAMES). The legacy stack+service join is preserved
//! behind `--legacy` for one release; `isd ps --legacy` will be
//! removed in v0.6.
//!
//! Default behaviour: one round-trip to `/containers` plus `--filter`
//! query params. `--no-trunc` widens CONTAINER ID + COMMAND to their
//! full value; `--format json` dumps the raw API rows.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::session::Session;
use crate::table::{ContainerPsRow, render_container_json, render_container_table};

/// Default width of the CONTAINER ID column. Docker uses 12 chars;
/// `--no-trunc` widens to the full 16. Default-and-document per the
/// Phase 0.18 spec.
const ID_DISPLAY_WIDTH: usize = 12;
const COMMAND_TRUNC_WIDTH: usize = 40;

#[derive(Debug, Args, Default)]
pub struct PsArgs {
    /// Include containers whose `removed_at` is non-NULL (defaults to
    /// alive only). Matches `docker ps -a`.
    #[arg(short = 'a', long)]
    pub all: bool,

    /// Disable CONTAINER ID and COMMAND truncation.
    #[arg(long)]
    pub no_trunc: bool,

    /// Repeatable `key=value` filter applied at the controller. Known
    /// keys: `host`, `stack`, `service`, `state`. Unknown keys are
    /// ignored (the dashboard rejects with 400 if a future version
    /// validates them).
    #[arg(long = "filter", value_name = "KEY=VALUE")]
    pub filters: Vec<String>,

    /// One of `table` (default) or `json`. JSON output is the raw API
    /// row array.
    #[arg(long, default_value = "table")]
    pub format: String,

    /// Fall back to the legacy stack+service join (deprecated).
    /// Removed in v0.6.
    #[arg(long)]
    pub legacy: bool,

    /// Deprecated: alias of `--format json` kept for one release so
    /// pre-0.18 scripts don't break.
    #[arg(long, hide = true)]
    pub json: bool,

    /// Deprecated: legacy stack+service join honoured this. The new
    /// container view filters fleet at the controller via
    /// `--filter host=<id>` instead.
    #[arg(long, hide = true)]
    pub fleet: Option<String>,

    /// Phase 0.20: which transport to use. `controller` talks to the
    /// Isengard controller via REST; `docker` talks to the host's
    /// Docker daemon directly via the isd-runtime crate, which
    /// requires `docker = "..."` set on the context (see `isd context
    /// create --docker <uri>`). Defaults to `controller`; Phase 0.21
    /// swaps the default to `docker` when the resolved context carries
    /// a docker URI.
    #[arg(long, value_enum, default_value_t = PsBackend::Controller)]
    pub backend: PsBackend,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lower")]
pub enum PsBackend {
    #[default]
    Controller,
    Docker,
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
    if args.legacy {
        return run_legacy(args, context).await;
    }

    if args.backend == PsBackend::Docker {
        return run_docker_backend(args, context).await;
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

    if args.json || args.format == "json" {
        println!("{}", render_container_json(&rows)?);
    } else {
        let ps_rows = build_ps_rows(&rows, args.no_trunc);
        let out = render_container_table(&ps_rows);
        println!("{}", out.trim_end());
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

// ----- legacy path -----

#[derive(Debug, Deserialize)]
struct LegacyStackDto {
    id: String,
    #[allow(dead_code)]
    host_id: String,
    name: String,
    #[allow(dead_code)]
    source: String,
    #[allow(dead_code)]
    discovered_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct LegacyServiceDto {
    #[allow(dead_code)]
    id: String,
    host_id: String,
    hostname: Option<String>,
    stack_id: Option<String>,
    name: String,
    image: String,
    state: String,
    last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct LegacyHostBackendDto {
    id: String,
    #[serde(default = "default_backend")]
    runtime_backend: String,
}

fn default_backend() -> String {
    "docker".to_string()
}

// ----- direct-bollard path (Phase 0.20) -----

async fn run_docker_backend(_args: PsArgs, context: Option<&str>) -> Result<()> {
    use isd_runtime::DockerBackend;

    let path = crate::credentials::default_credentials_path()?;
    let file = crate::credentials::load(&path)?;
    let target_name = context
        .map(str::to_string)
        .or_else(|| file.default_context.clone())
        .ok_or_else(|| {
            anyhow::anyhow!("no context selected (pass --context <name> or set default_context)")
        })?;
    let ctx = file
        .contexts
        .iter()
        .find(|c| c.name == target_name)
        .ok_or_else(|| anyhow::anyhow!("context {target_name:?} not found"))?;
    let docker_uri = ctx.docker.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "context {:?} has no docker endpoint set; add one via `isd context create --docker <uri>` or re-create with --docker",
            ctx.name
        )
    })?;

    let backend = DockerBackend::from_uri(docker_uri)
        .await
        .with_context(|| format!("opening docker backend at {docker_uri}"))?;

    // Phase 0.20 placeholder: prove the connection works. Phase 0.21
    // replaces this with the real container listing + table render.
    let banner = backend.ping().await.context("ping docker daemon")?;
    println!("docker backend reachable: {banner}");
    Ok(())
}

async fn run_legacy(args: PsArgs, context: Option<&str>) -> Result<()> {
    eprintln!(
        "isd ps --legacy: stack+service join is deprecated and will be removed in v0.6. Switch to the default container view."
    );
    let session = Session::open(context).await?;
    let mut url = format!("{}/api/v1/stacks", session.controller_url());
    if let Some(f) = args.fleet.as_deref() {
        url.push_str(&format!("?fleet={f}"));
    }
    let stacks: Vec<LegacyStackDto> = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("listing stacks")?
        .json()
        .await
        .context("decoding stacks JSON")?;

    let services_url = format!("{}/api/v1/services", session.controller_url());
    let services: Vec<LegacyServiceDto> = session
        .client
        .get(&services_url)
        .send()
        .await
        .with_context(|| format!("GET {services_url}"))?
        .error_for_status()
        .context("listing services")?
        .json()
        .await
        .context("decoding services JSON")?;

    let hosts_url = format!("{}/api/v1/hosts", session.controller_url());
    let hosts: Vec<LegacyHostBackendDto> = match session.client.get(&hosts_url).send().await {
        Ok(resp) => match resp.error_for_status() {
            Ok(ok) => ok.json().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    };

    let rows = build_legacy_rows(&stacks, &services, &hosts);
    if args.json || args.format == "json" {
        println!("{}", crate::table::render_json(&rows)?);
    } else {
        let out = crate::table::render_table(&rows);
        println!("{}", out.trim_end());
    }
    Ok(())
}

fn build_legacy_rows(
    stacks: &[LegacyStackDto],
    services: &[LegacyServiceDto],
    hosts: &[LegacyHostBackendDto],
) -> Vec<crate::table::PsRow> {
    let mut by_id: std::collections::HashMap<&str, &LegacyStackDto> =
        std::collections::HashMap::new();
    for s in stacks {
        by_id.insert(&s.id, s);
    }
    let mut backend_by_host: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::new();
    for h in hosts {
        backend_by_host.insert(h.id.as_str(), h.runtime_backend.as_str());
    }
    let mut rows: Vec<crate::table::PsRow> = services
        .iter()
        .map(|svc| {
            let stack_name = svc
                .stack_id
                .as_deref()
                .and_then(|id| by_id.get(id))
                .map(|s| s.name.as_str())
                .unwrap_or("(unstacked)");
            let host_label = svc
                .hostname
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| svc.host_id.chars().take(8).collect::<String>());
            let backend = backend_by_host
                .get(svc.host_id.as_str())
                .copied()
                .unwrap_or("docker")
                .to_string();
            crate::table::PsRow {
                stack: stack_name.to_string(),
                service: svc.name.clone(),
                host: host_label,
                state: svc.state.clone(),
                image: svc.image.clone(),
                last_seen: humanize_age(svc.last_seen_at),
                backend,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        a.stack
            .cmp(&b.stack)
            .then_with(|| a.service.cmp(&b.service))
    });
    rows
}

fn humanize_age(when: DateTime<Utc>) -> String {
    let now = Utc::now();
    let delta = now.signed_duration_since(when);
    if delta.num_seconds() < 60 {
        format!("{}s ago", delta.num_seconds().max(0))
    } else if delta.num_minutes() < 60 {
        format!("{}m ago", delta.num_minutes())
    } else if delta.num_hours() < 24 {
        format!("{}h ago", delta.num_hours())
    } else {
        format!("{}d ago", delta.num_days())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(args.format, ""); // Default::default for String is empty
        assert!(!args.legacy);
        assert!(!args.json);
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
            format: "table".into(),
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
            format: "table".into(),
            ..Default::default()
        };
        run(args, None).await.expect("ps should succeed");
    }
}
