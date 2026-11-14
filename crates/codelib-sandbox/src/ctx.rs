//! The data inside each wasmtime `Store`.

use std::time::Duration;

use rand::SeedableRng as _;
use rand::rngs::StdRng;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{
    HostMonotonicClock, HostWallClock, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};

use crate::capture::Capture;
use crate::limits::MemoryLimiter;

/// Store data for one session: the sandbox's own bookkeeping plus the
/// embedder's state `S`.
///
/// Custom host imports are linked against `Ctx<S>` and reach the embedder's
/// state through [`Ctx::state`].
pub struct Ctx<S: 'static> {
    /// The embedder's per-session state.
    pub state: S,
    pub(crate) wasi: WasiCtx,
    pub(crate) table: ResourceTable,
    pub(crate) limiter: MemoryLimiter,
    #[cfg(feature = "http")]
    pub(crate) http: wasmtime_wasi_http::WasiHttpCtx,
    #[cfg(feature = "http")]
    pub(crate) hooks: crate::http::EgressHooks,
}

impl<S: 'static> Ctx<S> {
    /// The resource table, for host imports that hand resources to the guest.
    pub fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl<S: Send> WasiView for Ctx<S> {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

#[cfg(feature = "http")]
impl<S: Send> wasmtime_wasi_http::WasiHttpView for Ctx<S> {
    fn http(&mut self) -> wasmtime_wasi_http::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

/// What the guest sees of the outside world through WASI. Everything is off
/// by default: no filesystem, no sockets, no environment, no arguments, and
/// stdout/stderr go to an in-memory capture.
#[derive(Clone, Debug, Default)]
pub struct WasiConfig {
    pub clocks: Clocks,
    pub random: Random,
    pub env: Vec<(String, String)>,
    pub args: Vec<String>,
}

impl WasiConfig {
    /// Frozen clocks and seeded randomness: the same program sees the same
    /// `Date.now()` and `Math.random()` on every run.
    pub fn deterministic(seed: u64) -> Self {
        Self {
            clocks: Clocks::Frozen {
                wall: Duration::from_secs(1_767_225_600),
            }, // 2026-01-01T00:00:00Z
            random: Random::Seeded(seed),
            ..Self::default()
        }
    }
}

/// Clock source for the guest.
#[derive(Clone, Copy, Debug, Default)]
pub enum Clocks {
    /// The host's clocks.
    #[default]
    Real,
    /// A wall clock stuck at `wall` (time since the Unix epoch) and a
    /// monotonic clock stuck at zero. Guest timers that wait on the
    /// monotonic clock still wait in real time.
    Frozen { wall: Duration },
}

/// Randomness source for the guest.
#[derive(Clone, Copy, Debug, Default)]
pub enum Random {
    /// The operating system's generator.
    #[default]
    Os,
    /// A deterministic generator: same seed, same sequence.
    Seeded(u64),
}

struct FrozenWall(Duration);

impl HostWallClock for FrozenWall {
    fn resolution(&self) -> Duration {
        Duration::from_millis(1)
    }
    fn now(&self) -> Duration {
        self.0
    }
}

struct FrozenMonotonic;

impl HostMonotonicClock for FrozenMonotonic {
    fn resolution(&self) -> u64 {
        1_000_000
    }
    fn now(&self) -> u64 {
        0
    }
}

pub(crate) fn build_wasi(config: &WasiConfig, stdout: &Capture, stderr: &Capture) -> WasiCtx {
    let mut builder = WasiCtxBuilder::new();
    builder.stdout(stdout.clone());
    builder.stderr(stderr.clone());
    for (name, value) in &config.env {
        builder.env(name, value);
    }
    for arg in &config.args {
        builder.arg(arg);
    }
    if let Clocks::Frozen { wall } = config.clocks {
        builder.wall_clock(FrozenWall(wall));
        builder.monotonic_clock(FrozenMonotonic);
    }
    if let Random::Seeded(seed) = config.random {
        builder.secure_random(StdRng::seed_from_u64(seed));
        builder.insecure_random(StdRng::seed_from_u64(seed ^ 0x9e37_79b9_7f4a_7c15));
        builder.insecure_random_seed(u128::from(seed) << 64 | u128::from(!seed));
    }
    builder.build()
}
