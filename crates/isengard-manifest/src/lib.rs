//! Parses `stack.toml` and `isengard.toml` into Rust values.
//!
//! [`StackManifest`] is the per-stack manifest sitting next to a stack's
//! compose file(s). It carries the orchestration metadata compose itself
//! can't express: name, fleet binding, secret names, deploy strategy,
//! lifecycle hooks, overlays.
//!
//! [`FleetManifest`] is the optional fleet-level sibling at the repo
//! root. It sets the default fleet and context used by `isd` commands
//! that don't take an explicit `--fleet`.
//!
//! The crate is `serde + toml` plus a small validator. The only I/O is
//! [`StackManifest::load`] and [`FleetManifest::load_from_walk`].
//! Consumers in `isd`, the controller, and the dashboard hand the parsed
//! shapes around as plain Rust values.
//!
//! The compose overlay merger lives in [`merge_compose_yaml`]. It runs
//! over `serde_yaml::Value` trees so this crate stays dep-light.
//!
//! Schema rationale: see
//! Stack manifest parsing and validation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

mod overlay;
mod parse;

pub use overlay::{MergeError, merge_compose_yaml};
pub use parse::{ManifestError, parse_fleet_manifest, parse_stack_manifest};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[doc = include_str!("../docs/stack-manifest.md")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackManifest {
    /// Stack identity. Required, non-empty.
    pub name: String,
    /// Fleet binding. Falls back to [`FleetManifest::fleet`] when absent.
    pub fleet: Option<String>,
    /// Ordered compose file list, relative to [`StackManifest::root`].
    /// Non-empty.
    pub compose: Vec<PathBuf>,
    /// Named overlay blocks keyed by overlay name. Each block adds extra
    /// compose files on top of `compose` when selected at deploy time.
    pub overlays: BTreeMap<String, OverlaySpec>,
    /// Stack-level deploy strategy. Defaults to [`Strategy::Auto`], which
    /// lets per-service phase 10g labels decide.
    pub strategy: Strategy,
    /// Per-fleet secret names mounted into every service in the stack.
    pub secrets: Vec<String>,
    /// Lifecycle hooks run on the host at pre-deploy, post-deploy, and
    /// failure transitions.
    pub hooks: Vec<HookSpec>,
    /// Absolute path to the directory holding `stack.toml`. Anchors
    /// compose path resolution and sets the hook `cwd` at deploy time.
    pub root: PathBuf,
}

/// A named overlay block (`[overlays.<name>]`) in `stack.toml`.
///
/// Selecting an overlay at deploy time appends `compose` to the base
/// `compose` list. Paths are relative to [`StackManifest::root`] and
/// validated alongside the base list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlaySpec {
    /// Extra compose files this overlay adds, in order.
    pub compose: Vec<PathBuf>,
}

/// Stack-level deploy strategy.
///
/// Maps to the per-service phase 10g labels at deploy time. The default
/// is [`Strategy::Auto`]: the controller picks per service based on its
/// own labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Defer to per-service labels.
    #[default]
    Auto,
    /// Bring up a parallel color, swap traffic, tear the old color down.
    BlueGreen,
    /// Replace instances one at a time.
    Rolling,
    /// Stop everything, then start the new desired state.
    Recreate,
}

impl Strategy {
    /// Canonical kebab-case spelling.
    ///
    /// Matches the serde `rename_all = "kebab-case"` representation and
    /// the spelling the TOML parser accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::BlueGreen => "blue-green",
            Self::Rolling => "rolling",
            Self::Recreate => "recreate",
        }
    }
}

/// A lifecycle hook attached to a stack.
///
/// Hooks run on the host, not in any container. They fire at one of
/// three points named by [`HookEvent`]: pre-deploy, post-deploy, failure.
/// The deploy driver runs `cmd` with `cwd` set to [`StackManifest::root`]
/// and aborts after `timeout`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSpec {
    /// Lifecycle event that triggers this hook.
    pub on: HookEvent,
    /// Argv to run. Must be non-empty. First element is the program;
    /// remaining elements are arguments.
    pub cmd: Vec<String>,
    /// Wall-clock cap on the hook's runtime. Defaults to 60 seconds.
    pub timeout: Duration,
    /// What the deploy driver does when the hook exits non-zero or hits
    /// the timeout. Defaults to [`HookErrorPolicy::Abort`].
    pub on_error: HookErrorPolicy,
}

/// The lifecycle point at which a [`HookSpec`] fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookEvent {
    /// Before any container is created or updated.
    PreDeploy,
    /// After the desired state is reconciled successfully.
    PostDeploy,
    /// On any deploy failure, including hook failures from earlier in
    /// the lifecycle.
    Failure,
}

impl HookEvent {
    /// Canonical kebab-case spelling.
    ///
    /// Matches the serde representation and the TOML parser's accepted
    /// values.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreDeploy => "pre-deploy",
            Self::PostDeploy => "post-deploy",
            Self::Failure => "failure",
        }
    }
}

/// What happens when a hook fails (exits non-zero or times out).
///
/// `Abort` rolls the deploy back through the failure hook. `Continue`
/// logs the failure and moves on. Defaults to `Abort`: deploys are
/// fail-closed by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookErrorPolicy {
    /// Fail the deploy. Default.
    #[default]
    Abort,
    /// Log and keep going.
    Continue,
}

impl HookErrorPolicy {
    /// Canonical kebab-case spelling.
    ///
    /// Matches the serde representation and the TOML parser's accepted
    /// values.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Abort => "abort",
            Self::Continue => "continue",
        }
    }
}

impl StackManifest {
    /// Loads + validates `stack.toml` from disk.
    ///
    /// `path` is the manifest file itself (not its parent directory).
    /// The parent directory becomes [`StackManifest::root`].
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Io`] when the file can't be read.
    /// Returns [`ManifestError::InvalidPath`] when `path` has no parent
    /// (e.g. the filesystem root). Returns any parse / validation error
    /// raised by [`parse_stack_manifest`].
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

    /// Parses a manifest from an in-memory string.
    ///
    /// Used by the dashboard's API handler, which receives the manifest
    /// body verbatim. The caller supplies `root` for hook cwd resolution;
    /// on the controller side this is a synthetic path like
    /// `/etc/isengard/stacks/<name>/`.
    ///
    /// # Errors
    ///
    /// Returns the parse / validation errors raised by
    /// [`parse_stack_manifest`].
    pub fn from_str(text: &str, root: PathBuf) -> Result<Self, ManifestError> {
        parse_stack_manifest(text, root)
    }

    /// Resolves the ordered compose paths a deploy should merge.
    ///
    /// Returns the base [`StackManifest::compose`] list with each entry
    /// joined against [`StackManifest::root`]. If `overlay` names a known
    /// entry in [`StackManifest::overlays`], its compose paths append
    /// after the base list.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::UnknownOverlay`] when `overlay` is
    /// `Some(name)` and `name` isn't in [`StackManifest::overlays`].
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

    /// Builds a minimal manifest for a legacy compose-only stack.
    ///
    /// The result has `name`, one compose entry (`compose.toml`), no
    /// fleet, no overlays, no secrets, no hooks, [`Strategy::Auto`], and
    /// an empty [`StackManifest::root`]. Operators can drop the
    /// serialized output into a stack directory and `isd stack up --all`
    /// picks it up.
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

    /// Serializes this manifest back to TOML text.
    ///
    /// Round-trips through [`StackManifest::from_str`]. The `root` field
    /// is not serialized: the manifest's location on disk implies it.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Serialize`] when the underlying TOML
    /// serializer fails. In practice this shouldn't happen for any
    /// manifest that parsed cleanly.
    pub fn to_toml_string(&self) -> Result<String, ManifestError> {
        parse::serialize_stack_manifest(self)
    }
}

/// The optional fleet-level manifest at the repo root.
///
/// `isengard.toml` sits next to `.git` and sets the defaults `isd`
/// commands use when the operator doesn't pass `--fleet` or `--context`.
/// Both fields are optional; an empty file is valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetManifest {
    /// Default fleet name. `isd` commands fall back to this when the
    /// per-stack manifest and CLI flags don't name one.
    pub fleet: Option<String>,
    /// Default docker context (or controller context) name. Same fallback
    /// rule as [`FleetManifest::fleet`].
    pub context: Option<String>,
}

impl FleetManifest {
    /// Walks up from `start` looking for `isengard.toml`.
    ///
    /// Stops at the first `.git` directory or the filesystem root,
    /// whichever comes first. The `.git` boundary keeps the walk from
    /// leaking into unrelated repos on the host.
    ///
    /// Returns `Ok(None)` when no `isengard.toml` is found before the
    /// boundary.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Io`] when an `isengard.toml` file is
    /// found but can't be read. Returns the parse errors raised by
    /// [`parse_fleet_manifest`].
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
