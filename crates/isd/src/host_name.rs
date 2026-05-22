//! Helpers for resolving the operator-facing host name at enroll time.
//!
//! Background: the agent reports its own hostname during enrollment
//! (`uname -n` inside the container), which on docker-in-docker
//! deployments is the container's short hash (e.g. `60999df9604f`).
//! That value is useless for the operator's mental model of the host
//! (and `isd ssh hosts` renders it verbatim as a name column).
//!
//! Resolution order at enroll time:
//!
//! 1. If the operator passed `--name <foo>`, use that.
//! 2. Else, query the target docker daemon's `info.name`
//!    (`docker info` JSON: matches `uname -n` on a Linux daemon host).
//! 3. Else, leave the row untouched (the agent's reported hostname,
//!    typically the container hash, stays in place; the operator can
//!    fix it later with `isd ssh hosts set <agent> --name <foo>`).

use anyhow::Result;

/// Resolve the operator-facing host name from the daemon's
/// `docker info` response.
///
/// Returns `Ok(Some(name))` when the daemon reports a non-empty
/// `name`. Returns `Ok(None)` when the field is missing or empty
/// (callers fall back to leaving the agent-reported value untouched).
/// Returns `Err` only on transport / decode failures so the caller can
/// decide whether to log + swallow or surface the error.
pub async fn resolve_from_docker_info(docker: &bollard::Docker) -> Result<Option<String>> {
    let info = docker
        .info()
        .await
        .map_err(|e| anyhow::anyhow!("bollard docker.info: {e}"))?;
    Ok(info.name.filter(|n| !n.is_empty()))
}

/// Pick the operator-facing host name to PATCH after enroll.
///
/// Pure function so the resolution order is unit-testable without a
/// docker daemon. The `daemon_name` argument is whatever
/// [`resolve_from_docker_info`] returned for the target docker
/// context.
pub fn pick_enroll_hostname(
    override_name: Option<&str>,
    daemon_name: Option<&str>,
) -> Option<String> {
    if let Some(n) = override_name {
        let trimmed = n.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(n) = daemon_name {
        let trimmed = n.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Best-effort PATCH of the most recently enrolled host's `hostname`
/// (and, when set, `dial_target` in the same request) so the
/// operator sees a real name in `isd ssh hosts` immediately after
/// `isd init` / `isd join`.
///
/// `hostname` should be the value produced by [`pick_enroll_hostname`];
/// `dial_target` is the operator-typeable SSH target (or `None` when
/// the docker context is not SSH). When both are `None`, this is a
/// no-op (we never PATCH for nothing).
///
/// Failures are reported on stderr and swallowed: enrollment already
/// succeeded; the operator's worst case is the container-hash name +
/// `(unset)` dial target, both of which they can fix with
/// `isd ssh hosts set <agent> --name <foo> [--dial <target>]`.
pub async fn patch_latest_host_best_effort(
    controller_url: &str,
    hostname: Option<&str>,
    dial_target: Option<&str>,
) {
    if hostname.is_none() && dial_target.is_none() {
        return;
    }
    if let Err(e) = patch_latest_host(controller_url, hostname, dial_target).await {
        eprintln!(
            "isd: warning: could not record host name/dial target on the enrolled host: {e:#}. \
             Run `isd ssh hosts set <agent> [--name <name>] [--dial <target>]` to set them manually."
        );
    }
}

/// Fallible inner: GET /api/v1/hosts, pick the most recently enrolled
/// row (the first in the list: the dashboard sorts by `last_seen_at
/// DESC NULLS LAST, enrolled_at DESC`), PATCH whichever fields the
/// caller wants to set.
async fn patch_latest_host(
    controller_url: &str,
    hostname: Option<&str>,
    dial_target: Option<&str>,
) -> Result<()> {
    use anyhow::Context as _;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("building HTTP client for host PATCH")?;
    let hosts_url = format!("{}/api/v1/hosts", controller_url.trim_end_matches('/'));
    let hosts: Vec<crate::dial_target::HostDtoSubset> = client
        .get(&hosts_url)
        .send()
        .await
        .with_context(|| format!("GET {hosts_url}"))?
        .error_for_status()
        .context("listing hosts to record host name")?
        .json()
        .await
        .context("decoding hosts JSON for host PATCH")?;
    let row = hosts
        .first()
        .ok_or_else(|| anyhow::anyhow!("controller returned an empty hosts list"))?;
    crate::dial_target::patch_host_fields(&client, controller_url, &row.id, dial_target, hostname)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--name foo` wins regardless of what the daemon reports.
    #[test]
    fn pick_prefers_operator_override() {
        let got = pick_enroll_hostname(Some("lausanne"), Some("daemon-name"));
        assert_eq!(got.as_deref(), Some("lausanne"));
    }

    /// Daemon name is used when the operator did not pass `--name`.
    #[test]
    fn pick_falls_back_to_daemon_name() {
        let got = pick_enroll_hostname(None, Some("daemon-name"));
        assert_eq!(got.as_deref(), Some("daemon-name"));
    }

    /// Returns None when neither source supplies a name; the caller
    /// then leaves the agent-reported value (container hash) in place.
    #[test]
    fn pick_returns_none_when_no_source() {
        assert_eq!(pick_enroll_hostname(None, None), None);
    }

    /// Empty / whitespace-only override is ignored so the daemon name
    /// (or None) wins. Guards against `--name ""` accidentally
    /// blanking the resolution.
    #[test]
    fn pick_skips_empty_override() {
        assert_eq!(
            pick_enroll_hostname(Some("   "), Some("daemon")).as_deref(),
            Some("daemon")
        );
        assert_eq!(pick_enroll_hostname(Some(""), None), None);
    }

    /// Whitespace-only daemon name is treated as missing (guards
    /// against an exotic daemon that returns `" "`).
    #[test]
    fn pick_skips_empty_daemon_name() {
        assert_eq!(pick_enroll_hostname(None, Some("")), None);
        assert_eq!(pick_enroll_hostname(None, Some("  ")), None);
    }

    /// Trims surrounding whitespace so a copy-paste `--name ` value
    /// does not leak a leading/trailing space into the row.
    #[test]
    fn pick_trims_whitespace() {
        assert_eq!(
            pick_enroll_hostname(Some("  lausanne  "), None).as_deref(),
            Some("lausanne")
        );
    }
}
