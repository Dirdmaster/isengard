//! Isengard core types: plugin trait, host services, event journal types.

pub mod context;
pub mod error;
pub mod event;

pub use context::{HostMode, PluginContext};
pub use error::{CoreError, Result};
pub use event::{Event, EventKind};
