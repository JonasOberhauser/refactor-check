mod common;

use common::{FakeSolver, SequenceLlm};
use refactor_check::agent::{run_with_providers, JUDGE_REASONABLE};
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

fn struct_max_analysis() -> String {
    r#"# Analysis of Solver Response

## Is This Result Reasonable?

**No, this result is NOT reasonable.** The solver's "sat" answer is unreliable because the formula has fundamental encoding problems.

## Key Issues

### 1. **Underspecified Key Function**
The `key` function is declared uninterpreted, and the solver arbitrarily assigns it values:
- `key(Entry!val!0) = -1`
- `key(Entry!val!1) = 0`
- `key(Entry!val!2) = 6`

This is problematic because the comparison operations (`>`, `ite`) in the function definitions depend on the actual integer values of keys. The solver is free to pick arbitrary integers, which can create edge cases.

### 2. **Non-Associativity in the Model**
The solver's finite model for `max_entry` does NOT correctly implement "returns entry with larger key":

Looking at the model, I can verify that for inputs like `(Entry!val!0, Entry!val!1, Entry!val!0)` the result `Entry!val!2` has key `6`, which is inconsistent with a proper "maximum by key" semantics where the result should come from the inputs.

### 3. **Fundamental Property Violation**
With keys `{-1, 0, 6}`, the `max` operation should be **associative**:
```
max(max(a, b), c) ≡ max(a, max(b, c)) ≡ max(a, b, c)
```

Both implementations are semantically computing the maximum of three elements, so they **must** agree. The solver incorrectly returning "sat" suggests the finite model doesn't properly capture the intended infinite-domain semantics.

### 4. **Logic ALL is Inappropriate**
`(set-logic ALL)` tells the solver to choose the strongest logic, but the resulting model with a small finite universe cannot properly encode the semantics of comparing arbitrary integers.

## Recommendation

**A new formula should be generated** with the following improvements:

1. Use `UFLIA` or `QF_LIA` logic with proper integer arithmetic
2. Either:
   - Constrain `key` to a finite set of values using `assert` statements, OR
   - Use a sufficiently large finite interpretation of Entry (at least 4+ elements) to test all key combinations
3. Add an explicit cardinality constraint like `(assert (= (Cardinality Entry) 4))` if using a theory with cardinality

This will produce a reliable equivalence check where the solver either correctly proves unsat (functions are equivalent) or provides a meaningful counterexample."#.to_string()
}

/// Reproduces the real-world bug from conversation.log: the primary LLM's analysis
/// explicitly rejects the SAT result as NOT reasonable (underspecified key function,
/// spurious finite model, inappropriate logic), but the judge LLM overrides and returns
/// REASONABLE. The judge now receives the formula + solver output directly (not the
/// primary's analysis), making this test document the scenario where the judge
/// independently evaluates the raw SAT result and incorrectly deems it reasonable.
#[test_log::test(tokio::test)]
async fn test_sat_primary_rejects_judge_overrules() {
    let llm = SequenceLlm::new(
        vec![struct_max_formula_response(), struct_max_analysis()],
        vec![JUDGE_REASONABLE.to_string()],
    );
    let solver = FakeSolver {
        outcome: SolverOutcome::Sat,
    };

    let _result = run_with_providers("max_of_three struct refactoring", &llm, &solver)
        .await
        .expect("agent should succeed (bug: judge overruled primary rejection)");
}
