use crate::phase::FormulaPhase;

const FENCE_BACKTICK_COUNT: usize = 3;
const BACKTICK_CHAR: char = '`';
const NEWLINE_CHAR: char = '\n';
const SMT_LANG_MARKER: &str = "smt";
const PY_LANG_MARKER: &str = "py";
const PYTHON_LANG_MARKER: &str = "python";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormulaSource {
    SmtLib,
    PyZ3,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Formula {
    content: String,
    source: FormulaSource,
    iteration: u32,
}

impl Formula {
    pub(crate) fn new(
        content: String,
        source: FormulaSource,
        iteration: u32,
    ) -> Self {
        Self {
            content,
            source,
            iteration,
        }
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn source(&self) -> &FormulaSource {
        &self.source
    }

    pub fn iteration(&self) -> u32 {
        self.iteration
    }
}

#[derive(Debug, Clone)]
pub struct ExtractedFormula {
    pub content: String,
    pub source: FormulaSource,
}

pub fn extract_fenced_blocks(response: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut in_fence = false;
    let mut fence_backticks = 0;
    let mut fence_lang = String::new();
    let mut buf = String::new();

    for line in response.lines() {
        let trimmed = line.trim();
        let bt_count = count_backticks(trimmed);

        if bt_count >= FENCE_BACKTICK_COUNT && !in_fence {
            in_fence = true;
            fence_backticks = bt_count;
            let after = trimmed.trim_start_matches(BACKTICK_CHAR).trim();
            fence_lang = after.to_string();
            buf.clear();
            continue;
        }

        if in_fence && bt_count >= FENCE_BACKTICK_COUNT && bt_count >= fence_backticks {
            in_fence = false;
            let content = buf.trim().to_string();
            if !content.is_empty() {
                blocks.push((fence_lang.clone(), content));
            }
            continue;
        }

        if in_fence {
            buf.push_str(line);
            buf.push(NEWLINE_CHAR);
        }
    }

    blocks
}

pub fn extract_formulas_from_response(response: &str) -> Vec<ExtractedFormula> {
    extract_fenced_blocks(response)
        .into_iter()
        .filter(|(lang, _)| is_formula_lang(lang))
        .map(|(lang, content)| ExtractedFormula {
            source: detect_formula_source(&lang),
            content,
        })
        .collect()
}

fn is_formula_lang(lang: &str) -> bool {
    let l = lang.to_lowercase();
    l.contains(SMT_LANG_MARKER) || l.contains(PY_LANG_MARKER) || l.contains(PYTHON_LANG_MARKER)
}

fn count_backticks(line: &str) -> usize {
    let trimmed = line.trim();
    let mut count = 0;
    for ch in trimmed.chars() {
            if ch == BACKTICK_CHAR {
            count += 1;
        } else {
            break;
        }
    }
    count
}

fn detect_formula_source(lang: &str) -> FormulaSource {
    let lang_lower = lang.to_lowercase();
    if lang_lower.contains(PY_LANG_MARKER) || lang_lower.contains(PYTHON_LANG_MARKER) {
        FormulaSource::PyZ3
    } else {
        FormulaSource::SmtLib
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaResult {
    pub phase: FormulaPhase,
    pub content: String,
    pub source: FormulaSource,
    pub iteration: u32,
}

impl FormulaResult {
    pub fn is_unsat(&self) -> bool {
        self.phase.is_unsat()
    }

    pub fn is_sat(&self) -> bool {
        self.phase.is_sat()
    }

    pub fn is_unknown(&self) -> bool {
        self.phase.is_unknown()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSE_WITH_SMT_BLOCK: &str = "Here is the formula:\n```smt2\n(set-logic QF_LIA)\n(declare-fun x () Int)\n(assert (= x 0))\n(check-sat)\n```\nDone.";
    const RESPONSE_WITH_PYTHON_BLOCK: &str = "```python\nfrom z3 import *\nx = Int('x')\nprove(x == 0)\n```";
    const RESPONSE_WITH_TWO_SMT_BLOCKS: &str = "```smt2\n(set-logic QF_LIA)\n(check-sat)\n```\nAnd another:\n```smt\n(set-logic QF_LIA)\n(check-sat)\n```";
    const RESPONSE_WITHOUT_FENCES: &str = "(set-logic QF_LIA)\n(check-sat)";
    const RESPONSE_WITH_RUST_AND_SMT_BLOCKS: &str = "Here is the code:\n```rust\nlet x = foo();\nexpect(bar);\n```\nAnd the formula:\n```smt2\n(set-logic ALL)\n(check-sat)\n```";
    const RESPONSE_WITHOUT_FORMULA: &str = "I cannot produce a formula because the code is too complex.";
    const EXPECTED_SMT_FORMULAS: usize = 1;
    const EXPECTED_PYTHON_FORMULAS: usize = 1;
    const EXPECTED_MULTIPLE_FORMULAS: usize = 2;
    const EXPECTED_NO_FORMULAS: usize = 0;
    const FIRST_FORMULA_INDEX: usize = 0;

    #[test]
    fn test_extract_smt_from_fenced_block() {
        let response = RESPONSE_WITH_SMT_BLOCK;
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), EXPECTED_SMT_FORMULAS);
        assert_eq!(formulas[FIRST_FORMULA_INDEX].source, FormulaSource::SmtLib);
        assert!(formulas[FIRST_FORMULA_INDEX].content.contains("(set-logic QF_LIA)"));
    }

    #[test]
    fn test_extract_python_from_fenced_block() {
        let response = RESPONSE_WITH_PYTHON_BLOCK;
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), EXPECTED_PYTHON_FORMULAS);
        assert_eq!(formulas[FIRST_FORMULA_INDEX].source, FormulaSource::PyZ3);
        assert!(formulas[FIRST_FORMULA_INDEX].content.contains("from z3 import"));
    }

    #[test]
    fn test_extract_multiple_formulas() {
        let response = RESPONSE_WITH_TWO_SMT_BLOCKS;
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), EXPECTED_MULTIPLE_FORMULAS);
    }

    #[test]
    fn test_bare_text_not_extracted_without_fences() {
        let response = RESPONSE_WITHOUT_FENCES;
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), EXPECTED_NO_FORMULAS);
    }

    #[test]
    fn test_rust_code_block_not_treated_as_formula() {
        let response = RESPONSE_WITH_RUST_AND_SMT_BLOCKS;
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), EXPECTED_SMT_FORMULAS);
        assert!(formulas[FIRST_FORMULA_INDEX].content.contains("(set-logic"));
    }

    #[test]
    fn test_bare_text_not_smt_not_treated_as_formula() {
        let response = RESPONSE_WITHOUT_FORMULA;
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), EXPECTED_NO_FORMULAS);
    }

    #[test]
    fn test_detect_formula_source_by_language_tag() {
        assert_eq!(detect_formula_source("smt2"), FormulaSource::SmtLib);
        assert_eq!(detect_formula_source("python"), FormulaSource::PyZ3);
        assert_eq!(detect_formula_source("py"), FormulaSource::PyZ3);
    }

    #[test]
    fn test_detect_formula_source_unknown_lang_defaults_to_smt() {
        assert_eq!(detect_formula_source(""), FormulaSource::SmtLib);
    }
}