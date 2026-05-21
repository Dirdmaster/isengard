//! Per-RPC client cert validation plus revocation check.
//!
//! Every RPC except the two in `PUBLIC_METHODS` requires a valid client
//! certificate signed by the controller's internal CA. The interceptor
//! additionally checks the cert's serial against the in-memory
//! [`RevocationSet`] so a revoked agent is rejected on the next RPC, not
//! at the next reconnect.
//!
//! `GetCaPem` is public because an agent that hasn't enrolled yet pins
//! its trust anchor over a skip-verify TLS channel. `Enroll` is public
//! because the same agent has no cert to present yet: the join token is
//! what gates that call. The tonic server is configured with
//! `client_auth_optional(true)` so both calls can complete without a
//! client cert; this layer then enforces cert presence per-RPC for
//! everything else.
//!
//! This is a `tower::Layer` rather than a `tonic::service::Interceptor`
//! because tonic interceptors receive `tonic::Request<()>`, which loses
//! the URI and the request extensions during the conversion in
//! `InterceptedService`. The URI is what dispatches by method
//! (`/isengard.v1.Controller/Enroll`) and the extensions are where
//! `TlsConnectInfo` lives. Both are still on the raw `http::Request`
//! the tower layer sees.

#![allow(clippy::result_large_err)]

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{Request, Response};
use tonic::Status;
use tonic::body::BoxBody;
use tonic::transport::server::TlsConnectInfo;
use tower::{Layer, Service};

use crate::ca::Authority;
use crate::revocation::RevocationSet;

/// RPC method paths that bypass mTLS, listed as the URI paths tonic
/// dispatches on.
///
/// Both members solve a bootstrap chicken-and-egg:
///
/// - `GetCaPem` returns the controller's CA root PEM. The agent fetches
///   this over a skip-verify TLS channel during pre-enroll fingerprint
///   verification; the response is public data, and the agent has no
///   cert to present yet.
/// - `Enroll` redeems a one-time join token for a freshly minted mTLS
///   cert. The token (not a cert) gates the call; every subsequent RPC
///   uses the new cert.
/// - `GetSshCa` returns the controller's SSH user-cert authority
///   public key. Agents install it as a `TrustedUserCAKeys` drop-in
///   right after enrollment. Response is public data (a pubkey).
const PUBLIC_METHODS: &[&str] = &[
    "/isengard.v1.Controller/GetCaPem",
    "/isengard.v1.Controller/Enroll",
    "/isengard.v1.Controller/GetSshCa",
];

/// Tower layer that wraps a service in [`CertAuth`].
///
/// Cheap to clone: the interior state is an [`Arc`].
#[derive(Clone)]
pub struct CertAuthInterceptor {
    /// Shared revocation set the inner service checks every request
    /// against. Cloning the layer clones the same `Arc<RwLock<_>>`.
    revocation: RevocationSet,
}

impl CertAuthInterceptor {
    /// Builds an interceptor.
    ///
    /// `_ca` is unused now: the per-handler cert CN extraction lives in
    /// `service.rs` (the Bl-1 and Bl-2 fixes), not at the layer level.
    /// The parameter stays in the signature so existing call sites in
    /// `lib.rs` and the test harnesses do not churn.
    pub fn new(revocation: RevocationSet, _ca: Arc<Authority>) -> Self {
        Self { revocation }
    }
}

impl<S> Layer<S> for CertAuthInterceptor {
    type Service = CertAuth<S>;
    fn layer(&self, inner: S) -> Self::Service {
        CertAuth {
            inner,
            revocation: self.revocation.clone(),
        }
    }
}

/// Tower service that gates each request behind cert presence and the
/// revocation check.
#[derive(Clone)]
pub struct CertAuth<S> {
    /// Wrapped service that handles the request after the gate passes.
    inner: S,
    /// Shared revocation set, read on every call.
    revocation: RevocationSet,
}

impl<S, B> Service<Request<B>> for CertAuth<S>
where
    S: Service<Request<B>, Response = Response<BoxBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let mut inner = self.inner.clone();
        let revocation = self.revocation.clone();

        Box::pin(async move {
            let path = req.uri().path();

            // gRPC reflection passes through unauthenticated so `grpcurl` can
            // still introspect the schema without a client cert. The Enroll
            // RPC is public for the same bootstrap reason.
            if path.starts_with("/grpc.reflection.") || PUBLIC_METHODS.contains(&path) {
                return inner.call(req).await;
            }

            // The TlsConnectInfo extension is set by tonic's TLS server only
            // when a client cert was presented and validated against the
            // configured CA. With `client_auth_optional(true)`, a missing
            // extension means the client connected without a cert.
            let peer_certs = req
                .extensions()
                .get::<TlsConnectInfo<tonic::transport::server::TcpConnectInfo>>()
                .and_then(|info| info.peer_certs());

            let Some(peer_certs) = peer_certs else {
                return Ok(Status::unauthenticated("client cert required").into_http());
            };

            let Some(cert) = peer_certs.first() else {
                return Ok(Status::unauthenticated("no client cert presented").into_http());
            };

            let Some(serial) = extract_serial(cert.as_ref()) else {
                return Ok(Status::unauthenticated("client cert has no serial").into_http());
            };

            // Bl-3 defense in depth. extract_serial already rejects empty
            // serials, but if the parser ever changes its behavior we'd
            // rather fail closed than skip the revocation check entirely
            // (an empty serial would never match any revoked entry).
            if serial.is_empty() {
                return Ok(Status::unauthenticated("client cert has empty serial").into_http());
            }

            if revocation.contains(&serial) {
                return Ok(Status::unauthenticated("client cert revoked").into_http());
            }

            inner.call(req).await
        })
    }
}

/// Pulls the X.509 serial out of a DER-encoded cert.
///
/// The layer uses this to look the serial up in the [`RevocationSet`].
/// `raw_serial` returns the ASN.1 INTEGER bytes verbatim. When the high
/// bit of the original 16-byte random serial happens to be set, ASN.1
/// prepends a leading `0x00` to keep the integer positive; the strip
/// step undoes that so the bytes match what `agent_certs.serial` holds
/// (the raw 16 bytes from `OsRng`).
///
/// Returns `None` for empty serials (the Bl-3 fix). A serial encoded as
/// the single byte `0x00` collapses to `[]` after sign-pad stripping;
/// an empty serial would never match any entry in the revocation set,
/// so a successful lookup would silently bypass revocation. The layer
/// treats this as a malformed cert and rejects upstream.
fn extract_serial(cert_der: &[u8]) -> Option<Vec<u8>> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der).ok()?;
    let raw = cert.tbs_certificate.raw_serial();
    let stripped = strip_asn1_sign_pad(raw);
    if stripped.is_empty() {
        return None;
    }
    Some(stripped.to_vec())
}

/// Strips the ASN.1 INTEGER positive-sign padding byte if present.
///
/// A leading `0x00` is significant only when the next byte's MSB is set
/// (the bytes would otherwise read as a negative integer). When the
/// next byte's MSB is clear the leading zero is either non-canonical or
/// absent.
fn strip_asn1_sign_pad(raw: &[u8]) -> &[u8] {
    match raw {
        [0x00, rest @ ..] if rest.first().is_some_and(|b| b & 0x80 != 0) => rest,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::strip_asn1_sign_pad;

    #[test]
    fn no_padding_passes_through() {
        assert_eq!(strip_asn1_sign_pad(&[0x12, 0x34]), &[0x12, 0x34]);
    }

    #[test]
    fn padded_high_bit_byte_strips_leading_zero() {
        // 0x00 prefix is required by ASN.1 INTEGER to keep the value positive
        // when the first significant byte's MSB is set; the canonical form
        // we persist is the post-strip slice.
        assert_eq!(strip_asn1_sign_pad(&[0x00, 0xab, 0xcd]), &[0xab, 0xcd]);
    }

    #[test]
    fn single_high_bit_byte_passes_through() {
        // Only one significant byte and its MSB is set, but there's no
        // leading zero — the rule strips a 0x00 prefix only when followed
        // by a high-bit byte; with no prefix the byte stays.
        assert_eq!(strip_asn1_sign_pad(&[0xff]), &[0xff]);
    }

    #[test]
    fn single_zero_byte_passes_through() {
        // [0x00] doesn't match the strip rule (rest is empty so the
        // is_some_and guard is false) — passes through unchanged. The
        // Bl-3 path that mattered was the empty-input case below.
        assert_eq!(strip_asn1_sign_pad(&[0x00]), &[0x00]);
    }

    #[test]
    fn empty_input_stays_empty() {
        // The Bl-3 case. ASN.1 INTEGER bytes shouldn't come through empty
        // for a valid cert, but if they do strip_asn1_sign_pad propagates
        // the empty slice — and extract_serial converts that to None so
        // the surrounding auth layer rejects the cert rather than silently
        // skipping the revocation check.
        assert!(strip_asn1_sign_pad(&[]).is_empty());
    }
}
