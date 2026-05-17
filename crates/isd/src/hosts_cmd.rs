//! `isd hosts list`: enumerate every host enrolled on the controller.
//! Talks to `GET /api/v1/hosts`.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use comfy_table::{ContentArrangement, Table, presets::NOTHING};
use serde::{Deserialize, Serialize};

use crate::session::Session;

#[derive(Debug, Args)]
pub struct HostsArgs {
    #[command(subcommand)]
    pub command: HostsCommand,
}

#[derive(Debug, Subcommand)]
pub enum HostsCommand {
    /// List enrolled hosts.
    List(ListArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = crate::output::Format::Table)]
    pub format: crate::output::Format,
}

/// Subset of the dashboard's `HostDto` we render. Pre-0.5 fields are
/// reused; pre-existing back-compat (e.g. `runtime_backend` default)
/// is enforced server-side, so we just decode whatever lands.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HostRow {
    pub id: String,
    pub hostname: String,
    pub enrolled_at: DateTime<Utc>,
}

pub async fn run(args: HostsArgs, context: Option<&str>) -> Result<()> {
    match args.command {
        HostsCommand::List(a) => run_list(a, context).await,
    }
}

async fn run_list(args: ListArgs, context: Option<&str>) -> Result<()> {
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
        crate::output::Format::Table => {
            let out = render_table(&rows);
            println!("{}", out.trim_end());
        }
    }
    Ok(())
}

/// Render the rows as a kubectl-style ASCII table. Empty input prints
/// just the header so scripts piping through `wc -l` get a stable shape.
fn render_table(rows: &[HostRow]) -> String {
    let mut t = Table::new();
    t.load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec!["HOST ID", "NAME", "ENROLLED"]);
    for row in rows {
        t.add_row(vec![
            row.id.as_str(),
            row.hostname.as_str(),
            // RFC 3339 / ISO 8601 with seconds, no fractional millis;
            // matches the rest of isd's timestamp surface.
            row.enrolled_at
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
                .as_str(),
        ]);
    }
    t.to_string()
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

    #[test]
    fn table_contains_header_and_each_row() {
        let rows = vec![
            host_row("01KRA25CW1F263ETCNTJCGJJ59", "iso-fresh-1"),
            host_row("01KRA25YV3HFJG2EKEKW5KFKY3", "iso-fresh-2"),
        ];
        let t = render_table(&rows);
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
    fn empty_input_still_renders_header() {
        let t = render_table(&[]);
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
        // pointing at it, then runs `isd hosts list --json`. The stub
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
            ListArgs {
                format: crate::output::Format::Json,
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
