//! Friendly diagnosis of enrollment failures.
//!
//! On first-boot, an agent's `Enroll` RPC can fail for half a dozen ops-level
//! reasons (wrong scheme, bad CA, expired token, controller down). All of
//! them surface today as the same opaque tonic/h2/transport chain. This
//! module pattern-matches those chains into a small enum with a renderable
//! human-friendly explanation.
//!
//! Matching is purely on string content of the error chain (display + debug
//! of every link). The concrete tonic/h2/hyper error types either expose
//! private variants or change between minor releases, so string matching is
//! the documented escape hatch and is plenty robust for the four cases we
//! cover. Each case has at least two anchor substrings to keep false
//! positives out.
//!
//! Used only for the first-boot enroll path. Post-enroll RPCs (sync stream,
//! cert renewal) have different operator implications and need their own
//! diagnostics, intentionally out of scope here.

/// What kind of enrollment failure the diagnoser recognized. `None` from
/// [`diagnose`] means: fall through to the raw anyhow chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrollmentDiagnosis {
    /// HTTP/2 protocol error during the bootstrap handshake. Almost always
    /// means the agent is talking plaintext to a TLS-only controller (i.e.
    /// the URL has scheme `http://` instead of `https://`).
    HttpSchemeMismatch,
    /// Controller responded `Unauthenticated` or `PermissionDenied` to the
    /// Enroll RPC. The token is invalid, expired, or already redeemed.
    TokenRejected,
    /// TCP connect refused. The controller is not listening on this address.
    ConnectionRefused,
    /// TLS handshake failed cert verification. Either the wrong CA is
    /// pinned, the controller regenerated its CA, or the URL hostname does
    /// not match the cert's SAN.
    CertificateUntrusted,
}

/// Walk an anyhow error chain and pattern-match it against the four known
/// enrollment failure modes. Returns `None` if nothing matches.
///
/// Order of evaluation: TokenRejected, HttpSchemeMismatch, ConnectionRefused,
/// CertificateUntrusted. First match wins. The order matters when chains
/// overlap (a misbehaving controller could hypothetically surface both an
/// auth failure and an h2 protocol error on the same call): we prefer the
/// most specific, application-layer reading.
pub fn diagnose(err: &anyhow::Error) -> Option<EnrollmentDiagnosis> {
    let haystack = collect_chain(err);

    if has_token_rejected(&haystack) {
        return Some(EnrollmentDiagnosis::TokenRejected);
    }
    if has_http_scheme_mismatch(&haystack) {
        return Some(EnrollmentDiagnosis::HttpSchemeMismatch);
    }
    if has_connection_refused(&haystack) {
        return Some(EnrollmentDiagnosis::ConnectionRefused);
    }
    if has_certificate_untrusted(&haystack) {
        return Some(EnrollmentDiagnosis::CertificateUntrusted);
    }
    None
}

/// Render a diagnosis to the multi-line block surfaced on stderr. Includes
/// the controller URL verbatim so the operator can correlate. Contains no
/// em dashes (U+2014) or en dashes (U+2013): tested.
pub fn render(diagnosis: EnrollmentDiagnosis, controller_url: &str) -> String {
    match diagnosis {
        EnrollmentDiagnosis::HttpSchemeMismatch => format!(
            "x Failed to enroll with controller at {controller_url}\n\n\
             \x20 Reason: HTTP/2 protocol error during the enrollment handshake.\n\n\
             \x20 Most likely cause: the controller listens on TLS but the URL uses http://\n\
             \x20 (not https://).\n\n\
             \x20 Fix:\n\
             \x20   1. Check the controller URL scheme. Should be https:// for 9417.\n\
             \x20   2. Mint a fresh join-token with `isd join-token` (it embeds the CA\n\
             \x20      fingerprint so the agent can verify the controller automatically).\n\
             \x20   3. Re-run with the token from step 2.\n\n\
             \x20 For full error chain: rerun with RUST_LOG=debug",
        ),
        EnrollmentDiagnosis::TokenRejected => format!(
            "x Enrollment token rejected by controller at {controller_url}.\n\n\
             \x20 The token has expired or was already redeemed. Mint a fresh one:\n\
             \x20   docker exec isd-controller isengard controller token mint --role agent\n\n\
             \x20 For full error chain: rerun with RUST_LOG=debug",
        ),
        EnrollmentDiagnosis::ConnectionRefused => format!(
            "x Cannot reach controller at {controller_url}\n\n\
             \x20 Reason: connection refused. The controller is not listening on this address.\n\n\
             \x20 Common fixes:\n\
             \x20   - Is the controller container running? `docker ps | grep isd-controller`\n\
             \x20   - Check the controller's listen address. Default 0.0.0.0:9417 for gRPC.\n\
             \x20   - From inside another container, the controller's hostname may differ\n\
             \x20     from the host (e.g. `controller` via Compose DNS, NOT 127.0.0.1).\n\n\
             \x20 For full error chain: rerun with RUST_LOG=debug",
        ),
        EnrollmentDiagnosis::CertificateUntrusted => format!(
            "x Failed to verify controller's TLS certificate at {controller_url}\n\n\
             \x20 The CA the agent has does not match the controller's cert. Either:\n\
             \x20   - Wrong CA bundled on the agent (re-export with `controller ca export`)\n\
             \x20   - Controller regenerated its CA but agent still has the old one\n\
             \x20   - Hostname in the URL doesn't match the cert's SAN\n\n\
             \x20 For full error chain: rerun with RUST_LOG=debug",
        ),
    }
}

/// Concatenate every link in the chain (display + debug) into one lowercase
/// haystack. Lowercasing once here keeps the matchers below allocation-free
/// and case-insensitive without per-call to_lowercase calls.
fn collect_chain(err: &anyhow::Error) -> String {
    let mut buf = String::new();
    for link in err.chain() {
        buf.push_str(&link.to_string());
        buf.push('\n');
        buf.push_str(&format!("{link:?}"));
        buf.push('\n');
    }
    buf.to_lowercase()
}

/// Internal helper: has token rejected.
fn has_token_rejected(h: &str) -> bool {
    // Tonic surfaces gRPC status codes as "status: Unauthenticated" /
    // "status: PermissionDenied" in both Display and Debug. Lowercase here.
    h.contains("status: unauthenticated")
        || h.contains("status: permissiondenied")
        || (h.contains("token") && (h.contains("invalid") || h.contains("expired")))
}

/// Internal helper: has http scheme mismatch.
fn has_http_scheme_mismatch(h: &str) -> bool {
    // h2's "frame with invalid size" reason fires when the transport reads a
    // TLS handshake byte stream as if it were h2 frames. The "h2 protocol
    // error" anchor is needed to disambiguate from non-h2 frame issues.
    h.contains("h2 protocol error")
        && (h.contains("frame with invalid size") || h.contains("frame_size_error"))
}

/// Internal helper: has connection refused.
fn has_connection_refused(h: &str) -> bool {
    // Linux: "Connection refused (os error 111)". macOS: "(os error 61)".
    // Some layers also stringify as "ECONNREFUSED". All forms appear in the
    // chain because hyper / tonic re-wrap the io::Error.
    h.contains("connection refused") || h.contains("econnrefused")
}

/// Internal helper: has certificate untrusted.
fn has_certificate_untrusted(h: &str) -> bool {
    // rustls surfaces verification failures as "InvalidCertificate(UnknownIssuer)"
    // or similar. tonic wraps that in "tls handshake error". We require the
    // word "certificate" plus a verification-failure anchor.
    h.contains("certificate")
        && (h.contains("verify")
            || h.contains("unknown issuer")
            || h.contains("unknownissuer")
            || h.contains("self-signed")
            || h.contains("self signed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn http_scheme_mismatch_diagnoses() {
        let err: anyhow::Error = anyhow!("connection error detected: frame with invalid size")
            .context("h2 protocol error")
            .context("transport error")
            .context("Enroll RPC failed");
        assert_eq!(
            diagnose(&err),
            Some(EnrollmentDiagnosis::HttpSchemeMismatch)
        );
    }

    #[test]
    fn token_rejected_diagnoses_unauthenticated() {
        let err: anyhow::Error =
            anyhow!("status: Unauthenticated, message: \"token expired\", details: []")
                .context("Enroll RPC failed");
        assert_eq!(diagnose(&err), Some(EnrollmentDiagnosis::TokenRejected));
    }

    #[test]
    fn connection_refused_diagnoses() {
        let err: anyhow::Error = anyhow!("tcp connect error: Connection refused (os error 61)")
            .context("connect bootstrap channel to https://controller:9417");
        assert_eq!(diagnose(&err), Some(EnrollmentDiagnosis::ConnectionRefused));
    }

    #[test]
    fn certificate_untrusted_diagnoses_unknown_issuer() {
        let err: anyhow::Error = anyhow!("invalid peer certificate: UnknownIssuer")
            .context("tls handshake error")
            .context("Enroll RPC failed");
        assert_eq!(
            diagnose(&err),
            Some(EnrollmentDiagnosis::CertificateUntrusted),
        );
    }

    #[test]
    fn unknown_error_returns_none() {
        let err: anyhow::Error = anyhow!("oops, unrelated error").context("Enroll RPC failed");
        assert_eq!(diagnose(&err), None);
    }

    #[test]
    fn render_has_no_em_dash_and_includes_url() {
        let url = "https://controller:9417";
        for d in [
            EnrollmentDiagnosis::HttpSchemeMismatch,
            EnrollmentDiagnosis::TokenRejected,
            EnrollmentDiagnosis::ConnectionRefused,
            EnrollmentDiagnosis::CertificateUntrusted,
        ] {
            let out = render(d, url);
            assert!(out.contains(url), "render({d:?}) missing url:\n{out}");
            assert!(
                !out.contains('\u{2014}'),
                "render({d:?}) contains em dash:\n{out}",
            );
            assert!(
                !out.contains('\u{2013}'),
                "render({d:?}) contains en dash:\n{out}",
            );
        }
    }

    #[test]
    fn token_rejected_wins_over_h2() {
        // Order-of-evaluation property: if a chain plausibly matches both
        // auth failure and a transport h2 error, prefer the auth reading.
        let err: anyhow::Error = anyhow!("status: Unauthenticated, message: \"bad token\"")
            .context("h2 protocol error: frame with invalid size")
            .context("Enroll RPC failed");
        assert_eq!(diagnose(&err), Some(EnrollmentDiagnosis::TokenRejected));
    }

    #[test]
    fn certificate_untrusted_self_signed_variant() {
        let err: anyhow::Error =
            anyhow!("invalid peer certificate: certificate signed by self-signed root not trusted")
                .context("tls handshake error");
        assert_eq!(
            diagnose(&err),
            Some(EnrollmentDiagnosis::CertificateUntrusted),
        );
    }
}
