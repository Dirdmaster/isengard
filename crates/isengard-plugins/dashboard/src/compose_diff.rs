//! v0.3d dashboard-side compose diff. Produces the same JSON shape the
//! agent reports back through `compose_apply::reconcile_stack` so that
//! the dashboard's "Apply preview" and `isd diff` can render a single
//! consistent ops list.
//!
//! Why this lives in the dashboard crate: the agent reconciler pulls in
//! bollard (we'd inherit a heavy dep tree on the controller). The diff
//! we serve from `POST /api/v1/stacks/<id>/diff` only needs to compare
//! two compose YAML strings, which is pure serde_yaml work. The agent
//! still runs its own canonical reconcile against live containers when
//! the file actually changes; this surface is preview-only.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DesiredService {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServiceOp {
    Start {
        service: String,
        image: String,
    },
    Recreate {
        service: String,
        image: String,
        reasons: Vec<String>,
    },
    Stop {
        service: String,
    },
    NoChange {
        service: String,
    },
}

impl ServiceOp {
    pub fn service(&self) -> &str {
        match self {
            ServiceOp::Start { service, .. }
            | ServiceOp::Recreate { service, .. }
            | ServiceOp::Stop { service }
            | ServiceOp::NoChange { service } => service,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcilePlan {
    pub stack: String,
    pub ops: Vec<ServiceOp>,
}

/// Parse `compose.yaml` body. Empty / whitespace-only input parses as
/// "no services," matching the "delete everything" semantics the
/// agent's reconciler implements.
pub fn parse(yaml: &str) -> Result<BTreeMap<String, DesiredService>, String> {
    if yaml.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let root: Value = serde_yaml::from_str(yaml).map_err(|e| e.to_string())?;
    let services_value = match &root {
        Value::Mapping(m) => m.get(Value::String("services".into())),
        _ => None,
    };
    let Some(Value::Mapping(map)) = services_value else {
        return Ok(BTreeMap::new());
    };

    let mut out: BTreeMap<String, DesiredService> = BTreeMap::new();
    for (k, v) in map {
        let Value::String(name) = k else { continue };
        let Value::Mapping(svc) = v else { continue };
        out.insert(name.clone(), parse_service(name, svc));
    }
    Ok(out)
}

fn parse_service(name: &str, m: &Mapping) -> DesiredService {
    let mut svc = DesiredService {
        name: name.to_string(),
        ..Default::default()
    };
    if let Some(Value::String(s)) = m.get(Value::String("image".into())) {
        svc.image = Some(s.clone());
    }
    if let Some(Value::Mapping(env)) = m.get(Value::String("environment".into())) {
        for (k, v) in env {
            let Value::String(key) = k else { continue };
            let val = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => String::new(),
                _ => continue,
            };
            svc.environment.insert(key.clone(), val);
        }
    } else if let Some(Value::Sequence(env_list)) = m.get(Value::String("environment".into())) {
        for e in env_list {
            if let Value::String(s) = e {
                if let Some((k, v)) = s.split_once('=') {
                    svc.environment.insert(k.to_string(), v.to_string());
                } else {
                    svc.environment.insert(s.clone(), String::new());
                }
            }
        }
    }
    if let Some(Value::Mapping(labels)) = m.get(Value::String("labels".into())) {
        for (k, v) in labels {
            let (Value::String(key), Value::String(val)) = (k, v) else {
                continue;
            };
            svc.labels.insert(key.clone(), val.clone());
        }
    }
    if let Some(Value::Sequence(ports)) = m.get(Value::String("ports".into())) {
        for p in ports {
            match p {
                Value::String(s) => svc.ports.push(s.clone()),
                Value::Number(n) => svc.ports.push(n.to_string()),
                _ => {}
            }
        }
    }
    svc
}

/// Diff `current` vs `desired`. Same op shape the agent emits; the
/// dashboard renders both interchangeably.
pub fn diff_yamls(stack: &str, current: &str, desired: &str) -> Result<ReconcilePlan, String> {
    let current = parse(current)?;
    let desired = parse(desired)?;
    Ok(diff_parsed(stack, &current, &desired))
}

fn diff_parsed(
    stack: &str,
    current: &BTreeMap<String, DesiredService>,
    desired: &BTreeMap<String, DesiredService>,
) -> ReconcilePlan {
    let cur_names: BTreeSet<&str> = current.keys().map(String::as_str).collect();
    let want_names: BTreeSet<&str> = desired.keys().map(String::as_str).collect();

    let mut ops: Vec<ServiceOp> = Vec::new();
    for name in &want_names {
        match current.get(*name) {
            None => ops.push(ServiceOp::Start {
                service: (*name).to_string(),
                image: desired[*name].image.clone().unwrap_or_default(),
            }),
            Some(cur) => {
                let want = &desired[*name];
                let reasons = service_drift(want, cur);
                if reasons.is_empty() {
                    ops.push(ServiceOp::NoChange {
                        service: (*name).to_string(),
                    });
                } else {
                    ops.push(ServiceOp::Recreate {
                        service: (*name).to_string(),
                        image: want
                            .image
                            .clone()
                            .unwrap_or_else(|| cur.image.clone().unwrap_or_default()),
                        reasons,
                    });
                }
            }
        }
    }
    for name in cur_names.difference(&want_names) {
        ops.push(ServiceOp::Stop {
            service: (*name).to_string(),
        });
    }
    ops.sort_by(|a, b| a.service().cmp(b.service()));
    ReconcilePlan {
        stack: stack.to_string(),
        ops,
    }
}

fn service_drift(want: &DesiredService, have: &DesiredService) -> Vec<String> {
    let mut reasons = Vec::new();
    match (want.image.as_ref(), have.image.as_ref()) {
        (Some(a), Some(b)) if a == b => {}
        (Some(a), Some(b)) => reasons.push(format!("image: {b} -> {a}")),
        (Some(a), None) => reasons.push(format!("image: <unset> -> {a}")),
        (None, Some(b)) => reasons.push(format!("image: {b} -> <unset>")),
        (None, None) => {}
    }
    for (k, v) in &want.environment {
        match have.environment.get(k) {
            Some(curr) if curr == v => {}
            Some(curr) => reasons.push(format!("env {k}: {curr:?} -> {v:?}")),
            None => reasons.push(format!("env {k}: <unset> -> {v:?}")),
        }
    }
    for k in have.environment.keys() {
        if !want.environment.contains_key(k) {
            reasons.push(format!("env {k}: <removed>"));
        }
    }
    for (k, v) in &want.labels {
        match have.labels.get(k) {
            Some(curr) if curr == v => {}
            Some(curr) => reasons.push(format!("label {k}: {curr:?} -> {v:?}")),
            None => reasons.push(format!("label {k}: <unset> -> {v:?}")),
        }
    }
    for k in have.labels.keys() {
        if !want.labels.contains_key(k) {
            reasons.push(format!("label {k}: <removed>"));
        }
    }
    let want_ports: BTreeSet<&str> = want.ports.iter().map(String::as_str).collect();
    let have_ports: BTreeSet<&str> = have.ports.iter().map(String::as_str).collect();
    if want_ports != have_ports {
        reasons.push(format!("ports: {:?} -> {:?}", have.ports, want.ports));
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_services() {
        let yaml = "services:\n  web:\n    image: nginx\n";
        let parsed = parse(yaml).unwrap();
        assert_eq!(parsed["web"].image.as_deref(), Some("nginx"));
    }

    #[test]
    fn diff_recognises_added_service() {
        let plan = diff_yamls(
            "hello",
            "services: {}\n",
            "services:\n  web:\n    image: nginx\n",
        )
        .unwrap();
        assert!(matches!(plan.ops[0], ServiceOp::Start { .. }));
    }

    #[test]
    fn diff_recognises_removed_service() {
        let plan = diff_yamls(
            "hello",
            "services:\n  legacy:\n    image: alpine\n",
            "services: {}\n",
        )
        .unwrap();
        assert!(matches!(plan.ops[0], ServiceOp::Stop { .. }));
    }

    #[test]
    fn diff_recognises_image_change() {
        let plan = diff_yamls(
            "hello",
            "services:\n  web:\n    image: nginx:1.27\n",
            "services:\n  web:\n    image: nginx:1.28\n",
        )
        .unwrap();
        match &plan.ops[0] {
            ServiceOp::Recreate { reasons, .. } => {
                assert!(reasons.iter().any(|r| r.contains("image:")));
            }
            other => panic!("expected Recreate, got {other:?}"),
        }
    }

    #[test]
    fn diff_returns_nochange_for_identical_yaml() {
        let yaml = "services:\n  web:\n    image: nginx\n";
        let plan = diff_yamls("hello", yaml, yaml).unwrap();
        assert!(matches!(plan.ops[0], ServiceOp::NoChange { .. }));
    }

    #[test]
    fn diff_invalid_yaml_returns_err() {
        assert!(
            diff_yamls(
                "hello",
                "not: valid: yaml",
                "services:\n  web:\n    image: nginx\n"
            )
            .is_err()
        );
    }
}
