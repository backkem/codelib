//! Host-side redirect following.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use http::{HeaderValue, Method, StatusCode, Uri, header};
use http_body::Body as _;
use http_body_util::BodyExt;

use crate::body::{Body, Request, Response, drain, empty_body, full_body};
use crate::egress::Inner;
use crate::error::EgressError;
use crate::rule::normalize_host;

/// What to do with 3xx responses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Redirects {
    /// Hand redirects back to the script unchanged (default).
    #[default]
    Manual,
    /// Follow up to `n` redirects on the host. Every hop goes through the full
    /// policy again; one more redirect than `n` fails with
    /// [`EgressError::TooManyRedirects`].
    ///
    /// Methods change as in the Fetch standard: 301 and 302 turn POST into
    /// GET; 303 turns everything but GET and HEAD into GET; 307 and 308 keep
    /// method and body. Whenever the method becomes GET the body and its
    /// headers (`Content-Length`, `Content-Type`, `Content-Encoding`,
    /// `Content-Language`, `Content-Location`, `Transfer-Encoding`) are
    /// dropped. The script's `Authorization`, `Cookie` and
    /// `Proxy-Authorization` are dropped when a hop leaves the original host
    /// and its subdomains, or goes from https to http. A `Location` with user
    /// info fails with [`EgressError::Protocol`].
    ///
    /// The request body is buffered (up to
    /// [`max_buffered_request_bytes`](crate::EgressBuilder::max_buffered_request_bytes))
    /// so 307/308 can resend it, but only after the first hop has passed the
    /// virtual-host, rule, method and https checks.
    Follow(usize),
}

/// Request headers that describe the body; dropped with it.
const BODY_HEADERS: [header::HeaderName; 6] = [
    header::CONTENT_LENGTH,
    header::CONTENT_TYPE,
    header::CONTENT_ENCODING,
    header::CONTENT_LANGUAGE,
    header::CONTENT_LOCATION,
    header::TRANSFER_ENCODING,
];

/// Does a redirect with `status` turn a `method` request into GET?
fn becomes_get(status: StatusCode, method: &Method) -> bool {
    match status.as_u16() {
        301 | 302 => *method == Method::POST,
        303 => *method != Method::GET && *method != Method::HEAD,
        _ => false,
    }
}

/// Script headers that must not follow a redirect off the original site or
/// onto plain http.
const SENSITIVE: [header::HeaderName; 3] = [
    header::AUTHORIZATION,
    header::COOKIE,
    header::PROXY_AUTHORIZATION,
];

fn is_followed(status: StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

/// Resolve `Location` against the URL of the hop that answered.
fn resolve_location(base: &Uri, location: &HeaderValue) -> Result<Uri, EgressError> {
    let bad = |why: &str| EgressError::Protocol(format!("invalid redirect: {why}"));
    let location = location
        .to_str()
        .map_err(|_| bad("Location is not ASCII"))?;
    let base = url::Url::parse(&base.to_string()).map_err(|_| bad("base URL"))?;
    let mut next = base
        .join(location)
        .map_err(|_| bad("unparsable Location"))?;
    if !matches!(next.scheme(), "http" | "https") {
        return Err(bad("unsupported scheme"));
    }
    if !next.username().is_empty() || next.password().is_some() {
        return Err(bad("Location contains user info"));
    }
    next.set_fragment(None);
    next.as_str()
        .parse::<Uri>()
        .map_err(|_| bad("unparsable Location"))
}

impl Inner {
    /// Send with [`Redirects::Follow`]: check the first hop, buffer the body
    /// (so 307/308 can resend it), then loop over hops.
    pub(crate) async fn follow(
        self: &Arc<Self>,
        mut req: Request,
        max: usize,
    ) -> Result<Response, EgressError> {
        // A request the policy refuses is refused before its body is held in
        // memory. (The first hop runs the same checks again; they are cheap.)
        if let Err(e) = self.policy(&mut req) {
            return Err(self.refuse(req, e).await);
        }
        let (parts, body) = req.into_parts();
        let mut body_bytes = if body.is_end_stream() {
            None
        } else {
            Some(self.buffer(body).await?)
        };
        let http::request::Parts {
            mut method,
            mut uri,
            version,
            mut headers,
            extensions,
            ..
        } = parts;
        headers.remove(header::HOST);
        let orig_host = normalize_host(uri.host().unwrap_or(""));
        let mut hops = 0usize;

        loop {
            let body = match &body_bytes {
                Some(b) => full_body(b.clone()),
                None => empty_body(),
            };
            let mut req = Request::new(body);
            *req.method_mut() = method.clone();
            *req.uri_mut() = uri.clone();
            *req.version_mut() = version;
            *req.headers_mut() = headers.clone();
            *req.extensions_mut() = extensions.clone();

            let resp = self.hop(req).await?;
            let status = resp.status();
            if !is_followed(status) {
                return Ok(resp);
            }
            let Some(location) = resp.headers().get(header::LOCATION).cloned() else {
                return Ok(resp);
            };
            drop(resp);
            if hops >= max {
                tracing::warn!(
                    host = %orig_host,
                    max,
                    "egress: too many redirects"
                );
                return Err(EgressError::TooManyRedirects);
            }
            hops += 1;
            let next = resolve_location(&uri, &location)?;

            if becomes_get(status, &method) {
                method = Method::GET;
                body_bytes = None;
                for name in &BODY_HEADERS {
                    headers.remove(name);
                }
            }

            let next_host = normalize_host(next.host().unwrap_or(""));
            let same_site = next_host == orig_host || next_host.ends_with(&format!(".{orig_host}"));
            let downgrade = uri.scheme_str() == Some("https") && next.scheme_str() == Some("http");
            if !same_site || downgrade {
                for name in &SENSITIVE {
                    headers.remove(name);
                }
            }
            tracing::debug!(
                status = status.as_u16(),
                from_host = uri.host().unwrap_or(""),
                to_host = next.host().unwrap_or(""),
                to_path = next.path(),
                hop = hops,
                "egress: following redirect"
            );
            uri = next;
        }
    }

    /// Buffer a request body for resending, bounded in size and time.
    async fn buffer(&self, body: Body) -> Result<Bytes, EgressError> {
        let limit = self.max_buffered_request_bytes;
        let collect = async move {
            let mut body = body;
            let mut buf = BytesMut::new();
            while let Some(frame) = body.frame().await {
                let frame =
                    frame.map_err(|e| EgressError::InvalidRequest(format!("request body: {e}")))?;
                if let Ok(data) = frame.into_data() {
                    if (buf.len() + data.len()) as u64 > limit {
                        drain(body).await;
                        return Err(EgressError::BodyTooLarge);
                    }
                    buf.extend_from_slice(&data);
                }
            }
            Ok(buf.freeze())
        };
        tokio::time::timeout(self.request_timeout, collect)
            .await
            .map_err(|_| EgressError::Timeout)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(base: &str, loc: &str) -> Result<String, EgressError> {
        resolve_location(&base.parse().unwrap(), &HeaderValue::from_str(loc).unwrap())
            .map(|u| u.to_string())
    }

    #[test]
    fn location_resolution() {
        assert_eq!(
            resolve("https://a.example/x/y?q=1", "z").unwrap(),
            "https://a.example/x/z"
        );
        assert_eq!(
            resolve("https://a.example/x/y", "/abs#frag").unwrap(),
            "https://a.example/abs"
        );
        assert_eq!(
            resolve("https://a.example/x", "//b.example/p").unwrap(),
            "https://b.example/p"
        );
        assert_eq!(
            resolve("https://a.example/x", "http://c.example:8080/").unwrap(),
            "http://c.example:8080/"
        );
        assert!(resolve("https://a.example/x", "file:///etc/passwd").is_err());
        assert!(resolve("https://a.example/x", "javascript:alert(1)").is_err());
        assert!(resolve("https://a.example/x", "https://u:p@b.example/").is_err());
        assert!(resolve("https://a.example/x", "//u@b.example/").is_err());
    }

    #[test]
    fn method_changes_follow_fetch() {
        let s = |n| StatusCode::from_u16(n).unwrap();
        for status in [301, 302] {
            assert!(becomes_get(s(status), &Method::POST));
            for m in [
                Method::GET,
                Method::HEAD,
                Method::PUT,
                Method::DELETE,
                Method::PATCH,
            ] {
                assert!(!becomes_get(s(status), &m), "{status} {m}");
            }
        }
        for m in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            assert!(becomes_get(s(303), &m), "{m}");
        }
        assert!(!becomes_get(s(303), &Method::GET));
        assert!(!becomes_get(s(303), &Method::HEAD));
        for status in [307, 308] {
            for m in [Method::POST, Method::PUT, Method::DELETE] {
                assert!(!becomes_get(s(status), &m), "{status} {m}");
            }
        }
    }
}
