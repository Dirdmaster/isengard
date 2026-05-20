#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]
#![doc = include_str!("../docs/crate.md")]

pub mod controller_discovery;
pub mod discovery_labels;
mod docker;
mod error;
mod ssh_tunnel;

pub use controller_discovery::{ControllerEndpoint, DiscoveryError, discover};
pub use docker::{ContainerSummary, DockerBackend};
pub use error::{Error, Result};
pub use ssh_tunnel::{SshTunnel, control_path_for};
