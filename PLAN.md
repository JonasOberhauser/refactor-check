# Refactor Check — Project Plan

## Purpose
A Rust CLI tool that checks whether a "before" and "after" function are equivalent by:
1. Asking an LLM to generate an SMT formula encoding equivalence
2. Running the formula through Z3 (or another SMT solver)
3. Iterating with the LLM if the solver reports errors or unreasonable results
4. Using a cheaper LLM to validate the first LLM's judgment of reasonableness

## Input Format
A `.txt` file (see `src_stats.stats_general_print.txt` example) with sections:
- Header: function name, source location
- `--- BEFORE ---` / `--- AFTER ---`: the two function versions
- `REFERENCED SYMBOLS`: recursively listed symbols (functions, macros, types)

## Architecture

### Modules
1. **`input`** — Parse the txt file into a structured `RefactorInput` (before_code, after_code, referenced_symbols)
2. **`llm`** — Async OpenAI-compatible client (via `async-openai`) supporting two models:
   - Primary LLM: generates formulas, analyzes solver output
   - Judge LLM (cheaper): summarizes as YES/NO
3. **`smt`** — Extract SMT formulas from LLM responses, run Z3, classify output (error vs clean)
4. **`agent`** — The main agent loop:
   - Send input to primary LLM → extract formula
   - If zero or multiple formulas found, insist until exactly one
   - Run Z3 on the formula
   - On solver error: send full history (input + all prior formulas + solver responses + current formula + current solver response) to primary LLM
   - On solver success: send formula + solver output to primary LLM, ask if reasonable or new formula needed
   - Pass primary LLM's response to judge LLM → YES/NO
   - On YES: output primary LLM response + formula to stdout, exit
   - On NO: loop back with full history
   - On other answer: insist judge LLM answer only YES or NO
5. **`main`** — CLI entry point: parse args (input file, solver path, model configs), run agent

## Key Design Decisions
- **LLM Client**: `async-openai` crate with custom base URL (OpenRouter: `https://openrouter.ai/api/v1`)
- **SMT Solver**: Z3 by default, configurable via CLI flag
- **Formula Extraction**: Look for SMT-LIB2 content (lines starting with `(set-logic`, `(declare-`, `(assert`, `(check-sat)` etc.) in code fences or bare text; count top-level SMT expressions
- **History**: Accumulate (formula, solver_output) pairs across iterations to provide context
- **Clippy**: Moderate on feature branch, strict (`-D warnings`) on main

## CLI Usage
```
refactor-check \
  --input <file.txt> \
  --solver-path z3 \
  --primary-model openrouter/z-ai/glm-5.1 \
  --judge-model openrouter/meta-llama/llama-3-8b-instruct \
  --api-base https://openrouter.ai/api/v1 \
  --api-key <KEY or env OPENROUTER_API_KEY>
```

## Git Workflow
- `main` branch: strict clippy (`cargo clippy -- -D warnings -D clippy::all`)
- `feature/agent-loop` branch: moderate clippy (`cargo clippy -- -W clippy::all`)
