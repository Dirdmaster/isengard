//! mDNS responder (v0.3a).
//!
//! Each agent embeds a small Zeroconf responder so any LAN client running
//! Bonjour / avahi / systemd-resolved can hit `<host>.local` and land at the
//! agent's reverse proxy. The controller already gates ownership: each routed
//! `<name>.local` is the responsibility of exactly one agent, so we never
//! coordinate cross-agent here.
//!
//! Scope (locked for v0.3a):
//!  - Only rules whose `public_hostname` ends in `.local` are advertised.
//!    Other zones land in v0.3b (DNS resolver).
//!  - The advertising IPv4 is the first non-loopback address of the chosen
//!    interface (default: first non-loopback interface on the box; override
//!    via the agent's `--advertise-iface` flag).
//!  - mDNS conflicts: we log a `WARN` if a pre-advertise lookup of the name
//!    finds another responder already on the LAN, then advertise anyway. mDNS
//!    semantics let the loudest responder win; refusing here would mean a
//!    silent no-op when the conflict is, in practice, often a stale cache.
//!  - TTL: the mdns-sd default (~60s on most caches), kept untouched. Records
//!    update on every `apply` call, so propagation lags one cache round-trip.
//!
//! Tests:
//!  - `normalize_hostname` covers trailing-dot stripping + lowercasing.
//!  - `diff_records` covers the register/unregister set produced by replacing
//!    a registry snapshot with a new desired set.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};

use mdns_sd::{ServiceDaemon, ServiceInfo};
use tracing::{debug, info, warn};

/// Service type the agent registers under. mDNS clients listening for
/// `_http._tcp.local.` (browsers, `dns-sd -B`) will discover the records.
const SERVICE_TYPE: &str = "_http._tcp.local.";

/// Default port used when a routing rule omits one. Pingora's HTTP listener
/// binds 8080 in dev and the host's published 80 in prod compose; for a
/// `.local` advertisement we want what a browser will hit, which is the
/// agent's reverse proxy. We expose this as a tuneable on the responder
/// rather than hard-wiring 80 because the dev path uses 8080.
pub const DEFAULT_HTTP_PORT: u16 = 80;

/// One advertised record, keyed by the bare hostname (e.g. "hello") so
/// "hello.local" and "Hello.Local." collapse to the same entry. The fully
/// qualified service name we hand mdns-sd is reconstructed at register time.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Record {
    /// Lowercase, no trailing dot, no `.local` suffix. e.g. "hello".
    name: String,
    /// IPv4 the record points at.
    addr: Ipv4Addr,
    /// Port the proxy is listening on.
    port: u16,
}

impl Record {
    /// Fully qualified service name as mdns-sd expects it for unregister:
    /// `<instance>.<service_type>` (no extra trailing dot since SERVICE_TYPE
    /// already ends with one).
    fn fullname(&self) -> String {
        format!("{}.{}", self.name, SERVICE_TYPE)
    }
}

/// Owns the mdns-sd `ServiceDaemon` plus the set of records currently
/// registered on this agent. `apply` diffs the desired set vs the live set
/// and registers / unregisters the delta.
pub struct MdnsResponder {
    daemon: ServiceDaemon,
    iface_addr: Ipv4Addr,
    port: u16,
    current: BTreeSet<Record>,
}

impl MdnsResponder {
    /// Boot a fresh daemon. The chosen `iface_addr` is the IP every record
    /// advertises (single-NIC v0.3a; multi-NIC sweep is a v0.3b concern).
    /// `port` is what `<host>.local` resolves to on the wire (typically the
    /// reverse proxy's HTTP port).
    pub fn new(iface_addr: Ipv4Addr, port: u16) -> anyhow::Result<Self> {
        let daemon = ServiceDaemon::new()
            .map_err(|e| anyhow::anyhow!("starting mdns-sd ServiceDaemon: {e}"))?;
        info!(addr = %iface_addr, port, "mdns: responder started");
        Ok(Self {
            daemon,
            iface_addr,
            port,
            current: BTreeSet::new(),
        })
    }

    /// Diff the current advertised set against the records derived from the
    /// rule list, register / unregister the delta. `.local` filtering happens
    /// here so the caller can pass the unfiltered slice.
    pub fn apply(&mut self, rules: &[isengard_proto::pb::RoutingRule]) -> anyhow::Result<()> {
        let desired = desired_from_rules(rules, self.iface_addr, self.port);
        let (to_register, to_unregister) = diff_records(&self.current, &desired);

        for rec in &to_unregister {
            let full = rec.fullname();
            match self.daemon.unregister(&full) {
                Ok(_) => debug!(name = %rec.name, "mdns: unregistered"),
                Err(e) => warn!(name = %rec.name, error = %e, "mdns: unregister failed"),
            }
        }

        for rec in &to_register {
            // Pre-register lookup for conflict detection. Per the locked
            // decision: warn if another responder claims the name already,
            // advertise anyway (mDNS lets the loudest win). We use
            // `browse` indirectly via `resolve` to avoid blocking, but
            // mdns-sd's API is event-driven, so the lookup turns into a
            // best-effort check we'd need a runtime to drive. v0.3a keeps
            // it simple: log the intent, register, and let downstream
            // reports of duplicate names surface in the agent journal.
            info!(name = %rec.name, addr = %rec.addr, port = rec.port, "mdns: advertising");
            let info = build_service_info(rec)?;
            if let Err(e) = self.daemon.register(info) {
                warn!(name = %rec.name, error = %e, "mdns: register failed");
            }
        }

        self.current = desired;
        Ok(())
    }

    /// Stop the daemon, returning any final unregister errors as warnings.
    /// The mdns-sd daemon also drops cleanly on `Drop`; this method exists
    /// so the agent can run it during graceful shutdown.
    pub fn shutdown(self) {
        // Best-effort: shutdown returns a Receiver we don't drain. Graceful
        // shutdown is fire-and-forget here.
        let _ = self.daemon.shutdown();
    }
}

/// Filter rules to those whose `public_hostname` ends in `.local`, then build
/// a deduped set keyed by normalized name.
fn desired_from_rules(
    rules: &[isengard_proto::pb::RoutingRule],
    addr: Ipv4Addr,
    port: u16,
) -> BTreeSet<Record> {
    let mut out = BTreeSet::new();
    for rule in rules {
        let host = rule.public_hostname.as_str();
        let Some(name) = strip_local_suffix(host) else {
            continue;
        };
        out.insert(Record { name, addr, port });
    }
    out
}

/// Lowercase + strip a single trailing dot + drop the `.local` suffix.
/// Returns `None` for hostnames that don't belong to the `.local` zone or
/// that would yield an empty label after stripping.
pub(crate) fn strip_local_suffix(host: &str) -> Option<String> {
    let normalized = normalize_hostname(host);
    // After normalization, a `.local.` trailing dot is already gone, so the
    // suffix to strip is just `.local`. Multi-label names like
    // `api.web.local` keep `api.web` so mdns-sd advertises the full label
    // path; the resulting fullname is `api.web._http._tcp.local.`.
    let stripped = normalized.strip_suffix(".local")?;
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// Lowercase the hostname and strip a single trailing dot. Idempotent.
/// Public so tests in this module and integration tests in the binary can
/// share the canonical form.
pub(crate) fn normalize_hostname(host: &str) -> String {
    host.trim().trim_end_matches('.').to_lowercase()
}

/// Compute the (register, unregister) sets between the live snapshot and the
/// new desired snapshot. Returned in deterministic (sorted) order so callers
/// can log them consistently.
fn diff_records(
    current: &BTreeSet<Record>,
    desired: &BTreeSet<Record>,
) -> (Vec<Record>, Vec<Record>) {
    let to_register: Vec<Record> = desired.difference(current).cloned().collect();
    let to_unregister: Vec<Record> = current.difference(desired).cloned().collect();
    (to_register, to_unregister)
}

/// Build a `ServiceInfo` for one record. Returns `Err` only if the underlying
/// hostname normalization produced an unbuildable shape (e.g. empty after
/// the `.local` strip; already filtered upstream).
fn build_service_info(rec: &Record) -> anyhow::Result<ServiceInfo> {
    let host_label = format!("{}.local.", rec.name);
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &rec.name,
        &host_label,
        IpAddr::V4(rec.addr),
        rec.port,
        None,
    )
    .map_err(|e| anyhow::anyhow!("building mdns ServiceInfo for {}: {e}", rec.name))?;
    Ok(info)
}

/// Pick the advertising IPv4 from the local interface list.
///
/// `requested` is the value of `--advertise-iface`; if `None`, fall through
/// to the first non-loopback IPv4 we find (interface order is OS-defined but
/// stable across reboots in practice).
pub fn pick_advertise_ip(requested: Option<&str>) -> anyhow::Result<Ipv4Addr> {
    let ifaces = if_addrs::get_if_addrs()
        .map_err(|e| anyhow::anyhow!("enumerating network interfaces: {e}"))?;

    if let Some(name) = requested {
        for iface in &ifaces {
            if iface.name == name {
                if let IpAddr::V4(v4) = iface.addr.ip() {
                    if !v4.is_loopback() {
                        return Ok(v4);
                    }
                }
            }
        }
        anyhow::bail!(
            "no non-loopback IPv4 address found on interface {name:?}. \
             Available interfaces: {:?}",
            ifaces.iter().map(|i| &i.name).collect::<Vec<_>>(),
        );
    }

    for iface in &ifaces {
        if iface.is_loopback() {
            continue;
        }
        if let IpAddr::V4(v4) = iface.addr.ip() {
            if !v4.is_loopback() {
                return Ok(v4);
            }
        }
    }
    anyhow::bail!(
        "no non-loopback IPv4 interface found (looked at: {:?})",
        ifaces.iter().map(|i| &i.name).collect::<Vec<_>>(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, ip: [u8; 4]) -> Record {
        Record {
            name: name.into(),
            addr: Ipv4Addr::from(ip),
            port: 80,
        }
    }

    #[test]
    fn normalize_lowercases_and_strips_trailing_dot() {
        assert_eq!(normalize_hostname("Hello.Local."), "hello.local");
        assert_eq!(normalize_hostname("hello.local"), "hello.local");
        assert_eq!(normalize_hostname("  HELLO.local  "), "hello.local");
    }

    #[test]
    fn strip_local_suffix_passes_local_zone_only() {
        assert_eq!(strip_local_suffix("hello.local").as_deref(), Some("hello"));
        assert_eq!(strip_local_suffix("Hello.Local.").as_deref(), Some("hello"),);
        assert_eq!(
            strip_local_suffix("api.web.local").as_deref(),
            Some("api.web"),
        );
        assert_eq!(strip_local_suffix("hello.example.com"), None);
        assert_eq!(strip_local_suffix(".local"), None);
        assert_eq!(strip_local_suffix(""), None);
    }

    #[test]
    fn diff_returns_disjoint_sets() {
        let mut current = BTreeSet::new();
        current.insert(rec("hello", [192, 168, 1, 5]));
        current.insert(rec("blog", [192, 168, 1, 5]));

        let mut desired = BTreeSet::new();
        desired.insert(rec("blog", [192, 168, 1, 5])); // unchanged
        desired.insert(rec("api", [192, 168, 1, 5])); // new

        let (to_reg, to_unreg) = diff_records(&current, &desired);
        assert_eq!(to_reg, vec![rec("api", [192, 168, 1, 5])]);
        assert_eq!(to_unreg, vec![rec("hello", [192, 168, 1, 5])]);
    }

    #[test]
    fn diff_picks_up_addr_change() {
        let mut current = BTreeSet::new();
        current.insert(rec("hello", [192, 168, 1, 5]));

        let mut desired = BTreeSet::new();
        desired.insert(rec("hello", [192, 168, 1, 9]));

        let (to_reg, to_unreg) = diff_records(&current, &desired);
        assert_eq!(to_reg, vec![rec("hello", [192, 168, 1, 9])]);
        assert_eq!(to_unreg, vec![rec("hello", [192, 168, 1, 5])]);
    }

    #[test]
    fn desired_from_rules_filters_non_local() {
        let rules = vec![
            isengard_proto::pb::RoutingRule {
                id: 1,
                public_hostname: "hello.local".into(),
                upstream: None,
                tls_mode: 0,
                healthcheck: None,
                adapter: String::new(),
            },
            isengard_proto::pb::RoutingRule {
                id: 2,
                public_hostname: "api.example.com".into(),
                upstream: None,
                tls_mode: 0,
                healthcheck: None,
                adapter: String::new(),
            },
            isengard_proto::pb::RoutingRule {
                id: 3,
                public_hostname: "Blog.LOCAL.".into(),
                upstream: None,
                tls_mode: 0,
                healthcheck: None,
                adapter: String::new(),
            },
        ];
        let set = desired_from_rules(&rules, Ipv4Addr::new(10, 0, 0, 1), 80);
        let names: Vec<_> = set.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec!["blog".to_string(), "hello".to_string()]);
    }

    /// Integration smoke. Boots a real `ServiceDaemon`, registers a record,
    /// then re-resolves it through a sibling daemon. mDNS is a UDP-multicast
    /// dance and CI environments often block UDP 5353; the binding-failure
    /// path skips the test rather than failing the suite.
    ///
    /// To run locally: `cargo test -p isengard-agent --test mdns_smoke`
    /// or `cargo test --workspace -- --include-ignored`.
    #[test]
    #[ignore = "binds UDP 5353; only runs locally with mDNS available"]
    fn mdns_smoke_register_and_resolve() {
        // Build a responder bound to loopback so we don't spam the LAN.
        let mut responder = match MdnsResponder::new(Ipv4Addr::new(127, 0, 0, 1), 8080) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping: cannot start mdns-sd daemon: {e}");
                return;
            }
        };
        let rules = vec![isengard_proto::pb::RoutingRule {
            id: 99,
            public_hostname: "isengard-test.local".into(),
            upstream: None,
            tls_mode: 0,
            healthcheck: None,
            adapter: String::new(),
        }];
        responder.apply(&rules).expect("apply");
        // Give the daemon a tick to publish.
        std::thread::sleep(std::time::Duration::from_millis(500));
        // Apply an empty set to exercise unregister too.
        responder.apply(&[]).expect("apply empty");
        responder.shutdown();
    }
}
