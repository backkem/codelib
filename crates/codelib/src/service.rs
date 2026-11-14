use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use codelib_http::{Body, BoxError};
use codelib_sandbox::wasmtime_wasi_http::p2::bindings::http::types::{ErrorCode, Scheme};
use codelib_sandbox::wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use codelib_sandbox::wasmtime_wasi_http::{Error as WasiError, WasiBody, WasiHttpView as _};
use codelib_sandbox::{Console, Session};
use http_body::{Frame, SizeHint};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::bindings;
use crate::host::State;
use crate::outcome::{Outcome, RunError};
use crate::script::outcome;

/// Requests waiting for the script, beyond the one it is answering.
const QUEUE: usize = 32;

/// What the guest hands to its response outparam.
type Head = Result<http::Response<HyperOutgoingBody>, ErrorCode>;

type Reply = oneshot::Sender<Result<http::Response<Body>, RunError>>;

/// A warm script instance that answers HTTP requests. Created by
/// [`Script::serve`](crate::Script::serve).
///
/// The instance lives on a task of its own. A `Service` can be shared
/// (`Arc<Service>`) by the connections of an HTTP server, but requests are
/// answered one at a time, in the order they arrive, each under the
/// session's time budget. JavaScript state (`globalThis`) persists from one
/// request to the next.
///
/// - A handler that throws (including one that runs out of memory and
///   throws) produces a 500 response, and the service carries on.
/// - A handler that traps or runs out of time ends the service: an
///   interrupted instance is in an undefined state. Later requests fail with
///   the error that ended it, and [`Service::finish`] reports that error,
///   along with the state and the console.
/// - Reading the request body and writing the response body happen inside
///   the handler, so they count against the per-request time budget: a
///   client that uploads or downloads slowly can run a request out of time
///   and end the service. Put your HTTP server's own timeouts in front.
/// - The service holds one [`max_concurrent`](crate::SandboxBuilder::max_concurrent)
///   slot for as long as it lives.
/// - Requests the host cannot hand to the script (e.g. an unparsable
///   authority) are refused with an error for that request only.
pub struct Service<S: 'static = ()> {
    commands: mpsc::Sender<Command<S>>,
    task: JoinHandle<()>,
    value: Value,
}

enum Command<S: 'static> {
    Handle {
        request: Box<http::Request<WasiBody>>,
        scheme: Scheme,
        reply: Reply,
    },
    WithState(Box<dyn FnOnce(&mut S) + Send>),
    Console(oneshot::Sender<Console>),
    Finish(oneshot::Sender<Outcome<S>>),
}

impl<S: Send + 'static> Service<S> {
    pub(crate) fn new(session: Session<State<S>>, value: Value) -> Self {
        let (commands, inbox) = mpsc::channel(QUEUE);
        let task = tokio::spawn(serve(session, value.clone(), inbox));
        Self {
            commands,
            task,
            value,
        }
    }

    /// What the program returned after registering its handler.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// Dispatch one request to the script's handler.
    ///
    /// Returns as soon as the handler has produced the status and headers.
    /// Both bodies stream: the request body into the script as it reads, the
    /// response body out as the script writes, with backpressure. The script
    /// is only finished with this request once the response body has been
    /// read to the end or dropped, and the next request waits until then; a
    /// reader that stalls uses up the request's time budget. If the handler
    /// fails after the head was sent, the body ends with an error.
    ///
    /// Use [`Service::handle_buffered`] when the whole body is wanted anyway.
    ///
    /// Dropping the returned future (a client that went away) does not
    /// cancel the request inside the script; it is answered and discarded.
    pub fn handle<B>(
        &self,
        request: http::Request<B>,
    ) -> impl Future<Output = Result<http::Response<Body>, RunError>> + Send + '_
    where
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        // Converted before the future is built, so the future does not hold
        // `B` and stays `Send` whatever `B` is.
        let (request, scheme) = prepare(request);
        self.send(request, scheme)
    }

    async fn send(
        &self,
        request: http::Request<WasiBody>,
        scheme: Scheme,
    ) -> Result<http::Response<Body>, RunError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Handle {
                request: Box::new(request),
                scheme,
                reply,
            })
            .await
            .map_err(|_| gone())?;
        response.await.map_err(|_| gone())?
    }

    /// [`Service::handle`], then read the whole response body, refusing a
    /// body longer than `max_bytes`. The script is untrusted and the
    /// collected body is host memory, outside the script's memory limit.
    pub fn handle_buffered<B>(
        &self,
        request: http::Request<B>,
        max_bytes: usize,
    ) -> impl Future<Output = Result<http::Response<Bytes>, RunError>> + Send + '_
    where
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        let (request, scheme) = prepare(request);
        async move {
            let (parts, mut body) = self.send(request, scheme).await?.into_parts();
            let mut collected = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|error| {
                    RunError::Sandbox(format!("the response body failed: {error}"))
                })?;
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                if collected.len() + data.len() > max_bytes {
                    // Dropping the body tells the script to stop writing.
                    return Err(RunError::Sandbox(format!(
                        "the response body is longer than {max_bytes} bytes"
                    )));
                }
                collected.extend_from_slice(&data);
            }
            Ok(http::Response::from_parts(parts, Bytes::from(collected)))
        }
    }

    /// Run `f` on the state host calls work on, between requests.
    pub async fn with_state<R, F>(&self, f: F) -> Result<R, RunError>
    where
        F: FnOnce(&mut S) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (reply, result) = oneshot::channel();
        let command = Command::WithState(Box::new(move |state| {
            let _ = reply.send(f(state));
        }));
        self.commands.send(command).await.map_err(|_| gone())?;
        result.await.map_err(|_| gone())
    }

    /// Everything the script has written to the console so far.
    pub async fn console(&self) -> Result<Console, RunError> {
        let (reply, console) = oneshot::channel();
        self.commands
            .send(Command::Console(reply))
            .await
            .map_err(|_| gone())?;
        console.await.map_err(|_| gone())
    }

    /// Stop serving, once the requests already queued are answered, and get
    /// the state, console and counters back. The result is the value the
    /// program returned when it started serving, or the error that ended the
    /// service.
    ///
    /// Await it on the tokio runtime the service was created on: the
    /// service task lives there, and if that runtime has shut down the state
    /// cannot be recovered (this panics).
    pub async fn finish(self) -> Outcome<S> {
        let (reply, outcome) = oneshot::channel();
        let sent = self.commands.send(Command::Finish(reply)).await.is_ok();
        if sent && let Ok(outcome) = outcome.await {
            return outcome;
        }
        // The task is gone without answering: it panicked (host code panics
        // are caught, so this is a bug), or the tokio runtime it ran on was
        // shut down. Either way the state went with it.
        match self.task.await {
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            _ => panic!("the service task ended without handing back its state"),
        }
    }
}

fn gone() -> RunError {
    RunError::Sandbox("the service task is gone".into())
}

/// Normalise the request and box its body the way wasi-http wants it.
fn prepare<B>(request: http::Request<B>) -> (http::Request<WasiBody>, Scheme)
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    let (request, scheme) = normalise(request);
    (
        request.map(|body| body.map_err(body_error).boxed_unsync()),
        scheme,
    )
}

fn body_error(error: impl Into<BoxError>) -> WasiError {
    WasiError::InternalError(Some(error.into().to_string()))
}

/// Give the request an absolute URI, which wasi-http requires: the authority
/// from the URI, else from a valid `Host` header, else `localhost`; the
/// scheme from the URI, else `http`.
fn normalise<B>(request: http::Request<B>) -> (http::Request<B>, Scheme) {
    let (mut parts, body) = request.into_parts();
    let authority = parts.uri.authority().cloned().or_else(|| {
        parts
            .headers
            .get(http::header::HOST)?
            .to_str()
            .ok()?
            .parse::<http::uri::Authority>()
            .ok()
    });
    let authority = authority.unwrap_or_else(|| http::uri::Authority::from_static("localhost"));
    let scheme = parts
        .uri
        .scheme()
        .cloned()
        .unwrap_or(http::uri::Scheme::HTTP);
    let path = parts
        .uri
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| http::uri::PathAndQuery::from_static("/"));
    if let Ok(uri) = http::Uri::builder()
        .scheme(scheme.clone())
        .authority(authority)
        .path_and_query(path)
        .build()
    {
        parts.uri = uri;
    }
    let scheme = if scheme == http::uri::Scheme::HTTP {
        Scheme::Http
    } else if scheme == http::uri::Scheme::HTTPS {
        Scheme::Https
    } else {
        Scheme::Other(scheme.as_str().to_string())
    };
    (http::Request::from_parts(parts, body), scheme)
}

/// The service task: owns the session and works through commands in order.
async fn serve<S: Send + 'static>(
    mut session: Session<State<S>>,
    value: Value,
    mut inbox: mpsc::Receiver<Command<S>>,
) {
    // The error that ended the service, once there is one.
    let mut ended: Option<RunError> = None;
    while let Some(command) = inbox.recv().await {
        match command {
            Command::Handle {
                request,
                scheme,
                reply,
            } => {
                if let Some(error) = &ended {
                    let _ = reply.send(Err(error.clone()));
                } else if !reply.is_closed() {
                    ended = dispatch(&mut session, *request, scheme, reply).await.err();
                }
            }
            Command::WithState(f) => f(&mut session.state_mut().user),
            Command::Console(reply) => {
                let _ = reply.send(session.console());
            }
            Command::Finish(reply) => {
                let result = match ended {
                    Some(error) => Err(error),
                    None => Ok(value),
                };
                let _ = reply.send(outcome(session, result));
                return;
            }
        }
    }
    // Every `Service` handle is gone: dropping the session ends the instance.
}

/// Run the handler for one request. The response head goes back to the
/// caller the moment the guest sets it; the guest keeps being driven here
/// until it has written the whole body. Returns the error that ends the
/// service, if the call failed.
async fn dispatch<S: Send + 'static>(
    session: &mut Session<State<S>>,
    request: http::Request<WasiBody>,
    scheme: Scheme,
    reply: Reply,
) -> Result<(), RunError> {
    let (outparam, mut head) = oneshot::channel::<Head>();

    // Set up outside the guest call: a request the host refuses is an error
    // for this request, not a failure of the instance.
    let resources = {
        let mut http = session.store_mut().data_mut().http();
        http.new_incoming_request(scheme, request)
            .and_then(|incoming| Ok((incoming, http.new_response_outparam(outparam)?)))
    };
    let (incoming, outparam) = match resources {
        Ok(resources) => resources,
        Err(error) => {
            let _ = reply.send(Err(RunError::Sandbox(format!(
                "the request was refused: {error:#}"
            ))));
            return Ok(());
        }
    };

    // Ends the forwarded body with an error if the call fails after the
    // head went out; dropped unused when the call succeeds.
    let (failure, failed) = oneshot::channel::<String>();
    let mut reply = Some(reply);
    let mut head_seen = false;

    let called = {
        let call = session.call(async |store, instance| {
            let script = bindings::Script::new(&mut *store, instance)?;
            script
                .wasi_http_incoming_handler()
                .call_handle(&mut *store, incoming, outparam)
                .await
        });
        let mut call = std::pin::pin!(call);
        let mut failed = Some(failed);
        std::future::poll_fn(|cx| {
            if !head_seen && let Poll::Ready(received) = Pin::new(&mut head).poll(cx) {
                head_seen = true;
                if let Ok(received) = received
                    && let Some(reply) = reply.take()
                {
                    let _ = reply.send(response(received, failed.take()));
                }
            }
            call.as_mut().poll(cx)
        })
        .await
    };

    let called = called.map_err(RunError::from);
    match reply {
        // The head was forwarded; a failure now reaches the body reader.
        None => {
            if let Err(error) = &called {
                let _ = failure.send(format!("the handler failed: {error}"));
            }
        }
        // The guest finished (or was stopped) without the head having been
        // forwarded above.
        Some(reply) => {
            let result = match &called {
                Err(error) => Err(error.clone()),
                Ok(()) => match (!head_seen).then(|| head.try_recv()) {
                    Some(Ok(received)) => response(received, None),
                    _ => Err(RunError::Sandbox(
                        "the handler returned without sending a response".into(),
                    )),
                },
            };
            let _ = reply.send(result);
        }
    }
    called
}

fn response(
    head: Head,
    failed: Option<oneshot::Receiver<String>>,
) -> Result<http::Response<Body>, RunError> {
    match head {
        Ok(response) => Ok(response.map(|body| {
            Streamed {
                body,
                failed,
                done: false,
            }
            .boxed_unsync()
        })),
        Err(code) => Err(RunError::Sandbox(format!("the handler failed: {code:?}"))),
    }
}

/// The response body as the guest writes it, ending with an error if the
/// guest's call fails before the body is complete.
struct Streamed {
    body: HyperOutgoingBody,
    failed: Option<oneshot::Receiver<String>>,
    done: bool,
}

impl http_body::Body for Streamed {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if self.done {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.body).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => return Poll::Ready(Some(Ok(frame))),
            Poll::Ready(Some(Err(error))) => {
                self.done = true;
                return Poll::Ready(Some(Err(
                    format!("the response body failed: {error}").into()
                )));
            }
            Poll::Ready(None) => {
                self.done = true;
                return Poll::Ready(None);
            }
            Poll::Pending => {}
        }
        if let Some(failed) = self.failed.as_mut() {
            match Pin::new(failed).poll(cx) {
                Poll::Ready(Ok(message)) => {
                    self.done = true;
                    return Poll::Ready(Some(Err(message.into())));
                }
                // The call succeeded: the body ends by itself.
                Poll::Ready(Err(_)) => self.failed = None,
                Poll::Pending => {}
            }
        }
        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        self.done || self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
