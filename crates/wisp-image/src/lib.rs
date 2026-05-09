//! Wisp image library: pull, store, extract OCI images for wisp-runtime.
//!
//! Phase 0.2: anonymous registry pulls, content-addressable layer
//! cache, refcount-based GC, bundle synthesis from image config +
//! operator overrides. Public registries only; no auth.

pub mod error;
pub use error::WispImageError;
