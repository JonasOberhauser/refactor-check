use anyhow::Result;
use servyi_states::checkpoint::with_checkpoint_pair;
use tracing::info;

use crate::piece_manager::DeductivePieceManager;
use crate::provider::Providers;
use crate::result::VerificationResult;
use crate::states::{AlgorithmState, PreflightGit, Step};

/// The machine: one checkpoint pair for the whole run; its node is the
/// current state.
type StateNode = Box<dyn AlgorithmState>;

pub fn run(
    project_path: &str,
    providers: &Providers<'_>,
    pm: &dyn DeductivePieceManager,
) -> Result<VerificationResult> {
    let state: StateNode = Box::new(PreflightGit::new(std::path::PathBuf::from(project_path)));
    with_checkpoint_pair(state, |mut handle, _requester| {
        loop {
            handle.safepoint();
            // The state transitions in place through the node's handle; its
            // successor is installed with *handle = Box::new(..).
            match (*handle).execute(providers, pm)? {
                Step::State(next) => *handle = next,
                Step::Result(result) => {
                    info!(
                        total_pieces = result.total_pieces,
                        closed = result.closed_pieces.len(),
                        unverified = result.unverified_pieces.len(),
                        bugs = result.bug_reports.len(),
                        "Verification complete"
                    );
                    return Ok(result);
                }
            }
        }
    })
}
