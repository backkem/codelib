//! Bridges the guest's `wasi:http` outgoing requests to a
//! [`codelib_http::Egress`].
//!
//! wasmtime hands requests from `wasi:http` 0.2 (`outgoing-handler`) and
//! 0.3 (`client.send`) to the same [`WasiHttpHooks::send_request`], so one
//! implementation serves both versions, counting and all.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use codelib_http::{BoxError, Egress, EgressError};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt as _;
use wasmtime_wasi_http::{Error as WasiError, RequestOptions, WasiBody, WasiHttpHooks};

use crate::capture::Capture;

/// How long to keep reading a request body nobody consumed before giving up.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

type IoFuture = Box<dyn Future<Output = Result<(), WasiError>> + Send>;
type SendFuture =
    Box<dyn Future<Output = Result<(http::Response<WasiBody>, IoFuture), WasiError>> + Send>;

/// The `wasi:http` hooks of one session: every outgoing request goes
/// through the session's [`Egress`].
pub(crate) struct EgressHooks {
    egress: Egress,
    requests: Arc<AtomicU64>,
    /// The session's captured stderr, for notes about refused requests.
    notes: Capture,
}

impl EgressHooks {
    pub(crate) fn new(egress: Egress, notes: Capture) -> Self {
        Self {
            egress,
            requests: Arc::default(),
            notes,
        }
    }

    pub(crate) fn requests(&self) -> Arc<AtomicU64> {
        self.requests.clone()
    }
}

impl WasiHttpHooks for EgressHooks {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        _options: Option<RequestOptions>,
        _fut: IoFuture,
    ) -> SendFuture {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let egress = self.egress.clone();
        let notes = self.notes.clone();
        let target = describe(request.method(), request.uri());

        Box::new(async move {
            // Lend the body out instead of giving it away, so that it can be
            // read to the end here if the egress drops the request without
            // consuming it (a denial, a virtual host that ignores the body).
            // A guest whose outgoing stream is left dangling tends to trap
            // later instead of seeing a catchable `fetch` error.
            let (parts, body) = request.into_parts();
            let slot = Arc::new(Mutex::new(Some(body)));
            let lent = LentBody { slot: slot.clone() };
            let request = http::Request::from_parts(parts, lent.boxed_unsync());

            let result = egress.send(request).await;
            drain_unconsumed(slot).await;

            match result {
                Ok(response) => {
                    let response = response.map(|body| body.map_err(body_error).boxed_unsync());
                    // The transport drives its own connections; there is no
                    // request-side I/O left for the caller to poll.
                    let io: IoFuture = Box::new(async { Ok(()) });
                    Ok((response, io))
                }
                Err(error) => {
                    // The guest only sees an opaque network error. Say why in
                    // the console, where the program's author will look.
                    match &error {
                        EgressError::Denied { reason } => {
                            notes.note(&format!("fetch denied: {target} ({reason})"));
                        }
                        EgressError::Credential(reason) => {
                            notes.note(&format!("fetch failed: {target} (credential: {reason})"));
                        }
                        _ => {}
                    }
                    Err(wasi_error(error))
                }
            }
        })
    }
}

/// How a request is named in console notes: method, scheme, host, port and
/// path. The query string can carry secrets, and the authority's user-info
/// is a credential, so neither is ever repeated.
fn describe(method: &http::Method, uri: &http::Uri) -> String {
    let port = uri.port_u16().map(|port| format!(":{port}"));
    format!(
        "{method} {}://{}{}{}",
        uri.scheme_str().unwrap_or("?"),
        uri.host().unwrap_or("?"),
        port.as_deref().unwrap_or(""),
        uri.path(),
    )
}

/// A request body that stays owned by the hook: polling goes through a
/// shared slot, so whatever the reader leaves behind can still be drained.
struct LentBody {
    slot: Arc<Mutex<Option<WasiBody>>>,
}

impl Body for LentBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        let Some(body) = slot.as_mut() else {
            return Poll::Ready(None);
        };
        match Pin::new(body).poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(Some(Err(error))) => {
                *slot = None;
                Poll::Ready(Some(Err(Box::new(error))))
            }
            Poll::Ready(None) => {
                *slot = None;
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        let slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().is_none_or(|body| body.is_end_stream())
    }

    fn size_hint(&self) -> SizeHint {
        let slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref()
            .map(|body| body.size_hint())
            .unwrap_or_else(|| SizeHint::with_exact(0))
    }
}

/// Read what is left of the request body, but only if nobody else still
/// holds it (a transport may legitimately keep streaming an upload after the
/// response headers arrived).
async fn drain_unconsumed(slot: Arc<Mutex<Option<WasiBody>>>) {
    if Arc::strong_count(&slot) > 1 {
        return;
    }
    let body = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(mut body) = body else { return };
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, async {
        while let Some(frame) = body.frame().await {
            if frame.is_err() {
                break;
            }
        }
    })
    .await;
}

fn wasi_error(error: EgressError) -> WasiError {
    match error {
        EgressError::Denied { .. } => WasiError::HttpRequestDenied,
        EgressError::Timeout => WasiError::ConnectionTimeout,
        EgressError::Dns(_) => WasiError::DnsError {
            rcode: None,
            info_code: None,
        },
        EgressError::Connect(_) => WasiError::ConnectionRefused,
        EgressError::Tls(_) => WasiError::TlsProtocolError,
        EgressError::Protocol(_) => WasiError::HttpProtocolError,
        EgressError::TooManyRedirects => WasiError::LoopDetected,
        EgressError::BodyTooLarge => WasiError::HttpResponseBodySize(None),
        EgressError::InvalidRequest(_) => WasiError::HttpRequestUriInvalid,
        other => WasiError::InternalError(Some(other.to_string())),
    }
}

fn body_error(error: BoxError) -> WasiError {
    wasi_error(EgressError::from_body_error(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(method: &str, uri: &str) -> String {
        describe(&method.parse().unwrap(), &uri.parse().unwrap())
    }

    #[test]
    fn names_host_port_and_path_only() {
        assert_eq!(
            target("GET", "https://api.example.com/v1/items?token=secret"),
            "GET https://api.example.com/v1/items"
        );
        assert_eq!(
            target("POST", "http://localhost:8080/x"),
            "POST http://localhost:8080/x"
        );
    }

    #[test]
    fn user_info_is_never_echoed() {
        let text = target("GET", "https://user:hunter2@example.com:444/p?q=1");
        assert_eq!(text, "GET https://example.com:444/p");
        assert!(!text.contains("hunter2") && !text.contains("user"));
    }

    #[test]
    fn missing_parts_are_marked() {
        assert_eq!(target("GET", "/only/a/path"), "GET ?://?/only/a/path");
    }
}
