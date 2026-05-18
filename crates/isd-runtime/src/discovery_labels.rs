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

/// Role values that mark a container as protected: removal/stop/restart/kill
/// from `isd` refuses these without an explicit `--force-system` override.
pub const ROLE_VALUES_PROTECTED: &[&str] = &["controller", "agent"];

/// Returns true when the given `io.isengard.role` label value identifies a
/// protected system container. Operator-side guards (in `isd ps` filter,
/// `isd rm/stop/restart/kill` pre-execute) call this against the container's
/// label map.
pub fn is_protected_label_value(role: &str) -> bool {
    ROLE_VALUES_PROTECTED.contains(&role)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_is_protected() {
        assert!(is_protected_label_value("controller"));
    }

    #[test]
    fn agent_is_protected() {
        assert!(is_protected_label_value("agent"));
    }

    #[test]
    fn other_roles_are_not_protected() {
        assert!(!is_protected_label_value("registry"));
        assert!(!is_protected_label_value(""));
        assert!(!is_protected_label_value("controller-old"));
    }
}
