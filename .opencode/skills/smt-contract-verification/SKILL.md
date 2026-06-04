---
name: smt-contract-verification
description: Use when formally verifying Rust contracts, preconditions, postconditions, or state machine transitions using Z3/SMT-LIB2. Use when the user asks about proving correctness of call sites, state transitions, or loop invariants.
---

# SMT-Based Contract Verification Strategy

## Core Principle

At every call site, the callee's preconditions must be implied by the **caller's preconditions plus the postconditions of all prior calls in the same chain**.

Encode each obligation: assume caller pre + chained prior posts, negate callee pre, check UNSAT.

- **UNSAT**: obligation proven (no counterexample exists)
- **SAT**: bug found, or a design constraint confirmed (e.g., a terminal state that can't re-enter)
- **UNKNOWN**: solver couldn't determine — simplify the formula

## Four Verification Scripts

### 01 — Function contracts at every call site

Model each function's pre/post as Z3 expressions. For each call site, chain postconditions forward from prior calls. Prove: `caller_pre ∧ prior_posts ⇒ callee_pre`.

Pattern:
```
f1(args1) -- post1 --> f2(args2) -- post2 --> f3(args3)
```
At f3's call site: assume `caller_pre ∧ post1 ∧ post2`, negate `f3_pre`, check UNSAT.

### 02 — State machine transition preconditions

For each transition (StateA → StateB), prove StateB's preconditions follow from StateA's invariants plus the transition code's effects.

### 03 — Loop invariant establishment and preservation

For each loop, prove:
1. **Base case**: invariant holds at loop entry
2. **Preservation**: for each cycling branch, invariant holds after one iteration
3. **Termination**: terminal branches exit the loop (no preservation needed — use SKIP, not Z3)

### 04 — Transitions verified from loop invariants

State transitions inside a loop must be verified from the **loop invariant**, not from code-path reasoning about how data was produced. See `smt-verify-from-invariant` skill.

## Modeling State Machines

Model shared mutable state (e.g., a global map) as `z3.Array(KeySort, ValSort)` with a sentinel value for "absent" keys.

**Every value that the code can produce must have a constant.** Read the full enum/variant definition from source before encoding. See `smt-model-exhaustive-variants` skill.

## Bug Confirmation Pattern

When SAT is found:
1. **By-design SAT**: Terminal states that can't re-enter the system (dead-ends). Label with `_CONFIRMED`.
2. **Stale-state SAT**: Counterexample involves state that shouldn't exist at a loop entry. The collection may be carrying stale items. Reset collections at loop boundaries.
3. **Real bug**: Precondition genuinely violated. Label with `BUG_`.

## Output Convention

```
[UNSAT/SAT/UNKNOWN] label  (proven/BUG/???)
```

Summary categories: Proven (UNSAT), Bugs (SAT, BUG_), Dead-end confirmed (SAT, by design, _CONFIRMED), SKIP (structural, not Z3).