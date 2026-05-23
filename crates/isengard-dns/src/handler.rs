//! Hickory `RequestHandler` impl that consults a [`ZoneSource`] first and
//! falls back to a [`TokioResolver`] upstream.
//!
//! The handler is cloned per inbound request by hickory's `ServerFuture`,
//! so all state lives behind `Arc`s.

use std::sync::Arc;

use hickory_proto::op::{Header, MessageType, ResponseCode};
use hickory_proto::rr::Record;
use hickory_resolver::TokioResolver;
use hickory_server::authority::MessageResponseBuilder;
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use tracing::{debug, warn};

use crate::zone_source::ZoneSource;

/// Concrete [`RequestHandler`] used by [`IsengardResolver`](crate::IsengardResolver).
///
/// Cloning is cheap: every field is an `Arc`. Hickory clones the handler
/// per inbound request internally.
#[derive(Clone)]
pub struct RequestHandlerImpl {
    zones: Arc<dyn ZoneSource + Send + Sync>,
    upstream: Arc<TokioResolver>,
}

impl RequestHandlerImpl {
    /// Build a handler from a shared [`ZoneSource`] and a shared upstream
    /// resolver.
    #[must_use]
    pub fn new(zones: Arc<dyn ZoneSource + Send + Sync>, upstream: Arc<TokioResolver>) -> Self {
        Self { zones, upstream }
    }

    /// Direct entry point used by tests: run the lookup logic against a
    /// query name + type and return the answer records.
    ///
    /// Wraps the same `zones-first, upstream-fallback` flow that the
    /// hickory `handle_request` path uses, but without a `ResponseHandler`
    /// so tests don't need to fake one.
    ///
    /// Returns `(response_code, answers)`. An empty `answers` vec with
    /// `NoError` is an authoritative "no data" answer; with `NXDomain`
    /// it's the upstream's negative answer.
    ///
    /// # Errors
    ///
    /// Propagates the zone source's own errors (translated to `ServFail`
    /// in the wire path). Upstream lookup failures are not surfaced: the
    /// handler maps them to a `ServFail` response, matching what every
    /// other forwarder does.
    pub async fn lookup_for_test(
        &self,
        name: &hickory_proto::rr::Name,
        rtype: hickory_proto::rr::RecordType,
    ) -> anyhow::Result<(ResponseCode, Vec<Record>)> {
        match self.zones.resolve(name, rtype).await? {
            Some(records) => Ok((ResponseCode::NoError, records)),
            None => match self.upstream.lookup(name.clone(), rtype).await {
                Ok(lookup) => {
                    let records: Vec<Record> = lookup.records().to_vec();
                    Ok((ResponseCode::NoError, records))
                }
                Err(_) => {
                    // Treat any upstream failure as NXDOMAIN-ish for the
                    // test surface. The wire path differentiates ServFail
                    // vs NXDomain via the actual ResolveErrorKind; the
                    // test surface keeps it simple.
                    Ok((ResponseCode::NXDomain, Vec::new()))
                }
            },
        }
    }
}

#[async_trait::async_trait]
impl RequestHandler for RequestHandlerImpl {
    async fn handle_request<R: ResponseHandler>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let queries = request.queries();
        let Some(query) = queries.first() else {
            return reply_error(request, &mut response_handle, ResponseCode::FormErr).await;
        };

        let qname = hickory_proto::rr::Name::from(query.name().clone());
        let qtype = query.query_type();
        debug!(qname = %qname, ?qtype, "isengard-dns: query received");

        // Authoritative first.
        match self.zones.resolve(&qname, qtype).await {
            Ok(Some(records)) => {
                return send_answer(request, &mut response_handle, records, true).await;
            }
            Ok(None) => {
                // Fall through to upstream.
            }
            Err(e) => {
                warn!(error = %e, qname = %qname, "zone source returned error; replying ServFail");
                return reply_error(request, &mut response_handle, ResponseCode::ServFail).await;
            }
        }

        // Forward upstream.
        match self.upstream.lookup(qname.clone(), qtype).await {
            Ok(lookup) => {
                let records: Vec<Record> = lookup.records().to_vec();
                send_answer(request, &mut response_handle, records, false).await
            }
            Err(e) => {
                // Negative-answer kinds become NXDOMAIN; everything else
                // is a server problem.
                let code = if e.is_no_records_found() {
                    ResponseCode::NXDomain
                } else {
                    warn!(error = %e, qname = %qname, "upstream lookup failed; replying ServFail");
                    ResponseCode::ServFail
                };
                reply_error(request, &mut response_handle, code).await
            }
        }
    }
}

/// Build and send a standard answer.
///
/// `authoritative` is `true` for zone-source hits (sets the AA bit) and
/// `false` for upstream-forwarded answers.
///
/// If the client query carried an EDNS0 OPT record we echo a server OPT
/// back. Hickory's response handler uses the EDNS UDP payload size to
/// decide when to set the TC bit, so omitting the echo would leave a
/// non-EDNS client and any EDNS-aware client both subject to the same
/// 4 KiB default.
async fn send_answer<R: ResponseHandler>(
    request: &Request,
    response_handle: &mut R,
    answers: Vec<Record>,
    authoritative: bool,
) -> ResponseInfo {
    let mut builder = MessageResponseBuilder::from_message_request(request);
    if let Some(client_edns) = request.edns() {
        builder.edns(server_edns_for(client_edns));
    }

    let mut header = Header::response_from_request(request.header());
    header.set_message_type(MessageType::Response);
    header.set_authoritative(authoritative);
    header.set_response_code(ResponseCode::NoError);

    let response = builder.build(header, answers.iter(), &[], &[], &[]);
    match response_handle.send_response(response).await {
        Ok(info) => info,
        Err(e) => {
            warn!(error = %e, "failed to send DNS answer");
            ResponseInfo::from(*request.header())
        }
    }
}

/// Build and send an error response.
async fn reply_error<R: ResponseHandler>(
    request: &Request,
    response_handle: &mut R,
    code: ResponseCode,
) -> ResponseInfo {
    let mut builder = MessageResponseBuilder::from_message_request(request);
    if let Some(client_edns) = request.edns() {
        builder.edns(server_edns_for(client_edns));
    }
    let response = builder.error_msg(request.header(), code);
    match response_handle.send_response(response).await {
        Ok(info) => info,
        Err(e) => {
            warn!(error = %e, ?code, "failed to send DNS error response");
            ResponseInfo::from(*request.header())
        }
    }
}

/// Build the server's EDNS OPT record to echo back to a client that sent
/// one. RFC 6891: server announces its own max payload (we cap at the
/// client's stated value so we never exceed what they can buffer) and
/// echoes EDNS version 0 unless they sent something newer (we don't
/// implement EDNS > 0 features yet, so anything else gets clamped).
fn server_edns_for(client: &hickory_proto::op::Edns) -> hickory_proto::op::Edns {
    let mut edns = hickory_proto::op::Edns::new();
    edns.set_max_payload(client.max_payload()).set_version(0);
    edns
}
