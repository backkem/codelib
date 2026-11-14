#![allow(dead_code)]

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use codelib_http::http::{HeaderMap, Method, Uri};
use codelib_http::{
    Body, BoxError, EgressError, Request, Response, Rewritten, StatusCode, Transport, async_trait,
    full_body, text_response,
};
use http_body::Frame;
use http_body_util::BodyExt;

/// A body that yields the given chunks one by one, with no exact size hint,
/// and records when it has been read to the end.
pub struct Chunks {
    chunks: VecDeque<Bytes>,
    pub finished: Arc<AtomicBool>,
}

impl Chunks {
    pub fn new(chunks: impl IntoIterator<Item = &'static [u8]>) -> Self {
        Self {
            chunks: chunks.into_iter().map(Bytes::from_static).collect(),
            finished: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn body(self) -> Body {
        self.boxed_unsync()
    }
}

impl http_body::Body for Chunks {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        match self.chunks.pop_front() {
            Some(c) => Poll::Ready(Some(Ok(Frame::data(c)))),
            None => {
                self.finished.store(true, Ordering::SeqCst);
                Poll::Ready(None)
            }
        }
    }
}

/// What the mock transport received.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub rewritten_from: Option<Uri>,
}

type Handler = dyn Fn(&Seen, usize) -> Response + Send + Sync;

/// Transport that records requests and answers with a closure
/// (`(request, index of this request)`).
#[derive(Clone)]
pub struct Mock {
    pub seen: Arc<Mutex<Vec<Seen>>>,
    handler: Arc<Handler>,
}

impl Mock {
    pub fn new(handler: impl Fn(&Seen, usize) -> Response + Send + Sync + 'static) -> Self {
        Self {
            seen: Arc::default(),
            handler: Arc::new(handler),
        }
    }

    pub fn ok() -> Self {
        Self::new(|_, _| text_response(StatusCode::OK, "ok"))
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait]
impl Transport for Mock {
    async fn send(&self, req: Request) -> Result<Response, EgressError> {
        let (parts, body) = req.into_parts();
        let body = body.collect().await.map_err(EgressError::Other)?.to_bytes();
        let seen = Seen {
            method: parts.method,
            uri: parts.uri,
            headers: parts.headers,
            body,
            rewritten_from: parts
                .extensions
                .get::<Rewritten>()
                .map(|r| r.original().clone()),
        };
        let idx = {
            let mut s = self.seen.lock().unwrap();
            s.push(seen.clone());
            s.len() - 1
        };
        Ok((self.handler)(&seen, idx))
    }
}

pub fn req(method: Method, url: &str) -> Request {
    codelib_http::http::Request::builder()
        .method(method)
        .uri(url)
        .body(codelib_http::empty_body())
        .unwrap()
}

pub fn get(url: &str) -> Request {
    req(Method::GET, url)
}

pub fn with_body(method: Method, url: &str, body: &'static str) -> Request {
    codelib_http::http::Request::builder()
        .method(method)
        .uri(url)
        .header("content-type", "text/plain")
        .header("content-length", body.len().to_string())
        .body(full_body(body))
        .unwrap()
}

pub async fn read_body(resp: Response) -> Result<Bytes, EgressError> {
    resp.into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(EgressError::from_body_error)
}

pub fn redirect(status: u16, location: &str) -> Response {
    let mut r = codelib_http::status_response(StatusCode::from_u16(status).unwrap());
    r.headers_mut()
        .insert("location", location.parse().unwrap());
    r
}

pub fn assert_denied<T: std::fmt::Debug>(r: Result<T, EgressError>) -> String {
    match r {
        Err(EgressError::Denied { reason }) => reason,
        other => panic!("expected Denied, got {other:?}"),
    }
}
