//! `isd route list` / `isd route create` / `isd route rm` (operator surface
//! for the controller's routing rules).
//!
//! Talks to the dashboard's `/api/v1/routing/rules[/<id>]` endpoints.
//! Routing rules are normally created automatically when a stack's
//! compose.yaml declares `expose.host`; this CLI is for the cases that
//! aren't a managed stack (e.g., routing the controller dashboard itself
//! through Pingora) or for operators who prefer the imperative path.

use anyhow::{Context as _, Result, anyhow};
use clap::{Args, Subcommand};
use comfy_table::{ContentArrangement, Table, presets::NOTHING};
use serde::{Deserialize, Serialize};

use crate::session::Session;

#[derive(Debug, Args)]
pub struct RouteArgs {
    #[command(subcommand)]
    pub command: RouteCommand,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)] // CreateArgs is large but only one is alive at a time
pub enum RouteCommand {
    /// List every routing rule across the fleet.
    List,
    /// Create a routing rule. Defaults: fleet="default", protocol="http",
    /// adapter="none", tls-mode="acme". Override via flags.
    Create(CreateArgs),
    /// Delete a routing rule by id (the integer printed by `list`).
    Rm(RmArgs),
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Public hostname the rule matches against (Pingora SNI / Host header),
    /// e.g. `iso.vallee.casa`. Wildcards are not allowed in this field; an
    /// `*.example.com` cert covers any single-label subdomain when present.
    pub public_hostname: String,
    /// Agent serving the upstream. Defaults to the only enrolled agent in
    /// the fleet (the homelab single-host case). Pass either `--host-id`
    /// (ULID) or `--host` (hostname) when more than one agent exists.
    #[arg(long, conflicts_with = "host")]
    pub host_id: Option<String>,
    /// Agent hostname (resolved client-side to a host_id). Mutually
    /// exclusive with `--host-id`.
    #[arg(long, conflicts_with = "host_id")]
    pub host: Option<String>,
    /// Upstream container hostname (DNS name resolvable on the agent's
    /// docker network) or service name. e.g. `iso-controller` to point at
    /// the controller's dashboard, `nginx` for a stack service named nginx.
    #[arg(long)]
    pub service: String,
    /// Upstream port on the container.
    #[arg(long)]
    pub port: u16,
    /// Fleet scope. Most installs run a single fleet.
    #[arg(long, default_value = "default")]
    pub fleet: String,
    /// Upstream protocol. `http` is correct when the proxy terminates TLS
    /// and the upstream serves plain HTTP (the common homelab case).
    #[arg(long, default_value = "http")]
    pub protocol: String,
    /// Networking adapter. `none` is direct docker-network routing, no
    /// tunnel. Other adapters: `tailscale`, `cf-tunnel`.
    #[arg(long, default_value = "none")]
    pub adapter: String,
    /// TLS termination mode at the proxy edge. `acme` uses a Let's Encrypt
    /// cert (wildcard or per-host). `edge` means TLS is already terminated
    /// upstream of the proxy. `manual` reads from `tls_certs`.
    #[arg(long, default_value = "acme")]
    pub tls_mode: String,
    /// Optional healthcheck path; the proxy probes this on the upstream.
    #[arg(long)]
    pub healthcheck_path: Option<String>,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Routing rule id (the integer column from `isd route list`).
    pub id: i64,
}

#[derive(Debug, Serialize)]
struct CreateBody<'a> {
    fleet: &'a str,
    host_id: &'a str,
    service_name: &'a str,
    container_port: u16,
    public_hostname: &'a str,
    protocol: &'a str,
    adapter: &'a str,
    tls_mode: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    healthcheck_path: Option<&'a str>,
    /// "ui" so the rule is operator-tagged in the source column. The
    /// controller treats this as informational.
    source: &'a str,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // host_id/protocol/adapter not in default table view, kept for JSON parity
struct RoutingRuleEntry {
    id: i64,
    public_hostname: String,
    service_name: String,
    container_port: u16,
    host_id: String,
    fleet: String,
    protocol: String,
    adapter: String,
    tls_mode: String,
    state: String,
    source: String,
}

/// Subset of `HostDto` we care about for host_id resolution.
#[derive(Debug, Clone, Deserialize)]
struct HostEntry {
    id: String,
    hostname: String,
    fleet: String,
}

pub async fn run(args: RouteArgs, context: Option<&str>) -> Result<()> {
    match args.command {
        RouteCommand::List => run_list(context).await,
        RouteCommand::Create(a) => run_create(a, context).await,
        RouteCommand::Rm(a) => run_rm(a, context).await,
    }
}

async fn run_list(context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let entries = list_rules(&session).await?;
    if entries.is_empty() {
        println!("No routing rules.");
        return Ok(());
    }
    let mut table = Table::new();
    table
        .load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec![
            "ID", "HOSTNAME", "UPSTREAM", "PORT", "TLS", "STATE", "SRC", "FLEET",
        ]);
    for e in &entries {
        table.add_row(vec![
            e.id.to_string(),
            e.public_hostname.clone(),
            e.service_name.clone(),
            e.container_port.to_string(),
            e.tls_mode.clone(),
            e.state.clone(),
            e.source.clone(),
            e.fleet.clone(),
        ]);
    }
    println!("{table}");
    Ok(())
}

async fn run_create(args: CreateArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let host_id = resolve_host_id(
        &session,
        args.host_id.as_deref(),
        args.host.as_deref(),
        &args.fleet,
    )
    .await?;
    let body = CreateBody {
        fleet: &args.fleet,
        host_id: &host_id,
        service_name: &args.service,
        container_port: args.port,
        public_hostname: &args.public_hostname,
        protocol: &args.protocol,
        adapter: &args.adapter,
        tls_mode: &args.tls_mode,
        healthcheck_path: args.healthcheck_path.as_deref(),
        source: "ui",
    };
    let id = create_rule(&session, &body).await?;
    println!("Created routing rule id={id} for {}.", args.public_hostname);
    Ok(())
}

/// Resolve the agent's host_id with this priority:
///   1. `--host-id <ulid>` -> use literally
///   2. `--host <hostname>` -> look up the matching agent in the fleet
///   3. neither -> if exactly one agent in the fleet, use it; otherwise
///      error with a numbered list so the operator can re-invoke with
///      `--host` or `--host-id`.
async fn resolve_host_id(
    session: &Session,
    host_id: Option<&str>,
    host: Option<&str>,
    fleet: &str,
) -> Result<String> {
    if let Some(id) = host_id {
        return Ok(id.to_string());
    }
    let hosts = list_hosts(session, fleet).await?;
    if let Some(name) = host {
        let lc = name.to_lowercase();
        let matches: Vec<&HostEntry> = hosts
            .iter()
            .filter(|h| h.hostname.eq_ignore_ascii_case(&lc))
            .collect();
        return match matches.len() {
            0 => Err(anyhow!(
                "no agent in fleet {fleet:?} with hostname {name:?}; \
                 known agents:\n{}",
                hosts_table(&hosts),
            )),
            1 => Ok(matches[0].id.clone()),
            _ => {
                let owned: Vec<HostEntry> = matches.into_iter().cloned().collect();
                Err(anyhow!(
                    "more than one agent in fleet {fleet:?} matches hostname \
                     {name:?}; pass --host-id <ulid> instead. matches:\n{}",
                    hosts_table(&owned),
                ))
            }
        };
    }
    match hosts.len() {
        0 => Err(anyhow!(
            "no agents enrolled in fleet {fleet:?}; enroll one before creating routes"
        )),
        1 => Ok(hosts[0].id.clone()),
        _ => Err(anyhow!(
            "more than one agent in fleet {fleet:?}; pass --host <hostname> or \
             --host-id <ulid>. enrolled agents:\n{}",
            hosts_table(&hosts),
        )),
    }
}

fn hosts_table(hosts: &[HostEntry]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for h in hosts {
        let _ = writeln!(&mut s, "  {}  {}  fleet={}", h.id, h.hostname, h.fleet);
    }
    s
}

async fn list_hosts(session: &Session, fleet: &str) -> Result<Vec<HostEntry>> {
    let url = format!("{}/api/v1/hosts?fleet={fleet}", session.controller_url());
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let entries: Vec<HostEntry> = resp.error_for_status()?.json().await?;
    Ok(entries)
}

async fn run_rm(args: RmArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    delete_rule(&session, args.id).await?;
    println!("Deleted routing rule id={}.", args.id);
    Ok(())
}

async fn list_rules(session: &Session) -> Result<Vec<RoutingRuleEntry>> {
    let url = format!("{}/api/v1/routing/rules", session.controller_url());
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let entries: Vec<RoutingRuleEntry> = resp.error_for_status()?.json().await?;
    Ok(entries)
}

async fn create_rule(session: &Session, body: &CreateBody<'_>) -> Result<i64> {
    let url = format!("{}/api/v1/routing/rules", session.controller_url());
    let resp = session
        .client
        .post(&url)
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        return Err(anyhow!("POST {url} -> {status}: {txt}"));
    }
    let entry: RoutingRuleEntry = resp.json().await?;
    Ok(entry.id)
}

async fn delete_rule(session: &Session, id: i64) -> Result<()> {
    let url = format!("{}/api/v1/routing/rules/{id}", session.controller_url());
    let resp = session
        .client
        .delete(&url)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(anyhow!("routing rule id={id} not found"));
    }
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(anyhow!("DELETE {url} -> {status}: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_args_minimum_required_no_host() {
        // The common homelab case: one agent, no --host-id needed.
        // resolve_host_id fills it in at runtime.
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: RouteCommand,
        }
        let w = Wrap::try_parse_from([
            "x",
            "create",
            "iso.vallee.casa",
            "--service",
            "iso-controller",
            "--port",
            "9418",
        ])
        .unwrap();
        match w.c {
            RouteCommand::Create(a) => {
                assert_eq!(a.public_hostname, "iso.vallee.casa");
                assert!(a.host_id.is_none());
                assert!(a.host.is_none());
                assert_eq!(a.service, "iso-controller");
                assert_eq!(a.port, 9418);
                assert_eq!(a.fleet, "default");
                assert_eq!(a.protocol, "http");
                assert_eq!(a.adapter, "none");
                assert_eq!(a.tls_mode, "acme");
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn create_args_with_explicit_host_id() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: RouteCommand,
        }
        let w = Wrap::try_parse_from([
            "x",
            "create",
            "iso.vallee.casa",
            "--host-id",
            "01H000000000000000000000",
            "--service",
            "iso-controller",
            "--port",
            "9418",
        ])
        .unwrap();
        match w.c {
            RouteCommand::Create(a) => {
                assert_eq!(a.host_id.as_deref(), Some("01H000000000000000000000"));
                assert!(a.host.is_none());
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn create_args_host_and_host_id_conflict() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: RouteCommand,
        }
        let res = Wrap::try_parse_from([
            "x",
            "create",
            "iso.vallee.casa",
            "--host-id",
            "01H000000000000000000000",
            "--host",
            "indra",
            "--service",
            "x",
            "--port",
            "1",
        ]);
        assert!(res.is_err(), "host_id + host should conflict");
    }

    #[test]
    fn rm_args_parse() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: RouteCommand,
        }
        let w = Wrap::try_parse_from(["x", "rm", "42"]).unwrap();
        match w.c {
            RouteCommand::Rm(a) => assert_eq!(a.id, 42),
            other => panic!("expected Rm, got {other:?}"),
        }
    }
}
