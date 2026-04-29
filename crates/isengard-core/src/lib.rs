//! Isengard core types: plugin trait, host services, event journal types.

pub mod error;
pub mod event;

pub use error::{CoreError, Result};
pub use event::{Event, EventKind};
