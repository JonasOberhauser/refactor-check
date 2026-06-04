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