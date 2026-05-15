//! Host eligibility under a `LabelSelector`. Phase 0.14 step 4.
//!
//! Two surfaces:
//! - [`match_selector`] is a thin wrapper around
//!   [`LabelSelector::matches`] (which itself lives in `isengard-core`).
//!   It exists so the scheduler reads one consistent entry point and so
//!   the `Option<&LabelSelector>` ergonomic (None == always-match) is in
//!   exactly one place.
//! - [`eligible_hosts`] walks the controller's host list, drops anything
//!   not `Healthy`, then filters by selector. The result is the
//!   "candidate set" the scheduler's assign / spread / global pass-throughs
//!   read.

use std::collections::{BTreeMap, HashMap};

use isengard_core::placement::LabelSelector;
use isengard_storage::host::{Host, HostId};

use super::HostHealth;

/// Return `true` iff `labels` satisfies every clause in `selector`.
/// A `None` selector matches anything (the singleton / spread / global
/// "no `where:` clause" case).
pub fn match_selector(selector: Option<&LabelSelector>, labels: &BTreeMap<String, String>) -> bool {
    match selector {
        Some(sel) => sel.matches(labels),
        None => true,
    }
}

/// Return the list of host ids that are eligible to receive a new
/// placement under `selector`. Eligibility = `HostHealth::Healthy` AND
/// labels match the selector. Hosts without a health entry are treated
/// as unknown (excluded) so a stale boot snapshot doesn't accidentally
/// place onto a host the scheduler hasn't seen yet.
///
/// Output is sorted by hostname for deterministic tie-breaks (the
/// assignment math then re-sorts by load when it needs to).
pub fn eligible_hosts(
    hosts: &[Host],
    labels_by_host: &HashMap<HostId, BTreeMap<String, String>>,
    health_by_host: &HashMap<HostId, HostHealth>,
    selector: Option<&LabelSelector>,
) -> Vec<HostId> {
    let empty: BTreeMap<String, String> = BTreeMap::new();
    let mut out: Vec<(String, HostId)> = hosts
        .iter()
        .filter(|h| matches!(health_by_host.get(&h.id), Some(HostHealth::Healthy)))
        .filter(|h| {
            let labels = labels_by_host.get(&h.id).unwrap_or(&empty);
            match_selector(selector, labels)
        })
        .map(|h| (h.hostname.clone(), h.id))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.into_iter().map(|(_, id)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn host(id: u8, name: &str) -> Host {
        Host {
            id: HostId::from_bytes([id; 16]),
            fingerprint: format!("fp-{name}"),
            hostname: name.into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.14".into(),
            docker_version: "24.0".into(),
            enrolled_at: 0,
            last_seen_at: Some(0),
            metadata: serde_json::Value::Null,
            fleet: "default".into(),
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn none_selector_matches_anything() {
        assert!(match_selector(None, &labels(&[])));
        assert!(match_selector(None, &labels(&[("role", "x")])));
    }

    #[test]
    fn eq_matches_present_value() {
        let sel = LabelSelector::parse("role==worker").unwrap();
        assert!(match_selector(Some(&sel), &labels(&[("role", "worker")])));
        assert!(!match_selector(Some(&sel), &labels(&[("role", "control")])));
    }

    #[test]
    fn neq_matches_absent_per_k8s_semantics() {
        let sel = LabelSelector::parse("role!=control").unwrap();
        assert!(match_selector(Some(&sel), &labels(&[])));
        assert!(match_selector(Some(&sel), &labels(&[("role", "worker")])));
        assert!(!match_selector(Some(&sel), &labels(&[("role", "control")])));
    }

    #[test]
    fn in_matches_one_of() {
        let sel = LabelSelector::parse("tier in (gpu, fast)").unwrap();
        assert!(match_selector(Some(&sel), &labels(&[("tier", "gpu")])));
        assert!(match_selector(Some(&sel), &labels(&[("tier", "fast")])));
        assert!(!match_selector(Some(&sel), &labels(&[("tier", "slow")])));
    }

    #[test]
    fn notin_matches_absent_per_k8s_semantics() {
        let sel = LabelSelector::parse("tier notin (gpu)").unwrap();
        assert!(match_selector(Some(&sel), &labels(&[])));
        assert!(!match_selector(Some(&sel), &labels(&[("tier", "gpu")])));
        assert!(match_selector(Some(&sel), &labels(&[("tier", "cpu")])));
    }

    #[test]
    fn exists_matches_only_when_key_present() {
        let sel = LabelSelector::parse("preempt").unwrap();
        assert!(match_selector(Some(&sel), &labels(&[("preempt", "")])));
        assert!(!match_selector(Some(&sel), &labels(&[("other", "x")])));
    }

    #[test]
    fn conjunction_requires_all_clauses() {
        let sel = LabelSelector::parse("role==worker, tier in (gpu, fast)").unwrap();
        assert!(match_selector(
            Some(&sel),
            &labels(&[("role", "worker"), ("tier", "gpu")])
        ));
        assert!(!match_selector(
            Some(&sel),
            &labels(&[("role", "worker"), ("tier", "slow")])
        ));
        assert!(!match_selector(
            Some(&sel),
            &labels(&[("role", "control"), ("tier", "gpu")])
        ));
    }

    #[test]
    fn eligible_hosts_excludes_disconnected() {
        let alice = host(1, "alice");
        let bob = host(2, "bob");
        let hosts = vec![alice.clone(), bob.clone()];
        let mut lbls = HashMap::new();
        lbls.insert(alice.id, labels(&[("role", "worker")]));
        lbls.insert(bob.id, labels(&[("role", "worker")]));
        let mut health = HashMap::new();
        health.insert(alice.id, HostHealth::Healthy);
        health.insert(bob.id, HostHealth::Disconnected { since: Utc::now() });

        let sel = LabelSelector::parse("role==worker").unwrap();
        let out = eligible_hosts(&hosts, &lbls, &health, Some(&sel));
        assert_eq!(out, vec![alice.id]);
    }

    #[test]
    fn eligible_hosts_excludes_flapping() {
        let alice = host(1, "alice");
        let hosts = vec![alice.clone()];
        let mut health = HashMap::new();
        health.insert(
            alice.id,
            HostHealth::Flapping {
                since: Utc::now(),
                flip_count: 2,
            },
        );
        let out = eligible_hosts(&hosts, &HashMap::new(), &health, None);
        assert!(out.is_empty());
    }

    #[test]
    fn eligible_hosts_excludes_unknown_health() {
        // A host without a HostHealth entry is treated as ineligible.
        // This guards against a boot-time race where the scheduler sees
        // a host before the first heartbeat lands.
        let alice = host(1, "alice");
        let hosts = vec![alice];
        let out = eligible_hosts(&hosts, &HashMap::new(), &HashMap::new(), None);
        assert!(out.is_empty());
    }

    #[test]
    fn eligible_hosts_returns_alphabetical_order() {
        let charlie = host(3, "charlie");
        let alice = host(1, "alice");
        let bob = host(2, "bob");
        let hosts = vec![charlie.clone(), alice.clone(), bob.clone()];
        let mut health = HashMap::new();
        health.insert(charlie.id, HostHealth::Healthy);
        health.insert(alice.id, HostHealth::Healthy);
        health.insert(bob.id, HostHealth::Healthy);
        let out = eligible_hosts(&hosts, &HashMap::new(), &health, None);
        assert_eq!(out, vec![alice.id, bob.id, charlie.id]);
    }

    #[test]
    fn eligible_hosts_none_selector_matches_all_healthy() {
        let alice = host(1, "alice");
        let bob = host(2, "bob");
        let hosts = vec![alice.clone(), bob.clone()];
        let mut health = HashMap::new();
        health.insert(alice.id, HostHealth::Healthy);
        health.insert(bob.id, HostHealth::Healthy);
        let out = eligible_hosts(&hosts, &HashMap::new(), &health, None);
        assert_eq!(out.len(), 2);
    }
}
