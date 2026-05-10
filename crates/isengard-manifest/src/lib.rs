//! `stack.toml` and `isengard.toml` manifest parser for Phase 0.13.
//!
//! `stack.toml` lives next to a stack's compose file(s) and carries the
//! orchestration metadata that compose itself can't express: stack name,
//! fleet binding, secret name list, deploy strategy, lifecycle hooks.
//!
//! `isengard.toml` is a fleet-level sibling at the repo root with a
//! default fleet + default context. Optional.
//!
//! The parser is `serde + toml` only; no I/O surface except `load()` and
//! `load_from_walk`. Consumers in `isd` and the dashboard hand the parsed
//! shapes around as plain Rust values.
//!
//! See `docs/superpowers/specs/2026-05-11-phase-0-13-stack-toml-manifest-design.md`
//! for the schema rationale.

#![forbid(unsafe_code)]

mod overlay;
mod parse;

pub use overlay::{MergeError, merge_compose_yaml};
pub use parse::{ManifestError, parse_fleet_manifest, parse_stack_manifest};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The parsed `stack.toml`, fully validated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackManifest {
    /// Stack identity. Required.
    pub name: String,
    /// Fleet binding. Optional.
    pub fleet: Option<String>,
    /// Compose file list (relative to `root`). Required, non-empty.
    pub compose: Vec<PathBuf>,
    /// Named overlays selected at deploy time.
    pub overlays: BTreeMap<String, OverlaySpec>,
    /// Stack-level deploy strategy. Defaults to [`Strategy::Auto`].
    pub strategy: Strategy,
    /// Per-fleet secret names mounted into every service in the stack.
    pub secrets: Vec<String>,
    /// Lifecycle hooks (pre-deploy, post-deploy, failure).
    pub hooks: Vec<HookSpec>,
    /// Absolute path to the directory holding `stack.toml`. Used for
    /// resolving compose paths and for hook cwd at deploy time.
    pub root: PathBuf,
}

/// Named overlay block (`[overlays.<name>]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlaySpec {
    pub compose: Vec<PathBuf>,
}

/// Deploy strategy: maps to the per-service phase 10g labels at deploy
/// time. The default is [`Strategy::Auto`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    #[default]
    Auto,
    BlueGreen,
    Rolling,
    Recreate,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::BlueGreen => "blue-green",
            Self::Rolling => "rolling",
            Self::Recreate => "recreate",
        }
    }
}

/// Lifecycle hook. Runs on the host (not in any container) at one of three
/// points: pre-deploy, post-deploy, failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSpec {
    pub on: HookEvent,
    pub cmd: Vec<String>,
    pub timeout: Duration,
    pub on_error: HookErrorPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookEvent {
    PreDeploy,
    PostDeploy,
    Failure,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreDeploy => "pre-deploy",
            Self::PostDeploy => "post-deploy",
            Self::Failure => "failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookErrorPolicy {
    #[default]
    Abort,
    Continue,
}

impl HookErrorPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Abort => "abort",
            Self::Continue => "continue",
        }
    }
}

impl StackManifest {
    /// Load + validate a `stack.toml` from disk. `path` is the manifest
    /// file itself (not its parent directory).
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(|e| ManifestError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let root = path
            .parent()
            .ok_or_else(|| ManifestError::InvalidPath(path.to_path_buf()))?
            .to_path_buf();
        parse_stack_manifest(&text, root)
    }

    /// Parse from an in-memory string. Used by the dashboard's API
    /// handler which receives the manifest body verbatim. The caller
    /// provides `root` for hook cwd resolution; on the controller side
    /// this can be a synthetic path (`/etc/isengard/stacks/<name>/`).
    pub fn from_str(text: &str, root: PathBuf) -> Result<Self, ManifestError> {
        parse_stack_manifest(text, root)
    }

    /// Resolve the ordered list of compose paths the deploy should
    /// merge: the base `compose = [...]` list, plus the named overlay
    /// if requested. Returns absolute paths anchored at `root`.
    pub fn resolved_compose_paths(
        &self,
        overlay: Option<&str>,
    ) -> Result<Vec<PathBuf>, ManifestError> {
        let mut out: Vec<PathBuf> = self.compose.iter().map(|p| self.root.join(p)).collect();
        if let Some(name) = overlay {
            let block = self
                .overlays
                .get(name)
                .ok_or_else(|| ManifestError::UnknownOverlay(name.to_string()))?;
            for p in &block.compose {
                out.push(self.root.join(p));
            }
        }
        Ok(out)
    }

    /// Produce a minimal manifest for a legacy compose-only stack.
    /// Operators can drop this into a stack dir and `isd deploy --all`
    /// will pick it up.
    pub fn minimal_for(name: &str) -> Self {
        Self {
            name: name.to_string(),
            fleet: None,
            compose: vec![PathBuf::from("compose.toml")],
            overlays: BTreeMap::new(),
            strategy: Strategy::Auto,
            secrets: Vec::new(),
            hooks: Vec::new(),
            root: PathBuf::new(),
        }
    }

    /// Serialize back to TOML text. Round-trips through `from_str`.
    pub fn to_toml_string(&self) -> Result<String, ManifestError> {
        parse::serialize_stack_manifest(self)
    }
}

/// Optional fleet-level manifest at repo root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetManifest {
    pub fleet: Option<String>,
    pub context: Option<String>,
}

impl FleetManifest {
    /// Walk up from `start` looking for `isengard.toml`. Stops at the
    /// first `.git` directory encountered or the filesystem root,
    /// whichever comes first. Returns `Ok(None)` if absent.
    pub fn load_from_walk(start: &Path) -> Result<Option<Self>, ManifestError> {
        let mut cursor = start.to_path_buf();
        loop {
            let candidate = cursor.join("isengard.toml");
            if candidate.is_file() {
                let text = std::fs::read_to_string(&candidate).map_err(|e| ManifestError::Io {
                    path: candidate.clone(),
                    source: e,
                })?;
                return Ok(Some(parse_fleet_manifest(&text)?));
            }
            if cursor.join(".git").exists() {
                return Ok(None);
            }
            match cursor.parent() {
                Some(p) => cursor = p.to_path_buf(),
                None => return Ok(None),
            }
        }
    }
}
