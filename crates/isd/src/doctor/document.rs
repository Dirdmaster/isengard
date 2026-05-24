#![allow(dead_code)]

use anyhow::{Context as _, Result, anyhow};
use serde_yaml::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeSyntax {
    Yaml,
    Toml,
}

pub struct ComposeDocument {
    pub syntax: ComposeSyntax,
    pub value: Value,
}

impl ComposeDocument {
    pub fn parse_path(path: &Path, content: &str) -> Result<Self> {
        let syntax = syntax_for_path(path)?;
        match syntax {
            ComposeSyntax::Yaml => Ok(Self {
                syntax,
                value: serde_yaml::from_str(content)
                    .with_context(|| format!("parsing YAML compose at {}", path.display()))?,
            }),
            ComposeSyntax::Toml => {
                let value: toml::Value = toml::from_str(content)
                    .with_context(|| format!("parsing TOML compose at {}", path.display()))?;
                Ok(Self {
                    syntax,
                    value: toml_value_to_yaml(value)?,
                })
            }
        }
    }

    pub fn parse_controller_yaml(content: &str) -> Result<Self> {
        Ok(Self {
            syntax: ComposeSyntax::Yaml,
            value: serde_yaml::from_str(content).context("parsing controller compose YAML")?,
        })
    }

    pub fn to_string(&self) -> Result<String> {
        match self.syntax {
            ComposeSyntax::Yaml => {
                serde_yaml::to_string(&self.value).context("serializing YAML compose")
            }
            ComposeSyntax::Toml => {
                let toml_value = yaml_value_to_toml(&self.value)?;
                toml::to_string_pretty(&toml_value).context("serializing TOML compose")
            }
        }
    }
}

fn syntax_for_path(path: &Path) -> Result<ComposeSyntax> {
    match path.extension().and_then(|s| s.to_str()) {
        Some("yaml" | "yml") => Ok(ComposeSyntax::Yaml),
        Some("toml") => Ok(ComposeSyntax::Toml),
        other => Err(anyhow!(
            "unsupported compose extension {:?}; expected yaml, yml, or toml",
            other
        )),
    }
}

fn toml_value_to_yaml(value: toml::Value) -> Result<Value> {
    let json = toml_to_json(value)?;
    serde_yaml::to_value(json).context("converting TOML compose to YAML value")
}

fn toml_to_json(value: toml::Value) -> Result<serde_json::Value> {
    use serde_json::Value as Json;
    Ok(match value {
        toml::Value::String(s) => Json::String(s),
        toml::Value::Integer(i) => Json::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(Json::Number)
            .ok_or_else(|| anyhow!("non-finite TOML float is not supported in compose: {f}"))?,
        toml::Value::Boolean(b) => Json::Bool(b),
        toml::Value::Datetime(dt) => Json::String(dt.to_string()),
        toml::Value::Array(items) => {
            Json::Array(items.into_iter().map(toml_to_json).collect::<Result<_>>()?)
        }
        toml::Value::Table(table) => {
            let mut out = serde_json::Map::new();
            for (key, value) in table {
                out.insert(key, toml_to_json(value)?);
            }
            Json::Object(out)
        }
    })
}

fn yaml_value_to_toml(value: &Value) -> Result<toml::Value> {
    Ok(match value {
        Value::Null => return Err(anyhow!("null YAML values cannot be serialized to TOML")),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                toml::Value::Float(f)
            } else {
                return Err(anyhow!("unsupported numeric YAML value: {n}"));
            }
        }
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Sequence(items) => toml::Value::Array(
            items
                .iter()
                .map(yaml_value_to_toml)
                .collect::<Result<Vec<_>>>()?,
        ),
        Value::Mapping(map) => {
            let mut table = toml::map::Map::new();
            for (key, value) in map {
                let key = key
                    .as_str()
                    .ok_or_else(|| anyhow!("TOML compose keys must be strings"))?;
                table.insert(key.to_string(), yaml_value_to_toml(value)?);
            }
            toml::Value::Table(table)
        }
        Value::Tagged(_) => return Err(anyhow!("tagged YAML values are not supported in compose")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_document_round_trips_services_shape() {
        let doc = ComposeDocument::parse_path(
            Path::new("compose.yaml"),
            "services:\n  web:\n    image: nginx\n",
        )
        .unwrap();
        assert_eq!(doc.syntax, ComposeSyntax::Yaml);
        assert!(doc.value.get("services").is_some());
        assert!(doc.to_string().unwrap().contains("services:"));
    }

    #[test]
    fn toml_document_normalizes_to_yaml_value_and_serializes_toml() {
        let doc = ComposeDocument::parse_path(
            Path::new("compose.toml"),
            "[services.web]\nimage = \"nginx\"\nports = [\"8080:80\"]\n",
        )
        .unwrap();
        assert_eq!(doc.syntax, ComposeSyntax::Toml);
        assert!(doc.value.get("services").is_some());
        let rendered = doc.to_string().unwrap();
        assert!(rendered.contains("[services.web]"), "{rendered}");
        assert!(rendered.contains("image = \"nginx\""), "{rendered}");
    }

    #[test]
    fn toml_dotted_label_key_stays_literal() {
        let doc = ComposeDocument::parse_path(
            Path::new("compose.toml"),
            "[services.web]\nimage = \"nginx\"\n\n[services.web.labels]\n\"isengard.expose\" = \"web.test\"\n",
        )
        .unwrap();
        assert_eq!(
            doc.value["services"]["web"]["labels"]["isengard.expose"].as_str(),
            Some("web.test")
        );
        let rendered = doc.to_string().unwrap();
        assert!(rendered.contains("[services.web.labels]"), "{rendered}");
        assert!(
            rendered.contains("\"isengard.expose\" = \"web.test\""),
            "{rendered}"
        );
        assert!(
            !rendered.contains("[services.web.labels.isengard]"),
            "{rendered}"
        );
    }

    #[test]
    fn yaml_null_cannot_serialize_to_toml() {
        let doc = ComposeDocument {
            syntax: ComposeSyntax::Toml,
            value: serde_yaml::from_str("services:\n  web:\n    labels:\n      maybe:\n").unwrap(),
        };
        let err = doc.to_string().unwrap_err().to_string();
        assert!(err.contains("null"), "{err}");
    }

    #[test]
    fn toml_non_finite_float_cannot_normalize_to_yaml_value() {
        let result = ComposeDocument::parse_path(
            Path::new("compose.toml"),
            "[services.web]\nimage = \"nginx\"\nscale = nan\n",
        );
        let err = match result {
            Ok(_) => panic!("expected non-finite float to fail"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("non-finite"), "{err}");
    }
}
