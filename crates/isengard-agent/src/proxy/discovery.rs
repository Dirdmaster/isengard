//! Container IP discovery for the proxy.
//!
//! The controller never knows the container's docker-network IP: it ships
//! `RoutingRule` messages with `container_id` set to the container NAME and
//! `container_ip` empty. The agent has to fill the IP in at apply-time from
//! its local Docker view.
//!
//! This module owns the discovery logic. Two halves:
//!
//! 1. [`pick_container_ip`]: pure function that ranks candidate IPs from a
//!    `network_settings.networks` map. Pure so it's exhaustively testable
//!    without a Docker daemon.
//! 2. [`resolve_container_ip`]: thin async wrapper that calls
//!    `bollard::Docker::inspect_container` and feeds the result into the
//!    picker.
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
//! 3. As a last resort, use whatever non-empty IP the inspect response
//!    surfaces on `network_settings.ip_address` (the legacy single-network
//!    field that bollard fills for default-bridge attachments).
//!
//! Step 2 will silently happen for any operator who has a stack on a
//! non-proxy bridge and didn't join `isengard-proxy`. The discovery returns
//! the bridge IP; pingora can't reach it cross-bridge; the healthcheck
//! evicts. The operator gets a "no upstream" error and a clear next step
//! (join `isengard-proxy`). That tradeoff is documented in `docker/README.md`.

use std::collections::HashMap;

use bollard::Docker;
use bollard::secret::EndpointSettings;
use tracing::{debug, warn};

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
    sorted.sort_by(|(a, _), (b, _)| a.cmp(b));

    for (name, settings) in sorted {
        if let Some(ip) = settings.ip_address.as_deref().filter(|s| !s.is_empty()) {
            debug!(network = %name, ip, "discovery: picked non-proxy bridge IP");
            return Some(ip.to_string());
        }
    }

    None
}

/// Resolve the container IP for a routing rule by calling
/// `inspect_container` against the local Docker daemon. The agent gets the
/// container reference (name or id) from `RoutingRule.upstream.container_id`,
/// which the controller populates from the labels report's container name.
///
/// Returns `None` (with a WARN log) on any of:
/// - Docker call fails (container exited between report and apply, daemon
///   unreachable, etc.)
/// - The inspect response carries no `network_settings.networks` map
/// - `pick_container_ip` finds no usable IP
///
/// The caller decides what to do with `None`: today the proxy logs and
/// uses `127.0.0.1` as a last resort (matching pre-fix behavior so a
/// regression in discovery doesn't take the entire proxy offline).
pub async fn resolve_container_ip(docker: &Docker, container_ref: &str) -> Option<String> {
    let inspect = match docker.inspect_container(container_ref, None).await {
        Ok(i) => i,
        Err(e) => {
            warn!(
                container = %container_ref,
                error = %e,
                "discovery: inspect_container failed"
            );
            return None;
        }
    };

    let networks = inspect
        .network_settings
        .as_ref()
        .and_then(|s| s.networks.as_ref());

    if let Some(nets) = networks {
        if let Some(ip) = pick_container_ip(nets) {
            return Some(ip);
        }
    }

    // Last-resort: legacy single-network field. Some bollard responses fill
    // this for default-bridge attachments even when `networks` is empty.
    let legacy = inspect
        .network_settings
        .as_ref()
        .and_then(|s| s.ip_address.as_deref())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    if let Some(ip) = &legacy {
        debug!(
            container = %container_ref,
            ip, "discovery: used legacy NetworkSettings.IPAddress"
        );
    } else {
        warn!(
            container = %container_ref,
            "discovery: no usable IP found in NetworkSettings"
        );
    }
    legacy
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
