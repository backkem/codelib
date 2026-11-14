//! Custom policy hooks, run after the allowlist and before credentials.

use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;

use crate::body::{Request, Response};
use crate::egress::Inner;
use crate::error::EgressError;

/// Custom policy hook, in the style of axum's `from_fn`.
///
/// Middlewares run in the order they were added with
/// [`EgressBuilder::layer`](crate::EgressBuilder::layer), only for requests
/// that matched an allow rule, and before credentials are attached, so they
/// never see secrets. A middleware may:
///
/// - deny: return `Err(EgressError::Denied { .. })` without calling `next`
///   (drain the body first with [`drain`](crate::drain) if the sandbox
///   runtime needs its outgoing stream consumed);
/// - short-circuit: return a response without calling `next`;
/// - modify the request and/or the response around `next.run(req)`.
///
/// If a middleware changes the request so that it no longer satisfies the
/// rule it matched (host, path, method or scheme), it is denied before any
/// credential is attached.
#[async_trait]
pub trait Middleware: Send + Sync + 'static {
    /// Handle one request.
    async fn handle(&self, req: Request, next: Next<'_>) -> Result<Response, EgressError>;
}

#[async_trait]
impl<T: Middleware + ?Sized> Middleware for Arc<T> {
    async fn handle(&self, req: Request, next: Next<'_>) -> Result<Response, EgressError> {
        (**self).handle(req, next).await
    }
}

/// The rest of the pipeline: later middlewares, then credentials, rewrite and
/// the transport.
///
/// `Next` currently owns its state (the lifetime is reserved), which is what
/// lets [`from_fn`] closures move it into an `async move` block; such
/// closures receive a `Next<'static>`.
pub struct Next<'a> {
    pub(crate) inner: Arc<Inner>,
    pub(crate) rule: usize,
    pub(crate) pos: usize,
    pub(crate) _marker: PhantomData<&'a ()>,
}

impl fmt::Debug for Next<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Next")
            .field("rule", &self.rule)
            .field("pos", &self.pos)
            .finish()
    }
}

impl Next<'_> {
    pub(crate) fn new(inner: Arc<Inner>, rule: usize) -> Next<'static> {
        Next {
            inner,
            rule,
            pos: 0,
            _marker: PhantomData,
        }
    }

    fn into_static(self) -> Next<'static> {
        Next {
            inner: self.inner,
            rule: self.rule,
            pos: self.pos,
            _marker: PhantomData,
        }
    }

    /// Pass the request on to the rest of the pipeline.
    pub async fn run(self, req: Request) -> Result<Response, EgressError> {
        let inner = self.inner.clone();
        match inner.layers.get(self.pos) {
            Some(mw) => {
                let next = Next {
                    pos: self.pos + 1,
                    ..self.into_static()
                };
                mw.handle(req, next).await
            }
            None => inner.dispatch(self.rule, req).await,
        }
    }
}

/// A [`Middleware`] built from a closure; see [`from_fn`].
pub struct FromFn<F>(F);

impl<F> fmt::Debug for FromFn<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FromFn")
    }
}

/// Build a middleware from an async closure.
///
/// ```
/// use codelib_http::{EgressError, Egress, Rule, middleware};
///
/// let egress = Egress::builder()
///     .allow(Rule::host("api.example.com"))
///     .layer(middleware::from_fn(|req, next| async move {
///         if req.uri().path().starts_with("/admin") {
///             return Err(EgressError::denied("admin API is off limits"));
///         }
///         next.run(req).await
///     }))
///     .build();
/// # let _ = egress;
/// ```
pub fn from_fn<F, Fut>(f: F) -> FromFn<F>
where
    F: Fn(Request, Next<'static>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response, EgressError>> + Send + 'static,
{
    FromFn(f)
}

#[async_trait]
impl<F, Fut> Middleware for FromFn<F>
where
    F: Fn(Request, Next<'static>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response, EgressError>> + Send + 'static,
{
    async fn handle(&self, req: Request, next: Next<'_>) -> Result<Response, EgressError> {
        (self.0)(req, next.into_static()).await
    }
}

/// In-process handler for a virtual host such as `files.internal`.
///
/// Requests to a virtual host never touch the network, bypass the allowlist
/// and middlewares, and never get credentials. Closures
/// `Fn(Request) -> impl Future<Output = Response> + Send` implement it too.
#[async_trait]
pub trait VirtualHost: Send + Sync + 'static {
    /// Produce the response. The handler owns the request body and should
    /// consume it (see [`drain`](crate::drain)).
    async fn handle(&self, req: Request) -> Response;
}

#[async_trait]
impl<F, Fut> VirtualHost for F
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    async fn handle(&self, req: Request) -> Response {
        (self)(req).await
    }
}
