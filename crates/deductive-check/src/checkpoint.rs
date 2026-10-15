//! Checkpoint integration: the snapshot shape and the checkpointed runner.
//!
//! The shape (`MachineSnapshot`) is the STABLE artifact — restore code may
//! change between versions while the shape does not (see
//! servyi-states-snapshot's cross-version methodology). The runner wraps
//! the state machine in the checkpoint protocol: safepoints between
//! transitions, a snapshotter thread saving the shape atomically, and a
//! final save so a completed run can be replayed from its artifact with
//! zero provider invocations.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::piece_manager::MachineCheckpointable;
use crate::provider::Providers;
use crate::result::VerificationResult;
use crate::states::{AlgorithmState, PreflightGit, Step};
use servyi_states::checkpoint::with_checkpoint_pair;

/// The stable snapshot shape. Phases and outcomes are keyed by the
/// ContextId hash; `result` is present once the machine finished.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct MachineSnapshot {
    pub piece_phases: Vec<(u64, crate::phase::CodePiecePhase)>,
    pub formula_phases: Vec<(u64, crate::phase::FormulaPhase)>,
    pub outcomes: Vec<(
        crate::code_piece::FunctionId,
        Option<crate::result::ClosedPiece>,
        Option<crate::result::UnverifiedPiece>,
    )>,
    pub result: Option<VerificationResult>,
}

impl MachineSnapshot {
    pub(crate) fn from_pm(
        pm: &dyn MachineCheckpointable,
        result: Option<&VerificationResult>,
    ) -> Self {
        Self {
            piece_phases: pm.piece_phases(),
            formula_phases: pm.formula_phases(),
            outcomes: pm.outcome_list(),
            result: result.cloned(),
        }
    }

    pub(crate) fn apply_to(&self, pm: &mut dyn MachineCheckpointable) -> anyhow::Result<()> {
        pm.set_piece_phases(&self.piece_phases)?;
        pm.set_formula_phases(&self.formula_phases)?;
        for (function_id, closed, unverified) in &self.outcomes {
            pm.restore_outcome(function_id.clone(), closed.clone(), unverified.clone());
        }
        Ok(())
    }
}

/// Run the machine under the checkpoint protocol: `snapshot_path` receives
/// atomic shape saves every `every` (from a snapshotter thread), the node
/// is refreshed after every state transition, and the final shape (with
/// the result) is saved on completion. Callers restore with
/// [`restore_snapshot`] + [`MachineCheckpointable::restore`] before
/// calling again to resume with zero re-invocation of completed pieces.
#[allow(clippy::missing_panics_doc)] // panics only if embedded runtime setup fails
pub fn run_checkpointed(
    project_path: &str,
    providers: &Providers<'_>,
    pm: &dyn MachineCheckpointable,
    snapshot_path: impl Into<std::path::PathBuf>,
    every: Duration,
) -> Result<VerificationResult> {
    let snapshot_path = snapshot_path.into();
    // The protocol is thread-based (safepoints park the mutator thread),
    // so this runner is blocking and drives the async machine on its own
    // current-thread runtime.
    with_checkpoint_pair(MachineSnapshot::default(), move |mut h, req| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(anyhow::Error::from)?;
        let result = std::thread::scope(|scope| -> Result<VerificationResult> {
            // Scoped snapshotter: borrows the requester, whose brand only
            // exists inside this pair — same shape as the states tests.
            let stop = std::sync::atomic::AtomicBool::new(false);
            let snap_path = snapshot_path.clone();
            let stop_handle = std::sync::Arc::new(stop);
            let stop_ref = Arc::clone(&stop_handle);
            let snapper =
                scope.spawn(move || {
                    servyi_states_snapshot::Snapshotter::new(req, &snap_path, every)
                        .run(&stop_ref)
                        .map_err(anyhow::Error::from)
                });

            let result: anyhow::Result<VerificationResult> = rt.block_on(async {
                let project = std::path::PathBuf::from(&project_path);
                let mut state: Box<dyn AlgorithmState> = Box::new(PreflightGit::new(project));
                loop {
                    // Parking only happens while a snapshot request is in
                    // flight (the snapshotter's try_save).
                    h.safepoint();
                    state = match state.execute(providers, pm).await? {
                        Step::State(next) => next,
                        Step::Result(result) => break Ok::<VerificationResult, anyhow::Error>(result),
                    };
                    // Refresh the pair's node so the next snapshotter
                    // round saves the post-transition state.
                    *h = pm.snapshot(None);
                }
            });
            let result = result?;

            stop_handle.store(true, std::sync::atomic::Ordering::SeqCst);
            let _snapshots = snapper
                .join()
                .map_err(|e| anyhow::anyhow!("snapshotter panicked: {e:?}"))??;
            Ok(result)
        })?;

        // Final save WITH the result — restore replays from this.
        *h = pm.snapshot(Some(&result));
        servyi_states_snapshot::save_handle(&h, &snapshot_path)?;
        info!("final checkpoint saved");
        Ok::<VerificationResult, anyhow::Error>(result)
    })
}

/// Deserialize a snapshot artifact written by [`run_checkpointed`].
pub fn restore_snapshot(path: &std::path::Path) -> Result<MachineSnapshot> {
    servyi_states_snapshot::restore_from(path).map_err(anyhow::Error::from)
}
