//! `wisp` is a Rust-native, daemonless OCI container runtime.
//!
//! Phase 0.1 lands the smallest deliverable that proves the runtime can take
//! a hand-prepared OCI bundle and run it: namespaces (PID, mount, UTS, IPC,
//! network), spec-driven mounts, `pivot_root`, cgroup v2 limits, capability
//! drop, and exec the entrypoint as PID 1. Image pulling, networking
//! primitives, overlay storage, rootless mode, seccomp, AppArmor, and SELinux
//! are explicitly out of scope for this phase.
//!
//! This crate has zero `isengard-*` dependencies and is consumed only by the
//! `wisp-cli` binary and `cargo run --example` flows.

pub mod cgroup;
pub mod error;
pub mod spec;
pub mod state;

pub use cgroup::{Cgroup, CgroupFs, HostCgroupFs};
pub use error::WispError;
pub use spec::{Spec, load_and_validate};
pub use state::{ContainerHandle, ContainerState};

/// Returns the crate version (from `CARGO_PKG_VERSION`).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
