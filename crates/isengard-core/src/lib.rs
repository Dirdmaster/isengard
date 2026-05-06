//! Isengard core types: plugin trait, host services, event journal types,
//! compile-time plugin registration.

pub mod approval_store;
pub mod context;
pub mod error;
pub mod event;
pub mod labels;
pub mod networking;
pub mod plugin;
pub mod policy;
pub mod policy_loader;
pub mod registration;
pub mod update_dispatch;

/// Stable, monotonic, unique host identifier. Aliased to `ulid::Ulid` so core
/// can reference it without depending on the storage crate. Storage's
/// `HostId` newtype provides `From`/`Into` conversions to/from this type.
pub use ulid::Ulid as HostId;

pub use approval_store::{
    ApprovalStore, ApprovalStoreError, InsertPendingApproval, PendingApprovalBody,
    PendingApprovalRecord,
};
pub use context::{HostMode, PluginContext};
pub use error::{CoreError, Result};
pub use event::{Event, EventEmitter, NoopEmitter, arc_emitter};
pub use plugin::{AgentPlugin, ControllerPlugin, EventSubscriber, HttpHandler, Plugin};
pub use policy::*;
pub use policy_loader::{LoadedPolicy, PolicyLoader, PolicyLoaderError};
pub use registration::{Capability, PluginRegistration, registrations_for};
pub use update_dispatch::{DispatchOutcome, UpdateDispatcher, UpdateTriggerInfo};
