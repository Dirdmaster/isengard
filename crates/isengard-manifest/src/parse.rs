//! TOML deserialization + post-parse validation for `stack.toml` and
//! `isengard.toml`. Uses `serde` for shape, then runs validators on top.

use crate::{
    FleetManifest, HookErrorPolicy, HookEvent, HookSpec, OverlaySpec, StackManifest, Strategy,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

/// Errors raised when a manifest body fails parsing or validation.
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid manifest path {0:?} (no parent directory)")]
    InvalidPath(PathBuf),
    #[error("toml: {0}")]
    Toml(String),
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    #[error("compose list must be non-empty")]
    EmptyCompose,
    #[error("compose path {path:?} is absolute; manifest paths must be relative")]
    AbsoluteComposePath { path: PathBuf },
    #[error("unknown strategy {0:?}; allowed: auto, blue-green, rolling, recreate")]
    UnknownStrategy(String),
    #[error("unknown hook event {0:?}; allowed: pre-deploy, post-deploy, failure")]
    UnknownHookEvent(String),
    #[error("unknown overlay {0:?}")]
    UnknownOverlay(String),
    #[error("hook timeout {0:?} could not be parsed: {1}")]
    HookTimeout(String, String),
    #[error("hook on_error {0:?} must be `abort` or `continue`")]
    UnknownHookErrorPolicy(String),
    #[error("hook cmd cannot be empty")]
    EmptyHookCmd,
    #[error("serialize: {0}")]
    Serialize(String),
}

#[derive(Debug, Deserialize)]
struct RawStack {
    name: Option<String>,
    fleet: Option<String>,
    compose: Option<Vec<PathBuf>>,
    strategy: Option<String>,
    #[serde(default)]
    secrets: Vec<String>,
    #[serde(default)]
    hooks: Vec<RawHook>,
    #[serde(default)]
    overlays: BTreeMap<String, RawOverlay>,
}

#[derive(Debug, Deserialize)]
struct RawOverlay {
    compose: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct RawHook {
    on: String,
    cmd: Vec<String>,
    timeout: Option<String>,
    on_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawFleet {
    fleet: Option<String>,
    context: Option<String>,
}

pub fn parse_stack_manifest(text: &str, root: PathBuf) -> Result<StackManifest, ManifestError> {
    let raw: RawStack = toml::from_str(text).map_err(|e| ManifestError::Toml(e.to_string()))?;
    let name = raw
        .name
        .filter(|s| !s.trim().is_empty())
        .ok_or(ManifestError::MissingField("name"))?;

    let compose = raw.compose.ok_or(ManifestError::MissingField("compose"))?;
    if compose.is_empty() {
        return Err(ManifestError::EmptyCompose);
    }
    for p in &compose {
        if p.is_absolute() {
            return Err(ManifestError::AbsoluteComposePath { path: p.clone() });
        }
    }

    let strategy = match raw.strategy.as_deref() {
        None => Strategy::Auto,
        Some("auto") => Strategy::Auto,
        Some("blue-green") => Strategy::BlueGreen,
        Some("rolling") => Strategy::Rolling,
        Some("recreate") => Strategy::Recreate,
        Some(other) => return Err(ManifestError::UnknownStrategy(other.to_string())),
    };

    let mut hooks = Vec::with_capacity(raw.hooks.len());
    for h in raw.hooks {
        hooks.push(parse_hook(h)?);
    }

    let mut overlays = BTreeMap::new();
    for (name, raw_overlay) in raw.overlays {
        for p in &raw_overlay.compose {
            if p.is_absolute() {
                return Err(ManifestError::AbsoluteComposePath { path: p.clone() });
            }
        }
        overlays.insert(
            name,
            OverlaySpec {
                compose: raw_overlay.compose,
            },
        );
    }

    Ok(StackManifest {
        name,
        fleet: raw.fleet,
        compose,
        overlays,
        strategy,
        secrets: raw.secrets,
        hooks,
        root,
    })
}

fn parse_hook(raw: RawHook) -> Result<HookSpec, ManifestError> {
    let on = match raw.on.as_str() {
        "pre-deploy" => HookEvent::PreDeploy,
        "post-deploy" => HookEvent::PostDeploy,
        "failure" => HookEvent::Failure,
        other => return Err(ManifestError::UnknownHookEvent(other.to_string())),
    };
    if raw.cmd.is_empty() {
        return Err(ManifestError::EmptyHookCmd);
    }
    let timeout = match raw.timeout.as_deref() {
        Some(s) => parse_duration(s)?,
        None => Duration::from_secs(60),
    };
    let on_error = match raw.on_error.as_deref() {
        None => HookErrorPolicy::Abort,
        Some("abort") => HookErrorPolicy::Abort,
        Some("continue") => HookErrorPolicy::Continue,
        Some(other) => return Err(ManifestError::UnknownHookErrorPolicy(other.to_string())),
    };
    Ok(HookSpec {
        on,
        cmd: raw.cmd,
        timeout,
        on_error,
    })
}

/// Tiny duration parser: accepts "<n>s", "<n>ms", "<n>m", "<n>h", or a
/// plain integer (interpreted as seconds). Designed to cover the handful
/// of shapes the spec calls out ("120s", "60000ms"). No external dep
/// because we want a single source of truth for the formats we accept.
fn parse_duration(s: &str) -> Result<Duration, ManifestError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(ManifestError::HookTimeout(
            s.to_string(),
            "empty string".into(),
        ));
    }
    // Match the longest suffix first so "ms" beats "s".
    let (num_str, mult_ms): (&str, u64) = if let Some(prefix) = trimmed.strip_suffix("ms") {
        (prefix, 1)
    } else if let Some(prefix) = trimmed.strip_suffix('s') {
        (prefix, 1_000)
    } else if let Some(prefix) = trimmed.strip_suffix('m') {
        (prefix, 60_000)
    } else if let Some(prefix) = trimmed.strip_suffix('h') {
        (prefix, 3_600_000)
    } else {
        (trimmed, 1_000)
    };
    let n: u64 = num_str
        .trim()
        .parse()
        .map_err(|e: std::num::ParseIntError| {
            ManifestError::HookTimeout(s.to_string(), e.to_string())
        })?;
    Ok(Duration::from_millis(n.saturating_mul(mult_ms)))
}

pub fn parse_fleet_manifest(text: &str) -> Result<FleetManifest, ManifestError> {
    let raw: RawFleet = toml::from_str(text).map_err(|e| ManifestError::Toml(e.to_string()))?;
    Ok(FleetManifest {
        fleet: raw.fleet,
        context: raw.context,
    })
}

/// Serialize a `StackManifest` back to TOML for `to_toml_string`.
/// Round-trips through `parse_stack_manifest`. `root` is NOT serialized
/// (the manifest path implies it on disk).
pub fn serialize_stack_manifest(m: &StackManifest) -> Result<String, ManifestError> {
    // Build a serde_json::Value tree so we get deterministic ordering
    // without writing a giant `#[derive(Serialize)]` wrapper. The TOML
    // crate accepts a `toml::Value::Table` directly.
    let mut root = toml::map::Map::new();
    root.insert("name".into(), toml::Value::String(m.name.clone()));
    if let Some(f) = &m.fleet {
        root.insert("fleet".into(), toml::Value::String(f.clone()));
    }
    let compose: Vec<toml::Value> = m
        .compose
        .iter()
        .map(|p| toml::Value::String(p.to_string_lossy().into_owned()))
        .collect();
    root.insert("compose".into(), toml::Value::Array(compose));
    if !matches!(m.strategy, Strategy::Auto) {
        root.insert(
            "strategy".into(),
            toml::Value::String(m.strategy.as_str().into()),
        );
    }
    if !m.secrets.is_empty() {
        let secrets: Vec<toml::Value> = m
            .secrets
            .iter()
            .map(|s| toml::Value::String(s.clone()))
            .collect();
        root.insert("secrets".into(), toml::Value::Array(secrets));
    }

    if !m.hooks.is_empty() {
        let mut hooks_arr = Vec::new();
        for h in &m.hooks {
            let mut hook = toml::map::Map::new();
            hook.insert("on".into(), toml::Value::String(h.on.as_str().into()));
            let cmd: Vec<toml::Value> = h
                .cmd
                .iter()
                .map(|s| toml::Value::String(s.clone()))
                .collect();
            hook.insert("cmd".into(), toml::Value::Array(cmd));
            let timeout_ms = h.timeout.as_millis();
            // Prefer "Ns" when divisible by 1000, otherwise "Nms".
            let timeout_str = if timeout_ms % 1000 == 0 {
                format!("{}s", timeout_ms / 1000)
            } else {
                format!("{}ms", timeout_ms)
            };
            hook.insert("timeout".into(), toml::Value::String(timeout_str));
            if !matches!(h.on_error, HookErrorPolicy::Abort) {
                hook.insert(
                    "on_error".into(),
                    toml::Value::String(h.on_error.as_str().into()),
                );
            }
            hooks_arr.push(toml::Value::Table(hook));
        }
        root.insert("hooks".into(), toml::Value::Array(hooks_arr));
    }

    if !m.overlays.is_empty() {
        let mut overlays = toml::map::Map::new();
        for (name, spec) in &m.overlays {
            let mut o = toml::map::Map::new();
            let compose: Vec<toml::Value> = spec
                .compose
                .iter()
                .map(|p| toml::Value::String(p.to_string_lossy().into_owned()))
                .collect();
            o.insert("compose".into(), toml::Value::Array(compose));
            overlays.insert(name.clone(), toml::Value::Table(o));
        }
        root.insert("overlays".into(), toml::Value::Table(overlays));
    }

    toml::to_string_pretty(&toml::Value::Table(root))
        .map_err(|e| ManifestError::Serialize(e.to_string()))
}
