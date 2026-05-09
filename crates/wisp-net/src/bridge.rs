//! Bridge command planner. Real implementation lands in commit 3.

use crate::error::WispNetError;

pub struct Network {
    pub name: String,
    pub bridge: String,
    pub subnet: ipnet::Ipv4Net,
    pub gateway: std::net::Ipv4Addr,
}

impl Network {
    pub fn new(_name: &str, _subnet: ipnet::Ipv4Net) -> Result<Self, WispNetError> {
        Err(WispNetError::Bridge("not implemented (skeleton)".into()))
    }
}

pub struct IpCommand {
    pub args: Vec<String>,
}

pub fn plan_create(_net: &Network) -> Vec<IpCommand> {
    Vec::new()
}

pub fn plan_delete(_net: &Network) -> Vec<IpCommand> {
    Vec::new()
}
