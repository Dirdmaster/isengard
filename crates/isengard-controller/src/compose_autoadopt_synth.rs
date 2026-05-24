//! Bridge from storage rows to [`crate::compose_synthesize::synthesize`].
//!
//! The auto-adoption tracker only knows about container ids. When it
//! decides to synthesize, this module:
//!
//! 1. Fetches the rich snapshot row + the base container row for every
//!    id in the chosen stack.
//! 2. Drops ids missing either side (the row may have been reaped
//!    between the observation tick and the synthesis tick).
//! 3. Maps the storage row pair onto a
//!    [`crate::compose_synthesize::ContainerRich`].
//! 4. Calls [`crate::compose_synthesize::synthesize`] and persists the
//!    result via [`isengard_storage::Inventory::set_stack_compose`]
//!    with [`isengard_storage::ComposeSource::AutoSynthesized`].
//! 5. Emits a `compose.auto_adopted` journal event.
//!
//! Pure plumbing: the synthesis logic itself lives in
//! [`crate::compose_synthesize`], and the gating lives in
//! [`crate::compose_autoadopt`].

use std::collections::HashMap;

use chrono::{SecondsFormat, Utc};
use isengard_core::Event;
use isengard_storage::{
    ComposeSource, ContainerRichRow, Inventory, Journal, RichHealthcheck, RichMount,
    RichPortMapping, host::HostId,
};
use sha2::{Digest, Sha256};

use crate::bus::EventBus;
use crate::compose_synthesize::{ContainerRich, HealthcheckSpec, MountSpec, PortMapping};
use crate::persist_and_broadcast;

/// Given a slice of operator-visible container ids, return the subset
/// that has a [`ContainerRichRow`] in storage.
///
/// Used as the `rich_lookup` closure for
/// [`crate::compose_autoadopt::run_auto_adoption_pass`]. The auto-adopt
/// tracker treats a smaller-than-input return as the
/// [`MissingRichData`](crate::compose_autoadopt::Decision::MissingRichData)
/// signal and refuses to synthesize.
pub async fn rich_ids_with_data(inventory: &Inventory, ids: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        match isengard_storage::get_container_rich(inventory.pool(), id).await {
            Ok(Some(_)) => out.push(id.clone()),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    container_id = %id,
                    error = %e,
                    "auto-adopt: get_container_rich failed; treating as missing",
                );
            }
        }
    }
    out
}

/// Fetch rich + base rows for every id, synthesize the compose YAML,
/// write it via [`Inventory::set_stack_compose`], and emit a
/// `compose.auto_adopted` journal event.
///
/// Returns `Ok(())` on success or a short error string suitable for
/// the tracker's warning log line. Caller is the heartbeat handler.
pub async fn synthesize_and_persist(
    inventory: &Inventory,
    journal: &Journal,
    bus: &EventBus,
    host_id: HostId,
    stack_name: &str,
    rich_ids: &[String],
) -> Result<(), String> {
    let mut rich: Vec<ContainerRich> = Vec::with_capacity(rich_ids.len());
    for id in rich_ids {
        let rich_row = match isengard_storage::get_container_rich(inventory.pool(), id).await {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) => return Err(format!("get_container_rich({id}): {e}")),
        };
        let base_row = match isengard_storage::get_container(inventory.pool(), id).await {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) => return Err(format!("get_container({id}): {e}")),
        };
        let (Some(service), Some(stack)) = (base_row.service.clone(), base_row.stack.clone())
        else {
            continue;
        };
        rich.push(map_to_container_rich(
            service,
            stack,
            base_row.image,
            rich_row,
        ));
    }

    if rich.is_empty() {
        return Err("no containers had rich + base rows after re-fetch".into());
    }

    let yaml = crate::compose_synthesize::synthesize(&rich);
    let sha = sha256_hex(&yaml);
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);

    let wrote = inventory
        .set_stack_compose(
            host_id,
            stack_name,
            &yaml,
            &sha,
            &now,
            ComposeSource::AutoSynthesized,
        )
        .await
        .map_err(|e| format!("set_stack_compose: {e}"))?;
    if !wrote {
        return Err(format!(
            "set_stack_compose matched zero rows for ({host_id:?}, {stack_name})"
        ));
    }

    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "container_count".into(),
        serde_json::Value::from(rich.len()),
    );
    metadata.insert("sha256".into(), serde_json::Value::from(sha));
    metadata.insert("source".into(), serde_json::Value::from("auto_synthesized"));

    let event = Event {
        kind: "compose.auto_adopted".into(),
        occurred_at: Utc::now(),
        host_id: Some(host_id.into()),
        summary: format!(
            "auto-adopted compose for stack {stack_name} ({} services)",
            rich.len()
        ),
        metadata: serde_json::Value::Object(metadata),
        ..Default::default()
    };
    persist_and_broadcast(journal, bus, event).await;

    Ok(())
}

/// Hex-encoded SHA-256 of `s`, matching the format stored in
/// `stacks.compose_sha256`.
fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fuse one [`ContainerRichRow`] with the `(service, stack, image)`
/// triple from the matching `containers` row into the
/// [`ContainerRich`] shape the synthesizer consumes.
fn map_to_container_rich(
    service: String,
    stack: String,
    image: String,
    row: ContainerRichRow,
) -> ContainerRich {
    ContainerRich {
        service,
        stack,
        image,
        ports: row.ports.into_iter().map(map_port).collect(),
        env: row.env,
        mounts: row.mounts.into_iter().map(map_mount).collect(),
        networks: row.networks,
        restart_policy: row.restart_policy,
        command: row.command,
        entrypoint: row.entrypoint,
        working_dir: row.working_dir,
        user: row.user_spec,
        healthcheck: row.healthcheck.map(map_healthcheck),
        // Labels live on the live container object, not in the storage
        // rich row. v0.1 synthesizes without them; compose readers
        // tolerate missing `labels:`, and `com.docker.compose.*`
        // metadata is re-derived by docker compose on deploy anyway.
        labels: HashMap::new(),
    }
}

/// Convert a storage [`RichPortMapping`] into the synthesizer's
/// [`PortMapping`]. A `host_port` of `0` (runtime didn't publish the
/// port to the host) collapses to `None`, which the synthesizer
/// emits as expose-only (no `published:`).
fn map_port(p: RichPortMapping) -> PortMapping {
    PortMapping {
        host_port: (p.host_port != 0).then_some(p.host_port),
        container_port: p.container_port,
        protocol: p.protocol,
    }
}

/// Route a storage [`RichMount`] to the right [`MountSpec`] variant
/// (`bind`, `tmpfs`, or named/anonymous `volume`). Unknown kinds fall
/// through to `Volume`: the synthesizer renders something compose
/// will accept, and the operator can edit it post-adoption.
fn map_mount(m: RichMount) -> MountSpec {
    match m.kind.as_str() {
        "bind" => MountSpec::Bind {
            source: m.source,
            target: m.target,
            read_only: m.read_only,
        },
        "tmpfs" => MountSpec::Tmpfs {
            target: m.target,
            size_bytes: None,
        },
        _ => MountSpec::Volume {
            name: if m.source.is_empty() {
                None
            } else {
                Some(m.source)
            },
            target: m.target,
            read_only: m.read_only,
        },
    }
}

/// Convert a storage [`RichHealthcheck`] into the synthesizer's
/// [`HealthcheckSpec`]. Nanosecond durations collapse to whole
/// seconds (compose only emits second precision) and zero values
/// drop out so the synthesized YAML doesn't claim an interval the
/// runtime never set.
fn map_healthcheck(h: RichHealthcheck) -> HealthcheckSpec {
    HealthcheckSpec {
        test: h.test,
        interval_secs: ns_to_secs(h.interval_ns),
        timeout_secs: ns_to_secs(h.timeout_ns),
        retries: (h.retries > 0).then_some(h.retries as u32),
        start_period_secs: ns_to_secs(h.start_period_ns),
    }
}

/// Floor-divide a nanosecond duration to whole seconds, mapping `0`
/// and negatives to `None` (the runtime's "field unset" signal).
fn ns_to_secs(ns: i64) -> Option<u64> {
    if ns <= 0 {
        None
    } else {
        Some((ns / 1_000_000_000) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_matches_known_vector() {
        // SHA-256("") = e3b0...b855
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn ns_to_secs_drops_zero_and_negative() {
        assert_eq!(ns_to_secs(0), None);
        assert_eq!(ns_to_secs(-1), None);
        assert_eq!(ns_to_secs(1_000_000_000), Some(1));
        assert_eq!(ns_to_secs(2_500_000_000), Some(2));
    }

    #[test]
    fn map_port_drops_zero_host_port() {
        let p = map_port(RichPortMapping {
            host_ip: String::new(),
            host_port: 0,
            container_port: 8080,
            protocol: "tcp".into(),
        });
        assert!(p.host_port.is_none());
        assert_eq!(p.container_port, 8080);
    }

    #[test]
    fn map_mount_routes_kinds() {
        let bind = map_mount(RichMount {
            kind: "bind".into(),
            source: "/host".into(),
            target: "/in".into(),
            read_only: true,
        });
        assert!(matches!(
            bind,
            MountSpec::Bind {
                read_only: true,
                ..
            }
        ));

        let tmpfs = map_mount(RichMount {
            kind: "tmpfs".into(),
            source: String::new(),
            target: "/tmp".into(),
            read_only: false,
        });
        assert!(matches!(tmpfs, MountSpec::Tmpfs { .. }));

        let vol = map_mount(RichMount {
            kind: "volume".into(),
            source: "data".into(),
            target: "/data".into(),
            read_only: false,
        });
        assert!(matches!(
            vol,
            MountSpec::Volume { name: Some(ref n), .. } if n == "data"
        ));

        let anon = map_mount(RichMount {
            kind: "volume".into(),
            source: String::new(),
            target: "/data".into(),
            read_only: false,
        });
        assert!(matches!(anon, MountSpec::Volume { name: None, .. }));
    }
}
