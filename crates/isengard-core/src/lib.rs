//! Isengard core types: plugin trait, host services, event journal types,
//! compile-time plugin registration.
//!
//! The crate is the dependency root for the rest of the workspace. Higher
//! crates (`isengard-storage`, `isengard-controller`, `isengard-agent`,
//! plugins) layer their own surface on top of these traits and value
//! types without taking dependencies on each other.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

pub mod approval_store;
pub mod context;
pub mod error;
pub mod event;
pub mod hooks;
pub mod join_token;
pub mod labels;
pub mod networking;
pub mod placement;
pub mod plugin;
pub mod policy;
pub mod policy_loader;
pub mod registration;
pub mod update_dispatch;

/// Stable, monotonic, unique host identifier.
///
/// Aliased to [`ulid::Ulid`] so core can reference it without depending on
/// the storage crate. Storage's `HostId` newtype provides `From`/`Into`
/// conversions to/from this type.
pub use ulid::Ulid as HostId;

pub use approval_store::{
    ApprovalStore, ApprovalStoreError, InsertPendingApproval, PendingApprovalBody,
    PendingApprovalRecord,
};
pub use context::{HostMode, PluginContext};
pub use error::{CoreError, Result};
pub use event::{Event, EventEmitter, NoopEmitter, arc_emitter};
pub use hooks::{ParsedHooks, has_any_hook_label, parse_hook_labels};
pub use plugin::{AgentPlugin, ControllerPlugin, EventSubscriber, HttpHandler, Plugin};
pub use policy::*;
pub use policy_loader::{LoadedPolicy, PolicyLoader, PolicyLoaderError};
pub use registration::{Capability, PluginRegistration, registrations_for};
pub use update_dispatch::{DispatchOutcome, UpdateDispatcher, UpdateTriggerInfo};
