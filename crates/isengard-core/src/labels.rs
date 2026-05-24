//! Parse `isengard.expose.*` Docker labels into structured rules.
//!
//! Rules:
//! - `isengard.expose = <hostname>` is the default unnamed rule.
//! - `isengard.expose.<prop> = <value>` where `<prop>` is one of
//!   `port`, `tls`, `health`, `adapter`, `auth` sets a property on the
//!   default rule.
//! - `isengard.expose.<name> = <hostname>` where `<name>` is not a
//!   reserved prop name creates a named rule.
//! - `isengard.expose.<name>.<prop> = <value>` sets a property on the
//!   named rule.

use std::collections::HashMap;

/// Reserved second-segment names that resolve to properties of the
/// default rule, not the names of new rules.
const KNOWN_PROPS: &[&str] = &["port", "tls", "health", "adapter", "auth"];

/// One parsed routing rule extracted from `isengard.expose.*` labels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelRule {
    /// Rule name. `None` is the default unnamed rule keyed off the bare
    /// `isengard.expose` label.
    pub name: Option<String>,
    /// Hostname the rule routes to. Required; rules with no hostname are
    /// dropped from the output of [`parse_labels`].
    pub hostname: String,
    /// Upstream port override. `None` means the agent must infer the port
    /// from local runtime data before the controller inserts a route.
    pub port: Option<u16>,
    /// TLS strategy keyword (e.g. `"edge"`, `"passthrough"`).
    pub tls: Option<String>,
    /// Healthcheck path or URL.
    pub health: Option<String>,
    /// Adapter id (e.g. `"cf-tunnel"`, `"tailscale"`).
    pub adapter: Option<String>,
    /// Auth strategy keyword (e.g. `"cf-access"`).
    pub auth: Option<String>,
}

/// Extract the `isengard.expose.*` subset of a Docker label map into a
/// sorted [`LabelRule`] list.
///
/// Rules with no hostname are dropped. Output is sorted by `name`
/// (unnamed default rule first) for stable diffs.
///
/// # Examples
///
/// ```
/// use isengard_core::labels::parse_labels;
/// use std::collections::HashMap;
///
/// let mut labels = HashMap::new();
/// labels.insert("isengard.expose".into(), "blog.example.com".into());
/// labels.insert("isengard.expose.port".into(), "8080".into());
///
/// let rules = parse_labels(&labels);
/// assert_eq!(rules.len(), 1);
/// assert_eq!(rules[0].hostname, "blog.example.com");
/// assert_eq!(rules[0].port, Some(8080));
/// ```
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

/// Set a single property on `rule` keyed by the reserved prop name.
///
/// Unknown prop names are silently ignored; the parser elsewhere has
/// already guaranteed `prop` is in [`KNOWN_PROPS`]. A `port` value that
/// fails to parse as a `u16` is dropped.
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
