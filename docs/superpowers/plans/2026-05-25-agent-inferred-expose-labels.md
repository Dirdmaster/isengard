# Agent-Inferred Expose Labels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `isengard.expose=<hostname>` enough for common routing by having the agent infer the upstream port and having doctor validate and repair the label contract.

**Architecture:** Keep label parsing in `isengard-core`, infer host-local ports in the agent label watcher, and make the controller store only complete label-source routing rules. Doctor becomes the operator memory layer: it writes hostname-only labels when safe and writes `isengard.expose.port` only for ambiguity or invalid overrides.

**Tech Stack:** Rust, tonic/prost protobuf, clap, serde YAML/TOML, sqlx-backed in-memory storage tests, cargo test, cargo clippy.

---

## File Structure

- Modify `crates/isengard-proto/proto/isengard.v1.proto`: add a `LabelRouteIntent` message and attach `repeated LabelRouteIntent label_route_intents = 5` to `ContainerLabelsReport`.
- Create `crates/isengard-proto/tests/label_route_intent_roundtrip.rs`: lock the additive protobuf shape.
- Modify `crates/isengard-core/src/labels.rs`: clarify missing port semantics and add parser regression tests.
- Modify `crates/isengard-agent/src/labels.rs`: infer label route intents from inspected `ContainerSnapshot` data and include them in `ContainerLabelsReport`.
- Modify `crates/isengard-controller/src/routing.rs`: resolve route ports from explicit labels first, then agent-supplied intents, never `80` by default.
- Modify `crates/isd/src/doctor/mod.rs`: add fix metadata for port override repairs and print label reference output.
- Modify `crates/isd/src/doctor/checks/expose_host.rs`: validate hostname-only labels, ambiguous ports, and invalid explicit port labels.
- Modify `crates/isd/src/doctor/fixers/mod.rs`: register the new expose-port fixer.
- Modify `crates/isd/src/doctor/fixers/expose_host.rs`: write port-only overrides when a hostname label already exists.
- Keep existing doctor document/apply machinery in `crates/isd/src/doctor/document.rs` and `crates/isd/src/doctor/apply.rs`.

## Task 1: Extend The Label Report Wire Shape

**Files:**
- Modify: `crates/isengard-proto/proto/isengard.v1.proto`
- Create: `crates/isengard-proto/tests/label_route_intent_roundtrip.rs`

- [ ] **Step 1: Write the failing proto roundtrip test**

Create `crates/isengard-proto/tests/label_route_intent_roundtrip.rs`:

```rust
use isengard_proto::pb::{ContainerLabelsReport, LabelRouteIntent};
use prost::Message;

#[test]
fn label_route_intents_round_trip() {
    let report = ContainerLabelsReport {
        container_id: "cid-plex".into(),
        container_name: "plex".into(),
        image: "lscr.io/linuxserver/plex:latest".into(),
        labels: [("isengard.expose".into(), "plex.vallee.casa".into())]
            .into_iter()
            .collect(),
        label_route_intents: vec![LabelRouteIntent {
            name: String::new(),
            hostname: "plex.vallee.casa".into(),
            container_port: 32400,
            unresolved_reason: String::new(),
        }],
    };

    let mut buf = Vec::new();
    report.encode(&mut buf).unwrap();
    let got = ContainerLabelsReport::decode(buf.as_slice()).unwrap();

    assert_eq!(got.label_route_intents.len(), 1);
    assert_eq!(got.label_route_intents[0].hostname, "plex.vallee.casa");
    assert_eq!(got.label_route_intents[0].container_port, 32400);
    assert!(got.label_route_intents[0].unresolved_reason.is_empty());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p isengard-proto --test label_route_intent_roundtrip`

Expected: compile failure mentioning missing `LabelRouteIntent` or missing `label_route_intents` field.

- [ ] **Step 3: Add the protobuf fields**

In `crates/isengard-proto/proto/isengard.v1.proto`, replace `ContainerLabelsReport` with this additive shape:

```proto
// Agent -> controller: container metadata snapshot.
//
// The label-driven router derives routing rules from these.
message ContainerLabelsReport {
  // Runtime container id.
  string              container_id         = 1;
  // Container name.
  string              container_name       = 2;
  // Image reference.
  string              image                = 3;
  // Full label map from the container's manifest.
  map<string, string> labels               = 4;
  // Agent-resolved route intents for `isengard.expose*` labels.
  repeated LabelRouteIntent label_route_intents = 5;
}

// Agent-side interpretation of one `isengard.expose*` label rule.
//
// `name` is empty for the unnamed `isengard.expose` rule. `container_port`
// is zero when the agent could not infer a single safe upstream port;
// `unresolved_reason` then explains what doctor should surface.
message LabelRouteIntent {
  // Named expose rule, or empty for the default rule.
  string name = 1;
  // Public hostname parsed from the label.
  string hostname = 2;
  // Inferred or explicit container port. Zero means unresolved.
  uint32 container_port = 3;
  // Empty when resolved. Human-readable reason when unresolved.
  string unresolved_reason = 4;
}
```

- [ ] **Step 4: Run the proto test to verify it passes**

Run: `cargo test -p isengard-proto --test label_route_intent_roundtrip`

Expected: `1 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-proto/proto/isengard.v1.proto crates/isengard-proto/tests/label_route_intent_roundtrip.rs
git commit -m "feat: add label route intents to reports"
```

## Task 2: Lock Core Label Parser Semantics

**Files:**
- Modify: `crates/isengard-core/src/labels.rs`

- [ ] **Step 1: Add parser regression tests**

Append this test module to `crates/isengard-core/src/labels.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn parse(pairs: &[(&str, &str)]) -> Vec<LabelRule> {
        let labels = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        parse_labels(&labels)
    }

    #[test]
    fn hostname_only_rule_has_no_port_until_agent_infers_it() {
        let rules = parse(&[("isengard.expose", "plex.vallee.casa")]);

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, None);
        assert_eq!(rules[0].hostname, "plex.vallee.casa");
        assert_eq!(rules[0].port, None);
    }

    #[test]
    fn explicit_port_remains_an_override() {
        let rules = parse(&[
            ("isengard.expose", "plex.vallee.casa"),
            ("isengard.expose.port", "32400"),
        ]);

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].port, Some(32400));
    }

    #[test]
    fn invalid_port_is_unresolved_not_zero_or_eighty() {
        let rules = parse(&[
            ("isengard.expose", "bad.vallee.casa"),
            ("isengard.expose.port", "not-a-port"),
        ]);

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].port, None);
    }

    #[test]
    fn named_rule_without_port_stays_unresolved() {
        let rules = parse(&[("isengard.expose.admin", "admin.vallee.casa")]);

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name.as_deref(), Some("admin"));
        assert_eq!(rules[0].port, None);
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p isengard-core labels`

Expected: tests pass or fail only on duplicate `mod tests` naming if the file already has a test module.

- [ ] **Step 3: If needed, merge test modules**

If `labels.rs` already has `mod tests`, move the new test functions into the existing module and keep the `parse` helper only once:

```rust
fn parse(pairs: &[(&str, &str)]) -> Vec<LabelRule> {
    let labels = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    parse_labels(&labels)
}
```

- [ ] **Step 4: Update the port doc comment**

Change the `LabelRule::port` comment to:

```rust
/// Upstream port override. `None` means the agent must infer the port
/// from local runtime data before the controller inserts a route.
pub port: Option<u16>,
```

- [ ] **Step 5: Run the tests again**

Run: `cargo test -p isengard-core labels`

Expected: all `labels` tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-core/src/labels.rs
git commit -m "test: lock expose label parser semantics"
```

## Task 3: Infer Route Ports In The Agent Label Watcher

**Files:**
- Modify: `crates/isengard-agent/src/labels.rs`

- [ ] **Step 1: Write failing agent tests**

Inside `crates/isengard-agent/src/labels.rs`, add these tests to the existing `mod tests`:

```rust
#[test]
fn labels_report_includes_inferred_plex_port() {
    let mut snap = snap_with_labels("plex", &[("isengard.expose", "plex.vallee.casa")]);
    snap.network_settings.ports.insert("32400/tcp".into(), Vec::new());

    let report = snapshot_to_report(&snap).expect("labels report");

    assert_eq!(report.label_route_intents.len(), 1);
    assert_eq!(report.label_route_intents[0].name, "");
    assert_eq!(report.label_route_intents[0].hostname, "plex.vallee.casa");
    assert_eq!(report.label_route_intents[0].container_port, 32400);
    assert_eq!(report.label_route_intents[0].unresolved_reason, "");
}

#[test]
fn explicit_port_label_wins_over_runtime_candidates() {
    let mut snap = snap_with_labels(
        "plex",
        &[
            ("isengard.expose", "plex.vallee.casa"),
            ("isengard.expose.port", "32400"),
        ],
    );
    snap.network_settings.ports.insert("80/tcp".into(), Vec::new());

    let report = snapshot_to_report(&snap).expect("labels report");

    assert_eq!(report.label_route_intents[0].container_port, 32400);
}

#[test]
fn ambiguous_runtime_ports_report_unresolved_intent() {
    let mut snap = snap_with_labels("qbittorrent", &[("isengard.expose", "qb.vallee.casa")]);
    snap.network_settings.ports.insert("8080/tcp".into(), Vec::new());
    snap.network_settings.ports.insert("6881/tcp".into(), Vec::new());

    let report = snapshot_to_report(&snap).expect("labels report");

    assert_eq!(report.label_route_intents.len(), 1);
    assert_eq!(report.label_route_intents[0].container_port, 0);
    assert!(
        report.label_route_intents[0]
            .unresolved_reason
            .contains("multiple candidate ports")
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p isengard-agent labels_report_includes_inferred_plex_port explicit_port_label_wins_over_runtime_candidates ambiguous_runtime_ports_report_unresolved_intent`

Expected: cargo rejects multiple test filters. Re-run as `cargo test -p isengard-agent label_route_intents` after renaming the three tests to include `label_route_intents` in their names, or run `cargo test -p isengard-agent labels`.

Expected failure after using a valid filter: compile errors for missing `label_route_intents` assignment or missing helper functions.

- [ ] **Step 3: Import the new proto type**

Update the import block near the top of `crates/isengard-agent/src/labels.rs`:

```rust
use isengard_proto::pb::{
    AgentMessage, ContainerLabelsRemoved, ContainerLabelsReport, LabelRouteIntent, agent_message,
};
```

- [ ] **Step 4: Add route intent helpers**

Add these helpers below `snapshot_to_report`:

```rust
fn label_route_intents(snap: &ContainerSnapshot) -> Vec<LabelRouteIntent> {
    let labels: std::collections::HashMap<String, String> = snap
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let candidates = infer_container_ports(snap);

    isengard_core::labels::parse_labels(&labels)
        .into_iter()
        .map(|rule| {
            let (container_port, unresolved_reason) = match rule.port {
                Some(port) => (u32::from(port), String::new()),
                None => match candidates.as_slice() {
                    [only] => (u32::from(*only), String::new()),
                    [] => (0, "no candidate ports from inspect".to_string()),
                    many => (
                        0,
                        format!(
                            "multiple candidate ports: {}",
                            many.iter()
                                .map(u16::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ),
                },
            };
            LabelRouteIntent {
                name: rule.name.unwrap_or_default(),
                hostname: rule.hostname,
                container_port,
                unresolved_reason,
            }
        })
        .collect()
}

fn infer_container_ports(snap: &ContainerSnapshot) -> Vec<u16> {
    let mut ports = Vec::new();
    for key in snap.network_settings.ports.keys() {
        push_port_key(&mut ports, key);
    }
    for binding in &snap.port_bindings {
        let container_side = binding.rsplit(':').next().unwrap_or(binding.as_str());
        push_port_key(&mut ports, container_side);
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn push_port_key(out: &mut Vec<u16>, key: &str) {
    let Some(port_str) = key.split('/').next() else {
        return;
    };
    if let Ok(port) = port_str.parse::<u16>() {
        out.push(port);
    }
}
```

- [ ] **Step 5: Include route intents in `snapshot_to_report`**

Change the report construction to:

```rust
Some(ContainerLabelsReport {
    container_id: snap.id.clone(),
    container_name: snap.name.clone(),
    image: snap.image.clone(),
    labels,
    label_route_intents: label_route_intents(snap),
})
```

- [ ] **Step 6: Update existing tests that construct `ContainerLabelsReport` directly**

Search: `ContainerLabelsReport {`

Every direct construction outside generated code needs:

```rust
label_route_intents: Vec::new(),
```

Expected touched files from current grep:

- `crates/isengard-controller/src/policy_ingest.rs`
- `crates/isengard-controller/src/hook_ingest.rs`
- any new routing tests from Task 4

- [ ] **Step 7: Run agent tests**

Run: `cargo test -p isengard-agent labels`

Expected: all label watcher tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/isengard-agent/src/labels.rs crates/isengard-controller/src/policy_ingest.rs crates/isengard-controller/src/hook_ingest.rs
git commit -m "feat: infer expose route ports in agent labels"
```

## Task 4: Stop Controller Label Ingest From Defaulting Missing Ports To 80

**Files:**
- Modify: `crates/isengard-controller/src/routing.rs`

- [ ] **Step 1: Add routing ingest tests**

Append this test module to `crates/isengard-controller/src/routing.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use isengard_proto::pb::LabelRouteIntent;
    use isengard_storage::{EnrollHost, Inventory};
    use std::collections::HashMap;

    async fn setup() -> (Arc<Inventory>, HostId, RoutingPusher) {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let host = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp".into(),
                hostname: "lausanne".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0".into(),
                docker_version: "27".into(),
            })
            .await
            .unwrap();
        let routing = RoutingPusher::new(inv.clone());
        (inv, host, routing)
    }

    fn report(
        pairs: &[(&str, &str)],
        intents: Vec<LabelRouteIntent>,
    ) -> ContainerLabelsReport {
        let labels: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        ContainerLabelsReport {
            container_id: "cid-plex".into(),
            container_name: "plex".into(),
            image: "plex".into(),
            labels,
            label_route_intents: intents,
        }
    }

    #[tokio::test]
    async fn hostname_only_without_resolved_intent_does_not_default_to_80() {
        let (inv, host, routing) = setup().await;
        let r = report(&[("isengard.expose", "plex.vallee.casa")], Vec::new());

        routing.ingest_labels(host, r).await.unwrap();

        assert!(inv.list_routing_rules_for_host(host).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn hostname_only_with_agent_intent_inserts_inferred_port() {
        let (inv, host, routing) = setup().await;
        let r = report(
            &[("isengard.expose", "plex.vallee.casa")],
            vec![LabelRouteIntent {
                name: String::new(),
                hostname: "plex.vallee.casa".into(),
                container_port: 32400,
                unresolved_reason: String::new(),
            }],
        );

        routing.ingest_labels(host, r).await.unwrap();

        let rules = inv.list_routing_rules_for_host(host).await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].public_hostname, "plex.vallee.casa");
        assert_eq!(rules[0].container_port, 32400);
    }

    #[tokio::test]
    async fn explicit_port_label_still_inserts_without_agent_intent() {
        let (inv, host, routing) = setup().await;
        let r = report(
            &[
                ("isengard.expose", "plex.vallee.casa"),
                ("isengard.expose.port", "32400"),
            ],
            Vec::new(),
        );

        routing.ingest_labels(host, r).await.unwrap();

        let rules = inv.list_routing_rules_for_host(host).await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].container_port, 32400);
    }
}
```

- [ ] **Step 2: Run the tests to verify the first one fails**

Run: `cargo test -p isengard-controller routing::tests::hostname_only_without_resolved_intent_does_not_default_to_80`

Expected: failure because a rule is inserted with port `80`.

- [ ] **Step 3: Add route-intent resolution helpers**

Add near `ingest_labels`:

```rust
fn route_intent_port(report: &ContainerLabelsReport, rule: &isengard_core::labels::LabelRule) -> Option<u16> {
    if let Some(port) = rule.port {
        return Some(port);
    }
    let expected_name = rule.name.as_deref().unwrap_or("");
    report
        .label_route_intents
        .iter()
        .find(|intent| intent.name == expected_name && intent.hostname == rule.hostname)
        .and_then(|intent| u16::try_from(intent.container_port).ok())
        .filter(|port| *port != 0)
}
```

- [ ] **Step 4: Use the helper in `ingest_labels`**

Replace:

```rust
let port = rule.port.unwrap_or(80);
```

with:

```rust
let Some(port) = route_intent_port(&report, &rule) else {
    tracing::warn!(
        host_id = %host_id,
        container_id = %report.container_id,
        hostname = %rule.hostname,
        "ingest_labels: skipping expose label with unresolved port"
    );
    continue;
};
```

- [ ] **Step 5: Run routing tests**

Run: `cargo test -p isengard-controller routing::tests`

Expected: the three new tests pass.

- [ ] **Step 6: Run dependent label ingest tests**

Run: `cargo test -p isengard-controller policy_ingest hook_ingest routing`

Expected: policy and hook tests compile after adding empty `label_route_intents` to report constructors.

- [ ] **Step 7: Commit**

```bash
git add crates/isengard-controller/src/routing.rs crates/isengard-controller/src/policy_ingest.rs crates/isengard-controller/src/hook_ingest.rs
git commit -m "fix: require resolved ports for label routes"
```

## Task 5: Teach Doctor The Hostname-Only Label Contract

**Files:**
- Modify: `crates/isd/src/doctor/mod.rs`
- Modify: `crates/isd/src/doctor/checks/expose_host.rs`
- Modify: `crates/isd/src/doctor/fixers/mod.rs`
- Modify: `crates/isd/src/doctor/fixers/expose_host.rs`

- [ ] **Step 1: Add doctor check tests for hostname-only and ambiguous ports**

Add these tests to `crates/isd/src/doctor/checks/expose_host.rs`:

```rust
#[test]
fn hostname_only_single_port_is_healthy() {
    let v = parse(
        r#"
services:
  plex:
    image: plex
    ports: ["32400:32400"]
    labels:
      isengard.expose: plex.vallee.casa
"#,
    );

    assert!(check(&v).is_empty());
}

#[test]
fn hostname_only_ambiguous_ports_warns_for_port_override() {
    let v = parse(
        r#"
services:
  qbittorrent:
    image: qbittorrent
    ports: ["8080:8080", "6881:6881"]
    labels:
      isengard.expose: qb.vallee.casa
"#,
    );

    let findings = check(&v);
    assert!(findings.iter().any(|f| f.id == "EXPOSE_PORT_AMBIGUOUS"));
}

#[test]
fn invalid_port_label_warns_for_repair() {
    let v = parse(
        r#"
services:
  plex:
    image: plex
    ports: ["32400:32400"]
    labels:
      isengard.expose: plex.vallee.casa
      isengard.expose.port: nope
"#,
    );

    let findings = check(&v);
    assert!(findings.iter().any(|f| f.id == "EXPOSE_PORT_INVALID"));
}
```

- [ ] **Step 2: Run the tests to verify failures**

Run: `cargo test -p isd expose_host`

Expected: `EXPOSE_PORT_AMBIGUOUS` and `EXPOSE_PORT_INVALID` tests fail until checks exist.

- [ ] **Step 3: Extend fix metadata**

In `crates/isd/src/doctor/mod.rs`, extend `FixSpec`:

```rust
pub enum FixSpec {
    /// Add `isengard.expose` metadata to a service.
    ExposeService {
        service: String,
        inferred_port: Option<u16>,
    },
    /// Add or replace `isengard.expose.port` for a service that already
    /// has a hostname label.
    SetExposePort {
        service: String,
        candidate_ports: Vec<u16>,
    },
}
```

- [ ] **Step 4: Add expose-port checks**

In `crates/isd/src/doctor/checks/expose_host.rs`, keep `EXPOSE_HOST_MISSING` for services with web-ish ports and no expose labels. Add helpers:

```rust
fn expose_port_label(svc: &serde_yaml::Mapping) -> Option<&str> {
    let labels = svc.get("labels")?;
    if let Some(map) = labels.as_mapping() {
        return map
            .get(Value::String("isengard.expose.port".to_string()))
            .and_then(Value::as_str);
    }
    labels.as_sequence()?.iter().filter_map(Value::as_str).find_map(|entry| {
        let (key, value) = entry.split_once('=')?;
        (key == "isengard.expose.port").then_some(value)
    })
}
```

When `has_expose_label(svc)` is true:

```rust
let candidate_ports = http_ish_ports(svc);
if let Some(port) = expose_port_label(svc) {
    if port.parse::<u16>().is_err() {
        out.push(Finding {
            id: "EXPOSE_PORT_INVALID",
            severity: Severity::Warning,
            message: format!("services.{name} has an invalid `isengard.expose.port` label"),
            hint: Some("choose a numeric container port".into()),
            target: Some(crate::doctor::FindingTarget::Service { name: name.to_string() }),
            fix: Some(crate::doctor::FixSpec::SetExposePort {
                service: name.to_string(),
                candidate_ports,
            }),
        });
    }
    continue;
}
if candidate_ports.len() > 1 {
    out.push(Finding {
        id: "EXPOSE_PORT_AMBIGUOUS",
        severity: Severity::Warning,
        message: format!("services.{name} has an expose hostname but multiple candidate ports"),
        hint: Some("choose the upstream container port for `isengard.expose.port`".into()),
        target: Some(crate::doctor::FindingTarget::Service { name: name.to_string() }),
        fix: Some(crate::doctor::FixSpec::SetExposePort {
            service: name.to_string(),
            candidate_ports,
        }),
    });
}
continue;
```

Use a helper `http_ish_ports(svc) -> Vec<u16>` that returns sorted deduped ports based on the existing `http_ish_port` parser.

- [ ] **Step 5: Add a port-only fixer test**

Add to `crates/isd/src/doctor/fixers/expose_host.rs`:

```rust
#[test]
fn writes_port_override_without_rewriting_hostname() {
    let mut v = parse(
        "services:\n  qbittorrent:\n    labels:\n      isengard.expose: qb.vallee.casa\n",
    );

    let changed = apply_expose_port(&mut v, "qbittorrent", 8080).unwrap();

    assert!(changed);
    assert_eq!(
        v["services"]["qbittorrent"]["labels"]["isengard.expose"].as_str(),
        Some("qb.vallee.casa")
    );
    assert_eq!(
        v["services"]["qbittorrent"]["labels"]["isengard.expose.port"].as_str(),
        Some("8080")
    );
}
```

- [ ] **Step 6: Implement `apply_expose_port`**

Add to `crates/isd/src/doctor/fixers/expose_host.rs`:

```rust
/// Add or replace `isengard.expose.port` while preserving the hostname label.
pub fn apply_expose_port(compose: &mut Value, service_name: &str, port: u16) -> Result<bool> {
    let services = compose
        .get_mut("services")
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| anyhow::anyhow!("compose has no services mapping"))?;
    let service = services
        .get_mut(Value::String(service_name.to_string()))
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| anyhow::anyhow!("service {:?} not found", service_name))?;
    let labels_key = Value::String("labels".to_string());
    let labels = service
        .get_mut(&labels_key)
        .ok_or_else(|| anyhow::anyhow!("service {:?} has no labels", service_name))?;
    if let Some(map) = labels.as_mapping_mut() {
        let key = Value::String("isengard.expose.port".into());
        let new_value = Value::String(port.to_string());
        let changed = map.get(&key) != Some(&new_value);
        map.insert(key, new_value);
        return Ok(changed);
    }
    if let Some(seq) = labels.as_sequence_mut() {
        let wanted = format!("isengard.expose.port={port}");
        for entry in seq.iter_mut() {
            if entry
                .as_str()
                .and_then(|s| s.split_once('=').map(|(key, _)| key))
                == Some("isengard.expose.port")
            {
                let changed = entry.as_str() != Some(wanted.as_str());
                *entry = Value::String(wanted);
                return Ok(changed);
            }
        }
        seq.push(Value::String(wanted));
        return Ok(true);
    }
    Err(anyhow::anyhow!(
        "services.{}.labels must be a map or list",
        service_name
    ))
}
```

- [ ] **Step 7: Register the fixer**

Update `crates/isd/src/doctor/fixers/mod.rs` so `fixer_id_for` returns:

```rust
match &finding.fix {
    Some(FixSpec::ExposeService { .. }) => Some("EXPOSE_HOST_MISSING"),
    Some(FixSpec::SetExposePort { .. }) => Some(finding.id),
    None => None,
}
```

Update `apply_fix_inputs` in `crates/isd/src/doctor/mod.rs` to handle `SetExposePort`. If `candidate_ports` has one port, use it. If it has more than one, prompt the operator with an `inquire::Select<u16>` helper named `prompt_expose_port`.

- [ ] **Step 8: Run doctor tests**

Run: `cargo test -p isd doctor`

Expected: all doctor tests pass.

- [ ] **Step 9: Commit**

```bash
git add crates/isd/src/doctor/mod.rs crates/isd/src/doctor/checks/expose_host.rs crates/isd/src/doctor/fixers/mod.rs crates/isd/src/doctor/fixers/expose_host.rs
git commit -m "feat: validate hostname-only expose labels"
```

## Task 6: Add CLI Label Reference

**Files:**
- Modify: `crates/isd/src/doctor/mod.rs`

- [ ] **Step 1: Add label reference test**

Add to `crates/isd/src/doctor/mod.rs` tests:

```rust
#[test]
fn label_reference_lists_required_optional_and_named_labels() {
    let text = label_reference_text();

    assert!(text.contains("Required:"));
    assert!(text.contains("isengard.expose=<hostname>"));
    assert!(text.contains("Optional:"));
    assert!(text.contains("isengard.expose.port=<container-port>"));
    assert!(text.contains("Named:"));
    assert!(text.contains("isengard.expose.<name>=<hostname>"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p isd label_reference_lists_required_optional_and_named_labels`

Expected: compile failure because `label_reference_text` does not exist.

- [ ] **Step 3: Add the label reference helper**

Add to `crates/isd/src/doctor/mod.rs`:

```rust
fn label_reference_text() -> &'static str {
    "Required:\n  isengard.expose=<hostname>\n\nOptional:\n  isengard.expose.port=<container-port>\n  isengard.expose.tls=acme|edge|manual\n  isengard.expose.adapter=none|tailscale|cf-tunnel\n  isengard.expose.auth=none|...\n  isengard.expose.health=/path\n\nNamed:\n  isengard.expose.<name>=<hostname>\n  isengard.expose.<name>.port=<container-port>\n\nStart with only isengard.expose=<hostname>. Add optional labels only when doctor asks or when you want a non-default behavior.\n"
}
```

- [ ] **Step 4: Wire `isd stack doctor labels`**

At the top of `run`, before `args.fix` handling, add:

```rust
if args.target.as_deref() == Some("labels") {
    print!("{}", label_reference_text());
    return Ok(());
}
```

This reserves the `labels` target for documentation. If a real stack is named `labels`, use `isd stack doctor ./compose.yaml` against its compose source until a richer doctor subcommand shape exists.

- [ ] **Step 5: Add a parse smoke test**

Add to `crates/isd/src/doctor/mod.rs` tests:

```rust
#[test]
fn doctor_args_parse_labels_target() {
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Wrap {
        #[command(subcommand)]
        c: crate::stack_cmd::StackCommand,
    }

    let w = Wrap::try_parse_from(["x", "doctor", "labels"]).unwrap();
    match w.c {
        crate::stack_cmd::StackCommand::Doctor(args) => {
            assert_eq!(args.target.as_deref(), Some("labels"));
        }
        other => panic!("expected doctor, got {other:?}"),
    }
}
```

- [ ] **Step 6: Run CLI docs tests**

Run: `cargo test -p isd label_reference doctor_args_parse_labels_target`

Expected: cargo rejects multiple filters. Re-run as `cargo test -p isd label_reference` and `cargo test -p isd doctor_args_parse_labels_target` separately.

- [ ] **Step 7: Commit**

```bash
git add crates/isd/src/doctor/mod.rs
git commit -m "docs: add expose label reference"
```

## Task 7: Final Verification And PR

**Files:**
- No new code files unless earlier tasks revealed compile fallout.

- [ ] **Step 1: Run package-level focused tests**

Run:

```bash
cargo test -p isengard-proto label_route_intent
cargo test -p isengard-core labels
cargo test -p isengard-agent labels
cargo test -p isengard-controller routing policy_ingest hook_ingest
cargo test -p isd doctor label_reference
```

Expected: all tests pass. If cargo rejects multiple filters, split that line into one command per filter.

- [ ] **Step 2: Run formatting**

Run: `cargo fmt --check`

Expected: no output and exit 0.

- [ ] **Step 3: Run clippy for touched packages**

Run:

```bash
cargo clippy -p isengard-proto -p isengard-core -p isengard-agent -p isengard-controller -p isd --all-targets -- -D warnings
```

Expected: clippy finishes with exit 0.

- [ ] **Step 4: Run fast PR gate**

Run: `just ci-fast`

Expected: fmt, check, and cargo-deny pass. Existing duplicate crate warnings from cargo-deny are acceptable only if the command exits 0.

- [ ] **Step 5: Push and open PR**

Run:

```bash
git push -u origin agent-inferred-expose-labels
gh pr create --base next --head agent-inferred-expose-labels --title "feat: infer expose label ports on agents" --body-file -
```

Use this body:

```markdown
## Summary
- make `isengard.expose=<hostname>` the canonical route label by adding agent-side port inference
- stop controller label ingest from defaulting missing ports to 80
- teach doctor to validate hostname-only labels and write port overrides only for ambiguity
- add CLI label reference for required, optional, and named expose labels

## Verification
- cargo test -p isengard-proto label_route_intent
- cargo test -p isengard-core labels
- cargo test -p isengard-agent labels
- cargo test -p isengard-controller routing policy_ingest hook_ingest
- cargo test -p isd doctor label_reference
- cargo fmt --check
- cargo clippy -p isengard-proto -p isengard-core -p isengard-agent -p isengard-controller -p isd --all-targets -- -D warnings
- just ci-fast
```

- [ ] **Step 6: Watch PR checks**

Run: `gh pr checks <number> --watch --interval 10`

Expected: build, lint, cargo-deny, migrations immutable, and nextest pass.

## Self-Review Checklist

- Spec coverage: Tasks cover proto report shape, core parser semantics, agent inference, controller no-80 behavior, doctor validation/fixers, CLI docs, migration-safe existing explicit labels, and tests.
- Red-flag scan: no incomplete task, no vague test step, no deferred implementation marker.
- Type consistency: `LabelRouteIntent`, `label_route_intents`, `container_port`, and `unresolved_reason` are used consistently across proto, agent, and controller tasks.
- Scope: Traefik import remains out of scope. Existing UI routes keep working. Explicit `.port` labels stay valid.
