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
//! Compose files are parsed in two formats:
//! - YAML (`.yml` / `.yaml`): docker-compose-compatible, `services:` wrapper.
//! - TOML (`.toml`): Isengard-native flat shape. Every top-level table IS
//!   a service; there is no `services:` wrapper. Top-level scalars are
//!   reserved for future metadata (version, description) and currently
//!   ignored. Top-level arrays-of-tables (`[[foo]]`) are rejected.
//!
//! The TOML path lifts every top-level table under a synthetic `services:`
//! map and re-runs the existing YAML parser, so env / ports / labels /
//! networks / secrets all use the same canonical decode.
//!
//! All YAML parsing uses `serde_yaml`. v0.3d still loses comments on
//! round-trip; the dashboard editor's banner calls this out explicitly.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bollard::secret::ContainerInspectResponse;
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

use crate::runtime::ContainerSnapshot;

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
    /// v0.3.6: per-service secret references. Each entry names a
    /// top-level `secrets:` key. The value's source (`external: true`)
    /// is resolved at apply time via the agent's FetchSecret RPC.
    /// Long-form `target:` overrides the default `/run/secrets/<name>`
    /// mount path.
    pub secrets: Vec<ServiceSecretRef>,
    /// Network names this service should join. Compose accepts both
    /// list and map forms; both produce the same flat list here. Used
    /// by `compose_apply` to attach the container to each network at
    /// create time. Empty = docker default bridge.
    pub networks: Vec<String>,
    /// Phase 0.17: per-service `cap_add:` list. Each entry is a Linux
    /// capability name (with or without the `CAP_` prefix, e.g.
    /// `CHOWN`, `CAP_NET_BIND_SERVICE`). The compose path forwards the
    /// raw strings unchanged; `compose_apply` joins them into the
    /// `isengard.cap.add` label that the WispBackend reader consumes
    /// to populate all five OCI capability sets. Empty = wisp's
    /// default `CAP_KILL` + `CAP_NET_BIND_SERVICE` allow-list applies.
    pub cap_add: Vec<String>,
}

/// One entry in a service's `secrets:` list.
///
/// Compose accepts both forms:
///   - Short:  `secrets: [foo, bar]`
///   - Long:   `secrets: [{source: foo, target: /custom/path, mode: 0400}]`
///
/// `source` is the top-level secret name; `target` is the in-container
/// mount path. We currently ignore `mode` and `uid/gid` fields: the
/// agent always writes the file with mode 0400 owned by root, matching
/// Docker Swarm's default semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSecretRef {
    pub source: String,
    pub target: Option<String>,
}

/// Parsed `compose.yaml`. Only the bits the reconciler needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DesiredCompose {
    pub services: BTreeMap<String, DesiredService>,
    /// v0.3.6: top-level `secrets:` map. Keys are secret names; the
    /// declared source determines how the agent resolves them.
    pub secrets: BTreeMap<String, TopLevelSecret>,
}

/// One top-level `secrets:` entry.
///
/// In v0.3.6 we only support `external: true` (resolved via the
/// controller's FetchSecret RPC). `file:`-source secrets used to be on
/// the roadmap but are explicitly NOT supported in v0.3.6: the parser
/// returns a clear error pointing operators at `isd secret put`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopLevelSecret {
    /// Resolves at apply time via the agent's FetchSecret RPC. Stored
    /// in the controller's encrypted secrets table.
    External,
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

impl DesiredCompose {
    /// v0.3.6: validate that every per-service `secrets:` reference
    /// points at a top-level entry declared `external: true`. Returns
    /// the union of names actually referenced across all services so
    /// the apply path can fetch them in one batch. Names that no
    /// service references are silently dropped: declaring a top-level
    /// secret without using it is a no-op.
    pub fn referenced_external_secrets(&self) -> anyhow::Result<Vec<String>> {
        let mut names: BTreeSet<String> = BTreeSet::new();
        for svc in self.services.values() {
            for r in &svc.secrets {
                match self.secrets.get(&r.source) {
                    Some(TopLevelSecret::External) => {
                        names.insert(r.source.clone());
                    }
                    None => {
                        return Err(anyhow::anyhow!(
                            "service {:?} references secret {:?} but no \
                             top-level `secrets.{}` entry exists",
                            svc.name,
                            r.source,
                            r.source,
                        ));
                    }
                }
            }
        }
        Ok(names.into_iter().collect())
    }

    /// Per-service helper: list `(secret_name, container_path)` pairs
    /// the agent should mount into `service`. Resolves long-form
    /// `target:` overrides; falls back to `/run/secrets/<name>` when
    /// the operator didn't specify one.
    pub fn service_secret_targets(&self, service: &str) -> anyhow::Result<Vec<(String, String)>> {
        let svc = self
            .services
            .get(service)
            .ok_or_else(|| anyhow::anyhow!("service {service:?} not in compose"))?;
        let mut out = Vec::with_capacity(svc.secrets.len());
        for r in &svc.secrets {
            if !matches!(self.secrets.get(&r.source), Some(TopLevelSecret::External)) {
                return Err(anyhow::anyhow!(
                    "service {service:?} references secret {:?} but no \
                     top-level `external: true` entry exists",
                    r.source,
                ));
            }
            let target = r
                .target
                .clone()
                .unwrap_or_else(|| format!("/run/secrets/{}", r.source));
            out.push((r.source.clone(), target));
        }
        Ok(out)
    }
}

/// Source format of a compose file. Detected from the on-disk extension
/// at read time; the wire/persistence representation that the agent
/// receives over the sync stream is always YAML today (the controller's
/// proto carries a `compose_yaml` field), so this enum is operator-side
/// scaffolding only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeFormat {
    /// `.yml` / `.yaml`: standard docker-compose, `services:` wrapper.
    Yaml,
    /// `.toml`: Isengard-native flat shape; every top-level table is a
    /// service.
    Toml,
}

/// Detect a compose file's format from its path's extension.
pub fn detect_format(path: &Path) -> anyhow::Result<ComposeFormat> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("yml") | Some("yaml") => Ok(ComposeFormat::Yaml),
        Some("toml") => Ok(ComposeFormat::Toml),
        Some(other) => Err(anyhow::anyhow!(
            "unrecognised compose extension {other:?} on {} (expected .yml, .yaml, or .toml)",
            path.display()
        )),
        None => Err(anyhow::anyhow!(
            "compose path {} has no extension; cannot detect format",
            path.display()
        )),
    }
}

/// Read a compose file from disk and parse it. Format is detected from
/// the file extension; see [`detect_format`].
pub fn parse_compose_path(path: &Path) -> anyhow::Result<DesiredCompose> {
    let format = detect_format(path)?;
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    parse_compose_str(&content, format)
}

/// Parse compose `content` as `format` into a [`DesiredCompose`].
///
/// For [`ComposeFormat::Yaml`] this is a straight passthrough to
/// [`parse_compose`]. For [`ComposeFormat::Toml`] the content is parsed
/// to a `toml::Value`, lifted into a synthetic `{services: <flat-map>}`
/// shape, re-emitted as YAML, and run back through [`parse_compose`] so
/// the canonical YAML path stays the only place that knows about env /
/// ports / labels / networks / secrets shapes.
pub fn parse_compose_str(content: &str, format: ComposeFormat) -> anyhow::Result<DesiredCompose> {
    match format {
        ComposeFormat::Yaml => parse_compose(content),
        ComposeFormat::Toml => {
            let toml_value: toml::Value =
                toml::from_str(content).map_err(|e| anyhow::anyhow!("parse compose.toml: {e}"))?;
            let yaml_equivalent = toml_to_compose_yaml(toml_value)?;
            parse_compose(&yaml_equivalent)
        }
    }
}

/// Lift a parsed flat-shape TOML compose into the YAML shape the
/// existing parser understands.
///
/// Rules:
/// - Every top-level table becomes an entry under `services:`.
/// - Top-level non-table values (scalars, arrays) are currently ignored.
///   They are reserved for future metadata (e.g. `version`, `description`).
/// - A top-level array-of-tables (`[[foo]]`) is rejected: it has no
///   sensible mapping into compose semantics.
fn toml_to_compose_yaml(value: toml::Value) -> anyhow::Result<String> {
    let toml::Value::Table(tbl) = value else {
        return Err(anyhow::anyhow!(
            "compose.toml root must be a table; got {value:?}"
        ));
    };
    let mut services_map = serde_json::Map::new();
    for (k, v) in tbl {
        match v {
            toml::Value::Table(_) => {
                services_map.insert(k, toml_value_to_json(v)?);
            }
            toml::Value::Array(ref arr)
                if arr.iter().all(|x| matches!(x, toml::Value::Table(_))) =>
            {
                // Array-of-tables (`[[k]]`). Compose has no sensible
                // mapping for this shape; reject loudly so an operator
                // doesn't think it silently became a service.
                return Err(anyhow::anyhow!(
                    "compose.toml top-level `[[{k}]]` array-of-tables is not supported; \
                     each service must be a single `[name]` table"
                ));
            }
            _other => {
                // Scalar or non-table-array. Reserved for future top-level
                // metadata; ignore for now.
                continue;
            }
        }
    }
    let mut top = serde_json::Map::new();
    top.insert("services".into(), serde_json::Value::Object(services_map));
    serde_yaml::to_string(&serde_json::Value::Object(top))
        .map_err(|e| anyhow::anyhow!("serialize toml-as-yaml: {e}"))
}

/// Recursively translate a `toml::Value` into a `serde_json::Value`.
///
/// `toml::Value::Datetime` has no native serde_json equivalent: we
/// project it as an RFC3339 string. compose has no datetime fields
/// today, so this is a defensive serialisation only.
fn toml_value_to_json(v: toml::Value) -> anyhow::Result<serde_json::Value> {
    use serde_json::Value as JV;
    Ok(match v {
        toml::Value::String(s) => JV::String(s),
        toml::Value::Integer(i) => JV::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(JV::Number)
            .unwrap_or(JV::Null),
        toml::Value::Boolean(b) => JV::Bool(b),
        toml::Value::Datetime(dt) => JV::String(dt.to_string()),
        toml::Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(toml_value_to_json(item)?);
            }
            JV::Array(out)
        }
        toml::Value::Table(tbl) => {
            let mut out = serde_json::Map::with_capacity(tbl.len());
            for (k, v) in tbl {
                out.insert(k, toml_value_to_json(v)?);
            }
            JV::Object(out)
        }
    })
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

    // v0.3.6: top-level `secrets:` block. Optional; declared entries are
    // resolved at apply time via the agent's FetchSecret RPC.
    if let Value::Mapping(root_map) = &root {
        if let Some(Value::Mapping(secrets_map)) = root_map.get(Value::String("secrets".into())) {
            for (k, v) in secrets_map {
                let Value::String(name) = k else { continue };
                let Value::Mapping(spec) = v else {
                    return Err(anyhow::anyhow!(
                        "top-level secret {name:?} must be a mapping"
                    ));
                };
                out.secrets
                    .insert(name.clone(), parse_top_level_secret(name, spec)?);
            }
        }
    }
    Ok(out)
}

fn parse_top_level_secret(name: &str, m: &Mapping) -> anyhow::Result<TopLevelSecret> {
    // file-source secrets are explicitly rejected: v0.3.6 only ships the
    // managed (`external: true`) path. Operators get a clear pointer at
    // `isd secret put` instead of a silent half-implementation.
    if m.contains_key(Value::String("file".into())) {
        return Err(anyhow::anyhow!(
            "secret {name:?}: file-source secrets are not supported in v0.4; \
             use `isd secret put {name}` to load the value, then mark the \
             secret `external: true` in compose.yaml",
        ));
    }
    let external = match m.get(Value::String("external".into())) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Mapping(_)) => true, // `external: { name: foo }` form
        Some(Value::Null) | None => false,
        Some(other) => {
            return Err(anyhow::anyhow!(
                "secret {name:?}: `external:` must be a bool, got {other:?}"
            ));
        }
    };
    if !external {
        return Err(anyhow::anyhow!(
            "secret {name:?}: must declare `external: true` (the only \
             supported source in v0.4); use `isd secret put` to load values",
        ));
    }
    Ok(TopLevelSecret::External)
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
    // Compose spec allows both shapes for `labels:`:
    //   labels:                                # dict form
    //     foo.bar: baz
    //   labels:                                # list form
    //     - "foo.bar=baz"
    // Real-world compose files in the wild (and the homelab's
    // pre-Isengard Swarm/Traefik shape) lean on the list form. Silently
    // dropping it was the bug behind 'isd route list shows nothing
    // after deploy' — the labels never made it onto the running
    // containers, so label-ingest had nothing to discover.
    if let Some(value) = m.get(Value::String("labels".into())) {
        match value {
            Value::Mapping(labels) => {
                for (k, v) in labels {
                    let (Value::String(key), Value::String(val)) = (k, v) else {
                        continue;
                    };
                    svc.labels.insert(key.clone(), val.clone());
                }
            }
            Value::Sequence(items) => {
                for item in items {
                    let Value::String(s) = item else {
                        continue;
                    };
                    if let Some((key, val)) = s.split_once('=') {
                        svc.labels
                            .insert(key.trim().to_string(), val.trim().to_string());
                    } else {
                        // Bare "key" with no `=` is valid compose syntax for a
                        // label with empty value. docker-compose handles it the
                        // same way; mirror that.
                        svc.labels.insert(s.trim().to_string(), String::new());
                    }
                }
            }
            _ => {}
        }
    }
    // Compose `networks:` per service. Two valid shapes:
    //   networks: [foo, bar]                   # list of names
    //   networks:                              # map (per-network opts
    //     foo:                                 #   like aliases / ipv4)
    //       aliases: [...]
    //     bar: null
    // We flatten both into a Vec<String> of network names. Per-network
    // options (aliases, ipv4_address, link_local_ips) aren't applied
    // yet; compose_apply attaches the container to each named network
    // with default endpoint settings. Common homelab use ('this stack
    // talks to its own bridge + is exposed via isengard-proxy') doesn't
    // need the per-network options; richer service-discovery (aliases)
    // can land later without breaking the field.
    if let Some(value) = m.get(Value::String("networks".into())) {
        match value {
            Value::Sequence(items) => {
                for item in items {
                    if let Value::String(s) = item {
                        svc.networks.push(s.clone());
                    }
                }
            }
            Value::Mapping(map) => {
                for (k, _v) in map {
                    if let Value::String(name) = k {
                        svc.networks.push(name.clone());
                    }
                }
            }
            _ => {}
        }
    }
    // Phase 0.17: per-service `cap_add:` list. Compose accepts the
    // list form (`cap_add: [CHOWN, SETUID]` or `cap_add: ["CAP_CHOWN"]`).
    // We forward each entry as-given; `bundle::parse_caps` downstream
    // accepts both `CHOWN` and `CAP_CHOWN` forms, so no normalisation
    // is needed here. Non-string entries are skipped quietly: that
    // mirrors the parser's stance on every other list-shaped field
    // (ports, environment, networks).
    if let Some(Value::Sequence(seq)) = m.get(Value::String("cap_add".into())) {
        for entry in seq {
            if let Value::String(s) = entry {
                svc.cap_add.push(s.clone());
            }
        }
    }
    // v0.3.6: per-service `secrets:` list. Both short and long form
    // resolve to a `ServiceSecretRef` with `source` plus optional
    // `target`. The actual fetch + tmpfs mount happens in compose_apply
    // once the agent has all the per-service secrets in hand.
    if let Some(Value::Sequence(seq)) = m.get(Value::String("secrets".into())) {
        for entry in seq {
            match entry {
                Value::String(s) => svc.secrets.push(ServiceSecretRef {
                    source: s.clone(),
                    target: None,
                }),
                Value::Mapping(em) => {
                    let source = match em.get(Value::String("source".into())) {
                        Some(Value::String(s)) => s.clone(),
                        _ => {
                            return Err(anyhow::anyhow!(
                                "service {name:?}: secret entry missing `source:` (or it isn't a string)"
                            ));
                        }
                    };
                    let target = match em.get(Value::String("target".into())) {
                        Some(Value::String(s)) => Some(s.clone()),
                        Some(Value::Null) | None => None,
                        Some(other) => {
                            return Err(anyhow::anyhow!(
                                "service {name:?} secret {source:?}: `target:` must be a string, got {other:?}"
                            ));
                        }
                    };
                    svc.secrets.push(ServiceSecretRef { source, target });
                }
                other => {
                    return Err(anyhow::anyhow!(
                        "service {name:?}: secrets list entries must be a name or mapping, got {other:?}"
                    ));
                }
            }
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
    /// Phase 0.6: build a [`RunningService`] from a backend-agnostic
    /// [`ContainerSnapshot`]. The snapshot already filters managed env
    /// keys and projects ports / restart strings, so this is a flat
    /// field copy.
    pub fn from_snapshot(snapshot: &ContainerSnapshot) -> Option<Self> {
        // Service name precedence matches the legacy bollard mapper:
        // compose label first, then the agent's `isengard.service`
        // override.
        let svc = snapshot
            .labels
            .get("com.docker.compose.service")
            .or_else(|| snapshot.labels.get("isengard.service"))
            .cloned()?;
        // Drop compose-internal labels from the diff surface; they are
        // injected by the agent and otherwise look like operator drift.
        let labels_map: BTreeMap<String, String> = snapshot
            .labels
            .iter()
            .filter(|(k, _)| !k.starts_with("com.docker.compose."))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Some(Self {
            service_name: svc,
            container_id: snapshot.id.clone(),
            image: snapshot.image.clone(),
            environment: snapshot.env.clone(),
            labels: labels_map,
            port_bindings: snapshot.port_bindings.clone(),
            restart: snapshot.restart.clone(),
        })
    }

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
    fn parse_compose_with_labels_dict_form() {
        let yaml = r#"services:
  web:
    image: nginx
    labels:
      isengard.expose: foo.example.com
      isengard.expose.port: "8080"
"#;
        let parsed = parse_compose(yaml).unwrap();
        let labels = &parsed.services["web"].labels;
        assert_eq!(labels["isengard.expose"], "foo.example.com");
        assert_eq!(labels["isengard.expose.port"], "8080");
    }

    #[test]
    fn parse_compose_with_labels_list_form() {
        // Real-world compose files (and the homelab repo's pre-Isengard
        // shape) lean on the list form. Regression test for the parser
        // silently dropping these.
        let yaml = r#"services:
  web:
    image: nginx
    labels:
      - "isengard.expose=foo.example.com"
      - "isengard.expose.port=8080"
      - bare-key
"#;
        let parsed = parse_compose(yaml).unwrap();
        let labels = &parsed.services["web"].labels;
        assert_eq!(labels["isengard.expose"], "foo.example.com");
        assert_eq!(labels["isengard.expose.port"], "8080");
        assert_eq!(labels["bare-key"], "");
    }

    #[test]
    fn parse_compose_with_networks_list_form() {
        // The homelab `servarr` stack declares `networks: [servarr,
        // isengard-proxy]` per service. Without this parser, the
        // reconciler silently dropped them and the agent attached every
        // container to docker0 only, breaking proxy reachability.
        let yaml = r#"services:
  web:
    image: nginx
    networks:
      - servarr
      - isengard-proxy
"#;
        let parsed = parse_compose(yaml).unwrap();
        let nets = &parsed.services["web"].networks;
        assert_eq!(
            nets,
            &vec!["servarr".to_string(), "isengard-proxy".to_string()]
        );
    }

    #[test]
    fn parse_compose_with_networks_map_form() {
        // Map form is standard compose for per-network options
        // (aliases, ipv4_address). We don't apply those yet; the test
        // just locks in that the network names round-trip.
        let yaml = r#"services:
  web:
    image: nginx
    networks:
      servarr:
        aliases: [api]
      isengard-proxy: null
"#;
        let parsed = parse_compose(yaml).unwrap();
        let nets = &parsed.services["web"].networks;
        assert!(nets.contains(&"servarr".to_string()));
        assert!(nets.contains(&"isengard-proxy".to_string()));
    }

    #[test]
    fn parse_compose_with_cap_add_bare_form() {
        // Phase 0.17: compose `cap_add:` list using the bare form
        // (no `CAP_` prefix). nginx:alpine needs this exact set to
        // bootstrap the master + worker handover.
        let yaml = r#"services:
  web:
    image: nginx:alpine
    cap_add:
      - CHOWN
      - SETUID
      - SETGID
      - DAC_OVERRIDE
      - FOWNER
      - SETPCAP
"#;
        let parsed = parse_compose(yaml).unwrap();
        let caps = &parsed.services["web"].cap_add;
        assert_eq!(
            caps,
            &vec![
                "CHOWN".to_string(),
                "SETUID".to_string(),
                "SETGID".to_string(),
                "DAC_OVERRIDE".to_string(),
                "FOWNER".to_string(),
                "SETPCAP".to_string(),
            ]
        );
    }

    #[test]
    fn parse_compose_with_cap_add_prefixed_form() {
        // The downstream `bundle::parse_caps` accepts both `CHOWN` and
        // `CAP_CHOWN`. The parser stores them unchanged.
        let yaml = r#"services:
  web:
    image: nginx
    cap_add:
      - CAP_CHOWN
      - CAP_SETUID
"#;
        let parsed = parse_compose(yaml).unwrap();
        let caps = &parsed.services["web"].cap_add;
        assert_eq!(
            caps,
            &vec!["CAP_CHOWN".to_string(), "CAP_SETUID".to_string()]
        );
    }

    #[test]
    fn parse_compose_omits_cap_add_when_absent() {
        let yaml = r#"services:
  web:
    image: nginx
"#;
        let parsed = parse_compose(yaml).unwrap();
        assert!(parsed.services["web"].cap_add.is_empty());
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

    // ----- TOML flat-shape -----

    #[test]
    fn parse_toml_flat_shape_with_one_service() {
        // Flat-shape: top-level `[web]` table IS a service. No `services:`
        // wrapper. Mirrors the basic YAML smoke shape (image, env, ports,
        // labels) so the canonical decode path is exercised end-to-end.
        let toml_str = r#"
[web]
image = "nginx:1.27"
ports = ["8080:80"]

[web.environment]
TZ = "Europe/Zurich"

[web.labels]
"isengard.expose" = "foo.example.com"
"#;
        let parsed = parse_compose_str(toml_str, ComposeFormat::Toml).unwrap();
        assert_eq!(parsed.services.len(), 1);
        let web = &parsed.services["web"];
        assert_eq!(web.image.as_deref(), Some("nginx:1.27"));
        assert_eq!(web.ports, vec!["8080:80".to_string()]);
        assert_eq!(web.environment["TZ"], "Europe/Zurich");
        assert_eq!(web.labels["isengard.expose"], "foo.example.com");
    }

    #[test]
    fn parse_toml_with_multiple_services() {
        let toml_str = r#"
[web]
image = "nginx"
networks = ["frontend"]

[api]
image = "alpine"
networks = ["frontend", "backend"]
"#;
        let parsed = parse_compose_str(toml_str, ComposeFormat::Toml).unwrap();
        assert_eq!(parsed.services.len(), 2);
        assert!(
            parsed.services["web"]
                .networks
                .contains(&"frontend".to_string())
        );
        assert_eq!(parsed.services["api"].networks.len(), 2);
    }

    #[test]
    fn parse_toml_top_level_scalars_are_ignored_metadata() {
        // Top-level scalars (version, description, etc.) are reserved
        // for future metadata. Today they're ignored without error: the
        // services list still parses cleanly.
        let toml_str = r#"
version = "1"
description = "smoke"

[web]
image = "nginx"
"#;
        let parsed = parse_compose_str(toml_str, ComposeFormat::Toml).unwrap();
        assert_eq!(parsed.services.len(), 1);
        assert_eq!(parsed.services["web"].image.as_deref(), Some("nginx"));
    }

    #[test]
    fn parse_toml_rejects_top_level_array_of_tables() {
        // `[[foo]]` is array-of-tables in TOML; there's no sensible
        // mapping into compose. Reject loudly so an operator doesn't
        // think it silently became a service.
        let toml_str = r#"
[[svc]]
image = "nginx"

[[svc]]
image = "alpine"
"#;
        let err = parse_compose_str(toml_str, ComposeFormat::Toml).unwrap_err();
        assert!(format!("{err}").contains("array-of-tables"));
    }

    #[test]
    fn parse_toml_with_native_placement_verbs_passes_through() {
        // Phase 0.9 marker: native placement verbs (`spread`, `global`,
        // `on`, `where`) aren't yet wired into the reconciler. They
        // should at least parse cleanly without rejection so an
        // operator can author them today; the apply path picks them up
        // when later phases land. TODO(phase 1.x): surface these on
        // DesiredService.
        let toml_str = r#"
[web]
image = "nginx"
spread = 2
global = true
on = "lausanne"
where = "node.role==worker"
"#;
        let parsed = parse_compose_str(toml_str, ComposeFormat::Toml).unwrap();
        assert_eq!(parsed.services.len(), 1);
        assert_eq!(parsed.services["web"].image.as_deref(), Some("nginx"));
    }

    #[test]
    fn parse_compose_path_dispatches_by_extension() {
        let dir = tempfile::tempdir().unwrap();

        let yaml_path = dir.path().join("compose.yaml");
        std::fs::write(&yaml_path, "services:\n  web:\n    image: nginx\n").unwrap();
        let parsed = parse_compose_path(&yaml_path).unwrap();
        assert_eq!(parsed.services["web"].image.as_deref(), Some("nginx"));

        let toml_path = dir.path().join("compose.toml");
        std::fs::write(&toml_path, "[web]\nimage = \"alpine\"\n").unwrap();
        let parsed = parse_compose_path(&toml_path).unwrap();
        assert_eq!(parsed.services["web"].image.as_deref(), Some("alpine"));

        let unknown_path = dir.path().join("compose.json");
        std::fs::write(&unknown_path, "{}").unwrap();
        assert!(parse_compose_path(&unknown_path).is_err());
    }

    #[test]
    fn detect_format_reads_extension() {
        assert_eq!(
            detect_format(Path::new("/etc/isengard/stacks/hello/compose.yaml")).unwrap(),
            ComposeFormat::Yaml
        );
        assert_eq!(
            detect_format(Path::new("compose.yml")).unwrap(),
            ComposeFormat::Yaml
        );
        assert_eq!(
            detect_format(Path::new("compose.toml")).unwrap(),
            ComposeFormat::Toml
        );
        assert!(detect_format(Path::new("compose.json")).is_err());
        assert!(detect_format(Path::new("compose")).is_err());
    }

    #[test]
    fn parse_toml_invalid_input_errors() {
        let err = parse_compose_str("not = = valid", ComposeFormat::Toml).unwrap_err();
        assert!(format!("{err}").contains("compose.toml"));
    }

    #[test]
    fn parse_toml_and_yaml_produce_equivalent_desired_compose() {
        // Same logical compose expressed in both formats should
        // round-trip into the same DesiredCompose. This is the smoke
        // assertion that the TOML lift-to-YAML path doesn't lose
        // information versus the canonical YAML path. (The Phase 0.9
        // wisp e2e test runs the YAML form; this test proves the TOML
        // form lands on the identical structural result.)
        let yaml = r#"services:
  hello:
    image: docker.io/library/busybox:latest
    container_name: hello-toml
    command: ["/bin/sh", "-c", "sleep 60"]
    labels:
      isengard.expose: e2e.wisp.local
    networks:
      - frontend
"#;
        let toml_str = r#"
[hello]
image = "docker.io/library/busybox:latest"
container_name = "hello-toml"
command = ["/bin/sh", "-c", "sleep 60"]
networks = ["frontend"]

[hello.labels]
"isengard.expose" = "e2e.wisp.local"
"#;
        let yaml_parsed = parse_compose_str(yaml, ComposeFormat::Yaml).unwrap();
        let toml_parsed = parse_compose_str(toml_str, ComposeFormat::Toml).unwrap();
        assert_eq!(yaml_parsed, toml_parsed);
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

    // ----- v0.3.6 secrets parsing -----

    #[test]
    fn parse_compose_top_level_external_secret() {
        let yaml =
            "services:\n  web:\n    image: nginx\nsecrets:\n  cf_token:\n    external: true\n";
        let parsed = parse_compose(yaml).unwrap();
        assert_eq!(
            parsed.secrets.get("cf_token"),
            Some(&TopLevelSecret::External)
        );
    }

    #[test]
    fn parse_compose_short_form_per_service_secrets() {
        let yaml = "services:\n  web:\n    image: nginx\n    secrets:\n      - cf_token\n      - db_pass\nsecrets:\n  cf_token:\n    external: true\n  db_pass:\n    external: true\n";
        let parsed = parse_compose(yaml).unwrap();
        let web = &parsed.services["web"];
        assert_eq!(web.secrets.len(), 2);
        assert_eq!(web.secrets[0].source, "cf_token");
        assert_eq!(web.secrets[0].target, None);
        assert_eq!(web.secrets[1].source, "db_pass");
    }

    #[test]
    fn parse_compose_long_form_per_service_secrets() {
        let yaml = "services:\n  web:\n    image: nginx\n    secrets:\n      - source: cf_token\n        target: /etc/secrets/cf.txt\n        mode: 0400\nsecrets:\n  cf_token:\n    external: true\n";
        let parsed = parse_compose(yaml).unwrap();
        let web = &parsed.services["web"];
        assert_eq!(web.secrets[0].source, "cf_token");
        assert_eq!(
            web.secrets[0].target.as_deref(),
            Some("/etc/secrets/cf.txt")
        );
    }

    #[test]
    fn parse_compose_file_source_secret_errors() {
        let yaml = "services:\n  web:\n    image: nginx\nsecrets:\n  cf_token:\n    file: /var/secrets/cf.txt\n";
        let err = parse_compose(yaml).unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("file-source secrets are not supported"));
        assert!(s.contains("isd secret put"));
    }

    #[test]
    fn parse_compose_secret_without_external_errors() {
        let yaml = "services:\n  web:\n    image: nginx\nsecrets:\n  cf_token: {}\n";
        let err = parse_compose(yaml).unwrap_err();
        assert!(format!("{err}").contains("must declare `external: true`"));
    }

    #[test]
    fn referenced_external_secrets_returns_used_names_only() {
        let yaml = "services:\n  web:\n    image: nginx\n    secrets: [cf_token]\nsecrets:\n  cf_token:\n    external: true\n  unused_one:\n    external: true\n";
        let parsed = parse_compose(yaml).unwrap();
        let names = parsed.referenced_external_secrets().unwrap();
        assert_eq!(names, vec!["cf_token".to_string()]);
    }

    #[test]
    fn referenced_external_secrets_errors_on_dangling_ref() {
        let yaml = "services:\n  web:\n    image: nginx\n    secrets: [cf_token]\n";
        let parsed = parse_compose(yaml).unwrap();
        let err = parsed.referenced_external_secrets().unwrap_err();
        assert!(format!("{err}").contains("no top-level"));
    }

    #[test]
    fn service_secret_targets_uses_default_run_secrets() {
        let yaml = "services:\n  web:\n    image: nginx\n    secrets: [cf_token]\nsecrets:\n  cf_token:\n    external: true\n";
        let parsed = parse_compose(yaml).unwrap();
        let targets = parsed.service_secret_targets("web").unwrap();
        assert_eq!(
            targets,
            vec![("cf_token".to_string(), "/run/secrets/cf_token".to_string())]
        );
    }

    #[test]
    fn service_secret_targets_honours_long_form_target() {
        let yaml = "services:\n  web:\n    image: nginx\n    secrets:\n      - source: cf_token\n        target: /etc/cf.txt\nsecrets:\n  cf_token:\n    external: true\n";
        let parsed = parse_compose(yaml).unwrap();
        let targets = parsed.service_secret_targets("web").unwrap();
        assert_eq!(
            targets,
            vec![("cf_token".to_string(), "/etc/cf.txt".to_string())]
        );
    }

    // ----- existing tests below -----

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

    // ----- Phase 0.6: RunningService::from_snapshot -----

    fn snap_with(
        id: &str,
        image: &str,
        labels: &[(&str, &str)],
        env: &[(&str, &str)],
        ports: &[&str],
        restart: Option<&str>,
    ) -> crate::runtime::ContainerSnapshot {
        crate::runtime::ContainerSnapshot {
            id: id.into(),
            name: id.into(),
            image: image.into(),
            state: crate::runtime::ContainerState::Running,
            stack: None,
            service: None,
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            started_at: None,
            finished_at: None,
            exit_code: None,
            restart_count: 0,
            network_settings: crate::runtime::NetworkSettings::default(),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            port_bindings: ports.iter().map(|p| (*p).to_string()).collect(),
            restart: restart.map(str::to_string),
        }
    }

    #[test]
    fn from_snapshot_extracts_service_name_and_image() {
        let snap = snap_with(
            "abc",
            "nginx:1.27",
            &[("com.docker.compose.service", "web")],
            &[],
            &[],
            None,
        );
        let rs = RunningService::from_snapshot(&snap).expect("Some");
        assert_eq!(rs.service_name, "web");
        assert_eq!(rs.image, "nginx:1.27");
        assert_eq!(rs.container_id, "abc");
    }

    #[test]
    fn from_snapshot_drops_compose_internal_labels() {
        let snap = snap_with(
            "abc",
            "nginx",
            &[
                ("com.docker.compose.service", "web"),
                ("com.docker.compose.project", "hello"),
                ("traefik.enable", "true"),
            ],
            &[],
            &[],
            None,
        );
        let rs = RunningService::from_snapshot(&snap).expect("Some");
        assert_eq!(
            rs.labels.get("traefik.enable").map(String::as_str),
            Some("true")
        );
        assert!(!rs.labels.contains_key("com.docker.compose.service"));
        assert!(!rs.labels.contains_key("com.docker.compose.project"));
    }

    #[test]
    fn from_snapshot_returns_none_without_service_label() {
        let snap = snap_with("abc", "nginx", &[], &[], &[], None);
        assert!(RunningService::from_snapshot(&snap).is_none());
    }

    #[test]
    fn from_snapshot_falls_back_to_isengard_service_label() {
        let snap = snap_with(
            "abc",
            "nginx",
            &[("isengard.service", "web")],
            &[],
            &[],
            None,
        );
        let rs = RunningService::from_snapshot(&snap).expect("Some");
        assert_eq!(rs.service_name, "web");
    }

    #[test]
    fn from_snapshot_carries_env_ports_restart() {
        let snap = snap_with(
            "abc",
            "nginx",
            &[("com.docker.compose.service", "web")],
            &[("TZ", "UTC")],
            &["8080:80"],
            Some("always"),
        );
        let rs = RunningService::from_snapshot(&snap).expect("Some");
        assert_eq!(rs.environment.get("TZ").map(String::as_str), Some("UTC"));
        assert_eq!(rs.port_bindings, vec!["8080:80".to_string()]);
        assert_eq!(rs.restart.as_deref(), Some("always"));
    }
}
