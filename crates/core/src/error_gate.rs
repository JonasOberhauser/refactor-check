use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

/// Returned by [`ErrorGate::report_and_wait`] when shutdown is requested.
/// Callers should propagate this via `?` to unwind the background thread.
#[derive(Debug)]
pub struct ShutdownRequested;

impl fmt::Display for ShutdownRequested {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "shutdown requested")
    }
}

impl std::error::Error for ShutdownRequested {}

/// Pause mechanism for recoverable errors in background work.
///
/// The background thread calls `report_and_wait` when it hits a recoverable
/// error. This sends the error message to a channel and blocks until the
/// epoch changes (someone bumped it) or shutdown is requested.
pub struct ErrorGate {
    epoch: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::Sender<String>,
}

impl ErrorGate {
    pub fn new(epoch: Arc<AtomicU64>, shutdown: Arc<AtomicBool>, tx: mpsc::Sender<String>) -> Self {
        Self { epoch, shutdown, tx }
    }

    pub async fn report_and_wait(&self, error: &str) -> Result<(), ShutdownRequested> {
        let my_epoch = self.epoch.load(Ordering::Acquire);
        let _ = self.tx.send(error.to_string());
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return Err(ShutdownRequested);
            }
            if self.epoch.load(Ordering::Acquire) != my_epoch {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok(())
    }
}
