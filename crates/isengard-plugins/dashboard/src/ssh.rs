//! SSH user-certificate issuance endpoint.
//!
//! Wires a single `POST /api/v1/ssh/cert` route into the dashboard
//! router. The handler validates the operator's pubkey, caps the
//! requested TTL against `ISENGARD_SSH_CERT_MAX_TTL` (default 1h),
//! signs the cert via the controller's [`SshAuthority`], and journals
//! an `ssh.cert.issued` event for audit.
//!
//! Phase 4 (`isd ssh <host>`) is the primary consumer: it generates a
//! short-lived operator keypair on the local machine, POSTs the
//! pubkey here, gets back the signed user cert, and feeds both into
//! `ssh -i <key> -o CertificateFile=<cert> <host>` to land on the
//! agent's pre-trusted sshd.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use isengard_controller::ControllerHandles;
use isengard_storage::InsertEvent;
use serde::{Deserialize, Serialize};
use ssh_key::{HashAlg, PublicKey};
use tracing::warn;

/// Hard upper bound on the TTL the dashboard will mint. `MAX_TTL_SECS`
/// is overridable per-deployment via the `ISENGARD_SSH_CERT_MAX_TTL`
/// controller env, but the absolute ceiling is enforced here too: a
/// misconfigured env can not push past 24h without a code change.
const ABSOLUTE_MAX_TTL_SECS: u64 = 86_400;

/// Default TTL cap when `ISENGARD_SSH_CERT_MAX_TTL` is unset. One hour
/// matches the spec's "short-lived" framing for operator certs.
const DEFAULT_MAX_TTL_SECS: u64 = 3_600;

/// Env var operators set on the controller to raise (or lower) the
/// per-request TTL ceiling. Capped against [`ABSOLUTE_MAX_TTL_SECS`].
pub const MAX_TTL_ENV_VAR: &str = "ISENGARD_SSH_CERT_MAX_TTL";

/// Request body for `POST /api/v1/ssh/cert`. Mirrors the public
/// surface the `isd ssh` CLI generates client-side.
#[derive(Debug, Deserialize)]
pub struct IssueSshCertBody {
    /// Operator's ephemeral SSH public key in OpenSSH
    /// `authorized_keys` format (e.g.
    /// `ssh-ed25519 AAAA... comment`). The signed cert pins this
    /// pubkey; only its matching private key can use the cert.
    pub pubkey: String,
    /// SSH principals the cert is valid for. Maps to Unix usernames
    /// on agent hosts. Common values: `["isengard"]` (a dedicated
    /// operator account) or `["root"]` (full host control).
    pub principals: Vec<String>,
    /// Requested TTL in seconds. Server caps via the configured
    /// `ISENGARD_SSH_CERT_MAX_TTL` ceiling.
    pub ttl_seconds: u64,
    /// Free-form key-id baked into the cert. Surfaces in `auditd` and
    /// `last` output on the agent host. Identifies the operator who
    /// requested the cert.
    pub comment: String,
}

/// Response body for `POST /api/v1/ssh/cert`. Carries the signed cert
/// and the effective TTL (after capping) so the client knows the real
/// validity window without re-parsing the cert.
#[derive(Debug, Serialize)]
pub struct IssueSshCertResponse {
    /// Signed OpenSSH user certificate bytes
    /// (`ssh-ed25519-cert-v01@openssh.com ...`). Caller writes this
    /// to a temp file and passes it via `ssh -o CertificateFile=`.
    pub certificate: String,
    /// Effective TTL in seconds after server-side capping.
    pub ttl_seconds: u64,
    /// SHA-256 fingerprint of the operator pubkey the cert binds to.
    /// Echoed back so the client (and the audit log) can match this
    /// issuance with the keypair the operator generated locally.
    pub pubkey_fingerprint: String,
}

/// `POST /api/v1/ssh/cert` handler. See module docs.
///
/// # Errors
///
/// Returns:
/// - `400 Bad Request` when the pubkey is not parseable OpenSSH.
/// - `422 Unprocessable Entity` when `principals` is empty.
/// - `500 Internal Server Error` when signing or journaling fails.
pub async fn post_ssh_cert(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<IssueSshCertBody>,
) -> Result<Json<IssueSshCertResponse>, Response> {
    if body.principals.is_empty() {
        return Err(error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "principals must not be empty",
        ));
    }

    let target = PublicKey::from_openssh(body.pubkey.trim())
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, format!("invalid pubkey: {e}")))?;

    let effective_ttl = cap_ttl(body.ttl_seconds);
    let cert_bytes = handles
        .ssh_ca
        .sign_user_cert(&target, &body.principals, effective_ttl, &body.comment)
        .map_err(|e| {
            warn!(error = %e, "sign_user_cert failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "sign failed")
        })?;
    let certificate = String::from_utf8(cert_bytes).map_err(|e| {
        warn!(error = %e, "signed cert is not UTF-8 OpenSSH (impossible)");
        error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal: cert encoding")
    })?;

    let fingerprint = target.fingerprint(HashAlg::Sha256).to_string();
    let metadata = serde_json::json!({
        "pubkey_fingerprint": fingerprint,
        "principals": body.principals,
        "ttl_seconds": effective_ttl.as_secs(),
        "comment": body.comment,
    })
    .to_string();
    if let Err(e) = handles
        .journal
        .insert(InsertEvent {
            host_id: None,
            kind: "ssh.cert.issued".into(),
            container_name: None,
            image: None,
            old_digest: None,
            new_digest: None,
            error: None,
            summary: format!(
                "ssh cert issued for principals {:?} (ttl={}s)",
                body.principals,
                effective_ttl.as_secs()
            ),
            metadata_json: Some(metadata),
            occurred_at: Utc::now(),
        })
        .await
    {
        warn!(error = %e, "journal insert for ssh.cert.issued failed");
        // Best-effort audit: still return the signed cert to the
        // caller. The cert is real and useable; missing audit is a
        // controller observability problem to surface separately.
    }

    Ok(Json(IssueSshCertResponse {
        certificate,
        ttl_seconds: effective_ttl.as_secs(),
        pubkey_fingerprint: fingerprint,
    }))
}

/// Cap `requested` seconds against the configured + absolute ceilings.
///
/// Reads `ISENGARD_SSH_CERT_MAX_TTL` once per request: keeps the
/// runtime override hot without restart. Invalid env values (non-
/// numeric, zero, above the absolute ceiling) fall back to
/// `DEFAULT_MAX_TTL_SECS`. The returned value is also pinned at
/// least 1 second so the cert builder never sees a zero window.
fn cap_ttl(requested: u64) -> Duration {
    let configured = std::env::var(MAX_TTL_ENV_VAR)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0 && n <= ABSOLUTE_MAX_TTL_SECS)
        .unwrap_or(DEFAULT_MAX_TTL_SECS);
    let secs = requested.min(configured).max(1);
    Duration::from_secs(secs)
}

/// Build a uniform JSON error body shaped like the rest of the
/// dashboard API.
fn error_response(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_ttl_respects_default_ceiling() {
        unsafe {
            std::env::remove_var(MAX_TTL_ENV_VAR);
        }
        assert_eq!(cap_ttl(60).as_secs(), 60);
        assert_eq!(cap_ttl(10_000).as_secs(), DEFAULT_MAX_TTL_SECS);
        assert_eq!(cap_ttl(0).as_secs(), 1);
    }

    #[test]
    fn cap_ttl_honors_env_override_within_absolute_bound() {
        unsafe {
            std::env::set_var(MAX_TTL_ENV_VAR, "7200");
        }
        let capped = cap_ttl(99_999);
        unsafe {
            std::env::remove_var(MAX_TTL_ENV_VAR);
        }
        assert_eq!(capped.as_secs(), 7200);
    }

    #[test]
    fn cap_ttl_rejects_env_above_absolute_bound() {
        unsafe {
            std::env::set_var(MAX_TTL_ENV_VAR, "999999");
        }
        let capped = cap_ttl(10_000);
        unsafe {
            std::env::remove_var(MAX_TTL_ENV_VAR);
        }
        assert_eq!(capped.as_secs(), DEFAULT_MAX_TTL_SECS);
    }
}
