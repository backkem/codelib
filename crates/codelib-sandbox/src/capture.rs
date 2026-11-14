//! Captured guest stdout/stderr.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};

/// What the guest wrote to stdout and stderr.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Console {
    pub stdout: String,
    pub stderr: String,
    /// True when either stream ran past [`Limits::console_bytes`] and the
    /// rest was dropped.
    ///
    /// [`Limits::console_bytes`]: crate::Limits::console_bytes
    pub truncated: bool,
}

/// An in-memory output stream with a cap. Unlike a plain bounded pipe, going
/// past the cap is not an error for the writer: the excess is dropped and the
/// stream is flagged, so a chatty guest keeps running.
#[derive(Clone)]
pub(crate) struct Capture {
    cap: usize,
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    bytes: Vec<u8>,
    truncated: bool,
}

impl Capture {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            inner: Arc::default(),
        }
    }

    pub(crate) fn text(&self) -> String {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&inner.bytes).into_owned()
    }

    pub(crate) fn truncated(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .truncated
    }

    /// Append bytes, dropping (and flagging) whatever does not fit under the
    /// cap.
    fn append(&self, buf: &[u8]) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let room = self.cap.saturating_sub(inner.bytes.len());
        let take = room.min(buf.len());
        inner.bytes.extend_from_slice(&buf[..take]);
        if take < buf.len() {
            inner.truncated = true;
        }
    }

    /// Add a line written by the host, not the guest: a note for whoever
    /// reads the console about something the guest could not see for itself.
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    pub(crate) fn note(&self, line: &str) {
        self.append(format!("[codelib] {line}\n").as_bytes());
    }
}

impl IsTerminal for Capture {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for Capture {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

impl AsyncWrite for Capture {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.append(buf);
        // Report the whole buffer as written: the excess is dropped on purpose.
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
