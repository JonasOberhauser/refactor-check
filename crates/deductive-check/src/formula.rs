use std::sync::atomic::{AtomicU64, Ordering};

use crate::phase::FormulaPhase;

static NEXT_FORMULA_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormulaSource {
    SmtLib,
    PyZ3,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Formula {
    id: u64,
    piece_id: u64,
    content: String,
    source: FormulaSource,
    iteration: u32,
}

impl Formula {
    pub(crate) fn with_id(
        id: u64,
        piece_id: u64,
        content: String,
        source: FormulaSource,
        iteration: u32,
    ) -> Self {
        Self {
            id,
            piece_id,
            content,
            source,
            iteration,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn piece_id(&self) -> u64 {
        self.piece_id
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

pub fn next_formula_id() -> u64 {
    NEXT_FORMULA_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone)]
pub struct ExtractedFormula {
    pub content: String,
    pub source: FormulaSource,
}

pub fn extract_formulas_from_response(response: &str) -> Vec<ExtractedFormula> {
    let mut formulas = Vec::new();
    let mut in_fence = false;
    let mut fence_backticks = 0;
    let mut fence_lang = String::new();
    let mut buf = String::new();

    for line in response.lines() {
        let trimmed = line.trim();
        let bt_count = count_backticks(trimmed);

        if bt_count >= 3 && !in_fence {
            in_fence = true;
            fence_backticks = bt_count;
            let after = trimmed.trim_start_matches('`').trim();
            fence_lang = after.to_string();
            buf.clear();
            continue;
        }

        if in_fence && bt_count >= 3 && bt_count >= fence_backticks {
            in_fence = false;
            let content = buf.trim().to_string();
            if !content.is_empty() {
                let source = detect_formula_source(&fence_lang, &content);
                formulas.push(ExtractedFormula { content, source });
            }
            continue;
        }

        if in_fence {
            buf.push_str(line);
            buf.push('\n');
        }
    }

    if formulas.is_empty() {
        let trimmed = response.trim().to_string();
        if !trimmed.is_empty() {
            formulas.push(ExtractedFormula {
                content: trimmed,
                source: FormulaSource::SmtLib,
            });
        }
    }

    formulas
}

fn count_backticks(line: &str) -> usize {
    let trimmed = line.trim();
    let mut count = 0;
    for ch in trimmed.chars() {
        if ch == '`' {
            count += 1;
        } else {
            break;
        }
    }
    count
}

fn detect_formula_source(lang: &str, content: &str) -> FormulaSource {
    let lang_lower = lang.to_lowercase();
    if lang_lower.contains("py") || lang_lower.contains("python") {
        return FormulaSource::PyZ3;
    }
    if lang_lower.contains("smt") {
        return FormulaSource::SmtLib;
    }
    if content.contains("(set-logic")
        || content.contains("(declare-")
        || content.contains("(assert")
        || content.contains("(check-sat)")
    {
        return FormulaSource::SmtLib;
    }
    if content.contains("from z3 import")
        || content.contains("import z3")
        || content.contains("Solver(")
        || content.contains("And(")
        || content.contains("Or(")
        || content.contains("Implies(")
    {
        return FormulaSource::PyZ3;
    }
    FormulaSource::SmtLib
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaResult {
    pub formula_id: u64,
    pub piece_id: u64,
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

    #[test]
    fn test_extract_smt_from_fenced_block() {
        let response = "Here is the formula:\n```smt2\n(set-logic QF_LIA)\n(declare-fun x () Int)\n(assert (= x 0))\n(check-sat)\n```\nDone.";
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), 1);
        assert_eq!(formulas[0].source, FormulaSource::SmtLib);
        assert!(formulas[0].content.contains("(set-logic QF_LIA)"));
    }

    #[test]
    fn test_extract_python_from_fenced_block() {
        let response = "```python\nfrom z3 import *\nx = Int('x')\nprove(x == 0)\n```";
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), 1);
        assert_eq!(formulas[0].source, FormulaSource::PyZ3);
        assert!(formulas[0].content.contains("from z3 import"));
    }

    #[test]
    fn test_extract_multiple_formulas() {
        let response = "```smt2\n(set-logic QF_LIA)\n(check-sat)\n```\nAnd another:\n```smt\n(set-logic QF_LIA)\n(check-sat)\n```";
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), 2);
    }

    #[test]
    fn test_extract_bare_text_when_no_fences() {
        let response = "(set-logic QF_LIA)\n(check-sat)";
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), 1);
        assert_eq!(formulas[0].source, FormulaSource::SmtLib);
    }

    #[test]
    fn test_detect_formula_source_by_language_tag() {
        assert_eq!(detect_formula_source("smt2", ""), FormulaSource::SmtLib);
        assert_eq!(detect_formula_source("python", ""), FormulaSource::PyZ3);
        assert_eq!(detect_formula_source("py", ""), FormulaSource::PyZ3);
    }

    #[test]
    fn test_detect_formula_source_by_content() {
        assert_eq!(detect_formula_source("", "(set-logic QF_LIA)"), FormulaSource::SmtLib);
        assert_eq!(detect_formula_source("", "from z3 import *"), FormulaSource::PyZ3);
    }
}