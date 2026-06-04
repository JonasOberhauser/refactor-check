# refactor-check workspace

Rust tooling for verifying code correctness using LLM + SMT solver.

## Crates

- **[`crates/refactor-check/`](crates/refactor-check/)** — Check whether a code refactoring preserved semantic equivalence. See [`crates/refactor-check/README.md`](crates/refactor-check/README.md).
- **[`crates/deductive-check/`](crates/deductive-check/)** — Deductive verification of code against specifications. See [`crates/deductive-check/AGENTS.md`](crates/deductive-check/AGENTS.md).
- **[`crates/core/`](crates/core/)** — Shared infrastructure (LLM client, solver interface, piece management).

## Build

```bash
cargo build --release --workspace
```

## Test

```bash
cargo test --workspace
```

## Lint

```bash
cargo clippy --workspace -- -W clippy::all
```