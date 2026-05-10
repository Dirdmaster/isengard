//! Networking knobs the runtime exposes to its callers.
//!
//! `wisp` owns these types because the runtime owns the lifecycle: a
//! `NetworkSpec` rides alongside a bundle through `Runtime::create_with_network`
//! / `Runtime::start`, and a `NetworkAttachmentRecord` is what
//! `lifecycle::start_container` writes to `state.json` once the container
//! actually has a veth + IP. The sibling `wisp-net` crate consumes these
//! types and is responsible for the syscalls that materialise them
//! (bridge, veth, IPAM, iptables). This direction keeps the `wisp`
//! crate dependency-free of the `wisp-net` crate while still letting
//! both ends agree on the wire shape.
//!
//! All fields are plain stdlib / `serde` types so `state.json` reads
//! and writes round-trip without external types in the schema.

use std::net::{IpAddr, Ipv4Addr};

use serde::{Deserialize, Serialize};

/// Layer-4 protocol for a published port. `Tcp` and `Udp` are the only
/// values the iptables planner understands. `Copy` because the field
/// shows up frequently in by-value contexts (rule planners, CLI parse).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortProtocol {
    Tcp,
    Udp,
}

impl PortProtocol {
    /// CLI flag value: `iptables -p <name>`.
    pub fn cli_name(self) -> &'static str {
        match self {
            PortProtocol::Tcp => "tcp",
            PortProtocol::Udp => "udp",
        }
    }
}

/// One published-port directive: `host_ip:host_port -> container:container_port`.
///
/// `host_ip` is `0.0.0.0` (IPv4 unspecified) for the default
/// `--port 18080:80` case. Phase 0.3 only emits IPv4 rules; the
/// stdlib `IpAddr` type leaves room for an IPv6 PortPublish in a
/// later phase without a schema break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortPublish {
    pub host_ip: IpAddr,
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: PortProtocol,
}

impl PortPublish {
    /// Convenience: build an IPv4 published port bound to `0.0.0.0`.
    pub fn v4_any(host_port: u16, container_port: u16, protocol: PortProtocol) -> Self {
        Self {
            host_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            host_port,
            container_port,
            protocol,
        }
    }
}

/// Where the container's `/etc/resolv.conf` should come from.
///
/// `HostCopy` reads the host's `/etc/resolv.conf`, filtering out
/// loopback nameservers (a 127.0.0.x stub resolver on the host is
/// useless to a container on a different bridge subnet).
///
/// `Static(vec)` writes one `nameserver <addr>` line per entry.
///
/// `None` skips the write entirely; the operator is responsible for
/// providing resolv.conf via a bind mount or similar.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolvSource {
    #[default]
    HostCopy,
    Static(Vec<IpAddr>),
    None,
}

/// What the operator asked for: which network to attach to, which
/// ports to publish, and how to populate resolv.conf. This is the
/// declarative input; the runtime turns it into a
/// [`NetworkAttachmentRecord`] at start time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSpec {
    pub network_name: String,
    #[serde(default)]
    pub ports: Vec<PortPublish>,
    #[serde(default)]
    pub resolv_source: ResolvSource,
}

impl NetworkSpec {
    /// Build a spec with the given network name, no ports, and the
    /// default `HostCopy` resolv source. Callers tack on `.with_ports`
    /// or set `resolv_source` directly.
    pub fn new(network_name: impl Into<String>) -> Self {
        Self {
            network_name: network_name.into(),
            ports: Vec::new(),
            resolv_source: ResolvSource::HostCopy,
        }
    }

    /// Builder-style helper to attach a port-publish list.
    pub fn with_ports(mut self, ports: Vec<PortPublish>) -> Self {
        self.ports = ports;
        self
    }
}

/// What the runtime actually attached: the resolved bridge name, the
/// IPv4 address we allocated, and the veth pair we created. Persisted
/// in `state.json` once `start_container` has run, so `Runtime::delete`
/// can revoke iptables rules / delete the veth / release the IP without
/// reconstructing them.
///
/// `container_id` lets the detach path replan the same iptables
/// rules (the rule comments are keyed by id) without the caller
/// having to thread the id through alongside the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAttachmentRecord {
    pub container_id: String,
    pub network_name: String,
    pub bridge: String,
    pub ipv4: Ipv4Addr,
    pub veth_host: String,
    pub veth_container: String,
    pub ports: Vec<PortPublish>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_protocol_cli_names() {
        assert_eq!(PortProtocol::Tcp.cli_name(), "tcp");
        assert_eq!(PortProtocol::Udp.cli_name(), "udp");
    }

    #[test]
    fn port_publish_v4_any_uses_unspecified_ipv4() {
        let p = PortPublish::v4_any(18080, 80, PortProtocol::Tcp);
        assert_eq!(p.host_ip, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(p.host_port, 18080);
        assert_eq!(p.container_port, 80);
        assert_eq!(p.protocol, PortProtocol::Tcp);
    }

    #[test]
    fn network_spec_new_defaults() {
        let s = NetworkSpec::new("app");
        assert_eq!(s.network_name, "app");
        assert!(s.ports.is_empty());
        assert!(matches!(s.resolv_source, ResolvSource::HostCopy));
    }

    #[test]
    fn network_spec_with_ports_sets_ports() {
        let p = vec![PortPublish::v4_any(8080, 80, PortProtocol::Tcp)];
        let s = NetworkSpec::new("app").with_ports(p.clone());
        assert_eq!(s.ports, p);
    }

    #[test]
    fn resolv_source_serde_round_trip() {
        let cases = vec![
            ResolvSource::HostCopy,
            ResolvSource::Static(vec!["1.1.1.1".parse().unwrap()]),
            ResolvSource::None,
        ];
        for c in cases {
            let json = serde_json::to_string(&c).unwrap();
            let back: ResolvSource = serde_json::from_str(&json).unwrap();
            assert_eq!(c, back);
        }
    }

    #[test]
    fn network_spec_serde_round_trip() {
        let s = NetworkSpec {
            network_name: "app".into(),
            ports: vec![PortPublish::v4_any(8080, 80, PortProtocol::Tcp)],
            resolv_source: ResolvSource::Static(vec!["8.8.8.8".parse().unwrap()]),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: NetworkSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn network_attachment_record_serde_round_trip() {
        let rec = NetworkAttachmentRecord {
            container_id: "ctr1".into(),
            network_name: "app".into(),
            bridge: "wbr-app".into(),
            ipv4: Ipv4Addr::new(10, 83, 0, 2),
            veth_host: "wveth-h-abc123".into(),
            veth_container: "wveth-c-abc123".into(),
            ports: vec![PortPublish::v4_any(18080, 80, PortProtocol::Tcp)],
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: NetworkAttachmentRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, back);
    }
}
