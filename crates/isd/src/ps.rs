//! `isd ps`: list stacks + services across the saved context's controller.
//!
//! Emits a kubectl-style table (default) or JSON (`--json`). Aligns with the
//! dashboard's `/api/v1/stacks` + `/api/v1/services` payloads so the JSON
//! shape is reusable downstream.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::Deserialize;

use crate::login::{pinned_session, verify_pinned_response};
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

pub async fn run(args: PsArgs, context: Option<&str>) -> Result<()> {
    let (ctx, client) = pinned_session(context).await?;
    let mut url = format!("{}/api/v1/stacks", ctx.controller_url);
    if let Some(f) = args.fleet.as_deref() {
        url.push_str(&format!("?fleet={f}"));
    }
    let stacks_resp = client
        .get(&url)
        .bearer_auth(&ctx.token)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    verify_pinned_response(&stacks_resp, &ctx.ca_fingerprint_sha256)?;
    let stacks: Vec<StackDto> = stacks_resp
        .error_for_status()
        .context("listing stacks")?
        .json()
        .await
        .context("decoding stacks JSON")?;

    let services_url = format!("{}/api/v1/services", ctx.controller_url);
    let svc_resp = client
        .get(&services_url)
        .bearer_auth(&ctx.token)
        .send()
        .await
        .with_context(|| format!("GET {services_url}"))?;
    verify_pinned_response(&svc_resp, &ctx.ca_fingerprint_sha256)?;
    let services: Vec<ServiceDto> = svc_resp
        .error_for_status()
        .context("listing services")?
        .json()
        .await
        .context("decoding services JSON")?;

    let rows = build_rows(&stacks, &services);
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
fn build_rows(stacks: &[StackDto], services: &[ServiceDto]) -> Vec<PsRow> {
    let mut by_id: std::collections::HashMap<&str, &StackDto> = std::collections::HashMap::new();
    for s in stacks {
        by_id.insert(&s.id, s);
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
            PsRow {
                stack: stack_name.to_string(),
                service: svc.name.clone(),
                host: host_label,
                state: svc.state.clone(),
                image: svc.image.clone(),
                last_seen: humanize_age(svc.last_seen_at),
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
        let rows = build_rows(&stacks, &services);
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
        let rows = build_rows(&stacks, &services);
        assert_eq!(rows[0].stack, "alpha");
        assert_eq!(rows[1].stack, "zulu");
        assert_eq!(rows[1].service, "a-svc");
        assert_eq!(rows[2].service, "b-svc");
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
