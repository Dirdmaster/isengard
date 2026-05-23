//! In-memory [`ZoneSource`] backed by a hand-coded record map.
//!
//! Used by tests and by the Step 1 binary's `--zone-file` loader. The
//! controller-backed impl in Step 2 replaces this in production; the trait
//! contract stays identical so consumers keep compiling.

use std::collections::HashMap;

use async_trait::async_trait;
use hickory_proto::rr::{Name, Record, RecordType};

use crate::zone_source::ZoneSource;

/// Static, immutable record store.
///
/// Records are keyed by `(name, record_type)`. The owning zone is implicit:
/// any name not in the map is treated as "not authoritative", causing the
/// resolver to forward upstream. That keeps the trait's contract honest
/// (an empty record vec is **not** the same as "not ours") at the cost of
/// requiring callers to enumerate names. Fine for tests and small
/// hand-curated zone files; not a long-term answer.
///
/// Build instances via [`StaticZoneSource::builder`].
pub struct StaticZoneSource {
    records: HashMap<(Name, RecordType), Vec<Record>>,
    /// Names that exist in *some* record type. Used to disambiguate
    /// "name exists, no data for this type" (empty answer, `NoError`) from
    /// "name unknown to us, forward upstream" (None).
    known_names: std::collections::HashSet<Name>,
}

impl StaticZoneSource {
    /// Start building a [`StaticZoneSource`].
    #[must_use]
    pub fn builder() -> StaticZoneSourceBuilder {
        StaticZoneSourceBuilder::default()
    }

    /// Number of `(name, type)` entries. Mostly useful in tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// `true` when no records have been added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

#[async_trait]
impl ZoneSource for StaticZoneSource {
    async fn resolve(&self, name: &Name, rtype: RecordType) -> anyhow::Result<Option<Vec<Record>>> {
        let key_name = canonical(name);

        // Exact `(name, rtype)` hit.
        if let Some(records) = self.records.get(&(key_name.clone(), rtype)) {
            return Ok(Some(records.clone()));
        }

        // ANY: collect every record-type for the name.
        if rtype == RecordType::ANY && self.known_names.contains(&key_name) {
            let mut bag = Vec::new();
            for ((n, _), recs) in &self.records {
                if n == &key_name {
                    bag.extend(recs.iter().cloned());
                }
            }
            return Ok(Some(bag));
        }

        // Name exists in another rtype: NoError empty answer ("we own this
        // name, just nothing of that flavour"). Caller distinguishes empty
        // vec from `None` ("forward upstream").
        if self.known_names.contains(&key_name) {
            return Ok(Some(Vec::new()));
        }

        Ok(None)
    }
}

/// Builder for [`StaticZoneSource`].
#[derive(Default)]
pub struct StaticZoneSourceBuilder {
    records: HashMap<(Name, RecordType), Vec<Record>>,
    known_names: std::collections::HashSet<Name>,
}

impl StaticZoneSourceBuilder {
    /// Append one record. Multiple records with the same `(name, rtype)`
    /// accumulate (round-robin A records, multi-target CNAME-after-A, etc.).
    #[must_use]
    pub fn record(mut self, record: Record) -> Self {
        let name = canonical(record.name());
        let rtype = record.record_type();
        self.known_names.insert(name.clone());
        self.records.entry((name, rtype)).or_default().push(record);
        self
    }

    /// Append every record in `iter`. Convenience over chained
    /// [`record`](Self::record) calls.
    #[must_use]
    pub fn records<I: IntoIterator<Item = Record>>(mut self, iter: I) -> Self {
        for r in iter {
            self = self.record(r);
        }
        self
    }

    /// Finish building.
    #[must_use]
    pub fn build(self) -> StaticZoneSource {
        StaticZoneSource {
            records: self.records,
            known_names: self.known_names,
        }
    }
}

/// Canonical form: lowercased, FQDN. Hickory query names land here as
/// FQDNs already, but record names supplied by hand may not.
fn canonical(name: &Name) -> Name {
    let mut n = name.to_lowercase();
    n.set_fqdn(true);
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::rr::rdata::{A, AAAA, CNAME};
    use hickory_proto::rr::{RData, Record};
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::str::FromStr;

    fn a_record(name: &str, ip: Ipv4Addr) -> Record {
        Record::from_rdata(Name::from_str(name).unwrap(), 5, RData::A(A(ip)))
    }

    fn aaaa_record(name: &str, ip: Ipv6Addr) -> Record {
        Record::from_rdata(Name::from_str(name).unwrap(), 5, RData::AAAA(AAAA(ip)))
    }

    fn cname_record(name: &str, target: &str) -> Record {
        Record::from_rdata(
            Name::from_str(name).unwrap(),
            5,
            RData::CNAME(CNAME(Name::from_str(target).unwrap())),
        )
    }

    #[tokio::test]
    async fn returns_some_for_known_name_type_pair() {
        let src = StaticZoneSource::builder()
            .record(a_record("foo.weavers.local.", Ipv4Addr::new(10, 0, 0, 1)))
            .build();

        let res = src
            .resolve(
                &Name::from_str("foo.weavers.local.").unwrap(),
                RecordType::A,
            )
            .await
            .unwrap();
        assert_eq!(res.as_ref().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn returns_none_for_unknown_name() {
        let src = StaticZoneSource::builder()
            .record(a_record("foo.weavers.local.", Ipv4Addr::new(10, 0, 0, 1)))
            .build();

        let res = src
            .resolve(&Name::from_str("example.com.").unwrap(), RecordType::A)
            .await
            .unwrap();
        assert!(
            res.is_none(),
            "unknown names must return None so the resolver forwards upstream",
        );
    }

    #[tokio::test]
    async fn known_name_unknown_type_returns_empty_vec_not_none() {
        let src = StaticZoneSource::builder()
            .record(a_record("foo.weavers.local.", Ipv4Addr::new(10, 0, 0, 1)))
            .build();

        let res = src
            .resolve(
                &Name::from_str("foo.weavers.local.").unwrap(),
                RecordType::AAAA,
            )
            .await
            .unwrap();
        assert_eq!(res, Some(Vec::new()));
    }

    #[tokio::test]
    async fn any_query_collects_all_types_for_a_name() {
        let src = StaticZoneSource::builder()
            .record(a_record("foo.weavers.local.", Ipv4Addr::new(10, 0, 0, 1)))
            .record(aaaa_record("foo.weavers.local.", Ipv6Addr::LOCALHOST))
            .build();

        let res = src
            .resolve(
                &Name::from_str("foo.weavers.local.").unwrap(),
                RecordType::ANY,
            )
            .await
            .unwrap();
        assert_eq!(res.as_ref().map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn cname_record_round_trips() {
        let src = StaticZoneSource::builder()
            .record(cname_record(
                "alias.weavers.local.",
                "target.weavers.local.",
            ))
            .build();

        let result = src
            .resolve(
                &Name::from_str("alias.weavers.local.").unwrap(),
                RecordType::CNAME,
            )
            .await
            .unwrap();
        let records = result.expect("authoritative");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_type(), RecordType::CNAME);
    }

    #[tokio::test]
    async fn lookup_is_case_insensitive() {
        let src = StaticZoneSource::builder()
            .record(a_record("Foo.Weavers.Local.", Ipv4Addr::new(10, 0, 0, 1)))
            .build();

        let res = src
            .resolve(
                &Name::from_str("FOO.WEAVERS.LOCAL.").unwrap(),
                RecordType::A,
            )
            .await
            .unwrap();
        assert_eq!(res.as_ref().map(Vec::len), Some(1));
    }
}
