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
    /// Upstream port. `None` falls through to the adapter's default.
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
