use std::fmt::Debug;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

pub trait PhaseTracker<P>: Send + Sync
where
    P: PartialEq + Debug + Clone + Send + Sync,
{
    fn advance(&self, id: u64, from: Option<P>, to: P);
    fn expect_any_and_set(&self, id: u64, valid_from: &[P], to: P);
    fn upsert(&self, id: u64, valid_from: &[P], to: P);
}

pub struct DefaultPhaseTracker<P> {
    next_id: AtomicU64,
    phases: DashMap<u64, P>,
}

impl<P> DefaultPhaseTracker<P> {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            phases: DashMap::new(),
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn phases(&self) -> &DashMap<u64, P> {
        &self.phases
    }
}

impl<P> PhaseTracker<P> for DefaultPhaseTracker<P>
where
    P: PartialEq + Debug + Clone + Send + Sync,
{
    fn advance(&self, id: u64, from: Option<P>, to: P) {
        let _ = self
            .phases
            .entry(id)
            .and_modify(|phase| {
                if let Some(expected) = &from {
                    assert_eq!(
                        *phase,
                        *expected,
                        "item {id} expected phase {expected:?} but was {phase:?}"
                    );
                }
                *phase = to.clone();
            })
            .or_insert_with(|| {
                assert!(
                    from.is_none(),
                    "item {id} expected phase {expected:?} but was absent",
                    expected = from.as_ref().unwrap(),
                );
                to
            });
    }

    fn expect_any_and_set(&self, id: u64, valid_from: &[P], to: P) {
        self.phases
            .entry(id)
            .and_modify(|phase| {
                assert!(
                    valid_from.contains(phase),
                    "item {id} expected one of {valid_from:?} but was {phase:?}"
                );
                *phase = to.clone();
            })
            .or_insert_with(|| {
                to
            });
    }

    fn upsert(&self, id: u64, valid_from: &[P], to: P) {
        self.phases
            .entry(id)
            .and_modify(|phase| {
                assert!(
                    valid_from.contains(phase),
                    "item {id} expected one of {valid_from:?} but was {phase:?}"
                );
                *phase = to.clone();
            })
            .or_insert_with(|| {
                to
            });
    }
}

impl<P> Default for DefaultPhaseTracker<P>
where
    P: PartialEq + Debug + Clone + Send + Sync,
{
    fn default() -> Self {
        Self::new()
    }
}