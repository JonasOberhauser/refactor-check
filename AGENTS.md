# Agents

## Git Workflow (three-layer branching)

- **`main`**: Only merge after code quality fixes + 100 randomized test runs. Never commit directly.
- **`feature/<name>`**: One branch per fix/feature. Must compile and be tested. Squash the wip commits to merge into here. The agent must consider the code to be ok. 
- **`feature/<name>/wip`**: Create when starting work. Commit every build attempt — doesn't need to compile. Delete after merging feature branch to `main`.

## Pre-merge Checklist


For merge into `main` branch:
- `cargo clippy -- -W clippy::all` passes
- `cargo build` succeeds
- 100 randomized test runs pass
- Critical code review from agent must pass
- Must fully implement at least one fix or feature without degrading other features or code quality

For merge into `feature/<name>` branch:
- `cargo clippy` must pass
- `cargo build` must pass
- one test must pass
- squash

For `feature/<name>/<wip>` branch:
- no checklist, just commit.


## Release branches (`release`, `release/x.y.z`)

- The user creates and manages release branches. **Never merge into `release` or create release tags without the user's explicit command.** Only push to `main` and stop there.

### Version tracking

- The `version` field in `Cargo.toml` must match the git release tag. When creating a release tag `vX.Y.Z`, first update `Cargo.toml` to `version = "X.Y.Z"` and commit it. The tag should point to that commit.

### Pre-release checklist

- **Before every release**, `README.md` must be updated to reflect the current state: CLI flags, architecture, usage instructions, and any new features.

## Important!

Every new feature branch must be based off of main, and previous feature branches either explicitly abandoned by the user (you can ask them whether a feature branch should be abandoned when they ask for a new feature) or merged into main.

## Commands

- Build: `cargo build`
- Build release: `cargo build --release`
- Lint: `cargo clippy -- -W clippy::all`

### Build habit

Always run `cargo build --release` after making a change. This ensures the release binary is up to date for manual testing and prevents stale artifacts.

## Design Decisions

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
