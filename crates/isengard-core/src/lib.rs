//! Isengard core types: plugin trait, host services, event journal types.

pub mod context;
pub mod error;
pub mod event;
pub mod plugin;

pub use context::{HostMode, PluginContext};
pub use error::{CoreError, Result};
pub use event::{Event, EventKind};
pub use plugin::{AgentPlugin, ControllerPlugin, EventSubscriber, HttpHandler, Plugin};
