//! Byte streams under the gRPC channel, and the record of how they failed.
//!
//! `tonic` reports a failed call as a `Status` that no longer carries the
//! transport error behind it (a connect failure becomes "service was not
//! ready", a dropped connection an h2 or hyper message). The failure classes
//! of `docs/plugin-contract#plugin-config` have to come from the real
//! outcome, so the dialer and every connection it produces record their
//! failures, classified at the point they happen, in a [`TransportLog`]. A
//! call that fails consults the log for failures recorded while it was in
//! flight; a failure with none recorded is an RPC error from a plugin that
//! was reached (`plugin_error`).

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use janus_core::contract::plugin::{PluginError, PluginFailureClass};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// How many recent failures the log keeps; more than any one call spans.
const LOG_DEPTH: usize = 16;

/// Recent transport failures, in order, each with a sequence number.
#[derive(Debug, Default)]
pub(crate) struct TransportLog {
    seq: AtomicU64,
    recent: Mutex<VecDeque<(u64, PluginError)>>,
}

/// How much a class says about the cause: a TLS verdict explains a dropped
/// connection, not the other way round.
fn specificity(class: PluginFailureClass) -> u8 {
    match class {
        PluginFailureClass::RejectedClientCert | PluginFailureClass::NoClientAuth => 3,
        PluginFailureClass::TlsError => 2,
        PluginFailureClass::Unreachable | PluginFailureClass::Timeout => 1,
        PluginFailureClass::Error => 0,
    }
}

impl TransportLog {
    /// The current position; pass to [`TransportLog::since`] after a call.
    pub(crate) fn mark(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    pub(crate) fn record(&self, err: PluginError) {
        let mut recent = self.recent.lock().unwrap_or_else(|p| p.into_inner());
        let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        if recent.len() == LOG_DEPTH {
            recent.pop_front();
        }
        recent.push_back((n, err));
    }

    /// The most specific failure recorded after `mark`, latest first among
    /// equals.
    pub(crate) fn since(&self, mark: u64) -> Option<PluginError> {
        let recent = self.recent.lock().unwrap_or_else(|p| p.into_inner());
        recent
            .iter()
            .filter(|(n, _)| *n > mark)
            .max_by_key(|(_, e)| specificity(e.class))
            .map(|(_, e)| e.clone())
    }
}

/// The streams the plugin client runs gRPC over.
pub(crate) trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// How a connection's I/O errors are classified.
pub(crate) type Classifier = fn(&io::Error) -> PluginError;

/// Classifier for connections without TLS.
pub(crate) fn classify_plain(err: &io::Error) -> PluginError {
    PluginError::new(
        PluginFailureClass::Unreachable,
        format!("plugin connection failed: {err}"),
    )
}

/// A connection that records its read/write failures, and the plugin
/// closing it, in the log.
pub(crate) struct RecordingIo {
    inner: Box<dyn Io>,
    log: Arc<TransportLog>,
    classify: Classifier,
}

impl RecordingIo {
    pub(crate) fn new(inner: Box<dyn Io>, log: Arc<TransportLog>, classify: Classifier) -> Self {
        RecordingIo {
            inner,
            log,
            classify,
        }
    }

    fn note<T>(&self, result: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        if let Poll::Ready(Err(e)) = &result {
            self.log.record((self.classify)(e));
        }
        result
    }
}

impl AsyncRead for RecordingIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let wanted = buf.remaining() > 0;
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            if wanted && buf.filled().len() == before {
                self.log.record(PluginError::new(
                    PluginFailureClass::Unreachable,
                    "the plugin closed the connection",
                ));
            }
        }
        self.note(result)
    }
}

impl AsyncWrite for RecordingIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.note(result)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        self.note(result)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.note(result)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_log_prefers_the_most_specific_failure_since_mark() {
        let log = TransportLog::default();
        log.record(PluginError::new(PluginFailureClass::TlsError, "old"));
        let mark = log.mark();
        assert_eq!(log.since(mark), None);
        log.record(PluginError::new(PluginFailureClass::Unreachable, "closed"));
        log.record(PluginError::new(
            PluginFailureClass::RejectedClientCert,
            "alert",
        ));
        log.record(PluginError::new(PluginFailureClass::Unreachable, "reset"));
        let got = log.since(mark).expect("recorded");
        assert_eq!(got.class, PluginFailureClass::RejectedClientCert);
        for n in 0..(LOG_DEPTH * 2) {
            log.record(PluginError::new(
                PluginFailureClass::Unreachable,
                format!("{n}"),
            ));
        }
        assert_eq!(log.recent.lock().expect("lock").len(), LOG_DEPTH);
        assert_eq!(
            log.since(log.mark() - 1).map(|e| e.message),
            Some(format!("{}", LOG_DEPTH * 2 - 1))
        );
    }
}
