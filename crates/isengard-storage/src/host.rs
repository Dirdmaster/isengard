//! Host row and the request type for inserting a new one.

use serde::{Deserialize, Serialize};

/// Stable, monotonic, unique host identifier. Wraps the 16-byte form of a
/// [`ulid::Ulid`] for compact SQLite storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(pub ulid::Ulid);

impl HostId {
    pub fn new() -> Self {
        Self(ulid::Ulid::new())
    }

    pub fn to_bytes(&self) -> [u8; 16] {
        self.0.to_bytes()
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(ulid::Ulid::from_bytes(bytes))
    }

    /// Decode a `host_id BLOB` column from a SQLite row. Returns
    /// `Error::Decode` if the byte slice isn't exactly 16 bytes.
    /// DRYs the `Vec<u8> -> [u8; 16] -> HostId` dance every entity that
    /// stores `host_id` as BLOB has to do.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    pub id: HostId,
    pub fingerprint: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub docker_version: String,
    /// Unix seconds when the host first enrolled.
    pub enrolled_at: i64,
    /// Unix seconds of the last heartbeat. `None` until first heartbeat lands.
    pub last_seen_at: Option<i64>,
    /// Free-form JSON metadata. Defaults to `{}`.
    pub metadata: serde_json::Value,
    /// Fleet tag this host belongs to. Defaults to `"default"`.
    pub fleet: String,
}

/// Request shape for inserting a new host. The controller calls this; the
/// storage layer assigns a fresh [`HostId`] and `enrolled_at`. The `fleet`
/// is required — every host belongs to exactly one fleet, named by the user
/// during onboarding. There is no auto-`default` fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollHost {
    pub fingerprint: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub docker_version: String,
    pub fleet: String,
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
