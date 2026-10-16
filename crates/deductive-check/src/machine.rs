use anyhow::Result;
use servyi_states::checkpoint::with_checkpoint_pair;
use tracing::info;

use crate::piece_manager::DeductivePieceManager;
use crate::provider::Providers;
use crate::result::VerificationResult;
use crate::states::{Flow, MachineState, step};

pub fn run(
    project_path: &str,
    providers: &Providers<'_>,
    pm: &dyn DeductivePieceManager,
) -> Result<VerificationResult> {
    let state = MachineState::start(std::path::PathBuf::from(project_path));
    with_checkpoint_pair(state, |mut handle, _requester| {
        loop {
            match step(&mut handle, providers, pm)? {
                Flow::Continue => {}
                Flow::Done(result) => {
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
