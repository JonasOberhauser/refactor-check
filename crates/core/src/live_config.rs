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
    use super::*;

    #[test]
    fn test_initial_snapshot() {
        let lc = LiveConfig::new(42u64);
        let (version, value) = lc.snapshot();
        assert_eq!(version, 0);
        assert_eq!(value, 42);
    }

    #[test]
    fn test_update_bumps_version() {
        let lc = LiveConfig::new(0u64);
        let v1 = lc.update(|v| *v = 10);
        assert_eq!(v1, 1);
        let (version, value) = lc.snapshot();
        assert_eq!(version, 1);
        assert_eq!(value, 10);

        let v2 = lc.update(|v| *v = 20);
        assert_eq!(v2, 2);
        let (version, value) = lc.snapshot();
        assert_eq!(version, 2);
        assert_eq!(value, 20);
    }

    #[test]
    fn test_version_method() {
        let lc = LiveConfig::new(String::from("hello"));
        assert_eq!(lc.version(), 0);
        lc.update(|v| v.push_str(" world"));
        assert_eq!(lc.version(), 1);
    }

    #[test]
    fn test_shared_works_across_clone() {
        let lc = LiveConfig::shared(100u64);
        let clone = Arc::clone(&lc);
        lc.update(|v| *v = 200);
        let (version, value) = clone.snapshot();
        assert_eq!(version, 1);
        assert_eq!(value, 200);
    }
}
