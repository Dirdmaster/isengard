//! iptables rule planner.
//!
//! Side-effect-free: builds a [`RuleSet`] of `creates` and `deletes` for
//! a given [`Network`] (network-level masquerade + forward rules) or for
//! a single container attachment (per-port DNAT + forward).
//!
//! Cleanup discipline: every rule carries a `wisp:<scope>:<purpose>`
//! comment so dispatch B's revoke path can locate them via `iptables-save
//! | grep "wisp:<scope>"` even after partial failure. The scope is the
//! sanitized network name for network rules and the sanitized container
//! id for attachment rules.
//!
//! For dispatch A this module is pure logic. Dispatch B (`apply` /
//! `revoke`) walks the [`RuleSet`] and shells out to `iptables`.
//!
//! The matching `creates` / `deletes` invariant is load-bearing: dispatch
//! B's revoke path uses `deletes` directly. Tests assert the
//! correspondence so any future planner edit that breaks symmetry trips
//! the suite.

use crate::bridge::Network;
use crate::cmd;
use crate::error::WispNetError;
use std::net::Ipv4Addr;

/// Iptables table: `nat`, `filter`, `mangle`. Phase 0.3 only emits into
/// `nat` and `filter`; `mangle` is here for future-proofing against
/// rate-limiting or DSCP-tag rules in a later phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Nat,
    Filter,
    Mangle,
}

impl Table {
    /// CLI flag value: `iptables -t <name>`. `filter` is the default
    /// table so we still pass the flag for explicitness.
    pub fn cli_name(self) -> &'static str {
        match self {
            Table::Nat => "nat",
            Table::Filter => "filter",
            Table::Mangle => "mangle",
        }
    }
}

/// One iptables rule, broken into the table, the chain it lands on, the
/// match + target args (everything after `-A <chain>` / `-D <chain>`),
/// and an optional grep-friendly comment.
///
/// Dispatch B emits the actual command as:
///
/// ```text
/// iptables -t <table> {-A | -D} <chain> <args...> -m comment --comment "<comment>"
/// ```
///
/// The comment is stored separately rather than baked into `args` so
/// alternate emitters (e.g. `iptables-restore` syntax) can format it
/// however they need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub table: Table,
    pub chain: String,
    pub args: Vec<String>,
    pub comment: Option<String>,
}

/// A pair of create + delete plans for the same logical group of rules.
///
/// Invariant: every entry in `creates` has a matching entry in `deletes`
/// with the same table / chain / args / comment, differing only in the
/// `-A` (in `creates`) vs `-D` (in `deletes`) verb that dispatch B
/// supplies. Tests in this module assert that invariant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleSet {
    pub creates: Vec<Rule>,
    pub deletes: Vec<Rule>,
}

impl RuleSet {
    fn append(&mut self, rule: Rule) {
        self.creates.push(rule.clone());
        self.deletes.push(rule);
    }

    /// Combine two RuleSets. Used by the runtime when applying network
    /// + attachment rules in a single batch.
    pub fn extend(&mut self, other: RuleSet) {
        self.creates.extend(other.creates);
        self.deletes.extend(other.deletes);
    }
}

/// Layer-4 protocol for a published port. `tcp` and `udp` are the only
/// values dispatch A emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// One published-port directive from `wisp run --port host:container`.
///
/// `host_ip` is `0.0.0.0` for the default `--port 18080:80` case.
/// Dispatch A doesn't currently restrict by host_ip in the PREROUTING
/// rule (parity with docker's default), but the field is here so a
/// future bind-to-specific-iface knob is additive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortPublish {
    pub host_ip: Ipv4Addr,
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: PortProtocol,
}

/// Sanitize a free-form id for use inside an iptables `--comment` value.
///
/// iptables comments are limited to 256 bytes and the CLI quotes the
/// argument, but stray colons / quotes / whitespace make the
/// `iptables-save | grep "wisp:<id>"` scan in dispatch B fragile. We
/// accept `[A-Za-z0-9._-]` and replace anything else with `_`.
fn sanitize_for_comment(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

fn comment_for(scope: &str, purpose: &str) -> String {
    format!("wisp:{}:{}", sanitize_for_comment(scope), purpose)
}

/// Plan the network-scope rules for a wisp bridge.
///
/// Emits four rule pairs (create + matching delete) tagged with the
/// `wisp:<network-name>:<purpose>` comment:
///
/// 1. `nat` POSTROUTING masquerade for the bridge subnet's outbound
///    traffic (skipped for traffic that stays on the bridge).
/// 2. `filter` FORWARD allow for intra-bridge container-to-container
///    traffic.
/// 3. `filter` FORWARD allow for outbound traffic leaving the bridge.
/// 4. `filter` FORWARD allow for inbound RELATED,ESTABLISHED responses.
pub fn plan_for_network(net: &Network) -> RuleSet {
    let mut rs = RuleSet::default();
    let subnet = net.subnet.to_string();

    // 1. MASQUERADE outbound traffic that does NOT exit the same bridge.
    rs.append(Rule {
        table: Table::Nat,
        chain: "POSTROUTING".into(),
        args: vec![
            "-s".into(),
            subnet.clone(),
            "!".into(),
            "-o".into(),
            net.bridge.clone(),
            "-j".into(),
            "MASQUERADE".into(),
        ],
        comment: Some(comment_for(&net.name, "masq")),
    });

    // 2. FORWARD intra-bridge.
    rs.append(Rule {
        table: Table::Filter,
        chain: "FORWARD".into(),
        args: vec![
            "-i".into(),
            net.bridge.clone(),
            "-o".into(),
            net.bridge.clone(),
            "-j".into(),
            "ACCEPT".into(),
        ],
        comment: Some(comment_for(&net.name, "bridge-to-bridge")),
    });

    // 3. FORWARD bridge -> elsewhere.
    rs.append(Rule {
        table: Table::Filter,
        chain: "FORWARD".into(),
        args: vec![
            "-i".into(),
            net.bridge.clone(),
            "!".into(),
            "-o".into(),
            net.bridge.clone(),
            "-j".into(),
            "ACCEPT".into(),
        ],
        comment: Some(comment_for(&net.name, "bridge-out")),
    });

    // 4. FORWARD elsewhere -> bridge for RELATED,ESTABLISHED.
    rs.append(Rule {
        table: Table::Filter,
        chain: "FORWARD".into(),
        args: vec![
            "!".into(),
            "-i".into(),
            net.bridge.clone(),
            "-o".into(),
            net.bridge.clone(),
            "-m".into(),
            "state".into(),
            "--state".into(),
            "RELATED,ESTABLISHED".into(),
            "-j".into(),
            "ACCEPT".into(),
        ],
        comment: Some(comment_for(&net.name, "bridge-in-related")),
    });

    // 5. POSTROUTING masquerade for loopback-sourced traffic that gets
    //    DNAT'd to the bridge (dispatch B finding: without this, the
    //    loopback OUTPUT DNAT in plan_for_attachment routes a 127.0.0.1
    //    -> 10.x packet onto the bridge, but the container can't ARP
    //    back to 127.0.0.1 and the connection times out). Pairs with
    //    `bridge::ensure` setting `route_localnet=1` on the bridge.
    rs.append(Rule {
        table: Table::Nat,
        chain: "POSTROUTING".into(),
        args: vec![
            "-o".into(),
            net.bridge.clone(),
            "-s".into(),
            "127.0.0.0/8".into(),
            "-j".into(),
            "MASQUERADE".into(),
        ],
        comment: Some(comment_for(&net.name, "loopback-snat")),
    });

    rs
}

/// Plan the per-attachment rules for a single container.
///
/// For each [`PortPublish`] entry, three rules are emitted (create +
/// matching delete) tagged with `wisp:<container-id>:<purpose>`:
///
/// 1. `nat` PREROUTING DNAT to the container ip:port (host-arriving
///    traffic).
/// 2. `nat` OUTPUT DNAT for `127.0.0.1:<host-port>` (loopback clients;
///    PREROUTING does not run for traffic originating on the host).
/// 3. `filter` FORWARD ACCEPT to the container ip:port (so the kernel's
///    default-deny FORWARD policy doesn't drop the DNAT'd packet).
///
/// `_net` is currently unused but kept in the signature so dispatch B
/// can scope rules per-network without an API break.
pub fn plan_for_attachment(
    _net: &Network,
    container_ip: Ipv4Addr,
    container_id: &str,
    ports: &[PortPublish],
) -> RuleSet {
    let mut rs = RuleSet::default();
    let dnat_target_for = |p: &PortPublish| format!("{}:{}", container_ip, p.container_port);

    for p in ports {
        let proto = p.protocol.cli_name();
        let host_port = p.host_port.to_string();
        let cport = p.container_port.to_string();
        let cip = container_ip.to_string();
        let target = dnat_target_for(p);

        // 1. PREROUTING DNAT.
        rs.append(Rule {
            table: Table::Nat,
            chain: "PREROUTING".into(),
            args: vec![
                "-p".into(),
                proto.into(),
                "--dport".into(),
                host_port.clone(),
                "-j".into(),
                "DNAT".into(),
                "--to-destination".into(),
                target.clone(),
            ],
            comment: Some(comment_for(container_id, "dnat")),
        });

        // 2. OUTPUT DNAT for loopback.
        rs.append(Rule {
            table: Table::Nat,
            chain: "OUTPUT".into(),
            args: vec![
                "-p".into(),
                proto.into(),
                "-d".into(),
                "127.0.0.1".into(),
                "--dport".into(),
                host_port.clone(),
                "-j".into(),
                "DNAT".into(),
                "--to-destination".into(),
                target,
            ],
            comment: Some(comment_for(container_id, "loopback-dnat")),
        });

        // 3. FORWARD accept the now-DNAT'd flow.
        rs.append(Rule {
            table: Table::Filter,
            chain: "FORWARD".into(),
            args: vec![
                "-p".into(),
                proto.into(),
                "-d".into(),
                cip,
                "--dport".into(),
                cport,
                "-j".into(),
                "ACCEPT".into(),
            ],
            comment: Some(comment_for(container_id, "fwd-accept")),
        });
    }

    rs
}

/// Build the full `iptables` arg list for a single rule.
///
/// `verb` is `"-A"` (creates) or `"-D"` (deletes). Output shape:
///
/// ```text
/// -t <table> <verb> <chain> <args...> [-m comment --comment <comment>]
/// ```
fn rule_to_args(rule: &Rule, verb: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(rule.args.len() + 8);
    out.push("-t".into());
    out.push(rule.table.cli_name().into());
    out.push(verb.into());
    out.push(rule.chain.clone());
    for a in &rule.args {
        out.push(a.clone());
    }
    if let Some(c) = &rule.comment {
        out.push("-m".into());
        out.push("comment".into());
        out.push("--comment".into());
        out.push(c.clone());
    }
    out
}

/// Apply a [`RuleSet`]: walk `creates` and run each via `iptables -A`.
///
/// On any failure, the rules already applied are revoked best-effort
/// before the original error is returned, so the host doesn't get
/// stuck with half a rule chain.
pub fn apply(rs: &RuleSet) -> Result<(), WispNetError> {
    let mut applied: Vec<&Rule> = Vec::new();
    for rule in &rs.creates {
        let args = rule_to_args(rule, "-A");
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match cmd::run_iptables(&arg_refs) {
            Ok(_) => applied.push(rule),
            Err(e) => {
                // Roll back what we already applied. Best-effort: a
                // failed delete here gets swallowed in favor of
                // surfacing the original apply error.
                for r in applied.iter().rev() {
                    let d_args = rule_to_args(r, "-D");
                    let d_refs: Vec<&str> = d_args.iter().map(String::as_str).collect();
                    let _ = cmd::run_iptables(&d_refs);
                }
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Revoke a [`RuleSet`]: walk `deletes` and run each via `iptables -D`.
///
/// Tolerates the kernel's "No chain/target/match by that name" error
/// (some distros' iptables CLI emits this when the rule is already
/// gone), so revoke is idempotent on repeat calls.
pub fn revoke(rs: &RuleSet) -> Result<(), WispNetError> {
    for rule in &rs.deletes {
        let args = rule_to_args(rule, "-D");
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match cmd::run_iptables(&arg_refs) {
            Ok(_) => {}
            Err(WispNetError::Cmd { stderr, .. }) if is_missing_rule(&stderr) => {
                // Already gone; treat as success.
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn is_missing_rule(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    // Covers iptables-legacy ("Bad rule (does a matching rule exist...")
    // and iptables-nft ("No chain/target/match by that name") variants.
    s.contains("no chain/target/match by that name")
        || s.contains("does a matching rule exist")
        || s.contains("no such file or directory")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net() -> Network {
        Network::new("app", "10.83.0.0/24".parse().unwrap()).unwrap()
    }

    #[test]
    fn network_rules_include_masquerade_and_forward() {
        let rs = plan_for_network(&net());
        assert_eq!(rs.creates.len(), 5);
        assert_eq!(rs.deletes.len(), 5);

        // 1. POSTROUTING masquerade in nat table.
        let masq = &rs.creates[0];
        assert_eq!(masq.table, Table::Nat);
        assert_eq!(masq.chain, "POSTROUTING");
        assert!(masq.args.contains(&"MASQUERADE".to_string()));
        assert!(masq.args.contains(&"10.83.0.0/24".to_string()));
        assert!(masq.args.contains(&"wbr-app".to_string()));

        // 2. Intra-bridge FORWARD.
        let intra = &rs.creates[1];
        assert_eq!(intra.table, Table::Filter);
        assert_eq!(intra.chain, "FORWARD");
        assert_eq!(
            intra.args,
            vec!["-i", "wbr-app", "-o", "wbr-app", "-j", "ACCEPT"]
        );

        // 3. Bridge -> elsewhere.
        let out = &rs.creates[2];
        assert!(out.args.contains(&"!".to_string()));
        assert!(out.args.contains(&"-o".to_string()));

        // 4. RELATED,ESTABLISHED inbound.
        let est = &rs.creates[3];
        assert!(est.args.contains(&"RELATED,ESTABLISHED".to_string()));
        assert!(est.args.contains(&"state".to_string()));
    }

    #[test]
    fn network_rules_include_loopback_snat() {
        // Without this rule, curl 127.0.0.1:<host-port> never reaches
        // the container (the kernel emits ARPs with src=127.0.0.1 onto
        // the bridge and the container can't reply). See
        // bridge::ensure for the matching route_localnet sysctl.
        let rs = plan_for_network(&net());
        let snat = rs
            .creates
            .iter()
            .find(|r| r.comment.as_deref() == Some("wisp:app:loopback-snat"))
            .expect("missing loopback-snat rule");
        assert_eq!(snat.table, Table::Nat);
        assert_eq!(snat.chain, "POSTROUTING");
        assert!(snat.args.contains(&"127.0.0.0/8".to_string()));
        assert!(snat.args.contains(&"MASQUERADE".to_string()));
        assert!(snat.args.contains(&"wbr-app".to_string()));
    }

    #[test]
    fn network_rules_match_in_creates_and_deletes() {
        // Invariant: deletes mirror creates exactly. Dispatch B uses
        // this to wire `-A` for creates and `-D` for deletes off the
        // same Rule shape.
        let rs = plan_for_network(&net());
        assert_eq!(rs.creates, rs.deletes);
    }

    #[test]
    fn attachment_rules_include_dnat_for_each_port() {
        let ports = vec![
            PortPublish {
                host_ip: Ipv4Addr::UNSPECIFIED,
                host_port: 18080,
                container_port: 80,
                protocol: PortProtocol::Tcp,
            },
            PortPublish {
                host_ip: Ipv4Addr::UNSPECIFIED,
                host_port: 18443,
                container_port: 443,
                protocol: PortProtocol::Tcp,
            },
        ];
        let rs = plan_for_attachment(&net(), Ipv4Addr::new(10, 83, 0, 2), "web", &ports);
        // 3 rules per port: PREROUTING DNAT + OUTPUT DNAT + FORWARD ACCEPT.
        assert_eq!(rs.creates.len(), 6);
        assert_eq!(rs.deletes.len(), 6);

        // Find the PREROUTING rules and assert there's one per host_port.
        let prerouting: Vec<&Rule> = rs
            .creates
            .iter()
            .filter(|r| r.chain == "PREROUTING")
            .collect();
        assert_eq!(prerouting.len(), 2);
        assert!(
            prerouting
                .iter()
                .any(|r| r.args.contains(&"18080".to_string()))
        );
        assert!(
            prerouting
                .iter()
                .any(|r| r.args.contains(&"18443".to_string()))
        );
    }

    #[test]
    fn loopback_dnat_present_for_localhost_clients() {
        let ports = vec![PortPublish {
            host_ip: Ipv4Addr::UNSPECIFIED,
            host_port: 18080,
            container_port: 80,
            protocol: PortProtocol::Tcp,
        }];
        let rs = plan_for_attachment(&net(), Ipv4Addr::new(10, 83, 0, 2), "web", &ports);
        let output_rules: Vec<&Rule> = rs.creates.iter().filter(|r| r.chain == "OUTPUT").collect();
        assert_eq!(output_rules.len(), 1);
        let r = output_rules[0];
        assert_eq!(r.table, Table::Nat);
        assert!(r.args.contains(&"127.0.0.1".to_string()));
        assert!(r.args.contains(&"--to-destination".to_string()));
        assert!(r.args.contains(&"10.83.0.2:80".to_string()));
        assert_eq!(r.comment.as_deref(), Some("wisp:web:loopback-dnat"));
    }

    #[test]
    fn comment_includes_wisp_marker_with_id() {
        let net = net();
        let net_rs = plan_for_network(&net);
        for rule in &net_rs.creates {
            let c = rule.comment.as_deref().unwrap_or("");
            assert!(c.starts_with("wisp:app:"), "bad network comment: {c}");
        }

        let ports = vec![PortPublish {
            host_ip: Ipv4Addr::UNSPECIFIED,
            host_port: 18080,
            container_port: 80,
            protocol: PortProtocol::Tcp,
        }];
        let att_rs = plan_for_attachment(&net, Ipv4Addr::new(10, 83, 0, 2), "web-1", &ports);
        for rule in &att_rs.creates {
            let c = rule.comment.as_deref().unwrap_or("");
            assert!(c.starts_with("wisp:web-1:"), "bad attachment comment: {c}");
        }
    }

    #[test]
    fn comment_sanitizes_unsafe_characters_in_id() {
        // Spaces / quotes / colons inside the raw id should be replaced
        // so the iptables-save grep stays unambiguous.
        let ports = vec![PortPublish {
            host_ip: Ipv4Addr::UNSPECIFIED,
            host_port: 18080,
            container_port: 80,
            protocol: PortProtocol::Tcp,
        }];
        let rs = plan_for_attachment(
            &net(),
            Ipv4Addr::new(10, 83, 0, 2),
            "weird id with:colons",
            &ports,
        );
        let c = rs.creates[0].comment.as_deref().unwrap();
        // Wisp prefix preserved; colon AFTER `wisp:` is the separator,
        // colons in the id are scrubbed.
        assert!(c.starts_with("wisp:weird_id_with_colons:"));
    }

    #[test]
    fn udp_protocol_emits_p_udp() {
        let ports = vec![PortPublish {
            host_ip: Ipv4Addr::UNSPECIFIED,
            host_port: 5353,
            container_port: 53,
            protocol: PortProtocol::Udp,
        }];
        let rs = plan_for_attachment(&net(), Ipv4Addr::new(10, 83, 0, 2), "dns", &ports);
        for rule in &rs.creates {
            // Each rule should reference -p udp, never -p tcp.
            let p_idx = rule.args.iter().position(|a| a == "-p").unwrap();
            assert_eq!(rule.args[p_idx + 1], "udp");
        }
    }

    #[test]
    fn attachment_rules_match_in_creates_and_deletes() {
        let ports = vec![PortPublish {
            host_ip: Ipv4Addr::UNSPECIFIED,
            host_port: 18080,
            container_port: 80,
            protocol: PortProtocol::Tcp,
        }];
        let rs = plan_for_attachment(&net(), Ipv4Addr::new(10, 83, 0, 2), "web", &ports);
        assert_eq!(rs.creates, rs.deletes);
    }

    #[test]
    fn rule_to_args_includes_table_verb_chain_and_comment() {
        let rs = plan_for_network(&net());
        let masq = &rs.creates[0];
        let args = super::rule_to_args(masq, "-A");
        assert_eq!(&args[0..4], &["-t", "nat", "-A", "POSTROUTING"]);
        // Comment is appended at the end as -m comment --comment <c>.
        assert_eq!(args[args.len() - 4], "-m");
        assert_eq!(args[args.len() - 3], "comment");
        assert_eq!(args[args.len() - 2], "--comment");
        assert!(args[args.len() - 1].starts_with("wisp:app:"));
    }

    #[test]
    fn rule_to_args_uses_d_for_delete() {
        let rs = plan_for_network(&net());
        let masq = &rs.deletes[0];
        let args = super::rule_to_args(masq, "-D");
        assert_eq!(&args[0..4], &["-t", "nat", "-D", "POSTROUTING"]);
    }

    #[test]
    fn ruleset_extend_concatenates_both_sides() {
        let mut a = plan_for_network(&net());
        let b = plan_for_attachment(
            &net(),
            Ipv4Addr::new(10, 83, 0, 2),
            "web",
            &[PortPublish {
                host_ip: Ipv4Addr::UNSPECIFIED,
                host_port: 18080,
                container_port: 80,
                protocol: PortProtocol::Tcp,
            }],
        );
        let create_total = a.creates.len() + b.creates.len();
        let delete_total = a.deletes.len() + b.deletes.len();
        a.extend(b);
        assert_eq!(a.creates.len(), create_total);
        assert_eq!(a.deletes.len(), delete_total);
    }
}
