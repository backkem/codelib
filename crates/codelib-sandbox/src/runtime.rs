use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use wasmtime::component::{InstancePre, Linker};
use wasmtime::{Config, Engine, EngineWeak};

use crate::cache::{self, CachePolicy};
use crate::ctx::{Ctx, WasiConfig};
use crate::error::StartError;
use crate::session::{Session, SessionOptions};
use crate::{Error, Limits};

/// How often the engine epoch advances by default. A running guest yields to
/// the async executor on every tick, and timeouts are counted in ticks.
pub const DEFAULT_EPOCH_TICK: Duration = Duration::from_millis(50);

type LinkFn<S> = Box<dyn FnOnce(&mut Linker<Ctx<S>>) -> wasmtime::Result<()>>;
type ConfigFn = Box<dyn FnOnce(&mut Config)>;

/// A compiled component plus everything needed to start sessions on it.
///
/// Build one at startup and share it: it is cheap to clone and safe to use
/// from many tasks at once. Each [`Runtime::session`] gets a fresh store, so
/// sessions share no guest state.
pub struct Runtime<S: 'static = ()> {
    inner: Arc<Inner<S>>,
}

impl<S: 'static> Clone for Runtime<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

pub(crate) struct Inner<S: 'static> {
    pub(crate) engine: Engine,
    pub(crate) pre: InstancePre<Ctx<S>>,
    pub(crate) limits: Limits,
    pub(crate) wasi: WasiConfig,
    pub(crate) tick: Duration,
    pub(crate) fuel_metering: bool,
    pub(crate) slots: Option<Arc<Semaphore>>,
}

impl<S: Send + 'static> Runtime<S> {
    /// Start configuring a runtime.
    pub fn builder() -> RuntimeBuilder<S> {
        RuntimeBuilder {
            source: None,
            cache: CachePolicy::Default,
            limits: Limits::default(),
            wasi: WasiConfig::default(),
            tick: DEFAULT_EPOCH_TICK,
            fuel_metering: false,
            max_concurrent: None,
            link_wasi: true,
            link: Vec::new(),
            configure: Vec::new(),
        }
    }

    /// Instantiate the component in a fresh store.
    ///
    /// Waits for a free slot first when [`RuntimeBuilder::max_concurrent`]
    /// is set. If the session cannot be started, the error hands the state
    /// from `options` back.
    pub async fn session(&self, options: SessionOptions<S>) -> Result<Session<S>, StartError<S>> {
        Session::start(self.inner.clone(), options).await
    }

    /// The wasmtime engine behind this runtime.
    pub fn engine(&self) -> &Engine {
        &self.inner.engine
    }

    /// The default limits sessions get unless they override them.
    pub fn limits(&self) -> &Limits {
        &self.inner.limits
    }
}

enum Source {
    Bytes(Cow<'static, [u8]>),
    File(PathBuf),
}

/// Configures and builds a [`Runtime`].
pub struct RuntimeBuilder<S: 'static> {
    source: Option<Source>,
    cache: CachePolicy,
    limits: Limits,
    wasi: WasiConfig,
    tick: Duration,
    fuel_metering: bool,
    max_concurrent: Option<usize>,
    link_wasi: bool,
    link: Vec<LinkFn<S>>,
    configure: Vec<ConfigFn>,
}

impl<S: Send + 'static> RuntimeBuilder<S> {
    /// The component to run, as a binary (or its text format).
    pub fn component_bytes(mut self, bytes: impl Into<Cow<'static, [u8]>>) -> Self {
        self.source = Some(Source::Bytes(bytes.into()));
        self
    }

    /// The component to run, read from a file when the runtime is built.
    pub fn component_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.source = Some(Source::File(path.into()));
        self
    }

    /// Keep compiled components in `dir` instead of the OS cache directory.
    ///
    /// The directory must be private to the user running the process:
    /// entries are native code. Each entry is checked against a checksum
    /// written next to it, which catches damaged or half-written files, not
    /// deliberate tampering.
    ///
    /// Every distinct component and wasmtime version or configuration adds an
    /// entry. Loading an entry refreshes its modified time, and writing a new
    /// one removes entries in the directory that have not been loaded for 30
    /// days (failures to do so are ignored).
    ///
    /// Without this call the directory is `$CODELIB_CACHE_DIR` if that is set
    /// and not empty, else `<OS cache dir>/codelib/compiled`.
    pub fn cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache = CachePolicy::Dir(dir.into());
        self
    }

    /// Compile in memory on every start instead of caching on disk.
    pub fn no_cache(mut self) -> Self {
        self.cache = CachePolicy::Disabled;
        self
    }

    /// Default limits for sessions. A fuel budget here turns on fuel metering.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Meter fuel even though the default limits set no budget, so individual
    /// sessions can set one. Without metering, a session that sets a fuel
    /// budget fails to start with [`Error::Config`].
    pub fn fuel_metering(mut self, yes: bool) -> Self {
        self.fuel_metering = yes;
        self
    }

    /// What the guest sees through WASI: clocks, randomness, env, args.
    pub fn wasi(mut self, wasi: WasiConfig) -> Self {
        self.wasi = wasi;
        self
    }

    /// How often a running guest yields to the executor, and the granularity
    /// of timeouts.
    pub fn epoch_tick(mut self, tick: Duration) -> Self {
        self.tick = tick.max(Duration::from_millis(1));
        self
    }

    /// Allow at most `n` sessions at once; further [`Runtime::session`]
    /// calls wait, first come first served.
    pub fn max_concurrent(mut self, n: usize) -> Self {
        self.max_concurrent = Some(n.max(1));
        self
    }

    /// Do not link WASI (and `wasi:http`, and with feature `p3` their 0.3
    /// versions) into the linker. For components that import nothing from
    /// WASI, or embedders that link it themselves.
    pub fn without_wasi(mut self) -> Self {
        self.link_wasi = false;
        self
    }

    /// Add host imports. This is where a custom world's `add_to_linker`
    /// goes; the closure runs once, when the runtime is built.
    pub fn link(
        mut self,
        f: impl FnOnce(&mut Linker<Ctx<S>>) -> wasmtime::Result<()> + 'static,
    ) -> Self {
        self.link.push(Box::new(f));
        self
    }

    /// Adjust the wasmtime [`Config`] before the engine is created, e.g. to
    /// switch to the pooling allocator. Component model and epoch
    /// interruption are always on, and with feature `p3` so is the async
    /// component model.
    pub fn configure(mut self, f: impl FnOnce(&mut Config) + 'static) -> Self {
        self.configure.push(Box::new(f));
        self
    }

    /// Compile (or load from cache) and link the component.
    ///
    /// This is CPU-bound and can take tens of seconds on a cold cache for a
    /// large component; call it at startup, off the async executor.
    pub fn build(self) -> Result<Runtime<S>, Error> {
        let fuel_metering = self.fuel_metering || self.limits.fuel.is_some();

        let mut config = Config::new();
        for configure in self.configure {
            configure(&mut config);
        }
        config.wasm_component_model(true);
        // WASI 0.3 components lift async exports and pass streams and
        // futures; without these the engine refuses to compile them.
        #[cfg(feature = "p3")]
        config
            .concurrency_support(true)
            .wasm_component_model_async(true);
        config.epoch_interruption(true);
        config.consume_fuel(fuel_metering);
        let engine = Engine::new(&config).map_err(Error::Load)?;

        let bytes: Cow<'static, [u8]> = match self.source {
            Some(Source::Bytes(bytes)) => bytes,
            Some(Source::File(path)) => std::fs::read(&path)
                .map_err(|e| Error::Load(wasmtime::format_err!("reading {}: {e}", path.display())))?
                .into(),
            None => return Err(Error::Load(wasmtime::format_err!("no component was given"))),
        };
        let component = cache::load(&engine, &bytes, &self.cache)?;

        let mut linker = Linker::<Ctx<S>>::new(&engine);
        if self.link_wasi {
            wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(Error::Load)?;
            #[cfg(feature = "http")]
            wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)
                .map_err(Error::Load)?;
            // The 0.3 interfaces have their own names (`@0.3.0`), so they sit
            // next to the 0.2 ones: one runtime hosts either kind of
            // component, or one that imports both (as Rust guests do, whose
            // standard library still uses 0.2). Both read the same `WasiCtx`
            // and egress hooks.
            #[cfg(feature = "p3")]
            wasmtime_wasi::p3::add_to_linker(&mut linker).map_err(Error::Load)?;
            #[cfg(all(feature = "p3", feature = "http"))]
            wasmtime_wasi_http::p3::add_to_linker(&mut linker).map_err(Error::Load)?;
        }
        for link in self.link {
            link(&mut linker).map_err(Error::Load)?;
        }
        let pre = linker.instantiate_pre(&component).map_err(Error::Load)?;

        spawn_ticker(engine.weak(), self.tick)?;

        Ok(Runtime {
            inner: Arc::new(Inner {
                engine,
                pre,
                limits: self.limits,
                wasi: self.wasi,
                tick: self.tick,
                fuel_metering,
                slots: self.max_concurrent.map(|n| Arc::new(Semaphore::new(n))),
            }),
        })
    }
}

/// Advance the engine epoch on a plain thread until the engine is gone.
///
/// A thread rather than an async task on purpose: it keeps ticking while
/// every executor thread is busy running guests, it needs no runtime to be
/// current when the `Runtime` is built, and it stops by itself once the last
/// engine handle is dropped.
fn spawn_ticker(engine: EngineWeak, tick: Duration) -> Result<(), Error> {
    std::thread::Builder::new()
        .name("codelib-epoch".into())
        .spawn(move || {
            loop {
                std::thread::sleep(tick);
                match engine.upgrade() {
                    Some(engine) => engine.increment_epoch(),
                    None => break,
                }
            }
        })
        .map(drop)
        // Without the ticker, CPU-bound guests are never interrupted.
        .map_err(|error| {
            Error::Load(wasmtime::format_err!(
                "could not start the epoch thread: {error}"
            ))
        })
}
