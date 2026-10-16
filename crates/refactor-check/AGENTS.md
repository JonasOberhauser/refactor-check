# Refactor-Check Crate — Design Decisions

### WaitForSplittingJudge (2026-05-27)

- After the splitter produces code pieces, a `WaitForSplittingJudge` state evaluates the decomposition before proceeding to `WaitForGeneration`.
- The splitting judge uses the judge LLM role (`LlmRole::SplittingJudge`) to determine if the decomposition is sound.
- If rejected, the judge's feedback is passed to `WaitForSplit` for re-splitting, and `split_depth` is incremented.
- If `split_depth >= MAX_SPLIT_DEPTH`, pieces become open instead of re-splitting.
- `split_depth` only lives in the split-judge loop (`WaitForSplit` → `WaitForSplittingJudge`). It does not appear in `WaitForGeneration` or `WaitForResults`.
- `PiecePhase::Resplitting` was removed. Solver Unknown transitions `Solving → Open` instead.

### Splitter validation (2026-05-27)

- `extract_split_pieces` now requires **both** BEFORE and AFTER to be non-empty (`&&` not `||`). Pieces missing either side are rejected.

### SMT formula extraction (2026-05-27)

- `extract_single_formula` returns empty string + warns when multiple fenced SMT blocks are found, instead of silently keeping the first.
- Generation treats empty formulas as invalid, entering the insist loop to request exactly one formula per piece.

### CodePiece encapsulation (2026-05-21)

- `CodePiece` has **private fields** and **no `Clone`/`Copy`**. It can only be constructed via `CodePiece::new(label, before, after)` which internally assigns a monotonically increasing `id` from a global `AtomicU64`.
- `CodePiece::with_id(...)` is available under `#[cfg(test)]` only.
- All consumers hold `&CodePiece` (never clone). Structs that need to own a piece use `Arc<CodePiece>` for cheap shared-ownership without cloning the underlying struct.
- `PieceFormula` struct was removed. Generation returns `Vec<String>` (formulas), and the caller (`WaitForGeneration::execute`) zips formulas with owned `Arc<CodePiece>` from `self.pieces`.
- `FormulaBranch.verified` field was removed (unused in Results phase).
- State structs carrying pieces use `Vec<Arc<CodePiece>>` — state is moved between phases, never cloned.

### PieceManager (2026-05-22)

- All piece ID generation and phase tracking is behind the `PieceManager` trait (see `src/piece_manager.rs`).
- `DefaultPieceManager` owns an `AtomicU64` counter and a `DashMap<u64, PiecePhase>` as normal fields — no global statics.
- `CodePiece::new(label, before, after)` was removed. Pieces are created via `pm.new_piece(label, before, after)`.
- `CodePiece::with_id(...)` is `pub(crate)` so `DefaultPieceManager` can assign deterministic IDs; it is NOT available to external crate consumers.
- The `AlgorithmState` trait accepts `pm: &dyn PieceManager`. All state transitions thread the PieceManager.
- Tests create their own `DefaultPieceManager` via `test_pm()`, ensuring isolated ID counters and phase maps per test.
- Mock providers (`LogReplayLlm`, `LogReplaySolver`) support per-pieceid message/outcome queues for replay-based testing.

### Phase tracking (2026-05-22)

- `src/phase.rs` now only contains the `PiecePhase` enum. All tracking logic lives in `DefaultPieceManager`.
- `enter_generation(piece_id, to)` allows entering generation from `absent | Open | Forming | Fixing` — absent handles fresh pieces from resplit, Open handles re-processing after solver errors.
- Open items in `WaitForResults::execute` are cleared (`open: Vec::new()`) when transitioning to `WaitForGeneration` — prevents accumulated open items from re-entering generation after they were already handled (e.g., resplit).

### Piece tracing (2026-05-21)

- Every LLM call carries `piece: Option<&CodePiece>` for `piece_id` in trace spans.
- Solver calls (`run_solver`) now thread `piece_id: Option<u64>` through to all internal logs (start, timeout, finished, raw output).
- `StreamHandler` emits `piece_id` in both `sending LLM streaming request` and `full LLM response` events.

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