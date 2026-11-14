//! Body type aliases and small constructors for requests and responses.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, StatusCode, header};
use http_body::{Frame, SizeHint};
use http_body_util::{BodyExt, Empty, Full};
use tokio::time::{Instant, Sleep};

use crate::error::EgressError;

/// Boxed error used for body errors and [`EgressError::Other`].
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Streaming body used on both sides of the egress pipeline.
///
/// It is `Send` but deliberately not `Sync` (it is an [`UnsyncBoxBody`]), so
/// never hold a `&Request`/`&Response` across an `.await` in a future that must
/// be `Send`; split the message into parts first.
///
/// [`UnsyncBoxBody`]: http_body_util::combinators::UnsyncBoxBody
pub type Body = http_body_util::combinators::UnsyncBoxBody<Bytes, BoxError>;

/// Outgoing request as seen by the egress pipeline.
pub type Request = http::Request<Body>;

/// Response handed back to the sandbox.
pub type Response = http::Response<Body>;

/// A body holding `bytes` in one frame.
pub fn full_body(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// A body with no data.
pub fn empty_body() -> Body {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// An empty response with the given status.
pub fn status_response(status: StatusCode) -> Response {
    let mut resp = Response::new(empty_body());
    *resp.status_mut() = status;
    resp
}

/// A `text/plain; charset=utf-8` response.
pub fn text_response(status: StatusCode, text: impl Into<String>) -> Response {
    with_content_type(status, full_body(text.into()), "text/plain; charset=utf-8")
}

/// An `application/json` response. `json` must already be serialized JSON.
pub fn json_response(status: StatusCode, json: impl Into<Bytes>) -> Response {
    with_content_type(status, full_body(json), "application/json")
}

fn with_content_type(status: StatusCode, body: Body, content_type: &'static str) -> Response {
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

/// Read a body to its end, discarding every frame as it arrives.
///
/// A sandbox runtime may trap when its outgoing body stream is left
/// unconsumed, so code that refuses a request (for example a custom
/// [`Middleware`](crate::Middleware) that denies) should drain the body
/// first. Memory use stays at one frame regardless of body size.
pub async fn drain(mut body: Body) {
    while let Some(frame) = body.frame().await {
        if frame.is_err() {
            break;
        }
    }
}

/// Wraps a response body and fails with [`EgressError::Timeout`] when the
/// inner body keeps the reader waiting for longer than `timeout` without
/// producing a frame. The timer runs only while the reader is waiting (from
/// the first `Pending` after a frame), so a slow reader is never blamed on
/// the server, and it restarts after every frame.
pub(crate) struct IdleTimeoutBody {
    inner: Body,
    timeout: Duration,
    timer: Option<Pin<Box<Sleep>>>,
    armed: bool,
    failed: bool,
}

impl IdleTimeoutBody {
    pub(crate) fn new(inner: Body, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            timer: None,
            armed: false,
            failed: false,
        }
    }

    fn deadline(&self) -> Instant {
        let now = Instant::now();
        // `Duration::MAX` and friends: wait "forever" instead of overflowing.
        now.checked_add(self.timeout)
            .unwrap_or_else(|| now + Duration::from_secs(60 * 60 * 24 * 365 * 30))
    }
}

impl http_body::Body for IdleTimeoutBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        if this.failed {
            return Poll::Ready(None);
        }
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Pending => {
                if !this.armed {
                    let deadline = this.deadline();
                    match &mut this.timer {
                        Some(t) => t.as_mut().reset(deadline),
                        None => this.timer = Some(Box::pin(tokio::time::sleep_until(deadline))),
                    }
                    this.armed = true;
                }
                let fired = this
                    .timer
                    .as_mut()
                    .is_some_and(|t| t.as_mut().poll(cx).is_ready());
                if fired {
                    this.failed = true;
                    return Poll::Ready(Some(Err(Box::new(EgressError::Timeout))));
                }
                Poll::Pending
            }
            ready => {
                this.armed = false;
                ready
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.failed || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Wraps a response body and fails with [`EgressError::BodyTooLarge`] once
/// more than `remaining` data bytes have streamed through.
pub(crate) struct CappedBody {
    inner: Body,
    remaining: u64,
    failed: bool,
}

impl CappedBody {
    pub(crate) fn new(inner: Body, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
            failed: false,
        }
    }
}

impl http_body::Body for CappedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if self.failed {
            return Poll::Ready(None);
        }
        let poll = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &poll
            && let Some(data) = frame.data_ref()
        {
            let len = data.len() as u64;
            if len > self.remaining {
                self.failed = true;
                return Poll::Ready(Some(Err(Box::new(EgressError::BodyTooLarge))));
            }
            self.remaining -= len;
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.failed || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
