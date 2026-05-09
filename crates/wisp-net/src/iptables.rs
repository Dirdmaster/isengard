//! iptables rule planner. Real implementation lands in commit 4.

use crate::bridge::Network;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Nat,
    Filter,
    Mangle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub table: Table,
    pub chain: String,
    pub args: Vec<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleSet {
    pub creates: Vec<Rule>,
    pub deletes: Vec<Rule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortProtocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortPublish {
    pub host_ip: std::net::Ipv4Addr,
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: PortProtocol,
}

pub fn plan_for_network(_net: &Network) -> RuleSet {
    RuleSet::default()
}

pub fn plan_for_attachment(
    _net: &Network,
    _container_ip: std::net::Ipv4Addr,
    _container_id: &str,
    _ports: &[PortPublish],
) -> RuleSet {
    RuleSet::default()
}
