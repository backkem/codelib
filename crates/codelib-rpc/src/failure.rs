//! The error type returned to callers.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable, machine-readable failure codes used by the router itself.
///
/// Handlers are free to use their own codes (`not-found`, `conflict`, ...).
pub mod codes {
    /// No method with that name is registered.
    pub const UNKNOWN_METHOD: &str = "unknown-method";
    /// The arguments did not decode into the handler's argument type.
    pub const BAD_ARGS: &str = "bad-args";
    /// A guard refused the call.
    pub const DENIED: &str = "denied";
    /// Something went wrong on the host side (for example a result that cannot be encoded).
    pub const INTERNAL: &str = "internal";
    /// An async method was called through a synchronous entry point
    /// ([`Router::call_sync`](crate::Router::call_sync)).
    pub const ASYNC_ONLY: &str = "async-only";
}

/// An error returned to the caller of a method.
///
/// It is serializable so transports can forward it as-is; `hint` and `detail` are omitted from
/// the serialized form when absent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    /// Stable, machine-readable code, see [`codes`].
    pub code: String,
    /// Human-readable description.
    pub message: String,
    /// What the caller could do about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Optional structured detail. Boxed so that `Result<_, Failure>` stays small even when
    /// `serde_json` keeps key order, which makes a bare `Value` large.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Box<serde_json::Value>>,
}

impl Failure {
    /// A failure with any code.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Failure {
            code: code.into(),
            message: message.into(),
            hint: None,
            detail: None,
        }
    }

    /// [`codes::UNKNOWN_METHOD`] for `name`. A name longer than
    /// 128 bytes is cut on a character boundary and
    /// marked with `…` in the message, so an attacker-sized name is not echoed back in full.
    pub fn unknown_method(name: &str) -> Self {
        let shown = truncate(name, MAX_ECHOED_NAME);
        let message = if shown.len() < name.len() {
            format!("no method \"{}…\"", shown.escape_debug())
        } else {
            format!("no method {name:?}")
        };
        Failure::new(codes::UNKNOWN_METHOD, message)
    }

    /// [`codes::BAD_ARGS`].
    pub fn bad_args(message: impl Into<String>) -> Self {
        Failure::new(codes::BAD_ARGS, message)
    }

    /// [`codes::DENIED`].
    pub fn denied(message: impl Into<String>) -> Self {
        Failure::new(codes::DENIED, message)
    }

    /// [`codes::INTERNAL`].
    pub fn internal(message: impl Into<String>) -> Self {
        Failure::new(codes::INTERNAL, message)
    }

    /// [`codes::ASYNC_ONLY`] for `name`.
    pub fn async_only(name: &str) -> Self {
        Failure::new(
            codes::ASYNC_ONLY,
            format!("method {name:?} is async and cannot be called synchronously"),
        )
    }

    /// Sets the hint.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Sets the structured detail.
    pub fn with_detail(mut self, detail: impl Into<serde_json::Value>) -> Self {
        self.detail = Some(Box::new(detail.into()));
        self
    }

    /// Whether this failure has the given code.
    pub fn is(&self, code: &str) -> bool {
        self.code == code
    }
}

/// Longest method name (in bytes) echoed in an unknown-method message.
const MAX_ECHOED_NAME: usize = 128;

/// The longest prefix of `s` of at most `max` bytes that ends on a character boundary.
pub(crate) fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, " ({hint})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Failure {}
