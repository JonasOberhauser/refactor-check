---
name: smt-verify-from-invariant
description: Use when writing SMT verification for code with loops or iterative state machines. Fires when you catch yourself proving preconditions by tracing the code path that produces data ("X == Y because the code does Z") instead of from the loop invariant. In a loop, the code-path derivation only holds on the first iteration.
---

# Verify From Loop Invariants, Not Code Paths

## The Anti-Pattern

You verify a transition inside a loop by tracing the code path that produces the data:

```python
# "Retry items have state X because function foo() sets state to X"
s.add(state[pid] == X)  # derived from code path, not loop invariant
```

This assumes the property holds because you traced how the data was **created**. But in a loop, collections carry items across iterations. After the first iteration, items may have been processed — their state mutated — while the collection still holds references to them. The creation-path property no longer holds.

## What To Do Instead

1. **Identify the loop invariant.** What properties does the data have at the **top of every iteration**, not just the first?

2. **Check for collection staleness.** Is the collection reset, rebuilt, or carried forward? If carried forward, items from previous iterations may have mutated state that no longer satisfies the creation-path property.

3. **Prove transitions from the invariant.** If the invariant is "all items in collection C have property P", use P directly. Don't derive P from the code path.

4. **If the invariant can't hold, fix the code.** If stale items violate preconditions:
   - Reset the collection at loop boundaries (items are consumed, not accumulated)
   - Prune stale items before re-entry
   - Or strengthen the loop invariant to account for stale items

5. **Demonstrate the bug.** Show SAT counterexamples with states that stale items could have after previous iterations — states that violate the callee's precondition.

## Red Flags

- **"X == Y because the code does Z"** in a verification comment — you're reasoning about a single code path, not the invariant
- **Tracing code paths to justify a state assumption in a loop** — the state at re-entry must come from the invariant
- **Assuming a collection is fresh** — always check whether it's reset or accumulated
- **A SAT "bug" about a state that "shouldn't exist"** — check whether you're missing a variant (see `smt-model-exhaustive-variants` skill) or whether stale items are the real problem

## Example

**Wrong** (code-path reasoning in a loop):
```python
# Retry items have state == X because function foo() sets state to X
s.add(state[pid] == X)  # Only true for items created THIS iteration
```

**Right** (loop-invariant reasoning):
```python
# Loop invariant: all items in collection C have state == X
# This holds because C is reset to empty at each iteration boundary,
# and only populated with fresh items from the current iteration.
s.add(state[pid] == X)  # From loop invariant
```

**Bug demonstration** (without the reset fix):
```python
# Stale items with state Y/Z/W (from prior iterations) violate precondition
for stale_state in [Y, Z, W]:
    s.add(state[pid] == stale_state)
    # Precondition is violated (SAT = real bug before fix)
```