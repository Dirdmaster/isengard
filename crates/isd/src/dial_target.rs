//! Helpers for the host-row `dial_target` field (and, as of the host
//! name PR, the `hostname` field that ships in the same PATCH).
//!
//! `dial_target` is the address an operator types into `ssh` to reach
//! a host. The CLI captures it at enroll time from the operator's
//! active docker context URL and PATCHes it onto the host row;
//! `isd ssh hosts set <agent> --dial <target>` overrides it later.
//! The combined PATCH helper [`patch_host_fields`] also accepts a
//! `hostname` so `isd init` / `isd join` can ship name + dial target
//! in one round-trip (see `crate::host_name`).
//!
//! Two responsibilities live here:
//!
//! 1. PATCH helpers against `/api/v1/hosts/{id}` for the dial_target
//!    and hostname fields. Used by the enroll flow (`init_cmd`,
//!    `join_cmd`, via `crate::host_name`) and the operator-override
//!    path (`isd ssh hosts set`).
//!
//! 2. Lookup of the dial target for a name token used by
//!    `isd ssh <host>`: a bare hostname that maps to a host row with
//!    a set `dial_target` is rewritten to that target so the operator
//!    dials the operator-visible address instead of the agent's
//!    reported container hash.

use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::json;

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

/// PATCH /api/v1/hosts/{id} with any combination of patchable
/// fields. Mirrors the dashboard's [`PatchHostRequest`] shape:
/// `dial_target` is set when `Some(t)` (empty string clears, per the
/// API contract) and `hostname` is set when `Some(n)` (the server
/// rejects empty `hostname` with 400, so callers should filter empty
/// values out before invoking).
///
/// Used by `isd init` / `isd join` to ship name + dial_target in a
/// single request after enroll, and by `isd ssh hosts set` for the
/// operator-driven override path.
pub(crate) async fn patch_host_fields(
    client: &reqwest::Client,
    controller_url: &str,
    host_id: &str,
    dial_target: Option<&str>,
    hostname: Option<&str>,
) -> Result<HostDtoSubset> {
    let url = format!(
        "{}/api/v1/hosts/{host_id}",
        controller_url.trim_end_matches('/')
    );
    let mut body = serde_json::Map::new();
    if let Some(t) = dial_target {
        body.insert("dial_target".to_string(), json!(t));
    }
    if let Some(n) = hostname {
        body.insert("hostname".to_string(), json!(n));
    }
    let resp = client
        .patch(&url)
        .json(&serde_json::Value::Object(body))
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
pub(crate) fn find_dial_target_for_token(hosts: &[HostDtoSubset], token: &str) -> Option<String> {
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
