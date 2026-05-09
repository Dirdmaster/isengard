//! Bridge command planner.
//!
//! Side-effect-free: builds a `Vec<IpCommand>` describing the `ip` calls
//! required to create or delete a wisp bridge. The actual subprocess
//! invocations land in dispatch B's `cmd.rs`. The planner-vs-executor
//! split mirrors `wisp::mount::plan_mounts` from phase 0.1 and keeps the
//! Mac dev loop unit-testable.

use crate::cmd;
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

/// Idempotent bridge create.
///
/// 1. If `<bridge>` already exists, verify its assigned address matches
///    `<gateway>/<prefix>`. Mismatch -> [`WispNetError::Conflict`]. Match
///    -> no-op.
/// 2. Otherwise walk [`plan_create`] in order, calling `ip` for each
///    step. On any failure mid-way, run [`plan_delete`] best-effort to
///    avoid leaving a half-configured bridge.
pub fn ensure(net: &Network) -> Result<(), WispNetError> {
    if bridge_exists(&net.bridge)? {
        verify_bridge_addr_matches(net)?;
        return Ok(());
    }

    let plan = plan_create(net);
    for (idx, step) in plan.iter().enumerate() {
        let args: Vec<&str> = step.args.iter().map(String::as_str).collect();
        if let Err(e) = cmd::run_ip(&args) {
            // Best-effort cleanup: if any step has executed (idx > 0)
            // the bridge may now exist partially; tear it down so a
            // retry starts from a clean slate.
            best_effort_delete(net);
            return Err(WispNetError::Bridge(format!(
                "ensure failed at step {idx} ({:?}): {e}",
                step.args
            )));
        }
    }
    Ok(())
}

/// Idempotent bridge delete.
///
/// Walks [`plan_delete`] and tolerates the kernel's "Cannot find device"
/// error so repeated `delete` calls (e.g. retry-on-create-failure) are
/// safe.
pub fn delete(net: &Network) -> Result<(), WispNetError> {
    let plan = plan_delete(net);
    for step in &plan {
        let args: Vec<&str> = step.args.iter().map(String::as_str).collect();
        match cmd::run_ip(&args) {
            Ok(_) => {}
            Err(WispNetError::Cmd { stderr, .. }) if is_missing_device(&stderr) => {
                // Already gone; no-op.
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// List bridge interfaces managed by wisp.
///
/// Reads `/sys/class/net/` and filters entries whose name starts with
/// `wbr-` (the prefix [`Network::new`] applies). Used by `wisp net ls`
/// + the integration tests.
pub fn list_wisp_bridges() -> Result<Vec<String>, WispNetError> {
    let mut out = Vec::new();
    let dir = match std::fs::read_dir("/sys/class/net") {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(WispNetError::Io(e)),
    };
    for entry in dir {
        let entry = entry.map_err(WispNetError::Io)?;
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with("wbr-") {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

fn bridge_exists(name: &str) -> Result<bool, WispNetError> {
    match cmd::run_ip(&["link", "show", name]) {
        Ok(_) => Ok(true),
        Err(WispNetError::Cmd { stderr, .. }) if is_missing_device(&stderr) => Ok(false),
        Err(e) => Err(e),
    }
}

fn is_missing_device(stderr: &str) -> bool {
    // iproute2 prints "Cannot find device" for missing links and
    // "Device does not exist" on some older versions. Match either.
    let s = stderr.to_ascii_lowercase();
    s.contains("cannot find device") || s.contains("does not exist")
}

fn verify_bridge_addr_matches(net: &Network) -> Result<(), WispNetError> {
    let want = format!("{}/{}", net.gateway, net.subnet.prefix_len());
    let stdout = cmd::run_ip(&["addr", "show", &net.bridge])?;
    if stdout
        .lines()
        .any(|line| line.trim_start().starts_with("inet ") && line.contains(&want))
    {
        Ok(())
    } else {
        Err(WispNetError::Conflict(format!(
            "bridge {} exists but does not have address {} (got: {})",
            net.bridge,
            want,
            stdout.trim()
        )))
    }
}

fn best_effort_delete(net: &Network) {
    let _ = delete(net);
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
