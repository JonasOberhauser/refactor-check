---
name: smt-avoid-trivial-obligations
description: Use when writing Z3 verification scripts. Fires when you notice you are adding z3.BoolVal(True) or a tautology to a solver and then calling check(). This always returns SAT, creating a false "bug" in the summary. Use SKIP or a structural note instead.
---

# Don't Create Trivial SAT/UNSAT Obligations

## The Anti-Pattern

You want to note a structural fact (e.g., "terminal branches exit the loop, no invariant to preserve"), so you write:

```python
s = z3.Solver()
s.add(z3.BoolVal(True))
check(s, "terminal_no_invariant_needed")
# Result: SAT — shows up as a "BUG" in the summary!
```

`z3.BoolVal(True)` is satisfiable, so Z3 returns SAT. Your summary reports a bug that doesn't exist.

Similarly, `s.add(z3.BoolVal(False))` produces trivial UNSAT — meaningless proof.

## What To Do Instead

For structural properties verified by code inspection (not Z3), record them directly:

```python
results["terminal_no_invariant_needed"] = "SKIP"
print("  Terminal branches exit the loop (no invariant needed)")
```

If you must use Z3, assert the **negation of the property you want to prove**, then check:

- To **prove P**: assert `Not(P)`, check UNSAT.
- To **show a bug exists**: assert preconditions that expose it, check SAT (counterexample).
- To **confirm a dead-end**: assert the terminal state IS reached, negate the re-entry precondition, check SAT (by design).

## Red Flags

- **`s.add(z3.BoolVal(True))`** — trivial SAT incoming
- **`s.add(z3.BoolVal(False))`** — trivial UNSAT incoming
- **Solver with no meaningful assertions** — the result is meaningless
- **A "bug" in the summary with a vacuous label** — it's a false positive

## General Rule

`check()` answers "is there a model satisfying all constraints?" Only use it when your constraints encode a **genuine proof obligation** — the negation of a property you want proven, or preconditions that could expose a bug.