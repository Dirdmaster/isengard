//! Isengard core types: plugin trait, host services, event journal types,
//! compile-time plugin registration.

pub mod context;
pub mod error;
pub mod event;
pub mod labels;
pub mod networking;
pub mod plugin;
pub mod registration;

/// Stable, monotonic, unique host identifier. Aliased to `ulid::Ulid` so core
/// can reference it without depending on the storage crate. Storage's
/// `HostId` newtype provides `From`/`Into` conversions to/from this type.
pub use ulid::Ulid as HostId;

pub use context::{HostMode, PluginContext};
pub use error::{CoreError, Result};
pub use event::{Event, EventEmitter, NoopEmitter, arc_emitter};
pub use plugin::{AgentPlugin, ControllerPlugin, EventSubscriber, HttpHandler, Plugin};
pub use registration::{Capability, PluginRegistration, registrations_for};
