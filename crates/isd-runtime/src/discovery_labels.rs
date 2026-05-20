//! Labels that identify Isengard system containers on a docker host.
//!
//! The controller compose recipe applies these labels to the controller
//! container. The agent install does the same on the agent container.
//! `isd` queries `docker ps --filter label=io.isengard.role=controller`
//! to find the controller; the operator-side protection guard reads
//! these labels to refuse destructive commands against system pieces.
//!
//! Bump [`API_VERSION`] when the discovery contract changes (port
//! relocation, auth path, etc.). [`crate::controller_discovery::discover`]
//! errors with an actionable message on version skew.

/// Reverse-DNS namespace for Isengard container metadata. The full label
/// key shape is `io.isengard.<thing>`; this constant is the role-marker
/// key specifically.
pub const ROLE_LABEL: &str = "io.isengard.role";

/// Value of [`ROLE_LABEL`] on the controller container.
pub const ROLE_CONTROLLER: &str = "controller";

/// Label key carrying the discovery-contract schema version. The
/// controller compose recipe sets this to [`API_VERSION`]; `isd`
/// compares the served value against [`API_VERSION`] at discovery time.
pub const API_VERSION_LABEL: &str = "io.isengard.api.version";

/// Discovery schema version `isd` speaks. Compared against the value of
/// [`API_VERSION_LABEL`] on the controller container. A mismatch yields
/// [`crate::DiscoveryError::VersionSkew`].
pub const API_VERSION: u32 = 1;

/// Role values that mark a container as protected. `isd rm`, `isd stop`,
/// `isd restart`, and `isd kill` refuse to run against containers
/// carrying any of these unless the operator passes `--force-system`.
pub const ROLE_VALUES_PROTECTED: &[&str] = &["controller", "agent"];

/// Reports whether a container's `io.isengard.role` value identifies a
/// protected system container.
///
/// Operator-side guards (the `isd ps` filter, the `isd rm/stop/restart/kill`
/// pre-execute check) call this against the container's label map.
/// Returns `true` for any value in [`ROLE_VALUES_PROTECTED`], `false`
/// otherwise (including the empty string and unknown role values).
pub fn is_protected_label_value(role: &str) -> bool {
    ROLE_VALUES_PROTECTED.contains(&role)
}

#[cfg(test)]
mod tests {
    //! Coverage for [`is_protected_label_value`] across the three cases
    //! the operator-side guard cares about: known-protected, empty, and
    //! arbitrary other role values.

    use super::*;

    /// `controller` is the canonical protected role value.
    #[test]
    fn controller_is_protected() {
        assert!(is_protected_label_value("controller"));
    }

    /// `agent` is protected too: removing the agent container from a
    /// host breaks the controller's fleet view.
    #[test]
    fn agent_is_protected() {
        assert!(is_protected_label_value("agent"));
    }

    /// Empty strings, near-misses, and unrelated role labels are not
    /// protected. The guard treats unknown roles as operator workloads.
    #[test]
    fn other_roles_are_not_protected() {
        assert!(!is_protected_label_value("registry"));
        assert!(!is_protected_label_value(""));
        assert!(!is_protected_label_value("controller-old"));
    }
}
