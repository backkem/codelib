use std::time::Duration;

/// Why a sandbox operation failed.
///
/// Script-level errors (an exception thrown by the guest program) are not in
/// here: those are ordinary return values of the world's exports. This type
/// covers the cases where the sandbox itself stopped the guest.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The component could not be read, compiled or linked.
    #[error("failed to load component: {0:#}")]
    Load(wasmtime::Error),

    /// The component could not be instantiated.
    #[error("failed to instantiate component: {0:#}")]
    Instantiate(wasmtime::Error),

    /// The session asked for something the runtime was not built for, e.g.
    /// a fuel budget on a runtime without fuel metering.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// The guest ran past its deadline and was interrupted.
    #[error("timed out after {0:?}")]
    Timeout(Duration),

    /// The guest asked for more memory than its limit allows and failed
    /// because of it.
    #[error("out of memory: exceeded the {0} byte limit")]
    OutOfMemory(usize),

    /// The guest used up its fuel budget.
    #[error("out of fuel")]
    OutOfFuel,

    /// The guest trapped for another reason.
    #[error("guest trapped: {0:#}")]
    Trap(wasmtime::Error),

    /// An earlier call on this session trapped, timed out or was cancelled;
    /// its instance is in an undefined state and cannot be used again.
    #[error("session is no longer usable after an earlier failure")]
    Poisoned,
}

/// A session that could not be started, with the embedder's state handed
/// back.
pub struct StartError<S> {
    /// Why the session did not start.
    pub error: Error,
    /// The state that was passed in [`SessionOptions`](crate::SessionOptions).
    pub state: S,
}

impl<S> StartError<S> {
    /// Drop the state and keep the error.
    pub fn into_error(self) -> Error {
        self.error
    }
}

impl<S> std::fmt::Debug for StartError<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<S> std::fmt::Display for StartError<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

impl<S> std::error::Error for StartError<S> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

impl<S> From<StartError<S>> for Error {
    fn from(error: StartError<S>) -> Self {
        error.error
    }
}
