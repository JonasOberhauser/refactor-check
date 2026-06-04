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
