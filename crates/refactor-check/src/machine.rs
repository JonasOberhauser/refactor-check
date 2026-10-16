use anyhow::Result;
use std::sync::Arc;
use tracing::info;

use refactor_check_core::checkpoint::{drive, fanout, try_fanout, Handle};

use crate::behaviors::{explain, generation, results};
use crate::piece_manager::PieceManager;
use crate::result::AgentResult;
use crate::provider::{DynLlmProvider, DynSolverProvider};
use crate::states::*;

pub fn run(
    input_content: &str,
    llm: &DynLlmProvider,
    solver: &DynSolverProvider,
    pm: &dyn PieceManager,
) -> Result<AgentResult> {
    let input_arc = Arc::new(input_content.to_string());
    let start = Stage::Split(WaitForSplit {
        input_content: Arc::clone(&input_arc),
        pieces_to_resplit: Vec::new(),
        verified: Vec::new(),
        open: Vec::new(),
        iteration: 0,
        split_depth: 0,
        judge_feedback: None,
    });

    drive(start, |h| fan_out(h, llm, solver, pm), |stage| stage.execute(llm, solver, pm))
}

/// Runs the parallel part of a fan-out stage: every child works on its own node,
/// on its own scoped thread, and stores its result there.
fn fan_out(
    h: &mut Handle<'_, Option<Stage>>,
    llm: &DynLlmProvider,
    solver: &DynSolverProvider,
    pm: &dyn PieceManager,
) -> Result<()> {
    match h.as_ref() {
        Some(Stage::Generation(state)) if matches!(state.insist, InsistState::Idle) => {
            let job = generation::GenerationJob::new(state);
            try_fanout(
                h,
                &|stage| match stage {
                    Some(Stage::Generation(state)) => state.pieces.iter_mut().collect(),
                    _ => Vec::new(),
                },
                |slot, _| job.run(slot, llm, pm),
            )
        }
        Some(Stage::Results(_)) => try_fanout(
            h,
            &|stage| match stage {
                Some(Stage::Results(state)) => state.branches.iter_mut().collect(),
                _ => Vec::new(),
            },
            |branch, cancellation| results::run_branch(branch, llm, solver, pm, cancellation),
        ),
        Some(Stage::Explanation(state)) => {
            let input_content = Arc::clone(&state.input_content);
            if !state.explanations.is_empty() {
                info!(count = state.explanations.len(), "generating bug explanations");
            }
            fanout(
                h,
                &|stage| match stage {
                    Some(Stage::Explanation(state)) => state.explanations.iter_mut().collect(),
                    _ => Vec::new(),
                },
                |slot| explain::explain(slot, &input_content, llm),
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece_manager::DefaultPieceManager;
    use crate::result::FormulaResult;
    use crate::smt::SolverOutcome;
    use refactor_check_core::checkpoint::with_checkpoint_pair;
    use refactor_check_core::context_id::ContextId;
    use refactor_check_core::provider::IOProvider;

    /// Any IO means a finished child ran again.
    struct NoIo;

    impl<I, O> IOProvider<I, O> for NoIo {
        fn invoke(&self, _input: I) -> anyhow::Result<O> {
            anyhow::bail!("a finished child did IO")
        }
    }

    const FORMULA: &str = "(check-sat)";

    fn input() -> Arc<String> {
        Arc::new(String::new())
    }

    #[test]
    fn restored_results_stage_completes_without_io() {
        let pm = DefaultPieceManager::new();
        let branches = ["a", "b"]
            .iter()
            .map(|label| {
                let piece = Arc::new(pm.new_piece(ContextId::root(), label, "x", "x"));
                FormulaBranch {
                    piece: Arc::clone(&piece),
                    input_content: input(),
                    formula: FORMULA.to_string(),
                    phase: BranchPhase::WaitForSolver { formula: FORMULA.to_string() },
                    retry_count: 0,
                    done: Some(ChildDone::Verified(VerifiedPiece {
                        piece,
                        formula: FORMULA.to_string(),
                        outcome: SolverOutcome::Unsat,
                    })),
                }
            })
            .collect();
        let start = Stage::Results(WaitForResults {
            input_content: input(),
            verified: Vec::new(),
            open: Vec::new(),
            iteration: 0,
            branches,
        });

        let result = drive(start, |h| fan_out(h, &NoIo, &NoIo, &pm), |stage| stage.execute(&NoIo, &NoIo, &pm))
            .expect("finished branches need no IO");

        let labels: Vec<&str> = result.formulas.iter().map(|f| f.piece_label.as_str()).collect();
        assert_eq!(labels, ["a", "b"]);
        assert!(result.overall_equivalent);
    }

    #[test]
    fn restored_generation_fan_out_does_no_io() {
        let pm = DefaultPieceManager::new();
        let piece = Arc::new(pm.new_piece(ContextId::root(), "a", "x", "x"));
        let generation = Stage::Generation(WaitForGeneration {
            input_content: input(),
            verified: Vec::new(),
            open: Vec::new(),
            iteration: 0,
            insist: InsistState::Idle,
            pieces: vec![GenerationSlot { piece, formula: Some(FORMULA.to_string()) }],
        });

        with_checkpoint_pair(Some(generation), |mut h, _requester| fan_out(&mut h, &NoIo, &NoIo, &pm))
            .expect("finished children need no IO");
    }

    #[test]
    fn restored_explanations_land_on_their_formulas() {
        let pm = DefaultPieceManager::new();
        let formula = |label: &str, outcome| FormulaResult {
            formula: FORMULA.to_string(),
            piece_label: label.to_string(),
            outcome,
            verdict: String::new(),
            explanation: None,
        };
        let mut explanation = WaitForExplanation::new(
            input(),
            AgentResult {
                formulas: vec![formula("a", SolverOutcome::Unsat), formula("b", SolverOutcome::Sat)],
                overall_equivalent: false,
                open_count: 0,
                reasonable_sat: 1,
                reasonable_unsat: 1,
                reasonable_unknown: 0,
            },
        );
        for slot in &mut explanation.explanations {
            slot.explanation = Some(format!("explains formula {}", slot.formula_index));
        }

        let result = drive(
            Stage::Explanation(explanation),
            |h| fan_out(h, &NoIo, &NoIo, &pm),
            |stage| stage.execute(&NoIo, &NoIo, &pm),
        )
        .expect("finished explanations need no IO");

        let explanations: Vec<Option<&str>> = result.formulas.iter().map(|f| f.explanation.as_deref()).collect();
        assert_eq!(explanations, [None, Some("explains formula 1")]);
    }
}
