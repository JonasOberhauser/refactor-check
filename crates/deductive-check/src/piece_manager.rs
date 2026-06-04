use std::sync::atomic::AtomicU64;

use refactor_check_core::phase_tracker::{DefaultPhaseTracker, PhaseTracker};

use crate::code_piece::{DeductiveCodePiece, FunctionId};
use crate::formula::{Formula, FormulaSource};
use crate::phase::{CodePiecePhase, FormulaPhase};

pub trait DeductivePieceManager: Send + Sync {
    fn new_piece(
        &self,
        file: std::path::PathBuf,
        function_id: FunctionId,
        start_line: u32,
        end_line: u32,
        code: String,
        condition_at_start: Option<String>,
    ) -> DeductiveCodePiece;

    fn advance_piece(&self, id: u64, from: Option<CodePiecePhase>, to: CodePiecePhase);
    fn expect_piece_phase_and_set(&self, id: u64, valid_from: &[CodePiecePhase], to: CodePiecePhase);
    fn enter_piece_formalizer(&self, id: u64, to: CodePiecePhase);
    fn piece_phase(&self, id: u64) -> Option<CodePiecePhase>;

    fn new_formula(
        &self,
        piece_id: u64,
        content: String,
        source: FormulaSource,
        iteration: u32,
    ) -> Formula;

    fn advance_formula(&self, id: u64, from: Option<FormulaPhase>, to: FormulaPhase);
    fn expect_formula_phase_and_set(&self, id: u64, valid_from: &[FormulaPhase], to: FormulaPhase);
    fn formula_phase(&self, id: u64) -> Option<FormulaPhase>;
}

pub struct DefaultDeductivePieceManager {
    piece_tracker: DefaultPhaseTracker<CodePiecePhase>,
    formula_tracker: DefaultPhaseTracker<FormulaPhase>,
    piece_counter: AtomicU64,
    formula_counter: AtomicU64,
}

impl DefaultDeductivePieceManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            piece_tracker: DefaultPhaseTracker::new(),
            formula_tracker: DefaultPhaseTracker::new(),
            piece_counter: AtomicU64::new(1),
            formula_counter: AtomicU64::new(1),
        }
    }

    fn next_piece_id(&self) -> u64 {
        self.piece_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn next_formula_id(&self) -> u64 {
        self.formula_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for DefaultDeductivePieceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DeductivePieceManager for DefaultDeductivePieceManager {
    fn new_piece(
        &self,
        file: std::path::PathBuf,
        function_id: FunctionId,
        start_line: u32,
        end_line: u32,
        code: String,
        condition_at_start: Option<String>,
    ) -> DeductiveCodePiece {
        let id = self.next_piece_id();
        let piece = DeductiveCodePiece::with_id(
            id,
            file,
            function_id,
            start_line,
            end_line,
            code,
            condition_at_start,
        );
        self.piece_tracker.advance(id, None, CodePiecePhase::Open);
        piece
    }

    fn advance_piece(&self, id: u64, from: Option<CodePiecePhase>, to: CodePiecePhase) {
        self.piece_tracker.advance(id, from, to);
    }

    fn expect_piece_phase_and_set(&self, id: u64, valid_from: &[CodePiecePhase], to: CodePiecePhase) {
        self.piece_tracker.expect_any_and_set(id, valid_from, to);
    }

    fn enter_piece_formalizer(&self, id: u64, to: CodePiecePhase) {
        self.piece_tracker.upsert(
            id,
            &[CodePiecePhase::Open, CodePiecePhase::GetContext, CodePiecePhase::Formalizer, CodePiecePhase::Check],
            to,
        );
    }

    fn piece_phase(&self, id: u64) -> Option<CodePiecePhase> {
        self.piece_tracker.phases().get(&id).map(|g| *g.value())
    }

    fn new_formula(
        &self,
        piece_id: u64,
        content: String,
        source: FormulaSource,
        iteration: u32,
    ) -> Formula {
        let id = self.next_formula_id();
        let formula = Formula::with_id(id, piece_id, content, source, iteration);
        self.formula_tracker.advance(id, None, FormulaPhase::Open);
        formula
    }

    fn advance_formula(&self, id: u64, from: Option<FormulaPhase>, to: FormulaPhase) {
        self.formula_tracker.advance(id, from, to);
    }

    fn expect_formula_phase_and_set(&self, id: u64, valid_from: &[FormulaPhase], to: FormulaPhase) {
        self.formula_tracker.expect_any_and_set(id, valid_from, to);
    }

    fn formula_phase(&self, id: u64) -> Option<FormulaPhase> {
        self.formula_tracker.phases().get(&id).map(|g| *g.value())
    }
}

#[cfg(test)]
pub fn test_pm() -> DefaultDeductivePieceManager {
    DefaultDeductivePieceManager::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase::{CodePiecePhase, FormulaPhase};
    use std::path::PathBuf;

    fn make_function_id(name: &str) -> FunctionId {
        FunctionId::new(PathBuf::from("test.rs"), name, None, 1)
    }

    #[test]
    fn test_piece_phase_transitions() {
        let pm = test_pm();
        let piece = pm.new_piece(
            PathBuf::from("test.rs"),
            make_function_id("foo"),
            1,
            10,
            "code".to_string(),
            None,
        );
        let id = piece.id();

        assert_eq!(pm.piece_phase(id), Some(CodePiecePhase::Open));

        pm.advance_piece(id, Some(CodePiecePhase::Open), CodePiecePhase::GetContext);
        assert_eq!(pm.piece_phase(id), Some(CodePiecePhase::GetContext));

        pm.advance_piece(id, Some(CodePiecePhase::GetContext), CodePiecePhase::Formalizer);
        assert_eq!(pm.piece_phase(id), Some(CodePiecePhase::Formalizer));

        pm.advance_piece(id, Some(CodePiecePhase::Formalizer), CodePiecePhase::Check);
        assert_eq!(pm.piece_phase(id), Some(CodePiecePhase::Check));

        pm.advance_piece(id, Some(CodePiecePhase::Check), CodePiecePhase::Judge);
        assert_eq!(pm.piece_phase(id), Some(CodePiecePhase::Judge));

        pm.advance_piece(id, Some(CodePiecePhase::Judge), CodePiecePhase::Closed);
        assert_eq!(pm.piece_phase(id), Some(CodePiecePhase::Closed));
    }

    #[test]
    #[should_panic(expected = "expected")]
    fn test_piece_invalid_transition_panics() {
        let pm = test_pm();
        let piece = pm.new_piece(
            PathBuf::from("test.rs"),
            make_function_id("bar"),
            1,
            5,
            "code".to_string(),
            None,
        );
        let id = piece.id();

        pm.advance_piece(id, Some(CodePiecePhase::Formalizer), CodePiecePhase::Check);
    }

    #[test]
    fn test_formula_phase_transitions() {
        let pm = test_pm();
        let piece = pm.new_piece(
            PathBuf::from("test.rs"),
            make_function_id("baz"),
            1,
            5,
            "code".to_string(),
            None,
        );
        let formula = pm.new_formula(piece.id(), "(check-sat)".to_string(), FormulaSource::SmtLib, 0);
        let fid = formula.id();

        assert_eq!(pm.formula_phase(fid), Some(FormulaPhase::Open));

        pm.advance_formula(fid, Some(FormulaPhase::Open), FormulaPhase::Check);
        assert_eq!(pm.formula_phase(fid), Some(FormulaPhase::Check));

        pm.advance_formula(fid, Some(FormulaPhase::Check), FormulaPhase::ClosedUnsat);
        assert_eq!(pm.formula_phase(fid), Some(FormulaPhase::ClosedUnsat));
    }

    #[test]
    fn test_formula_fix_loop() {
        let pm = test_pm();
        let piece = pm.new_piece(
            PathBuf::from("test.rs"),
            make_function_id("qux"),
            1,
            5,
            "code".to_string(),
            None,
        );
        let formula = pm.new_formula(piece.id(), "(check-sat)".to_string(), FormulaSource::SmtLib, 0);
        let fid = formula.id();

        pm.advance_formula(fid, Some(FormulaPhase::Open), FormulaPhase::Check);

        pm.advance_formula(fid, Some(FormulaPhase::Check), FormulaPhase::Fix);
        assert_eq!(pm.formula_phase(fid), Some(FormulaPhase::Fix));

        pm.expect_formula_phase_and_set(fid, &[FormulaPhase::Fix], FormulaPhase::Check);
        assert_eq!(pm.formula_phase(fid), Some(FormulaPhase::Check));

        pm.advance_formula(fid, Some(FormulaPhase::Check), FormulaPhase::ClosedSat);
        assert_eq!(pm.formula_phase(fid), Some(FormulaPhase::ClosedSat));
    }

    #[test]
    fn test_enter_piece_formalizer() {
        let pm = test_pm();
        let piece = pm.new_piece(
            PathBuf::from("test.rs"),
            make_function_id("quux"),
            1,
            5,
            "code".to_string(),
            None,
        );
        let id = piece.id();

        pm.enter_piece_formalizer(id, CodePiecePhase::GetContext);
        assert_eq!(pm.piece_phase(id), Some(CodePiecePhase::GetContext));

        pm.enter_piece_formalizer(id, CodePiecePhase::Formalizer);
        assert_eq!(pm.piece_phase(id), Some(CodePiecePhase::Formalizer));

        pm.expect_piece_phase_and_set(id, &[CodePiecePhase::Formalizer], CodePiecePhase::Check);
        assert_eq!(pm.piece_phase(id), Some(CodePiecePhase::Check));
    }
}