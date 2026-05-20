#![doc = include_str!("../docs/compose-overlay.md")]

use serde_yaml::{Mapping, Value};
use thiserror::Error;

/// Failure modes for [`merge_compose_yaml`].
///
/// Bad YAML on either side of the merge raises [`MergeError::Yaml`].
/// An entirely empty input set raises [`MergeError::EmptyBase`].
#[derive(Debug, Error)]
pub enum MergeError {
    /// `serde_yaml` couldn't parse one of the inputs (or couldn't
    /// re-serialize the merged tree).
    #[error("yaml parse: {0}")]
    Yaml(String),
    /// `base` was empty and no overlay supplied anything either. The
    /// merger needs at least one non-empty document to produce output.
    #[error("base compose is empty")]
    EmptyBase,
}

/// Merges `base` against each overlay in order, returning the merged YAML
/// text.
///
/// Pure transformation: no I/O. Overlays apply in the order given. The
/// merge rules are documented at the module level.
///
/// An empty entry in `overlays` is skipped. A null top-level value in
/// `base` is treated as an empty mapping so the first overlay populates
/// it.
///
/// # Errors
///
/// Returns [`MergeError::EmptyBase`] when `base` and every overlay are
/// blank. Returns [`MergeError::Yaml`] when any document fails to parse
/// or the merged tree fails to re-serialize.
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

/// Recursively merges `src` into `dst` following the compose overlay
/// rules.
///
/// Map + map recurses through [`merge_mapping`]. Sequence + sequence
/// uses [`append_dedupe`] (the top-level shouldn't hit this since
/// compose is a mapping, but the path is correct for any subtree the
/// caller hands in). Any other combination is last-write-wins.
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

/// Merges every entry of `src` into `dst`.
///
/// Keys in [`LIST_FIELDS`] use [`merge_list_field`] (append + dedupe).
/// Other keys recurse through [`merge_value`]. Missing keys are inserted
/// from `src` verbatim.
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

/// Merges a list-shaped compose field.
///
/// Two same-shape sequences go through [`append_dedupe`]. Two mappings
/// (the alternate shape for `environment`) merge per-key with
/// last-write-wins, matching the "dedupe by key" semantic. Any other
/// combination is last-write-wins.
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

/// Appends every entry of `src` into `dst`, replacing any entry whose
/// dedupe key already exists.
///
/// Replace-in-place lets overlay values win over base values for the
/// same key: `["K=v1"]` + `["K=v2"]` becomes `["K=v2"]`.
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

/// Computes the dedupe key for a single sequence entry.
///
/// Strings dedupe on their first component (see [`first_component`]).
/// Maps dedupe on the first present field of `name` / `source` /
/// `target` / `published`, covering the common compose shapes for
/// volumes and ports. Anything else falls back to its serialized form.
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

/// Returns the first component of a `K=V` or `k:v` string.
///
/// Lets the merger dedupe environment + volume entries that update an
/// existing key. Strings with neither separator dedupe against
/// themselves.
fn first_component(s: &str) -> String {
    if let Some(idx) = s.find('=') {
        s[..idx].to_string()
    } else if let Some(idx) = s.find(':') {
        s[..idx].to_string()
    } else {
        s.to_string()
    }
}
