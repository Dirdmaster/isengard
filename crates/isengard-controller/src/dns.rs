//! v0.3b: controller-hosted DNS resolver for a configurable zone.
//!
//! mDNS (v0.3a) covers `.local`. Operators with their own zone (e.g. `.iso`,
//! `.weavers`, `.lan`) get a small embedded server here. Records derive from
//! the existing `routing_rules` table: `<public_hostname>.<zone>` resolves to
//! the LAN IP of the agent that owns the rule.
//!
//! Locked decisions (per `1 Projects/Isengard/v0.3b DNS Resolver.md`):
//!
//! - One zone per controller for v0.3b. Multi-zone is v0.4.
//! - Zone is configured via `--dns-zone <name>` on the controller subcommand.
//! - Listen address defaults to `0.0.0.0:5300` for dev; prod compose maps :53
//!   with `cap_add: NET_BIND_SERVICE`.
//! - TTL: 30 seconds.
//! - Names ending in `.local` are filtered out: mDNS owns that zone.
//! - Inside-zone unknown name -> NXDOMAIN. Outside-zone -> REFUSED so the
//!   client OS falls through to its upstream resolver.
//! - Records are refreshed by polling routing rules every 5s. v0.4 will
//!   switch to event-driven via the bus.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hickory_proto::op::{Header, MessageType, ResponseCode};
use hickory_proto::rr::{Name, RData, Record, RecordType, rdata};
use hickory_server::ServerFuture;
use hickory_server::authority::MessageResponseBuilder;
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use isengard_storage::{HostId, Inventory, RoutingRule};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, info, instrument, warn};

/// TTL for every A record we serve. Short enough that a stack reassignment
/// lands fast; long enough to not drown clients in lookups.
pub const RECORD_TTL_SECS: u32 = 30;

/// Cadence at which we re-derive records from the routing-rules table.
///
/// 5 seconds is a deliberate compromise: faster than a TTL, slow enough to
/// not hammer SQLite under steady state. v0.4 will replace this with an
/// event-bus subscription so changes propagate within milliseconds.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// One A record. Carried in the live record map. Hostnames are stored
/// fully-qualified and lower-cased so lookups can use exact equality on the
/// query name's lowercased UTF-8 form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneRecord {
    /// Full FQDN: `<public_hostname>.<zone>` plus a trailing dot.
    pub fqdn: String,
    /// Resolved IPv4 address of the owning agent.
    pub ip: Ipv4Addr,
}

/// Ownership of the resolver: holds the live record map (shared with the
/// hickory `RequestHandler`) plus the polling task. Used by the controller
/// runtime to bind a socket and spawn the server.
pub struct DnsResolver {
    zone: String,
    records: SharedRecords,
    inventory: Arc<Inventory>,
}

type SharedRecords = Arc<RwLock<HashMap<String, Ipv4Addr>>>;

impl DnsResolver {
    /// Build a resolver wired to a specific zone and inventory. Does not yet
    /// bind a socket; call `start` to do that.
    pub fn new(zone: String, inventory: Arc<Inventory>) -> Self {
        Self {
            zone: normalize_zone(&zone),
            records: Arc::new(RwLock::new(HashMap::new())),
            inventory,
        }
    }

    /// Bind a UDP socket and begin serving. Returns the join handle for the
    /// background polling task; the hickory server itself is owned by the
    /// returned future via the `JoinSet` it manages internally.
    ///
    /// Cancellation: drop the returned `JoinHandle` to stop the polling
    /// task. The hickory server runs until the underlying socket is dropped
    /// (the polling handle owns the only reference path back to it).
    pub async fn start(self, listen: SocketAddr) -> Result<DnsResolverHandle> {
        let socket = UdpSocket::bind(listen)
            .await
            .with_context(|| format!("binding DNS UDP socket on {listen}"))?;
        info!(zone = %self.zone, %listen, "DNS resolver listening");

        let handler = ZoneHandler {
            zone: self.zone.clone(),
            records: self.records.clone(),
        };

        let mut server = ServerFuture::new(handler);
        server.register_socket(socket);

        // Initial sync so the first query after boot already sees current
        // rules. Subsequent ticks pick up changes.
        if let Err(e) = self.refresh_once().await {
            warn!(error = %e, "DNS resolver: initial record refresh failed");
        }

        let inv = self.inventory.clone();
        let records = self.records.clone();
        let zone = self.zone.clone();
        let poll_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(POLL_INTERVAL);
            // Skip the immediate first tick: refresh_once above already ran.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if let Err(e) = refresh(&inv, &zone, &records).await {
                    warn!(error = %e, "DNS resolver: refresh failed");
                }
            }
        });

        Ok(DnsResolverHandle {
            poll_handle,
            _server: server,
        })
    }

    /// Re-derive records once. Exposed for tests; production calls the
    /// version embedded in the polling task.
    pub async fn refresh_once(&self) -> Result<()> {
        refresh(&self.inventory, &self.zone, &self.records).await
    }

    /// Snapshot of the current record map. Used by tests + future REST
    /// debug endpoint (see spec "API surface" §).
    pub async fn snapshot(&self) -> HashMap<String, Ipv4Addr> {
        self.records.read().await.clone()
    }
}

/// Combined ownership of the polling task + server future. Aborting the
/// poll handle stops the polling loop; dropping the whole handle drops the
/// `ServerFuture` and unbinds the UDP socket.
pub struct DnsResolverHandle {
    pub poll_handle: JoinHandle<()>,
    _server: ServerFuture<ZoneHandler>,
}

impl DnsResolverHandle {
    /// Abort the polling task. Equivalent to `self.poll_handle.abort()`.
    pub fn abort(&self) {
        self.poll_handle.abort();
    }
}

/// Hickory `RequestHandler` impl: looks up the queried name in the live
/// record map and either answers, NXDOMAINs (inside-zone miss), or REFUSEDs
/// (outside-zone). Cloned per inbound request by hickory's internals.
#[derive(Clone)]
pub struct ZoneHandler {
    zone: String,
    records: SharedRecords,
}

#[async_trait::async_trait]
impl RequestHandler for ZoneHandler {
    #[instrument(skip(self, request, response_handle), fields(zone = %self.zone))]
    async fn handle_request<R: ResponseHandler>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let queries = request.queries();
        let Some(query) = queries.first() else {
            return reply_error(request, &mut response_handle, ResponseCode::FormErr).await;
        };

        let qname = format_query_name(&query.name().to_string());
        let qtype = query.query_type();
        debug!(qname = %qname, ?qtype, "DNS query received");

        // Outside our zone: REFUSED. Tells the client OS to try its upstream.
        if !is_in_zone(&qname, &self.zone) {
            return reply_error(request, &mut response_handle, ResponseCode::Refused).await;
        }

        // Only handle A queries (and ANY). v0.3b is IPv4-only.
        if qtype != RecordType::A && qtype != RecordType::ANY {
            return reply_nxdomain_or_empty(request, &mut response_handle, &qname, &self.records)
                .await;
        }

        let key = qname.trim_end_matches('.').to_ascii_lowercase();
        let ip = { self.records.read().await.get(&key).copied() };

        match ip {
            Some(ip) => {
                let name = match Name::from_utf8(&qname) {
                    Ok(n) => n,
                    Err(e) => {
                        warn!(qname = %qname, error = %e, "failed to parse query name");
                        return reply_error(request, &mut response_handle, ResponseCode::ServFail)
                            .await;
                    }
                };
                let record = Record::from_rdata(name, RECORD_TTL_SECS, RData::A(rdata::A(ip)));
                send_answer(request, &mut response_handle, vec![record]).await
            }
            None => reply_error(request, &mut response_handle, ResponseCode::NXDomain).await,
        }
    }
}

async fn send_answer<R: ResponseHandler>(
    request: &Request,
    response_handle: &mut R,
    answers: Vec<Record>,
) -> ResponseInfo {
    let builder = MessageResponseBuilder::from_message_request(request);
    let mut header = Header::response_from_request(request.header());
    header.set_message_type(MessageType::Response);
    header.set_authoritative(true);
    header.set_response_code(ResponseCode::NoError);

    let response = builder.build(header, answers.iter(), &[], &[], &[]);
    match response_handle.send_response(response).await {
        Ok(info) => info,
        Err(e) => {
            warn!(error = %e, "failed to send DNS answer");
            ResponseInfo::from(*request.header())
        }
    }
}

async fn reply_error<R: ResponseHandler>(
    request: &Request,
    response_handle: &mut R,
    code: ResponseCode,
) -> ResponseInfo {
    let builder = MessageResponseBuilder::from_message_request(request);
    let response = builder.error_msg(request.header(), code);
    match response_handle.send_response(response).await {
        Ok(info) => info,
        Err(e) => {
            warn!(error = %e, ?code, "failed to send DNS error response");
            ResponseInfo::from(*request.header())
        }
    }
}

/// In-zone non-A query: respond NoError but with no answers (matches what
/// authoritative servers do for AAAA on an IPv4-only zone). Out-of-zone is
/// already handled upstream as REFUSED.
async fn reply_nxdomain_or_empty<R: ResponseHandler>(
    request: &Request,
    response_handle: &mut R,
    qname: &str,
    records: &SharedRecords,
) -> ResponseInfo {
    let key = qname.trim_end_matches('.').to_ascii_lowercase();
    let known = { records.read().await.contains_key(&key) };
    if known {
        // Name exists, just not for this RR type -> NoError, empty answer.
        let builder = MessageResponseBuilder::from_message_request(request);
        let mut header = Header::response_from_request(request.header());
        header.set_message_type(MessageType::Response);
        header.set_authoritative(true);
        header.set_response_code(ResponseCode::NoError);
        let response = builder.build(header, &[], &[], &[], &[]);
        return match response_handle.send_response(response).await {
            Ok(info) => info,
            Err(e) => {
                warn!(error = %e, "failed to send empty DNS answer");
                ResponseInfo::from(*request.header())
            }
        };
    }
    reply_error(request, response_handle, ResponseCode::NXDomain).await
}

/// Pull every routing rule, drop `.local` (mDNS territory), join with each
/// owning host's cached LAN IP, swap the live map atomically.
async fn refresh(inventory: &Inventory, zone: &str, records: &SharedRecords) -> Result<()> {
    let rules = inventory
        .list_all_routing_rules()
        .await
        .context("list_all_routing_rules")?;

    // Per-host LAN IP cache: avoid re-querying inventory for the same host
    // many times when multiple rules share an owner.
    let mut ip_cache: HashMap<HostId, Option<Ipv4Addr>> = HashMap::new();
    let mut next: HashMap<String, Ipv4Addr> = HashMap::new();

    for rule in &rules {
        let Some(record) = build_record_for_rule(inventory, rule, zone, &mut ip_cache).await else {
            continue;
        };
        // Store keys without trailing dot, lower-cased: matches the lookup
        // form used by the request handler.
        let key = record.fqdn.trim_end_matches('.').to_ascii_lowercase();
        next.insert(key, record.ip);
    }

    let mut guard = records.write().await;
    let changed = guard.len() != next.len() || guard.iter().any(|(k, v)| next.get(k) != Some(v));
    if changed {
        info!(
            zone = %zone,
            record_count = next.len(),
            "DNS resolver: record map updated"
        );
    }
    *guard = next;
    Ok(())
}

async fn build_record_for_rule(
    inventory: &Inventory,
    rule: &RoutingRule,
    zone: &str,
    ip_cache: &mut HashMap<HostId, Option<Ipv4Addr>>,
) -> Option<ZoneRecord> {
    // mDNS coexistence: `.local` rules are advertised by the agent itself
    // via the Phase 9d (v0.3a) mDNS responder. Skip them here so we don't
    // shadow the multicast answer.
    if rule.public_hostname.ends_with(".local") {
        return None;
    }

    // Only build records for hostnames inside the configured zone. A rule
    // with `foo.bar.example.com` won't be exposed via the `.iso` zone.
    let fqdn = rule
        .public_hostname
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if !is_in_zone(&fqdn, zone) {
        return None;
    }

    let ip_opt = if let Some(cached) = ip_cache.get(&rule.host_id).copied() {
        cached
    } else {
        let resolved = match inventory.host_lan_ip(rule.host_id).await {
            Ok(ip_str) => ip_str
                .and_then(|s| s.parse::<IpAddr>().ok())
                .and_then(|ip| {
                    if let IpAddr::V4(v4) = ip {
                        Some(v4)
                    } else {
                        // v0.3b is IPv4-only. AAAA lands in v0.4.
                        None
                    }
                }),
            Err(e) => {
                warn!(
                    host_id = %rule.host_id,
                    error = %e,
                    "DNS resolver: host_lan_ip lookup failed",
                );
                None
            }
        };
        ip_cache.insert(rule.host_id, resolved);
        resolved
    };

    // TODO v0.3b: surface real agent IP. For hosts that have never opened a
    // sync stream since boot the LAN IP is unknown; rather than serve a
    // placeholder we omit the record entirely so the resolver replies
    // NXDOMAIN until the agent reconnects.
    let ip = ip_opt?;

    Some(ZoneRecord {
        fqdn: format!("{fqdn}."),
        ip,
    })
}

/// Lower-case + trim a query name for matching. `Name::to_string` returns
/// `foo.iso.` (with the trailing dot); we keep that form for the return
/// value but match against the dot-stripped, lower-cased map key.
fn format_query_name(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.ends_with('.') {
        trimmed
    } else {
        format!("{trimmed}.")
    }
}

/// Strip leading + trailing dots, lower-case, drop empty segments.
fn normalize_zone(zone: &str) -> String {
    zone.trim().trim_matches('.').to_ascii_lowercase()
}

/// Is `name` (trimmed of trailing dot) inside the zone? `foo.iso` matches
/// `iso`, `a.b.iso` matches `iso`, `iso` itself matches (apex), `foo.local`
/// does not, and `foo.isolation` does not (suffix-with-dot enforced).
pub fn is_in_zone(name: &str, zone: &str) -> bool {
    let n = name.trim_end_matches('.').to_ascii_lowercase();
    let z = normalize_zone(zone);
    if z.is_empty() {
        return false;
    }
    if n == z {
        return true;
    }
    // Need a label boundary, not a substring match: `foo.iso` ends with
    // `.iso`, but `foo.isolation` does not.
    let suffix = format!(".{z}");
    n.ends_with(&suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_storage::{
        EnrollHost, InsertRoutingRule, RoutingRuleSource, RoutingRuleState, TlsMode,
    };
    use std::sync::Arc;

    #[test]
    fn zone_matching_basic() {
        assert!(is_in_zone("foo.iso", "iso"));
        assert!(is_in_zone("foo.iso.", "iso"));
        assert!(is_in_zone("a.b.iso", "iso"));
        assert!(is_in_zone("iso", "iso"));
        assert!(!is_in_zone("foo.local", "iso"));
        assert!(!is_in_zone("foo.isolation", "iso"));
        assert!(!is_in_zone("google.com", "iso"));
    }

    #[test]
    fn zone_matching_normalizes_zone_input() {
        assert!(is_in_zone("foo.iso", ".iso."));
        assert!(is_in_zone("foo.iso", "ISO"));
        assert!(!is_in_zone("foo.iso", ""));
    }

    #[test]
    fn zone_matching_case_insensitive() {
        assert!(is_in_zone("FOO.ISO", "iso"));
        assert!(is_in_zone("Hello.Iso", "ISO"));
    }

    #[test]
    fn format_query_name_adds_trailing_dot() {
        assert_eq!(format_query_name("FOO.ISO"), "foo.iso.");
        assert_eq!(format_query_name("foo.iso."), "foo.iso.");
    }

    async fn fixture_inventory_with_rule(
        zone: &str,
        hostname: &str,
        lan_ip: Option<&str>,
    ) -> Arc<Inventory> {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let host = inv
            .enroll_host(EnrollHost {
                fingerprint: format!("fp-{hostname}"),
                hostname: format!("host-{hostname}"),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0".into(),
                docker_version: "27".into(),
                fleet: "default".into(),
            })
            .await
            .unwrap();

        if let Some(ip) = lan_ip {
            inv.set_host_lan_ip(host, ip).await.unwrap();
        }

        inv.insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id: host,
            stack_id: None,
            service_name: "svc".into(),
            container_port: 80,
            public_hostname: format!("{hostname}.{zone}"),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Edge,
            healthcheck_path: None,
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Pending,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

        inv
    }

    #[tokio::test]
    async fn record_builder_emits_records_for_in_zone_rules() {
        let inv = fixture_inventory_with_rule("iso", "hello", Some("192.168.1.10")).await;
        let resolver = DnsResolver::new("iso".into(), inv);
        resolver.refresh_once().await.unwrap();
        let snap = resolver.snapshot().await;
        assert_eq!(
            snap.get("hello.iso"),
            Some(&"192.168.1.10".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn record_builder_skips_local_hostnames() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let host = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp-mdns".into(),
                hostname: "mdns-host".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0".into(),
                docker_version: "27".into(),
                fleet: "default".into(),
            })
            .await
            .unwrap();
        inv.set_host_lan_ip(host, "10.0.0.5").await.unwrap();
        inv.insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id: host,
            stack_id: None,
            service_name: "svc".into(),
            container_port: 80,
            public_hostname: "foo.local".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Edge,
            healthcheck_path: None,
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Pending,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

        let resolver = DnsResolver::new("local".into(), inv);
        resolver.refresh_once().await.unwrap();
        let snap = resolver.snapshot().await;
        assert!(
            snap.is_empty(),
            "`.local` rules belong to mDNS; resolver must not shadow them: {snap:?}"
        );
    }

    #[tokio::test]
    async fn record_builder_skips_rules_outside_zone() {
        let inv = fixture_inventory_with_rule("iso", "outside.example", Some("192.168.1.20")).await;
        let resolver = DnsResolver::new("weavers".into(), inv);
        resolver.refresh_once().await.unwrap();
        let snap = resolver.snapshot().await;
        assert!(
            snap.is_empty(),
            "rule `outside.example.iso` is not in `weavers` zone: {snap:?}"
        );
    }

    #[tokio::test]
    async fn record_builder_skips_hosts_without_lan_ip() {
        // Host enrolled but never opened a sync stream -> no LAN IP cached.
        // Resolver should omit the record (will reply NXDOMAIN) rather than
        // serve a stale or placeholder address.
        let inv = fixture_inventory_with_rule("iso", "ghost", None).await;
        let resolver = DnsResolver::new("iso".into(), inv);
        resolver.refresh_once().await.unwrap();
        let snap = resolver.snapshot().await;
        assert!(snap.is_empty(), "no IP cached -> no record: {snap:?}");
    }
}
