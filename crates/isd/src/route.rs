//! `isd route ls` / `isd route add` / `isd route rm` (operator surface
//! for the controller's routing rules).
//!
//! Talks to the dashboard's `/api/v1/routing/rules[/<id>]` endpoints.
//! Routing rules are normally created automatically when a stack's
//! compose.yaml declares `expose.host`; this CLI is for the cases that
//! aren't a managed stack (e.g., routing the controller dashboard itself
//! through Pingora) or for operators who prefer the imperative path.

use crate::render::{Align, CellStyle, Column, Table, render, render_plain};
use anyhow::{Context as _, Result, anyhow};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::session::Session;

/// CLI flags for `isd route`.
#[derive(Debug, Args)]
pub struct RouteArgs {
    /// Resolved sub-verb.
    #[command(subcommand)]
    pub command: RouteCommand,
}

/// Sub-verbs under `isd route`.
#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)] // AddArgs is large but only one is alive at a time
pub enum RouteCommand {
    /// List routing rules.
    Ls,
    /// Add a routing rule.
    Add(CreateArgs),
    /// Delete a routing rule by id.
    Rm(RmArgs),
}

/// CLI flags for `isd route add`.
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
    /// Upstream container or service name (prompts via picker when omitted).
    ///
    /// Opens a fuzzy picker over the running containers when omitted.
    #[arg(long)]
    pub service: Option<String>,
    /// Upstream port (auto-detected from exposed ports when omitted).
    ///
    /// Auto-detection: single port wins; if multiple, the first common
    /// HTTP-ish port wins; else docker's first reported port.
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

/// CLI flags for `isd route rm`.
#[derive(Debug, Args)]
pub struct RmArgs {
    /// Routing rule id.
    pub id: i64,
}

/// POST body shape for `/api/v1/routing/rules`.
#[derive(Debug, Serialize)]
struct CreateBody<'a> {
    /// Agent ULID serving the upstream.
    host_id: &'a str,
    /// Upstream container or compose service name.
    service_name: &'a str,
    /// Upstream port.
    container_port: u16,
    /// Public hostname the rule matches.
    public_hostname: &'a str,
    /// Upstream protocol (`http` / `https`).
    protocol: &'a str,
    /// Networking adapter (`none`, `tailscale`, `cf-tunnel`).
    adapter: &'a str,
    /// TLS termination mode (`acme`, `edge`, `manual`).
    tls_mode: &'a str,
    /// Optional healthcheck path on the upstream.
    #[serde(skip_serializing_if = "Option::is_none")]
    healthcheck_path: Option<&'a str>,
    /// "ui" so the rule is operator-tagged in the source column. The
    /// controller treats this as informational.
    source: &'a str,
}

/// Subset of the dashboard's routing-rule DTO we decode.
#[derive(Debug, Deserialize)]
#[allow(dead_code)] // host_id/protocol/adapter not in default table view, kept for JSON parity
struct RoutingRuleEntry {
    /// Surrogate key.
    id: i64,
    /// Public hostname.
    public_hostname: String,
    /// Upstream container / service name.
    service_name: String,
    /// Upstream port.
    container_port: u16,
    /// Agent ULID.
    host_id: String,
    /// Upstream protocol.
    protocol: String,
    /// Networking adapter.
    adapter: String,
    /// TLS mode.
    tls_mode: String,
    /// Operational state.
    state: String,
    /// Origin tag (`compose`, `ui`, ...).
    source: String,
}

/// Subset of `HostDto` we care about for host_id resolution.
#[derive(Debug, Clone, Deserialize)]
struct HostEntry {
    /// Agent ULID.
    id: String,
    /// Reported hostname.
    hostname: String,
}

/// Dispatch to the matching `route` sub-verb.
///
/// # Errors
///
/// Propagates the sub-verb's error.
pub async fn run(args: RouteArgs, context: Option<&str>) -> Result<()> {
    match args.command {
        RouteCommand::Ls => run_list(context).await,
        RouteCommand::Add(a) => run_create(a, context).await,
        RouteCommand::Rm(a) => run_rm(a, context).await,
    }
}

/// Fetch the controller's routing rules and print them as a table.
async fn run_list(context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let entries = list_rules(&session).await?;
    if entries.is_empty() {
        println!("No routing rules.");
        return Ok(());
    }
    let table = Table {
        columns: vec![
            Column::new("#", Align::Right, CellStyle::Dim, 9, 1),
            Column::new("ID", Align::Right, CellStyle::Dim, 8, 2),
            Column::new("HOSTNAME", Align::Left, CellStyle::Emphasis, 1, 12),
            Column::new("UPSTREAM", Align::Left, CellStyle::Cyan, 4, 10),
            Column::new("PORT", Align::Right, CellStyle::Plain, 7, 4),
            Column::new("TLS", Align::Left, CellStyle::Plain, 6, 3),
            Column::new("STATE", Align::Left, CellStyle::State, 5, 5),
            Column::new("SRC", Align::Left, CellStyle::Dim, 3, 3),
        ],
        rows: entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                vec![
                    i.to_string(),
                    e.id.to_string(),
                    e.public_hostname.clone(),
                    e.service_name.clone(),
                    e.container_port.to_string(),
                    e.tls_mode.clone(),
                    e.state.clone(),
                    e.source.clone(),
                ]
            })
            .collect(),
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

/// Create a routing rule. Runs the wizard when any required field is
/// missing, then POSTs the resolved body to the controller.
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
/// auto-detects the upstream port from the container's exposed ports,
/// then prompts for the public hostname. Already-supplied fields pass
/// through unchanged; pass `--port` explicitly to override the
/// auto-detected choice.
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

    let picked_row = if args.service.is_none() {
        let picked = inquire::Select::new("Pick the upstream container", rows.clone())
            .with_help_message("type to filter")
            .with_page_size(15)
            .prompt()
            .map_err(|e| anyhow!("picker cancelled: {e}"))?;
        args.service = Some(picked.service_name.clone());
        Some(picked)
    } else {
        // --service supplied but no --port: still need the container's
        // port info to auto-detect. Match the service name against
        // the docker view.
        let svc = args.service.as_deref().unwrap();
        rows.iter().find(|r| r.service_name == svc).cloned()
    };

    if args.port.is_none() {
        let mut row = picked_row.as_ref().cloned().ok_or_else(|| {
            anyhow!(
                "service {:?} not found among running containers on {docker_uri}; \
                 pass --port explicitly or pick from the wizard",
                args.service.as_deref().unwrap_or("")
            )
        })?;
        let container_ref = if row.id.is_empty() {
            row.service_name.as_str()
        } else {
            row.id.as_str()
        };
        if row.private_ports.is_empty() {
            row.private_ports = inspect_private_ports(&docker, container_ref).await?;
        }
        let env_hint = if row.private_ports.len() > 1 {
            inspect_env_port_hint(&docker, container_ref, &row.private_ports).await?
        } else {
            None
        };
        let port = env_hint
            .or_else(|| auto_detect_port(&row.private_ports))
            .ok_or_else(|| {
                anyhow!(
                    "container {:?} exposes no ports; pass --port explicitly",
                    row.service_name
                )
            })?;
        eprintln!("  using upstream port {port}");
        args.port = Some(port);
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

/// Pick the most likely upstream HTTP port from a container's exposed
/// private ports.
///
/// Heuristic:
///   1. Exactly one port -> use it.
///   2. Otherwise, prefer the first port that matches a common HTTP
///      vocabulary (80, 443, 3000, 5000, 8000, 8080, 8081, 8443, 9000).
///   3. Otherwise, fall back to the first port docker reported.
///   4. Empty -> None.
///
/// Real-world calibration for the typical homelab containers:
///   - plex (32400)            -> 32400 (single)
///   - radarr/sonarr/etc       -> single port, used directly
///   - qbittorrent (8080, 6881)-> 8080 (common-HTTP wins over 6881 BT)
///   - flaresolverr (8191, 8192) -> 8191 (neither common; first wins)
fn auto_detect_port(ports: &[u16]) -> Option<u16> {
    if let [only] = ports {
        return Some(*only);
    }
    const COMMON_HTTP: &[u16] = &[80, 443, 3000, 5000, 8000, 8080, 8081, 8443, 9000];
    ports
        .iter()
        .copied()
        .find(|p| COMMON_HTTP.contains(p))
        .or_else(|| ports.first().copied())
}

/// Inspect the selected container when Docker's list summary omitted
/// ports. Some images, including host-networked services, can still
/// declare exposed ports in the inspect payload.
async fn inspect_private_ports(
    docker: &isd_runtime::DockerBackend,
    container_ref: &str,
) -> Result<Vec<u16>> {
    use bollard::container::InspectContainerOptions;

    let inspect = docker
        .client()
        .inspect_container(container_ref, None::<InspectContainerOptions>)
        .await
        .with_context(|| format!("inspect container {container_ref:?}"))?;
    Ok(private_ports_from_inspect(&inspect))
}

async fn inspect_env_port_hint(
    docker: &isd_runtime::DockerBackend,
    container_ref: &str,
    candidates: &[u16],
) -> Result<Option<u16>> {
    use bollard::container::InspectContainerOptions;

    let inspect = docker
        .client()
        .inspect_container(container_ref, None::<InspectContainerOptions>)
        .await
        .with_context(|| format!("inspect container {container_ref:?}"))?;
    Ok(env_port_hint_from_inspect(&inspect, candidates))
}

fn env_port_hint_from_inspect(
    inspect: &bollard::models::ContainerInspectResponse,
    candidates: &[u16],
) -> Option<u16> {
    let env = inspect.config.as_ref()?.env.as_ref()?;
    env.iter()
        .find_map(|entry| env_port_hint(entry, candidates))
}

fn env_port_hint(entry: &str, candidates: &[u16]) -> Option<u16> {
    let (key, value) = entry.split_once('=')?;
    const WEB_PORT_KEYS: &[&str] = &["WEBUI_PORT", "WEB_UI_PORT", "WEB_PORT", "HTTP_PORT"];
    if !WEB_PORT_KEYS.contains(&key) {
        return None;
    }
    let port = value.parse::<u16>().ok()?;
    candidates.contains(&port).then_some(port)
}

/// Extract distinct container-internal ports from an inspect response.
/// Prefer explicit host bindings, then fall back to image-declared
/// exposed ports for host-networked containers whose list summary is empty.
fn private_ports_from_inspect(inspect: &bollard::models::ContainerInspectResponse) -> Vec<u16> {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut push_key = |key: &str| {
        let port = key.split('/').next().and_then(|s| s.parse::<u16>().ok());
        if let Some(port) = port
            && seen.insert(port)
        {
            out.push(port);
        }
    };

    if let Some(bindings) = inspect
        .host_config
        .as_ref()
        .and_then(|host_config| host_config.port_bindings.as_ref())
    {
        let mut keys: Vec<&String> = bindings.keys().collect();
        keys.sort();
        for key in keys {
            push_key(key);
        }
    }

    if let Some(exposed) = inspect
        .config
        .as_ref()
        .and_then(|config| config.exposed_ports.as_ref())
    {
        let mut keys: Vec<&String> = exposed.keys().collect();
        keys.sort();
        for key in keys {
            push_key(key);
        }
    }

    out
}

/// Display row for the inquire `Select`. Carries both the displayed
/// label and the structured fields the wizard needs after the user
/// picks a row.
#[derive(Clone)]
struct ContainerRow {
    /// Full runtime id. Used as the stable inspect target when list
    /// summaries omit private ports.
    id: String,
    /// Compose service name (preferred) or container name fallback.
    service_name: String,
    /// Container image string for the right-hand label.
    image: String,
    /// Ports the container exposes; fed to [`auto_detect_port`].
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
            id: c.id,
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

/// Render the enrolled-hosts list as a compact `id  hostname` block.
/// Used in the multi-match / no-match error messages from
/// [`resolve_host_id`].
fn hosts_table(hosts: &[HostEntry]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for h in hosts {
        let _ = writeln!(&mut s, "  {}  {}", h.id, h.hostname);
    }
    s
}

/// Fetch the controller's enrolled-hosts list.
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

/// Delete a routing rule by id.
async fn run_rm(args: RmArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    delete_rule(&session, args.id).await?;
    println!("Deleted routing rule id={}.", args.id);
    Ok(())
}

/// `GET /api/v1/routing/rules`.
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

/// `POST /api/v1/routing/rules` and return the created row's id.
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

/// `DELETE /api/v1/routing/rules/<id>` with 404 surfaced as an
/// actionable "not found" error.
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
    fn auto_detect_port_single() {
        assert_eq!(auto_detect_port(&[32400]), Some(32400));
        assert_eq!(auto_detect_port(&[7878]), Some(7878));
    }

    #[test]
    fn auto_detect_port_prefers_common_http() {
        // qbittorrent-shape: web UI on 8080, BT on 6881. We want 8080
        // regardless of docker's enumeration order.
        assert_eq!(auto_detect_port(&[6881, 8080]), Some(8080));
        assert_eq!(auto_detect_port(&[8080, 6881]), Some(8080));
        // Common ports beat arbitrary-app ports.
        assert_eq!(auto_detect_port(&[9091, 80]), Some(80));
        assert_eq!(auto_detect_port(&[12345, 3000]), Some(3000));
    }

    #[test]
    fn auto_detect_port_falls_back_to_first_when_no_common_match() {
        // flaresolverr-shape: 8191 API, 8192 metrics. Neither in the
        // common list; docker reports 8191 first, that wins.
        assert_eq!(auto_detect_port(&[8191, 8192]), Some(8191));
        assert_eq!(auto_detect_port(&[55555, 44444]), Some(55555));
    }

    #[test]
    fn auto_detect_port_empty_is_none() {
        assert_eq!(auto_detect_port(&[]), None);
    }

    #[test]
    fn inspect_ports_reads_host_config_bindings() {
        use std::collections::HashMap;

        let mut port_bindings = HashMap::new();
        port_bindings.insert(
            "32400/tcp".to_string(),
            Some(vec![bollard::secret::PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some("32400".to_string()),
            }]),
        );
        let inspect = bollard::models::ContainerInspectResponse {
            host_config: Some(bollard::secret::HostConfig {
                port_bindings: Some(port_bindings),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(private_ports_from_inspect(&inspect), vec![32400]);
    }

    #[test]
    fn inspect_ports_falls_back_to_exposed_ports() {
        use std::collections::HashMap;

        let mut exposed_ports = HashMap::new();
        exposed_ports.insert("32400/tcp".to_string(), HashMap::new());
        let inspect = bollard::models::ContainerInspectResponse {
            config: Some(bollard::secret::ContainerConfig {
                exposed_ports: Some(exposed_ports),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(private_ports_from_inspect(&inspect), vec![32400]);
    }

    #[test]
    fn env_port_hint_prefers_webui_candidate() {
        let inspect = bollard::models::ContainerInspectResponse {
            config: Some(bollard::secret::ContainerConfig {
                env: Some(vec![
                    "WEBUI_PORT=8069".to_string(),
                    "TORRENTING_PORT=6881".to_string(),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(
            env_port_hint_from_inspect(&inspect, &[6881, 8069, 8080]),
            Some(8069)
        );
    }

    #[test]
    fn env_port_hint_ignores_non_web_port() {
        let inspect = bollard::models::ContainerInspectResponse {
            config: Some(bollard::secret::ContainerConfig {
                env: Some(vec!["TORRENTING_PORT=6881".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(env_port_hint_from_inspect(&inspect, &[6881, 8080]), None);
    }

    #[test]
    fn env_port_hint_requires_candidate_port() {
        let inspect = bollard::models::ContainerInspectResponse {
            config: Some(bollard::secret::ContainerConfig {
                env: Some(vec!["WEBUI_PORT=8069".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(env_port_hint_from_inspect(&inspect, &[6881, 8080]), None);
    }

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
            "add",
            "iso.vallee.casa",
            "--service",
            "isd-controller",
            "--port",
            "9418",
        ])
        .unwrap();
        match w.c {
            RouteCommand::Add(a) => {
                assert_eq!(a.public_hostname.as_deref(), Some("iso.vallee.casa"));
                assert!(a.host_id.is_none());
                assert!(a.host.is_none());
                assert_eq!(a.service.as_deref(), Some("isd-controller"));
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
            "add",
            "iso.vallee.casa",
            "--host-id",
            "01H000000000000000000000",
            "--service",
            "isd-controller",
            "--port",
            "9418",
        ])
        .unwrap();
        match w.c {
            RouteCommand::Add(a) => {
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
            "add",
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
