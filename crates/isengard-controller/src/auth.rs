//! Phase 14: per-RPC client cert validation + revocation check.
//! Replaces Phase 2c's `TokenAuthLayer` (bearer-token middleware).
//!
//! All RPCs except those listed in [`PUBLIC_METHODS`] require a valid client
//! certificate signed by the controller's internal CA. The interceptor
//! additionally checks the cert's serial against the in-memory
//! [`RevocationSet`] so revoked agents are rejected on the very next RPC.
//!
//! `Enroll` is intentionally public: an agent that hasn't enrolled yet has no
//! cert to present, so the bootstrap chicken-and-egg is solved by configuring
//! the tonic server with `client_auth_optional(true)` and letting the layer
//! enforce cert presence per-RPC.
//!
//! Why a tower layer (not a [`tonic::service::Interceptor`]): tonic
//! interceptors receive `tonic::Request<()>`, which drops the URI during the
//! `http::Request -> tonic::Request` conversion in `InterceptedService`. We
//! need the URI path to dispatch by method (`/isengard.v1.Controller/Enroll`)
//! AND we need the request extensions (where `TlsConnectInfo` lives) — both
//! of which are available on the raw `http::Request` before it's split.

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

/// RPCs that don't require a client cert (the bootstrap chicken-and-egg).
/// Format matches the gRPC URI path tonic dispatches on.
const PUBLIC_METHODS: &[&str] = &["/isengard.v1.Controller/Enroll"];

/// Tower layer producing a [`CertAuth`] middleware around a service.
///
/// Cheap to clone: internally `Arc`s.
#[derive(Clone)]
pub struct CertAuthInterceptor {
    revocation: RevocationSet,
    // Kept for future use: the renew_cert handler will cross-check that the
    // host_id in the request body matches the host_id encoded in the caller's
    // client cert (see TODO in service.rs::renew_cert).
    _ca: Arc<Authority>,
}

impl CertAuthInterceptor {
    pub fn new(revocation: RevocationSet, ca: Arc<Authority>) -> Self {
        Self {
            revocation,
            _ca: ca,
        }
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

/// Tower service that gates each request behind cert presence + revocation.
#[derive(Clone)]
pub struct CertAuth<S> {
    inner: S,
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

            if revocation.contains(&serial) {
                return Ok(Status::unauthenticated("client cert revoked").into_http());
            }

            inner.call(req).await
        })
    }
}

/// Pull the X.509 serial out of a DER-encoded cert. Used by the layer to
/// look the serial up in the [`RevocationSet`].
///
/// `raw_serial` returns the ASN.1 INTEGER bytes verbatim. If the high bit of
/// the original 16-byte random serial happens to be set, ASN.1 prepends a
/// leading 0x00 to keep the integer positive — strip it so the result lines
/// up with the serial we persisted in `agent_certs.serial` (which is the
/// raw 16 bytes from `OsRng`).
fn extract_serial(cert_der: &[u8]) -> Option<Vec<u8>> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der).ok()?;
    let raw = cert.tbs_certificate.raw_serial();
    Some(strip_asn1_sign_pad(raw).to_vec())
}

/// Strip the ASN.1 INTEGER positive-sign padding byte, if present. A leading
/// 0x00 is significant only when the next byte's MSB is set; otherwise it's
/// either non-canonical encoding or simply absent.
fn strip_asn1_sign_pad(raw: &[u8]) -> &[u8] {
    match raw {
        [0x00, rest @ ..] if rest.first().is_some_and(|b| b & 0x80 != 0) => rest,
        other => other,
    }
}
