use anyhow::Result;
use refactor_check_core::context_id::ContextId;
use servyi_states::checkpoint::{Handle, with_checkpoint_pair};
use tracing::info;

use crate::piece_manager::DeductivePieceManager;
use crate::provider::Providers;
use crate::result::VerificationResult;
use crate::states::{
    AlgorithmState, PieceSlot, PreflightGit, SplitSlot, Step, process_piece, split_function,
};

/// The machine: one checkpoint pair for the whole run; its node is the
/// current state.
type StateNode = Box<dyn AlgorithmState>;

/// One transition. The current state runs as the node's single fan-out
/// child — dispatched and accessed through `Handle<Box<dyn AlgorithmState>>` —
/// and its declared work is fanned out off that same handle, so every child
/// splits directly off the node. The successor is installed with
/// `*handle = Box::new(..)`.
fn step(handle: &mut Handle<'_, StateNode>, providers: &Providers<'_>, pm: &dyn DeductivePieceManager) -> Result<Step> {
    handle.safepoint();
    // The action runs exactly once (single child); the Mutex is only
    // there because fan-out actions must be Fn + Send + Sync.
    let outcome = std::sync::Mutex::new(None::<Result<Step>>);
    handle.fanout(
        &|node: &mut StateNode| vec![&mut *node],
        |mut child: Handle<'_, StateNode>| {
            child.safepoint();
            // The state's fan-outs, split directly off the child (the node):
            child.fanout(
                &|node| {
                    node.piece_work()
                        .map(|work| work.iter_mut().collect::<Vec<_>>())
                        .unwrap_or_default()
                },
                |mut piece: Handle<'_, PieceSlot>| {
                    piece.safepoint();
                    piece.outcome = Some(process_piece(&mut piece, providers, pm));
                    piece.safepoint();
                },
            );
            child.fanout(
                &|node| {
                    node.split_work()
                        .map(|work| work.iter_mut().collect::<Vec<_>>())
                        .unwrap_or_default()
                },
                |mut split: Handle<'_, SplitSlot>| {
                    split.safepoint();
                    split.outcome =
                        Some(split_function(&mut split, providers, pm, ContextId::root()));
                    split.safepoint();
                },
            );
            child.safepoint();
            *outcome.lock().expect("outcome slot: pure option ops only") =
                Some((**child).execute(providers, pm));
        },
    );
    outcome
        .into_inner()
        .expect("outcome slot: pure option ops only")
        .expect("the single child always runs")
}

pub fn run(
    project_path: &str,
    providers: &Providers<'_>,
    pm: &dyn DeductivePieceManager,
) -> Result<VerificationResult> {
    let state: StateNode = Box::new(PreflightGit::new(std::path::PathBuf::from(project_path)));
    with_checkpoint_pair(state, |mut handle, _requester| {
        loop {
            handle.safepoint();
            match step(&mut handle, providers, pm)? {
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
