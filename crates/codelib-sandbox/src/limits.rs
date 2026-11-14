use std::time::Duration;

use wasmtime::ResourceLimiter;

/// Resource budget for one session.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Wall-clock budget for instantiation and for each call into the guest.
    pub timeout: Duration,
    /// Cap on guest linear memory, in bytes. Table growth is counted against
    /// it too (two pointers per element).
    pub memory_bytes: usize,
    /// Fuel budget for instantiation and for each call into the guest, like
    /// the time budget: every call starts with a full tank. `None` means
    /// unmetered.
    ///
    /// Fuel needs metering in the engine, which is decided when the runtime
    /// is built: setting a budget on the runtime's default limits turns it
    /// on, as does [`RuntimeBuilder::fuel_metering`]. A session that asks for
    /// a budget on a runtime without metering fails to start with
    /// [`Error::Config`] rather than running unmetered.
    ///
    /// [`RuntimeBuilder::fuel_metering`]: crate::RuntimeBuilder::fuel_metering
    /// [`Error::Config`]: crate::Error::Config
    pub fuel: Option<u64>,
    /// Cap on captured stdout and on captured stderr, each. Output past the
    /// cap is dropped and flagged, never an error for the guest.
    pub console_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            memory_bytes: 256 * 1024 * 1024,
            fuel: None,
            console_bytes: 1024 * 1024,
        }
    }
}

impl Limits {
    /// Set the wall-clock budget.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the linear-memory cap in bytes.
    pub fn memory(mut self, bytes: usize) -> Self {
        self.memory_bytes = bytes;
        self
    }

    /// Set the fuel budget per call.
    pub fn fuel(mut self, fuel: u64) -> Self {
        self.fuel = Some(fuel);
        self
    }

    /// Set the cap on captured stdout and stderr, each.
    pub fn console(mut self, bytes: usize) -> Self {
        self.console_bytes = bytes;
        self
    }
}

/// Host memory a table element is charged for: a pointer plus bookkeeping.
const TABLE_ELEMENT_BYTES: usize = 2 * std::mem::size_of::<usize>();

/// Caps guest memory and remembers whether the cap was hit.
///
/// A denied growth makes `memory.grow` (or `table.grow`) return -1 rather
/// than trap, so the guest usually reports it as an ordinary error of its own
/// (a JavaScript engine throws an out-of-memory exception). The recorded flag
/// is what lets the host tell that apart from a program-authored error. It is
/// kept per call (cleared when a call starts) and, separately, for the whole
/// session.
pub(crate) struct MemoryLimiter {
    max_bytes: usize,
    hit: bool,
    ever_hit: bool,
}

impl MemoryLimiter {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            hit: false,
            ever_hit: false,
        }
    }

    /// Start counting for a new call.
    pub(crate) fn begin_call(&mut self) {
        self.hit = false;
    }

    /// Whether a growth was refused since the current call began.
    pub(crate) fn hit(&self) -> bool {
        self.hit
    }

    /// Whether a growth was ever refused in this session.
    pub(crate) fn ever_hit(&self) -> bool {
        self.ever_hit
    }

    pub(crate) fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    fn record(&mut self, allowed: bool) -> bool {
        if !allowed {
            self.hit = true;
            self.ever_hit = true;
        }
        allowed
    }
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(self.record(desired <= self.max_bytes))
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let bytes = desired.checked_mul(TABLE_ELEMENT_BYTES);
        Ok(self.record(bytes.is_some_and(|bytes| bytes <= self.max_bytes)))
    }
}
