//! Controller-backed [`ZoneSource`].
//!
//! Step 2 of the per-host DNS plugin (spec
//! `2026-05-23-isengard-dns-design.md`). The trait lives in
//! [`isengard_dns`]; this module owns the controller-side implementation
//! that derives records from inventory state.
//!
//! # Authority surface
//!
//! For a configured `zone` ("weavers.local") and `service_suffix` ("svc"),
//! the source answers two name shapes:
//!
//! - **Expose records.** Any routing rule whose `public_hostname` is a
//!   subdomain of `zone` (e.g. `myapp.weavers.local`) becomes an A record
//!   pointing at the LAN IP of the host that owns the rule. Every host
//!   runs Pingora, so the LAN IP of the owning host is the right
//!   destination for the proxied request.
//! - **Service-discovery records.** `<svc>.<stack>.<suffix>.<zone>`
//!   (e.g. `web.app.svc.weavers.local`) becomes A records for every
//!   running container with that `(stack, service)` pair, mapped to its
//!   host's LAN IP. Multiple replicas → multiple records (client-side
//!   round-robin; locality preference is a future step).
//!
//! Any other name inside the zone returns `Some(vec![])` → the resolver
//! sets the AA bit and answers `NXDOMAIN` (we own the zone, but no
//! such name exists). Names outside the zone return `None` so the
//! resolver forwards upstream.
//!
//! # What's deferred
//!
//! - **SSE invalidation hook.** Step 2 deliberately re-derives records on
//!   every query; the controller bus surface for emitting
//!   `dns.zone.updated` lands with step 3 when the plugin needs it.
//! - **CNAME / AAAA.** Spec calls A out as v1; IPv6 + alias records are
//!   future-only. Queries of those rtypes against known names return
//!   the empty `Some` (NoData) per the trait contract.
//! - **Operator-managed extra zones / Cloudflare split-horizon.** Same
//!   reason: keeps the v1 wedge narrow.

use std::sync::Arc;

use async_trait::async_trait;
use isengard_dns::ZoneSource;
use isengard_storage::{ContainerListFilter, Inventory};
use std::net::Ipv4Addr;
use std::str::FromStr;

use hickory_proto::rr::{Name, RData, Record, RecordType, rdata};

/// Default TTL for every record this source emits. Spec calls 5s; using
/// it as the floor lets a downstream cache pick up moves within one
/// poll cycle. Caller can override via
/// [`ControllerZoneSource::with_ttl`].
pub const DEFAULT_TTL_SECS: u32 = 5;

/// Authoritative zone source backed by the controller's inventory.
///
/// Cheap to clone (single `Arc<Inventory>`). One instance is shared
/// across the controller's HTTP handler that serves the per-host DNS
/// plugin in step 3.
#[derive(Clone)]
pub struct ControllerZoneSource {
    /// Inventory used to read routing rules, containers, and host LAN
    /// IPs. Wrapped in `Arc` so cloning the zone source is cheap.
    inventory: Arc<Inventory>,
    /// Configured zone apex, e.g. `weavers.local.`. Always stored with
    /// a trailing dot so canonical comparison is straightforward.
    zone: Name,
    /// Service-discovery suffix sitting between the stack name and the
    /// zone (e.g. `svc` → `web.app.svc.weavers.local`). The label is
    /// stored as a single utf-8 token; comparison is case-insensitive.
    service_suffix: String,
    /// TTL applied to every emitted record.
    ttl_secs: u32,
}

impl ControllerZoneSource {
    /// Build a new source for `zone` (e.g. `"weavers.local"`) with the
    /// given `service_suffix` (e.g. `"svc"`).
    ///
    /// # Errors
    ///
    /// Returns an error if `zone` is not a syntactically valid DNS name.
    /// The service suffix is treated as a plain UTF-8 token; the
    /// composed full name is validated on every query, so a typo there
    /// surfaces at lookup time as a `None` (not authoritative) rather
    /// than at construction.
    pub fn new(
        inventory: Arc<Inventory>,
        zone: &str,
        service_suffix: &str,
    ) -> anyhow::Result<Self> {
        let mut zone = Name::from_str(zone)?;
        zone.set_fqdn(true);
        Ok(Self {
            inventory,
            zone,
            service_suffix: service_suffix.to_ascii_lowercase(),
            ttl_secs: DEFAULT_TTL_SECS,
        })
    }

    /// Override the TTL applied to every emitted record. Returns `self`
    /// for builder-style chaining.
    #[must_use]
    pub fn with_ttl(mut self, ttl_secs: u32) -> Self {
        self.ttl_secs = ttl_secs;
        self
    }

    /// Number of labels in the configured zone (including the implicit
    /// root). Used to split queried names into "labels before the zone"
    /// vs. "the zone itself".
    fn zone_label_count(&self) -> usize {
        self.zone.num_labels() as usize
    }

    /// Returns the labels of `name` that sit strictly above the
    /// configured zone, in order from most specific to zone-adjacent.
    /// `None` means `name` is not inside the zone (caller forwards
    /// upstream).
    fn labels_above_zone(&self, name: &Name) -> Option<Vec<String>> {
        if !self.zone.zone_of(name) {
            return None;
        }
        let total = name.num_labels() as usize;
        let zone_n = self.zone_label_count();
        if total < zone_n {
            return None;
        }
        let above = total - zone_n;
        let mut out = Vec::with_capacity(above);
        for i in 0..above {
            // Name iterator yields labels from most-specific to least;
            // `name.iter()` returns `&[u8]` slices.
            let label = name.iter().nth(i).unwrap_or_default();
            out.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        }
        Some(out)
    }

    /// Build an A record `name -> ip` with the configured TTL.
    fn a_record(&self, name: &Name, ip: Ipv4Addr) -> Record {
        Record::from_rdata(name.clone(), self.ttl_secs, RData::A(rdata::A(ip)))
    }

    /// Look up A records for the expose name `<labels-above-zone>`.
    /// Returns the empty vec when nothing matches (NoData inside zone).
    async fn resolve_expose(
        &self,
        name: &Name,
        full_hostname: &str,
    ) -> anyhow::Result<Vec<Record>> {
        let rules = self.inventory.list_all_routing_rules().await?;
        let mut out = Vec::new();
        for rule in rules {
            if rule.public_hostname.eq_ignore_ascii_case(full_hostname) {
                if let Some(ip_str) = self.inventory.host_lan_ip(rule.host_id).await? {
                    if let Ok(ip) = Ipv4Addr::from_str(&ip_str) {
                        out.push(self.a_record(name, ip));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Look up A records for a service-discovery name
    /// `<service>.<stack>.<suffix>.<zone>`. Returns the empty vec when
    /// no running container matches.
    async fn resolve_service(
        &self,
        name: &Name,
        service: &str,
        stack: &str,
    ) -> anyhow::Result<Vec<Record>> {
        let filter = ContainerListFilter {
            stack: Some(stack.to_string()),
            service: Some(service.to_string()),
            state: Some("running".into()),
            include_removed: false,
            ..Default::default()
        };
        let containers = isengard_storage::list_containers(self.inventory.pool(), filter).await?;
        let mut out = Vec::new();
        for c in containers {
            if let Some(ip_str) = self.inventory.host_lan_ip(c.host_id).await? {
                if let Ok(ip) = Ipv4Addr::from_str(&ip_str) {
                    out.push(self.a_record(name, ip));
                }
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl ZoneSource for ControllerZoneSource {
    async fn resolve(&self, name: &Name, rtype: RecordType) -> anyhow::Result<Option<Vec<Record>>> {
        let Some(labels) = self.labels_above_zone(name) else {
            return Ok(None);
        };

        // Apex (`<zone>` itself) has no isengard-managed records yet.
        if labels.is_empty() {
            return Ok(Some(Vec::new()));
        }

        if rtype != RecordType::A && rtype != RecordType::ANY {
            return Ok(Some(Vec::new()));
        }

        // Single label above zone -> expose record.
        // Example: `myapp` -> hostname `myapp.weavers.local`.
        if labels.len() == 1 {
            let hostname = format!("{}.{}", labels[0], trim_trailing_dot(&self.zone.to_utf8()));
            return Ok(Some(self.resolve_expose(name, &hostname).await?));
        }

        // Service-discovery shape: `<svc>.<stack>.<suffix>` above zone.
        // Example: `web.app.svc` -> service `web` in stack `app` when
        // suffix is `svc`.
        if labels.len() == 3 && labels[2] == self.service_suffix {
            return Ok(Some(
                self.resolve_service(name, &labels[0], &labels[1]).await?,
            ));
        }

        // Any other in-zone shape: authoritative NoData (NXDOMAIN at
        // the resolver layer once it confirms no records).
        Ok(Some(Vec::new()))
    }
}

/// Strip a single trailing `.` from a DNS-style FQDN string. Used
/// when assembling expose hostnames so the matched value lines up
/// with the operator-visible form stored in `routing_rules`.
fn trim_trailing_dot(s: &str) -> &str {
    s.strip_suffix('.').unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_storage::{
        ContainerRow, EnrollHost, HostId, InsertRoutingRule, Inventory, RoutingRuleSource,
        RoutingRuleState, TlsMode,
    };

    async fn fixture_inventory() -> Inventory {
        Inventory::open_in_memory()
            .await
            .expect("inventory open_in_memory")
    }

    async fn enroll_host(inv: &Inventory, hostname: &str, ip: &str) -> HostId {
        let id = inv
            .enroll_host(EnrollHost {
                fingerprint: format!("fp-{hostname}"),
                hostname: hostname.into(),
                os: "linux".into(),
                arch: "arm64".into(),
                agent_version: "test".into(),
                docker_version: "27.0.0".into(),
            })
            .await
            .expect("enroll_host");
        inv.set_host_lan_ip(id, ip).await.expect("set_host_lan_ip");
        id
    }

    async fn insert_expose_rule(inv: &Inventory, host_id: HostId, hostname: &str) {
        inv.insert_routing_rule(InsertRoutingRule {
            host_id,
            stack_id: None,
            service_name: "web".into(),
            container_port: 8080,
            public_hostname: hostname.into(),
            protocol: "http".into(),
            adapter: "caddy".into(),
            tls_mode: TlsMode::Edge,
            healthcheck_path: None,
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Active,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .expect("insert_routing_rule");
    }

    async fn insert_container(
        inv: &Inventory,
        host_id: HostId,
        id: &str,
        stack: &str,
        service: &str,
    ) {
        let now = chrono::Utc::now().timestamp();
        isengard_storage::upsert_container(
            inv.pool(),
            &ContainerRow {
                id: id.into(),
                host_id,
                service_id: None,
                runtime_container_id: format!("rt-{id}"),
                image: "nginx".into(),
                command: None,
                state: "running".into(),
                status_message: None,
                names: format!("/{service}"),
                stack: Some(stack.into()),
                service: Some(service.into()),
                created_at: Some(now),
                first_seen_at: now,
                last_seen_at: now,
                removed_at: None,
            },
        )
        .await
        .expect("upsert_container");
    }

    fn fqdn(name: &str) -> Name {
        let mut n = Name::from_str(name).expect("parse");
        n.set_fqdn(true);
        n
    }

    #[tokio::test]
    async fn outside_zone_returns_none() {
        let inv = Arc::new(fixture_inventory().await);
        let src = ControllerZoneSource::new(inv, "weavers.local", "svc").unwrap();
        let answer = src
            .resolve(&fqdn("example.com"), RecordType::A)
            .await
            .unwrap();
        assert!(
            answer.is_none(),
            "queries outside the zone forward upstream"
        );
    }

    #[tokio::test]
    async fn expose_label_resolves_to_owning_host_ip() {
        let inv = Arc::new(fixture_inventory().await);
        let host = enroll_host(&inv, "iso-1", "192.168.1.10").await;
        insert_expose_rule(&inv, host, "myapp.weavers.local").await;

        let src = ControllerZoneSource::new(inv, "weavers.local", "svc").unwrap();
        let answer = src
            .resolve(&fqdn("myapp.weavers.local"), RecordType::A)
            .await
            .unwrap()
            .expect("inside zone");
        assert_eq!(answer.len(), 1);
        match answer[0].data() {
            RData::A(rdata::A(ip)) => assert_eq!(*ip, Ipv4Addr::new(192, 168, 1, 10)),
            other => panic!("expected A, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn service_discovery_returns_one_record_per_replica() {
        let inv = Arc::new(fixture_inventory().await);
        let h1 = enroll_host(&inv, "iso-1", "192.168.1.10").await;
        let h2 = enroll_host(&inv, "iso-2", "192.168.1.11").await;
        insert_container(&inv, h1, "aaaa1", "app", "web").await;
        insert_container(&inv, h2, "bbbb2", "app", "web").await;

        let src = ControllerZoneSource::new(inv, "weavers.local", "svc").unwrap();
        let answer = src
            .resolve(&fqdn("web.app.svc.weavers.local"), RecordType::A)
            .await
            .unwrap()
            .expect("inside zone");
        let mut ips: Vec<_> = answer
            .iter()
            .filter_map(|r| match r.data() {
                RData::A(rdata::A(ip)) => Some(ip),
                _ => None,
            })
            .copied()
            .collect();
        ips.sort();
        assert_eq!(
            ips,
            vec![
                Ipv4Addr::new(192, 168, 1, 10),
                Ipv4Addr::new(192, 168, 1, 11)
            ]
        );
    }

    #[tokio::test]
    async fn unknown_in_zone_name_returns_nodata_inside_zone() {
        let inv = Arc::new(fixture_inventory().await);
        let src = ControllerZoneSource::new(inv, "weavers.local", "svc").unwrap();
        let answer = src
            .resolve(&fqdn("ghost.weavers.local"), RecordType::A)
            .await
            .unwrap()
            .expect("inside zone");
        assert!(
            answer.is_empty(),
            "in-zone with no match returns empty Some (NXDOMAIN at resolver)"
        );
    }

    #[tokio::test]
    async fn aaaa_query_against_known_name_returns_nodata() {
        let inv = Arc::new(fixture_inventory().await);
        let host = enroll_host(&inv, "iso-1", "192.168.1.10").await;
        insert_expose_rule(&inv, host, "myapp.weavers.local").await;

        let src = ControllerZoneSource::new(inv, "weavers.local", "svc").unwrap();
        let answer = src
            .resolve(&fqdn("myapp.weavers.local"), RecordType::AAAA)
            .await
            .unwrap()
            .expect("inside zone");
        assert!(
            answer.is_empty(),
            "AAAA against an A-only authoritative name is NoData, not None"
        );
    }

    #[tokio::test]
    async fn service_with_wrong_suffix_returns_nodata() {
        let inv = Arc::new(fixture_inventory().await);
        let h1 = enroll_host(&inv, "iso-1", "192.168.1.10").await;
        insert_container(&inv, h1, "aaaa1", "app", "web").await;

        let src = ControllerZoneSource::new(inv, "weavers.local", "svc").unwrap();
        let answer = src
            .resolve(&fqdn("web.app.other.weavers.local"), RecordType::A)
            .await
            .unwrap()
            .expect("inside zone");
        assert!(answer.is_empty(), "wrong suffix is treated as unknown");
    }

    #[tokio::test]
    async fn host_without_lan_ip_is_skipped_silently() {
        let inv = Arc::new(fixture_inventory().await);
        // host enrolled, no lan_ip set
        let id = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp-noip".into(),
                hostname: "iso-noip".into(),
                os: "linux".into(),
                arch: "arm64".into(),
                agent_version: "test".into(),
                docker_version: "27.0.0".into(),
            })
            .await
            .unwrap();
        insert_expose_rule(&inv, id, "noip.weavers.local").await;

        let src = ControllerZoneSource::new(inv, "weavers.local", "svc").unwrap();
        let answer = src
            .resolve(&fqdn("noip.weavers.local"), RecordType::A)
            .await
            .unwrap()
            .expect("inside zone");
        assert!(answer.is_empty(), "no IP yet -> no record");
    }
}
