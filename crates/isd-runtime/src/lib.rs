//! `isd-runtime`: direct Docker Engine API access for the operator's
//! `isd` CLI.
//!
//! Wraps `bollard` with an SSH tunnel lifecycle so the operator can
//! target remote Docker daemons from their laptop without an Isengard
//! agent or controller installed on the target. The two-tier shape:
//!
//!   - [`SshTunnel`] spawns `ssh -L <local-port>:/var/run/docker.sock
//!     <target>` and owns the child process (Drop kills it).
//!   - [`DockerBackend`] wraps `bollard::Docker` against either the
//!     local default socket or a tunneled remote.
//!
//! Phase 0.20 ships the transport plumbing. Phase 0.21 swaps `isd ps`
//! over to use this in place of the controller round-trip.

pub mod discovery_labels;
mod docker;
mod error;
mod ssh_tunnel;

pub use docker::{ContainerSummary, DockerBackend};
pub use error::{Error, Result};
pub use ssh_tunnel::SshTunnel;
