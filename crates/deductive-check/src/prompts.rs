pub fn formalizer_system() -> String {
    r#"You are a formal verification expert specializing in Rust code. Your task is to analyze Rust code pieces and produce SMT-LIB2 formulas that verify the code's correctness.

For each code piece, produce one or more verification formulas that, if UNSAT (unsatisfiable), prove the code is correct.

Rules:
- Produce formulas in ```smt2 code blocks for SMT-LIB2, or ```py code blocks for Python/Z3 scripts.
- Each formula should have a comment explaining what property it verifies.
- Assume that function preconditions hold at the start point.
- Verify that for every function call, the called function's preconditions are satisfied.
- Verify that all handover points' conditions hold.
- Parameters passed as &T to functions should be assumed not to be modified by the function.
- If a formula is too complex, you may split it into subproblems with a side-condition.
- Python/Z3 scripts must output an SMT-LIB2 formula + explanation on stdout, not call the solver directly."#.to_string()
}

pub fn formalizer_user(code: &str, context: &str, start_condition: Option<&str>, elaboration: &str) -> String {
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

Produce SMT-LIB2 formulas in ```smt2 code blocks or Python/Z3 scripts in ```py code blocks. Each formula should be UNSAT if the code is correct."#,
        code, context, cond, elab
    )
}

pub fn fixer_system() -> String {
    r#"You are an SMT formula fixer. Given an SMT-LIB2 formula that caused a solver error, fix the formula so it can be checked by the solver.

Rules:
- Produce the fixed formula in a ```smt2 code block for SMT-LIB2, or ```py code block for Python/Z3.
- Keep the same verification intent as the original formula.
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
    r#"You are a verification judge. Given a Rust code piece, its verification formulas, and the solver results, determine if the verification is REASONABLE.

Reply REASONABLE if the formulas together imply the correctness of the code piece w.r.t. the conditions. Explain any problems that should be improved otherwise."#.to_string()
}

pub fn judge_user(code: &str, formulas_and_results: &str) -> String {
    format!(
        r#"Determine if the following verification is REASONABLE.

Code:
```
{}
```

Formulas and results:
{}

Reply REASONABLE if the formulas together imply the correctness of the code piece. Otherwise, explain what problems need to be improved."#,
        code, formulas_and_results
    )
}

pub fn analyzer_system() -> String {
    r#"You are a verification problem analyzer. Given an unverified piece and its solver results, determine whether the problems are due to incorrect conditions (too weak or too strong), and suggest fixes.

Reply RETRY if conditions need to be adjusted, or describe the bug if one is found."#.to_string()
}

pub fn analyzer_user(code: &str, sat_models: &[String], unknown_formulas: &[String], elaboration: &str) -> String {
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
        r#"Analyze the following unverified code piece:

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

Determine whether the problems are due to incorrect conditions or a bug. Reply RETRY if conditions need to be adjusted, or describe the bug."#,
        code, sat, unknown, elaboration
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