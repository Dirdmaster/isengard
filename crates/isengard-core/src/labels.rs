//! Parse `isengard.expose.*` labels into structured rules.
//!
//! Rules:
//! - `isengard.expose = <hostname>` → default unnamed rule
//! - `isengard.expose.<prop> = <value>` where <prop> ∈ KNOWN_PROPS → property of default rule
//! - `isengard.expose.<name> = <hostname>` where <name> ∉ KNOWN_PROPS → named rule
//! - `isengard.expose.<name>.<prop> = <value>` → property of named rule

use std::collections::HashMap;

const KNOWN_PROPS: &[&str] = &["port", "tls", "health", "adapter", "auth"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelRule {
    /// None = default unnamed rule
    pub name: Option<String>,
    pub hostname: String,
    pub port: Option<u16>,
    pub tls: Option<String>,
    pub health: Option<String>,
    pub adapter: Option<String>,
    pub auth: Option<String>,
}

pub fn parse_labels(labels: &HashMap<String, String>) -> Vec<LabelRule> {
    let mut by_name: HashMap<Option<String>, LabelRule> = HashMap::new();

    for (k, v) in labels {
        let Some(rest) = k.strip_prefix("isengard.expose") else {
            continue;
        };
        let segments: Vec<&str> = if rest.is_empty() {
            vec![]
        } else {
            rest.trim_start_matches('.').split('.').collect()
        };

        match segments.len() {
            0 => {
                by_name.entry(None).or_default().hostname = v.clone();
            }
            1 => {
                let s = segments[0];
                if KNOWN_PROPS.contains(&s) {
                    apply_prop(by_name.entry(None).or_default(), s, v);
                } else {
                    let r = by_name.entry(Some(s.to_string())).or_default();
                    r.name = Some(s.to_string());
                    r.hostname = v.clone();
                }
            }
            2 => {
                let name = segments[0];
                let prop = segments[1];
                if KNOWN_PROPS.contains(&name) {
                    continue;
                }
                let r = by_name.entry(Some(name.to_string())).or_default();
                r.name = Some(name.to_string());
                apply_prop(r, prop, v);
            }
            _ => continue,
        }
    }

    let mut out: Vec<LabelRule> = by_name
        .into_values()
        .filter(|r| !r.hostname.is_empty())
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn apply_prop(rule: &mut LabelRule, prop: &str, value: &str) {
    match prop {
        "port" => rule.port = value.parse().ok(),
        "tls" => rule.tls = Some(value.to_string()),
        "health" => rule.health = Some(value.to_string()),
        "adapter" => rule.adapter = Some(value.to_string()),
        "auth" => rule.auth = Some(value.to_string()),
        _ => {}
    }
}
