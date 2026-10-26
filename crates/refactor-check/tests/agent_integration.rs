use std::sync::{Arc, Mutex};


use refactor_check::consts::JUDGE_REASONABLE;
use refactor_check::machine;
use refactor_check::provider::{IOProvider, SolverRequest, WithContext};
use refactor_check::smt::{SolverOutcome, SolverResult};
use refactor_check_test_helpers::sequence::{FakeSolver, SequenceLlm};
use refactor_check_test_helpers::test_pm;

/// Description passed to every machine run.
const RUN_DESCRIPTION: &str = "refactoring desc";
/// Label of the single piece produced by a no-split response.
const LABEL_WHOLE: &str = "whole";
/// Fake solver stdout values.
const STDOUT_UNKNOWN: &str = "unknown";
const STDOUT_UNSAT: &str = "unsat";
/// Generator response that contains no formula (insist trigger).
const NO_FORMULA: &str = "no formula here";
/// Judge feedback that forces a fixer round.
const JUDGE_FEEDBACK_INVARIANT: &str = "formula does not capture the loop invariant";
/// Expected piece counts.
const ONE_FORMULA: usize = 1;
const TWO_FORMULAS: usize = 2;
const FOUR_FORMULAS: usize = 4;
/// First solver call in the timeout test.
const FIRST_CALL: usize = 1;
const NO_CALLS: usize = 0;
/// Index of the first formula in the results.
const FIRST_FORMULA: usize = 0;
/// Fixtures below are shared by name via consts so every response is a named choice.
const SMT_FORMULA_QF_LIA: &str = "
(set-logic QF_LIA)
(declare-fun x () Int)
(declare-fun y () Int)
(assert (= x y))
(check-sat)";
const SPLIT_NO_SPLIT_WHOLE: &str = "
Piece: whole
---- BEFORE ----
fn main() { x + 1 }
---- AFTER ----
fn main() { x + 1 }";
const SPLIT_TWO_PRELUDE_BODY: &str = "
Piece: prelude
---- BEFORE ----
let x = 1;
---- AFTER ----
let x = 1;

Piece: body
---- BEFORE ----
x + 1
---- AFTER ----
x + 1";
const SPLIT_FOUR_ABCD: &str = "
Piece: a
---- BEFORE ----
fn a() { 1 }
---- AFTER ----
fn a() { 1 }

Piece: b
---- BEFORE ----
fn b() { 2 }
---- AFTER ----
fn b() { 2 }

Piece: c
---- BEFORE ----
fn c() { 3 }
---- AFTER ----
fn c() { 3 }

Piece: d
---- BEFORE ----
fn d() { 4 }
---- AFTER ----
fn d() { 4 }";

fn smt_formula_single() -> String {
    SMT_FORMULA_QF_LIA.to_string()
}

fn formula_response_single() -> String {
    format!("Here is the formula:\n\n```smt2\n{}\n```", smt_formula_single())
}

const FORMULA_RESPONSE_PIECE_1: &str = "Piece 1:\n```smt2\n(set-logic QF_LIA)\n(declare-fun x () Int)\n(declare-fun y () Int)\n(assert (= x y))\n(check-sat)\n```";
const FORMULA_RESPONSE_PIECE_2: &str = "Piece 2:\n```smt2\n(set-logic QF_LIA)\n(declare-fun a () Int)\n(declare-fun b () Int)\n(assert (= a b))\n(check-sat)\n```";
const FORMULA_RESPONSE_PIECE_3: &str = "Piece 3:\n```smt2\n(set-logic QF_LIA)\n(declare-fun c () Int)\n(declare-fun d () Int)\n(assert (= c d))\n(check-sat)\n```";
const FORMULA_RESPONSE_PIECE_4: &str = "Piece 4:\n```smt2\n(set-logic QF_LIA)\n(declare-fun e () Int)\n(declare-fun f () Int)\n(assert (= e f))\n(check-sat)\n```";
const SPLIT_BIG_TIMEOUT: &str = "
Piece: big
---- BEFORE ----
large code
---- AFTER ----
large code";

fn split_no_split() -> String {
    SPLIT_NO_SPLIT_WHOLE.to_string()
}

fn split_two_pieces() -> String {
    SPLIT_TWO_PRELUDE_BODY.to_string()
}

fn split_four_pieces() -> String {
    SPLIT_FOUR_ABCD.to_string()
}

/// No-split: splitter says one piece, happy path.
#[test_log::test]
fn test_split_no_split_happy_path() {
    let llm = SequenceLlm::with_accepting_judge(
        vec![formula_response_single()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
        vec![split_no_split()],
    );
    let solver = FakeSolver {
        outcome: SolverOutcome::Unsat,
    };

    let pm = test_pm();
    let result = machine::run(RUN_DESCRIPTION, &llm, &solver, &pm)
        
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), ONE_FORMULA);
    assert_eq!(result.formulas[FIRST_FORMULA].piece_label, LABEL_WHOLE);
    assert_eq!(result.reasonable_unsat, ONE_FORMULA);
    assert!(result.overall_equivalent);
}

/// Splitter gives 2 pieces, each generates one formula.
#[test_log::test]
fn test_split_two_pieces() {
    let llm = SequenceLlm::with_accepting_judge(
        vec![formula_response_single(), formula_response_single()],
        vec![],
        vec![
            JUDGE_REASONABLE.to_string(),
            JUDGE_REASONABLE.to_string(),
        ],
        vec![split_two_pieces()],
    );
    let solver = FakeSolver {
        outcome: SolverOutcome::Unsat,
    };

    let pm = test_pm();
    let result = machine::run(RUN_DESCRIPTION, &llm, &solver, &pm)
        
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), TWO_FORMULAS);
    assert_eq!(result.reasonable_unsat, TWO_FORMULAS);
    assert!(result.overall_equivalent);
}

/// Splitter gives 4 pieces, all verified.
#[test_log::test]
fn test_split_four_pieces() {
    let llm = SequenceLlm::with_accepting_judge(
        vec![
            FORMULA_RESPONSE_PIECE_1.to_string(),
            FORMULA_RESPONSE_PIECE_2.to_string(),
            FORMULA_RESPONSE_PIECE_3.to_string(),
            FORMULA_RESPONSE_PIECE_4.to_string(),
        ],
        vec![],
        vec![
            JUDGE_REASONABLE.to_string(),
            JUDGE_REASONABLE.to_string(),
            JUDGE_REASONABLE.to_string(),
            JUDGE_REASONABLE.to_string(),
        ],
        vec![split_four_pieces()],
    );
    let solver = FakeSolver {
        outcome: SolverOutcome::Unsat,
    };

    let pm = test_pm();
    let result = machine::run(RUN_DESCRIPTION, &llm, &solver, &pm)
        
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), FOUR_FORMULAS);
    assert_eq!(result.reasonable_unsat, FOUR_FORMULAS);
    assert!(result.overall_equivalent);
}

/// One piece times out → splitter asked to resplit → children verified with UNSAT.
#[test_log::test]
fn test_split_timeout_resplit() {
    struct ToggleSolver {
        call_count: Arc<Mutex<usize>>,
    }
        impl IOProvider<SolverRequest, WithContext<SolverResult>> for ToggleSolver {
        fn invoke(&self, input: SolverRequest) -> anyhow::Result<WithContext<SolverResult>> {
            let SolverRequest { context_id, .. } = input;
            let mut count = self.call_count.lock().expect("lock poisoned");
            *count += 1;
            let value = if *count == FIRST_CALL {
                SolverResult {
                    outcome: SolverOutcome::Unknown,
                    stdout: STDOUT_UNKNOWN.to_string(),
                    stderr: String::new(),
                }
            } else {
                SolverResult {
                    outcome: SolverOutcome::Unsat,
                    stdout: STDOUT_UNSAT.to_string(),
                    stderr: String::new(),
                }
            };
            Ok(WithContext { value, context_id })
        }
    }

    let llm = SequenceLlm::with_accepting_judge(
        vec![
            formula_response_single(),
            formula_response_single(),
            formula_response_single(),
        ],
        vec![],
        vec![
            JUDGE_REASONABLE.to_string(),
            JUDGE_REASONABLE.to_string(),
        ],
        vec![
            SPLIT_BIG_TIMEOUT.to_string(),
            split_two_pieces(),
        ],
    );
    let solver = ToggleSolver {
        call_count: Arc::new(Mutex::new(NO_CALLS)),
    };

    let pm = test_pm();
    let result = machine::run(RUN_DESCRIPTION, &llm, &solver, &pm)
        
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), TWO_FORMULAS);
    assert_eq!(result.reasonable_unsat, TWO_FORMULAS);
    assert!(result.overall_equivalent);
}

/// SAT result should be detected and not-equivalent reported.
#[test_log::test]
fn test_split_sat_result() {
    let llm = SequenceLlm::with_accepting_judge(
        vec![formula_response_single()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
        vec![split_no_split()],
    );
    let solver = FakeSolver {
        outcome: SolverOutcome::Sat,
    };

    let pm = test_pm();
    let result = machine::run(RUN_DESCRIPTION, &llm, &solver, &pm)
        
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), ONE_FORMULA);
    assert_eq!(result.reasonable_sat, ONE_FORMULA);
    assert!(!result.overall_equivalent);
}

/// Per-piece generator retries on empty response.
#[test_log::test]
fn test_split_generator_insist_then_ok() {
    let llm = SequenceLlm::with_accepting_judge(
        vec![NO_FORMULA.to_string(), formula_response_single()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
        vec![split_no_split()],
    );
    let solver = FakeSolver {
        outcome: SolverOutcome::Unsat,
    };

    let pm = test_pm();
    let result = machine::run(RUN_DESCRIPTION, &llm, &solver, &pm)
        
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), ONE_FORMULA);
    assert!(result.overall_equivalent);
}

/// Simple 1-piece verification with UNSAT.
#[test_log::test]
fn test_split_one_piece_unsat() {
    let llm = SequenceLlm::with_accepting_judge(
        vec![formula_response_single()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
        vec![split_no_split()],
    );
    let solver = FakeSolver {
        outcome: SolverOutcome::Unsat,
    };

    let pm = test_pm();
    let result = machine::run(RUN_DESCRIPTION, &llm, &solver, &pm)
        
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), ONE_FORMULA);
    assert!(result.overall_equivalent);
}

/// Judge says retry for one piece of a split → generator fixes → verified.
#[test_log::test]
fn test_split_one_piece_judge_retry_then_verified() {
    let llm = SequenceLlm::with_accepting_judge(
        vec![formula_response_single(), formula_response_single()],
        vec![formula_response_single()],
        vec![
            JUDGE_FEEDBACK_INVARIANT.to_string(),
            JUDGE_REASONABLE.to_string(),
            JUDGE_REASONABLE.to_string(),
        ],
        vec![split_two_pieces()],
    );
    let solver = FakeSolver {
        outcome: SolverOutcome::Unsat,
    };

    let pm = test_pm();
    let result = machine::run(RUN_DESCRIPTION, &llm, &solver, &pm)
        
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), TWO_FORMULAS);
    assert_eq!(result.reasonable_unsat, TWO_FORMULAS);
    assert!(result.overall_equivalent);
}

/// SAT piece with judge verifying it's a spurious SAT → overall not-equivalent.
#[test_log::test]
fn test_split_sat_piece_detected() {
    let llm = SequenceLlm::with_accepting_judge(
        vec![formula_response_single()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
        vec![split_no_split()],
    );
    let solver = FakeSolver {
        outcome: SolverOutcome::Sat,
    };

    let pm = test_pm();
    let result = machine::run(RUN_DESCRIPTION, &llm, &solver, &pm)
        
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), ONE_FORMULA);
    assert_eq!(result.reasonable_sat, ONE_FORMULA);
    assert!(!result.overall_equivalent);
}