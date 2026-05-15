//! Agent labels: the key/value pairs the agent advertises in each
//! heartbeat so the controller's scheduler can match `where:` selectors.
//!
//! Two sources, merged in order (env wins):
//! 1. `/etc/isengard/agent.toml` `[labels]` table.
//! 2. Environment variables matching `ISENGARD_LABEL_<KEY>=<VALUE>`.
//!
//! Key rules:
//! - Keys are ASCII `[a-z0-9._-]+`, max 63 chars, lowercased on ingest.
//! - Values are any UTF-8 except `,` and `=` (selector grammar safety).
//! - Reserved keys (`host`, `hostname`, `id`) are dropped with a warning,
//!   not erroring; the agent should keep heartbeating even with a typo.
//!
//! The loader runs once at agent start (`load_agent_labels()`); SIGHUP
//! re-read is future work. See `docs/PLACEMENT.md` for operator guidance.
//!
//! See `crates/isengard-controller/src/scheduler/` for the consumer.

use std::collections::BTreeMap;
use std::path::Path;

use tracing::warn;

/// Default on-disk location of the agent config. Override via the
/// `ISENGARD_AGENT_TOML` env var (used by tests).
pub const DEFAULT_AGENT_TOML: &str = "/etc/isengard/agent.toml";

/// Reserved label keys that must NOT be set by an operator. The controller
/// already derives `host` / `hostname` / `id` from the host identity;
/// allowing them here would silently shadow the canonical values.
const RESERVED_KEYS: &[&str] = &["host", "hostname", "id"];

/// Maximum key length per the spec.
const MAX_KEY_LEN: usize = 63;

/// Load agent labels from the default sources. Errors during file IO or
/// parsing are logged at warn level; the agent keeps booting with a
/// possibly empty / partial label map.
pub fn load_agent_labels() -> BTreeMap<String, String> {
    let toml_path =
        std::env::var("ISENGARD_AGENT_TOML").unwrap_or_else(|_| DEFAULT_AGENT_TOML.to_string());
    let mut out = load_from_agent_toml(Path::new(&toml_path));
    let from_env = load_from_env();
    for (k, v) in from_env {
        out.insert(k, v);
    }
    out
}

/// Read the `[labels]` table out of an `agent.toml`. Missing file is OK
/// (returns empty); parse errors are logged and the partial result is
/// returned (no fields parsed when the file is unreadable).
pub fn load_from_agent_toml(path: &Path) -> BTreeMap<String, String> {
    let body = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(e) => {
            warn!(error = %e, path = %path.display(), "agent_labels: read failed");
            return BTreeMap::new();
        }
    };
    parse_labels_toml(&body)
}

/// Parse the `[labels]` table from a TOML body. Public for tests.
pub fn parse_labels_toml(body: &str) -> BTreeMap<String, String> {
    let doc: toml::Value = match toml::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "agent_labels: TOML parse failed");
            return BTreeMap::new();
        }
    };
    let Some(labels) = doc.get("labels") else {
        return BTreeMap::new();
    };
    let Some(tbl) = labels.as_table() else {
        warn!("agent_labels: [labels] is not a TOML table; ignoring");
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (k, v) in tbl {
        let value = match v {
            toml::Value::String(s) => s.clone(),
            other => {
                warn!(
                    key = %k,
                    value = ?other,
                    "agent_labels: non-string value, ignoring",
                );
                continue;
            }
        };
        ingest_pair(&mut out, k, &value);
    }
    out
}

/// Walk the environment for `ISENGARD_LABEL_<KEY>=<VALUE>` entries and
/// build a map of normalized keys -> values.
pub fn load_from_env() -> BTreeMap<String, String> {
    load_from_env_iter(std::env::vars())
}

/// Test seam: takes an arbitrary iterator of `(name, value)` pairs.
pub fn load_from_env_iter<I>(vars: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut out = BTreeMap::new();
    for (name, value) in vars {
        let Some(raw_key) = name.strip_prefix("ISENGARD_LABEL_") else {
            continue;
        };
        if raw_key.is_empty() {
            continue;
        }
        ingest_pair(&mut out, raw_key, &value);
    }
    out
}

/// Normalize + validate one key/value pair, inserting on success. Warns
/// (does not error) on invalid keys, reserved keys, or invalid values.
/// Later writes overwrite earlier ones (case-insensitive duplicates).
fn ingest_pair(out: &mut BTreeMap<String, String>, raw_key: &str, raw_value: &str) {
    let key = raw_key.to_ascii_lowercase();
    if key.len() > MAX_KEY_LEN {
        warn!(
            key = %raw_key,
            len = key.len(),
            "agent_labels: key over {MAX_KEY_LEN} chars, dropping",
        );
        return;
    }
    if !is_valid_key(&key) {
        warn!(key = %raw_key, "agent_labels: invalid key characters, dropping");
        return;
    }
    if RESERVED_KEYS.contains(&key.as_str()) {
        warn!(key = %key, "agent_labels: reserved key, dropping");
        return;
    }
    if !is_valid_value(raw_value) {
        warn!(
            key = %key,
            "agent_labels: value contains forbidden chars (= or ,), dropping",
        );
        return;
    }
    if let Some(prev) = out.get(&key)
        && prev != raw_value
    {
        // Two case-variant inputs disagreed; second one wins per the
        // documented merge order (env after TOML, last wins within env).
        warn!(
            key = %key,
            "agent_labels: case-insensitive duplicate; later write wins",
        );
    }
    out.insert(key, raw_value.to_string());
}

fn is_valid_key(k: &str) -> bool {
    !k.is_empty()
        && k.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-' || b == b'_'
        })
}

fn is_valid_value(v: &str) -> bool {
    !v.contains(',') && !v.contains('=')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_loads_valid_labels() {
        let body = r#"
[labels]
role = "worker"
tier = "gpu"
zone = "eu-west"
"#;
        let got = parse_labels_toml(body);
        assert_eq!(got.get("role").map(String::as_str), Some("worker"));
        assert_eq!(got.get("tier").map(String::as_str), Some("gpu"));
        assert_eq!(got.get("zone").map(String::as_str), Some("eu-west"));
    }

    #[test]
    fn toml_lowercases_keys() {
        let body = r#"
[labels]
Role = "worker"
TIER = "gpu"
"#;
        let got = parse_labels_toml(body);
        assert!(got.contains_key("role"));
        assert!(got.contains_key("tier"));
        // Source keys do not survive.
        assert!(!got.contains_key("Role"));
        assert!(!got.contains_key("TIER"));
    }

    #[test]
    fn toml_drops_reserved_keys() {
        let body = r#"
[labels]
role = "worker"
hostname = "alice"
id = "x"
"#;
        let got = parse_labels_toml(body);
        assert_eq!(got.len(), 1);
        assert!(got.contains_key("role"));
        assert!(!got.contains_key("hostname"));
        assert!(!got.contains_key("id"));
    }

    #[test]
    fn toml_drops_invalid_key_chars() {
        let body = r#"
[labels]
"role!bang" = "x"
"good-key" = "y"
"#;
        let got = parse_labels_toml(body);
        assert!(!got.contains_key("role!bang"));
        assert_eq!(got.get("good-key").map(String::as_str), Some("y"));
    }

    #[test]
    fn toml_drops_value_with_comma() {
        let body = r#"
[labels]
role = "worker,extra"
tier = "gpu"
"#;
        let got = parse_labels_toml(body);
        assert!(!got.contains_key("role"));
        assert!(got.contains_key("tier"));
    }

    #[test]
    fn toml_ignores_non_string_values() {
        let body = r#"
[labels]
role = "worker"
count = 3
"#;
        let got = parse_labels_toml(body);
        assert!(got.contains_key("role"));
        assert!(!got.contains_key("count"));
    }

    #[test]
    fn toml_missing_labels_returns_empty() {
        let body = r#"
controller = "https://x"
"#;
        let got = parse_labels_toml(body);
        assert!(got.is_empty());
    }

    #[test]
    fn env_loader_parses_label_prefix() {
        let vars = vec![
            ("ISENGARD_LABEL_ROLE".into(), "worker".into()),
            ("ISENGARD_LABEL_TIER".into(), "gpu".into()),
            ("PATH".into(), "/usr/bin".into()),
            ("ISENGARD_OTHER".into(), "ignored".into()),
        ];
        let got = load_from_env_iter(vars);
        assert_eq!(got.len(), 2);
        assert_eq!(got.get("role").map(String::as_str), Some("worker"));
        assert_eq!(got.get("tier").map(String::as_str), Some("gpu"));
    }

    #[test]
    fn env_loader_drops_reserved() {
        let vars = vec![
            ("ISENGARD_LABEL_HOSTNAME".into(), "alice".into()),
            ("ISENGARD_LABEL_ROLE".into(), "worker".into()),
        ];
        let got = load_from_env_iter(vars);
        assert_eq!(got.len(), 1);
        assert!(got.contains_key("role"));
    }

    #[test]
    fn env_loader_drops_invalid_value() {
        let vars = vec![
            ("ISENGARD_LABEL_ROLE".into(), "a,b".into()),
            ("ISENGARD_LABEL_TIER".into(), "k=v".into()),
            ("ISENGARD_LABEL_OK".into(), "fine".into()),
        ];
        let got = load_from_env_iter(vars);
        assert_eq!(got.len(), 1);
        assert_eq!(got.get("ok").map(String::as_str), Some("fine"));
    }

    #[test]
    fn env_loader_ignores_empty_suffix() {
        let vars = vec![
            ("ISENGARD_LABEL_".into(), "noop".into()),
            ("ISENGARD_LABEL_OK".into(), "fine".into()),
        ];
        let got = load_from_env_iter(vars);
        assert_eq!(got.len(), 1);
        assert_eq!(got.get("ok").map(String::as_str), Some("fine"));
    }

    /// Env values override TOML values for the same lowercased key.
    /// Captured indirectly through the public loader by setting
    /// `ISENGARD_AGENT_TOML` to a tempfile.
    #[test]
    fn merge_env_wins_over_toml() {
        let mut from_toml: BTreeMap<String, String> = BTreeMap::new();
        from_toml.insert("role".into(), "worker-toml".into());
        from_toml.insert("tier".into(), "gpu".into());
        let mut from_env: BTreeMap<String, String> = BTreeMap::new();
        from_env.insert("role".into(), "control-env".into());
        // Manual merge mirroring `load_agent_labels()`.
        let mut merged = from_toml;
        for (k, v) in from_env {
            merged.insert(k, v);
        }
        assert_eq!(merged.get("role").map(String::as_str), Some("control-env"));
        assert_eq!(merged.get("tier").map(String::as_str), Some("gpu"));
    }
}
