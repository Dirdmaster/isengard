//! Orphan-state cleanup on agent restart.
//!
//! Background. wisp-net creates bridges + veths + iptables NAT rules on
//! container start, and cleans them up on container stop. If the agent
//! crashes or the host reboots mid-stack, the registry stays consistent
//! but kernel state may have leftovers: a bridge whose containers are
//! all gone, a veth pair stranded in the host netns, an iptables DNAT
//! rule pointing at a container IP that's no longer allocated.
//!
//! Phase 0.7+0.8 moved the agent to systemd-managed install: every
//! kernel-update reboot now lands us on a clean re-exec of the agent
//! with stale kernel networking still around. The reconcile pass is the
//! "boot sweep" that brings the two views back into sync.
//!
//! Surface:
//!
//! - [`reconcile`] is the entry point. Takes a [`ReconcileInputs`]
//!   describing where to find the on-disk registry and which container
//!   ids the caller considers live. Returns a [`ReconcileReport`] with
//!   counts + the cleaned identifiers, for the agent's log line.
//! - [`diff_bridges`], [`diff_veths`], [`diff_iptables`] are pure
//!   functions over `(kernel_state, registry_state, known_ids)` so the
//!   diff logic stays unit-testable on Mac.
//! - Side-effect code (filesystem scan + `iptables-save` + `ip link
//!   del`) lives behind small adapter functions that are easy to swap
//!   for fakes.
//!
//! Bridges + network-level iptables rules are scoped by network name
//! and reconciled against `<state_dir>/networks/*/network.json`.
//! Per-attachment iptables rules are scoped by container id and
//! reconciled against the caller-supplied `known_container_ids` set
//! (the agent gathers these from `wisp::Runtime::list` before calling
//! [`reconcile`]). Veth halves are reconciled structurally: any
//! `wveth-c-*` device in the host netns is by definition orphaned (the
//! attach flow renames it to `eth0` and moves it into the container
//! netns), and any `wveth-h-*` not enslaved to a registered wisp bridge
//! is treated as a failed mid-attach to clean.

use crate::cmd;
use crate::error::WispNetError;
use std::collections::BTreeSet;
use std::path::Path;

/// Inputs to [`reconcile`].
///
/// - `networks_dir`: agent's `<state_dir>/networks/` (each subdir
///   contains a `network.json`).
/// - `known_container_ids`: container ids the agent considers live.
///   `None` means "don't touch container-scoped iptables rules" (use
///   when the caller can't enumerate containers safely). `Some(set)`
///   means "any container id NOT in this set is orphaned".
#[derive(Debug, Clone)]
pub struct ReconcileInputs<'a> {
    pub networks_dir: &'a Path,
    pub known_container_ids: Option<&'a BTreeSet<String>>,
}

/// Outcome of a reconcile pass. The agent logs this and the wisp-cli
/// debug subcommand prints it; counts are also useful for assertions in
/// the integration test.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Bridges (`wbr-*`) that existed in the kernel but had no entry
    /// in the registry, now deleted.
    pub orphan_bridges: Vec<String>,
    /// Veth host-side interfaces (`wveth-h-*`) cleaned because they
    /// weren't enslaved to any registered wisp bridge, plus orphaned
    /// container-side stubs (`wveth-c-*`) that never made it into a
    /// container netns.
    pub orphan_veths: Vec<String>,
    /// `iptables` comment scopes (`wisp:<scope>:<purpose>`) whose
    /// matching rules were revoked because the scope (network name or
    /// container id) was not registered + not in `known_container_ids`.
    pub orphan_iptables_scopes: Vec<String>,
    /// Registry directories (`<state_dir>/networks/<name>/`) that
    /// pointed at bridges no longer present in the kernel; the JSON
    /// file is removed so the next ensure rebuilds it from scratch.
    pub stale_registry_entries: Vec<String>,
}

impl ReconcileReport {
    /// Total number of cleanup actions taken. Convenience for the
    /// agent's `info!` line ("reconcile: cleaned N orphans").
    pub fn total(&self) -> usize {
        self.orphan_bridges.len()
            + self.orphan_veths.len()
            + self.orphan_iptables_scopes.len()
            + self.stale_registry_entries.len()
    }

    /// True when reconcile didn't have to clean anything.
    pub fn is_clean(&self) -> bool {
        self.total() == 0
    }
}

/// One persisted entry in the agent's network registry.
///
/// Mirrors the shape that `isengard_agent::runtime::wisp_backend`
/// writes today. Defined here (read-only) so wisp-net's reconcile pass
/// doesn't need to take a dep on isengard-agent. Round-trips with the
/// agent's serializer: the agent writes, wisp-net reads.
///
/// Unknown / future fields are ignored on parse (serde default).
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct PersistedNetwork {
    pub name: String,
    pub subnet: String,
    pub bridge: String,
}

/// Top-level reconcile pass. Reads kernel state, diffs against the
/// registry + the caller's known-container set, and applies the
/// minimal cleanup actions.
///
/// Idempotent: a second call (against now-clean state) is a no-op and
/// returns an all-empty [`ReconcileReport`].
///
/// Errors. Filesystem walk + subprocess invocation propagate as
/// [`WispNetError`]. A missing networks dir is treated as "no registry
/// entries", not a hard error (fresh install path).
pub fn reconcile(inputs: ReconcileInputs<'_>) -> Result<ReconcileReport, WispNetError> {
    let registry = read_registry(inputs.networks_dir)?;
    let kernel_bridges = list_kernel_bridges()?;
    let kernel_veths = list_kernel_veths()?;
    let kernel_rules = list_kernel_iptables_scopes()?;

    let mut report = ReconcileReport::default();

    let bridge_diff = diff_bridges(&kernel_bridges, &registry);
    for name in &bridge_diff.orphan_bridges {
        match delete_bridge(name) {
            Ok(()) => report.orphan_bridges.push(name.clone()),
            Err(e) => tracing::warn!(bridge = %name, error = %e, "reconcile: delete bridge failed"),
        }
    }
    for entry in &bridge_diff.stale_registry_entries {
        match drop_registry_entry(inputs.networks_dir, entry) {
            Ok(()) => report.stale_registry_entries.push(entry.clone()),
            Err(e) => {
                tracing::warn!(entry = %entry, error = %e, "reconcile: stale registry cleanup failed")
            }
        }
    }

    // After bridge cleanup, the set of "active wisp bridges" is what
    // we keep. Veth diff treats `wveth-h-*` whose master is in that
    // set as live.
    let active_bridges: BTreeSet<String> = registry
        .iter()
        .map(|p| p.bridge.clone())
        // Skip bridges we just deleted (registry might say it exists
        // but the kernel just removed it).
        .filter(|b| !bridge_diff.orphan_bridges.contains(b))
        .collect();

    let veth_diff = diff_veths(&kernel_veths, &active_bridges);
    for name in &veth_diff {
        match delete_veth(name) {
            Ok(()) => report.orphan_veths.push(name.clone()),
            Err(e) => tracing::warn!(veth = %name, error = %e, "reconcile: delete veth failed"),
        }
    }

    let registered_networks: BTreeSet<String> = registry.iter().map(|p| p.name.clone()).collect();
    let rule_diff = diff_iptables(
        &kernel_rules,
        &registered_networks,
        inputs.known_container_ids,
    );
    for scope in &rule_diff {
        match revoke_iptables_scope(scope) {
            Ok(()) => report.orphan_iptables_scopes.push(scope.clone()),
            Err(e) => {
                tracing::warn!(scope = %scope, error = %e, "reconcile: revoke iptables scope failed")
            }
        }
    }

    Ok(report)
}

/// Diff result for bridge reconciliation: which bridges to delete, plus
/// which registry entries are now stale (registry says yes, kernel says
/// no).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BridgeDiff {
    pub orphan_bridges: Vec<String>,
    pub stale_registry_entries: Vec<String>,
}

/// Pure bridge diff: kernel bridges that aren't in the registry are
/// orphans, registry entries pointing at bridges the kernel doesn't
/// have are stale.
pub fn diff_bridges(kernel_bridges: &[String], registry: &[PersistedNetwork]) -> BridgeDiff {
    let registered_bridges: BTreeSet<&str> = registry.iter().map(|p| p.bridge.as_str()).collect();
    let mut diff = BridgeDiff::default();

    for b in kernel_bridges {
        if !registered_bridges.contains(b.as_str()) {
            diff.orphan_bridges.push(b.clone());
        }
    }
    for entry in registry {
        if !kernel_bridges.iter().any(|b| b == &entry.bridge) {
            diff.stale_registry_entries.push(entry.name.clone());
        }
    }
    diff
}

/// One veth interface as seen from the host netns.
///
/// `master` is the bridge enslavement (`Some("wbr-app")`) or `None`
/// when the veth is loose. Populated from `ip -d link show`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VethEntry {
    pub name: String,
    pub master: Option<String>,
}

/// Pure veth diff. Returns the names to delete.
///
/// Rules:
/// - Any `wveth-c-*` (container-side stub) in the host netns is an
///   orphan. The attach flow renames the container-side end to `eth0`
///   and moves it into the container's netns; a `wveth-c-*` in the
///   host netns means attach died after `create_pair` but before
///   `attach_to_ns`.
/// - Any `wveth-h-*` (host-side stub) whose master bridge is NOT in
///   `active_bridges` is an orphan. The active set comes from the
///   registry after bridge cleanup.
pub fn diff_veths(kernel_veths: &[VethEntry], active_bridges: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    for v in kernel_veths {
        if v.name.starts_with("wveth-c-") {
            out.push(v.name.clone());
            continue;
        }
        if v.name.starts_with("wveth-h-") {
            let is_attached = v
                .master
                .as_ref()
                .map(|m| active_bridges.contains(m))
                .unwrap_or(false);
            if !is_attached {
                out.push(v.name.clone());
            }
        }
    }
    out
}

/// Pure iptables diff. Returns the comment scopes (e.g. `"wisp:app"`,
/// `"wisp:web-1"`) whose rules should be revoked.
///
/// Rules:
/// - Bare scope (after stripping `wisp:`) in `registered_networks`?
///   Keep (this is a network-level rule whose network is alive).
/// - Bare scope in `known_container_ids` (only when `Some(set)`)?
///   Keep (this is an attachment rule whose container is alive).
/// - `known_container_ids = None` means "the caller can't enumerate
///   containers; treat all non-network scopes as live containers".
///   Used by the standalone CLI; preserves attachment rules so the
///   operator doesn't accidentally cut a running container's
///   networking.
/// - Otherwise -> orphan.
///
/// The agent passes `Some(set)` from its boot hook (it does know the
/// live container ids), so attachment-rule orphans get cleaned. The
/// CLI passes `None` by default and `Some(empty_set)` under `--all`.
pub fn diff_iptables(
    kernel_scopes: &[String],
    registered_networks: &BTreeSet<String>,
    known_container_ids: Option<&BTreeSet<String>>,
) -> Vec<String> {
    let mut out = Vec::new();
    for scope in kernel_scopes {
        // `kernel_scopes` carries the full `wisp:<name>` prefix; the
        // registry sets are bare names. Strip the prefix to compare.
        let bare = scope.strip_prefix("wisp:").unwrap_or(scope.as_str());
        if registered_networks.contains(bare) {
            continue;
        }
        match known_container_ids {
            Some(set) => {
                if set.contains(bare) {
                    continue;
                }
                // Some(...) + not in set -> orphan (fall through).
            }
            None => {
                // Caller can't enumerate; assume any non-network scope
                // belongs to a live container.
                continue;
            }
        }
        if !out.contains(scope) {
            out.push(scope.clone());
        }
    }
    out
}

// ----- side-effect adapters -----

/// Read every `<networks_dir>/<name>/network.json` into a
/// [`PersistedNetwork`]. Missing dir -> empty vec. Corrupt JSON entries
/// are logged + skipped (treat as "first ensure" the same way the
/// agent's read helper does).
fn read_registry(networks_dir: &Path) -> Result<Vec<PersistedNetwork>, WispNetError> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(networks_dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(WispNetError::Io(e)),
    };
    for entry in entries {
        let entry = entry.map_err(WispNetError::Io)?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path().join("network.json");
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<PersistedNetwork>(&bytes) {
                Ok(p) => out.push(p),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "reconcile: skip corrupt network.json"
                    );
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(WispNetError::Io(e)),
        }
    }
    Ok(out)
}

/// Read the names of every wbr-* bridge interface in the host netns.
/// Delegates to the existing helper so kernel discovery stays in one
/// place.
fn list_kernel_bridges() -> Result<Vec<String>, WispNetError> {
    crate::bridge::list_wisp_bridges()
}

/// Read every wveth-* veth half in the host netns plus its master
/// bridge (if any). Implementation: `ip -d link show type veth` and
/// parse master from the per-link line.
///
/// Returns an empty vec when /sys/class/net is missing (Mac dev path).
fn list_kernel_veths() -> Result<Vec<VethEntry>, WispNetError> {
    // Cheap structural scan via /sys/class/net first to filter names.
    let dir = match std::fs::read_dir("/sys/class/net") {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(WispNetError::Io(e)),
    };
    let mut candidates: Vec<String> = Vec::new();
    for entry in dir {
        let entry = entry.map_err(WispNetError::Io)?;
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with("wveth-h-") || name.starts_with("wveth-c-") {
                candidates.push(name.to_string());
            }
        }
    }
    candidates.sort();

    let mut out = Vec::with_capacity(candidates.len());
    for name in candidates {
        let master = read_master_link(&name)?;
        out.push(VethEntry { name, master });
    }
    Ok(out)
}

/// Read `/sys/class/net/<name>/master` (a symlink to the master
/// bridge's sysfs dir) and return the basename, or `None` if the link
/// doesn't exist.
fn read_master_link(name: &str) -> Result<Option<String>, WispNetError> {
    let path = format!("/sys/class/net/{name}/master");
    match std::fs::read_link(&path) {
        Ok(target) => Ok(target
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(WispNetError::Io(e)),
    }
}

/// Run `iptables-save -t nat` + `iptables-save -t filter`, parse out
/// every distinct `wisp:<scope>:<purpose>` comment, and return the
/// unique scope set.
///
/// Tolerant: if `iptables-save` isn't on PATH (e.g. running this from
/// a unit test on a stripped image) the helper returns an empty vec
/// instead of erroring. Real misconfigurations (missing modules,
/// missing perms) still surface as [`WispNetError::Cmd`].
fn list_kernel_iptables_scopes() -> Result<Vec<String>, WispNetError> {
    let mut scopes: BTreeSet<String> = BTreeSet::new();
    for table in ["nat", "filter"] {
        match run_iptables_save(table) {
            Ok(output) => {
                for s in extract_wisp_scopes(&output) {
                    scopes.insert(s);
                }
            }
            Err(WispNetError::Cmd { stderr, .. })
                if stderr.contains("No such file or directory") || stderr.contains("not found") =>
            {
                tracing::warn!(
                    table,
                    "reconcile: iptables-save unavailable, skipping table"
                );
            }
            Err(e) => return Err(e),
        }
    }
    Ok(scopes.into_iter().collect())
}

fn run_iptables_save(table: &str) -> Result<String, WispNetError> {
    let out = std::process::Command::new("iptables-save")
        .args(["-t", table])
        .output()
        .map_err(|e| WispNetError::Cmd {
            command: format!("iptables-save -t {table}"),
            exit_code: -1,
            stderr: format!("spawn failed: {e}"),
        })?;
    if !out.status.success() {
        return Err(WispNetError::Cmd {
            command: format!("iptables-save -t {table}"),
            exit_code: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Pure helper: parse an `iptables-save` dump, return every unique
/// `wisp:<scope>` prefix (everything between the first colon and the
/// next colon).
///
/// `iptables-save` emits comments as `... -m comment --comment "wisp:scope:purpose" ...`.
/// We tolerate both quoted and unquoted forms.
fn extract_wisp_scopes(dump: &str) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for line in dump.lines() {
        let mut rest = line;
        while let Some(idx) = rest.find("wisp:") {
            let after = &rest[idx + "wisp:".len()..];
            // Scope ends at the next `:` (which prefixes the purpose).
            // If there's no inner colon, the comment is malformed; skip.
            if let Some(colon) = after.find(':') {
                let scope = &after[..colon];
                if !scope.is_empty() {
                    out.insert(format!("wisp:{scope}"));
                }
                rest = &after[colon..];
            } else {
                break;
            }
        }
    }
    out.into_iter().collect()
}

fn delete_bridge(bridge: &str) -> Result<(), WispNetError> {
    match cmd::run_ip(&["link", "delete", bridge, "type", "bridge"]) {
        Ok(_) => Ok(()),
        Err(WispNetError::Cmd { stderr, .. }) if is_missing_device(&stderr) => Ok(()),
        Err(e) => Err(e),
    }
}

fn delete_veth(name: &str) -> Result<(), WispNetError> {
    match cmd::run_ip(&["link", "delete", name]) {
        Ok(_) => Ok(()),
        Err(WispNetError::Cmd { stderr, .. }) if is_missing_device(&stderr) => Ok(()),
        Err(e) => Err(e),
    }
}

fn is_missing_device(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("cannot find device") || s.contains("does not exist")
}

/// Walk `iptables-save -t nat|filter`, find every rule matching the
/// scope (`wisp:<scope>:<purpose>`), reconstruct the `-D` invocation,
/// and call `iptables -t <table> -D ...`.
fn revoke_iptables_scope(scope: &str) -> Result<(), WispNetError> {
    for table in ["nat", "filter"] {
        let dump = match run_iptables_save(table) {
            Ok(s) => s,
            Err(WispNetError::Cmd { stderr, .. })
                if stderr.contains("No such file or directory") =>
            {
                continue;
            }
            Err(e) => return Err(e),
        };
        for rule_args in extract_delete_args_for_scope(&dump, scope) {
            let mut full = vec!["-t".to_string(), table.to_string()];
            full.extend(rule_args);
            let refs: Vec<&str> = full.iter().map(String::as_str).collect();
            match cmd::run_iptables(&refs) {
                Ok(_) => {}
                Err(WispNetError::Cmd { stderr, .. }) if is_missing_rule(&stderr) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

fn is_missing_rule(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("no chain/target/match by that name")
        || s.contains("does a matching rule exist")
        || s.contains("no such file or directory")
}

/// Pure: scan an `iptables-save` dump for `-A <chain> ...` lines that
/// carry the given scope's comment. Return each as a `-D ...` arg
/// vector (verbatim args, with the leading `-A` swapped for `-D`).
fn extract_delete_args_for_scope(dump: &str, scope: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    // `scope` is `wisp:<name>`; rules carry a comment of shape
    // `wisp:<name>:<purpose>`. Match the comment as
    // "starts with <scope>:".
    let needle_prefix = format!("{scope}:");
    for line in dump.lines() {
        let line = line.trim();
        if !line.starts_with("-A ") {
            continue;
        }
        // Comment value lives between `--comment ` and the next quote
        // boundary OR the next whitespace. Accept both `"wisp:foo:bar"`
        // (older iptables-save) and `wisp:foo:bar` (newer iptables-nft).
        let comment_value = match extract_comment_value(line) {
            Some(v) => v,
            None => continue,
        };
        if !comment_value.starts_with(&needle_prefix) {
            continue;
        }
        let args = parse_save_args(line);
        if !args.is_empty() {
            // Swap leading `-A` for `-D`.
            let mut d = args.clone();
            d[0] = "-D".to_string();
            out.push(d);
        }
    }
    out
}

fn extract_comment_value(line: &str) -> Option<String> {
    let idx = line.find("--comment")?;
    let rest = line[idx + "--comment".len()..].trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

/// Tokenize an `iptables-save` rule line into argv-style args,
/// preserving the `"quoted strings"` as single tokens.
fn parse_save_args(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for c in line.chars() {
        if in_quote {
            if c == '"' {
                out.push(std::mem::take(&mut current));
                in_quote = false;
            } else {
                current.push(c);
            }
        } else if c == '"' {
            in_quote = true;
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else if c.is_whitespace() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Remove a stale `<networks_dir>/<name>/network.json`. Idempotent.
fn drop_registry_entry(networks_dir: &Path, name: &str) -> Result<(), WispNetError> {
    let path = networks_dir.join(name).join("network.json");
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(WispNetError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pn(name: &str, bridge: &str) -> PersistedNetwork {
        PersistedNetwork {
            name: name.to_string(),
            subnet: "10.83.0.0/24".to_string(),
            bridge: bridge.to_string(),
        }
    }

    #[test]
    fn diff_bridges_flags_kernel_only_as_orphan() {
        let kernel = vec!["wbr-app".to_string(), "wbr-ghost".to_string()];
        let registry = vec![pn("app", "wbr-app")];
        let diff = diff_bridges(&kernel, &registry);
        assert_eq!(diff.orphan_bridges, vec!["wbr-ghost".to_string()]);
        assert!(diff.stale_registry_entries.is_empty());
    }

    #[test]
    fn diff_bridges_flags_registry_only_as_stale() {
        let kernel = vec!["wbr-app".to_string()];
        let registry = vec![pn("app", "wbr-app"), pn("db", "wbr-db")];
        let diff = diff_bridges(&kernel, &registry);
        assert!(diff.orphan_bridges.is_empty());
        assert_eq!(diff.stale_registry_entries, vec!["db".to_string()]);
    }

    #[test]
    fn diff_bridges_both_present_is_noop() {
        let kernel = vec!["wbr-app".to_string()];
        let registry = vec![pn("app", "wbr-app")];
        let diff = diff_bridges(&kernel, &registry);
        assert!(diff.orphan_bridges.is_empty());
        assert!(diff.stale_registry_entries.is_empty());
    }

    #[test]
    fn diff_bridges_empty_state_is_noop() {
        let diff = diff_bridges(&[], &[]);
        assert!(diff.orphan_bridges.is_empty());
        assert!(diff.stale_registry_entries.is_empty());
    }

    #[test]
    fn diff_veths_flags_loose_container_stubs() {
        // wveth-c-* in the host ns is always an orphan.
        let kernel = vec![
            VethEntry {
                name: "wveth-c-abc123".into(),
                master: None,
            },
            VethEntry {
                name: "wveth-c-def456".into(),
                master: Some("wbr-app".into()),
            },
        ];
        let active: BTreeSet<String> = ["wbr-app".to_string()].into_iter().collect();
        let orphans = diff_veths(&kernel, &active);
        assert_eq!(
            orphans,
            vec!["wveth-c-abc123".to_string(), "wveth-c-def456".to_string()]
        );
    }

    #[test]
    fn diff_veths_keeps_host_stubs_with_active_master() {
        let kernel = vec![VethEntry {
            name: "wveth-h-aaa111".into(),
            master: Some("wbr-app".into()),
        }];
        let active: BTreeSet<String> = ["wbr-app".to_string()].into_iter().collect();
        let orphans = diff_veths(&kernel, &active);
        assert!(orphans.is_empty(), "active master should keep host stub");
    }

    #[test]
    fn diff_veths_flags_host_stubs_without_master() {
        let kernel = vec![VethEntry {
            name: "wveth-h-bbb222".into(),
            master: None,
        }];
        let active: BTreeSet<String> = BTreeSet::new();
        let orphans = diff_veths(&kernel, &active);
        assert_eq!(orphans, vec!["wveth-h-bbb222".to_string()]);
    }

    #[test]
    fn diff_veths_flags_host_stubs_with_dead_master() {
        // host stub points at a bridge no longer in `active_bridges`.
        let kernel = vec![VethEntry {
            name: "wveth-h-ccc333".into(),
            master: Some("wbr-ghost".into()),
        }];
        let active: BTreeSet<String> = ["wbr-app".to_string()].into_iter().collect();
        let orphans = diff_veths(&kernel, &active);
        assert_eq!(orphans, vec!["wveth-h-ccc333".to_string()]);
    }

    #[test]
    fn diff_iptables_keeps_registered_network_scopes_when_set_known() {
        // With Some(known) the diff is "strict": anything not in
        // either set is an orphan.
        let kernel = vec!["wisp:app".to_string(), "wisp:ghost".to_string()];
        let registered: BTreeSet<String> = ["app".to_string()].into_iter().collect();
        let known: BTreeSet<String> = BTreeSet::new();
        let orphans = diff_iptables(&kernel, &registered, Some(&known));
        assert_eq!(orphans, vec!["wisp:ghost".to_string()]);
    }

    #[test]
    fn diff_iptables_keeps_known_container_ids() {
        let kernel = vec!["wisp:web-1".to_string(), "wisp:web-old".to_string()];
        let registered: BTreeSet<String> = BTreeSet::new();
        let containers: BTreeSet<String> = ["web-1".to_string()].into_iter().collect();
        let orphans = diff_iptables(&kernel, &registered, Some(&containers));
        assert_eq!(orphans, vec!["wisp:web-old".to_string()]);
    }

    #[test]
    fn diff_iptables_none_preserves_unknown_scopes() {
        // None means "caller can't enumerate containers"; the diff
        // pretends every unknown scope is a live container. Used by
        // the standalone CLI so it doesn't cut a running container's
        // networking.
        let kernel = vec!["wisp:web-1".to_string(), "wisp:ghost".to_string()];
        let registered: BTreeSet<String> = ["app".to_string()].into_iter().collect();
        let orphans = diff_iptables(&kernel, &registered, None);
        assert!(
            orphans.is_empty(),
            "None should preserve all unknowns: {orphans:?}"
        );
    }

    #[test]
    fn diff_iptables_empty_state_is_noop() {
        let orphans = diff_iptables(&[], &BTreeSet::new(), None);
        assert!(orphans.is_empty());
    }

    #[test]
    fn extract_wisp_scopes_handles_quoted_comments() {
        let dump = r#"
            # Generated by iptables-save
            *nat
            :PREROUTING ACCEPT [0:0]
            -A POSTROUTING -s 10.83.0.0/24 ! -o wbr-app -j MASQUERADE -m comment --comment "wisp:app:masq"
            -A PREROUTING -p tcp --dport 18080 -j DNAT --to-destination 10.83.0.2:80 -m comment --comment "wisp:web-1:dnat"
            COMMIT
        "#;
        let scopes = extract_wisp_scopes(dump);
        assert!(scopes.contains(&"wisp:app".to_string()));
        assert!(scopes.contains(&"wisp:web-1".to_string()));
        assert_eq!(scopes.len(), 2, "unique scopes: {scopes:?}");
    }

    #[test]
    fn extract_wisp_scopes_dedups_multiple_purposes() {
        // Same scope, multiple rules: should collapse to one.
        let dump = r#"
            -A POSTROUTING -j MASQUERADE -m comment --comment "wisp:app:masq"
            -A FORWARD -i wbr-app -j ACCEPT -m comment --comment "wisp:app:bridge-to-bridge"
            -A FORWARD ! -i wbr-app -o wbr-app -j ACCEPT -m comment --comment "wisp:app:bridge-in-related"
        "#;
        let scopes = extract_wisp_scopes(dump);
        assert_eq!(scopes, vec!["wisp:app".to_string()]);
    }

    #[test]
    fn extract_wisp_scopes_skips_unrelated_comments() {
        let dump = r#"
            -A INPUT -m comment --comment "docker: bridge -> filter"
            -A POSTROUTING -m comment --comment "other rule"
        "#;
        let scopes = extract_wisp_scopes(dump);
        assert!(scopes.is_empty());
    }

    #[test]
    fn extract_delete_args_swaps_a_for_d() {
        let dump = r#"
            -A POSTROUTING -s 10.83.0.0/24 ! -o wbr-app -j MASQUERADE -m comment --comment "wisp:app:masq"
        "#;
        let args = extract_delete_args_for_scope(dump, "wisp:app");
        assert_eq!(args.len(), 1);
        assert_eq!(args[0][0], "-D");
        assert!(args[0].iter().any(|a| a == "POSTROUTING"));
        assert!(args[0].iter().any(|a| a == "MASQUERADE"));
        assert!(args[0].iter().any(|a| a == "wisp:app:masq"));
    }

    #[test]
    fn extract_delete_args_ignores_other_scopes() {
        let dump = r#"
            -A POSTROUTING -j MASQUERADE -m comment --comment "wisp:app:masq"
            -A POSTROUTING -j MASQUERADE -m comment --comment "wisp:other:masq"
        "#;
        let args = extract_delete_args_for_scope(dump, "wisp:app");
        assert_eq!(args.len(), 1, "got {args:?}");
        assert!(args[0].iter().any(|a| a == "wisp:app:masq"));
    }

    #[test]
    fn parse_save_args_handles_quoted_token() {
        let line = r#"-A POSTROUTING -j MASQUERADE -m comment --comment "wisp:app:masq""#;
        let args = parse_save_args(line);
        assert_eq!(args[0], "-A");
        assert_eq!(args[1], "POSTROUTING");
        assert_eq!(args[args.len() - 1], "wisp:app:masq");
    }

    #[test]
    fn report_total_sums_all_fields() {
        let r = ReconcileReport {
            orphan_bridges: vec!["a".into()],
            orphan_veths: vec!["b".into(), "c".into()],
            orphan_iptables_scopes: vec!["d".into()],
            stale_registry_entries: vec!["e".into()],
        };
        assert_eq!(r.total(), 5);
        assert!(!r.is_clean());

        let empty = ReconcileReport::default();
        assert_eq!(empty.total(), 0);
        assert!(empty.is_clean());
    }

    #[test]
    fn read_registry_returns_empty_for_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist");
        let out = read_registry(&path).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn read_registry_parses_agent_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let networks = tmp.path().join("networks");
        let app_dir = networks.join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("network.json"),
            br#"{"name":"app","subnet":"10.83.0.0/24","gateway":"10.83.0.1","bridge":"wbr-app"}"#,
        )
        .unwrap();

        let out = read_registry(&networks).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "app");
        assert_eq!(out[0].bridge, "wbr-app");
        assert_eq!(out[0].subnet, "10.83.0.0/24");
    }

    #[test]
    fn read_registry_skips_corrupt_files() {
        let tmp = tempfile::tempdir().unwrap();
        let networks = tmp.path().join("networks");
        let bad = networks.join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("network.json"), b"{ not valid").unwrap();
        let good = networks.join("good");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(
            good.join("network.json"),
            br#"{"name":"good","subnet":"10.84.0.0/24","gateway":"10.84.0.1","bridge":"wbr-good"}"#,
        )
        .unwrap();
        let out = read_registry(&networks).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "good");
    }

    #[test]
    fn drop_registry_entry_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let nets = tmp.path().join("networks");
        let app = nets.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("network.json"), b"{}").unwrap();
        drop_registry_entry(&nets, "app").unwrap();
        assert!(!app.join("network.json").exists());
        // Second call: no error.
        drop_registry_entry(&nets, "app").unwrap();
        drop_registry_entry(&nets, "never-existed").unwrap();
    }
}
