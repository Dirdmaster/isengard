//! `isd service ls`: list every service across every stack.
//! Talks to `GET /api/v1/services`.

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use comfy_table::{ContentArrangement, Table, presets::NOTHING};
use serde::{Deserialize, Serialize};

use crate::session::Session;
use crate::stack_cmd::{ServiceApiRow, StackApiRow};

#[derive(Debug, Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub command: ServiceCommand,
}

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// List services across stacks.
    Ls(LsArgs),
}

#[derive(Debug, Args)]
pub struct LsArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = crate::output::Format::Table)]
    pub format: crate::output::Format,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceLsRow {
    pub stack: Option<String>,
    pub name: String,
    pub host: String,
    pub state: String,
    pub image: String,
    pub last_seen_at: DateTime<Utc>,
}

pub async fn run(args: ServiceArgs, context: Option<&str>) -> Result<()> {
    match args.command {
        ServiceCommand::Ls(a) => run_ls(a, context).await,
    }
}

async fn run_ls(args: LsArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;

    let stacks: Vec<StackApiRow> = fetch(
        &session,
        &format!("{}/api/v1/stacks", session.controller_url()),
    )
    .await?;
    let services: Vec<ServiceApiRow> = fetch(
        &session,
        &format!("{}/api/v1/services", session.controller_url()),
    )
    .await?;

    let rows = build_rows(&stacks, &services);

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

fn render_table(rows: &[ServiceLsRow]) -> String {
    let mut t = Table::new();
    t.load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec![
            "STACK",
            "SERVICE",
            "HOST",
            "STATE",
            "IMAGE",
            "LAST SEEN",
        ]);
    for row in rows {
        t.add_row(vec![
            row.stack.clone().unwrap_or_else(|| "-".into()),
            row.name.clone(),
            row.host.clone(),
            row.state.clone(),
            row.image.clone(),
            row.last_seen_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
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
        let t = render_table(&rows);
        assert!(t.contains("STACK"));
        assert!(t.contains("SERVICE"));
        assert!(t.contains("orphan"));
        // The dash placeholder appears in the STACK cell.
        assert!(t.lines().any(|l| l.contains("orphan") && l.contains("-")));
    }
}
