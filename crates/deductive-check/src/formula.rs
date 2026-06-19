use crate::phase::FormulaPhase;

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
                blocks.push((fence_lang.clone(), content));
            }
            continue;
        }

        if in_fence {
            buf.push_str(line);
            buf.push('\n');
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
    l.contains("smt") || l.contains("py") || l.contains("python")
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

fn detect_formula_source(lang: &str) -> FormulaSource {
    let lang_lower = lang.to_lowercase();
    if lang_lower.contains("py") || lang_lower.contains("python") {
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
    fn test_bare_text_not_extracted_without_fences() {
        let response = "(set-logic QF_LIA)\n(check-sat)";
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), 0);
    }

    #[test]
    fn test_rust_code_block_not_treated_as_formula() {
        let response = "Here is the code:\n```rust\nlet x = foo();\nexpect(\"bar\");\n```\nAnd the formula:\n```smt2\n(set-logic ALL)\n(check-sat)\n```";
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), 1);
        assert!(formulas[0].content.contains("(set-logic"));
    }

    #[test]
    fn test_bare_text_not_smt_not_treated_as_formula() {
        let response = "I cannot produce a formula because the code is too complex.";
        let formulas = extract_formulas_from_response(response);
        assert_eq!(formulas.len(), 0);
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