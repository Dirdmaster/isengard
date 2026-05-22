//! Helpers for the host-row `dial_target` field.
//!
//! `dial_target` is the address an operator types into `ssh` to reach
//! a host. The CLI captures it at enroll time from the operator's
//! active docker context URL and PATCHes it onto the host row;
//! `isd ssh hosts set <agent> --dial <target>` overrides it later.
//!
//! Two responsibilities live here:
//!
//! 1. Best-effort PATCH of the most recently enrolled host with the
//!    operator's docker dial target. Called from `init_cmd` and
//!    `join_cmd` after the agent appears in the host list. Failures
//!    are logged + swallowed: the enrollment already succeeded; the
//!    operator's worst case is `(unset)` in `isd ssh hosts` (which
//!    they can fix with `isd ssh hosts set <agent> --dial <target>`).
//!
//! 2. Lookup of the dial target for a name token used by
//!    `isd ssh <host>`: a bare hostname that maps to a host row with
//!    a set `dial_target` is rewritten to that target so the operator
//!    dials the operator-visible address instead of the agent's
//!    reported container hash.

use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::json;

use crate::docker_context;

/// Subset of `HostDto` the dial-target helpers care about. Other
/// fields on the wire are tolerated thanks to serde's default behavior.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HostDtoSubset {
    /// ULID of the host row, as printed by the dashboard.
    pub id: String,
    /// Reported hostname (`uname -n` on the agent host).
    pub hostname: String,
    /// Operator-facing dial target (e.g. `dirdmaster@10.17.0.125`).
    /// `None` for pre-PR-B rows or hosts where the operator never
    /// enrolled from an SSH-backed docker context.
    #[serde(default)]
    pub dial_target: Option<String>,
}

/// Send a PATCH against the most recently enrolled host row, writing
/// the dial target derived from the operator's active docker context.
///
/// Best-effort: any failure (no SSH context, no controller URL,
/// network error, no hosts on the controller) is reported on stderr
/// and swallowed. Enrollment already succeeded; the operator can
/// recover with `isd ssh hosts set <agent> --dial <target>`.
///
/// `controller_url` is the base URL the CLI uses to talk to the
/// controller (same value `Session::require_controller` returns).
/// `docker_uri` is the operator's docker context URL (e.g.
/// `ssh://dirdmaster@10.17.0.125`).
pub async fn patch_latest_host_dial_target_best_effort(
    controller_url: &str,
    docker_uri: &str,
) {
    let Some(target) = docker_context::dial_target_from_docker_uri(docker_uri) else {
        // Non-SSH docker context: no operator-typeable target to
        // capture. Leave the row's dial_target as-is.
        return;
    };
    if let Err(e) = patch_latest_host_dial_target(controller_url, &target).await {
        eprintln!(
            "isd: warning: could not record dial_target on the enrolled host: {e:#}. \
             Run `isd ssh hosts set <agent> --dial {target}` to set it manually."
        );
    }
}

/// Fallible inner: GET /api/v1/hosts, pick the most recently enrolled
/// row (the first in the list: the dashboard sorts by `last_seen_at
/// DESC NULLS LAST, enrolled_at DESC`), PATCH its dial_target.
async fn patch_latest_host_dial_target(controller_url: &str, target: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("building HTTP client for dial_target PATCH")?;
    let hosts_url = format!("{}/api/v1/hosts", controller_url.trim_end_matches('/'));
    let hosts: Vec<HostDtoSubset> = client
        .get(&hosts_url)
        .send()
        .await
        .with_context(|| format!("GET {hosts_url}"))?
        .error_for_status()
        .context("listing hosts to record dial_target")?
        .json()
        .await
        .context("decoding hosts JSON for dial_target PATCH")?;
    let row = hosts
        .first()
        .ok_or_else(|| anyhow::anyhow!("controller returned an empty hosts list"))?;
    patch_host_dial_target(&client, controller_url, &row.id, target).await?;
    Ok(())
}

/// PATCH /api/v1/hosts/{id} with `dial_target`. Returns the parsed
/// response body on success.
pub(crate) async fn patch_host_dial_target(
    client: &reqwest::Client,
    controller_url: &str,
    host_id: &str,
    target: &str,
) -> Result<HostDtoSubset> {
    let url = format!(
        "{}/api/v1/hosts/{host_id}",
        controller_url.trim_end_matches('/')
    );
    let body = json!({ "dial_target": target });
    let resp = client
        .patch(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("PATCH {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("PATCH {url} -> {status}: {text}");
    }
    resp.json::<HostDtoSubset>()
        .await
        .context("decoding PATCH /hosts response")
}

/// Find a host row matching `token` and return its dial_target (when
/// set). Used by `isd ssh <token>` so a bare hostname that registered
/// with a dial_target is dialed via the operator-visible address.
///
/// `token` matches against the row's `hostname`, against the row's
/// `dial_target` (so the operator can also dial by the address they
/// set), or against the host id ULID. Returns `Ok(None)` when no row
/// matches or when the matching row has no dial_target.
pub async fn lookup_dial_target(
    client: &reqwest::Client,
    controller_url: &str,
    token: &str,
) -> Result<Option<String>> {
    let url = format!("{}/api/v1/hosts", controller_url.trim_end_matches('/'));
    let hosts: Vec<HostDtoSubset> = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("listing hosts for dial-target lookup")?
        .json()
        .await
        .context("decoding hosts JSON for dial-target lookup")?;
    Ok(find_dial_target_for_token(&hosts, token))
}

/// Pure resolver: pick a host row whose `hostname`, `dial_target`, or
/// `id` matches `token` exactly, and return its `dial_target` (when
/// set). Pure to keep the host-list filtering policy unit-testable
/// without an HTTP fixture.
pub(crate) fn find_dial_target_for_token(
    hosts: &[HostDtoSubset],
    token: &str,
) -> Option<String> {
    for h in hosts {
        if h.hostname == token || h.id == token || h.dial_target.as_deref() == Some(token) {
            return h.dial_target.clone();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, hostname: &str, dial: Option<&str>) -> HostDtoSubset {
        HostDtoSubset {
            id: id.into(),
            hostname: hostname.into(),
            dial_target: dial.map(str::to_string),
        }
    }

    /// A bare hostname that matches an enrolled host with a set
    /// `dial_target` resolves to that target.
    #[test]
    fn resolver_matches_hostname_to_dial_target() {
        let hosts = vec![
            host("01HEDGE1", "edge-1", Some("dirdmaster@10.17.0.125")),
            host("01HEDGE2", "edge-2", None),
        ];
        assert_eq!(
            find_dial_target_for_token(&hosts, "edge-1"),
            Some("dirdmaster@10.17.0.125".into())
        );
    }

    /// A hostname that matches but the row has no dial_target falls
    /// back to None so the dial path uses the bare hostname.
    #[test]
    fn resolver_returns_none_when_dial_target_missing() {
        let hosts = vec![host("01HEDGE2", "edge-2", None)];
        assert_eq!(find_dial_target_for_token(&hosts, "edge-2"), None);
    }

    /// An unknown token returns None so the dial path uses the
    /// operator-typed value verbatim (preserves the pre-PR-B behavior).
    #[test]
    fn resolver_unknown_token_returns_none() {
        let hosts = vec![host("01HEDGE1", "edge-1", Some("op@host"))];
        assert_eq!(find_dial_target_for_token(&hosts, "edge-99"), None);
    }

    /// Matching by the row's id ULID also resolves to its dial target.
    /// Operator never types this in practice; the path exists so a
    /// scripted `isd ssh $(jq id ...)` works without extra plumbing.
    #[test]
    fn resolver_matches_by_id() {
        let hosts = vec![host("01HEDGE1", "edge-1", Some("op@host"))];
        assert_eq!(
            find_dial_target_for_token(&hosts, "01HEDGE1"),
            Some("op@host".into())
        );
    }

    /// Matching by the dial target itself is idempotent (returns the
    /// same target). Operator gains nothing by typing it twice; the
    /// path is here so the dial does not skip the resolver if a
    /// scripted caller already passed `user@host`.
    #[test]
    fn resolver_matches_by_dial_target_itself() {
        let hosts = vec![host("01HEDGE1", "edge-1", Some("op@host"))];
        assert_eq!(
            find_dial_target_for_token(&hosts, "op@host"),
            Some("op@host".into())
        );
    }
}
