use refactor_check::machine;
use refactor_check::smt::SolverOutcome;
use refactor_check_test_helpers::replay::{LogReplayLlm, LogReplaySolver};
use refactor_check_test_helpers::test_pm;

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
    llm.splitting_judge_push("REASONABLE".to_string());
    llm.formalizer_push(smt_formula_response());
    llm.judge_push("REASONABLE".to_string());

    let mut solver = LogReplaySolver::new();
    solver.push(SolverOutcome::Unsat, "unsat".to_string(), String::new());

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
    llm.splitting_judge_push("REASONABLE".to_string());
    // Formalizer for piece 1 at iteration 0
    llm.formalizer_push(smt_formula_response());

    // Piece 1 re-enters generation at iteration 1 (Open piece → Fixer)
    llm.fixer_push(smt_formula_response());

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
    llm.splitting_judge_push("REASONABLE".to_string());
    // Resplit at iteration=1, so new pieces use Fixer role
    llm.fixer_push(smt_formula_response());
    llm.fixer_push(smt_formula_response());

    // Judge calls for the two resplit pieces
    llm.judge_push("REASONABLE".to_string());
    llm.judge_push("REASONABLE".to_string());

    // Solver: Error → Open, then Unknown → resplit, then UNSAT for new pieces
    let mut solver = LogReplaySolver::new();
    solver.push(
        SolverOutcome::Error("bad formula".into()),
        String::new(),
        String::new(),
    );
    solver.push(SolverOutcome::Unknown, "unknown".into(), String::new());
    solver.push(SolverOutcome::Unsat, "unsat".into(), String::new());
    solver.push(SolverOutcome::Unsat, "unsat".into(), String::new());

    let pm = test_pm();
    let result = machine::run("test", &llm, &solver, &pm)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 2);
    assert_eq!(result.reasonable_unsat, 2);
    assert!(result.overall_equivalent);
}

/// Splitting judge rejects splits until depth exhausted → pieces become open
/// and are re-processed without phase mismatch panic.
#[test_log::test(tokio::test)]
async fn test_replay_split_depth_exhausted_via_judge() {
    let mut llm = LogReplayLlm::new();

    // Split 1 (depth 0): accepted
    llm.splitter_push(
        "Piece: big\n---- BEFORE ----\nbig before\n---- AFTER ----\nbig after\n".to_string(),
    );
    llm.splitting_judge_push("REASONABLE".to_string());
    llm.formalizer_push(smt_formula_response());

    // Solver Unknown → resplit
    // Split 2 (depth 1): rejected by judge
    llm.splitter_push(
        "Piece: sub1\n---- BEFORE ----\nsub1 before\n---- AFTER ----\nsub1 after\n".to_string(),
    );
    llm.splitting_judge_push("pieces too large, split further".to_string());

    // Split 3 (depth 2): rejected by judge
    llm.splitter_push(
        "Piece: sub2\n---- BEFORE ----\nsub2 before\n---- AFTER ----\nsub2 after\n".to_string(),
    );
    llm.splitting_judge_push("still too large, split further".to_string());

    // Split 4 (depth 3): rejected → depth exhausted → piece becomes open
    llm.splitter_push(
        "Piece: sub3\n---- BEFORE ----\nsub3 before\n---- AFTER ----\nsub3 after\n".to_string(),
    );
    llm.splitting_judge_push("still too large, split further".to_string());

    // Open piece (id=4, "sub3") re-enters generation at iteration 0 → Formalizer, then Judge
    llm.formalizer_push(smt_formula_response());
    llm.judge_push("REASONABLE".to_string());

    let mut solver = LogReplaySolver::new();
    solver.push(SolverOutcome::Unknown, "unknown".into(), String::new());
    solver.push(SolverOutcome::Unsat, "unsat".into(), String::new());

    let pm = test_pm();
    let result = machine::run("test", &llm, &solver, &pm)
        .await
        .expect("agent should succeed despite split depth exhaustion");

    assert_eq!(result.reasonable_unsat, 1);
    assert!(result.overall_equivalent);
}
