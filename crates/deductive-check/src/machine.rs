use anyhow::Result;
use tracing::info;

use servyi_states::checkpoint::with_checkpoint_pair;
use servyi_states::state::Context;
use servyi_states::ContextId;

use crate::piece_manager::DeductivePieceManager;
use crate::provider::Providers;
use crate::result::VerificationResult;
use crate::states::{MachineEnv, PreflightGit};

/// The root task: the workload's ONE checkpoint pair around the
/// machine. Every state is a phase of the algorithm ([Preflight] …
/// [Splitter] … [Full Formalizer] … [Restarter]); the fan-out phases
/// contain their fan-out inside their `execute`, in this tree, so a
/// checkpoint request parks everything from the root and the
/// checkpoint serializes from the root.
pub fn run(
    project_path: &str,
    providers: &Providers<'_>,
    pm: &dyn DeductivePieceManager,
) -> Result<VerificationResult> {
    let env = MachineEnv { providers, pm };
    let machine = Context::new(
        ContextId::root(),
        Box::new(PreflightGit::new(std::path::PathBuf::from(project_path))),
    );
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
