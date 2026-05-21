//! TOML deserialization + post-parse validation for `stack.toml` and
//! `isengard.toml`.
//!
//! Two passes: `serde` decodes the TOML into a `Raw*` shape that mirrors
//! the file format. Then validators promote the raw shape to the
//! public types ([`StackManifest`], [`FleetManifest`]) and reject
//! anything that violates the schema rules (missing `name`, empty
//! `compose`, absolute paths, unknown strategy keywords).
//!
//! Serialization is a single pass via [`serialize_stack_manifest`],
//! which writes a `toml::Value::Table` to keep field ordering
//! deterministic without introducing a parallel `#[derive(Serialize)]`
//! shape.

use crate::{
    FleetManifest, HookErrorPolicy, HookEvent, HookSpec, OverlaySpec, StackManifest, Strategy,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

/// Every way a manifest body can fail to load.
///
/// The error variants name the specific schema rule that failed; the CLI
/// surfaces them verbatim. Each variant carries enough context (path,
/// offending value) for the operator to fix the input without grepping
/// the source.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// Reading the file from disk failed.
    #[error("read {path}: {source}")]
    Io {
        /// File path the I/O call targeted.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The manifest path can't be turned into a parent directory (e.g.
    /// it points at the filesystem root). Blocks [`StackManifest::load`]
    /// from resolving [`StackManifest::root`].
    #[error("invalid manifest path {0:?} (no parent directory)")]
    InvalidPath(PathBuf),
    /// TOML syntax error from `toml::de::Error`. `span` carries the
    /// byte range when the underlying error reported one (most syntax
    /// errors do; some structural validation errors do not). Editors
    /// consuming this via the LSP map the span to a `Range` and emit a
    /// `Diagnostic`.
    #[error("toml: {message}")]
    Toml {
        /// The `toml::de::Error::message()` rendering. Kept narrow so it
        /// fits in an LSP diagnostic without flooding the operator.
        message: String,
        /// Byte range of the offending TOML span when the underlying
        /// parser provided one. `None` for structural validation errors
        /// (missing fields, bad enum values) that don't map to a single
        /// region of the source text.
        span: Option<std::ops::Range<usize>>,
    },
    /// A required field is absent or empty. Carries the field name.
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    /// `compose = []` was declared, which would deploy nothing.
    #[error("compose list must be non-empty")]
    EmptyCompose,
    /// A `compose` entry (base or overlay) is absolute. Manifest paths
    /// must be relative so the manifest stays portable across hosts.
    #[error("compose path {path:?} is absolute; manifest paths must be relative")]
    AbsoluteComposePath {
        /// The offending absolute path.
        path: PathBuf,
    },
    /// `strategy = "..."` named a value outside the known set.
    #[error("unknown strategy {0:?}; allowed: auto, blue-green, rolling, recreate")]
    UnknownStrategy(String),
    /// `[[hooks]] on = "..."` named a value outside the known set.
    #[error("unknown hook event {0:?}; allowed: pre-deploy, post-deploy, failure")]
    UnknownHookEvent(String),
    /// [`StackManifest::resolved_compose_paths`] was asked for an
    /// overlay name that isn't declared in [`StackManifest::overlays`].
    #[error("unknown overlay {0:?}")]
    UnknownOverlay(String),
    /// A `[[hooks]] timeout = "..."` value couldn't be parsed.
    ///
    /// First field is the raw input. Second field is the underlying
    /// reason ("empty string", a `ParseIntError` message, etc.).
    #[error("hook timeout {0:?} could not be parsed: {1}")]
    HookTimeout(String, String),
    /// `[[hooks]] on_error = "..."` named a value other than `abort`
    /// or `continue`.
    #[error("hook on_error {0:?} must be `abort` or `continue`")]
    UnknownHookErrorPolicy(String),
    /// A hook declared no command. Hooks must run something.
    #[error("hook cmd cannot be empty")]
    EmptyHookCmd,
    /// TOML serialization (via [`StackManifest::to_toml_string`])
    /// failed.
    #[error("serialize: {0}")]
    Serialize(String),
}

/// Pre-validation shape matching `stack.toml`. Fields are `Option` so
/// the validator can produce structured [`ManifestError::MissingField`]
/// errors instead of opaque serde errors.
#[derive(Debug, Deserialize)]
struct RawStack {
    /// Stack name. Required after validation.
    name: Option<String>,
    /// Fleet binding.
    fleet: Option<String>,
    /// Compose file list. Required and non-empty after validation.
    compose: Option<Vec<PathBuf>>,
    /// Deploy strategy keyword.
    strategy: Option<String>,
    /// Per-stack secret names. Defaults to empty.
    #[serde(default)]
    secrets: Vec<String>,
    /// Lifecycle hooks. Defaults to empty.
    #[serde(default)]
    hooks: Vec<RawHook>,
    /// Named overlay blocks keyed by overlay name. Defaults to empty.
    #[serde(default)]
    overlays: BTreeMap<String, RawOverlay>,
}

/// Pre-validation shape for an `[overlays.<name>]` block.
#[derive(Debug, Deserialize)]
struct RawOverlay {
    /// Extra compose files this overlay adds, in declared order.
    compose: Vec<PathBuf>,
}

/// Pre-validation shape for a `[[hooks]]` entry.
#[derive(Debug, Deserialize)]
struct RawHook {
    /// Lifecycle event keyword. Validated against [`HookEvent`].
    on: String,
    /// Argv to run. Must be non-empty after validation.
    cmd: Vec<String>,
    /// Duration string ("120s", "60000ms"). Defaults to 60 seconds when
    /// absent.
    timeout: Option<String>,
    /// Error policy keyword. Validated against [`HookErrorPolicy`].
    /// Defaults to `abort` when absent.
    on_error: Option<String>,
}

/// Pre-validation shape for `isengard.toml`.
#[derive(Debug, Deserialize)]
struct RawFleet {
    /// Default fleet name.
    fleet: Option<String>,
    /// Default docker / controller context name.
    context: Option<String>,
}

/// Parses a `stack.toml` body into a validated [`StackManifest`].
///
/// `root` becomes [`StackManifest::root`] verbatim. The caller is
/// responsible for picking it (parent directory on disk, synthetic
/// `/etc/isengard/...` on the controller side).
///
/// # Errors
///
/// Returns the full set of validation errors in [`ManifestError`]:
/// missing `name` / `compose`, empty compose list, absolute compose
/// paths, unknown strategy / hook / on-error keywords.
pub fn parse_stack_manifest(text: &str, root: PathBuf) -> Result<StackManifest, ManifestError> {
    let raw: RawStack = toml::from_str(text).map_err(|e| ManifestError::Toml {
        message: e.message().to_string(),
        span: e.span(),
    })?;
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

/// Promotes a [`RawHook`] to a validated [`HookSpec`].
///
/// Validates the event keyword, rejects empty `cmd`, parses the timeout
/// string (defaulting to 60 seconds), and validates the error policy
/// keyword (defaulting to `abort`).
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

/// Parses one of the small duration shapes the spec accepts.
///
/// Accepts `<n>s`, `<n>ms`, `<n>m`, `<n>h`, or a plain integer
/// interpreted as seconds. Designed to cover the handful of shapes the
/// spec calls out (`120s`, `60000ms`). No external dep: one source of
/// truth for the formats this crate accepts.
///
/// Suffix matching is longest-first so `ms` beats `s`.
///
/// # Errors
///
/// Returns [`ManifestError::HookTimeout`] for an empty string or a
/// numeric component that doesn't parse as `u64`.
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

/// Parses an `isengard.toml` body into a [`FleetManifest`].
///
/// Both fields are optional. An empty body returns a manifest with no
/// fleet and no context.
///
/// # Errors
///
/// Returns [`ManifestError::Toml`] when the TOML parser rejects the
/// body.
pub fn parse_fleet_manifest(text: &str) -> Result<FleetManifest, ManifestError> {
    let raw: RawFleet = toml::from_str(text).map_err(|e| ManifestError::Toml {
        message: e.message().to_string(),
        span: e.span(),
    })?;
    Ok(FleetManifest {
        fleet: raw.fleet,
        context: raw.context,
    })
}

/// Serializes a [`StackManifest`] back to TOML for
/// [`StackManifest::to_toml_string`].
///
/// Round-trips through [`parse_stack_manifest`]. Builds the output as a
/// `toml::Value::Table` so field ordering stays deterministic across
/// runs. [`StackManifest::root`] is omitted: the manifest's location on
/// disk implies it.
///
/// # Errors
///
/// Returns [`ManifestError::Serialize`] when the TOML writer fails.
pub fn serialize_stack_manifest(m: &StackManifest) -> Result<String, ManifestError> {
    // Build a toml::Value::Table tree so we get deterministic ordering
    // without writing a giant `#[derive(Serialize)]` wrapper.
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
