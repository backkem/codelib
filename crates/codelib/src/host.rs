//! The host side of the script's `host.call` and `host.emit`.

use std::any::Any;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use codelib_rpc::{Failure, Json, Router};
use codelib_sandbox::{Ctx, wasmtime};
use serde_json::Value;

use crate::bindings::{host, types};

/// A boxed, sendable future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Answers the script's `host.call(method, args)`.
///
/// A [`codelib_rpc::Router`] implements this, and is the usual choice;
/// implement it by hand (or use [`host_fn`]) to plug in an existing
/// dispatcher.
///
/// What `args` holds depends on how the script called:
///
/// - `host.call(method, value)`: the JSON text of `value` (`null` if it was
///   omitted), and the returned string must be JSON text (empty for no
///   result, which the script sees as `undefined`).
/// - `host.callRaw(method, text)`: `text` exactly as the script passed it,
///   and the returned string goes back to the script exactly as it is. The
///   handler may use any format it likes for both. (A [`Router`] reads its
///   arguments as JSON, so it suits `host.call`.)
///
/// # Calling back into the sandbox
///
/// A handler that needs to run another script keeps a
/// [`Sandbox`](crate::Sandbox) clone in its state. Nested runs each take a
/// [`max_concurrent`](crate::SandboxBuilder::max_concurrent) slot while the
/// outer run still holds its own, so a limit of 1 deadlocks. The outer
/// run's deadline is not passed to handlers: if nested work must respect
/// it, keep a deadline in the state and apply it to the nested run.
///
/// # Cancellation and panics
///
/// The returned future can be dropped at any `.await`: when the run's time
/// budget runs out while the script waits for the call, or when the caller
/// drops the run. A handler that panics ends the run with
/// [`RunError::Sandbox`](crate::RunError::Sandbox). In both cases the state
/// is still handed back in the [`Outcome`](crate::Outcome), but possibly
/// half-updated: keep changes to it that must happen together on one side of
/// an `.await`, or make them in a way that tolerates being cut short.
pub trait HostCalls<S>: Send + Sync + 'static {
    /// Handle one call with exclusive access to the run's state.
    fn call<'a>(
        &'a self,
        state: &'a mut S,
        method: &'a str,
        args: &'a str,
    ) -> BoxFuture<'a, Result<String, Failure>>;
}

impl<S: Send + 'static> HostCalls<S> for Router<S, Json> {
    fn call<'a>(
        &'a self,
        state: &'a mut S,
        method: &'a str,
        args: &'a str,
    ) -> BoxFuture<'a, Result<String, Failure>> {
        Box::pin(self.call_json(state, method, args))
    }
}

/// [`HostCalls`] from a plain synchronous function
/// `fn(&mut S, method, args_json) -> Result<result_json, Failure>`.
///
/// The function runs on the async executor and cannot be interrupted: the
/// run's deadline only takes effect once it returns. Keep it short; do
/// long or blocking work in an async [`HostCalls`] (e.g. a
/// [`Router`] with `method_async`) instead.
pub fn host_fn<S, F>(f: F) -> impl HostCalls<S>
where
    S: Send + 'static,
    F: Fn(&mut S, &str, &str) -> Result<String, Failure> + Send + Sync + 'static,
{
    HostFn(f)
}

struct HostFn<F>(F);

impl<S, F> HostCalls<S> for HostFn<F>
where
    S: Send + 'static,
    F: Fn(&mut S, &str, &str) -> Result<String, Failure> + Send + Sync + 'static,
{
    fn call<'a>(
        &'a self,
        state: &'a mut S,
        method: &'a str,
        args: &'a str,
    ) -> BoxFuture<'a, Result<String, Failure>> {
        let result = (self.0)(state, method, args);
        Box::pin(std::future::ready(result))
    }
}

/// Receives the script's `host.emit(kind, item)` items as they happen.
pub(crate) type EmitSink = Box<dyn FnMut(&str, Value) + Send>;

/// What codelib keeps in the store for one run, around the embedder's state.
pub(crate) struct State<S: 'static> {
    pub(crate) user: S,
    pub(crate) calls: Option<Arc<dyn HostCalls<S>>>,
    pub(crate) emit: Option<EmitSink>,
}

/// A panic in host code, turned into the trap that ends the run.
#[derive(Debug)]
pub(crate) struct HostPanic {
    what: &'static str,
    message: String,
}

impl HostPanic {
    fn new(what: &'static str, payload: Box<dyn Any + Send>) -> Self {
        let message = if let Some(text) = payload.downcast_ref::<&str>() {
            (*text).to_string()
        } else if let Some(text) = payload.downcast_ref::<String>() {
            text.clone()
        } else {
            "(no message)".to_string()
        };
        Self { what, message }
    }
}

impl std::fmt::Display for HostPanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} panicked: {}", self.what, self.message)
    }
}

impl std::error::Error for HostPanic {}

/// Polls a future, catching a panic in any poll.
struct CatchUnwind<F>(F);

impl<F: Future + Unpin> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn Any + Send>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let future = &mut self.0;
        match catch_unwind(AssertUnwindSafe(|| Pin::new(future).poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

const HOST_CALL: &str = "a host call";

fn catch_panic<R>(f: impl FnOnce() -> R) -> Result<R, Box<dyn Any + Send>> {
    catch_unwind(AssertUnwindSafe(f))
}

impl<S: Send + 'static> types::Host for Ctx<State<S>> {}

impl<S: Send + 'static> host::Host for Ctx<State<S>> {
    async fn call(
        &mut self,
        method: String,
        args: String,
    ) -> wasmtime::Result<Result<String, host::Failure>> {
        let State { user, calls, .. } = &mut self.state;
        let Some(calls) = calls.as_ref() else {
            return Ok(Err(Failure::unknown_method(&method).into()));
        };
        // Building the future runs synchronous handlers (`host_fn`), so it
        // can panic too.
        let calls: &dyn HostCalls<S> = &**calls;
        let (name, json) = (method.as_str(), args.as_str());
        let future = catch_panic(move || calls.call(user, name, json))
            .map_err(|payload| wasmtime::Error::new(HostPanic::new(HOST_CALL, payload)))?;
        match CatchUnwind(future).await {
            Ok(result) => Ok(result.map_err(Into::into)),
            Err(payload) => Err(wasmtime::Error::new(HostPanic::new(HOST_CALL, payload))),
        }
    }

    async fn emit(&mut self, kind: String, item: String) -> wasmtime::Result<()> {
        if let Some(sink) = self.state.emit.as_mut() {
            let item = serde_json::from_str(&item).unwrap_or(Value::String(item));
            catch_unwind(AssertUnwindSafe(|| sink(&kind, item))).map_err(|payload| {
                wasmtime::Error::new(HostPanic::new("the emit sink", payload))
            })?;
        }
        Ok(())
    }
}

impl From<Failure> for host::Failure {
    fn from(failure: Failure) -> Self {
        host::Failure {
            code: failure.code,
            message: failure.message,
            hint: failure.hint,
            detail: failure.detail.map(|detail| detail.to_string()),
        }
    }
}
