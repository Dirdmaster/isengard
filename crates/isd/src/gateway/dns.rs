//! DNS server: answer `*.<zone>` -> 127.0.0.1, REFUSED for out-of-zone.
//!
//! Modelled on the controller's `crates/isengard-controller/src/dns.rs` but
//! simpler: every query that lands inside the zone gets the same answer
//! (`127.0.0.1`), so there is no record map to refresh. The proxy does the
//! per-host routing on the data path.

use std::net::{Ipv4Addr, SocketAddr};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use hickory_proto::op::{Header, MessageType, ResponseCode};
use hickory_proto::rr::{Name, RData, Record, RecordType, rdata};
use hickory_server::ServerFuture;
use hickory_server::authority::MessageResponseBuilder;
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

/// TTL for every A record we serve. Short so changes (new container, new
/// rule) reach the browser fast.
pub const RECORD_TTL_SECS: u32 = 5;

/// Owning handle for the running server. Drop to unbind the UDP socket.
pub struct DnsHandle {
    _server: ServerFuture<ZoneHandler>,
}

/// Bind a UDP socket on `listen` and start serving `*.<zone>` -> 127.0.0.1.
/// The returned handle keeps the listener alive; drop it to stop.
pub async fn start(listen: SocketAddr, zone: String) -> Result<DnsHandle> {
    let socket = UdpSocket::bind(listen)
        .await
        .with_context(|| format!("binding DNS UDP socket on {listen}"))?;
    info!(zone = %zone, %listen, "gateway DNS listening");

    let handler = ZoneHandler {
        zone: normalize_zone(&zone),
    };
    let mut server = ServerFuture::new(handler);
    server.register_socket(socket);
    Ok(DnsHandle { _server: server })
}

/// hickory `RequestHandler` impl. Cloned per inbound request internally; we
/// keep the state immutable + small so the clone is free.
#[derive(Clone)]
pub struct ZoneHandler {
    zone: String,
}

#[async_trait]
impl RequestHandler for ZoneHandler {
    async fn handle_request<R: ResponseHandler>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let queries = request.queries();
        let Some(query) = queries.first() else {
            return reply_error(request, &mut response_handle, ResponseCode::FormErr).await;
        };
        let qname = format_query_name(&query.name().to_string());
        let qtype = query.query_type();
        debug!(qname = %qname, ?qtype, "gateway DNS query");

        if !is_in_zone(&qname, &self.zone) {
            // Out of zone: REFUSED tells the OS resolver to fall through.
            return reply_error(request, &mut response_handle, ResponseCode::Refused).await;
        }

        if qtype != RecordType::A && qtype != RecordType::ANY {
            // In-zone non-A (e.g. AAAA): NoError + empty answer. Mirrors
            // the controller's resolver behaviour for consistency.
            return reply_empty_noerror(request, &mut response_handle).await;
        }

        let name = match Name::from_utf8(&qname) {
            Ok(n) => n,
            Err(e) => {
                warn!(qname = %qname, error = %e, "gateway DNS: bad query name");
                return reply_error(request, &mut response_handle, ResponseCode::ServFail).await;
            }
        };
        let record = Record::from_rdata(
            name,
            RECORD_TTL_SECS,
            RData::A(rdata::A(Ipv4Addr::LOCALHOST)),
        );
        send_answer(request, &mut response_handle, vec![record]).await
    }
}

async fn send_answer<R: ResponseHandler>(
    request: &Request,
    response_handle: &mut R,
    answers: Vec<Record>,
) -> ResponseInfo {
    let builder = MessageResponseBuilder::from_message_request(request);
    let mut header = Header::response_from_request(request.header());
    header.set_message_type(MessageType::Response);
    header.set_authoritative(true);
    header.set_response_code(ResponseCode::NoError);
    let response = builder.build(header, answers.iter(), &[], &[], &[]);
    match response_handle.send_response(response).await {
        Ok(info) => info,
        Err(e) => {
            warn!(error = %e, "gateway DNS: failed to send answer");
            ResponseInfo::from(*request.header())
        }
    }
}

async fn reply_error<R: ResponseHandler>(
    request: &Request,
    response_handle: &mut R,
    code: ResponseCode,
) -> ResponseInfo {
    let builder = MessageResponseBuilder::from_message_request(request);
    let response = builder.error_msg(request.header(), code);
    match response_handle.send_response(response).await {
        Ok(info) => info,
        Err(e) => {
            warn!(error = %e, ?code, "gateway DNS: failed to send error");
            ResponseInfo::from(*request.header())
        }
    }
}

async fn reply_empty_noerror<R: ResponseHandler>(
    request: &Request,
    response_handle: &mut R,
) -> ResponseInfo {
    let builder = MessageResponseBuilder::from_message_request(request);
    let mut header = Header::response_from_request(request.header());
    header.set_message_type(MessageType::Response);
    header.set_authoritative(true);
    header.set_response_code(ResponseCode::NoError);
    let response = builder.build(header, &[], &[], &[], &[]);
    match response_handle.send_response(response).await {
        Ok(info) => info,
        Err(e) => {
            warn!(error = %e, "gateway DNS: failed to send empty NoError");
            ResponseInfo::from(*request.header())
        }
    }
}

/// Lower-case + ensure the trailing dot. Mirrors hickory's canonical form.
fn format_query_name(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.ends_with('.') {
        trimmed
    } else {
        format!("{trimmed}.")
    }
}

/// Strip leading/trailing dots, lower-case, drop empty segments.
fn normalize_zone(zone: &str) -> String {
    zone.trim().trim_matches('.').to_ascii_lowercase()
}

/// Is `name` inside `zone`? `foo.isd` matches `isd`, `a.b.isd` matches
/// `isd`, the zone apex matches itself, and `foo.local` does not match
/// `isd`. Suffix-with-dot is enforced so `foo.isolation` doesn't sneak in.
pub fn is_in_zone(name: &str, zone: &str) -> bool {
    let n = name.trim_end_matches('.').to_ascii_lowercase();
    let z = normalize_zone(zone);
    if z.is_empty() {
        return false;
    }
    if n == z {
        return true;
    }
    let suffix = format!(".{z}");
    n.ends_with(&suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_matching_basic() {
        assert!(is_in_zone("foo.isd", "isd"));
        assert!(is_in_zone("foo.isd.", "isd"));
        assert!(is_in_zone("a.b.isd", "isd"));
        assert!(is_in_zone("isd", "isd"));
        assert!(!is_in_zone("foo.local", "isd"));
        assert!(!is_in_zone("foo.isolation", "isd"));
        assert!(!is_in_zone("google.com", "isd"));
    }

    #[test]
    fn zone_matching_normalizes_zone_input() {
        assert!(is_in_zone("foo.isd", ".isd."));
        assert!(is_in_zone("foo.isd", "ISD"));
        assert!(!is_in_zone("foo.isd", ""));
    }

    #[test]
    fn zone_matching_case_insensitive() {
        assert!(is_in_zone("FOO.ISD", "isd"));
        assert!(is_in_zone("Hello.Isd", "ISD"));
    }

    #[test]
    fn format_query_name_adds_trailing_dot() {
        assert_eq!(format_query_name("FOO.ISD"), "foo.isd.");
        assert_eq!(format_query_name("foo.isd."), "foo.isd.");
    }

    /// Spin up a real DNS server on an ephemeral port and confirm an in-zone
    /// A query gets `127.0.0.1` back. Skipped if UDP binding fails (CI
    /// sandboxes occasionally block UDP).
    #[tokio::test]
    async fn ephemeral_server_answers_in_zone_with_loopback() {
        use hickory_proto::op::{Message, MessageType, OpCode, Query};
        use hickory_proto::serialize::binary::{BinDecodable, BinEncodable};

        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let bind_probe = match UdpSocket::bind(listen).await {
            Ok(s) => s,
            Err(_) => return, // CI without UDP -> skip silently
        };
        let bound = bind_probe.local_addr().unwrap();
        drop(bind_probe);

        let _handle = start(bound, "isd".into()).await.expect("dns start");

        // Send a small A query for `hello.isd.` to the bound port.
        let mut q = Query::new();
        q.set_name(Name::from_utf8("hello.isd.").unwrap());
        q.set_query_type(RecordType::A);
        let mut msg = Message::new();
        msg.add_query(q);
        msg.set_id(0x4242);
        msg.set_message_type(MessageType::Query);
        msg.set_op_code(OpCode::Query);
        msg.set_recursion_desired(true);
        let bytes = msg.to_bytes().unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(bound).await.unwrap();
        client.send(&bytes).await.unwrap();
        let mut buf = vec![0u8; 512];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv(&mut buf))
            .await
            .expect("dns response timed out")
            .expect("dns recv");
        let resp = Message::from_bytes(&buf[..n]).expect("parse dns response");
        assert_eq!(resp.response_code(), ResponseCode::NoError);
        let answers = resp.answers();
        assert_eq!(answers.len(), 1, "expected exactly one A record");
        match answers[0].data() {
            RData::A(a) => assert_eq!(a.0, Ipv4Addr::LOCALHOST),
            other => panic!("unexpected answer rdata: {other:?}"),
        }
    }
}
