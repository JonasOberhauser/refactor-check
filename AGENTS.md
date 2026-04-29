# Agents

## Git Workflow (three-layer branching)

- **`main`**: Only merge after code quality fixes + 100 randomized test runs. Never commit directly.
- **`feature/<name>`**: One branch per fix/feature. Must compile and be tested before merging to `main`.
- **`feature/<name>/wip`**: Create when starting work. Commit every build attempt — doesn't need to compile. Delete after merging feature branch to `main`.

## Pre-merge Checklist

- `cargo clippy -- -W clippy::all` passes
- `cargo build` succeeds
- 100 randomized test runs pass

## Commands

- Build: `cargo build`
- Lint: `cargo clippy -- -W clippy::all`
