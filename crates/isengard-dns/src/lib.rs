//! Foundation crate for the isengard DNS resolver.
//!
//! Step 1 of the per-host DNS plugin laid out in
//! Embedded DNS resolver for Isengard route hostnames.
//!
//! # Shape
//!
//! - [`IsengardResolver`] owns one [`ZoneSource`] (the authoritative seam) and
//!   a `hickory_resolver::TokioResolver` upstream.
//! - The resolver exposes [`serve_udp`](IsengardResolver::serve_udp),
//!   [`serve_tcp`](IsengardResolver::serve_tcp), and the convenience
//!   [`serve`](IsengardResolver::serve) that runs both. They all hand control
//!   to `hickory_server::ServerFuture` so EDNS0, truncation, and TCP
//!   framing come for free.
//! - [`ZoneSource::resolve`] returns `Some(records)` for authoritative
//!   answers and `None` to mean "not authoritative; forward upstream". That
//!   single boolean is the contract Step 2 (the controller-backed
//!   `ZoneSource`) and the placeholder [`StaticZoneSource`] both honour.
//!
//! # Why this is the right seam
//!
//! Every isengard-specific decision (zone derivation from labels,
//! split-horizon CF zones, locality-aware multi-replica answers) lives on
//! one side of the [`ZoneSource`] trait. Everything below (wire format,
//! UDP/TCP/EDNS0, upstream forwarding) is hickory's job. Subsequent PRs
//! swap the zone source without touching the resolver glue.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

mod handler;
mod resolver;
mod static_source;
mod zone_source;

pub use handler::RequestHandlerImpl;
pub use resolver::IsengardResolver;
pub use static_source::{StaticZoneSource, StaticZoneSourceBuilder};
pub use zone_source::ZoneSource;

// Re-export the hickory types every consumer of the trait needs. Pinning
// them here keeps the `ZoneSource` impls in sibling crates (Step 2: the
// controller) from having to pick the same hickory minor independently.
pub use hickory_proto::rr::{Name, RData, Record, RecordType};
