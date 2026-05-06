mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{FakeSolver, SequenceLlm};
use refactor_check::agent::{JUDGE_REASONABLE, JUDGE_RETRY, run_with_providers};
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
        "One way:\n\n```smt2\n{0}\n```\n\nAnother way:\n\n```smt2\n{0}\n```",
        smt_formula()
    )
}

#[test_log::test(tokio::test)]
async fn test_happy_path_unsat() {
    let llm = SequenceLlm::new(
        vec![formula_response()],
        vec![JUDGE_REASONABLE.to_string()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = run_with_providers("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert!(result.formula.contains("(set-logic"));
    assert_eq!(result.solver_outcome, SolverOutcome::Unsat);
    assert!(result.solver_stdout.contains("unsat"));
    assert_eq!(llm.primary_remaining(), 0, "all primary responses consumed");
    assert_eq!(llm.judge_remaining(), 0, "all judge responses consumed");
}

#[test_log::test(tokio::test)]
async fn test_formula_not_found_then_found() {
    let llm = SequenceLlm::new(
        vec![
            "I don't have a formula yet.".to_string(),
            formula_response(),
        ],
        vec![JUDGE_REASONABLE.to_string()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = run_with_providers("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert!(result.formula.contains("(set-logic"));
    assert_eq!(result.solver_outcome, SolverOutcome::Unsat);
    assert_eq!(llm.primary_remaining(), 0, "all primary responses consumed");
    assert_eq!(llm.judge_remaining(), 0, "all judge responses consumed");
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
                stdout: "sat".to_string(),
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

#[test_log::test(tokio::test)]
async fn test_solver_error_then_success() {
    let llm = SequenceLlm::new(
        vec![
            formula_response(),
            "The solver had an error, let me fix it.".to_string(),
            formula_response(),
        ],
        vec![JUDGE_REASONABLE.to_string()],
    );
    let call_count = Arc::new(Mutex::new(0usize));
    let solver = ToggleSolver { call_count: call_count.clone(), error_until: 1 };

    let result = run_with_providers("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.solver_outcome, SolverOutcome::Unsat);
    assert_eq!(llm.primary_remaining(), 0, "all primary responses consumed");
    assert_eq!(llm.judge_remaining(), 0, "all judge responses consumed");
    assert_eq!(*call_count.lock().unwrap(), 2, "solver should be called twice");
}

#[test_log::test(tokio::test)]
async fn test_judge_says_retry_when_analysis_rejects_formula() {
    let llm = SequenceLlm::new(
        vec![
            formula_response(),
            formula_response(),
        ],
        vec![JUDGE_RETRY.to_string(), JUDGE_REASONABLE.to_string()],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = run_with_providers("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.solver_outcome, SolverOutcome::Unsat);
    assert_eq!(llm.primary_remaining(), 0, "all primary responses consumed");
    assert_eq!(llm.judge_remaining(), 0, "all judge responses consumed");
}

#[test_log::test(tokio::test)]
async fn test_judge_gives_unclear_answer_then_clear() {
    let llm = SequenceLlm::new(
        vec![
            formula_response(),
        ],
        vec![
            "The solver response is not reasonable, a new formula should be generated.".to_string(),
            JUDGE_REASONABLE.to_string(),
        ],
    );
    let solver = FakeSolver { outcome: SolverOutcome::Unsat };

    let result = run_with_providers("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert_eq!(result.solver_outcome, SolverOutcome::Unsat);
    assert_eq!(llm.primary_remaining(), 0, "all primary responses consumed");
    assert_eq!(llm.judge_remaining(), 0, "all judge responses consumed");
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
         **Expected result: UNSAT** — The formula is unsatisfiable because:\n\n\
         1. **Same operation sequence**: Both versions call `f_version` first, then the \
         same five sections in the same order.\n\
         2. **Helper functions match inline sections**: Each extracted helper function \
         performs exactly the same emitter API calls as its corresponding inline code.\n\
         3. **No cross-section variable dependencies**: All local variables are written \
         before being read within each section.\n\
         4. **Same conditional behavior**: All conditions are evaluated the same way in \
         both versions.",
        jemalloc_refactoring_formula()
    )
}

#[test_log::test(tokio::test)]
async fn test_jemalloc_refactoring_unsat() {
    let llm = SequenceLlm::new(
        vec![jemalloc_formula_response()],
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

    assert!(result.formula.contains("(set-logic UF)"));
    assert!(result.formula.contains("(declare-sort Emitter 0)"));
    assert!(result.formula.contains("(check-sat)"));
    assert_eq!(result.solver_outcome, SolverOutcome::Unsat);
    assert_eq!(llm.primary_remaining(), 0, "all primary responses consumed");
    assert_eq!(llm.judge_remaining(), 0, "all judge responses consumed");
}

#[test_log::test(tokio::test)]
async fn test_solver_multi_round_multi_formula() {
    let llm = SequenceLlm::new(
        vec![
            "Seems like a fun problem, good luck!.".to_string(),
            two_formula_response(),
            formula_response(),
            "The solver had an error, I'll regenerate.".to_string(),
            formula_response(),
            "The solver errored again, retrying.".to_string(),
            formula_response(),
            formula_response(),
        ],
        vec![JUDGE_RETRY.to_string(), JUDGE_REASONABLE.to_string()],
    );
    let call_count = Arc::new(Mutex::new(0usize));
    let solver = ToggleSolver { call_count: call_count.clone(), error_until: 2 };

    let result = run_with_providers("refactoring desc", &llm, &solver)
        .await
        .expect("agent should succeed");

    assert!(result.formula.contains("(set-logic"));
    assert_eq!(result.solver_outcome, SolverOutcome::Unsat);
    assert_eq!(llm.primary_remaining(), 0, "all primary responses consumed");
    assert_eq!(llm.judge_remaining(), 0, "all judge responses consumed");
    assert_eq!(*call_count.lock().unwrap(), 4, "solver should be called four times");
}