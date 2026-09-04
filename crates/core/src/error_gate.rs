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

#[cfg(test)]
mod tests {
    use super::*;

    async fn eventually(what: impl Fn() -> bool) {
        for _ in 0..200 {
            if what() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition never became true");
    }

    #[tokio::test]
    async fn parking_registers_and_continue_clears() {
        let epoch = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, _rx) = mpsc::channel();
        let parked: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let gate = Arc::new(ErrorGate::new(
            epoch.clone(),
            shutdown.clone(),
            tx,
            parked.clone(),
        ));

        let g = gate.clone();
        let handle = tokio::spawn(async move { g.report_and_wait("z3 exploded").await });

        eventually(|| parked.lock().unwrap().len() == 1).await;
        assert_eq!(parked.lock().unwrap()[0], "z3 exploded");

        // 'continue' bumps the epoch -> the parked thread resumes and
        // unregisters itself.
        epoch.store(1, Ordering::Release);
        handle.await.unwrap().unwrap();
        assert!(parked.lock().unwrap().is_empty(), "release must clear the parked list");
    }

    #[tokio::test]
    async fn shutdown_releases_and_clears() {
        let epoch = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, _rx) = mpsc::channel();
        let parked: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let gate = Arc::new(ErrorGate::new(
            epoch.clone(),
            shutdown.clone(),
            tx,
            parked.clone(),
        ));

        let g = gate.clone();
        let handle = tokio::spawn(async move { g.report_and_wait("opencode auth failed").await });
        eventually(|| parked.lock().unwrap().len() == 1).await;

        shutdown.store(true, Ordering::Release);
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("shutdown"));
        assert!(parked.lock().unwrap().is_empty(), "abort must clear the parked list");
    }
}
