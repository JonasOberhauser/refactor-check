fn splitter_notation_section() -> &'static str {
    r#"

## Splitter Notation Reference

Code pieces from the splitter use special notation to make control flow explicit:

- `if exit <loop_pattern>:` — code that runs when a loop exits normally (loop condition becomes false)
- `else <loop_pattern>:` — the back-edge (loop continues); contains only a handover comment
- `// break` + `goto <label>;` — replaces Rust `break`; label marks the landing point in the `if exit` block of the enclosing loop that the break targets
- `/* Next handover point: <loop header> <assertions> // Start point: file:line */` — describes conditions that hold at the start of the next code piece
- `/* handover point: <loop header> <assertions> // Start point: file:line */` — same as above, used inside `else` back-edge blocks
- `/* <loop header> */` before `// Start point:` — indicates which loop this piece starts at

When analyzing code pieces, treat each `if exit`/`else` pair as covering all possible exits from the loop body. The `if exit` block handles normal loop exit and break (via goto). The `else` block represents the back edge where execution returns to the loop header.
"#
}

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
- Python/Z3 scripts must output an SMT-LIB2 formula + explanation on stdout, not call the solver directly."#.to_string() + splitter_notation_section()
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

pub fn fixer_user(formula: &str, error: &str, context: &str, docs_section: &str) -> String {
    format!(
        r#"The following SMT formula caused an error when checked:

```
{}
```

Error output:
```
{}
```

Context (called functions and their specifications):
{}
{}
Please fix the formula."#,
        formula, error, context, docs_section
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
    ) + splitter_notation_section()
}

pub fn judge_user(code: &str, formulas_and_results: &str, context: &str, docs_section: &str) -> String {
    format!(
        r#"Determine if the following verification is REASONABLE.

Code:
```
{}
```

Context (called functions and their specifications):
{}
{}

Formulas and results:
{}

Check:
1. Do the formulas correctly encode the code and its correctness conditions?
2. Are the SAT/UNSAT results due to the actual code, not formula imprecision?

If both checks pass, your ENTIRE response must be the single word REASONABLE — nothing else, no explanation, no preamble.
If not reasonable, explain the problems but do NOT use the word REASONABLE anywhere."#,
        code, context, docs_section, formulas_and_results
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

Reply RETRY if conditions anywhere in the project need to be adjusted (including functions not directly involved in the failing verification), or describe the bug if one is found."#.to_string() + splitter_notation_section()
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
    r#"You are a code splitter for formal verification. Given a Rust function with line numbers, split it into code pieces at key lines.

## Key Lines

Key lines are:
- Function entry point (start of the function body)
- Loop headers (for, while, loop)

NOT key lines (they belong to whatever piece they appear in):
- Assertions (assert!, assert_eq!, assert_ne!, debug_assert!, etc.)
- Break statements
- Return statements

## Rules

1. **Every key line gets at least one piece.** A piece spans from its key line to the next key line reached along every execution path. More pieces are allowed if they cover every code path to the next piece.

2. **Piece format:**
    /* <starting loop header> */    ← only if piece starts at a loop header
    // Start point: <file:line>
    <code>
    // Handover point: <file:line>
    /* Next handover point:
       <next loop header>
       <assertions inside that loop>
       // Start point: <file:line>
    */                                ← only if handing over to a loop

3. **No enclosing loop context at top of piece.** Only show the starting loop's header in the /* ... */ block comment before // Start point:. Do NOT include enclosing/outer loop headers.

4. **Outer→inner transition piece.** When an outer loop contains an inner loop, the code between the outer loop header and the inner loop header forms a piece. When there is NO code between them, still create a piece (with just start/handover comments).

5. **Inner loop piece carries all subsequent code.** The piece starting at the inner loop header must contain ALL code after the inner loop body until the next key line, including code in the outer loop after the inner loop (outer loop continue/exit logic).

6. **if exit / else notation for loop exits.** After the loop body code, write:
    - if exit <loop_pattern>: — code block for when the loop exits normally
    - else <loop_pattern>: — code block for when the loop continues (back edge)
    Use the loop header pattern to disambiguate nested loops (e.g., if exit _ in 0..50:, else while idx < data.len():).

7. **Break notation.** Replace break with // break followed by goto <label>;. The label is placed at the landing point inside the appropriate if exit <outer_loop>: block. Break is NOT a key line.

8. **Short else comments.** The else <loop>: block contains only a short handover comment:
    /* handover point: <loop header>
       <assertions at start of that loop's body>
       // Start point: <file:line>
    */
    Do NOT repeat full code paths or inner-loop handover details.

9. **Next handover point comments.** When a piece's handover point is a loop, add a /* Next handover point: ... */ comment after // Handover point: describing what conditions hold at the start of the next piece: the loop header, assertions at the start of the loop body, and the start point.

10. **Assertions are not key lines.** Assertions belong to whatever piece they appear in. They do not start new pieces.

11. **Explicit returns.** All function exit points must use explicit return. If the original code has an implicit return (e.g., count at the end), write return count;.

## Example 1 — With invariants (#[requires], type invariant assertions)

Source (example.rs):
```text
 1: #[requires(items.len() <= 1000)]
 2: pub fn process_all(items: &mut Vec<Item>, threshold: i32) -> usize {
 3:     let mut count = 0;
 4:     for item in items.iter_mut() {
 5:         for _ in 0..50 {
 6:             if item.value >= threshold {
 7:                 break;
 8:             }
 9:             assert!(item.value < item.max_value);
10:             item.inc();
11:         }
12:         assert!(count < items.len());
13:         count += 1;
14:     }
15:     count
16: }
```

Piece 1 (entry → outer loop):
```
// Start point: example.rs:2
    let mut count = 0;
// Handover point: example.rs:4
/* Next handover point:
   for item in items.iter_mut() {
       // Start point: example.rs:4
   }
*/
```

Piece 2 (outer → inner):
```
/* for item in items.iter_mut() { */
// Start point: example.rs:4
        // (no code before inner loop)
// Handover point: example.rs:5
/* Next handover point:
   for _ in 0..50 {
       assert!(item.value < item.max_value);
       // Start point: example.rs:5
   }
*/
```

Piece 3 (inner loop body):
```
/* for _ in 0..50 { */
// Start point: example.rs:5
            if item.value >= threshold {
                // break
                goto inner_exit;
            }
            assert!(item.value < item.max_value);
            item.inc();
        if exit _ in 0..50: {
            inner_exit:
            assert!(count < items.len());
            count += 1;
            if exit item in items.iter_mut(): {
                return count;
// Handover point: example.rs:16
            } else item in items.iter_mut(): {
                /* handover point: for item in items.iter_mut() {
                   // Start point: example.rs:4
                } */
            }
        } else _ in 0..50: {
            /* handover point: for _ in 0..50 {
               assert!(item.value < item.max_value);
               // Start point: example.rs:5
            } */
        }
```

## Example 2 — Without invariants (plain assertions only)

Source (example.rs):
```text
20: pub fn find_and_update(data: &mut [Entry], target: u32) -> bool {
21:     let mut found = false;
22:     let mut idx = 0;
23:     while idx < data.len() {
24:         assert!(idx < data.len());
25:         for j in 0..3 {
26:             assert!(j < 3);
27:             data[idx].flags[j] += 1;
28:         }
29:         if data[idx].key == target {
30:             found = true;
31:             break;
32:         }
33:         idx += 1;
34:     }
35:     found
36: }
```

Piece 1 (entry → while):
```
// Start point: example.rs:20
    let mut found = false;
    let mut idx = 0;
// Handover point: example.rs:23
/* Next handover point:
   while idx < data.len() {
       assert!(idx < data.len());
       // Start point: example.rs:23
   }
*/
```

Piece 2 (while → for):
```
/* while idx < data.len() { */
// Start point: example.rs:23
        assert!(idx < data.len());
// Handover point: example.rs:25
/* Next handover point:
   for j in 0..3 {
       assert!(j < 3);
       // Start point: example.rs:25
   }
*/
```

Piece 3 (for loop body):
```
/* for j in 0..3 { */
// Start point: example.rs:25
            assert!(j < 3);
            data[idx].flags[j] += 1;
        if exit j in 0..3: {
            if data[idx].key == target {
                found = true;
                // break
                goto while_exit;
            }
            idx += 1;
            if exit while idx < data.len(): {
                while_exit:
                return found;
// Handover point: example.rs:36
            } else while idx < data.len(): {
                /* handover point: while idx < data.len() {
                   assert!(idx < data.len());
                   // Start point: example.rs:23
                } */
            }
        } else j in 0..3: {
            /* handover point: for j in 0..3 {
               assert!(j < 3);
               // Start point: example.rs:25
            } */
        }
```

Output each piece in a separate ```code block. Do not add any extra explanation."#.to_string()
}

pub fn splitter_user(file: &str, function_name: &str, code: &str, start_line: u32) -> String {
    let numbered_code = code
        .lines()
        .enumerate()
        .map(|(i, line)| format!("{:>4}: {}", start_line + i as u32, line))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"Split the following Rust function into code pieces at key lines.

File: {}
Function: {} (starts at line {})
Code:
```
{}
```

Output each piece in a separate ```code block following the rules and format from the system prompt."#,
        file, function_name, start_line, numbered_code
    )
}