//! `TokenAuthLayer`: a tower middleware that gates a tonic service behind a
//! bearer-token check on the `authorization` metadata header.
//!
//! v1 is shared-secret-token auth — the controller runs with `ISENGARD_TOKEN`
//! in env, every agent presents the same value. Per-agent tokens / rotation
//! / mTLS upgrade is post-v1.

use std::sync::Arc;
use std::task::{Context, Poll};

use http::{HeaderValue, Request, Response};
use subtle::ConstantTimeEq;
use tonic::body::BoxBody;
use tower::{Layer, Service};

/// Tower layer producing [`TokenAuth`] middleware around a service.
#[derive(Clone)]
pub struct TokenAuthLayer {
    expected: Arc<Vec<u8>>,
}

impl TokenAuthLayer {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            expected: Arc::new(token.into().into_bytes()),
        }
    }
}

impl<S> Layer<S> for TokenAuthLayer {
    type Service = TokenAuth<S>;
    fn layer(&self, inner: S) -> Self::Service {
        TokenAuth {
            inner,
            expected: self.expected.clone(),
        }
    }
}

#[derive(Clone)]
pub struct TokenAuth<S> {
    inner: S,
    expected: Arc<Vec<u8>>,
}

impl<S, B> Service<Request<B>> for TokenAuth<S>
where
    S: Service<Request<B>, Response = Response<BoxBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let mut inner = self.inner.clone();
        let expected = self.expected.clone();

        Box::pin(async move {
            // Skip auth for the gRPC reflection service so `grpcurl` can still
            // introspect the schema without a token.
            let path = req.uri().path();
            if path.starts_with("/grpc.reflection.") {
                return inner.call(req).await;
            }

            let header = req
                .headers()
                .get("authorization")
                .and_then(|v: &HeaderValue| v.to_str().ok())
                .unwrap_or("");

            let presented = header.strip_prefix("Bearer ").unwrap_or("");

            // Constant-time compare. CtOption truncates at the shorter length
            // and reports inequality if lengths differ.
            let ok = presented.as_bytes().ct_eq(expected.as_slice()).into();

            if ok {
                return inner.call(req).await;
            }

            // Reject with UNAUTHENTICATED. Build the gRPC-conformant response.
            let status = tonic::Status::unauthenticated("missing or invalid bearer token");
            Ok(status.into_http())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_rejects_mismatched_lengths() {
        let a: &[u8] = b"foo";
        let b: &[u8] = b"foobar";
        let eq: bool = a.ct_eq(b).into();
        assert!(!eq);
    }

    #[test]
    fn ct_eq_accepts_exact_match() {
        let a: &[u8] = b"hunter2";
        let b: &[u8] = b"hunter2";
        let eq: bool = a.ct_eq(b).into();
        assert!(eq);
    }

    #[test]
    fn ct_eq_rejects_close_mismatch() {
        let a: &[u8] = b"hunter2";
        let b: &[u8] = b"hunter3";
        let eq: bool = a.ct_eq(b).into();
        assert!(!eq);
    }
}
