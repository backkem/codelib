//! Egress policy engine for sandboxed `fetch()`.
//!
//! Scripts running in the codelib sandbox reach the network only through
//! [`Egress::send`]. This crate decides what may leave and what is attached
//! on the way out:
//!
//! - **deny by default**: only hosts matched by an allow [`Rule`] are
//!   reachable (exact or `*.suffix` patterns, optional path prefix, method and
//!   https restrictions);
//! - **host-side credentials**: a rule's [`CredentialProvider`] (or
//!   [`Signer`]) runs on the host after the allowlist matched and overwrites
//!   script-set headers, so secrets never enter the sandbox;
//! - **virtual hosts**: in-process handlers such as `files.internal`;
//! - **redirects** ([`Redirects`]), **URL rewrites**, **timeouts**, **size
//!   caps** and **private-IP blocking** (in [`HyperTransport`]);
//! - **custom policy** via [`Middleware`] / [`middleware::from_fn`].
//!
//! Requests whose URL carries user info (`user:password@`) are refused, and
//! `TRACE`, `TRACK` and `CONNECT` are refused on every rule.
//!
//! The order of these steps is fixed by [`Egress`] (see its docs) so an
//! embedder cannot mis-order them. The crate works on plain `http` /
//! `http-body` types and knows nothing about wasm runtimes.
//!
//! # Limits you should set
//!
//! The script decides how many requests it makes and how large they are, so
//! size the limits for your host: [`max_response_bytes`] (unlimited by
//! default), [`max_buffered_request_bytes`] (32 MiB per request with
//! [`Redirects::Follow`]), [`request_timeout`] (30 s until response headers)
//! and [`body_idle_timeout`] (longest silence while a response body streams;
//! defaults to the request timeout). Concurrency is not limited by this crate.
//!
//! [`max_response_bytes`]: EgressBuilder::max_response_bytes
//! [`max_buffered_request_bytes`]: EgressBuilder::max_buffered_request_bytes
//! [`request_timeout`]: EgressBuilder::request_timeout
//! [`body_idle_timeout`]: EgressBuilder::body_idle_timeout
//!
//! ```no_run
//! use codelib_http::{
//!     EgressError, Egress, EnvBearer, Redirects, Rule, StatusCode, empty_body, middleware,
//!     text_response,
//! };
//! use std::time::Duration;
//!
//! # async fn demo() -> Result<(), EgressError> {
//! let egress = Egress::builder()
//!     .allow(
//!         Rule::host("api.github.com")
//!             .read_only()
//!             .credential(EnvBearer::new("GITHUB_TOKEN")),
//!     )
//!     .allow(Rule::host("*.atlassian.net"))
//!     .virtual_host("files.internal", |_req| async {
//!         text_response(StatusCode::OK, "hello from the host")
//!     })
//!     .layer(middleware::from_fn(|req, next| async move {
//!         // Custom policy; never sees credentials.
//!         next.run(req).await
//!     }))
//!     .redirects(Redirects::Follow(5))
//!     .request_timeout(Duration::from_secs(20))
//!     .max_response_bytes(16 * 1024 * 1024)
//!     .build();
//!
//! let req = codelib_http::http::Request::get("https://api.github.com/user")
//!     .body(empty_body())
//!     .unwrap();
//! let resp = egress.send(req).await?;
//! println!("{}", resp.status());
//! # Ok(())
//! # }
//! ```

mod body;
mod credential;
mod egress;
mod error;
pub mod middleware;
mod redirect;
mod rule;
mod transport;

pub use body::{
    Body, BoxError, Request, Response, drain, empty_body, full_body, json_response,
    status_response, text_response,
};
pub use credential::{
    Credential, CredentialProvider, EnvBasic, EnvBearer, EnvHeader, RequestInfo, Secret, Signer,
    StaticCredential,
};
pub use egress::{
    DEFAULT_MAX_BUFFERED_REQUEST_BYTES, DEFAULT_REQUEST_TIMEOUT, Egress, EgressBuilder,
};
pub use error::EgressError;
pub use middleware::{Middleware, Next, VirtualHost};
pub use redirect::Redirects;
pub use rule::Rule;
pub use transport::{HyperTransport, HyperTransportBuilder, Rewritten, Transport, is_private_ip};

pub use async_trait::async_trait;
pub use http;
pub use http::{HeaderName, HeaderValue, Method, StatusCode, Uri};
