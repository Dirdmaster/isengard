//! v0.3d compose reconciler: parse a desired `compose.yaml`, diff it
//! against the running containers, return a [`ReconcilePlan`] of ops the
//! agent can either apply or surface in `isd diff` / dashboard previews.
//!
//! The plan is intentionally simple: each service is one of
//! `Start | Recreate | Stop | NoChange`. v0.3d targets the smoke-test
//! shape (image + ports + env + labels + restart): full compose semantics
//! (depends_on, env_file, healthcheck.test rewrites, secrets) are
//! follow-ups. The diff logic here is what `isd diff` shows the operator
//! before they say yes.
//!
//! All YAML parsing uses `serde_yaml`. v0.3d still loses comments on
//! round-trip; the dashboard editor's banner calls this out explicitly.

use std::collections::{BTreeMap, BTreeSet};

use bollard::secret::ContainerInspectResponse;
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

/// One service entry in `services:` after parsing. We keep the raw
/// `Value` for fields we don't model so apply can faithfully forward
/// labels / extra config to bollard. Only the fields used by the diff
/// surface are modelled explicitly.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DesiredService {
    pub name: String,
    pub image: Option<String>,
    pub container_name: Option<String>,
    pub command: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub environment: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub ports: Vec<String>,
    pub restart: Option<String>,
}

/// Parsed `compose.yaml`. Only the bits the reconciler needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DesiredCompose {
    pub services: BTreeMap<String, DesiredService>,
}

/// Per-service op the reconciler will take to bring the running state
/// in line with the desired compose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServiceOp {
    /// Service is in compose, no container running. Pull + create + start.
    Start { service: String, image: String },
    /// Service running but image / env / ports / labels drifted.
    /// `reasons` is human-readable so `isd diff` can show why.
    Recreate {
        service: String,
        image: String,
        reasons: Vec<String>,
    },
    /// Container running, service is no longer in compose. Stop + remove.
    Stop {
        service: String,
        container_id: String,
    },
    /// Identical: no work.
    NoChange { service: String },
}

impl ServiceOp {
    pub fn service(&self) -> &str {
        match self {
            ServiceOp::Start { service, .. }
            | ServiceOp::Recreate { service, .. }
            | ServiceOp::Stop { service, .. }
            | ServiceOp::NoChange { service } => service,
        }
    }
}

/// Plan returned to dashboards / `isd diff`. Sorted alphabetically by
/// service name for stable output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcilePlan {
    pub stack: String,
    pub ops: Vec<ServiceOp>,
}

impl ReconcilePlan {
    pub fn is_noop(&self) -> bool {
        self.ops
            .iter()
            .all(|op| matches!(op, ServiceOp::NoChange { .. }))
    }
}

/// Parse `compose.yaml` text into a [`DesiredCompose`]. Strict-ish: the
/// file must have a top-level `services:` mapping; missing `image:` on
/// a service is allowed only if `Start` would have nothing to do (the
/// caller's apply path then has to decide).
pub fn parse_compose(yaml: &str) -> anyhow::Result<DesiredCompose> {
    let root: Value =
        serde_yaml::from_str(yaml).map_err(|e| anyhow::anyhow!("parse compose.yaml: {e}"))?;
    let services_value = match &root {
        Value::Mapping(m) => m.get(Value::String("services".into())),
        _ => None,
    };
    let Some(services_value) = services_value else {
        // No services key means an empty compose. Useful for the "stop
        // everything" case (delete the file -> reconcile says stop all).
        return Ok(DesiredCompose::default());
    };
    let Value::Mapping(services_map) = services_value else {
        return Err(anyhow::anyhow!("`services:` must be a mapping"));
    };

    let mut out = DesiredCompose::default();
    for (k, v) in services_map {
        let Value::String(name) = k else { continue };
        let Value::Mapping(svc) = v else { continue };
        let parsed = parse_service(name, svc)?;
        out.services.insert(name.clone(), parsed);
    }
    Ok(out)
}

fn parse_service(name: &str, m: &Mapping) -> anyhow::Result<DesiredService> {
    let mut svc = DesiredService {
        name: name.to_string(),
        ..Default::default()
    };
    if let Some(Value::String(s)) = m.get(Value::String("image".into())) {
        svc.image = Some(s.clone());
    }
    if let Some(Value::String(s)) = m.get(Value::String("container_name".into())) {
        svc.container_name = Some(s.clone());
    }
    if let Some(v) = m.get(Value::String("command".into())) {
        svc.command = Some(string_seq_or_split(v));
    }
    if let Some(v) = m.get(Value::String("entrypoint".into())) {
        svc.entrypoint = Some(string_seq_or_split(v));
    }
    if let Some(Value::String(s)) = m.get(Value::String("restart".into())) {
        svc.restart = Some(s.clone());
    }
    if let Some(Value::Sequence(seq)) = m.get(Value::String("ports".into())) {
        for p in seq {
            if let Value::String(s) = p {
                svc.ports.push(s.clone());
            } else if let Value::Number(n) = p {
                svc.ports.push(n.to_string());
            }
        }
    }
    if let Some(Value::Mapping(env)) = m.get(Value::String("environment".into())) {
        for (k, v) in env {
            let key = match k {
                Value::String(s) => s.clone(),
                _ => continue,
            };
            let val = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => String::new(),
                _ => continue,
            };
            svc.environment.insert(key, val);
        }
    } else if let Some(Value::Sequence(env_list)) = m.get(Value::String("environment".into())) {
        // List form: ["KEY=value", "OTHER=foo"]
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
    Ok(svc)
}

/// Compose accepts both `command: foo bar` and `command: [foo, bar]`.
/// Normalise to a `Vec<String>`.
fn string_seq_or_split(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => s.split_whitespace().map(str::to_string).collect(),
        Value::Sequence(seq) => seq
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Snapshot of one running container, projected to the same shape the
/// reconciler diffs against. Built from `bollard::Docker::inspect_container`
/// in the apply path; tests build it directly.
#[derive(Debug, Clone, Default)]
pub struct RunningService {
    pub service_name: String,
    pub container_id: String,
    pub image: String,
    pub environment: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub port_bindings: Vec<String>,
    pub restart: Option<String>,
}

impl RunningService {
    /// Build a [`RunningService`] from a bollard `inspect_container`
    /// response. Returns `None` when the container has no service-name
    /// label / is otherwise unmappable.
    pub fn from_inspect(inspect: &ContainerInspectResponse) -> Option<Self> {
        let labels = inspect
            .config
            .as_ref()
            .and_then(|c| c.labels.as_ref())?
            .clone();
        let svc = labels
            .get("com.docker.compose.service")
            .or_else(|| labels.get("isengard.service"))
            .cloned()?;

        let mut env = BTreeMap::new();
        if let Some(env_list) = inspect.config.as_ref().and_then(|c| c.env.as_ref()) {
            for e in env_list {
                if let Some((k, v)) = e.split_once('=') {
                    if k == "PATH"
                        || k == "HOSTNAME"
                        || k.starts_with("com.docker.")
                        || k.starts_with("COMPOSE_")
                    {
                        continue;
                    }
                    env.insert(k.to_string(), v.to_string());
                }
            }
        }

        let labels_map: BTreeMap<String, String> = labels
            .into_iter()
            .filter(|(k, _)| !k.starts_with("com.docker.compose."))
            .collect();

        let mut ports: Vec<String> = Vec::new();
        if let Some(pb) = inspect
            .host_config
            .as_ref()
            .and_then(|h| h.port_bindings.as_ref())
        {
            let mut keys: Vec<&String> = pb.keys().collect();
            keys.sort();
            for cport in keys {
                let Some(Some(bs)) = pb.get(cport).map(|v| v.as_ref()) else {
                    continue;
                };
                for b in bs {
                    let host = b.host_port.as_deref().unwrap_or("");
                    let cport_compose: String = match cport.split_once('/') {
                        Some((p, "tcp")) => p.to_string(),
                        _ => cport.clone(),
                    };
                    if host.is_empty() {
                        ports.push(cport_compose);
                    } else {
                        ports.push(format!("{host}:{cport_compose}"));
                    }
                }
            }
            ports.sort();
        }

        let restart = inspect
            .host_config
            .as_ref()
            .and_then(|h| h.restart_policy.as_ref())
            .and_then(|r| r.name)
            .and_then(|n| match n {
                bollard::secret::RestartPolicyNameEnum::ALWAYS => Some("always".to_string()),
                bollard::secret::RestartPolicyNameEnum::UNLESS_STOPPED => {
                    Some("unless-stopped".to_string())
                }
                bollard::secret::RestartPolicyNameEnum::ON_FAILURE => {
                    Some("on-failure".to_string())
                }
                bollard::secret::RestartPolicyNameEnum::NO => Some("no".to_string()),
                bollard::secret::RestartPolicyNameEnum::EMPTY => None,
            });

        Some(Self {
            service_name: svc,
            container_id: inspect.id.clone().unwrap_or_default(),
            image: inspect
                .config
                .as_ref()
                .and_then(|c| c.image.clone())
                .unwrap_or_default(),
            environment: env,
            labels: labels_map,
            port_bindings: ports,
            restart,
        })
    }
}

/// Diff `desired` against `running` and return a deterministic
/// [`ReconcilePlan`].
pub fn build_plan(
    stack: &str,
    desired: &DesiredCompose,
    running: &[RunningService],
) -> ReconcilePlan {
    let running_by_name: BTreeMap<&str, &RunningService> = running
        .iter()
        .map(|r| (r.service_name.as_str(), r))
        .collect();
    let desired_names: BTreeSet<&str> = desired.services.keys().map(String::as_str).collect();
    let running_names: BTreeSet<&str> = running_by_name.keys().copied().collect();

    let mut ops: Vec<ServiceOp> = Vec::new();

    // Services in compose.
    for name in desired_names.iter() {
        let want = &desired.services[*name];
        match running_by_name.get(name) {
            None => ops.push(ServiceOp::Start {
                service: (*name).to_string(),
                image: want.image.clone().unwrap_or_default(),
            }),
            Some(have) => {
                let reasons = service_drift(want, have);
                if reasons.is_empty() {
                    ops.push(ServiceOp::NoChange {
                        service: (*name).to_string(),
                    });
                } else {
                    ops.push(ServiceOp::Recreate {
                        service: (*name).to_string(),
                        image: want.image.clone().unwrap_or_else(|| have.image.clone()),
                        reasons,
                    });
                }
            }
        }
    }

    // Services running but no longer in compose.
    for name in running_names.difference(&desired_names) {
        let have = running_by_name[name];
        ops.push(ServiceOp::Stop {
            service: (*name).to_string(),
            container_id: have.container_id.clone(),
        });
    }

    ops.sort_by(|a, b| a.service().cmp(b.service()));
    ReconcilePlan {
        stack: stack.to_string(),
        ops,
    }
}

fn service_drift(want: &DesiredService, have: &RunningService) -> Vec<String> {
    let mut reasons: Vec<String> = Vec::new();
    if let Some(image) = want.image.as_ref() {
        if image != &have.image {
            reasons.push(format!("image: {} -> {}", have.image, image));
        }
    }
    // Environment drift: only check keys we care about. `want` is the
    // truth for managed keys; extras the runtime sets (e.g. PATH) are
    // already filtered out of `have`.
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
    // Labels drift.
    for (k, v) in &want.labels {
        match have.labels.get(k) {
            Some(curr) if curr == v => {}
            Some(curr) => reasons.push(format!("label {k}: {curr:?} -> {v:?}")),
            None => reasons.push(format!("label {k}: <unset> -> {v:?}")),
        }
    }
    for k in have.labels.keys() {
        if !want.labels.contains_key(k)
            && !k.starts_with("isengard.")
            && !k.starts_with("com.docker.")
        {
            reasons.push(format!("label {k}: <removed>"));
        }
    }
    // Ports: order-insensitive set comparison.
    let want_ports: BTreeSet<&str> = want.ports.iter().map(String::as_str).collect();
    let have_ports: BTreeSet<&str> = have.port_bindings.iter().map(String::as_str).collect();
    if want_ports != have_ports {
        reasons.push(format!(
            "ports: {:?} -> {:?}",
            have.port_bindings, want.ports
        ));
    }
    if let (Some(w), Some(h)) = (want.restart.as_ref(), have.restart.as_ref()) {
        if w != h {
            reasons.push(format!("restart: {h} -> {w}"));
        }
    } else if want.restart.is_some() && have.restart.is_none() {
        reasons.push(format!("restart: <unset> -> {:?}", want.restart));
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, image: &str) -> DesiredService {
        DesiredService {
            name: name.to_string(),
            image: Some(image.to_string()),
            ..Default::default()
        }
    }

    fn running(name: &str, image: &str, cid: &str) -> RunningService {
        RunningService {
            service_name: name.to_string(),
            container_id: cid.to_string(),
            image: image.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn parse_compose_extracts_services_and_image() {
        let yaml = "services:\n  web:\n    image: nginx:1.27\n";
        let parsed = parse_compose(yaml).unwrap();
        assert_eq!(parsed.services.len(), 1);
        assert_eq!(parsed.services["web"].image.as_deref(), Some("nginx:1.27"));
    }

    #[test]
    fn parse_compose_with_environment_map() {
        let yaml = "services:\n  web:\n    image: nginx\n    environment:\n      FOO: bar\n      BAZ: '1'\n";
        let parsed = parse_compose(yaml).unwrap();
        let env = &parsed.services["web"].environment;
        assert_eq!(env["FOO"], "bar");
        assert_eq!(env["BAZ"], "1");
    }

    #[test]
    fn parse_compose_with_environment_list() {
        let yaml = "services:\n  web:\n    image: nginx\n    environment:\n      - FOO=bar\n      - LONE\n";
        let parsed = parse_compose(yaml).unwrap();
        let env = &parsed.services["web"].environment;
        assert_eq!(env["FOO"], "bar");
        assert_eq!(env["LONE"], "");
    }

    #[test]
    fn parse_compose_missing_services_block_yields_empty() {
        let yaml = "version: '3'\n";
        let parsed = parse_compose(yaml).unwrap();
        assert!(parsed.services.is_empty());
    }

    #[test]
    fn parse_compose_invalid_yaml_errors() {
        assert!(parse_compose(":\n  not: yaml: at-all").is_err());
    }

    #[test]
    fn plan_starts_new_service_not_yet_running() {
        let mut desired = DesiredCompose::default();
        desired
            .services
            .insert("web".into(), svc("web", "nginx:1.27"));
        let plan = build_plan("hello", &desired, &[]);
        assert_eq!(plan.ops.len(), 1);
        assert!(matches!(
            &plan.ops[0],
            ServiceOp::Start { service, image } if service == "web" && image == "nginx:1.27"
        ));
    }

    #[test]
    fn plan_recreates_when_image_drifts() {
        let mut desired = DesiredCompose::default();
        desired
            .services
            .insert("web".into(), svc("web", "nginx:1.28"));
        let running = vec![running("web", "nginx:1.27", "c1")];
        let plan = build_plan("hello", &desired, &running);
        let op = &plan.ops[0];
        match op {
            ServiceOp::Recreate {
                service, reasons, ..
            } => {
                assert_eq!(service, "web");
                assert!(reasons.iter().any(|r| r.contains("image:")));
            }
            other => panic!("expected Recreate, got {other:?}"),
        }
    }

    #[test]
    fn plan_marks_nochange_when_identical() {
        let mut desired = DesiredCompose::default();
        desired
            .services
            .insert("web".into(), svc("web", "nginx:1.27"));
        let running = vec![running("web", "nginx:1.27", "c1")];
        let plan = build_plan("hello", &desired, &running);
        assert!(matches!(plan.ops[0], ServiceOp::NoChange { .. }));
        assert!(plan.is_noop());
    }

    #[test]
    fn plan_stops_service_no_longer_in_compose() {
        let desired = DesiredCompose::default();
        let running = vec![running("legacy", "alpine", "c2")];
        let plan = build_plan("hello", &desired, &running);
        assert_eq!(plan.ops.len(), 1);
        assert!(matches!(
            &plan.ops[0],
            ServiceOp::Stop { service, container_id } if service == "legacy" && container_id == "c2"
        ));
    }

    #[test]
    fn plan_recreate_on_env_drift() {
        let mut desired = DesiredCompose::default();
        let mut want = svc("web", "nginx");
        want.environment.insert("TZ".into(), "UTC".into());
        desired.services.insert("web".into(), want);

        let mut have = running("web", "nginx", "c1");
        have.environment.insert("TZ".into(), "Europe/Zurich".into());
        let plan = build_plan("hello", &desired, &[have]);
        match &plan.ops[0] {
            ServiceOp::Recreate { reasons, .. } => {
                assert!(reasons.iter().any(|r| r.contains("env TZ")));
            }
            other => panic!("expected Recreate, got {other:?}"),
        }
    }

    #[test]
    fn plan_recreate_on_label_drift() {
        let mut desired = DesiredCompose::default();
        let mut want = svc("web", "nginx");
        want.labels.insert("traefik.enable".into(), "true".into());
        desired.services.insert("web".into(), want);

        let mut have = running("web", "nginx", "c1");
        have.labels.insert("traefik.enable".into(), "false".into());
        let plan = build_plan("hello", &desired, &[have]);
        match &plan.ops[0] {
            ServiceOp::Recreate { reasons, .. } => {
                assert!(reasons.iter().any(|r| r.contains("label traefik.enable")));
            }
            other => panic!("expected Recreate, got {other:?}"),
        }
    }

    #[test]
    fn plan_recreate_on_port_drift() {
        let mut desired = DesiredCompose::default();
        let mut want = svc("web", "nginx");
        want.ports.push("8080:80".into());
        desired.services.insert("web".into(), want);

        let mut have = running("web", "nginx", "c1");
        have.port_bindings.push("9090:80".into());
        let plan = build_plan("hello", &desired, &[have]);
        match &plan.ops[0] {
            ServiceOp::Recreate { reasons, .. } => {
                assert!(reasons.iter().any(|r| r.contains("ports:")));
            }
            other => panic!("expected Recreate, got {other:?}"),
        }
    }

    #[test]
    fn plan_ops_sorted_alphabetically() {
        let mut desired = DesiredCompose::default();
        desired
            .services
            .insert("zeta".into(), svc("zeta", "alpine"));
        desired
            .services
            .insert("alpha".into(), svc("alpha", "alpine"));
        let plan = build_plan("hello", &desired, &[]);
        assert_eq!(plan.ops[0].service(), "alpha");
        assert_eq!(plan.ops[1].service(), "zeta");
    }
}
