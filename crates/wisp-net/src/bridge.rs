//! Bridge command planner.
//!
//! Side-effect-free: builds a `Vec<IpCommand>` describing the `ip` calls
//! required to create or delete a wisp bridge. The actual subprocess
//! invocations land in dispatch B's `cmd.rs`. The planner-vs-executor
//! split mirrors `wisp::mount::plan_mounts` from phase 0.1 and keeps the
//! Mac dev loop unit-testable.

use crate::error::WispNetError;
use std::net::Ipv4Addr;

/// One wisp-managed bridge network.
///
/// Construct via [`Network::new`], which derives a sane default
/// [`Network::bridge`] name and the [`Network::gateway`] from the subnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub name: String,
    pub bridge: String,
    pub subnet: ipnet::Ipv4Net,
    pub gateway: Ipv4Addr,
}

/// Maximum interface name length the Linux kernel accepts (`IFNAMSIZ - 1`).
/// Bridge names longer than this are rejected by `ip link add`.
const IFNAMSIZ_MAX: usize = 15;

impl Network {
    /// Build a `Network` from a name and subnet.
    ///
    /// - Bridge name: `wbr-<sanitized-name>`, truncated to 15 bytes
    ///   (the kernel's `IFNAMSIZ - 1` cap). The `wbr-` prefix is the
    ///   discriminator `bridge::list_wisp_bridges` keys off in dispatch
    ///   B.
    /// - Gateway: the first usable host address inside `subnet`
    ///   (e.g. `10.83.0.1` for `10.83.0.0/24`).
    pub fn new(name: &str, subnet: ipnet::Ipv4Net) -> Result<Self, WispNetError> {
        if name.is_empty() {
            return Err(WispNetError::Bridge(
                "network name must be non-empty".into(),
            ));
        }
        let bridge = bridge_name_for(name);
        let gateway = subnet.hosts().next().ok_or_else(|| {
            WispNetError::Bridge(format!(
                "subnet {subnet} has no usable host addresses for a gateway"
            ))
        })?;
        Ok(Self {
            name: name.to_string(),
            bridge,
            subnet,
            gateway,
        })
    }
}

/// Derive the host-side bridge interface name for a given wisp network
/// name. Public for tests; production callers go through `Network::new`.
pub(crate) fn bridge_name_for(name: &str) -> String {
    let candidate = format!("wbr-{name}");
    if candidate.len() <= IFNAMSIZ_MAX {
        candidate
    } else {
        // Byte-wise truncation; wisp network names are ASCII-only by
        // contract (the CLI rejects non-ASCII before this is reached),
        // so this never splits a multibyte char in practice.
        candidate[..IFNAMSIZ_MAX].to_string()
    }
}

/// One `ip ...` invocation, ready to be passed to dispatch B's runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpCommand {
    pub args: Vec<String>,
}

impl IpCommand {
    fn new<S: Into<String>>(args: impl IntoIterator<Item = S>) -> Self {
        Self {
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// Plan the `ip` invocations needed to create the bridge and bring it
/// up with the gateway address assigned. Order matters; dispatch B walks
/// this vector top-to-bottom.
///
/// 1. `ip link add name <bridge> type bridge`
/// 2. `ip addr add <gateway>/<prefix> dev <bridge>`
/// 3. `ip link set <bridge> up`
pub fn plan_create(net: &Network) -> Vec<IpCommand> {
    let prefix = net.subnet.prefix_len();
    vec![
        IpCommand::new(["link", "add", "name", &net.bridge, "type", "bridge"]),
        IpCommand::new([
            "addr",
            "add",
            &format!("{}/{}", net.gateway, prefix),
            "dev",
            &net.bridge,
        ]),
        IpCommand::new(["link", "set", &net.bridge, "up"]),
    ]
}

/// Plan the `ip` invocation that tears the bridge back down.
///
/// `ip link delete <bridge> type bridge`. Dispatch B treats a missing
/// link as a no-op so the call is idempotent on repeated `Network::delete`.
pub fn plan_delete(net: &Network) -> Vec<IpCommand> {
    vec![IpCommand::new([
        "link",
        "delete",
        &net.bridge,
        "type",
        "bridge",
    ])]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net_default() -> Network {
        Network::new("app", "10.83.0.0/24".parse().unwrap()).unwrap()
    }

    #[test]
    fn plan_create_emits_link_add_addr_set_up_in_order() {
        let plan = plan_create(&net_default());
        assert_eq!(plan.len(), 3);
        assert_eq!(
            plan[0].args,
            vec!["link", "add", "name", "wbr-app", "type", "bridge"]
        );
        assert_eq!(
            plan[1].args,
            vec!["addr", "add", "10.83.0.1/24", "dev", "wbr-app"]
        );
        assert_eq!(plan[2].args, vec!["link", "set", "wbr-app", "up"]);
    }

    #[test]
    fn plan_delete_emits_link_delete() {
        let plan = plan_delete(&net_default());
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].args,
            vec!["link", "delete", "wbr-app", "type", "bridge"]
        );
    }

    #[test]
    fn bridge_name_truncates_to_15_chars() {
        // "wbr-" is 4 chars; need 12+ chars of name to overflow.
        let name = "verylongnetname";
        let net = Network::new(name, "10.83.0.0/24".parse().unwrap()).unwrap();
        assert!(
            net.bridge.len() <= 15,
            "bridge name {} exceeds IFNAMSIZ-1",
            net.bridge
        );
        assert_eq!(net.bridge, "wbr-verylongnet");
    }

    #[test]
    fn bridge_name_preserves_short_names_intact() {
        let net = Network::new("app", "10.83.0.0/24".parse().unwrap()).unwrap();
        assert_eq!(net.bridge, "wbr-app");
    }

    #[test]
    fn gateway_defaults_to_first_usable() {
        let cases = &[
            ("10.83.0.0/24", Ipv4Addr::new(10, 83, 0, 1)),
            ("192.168.50.0/24", Ipv4Addr::new(192, 168, 50, 1)),
            ("172.20.4.0/22", Ipv4Addr::new(172, 20, 4, 1)),
        ];
        for (subnet, expected_gw) in cases {
            let net = Network::new("test", subnet.parse().unwrap()).unwrap();
            assert_eq!(net.gateway, *expected_gw, "subnet {subnet}");
        }
    }

    #[test]
    fn plan_create_uses_subnet_prefix_in_addr() {
        let net = Network::new("wide", "172.20.0.0/16".parse().unwrap()).unwrap();
        let plan = plan_create(&net);
        // The /16 prefix must show up verbatim in the addr add command.
        assert_eq!(plan[1].args[2], "172.20.0.1/16");
    }

    #[test]
    fn empty_network_name_is_rejected() {
        let result = Network::new("", "10.83.0.0/24".parse().unwrap());
        assert!(matches!(result, Err(WispNetError::Bridge(_))));
    }
}
