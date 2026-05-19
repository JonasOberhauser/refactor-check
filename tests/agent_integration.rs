mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{FakeSolver, SequenceLlm};
use refactor_check::consts::JUDGE_REASONABLE;
use refactor_check::machine;
use refactor_check::provider::SolverProvider;
use refactor_check::smt::{SolverOutcome, SolverResult};

fn smt_formula_single() -> String {
    "\
(set-logic QF_LIA)
(declare-fun x () Int)
(declare-fun y () Int)
(assert (= x y))
(check-sat)"
        .to_string()
}

fn formula_response_single() -> String {
    format!("Here is the formula:\n\n```smt2\n{}\n```", smt_formula_single())
}

fn formula_response_two() -> String {
    format!(
        "Piece 1:\n\n```smt2\n{}\n```\n\n\
         Piece 2:\n\n```smt2\n(set-logic QF_LIA)\n(declare-fun z () Int)\n(declare-fun w () Int)\n(assert (= z w))\n(check-sat)\n```",
        smt_formula_single()
    )
}

fn split_no_split() -> String {
    "\
Piece: whole
---- BEFORE ----
fn main() { x + 1 }
---- AFTER ----
fn main() { x + 1 }"
        .to_string()
}

fn split_two_pieces() -> String {
    "\
Piece: prelude
---- BEFORE ----
let x = 1;
---- AFTER ----
let x = 1;

Piece: body
---- BEFORE ----
x + 1
---- AFTER ----
x + 1"
        .to_string()
}

fn split_four_pieces() -> String {
    "\
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
fn d() { 4 }"
        .to_string()
}

/// No-split: splitter says one piece, happy path.
#[test_log::test(tokio::test)]
async fn test_split_no_split_happy_path() {
    let llm = SequenceLlm::new(
        vec![formula_response_single()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
        vec![split_no_split()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = machine::run("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 1);
    assert_eq!(result.formulas[0].piece_label, "whole");
    assert_eq!(result.reasonable_unsat, 1);
    assert!(result.overall_equivalent);
}

/// Splitter gives 2 pieces, each generates one formula.
#[test_log::test(tokio::test)]
async fn test_split_two_pieces() {
    let llm = SequenceLlm::new(
        vec![formula_response_two()],
        vec![],
        vec![
            JUDGE_REASONABLE.to_string(),
            JUDGE_REASONABLE.to_string(),
        ],
        vec![split_two_pieces()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = machine::run("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 2);
    assert_eq!(result.reasonable_unsat, 2);
    assert!(result.overall_equivalent);
}

/// Splitter gives 4 pieces, all verified.
#[test_log::test(tokio::test)]
async fn test_split_four_pieces() {
    let llm = SequenceLlm::new(
        vec![
            "\
Piece 1:\n```smt2\n(set-logic QF_LIA)\n(declare-fun x () Int)\n(declare-fun y () Int)\n(assert (= x y))\n(check-sat)\n```\n\n\
Piece 2:\n```smt2\n(set-logic QF_LIA)\n(declare-fun a () Int)\n(declare-fun b () Int)\n(assert (= a b))\n(check-sat)\n```\n\n\
Piece 3:\n```smt2\n(set-logic QF_LIA)\n(declare-fun c () Int)\n(declare-fun d () Int)\n(assert (= c d))\n(check-sat)\n```\n\n\
Piece 4:\n```smt2\n(set-logic QF_LIA)\n(declare-fun e () Int)\n(declare-fun f () Int)\n(assert (= e f))\n(check-sat)\n```"
                .to_string(),
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
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = machine::run("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 4);
    assert_eq!(result.reasonable_unsat, 4);
    assert!(result.overall_equivalent);
}

/// One piece times out → splitter asked to resplit → children verified with UNSAT.
#[test_log::test(tokio::test)]
async fn test_split_timeout_resplit() {
    struct ToggleSolver {
        call_count: Arc<Mutex<usize>>,
    }
    #[async_trait]
    impl SolverProvider for ToggleSolver {
        async fn run(&self, _formula: &str) -> anyhow::Result<SolverResult> {
            let mut count = self.call_count.lock().expect("lock poisoned");
            *count += 1;
            if *count == 1 {
                Ok(SolverResult {
                    outcome: SolverOutcome::Unknown,
                    stdout: "unknown".to_string(),
                    stderr: String::new(),
                })
            } else {
                Ok(SolverResult {
                    outcome: SolverOutcome::Unsat,
                    stdout: "unsat".to_string(),
                    stderr: String::new(),
                })
            }
        }
    }

    let llm = SequenceLlm::new(
        vec![
            formula_response_single(),
            formula_response_two(),
        ],
        vec![],
        vec![
            JUDGE_REASONABLE.to_string(),
            JUDGE_REASONABLE.to_string(),
        ],
        vec![
            "\
Piece: big
---- BEFORE ----
large code
---- AFTER ----
large code"
                .to_string(),
            split_two_pieces(),
        ],
    );
    let solver = ToggleSolver { call_count: Arc::new(Mutex::new(0)) };

    let result = machine::run("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 2);
    assert_eq!(result.reasonable_unsat, 2);
    assert!(result.overall_equivalent);
}

/// SAT result should be detected and not-equivalent reported.
#[test_log::test(tokio::test)]
async fn test_split_sat_result() {
    let llm = SequenceLlm::new(
        vec![formula_response_single()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
        vec![split_no_split()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Sat };

    let result = machine::run("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 1);
    assert_eq!(result.reasonable_sat, 1);
    assert!(!result.overall_equivalent);
}

/// Old happy path without splitter (empty splitter queue).
#[test_log::test(tokio::test)]
async fn test_old_happy_path_unsat() {
    let llm = SequenceLlm::new(
        vec![formula_response_single()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
        vec!["".to_string()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = machine::run("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 1);
    assert!(result.overall_equivalent);
}

/// Generator insist then succeeds on split pieces.
#[test_log::test(tokio::test)]
async fn test_split_generator_insist_then_ok() {
    let llm = SequenceLlm::new(
        vec![
            "no formula here".to_string(),
            formula_response_single(),
        ],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
        vec![split_no_split()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = machine::run("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 1);
    assert!(result.overall_equivalent);
}

/// Splitter returns empty → falls back to one-piece generation without split.
#[test_log::test(tokio::test)]
async fn test_splitter_returns_empty_falls_back() {
    let llm = SequenceLlm::new(
        vec![formula_response_single()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
        vec!["".to_string()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = machine::run("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 1);
    assert!(result.overall_equivalent);
}

/// Judge says retry for one piece of a split → generator fixes → verified.
#[test_log::test(tokio::test)]
async fn test_split_one_piece_judge_retry_then_verified() {
    let llm = SequenceLlm::new(
        vec![formula_response_two()],
        vec![formula_response_single()],
        vec![
            "formula does not capture the loop invariant".to_string(),
            JUDGE_REASONABLE.to_string(),
            JUDGE_REASONABLE.to_string(),
        ],
        vec![split_two_pieces()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = machine::run("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 2);
    assert_eq!(result.reasonable_unsat, 2);
    assert!(result.overall_equivalent);
}

/// Old multi-formula batch without splitter.
#[test_log::test(tokio::test)]
async fn test_old_multi_formula_batch() {
    let llm = SequenceLlm::new(
        vec![formula_response_two()],
        vec![],
        vec![
            JUDGE_REASONABLE.to_string(),
            JUDGE_REASONABLE.to_string(),
        ],
        vec!["".to_string()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = machine::run("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 2);
    assert!(result.overall_equivalent);
}