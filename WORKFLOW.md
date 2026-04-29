# Git Workflow

This project uses a three-layer branching strategy.

## Branch Layers

### 1. `main`
- The stable branch. Only merge into `main` after:
  - Code quality fixes are applied (clippy, style, etc.)
  - Correctness is verified with **100 randomized test runs**
- Never commit directly to `main` — always merge from a feature branch.

### 2. Feature branches (`feature/<name>`)
- One branch per fix or feature request.
- Commit when you believe the fix or feature is complete.
- The project **must compile** and **must have been tested** before merging into `main`.
- Merge into `main` via a merge commit (no squashing).

### 3. WIP branches (`feature/<name>/wip`)
- Create a WIP branch when you start working on a fix or feature.
- Commit frequently — every time you try to build, even if it doesn't compile or pass tests.
- These are throwaway branches: delete after merging the corresponding feature branch into `main`.

## Workflow Summary

```
feature/<name>/wip   →   feature/<name>   →   main
   (frequent commits,      (compiles, tested,     (quality fixes + 100
    may not compile)         ready to merge)        randomized test runs)
```

## Checklist Before Merging to `main`

- [ ] `cargo clippy -- -W clippy::all` passes cleanly
- [ ] `cargo build` succeeds
- [ ] 100 randomized test runs pass
- [ ] No `TODO` or `FIXME` comments left behind