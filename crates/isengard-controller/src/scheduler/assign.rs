//! Spread / global / on / singleton assignment math. Phase 0.14 step 5.
//!
//! Each verb maps to a function returning `Vec<(HostId, replica_index)>`
//! pairs. The reconcile loop (step 6) diffs this against the current
//! placement rows in the DB and emits create/drain ops.
//!
//! Determinism: the input `eligible` slice is expected to come from
//! [`crate::scheduler::eligible::eligible_hosts`] (hostname-sorted).
//! Within this module spread additionally re-orders by per-service +
//! global load so the operator can predict where the next replica
//! lands.

use std::collections::HashMap;

use isengard_storage::host::HostId;

use super::PlacementAssignment;

/// Singleton: one replica on the least-loaded eligible host. When the
/// current set already includes a healthy `Active` placement, keep it
/// (avoid churn).
pub fn assign_singleton(
    eligible: &[HostId],
    current: &[PlacementAssignment],
) -> Vec<(HostId, u32)> {
    use isengard_storage::PlacementState;
    // Prefer an existing active placement that's still eligible.
    if let Some(active) = current
        .iter()
        .find(|p| p.state == PlacementState::Active && eligible.contains(&p.host_id))
    {
        return vec![(active.host_id, 0)];
    }
    let Some(&host) = eligible.first() else {
        return vec![];
    };
    vec![(host, 0)]
}

/// Spread: N replicas across eligible hosts. When `count <= eligible`,
/// each gets one replica (replica_index 0..count). When `count >
/// eligible`, replicas stack round-robin onto already-used hosts.
///
/// `load` lets the caller bias the sort by per-host total load so the
/// scheduler doesn't dogpile a single low-name-sorted host across
/// services. `None` means "ignore total load." Within a tie the slice
/// order from `eligible` wins.
pub fn assign_spread(
    count: u32,
    eligible: &[HostId],
    _current: &[PlacementAssignment],
    load: Option<&HashMap<HostId, u32>>,
) -> Vec<(HostId, u32)> {
    if count == 0 || eligible.is_empty() {
        return vec![];
    }
    // Sort by (current total load, original index) for a stable tiebreak.
    let mut sorted: Vec<(usize, HostId, u32)> = eligible
        .iter()
        .enumerate()
        .map(|(idx, h)| {
            let l = load.and_then(|m| m.get(h)).copied().unwrap_or(0);
            (idx, *h, l)
        })
        .collect();
    sorted.sort_by(|a, b| a.2.cmp(&b.2).then(a.0.cmp(&b.0)));
    let ordered: Vec<HostId> = sorted.into_iter().map(|(_, h, _)| h).collect();

    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let host = ordered[(i as usize) % ordered.len()];
        out.push((host, i));
    }
    out
}

/// Global: exactly one replica per eligible host. Indices 0..N.
pub fn assign_global(eligible: &[HostId]) -> Vec<(HostId, u32)> {
    eligible
        .iter()
        .enumerate()
        .map(|(i, h)| (*h, i as u32))
        .collect()
}

/// On: pinned to a host. Returns the single (host, 0) pair if the host
/// is in the eligible set; empty otherwise.
pub fn assign_on(host: HostId, eligible: &[HostId]) -> Vec<(HostId, u32)> {
    if eligible.contains(&host) {
        vec![(host, 0)]
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use isengard_storage::service::ServiceId;
    use isengard_storage::{PlacementRow, PlacementState};

    fn host(i: u8) -> HostId {
        HostId::from_bytes([i; 16])
    }

    fn current(host_id: HostId, replica: u32, state: PlacementState) -> PlacementAssignment {
        PlacementAssignment::from(PlacementRow {
            id: 0,
            service_id: ServiceId(1),
            host_id,
            replica_index: replica,
            state,
            assigned_at: Utc::now(),
            last_event: None,
        })
    }

    #[test]
    fn singleton_picks_first_eligible_when_no_current() {
        let alice = host(1);
        let bob = host(2);
        let out = assign_singleton(&[alice, bob], &[]);
        assert_eq!(out, vec![(alice, 0)]);
    }

    #[test]
    fn singleton_keeps_existing_active_when_eligible() {
        let alice = host(1);
        let bob = host(2);
        let cur = vec![current(bob, 0, PlacementState::Active)];
        let out = assign_singleton(&[alice, bob], &cur);
        assert_eq!(out, vec![(bob, 0)]);
    }

    #[test]
    fn singleton_drops_existing_when_no_longer_eligible() {
        let alice = host(1);
        let bob = host(2);
        let cur = vec![current(bob, 0, PlacementState::Active)];
        let out = assign_singleton(&[alice], &cur);
        assert_eq!(out, vec![(alice, 0)]);
    }

    #[test]
    fn singleton_empty_when_no_eligible() {
        assert!(assign_singleton(&[], &[]).is_empty());
    }

    #[test]
    fn spread_three_replicas_three_hosts_one_each() {
        let a = host(1);
        let b = host(2);
        let c = host(3);
        let out = assign_spread(3, &[a, b, c], &[], None);
        assert_eq!(out, vec![(a, 0), (b, 1), (c, 2)]);
    }

    #[test]
    fn spread_three_replicas_two_hosts_round_robins() {
        let a = host(1);
        let b = host(2);
        let out = assign_spread(3, &[a, b], &[], None);
        // i=0 -> a, i=1 -> b, i=2 -> a (round robin).
        assert_eq!(out, vec![(a, 0), (b, 1), (a, 2)]);
    }

    #[test]
    fn spread_one_replica_one_host() {
        let a = host(1);
        let out = assign_spread(1, &[a, host(2), host(3)], &[], None);
        assert_eq!(out, vec![(a, 0)]);
    }

    #[test]
    fn spread_zero_count_empty() {
        let a = host(1);
        let out = assign_spread(0, &[a], &[], None);
        assert!(out.is_empty());
    }

    #[test]
    fn spread_zero_eligible_empty() {
        let out = assign_spread(3, &[], &[], None);
        assert!(out.is_empty());
    }

    #[test]
    fn spread_prefers_least_loaded_host_first() {
        let a = host(1);
        let b = host(2);
        let mut load = HashMap::new();
        load.insert(a, 5);
        load.insert(b, 1);
        let out = assign_spread(2, &[a, b], &[], Some(&load));
        // Bob is less loaded; he gets replica 0.
        assert_eq!(out, vec![(b, 0), (a, 1)]);
    }

    #[test]
    fn global_one_per_eligible() {
        let a = host(1);
        let b = host(2);
        let c = host(3);
        let out = assign_global(&[a, b, c]);
        assert_eq!(out, vec![(a, 0), (b, 1), (c, 2)]);
    }

    #[test]
    fn global_zero_eligible_empty() {
        assert!(assign_global(&[]).is_empty());
    }

    #[test]
    fn on_returns_pinned_host_when_eligible() {
        let a = host(1);
        let b = host(2);
        let out = assign_on(a, &[a, b]);
        assert_eq!(out, vec![(a, 0)]);
    }

    #[test]
    fn on_returns_empty_when_pinned_host_not_eligible() {
        let a = host(1);
        let b = host(2);
        let out = assign_on(a, &[b]);
        assert!(out.is_empty());
    }
}
