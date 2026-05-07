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

## Important!

Every new feature branch must be based off of main, and previous feature branches either explicitly abandoned by the user (you can ask them whether a feature branch should be abandoned when they ask for a new feature) or merged into main.

## Commands

- Build: `cargo build`
- Lint: `cargo clippy -- -W clippy::all`
