use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub struct LiveConfig<T> {
    inner: RwLock<T>,
    version: AtomicU64,
}

impl<T> LiveConfig<T> {
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            inner: RwLock::new(value),
            version: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> (u64, T)
    where
        T: Clone,
    {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let version = self.version.load(Ordering::Acquire);
        (version, guard.clone())
    }

    pub fn update(&self, f: impl FnOnce(&mut T)) -> u64 {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|e| e.into_inner());
        f(&mut guard);
        self.version.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

impl<T> LiveConfig<T> {
    #[must_use]
    pub fn shared(value: T) -> Arc<Self> {
        Arc::new(Self::new(value))
    }
}

#[cfg(test)]
mod tests {
    /// Version-counter fixtures.
    const INITIAL_VERSION: u64 = 0;
    const FIRST_UPDATE_VERSION: u64 = 1;
    const SECOND_UPDATE_VERSION: u64 = 2;
    /// Value fixtures.
    const SEED_VALUE: u64 = 42;
    const FIRST_VALUE: u64 = 10;
    const SECOND_VALUE: u64 = 20;
    const SHARED_SEED: u64 = 100;
    const SHARED_UPDATED: u64 = 200;
    /// String-update fixture pieces.
    const GREETING: &str = "hello";
    const GREETING_SUFFIX: &str = " world";

    use super::*;

    #[test]
    fn test_initial_snapshot() {
        let lc = LiveConfig::new(SEED_VALUE);
        let (version, value) = lc.snapshot();
        assert_eq!(version, INITIAL_VERSION);
        assert_eq!(value, SEED_VALUE);
    }

    #[test]
    fn test_update_bumps_version() {
        let lc = LiveConfig::new(0u64);
        let v1 = lc.update(|v| *v = FIRST_VALUE);
        assert_eq!(v1, FIRST_UPDATE_VERSION);
        let (version, value) = lc.snapshot();
        assert_eq!(version, FIRST_UPDATE_VERSION);
        assert_eq!(value, FIRST_VALUE);

        let v2 = lc.update(|v| *v = SECOND_VALUE);
        assert_eq!(v2, SECOND_UPDATE_VERSION);
        let (version, value) = lc.snapshot();
        assert_eq!(version, SECOND_UPDATE_VERSION);
        assert_eq!(value, SECOND_VALUE);
    }

    #[test]
    fn test_version_method() {
        let lc = LiveConfig::new(String::from(GREETING));
        assert_eq!(lc.version(), INITIAL_VERSION);
        lc.update(|v| v.push_str(GREETING_SUFFIX));
        assert_eq!(lc.version(), FIRST_UPDATE_VERSION);
    }

    #[test]
    fn test_shared_works_across_clone() {
        let lc = LiveConfig::shared(SHARED_SEED);
        let clone = Arc::clone(&lc);
        lc.update(|v| *v = SHARED_UPDATED);
        let (version, value) = clone.snapshot();
        assert_eq!(version, FIRST_UPDATE_VERSION);
        assert_eq!(value, SHARED_UPDATED);
    }
}
