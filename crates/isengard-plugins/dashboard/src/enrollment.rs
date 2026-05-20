//! REST endpoints for enrollment-token management + per-host cert revocation
//!
//! Endpoints (mounted under `/api/v1`):
//!
//! | Method | Path                             | Purpose                                        |
//! |--------|----------------------------------|------------------------------------------------|
//! | POST   | `/enrollment/tokens`             | Mint a fresh enrollment token (shown once).    |
//! | GET    | `/enrollment/tokens`             | List active (unexpired, unconsumed) tokens.    |
//! | DELETE | `/enrollment/tokens/:hash_prefix`| Revoke an active token by its hash prefix.     |
//! | DELETE | `/hosts/:host_id/cert`           | Revoke the active leaf cert for a host.        |
//!
//! Token list rows expose only the first 8 bytes of the SHA-256 hash (16 hex
//! chars) so the dashboard can identify a row without ever holding the
//! plaintext token (which is shown to the operator once at mint time).

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{delete, post};
use chrono::Duration;
use isengard_controller::ControllerHandles;
use isengard_controller::revocation::revoke_agent;
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::host::HostId;
use serde::{Deserialize, Serialize};

/// Bytes of `token_hash` exposed in list/delete URLs. SHA-256 prefix
/// collisions in 8 bytes are negligible for the per-controller token
/// population we expect (a handful at any one time).
const HASH_PREFIX_LEN: usize = 8;

/// Builds the axum router for this resource.
pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/enrollment/tokens", post(mint_token).get(list_tokens))
        .route("/enrollment/tokens/{hash_prefix}", delete(revoke_token))
        .route("/hosts/{host_id}/cert", delete(revoke_host_cert))
        .with_state(handles)
}

#[derive(Debug, Deserialize)]
/// MintTokenBody.
pub struct MintTokenBody {
    /// Currently always `"agent"`; reserved for future controller-admin tokens.
    pub role: String,
    /// Token validity in seconds. Bounded to `1..=86_400` (1s..1 day) so a
    /// fat-fingered request can't mint a permanent invitation.
    pub ttl_seconds: u64,
}

#[derive(Debug, Serialize, Deserialize)]
/// MintTokenResponse.
pub struct MintTokenResponse {
    /// Plaintext token. Shown to the operator once; only the SHA-256 hash is
    /// stored on the controller.
    pub token: String,
    /// RFC3339 expiry. Computed client-side from the requested TTL — the
    /// stored row uses the same `now + ttl` clock.
    pub expires_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
/// TokenListEntry.
pub struct TokenListEntry {
    /// Hex-encoded first [`HASH_PREFIX_LEN`] bytes of `token_hash`. Used as the
    /// stable identifier in delete URLs.
    pub hash_prefix: String,
    /// `role` field.
    pub role: String,
    /// `expires_at` field.
    pub expires_at: String,
    /// `created_at` field.
    pub created_at: String,
}

/// `TTL_MIN_SECS` constant.
const TTL_MIN_SECS: u64 = 1;
/// `TTL_MAX_SECS` constant.
const TTL_MAX_SECS: u64 = 86_400;

/// `mint_token`.
async fn mint_token(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<MintTokenBody>,
) -> Result<(StatusCode, Json<MintTokenResponse>), (StatusCode, String)> {
    let role = match body.role.as_str() {
        "agent" => TokenRole::Agent,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown role '{other}' (expected 'agent')"),
            ));
        }
    };

    if !(TTL_MIN_SECS..=TTL_MAX_SECS).contains(&body.ttl_seconds) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "ttl_seconds must be in {TTL_MIN_SECS}..={TTL_MAX_SECS}, got {}",
                body.ttl_seconds
            ),
        ));
    }

    let ttl = Duration::seconds(body.ttl_seconds as i64);
    let expires_at = chrono::Utc::now() + ttl;
    let token = handles
        .enrollment
        .mint(role, ttl)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("mint: {e}")))?;

    Ok((
        StatusCode::CREATED,
        Json(MintTokenResponse {
            token,
            expires_at: expires_at.to_rfc3339(),
        }),
    ))
}

/// `GET` handler for tokens.
async fn list_tokens(
    State(handles): State<Arc<ControllerHandles>>,
) -> Result<Json<Vec<TokenListEntry>>, (StatusCode, String)> {
    let rows = handles
        .inventory
        .list_active_tokens()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("list: {e}")))?;

    let entries = rows
        .into_iter()
        .map(|r| TokenListEntry {
            hash_prefix: hex_encode(&r.token_hash[..HASH_PREFIX_LEN.min(r.token_hash.len())]),
            role: token_role_str(r.role).to_string(),
            expires_at: r.expires_at.to_rfc3339(),
            created_at: r.created_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(entries))
}

/// `DELETE /enrollment/tokens/:hash_prefix`. Imp-4 fix: marks the matching
/// active token as cancelled (NEW dedicated `cancelled_at` column from
/// migration 0015) so it can never be redeemed. Pre-fix the handler faked
/// a consumption with a sentinel `HostId::new()` to lock the token out,
/// which polluted the audit trail by making it look like a fresh-ULID
/// host had enrolled.
async fn revoke_token(
    State(handles): State<Arc<ControllerHandles>>,
    Path(hash_prefix): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    if hash_prefix.len() != HASH_PREFIX_LEN * 2
        || !hash_prefix.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "hash_prefix must be {} hex chars, got {:?}",
                HASH_PREFIX_LEN * 2,
                hash_prefix
            ),
        ));
    }

    let prefix_bytes = match hex_decode(&hash_prefix) {
        Some(b) => b,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                "hash_prefix is not valid hex".into(),
            ));
        }
    };

    let active = handles
        .inventory
        .list_active_tokens()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("list: {e}")))?;

    let target = active
        .into_iter()
        .find(|t| t.token_hash.starts_with(&prefix_bytes));

    let Some(token) = target else {
        return Err((
            StatusCode::NOT_FOUND,
            "no active token matches the given prefix".into(),
        ));
    };

    handles
        .inventory
        .cancel_enrollment_token(&token.token_hash)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("cancel: {e}")))?;

    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /hosts/:host_id/cert`. Revokes the active leaf cert for the host
/// (both persistently in the `agent_certs` table and in the in-memory
/// revocation set so the next RPC from that host is rejected).
async fn revoke_host_cert(
    State(handles): State<Arc<ControllerHandles>>,
    Path(host_id_str): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let ulid = ulid::Ulid::from_string(&host_id_str)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid host_id: {e}")))?;
    let host_id = HostId::from(ulid);

    // Confirm the host exists before attempting revocation so we can return a
    // clean 404 distinct from the "host exists but has no active cert" case
    // that revoke_agent surfaces as a generic error.
    let exists = handles
        .inventory
        .get_host(host_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("get_host: {e}")))?
        .is_some();
    if !exists {
        return Err((StatusCode::NOT_FOUND, "host not found".into()));
    }

    revoke_agent(
        &handles.inventory,
        &handles.revocation,
        host_id,
        "dashboard-revoke",
    )
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.contains("no active cert") {
            (StatusCode::NOT_FOUND, msg)
        } else {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("revoke: {e}"))
        }
    })?;

    Ok(StatusCode::NO_CONTENT)
}

/// `token_role_str`.
fn token_role_str(role: TokenRole) -> &'static str {
    match role {
        TokenRole::Agent => "agent",
    }
}

/// `hex_encode`.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// `hex_decode`.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in bytes.chunks(2) {
        let hi = nibble(chunk[0])?;
        let lo = nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

/// `nibble`.
fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
