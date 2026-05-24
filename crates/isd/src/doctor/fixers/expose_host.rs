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

/// Add expose labels to a compose service without overwriting existing expose metadata.
#[allow(dead_code)]
// Wired into command flow by a later doctor fixer task.
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
            .any(|k| k.starts_with("isengard.expose"))
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
        if seq.iter().filter_map(Value::as_str).any(|entry| {
            entry
                .split('=')
                .next()
                .unwrap_or(entry)
                .starts_with("isengard.expose")
        }) {
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
                port: Some(32400),
            },
        )
        .unwrap();
        assert!(changed);
        let labels = &v["services"]["plex"]["labels"];
        assert_eq!(labels["isengard.expose"].as_str(), Some("plex.vallee.casa"));
        assert_eq!(labels["isengard.expose.port"].as_str(), Some("32400"));
    }

    #[test]
    fn appends_to_label_list() {
        let mut v = parse("services:\n  web:\n    image: nginx\n    ports: [\"8080:80\"]\n    labels:\n      - \"foo=bar\"\n");
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
        assert!(labels.iter().any(|v| v.as_str() == Some("isengard.expose=web.test")));
        assert!(labels.iter().any(|v| v.as_str() == Some("isengard.expose.port=80")));
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
        assert_eq!(v["services"]["web"]["labels"]["isengard.expose"].as_str(), Some("old.test"));
    }
}
