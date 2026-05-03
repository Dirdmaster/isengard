//! TLS subsystem: cert storage, ACME issuance, in-memory cert store.
//!
//! Phase 8e (Plan B) seeds this module with `storage` — the on-disk cert/key
//! read/write layer used by both ACME issuance (writes new pairs) and the
//! cert_store cache (warm-loads pairs at startup). Subsequent tasks add
//! `cert_store`, `acme`, and the SNI resolver.

pub mod cert_store;
pub mod challenge_state;
pub mod storage;

pub use cert_store::{CertEntry, CertStore};
pub use challenge_state::ChallengeState;
pub use storage::{CertFiles, TlsStorage};
