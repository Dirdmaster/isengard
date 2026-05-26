//! Host row and the request type for inserting a new one.
//!
//! `hosts` is the spine of the schema. Every other entity that lives
//! "on" a host carries `host_id BLOB(16)` as a foreign key. The table
//! lands via migration `0001` and grows fields through later migrations
//! (`fleet` added then dropped, `metadata` JSON, `fingerprint_alg`).

use serde::{Deserialize, Serialize};

/// Stable, monotonic, unique host identifier.
///
/// Wraps the 16-byte form of a [`ulid::Ulid`] for compact SQLite
/// storage. The newtype is `Copy` and round-trips through bytes, hex,
/// and JSON; it also implements `Display` (ULID Base32) so it can be
/// dropped straight into log lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(pub ulid::Ulid);

impl HostId {
    /// Mint a fresh, time-ordered HostId. Uses the system clock; two
    /// IDs minted in the same millisecond differ by the random tail.
    pub fn new() -> Self {
        Self(ulid::Ulid::new())
    }

    /// Encode as the canonical 16 raw bytes for the SQLite BLOB column.
    pub fn to_bytes(&self) -> [u8; 16] {
        self.0.to_bytes()
    }

    /// Decode from raw bytes (e.g. round-tripped through `to_bytes`).
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(ulid::Ulid::from_bytes(bytes))
    }

    /// Decode a `host_id BLOB` column from a SQLite row.
    ///
    /// DRYs the `Vec<u8> -> [u8; 16] -> HostId` dance every entity that
    /// stores `host_id` as BLOB has to do.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Decode`] when the byte slice isn't
    /// exactly 16 bytes (an arbitrary-length BLOB landed in the
    /// column, schema invariant broken).
    pub fn from_db_bytes(bytes: Vec<u8>) -> crate::error::Result<Self> {
        let arr: [u8; 16] = bytes.try_into().map_err(|_| crate::error::Error::Decode {
            reason: "host_id BLOB is not 16 bytes".into(),
        })?;
        Ok(Self::from_bytes(arr))
    }
}

impl Default for HostId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<ulid::Ulid> for HostId {
    fn from(u: ulid::Ulid) -> Self {
        Self(u)
    }
}

impl From<HostId> for ulid::Ulid {
    fn from(h: HostId) -> Self {
        h.0
    }
}

/// One row from the `hosts` table.
///
/// `enrolled_at` is set at insert; `last_seen_at` flips from `None` to
/// `Some(ts)` on the first heartbeat and continues to advance.
/// `metadata` is observational JSON: cached LAN IP, the active
/// runtime backend, anything the dashboard wants to display without
/// earning a column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// Primary key (16-byte ULID).
    pub id: HostId,
    /// Operator-visible fingerprint string. Today this is the SHA-256
    /// of the leaf cert; older builds wrote a free-form string.
    pub fingerprint: String,
    /// Reported hostname (`uname -n`).
    pub hostname: String,
    /// Reported OS family (e.g. `linux`, `darwin`).
    pub os: String,
    /// Reported architecture (e.g. `x86_64`, `aarch64`).
    pub arch: String,
    /// Agent version string from the enrolling binary.
    pub agent_version: String,
    /// Docker engine version reported by the agent at enrollment.
    pub docker_version: String,
    /// Unix seconds when the host first enrolled.
    pub enrolled_at: i64,
    /// Unix seconds of the last heartbeat. `None` until first heartbeat lands.
    pub last_seen_at: Option<i64>,
    /// Free-form JSON metadata. Defaults to `{}`.
    pub metadata: serde_json::Value,
    /// Address an operator types into `ssh` to reach this host
    /// (e.g. `alice@192.0.2.10`). Captured at enroll time from
    /// the operator's active docker context URL. `None` for pre-0.7
    /// host rows that predate the column. The operator can override
    /// via `isd ssh hosts set <agent> --dial <target>`.
    pub dial_target: Option<String>,
}

/// Request shape for inserting a new host.
///
/// The controller calls this; the storage layer assigns a fresh
/// [`HostId`] and `enrolled_at`. Host grouping (previously the `fleet`
/// column) is now expressed via labels: agents report `[labels]` in
/// their config and the scheduler matches them with placement
/// selectors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollHost {
    /// Operator-visible fingerprint string.
    pub fingerprint: String,
    /// Reported hostname.
    pub hostname: String,
    /// Reported OS family.
    pub os: String,
    /// Reported architecture.
    pub arch: String,
    /// Agent version at enrollment time.
    pub agent_version: String,
    /// Docker engine version at enrollment time.
    pub docker_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_id_round_trips_through_bytes() {
        let id = HostId::new();
        let bytes = id.to_bytes();
        let back = HostId::from_bytes(bytes);
        assert_eq!(id, back);
    }

    #[test]
    fn host_id_displays_as_ulid() {
        let id = HostId::from_bytes([0; 16]);
        assert_eq!(id.to_string(), "00000000000000000000000000");
    }

    #[test]
    fn host_id_round_trips_through_json() {
        let id = HostId::new();
        let s = serde_json::to_string(&id).unwrap();
        let back: HostId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, back);
    }
}
