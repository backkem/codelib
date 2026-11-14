//! The single error type of the egress pipeline.

use crate::body::BoxError;

/// Why an outgoing request did not produce a response.
///
/// Messages never contain header values or secrets, so they are safe to hand
/// back to the sandboxed script.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EgressError {
    /// Refused by policy: no matching rule, wrong method or scheme, a
    /// middleware said no, or the destination address is not public.
    #[error("request denied: {reason}")]
    Denied {
        /// Human-readable reason (no secrets).
        reason: String,
    },
    /// No response headers within the request timeout (or the connect
    /// timeout of the transport elapsed).
    #[error("request timed out")]
    Timeout,
    /// Name resolution failed.
    #[error("dns error: {0}")]
    Dns(String),
    /// TCP connection could not be established.
    #[error("connect error: {0}")]
    Connect(String),
    /// TLS handshake or certificate verification failed.
    #[error("tls error: {0}")]
    Tls(String),
    /// HTTP protocol error (malformed response, connection reset mid-exchange, bad redirect).
    #[error("protocol error: {0}")]
    Protocol(String),
    /// More redirects than [`Redirects::Follow`](crate::Redirects::Follow) allows.
    #[error("too many redirects")]
    TooManyRedirects,
    /// The response body exceeded `max_response_bytes`, or the request body
    /// exceeded the buffer used for redirect following.
    #[error("body too large")]
    BodyTooLarge,
    /// The request itself is malformed (no host, unsupported scheme, ...).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// A credential provider or signer failed.
    #[error("credential error: {0}")]
    Credential(String),
    /// Anything else.
    #[error(transparent)]
    Other(BoxError),
}

impl EgressError {
    /// Shorthand for [`EgressError::Denied`].
    pub fn denied(reason: impl Into<String>) -> Self {
        Self::Denied {
            reason: reason.into(),
        }
    }

    /// Classify an error yielded by a response [`Body`](crate::Body) while
    /// streaming.
    ///
    /// Bodies produced by [`Egress`](crate::Egress) report the size cap as a
    /// boxed `EgressError::BodyTooLarge`; this recovers it (and any other
    /// boxed `EgressError`). Other errors become [`EgressError::Protocol`].
    pub fn from_body_error(err: BoxError) -> Self {
        match err.downcast::<EgressError>() {
            Ok(e) => *e,
            Err(other) => Self::Protocol(error_chain(&*other)),
        }
    }
}

/// `err` and its sources joined with ": ".
pub(crate) fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(e) = cur {
        let s = e.to_string();
        if !out.contains(&s) {
            out.push_str(": ");
            out.push_str(&s);
        }
        cur = e.source();
    }
    out
}
