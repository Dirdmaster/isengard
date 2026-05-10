//! `isd ps`: list stacks + services across the saved context's controller.
//!
//! Emits a kubectl-style table (default) or JSON (`--json`). Aligns with the
//! dashboard's `/api/v1/stacks` + `/api/v1/services` payloads so the JSON
//! shape is reusable downstream.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::Deserialize;

use crate::session::Session;
use crate::table::{PsRow, render_json, render_table};

#[derive(Debug, Args)]
pub struct PsArgs {
    /// Emit JSON instead of the table. The shape matches [`PsRow`] and is
    /// stable across patch releases.
    #[arg(long)]
    pub json: bool,

    /// Optional fleet filter. Mirrors the dashboard's `?fleet=` query param.
    #[arg(long)]
    pub fleet: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StackDto {
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
struct ServiceDto {
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

/// Subset of `HostDto` we need to render the BACKEND column.
/// `runtime_backend` defaults to "docker" via serde for back-compat
/// with older controllers that don't yet emit the field.
#[derive(Debug, Deserialize)]
struct HostBackendDto {
    id: String,
    #[serde(default = "default_backend")]
    runtime_backend: String,
}

fn default_backend() -> String {
    "docker".to_string()
}

pub async fn run(args: PsArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let mut url = format!("{}/api/v1/stacks", session.controller_url());
    if let Some(f) = args.fleet.as_deref() {
        url.push_str(&format!("?fleet={f}"));
    }
    let stacks_resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;

    let stacks: Vec<StackDto> = stacks_resp
        .error_for_status()
        .context("listing stacks")?
        .json()
        .await
        .context("decoding stacks JSON")?;

    let services_url = format!("{}/api/v1/services", session.controller_url());
    let svc_resp = session
        .client
        .get(&services_url)
        .send()
        .await
        .with_context(|| format!("GET {services_url}"))?;

    let services: Vec<ServiceDto> = svc_resp
        .error_for_status()
        .context("listing services")?
        .json()
        .await
        .context("decoding services JSON")?;

    // Phase 0.5 wisp: pull /api/v1/hosts so we can attach a BACKEND
    // column. Older controllers that don't yet emit `runtime_backend`
    // default to "docker" via serde-default in HostBackendDto.
    let hosts_url = format!("{}/api/v1/hosts", session.controller_url());
    let hosts: Vec<HostBackendDto> = match session.client.get(&hosts_url).send().await {
        Ok(resp) => match resp.error_for_status() {
            Ok(ok) => ok.json().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    };

    let rows = build_rows(&stacks, &services, &hosts);
    if args.json {
        println!("{}", render_json(&rows)?);
    } else {
        let out = render_table(&rows);
        // comfy-table emits a trailing newline only when present in cells;
        // ensure shell composability by appending one.
        println!("{}", out.trim_end());
    }
    Ok(())
}

/// Group services by stack and produce one [`PsRow`] per service. Services
/// with no stack are surfaced under the synthetic `(unstacked)` group so
/// they're not silently dropped.
///
/// Phase 0.5 wisp: each service joins to its host on `host_id` so the
/// row carries the host's runtime backend (`docker` / `wisp`). Hosts
/// the controller doesn't recognise (or never gossiped a backend)
/// fall back to `docker` for the column value.
fn build_rows(
    stacks: &[StackDto],
    services: &[ServiceDto],
    hosts: &[HostBackendDto],
) -> Vec<PsRow> {
    let mut by_id: std::collections::HashMap<&str, &StackDto> = std::collections::HashMap::new();
    for s in stacks {
        by_id.insert(&s.id, s);
    }
    let mut backend_by_host: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::new();
    for h in hosts {
        backend_by_host.insert(h.id.as_str(), h.runtime_backend.as_str());
    }
    let mut rows: Vec<PsRow> = services
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
            PsRow {
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
    // Stable, predictable order: stack name, then service name. Avoids
    // table-row jitter between calls when the controller orders services
    // by id.
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

    fn stack(id: &str, name: &str) -> StackDto {
        StackDto {
            id: id.into(),
            host_id: "h1".into(),
            name: name.into(),
            source: "compose".into(),
            discovered_at: Utc::now(),
        }
    }

    fn service(name: &str, stack_id: Option<&str>) -> ServiceDto {
        ServiceDto {
            id: format!("svc-{name}"),
            host_id: "01HXABCDEF".into(),
            hostname: Some("host-a".into()),
            stack_id: stack_id.map(|s| s.to_string()),
            name: name.into(),
            image: "alpine:3.20".into(),
            state: "running".into(),
            last_seen_at: Utc::now(),
        }
    }

    #[test]
    fn services_with_no_stack_are_grouped_under_unstacked() {
        let stacks = vec![stack("s1", "blog")];
        let services = vec![service("wordpress", Some("s1")), service("orphan", None)];
        let rows = build_rows(&stacks, &services, &[]);
        assert!(rows.iter().any(|r| r.stack == "(unstacked)"));
        assert!(rows.iter().any(|r| r.stack == "blog"));
    }

    #[test]
    fn rows_sorted_by_stack_then_service() {
        let stacks = vec![stack("s1", "alpha"), stack("s2", "zulu")];
        let services = vec![
            service("b-svc", Some("s2")),
            service("a-svc", Some("s2")),
            service("z-svc", Some("s1")),
        ];
        let rows = build_rows(&stacks, &services, &[]);
        assert_eq!(rows[0].stack, "alpha");
        assert_eq!(rows[1].stack, "zulu");
        assert_eq!(rows[1].service, "a-svc");
        assert_eq!(rows[2].service, "b-svc");
    }

    /// Phase 0.5 wisp: each row joins its service.host_id to the
    /// controller's hosts list to pick up the runtime backend. Hosts
    /// the controller doesn't know about fall back to `docker`.
    #[test]
    fn build_rows_joins_backend_per_host() {
        let stacks = vec![stack("s1", "blog")];
        let services = vec![service("wordpress", Some("s1"))];
        let hosts = vec![HostBackendDto {
            id: "01HXABCDEF".into(),
            runtime_backend: "wisp".into(),
        }];
        let rows = build_rows(&stacks, &services, &hosts);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].backend, "wisp");
    }

    #[test]
    fn build_rows_unknown_host_defaults_backend_to_docker() {
        let stacks = vec![stack("s1", "blog")];
        let services = vec![service("wordpress", Some("s1"))];
        // No matching host in the registry: row's backend defaults
        // to "docker" for back-compat with pre-0.5 controllers.
        let rows = build_rows(&stacks, &services, &[]);
        assert_eq!(rows[0].backend, "docker");
    }

    #[test]
    fn humanize_age_buckets_correctly() {
        let now = Utc::now();
        let s = humanize_age(now - chrono::Duration::seconds(5));
        assert!(s.ends_with("s ago"), "{s}");
        let m = humanize_age(now - chrono::Duration::minutes(7));
        assert!(m.ends_with("m ago"), "{m}");
        let h = humanize_age(now - chrono::Duration::hours(3));
        assert!(h.ends_with("h ago"), "{h}");
        let d = humanize_age(now - chrono::Duration::hours(48));
        assert!(d.ends_with("d ago"), "{d}");
    }
}
