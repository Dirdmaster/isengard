//! Synthesizes a `compose.yaml` from the rich container snapshots the
//! agent reports back to the controller.
//!
//! Pure function module. No IO, no DB, no HTTP. The caller (a dashboard
//! endpoint added in a follow-up PR) loads rich container records out of
//! storage, hands them to [`synthesize`], and returns the resulting
//! string with an `X-Compose-Source: synthesized` header.
//!
//! The local [`ContainerRich`] struct is the contract between this
//! synthesizer and the parallel "agent + storage widening" PR that will
//! land alongside it. That PR adapts to the field set defined here; the
//! synthesizer never depends on storage types directly.
//!
//! Semantics intentionally lossy:
//!
//! - Multi-replica services collapse to a single compose entry built
//!   from the first container's spec. Compose v3 has no per-replica
//!   override, so any divergence between replicas is dropped on the
//!   floor.
//! - `build:`, build args, original `secrets:` / `configs:` intent, and
//!   compose interpolation are not recoverable. The generated YAML is a
//!   starting point for operator review, not a lossless export.
//!
//! Output is annotated with a header comment so the operator immediately
//! sees that this YAML was generated rather than written by hand.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::Utc;
use serde_yaml::Value;

/// Rich snapshot of one running container.
///
/// The contract the agent's heartbeat will fill in once the parallel
/// widening PR lands. Keep field shapes here authoritative; storage and
/// the agent both adapt to this type.
#[derive(Debug, Clone)]
pub struct ContainerRich {
    /// Compose service name. Sourced from
    /// `com.docker.compose.service` label on the container, falling
    /// back to the container name when the label is absent (containers
    /// not started through compose still synthesize to one-service
    /// entries).
    pub service: String,
    /// Compose project (stack) name. Sourced from
    /// `com.docker.compose.project` label. Synthesis groups by service
    /// inside one stack; the caller is responsible for filtering
    /// containers down to a single stack before calling [`synthesize`].
    pub stack: String,
    /// Image reference (e.g. `nginx:1.27`, `ghcr.io/owner/img@sha256:...`).
    pub image: String,
    /// Port mappings the container publishes.
    pub ports: Vec<PortMapping>,
    /// Environment variables in `KEY=value` form. Order preserved from
    /// the container inspect; duplicates are not deduplicated (compose
    /// itself tolerates them).
    pub env: Vec<String>,
    /// Mount entries (bind, named volume, or tmpfs).
    pub mounts: Vec<MountSpec>,
    /// Network names the container is attached to. The literal
    /// `bridge`, `host`, and `none` Docker built-ins are emitted into
    /// `services.<svc>.networks` but skipped from the top-level
    /// `networks:` map; everything else is treated as a user network
    /// and registered top-level.
    pub networks: Vec<String>,
    /// Restart policy string (`no`, `always`, `unless-stopped`,
    /// `on-failure`, `on-failure:N`). `None` means "no restart key in
    /// the synthesized service" (compose default).
    pub restart_policy: Option<String>,
    /// `command:` array override. `None` keeps the image default.
    pub command: Option<Vec<String>>,
    /// `entrypoint:` array override. `None` keeps the image default.
    pub entrypoint: Option<Vec<String>>,
    /// `working_dir:` override. `None` keeps the image default.
    pub working_dir: Option<String>,
    /// `user:` override (e.g. `1000:1000`). `None` keeps the image default.
    pub user: Option<String>,
    /// Container healthcheck spec. `None` means no healthcheck key is
    /// emitted (the image's own HEALTHCHECK directive still applies on
    /// the live container; synthesis only emits what the operator can
    /// recover in YAML).
    pub healthcheck: Option<HealthcheckSpec>,
    /// All container labels, including the `com.docker.compose.*` ones.
    /// Surfaced in the synthesized service so the operator can see
    /// what the live container actually carries.
    pub labels: HashMap<String, String>,
}

/// Port publish entry.
#[derive(Debug, Clone)]
pub struct PortMapping {
    /// Host port the container is published on. `None` means
    /// expose-only (compose `expose:` semantics); the synthesizer
    /// emits a long-form entry with no `published:` key.
    pub host_port: Option<u16>,
    /// Container port the host port maps to.
    pub container_port: u16,
    /// `tcp` or `udp`. Synthesis emits it verbatim; the validator on
    /// the consuming side rejects anything else.
    pub protocol: String,
}

/// Container mount entry.
///
/// Mirrors the three flavours docker tracks: bind mount (host path),
/// named volume (managed by docker), and tmpfs.
#[derive(Debug, Clone)]
pub enum MountSpec {
    /// Host path bind-mounted into the container.
    Bind {
        /// Absolute or relative host path. Synthesis emits the value
        /// verbatim; relative paths are interpreted by compose at deploy
        /// time against the file's own directory.
        source: String,
        /// In-container mount path.
        target: String,
        /// Whether the mount is read-only.
        read_only: bool,
    },
    /// Docker-managed named volume.
    Volume {
        /// Optional volume name. `None` means an anonymous volume;
        /// the synthesizer emits the bare target in that case and
        /// skips registering anything in the top-level `volumes:` map.
        name: Option<String>,
        /// In-container mount path.
        target: String,
        /// Whether the mount is read-only.
        read_only: bool,
    },
    /// In-memory `tmpfs` mount.
    Tmpfs {
        /// In-container mount path.
        target: String,
        /// Optional size limit in bytes (emitted as the long-form
        /// `tmpfs.size` field when present).
        size_bytes: Option<u64>,
    },
}

/// Container healthcheck.
///
/// Field semantics match the compose healthcheck schema: `test` is the
/// verbatim CMD array (e.g. `["CMD", "curl", "-f", "http://localhost/"]`
/// or `["CMD-SHELL", "stat /tmp/ok"]`), the duration fields are emitted
/// as `<n>s` strings, and `retries` is a bare integer.
#[derive(Debug, Clone)]
pub struct HealthcheckSpec {
    /// Healthcheck test array. First element is one of `CMD`,
    /// `CMD-SHELL`, or `NONE`; the rest are arguments.
    pub test: Vec<String>,
    /// Optional interval between checks, in seconds.
    pub interval_secs: Option<u64>,
    /// Optional per-check timeout, in seconds.
    pub timeout_secs: Option<u64>,
    /// Optional number of consecutive failures before unhealthy.
    pub retries: Option<u32>,
    /// Optional grace period at container start, in seconds.
    pub start_period_secs: Option<u64>,
}

/// Built-in Docker networks that exist on every host. We emit them in
/// a service's `networks:` list but skip registering them top-level,
/// matching how a hand-written compose file is conventionally shaped.
const BUILT_IN_NETWORKS: &[&str] = &["bridge", "host", "none"];

/// Builds a compose YAML string from a slice of rich container records.
///
/// The caller should pre-filter `containers` to a single stack; the
/// `stack` field on each record is informational only (the synthesizer
/// does not branch on it). Multiple containers sharing the same
/// `service` value collapse to one compose entry; the first such record
/// wins as the template.
///
/// Output ordering is deterministic: services, named volumes, and
/// non-default networks are sorted alphabetically. Field order inside
/// each service follows the conventional compose ordering (image,
/// command, entrypoint, working_dir, user, environment, ports,
/// volumes, networks, restart, healthcheck, labels) rather than
/// alphabetical, so the output reads like something a human wrote.
///
/// The returned string starts with a multi-line header comment that
/// flags the YAML as synthesized and points the operator at
/// `isd stack deploy` for taking ownership.
#[must_use]
pub fn synthesize(containers: &[ContainerRich]) -> String {
    // Group containers by service, preserving the first occurrence as
    // the template. BTreeMap keeps service names sorted in the output.
    let mut services: BTreeMap<String, &ContainerRich> = BTreeMap::new();
    for c in containers {
        services.entry(c.service.clone()).or_insert(c);
    }

    // Two top-level registries: named volumes referenced by any
    // service, and non-default networks. Both BTreeSet for sort order.
    let mut named_volumes: BTreeSet<String> = BTreeSet::new();
    let mut user_networks: BTreeSet<String> = BTreeSet::new();

    let mut services_map = serde_yaml::Mapping::new();
    for (name, template) in &services {
        let svc_value = build_service(template, &mut named_volumes, &mut user_networks);
        services_map.insert(Value::String(name.clone()), svc_value);
    }

    let mut root = serde_yaml::Mapping::new();
    root.insert(
        Value::String("services".to_string()),
        Value::Mapping(services_map),
    );

    if !named_volumes.is_empty() {
        let mut vmap = serde_yaml::Mapping::new();
        for v in &named_volumes {
            vmap.insert(Value::String(v.clone()), Value::Mapping(Default::default()));
        }
        root.insert(Value::String("volumes".to_string()), Value::Mapping(vmap));
    }

    if !user_networks.is_empty() {
        let mut nmap = serde_yaml::Mapping::new();
        for n in &user_networks {
            nmap.insert(Value::String(n.clone()), Value::Mapping(Default::default()));
        }
        root.insert(Value::String("networks".to_string()), Value::Mapping(nmap));
    }

    let body = serde_yaml::to_string(&Value::Mapping(root))
        .expect("serde_yaml cannot fail on a hand-built Mapping with String keys");
    let header = header_comment(Utc::now());
    format!("{header}{body}")
}

/// Builds the per-service mapping for `template`.
///
/// Side-effect: registers any named volumes in `named_volumes` and any
/// non-builtin networks in `user_networks` so the caller can emit the
/// top-level `volumes:` and `networks:` registries.
fn build_service(
    template: &ContainerRich,
    named_volumes: &mut BTreeSet<String>,
    user_networks: &mut BTreeSet<String>,
) -> Value {
    let mut svc = serde_yaml::Mapping::new();

    svc.insert(
        Value::String("image".into()),
        Value::String(template.image.clone()),
    );

    if let Some(cmd) = &template.command {
        svc.insert(Value::String("command".into()), string_seq(cmd));
    }
    if let Some(ep) = &template.entrypoint {
        svc.insert(Value::String("entrypoint".into()), string_seq(ep));
    }
    if let Some(wd) = &template.working_dir {
        svc.insert(
            Value::String("working_dir".into()),
            Value::String(wd.clone()),
        );
    }
    if let Some(u) = &template.user {
        svc.insert(Value::String("user".into()), Value::String(u.clone()));
    }

    if !template.env.is_empty() {
        svc.insert(
            Value::String("environment".into()),
            string_seq(&template.env),
        );
    }

    if !template.ports.is_empty() {
        svc.insert(
            Value::String("ports".into()),
            Value::Sequence(template.ports.iter().map(port_value).collect()),
        );
    }

    if !template.mounts.is_empty() {
        svc.insert(
            Value::String("volumes".into()),
            Value::Sequence(
                template
                    .mounts
                    .iter()
                    .map(|m| mount_value(m, named_volumes))
                    .collect(),
            ),
        );
    }

    if !template.networks.is_empty() {
        let mut net_seq: Vec<Value> = Vec::with_capacity(template.networks.len());
        for n in &template.networks {
            net_seq.push(Value::String(n.clone()));
            if !BUILT_IN_NETWORKS.contains(&n.as_str()) {
                user_networks.insert(n.clone());
            }
        }
        svc.insert(Value::String("networks".into()), Value::Sequence(net_seq));
    }

    if let Some(rp) = &template.restart_policy {
        svc.insert(Value::String("restart".into()), Value::String(rp.clone()));
    }

    if let Some(hc) = &template.healthcheck {
        svc.insert(Value::String("healthcheck".into()), healthcheck_value(hc));
    }

    if !template.labels.is_empty() {
        // Labels: emit as a mapping (compose accepts both list and map
        // form; map is the more readable shape for synthesized output).
        // Sort by key for determinism since the source HashMap is
        // iteration-unstable.
        let mut sorted: Vec<(&String, &String)> = template.labels.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let mut lmap = serde_yaml::Mapping::new();
        for (k, v) in sorted {
            lmap.insert(Value::String(k.clone()), Value::String(v.clone()));
        }
        svc.insert(Value::String("labels".into()), Value::Mapping(lmap));
    }

    Value::Mapping(svc)
}

/// Long-form port entry. We always emit the long form so the consumer
/// doesn't have to disambiguate `"8080:80"` (host:container) from
/// `"8080"` (container-only exposure) at parse time.
fn port_value(p: &PortMapping) -> Value {
    let mut m = serde_yaml::Mapping::new();
    m.insert(
        Value::String("target".into()),
        Value::Number(serde_yaml::Number::from(u64::from(p.container_port))),
    );
    if let Some(hp) = p.host_port {
        m.insert(
            Value::String("published".into()),
            Value::Number(serde_yaml::Number::from(u64::from(hp))),
        );
    }
    m.insert(
        Value::String("protocol".into()),
        Value::String(p.protocol.clone()),
    );
    Value::Mapping(m)
}

/// Mount entry. Bind and named-volume mounts use the short
/// `source:target[:ro]` string form (the conventional shape in
/// hand-written compose); tmpfs falls back to a long-form mapping
/// because the short form can't express size.
fn mount_value(m: &MountSpec, named_volumes: &mut BTreeSet<String>) -> Value {
    match m {
        MountSpec::Bind {
            source,
            target,
            read_only,
        } => {
            let suffix = if *read_only { ":ro" } else { "" };
            Value::String(format!("{source}:{target}{suffix}"))
        }
        MountSpec::Volume {
            name,
            target,
            read_only,
        } => {
            let suffix = if *read_only { ":ro" } else { "" };
            match name {
                Some(n) => {
                    named_volumes.insert(n.clone());
                    Value::String(format!("{n}:{target}{suffix}"))
                }
                None => Value::String(format!("{target}{suffix}")),
            }
        }
        MountSpec::Tmpfs { target, size_bytes } => {
            let mut tmap = serde_yaml::Mapping::new();
            tmap.insert(Value::String("type".into()), Value::String("tmpfs".into()));
            tmap.insert(
                Value::String("target".into()),
                Value::String(target.clone()),
            );
            if let Some(sz) = size_bytes {
                let mut opts = serde_yaml::Mapping::new();
                opts.insert(
                    Value::String("size".into()),
                    Value::Number(serde_yaml::Number::from(*sz)),
                );
                tmap.insert(Value::String("tmpfs".into()), Value::Mapping(opts));
            }
            Value::Mapping(tmap)
        }
    }
}

/// Healthcheck mapping. `test` is required; the rest are emitted only
/// when the source had them. Durations come out as `<n>s` strings,
/// matching what compose authors actually write.
fn healthcheck_value(h: &HealthcheckSpec) -> Value {
    let mut m = serde_yaml::Mapping::new();
    m.insert(Value::String("test".into()), string_seq(&h.test));
    if let Some(s) = h.interval_secs {
        m.insert(
            Value::String("interval".into()),
            Value::String(format!("{s}s")),
        );
    }
    if let Some(s) = h.timeout_secs {
        m.insert(
            Value::String("timeout".into()),
            Value::String(format!("{s}s")),
        );
    }
    if let Some(r) = h.retries {
        m.insert(
            Value::String("retries".into()),
            Value::Number(serde_yaml::Number::from(u64::from(r))),
        );
    }
    if let Some(s) = h.start_period_secs {
        m.insert(
            Value::String("start_period".into()),
            Value::String(format!("{s}s")),
        );
    }
    Value::Mapping(m)
}

/// Sequence of strings as a YAML value. Used for env, command,
/// entrypoint, healthcheck test, etc. Avoids duplicating
/// `Value::Sequence(... .iter().map(...).collect())` everywhere.
fn string_seq(items: &[String]) -> Value {
    Value::Sequence(items.iter().map(|s| Value::String(s.clone())).collect())
}

/// Top-of-file comment block. Includes the UTC timestamp so the
/// operator can tell at a glance when the snapshot was taken.
fn header_comment(now: chrono::DateTime<Utc>) -> String {
    let ts = now.format("%Y-%m-%dT%H:%M:%SZ");
    format!(
        "# Synthesized from running containers by isd controller at {ts}.\n\
         # Source of truth is the live container set; edits here will not affect\n\
         # the running stack until you ship them via `isd stack deploy`.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: minimal `ContainerRich` skeleton tests fill in.
    fn base(service: &str, image: &str) -> ContainerRich {
        ContainerRich {
            service: service.into(),
            stack: "teststack".into(),
            image: image.into(),
            ports: Vec::new(),
            env: Vec::new(),
            mounts: Vec::new(),
            networks: Vec::new(),
            restart_policy: None,
            command: None,
            entrypoint: None,
            working_dir: None,
            user: None,
            healthcheck: None,
            labels: HashMap::new(),
        }
    }

    /// Parses a synthesized string back into a generic
    /// `serde_yaml::Value` so tests can assert against the structure
    /// without coupling to a specific compose schema.
    fn parse(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).expect("synthesized YAML must parse")
    }

    fn services(v: &Value) -> &serde_yaml::Mapping {
        v.get("services").unwrap().as_mapping().unwrap()
    }

    #[test]
    fn header_comment_precedes_yaml() {
        let out = synthesize(&[base("svc", "nginx:1.27")]);
        assert!(out.starts_with("# Synthesized from running containers"));
        assert!(out.contains("isd stack deploy"));
        // Header is three commented lines, then the body.
        let header_lines = out.lines().take(3).count();
        assert_eq!(header_lines, 3);
    }

    #[test]
    fn empty_input_emits_empty_services_map() {
        let out = synthesize(&[]);
        let parsed = parse(&out);
        let svcs = services(&parsed);
        assert!(svcs.is_empty(), "services must be present but empty");
        assert!(parsed.get("volumes").is_none());
        assert!(parsed.get("networks").is_none());
    }

    #[test]
    fn single_service_round_trips_through_yaml_parser() {
        let mut c = base("web", "nginx:1.27");
        c.env.push("FOO=bar".into());
        c.restart_policy = Some("unless-stopped".into());

        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let svc = services(&parsed).get(Value::String("web".into())).unwrap();
        assert_eq!(svc.get("image").unwrap().as_str().unwrap(), "nginx:1.27");
        assert_eq!(
            svc.get("restart").unwrap().as_str().unwrap(),
            "unless-stopped"
        );
        let env = svc.get("environment").unwrap().as_sequence().unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].as_str().unwrap(), "FOO=bar");
    }

    #[test]
    fn multi_replica_collapses_to_one_service_entry() {
        let c1 = base("api", "img:1");
        let mut c2 = base("api", "img:1");
        // Second replica has a divergent env value; spec says first wins.
        c2.env.push("X=2".into());
        let c3 = base("api", "img:1");

        let out = synthesize(&[c1, c2, c3]);
        let parsed = parse(&out);
        let svcs = services(&parsed);
        assert_eq!(svcs.len(), 1, "three replicas must collapse to one entry");
        let api = svcs.get(Value::String("api".into())).unwrap();
        // First container had no env; later replicas dropped.
        assert!(api.get("environment").is_none());
    }

    #[test]
    fn ports_emit_long_form_with_target_published_protocol() {
        let mut c = base("web", "nginx");
        c.ports.push(PortMapping {
            host_port: Some(8080),
            container_port: 80,
            protocol: "tcp".into(),
        });
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let svc = services(&parsed).get(Value::String("web".into())).unwrap();
        let ports = svc.get("ports").unwrap().as_sequence().unwrap();
        assert_eq!(ports.len(), 1);
        let p = ports[0].as_mapping().unwrap();
        assert_eq!(
            p.get(Value::String("target".into())).unwrap(),
            &Value::from(80)
        );
        assert_eq!(
            p.get(Value::String("published".into())).unwrap(),
            &Value::from(8080)
        );
        assert_eq!(
            p.get(Value::String("protocol".into()))
                .unwrap()
                .as_str()
                .unwrap(),
            "tcp"
        );
    }

    #[test]
    fn expose_only_port_omits_published_key() {
        let mut c = base("internal", "img");
        c.ports.push(PortMapping {
            host_port: None,
            container_port: 9000,
            protocol: "tcp".into(),
        });
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let svc = services(&parsed)
            .get(Value::String("internal".into()))
            .unwrap();
        let p = svc.get("ports").unwrap().as_sequence().unwrap()[0]
            .as_mapping()
            .unwrap();
        assert!(p.get(Value::String("published".into())).is_none());
    }

    #[test]
    fn udp_protocol_round_trips_verbatim() {
        let mut c = base("dns", "coredns");
        c.ports.push(PortMapping {
            host_port: Some(53),
            container_port: 53,
            protocol: "udp".into(),
        });
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let proto = services(&parsed)
            .get(Value::String("dns".into()))
            .unwrap()
            .get("ports")
            .unwrap()
            .as_sequence()
            .unwrap()[0]
            .as_mapping()
            .unwrap()
            .get(Value::String("protocol".into()))
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(proto, "udp");
    }

    #[test]
    fn bind_mount_emits_short_form_string() {
        let mut c = base("app", "img");
        c.mounts.push(MountSpec::Bind {
            source: "./host".into(),
            target: "/container".into(),
            read_only: false,
        });
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let vols = services(&parsed)
            .get(Value::String("app".into()))
            .unwrap()
            .get("volumes")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(vols.len(), 1);
        assert_eq!(vols[0].as_str().unwrap(), "./host:/container");
        // No named volume registered top-level for a pure bind mount.
        assert!(parsed.get("volumes").is_none());
    }

    #[test]
    fn bind_mount_read_only_appends_ro_suffix() {
        let mut c = base("app", "img");
        c.mounts.push(MountSpec::Bind {
            source: "/data".into(),
            target: "/var/data".into(),
            read_only: true,
        });
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let v = services(&parsed)
            .get(Value::String("app".into()))
            .unwrap()
            .get("volumes")
            .unwrap()
            .as_sequence()
            .unwrap()[0]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(v, "/data:/var/data:ro");
    }

    #[test]
    fn named_volume_registers_top_level() {
        let mut c = base("db", "postgres:16");
        c.mounts.push(MountSpec::Volume {
            name: Some("myvol".into()),
            target: "/var/lib/postgresql/data".into(),
            read_only: false,
        });
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let v = services(&parsed)
            .get(Value::String("db".into()))
            .unwrap()
            .get("volumes")
            .unwrap()
            .as_sequence()
            .unwrap()[0]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(v, "myvol:/var/lib/postgresql/data");
        let top_vols = parsed.get("volumes").unwrap().as_mapping().unwrap();
        assert!(top_vols.contains_key(Value::String("myvol".into())));
    }

    #[test]
    fn anonymous_volume_skips_top_level_registration() {
        let mut c = base("cache", "img");
        c.mounts.push(MountSpec::Volume {
            name: None,
            target: "/tmp/cache".into(),
            read_only: false,
        });
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let v = services(&parsed)
            .get(Value::String("cache".into()))
            .unwrap()
            .get("volumes")
            .unwrap()
            .as_sequence()
            .unwrap()[0]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(v, "/tmp/cache");
        assert!(parsed.get("volumes").is_none());
    }

    #[test]
    fn tmpfs_mount_emits_long_form_with_size() {
        let mut c = base("worker", "img");
        c.mounts.push(MountSpec::Tmpfs {
            target: "/scratch".into(),
            size_bytes: Some(1024 * 1024),
        });
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let m = services(&parsed)
            .get(Value::String("worker".into()))
            .unwrap()
            .get("volumes")
            .unwrap()
            .as_sequence()
            .unwrap()[0]
            .as_mapping()
            .unwrap()
            .clone();
        assert_eq!(
            m.get(Value::String("type".into()))
                .unwrap()
                .as_str()
                .unwrap(),
            "tmpfs"
        );
        assert_eq!(
            m.get(Value::String("target".into()))
                .unwrap()
                .as_str()
                .unwrap(),
            "/scratch"
        );
        let opts = m
            .get(Value::String("tmpfs".into()))
            .unwrap()
            .as_mapping()
            .unwrap();
        assert_eq!(
            opts.get(Value::String("size".into())).unwrap(),
            &Value::from(1024u64 * 1024)
        );
    }

    #[test]
    fn user_network_registers_top_level_but_builtins_dont() {
        let mut c = base("svc", "img");
        c.networks.push("bridge-net".into());
        c.networks.push("bridge".into()); // built-in, skipped
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let svc_nets = services(&parsed)
            .get(Value::String("svc".into()))
            .unwrap()
            .get("networks")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(svc_nets.len(), 2);
        let top = parsed.get("networks").unwrap().as_mapping().unwrap();
        assert!(top.contains_key(Value::String("bridge-net".into())));
        assert!(
            !top.contains_key(Value::String("bridge".into())),
            "built-in `bridge` must not appear in top-level networks"
        );
    }

    #[test]
    fn only_builtin_networks_means_no_top_level_block() {
        let mut c = base("svc", "img");
        c.networks.push("host".into());
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        assert!(parsed.get("networks").is_none());
    }

    #[test]
    fn healthcheck_with_all_fields_emits_all_keys() {
        let mut c = base("api", "img");
        c.healthcheck = Some(HealthcheckSpec {
            test: vec![
                "CMD".into(),
                "curl".into(),
                "-f".into(),
                "http://localhost/".into(),
            ],
            interval_secs: Some(30),
            timeout_secs: Some(5),
            retries: Some(3),
            start_period_secs: Some(10),
        });
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let hc = services(&parsed)
            .get(Value::String("api".into()))
            .unwrap()
            .get("healthcheck")
            .unwrap()
            .as_mapping()
            .unwrap()
            .clone();
        let test = hc
            .get(Value::String("test".into()))
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(test.len(), 4);
        assert_eq!(test[0].as_str().unwrap(), "CMD");
        assert_eq!(
            hc.get(Value::String("interval".into()))
                .unwrap()
                .as_str()
                .unwrap(),
            "30s"
        );
        assert_eq!(
            hc.get(Value::String("timeout".into()))
                .unwrap()
                .as_str()
                .unwrap(),
            "5s"
        );
        assert_eq!(
            hc.get(Value::String("retries".into())).unwrap(),
            &Value::from(3u64)
        );
        assert_eq!(
            hc.get(Value::String("start_period".into()))
                .unwrap()
                .as_str()
                .unwrap(),
            "10s"
        );
    }

    #[test]
    fn healthcheck_with_only_test_omits_optional_keys() {
        let mut c = base("api", "img");
        c.healthcheck = Some(HealthcheckSpec {
            test: vec!["CMD-SHELL".into(), "stat /tmp/ok".into()],
            interval_secs: None,
            timeout_secs: None,
            retries: None,
            start_period_secs: None,
        });
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let hc = services(&parsed)
            .get(Value::String("api".into()))
            .unwrap()
            .get("healthcheck")
            .unwrap()
            .as_mapping()
            .unwrap()
            .clone();
        assert_eq!(hc.len(), 1, "only `test` should be present");
        assert!(hc.contains_key(Value::String("test".into())));
    }

    #[test]
    fn restart_policy_values_emit_verbatim() {
        for policy in [
            "no",
            "always",
            "unless-stopped",
            "on-failure",
            "on-failure:3",
        ] {
            let mut c = base("svc", "img");
            c.restart_policy = Some(policy.into());
            let out = synthesize(&[c]);
            let parsed = parse(&out);
            let r = services(&parsed)
                .get(Value::String("svc".into()))
                .unwrap()
                .get("restart")
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(r, policy, "restart policy must round-trip exactly");
        }
    }

    #[test]
    fn command_and_entrypoint_emit_as_sequences() {
        let mut c = base("svc", "img");
        c.command = Some(vec!["serve".into(), "--port".into(), "80".into()]);
        c.entrypoint = Some(vec!["/init".into()]);
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let svc = services(&parsed).get(Value::String("svc".into())).unwrap();
        let cmd = svc.get("command").unwrap().as_sequence().unwrap();
        assert_eq!(cmd.len(), 3);
        assert_eq!(cmd[2].as_str().unwrap(), "80");
        let ep = svc.get("entrypoint").unwrap().as_sequence().unwrap();
        assert_eq!(ep.len(), 1);
        assert_eq!(ep[0].as_str().unwrap(), "/init");
    }

    #[test]
    fn working_dir_and_user_emit_when_set() {
        let mut c = base("svc", "img");
        c.working_dir = Some("/srv".into());
        c.user = Some("1000:1000".into());
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let svc = services(&parsed).get(Value::String("svc".into())).unwrap();
        assert_eq!(svc.get("working_dir").unwrap().as_str().unwrap(), "/srv");
        assert_eq!(svc.get("user").unwrap().as_str().unwrap(), "1000:1000");
    }

    #[test]
    fn labels_emit_as_sorted_mapping() {
        let mut c = base("svc", "img");
        c.labels.insert("z.last".into(), "z".into());
        c.labels.insert("a.first".into(), "a".into());
        c.labels
            .insert("com.docker.compose.service".into(), "svc".into());
        let out = synthesize(&[c]);
        let parsed = parse(&out);
        let labels = services(&parsed)
            .get(Value::String("svc".into()))
            .unwrap()
            .get("labels")
            .unwrap()
            .as_mapping()
            .unwrap()
            .clone();
        let keys: Vec<&str> = labels.iter().map(|(k, _)| k.as_str().unwrap()).collect();
        // Sorted alphabetically.
        assert_eq!(
            keys,
            vec!["a.first", "com.docker.compose.service", "z.last"]
        );
    }

    #[test]
    fn services_emit_in_alphabetical_order() {
        let containers = vec![
            base("zebra", "img"),
            base("alpha", "img"),
            base("middle", "img"),
        ];
        let out = synthesize(&containers);
        let parsed = parse(&out);
        let keys: Vec<&str> = services(&parsed)
            .iter()
            .map(|(k, _)| k.as_str().unwrap())
            .collect();
        assert_eq!(keys, vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn optional_fields_omitted_when_none() {
        // Base container has all optionals as None and all collections empty;
        // service entry should only contain `image`.
        let out = synthesize(&[base("svc", "img")]);
        let parsed = parse(&out);
        let svc = services(&parsed)
            .get(Value::String("svc".into()))
            .unwrap()
            .as_mapping()
            .unwrap()
            .clone();
        let keys: Vec<&str> = svc.iter().map(|(k, _)| k.as_str().unwrap()).collect();
        assert_eq!(keys, vec!["image"]);
    }

    #[test]
    fn multiple_services_mix_named_and_anonymous_volumes_correctly() {
        let mut a = base("a", "img");
        a.mounts.push(MountSpec::Volume {
            name: Some("shared".into()),
            target: "/data".into(),
            read_only: false,
        });
        let mut b = base("b", "img");
        b.mounts.push(MountSpec::Volume {
            name: Some("shared".into()),
            target: "/mnt".into(),
            read_only: true,
        });
        b.mounts.push(MountSpec::Volume {
            name: None,
            target: "/tmp".into(),
            read_only: false,
        });
        let out = synthesize(&[a, b]);
        let parsed = parse(&out);
        let top_vols = parsed.get("volumes").unwrap().as_mapping().unwrap();
        assert_eq!(top_vols.len(), 1, "only `shared` should register top-level");
        assert!(top_vols.contains_key(Value::String("shared".into())));
        let b_vols = services(&parsed)
            .get(Value::String("b".into()))
            .unwrap()
            .get("volumes")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(b_vols[0].as_str().unwrap(), "shared:/mnt:ro");
        assert_eq!(b_vols[1].as_str().unwrap(), "/tmp");
    }

    #[test]
    fn synthesized_output_is_deterministic_across_runs() {
        // Same input must produce the same body twice (modulo the
        // timestamp in the header, which we strip).
        let mut c = base("svc", "img");
        c.labels.insert("k".into(), "v".into());
        c.networks.push("net".into());

        let strip_header = |s: String| {
            s.lines()
                .skip_while(|l| l.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let a = strip_header(synthesize(&[c.clone()]));
        let b = strip_header(synthesize(&[c]));
        assert_eq!(a, b);
    }
}
