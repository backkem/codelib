use std::time::Duration;

use codelib_http::Egress;
use codelib_sandbox::{Limits, Session, SessionOptions, Stats};
use serde::Serialize;
use serde_json::Value;

use crate::bindings::{self, types};
use crate::builder::Sandbox;
use crate::host::{EmitSink, State};
use crate::outcome::{Outcome, RunError};
use crate::service::Service;

/// One program, ready to run. Created by [`Sandbox::script`].
///
/// ```no_run
/// # use codelib::{Egress, EnvBearer, Rule, Sandbox};
/// # async fn demo(sandbox: Sandbox) {
/// let egress = Egress::builder()
///     .allow(Rule::host("api.github.com").read_only().credential(EnvBearer::new("GITHUB_TOKEN")))
///     .build();
///
/// let outcome = sandbox
///     .script("const r = await fetch(`https://api.github.com/users/${host.input.user}`); return (await r.json()).name")
///     .input(serde_json::json!({ "user": "octocat" }))
///     .egress(egress)
///     .run()
///     .await;
/// # }
/// ```
pub struct Script<S: 'static = ()> {
    sandbox: Sandbox<S>,
    program: types::Source,
    preludes: Vec<types::Source>,
    input: Result<String, String>,
    egress: Option<Egress>,
    limits: Option<Limits>,
    timeout: Option<Duration>,
    emit: Option<EmitSink>,
}

/// What the guest's `run`/`serve` exports are called with.
struct Request {
    preludes: Vec<types::Source>,
    program: types::Source,
    input: String,
}

type Called = Result<Result<String, types::ScriptError>, codelib_sandbox::Error>;

impl<S: Send + 'static> Script<S> {
    pub(crate) fn new(sandbox: Sandbox<S>, code: String) -> Self {
        Self {
            sandbox,
            program: types::Source {
                name: "script.js".into(),
                code,
            },
            preludes: Vec::new(),
            input: Ok("null".into()),
            egress: None,
            limits: None,
            timeout: None,
            emit: None,
        }
    }

    /// Name the program; it shows up in stack traces. Default `script.js`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.program.name = name.into();
        self
    }

    /// Evaluate `code` before the program (after the sandbox's own
    /// preludes), in its own scope.
    pub fn prelude(mut self, name: impl Into<String>, code: impl Into<String>) -> Self {
        self.preludes.push(types::Source {
            name: name.into(),
            code: code.into(),
        });
        self
    }

    /// Data for the program, visible to it as `host.input`.
    pub fn input(mut self, input: impl Serialize) -> Self {
        self.input = serde_json::to_string(&input).map_err(|e| e.to_string());
        self
    }

    /// Let the program's `fetch` reach what `egress` allows. Without this
    /// every request is denied.
    pub fn egress(mut self, egress: Egress) -> Self {
        self.egress = Some(egress);
        self
    }

    /// Override the sandbox's default limits for this run. A
    /// [`Self::timeout`] still applies on top, whichever is called first.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Override just the time budget for this run, on top of the sandbox's
    /// limits or those given to [`Self::limits`].
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Receive the program's `host.emit(kind, item)` items as they happen.
    ///
    /// Items arrive as parsed JSON. An item the host cannot parse (in
    /// practice: nested more deeply than `serde_json` accepts, 128 levels) is
    /// delivered as a [`Value::String`] holding its raw JSON text.
    pub fn on_emit(mut self, sink: impl FnMut(&str, Value) + Send + 'static) -> Self {
        self.emit = Some(Box::new(sink));
        self
    }

    /// Run the program with a default state.
    pub async fn run(self) -> Outcome<S>
    where
        S: Default,
    {
        self.run_with(S::default()).await
    }

    /// Run the program, giving its host calls `state` to work on. The state
    /// comes back in the [`Outcome`], whatever the program does.
    ///
    /// Dropping the returned future before it completes drops the state with
    /// it.
    pub async fn run_with(self, state: S) -> Outcome<S> {
        let (mut session, request) = match self.start(state).await {
            Ok(started) => started,
            Err((error, state)) => return failed(error, state),
        };

        let called: Called = session
            .call(async |store, instance| {
                let script = bindings::Script::new(&mut *store, instance)?;
                script
                    .call_run(
                        &mut *store,
                        &request.preludes,
                        &request.program,
                        &request.input,
                    )
                    .await
            })
            .await;
        finish(session, called)
    }

    /// Run the program to completion on a small runtime of its own, for
    /// callers that are not async. Inside a tokio runtime use
    /// [`Self::run`] instead: called from there, this returns a
    /// [`RunError::Sandbox`] without running the program.
    pub fn run_blocking(self) -> Outcome<S>
    where
        S: Default,
    {
        self.run_blocking_with(S::default())
    }

    /// [`Self::run_blocking`] with an explicit state.
    pub fn run_blocking_with(self, state: S) -> Outcome<S> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return failed(
                RunError::Sandbox(
                    "run_blocking was called from inside a tokio runtime; use run/run_with there"
                        .into(),
                ),
                state,
            );
        }
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(self.run_with(state)),
            Err(error) => failed(
                RunError::Sandbox(format!("could not start a runtime: {error}")),
                state,
            ),
        }
    }

    /// Evaluate a program that registers a request handler
    /// (`host.serve(fn)` or `addEventListener("fetch", ...)`) and keep its
    /// instance warm. The returned [`Service`] answers requests one at a
    /// time; JavaScript state persists between them.
    ///
    /// If the program fails before it is serving, the error arm carries the
    /// usual [`Outcome`].
    #[allow(clippy::result_large_err)] // the failure is the full Outcome, once per run
    pub async fn serve(self) -> Result<Service<S>, Outcome<S>>
    where
        S: Default,
    {
        self.serve_with(S::default()).await
    }

    /// [`Self::serve`] with an explicit state.
    #[allow(clippy::result_large_err)]
    pub async fn serve_with(self, state: S) -> Result<Service<S>, Outcome<S>> {
        let (mut session, request) = match self.start(state).await {
            Ok(started) => started,
            Err((error, state)) => return Err(failed(error, state)),
        };

        let called: Called = session
            .call(async |store, instance| {
                let script = bindings::Script::new(&mut *store, instance)?;
                script
                    .call_serve(
                        &mut *store,
                        &request.preludes,
                        &request.program,
                        &request.input,
                    )
                    .await
            })
            .await;

        match called {
            Ok(Ok(json)) => match parse_value(&json) {
                Ok(value) => Ok(Service::new(session, value)),
                Err(error) => Err(outcome(session, Err(error))),
            },
            other => Err(finish(session, other)),
        }
    }

    /// Start a session for this script. On failure the state comes back.
    #[allow(clippy::type_complexity)]
    async fn start(self, state: S) -> Result<(Session<State<S>>, Request), (RunError, S)> {
        let input = match self.input {
            Ok(input) => input,
            Err(e) => {
                let error = RunError::Sandbox(format!("the input is not serializable: {e}"));
                return Err((error, state));
            }
        };

        let inner = &self.sandbox.inner;
        let mut preludes = inner.preludes.clone();
        preludes.extend(self.preludes);

        let mut options = SessionOptions::new(State {
            user: state,
            calls: inner.calls.clone(),
            emit: self.emit,
        });
        if self.limits.is_some() || self.timeout.is_some() {
            let mut limits = self
                .limits
                .unwrap_or_else(|| inner.runtime.limits().clone());
            if let Some(timeout) = self.timeout {
                limits.timeout = timeout;
            }
            options = options.limits(limits);
        }
        if let Some(egress) = self.egress {
            options = options.egress(egress);
        }

        match inner.runtime.session(options).await {
            Ok(session) => Ok((
                session,
                Request {
                    preludes,
                    program: self.program,
                    input,
                },
            )),
            Err(start) => Err((start.error.into(), start.state.user)),
        }
    }
}

/// The outcome of a run that never started.
fn failed<S>(error: RunError, state: S) -> Outcome<S> {
    Outcome {
        result: Err(error),
        console: Default::default(),
        state,
        stats: Stats::default(),
    }
}

/// Parse the JSON text of a value the guest returned.
fn parse_value(json: &str) -> Result<Value, RunError> {
    serde_json::from_str(json).map_err(|error| {
        if error.to_string().starts_with("recursion limit exceeded") {
            RunError::Sandbox(
                "the program's value is nested more than 128 levels deep, more than the host \
                 accepts"
                    .into(),
            )
        } else {
            RunError::Sandbox(format!("the guest returned invalid JSON: {error}"))
        }
    })
}

/// Close the session and turn what the guest returned into an [`Outcome`].
pub(crate) fn finish<S: Send + 'static>(session: Session<State<S>>, called: Called) -> Outcome<S> {
    let result = match called {
        Ok(Ok(json)) => parse_value(&json),
        // A refused allocation surfaces in the guest as an ordinary
        // exception; the limiter's record for this call is what tells it
        // apart from a program's own error.
        Ok(Err(_)) if session.memory_limit_hit() => Err(session.out_of_memory().into()),
        Ok(Err(error)) => Err(RunError::Script(error.into())),
        Err(error) => Err(error.into()),
    };
    outcome(session, result)
}

/// Close the session with a result already decided.
pub(crate) fn outcome<S: Send + 'static>(
    session: Session<State<S>>,
    result: Result<Value, RunError>,
) -> Outcome<S> {
    let finished = session.finish();
    Outcome {
        result,
        console: finished.console,
        state: finished.state.user,
        stats: finished.stats,
    }
}
