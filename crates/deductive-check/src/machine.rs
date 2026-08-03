use anyhow::Result;
use tracing::info;

use crate::piece_manager::DeductivePieceManager;
use crate::provider::Providers;
use crate::result::VerificationResult;
use crate::states::{AlgorithmState, PreflightGit, Step};

pub async fn run(
    project_path: &str,
    providers: &Providers<'_>,
    pm: &dyn DeductivePieceManager,
) -> Result<VerificationResult> {
    let project_path = std::path::PathBuf::from(project_path);
    let mut state: Box<dyn AlgorithmState> = Box::new(PreflightGit::new(project_path));

    loop {
        state = match state.execute(providers, pm).await? {
            Step::State(next) => {
                info!("State transition complete");
                next
            }
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
        };
    }
}