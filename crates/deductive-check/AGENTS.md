# Deductive-Check Crate — Design Specification

## Parameters

- target project
- LLM host + model + api key
- SMT solver path
- sub LLM host + models + keys (default to overall LLM host, model, key respectively)

## Definitions

### Key Line

A **Key Line** is one of:
- the function entry point,
- loop headers,
- function exit points

### Condition of a key line (l)

- if l is the entry point: the function's precondition
- if l is a loop header: loop invariants of the loop
- if l is a function exit point: the function's postcondition

## Rules

- When doing the verification, `#[cfg(verification)]` should be considered to be on, and should be passed correctly to rust-analyzer.
- Use `ra_ap_project_model::ProjectWorkspace` to load the entire Cargo project (not `Analysis::from_single_file`). Configure `CargoConfig` with `set_test: false` and `features: ["verification"]` so that `#[cfg(test)]` items are excluded and `#[cfg(verification)]` items are included. This gives correct cfg-aware parsing without manual `strip_cfg_attribute` hacks.
- For dynamic trait impls, such as `Box<Trait>`, methods called in that area should have a `const invariant_<method>(&Self, ...) -> bool` function. It should be verified that `invariant_<method>(...) == true` implies that the preconditions of `method(...)` are satisfied for every impl of that trait. This mechanism can be used to state preconditions of a dynamic trait object in the conditions of key lines, such as loop invariants where the loop accesses a dyn trait object.
- Parameters passed as `&T` to functions should be assumed not to be modified by the function.

## State Machine

States are listed in `[]`. I/O are listed with `--->external thing` and `<---response`. If multiple I/O, those should be multiple sub-states.

---

### [Initializer]

---> bash: git (make a new verification branch, timestamped)
<---
---> bash: mkdir (make a new subdirectory for verification files)
<---

next: [Function Lister(all rust files in the project)]

---

### [Function Lister(files to check)]

---> rust-analyzer
<--- List of all (file, function in that file)s in target project.

Ignore any functions that are neither in 'files to check', nor call a function in 'files to check', nor are called from 'files to check'. Store remaining functions as a hashmap `F[file] = [fun1, ..., funn]` mapping file to list of remaining functions in that file. Use something that uniquely identifies the function in that file to represent the function. For example, there might be multiple functions of the same name if they are impl of different types, so just using the name is not acceptable.

next: [Function Analyzer (F)]

---

### [Function Analyzer (F)]

for each file in F in parallel:
  for each function fun in F[file], in parallel:
    [Splitter (file, fun, M)]

    ---> Splitter Model
    <--- list `CP_fun` of code pieces of the form:
        ```
        // Start point: <file, line number>
        <lines of code> {
            return ...                            // Handover point: <file, line number>
        } ... {
            continue;                             // Handover point: <file, line number>
        } ... {
            while { ...                           // Handover point: <file, line number>
        }
        ```
    Each function fragment contains all branches of a code from one key line to the next reached key line. In loops, this may be until the next (implicit) continue, i.e., reaching back the same loop. For function exit points, add an explicit return for simplicity. Record the starting and ending key lines' file+line number in a comment.

    `CP_fun` are initialized as OPEN

collect every code piece in a big list CP as a triple (file, fun, code piece).

Next: [Full Formalizer (CP)]

---

### [Full Formalizer (CP)]

for each (file, fun, code-piece) in parallel:
  transition code piece to GET_CONTEXT, then

  #### [Context Provider (file, fun, code-piece, context, ...)]

  ---> rust-analyzer
  <--- full code (including pre & post conditions) of called function

  This state repeats until all called functions have been looked up and inserted into the context. Transition code piece to FORMALIZER.

  next: [CP Formalizer (file, fun, code-piece, context, elaboration="")]

  #### [CP Formalizer (file, fun, code-piece, context, elaboration)]

  ---> Formalizer LLM
  <--- list of SMT formulas, either as a pyz3 script in ` ```py ` block, or directly as an SMTlib formula in ` ```smt ` block, of that code piece + context, which are all UNSAT if we can guarantee:
      1) for every function call, that function's preconditions
      2) all handover points' conditions
      assuming that the start point's condition holds, and that all postconditions of functions encountered along the way hold.
      It is permitted to split a verification problem into subproblems, including 1) simplified formula, and 2) a side-condition formula to check that the simplification is always sound.
      Each formula should have a comment.
      If a python script is generated, it must output an SMTlib formula + explanation on stdout, and not call the solver itself.

  All formulas are initially in OPEN state.

  Transition the code piece to CHECK.

  For every formula:
    Transition the formula to CHECK.

    ##### [Formula Storer (file, fun, code-piece, context, formula)]

    ---> store the SMT formula as a file with a reasonable name and file ending in the verification folder
    <---

    ---> git add the formula's file
    <---

    ##### [Formula Translator (file, fun, code-piece, context, formula)]

    if file is a python file:
    ---> python
    <---

    next step with the SMTlib formula

    ##### [Formula Checker (file, fun, code-piece, context, formula, SMTlib formula)]

    ---> SMT solver
    <--- error, SAT, UNSAT, UNKNOWN

    if error:
      Transition the formula to FIX.

      ###### [Fixer (file, fun, code-piece, context, formula, error)]

      ---> Fixer LLM
      <--- Fixed SMT Formula

      The fixer LLM should be allowed to switch between pyz3 or smt.

      next: [Formula Checker (file, fun, code-piece, context, new formula)]

    if SAT/UNKNOWN/UNSAT:
      Transition the formula to CLOSED(UNSAT/UNKNOWN/UNSAT)

  collect the result of each formula.

  Make a commit for adding the formulas, counting the iteration number, before the judge response.

  Transition the code piece to JUDGE.

  #### [Overall checker (file, fun, code-piece, context, elaboration, SAT formulas, UNKNOWN formulas, UNSAT formulas)]

  ---> Judge LLM
  <--- "REASONABLE" if the formulas together imply the correctness of the code piece w.r.t. conditions. An explanation for problems that should be improved otherwise.

  if == "REASONABLE":
    if all formulas are UNSAT:
      Transition the code piece to CLOSED
      return
    else:
      Transition the code piece to UNVERIFIED(model, formula)
      return

  if not:
    Add the new problems given by the judge to the elaboration.
    Transition the code piece to FORMALIZER.
    [CP Formalizer (file, fun, code-piece, context, elaboration=new problems + "Also, previous formalizations had some problems that you should not repeat when fixing the current problems: " + elaboration)]

collect all the code piece results, then go to:
[Analyzer (closed code pieces, unverified code pieces)]

---

### [Analyzer (closed code pieces, unverified code pieces + models + formulas)]

if no unverified code pieces:
  return

record current commit in the current branch as the base commit.
Create a new verification feature branch.

then:
  [AnalyzeProblem (closed code pieces, all unverified code pieces + models + formulas, list of files with unverified code pieces = need to be rechecked later, base commit, name of feature branch)]

---

### [AnalyzeProblem (closed code pieces, remaining unverified code pieces + models + formulas, list of files that need to be rechecked later, base commit, name of feature branch)]

pick the first remaining (code piece + model + formula)

---> opencode CLI with Analyzer LLM:
    analyze whether the problem are incorrect conditions (too weak, too strong), and if so add the necessary conditions and propagate the changes to callers, loops, etc. as needed.
    Commit your work into a wip branch (based on the feature branch) before every compilation.
    Once you are satisfied, merge a squashed wip branch into the verification feature branch.
    Or the formula is too complex, in that case, add comments into the code of the format `/* VERIFICATION HINT: .... */` with an explanation of how the formula should be simplified or broken down into multiple steps in the future.
    In both cases, just respond with "RETRY".
    Or if the problem is a bug somewhere in the codepiece, explain the bug.
<--- RETRY or a bug report

Output any bug report in stdout.

then if no unverified pieces remain:
  [Restarter(base commit, name of feature branch, list of files that need to be rechecked later)]

---

### [Restarter(base commit, name of feature branch, list of files that need to be rechecked later)]

---> bash: with git find all rust files changed since the base commit
<---

collect these files as modified files. Go to:
[Function Lister(modified files + list of files that need to be rechecked later)]

---

## Implementation Status & Outstanding Issues

### Completed

- Replaced `Analysis::from_single_file()` with `ProjectWorkspace`-based `CliRustAnalyzerProvider` that loads entire Cargo project via `ra_ap_project_model`
- Fixed byte offset vs line number bug: `node_range.start()/end()` are `TextSize` (byte offsets), not line numbers — added `byte_offset_to_line()` / `line_to_byte_offset()` conversion helpers with bounds-checking assertions
- Fixed VFS setup: use `vfs.take_changes()` to get file contents instead of re-reading from disk (was causing rowan "Bad offset" panics)
- Fixed empty code passed to formalizer/judge: `split_function` retries up to 3 times on empty splitter response, filters out empty-code pieces, skips functions with empty bodies
- Fixed formula not shown to fixer LLM on solver error: `check_formula` now returns `(Formula, SolverOutcome, String)` where third element is the actual SMT content sent to solver; fix loop passes `current_smt` to `fix_formula` instead of raw `ef.content`
- Formula summary shown to judge now includes actual formula content (not just outcome)
- Judge prompt uses `refactor_check_core::consts::JUDGE_REASONABLE` constant instead of hardcoded "REASONABLE"
- Judge prompt says "reply only REASONABLE and nothing else" so `starts_with(JUDGE_REASONABLE)` check works correctly
- Added function docs retrieval via `analysis.goto_definition()` → `NavigationTarget.docs` (cloned out of locked scope)
- Added `docs: String` field to `FunctionInfo`, `GetFunctionDocs`/`FunctionDocs` request/response
- Added `fetch_docs()` helper using `goto_definition` at function name offset to get docs
- Docs flow: `FunctionLister` builds `function_docs: HashMap<FunctionId, String>` → `FunctionAnalyzer` → `Splitter` → `FullFormalizer`; `Splitter` stores docs via `pm.store_function_docs()`
- `gather_context` fetches called functions' docs via `GetFunctionDocs` and includes them
- `process_piece` builds `own_docs_section` with safety hint, passes to formalizer and judge
- `formalizer_user`, `judge_user`, `analyzer_user` prompts accept `docs_section: &str` parameter
- Added `store_function_docs`/`get_function_docs` to `DeductivePieceManager` trait and impl (backed by `DashMap<FunctionId, String>`)
- Build, clippy, and tests pass

### Outstanding: Postconditions Detection (HIGH PRIORITY)

~~Currently the formalizer only verifies that preconditions of called functions are met.~~ **DONE.**

`has_guarantees` field on `FunctionInfo` detects:
1. **`unsafe` blocks** in function body (not `unsafe fn` signature) — via `ra_ap_syntax` AST traversal: `fn_ast.body().syntax().descendants_with_tokens()` checking for `T![unsafe]` tokens
2. **Assertions** — `assert!`, `assert_eq!`, `assert_ne!`, `debug_assert!`, `panic!`, `.unwrap()`, `.expect()`, `unreachable!`, `unimplemented!` — via AST traversal: `MacroCall::cast` + path matching, `MethodCallExpr::cast` + name matching
3. **Doc sections** — `# Guarantees`, `# Postconditions`, `# Ensures`, `# Safety`

Functions without guarantees are filtered out in `FunctionLister`. When `has_guarantees` is true, the formalizer prompt adds: "You must verify not only that preconditions of called functions are satisfied, but also that this function's own postconditions and safety guarantees hold under the stated preconditions."

**Remaining enhancement:** Detect calls to functions with preconditions (e.g., safe wrappers around unsafe). This requires querying `outgoing_calls` for each function at listing time, which is expensive.

### Outstanding: Cross-Piece Boundary Relations

See `PLAN.md` in the workspace root for the full plan. Summary:

Currently each piece must be strictly equivalent in isolation. This breaks when a refactoring moves a variable across piece boundaries (e.g., `y` in before becomes `z` in after). The fix is to let the splitter annotate **boundary relations** as `/* relation: ... */` comments, the formalizer verifies the relation holds (not strict equivalence), the splitting judge checks relations are consistent across adjacent pieces, and the child judge verifies the formula encodes the relation correctly.

No structural changes needed — relation comments are inline in `before`/`after` text. Changes needed in: `splitter.rs` system prompt, `generation.rs` formalizer/fixer prompts, `splitting_judge.rs`, `child_judge.rs`.

### Outstanding: End-to-End Testing

Test the full pipeline with a real project to verify VFS/offset/docs integration works correctly from start to finish.

### Outstanding: Git Branch Cleanup

Many local git branches exist that are already merged. Clean them up.

### Critical Technical Context

- `CliRustAnalyzerProvider::new()` returns `Result<Self>` (not `Self`) — can fail if Cargo.toml missing or project loading fails
- `main.rs` calls `CliRustAnalyzerProvider::new(cli.project.clone())?` — removed old `rust_analyzer_path` CLI arg
- Both `main.rs` files use `fn main()` → `run_with_error_shell(closure)` instead of `#[tokio::main]`. All provider construction happens inside the closure. `agent::run` takes `(input_file, config, Option<Arc<ErrorGate>>)`.
- `ra_ap_vfs::Vfs::file_id()` returns `Option<(FileId, FileExcluded)>`, not just `Option<FileId>`
- `ra_ap_vfs::Vfs::set_file_contents()` returns `bool` (whether changed), doesn't return `FileId` — must query with `file_id()` after
- `ra_ap_vfs::Change::Create(bytes, hash)` / `Modify(bytes, hash)` — bytes available via `vfs.take_changes()` for feeding into `ChangeWithProcMacros`
- `NavigationTarget.docs` is `Option<Documentation<'static>>` — `Documentation::as_str()` gives the concatenated doc comment text including macro-expanded docs
- `GotoDefinitionConfig` only needs `ra_fixture: RaFixtureConfig::default()`
- `byte_offset_to_line` and `line_to_byte_offset` have `assert!` bounds checks — will give clear error messages instead of rowan panics if offsets are still wrong
- Remote `origin` → `https://github.com/JonasOberhauser/refactor-check.git`
- Must use `--manifest-path /workspace/refactor-check/Cargo.toml` for cargo commands
- Must use `GIT_DIR=/workspace/refactor-check/.git GIT_WORK_TREE=/workspace/refactor-check` for git commands
- `ra_ap_hir::Function::is_unsafe(db)` exists but checks if the function IS `unsafe fn` (preconditions for caller), NOT whether it contains unsafe blocks
- `ra_ap_hir::AnyFunctionId::is_unsafe(db)` similarly checks the function signature