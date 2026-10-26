use refactor_check::machine;
use refactor_check::smt::SolverOutcome;
use refactor_check_test_helpers::replay::{LogReplayLlm, LogReplaySolver};
use refactor_check_test_helpers::test_pm;

/// Description passed to every machine run.
const RUN_DESC: &str = "test";
/// The judge verdict that accepts a response.
const VERDICT_REASONABLE: &str = "REASONABLE";
/// Fake solver stdout values.
const STDOUT_UNSAT: &str = "unsat";
const STDOUT_UNKNOWN: &str = "unknown";
/// Solver error payload for a bad formula.
const ERR_BAD_FORMULA: &str = "bad formula";
/// Splitting-judge rejections at increasing depth.
const JUDGE_REJECT_TOO_LARGE: &str = "pieces too large, split further";
const JUDGE_REJECT_STILL_LARGE: &str = "still too large, split further";
/// Splits.
const SPLIT_WHOLE_A: &str = "Piece: whole\n---- BEFORE ----\na\n---- AFTER ----\na\n";
const SPLIT_BIG: &str = "Piece: big\n---- BEFORE ----\nbig before\n---- AFTER ----\nbig after\n";
const SPLIT_SUB1_ONE_LINE: &str =
    "Piece: sub1\n---- BEFORE ----\nsub1 before\n---- AFTER ----\nsub1 after\n";
const SPLIT_SUB2_ONE_LINE: &str =
    "Piece: sub2\n---- BEFORE ----\nsub2 before\n---- AFTER ----\nsub2 after\n";
const SPLIT_SUB3_ONE_LINE: &str =
    "Piece: sub3\n---- BEFORE ----\nsub3 before\n---- AFTER ----\nsub3 after\n";
const SPLIT_SUB1_SUB2: &str = "
Piece: sub1
---- BEFORE ----
sub1 before
---- AFTER ----
sub1 after

Piece: sub2
---- BEFORE ----
sub2 before
---- AFTER ----
sub2 after";
/// Expected piece counts.
const ONE_FORMULA: usize = 1;
const TWO_FORMULAS: usize = 2;

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
#[test_log::test]
fn test_replay_simple_happy() {
    let mut llm = LogReplayLlm::new();
    llm.splitter_push(SPLIT_WHOLE_A.to_string());
    llm.splitting_judge_push(VERDICT_REASONABLE.to_string());
    llm.formalizer_push(smt_formula_response());
    llm.judge_push(VERDICT_REASONABLE.to_string());

    let mut solver = LogReplaySolver::new();
    solver.push(SolverOutcome::Unsat, STDOUT_UNSAT.to_string(), String::new());

    let pm = test_pm();
    let result = machine::run(RUN_DESC, &llm, &solver, &pm)
        
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), ONE_FORMULA);
    assert_eq!(result.reasonable_unsat, ONE_FORMULA);
    assert!(result.overall_equivalent);
}

/// Replay: solver Error at iteration=0 → piece becomes Open → solver Unknown at
/// iteration=1 → resplit → fresh pieces created at non-zero iteration.
/// Regression: absent pieces used to panic with "expected phase Open but was absent"
/// before `enter_generation` was introduced.
#[test_log::test]
fn test_replay_resplit_at_iteration_one() {
    let mut llm = LogReplayLlm::new();

    // Initial split: 1 piece
    llm.splitter_push(SPLIT_BIG.to_string());
    llm.splitting_judge_push(VERDICT_REASONABLE.to_string());
    // Formalizer for piece 1 at iteration 0
    llm.formalizer_push(smt_formula_response());

    // Piece 1 re-enters generation at iteration 1 (Open piece → Fixer)
    llm.fixer_push(smt_formula_response());

    // Resplit: splitter produces 2 sub-pieces
    llm.splitter_push(SPLIT_SUB1_SUB2.to_string());
    llm.splitting_judge_push(VERDICT_REASONABLE.to_string());
    // Resplit at iteration=1, so new pieces use Fixer role
    llm.fixer_push(smt_formula_response());
    llm.fixer_push(smt_formula_response());

    // Judge calls for the two resplit pieces
    llm.judge_push(VERDICT_REASONABLE.to_string());
    llm.judge_push(VERDICT_REASONABLE.to_string());

    // Solver: Error → Open, then Unknown → resplit, then UNSAT for new pieces
    let mut solver = LogReplaySolver::new();
    solver.push(
        SolverOutcome::Error(ERR_BAD_FORMULA.into()),
        String::new(),
        String::new(),
    );
    solver.push(SolverOutcome::Unknown, STDOUT_UNKNOWN.into(), String::new());
    solver.push(SolverOutcome::Unsat, STDOUT_UNSAT.into(), String::new());
    solver.push(SolverOutcome::Unsat, STDOUT_UNSAT.into(), String::new());

    let pm = test_pm();
    let result = machine::run(RUN_DESC, &llm, &solver, &pm)
        
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), TWO_FORMULAS);
    assert_eq!(result.reasonable_unsat, TWO_FORMULAS);
    assert!(result.overall_equivalent);
}

/// Splitting judge rejects splits until depth exhausted → pieces become open
/// and are re-processed without phase mismatch panic.
#[test_log::test]
fn test_replay_split_depth_exhausted_via_judge() {
    let mut llm = LogReplayLlm::new();

    // Split 1 (depth 0): accepted
    llm.splitter_push(SPLIT_BIG.to_string());
    llm.splitting_judge_push(VERDICT_REASONABLE.to_string());
    llm.formalizer_push(smt_formula_response());

    // Solver Unknown → resplit
    // Split 2 (depth 1): rejected by judge
    llm.splitter_push(SPLIT_SUB1_ONE_LINE.to_string());
    llm.splitting_judge_push(JUDGE_REJECT_TOO_LARGE.to_string());

    // Split 3 (depth 2): rejected by judge
    llm.splitter_push(SPLIT_SUB2_ONE_LINE.to_string());
    llm.splitting_judge_push(JUDGE_REJECT_STILL_LARGE.to_string());

    // Split 4 (depth 3): rejected → depth exhausted → piece becomes open
    llm.splitter_push(SPLIT_SUB3_ONE_LINE.to_string());
    llm.splitting_judge_push(JUDGE_REJECT_STILL_LARGE.to_string());

    // Open piece (id=4, "sub3") re-enters generation at iteration 0 → Formalizer, then Judge
    llm.formalizer_push(smt_formula_response());
    llm.judge_push(VERDICT_REASONABLE.to_string());

    let mut solver = LogReplaySolver::new();
    solver.push(SolverOutcome::Unknown, STDOUT_UNKNOWN.into(), String::new());
    solver.push(SolverOutcome::Unsat, STDOUT_UNSAT.into(), String::new());

    let pm = test_pm();
    let result = machine::run(RUN_DESC, &llm, &solver, &pm)
        
        .expect("agent should succeed despite split depth exhaustion");

    assert_eq!(result.reasonable_unsat, ONE_FORMULA);
    assert!(result.overall_equivalent);
}
