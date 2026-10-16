# Agents

Rust workspace of tools that verify code with an LLM and an SMT solver.

## Layout

- `crates/core/` (`refactor-check-core`): shared infrastructure: LLM client, SMT solver, error gate, live config, status layer and shell protocols. Re-exports `servyi-states` (`ContextId`, phase trackers, message log).
- `crates/refactor-check/`: checks that a refactoring preserved semantics (split, judge the split, generate formulas, solve, judge, explain). CLI in `crates/refactor-check/README.md`.
- `crates/deductive-check/`: verifies a Cargo project's functions against their pre- and postconditions. It implements `crates/deductive-check/DESIGN.md`; read that before changing its state machine.
- `crates/deductive-shell/`: TUI client for a running deductive-check server.
- `crates/deductive-check/fuzz/`: cargo-fuzz harness, a separate workspace with its own `Cargo.lock`.

The `servyi-*` crates (states, ioprovider, servatui) are git dependencies on their `main` branches.

## Commands

Use the toolchain CI pins, `nightly-2026-09-21` (`servyi-states` needs `#![feature(register_tool)]`).

- Build: `cargo +nightly-2026-09-21 build --workspace`, and after a change also `--release`: the release binaries are used for manual testing.
- Test: `cargo +nightly-2026-09-21 test --workspace --all-targets`. The `duv_demo` integration tests skip themselves when their fixture project is absent.
- Lint: CI runs the shared servyi policy from servyi/lints (`.github/actions/lint-policy/action.yml`): clippy under its flag list, the servyi-lint driver, and `cargo doc` with `-D rustdoc::broken_intra_doc_links`. It denies all warnings, `clippy::unwrap_used`, `clippy::panic`, `clippy::missing_panics_doc` and `unused_results`, among others; only test code may allow `unwrap_used`/`panic`.
- Fuzz: in `crates/deductive-check/fuzz`, `cargo +nightly-2026-09-21 fuzz run state_machine` (or `formula_extraction`); `cargo test` there runs the harness's own tests.

The code is not rustfmt-formatted; don't reformat code you don't change.

## Git workflow

- `main`: never commit directly; merge only from a feature branch that passed the checklist below.
- `feature/<name>`: one per fix or feature, based on `main`. It must compile and be tested; squash the WIP commits into it. Before starting a new one, every earlier feature branch must be merged into `main` or explicitly abandoned by the user; ask if unsure.
- `feature/<name>/wip`: commit every build attempt; it doesn't need to compile. Delete it once the feature branch is merged into `main`.
- Release branches (`release`, `release/x.y.z`) and release tags belong to the user: never merge into them or tag without the user's explicit command. Before a release, `README.md` must describe the current CLI flags, architecture and usage, and `version` in `Cargo.toml` must match the tag `vX.Y.Z` (commit the bump, tag that commit).

Before merging into `feature/<name>`: build, lint and at least one test pass. Before merging into `main`: all tests pass without warnings, 100 randomized test runs pass, a critical agent code review passes, no `TODO`/`FIXME` is left behind, and the branch fully implements at least one fix or feature without degrading others or code quality.

## Code rules

- Never silence warnings with `#[allow(...)]` or similar. Code used only in some scenarios goes behind a feature or into a submodule compiled only where it is used.
- No duplication: put shared code into self-contained submodules (`core` for code both binaries use).
- Tests must never assert known buggy behavior; fix the code instead.
- Analyze Rust source only through `ra_ap_syntax::ast` traversal (`descendants()`, `descendants_with_tokens()`, `cast()`), never with hand-written lexers or substring matching.

## Context IDs and tracing

- `ContextId` must never be cloneable: a clone resets the child counter and repeats child IDs. Pass it by reference or move it.
- IO calls pass the context through: move it into the request and take it back from the `WithContext<T>` response. Don't create a child context for an IO call; one context follows a piece or formula through all its stages.
- Phase trackers are keyed by `ctx.hash()` (`u64`), not `&ContextId`.
- Every tracing macro includes the context whenever one is available, also in helpers, which take it by reference. The message log only picks up fields named `ctx`, `context_id`, `func_ctx` or `fctx`. A context displays as its dotted name (`1.2.3`).

## Error handling

- Errors that outside intervention can fix (LLM or API failures, rate limits, a bad key) go through a retry loop around `ErrorGate::report_and_wait`, as in `LlmClient::invoke`. When unsure whether an error is fixable from outside, use the gate.
- Errors nothing outside can fix (invalid file references, missing dependencies, configuration errors) propagate to `main` for a clean exit.
- deductive-check runs the machine on a background thread; a gated error parks until the user retries or aborts in `deductive-shell`. Aborting returns `ShutdownRequested`, which callers propagate with `?`. refactor-check runs without a gate.

## refactor-check pieces

- `CodePiece` has private fields and no `Clone`/`Copy`. Create pieces only through `PieceManager::new_piece`, which assigns IDs and tracks phases. Hold `&CodePiece`, or `Arc<CodePiece>` to own one.
- Tests create their own `DefaultPieceManager` (`test_pm()`), so IDs and phases are isolated per test.

## rust-analyzer (deductive-check)

- Load the whole Cargo project with `ra_ap_project_model::ProjectWorkspace`, not `Analysis::from_single_file`. Configure `CargoConfig` with `set_test: false` and `features: ["verification"]`, so `#[cfg(test)]` items are excluded and `#[cfg(verification)]` items included.
- Run the build scripts (`run_build_scripts` + `set_build_scripts`) before `load_workspace`, otherwise proc-macro dylib paths are not resolved.
- ra_ap APIs can panic on malformed input: wrap expensive or fragile calls, especially `outgoing_calls`, in `std::panic::catch_unwind(std::panic::AssertUnwindSafe(...))`.
