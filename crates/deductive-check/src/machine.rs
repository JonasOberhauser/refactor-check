use anyhow::Result;
use servyi_states::checkpoint::with_checkpoint_pair;
use servyi_states::context_id::ContextId;
use servyi_states::state::Context;
use tracing::info;

use crate::piece_manager::DeductivePieceManager;
use crate::provider::Providers;
use crate::result::VerificationResult;
use crate::states::{MachineEnv, PreflightGit};

pub fn run(
    project_path: &str,
    providers: &Providers<'_>,
    pm: &dyn DeductivePieceManager,
) -> Result<VerificationResult> {
    let env = MachineEnv { providers, pm };
    let machine = Context::new(ContextId::root(), Box::new(PreflightGit::new(
        std::path::PathBuf::from(project_path),
    )));
    let result = with_checkpoint_pair(machine, |mut handle, _requester| {
        handle.run_to_completion(&env)
    })?;
    info!(
        total_pieces = result.total_pieces,
        closed = result.closed_pieces.len(),
        unverified = result.unverified_pieces.len(),
        bugs = result.bug_reports.len(),
        "Verification complete"
    );
    Ok(result)
}
