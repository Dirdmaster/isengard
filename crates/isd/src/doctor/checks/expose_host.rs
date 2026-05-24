//! `EXPOSE_HOST_MISSING`: services that publish an HTTP-ish port but
//! have no `isengard.expose` label are unreachable through Pingora.
//!
//! Pingora reverse-proxies inbound HTTPS to the upstream named in
//! `isengard.expose`. A service with `ports: ["8080:80"]` and no
//! `isengard.expose` works (host port published, dockerd routes) but
//! the operator usually wanted "public hostname maps to this service"
//! and just forgot the label.
//!
//! Heuristic: emit a finding when a service publishes any port from
//! [`HTTP_ISH_PORTS`] AND carries no key under `labels` that starts
//! with `isengard.expose`. The hint suggests adding the label and
//! points at `isd stack doctor` for the interactive fix (v0.2).

use serde_yaml::Value;

use crate::doctor::{Finding, Severity};

/// Container-internal ports we treat as web traffic when deciding
/// whether to emit the finding. Captures the common HTTP defaults
/// (80, 443), Plex (32400), Node / Rails / Flask defaults (3000, 5000,
/// 8000), the `:8080` convention, and grafana / prometheus / minio
/// dashboards (9000 family).
const HTTP_ISH_PORTS: &[u16] = &[80, 443, 3000, 5000, 8000, 8080, 9000, 32400];

/// Walk every `services.<name>` entry and emit findings for the
/// services that publish HTTP-ish ports without an `isengard.expose`
/// label.
pub fn check(compose: &Value) -> Vec<Finding> {
    let mut out = Vec::new();
    let services = match compose.get("services").and_then(Value::as_mapping) {
        Some(m) => m,
        None => return out,
    };
    for (name, svc) in services {
        let Some(name) = name.as_str() else { continue };
        let Some(svc) = svc.as_mapping() else {
            continue;
        };
        if has_expose_label(svc) {
            let candidate_ports = published_container_ports(svc);
            if let Some(port) = expose_port_label(svc) {
                if port.parse::<u16>().is_err() {
                    out.push(Finding {
                        id: "EXPOSE_PORT_INVALID",
                        severity: Severity::Warning,
                        message: format!(
                            "services.{name} has an invalid `isengard.expose.port` label"
                        ),
                        hint: Some("choose a numeric container port".into()),
                        target: Some(crate::doctor::FindingTarget::Service {
                            name: name.to_string(),
                        }),
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
                    message: format!(
                        "services.{name} has an expose hostname but multiple candidate ports"
                    ),
                    hint: Some(
                        "choose the upstream container port for `isengard.expose.port`".into(),
                    ),
                    target: Some(crate::doctor::FindingTarget::Service {
                        name: name.to_string(),
                    }),
                    fix: Some(crate::doctor::FixSpec::SetExposePort {
                        service: name.to_string(),
                        candidate_ports,
                    }),
                });
            }
            continue;
        }
        let candidate_ports = http_ish_ports(svc);
        let inferred_port = match candidate_ports.as_slice() {
            [] => continue,
            [port] => Some(*port),
            _ => None,
        };
        out.push(Finding {
            id: "EXPOSE_HOST_MISSING",
            severity: Severity::Warning,
            message: format!(
                "services.{name} publishes a web port but has no `isengard.expose` label"
            ),
            hint: Some(format!(
                "add `labels: {{ isengard.expose: <hostname> }}` so Pingora proxies inbound HTTPS to services.{name}"
            )),
            target: Some(crate::doctor::FindingTarget::Service {
                name: name.to_string(),
            }),
            fix: Some(crate::doctor::FixSpec::ExposeService {
                service: name.to_string(),
                inferred_port,
            }),
        });
    }
    out
}

/// Return sorted unique HTTP-ish container ports for a service.
fn http_ish_ports(svc: &serde_yaml::Mapping) -> Vec<u16> {
    let Some(ports) = svc.get("ports").and_then(Value::as_sequence) else {
        return Vec::new();
    };
    let mut found = ports.iter().filter_map(http_ish_port).collect::<Vec<_>>();
    found.sort_unstable();
    found.dedup();
    found
}

/// Return sorted unique published container ports for an exposed service.
fn published_container_ports(svc: &serde_yaml::Mapping) -> Vec<u16> {
    let Some(ports) = svc.get("ports").and_then(Value::as_sequence) else {
        return Vec::new();
    };
    let mut found = ports.iter().filter_map(container_port).collect::<Vec<_>>();
    found.sort_unstable();
    found.dedup();
    found
}

fn container_port(spec: &Value) -> Option<u16> {
    if let Some(s) = spec.as_str() {
        let container = s.rsplit(':').next().unwrap_or(s);
        let container = container.split('/').next().unwrap_or(container);
        container.parse::<u16>().ok()
    } else if let Some(m) = spec.as_mapping() {
        let n = m.get("target")?.as_u64()?;
        u16::try_from(n).ok()
    } else {
        None
    }
}

/// Return the container port when `spec` resolves to a port from
/// [`HTTP_ISH_PORTS`]. Accepts the short form (`"HOST:CONTAINER"` or
/// `"PORT"` strings, with an optional `/tcp` or `/udp` suffix) and
/// the long form (a mapping with `target: N`).
fn http_ish_port(spec: &Value) -> Option<u16> {
    let port = container_port(spec)?;
    HTTP_ISH_PORTS.contains(&port).then_some(port)
}

/// True when `services.<name>.labels` carries any key under the
/// `isengard.expose*` namespace (`isengard.expose`, `isengard.expose.host`,
/// `isengard.expose.port`, etc.). Accepts both the map form (`labels:
/// { isengard.expose: foo }`) and the list form (`labels: ["isengard.expose=foo"]`).
fn has_expose_label(svc: &serde_yaml::Mapping) -> bool {
    let Some(labels) = svc.get("labels") else {
        return false;
    };
    if let Some(map) = labels.as_mapping() {
        return map
            .keys()
            .filter_map(Value::as_str)
            .any(|k| k.starts_with("isengard.expose"));
    }
    if let Some(seq) = labels.as_sequence() {
        return seq.iter().filter_map(Value::as_str).any(|entry| {
            let key = entry.split('=').next().unwrap_or(entry);
            key.starts_with("isengard.expose")
        });
    }
    false
}

fn expose_port_label(svc: &serde_yaml::Mapping) -> Option<&str> {
    let labels = svc.get("labels")?;
    if let Some(map) = labels.as_mapping() {
        return map
            .get(Value::String("isengard.expose.port".to_string()))
            .and_then(Value::as_str);
    }
    labels
        .as_sequence()?
        .iter()
        .filter_map(Value::as_str)
        .find_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            (key == "isengard.expose.port").then_some(value)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn idiomatic_service_emits_no_finding() {
        let v = parse(
            r#"
services:
  web:
    image: nginx:alpine
    ports:
      - "8080:80"
    labels:
      isengard.expose: "demo.example.com"
"#,
        );
        assert!(check(&v).is_empty());
    }

    #[test]
    fn bare_http_service_emits_finding() {
        let v = parse(
            r#"
services:
  web:
    image: nginx:alpine
    ports:
      - "8080:80"
"#,
        );
        let f = check(&v);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].id, "EXPOSE_HOST_MISSING");
        assert!(f[0].message.contains("services.web"));
    }

    #[test]
    fn bare_http_service_carries_structured_fix_data() {
        let v = parse(
            r#"
services:
  plex:
    image: lscr.io/linuxserver/plex
    ports:
      - "32400:32400"
"#,
        );
        let f = check(&v);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].id, "EXPOSE_HOST_MISSING");
        assert_eq!(
            f[0].target,
            Some(crate::doctor::FindingTarget::Service {
                name: "plex".to_string()
            })
        );
        assert_eq!(
            f[0].fix,
            Some(crate::doctor::FixSpec::ExposeService {
                service: "plex".to_string(),
                inferred_port: Some(32400),
            })
        );
    }

    #[test]
    fn ambiguous_http_ports_emit_finding_without_inferred_port() {
        let v = parse(
            r#"
services:
  web:
    image: nginx:alpine
    ports:
      - "8080:80"
      - "8443:443"
"#,
        );

        let f = check(&v);

        assert_eq!(f.len(), 1);
        assert_eq!(f[0].id, "EXPOSE_HOST_MISSING");
        assert_eq!(
            f[0].fix,
            Some(crate::doctor::FixSpec::ExposeService {
                service: "web".to_string(),
                inferred_port: None,
            })
        );
    }

    #[test]
    fn long_form_port_mapping_is_detected() {
        let v = parse(
            r#"
services:
  grafana:
    image: grafana/grafana
    ports:
      - target: 3000
        published: 3000
        protocol: tcp
"#,
        );
        let f = check(&v);
        assert_eq!(f.len(), 1);
        assert!(f[0].message.contains("services.grafana"));
    }

    #[test]
    fn label_list_form_is_recognized() {
        let v = parse(
            r#"
services:
  web:
    image: nginx
    ports: ["80"]
    labels:
      - "isengard.expose=demo.test"
"#,
        );
        assert!(check(&v).is_empty());
    }

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

    #[test]
    fn invalid_port_label_warns_even_without_candidate_ports() {
        let v = parse(
            r#"
services:
  custom:
    image: custom
    labels:
      isengard.expose: custom.vallee.casa
      isengard.expose.port: nope
"#,
        );

        let findings = check(&v);
        assert!(findings.iter().any(|f| f.id == "EXPOSE_PORT_INVALID"));
    }

    #[test]
    fn non_http_port_is_skipped() {
        // SSH (22) shouldn't trigger the heuristic; it's not a web
        // service.
        let v = parse(
            r#"
services:
  sshd:
    image: alpine
    ports:
      - "2222:22"
"#,
        );
        assert!(check(&v).is_empty());
    }

    #[test]
    fn peer_only_bittorrent_port_does_not_need_expose_host() {
        let v = parse(
            r#"
services:
  qbittorrent:
    image: qbittorrent
    ports: ["6881:6881"]
"#,
        );

        let findings = check(&v);
        assert!(!findings.iter().any(|f| f.id == "EXPOSE_HOST_MISSING"));
    }

    #[test]
    fn protocol_suffix_is_stripped() {
        // `8080/tcp` should still parse as 8080.
        let v = parse(
            r#"
services:
  web:
    image: nginx
    ports:
      - "8080:80/tcp"
"#,
        );
        let f = check(&v);
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn service_with_no_ports_does_not_warn() {
        // A backend with no published port is presumably internal-only
        // and doesn't want a public hostname.
        let v = parse(
            r#"
services:
  worker:
    image: rust:alpine
"#,
        );
        assert!(check(&v).is_empty());
    }
}
