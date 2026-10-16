use anyhow::Result;
use refactor_check_core::checkpoint::drive;
use tracing::info;

use crate::piece_manager::DeductivePieceManager;
use crate::provider::Providers;
use crate::result::VerificationResult;
use crate::states::{fan_out, AlgorithmState, PreflightGit, Stage, Step};

pub fn run(
    project_path: &str,
    providers: &Providers<'_>,
    pm: &dyn DeductivePieceManager,
) -> Result<VerificationResult> {
    let project_path = std::path::PathBuf::from(project_path);
    drive(
        Stage::PreflightGit(PreflightGit::new(project_path)),
        |h| fan_out(h, providers, pm),
        |stage| {
            let step = stage.execute(providers, pm)?;
            match &step {
                Step::State(_) => info!("State transition complete"),
                Step::Result(result) => info!(
                    total_pieces = result.total_pieces,
                    closed = result.closed_pieces.len(),
                    unverified = result.unverified_pieces.len(),
                    bugs = result.bug_reports.len(),
                    "Verification complete"
                ),
            }
            Ok(step)
        },
    )
}
