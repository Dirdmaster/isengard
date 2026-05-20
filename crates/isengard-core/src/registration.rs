//! Compile-time plugin registration via the [`inventory`] crate.
//!
//! Each plugin crate calls `inventory::submit!(PluginRegistration { ... })` at
//! module scope. The host enumerates them at startup by mode.

use crate::context::HostMode;
use crate::plugin::Plugin;

/// Capabilities a plugin advertises.
///
/// Used by the host to skip plugins that don't apply to its current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// The plugin can run on agents.
    Agent,
    /// The plugin can run on the controller.
    Controller,
}

/// Compile-time plugin registration entry.
///
/// Plugin crates submit one of these per plugin via [`inventory::submit!`].
/// The host enumerates the registry at startup and constructs the boxed
/// plugins it needs for the current mode via [`registrations_for`].
pub struct PluginRegistration {
    /// Stable plugin name, matches [`Plugin::name`].
    pub name: &'static str,
    /// Capabilities the plugin advertises. The host filters by mode.
    pub capabilities: &'static [Capability],
    /// Factory returning a fresh boxed plugin.
    ///
    /// Returns `Plugin` so callers don't need to know the concrete type. The
    /// host downcasts when calling capability sub-traits.
    pub constructor: fn() -> Box<dyn Plugin>,
}

inventory::collect!(PluginRegistration);

/// Enumerate every registered plugin that advertises a capability matching
/// `mode`.
///
/// Returned slice order is implementation-defined; callers that need a
/// stable order should sort by `name`.
pub fn registrations_for(mode: HostMode) -> Vec<&'static PluginRegistration> {
    let want = match mode {
        HostMode::Agent => Capability::Agent,
        HostMode::Controller => Capability::Controller,
    };
    inventory::iter::<PluginRegistration>()
        .filter(|r| r.capabilities.contains(&want))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Plugin;
    use crate::plugin::tests::NoopPlugin;

    inventory::submit! {
        PluginRegistration {
            name: "noop",
            capabilities: &[Capability::Agent, Capability::Controller],
            constructor: || Box::new(NoopPlugin) as Box<dyn Plugin>,
        }
    }

    #[test]
    fn noop_is_visible_to_agent_mode() {
        let regs = registrations_for(HostMode::Agent);
        assert!(regs.iter().any(|r| r.name == "noop"));
    }

    #[test]
    fn noop_is_visible_to_controller_mode() {
        let regs = registrations_for(HostMode::Controller);
        assert!(regs.iter().any(|r| r.name == "noop"));
    }

    #[test]
    fn registration_constructor_yields_a_working_plugin() {
        let regs = registrations_for(HostMode::Agent);
        let noop = regs.iter().find(|r| r.name == "noop").unwrap();
        let plugin = (noop.constructor)();
        assert_eq!(plugin.name(), "noop");
        assert_eq!(plugin.version(), "0.0.0");
    }
}
