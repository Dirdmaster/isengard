//! Error type for the wisp-net crate.
//!
//! Variants cover both the dispatch-A planner concerns (Ipam, Iptables,
//! Bridge, Veth, Resolv, Json, Parse, Conflict, NotFound) and the
//! dispatch-B+C side-effect concerns (Io, Cmd). Listing them up front
//! keeps later dispatches additive and avoids variant churn.

#[derive(thiserror::Error, Debug)]
pub enum WispNetError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("parse: {0}")]
    Parse(String),

    #[error("command failed: `{command}` (exit {exit_code}): {stderr}")]
    Cmd {
        command: String,
        exit_code: i32,
        stderr: String,
    },

    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("ipam: {0}")]
    Ipam(String),

    #[error("iptables: {0}")]
    Iptables(String),

    #[error("bridge: {0}")]
    Bridge(String),

    #[error("veth: {0}")]
    Veth(String),

    #[error("resolv: {0}")]
    Resolv(String),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}
