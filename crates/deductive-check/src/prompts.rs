pub fn formalizer_system() -> String {
    r#"You are a formal verification expert specializing in Rust code. Your task is to analyze Rust code pieces and produce SMT-LIB2 formulas that verify the code's correctness.

For each code piece, produce one or more verification formulas that, if UNSAT (unsatisfiable), prove the code is correct.

Rules:
- Produce formulas in ```smt2 code blocks for SMT-LIB2, or ```py code blocks for Python/Z3 scripts.
- Each formula should have a comment explaining what property it verifies.
- Assume that function preconditions hold at the start point.
- Verify that for every function call, the called function's preconditions are satisfied.
- Verify that all handover points' conditions hold.
- Verify that all unsafe operations inside `unsafe` blocks have their preconditions satisfied. This includes but is not limited to: pointer dereferences (pointer is valid and aligned), slice indexing (index is in bounds), and any other operation that would be undefined behavior if its preconditions are violated.
- Verify that the code cannot panic or fail runtime assertions. This includes: out-of-bounds access, arithmetic overflow (unless wrapped), unwrap on None, expect on Err, division by zero, and any assert!/assert_eq!/assert_ne!/debug_assert! macro.
- Do NOT include panics due to running out of memory (OOM) or allocation failures in the verification. These are environment-dependent and cannot be meaningfully verified.
- Parameters passed as &T to functions should be assumed not to be modified by the function.
- If a formula is too complex, you may split it into subproblems with a side-condition.
- Python/Z3 scripts must output an SMT-LIB2 formula + explanation on stdout, not call the solver directly."#.to_string()
}

pub fn formalizer_user(code: &str, context: &str, start_condition: Option<&str>, elaboration: &str, docs_section: &str) -> String {
    let cond = start_condition.unwrap_or("(the function's precondition)");
    let elab = if elaboration.is_empty() {
        String::new()
    } else {
        format!("\n\nPrevious problems to avoid:\n{}", elaboration)
    };
    format!(
        r#"Analyze the following Rust code piece and produce verification formulas.

Code:
```
{}
```

Context (called functions and their specifications):
{}

Starting condition: {}
{}
{}

Produce SMT-LIB2 formulas in ```smt2 code blocks or Python/Z3 scripts in ```py code blocks. Each formula should be UNSAT if the code is correct."#,
        code, context, cond, elab, docs_section
    )
}

pub fn fixer_system() -> String {
    r#"You are an SMT formula fixer. Given an SMT-LIB2 formula that caused a solver error, fix the formula so it can be checked by the solver.

Rules:
- Produce the fixed formula in a ```smt2 code block for SMT-LIB2, or ```py code block for Python/Z3.
- Keep the same verification intent as the original formula.
- Do NOT include panics due to running out of memory (OOM) or allocation failures in the verification scope.
- If the formula cannot be fixed, explain why."#.to_string()
}

pub fn fixer_user(formula: &str, error: &str) -> String {
    format!(
        r#"The following SMT formula caused an error when checked:

```
{}
```

Error output:
```
{}
```

Please fix the formula."#,
        formula, error
    )
}

pub fn judge_system() -> String {
    format!(
        r##"You are a verification judge. Given a Rust code piece, its verification formulas, and the solver results, determine if the verification is reasonable.

You must check two things:
1. The formulas correctly represent the code and its correctness conditions — including that preconditions of unsafe operations are satisfied, and that the code cannot panic or fail runtime assertions. Panics due to running out of memory (OOM) or allocation failures should NOT be considered — these are out of scope.
2. Any SAT or UNSAT result is due to the actual code behavior, not due to imprecision or over-approximation in the formula. If the formula is too imprecise (e.g., uses uninterpreted functions where concrete semantics are needed, or abstracts away relevant state), it is NOT reasonable even if the solver says UNSAT.

OUTPUT FORMAT (CRITICAL — violating this wastes resources):
- If the verification IS reasonable, your ENTIRE response must be the single word: {JUDGE_REASONABLE}
  Do NOT add any explanation, analysis, preamble, or trailing text.
  Do NOT write any heading before the word.
  The word {JUDGE_REASONABLE} must be the ONLY text in your response.
- If the verification is NOT reasonable, write your explanation of the problems.
  Do NOT include the word {JUDGE_REASONABLE} anywhere in your explanation."##,
        JUDGE_REASONABLE = refactor_check_core::consts::JUDGE_REASONABLE,
    )
}

pub fn judge_user(code: &str, formulas_and_results: &str, docs_section: &str) -> String {
    format!(
        r#"Determine if the following verification is REASONABLE.

Code:
```
{}
```

Formulas and results:
{}
{}

Check:
1. Do the formulas correctly encode the code and its correctness conditions?
2. Are the SAT/UNSAT results due to the actual code, not formula imprecision?

If both checks pass, your ENTIRE response must be the single word REASONABLE — nothing else, no explanation, no preamble.
If not reasonable, explain the problems but do NOT use the word REASONABLE anywhere."#,
        code, formulas_and_results, docs_section
    )
}

pub fn judge_retry(previous_response: &str) -> String {
    format!(
        r#"Your previous response was:

{prev}

Your response contained the word 'REASONABLE' but also other text. Please either:
- Reply ONLY with the word REASONABLE (nothing else), or
- Reply with an explanation that does NOT contain the word 'REASONABLE' at all, in case the code is not reasonable."#,
        prev = previous_response,
    )
}

pub fn analyzer_system() -> String {
    r#"You are a verification problem analyzer. When verification of a code piece fails, you must determine the root cause by analyzing the entire call chain, not just the failing function in isolation.

Common patterns where the root cause is elsewhere:
- Function f fails verification because it cannot satisfy the precondition of its callee g. But that precondition may actually stem from an overly strict precondition in g's callee h (which f does not even call directly). In this case, the fix is to relax the preconditions of h, not to change f or g.
- A loop invariant is too strong because a helper function called inside the loop has unnecessarily strict preconditions. The fix may be to weaken the helper's preconditions.
- An assertion in a function fails because a caller passes values that violate an undocumented assumption. The fix may be to document and propagate a precondition upward through the call chain.

Always trace the root cause through the full dependency chain before proposing a fix.

Reply RETRY if conditions anywhere in the project need to be adjusted (including functions not directly involved in the failing verification), or describe the bug if one is found."#.to_string()
}

pub fn analyzer_user(code: &str, sat_models: &[String], unknown_formulas: &[String], elaboration: &str, docs_section: &str) -> String {
    let sat = if sat_models.is_empty() {
        "None".to_string()
    } else {
        sat_models.iter().map(|m| format!("```\n{}\n```", m)).collect::<Vec<_>>().join("\n\n")
    };
    let unknown = if unknown_formulas.is_empty() {
        "None".to_string()
    } else {
        unknown_formulas.iter().map(|f| format!("```\n{}\n```", f)).collect::<Vec<_>>().join("\n\n")
    };
    format!(
        r#"Analyze the following unverified code piece and trace the root cause through the full call chain:

Code:
```
{}
```

SAT models (counterexamples):
{}

Unknown formulas:
{}

Previous elaboration:
{}
{}

Do NOT fix only the failing function in isolation. Trace the dependency chain to find where conditions should actually be changed. For example:
- If f cannot satisfy g's precondition, check whether g's precondition is too strict because of g's callee h.
- If an assertion fails, check whether the caller should have a precondition that was never documented.

Reply RETRY if conditions anywhere in the project need to be adjusted, or describe the bug."#,
        code, sat, unknown, elaboration, docs_section
    )
}

pub fn splitter_system() -> String {
    r#"You are a code splitter for formal verification. Given a Rust function, split it into code pieces at key lines.

Key lines are:
- The function entry point (start of the function body)
- Loop headers (while, for, loop)
- Function exit points (return statements, end of function)

Each code piece spans from one key line to the next reached key line. In loops, this may be until the next implicit continue (reaching back to the same loop header).

For each code piece, output:
```
// Start point: <file:line_number>
<lines of code>
// Handover point: <file:line_number>
```

Rules:
- Each piece should be a contiguous block of code from one key line to the next.
- For branches (if/else, match arms), include all branches in the same piece from the start key line to the handover key line.
- Add explicit return statements at function exit points if not present.
- Preserve the original code exactly — do not modify, simplify, or add code.
- The last piece should end with `// Handover point: <file:line_number>` where line_number is the end of the function."#.to_string()
}

pub fn splitter_user(file: &str, function_name: &str, code: &str, start_line: u32) -> String {
    format!(
        r#"Split the following Rust function into code pieces at key lines.

File: {}
Function: {} (starts at line {})
Code:
```
{}
```

Split this function at key lines (entry point, loop headers, exit points). Output each code piece with start and handover point comments."#,
        file, function_name, start_line, code
    )
}