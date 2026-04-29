//! Isengard core types: plugin trait, host services, event journal types,
//! compile-time plugin registration.

pub mod context;
pub mod error;
pub mod event;
pub mod plugin;
pub mod registration;

pub use context::{HostMode, PluginContext};
pub use error::{CoreError, Result};
pub use event::{Event, EventKind};
pub use plugin::{AgentPlugin, ControllerPlugin, EventSubscriber, HttpHandler, Plugin};
pub use registration::{registrations_for, Capability, PluginRegistration};
