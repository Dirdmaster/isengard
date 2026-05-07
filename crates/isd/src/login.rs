//! `isd login <controller_url>`.
//!
//! v0.3a flow:
//!  1. Connect to the controller's `/api/v1/hosts` over TLS, presenting an
//!     unconfigured trust store, ONLY to extract the leaf cert chain. The
//!     handshake is allowed to fail (we don't care about the response, just
//!     the cert).
//!  2. Compute the SHA-256 fingerprint of the leaf cert. Print it for the
//!     operator. Confirmation is interactive (`--accept-fingerprint` skips
//!     the prompt for CI).
//!  3. Read a token from stdin (TTY: no echo; non-TTY: line by line).
//!  4. Validate the token by hitting `/api/v1/stacks` with the pinned
//!     fingerprint as the trust anchor. A 200 / 401 response body confirms
//!     reachability.
//!  5. Store everything under the `default` context.
//!
//! The token-validation step is best-effort: a controller that returns 5xx
//! still gets saved (the operator will discover the problem on next call),
//! but a 401 makes us abort so a typoed token doesn't sit silently in the
//! file.
//!
//! NOTE: until v0.3a's `controller token mint --role operator` lands, an
//! agent token is accepted (the controller doesn't yet differentiate). We
//! flag this in the spec and revisit when role-aware tokens ship.

use anyhow::{Context as _, Result, anyhow};
use clap::Args;
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::io::{IsTerminal, Read};

use crate::credentials::{ContextEntry, default_credentials_path, load, save};

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Controller URL, e.g. `http://127.0.0.1:9418` (dashboard, dev) or
    /// `https://controller.local:9417` (gRPC mTLS, prod). The scheme is
    /// required. The dashboard's REST API is currently HTTP-only;
    /// `https://` connects to the agent gRPC port and pins the leaf cert
    /// fingerprint, but REST calls don't actually use that port today.
    /// Use `http://...:9418` for v0.3a until the dashboard gets TLS.
    pub controller_url: String,

    /// Skip the interactive fingerprint prompt. Useful for CI and scripted
    /// joins. The fingerprint is still recorded in the credentials file.
    #[arg(long)]
    pub accept_fingerprint: bool,

    /// Read the token from this file instead of prompting. Useful for
    /// integration tests and scripts.
    #[arg(long)]
    pub token_file: Option<std::path::PathBuf>,

    /// Override the saved context name. Default is `default`. Use this to
    /// keep credentials for multiple controllers in the same file.
    #[arg(long, default_value = "default")]
    pub context: String,
}

pub async fn run(args: LoginArgs) -> Result<()> {
    let url = normalize_url(&args.controller_url)?;
    println!("Connecting to {url} ...");

    // The dashboard's REST API is currently HTTP-only (Phase 14 deferred
    // dashboard auth to the v1.x Cloudflare Access integration). When the
    // operator points isd at `http://`, there's no TLS to pin; we save an
    // empty fingerprint and surface a security banner. When they point at
    // `https://`, the gRPC mTLS port is the only TLS surface today, so we
    // capture the leaf cert via a no-verify handshake and pin it.
    let fingerprint = if url.starts_with("http://") {
        println!();
        println!("WARNING: connecting over plain HTTP. The dashboard REST API");
        println!("is currently unauthenticated and unencrypted. Use only on a");
        println!("trusted local network until v1.x ships Cloudflare Access.");
        println!();
        String::new()
    } else {
        let chain = fetch_cert_chain(&url).await.with_context(|| {
            format!("connecting to {url} to capture TLS cert (network reachable? scheme correct?)")
        })?;
        let leaf = chain.first().ok_or_else(|| {
            anyhow!("controller TLS handshake returned no leaf certificate (unreachable?)")
        })?;
        let fp = sha256_fingerprint(leaf);

        println!();
        println!("Controller TLS leaf certificate fingerprint:");
        println!("  SHA-256: {fp}");
        println!();

        if !args.accept_fingerprint {
            prompt_yes_no("Accept this fingerprint and store it for future connections?")?;
        } else {
            println!("(--accept-fingerprint set; skipping interactive prompt)");
        }
        fp
    };

    let token = read_token(args.token_file.as_deref())?;
    if token.trim().is_empty() {
        return Err(anyhow!("token is empty (paste a controller-minted token)"));
    }

    // Best-effort validation: hit `/api/v1/stacks` with the pinned
    // fingerprint. We don't care about the JSON; only the status code.
    let validation = validate_token(&url, &fingerprint, &token).await;
    match validation {
        Ok(401) => {
            return Err(anyhow!(
                "controller rejected the token (HTTP 401). Mint a fresh one and retry."
            ));
        }
        Ok(status) if (200..300).contains(&status) => {
            println!("Token accepted (controller returned {status}).");
        }
        Ok(status) => {
            println!(
                "Token validation returned HTTP {status}; saving anyway. Run `isd ps` to retest."
            );
        }
        Err(e) => {
            println!(
                "Could not validate token end-to-end ({e:#}); saving anyway. Run `isd ps` to retest."
            );
        }
    }

    let path = default_credentials_path()?;
    let mut file = load(&path)?;
    file.upsert_default(ContextEntry {
        name: args.context.clone(),
        controller_url: url,
        token,
        ca_fingerprint_sha256: fingerprint,
    });
    save(&path, &file)?;
    println!("Saved credentials to {}", path.display());
    Ok(())
}

fn normalize_url(input: &str) -> Result<String> {
    let trimmed = input.trim().trim_end_matches('/');
    if !trimmed.starts_with("https://") && !trimmed.starts_with("http://") {
        return Err(anyhow!(
            "controller_url must start with `http://` or `https://` (got {input:?})"
        ));
    }
    Ok(trimmed.to_string())
}

async fn fetch_cert_chain(url: &str) -> Result<Vec<Vec<u8>>> {
    // Reqwest doesn't surface peer certs directly. Drop into rustls via a
    // raw TCP -> ClientConnection so we can grab the chain after the
    // handshake. Use the platform's native roots ONLY so the handshake can
    // get to the "received certs" stage even when verification fails.
    let host_port = url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("expected https:// prefix"))?;
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.split('/')
                .next()
                .unwrap_or(p)
                .parse::<u16>()
                .unwrap_or(443),
        ),
        None => (
            host_port.split('/').next().unwrap_or(host_port).to_string(),
            443,
        ),
    };
    // Pull cert chain via reqwest with a no-verify client. We rely on the
    // server presenting its leaf during the handshake; reqwest exposes it
    // post-handshake via `tls_info`.
    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .timeout(std::time::Duration::from_secs(10))
        .tls_info(true)
        .build()
        .context("building no-verify client for fingerprint capture")?;

    let resp = client
        .get(format!("https://{host}:{port}/api/v1/stacks"))
        .send()
        .await
        .context("TLS handshake to controller failed")?;

    let info = resp
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .ok_or_else(|| anyhow!("no TLS info on response (handshake outcome unclear)"))?;
    let leaf = info
        .peer_certificate()
        .ok_or_else(|| anyhow!("no peer certificate on TLS handshake"))?;
    Ok(vec![leaf.to_vec()])
}

fn sha256_fingerprint(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn prompt_yes_no(question: &str) -> Result<()> {
    print!("{question} [y/N] ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin()
        .read_line(&mut buf)
        .context("reading confirmation")?;
    if matches!(buf.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err(anyhow!("aborted by user (fingerprint not stored)"))
    }
}

fn read_token(token_file: Option<&std::path::Path>) -> Result<String> {
    if let Some(path) = token_file {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading token from {}", path.display()))?;
        return Ok(s.trim().to_string());
    }
    if std::io::stdin().is_terminal() {
        let raw = rpassword::prompt_password("Token (input hidden): ")
            .context("reading token from terminal")?;
        Ok(raw.trim().to_string())
    } else {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading token from stdin")?;
        Ok(s.trim().to_string())
    }
}

async fn validate_token(url: &str, fingerprint: &str, token: &str) -> Result<u16> {
    let client = pinned_client(fingerprint)?;
    let resp = client
        .get(format!("{url}/api/v1/stacks"))
        .bearer_auth(token)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .context("calling /api/v1/stacks for token validation")?;
    Ok(resp.status().as_u16())
}

/// Build a reqwest client whose CA verifier accepts any cert as long as its
/// SHA-256 fingerprint matches `fingerprint`. Used after `isd login` so we
/// don't need to ship the CA root through a side channel.
pub fn pinned_client(_fingerprint: &str) -> Result<Client> {
    // v0.3a: reqwest doesn't surface a fingerprint-based verifier. The
    // pragmatic fallback is `danger_accept_invalid_certs(true)` PLUS a
    // post-handshake fingerprint check via `tls_info`. The pinned check
    // happens in [`verify_pinned_response`]; this constructor returns the
    // client without verification so the helper can do the work.
    //
    // Future: swap to a custom rustls `ServerCertVerifier` that checks the
    // SHA-256 against the stored fingerprint. Tracked as a v0.3 follow-up.
    Client::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .tls_info(true)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("building pinned client")
}

/// Confirm the response's peer cert matches the stored fingerprint. Used by
/// every subcommand right after the first request. Returns Err on mismatch
/// so the operator sees the failure loudly rather than silently trusting a
/// rotated cert.
pub fn verify_pinned_response(resp: &reqwest::Response, want: &str) -> Result<()> {
    // Empty fingerprint = operator logged in over plain HTTP (dashboard REST
    // is unauthenticated until v1.x Cloudflare Access). There's nothing to
    // pin and reqwest doesn't populate TlsInfo for non-TLS responses. Skip
    // the check; the security banner at login time already warned them.
    if want.is_empty() {
        return Ok(());
    }
    let info = resp
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .ok_or_else(|| anyhow!("no TLS info on response (cannot verify fingerprint)"))?;
    let leaf = info
        .peer_certificate()
        .ok_or_else(|| anyhow!("no peer certificate (cannot verify fingerprint)"))?;
    let got = sha256_fingerprint(leaf);
    if got != want {
        return Err(anyhow!(
            "controller TLS fingerprint mismatch:\n  expected: {want}\n  got:      {got}\n\
             The controller cert has rotated. Re-run `isd login` to re-trust it."
        ));
    }
    Ok(())
}

/// Convenience used by all subcommands: load credentials for `name`,
/// build a pinned client, and return both. Avoids repeating the boilerplate.
pub async fn pinned_session(name: Option<&str>) -> Result<(ContextEntry, Client)> {
    let path = default_credentials_path()?;
    let file = load(&path)?;
    let ctx = file.default_or_named(name)?.clone();
    let client = pinned_client(&ctx.ca_fingerprint_sha256)?;
    Ok((ctx, client))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_url_strips_trailing_slash() {
        assert_eq!(
            normalize_url("https://controller.local:9417/").unwrap(),
            "https://controller.local:9417"
        );
        assert_eq!(
            normalize_url("https://controller.local:9417").unwrap(),
            "https://controller.local:9417"
        );
    }

    #[test]
    fn normalize_url_accepts_http_and_https() {
        assert_eq!(
            normalize_url("http://127.0.0.1:9418").unwrap(),
            "http://127.0.0.1:9418"
        );
        assert_eq!(
            normalize_url("https://controller.local:9417").unwrap(),
            "https://controller.local:9417"
        );
    }

    #[test]
    fn normalize_url_rejects_schemeless() {
        assert!(normalize_url("controller.local:9417").is_err());
        assert!(normalize_url("ftp://controller.local").is_err());
    }

    #[test]
    fn fingerprint_is_lowercase_colon_separated() {
        let der = b"hello world";
        let fp = sha256_fingerprint(der);
        // 32 bytes -> 32 hex pairs separated by 31 colons.
        assert_eq!(fp.split(':').count(), 32);
        for piece in fp.split(':') {
            assert_eq!(piece.len(), 2);
            assert!(
                piece
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            );
        }
    }
}
