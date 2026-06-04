---
name: smt-model-exhaustive-variants
description: Use when encoding a Rust enum, tagged union, or state machine as integer constants for SMT verification. Fires when you notice your constants skip variants — especially terminal/error/edge-case ones. If your encoding omits any variant that the code can produce, stop and add all variants before proceeding.
---

# Model ALL Enum Variants

## The Anti-Pattern

You encode an enum as integer constants for Z3. You model the "happy path" variants but omit terminal or error ones:

```python
# Rust enum has 8 variants, but you only model 7:
STATE_A = 0
STATE_B = 1
STATE_C = 2
STATE_D = 3
STATE_E = 4
STATE_F = 5
STATE_G = 6
# STATE_H = ??? — OMITTED!
```

This causes **false bug reports**. When the code transitions to the omitted variant, your model doesn't represent it. You report "Bug: state stays as D after transition-to-H" — but the code actually transitions to H, you just didn't model it.

## What To Do Instead

Before writing any verification script:

1. **Read the full enum/variant definition** from the source code. Every variant, including error/terminal/default ones.

2. **Assign a constant to EVERY variant.** No exceptions.

3. **Model every transition** in the codebase, including transitions to terminal variants.

4. **Identify terminal states** — variants that are never transitioned away from. Document them explicitly.

5. **Verify terminal states are dead-ends.** If a terminal state can't re-enter the system, prove it: assert the state IS the terminal variant, negate the re-entry precondition, check SAT (confirming the dead-end is by design).

## Red Flags

- **"Bug: state stays as X after Y"** — check whether the code actually transitions to an unmodeled variant
- **A constant is assigned but never used in any transition** — you may be missing a code path
- **An enum variant exists in source but has no constant** — add it immediately
- **You skipped a variant because "it's never reached"** — prove that with Z3, don't assume it

## The Fix

```python
# Complete encoding with ALL variants
STATE_A = 0
STATE_B = 1
STATE_C = 2
STATE_D = 3
STATE_E = 4
STATE_F = 5
STATE_G = 6
STATE_H = 7  # MUST include all variants

TERMINAL_STATES = [STATE_E, STATE_F, STATE_G, STATE_H]
```

Then verify dead-ends:
```python
# Verify: STATE_H cannot re-enter the system (by design)
s.add(state[key] == STATE_H)
s.add(z3.Not(re_entry_precondition))
check(s, "STATE_H_dead_end_CONFIRMED")
# Expected: SAT — confirming the dead-end is intentional
```