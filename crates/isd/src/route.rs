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
    /// List routing rules.
    List,
    /// Create a routing rule.
    Create(CreateArgs),
    /// Delete a routing rule by id.
    Rm(RmArgs),
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Public hostname the rule matches. Prompted when omitted.
    pub public_hostname: Option<String>,
    /// Agent serving the upstream (ULID).
    #[arg(long, conflicts_with = "host")]
    pub host_id: Option<String>,
    /// Agent hostname (resolved to host_id).
    #[arg(long, conflicts_with = "host_id")]
    pub host: Option<String>,
    /// Upstream container or service name. Prompted via a fuzzy
    /// picker over the running containers when omitted.
    #[arg(long)]
    pub service: Option<String>,
    /// Upstream port. Prompted when omitted; default is the
    /// container's sole private port when it has exactly one.
    #[arg(long)]
    pub port: Option<u16>,
    /// Upstream protocol (http or https).
    #[arg(long, default_value = "http")]
    pub protocol: String,
    /// Networking adapter (none, tailscale, cf-tunnel).
    #[arg(long, default_value = "none")]
    pub adapter: String,
    /// TLS termination mode (acme, edge, manual).
    #[arg(long, default_value = "acme")]
    pub tls_mode: String,
    /// Healthcheck path on the upstream.
    #[arg(long)]
    pub healthcheck_path: Option<String>,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Routing rule id.
    pub id: i64,
}

#[derive(Debug, Serialize)]
struct CreateBody<'a> {
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
            "ID", "HOSTNAME", "UPSTREAM", "PORT", "TLS", "STATE", "SRC",
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
        ]);
    }
    println!("{table}");
    Ok(())
}

async fn run_create(args: CreateArgs, context: Option<&str>) -> Result<()> {
    let args = if args.service.is_none() || args.port.is_none() || args.public_hostname.is_none() {
        run_wizard(args, context).await?
    } else {
        args
    };

    let public_hostname = args
        .public_hostname
        .as_deref()
        .expect("public_hostname populated by wizard or CLI");
    let service = args
        .service
        .as_deref()
        .expect("service populated by wizard or CLI");
    let port = args.port.expect("port populated by wizard or CLI");

    let session = Session::open(context).await?;
    let host_id = resolve_host_id(&session, args.host_id.as_deref(), args.host.as_deref()).await?;
    let body = CreateBody {
        host_id: &host_id,
        service_name: service,
        container_port: port,
        public_hostname,
        protocol: &args.protocol,
        adapter: &args.adapter,
        tls_mode: &args.tls_mode,
        healthcheck_path: args.healthcheck_path.as_deref(),
        source: "ui",
    };
    let id = create_rule(&session, &body).await?;
    println!("Created routing rule id={id} for {public_hostname}.");
    Ok(())
}

/// Interactive wizard. Lists running containers on the resolved docker
/// context, lets the operator pick one via a fuzzy-substring filter,
/// then prompts for the missing port (auto-defaulting when the chosen
/// container has exactly one private port) and the public hostname.
/// Already-supplied fields pass through unchanged.
async fn run_wizard(mut args: CreateArgs, context: Option<&str>) -> Result<CreateArgs> {
    let docker_uri = crate::docker_context::resolve_docker_uri(context)?;
    let docker = isd_runtime::DockerBackend::from_uri(&docker_uri).await?;
    let raw = docker.list_containers(false).await?;

    // Hide controller/agent so operators don't accidentally route to
    // the system plane.
    let candidates: Vec<isd_runtime::ContainerSummary> = raw
        .into_iter()
        .filter(|c| {
            !matches!(
                c.labels.get("io.isengard.role").map(String::as_str),
                Some("controller") | Some("agent")
            )
        })
        .collect();

    if candidates.is_empty() {
        return Err(anyhow!(
            "no workload containers found on {docker_uri}; \
             start one and try again, or pass --service / --port to skip the picker"
        ));
    }

    let rows: Vec<ContainerRow> = candidates.into_iter().map(ContainerRow::from).collect();

    if args.service.is_none() {
        let picked = inquire::Select::new("Pick the upstream container", rows.clone())
            .with_help_message("type to filter")
            .with_page_size(15)
            .prompt()
            .map_err(|e| anyhow!("picker cancelled: {e}"))?;
        args.service = Some(picked.service_name.clone());
        if args.port.is_none() {
            args.port = Some(prompt_port(&picked)?);
        }
    } else if args.port.is_none() {
        // --service was supplied but --port wasn't. Try to match the
        // service name against the docker view so we can still default
        // the port; fall back to a plain prompt if nothing matches.
        let svc = args.service.as_deref().unwrap();
        let matched = rows.iter().find(|r| r.service_name == svc);
        args.port = Some(match matched {
            Some(row) => prompt_port(row)?,
            None => inquire::CustomType::<u16>::new("Upstream port")
                .with_error_message("ports are 1..=65535")
                .prompt()
                .map_err(|e| anyhow!("port prompt cancelled: {e}"))?,
        });
    }

    if args.public_hostname.is_none() {
        let hostname = inquire::Text::new("Public hostname")
            .with_help_message("e.g. plex.vallee.casa")
            .with_validator(|input: &str| {
                if input.trim().is_empty() {
                    Ok(inquire::validator::Validation::Invalid(
                        "hostname can't be empty".into(),
                    ))
                } else {
                    Ok(inquire::validator::Validation::Valid)
                }
            })
            .prompt()
            .map_err(|e| anyhow!("hostname prompt cancelled: {e}"))?;
        args.public_hostname = Some(hostname.trim().to_string());
    }

    Ok(args)
}

/// Prompt for the upstream port. When the picked container has
/// exactly one distinct private port, pre-fill it as the default so
/// the operator can just hit Enter.
fn prompt_port(row: &ContainerRow) -> Result<u16> {
    let mut prompt =
        inquire::CustomType::<u16>::new("Upstream port").with_error_message("ports are 1..=65535");
    if let [only] = row.private_ports.as_slice() {
        prompt = prompt
            .with_default(*only)
            .with_help_message("press Enter to accept the container's exposed port");
    } else if !row.private_ports.is_empty() {
        // Multiple candidates: surface them so the operator picks
        // knowingly. Pre-filling would be a coin flip.
        let listed = row
            .private_ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        prompt = prompt.with_help_message(Box::leak(
            format!("container exposes: {listed}").into_boxed_str(),
        ));
    }
    prompt
        .prompt()
        .map_err(|e| anyhow!("port prompt cancelled: {e}"))
}

/// Display row for the inquire `Select`. Carries both the displayed
/// label and the structured fields the wizard needs after the user
/// picks a row.
#[derive(Clone)]
struct ContainerRow {
    service_name: String,
    image: String,
    private_ports: Vec<u16>,
}

impl From<isd_runtime::ContainerSummary> for ContainerRow {
    fn from(c: isd_runtime::ContainerSummary) -> Self {
        // Prefer the compose-service label when present; falls back
        // to the container name so non-compose containers still show
        // up usefully.
        let service_name = c
            .labels
            .get("com.docker.compose.service")
            .cloned()
            .unwrap_or_else(|| c.names.clone());
        Self {
            service_name,
            image: c.image,
            private_ports: c.private_ports,
        }
    }
}

impl std::fmt::Display for ContainerRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Two columns: service-name (left, fixed-ish) + image (right,
        // dim). inquire renders this whole line in one selection row.
        write!(f, "{:<22} {}", self.service_name, self.image)
    }
}

/// Resolve the agent's host_id with this priority:
///   1. `--host-id <ulid>` -> use literally
///   2. `--host <hostname>` -> look up the matching agent
///   3. neither -> if exactly one agent is enrolled, use it; otherwise
///      error with a numbered list so the operator can re-invoke with
///      `--host` or `--host-id`.
async fn resolve_host_id(
    session: &Session,
    host_id: Option<&str>,
    host: Option<&str>,
) -> Result<String> {
    if let Some(id) = host_id {
        return Ok(id.to_string());
    }
    let hosts = list_hosts(session).await?;
    if let Some(name) = host {
        let lc = name.to_lowercase();
        let matches: Vec<&HostEntry> = hosts
            .iter()
            .filter(|h| h.hostname.eq_ignore_ascii_case(&lc))
            .collect();
        return match matches.len() {
            0 => Err(anyhow!(
                "no agent with hostname {name:?}; known agents:\n{}",
                hosts_table(&hosts),
            )),
            1 => Ok(matches[0].id.clone()),
            _ => {
                let owned: Vec<HostEntry> = matches.into_iter().cloned().collect();
                Err(anyhow!(
                    "more than one agent matches hostname {name:?}; pass \
                     --host-id <ulid> instead. matches:\n{}",
                    hosts_table(&owned),
                ))
            }
        };
    }
    match hosts.len() {
        0 => Err(anyhow!(
            "no agents enrolled; enroll one before creating routes"
        )),
        1 => Ok(hosts[0].id.clone()),
        _ => Err(anyhow!(
            "more than one agent; pass --host <hostname> or \
             --host-id <ulid>. enrolled agents:\n{}",
            hosts_table(&hosts),
        )),
    }
}

fn hosts_table(hosts: &[HostEntry]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for h in hosts {
        let _ = writeln!(&mut s, "  {}  {}", h.id, h.hostname);
    }
    s
}

async fn list_hosts(session: &Session) -> Result<Vec<HostEntry>> {
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/hosts");
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
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/routing/rules");
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
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/routing/rules");
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
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/routing/rules/{id}");
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
                assert_eq!(a.public_hostname.as_deref(), Some("iso.vallee.casa"));
                assert!(a.host_id.is_none());
                assert!(a.host.is_none());
                assert_eq!(a.service.as_deref(), Some("iso-controller"));
                assert_eq!(a.port, Some(9418));
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
