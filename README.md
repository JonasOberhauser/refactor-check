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
| `--primary-model` | `qwen/qwen3-coder:free` | LLM that generates SMT formulas |
| `--judge-model` | `google/gemma-3-4b-it:free` | LLM that evaluates results |
| `--api-base` | `https://openrouter.ai/api/v1` | API endpoint |
| `--stream-timeout-ms` | `3000` | Per-chunk stream timeout |
| `--max-stream-retries` | `5` | Retries on stream errors |

## Example input file

See [`example_input.txt`](example_input.txt) for a minimal example. The format is a plain-text before/after function comparison. Any human-readable format works — the LLM parses it.

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
Overall Equivalent: true
Verified Formulas: 1/1 reasonable (SAT: 0, UNSAT: 1, UNKNOWN: 0)
Open Pieces: 0
```

- **UNSAT**: the SMT solver proved no counterexample exists → this piece is equivalent
- **SAT**: the solver found a counterexample → this piece is NOT equivalent
- **UNKNOWN**: the solver couldn't decide → inconclusive, may need manual review
- **Open Pieces**: formulas that timed out or were rejected by the judge — not fully resolved

## Architecture

- `src/llm.rs` — async streaming LLM client (async-openai)
- `src/smt.rs` — solver subprocess manager (tokio::process)
- `src/agent.rs` — compositional verification loop: batch generation → parallel solving → parallel judging
- `src/provider.rs` — trait abstractions for LLM and solver backends
