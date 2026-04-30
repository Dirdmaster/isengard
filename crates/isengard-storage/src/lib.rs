//! SQLite-backed inventory storage for the Isengard controller.
//!
//! Phase 2b surface: hosts table only. Containers and journal land in
//! later phases as new migrations.

pub mod error;
pub mod host;
pub mod inventory;
pub mod journal;

pub use error::{Error, Result};
pub use host::{EnrollHost, Host, HostId};
pub use inventory::Inventory;
pub use journal::{EventRow, InsertEvent, Journal};
