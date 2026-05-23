//! `isd hosts ls`: enumerate every host enrolled on the controller.
//! Talks to `GET /api/v1/hosts`.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::render::{Align, CellStyle, Column, Table, render, render_plain};
use crate::session::Session;

/// CLI flags for `isd hosts`.
#[derive(Debug, Args)]
pub struct HostsArgs {
    /// Resolved sub-verb.
    #[command(subcommand)]
    pub command: HostsCommand,
}

/// Sub-verbs under `isd hosts`.
#[derive(Debug, Subcommand)]
pub enum HostsCommand {
    /// List enrolled hosts.
    Ls(LsArgs),
}

/// CLI flags for `isd hosts ls`.
#[derive(Debug, Args)]
pub struct LsArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = crate::output::Format::Table)]
    pub format: crate::output::Format,
    /// Render the full 26-char ULID instead of the short suffix.
    ///
    /// JSON output always carries the full id regardless of this flag.
    #[arg(long)]
    pub full_id: bool,
}

/// Subset of the dashboard's `HostDto` we render. Pre-0.5 fields are
/// reused; pre-existing back-compat (e.g. `runtime_backend` default)
/// is enforced server-side, so we just decode whatever lands.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HostRow {
    /// Host ULID (26-char base32).
    pub id: String,
    /// Reported hostname.
    pub hostname: String,
    /// When the agent first enrolled.
    pub enrolled_at: DateTime<Utc>,
}

/// Dispatch to the matching `hosts` sub-verb.
///
/// # Errors
///
/// Propagates the sub-verb's error.
pub async fn run(args: HostsArgs, context: Option<&str>) -> Result<()> {
    match args.command {
        HostsCommand::Ls(a) => run_list(a, context).await,
    }
}

/// Fetch `GET /api/v1/hosts` and render the rows in the requested
/// format. Empty input still emits the header so a downstream `wc -l`
/// gets a stable shape.
///
/// # Errors
///
/// Returns `Err` on HTTP failure, controller-less context, or JSON
/// decode errors.
async fn run_list(args: LsArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/hosts");
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let rows: Vec<HostRow> = resp
        .error_for_status()
        .context("listing hosts")?
        .json()
        .await
        .context("decoding hosts JSON")?;

    match args.format {
        crate::output::Format::Json => {
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        crate::output::Format::Table => print_hosts_table(&rows, args.full_id),
    }
    Ok(())
}

/// Column layout for `isd hosts ls`: `#` dim + right-aligned, `HOST ID`
/// dim (renders the short suffix by default), `NAME` emphasized (bold),
/// `ENROLLED` dim. `min_width` on HOST ID stays 12 so the column has
/// room for the full ULID when `--full-id` is set.
fn hosts_columns() -> Vec<Column> {
    vec![
        Column::new("#", Align::Right, CellStyle::Dim, 9, 1),
        Column::new("HOST ID", Align::Left, CellStyle::Dim, 7, 8),
        Column::new("NAME", Align::Left, CellStyle::Emphasis, 1, 8),
        Column::new("ENROLLED", Align::Left, CellStyle::Dim, 4, 20),
    ]
}

/// Build the display rows: `#` plus the three data columns. Timestamp
/// is RFC 3339 with seconds (matches the rest of isd's surface).
/// `full_id` controls the HOST ID rendering: short suffix by default,
/// full ULID when the operator opts in.
fn build_rows(rows: &[HostRow], full_id: bool) -> Vec<Vec<String>> {
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let id = if full_id {
                r.id.clone()
            } else {
                crate::host_id::short(&r.id)
            };
            vec![
                i.to_string(),
                id,
                r.hostname.clone(),
                r.enrolled_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            ]
        })
        .collect()
}

/// Print the hosts table: rounded-corner boxed renderer on a TTY, plain
/// tab-separated when stdout is redirected so scripts stay clean.
fn print_hosts_table(rows: &[HostRow], full_id: bool) {
    let table = Table {
        columns: hosts_columns(),
        rows: build_rows(rows, full_id),
    };
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

    fn host_row(id: &str, name: &str) -> HostRow {
        HostRow {
            id: id.into(),
            hostname: name.into(),
            enrolled_at: Utc.with_ymd_and_hms(2026, 5, 11, 1, 35, 42).unwrap(),
        }
    }

    fn render_for_test(rows: &[HostRow], full_id: bool) -> String {
        let table = Table {
            columns: hosts_columns(),
            rows: build_rows(rows, full_id),
        };
        render_plain(&table)
    }

    #[test]
    fn table_contains_header_and_each_row_with_full_id() {
        let rows = vec![
            host_row("01KRA25CW1F263ETCNTJCGJJ59", "iso-fresh-1"),
            host_row("01KRA25YV3HFJG2EKEKW5KFKY3", "iso-fresh-2"),
        ];
        let t = render_for_test(&rows, true);
        assert!(t.contains("HOST ID"), "header has HOST ID column");
        assert!(t.contains("NAME"), "header has NAME column");
        assert!(t.contains("ENROLLED"), "header has ENROLLED column");
        // kill-fleets: no FLEET column.
        assert!(!t.contains("FLEET"));
        assert!(t.contains("01KRA25CW1F263ETCNTJCGJJ59"));
        assert!(t.contains("01KRA25YV3HFJG2EKEKW5KFKY3"));
        assert!(t.contains("iso-fresh-1"));
        assert!(t.contains("iso-fresh-2"));
        assert!(t.contains("2026-05-11T01:35:42Z"));
    }

    #[test]
    fn default_render_shows_short_host_id_suffix() {
        let rows = vec![host_row("01KRA25CW1F263ETCNTJCGJJ59", "iso-fresh-1")];
        let t = render_for_test(&rows, false);
        // Last 8 chars of the ULID.
        assert!(
            t.contains("TJCGJJ59"),
            "rendered table omits short id: {t:?}"
        );
        // Full ULID is NOT rendered by default; the operator opts in via
        // --full-id.
        assert!(
            !t.contains("01KRA25CW1F263ETCNTJCGJJ59"),
            "default render must not carry full ULID: {t:?}"
        );
    }

    #[test]
    fn empty_input_still_renders_header() {
        let t = render_for_test(&[], false);
        assert!(t.contains("HOST ID"));
    }

    #[test]
    fn host_row_decodes_from_dashboard_dto_shape() {
        // The dashboard's HostDto carries extra fields (fingerprint, os,
        // arch, agent/docker versions, last_seen_at, runtime_backend).
        // We use serde's default tolerance: extra fields are ignored.
        let json = r#"{
            "id": "01KRA25CW1F263ETCNTJCGJJ59",
            "fingerprint": "ed25519-fp",
            "hostname": "iso-fresh-1",
            "os": "linux",
            "arch": "x86_64",
            "agent_version": "0.3.0",
            "docker_version": "27.0",
            "enrolled_at": "2026-05-11T01:35:42Z",
            "last_seen_at": "2026-05-11T01:36:00Z",
            "runtime_backend": "docker"
        }"#;
        let row: HostRow = serde_json::from_str(json).unwrap();
        assert_eq!(row.id, "01KRA25CW1F263ETCNTJCGJJ59");
        assert_eq!(row.hostname, "iso-fresh-1");
    }

    #[tokio::test]
    #[ignore]
    async fn list_hits_hosts_endpoint_against_stub() {
        // Spins up a stub controller, writes a temp credentials file
        // pointing at it, then runs `isd hosts ls --json`. The stub
        // verifies one GET /api/v1/hosts arrives and returns a fixture
        // payload. Marked #[ignore] because it sets ISD_CREDENTIALS_FILE
        // (process-global) and binds a TCP port; the same pattern as the
        // secret::tests::global_put_fans_out_to_every_context test.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = serde_json::json!([
            {
                "id": "01KRA25CW1F263ETCNTJCGJJ59",
                "fingerprint": "ed25519-fp",
                "hostname": "iso-fresh-1",
                "os": "linux",
                "arch": "x86_64",
                "agent_version": "0.3.0",
                "docker_version": "27.0",
                "enrolled_at": "2026-05-11T01:35:42Z",
                "last_seen_at": "2026-05-11T01:36:00Z",
                "runtime_backend": "docker"
            }
        ]);
        Mock::given(method("GET"))
            .and(path("/api/v1/hosts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
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

        // SAFETY: matches the secret-test pattern; the test is ignored
        // by default so it won't race with other tests in normal runs.
        unsafe {
            std::env::set_var("ISD_CREDENTIALS_FILE", &creds_path);
        }

        let result = run_list(
            LsArgs {
                format: crate::output::Format::Json,
                full_id: false,
            },
            None,
        )
        .await;

        unsafe {
            std::env::remove_var("ISD_CREDENTIALS_FILE");
        }

        result.expect("hosts list should succeed when the stub returns 200");
    }
}
