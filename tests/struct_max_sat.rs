mod common;

use common::{FakeSolver, SequenceLlm, test_pm};
use refactor_check::consts::JUDGE_REASONABLE;
use refactor_check::machine;
use refactor_check::smt::SolverOutcome;

fn struct_max_formula_response() -> String {
    r#"```smt2
(set-logic ALL)

;; -----------------------------------------------------------------
;; Sort for pointers to struct entry objects
;; -----------------------------------------------------------------
(declare-sort Entry 0)

;; Projection: the only field that matters for the comparison
(declare-fun key (Entry) Int)

;; -----------------------------------------------------------------
;; max_entry: returns the entry with the larger key
;; -----------------------------------------------------------------
(declare-fun max_entry (Entry Entry) Entry)

(assert (forall ((a Entry) (b Entry))
          (= (max_entry a b)
             (ite (> (key a) (key b)) a b))))

;; -----------------------------------------------------------------
;; max_of_three_before: original sequential‑if implementation
;; -----------------------------------------------------------------
(declare-fun max_of_three_before (Entry Entry Entry) Entry)

(assert (forall ((a Entry) (b Entry) (c Entry))
          (let ((m (ite (> (key b) (key a)) b a)))
            (let ((m2 (ite (> (key c) (key m)) c m)))
              (= (max_of_three_before a b c) m2)))))

;; -----------------------------------------------------------------
;; max_of_three_after: refactored version using nested max_entry
;; -----------------------------------------------------------------
(declare-fun max_of_three_after (Entry Entry Entry) Entry)

(assert (forall ((a Entry) (b Entry) (c Entry))
          (= (max_of_three_after a b c)
             (max_entry (max_entry a b) c))))

;; -----------------------------------------------------------------
;; Check equivalence: there must not exist inputs where the two
;; functions return different pointers.
;; -----------------------------------------------------------------
(assert (exists ((a Entry) (b Entry) (c Entry))
          (not (= (max_of_three_before a b c)
                  (max_of_three_after a b c)))))

;; -----------------------------------------------------------------
;; Run the solver
;; -----------------------------------------------------------------
(check-sat)
(get-model)
```"#.to_string()
}

fn struct_max_split() -> String {
    "\
Piece: max_of_three
---- BEFORE ----
(declare-fun max_of_three_before (Entry Entry Entry) Entry)
(assert (forall ((a Entry) (b Entry) (c Entry))
  (let ((m (ite (> (key b) (key a)) b a)))
    (let ((m2 (ite (> (key c) (key m)) c m)))
      (= (max_of_three_before a b c) m2)))))
---- AFTER ----
(declare-fun max_of_three_after (Entry Entry Entry) Entry)
(assert (forall ((a Entry) (b Entry) (c Entry))
  (= (max_of_three_after a b c)
     (max_entry (max_entry a b) c))))"
        .to_string()
}
#[test_log::test(tokio::test)]
async fn test_sat_judge_overrules() {
    let llm = SequenceLlm::new(
        vec![struct_max_formula_response()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
        vec![struct_max_split()],
    );
    let solver = FakeSolver {
        outcome: SolverOutcome::Sat,
    };

    let pm = test_pm();
    let result = machine::run("max_of_three struct refactoring", &llm, &solver, &pm)
        .await
        .expect("agent should succeed (bug: judge overruled)");

    assert_eq!(result.formulas.len(), 1);
    assert_eq!(result.formulas[0].outcome, SolverOutcome::Sat);
    assert!(result.formulas[0].formula.contains("(set-logic ALL)"));
    assert!(!result.overall_equivalent);
    assert_eq!(result.reasonable_sat, 1);
}
