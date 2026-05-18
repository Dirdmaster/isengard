//! Phase 14: agent → controller enrollment.
//!
//! On first boot the agent has no CA cert to validate against. Trust for the
//! bootstrap `Enroll` channel is resolved in this order:
//!
//!   1. `ISENGARD_CONTROLLER_CA_PEM_PATH` env var (or `--controller-ca-pem-path`
//!      CLI arg): read the CA root cert PEM from disk and pin it.
//!   2. `ISENGARD_CONTROLLER_CA_PEM_BASE64` env var: standard-alphabet base64
//!      of the CA root PEM. Decoded once at startup and pinned. This is the
//!      form the swarm-style join command emits (single-line, shell-safe).
//!   3. `ISENGARD_CONTROLLER_CA_PEM` env var: inline PEM string, pin directly.
//!      Multiline; works for `docker run -e VAR="$(cat ca.pem)"` but breaks
//!      in many shells, hence the base64 form.
//!   4. Fallback: trust the platform's native root store. Works when the
//!      controller serves a publicly-signed cert (e.g. Let's Encrypt) but
//!      FAILS for the default self-signed internal CA: operators running
//!      that setup MUST pass a pinned CA via 1, 2, or 3.
//!
//! Every RPC after Enroll runs over an mTLS channel rooted at the CA returned
//! in `EnrollResponse.ca_root_pem` (which the agent persists and reuses).

#![allow(clippy::result_large_err)]

use anyhow::{Context, Result, anyhow};
use base64::Engine;

use isengard_proto::pb::EnrollRequest;
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_storage::host::HostId;
use tonic::transport::{Certificate, ClientTlsConfig};

use crate::cert_store::CertBundle;

/// Env var pointing at a PEM file containing the controller's CA root cert.
/// Read once at enrollment to pin the bootstrap channel.
pub const CONTROLLER_CA_PEM_PATH_ENV: &str = "ISENGARD_CONTROLLER_CA_PEM_PATH";
/// Env var carrying base64 (standard alphabet, padded) of the controller's CA
/// root PEM. Single-line, shell-safe: this is what the swarm-style
/// `controller token mint` join command embeds. Decoded once at startup.
pub const CONTROLLER_CA_PEM_BASE64_ENV: &str = "ISENGARD_CONTROLLER_CA_PEM_BASE64";
/// Env var carrying the controller's CA root cert PEM inline. Used when a
/// file path isn't convenient (e.g. CI secrets). Multiline; prefer the
/// `_BASE64` form for shell-safe transport.
pub const CONTROLLER_CA_PEM_ENV: &str = "ISENGARD_CONTROLLER_CA_PEM";

/// Optional pinned CA material for the bootstrap channel. Resolution order
/// inside [`enroll`] is verified > path env > inline env > caller-supplied >
/// native roots.
#[derive(Debug, Clone, Default)]
pub struct BootstrapTrust {
    /// Path to a PEM file holding the controller CA. Equivalent to setting
    /// `ISENGARD_CONTROLLER_CA_PEM_PATH` but plumbed through `AgentOptions`.
    pub ca_pem_path: Option<std::path::PathBuf>,
    /// Inline PEM bytes. Equivalent to `ISENGARD_CONTROLLER_CA_PEM`.
    pub ca_pem: Option<String>,
    /// Track F: CA PEM that has already been fingerprint-verified against
    /// the join token's embedded SHA-256. When set, this wins over every
    /// env-var path: a verified PEM is the cryptographic source of truth
    /// and must not be overridden by a stale `ISENGARD_CONTROLLER_CA_PEM_*`
    /// left over from a previous mint.
    pub verified_ca_pem: Option<Vec<u8>>,
}

/// Resolved host metadata included in the EnrollRequest.
#[derive(Debug, Clone)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub version: String,
}

impl HostInfo {
    /// Best-effort host detection. `hostname` falls back to `"unknown"` if the
    /// OS call fails or returns non-UTF-8 bytes.
    pub fn detect() -> Self {
        Self {
            hostname: hostname::get()
                .ok()
                .and_then(|s| s.into_string().ok())
                .unwrap_or_else(|| "unknown".into()),
            os: std::env::consts::OS.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// Successful enrollment outcome. Caller persists `bundle` via
/// [`crate::cert_store::save`] and `host_id` + `heartbeat_interval_secs` via
/// [`crate::agent_state::save`] before any subsequent RPC.
#[derive(Debug)]
pub struct EnrollOutcome {
    pub host_id: HostId,
    pub bundle: CertBundle,
    pub heartbeat_interval_secs: u32,
}

/// Bootstrap-trust enrollment. The bootstrap channel's trust anchor is
/// resolved per [`BootstrapTrust`] / env-var precedence (see module docs).
/// All subsequent RPCs run over an mTLS channel rooted at the CA returned
/// in `EnrollResponse`.
pub async fn enroll(
    controller_url: &str,
    enroll_token: &str,
    host_info: HostInfo,
    trust: BootstrapTrust,
) -> Result<EnrollOutcome> {
    let bootstrap_tls = build_bootstrap_tls(&trust)?;

    let channel = tonic::transport::Channel::from_shared(controller_url.to_string())
        .with_context(|| format!("invalid controller url {controller_url:?}"))?
        .tls_config(bootstrap_tls)
        .context("install bootstrap tls config")?
        .connect()
        .await
        .with_context(|| format!("connect bootstrap channel to {controller_url}"))?;

    let mut client = ControllerClient::new(channel);
    let resp = client
        .enroll(EnrollRequest {
            token: enroll_token.to_string(),
            hostname: host_info.hostname,
            os: host_info.os,
            version: host_info.version,
        })
        .await
        .context("Enroll RPC failed")?
        .into_inner();

    let host_id = HostId::from_db_bytes(resp.host_id)
        .map_err(|e| anyhow!("invalid host_id from controller: {e}"))?;
    let bundle = CertBundle {
        ca_pem: resp.ca_root_pem,
        cert_pem: resp.agent_cert_pem,
        key_pem: resp.agent_key_pem,
    };

    Ok(EnrollOutcome {
        host_id,
        bundle,
        heartbeat_interval_secs: resp.heartbeat_interval_secs,
    })
}

/// Resolve a [`ClientTlsConfig`] for the bootstrap channel. Precedence:
/// fingerprint-verified PEM > path env > base64 env > inline-pem env >
/// caller-provided path > caller-provided inline > native roots fallback.
/// The first source that yields a non-empty PEM wins.
fn build_bootstrap_tls(trust: &BootstrapTrust) -> Result<ClientTlsConfig> {
    if let Some(pem) = trust.verified_ca_pem.as_ref() {
        if !pem.is_empty() {
            return Ok(ClientTlsConfig::new().ca_certificate(Certificate::from_pem(pem)));
        }
    }
    if let Ok(path) = std::env::var(CONTROLLER_CA_PEM_PATH_ENV) {
        if !path.is_empty() {
            let pem = std::fs::read_to_string(&path).with_context(|| {
                format!("reading {CONTROLLER_CA_PEM_PATH_ENV}={path:?} for bootstrap CA")
            })?;
            return Ok(pin_ca(&pem));
        }
    }
    if let Ok(b64) = std::env::var(CONTROLLER_CA_PEM_BASE64_ENV) {
        if !b64.is_empty() {
            let pem = decode_base64_ca(&b64)?;
            return Ok(pin_ca(&pem));
        }
    }
    if let Ok(pem) = std::env::var(CONTROLLER_CA_PEM_ENV) {
        if !pem.is_empty() {
            return Ok(pin_ca(&pem));
        }
    }
    if let Some(path) = trust.ca_pem_path.as_ref() {
        let pem = std::fs::read_to_string(path)
            .with_context(|| format!("reading bootstrap CA from {path:?}"))?;
        return Ok(pin_ca(&pem));
    }
    if let Some(pem) = trust.ca_pem.as_ref() {
        if !pem.is_empty() {
            return Ok(pin_ca(pem));
        }
    }
    // Imp-6: make the bootstrap-trust fallback noisy. With no pin, the
    // agent will only succeed against a controller serving a publicly
    // signed cert (e.g. Let's Encrypt). The default Phase 14 deployment
    // is self-signed (internal CA), so this almost always means the
    // operator forgot to wire the CA.
    tracing::warn!(
        "no controller CA pinned (ISENGARD_CONTROLLER_CA_PEM_PATH, _BASE64, or _PEM); \
         falling back to system trust store. This will fail with self-signed \
         CAs: re-run `isengard controller token mint --role agent` to get a \
         join command with the CA inlined, or use `isengard controller ca export`."
    );
    Ok(ClientTlsConfig::new().with_native_roots())
}

fn pin_ca(pem: &str) -> ClientTlsConfig {
    ClientTlsConfig::new().ca_certificate(Certificate::from_pem(pem.as_bytes()))
}

/// Track F pre-enroll fingerprint verify.
///
/// Fetches the controller's CA PEM over skip-verify TLS, compares its
/// SHA-256 against the fingerprint embedded in the packed token, and
/// returns the verified PEM bytes on match. Caller uses the returned
/// PEM to build a properly-validating reqwest::Client (or tonic channel)
/// for the actual Enroll RPC.
///
/// Threat model: the MITM window is one HTTP request long. Successfully
/// spoofing the CA requires preimage resistance on SHA-256, which is
/// computationally infeasible. Same mechanic as `docker swarm join`.
pub async fn fetch_and_verify_ca(
    controller_url: &str,
    packed_token: &str,
) -> anyhow::Result<Vec<u8>> {
    let parsed = isengard_core::join_token::parse(packed_token)
        .map_err(|e| anyhow!("invalid token: {e}"))?;

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow!("building skip-verify client: {e}"))?;

    let url = format!("{}/api/v1/ca/pem", controller_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("fetching CA from controller: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("controller returned {status} for GET {url}"));
    }
    let pem = resp
        .bytes()
        .await
        .map_err(|e| anyhow!("reading CA response body: {e}"))?
        .to_vec();

    let actual = isengard_core::join_token::fingerprint(&pem);
    if actual != parsed.fingerprint {
        return Err(anyhow!(
            "controller CA fingerprint mismatch: token says {} but controller served CA with fingerprint {}. \
             Token was either (a) minted against a different controller, (b) intercepted, or (c) the controller's CA was rotated since the token was minted",
            hex_full(&parsed.fingerprint),
            hex_full(&actual)
        ));
    }

    Ok(pem)
}

fn hex_full(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode the value of `ISENGARD_CONTROLLER_CA_PEM_BASE64` into a PEM string.
/// Whitespace inside the value is stripped (docker/compose may wrap long
/// values onto multiple lines). Returns a precise error mentioning the env
/// var name on either base64 decode failure or non-UTF-8 output.
fn decode_base64_ca(value: &str) -> Result<String> {
    let cleaned: String = value.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .with_context(|| {
            format!("decoding {CONTROLLER_CA_PEM_BASE64_ENV} (expected standard base64)")
        })?;
    String::from_utf8(bytes).with_context(|| {
        format!("{CONTROLLER_CA_PEM_BASE64_ENV} did not decode to valid UTF-8 PEM")
    })
}

#[cfg(test)]
mod bootstrap_tls_tests {
    //! Resolution-order checks for [`build_bootstrap_tls`]. We can't easily
    //! assert the resulting `ClientTlsConfig` (no public accessors), so these
    //! exercise the I/O side: missing files surface, present files are read.

    use super::{BootstrapTrust, build_bootstrap_tls};
    use std::io::Write;

    #[test]
    fn caller_provided_path_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n")
            .unwrap();
        let trust = BootstrapTrust {
            ca_pem_path: Some(path),
            ca_pem: None,
            verified_ca_pem: None,
        };
        // Just asserting the function doesn't error on a real file. The CA is
        // not parsed here — tonic does that lazily when the channel handshakes.
        build_bootstrap_tls(&trust).unwrap();
    }

    #[test]
    fn missing_caller_path_surfaces_error() {
        let trust = BootstrapTrust {
            ca_pem_path: Some("/nonexistent/path/ca.pem".into()),
            ca_pem: None,
            verified_ca_pem: None,
        };
        let err = build_bootstrap_tls(&trust).unwrap_err();
        assert!(format!("{err:#}").contains("bootstrap CA"));
    }
}

#[cfg(test)]
mod base64_ca_tests {
    //! Unit checks for the `_BASE64` env var decode helper. Exercised here
    //! rather than via [`build_bootstrap_tls`] to keep tests env-free
    //! (parallel-safe).

    use super::{CONTROLLER_CA_PEM_BASE64_ENV, decode_base64_ca};
    use base64::Engine;

    const STUB_PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";

    #[test]
    fn round_trips_pem() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(STUB_PEM);
        let decoded = decode_base64_ca(&encoded).unwrap();
        assert_eq!(decoded, STUB_PEM);
    }

    #[test]
    fn tolerates_internal_whitespace() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(STUB_PEM);
        // Mimic a docker-compose .env line wrapped after 76 chars.
        let mut wrapped = String::new();
        for (i, ch) in encoded.chars().enumerate() {
            if i > 0 && i % 32 == 0 {
                wrapped.push('\n');
            }
            wrapped.push(ch);
        }
        let decoded = decode_base64_ca(&wrapped).unwrap();
        assert_eq!(decoded, STUB_PEM);
    }

    #[test]
    fn invalid_base64_surfaces_named_error() {
        let err = decode_base64_ca("not-base64!@#$").unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains(CONTROLLER_CA_PEM_BASE64_ENV),
            "error should mention env var, got: {rendered}"
        );
    }

    #[test]
    fn non_utf8_surfaces_named_error() {
        // 0xFF 0xFE is not valid UTF-8; encode and decode.
        let raw = [0xFFu8, 0xFE, 0xFD];
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let err = decode_base64_ca(&encoded).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains(CONTROLLER_CA_PEM_BASE64_ENV),
            "error should mention env var, got: {rendered}"
        );
    }
}
