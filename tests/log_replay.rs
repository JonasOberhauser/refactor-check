mod common;

use common::{LogReplayLlm, LogReplaySolver, test_pm};
use refactor_check::machine;
use refactor_check::smt::SolverOutcome;

fn smt_formula_response() -> String {
    format!(
        "Here is the formula:\n```smt2\n{}\n```",
        "\
(set-logic QF_LIA)
(declare-fun x () Int)
(declare-fun y () Int)
(assert (= x y))
(check-sat)"
    )
}

/// Simple log replay: one piece, formalizer → solver UNSAT → judge REASONABLE.
#[test_log::test(tokio::test)]
async fn test_replay_simple_happy() {
    let mut llm = LogReplayLlm::new();
    llm.splitter_push(
        "Piece: whole\n---- BEFORE ----\na\n---- AFTER ----\na\n".to_string(),
    );
    llm.formalizer_push(1, smt_formula_response());
    llm.judge_push(1, "REASONABLE".to_string());

    let mut solver = LogReplaySolver::new();
    solver.push(1, SolverOutcome::Unsat, "unsat".to_string(), String::new());

    let pm = test_pm();
    let result = machine::run("test", &llm, &solver, &pm)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 1);
    assert_eq!(result.reasonable_unsat, 1);
    assert!(result.overall_equivalent);
}

/// Replay: solver Error at iteration=0 → piece becomes Open → solver Unknown at
/// iteration=1 → resplit → fresh pieces created at non-zero iteration.
/// Regression: absent pieces used to panic with "expected phase Open but was absent"
/// before `enter_generation` was introduced.
#[test_log::test(tokio::test)]
async fn test_replay_resplit_at_iteration_one() {
    let mut llm = LogReplayLlm::new();

    // Initial split: 1 piece
    llm.splitter_push(
        "Piece: big\n---- BEFORE ----\nbig before\n---- AFTER ----\nbig after\n".to_string(),
    );
    // Formalizer for piece 1 at iteration 0
    llm.formalizer_push(1, smt_formula_response());

    // Piece 1 re-enters generation at iteration 1 (Open piece → Fixer)
    llm.fixer_push(1, smt_formula_response());

    // Resplit: splitter produces 2 sub-pieces
    llm.splitter_push(
        "\
Piece: sub1
---- BEFORE ----
sub1 before
---- AFTER ----
sub1 after

Piece: sub2
---- BEFORE ----
sub2 before
---- AFTER ----
sub2 after"
            .to_string(),
    );
    // Resplit at iteration=1, so new pieces use Fixer role
    llm.fixer_push(2, smt_formula_response());
    llm.fixer_push(3, smt_formula_response());

    // Judge calls for the two resplit pieces
    llm.judge_push(2, "REASONABLE".to_string());
    llm.judge_push(3, "REASONABLE".to_string());

    // Solver: Error → Open, then Unknown → resplit, then UNSAT for new pieces
    let mut solver = LogReplaySolver::new();
    solver.push(
        1,
        SolverOutcome::Error("bad formula".into()),
        String::new(),
        String::new(),
    );
    solver.push(1, SolverOutcome::Unknown, "unknown".into(), String::new());
    solver.push(2, SolverOutcome::Unsat, "unsat".into(), String::new());
    solver.push(3, SolverOutcome::Unsat, "unsat".into(), String::new());

    let pm = test_pm();
    let result = machine::run("test", &llm, &solver, &pm)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 2);
    assert_eq!(result.reasonable_unsat, 2);
    assert!(result.overall_equivalent);
}