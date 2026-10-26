use refactor_check::consts::JUDGE_REASONABLE;
use refactor_check::machine;
use refactor_check::smt::SolverOutcome;
use refactor_check_test_helpers::sequence::{FakeSolver, SequenceLlm};
use refactor_check_test_helpers::test_pm;

/// Description passed to the machine run.
const RUN_DESC: &str = "max_of_three struct refactoring";
/// Formula fragment asserted on the extracted formula.
const EXPECTED_LOGIC_HEADER: &str = "(set-logic ALL)";
/// Expected formula count.
const ONE_FORMULA: usize = 1;
/// Index of the first formula in the results.
const FIRST_FORMULA: usize = 0;

fn struct_max_formula_response() -> String {
    STRUCT_MAX_FORMULA_RESPONSE.to_string()
}

/// The full SMT-LIB response fixture returned by the formalizer.
const STRUCT_MAX_FORMULA_RESPONSE: &str = r#"```smt2
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
```"#;


/// The split fixture: a single max_of_three piece.
const STRUCT_MAX_SPLIT: &str = "
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
     (max_entry (max_entry a b) c))))";

fn struct_max_split() -> String {
    STRUCT_MAX_SPLIT.to_string()
}
#[test_log::test]
fn test_sat_judge_overrules() {
    let llm = SequenceLlm::with_accepting_judge(
        vec![struct_max_formula_response()],
        vec![],
        vec![JUDGE_REASONABLE.to_string()],
        vec![struct_max_split()],
    );
    let solver = FakeSolver {
        outcome: SolverOutcome::Sat,
    };

    let pm = test_pm();
    let result = machine::run(RUN_DESC, &llm, &solver, &pm)
        
        .expect("agent should succeed (bug: judge overruled)");

    assert_eq!(result.formulas.len(), ONE_FORMULA);
    assert_eq!(result.formulas[FIRST_FORMULA].outcome, SolverOutcome::Sat);
    assert!(result.formulas[FIRST_FORMULA].formula.contains(EXPECTED_LOGIC_HEADER));
    assert!(!result.overall_equivalent);
    assert_eq!(result.reasonable_sat, ONE_FORMULA);
}
