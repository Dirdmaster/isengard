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
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use isengard_controller::ControllerHandles;
use isengard_storage::InsertEvent;
use isengard_storage::journal::EventRow;
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

/// Response body for `GET /api/v1/ssh/ca`. Carries the controller's
/// SSH user CA pubkey in OpenSSH wire format so operators (and the
/// CLI) can drop it into a `TrustedUserCAKeys` file or compare against
/// what an agent already trusts.
#[derive(Debug, Serialize)]
pub struct CaPubkeyResponse {
    /// SSH CA pubkey in OpenSSH `authorized_keys` format
    /// (`ssh-ed25519 AAAA...`). The exact bytes the agent's sshd
    /// drop-in references via `TrustedUserCAKeys`.
    pub pubkey: String,
}

/// `GET /api/v1/ssh/ca` handler. Returns the controller's SSH CA
/// pubkey in OpenSSH wire format. Read-only; safe to expose to any
/// caller that already reached the dashboard surface.
pub async fn get_ssh_ca_pubkey(
    State(handles): State<Arc<ControllerHandles>>,
) -> Json<CaPubkeyResponse> {
    let pubkey = String::from_utf8_lossy(handles.ssh_ca.public_key_openssh()).to_string();
    Json(CaPubkeyResponse { pubkey })
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

/// Default cap on returned audit rows. Matches the dashboard's other
/// list endpoints (e.g. `GET /events`) and keeps the payload small for
/// the common `isd ssh audit` interactive call.
const DEFAULT_AUDIT_LIMIT: usize = 100;

/// Hard upper bound on rows the audit endpoint will return. Mirrors
/// the journal scan ceiling already used by `list_events` so a
/// misbehaving client cannot ask the controller to walk an unbounded
/// slice of the journal.
const MAX_AUDIT_LIMIT: usize = 5_000;

/// Journal-scan ceiling. We filter the `ssh.cert.*` slice in memory,
/// so the SQL `LIMIT` must be wide enough that unrelated newer events
/// do not push every cert issuance out of the window. 5k matches the
/// `deployment_id` filter path in `list_events`.
const JOURNAL_SCAN_LIMIT: i64 = 5_000;

/// Query params for `GET /api/v1/ssh/audit`. Both fields are optional.
#[derive(Debug, Deserialize, Default)]
pub struct AuditQuery {
    /// Inclusive lower bound on `occurred_at`. Accepts any RFC3339
    /// timestamp; entries with `occurred_at < since` are dropped.
    pub since: Option<String>,
    /// Cap on returned entries. Defaults to [`DEFAULT_AUDIT_LIMIT`]
    /// and is clamped to [`MAX_AUDIT_LIMIT`].
    pub limit: Option<usize>,
}

/// One row in the `GET /api/v1/ssh/audit` response. The shape is
/// projected from [`EventRow`]; we keep only the columns operators
/// actually read when answering "who minted what, when?".
#[derive(Debug, Serialize)]
pub struct SshAuditEntry {
    /// Journal row id. Stable across reads, useful for `isd ssh audit`
    /// to deduplicate across paginated calls.
    pub id: i64,
    /// Event kind. Always begins with `ssh.cert.` for rows returned
    /// here (currently `ssh.cert.issued`; revocation lands in v0.2).
    pub kind: String,
    /// When the event happened, RFC3339 in UTC.
    pub occurred_at: String,
    /// Operator-readable one-line summary.
    pub summary: String,
    /// Decoded metadata payload. Carries `pubkey_fingerprint`,
    /// `principals`, `ttl_seconds`, `comment` for `ssh.cert.issued`.
    pub metadata: serde_json::Value,
}

impl From<EventRow> for SshAuditEntry {
    fn from(r: EventRow) -> Self {
        let metadata = r
            .metadata_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::Value::Null);
        Self {
            id: r.id,
            kind: r.kind,
            occurred_at: r.occurred_at.to_rfc3339(),
            summary: r.summary,
            metadata,
        }
    }
}

/// `GET /api/v1/ssh/audit` handler. Returns the journal's
/// `ssh.cert.*` slice, newest-first.
///
/// Filters: `?since=<RFC3339>` (inclusive lower bound on
/// `occurred_at`) and `?limit=<N>` (default 100, capped at 5000).
///
/// # Errors
///
/// Returns:
/// - `400 Bad Request` when `since` is set but not parseable RFC3339.
/// - `500 Internal Server Error` when the journal read fails.
pub async fn get_ssh_audit(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Vec<SshAuditEntry>>, Response> {
    let since = match q.since.as_deref() {
        Some(s) => Some(
            DateTime::parse_from_rfc3339(s)
                .map_err(|e| {
                    error_response(StatusCode::BAD_REQUEST, format!("invalid since: {e}"))
                })?
                .with_timezone(&Utc),
        ),
        None => None,
    };
    let limit = q
        .limit
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .clamp(1, MAX_AUDIT_LIMIT);

    let rows = handles
        .journal
        .list_recent(JOURNAL_SCAN_LIMIT)
        .await
        .map_err(|e| {
            warn!(error = %e, "ssh audit: journal list_recent failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "journal read failed")
        })?;

    let entries: Vec<SshAuditEntry> = rows
        .into_iter()
        .filter(|r| r.kind.starts_with("ssh.cert."))
        .filter(|r| match since {
            Some(cutoff) => r.occurred_at >= cutoff,
            None => true,
        })
        .take(limit)
        .map(SshAuditEntry::from)
        .collect();

    Ok(Json(entries))
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
