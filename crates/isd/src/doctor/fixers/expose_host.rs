//! Mutations for the `EXPOSE_HOST_MISSING` doctor fixer.

use anyhow::Result;
use serde_yaml::Value;

/// Input required to add Isengard expose metadata to a service.
#[allow(dead_code)]
// Wired into command flow by a later doctor fixer task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposeHostInput {
    /// Compose service name to mutate.
    pub service: String,
    /// Hostname to publish through `isengard.expose`.
    pub hostname: String,
    /// Optional service port to publish through `isengard.expose.port`.
    pub port: Option<u16>,
}

/// Build expose-host fixer input from a doctor finding.
pub fn input_from_finding(finding: &crate::doctor::Finding) -> Option<ExposeHostInput> {
    let crate::doctor::FixSpec::ExposeService {
        service,
        inferred_port,
    } = finding.fix.clone()?
    else {
        return None;
    };
    Some(ExposeHostInput {
        service,
        hostname: String::new(),
        port: inferred_port,
    })
}

/// Add expose labels to a compose service without overwriting existing expose metadata.
pub fn apply_expose_host(compose: &mut Value, input: &ExposeHostInput) -> Result<bool> {
    let services = compose
        .get_mut("services")
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| anyhow::anyhow!("compose has no services mapping"))?;
    let service = services
        .get_mut(Value::String(input.service.clone()))
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| anyhow::anyhow!("service {:?} not found", input.service))?;

    let labels_key = Value::String("labels".to_string());
    if !service.contains_key(&labels_key) {
        let mut labels = serde_yaml::Mapping::new();
        labels.insert(
            Value::String("isengard.expose".into()),
            Value::String(input.hostname.clone()),
        );
        if let Some(port) = input.port {
            labels.insert(
                Value::String("isengard.expose.port".into()),
                Value::String(port.to_string()),
            );
        }
        service.insert(labels_key, Value::Mapping(labels));
        return Ok(true);
    }

    let labels = service.get_mut(&labels_key).expect("checked above");
    if let Some(map) = labels.as_mapping_mut() {
        if map
            .keys()
            .filter_map(Value::as_str)
            .any(is_expose_hostname_label_key)
        {
            return Ok(false);
        }
        map.insert(
            Value::String("isengard.expose".into()),
            Value::String(input.hostname.clone()),
        );
        if let Some(port) = input.port {
            map.insert(
                Value::String("isengard.expose.port".into()),
                Value::String(port.to_string()),
            );
        }
        return Ok(true);
    }
    if let Some(seq) = labels.as_sequence_mut() {
        if seq
            .iter()
            .filter_map(Value::as_str)
            .any(|entry| is_expose_hostname_label_key(entry.split('=').next().unwrap_or(entry)))
        {
            return Ok(false);
        }
        seq.push(Value::String(format!("isengard.expose={}", input.hostname)));
        if let Some(port) = input.port {
            seq.push(Value::String(format!("isengard.expose.port={port}")));
        }
        return Ok(true);
    }
    Err(anyhow::anyhow!(
        "services.{}.labels must be a map or list",
        input.service
    ))
}

fn is_expose_hostname_label_key(key: &str) -> bool {
    if key == "isengard.expose" {
        return true;
    }

    let Some(name) = key.strip_prefix("isengard.expose.") else {
        return false;
    };

    !name.is_empty()
        && !name.contains('.')
        && !matches!(name, "port" | "tls" | "health" | "adapter" | "auth")
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn creates_label_map_when_missing() {
        let mut v = parse("services:\n  plex:\n    image: plex\n    ports: [\"32400:32400\"]\n");
        let changed = apply_expose_host(
            &mut v,
            &ExposeHostInput {
                service: "plex".into(),
                hostname: "plex.vallee.casa".into(),
                port: None,
            },
        )
        .unwrap();
        assert!(changed);
        let labels = &v["services"]["plex"]["labels"];
        assert_eq!(labels["isengard.expose"].as_str(), Some("plex.vallee.casa"));
        assert!(labels["isengard.expose.port"].is_null());
    }

    #[test]
    fn omits_port_label_when_port_is_unknown() {
        let mut v =
            parse("services:\n  web:\n    image: nginx\n    ports: [\"8080:80\", \"8443:443\"]\n");
        let changed = apply_expose_host(
            &mut v,
            &ExposeHostInput {
                service: "web".into(),
                hostname: "web.test".into(),
                port: None,
            },
        )
        .unwrap();

        assert!(changed);
        let labels = &v["services"]["web"]["labels"];
        assert_eq!(labels["isengard.expose"].as_str(), Some("web.test"));
        assert!(labels["isengard.expose.port"].is_null());
    }

    #[test]
    fn appends_to_label_list() {
        let mut v = parse(
            "services:\n  web:\n    image: nginx\n    ports: [\"8080:80\"]\n    labels:\n      - \"foo=bar\"\n",
        );
        apply_expose_host(
            &mut v,
            &ExposeHostInput {
                service: "web".into(),
                hostname: "web.test".into(),
                port: Some(80),
            },
        )
        .unwrap();
        let labels = v["services"]["web"]["labels"].as_sequence().unwrap();
        assert!(labels.iter().any(|v| v.as_str() == Some("foo=bar")));
        assert!(
            labels
                .iter()
                .any(|v| v.as_str() == Some("isengard.expose=web.test"))
        );
        assert!(
            labels
                .iter()
                .any(|v| v.as_str() == Some("isengard.expose.port=80"))
        );
    }

    #[test]
    fn adds_hostname_when_only_port_label_exists() {
        let mut v = parse("services:\n  web:\n    labels:\n      isengard.expose.port: \"8080\"\n");

        let changed = apply_expose_host(
            &mut v,
            &ExposeHostInput {
                service: "web".into(),
                hostname: "web.test".into(),
                port: None,
            },
        )
        .unwrap();

        assert!(changed);
        let labels = &v["services"]["web"]["labels"];
        assert_eq!(labels["isengard.expose"].as_str(), Some("web.test"));
        assert_eq!(labels["isengard.expose.port"].as_str(), Some("8080"));
    }

    #[test]
    fn appends_hostname_to_label_list_with_only_port_label() {
        let mut v =
            parse("services:\n  web:\n    labels:\n      - \"isengard.expose.port=8080\"\n");

        let changed = apply_expose_host(
            &mut v,
            &ExposeHostInput {
                service: "web".into(),
                hostname: "web.test".into(),
                port: None,
            },
        )
        .unwrap();

        assert!(changed);
        let labels = v["services"]["web"]["labels"].as_sequence().unwrap();
        assert!(
            labels
                .iter()
                .any(|v| v.as_str() == Some("isengard.expose.port=8080"))
        );
        assert!(
            labels
                .iter()
                .any(|v| v.as_str() == Some("isengard.expose=web.test"))
        );
    }

    #[test]
    fn refuses_to_overwrite_existing_expose_label() {
        let mut v = parse("services:\n  web:\n    labels:\n      isengard.expose: old.test\n");
        let changed = apply_expose_host(
            &mut v,
            &ExposeHostInput {
                service: "web".into(),
                hostname: "new.test".into(),
                port: Some(80),
            },
        )
        .unwrap();
        assert!(!changed);
        assert_eq!(
            v["services"]["web"]["labels"]["isengard.expose"].as_str(),
            Some("old.test")
        );
    }

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
}
