//! Compose YAML overlay merging.
//!
//! `stack.toml` declares one or more compose files to ship as a single
//! merged document. The base file is the first entry; subsequent entries
//! (plus any selected overlay) merge on top of it with these rules:
//!
//! - Scalar fields: last-write-wins.
//! - List fields by name (volumes, environment, ports, depends_on,
//!   cap_add, cap_drop, networks, expose, labels, profiles, dns):
//!   append-and-dedupe.
//! - Dedupe key for "k:v" / "key=value" strings: the first component.
//! - Nested maps (deploy, healthcheck, logging): recurse with same
//!   rules.
//!
//! The merge is done over `serde_yaml::Value` trees rather than over the
//! existing controller-side `DesiredCompose` struct to keep the manifest
//! crate dep-light: it only needs `serde_yaml`, not the entire reconciler.

use serde_yaml::{Mapping, Value};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MergeError {
    #[error("yaml parse: {0}")]
    Yaml(String),
    #[error("base compose is empty")]
    EmptyBase,
}

/// Merge `base` against each `overlay` in order, returning the resulting
/// YAML text. Pure transformation; no I/O.
pub fn merge_compose_yaml(base: &str, overlays: &[String]) -> Result<String, MergeError> {
    if base.trim().is_empty() && overlays.iter().all(|s| s.trim().is_empty()) {
        return Err(MergeError::EmptyBase);
    }
    let mut merged: Value =
        serde_yaml::from_str(base).map_err(|e| MergeError::Yaml(e.to_string()))?;
    if merged.is_null() {
        merged = Value::Mapping(Mapping::new());
    }
    for overlay in overlays {
        if overlay.trim().is_empty() {
            continue;
        }
        let next: Value =
            serde_yaml::from_str(overlay).map_err(|e| MergeError::Yaml(e.to_string()))?;
        merge_value(&mut merged, next);
    }
    serde_yaml::to_string(&merged).map_err(|e| MergeError::Yaml(e.to_string()))
}

fn merge_value(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Mapping(d), Value::Mapping(s)) => merge_mapping(d, s),
        (Value::Sequence(d), Value::Sequence(s)) => {
            // At the top level the caller usually doesn't hit a raw
            // sequence (compose is a mapping), but if someone overlays
            // a sequence we follow append-and-dedupe by string.
            append_dedupe(d, s);
        }
        (dst_slot, src_val) => {
            *dst_slot = src_val;
        }
    }
}

fn merge_mapping(dst: &mut Mapping, src: Mapping) {
    for (key, val) in src {
        let merge_as_list = matches!(
            key.as_str(),
            Some(
                "volumes"
                    | "environment"
                    | "ports"
                    | "depends_on"
                    | "cap_add"
                    | "cap_drop"
                    | "networks"
                    | "expose"
                    | "labels"
                    | "profiles"
                    | "dns"
                    | "extra_hosts"
                    | "external_links"
                    | "links"
                    | "tmpfs"
                    | "security_opt"
                    | "device_cgroup_rules"
                    | "devices"
                    | "sysctls"
            )
        );
        match dst.get_mut(&key) {
            Some(existing) => {
                if merge_as_list {
                    merge_list_field(existing, val);
                } else {
                    merge_value(existing, val);
                }
            }
            None => {
                dst.insert(key, val);
            }
        }
    }
}

fn merge_list_field(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Sequence(d), Value::Sequence(s)) => append_dedupe(d, s),
        (Value::Mapping(d), Value::Mapping(s)) => {
            // environment can also be expressed as a map. Last-write-wins
            // per-key matches the "key" dedupe semantic.
            for (k, v) in s {
                d.insert(k, v);
            }
        }
        (dst_slot, src_val) => {
            *dst_slot = src_val;
        }
    }
}

fn append_dedupe(dst: &mut Vec<Value>, src: Vec<Value>) {
    for item in src {
        let key = dedupe_key(&item);
        if let Some(pos) = dst.iter().position(|existing| dedupe_key(existing) == key) {
            // Replace in place so overlay wins for "K=v1" -> "K=v2".
            dst[pos] = item;
        } else {
            dst.push(item);
        }
    }
}

fn dedupe_key(v: &Value) -> String {
    match v {
        Value::String(s) => first_component(s),
        Value::Mapping(m) => {
            // Common compose shapes: `{ source: ..., target: ..., type: ... }`
            // for volumes; `{ target: 80, published: 8080 }` for ports.
            // Use `source` / `target` / `published` / `name` if present,
            // otherwise fall back to a serialized form.
            for key in ["name", "source", "target", "published"] {
                if let Some(val) = m.get(Value::String(key.into()))
                    && let Some(s) = val.as_str()
                {
                    return format!("{key}={s}");
                }
            }
            serde_yaml::to_string(v).unwrap_or_default()
        }
        _ => serde_yaml::to_string(v).unwrap_or_default(),
    }
}

/// Return the first component of a "K=V" or "k:v" string. Used to dedupe
/// environment + volume entries that update an existing key.
fn first_component(s: &str) -> String {
    if let Some(idx) = s.find('=') {
        s[..idx].to_string()
    } else if let Some(idx) = s.find(':') {
        s[..idx].to_string()
    } else {
        s.to_string()
    }
}
