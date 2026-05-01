//! SQLite-backed inventory storage for the Isengard controller.
//!
//! Phase 2b surface: hosts table only. Containers and journal land in
//! later phases as new migrations.

pub mod error;
pub mod fleet;
pub mod host;
pub mod host_action;
pub mod inventory;
pub mod journal;
pub mod service;
pub mod setting;
pub mod stack;

pub use error::{Error, Result};
pub use fleet::Fleet;
pub use host::{EnrollHost, Host, HostId};
pub use host_action::{HostAction, HostActionId, HostActionKind};
pub use inventory::Inventory;
pub use journal::{EventRow, InsertEvent, Journal};
pub use service::{InsertService, Service, ServiceId, ServiceState};
pub use setting::Setting;
pub use stack::{InsertStack, Stack, StackId, StackSource};
