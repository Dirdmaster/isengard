//! Glue between the agent's heartbeat payload and the auto-adoption
//! tracker.
//!
//! The tracker (in [`crate::compose_autoadopt`]) is wire-shape
//! agnostic on purpose: it takes a `Vec<(stack_name, container_ids)>`
//! and decides what to do. This module owns the small bit of plumbing
//! that turns one `Heartbeat.containers` slice into that input shape
//! by:
//!
//! 1. Grouping containers by their `stack` field (set when the
//!    container carries the `com.docker.compose.project` label or
//!    the `isengard.stack=<name>` override).
//! 2. Deriving the operator-visible container id from
//!    `(host_hostname, runtime_container_id)` so the ids match what
//!    `containers_rich` is keyed on. Containers with an empty
//!    `runtime_container_id` are dropped (matches the existing
//!    `sync_containers` filter).
//! 3. Skipping containers without a stack name (no
//!    `com.docker.compose.project`, no manual override): those are
//!    the bare `docker run ...` containers, which auto-adoption
//!    intentionally does not touch (one-off lifestyle containers
//!    aren't a stack).
//!
//! Pure function; no IO. Lives in its own module so the heartbeat
//! handler reads as one straight-line block instead of a
//! 30-line inline construction.

use std::collections::BTreeMap;

use isengard_proto::pb::ContainerInfo;

use crate::container_id::derive_container_id;

/// Builds the `[(stack_name, container_ids)]` input shape expected by
/// [`crate::compose_autoadopt::run_auto_adoption_pass`].
///
/// Containers without a stack name are dropped; containers with an
/// empty `runtime_container_id` are dropped. The resulting outer
/// `Vec` is sorted by stack name so the iteration order is
/// deterministic (mainly useful in tests; production callers don't
/// rely on the order).
#[must_use]
pub fn group_containers_by_stack(
    host_hostname: &str,
    containers: &[ContainerInfo],
) -> Vec<(String, Vec<String>)> {
    let mut by_stack: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for info in containers {
        if info.runtime_container_id.is_empty() {
            continue;
        }
        if info.stack.is_empty() {
            continue;
        }
        let derived = derive_container_id(host_hostname, &info.runtime_container_id);
        by_stack
            .entry(info.stack.clone())
            .or_default()
            .push(derived);
    }
    by_stack.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ci(rid: &str, stack: &str) -> ContainerInfo {
        ContainerInfo {
            runtime_container_id: rid.into(),
            image: "nginx".into(),
            command: String::new(),
            state: "running".into(),
            status_message: String::new(),
            names: String::new(),
            stack: stack.into(),
            service: String::new(),
            created_at_ms: 0,
            observed_at_ms: 0,
            rich: None,
        }
    }

    #[test]
    fn groups_containers_by_stack_name() {
        let host = "iso-1";
        let containers = vec![
            ci("aaaa1", "servarr"),
            ci("bbbb2", "servarr"),
            ci("cccc3", "plex"),
        ];
        let groups = group_containers_by_stack(host, &containers);
        assert_eq!(groups.len(), 2);
        // BTreeMap orders alphabetically.
        assert_eq!(groups[0].0, "plex");
        assert_eq!(groups[1].0, "servarr");
        assert_eq!(groups[0].1.len(), 1);
        assert_eq!(groups[1].1.len(), 2);
    }

    #[test]
    fn drops_containers_without_runtime_id_or_stack() {
        let containers = vec![
            ci("", "servarr"),      // no runtime id
            ci("aaaa1", ""),        // no stack
            ci("bbbb2", "servarr"), // keeps
        ];
        let groups = group_containers_by_stack("h", &containers);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "servarr");
        assert_eq!(groups[0].1.len(), 1);
    }

    #[test]
    fn empty_input_yields_empty_groups() {
        let groups = group_containers_by_stack("h", &[]);
        assert!(groups.is_empty());
    }

    #[test]
    fn derived_id_matches_sync_containers_filter() {
        // group_containers_by_stack must produce ids that match what
        // sync_containers uses, so a future rich-data lookup keyed on
        // `containers.id` finds the right row. We assert that the
        // derived id is non-empty and deterministic.
        let host = "iso-2";
        let containers = vec![ci("deadbeef", "media")];
        let g1 = group_containers_by_stack(host, &containers);
        let g2 = group_containers_by_stack(host, &containers);
        assert_eq!(g1, g2, "derivation is deterministic");
        assert_eq!(g1.len(), 1);
        assert!(!g1[0].1[0].is_empty(), "derived id is non-empty");
    }
}
