use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use codelib_sandbox::wasmtime::component::HasSelf;
use codelib_sandbox::{Ctx, Error, Limits, Runtime, WasiConfig};

use crate::bindings::{host, types};
use crate::host::{HostCalls, State};
use crate::outcome::Outcome;
use crate::script::Script;

/// A JavaScript sandbox: the compiled engine plus the capabilities every
/// script run on it gets.
///
/// Build one at startup and share it (it is cheap to clone). Every run gets
/// its own engine instance; nothing a script does is visible to the next.
///
/// `S` is the state host calls work on. Each run is handed a value of it and
/// gives it back in the [`Outcome`].
///
/// ```no_run
/// # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// let sandbox = codelib::Sandbox::new()?;
/// let outcome = sandbox.run("return [1, 2, 3].map((n) => n * 2)").await;
/// assert_eq!(outcome.result?, serde_json::json!([2, 4, 6]));
/// # Ok(()) }
/// ```
pub struct Sandbox<S: 'static = ()> {
    pub(crate) inner: Arc<Inner<S>>,
}

impl<S: 'static> Clone for Sandbox<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

pub(crate) struct Inner<S: 'static> {
    pub(crate) runtime: Runtime<State<S>>,
    pub(crate) calls: Option<Arc<dyn HostCalls<S>>>,
    pub(crate) preludes: Vec<types::Source>,
}

#[cfg(feature = "embedded-guest")]
impl Sandbox<()> {
    /// A sandbox with the embedded engine, default limits, no host calls and
    /// no network access.
    ///
    /// The first call on a machine compiles the engine, which takes a while
    /// (tens of seconds); the result is cached on disk and later starts take
    /// milliseconds.
    pub fn new() -> Result<Self, Error> {
        Self::builder().build()
    }
}

impl<S: Send + 'static> Sandbox<S> {
    /// Start configuring a sandbox.
    pub fn builder() -> SandboxBuilder<S> {
        SandboxBuilder {
            guest: None,
            cache: Cache::Default,
            limits: Limits::default(),
            wasi: WasiConfig::default(),
            max_concurrent: None,
            fuel_metering: false,
            calls: None,
            preludes: Vec::new(),
        }
    }

    /// Prepare a program to run: add input, preludes, network access and
    /// limits on the returned [`Script`], then `run()` it.
    ///
    /// The program is the body of an async function: top-level `await` and
    /// `return` work, `import` does not.
    pub fn script(&self, code: impl Into<String>) -> Script<S> {
        Script::new(self.clone(), code.into())
    }

    /// Run a program with default settings and a default state.
    pub async fn run(&self, code: impl Into<String>) -> Outcome<S>
    where
        S: Default,
    {
        self.script(code).run().await
    }

    /// The limits runs get unless they override them.
    pub fn limits(&self) -> &Limits {
        self.inner.runtime.limits()
    }
}

enum Guest {
    Bytes(Cow<'static, [u8]>),
    File(PathBuf),
}

enum Cache {
    Default,
    Dir(PathBuf),
    Disabled,
}

/// Configures and builds a [`Sandbox`].
pub struct SandboxBuilder<S: 'static> {
    guest: Option<Guest>,
    cache: Cache,
    limits: Limits,
    wasi: WasiConfig,
    max_concurrent: Option<usize>,
    fuel_metering: bool,
    calls: Option<Arc<dyn HostCalls<S>>>,
    preludes: Vec<types::Source>,
}

impl<S: Send + 'static> SandboxBuilder<S> {
    /// Answer the scripts' `host.call(method, args)` with `calls`, usually a
    /// [`codelib_rpc::Router`]. Without this every call fails with
    /// `unknown-method`.
    pub fn host_calls(mut self, calls: impl HostCalls<S>) -> Self {
        self.calls = Some(Arc::new(calls));
        self
    }

    /// Evaluate `code` before every program, in its own scope. Preludes
    /// publish what they define on `globalThis`; this is where a client
    /// library for your host calls goes.
    pub fn prelude(mut self, name: impl Into<String>, code: impl Into<String>) -> Self {
        self.preludes.push(types::Source {
            name: name.into(),
            code: code.into(),
        });
        self
    }

    /// Default limits for runs: timeout, memory, fuel, console size. A fuel
    /// budget here turns on fuel metering.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Meter fuel, so that runs can set a fuel budget with
    /// [`Script::limits`](crate::Script::limits) even though the default
    /// limits have none. Without metering, a run that sets a fuel budget
    /// fails with [`RunError::Sandbox`](crate::RunError::Sandbox) instead of
    /// running unmetered. Metering makes guest code somewhat slower.
    pub fn fuel_metering(mut self, yes: bool) -> Self {
        self.fuel_metering = yes;
        self
    }

    /// What scripts see of clocks and randomness, e.g.
    /// [`WasiConfig::deterministic`].
    pub fn wasi(mut self, wasi: WasiConfig) -> Self {
        self.wasi = wasi;
        self
    }

    /// Allow at most `n` runs at once; further runs wait their turn. A
    /// [`Service`](crate::Service) counts as one run for as long as it lives.
    pub fn max_concurrent(mut self, n: usize) -> Self {
        self.max_concurrent = Some(n);
        self
    }

    /// Keep the compiled engine in `dir` instead of the default directory:
    /// `$CODELIB_CACHE_DIR` if that is set and not empty, else
    /// `<OS cache dir>/codelib/compiled`. The directory must be private to
    /// the user running the process. Entries are about 40 MB each; loading
    /// one refreshes its modified time, and writing a new one removes entries
    /// in the directory that have not been loaded for 30 days.
    pub fn cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache = Cache::Dir(dir.into());
        self
    }

    /// Compile the engine in memory on every start (slow) instead of caching
    /// it on disk.
    pub fn no_cache(mut self) -> Self {
        self.cache = Cache::Disabled;
        self
    }

    /// Use this guest component instead of the embedded one. It must
    /// implement the `codelib:runtime/script` world; build one from the
    /// repository's `guest/` directory.
    pub fn guest_bytes(mut self, bytes: impl Into<Cow<'static, [u8]>>) -> Self {
        self.guest = Some(Guest::Bytes(bytes.into()));
        self
    }

    /// Like [`Self::guest_bytes`], read from a file when the sandbox is built.
    pub fn guest_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.guest = Some(Guest::File(path.into()));
        self
    }

    /// Compile (or load from cache) the engine and link the host.
    ///
    /// CPU-bound, and slow on a cold cache: call it once at startup, off the
    /// async executor.
    pub fn build(self) -> Result<Sandbox<S>, Error> {
        let mut runtime = Runtime::<State<S>>::builder()
            .limits(self.limits)
            .wasi(self.wasi)
            .link(|linker| {
                host::add_to_linker::<Ctx<State<S>>, HasSelf<Ctx<State<S>>>>(linker, |ctx| ctx)?;
                types::add_to_linker::<Ctx<State<S>>, HasSelf<Ctx<State<S>>>>(linker, |ctx| ctx)?;
                Ok(())
            });

        runtime = match self.guest {
            Some(Guest::Bytes(bytes)) => runtime.component_bytes(bytes),
            Some(Guest::File(path)) => runtime.component_file(path),
            #[cfg(feature = "embedded-guest")]
            None => runtime.component_bytes(codelib_guest::COMPONENT),
            #[cfg(not(feature = "embedded-guest"))]
            None => runtime, // `build` below reports the missing component.
        };
        runtime = match self.cache {
            Cache::Default => runtime,
            Cache::Dir(dir) => runtime.cache_dir(dir),
            Cache::Disabled => runtime.no_cache(),
        };
        if let Some(n) = self.max_concurrent {
            runtime = runtime.max_concurrent(n);
        }
        runtime = runtime.fuel_metering(self.fuel_metering);

        Ok(Sandbox {
            inner: Arc::new(Inner {
                runtime: runtime.build()?,
                calls: self.calls,
                preludes: self.preludes,
            }),
        })
    }
}
