//! Agent → controller enrollment.
//!
//! On first boot the agent has no CA cert to validate against. The join
//! token carries the SHA-256 of the controller's CA:
//! [`fetch_and_verify_ca`] fetches the controller's CA over skip-verify
//! TLS and confirms its digest matches the fingerprint embedded in the
//! packed token before the real Enroll RPC runs over an mTLS channel
//! rooted at the verified CA.
//!
//! The fingerprint-verified PEM is the only trust anchor: there is no
//! env-var or path fallback.

#![allow(clippy::result_large_err)]

use anyhow::{Context, Result, anyhow};

use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::{EnrollRequest, GetCaPemRequest, GetSshCaRequest};
use isengard_storage::host::HostId;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};

use crate::cert_store::CertBundle;

/// Trust anchor for the bootstrap channel. The only path
/// is the fingerprint flow: [`fetch_and_verify_ca`] populates
/// `verified_ca_pem` after confirming the CA fetched over skip-verify
/// TLS matches the SHA-256 embedded in the packed join token.
#[derive(Debug, Clone, Default)]
pub struct BootstrapTrust {
    /// CA PEM that has already been fingerprint-verified against
    /// the join token's embedded SHA-256.
    pub verified_ca_pem: Option<Vec<u8>>,
}

/// Resolved host metadata included in the EnrollRequest.
#[derive(Debug, Clone)]
pub struct HostInfo {
    /// Hostname the OS reports. Falls back to `"unknown"` when detection fails.
    pub hostname: String,
    /// Compile-time target OS (`linux`, `macos`, ...).
    pub os: String,
    /// Agent binary semver string.
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
    /// Controller-assigned stable host id (Ulid in db-bytes form).
    pub host_id: HostId,
    /// Signed cert bundle the agent dials mTLS with from now on.
    pub bundle: CertBundle,
    /// Heartbeat cadence the controller wants. Persisted to `agent.json`.
    pub heartbeat_interval_secs: u32,
}

/// Bootstrap-trust enrollment. The bootstrap channel's trust anchor is
/// the fingerprint-verified CA PEM from [`BootstrapTrust::verified_ca_pem`].
/// All subsequent RPCs run over an mTLS channel rooted at the CA returned
/// in `EnrollResponse`.
///
/// # Errors
///
/// Returns `Err` when the bootstrap trust is missing (missing fingerprint
/// verify), when the bootstrap channel can't dial the controller, or when
/// the `Enroll` RPC rejects the token.
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

/// Build the bootstrap channel TLS config. Only the fingerprint-verified
/// PEM is accepted; a `None` trust value means the caller did not run the
/// pre-enroll verify, which is a hard error pointing the operator at
/// minting a fresh packed token.
fn build_bootstrap_tls(trust: &BootstrapTrust) -> Result<ClientTlsConfig> {
    let pem = trust
        .verified_ca_pem
        .as_ref()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "token is missing the controller CA fingerprint. \
                 Mint a fresh token with `isd join-token` and re-run `isd join`."
            )
        })?;
    Ok(ClientTlsConfig::new().ca_certificate(Certificate::from_pem(pem)))
}

/// Pre-enroll fingerprint verify.
///
/// Calls the unauthenticated `GetCaPem` RPC over a skip-verify TLS
/// channel, compares the served CA's SHA-256 against the fingerprint
/// embedded in the packed token, and returns the verified PEM bytes
/// on match. Caller uses the returned PEM to build a properly-
/// validating bootstrap channel for the real `Enroll` RPC.
///
/// Threat model: the MITM window is one RPC long. A successful spoof
/// requires preimage resistance on SHA-256, which is computationally
/// infeasible. Same mechanic as `docker swarm join`.
pub async fn fetch_and_verify_ca(
    controller_url: &str,
    packed_token: &str,
) -> anyhow::Result<Vec<u8>> {
    let parsed = isengard_core::join_token::parse(packed_token)
        .map_err(|e| anyhow!("invalid token: {e}"))?;

    let channel = skip_verify_channel(controller_url)
        .await
        .with_context(|| format!("connect skip-verify channel to {controller_url}"))?;

    let mut client = ControllerClient::new(channel);
    let pem = client
        .get_ca_pem(GetCaPemRequest {})
        .await
        .context("GetCaPem RPC failed")?
        .into_inner()
        .pem;

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

/// Fetch the controller's SSH user-cert authority public key over the
/// fingerprint-verified bootstrap channel.
///
/// Run after `enroll()` succeeds but before mTLS-only operations
/// begin. The `GetSshCa` RPC is in the controller's `PUBLIC_METHODS`
/// allow-list, so the bootstrap TLS config (server-cert verified by
/// fingerprint) is enough to authenticate it.
///
/// # Errors
///
/// Returns `Err` when the bootstrap channel can't dial or when the
/// RPC itself fails.
pub async fn fetch_ssh_ca(controller_url: &str, trust: &BootstrapTrust) -> anyhow::Result<Vec<u8>> {
    let bootstrap_tls = build_bootstrap_tls(trust)?;
    let channel = tonic::transport::Channel::from_shared(controller_url.to_string())
        .with_context(|| format!("invalid controller url {controller_url:?}"))?
        .tls_config(bootstrap_tls)
        .context("install bootstrap tls config for GetSshCa")?
        .connect()
        .await
        .with_context(|| format!("connect bootstrap channel to {controller_url}"))?;

    let pubkey = ControllerClient::new(channel)
        .get_ssh_ca(GetSshCaRequest {})
        .await
        .context("GetSshCa RPC failed")?
        .into_inner()
        .pubkey;
    Ok(pubkey)
}

/// Build a tonic [`Channel`] that skips server-cert verification.
///
/// Only used for the one-shot pre-enroll `GetCaPem` RPC: the agent has
/// no CA yet, so tonic cannot validate the controller's server cert.
/// The fingerprint check the caller does after this is what restores
/// trust. Never reuse the returned channel for any other RPC.
///
/// Built as a custom `tower::Service<Uri>` connector that does TCP →
/// TLS handshake against the *original* `https://` URL, then hands the
/// already-TLS-wrapped stream to tonic via an `http://` URL so
/// tonic's connector doesn't try to TLS-wrap on top of it.
async fn skip_verify_channel(controller_url: &str) -> anyhow::Result<Channel> {
    let original: tonic::transport::Uri = controller_url
        .parse()
        .with_context(|| format!("invalid controller url {controller_url:?}"))?;
    let host = original
        .host()
        .ok_or_else(|| anyhow!("controller url {controller_url:?} missing host"))?
        .to_string();
    let port = original.port_u16().unwrap_or(9417);
    let connector = skip_verify::Connector::new(host, port)?;

    // Dial with an `http://` placeholder so tonic doesn't try to TLS
    // on top of the already-TLS-wrapped stream the connector returns.
    // The connector ignores the URI passed in `call`; it always
    // connects to the host:port it was constructed with.
    let placeholder = format!("http://{}:{}", original.host().unwrap(), port);
    let endpoint = Channel::from_shared(placeholder.clone())
        .with_context(|| format!("invalid placeholder uri {placeholder:?}"))?
        .timeout(std::time::Duration::from_secs(10));
    let channel = endpoint
        .connect_with_connector(connector)
        .await
        .with_context(|| format!("dial {controller_url}"))?;
    Ok(channel)
}

/// Skip-verify TLS connector for the one-shot pre-enroll `GetCaPem`
/// call. Wraps the TCP+TLS dance behind a `tower::Service<Uri>` so
/// tonic's `connect_with_connector` can drive it.
mod skip_verify {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use anyhow::{Context as _, Result};
    use hyper_util::rt::TokioIo;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::ring;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;
    use tonic::transport::Uri;

    #[derive(Debug)]
    /// Internal struct: NoVerify.
    struct NoVerify;

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    #[derive(Clone)]
    /// Internal struct: Connector.
    pub(super) struct Connector {
        /// `host` field.
        host: String,
        /// `port` field.
        port: u16,
        /// `tls` field.
        tls: TlsConnector,
        /// `server_name` field.
        server_name: ServerName<'static>,
    }

    impl Connector {
        /// Internal associated function: new.
        pub(super) fn new(host: String, port: u16) -> Result<Self> {
            let mut tls = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth();
            // gRPC is HTTP/2 only; advertise h2 via ALPN so the server
            // accepts the handshake.
            tls.alpn_protocols = vec![b"h2".to_vec()];
            let server_name = ServerName::try_from(host.clone())
                .with_context(|| format!("invalid TLS server name {host:?}"))?;
            Ok(Self {
                host,
                port,
                tls: TlsConnector::from(Arc::new(tls)),
                server_name,
            })
        }
    }

    impl tower::Service<Uri> for Connector {
        type Response = TokioIo<tokio_rustls::client::TlsStream<TcpStream>>;
        type Error = anyhow::Error;
        type Future =
            Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _uri: Uri) -> Self::Future {
            let host = self.host.clone();
            let port = self.port;
            let tls = self.tls.clone();
            let name = self.server_name.clone();
            Box::pin(async move {
                let tcp = TcpStream::connect((host.as_str(), port))
                    .await
                    .with_context(|| format!("tcp connect {host}:{port}"))?;
                let stream = tls.connect(name, tcp).await.context("tls handshake")?;
                Ok(TokioIo::new(stream))
            })
        }
    }
}

/// Internal helper: hex full.
fn hex_full(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod bootstrap_tls_tests {
    //! `build_bootstrap_tls` only accepts the fingerprint-verified PEM.
    //! Missing / empty PEM hard-fails with an operator-actionable error
    //! pointing at `isd join-token`.

    use super::{BootstrapTrust, build_bootstrap_tls};

    #[test]
    fn verified_pem_is_accepted() {
        let pem = b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".to_vec();
        let trust = BootstrapTrust {
            verified_ca_pem: Some(pem),
        };
        // Just asserting the function doesn't error on a real PEM. The CA is
        // not parsed here: tonic does that lazily when the channel handshakes.
        build_bootstrap_tls(&trust).unwrap();
    }

    #[test]
    fn missing_verified_pem_surfaces_operator_pointer() {
        let trust = BootstrapTrust {
            verified_ca_pem: None,
        };
        let err = build_bootstrap_tls(&trust).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("missing the controller CA fingerprint"),
            "{rendered}"
        );
        assert!(rendered.contains("isd join-token"), "{rendered}");
    }

    #[test]
    fn empty_verified_pem_surfaces_operator_pointer() {
        let trust = BootstrapTrust {
            verified_ca_pem: Some(Vec::new()),
        };
        let err = build_bootstrap_tls(&trust).unwrap_err();
        assert!(format!("{err:#}").contains("missing the controller CA fingerprint"));
    }
}
