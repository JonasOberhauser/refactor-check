//! Checkpointable machines on servyi-states' [`Handle`].
//!
//! A machine's root node is its current stage, and [`drive`] runs it. Fan-out
//! stages run their children through [`fanout`] / [`try_fanout`], never
//! `Handle::fanout` directly, which would bypass `sequential-fanout`. Each
//! child keeps its progress and result in its own node and serves a safepoint
//! before each IO call, so a resumed child that already holds its result does
//! no work. Contexts the children use are handed out in child order before the
//! fan-out, so their IDs do not depend on scheduling.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

pub use servyi_states::checkpoint::*;

/// What a stage's transition yields: the machine's next stage, or its result.
pub enum Step<S, R> {
    State(S),
    Result(R),
}

/// Drives a machine whose root node is its current stage. Each step serves a
/// safepoint, runs the stage's fan-out on the handle, then the stage's owned
/// transition. The node is `None` only within a step, between taking the
/// stage out and storing its successor, where no safepoint can occur.
pub fn drive<S, R>(
    start: S,
    fan_out: impl Fn(&mut Handle<'_, Option<S>>) -> anyhow::Result<()>,
    transition: impl Fn(S) -> anyhow::Result<Step<S, R>>,
) -> anyhow::Result<R> {
    with_checkpoint_pair(Some(start), |mut h, _requester| loop {
        h.safepoint();
        fan_out(&mut h)?;
        let stage = h.take().ok_or_else(|| anyhow::anyhow!("machine stage missing at step start"))?;
        match transition(stage)? {
            Step::State(next) => *h = Some(next),
            Step::Result(result) => return Ok(result),
        }
    })
}

/// Lets the children of one [`try_fanout`] see that a sibling failed, so they
/// stop at their next step instead of finishing work whose result is thrown
/// away.
pub struct Cancellation(AtomicBool);

impl Cancellation {
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The splitter of a fan-out: projects a node onto its children.
pub type Split<T, C> = dyn for<'x> Fn(&'x mut T) -> Vec<&'x mut C> + Send + Sync;

/// [`Handle::fanout`]: `action` runs on every child, each on its own scoped
/// thread, and stores its result inside the child's node. With the
/// `sequential-fanout` feature the children take turns in child order, so runs
/// against providers that share one fuzz stream replay deterministically.
pub fn fanout<'o, T, C, F>(h: &mut Handle<'o, T>, split: &Split<T, C>, action: F)
where
    C: Send + 'o,
    F: Fn(Handle<'_, C>) + Send + Sync,
{
    #[cfg(feature = "sequential-fanout")]
    {
        let turns = sequential::Turns::new(split(&mut **h));
        h.fanout(split, |child| turns.run(child, &action));
    }
    #[cfg(not(feature = "sequential-fanout"))]
    h.fanout(split, action);
}

/// [`fanout`] for fallible children: the first error cancels the siblings and
/// is returned once all children joined. Siblings stop at their next
/// [`Cancellation`] check, so one waiting in the error gate keeps the fan-out
/// alive until the gate releases it. The error is not part of the tree: a
/// snapshot taken before the join records the failed child as unfinished, and
/// a resumed run retries it.
pub fn try_fanout<'o, T, C, F>(h: &mut Handle<'o, T>, split: &Split<T, C>, action: F) -> anyhow::Result<()>
where
    C: Send + 'o,
    F: Fn(Handle<'_, C>, &Cancellation) -> anyhow::Result<()> + Send + Sync,
{
    let cancellation = Cancellation(AtomicBool::new(false));
    let first_error: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    fanout(h, split, |child| {
        if let Err(e) = action(child, &cancellation) {
            cancellation.0.store(true, Ordering::SeqCst);
            // Only a check-and-store of the Option runs under this lock, so a
            // poisoned guard still holds a valid Option.
            let mut slot = first_error.lock().unwrap_or_else(PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(e);
            }
        }
    });
    // See above: the Option is valid even if the lock was poisoned.
    match first_error.into_inner().unwrap_or_else(PoisonError::into_inner) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(feature = "sequential-fanout")]
mod sequential {
    use std::sync::{Condvar, Mutex, PoisonError};
    use std::time::Duration;

    use super::Handle;

    /// How long a waiting child sleeps between safepoints.
    const TURN_POLL_INTERVAL: Duration = Duration::from_millis(1);

    /// Hands the children of one fan-out their turns, in child order.
    pub(super) struct Turns {
        /// Child node addresses in child order; a child's index is its turn.
        order: Vec<usize>,
        next: Mutex<usize>,
        turn_passed: Condvar,
    }

    impl Turns {
        pub(super) fn new<C>(children: Vec<&mut C>) -> Self {
            const { assert!(size_of::<C>() > 0, "children are told apart by address") };
            Self {
                order: children.into_iter().map(|c| std::ptr::from_mut(c) as usize).collect(),
                next: Mutex::new(0),
                turn_passed: Condvar::new(),
            }
        }

        /// Waits for the child's turn, serving safepoints meanwhile, runs
        /// `action`, then passes the turn on, also when `action` panics.
        pub(super) fn run<C>(&self, mut child: Handle<'_, C>, action: impl FnOnce(Handle<'_, C>)) {
            let address = std::ptr::from_ref::<C>(&child) as usize;
            // The split yields the same children for `new` and the fan-out, so
            // every child has a turn; one without would run out of order, but
            // never take another child's turn.
            let Some(turn) = self.order.iter().position(|&a| a == address) else {
                action(child);
                return;
            };
            loop {
                {
                    // Only a usize is read and written under the lock, so a
                    // poisoned guard still holds a valid turn.
                    let next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
                    if *next >= turn {
                        break;
                    }
                    let _woken = self
                        .turn_passed
                        .wait_timeout(next, TURN_POLL_INTERVAL)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                child.safepoint();
            }
            let _pass_on = PassOn(self);
            action(child);
        }
    }

    struct PassOn<'a>(&'a Turns);

    impl Drop for PassOn<'_> {
        fn drop(&mut self) {
            // See `Turns::run`: the turn stays valid under a poisoned lock.
            *self.0.next.lock().unwrap_or_else(PoisonError::into_inner) += 1;
            self.0.turn_passed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Child {
        fail: bool,
        steps: u32,
        done: bool,
    }

    const STEPS: u32 = 1000;
    const STEP_DELAY: std::time::Duration = std::time::Duration::from_millis(1);

    fn run_child(mut h: Handle<'_, Child>, cancellation: &Cancellation) -> anyhow::Result<()> {
        if h.fail {
            anyhow::bail!("child failed");
        }
        while h.steps < STEPS {
            h.safepoint();
            if cancellation.is_cancelled() {
                return Ok(());
            }
            h.steps += 1;
            std::thread::sleep(STEP_DELAY);
        }
        h.done = true;
        Ok(())
    }

    fn children(fail: &[bool]) -> Vec<Child> {
        fail.iter().map(|&fail| Child { fail, steps: 0, done: false }).collect()
    }

    #[test]
    fn results_stay_in_the_children() {
        let out = with_checkpoint_pair(children(&[false, false]), |mut h, _req| {
            try_fanout(&mut h, &|c| c.iter_mut().collect(), run_child).unwrap();
            h.iter().map(|c| c.done).collect::<Vec<_>>()
        });
        assert_eq!(out, vec![true, true]);
    }

    #[test]
    fn first_error_cancels_siblings() {
        let (err, steps) = with_checkpoint_pair(children(&[false, true, false]), |mut h, _req| {
            let err = try_fanout(&mut h, &|c| c.iter_mut().collect(), run_child).unwrap_err();
            (err.to_string(), h.iter().map(|c| c.steps).collect::<Vec<_>>())
        });
        assert_eq!(err, "child failed");
        #[cfg(not(feature = "sequential-fanout"))]
        assert!(steps.iter().all(|&s| s < STEPS), "siblings must stop early: {steps:?}");
        #[cfg(feature = "sequential-fanout")]
        assert_eq!(steps, vec![STEPS, 0, 0], "later siblings must not start working");
    }

    /// A snapshotter stops the world mid-fan-out (children waiting for their
    /// turn included) and sees progress only ever accumulate.
    #[test]
    fn snapshots_mid_fanout_see_progress_accumulate() {
        const SNAPSHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
        let stop = AtomicBool::new(false);
        let observed = with_checkpoint_pair(children(&[false, false, false]), |mut h, req| {
            std::thread::scope(|s| {
                let snapper = s.spawn(|| {
                    let mut seen = Vec::new();
                    while !stop.load(Ordering::SeqCst) {
                        if let Some(guard) = req.request_timeout(SNAPSHOT_TIMEOUT) {
                            let steps: u32 = guard.iter().map(|c| c.steps).sum();
                            seen.push((guard.iter().filter(|c| c.done).count(), steps));
                        }
                        std::thread::sleep(STEP_DELAY * 20);
                    }
                    seen
                });
                try_fanout(&mut h, &|c| c.iter_mut().collect(), run_child).unwrap();
                stop.store(true, Ordering::SeqCst);
                while !snapper.is_finished() {
                    h.safepoint();
                    std::thread::sleep(STEP_DELAY);
                }
                snapper.join().unwrap()
            })
        });
        assert!(observed.iter().any(|&(done, _)| done < 3), "no snapshot mid-fan-out: {observed:?}");
        for pair in observed.windows(2) {
            assert!(pair[0].0 <= pair[1].0 && pair[0].1 <= pair[1].1, "progress went back: {observed:?}");
        }
    }

    #[cfg(feature = "sequential-fanout")]
    #[test]
    fn children_take_turns_in_order() {
        const CHILDREN: usize = 8;
        let ran = Mutex::new(Vec::new());
        let order = with_checkpoint_pair((0..CHILDREN).collect::<Vec<usize>>(), |mut h, _req| {
            fanout(&mut h, &|c| c.iter_mut().collect(), |child| {
                std::thread::sleep(STEP_DELAY * u32::try_from(CHILDREN - *child).unwrap());
                ran.lock().unwrap().push(*child);
            });
            ran.into_inner().unwrap()
        });
        assert_eq!(order, (0..CHILDREN).collect::<Vec<_>>());
    }
}
