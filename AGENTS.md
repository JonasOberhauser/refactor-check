# Agents

## Git Workflow (three-layer branching)

- **`main`**: Only merge after code quality fixes + 100 randomized test runs. Never commit directly.
- **`feature/<name>`**: One branch per fix/feature. Must compile and be tested. Squash the wip commits to merge into here. The agent must consider the code to be ok. 
- **`feature/<name>/wip`**: Create when starting work. Commit every build attempt — doesn't need to compile. Delete after merging feature branch to `main`.

## Pre-merge Checklist


For merge into `main` branch:
- `cargo clippy -- -W clippy::all` passes
- `cargo build` succeeds
- `cargo test` passes with zero warnings
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

Tests must never document or assert known buggy behavior. If a test reveals a bug, fix the code rather than encoding the buggy behavior as an expected result.

Never bypass compiler warnings with `#[allow(...)]` or similar suppression attributes. If code is only used in some scenarios, use feature flags or restructure into submodules so items are only compiled where they are used.

No code duplication. Use good abstractions and put common code into logically self-contained submodules.

**Never do parsing yourself.** For any syntax analysis (unsafe detection, assertions, macro calls, method calls, cfg attributes, etc.), use `ra_ap_syntax::ast` tree traversal (`descendants()`, `descendants_with_tokens()`, `cast()`). Never write hand-rolled text lexers or substring matching on source code.

## ContextId Rules

- **`ContextId` Clone is illegal.** Cloning resets `child_counter`, producing duplicate child IDs. There is no `Clone` impl and there must never be one.
- **Pass-through pattern for IO calls:** Move `ContextId` into I/O input, and recover it from `WithContext<T>` response. Never call `new_child()` for IO calls; the same context traces through all stages of a piece/formula.
- **`PhaseTracker` uses `u64` keys** (from `ctx.id()`), not `&ContextId`.

## Error Handling

- **Recoverable errors** (e.g., LLM failures, API errors, rate limits — anything that could be resolved by external human intervention) should be wrapped in a retry loop around `ErrorGate::report_and_wait`. The `LlmClient::invoke` retry loop is one example; every provider that can fail in externally-recoverable ways should follow the same pattern.
- **Unrecoverable errors** (e.g., invalid file references, missing dependencies, configuration errors — anything that cannot possibly be resolved by external intervention) should be propagated back to `main` for graceful exit.
- **Err on the side of caution:** when it is unclear whether an error can be resolved by external intervention, send it through `report_and_wait` rather than propagating it.
- `run_with_error_shell` runs async work on a background thread with an interactive shell on the foreground thread. In non-TTY mode, the error gate is disabled and all errors propagate directly.

## Tracing Rules

- **All tracing macros must include `%context_id`** whenever a context_id is available — not just in the direct call site, but in any helper function called (recursively). The context_id should be passed to helper functions (e.g., by reference), and any tracing in those functions should also use it.
- The context_id `Display` output is the dotted name (e.g., `1.2.3`), not the raw `u64` id.

## Rust-Analyzer Integration

- **Build scripts must run before `load_workspace`** — `workspace.run_build_scripts()` + `workspace.set_build_scripts()` must be called before `load_workspace()`, otherwise proc macro dylib paths aren't resolved.
- **`catch_unwind` on ra_ap APIs** — ra_ap APIs can panic on malformed input. Wrap expensive or fragile calls (especially `outgoing_calls`) in `std::panic::catch_unwind(std::panic::AssertUnwindSafe(...))`.
- **Rate limit detection** — `is_rate_limit` checks for: "rate limit", "rate_limit", "too many requests", "code: 429", "code: 1302", "code: 1305". Rate limit retries use exponential backoff with jitter and do NOT increment the attempt counter.

## Commands

- Build: `cargo build --workspace`
- Build release: `cargo build --release --workspace`
- Test: `cargo test --workspace`
- Lint: `cargo clippy --workspace -- -W clippy::all`

### Build habit

Always run `cargo build --release --workspace` after making a change. This ensures the release binaries are up to date for manual testing and prevents stale artifacts.

## Workspace Structure

```
refactor-check/
  Cargo.toml              ← workspace root
  crates/
    core/                  ← refactor-check-core (shared infrastructure)
    refactor-check/        ← refactor-check binary (split/judge/fix workflow)
    deductive-check/       ← deductive-check binary (new workflow, stub)
```

## Crate-Specific Documentation

- **refactor-check** — see `crates/refactor-check/AGENTS.md` and `crates/refactor-check/PLAN.md` for design decisions, architecture, and CLI usage
- **deductive-check** — see `crates/deductive-check/AGENTS.md` for the deductive verification design specification
