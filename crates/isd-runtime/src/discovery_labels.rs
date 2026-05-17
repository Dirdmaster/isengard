//! Labels used to discover the Isengard controller container on a docker host.
//!
//! The controller's compose recipe applies these labels to the controller
//! container. `isd` (via `controller_discovery`) queries `docker ps`
//! with `--filter label=io.isengard.role=controller` to find it.
//!
//! Bump `API_VERSION` when the discovery contract changes (port relocation,
//! auth path, etc.). `isd` errors with an actionable message on version skew.

/// Reverse-DNS label namespace for Isengard container metadata.
pub const ROLE_LABEL: &str = "io.isengard.role";

/// Value of `ROLE_LABEL` on the controller container.
pub const ROLE_CONTROLLER: &str = "controller";

/// Label carrying the discovery-contract schema version.
pub const API_VERSION_LABEL: &str = "io.isengard.api.version";

/// Discovery schema version `isd` speaks. Compared against the value of
/// `API_VERSION_LABEL` on the controller container.
pub const API_VERSION: u32 = 1;
