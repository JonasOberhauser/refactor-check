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