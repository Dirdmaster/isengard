//! The [`ZoneSource`] trait: the authoritative seam between hickory and
//! anything isengard wants to call "a zone".
//!
//! Step 2's controller-backed impl reads stack labels, container placements
//! and operator-managed zones to synthesise the record set. Step 1 ships a
//! [`StaticZoneSource`](crate::StaticZoneSource) for tests and for the
//! single-host binary smoke test.

use async_trait::async_trait;
use hickory_proto::rr::{Name, Record, RecordType};

/// Source of authoritative DNS records.
///
/// Each lookup returns:
///
/// - `Some(records)`: the source is authoritative for `name` and the
///   resolver should answer with these records (may be empty for a
///   "no data, but the name exists" `NoError` shape).
/// - `None`: the source is **not** authoritative; the resolver forwards
///   the query upstream.
///
/// The distinction matters because hickory needs to set the AA bit on
/// authoritative answers and only return `NXDOMAIN` from inside a zone we
/// own. An empty `Vec` from a `None`-returning source would conflate
/// "not ours, ask upstream" with "ours, no answer".
#[async_trait]
pub trait ZoneSource: Send + Sync {
    /// Look up `name` of type `rtype` in any authoritative zone.
    ///
    /// # Errors
    ///
    /// Implementations may return `Err` when the source itself failed
    /// (database unreachable, cache poisoned, etc.). The resolver
    /// translates that into `ServFail` rather than forwarding upstream:
    /// a misconfigured controller should not silently leak queries to
    /// 1.1.1.1.
    async fn resolve(&self, name: &Name, rtype: RecordType) -> anyhow::Result<Option<Vec<Record>>>;
}
