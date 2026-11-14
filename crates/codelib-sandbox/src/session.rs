use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::OwnedSemaphorePermit;
#[cfg(feature = "p3")]
use wasmtime::component::Accessor;
use wasmtime::component::{Instance, ResourceTable};
use wasmtime::{Store, Trap, UpdateDeadline};

use crate::capture::{Capture, Console};
use crate::ctx::{Ctx, build_wasi};
use crate::error::StartError;
use crate::limits::MemoryLimiter;
use crate::runtime::Inner;
use crate::{Error, Limits};

/// Per-session settings: the embedder's state, and optional overrides.
pub struct SessionOptions<S> {
    state: S,
    limits: Option<Limits>,
    #[cfg(feature = "http")]
    egress: Option<codelib_http::Egress>,
}

impl<S> SessionOptions<S> {
    /// Options with the given state, the runtime's default limits, and no
    /// network access.
    pub fn new(state: S) -> Self {
        Self {
            state,
            limits: None,
            #[cfg(feature = "http")]
            egress: None,
        }
    }

    /// Override the runtime's default limits for this session.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Route the guest's outgoing HTTP requests through `egress`. Without
    /// this every request is denied.
    #[cfg(feature = "http")]
    pub fn egress(mut self, egress: codelib_http::Egress) -> Self {
        self.egress = Some(egress);
        self
    }
}

impl<S: Default> Default for SessionOptions<S> {
    fn default() -> Self {
        Self::new(S::default())
    }
}

/// Counters for one session.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Time since the session was created.
    pub duration: Duration,
    /// Fuel consumed over the whole session (instantiation and every call),
    /// when fuel metering is on.
    pub fuel_used: Option<u64>,
    /// Outgoing HTTP requests the guest attempted (allowed or not).
    pub http_requests: u64,
    /// Whether the guest was ever refused memory in this session, even if
    /// it recovered and carried on. A guest that returns normally after a
    /// refused allocation is not an error; check this to treat it as one.
    pub memory_limit_hit: bool,
}

/// What is left of a session once it is finished.
pub struct Finished<S> {
    /// The embedder's state, handed back.
    pub state: S,
    pub console: Console,
    pub stats: Stats,
}

/// One instance of the component in its own store.
///
/// Call into the guest with [`Session::call`] (or, to run several async
/// exports of a WASI 0.3 component at once, `Session::call_concurrent` with
/// feature `p3`); every call gets a fresh time (and fuel) budget. A session
/// whose call trapped, timed out or was cancelled is poisoned and refuses
/// further calls. [`Session::finish`] hands the state and the captured
/// console back.
pub struct Session<S: 'static> {
    store: Store<Ctx<S>>,
    instance: Instance,
    limits: Limits,
    tick: Duration,
    stdout: Capture,
    stderr: Capture,
    started: Instant,
    fuel: Option<Fuel>,
    poisoned: bool,
    #[cfg(feature = "http")]
    http_requests: Arc<std::sync::atomic::AtomicU64>,
    _slot: Option<OwnedSemaphorePermit>,
}

/// Fuel bookkeeping: the store's tank is refilled for every call, and what
/// earlier tanks used up is added to `used`.
#[derive(Clone, Copy)]
struct Fuel {
    /// What each call starts with.
    budget: u64,
    /// Used by earlier tanks.
    used: u64,
    /// What the current tank started with.
    tank: u64,
}

impl<S: Send + 'static> Session<S> {
    pub(crate) async fn start(
        inner: Arc<Inner<S>>,
        options: SessionOptions<S>,
    ) -> Result<Self, StartError<S>> {
        let limits = options.limits.unwrap_or_else(|| inner.limits.clone());
        if limits.fuel.is_some() && !inner.fuel_metering {
            return Err(StartError {
                error: Error::Config(
                    "a fuel budget was set, but the runtime was built without fuel metering \
                     (enable it with `fuel_metering(true)` when building the runtime)"
                        .into(),
                ),
                state: options.state,
            });
        }

        let slot = match &inner.slots {
            Some(slots) => Some(
                slots
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("the session semaphore is never closed"),
            ),
            None => None,
        };

        let stdout = Capture::new(limits.console_bytes);
        let stderr = Capture::new(limits.console_bytes);

        #[cfg(feature = "http")]
        let hooks = crate::http::EgressHooks::new(
            options
                .egress
                .unwrap_or_else(codelib_http::Egress::deny_all),
            stderr.clone(),
        );
        #[cfg(feature = "http")]
        let http_requests = hooks.requests();

        let ctx = Ctx {
            state: options.state,
            wasi: build_wasi(&inner.wasi, &stdout, &stderr),
            table: ResourceTable::new(),
            limiter: MemoryLimiter::new(limits.memory_bytes),
            #[cfg(feature = "http")]
            http: wasmtime_wasi_http::WasiHttpCtx::new(),
            #[cfg(feature = "http")]
            hooks,
        };

        let mut store = Store::new(&inner.engine, ctx);
        store.limiter(|ctx| &mut ctx.limiter);

        let mut fuel = inner.fuel_metering.then(|| {
            let budget = limits.fuel.unwrap_or(u64::MAX);
            Fuel {
                budget,
                used: 0,
                tank: 0,
            }
        });
        if let Err(error) = refuel(&mut store, &mut fuel) {
            return Err(StartError {
                error: Error::Instantiate(error),
                state: store.into_data().state,
            });
        }

        let started = Instant::now();
        arm_deadline(&mut store, limits.timeout, inner.tick);
        let instantiated = tokio::time::timeout(
            wall_budget(limits.timeout, inner.tick),
            inner.pre.instantiate_async(&mut store),
        )
        .await;
        let instance = match instantiated {
            Ok(Ok(instance)) => instance,
            Ok(Err(error)) => {
                let error = match classify(error, store.data().limiter.hit(), &limits) {
                    Error::Trap(error) => Error::Instantiate(error),
                    other => other,
                };
                return Err(StartError {
                    error,
                    state: store.into_data().state,
                });
            }
            Err(_) => {
                return Err(StartError {
                    error: Error::Timeout(limits.timeout),
                    state: store.into_data().state,
                });
            }
        };

        Ok(Self {
            store,
            instance,
            limits,
            tick: inner.tick,
            stdout,
            stderr,
            started,
            fuel,
            poisoned: false,
            #[cfg(feature = "http")]
            http_requests,
            _slot: slot,
        })
    }

    /// Run `f` against the store and instance under a fresh time budget.
    ///
    /// `f` is where the world's exports are called, typically through
    /// `bindgen!`-generated bindings:
    ///
    /// ```ignore
    /// let sum = session
    ///     .call(async |store, instance| {
    ///         let world = MyWorld::new(&mut *store, instance)?;
    ///         world.call_add(store, 1, 2).await
    ///     })
    ///     .await?;
    /// ```
    ///
    /// The guest is interrupted when the budget runs out, whether it is busy
    /// computing or waiting on I/O. Traps come back as [`Error`] variants
    /// (`Timeout`, `OutOfMemory`, `OutOfFuel`, `Trap`) and poison the
    /// session. So does dropping the returned future before it completes:
    /// the guest is unwound mid-call and its instance cannot be trusted
    /// afterwards.
    ///
    /// A guest that returns normally after being refused memory is a
    /// success; [`Session::memory_limit_hit`] tells whether that happened.
    ///
    /// This also calls the exports of a WASI 0.3 component, async ones
    /// included: wasmtime runs the component's event loop until the export
    /// returns. Calls go one at a time, though; to run several exports at
    /// once, use `Session::call_concurrent` (feature `p3`).
    pub async fn call<R, F>(&mut self, f: F) -> Result<R, Error>
    where
        F: AsyncFnOnce(&mut Store<Ctx<S>>, &Instance) -> wasmtime::Result<R>,
    {
        let budget = self.begin_call()?;
        let outcome = tokio::time::timeout(budget, f(&mut self.store, &self.instance)).await;
        self.end_call(outcome)
    }

    /// Run `f` inside the store's concurrent event loop under a fresh time
    /// budget, for the async exports of WASI 0.3 components.
    ///
    /// `f` gets an [`Accessor`] instead of the store, which is what
    /// `call_concurrent` on a typed function (or on `bindgen!` exports
    /// generated with `store`) takes. Several export calls can be in flight
    /// at once inside `f`; the guest interleaves them as its tasks suspend:
    ///
    /// ```ignore
    /// let (a, b) = session
    ///     .call_concurrent(async |accessor, instance| {
    ///         let echo = accessor.with(|mut store| {
    ///             instance.get_typed_func::<(String,), (String,)>(&mut store, "echo")
    ///         })?;
    ///         let (a, b) = tokio::join!(
    ///             echo.call_concurrent(accessor, ("a".into(),)),
    ///             echo.call_concurrent(accessor, ("b".into(),)),
    ///         );
    ///         Ok((a?.0, b?.0))
    ///     })
    ///     .await?;
    /// ```
    ///
    /// The guarantees are those of [`Session::call`], for the call as a
    /// whole rather than per export: one time budget (epoch deadline plus
    /// wall-clock backstop) and one fuel tank shared by everything `f`
    /// starts, the memory flag cleared when it begins, and a trap, timeout,
    /// error returned by `f` or dropped future poisons the session.
    ///
    /// The call ends when `f` returns. Guest tasks still running then (an
    /// export that returned its value but keeps working) are not waited for;
    /// they only make progress during later calls.
    #[cfg(feature = "p3")]
    pub async fn call_concurrent<R, F>(&mut self, f: F) -> Result<R, Error>
    where
        F: AsyncFnOnce(&Accessor<Ctx<S>>, &Instance) -> wasmtime::Result<R>,
    {
        let budget = self.begin_call()?;
        let instance = self.instance;
        // The deadline wraps the whole event loop rather than racing inside
        // it: a select inside `run_concurrent` is not reliably polled while
        // a guest task holds the store.
        let outcome = tokio::time::timeout(
            budget,
            self.store
                .run_concurrent(async move |accessor| f(accessor, &instance).await),
        )
        .await;
        self.end_call(outcome.map(|ran| ran.and_then(|result| result)))
    }

    /// Arm a fresh time budget and fuel tank for a call, and return the
    /// wall-clock backstop for it.
    fn begin_call(&mut self) -> Result<Duration, Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        let timeout = self.limits.timeout;
        arm_deadline(&mut self.store, timeout, self.tick);
        self.store.data_mut().limiter.begin_call();
        if let Err(error) = refuel(&mut self.store, &mut self.fuel) {
            self.poisoned = true;
            return Err(Error::Trap(error));
        }

        // Poisoned until the call completes: if the call's future is dropped
        // mid-call, the session stays poisoned.
        self.poisoned = true;
        Ok(wall_budget(timeout, self.tick))
    }

    /// Turn what a call produced (or the backstop firing) into its result.
    fn end_call<R>(
        &mut self,
        outcome: Result<wasmtime::Result<R>, tokio::time::error::Elapsed>,
    ) -> Result<R, Error> {
        match outcome {
            Ok(Ok(value)) => {
                self.poisoned = false;
                Ok(value)
            }
            Ok(Err(error)) => Err(classify(
                error,
                self.store.data().limiter.hit(),
                &self.limits,
            )),
            // The guest was waiting on the host (I/O), where the epoch
            // deadline cannot reach it. Dropping the call unwinds it.
            Err(_) => Err(Error::Timeout(self.limits.timeout)),
        }
    }

    /// Change the time budget for subsequent calls.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.limits.timeout = timeout;
    }

    /// The limits this session runs under.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The embedder's state.
    pub fn state(&self) -> &S {
        &self.store.data().state
    }

    /// The embedder's state, mutably.
    pub fn state_mut(&mut self) -> &mut S {
        &mut self.store.data_mut().state
    }

    /// Direct access to the store, for setup that has to happen outside a
    /// guest call (e.g. pushing resources into the table).
    ///
    /// Do not call into the guest through it: such a call runs without a
    /// fresh deadline or fuel, and a failure does not poison the session.
    /// Use [`Session::call`] for that.
    pub fn store_mut(&mut self) -> &mut Store<Ctx<S>> {
        &mut self.store
    }

    /// The component instance.
    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    /// Everything the guest has written to stdout/stderr so far.
    pub fn console(&self) -> Console {
        Console {
            stdout: self.stdout.text(),
            stderr: self.stderr.text(),
            truncated: self.stdout.truncated() || self.stderr.truncated(),
        }
    }

    /// Whether the guest was refused memory during the most recent call (or
    /// during instantiation, before the first call). A guest usually turns
    /// that refusal into an error of its own; this is how to recognise it.
    /// [`Stats::memory_limit_hit`] covers the whole session.
    pub fn memory_limit_hit(&self) -> bool {
        self.store.data().limiter.hit()
    }

    /// The [`Error::OutOfMemory`] to report when [`Self::memory_limit_hit`].
    pub fn out_of_memory(&self) -> Error {
        Error::OutOfMemory(self.store.data().limiter.max_bytes())
    }

    /// Whether an earlier failure made this session unusable.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Counters so far.
    pub fn stats(&self) -> Stats {
        Stats {
            duration: self.started.elapsed(),
            fuel_used: self.fuel.map(|fuel| {
                let left = self.store.get_fuel().unwrap_or(0);
                fuel.used.saturating_add(fuel.tank.saturating_sub(left))
            }),
            #[cfg(feature = "http")]
            http_requests: self
                .http_requests
                .load(std::sync::atomic::Ordering::Relaxed),
            #[cfg(not(feature = "http"))]
            http_requests: 0,
            memory_limit_hit: self.store.data().limiter.ever_hit(),
        }
    }

    /// End the session: drop the instance and hand back the state, the
    /// captured console and the counters.
    pub fn finish(self) -> Finished<S> {
        let console = self.console();
        let stats = self.stats();
        Finished {
            state: self.store.into_data().state,
            console,
            stats,
        }
    }
}

/// Fill the tank for the next instantiation or call, booking what the last
/// one used.
fn refuel<S>(store: &mut Store<Ctx<S>>, fuel: &mut Option<Fuel>) -> wasmtime::Result<()> {
    let Some(fuel) = fuel else { return Ok(()) };
    let left = store.get_fuel().unwrap_or(0);
    fuel.used = fuel.used.saturating_add(fuel.tank.saturating_sub(left));
    store.set_fuel(fuel.budget)?;
    fuel.tank = fuel.budget;
    Ok(())
}

/// Budget `timeout` in epoch ticks. The guest yields to the executor on
/// every tick, so a long computation cannot monopolise a worker thread, and
/// is interrupted once the budget is spent.
fn arm_deadline<S>(store: &mut Store<Ctx<S>>, timeout: Duration, tick: Duration) {
    let ticks = timeout.as_nanos() / tick.as_nanos().max(1);
    let mut ticks_left = u64::try_from(ticks).unwrap_or(u64::MAX).max(1);
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(move |_| {
        ticks_left = ticks_left.saturating_sub(1);
        if ticks_left == 0 {
            Ok(UpdateDeadline::Interrupt)
        } else {
            Ok(UpdateDeadline::Yield(1))
        }
    });
}

/// The wall-clock backstop sits slightly past the epoch budget, so a
/// computing guest gets the cleaner epoch interrupt first.
fn wall_budget(timeout: Duration, tick: Duration) -> Duration {
    timeout.saturating_add(tick.saturating_mul(4))
}

/// `hit_memory_limit` is whether memory was refused during the failed call.
fn classify(error: wasmtime::Error, hit_memory_limit: bool, limits: &Limits) -> Error {
    match error.downcast_ref::<Trap>() {
        Some(Trap::Interrupt) => Error::Timeout(limits.timeout),
        Some(Trap::OutOfFuel) => Error::OutOfFuel,
        _ if hit_memory_limit => Error::OutOfMemory(limits.memory_bytes),
        _ => Error::Trap(error),
    }
}
