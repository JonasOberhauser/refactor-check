# refactor-check

Check whether a code refactoring preserved semantic equivalence — automatically, using LLM + SMT solver.

## What it does

`refactor-check` takes a human-readable description of a refactoring ("before" and "after" versions of the same function) and verifies that the two versions are semantically equivalent.

It works by:

1. **Decomposing** the code into sub-pieces (loops, conditionals, helper functions) using an LLM
2. **Generating** an SMT-LIB2 formula for each piece that asserts the "before" and "after" produce the same results
3. **Running** an SMT solver (Z3) on each formula — in parallel
4. **Judging** each result: a cheap LLM checks whether SAT/UNSAT/UNKNOWN is actually reasonable for the piece
5. **Reporting**: if all pieces are `unsat` (no counterexample) → equivalent. If any piece is `sat` → not equivalent.

If a formula is too complex and the solver times out, the system automatically asks the LLM to split the code into smaller pieces and try again.

## Build

```bash
cargo build --release
```

## Usage

```bash
./target/release/refactor-check \
  --input example_input.txt \
  --api-key $OPENROUTER_API_KEY
```

### Required

| Flag | Description |
|------|-------------|
| `--input <path>` | Path to the refactoring description file |
| `--api-key <key>` | LLM API key (or set `OPENROUTER_API_KEY`) |

### Optional

| Flag | Default | Description |
|------|---------|-------------|
| `--solver-path` | `z3` | Path to the SMT solver binary |
| `--solver-args` | `-in` | Arguments passed to the solver |
| `--solver-timeout-secs` | `60` | Max seconds per solver invocation |
| `--api-model` | `openrouter/free` | Default LLM model |
| `--splitter-model` | (same as api-model) | Model for code decomposition |
| `--formalizer-model` | (same as api-model) | Model for SMT formula generation |
| `--fixer-model` | (same as api-model) | Model for formula correction |
| `--judge-model` | (same as api-model) | Model for result evaluation |
| `--splitting-judge-model` | (same as judge-model) | Model for evaluating code decompositions |
| `--api-base` | `https://openrouter.ai/api/v1` | API endpoint |
| `--stream-timeout-ms` | `3000` | Per-chunk stream timeout |
| `--max-stream-retries` | `5` | Retries on stream errors |
| `--service-tier` | `priority` | API service tier (auto/default/flex/scale/priority) |

Role-specific API keys can be set via env vars: `JUDGE_API_KEY`, `FORMALIZER_API_KEY`, `FIXER_API_KEY`, `SPLITTER_API_KEY`.

## Example input file

The format is a plain-text before/after function comparison. Any human-readable format works — the LLM parses it.

```
--- BEFORE ---

int abs(int x) {
    if (x < 0) return -x;
    return x;
}

--- AFTER ---

int abs(int x) {
    return (x < 0) ? -x : x;
}
```

## How to read the output

```
=== Overall Equivalent ===
true

=== Verified Formulas ===
1/1 reasonable (SAT: 0, UNSAT: 1, UNKNOWN: 0)

=== Open Pieces ===
0

--- Formula ---
[abs #1]
(declare-fun ...)
Outcome: Unsat, Verdict: REASONABLE
```

- **SAT**: the solver found a counterexample → this piece is NOT equivalent
- **UNSAT**: the SMT solver proved no counterexample exists → this piece is equivalent
- **UNKNOWN**: the solver couldn't decide → inconclusive, may need manual review
- **Open Pieces**: formulas that timed out or were rejected by the judge — not fully resolved

## Architecture

- `src/llm.rs` — async streaming LLM client (async-openai), per-role models and API keys, retry with partial content tolerance
- `src/smt.rs` — SMT solver subprocess manager with timeout support, per-piece tracing
- `src/agent.rs` — compositional verification entry point
- `src/machine.rs` — state machine orchestrating split → judge-split → generate → solve → judge → result
- `src/transitions.rs` — state machine transitions for all phases
- `src/states.rs` — state structs: `WaitForSplit`, `WaitForSplittingJudge`, `WaitForGeneration`, `WaitForResults`, `WaitForExplanation`
- `src/behaviors/` — per-phase logic: `splitter`, `splitting_judge`, `generation`, `results`, `child_judge`, `explain`
- `src/piece_manager.rs` — `PieceManager` trait for piece lifecycle (ID assignment, phase tracking)
- `src/provider.rs` — trait abstractions for `LlmProvider` and `SolverProvider`

### Testing

- `tests/agent_integration.rs` — integration tests with `SequenceLlm` and `FakeSolver`
- `tests/struct_max_sat.rs` — SAT detection regression test
- `tests/log_replay.rs` — replay-based tests using `LogReplayLlm` and `LogReplaySolver` mocks with per-pieceid message queues, including split depth exhaustion via splitting judge
- `tests/e2e_api.rs` — end-to-end tests against live API
- `tests/helpers/` — shared test utilities sub-crate (`refactor-check-test-helpers`)
- `parse_log.rb` — Ruby script to convert a refactor-check trace log into a Rust test case