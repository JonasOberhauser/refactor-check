mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{FakeSolver, SequenceLlm};
use refactor_check::agent::{JUDGE_REASONABLE, run_with_providers};
use refactor_check::provider::SolverProvider;
use refactor_check::smt::{SolverOutcome, SolverResult};

fn smt_formula() -> String {
    "\
(set-logic QF_LIA)
(declare-fun x () Int)
(declare-fun y () Int)
(assert (= x y))
(check-sat)"
        .to_string()
}

fn formula_response() -> String {
    format!("Here is the formula:\n\n```smt2\n{}\n```", smt_formula())
}

fn two_formula_response() -> String {
    format!(
        "Formula for part A:\n\n```smt2\n{}\n```\n\nFormula for part B:\n\n```smt2\n(set-logic QF_LIA)\n(declare-fun z () Int)\n(declare-fun w () Int)\n(assert (= z w))\n(check-sat)\n```",
        smt_formula()
    )
}

#[test_log::test(tokio::test)]
async fn test_happy_path_unsat() {
    let llm = SequenceLlm::new(
        vec![formula_response()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = run_with_providers("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 1);
    assert!(result.formulas[0].formula.contains("(set-logic"));
    assert_eq!(result.formulas[0].outcome, SolverOutcome::Unsat);
    assert!(result.overall_equivalent);
    assert_eq!(result.reasonable_unsat, 1);
    assert_eq!(llm.formalizer_remaining(), 0);
    assert_eq!(llm.judge_remaining(), 0);
}

#[test_log::test(tokio::test)]
async fn test_formula_not_found_then_found() {
    let llm = SequenceLlm::new(
        vec![
            "I don't have a formula yet.".to_string(),
            formula_response(),
        ],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = run_with_providers("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 1);
    assert!(result.formulas[0].formula.contains("(set-logic"));
    assert_eq!(result.formulas[0].outcome, SolverOutcome::Unsat);
    assert!(result.overall_equivalent);
    assert_eq!(llm.formalizer_remaining(), 0);
    assert_eq!(llm.judge_remaining(), 0);
}

struct ToggleSolver {
    call_count: Arc<Mutex<usize>>,
    error_until: usize,
}

#[async_trait]
impl SolverProvider for ToggleSolver {
    async fn run(&self, _formula: &str) -> anyhow::Result<SolverResult> {
        let mut count = self.call_count.lock().expect("lock poisoned");
        *count += 1;
        let n = *count;
        if n <= self.error_until {
            Ok(SolverResult {
                outcome: SolverOutcome::Error("parse error".to_string()),
                stdout: "parse error".to_string(),
                stderr: "error".to_string(),
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

/// Generates a unique formula with distinct variable names to avoid accidental matching.
fn unique_formula_response(i: usize) -> String {
    format!(
        "Formula {}:\n\n```smt2\n(set-logic QF_LIA)\n(declare-fun v{i} () Int)\n(declare-fun u{i} () Int)\n(assert (= v{i} u{i}))\n(check-sat)\n```",
        i + 1
    )
}

#[test_log::test(tokio::test)]
async fn test_solver_error_in_batch_then_partial_result() {
    // In the compositional flow, an errored formula stays open until explicitly replaced.
    // With fixed LLM responses, the agent never generates a replacement for the specific
    // errored item, so it reaches MAX_ITERATIONS with a partial result.
    let formalizer: Vec<String> = vec![two_formula_response()];
    let fixer: Vec<String> = (0..29).map(unique_formula_response).collect();
    let judge: Vec<String> = (0..30).map(|_| JUDGE_REASONABLE.to_string()).collect();
    let llm = SequenceLlm::new(formalizer, fixer, judge);

    let call_count = Arc::new(Mutex::new(0usize));
    let solver = ToggleSolver { call_count: call_count.clone(), error_until: 1 };

    // After MAX_ITERATIONS the agent returns Ok with a partial result
    let result = run_with_providers("refactoring desc", &llm, &solver)
        .await
        .expect("agent should return partial result after max iterations");

    // 1 errored formula never resolved, 30 others verified (1 in iter 1 + 29 rest)
    assert_eq!(result.open_count, 1);
    assert_eq!(result.formulas.len(), 30);
    assert_eq!(*call_count.lock().unwrap(), 31, "solver called 31 times (2 in iter 1 + 29 rest)");
}

#[test_log::test(tokio::test)]
async fn test_judge_retry_in_batch_then_partial_result() {
    // A RETRY verdict moves the formula to open items, where it persists until
    // explicitly replaced. With fixed LLM responses the agent never generates
    // a targeted replacement, so it reaches MAX_ITERATIONS with a partial result.
    let formalizer: Vec<String> = vec![two_formula_response()];
    let fixer: Vec<String> = (0..29).map(unique_formula_response).collect();
    // 2 judge calls in iteration 1 (RETRY + REASONABLE) + 29 more in iterations 2..30
    let judge: Vec<String> = std::iter::once("The formula does not capture the loop invariant properly".to_string())
        .chain((0..30).map(|_| JUDGE_REASONABLE.to_string()))
        .collect();
    let llm = SequenceLlm::new(formalizer, fixer, judge);
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = run_with_providers("refactoring desc", &llm, &solver)
        .await
        .expect("agent should return partial result after max iterations");

    assert_eq!(result.open_count, 1);
    assert_eq!(result.formulas.len(), 30);
    assert_eq!(result.reasonable_unsat, 30);
}

#[test_log::test(tokio::test)]
async fn test_judge_gives_unclear_answer_then_clear() {
    let llm = SequenceLlm::new(
        vec![formula_response()],
        vec![formula_response()],
        vec![
            "The formula doesn't capture the loop invariant correctly".to_string(),
            JUDGE_REASONABLE.to_string(),
        ],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = run_with_providers("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 1);
    assert_eq!(result.formulas[0].outcome, SolverOutcome::Unsat);
    assert!(result.overall_equivalent);
    assert_eq!(llm.formalizer_remaining(), 0);
    assert_eq!(llm.fixer_remaining(), 0);
    assert_eq!(llm.judge_remaining(), 0);
}

fn jemalloc_refactoring_formula() -> String {
    "\
(set-logic UF)

(declare-sort Emitter 0)

(declare-fun f_version (Emitter) Emitter)
(declare-fun f_config  (Emitter) Emitter)
(declare-fun f_system  (Emitter) Emitter)
(declare-fun f_opt     (Emitter) Emitter)
(declare-fun f_prof    (Emitter) Emitter)
(declare-fun f_arenas  (Emitter) Emitter)

(declare-fun h_config  (Emitter) Emitter)
(declare-fun h_system  (Emitter) Emitter)
(declare-fun h_opt     (Emitter) Emitter)
(declare-fun h_prof    (Emitter) Emitter)
(declare-fun h_arenas  (Emitter) Emitter)

(declare-fun emit (Emitter Int) Emitter)

(define-fun op_cfg_dict_begin       () Int 100)
(define-fun op_cfg_cache_oblivious () Int 101)
(define-fun op_cfg_debug           () Int 102)
(define-fun op_cfg_fill            () Int 103)
(define-fun op_cfg_lazy_lock       () Int 104)
(define-fun op_cfg_malloc_conf     () Int 105)
(define-fun op_cfg_opt_safety      () Int 106)
(define-fun op_cfg_prof            () Int 107)
(define-fun op_cfg_prof_libgcc     () Int 108)
(define-fun op_cfg_prof_libunwind  () Int 109)
(define-fun op_cfg_prof_frameptr   () Int 110)
(define-fun op_cfg_stats           () Int 111)
(define-fun op_cfg_utrace          () Int 112)
(define-fun op_cfg_xmalloc         () Int 113)
(define-fun op_cfg_dict_end        () Int 114)

(define-fun op_sys_dict_begin () Int 200)
(define-fun op_sys_thp_mode   () Int 201)
(define-fun op_sys_dict_end   () Int 202)

(define-fun f_config_detail ((e Emitter)) Emitter
  (let ((e1  (emit e  op_cfg_dict_begin)))
  (let ((e2  (emit e1 op_cfg_cache_oblivious)))
  (let ((e3  (emit e2 op_cfg_debug)))
  (let ((e4  (emit e3 op_cfg_fill)))
  (let ((e5  (emit e4 op_cfg_lazy_lock)))
  (let ((e6  (emit e5 op_cfg_malloc_conf)))
  (let ((e7  (emit e6 op_cfg_opt_safety)))
  (let ((e8  (emit e7 op_cfg_prof)))
  (let ((e9  (emit e8 op_cfg_prof_libgcc)))
  (let ((e10 (emit e9 op_cfg_prof_libunwind)))
  (let ((e11 (emit e10 op_cfg_prof_frameptr)))
  (let ((e12 (emit e11 op_cfg_stats)))
  (let ((e13 (emit e12 op_cfg_utrace)))
  (let ((e14 (emit e13 op_cfg_xmalloc)))
  (let ((e15 (emit e14 op_cfg_dict_end)))
    e15)))))))))))))))

(define-fun h_config_detail ((e Emitter)) Emitter
  (let ((e1  (emit e  op_cfg_dict_begin)))
  (let ((e2  (emit e1 op_cfg_cache_oblivious)))
  (let ((e3  (emit e2 op_cfg_debug)))
  (let ((e4  (emit e3 op_cfg_fill)))
  (let ((e5  (emit e4 op_cfg_lazy_lock)))
  (let ((e6  (emit e5 op_cfg_malloc_conf)))
  (let ((e7  (emit e6 op_cfg_opt_safety)))
  (let ((e8  (emit e7 op_cfg_prof)))
  (let ((e9  (emit e8 op_cfg_prof_libgcc)))
  (let ((e10 (emit e9 op_cfg_prof_libunwind)))
  (let ((e11 (emit e10 op_cfg_prof_frameptr)))
  (let ((e12 (emit e11 op_cfg_stats)))
  (let ((e13 (emit e12 op_cfg_utrace)))
  (let ((e14 (emit e13 op_cfg_xmalloc)))
  (let ((e15 (emit e14 op_cfg_dict_end)))
    e15)))))))))))))))

(define-fun f_system_detail ((e Emitter)) Emitter
  (let ((e1 (emit e  op_sys_dict_begin)))
  (let ((e2 (emit e1 op_sys_thp_mode)))
  (let ((e3 (emit e2 op_sys_dict_end)))
    e3))))

(define-fun h_system_detail ((e Emitter)) Emitter
  (let ((e1 (emit e  op_sys_dict_begin)))
  (let ((e2 (emit e1 op_sys_thp_mode)))
  (let ((e3 (emit e2 op_sys_dict_end)))
    e3))))

(assert (forall ((e Emitter)) (= (h_config_detail e) (f_config_detail e))))
(assert (forall ((e Emitter)) (= (h_system_detail e) (f_system_detail e))))

(assert (forall ((e Emitter)) (= (f_config e) (f_config_detail e))))
(assert (forall ((e Emitter)) (= (h_config e) (h_config_detail e))))
(assert (forall ((e Emitter)) (= (f_system e) (f_system_detail e))))
(assert (forall ((e Emitter)) (= (h_system e) (h_system_detail e))))

(assert (forall ((e Emitter)) (= (h_opt e) (f_opt e))))
(assert (forall ((e Emitter)) (= (h_prof e) (f_prof e))))
(assert (forall ((e Emitter)) (= (h_arenas e) (f_arenas e))))

(define-fun before ((e Emitter)) Emitter
  (f_arenas (f_prof (f_opt (f_system (f_config (f_version e)))))))

(define-fun after ((e Emitter)) Emitter
  (h_arenas (h_prof (h_opt (h_system (h_config (f_version e)))))))

(assert (exists ((e Emitter)) (not (= (before e) (after e)))))

(check-sat)
(get-model)"
        .to_string()
}

fn jemalloc_formula_response() -> String {
    format!(
        "Looking at this refactoring, the \"before\" version is a monolithic \
         `stats_general_print` function that performs all operations inline, while the \
         \"after\" version decomposes it into helper functions called in the same order.\n\n\
          ```smt2\n{}\n```\n\n\
          **Expected result: UNSAT**",
        jemalloc_refactoring_formula()
    )
}

#[test_log::test(tokio::test)]
async fn test_jemalloc_refactoring_unsat() {
    let llm = SequenceLlm::new(
        vec![jemalloc_formula_response()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = run_with_providers(
        "stats_general_print refactoring: inline sections extracted to helper functions",
        &llm,
        &solver,
    )
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 1);
    assert!(result.formulas[0].formula.contains("(set-logic UF)"));
    assert!(result.formulas[0].formula.contains("(declare-sort Emitter 0)"));
    assert!(result.formulas[0].formula.contains("(check-sat)"));
    assert_eq!(result.formulas[0].outcome, SolverOutcome::Unsat);
    assert!(result.overall_equivalent);
    assert_eq!(llm.formalizer_remaining(), 0);
    assert_eq!(llm.judge_remaining(), 0);
}

#[test_log::test(tokio::test)]
async fn test_multi_formula_batch() {
    let llm = SequenceLlm::new(
        vec![two_formula_response()],
        vec![],
        vec![JUDGE_REASONABLE.to_string(), JUDGE_REASONABLE.to_string()],
    );
    let call_count = Arc::new(Mutex::new(0usize));
    let solver = ToggleSolver { call_count: call_count.clone(), error_until: 0 };

    let result = run_with_providers("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.formulas.len(), 2);
    assert_eq!(*call_count.lock().unwrap(), 2, "both formulas should be solved in parallel");
    assert!(result.overall_equivalent);
    assert_eq!(result.reasonable_unsat, 2);
    assert_eq!(llm.formalizer_remaining(), 0);
    assert_eq!(llm.judge_remaining(), 0);
}
