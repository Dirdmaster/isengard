//! Native placement verbs + label selectors.
//!
//! The canonical types live in [`isengard_core::placement`]; this module
//! re-exports them so existing agent-side call sites
//! (`crate::placement::Placement`, etc.) keep compiling. The move into
//! `isengard-core` was forced wiring: the controller's
//! scheduler needs `Placement` + `LabelSelector` too, and `isengard-agent`
//! already depends on `isengard-controller`, which would create a cycle
//! if these types stayed in the agent crate.

pub use isengard_core::placement::*;
