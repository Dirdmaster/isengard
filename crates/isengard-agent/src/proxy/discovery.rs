//! Container IP discovery for the proxy.
//!
//! The controller never knows the container's runtime-network IP: it ships
//! `RoutingRule` messages with `container_id` set to the container NAME and
//! `container_ip` empty. The agent has to fill the IP in at apply-time from
//! its local view of the runtime.
//!
//! This module owns the discovery logic. Two halves:
//!
//! 1. [`pick_container_ip`]: pure function that ranks candidate IPs from a
//!    `network_settings.networks` map (the bollard-shaped surface). Pure
//!    so it's exhaustively testable without a Docker daemon.
//! 2. [`resolve_container_ip`]: thin async wrapper that calls
//!    [`crate::runtime::RuntimeBackend::inspect_container`] and feeds the
//!    result into a sibling picker over the trait's
//!    `ContainerSnapshot.network_settings.ip_addresses` map. Phase 0.5
//!    moved this off the bollard-direct path so wisp can serve discovery
//!    too.
//!
//! Selection rules (in order):
//!
//! 1. If the container is attached to the shared `isengard-proxy` network,
//!    return that IP. This is the Traefik recipe: pingora and the routed
//!    container are on the same L3 fabric, so reachability is guaranteed.
//! 2. Otherwise, fall back to the first non-empty IP on any non-driver
//!    network (`bridge`, `host`, `none` are skipped). Legacy stacks that
//!    haven't joined `isengard-proxy` still get a rule, but pingora's L3
//!    reachability depends on the agent ALSO being on that network. The
//!    healthcheck loop will evict the rule if it can't reach the IP.
//!
//! Step 2 will silently happen for any operator who has a stack on a
//! non-proxy bridge and didn't join `isengard-proxy`. The discovery returns
//! the bridge IP; pingora can't reach it cross-bridge; the healthcheck
//! evicts. The operator gets a "no upstream" error and a clear next step
//! (join `isengard-proxy`). That tradeoff is documented in `docker/README.md`.
//!
//! Note on the legacy single-network field: the pre-0.5 bollard-direct
//! path had a third "last-resort" rule that read
//! `NetworkSettings.IPAddress` (the legacy default-bridge field). Phase
//! 0.5 dropped it because the trait surface doesn't carry it; legacy
//! default-bridge stacks now resolve via rule 2 if they have any other
//! network attachment, or fall back to 127.0.0.1 in the proxy. Operators
//! who depended on this should join `isengard-proxy`.

use std::collections::HashMap;

use bollard::secret::EndpointSettings;
use tracing::{debug, warn};

use crate::runtime::RuntimeBackend;

/// The shared external network name that pingora and routed containers must
/// both join for cross-stack reachability. Mirrors the Traefik `proxy:
/// external: true` recipe documented in `docker/README.md`.
pub const SHARED_PROXY_NETWORK: &str = "isengard-proxy";

/// Networks docker creates by default that should never be used as an
/// upstream IP source. `bridge` is the default-bridge driver: containers on
/// it can't be reliably named. `host` exposes no per-container IP. `none`
/// has no networking at all.
const DRIVER_ONLY_NETWORKS: &[&str] = &["bridge", "host", "none"];

/// Pick the best IP for an upstream from a map of network attachments.
///
/// Returns `None` if no usable IP is found; the caller should warn and
/// either fall back to `127.0.0.1` (preserves the legacy behavior) or skip
/// the rule.
///
/// Operates on the bollard `EndpointSettings` shape; preserved for the
/// existing unit-test corpus and any caller that already holds a bollard
/// inspect response. The trait-driven path uses [`pick_snapshot_ip`].
pub fn pick_container_ip(networks: &HashMap<String, EndpointSettings>) -> Option<String> {
    // Rule 1: prefer the shared proxy network.
    if let Some(settings) = networks.get(SHARED_PROXY_NETWORK) {
        if let Some(ip) = settings.ip_address.as_deref().filter(|s| !s.is_empty()) {
            debug!(
                network = SHARED_PROXY_NETWORK,
                ip, "discovery: picked shared-proxy IP"
            );
            return Some(ip.to_string());
        }
    }

    // Rule 2: first non-driver network with a non-empty IP. Sort by network
    // name for determinism: without a sort, HashMap iteration order is
    // randomised per-process and would make the picked IP non-reproducible
    // across agent restarts when a container has multiple bridge networks.
    let mut sorted: Vec<(&String, &EndpointSettings)> = networks
        .iter()
        .filter(|(name, _)| !DRIVER_ONLY_NETWORKS.contains(&name.as_str()))
        .collect();
    sorted.sort_by_key(|(a, _)| *a);

    for (name, settings) in sorted {
        if let Some(ip) = settings.ip_address.as_deref().filter(|s| !s.is_empty()) {
            debug!(network = %name, ip, "discovery: picked non-proxy bridge IP");
            return Some(ip.to_string());
        }
    }

    None
}

/// Pure picker over the trait's `ContainerSnapshot.network_settings.ip_addresses`
/// map (`BTreeMap<String, IpAddr>`). Same selection rules as
/// [`pick_container_ip`]: shared proxy network first, then alphabetical
/// non-driver. The BTreeMap is already sort-stable; we still skip the
/// driver-only network names for parity.
fn pick_snapshot_ip(
    networks: &std::collections::BTreeMap<String, std::net::IpAddr>,
) -> Option<std::net::IpAddr> {
    if let Some(ip) = networks.get(SHARED_PROXY_NETWORK) {
        debug!(
            network = SHARED_PROXY_NETWORK,
            ip = %ip,
            "discovery: picked shared-proxy IP",
        );
        return Some(*ip);
    }
    for (name, ip) in networks.iter() {
        if DRIVER_ONLY_NETWORKS.contains(&name.as_str()) {
            continue;
        }
        debug!(network = %name, ip = %ip, "discovery: picked non-proxy bridge IP");
        return Some(*ip);
    }
    None
}

/// Resolve the container IP for a routing rule by calling
/// [`crate::runtime::RuntimeBackend::inspect_container`]. The agent gets
/// the container reference (name or id) from
/// `RoutingRule.upstream.container_id`, which the controller populates
/// from the labels report's container name.
///
/// Returns `None` (with a WARN log) on any of:
/// - The backend call errors (container exited between report and apply,
///   daemon unreachable, etc.)
/// - Inspect returns `None` (container gone)
/// - The picker finds no usable IP across the snapshot's network attachments
///
/// The caller decides what to do with `None`: today the proxy logs and
/// uses `127.0.0.1` as a last resort (matching pre-fix behavior so a
/// regression in discovery doesn't take the entire proxy offline).
pub async fn resolve_container_ip(
    backend: &dyn RuntimeBackend,
    container_ref: &str,
) -> Option<String> {
    let snap = match backend.inspect_container(container_ref).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            warn!(
                container = %container_ref,
                "discovery: inspect returned None (container gone?)",
            );
            return None;
        }
        Err(e) => {
            warn!(
                container = %container_ref,
                error = %e,
                "discovery: inspect_container failed",
            );
            return None;
        }
    };

    if let Some(ip) = pick_snapshot_ip(&snap.network_settings.ip_addresses) {
        return Some(ip.to_string());
    }
    warn!(
        container = %container_ref,
        "discovery: no usable IP found in NetworkSettings",
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        ContainerSnapshot, ContainerState, HealthState, HealthcheckSpec, ListFilter, LogChunk,
        LogOptions, NetworkSettings, RuntimeError, RuntimeEvent,
    };
    use async_trait::async_trait;
    use futures_util::Stream;
    use std::collections::BTreeMap;
    use std::net::IpAddr;
    use std::pin::Pin;
    use std::time::SystemTime;

    fn endpoint(ip: &str) -> EndpointSettings {
        EndpointSettings {
            ip_address: Some(ip.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn empty_map_returns_none() {
        let nets: HashMap<String, EndpointSettings> = HashMap::new();
        assert_eq!(pick_container_ip(&nets), None);
    }

    #[test]
    fn shared_proxy_network_wins_when_present() {
        let mut nets = HashMap::new();
        nets.insert("hello_default".into(), endpoint("172.20.0.5"));
        nets.insert(SHARED_PROXY_NETWORK.into(), endpoint("192.168.99.10"));
        nets.insert("other".into(), endpoint("10.0.0.5"));
        assert_eq!(
            pick_container_ip(&nets).as_deref(),
            Some("192.168.99.10"),
            "isengard-proxy should win regardless of insertion order"
        );
    }

    #[test]
    fn shared_proxy_with_empty_ip_falls_through() {
        // The container is attached to isengard-proxy but Docker hasn't
        // populated the IP yet (race during start). Fall back to the next
        // viable network rather than installing an empty IP.
        let mut nets = HashMap::new();
        nets.insert(SHARED_PROXY_NETWORK.into(), endpoint(""));
        nets.insert("hello_default".into(), endpoint("172.20.0.5"));
        assert_eq!(
            pick_container_ip(&nets).as_deref(),
            Some("172.20.0.5"),
            "empty isengard-proxy IP should fall through to the bridge"
        );
    }

    #[test]
    fn bridge_only_returns_first_non_driver_network() {
        // Legacy stack: container has a default-bridge attachment plus its
        // compose-project bridge. We must skip `bridge` and pick the
        // compose network.
        let mut nets = HashMap::new();
        nets.insert("bridge".into(), endpoint("172.17.0.2"));
        nets.insert("hello_default".into(), endpoint("172.20.0.5"));
        assert_eq!(
            pick_container_ip(&nets).as_deref(),
            Some("172.20.0.5"),
            "driver-only `bridge` should be skipped"
        );
    }

    #[test]
    fn bridge_host_none_all_skipped() {
        let mut nets = HashMap::new();
        nets.insert("bridge".into(), endpoint("172.17.0.2"));
        nets.insert("host".into(), endpoint("0.0.0.0"));
        nets.insert("none".into(), endpoint(""));
        assert_eq!(
            pick_container_ip(&nets),
            None,
            "all driver-only networks must be skipped, returning None"
        );
    }

    #[test]
    fn multiple_bridges_pick_alphabetical_first() {
        // Determinism: HashMap iteration order is randomised; we must
        // sort so the agent picks the same network across restarts.
        let mut nets = HashMap::new();
        nets.insert("zeta".into(), endpoint("10.0.0.99"));
        nets.insert("alpha".into(), endpoint("10.0.0.1"));
        nets.insert("middle".into(), endpoint("10.0.0.50"));
        assert_eq!(
            pick_container_ip(&nets).as_deref(),
            Some("10.0.0.1"),
            "alphabetical-first network should win for stable picks"
        );
    }

    #[test]
    fn proxy_network_only() {
        // Modern stack: only the shared proxy network is attached.
        let mut nets = HashMap::new();
        nets.insert(SHARED_PROXY_NETWORK.into(), endpoint("192.168.99.10"));
        assert_eq!(pick_container_ip(&nets).as_deref(), Some("192.168.99.10"));
    }

    #[test]
    fn empty_ips_everywhere_returns_none() {
        let mut nets = HashMap::new();
        nets.insert("a".into(), endpoint(""));
        nets.insert("b".into(), endpoint(""));
        assert_eq!(pick_container_ip(&nets), None);
    }

    // --- snapshot-driven picker tests (Phase 0.5) ---------------------------

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn snapshot_picker_prefers_shared_proxy_network() {
        let mut nets: BTreeMap<String, IpAddr> = BTreeMap::new();
        nets.insert("alpha".into(), ip("10.0.0.1"));
        nets.insert(SHARED_PROXY_NETWORK.into(), ip("192.168.99.10"));
        nets.insert("zeta".into(), ip("10.0.0.99"));
        assert_eq!(pick_snapshot_ip(&nets), Some(ip("192.168.99.10")));
    }

    #[test]
    fn snapshot_picker_alphabetical_non_driver_when_no_proxy() {
        let mut nets: BTreeMap<String, IpAddr> = BTreeMap::new();
        nets.insert("zeta".into(), ip("10.0.0.99"));
        nets.insert("alpha".into(), ip("10.0.0.1"));
        nets.insert("bridge".into(), ip("172.17.0.2"));
        assert_eq!(pick_snapshot_ip(&nets), Some(ip("10.0.0.1")));
    }

    #[test]
    fn snapshot_picker_returns_none_on_driver_only() {
        let mut nets: BTreeMap<String, IpAddr> = BTreeMap::new();
        nets.insert("bridge".into(), ip("172.17.0.2"));
        nets.insert("host".into(), ip("127.0.0.1"));
        assert_eq!(pick_snapshot_ip(&nets), None);
    }

    // --- resolve_container_ip tests with a mock backend ---------------------

    #[derive(Debug, Default)]
    struct MockBackend {
        snapshots: std::collections::HashMap<String, ContainerSnapshot>,
    }

    #[async_trait]
    impl RuntimeBackend for MockBackend {
        async fn ensure_image(&self, _r: &str) -> Result<String, RuntimeError> {
            Ok(String::new())
        }
        async fn create_container(
            &self,
            _s: &crate::runtime::ContainerCreateSpec,
        ) -> Result<String, RuntimeError> {
            Ok(String::new())
        }
        async fn start_container(&self, _id: &str) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn stop_container(&self, _id: &str, _t: u32) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn remove_container(&self, _id: &str, _f: bool) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn list_containers(
            &self,
            _f: ListFilter,
        ) -> Result<Vec<ContainerSnapshot>, RuntimeError> {
            Ok(Vec::new())
        }
        async fn inspect_container(
            &self,
            id: &str,
        ) -> Result<Option<ContainerSnapshot>, RuntimeError> {
            Ok(self.snapshots.get(id).cloned())
        }
        async fn connect_network(&self, _c: &str, _n: &str) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn disconnect_network(&self, _c: &str, _n: &str) -> Result<(), RuntimeError> {
            Ok(())
        }
        fn stream_logs(
            &self,
            _id: &str,
            _o: LogOptions,
        ) -> Pin<Box<dyn Stream<Item = LogChunk> + Send>> {
            Box::pin(futures_util::stream::empty())
        }
        fn stream_events(&self) -> Pin<Box<dyn Stream<Item = RuntimeEvent> + Send>> {
            Box::pin(futures_util::stream::empty())
        }
        async fn run_healthcheck(
            &self,
            _id: &str,
            _hc: &HealthcheckSpec,
        ) -> Result<HealthState, RuntimeError> {
            Ok(HealthState::Healthy)
        }
        fn name(&self) -> &'static str {
            "mock"
        }
    }

    fn snap_with_networks(id: &str, networks: &[(&str, &str)]) -> ContainerSnapshot {
        let mut ns = NetworkSettings::default();
        for (name, ip_str) in networks {
            ns.ip_addresses.insert((*name).to_string(), ip(ip_str));
        }
        ContainerSnapshot {
            id: id.into(),
            name: id.into(),
            image: "nginx".into(),
            state: ContainerState::Running,
            stack: None,
            service: None,
            labels: BTreeMap::new(),
            created_at: SystemTime::UNIX_EPOCH,
            started_at: None,
            finished_at: None,
            exit_code: None,
            restart_count: 0,
            network_settings: ns,
        }
    }

    #[tokio::test]
    async fn discovery_resolves_to_named_network_first() {
        let mut mock = MockBackend::default();
        mock.snapshots.insert(
            "web".into(),
            snap_with_networks(
                "web",
                &[
                    ("alpha", "10.0.0.1"),
                    (SHARED_PROXY_NETWORK, "192.168.99.10"),
                    ("zeta", "10.0.0.99"),
                ],
            ),
        );
        let resolved = resolve_container_ip(&mock, "web").await;
        assert_eq!(resolved.as_deref(), Some("192.168.99.10"));
    }

    #[tokio::test]
    async fn discovery_falls_back_to_alphabetical_non_driver() {
        let mut mock = MockBackend::default();
        mock.snapshots.insert(
            "legacy".into(),
            snap_with_networks(
                "legacy",
                &[
                    ("bridge", "172.17.0.2"),
                    ("zeta", "10.0.0.99"),
                    ("hello_default", "172.20.0.5"),
                ],
            ),
        );
        let resolved = resolve_container_ip(&mock, "legacy").await;
        // BTreeMap sorts alphabetically: hello_default < zeta, bridge skipped.
        assert_eq!(resolved.as_deref(), Some("172.20.0.5"));
    }

    #[tokio::test]
    async fn discovery_returns_none_when_container_missing() {
        let mock = MockBackend::default();
        let resolved = resolve_container_ip(&mock, "ghost").await;
        assert!(resolved.is_none());
    }
}
