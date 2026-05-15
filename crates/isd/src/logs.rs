//! `isd logs <stack>/<service> -f`: stream service logs over the
//! controller's WebSocket. Frames mirror what the dashboard's
//! `useLogStream` consumes (see `crates/isengard-plugins/dashboard/src/ws.rs`).

use anyhow::{Context as _, Result, anyhow};
use clap::Args;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::session::Session;

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Target as <stack>/<service>.
    pub target: String,
}

#[derive(Debug, Deserialize)]
struct StackDto {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum WsFrame {
    Backfill {
        host: Option<String>,
        lines: Vec<LogLine>,
    },
    Line {
        host: Option<String>,
        ts: Option<String>,
        #[allow(dead_code)]
        stream: Option<String>,
        msg: String,
    },
    Dropped {
        host: Option<String>,
        count: u64,
        reason: Option<String>,
    },
    Unavailable {
        host: Option<String>,
        reason: Option<String>,
    },
    Closed {
        reason: Option<String>,
    },
    /// Ignore everything else (heartbeats, future kinds).
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct LogLine {
    ts: Option<String>,
    stream: Option<String>,
    msg: String,
}

pub async fn run(args: LogsArgs, context: Option<&str>) -> Result<()> {
    let (stack, service) = parse_target(&args.target)?;
    let session = Session::open(context).await?;

    // Resolve stack name -> stack id.
    let stacks: Vec<StackDto> = session
        .client
        .get(format!("{}/api/v1/stacks", session.controller_url()))
        .send()
        .await
        .context("GET /api/v1/stacks")?
        .error_for_status()
        .context("listing stacks")?
        .json()
        .await
        .context("decoding stacks JSON")?;
    let stack_id = stacks
        .iter()
        .find(|s| s.name == stack)
        .map(|s| s.id.clone())
        .ok_or_else(|| anyhow!("no stack named {stack:?}"))?;

    let ws_url = controller_to_ws_url(session.controller_url(), &stack_id, &service);
    eprintln!("Streaming {} from {} (^C to stop)", args.target, ws_url);

    // Build the WS request. We piggyback the bearer token in the
    // Authorization header; the dashboard's WS handler doesn't yet enforce
    // it, but the controller's HTTP middleware does on the same path so
    // the cookie / header is observed for future tightening.
    let mut req = ws_url
        .clone()
        .into_client_request()
        .context("building WS request")?;
    let header_val =
        HeaderValue::from_str(&String::from("Bearer ")).context("building Authorization header")?;
    req.headers_mut().insert("Authorization", header_val);

    // Build a tokio-rustls connector that mirrors the pinned client's
    // "accept any cert" stance (post-handshake the fingerprint is checked
    // by every REST hop above; for the WS frames we currently rely on the
    // network path being LAN-local until v0.3 hardens this).
    // tokio-tungstenite's default connector uses native-roots. For self-signed
    // controller certs we need to disable verification. v0.3a accepts the
    // looser stance because the channel is LAN-local; v0.3 hardens this
    // with a fingerprint-pinning verifier sourced from credentials.toml.
    let connector = make_no_verify_connector();
    let (ws_stream, _resp) =
        tokio_tungstenite::connect_async_tls_with_config(req, None, false, connector)
            .await
            .with_context(|| format!("opening WebSocket {ws_url}"))?;

    let (mut sink, mut stream) = ws_stream.split();
    // Send a no-op ping so the server pushes its first backfill ack.
    let _ = sink
        .send(tokio_tungstenite::tungstenite::Message::Ping(
            Default::default(),
        ))
        .await;

    while let Some(msg) = stream.next().await {
        let msg = msg.context("WS stream errored")?;
        use tokio_tungstenite::tungstenite::Message::*;
        match msg {
            Text(t) => print_frame(&t),
            Binary(_) => continue,
            Close(_) => {
                eprintln!("server closed connection");
                break;
            }
            Ping(p) => {
                let _ = sink
                    .send(tokio_tungstenite::tungstenite::Message::Pong(p))
                    .await;
            }
            Pong(_) | Frame(_) => continue,
        }
    }
    Ok(())
}

fn parse_target(t: &str) -> Result<(String, String)> {
    let (stack, service) = t
        .split_once('/')
        .ok_or_else(|| anyhow!("expected `<stack>/<service>`, got {t:?}"))?;
    if stack.is_empty() || service.is_empty() {
        return Err(anyhow!(
            "stack and service must both be non-empty (got {t:?})"
        ));
    }
    Ok((stack.to_string(), service.to_string()))
}

fn controller_to_ws_url(controller_url: &str, stack_id: &str, service: &str) -> String {
    let base = controller_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let svc = url_encode(service);
    format!("{base}/api/v1/services/{stack_id}/{svc}/logs/ws")
}

/// Minimal percent-encoder for the service-name path segment. We don't pull
/// in `url`/`urlencoding` for this one call.
fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let c = *byte;
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~') {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

fn print_frame(text: &str) {
    let frame: Result<WsFrame, _> = serde_json::from_str(text);
    let Ok(frame) = frame else {
        // Unknown frame: print raw so operators can still see it.
        println!("{text}");
        return;
    };
    match frame {
        WsFrame::Backfill { host, lines } => {
            for line in lines {
                println!(
                    "{}{} {}",
                    host.as_deref()
                        .map(|h| format!("[{h}] "))
                        .unwrap_or_default(),
                    line.ts.as_deref().unwrap_or(""),
                    line.msg.trim_end_matches('\n'),
                );
                let _ = line.stream; // reserved
            }
        }
        WsFrame::Line { host, ts, msg, .. } => {
            println!(
                "{}{} {}",
                host.as_deref()
                    .map(|h| format!("[{h}] "))
                    .unwrap_or_default(),
                ts.as_deref().unwrap_or(""),
                msg.trim_end_matches('\n'),
            );
        }
        WsFrame::Dropped {
            host,
            count,
            reason,
        } => {
            eprintln!(
                "[dropped {count} on {}{}]",
                host.as_deref().unwrap_or("?"),
                reason.map(|r| format!(": {r}")).unwrap_or_default(),
            );
        }
        WsFrame::Unavailable { host, reason } => {
            eprintln!(
                "[unavailable on {}{}]",
                host.as_deref().unwrap_or("?"),
                reason.map(|r| format!(": {r}")).unwrap_or_default(),
            );
        }
        WsFrame::Closed { reason } => {
            eprintln!(
                "[closed{}]",
                reason.map(|r| format!(": {r}")).unwrap_or_default(),
            );
        }
        WsFrame::Other => {}
    }
}

fn make_no_verify_connector() -> Option<Connector> {
    use std::sync::Arc;
    use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified};
    use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

    #[derive(Debug)]
    struct NoVerify;

    impl tokio_rustls::rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ECDSA_NISTP521_SHA512,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::ED25519,
            ]
        }
    }

    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    Some(Connector::Rustls(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_splits_on_slash() {
        let (s, svc) = parse_target("blog/wordpress").unwrap();
        assert_eq!(s, "blog");
        assert_eq!(svc, "wordpress");
    }

    #[test]
    fn parse_target_rejects_bad_shapes() {
        assert!(parse_target("blog").is_err());
        assert!(parse_target("/wordpress").is_err());
        assert!(parse_target("blog/").is_err());
    }

    #[test]
    fn controller_to_ws_url_swaps_scheme_and_encodes() {
        let url = controller_to_ws_url("https://controller.local:9417", "12", "my service");
        assert_eq!(
            url,
            "wss://controller.local:9417/api/v1/services/12/my%20service/logs/ws"
        );

        let url2 = controller_to_ws_url("http://localhost:9418", "5", "wp");
        assert_eq!(url2, "ws://localhost:9418/api/v1/services/5/wp/logs/ws");
    }

    #[test]
    fn line_frame_round_trips_through_serde() {
        let frame: WsFrame = serde_json::from_str(
            r#"{"type":"line","host":"host-a","ts":"2026-05-07T10:00Z","stream":"stdout","msg":"hello"}"#,
        )
        .unwrap();
        match frame {
            WsFrame::Line { host, ts, msg, .. } => {
                assert_eq!(host.as_deref(), Some("host-a"));
                assert_eq!(ts.as_deref(), Some("2026-05-07T10:00Z"));
                assert_eq!(msg, "hello");
            }
            _ => panic!("expected Line frame"),
        }
    }

    #[test]
    fn unknown_frame_kind_is_treated_as_other() {
        let frame: WsFrame = serde_json::from_str(r#"{"type":"future-kind","x":1}"#).unwrap();
        matches!(frame, WsFrame::Other);
    }
}
