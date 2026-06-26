use std::sync::Arc;

use dashmap::DashMap;
use refactor_check_core::context_id::ContextId;
use refactor_check_core::message_log::MessageLog;
use refactor_check_core::phase_tracker::{DefaultPhaseTracker, PhaseTracker};

use crate::code_piece::{DeductiveCodePiece, FunctionId};
use crate::formula::{Formula, FormulaSource};
use crate::phase::{CodePiecePhase, FormulaPhase};

pub trait DeductivePieceManager: Send + Sync {
    fn new_piece(
        &self,
        parent_ctx: &ContextId,
        file: std::path::PathBuf,
        function_id: FunctionId,
        start_line: u32,
        end_line: u32,
        code: String,
    ) -> (DeductiveCodePiece, ContextId);

    fn advance_piece(&self, ctx: &ContextId, from: Option<CodePiecePhase>, to: CodePiecePhase);
    fn expect_piece_phase_and_set(&self, ctx: &ContextId, valid_from: &[CodePiecePhase], to: CodePiecePhase);
    fn enter_piece_formalizer(&self, ctx: &ContextId, to: CodePiecePhase);
    fn piece_phase(&self, ctx: &ContextId) -> Option<CodePiecePhase>;

    fn new_formula(
        &self,
        parent_ctx: &ContextId,
        content: String,
        source: FormulaSource,
        iteration: u32,
    ) -> (Formula, ContextId);

    fn advance_formula(&self, ctx: &ContextId, from: Option<FormulaPhase>, to: FormulaPhase);
    fn expect_formula_phase_and_set(&self, ctx: &ContextId, valid_from: &[FormulaPhase], to: FormulaPhase);
    fn formula_phase(&self, ctx: &ContextId) -> Option<FormulaPhase>;

    fn store_function_docs(&self, function_id: FunctionId, docs: String);
    fn get_function_docs(&self, function_id: &FunctionId) -> String;
}

pub struct DefaultDeductivePieceManager {
    piece_tracker: DefaultPhaseTracker<CodePiecePhase>,
    formula_tracker: DefaultPhaseTracker<FormulaPhase>,
    function_docs: DashMap<FunctionId, String>,
    message_log: Arc<MessageLog>,
}

impl DefaultDeductivePieceManager {
    #[must_use]
    pub fn new(message_log: Arc<MessageLog>) -> Self {
        Self {
            piece_tracker: DefaultPhaseTracker::new(),
            formula_tracker: DefaultPhaseTracker::new(),
            function_docs: DashMap::new(),
            message_log,
        }
    }
}

impl Default for DefaultDeductivePieceManager {
    fn default() -> Self {
        Self::new(Arc::new(MessageLog::new()))
    }
}

impl DeductivePieceManager for DefaultDeductivePieceManager {
    fn new_piece(
        &self,
        parent_ctx: &ContextId,
        file: std::path::PathBuf,
        function_id: FunctionId,
        start_line: u32,
        end_line: u32,
        code: String,
    ) -> (DeductiveCodePiece, ContextId) {
        let ctx = parent_ctx.new_child();
        let piece = DeductiveCodePiece::new(
            file,
            function_id,
            start_line,
            end_line,
            code,
        );
        assert!(piece.type_invariant(), "type invariant violated: code piece {} has empty code", ctx);
        self.piece_tracker.advance(&ctx, None, CodePiecePhase::Open);
        self.message_log.set_status(ctx.to_string(), CodePiecePhase::Open.to_string());
        (piece, ctx)
    }

    fn advance_piece(&self, ctx: &ContextId, from: Option<CodePiecePhase>, to: CodePiecePhase) {
        self.piece_tracker.advance(ctx, from, to);
        self.message_log.set_status(ctx.to_string(), to.to_string());
    }

    fn expect_piece_phase_and_set(&self, ctx: &ContextId, valid_from: &[CodePiecePhase], to: CodePiecePhase) {
        self.piece_tracker.expect_any_and_set(ctx, valid_from, to);
        self.message_log.set_status(ctx.to_string(), to.to_string());
    }

    fn enter_piece_formalizer(&self, ctx: &ContextId, to: CodePiecePhase) {
        self.piece_tracker.expect_any_and_set(
            ctx,
            &[CodePiecePhase::Open, CodePiecePhase::GetContext, CodePiecePhase::Formalizer, CodePiecePhase::Check],
            to,
        );
        self.message_log.set_status(ctx.to_string(), to.to_string());
    }

    fn piece_phase(&self, ctx: &ContextId) -> Option<CodePiecePhase> {
        self.piece_tracker.get_phase(ctx)
    }

    fn new_formula(
        &self,
        parent_ctx: &ContextId,
        content: String,
        source: FormulaSource,
        iteration: u32,
    ) -> (Formula, ContextId) {
        let ctx = parent_ctx.new_child();
        let formula = Formula::new(content, source, iteration);
        self.formula_tracker.advance(&ctx, None, FormulaPhase::Open);
        self.message_log.set_status(ctx.to_string(), FormulaPhase::Open.to_string());
        (formula, ctx)
    }

    fn advance_formula(&self, ctx: &ContextId, from: Option<FormulaPhase>, to: FormulaPhase) {
        self.formula_tracker.advance(ctx, from, to);
        self.message_log.set_status(ctx.to_string(), to.to_string());
    }

    fn expect_formula_phase_and_set(&self, ctx: &ContextId, valid_from: &[FormulaPhase], to: FormulaPhase) {
        self.formula_tracker.expect_any_and_set(ctx, valid_from, to);
        self.message_log.set_status(ctx.to_string(), to.to_string());
    }

    fn formula_phase(&self, ctx: &ContextId) -> Option<FormulaPhase> {
        self.formula_tracker.get_phase(ctx)
    }

    fn store_function_docs(&self, function_id: FunctionId, docs: String) {
        self.function_docs.insert(function_id, docs);
    }

    fn get_function_docs(&self, function_id: &FunctionId) -> String {
        self.function_docs.get(function_id).map(|g| g.value().clone()).unwrap_or_default()
    }
}

#[cfg(test)]
pub fn test_pm() -> DefaultDeductivePieceManager {
    DefaultDeductivePieceManager::new(Arc::new(MessageLog::new()))
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
        let root = ContextId::root();
        let (_piece, ctx) = pm.new_piece(
            root,
            PathBuf::from("test.rs"),
            make_function_id("foo"),
            1,
            10,
            "code".to_string(),
        );

        assert_eq!(pm.piece_phase(&ctx), Some(CodePiecePhase::Open));

        pm.advance_piece(&ctx, Some(CodePiecePhase::Open), CodePiecePhase::GetContext);
        assert_eq!(pm.piece_phase(&ctx), Some(CodePiecePhase::GetContext));

        pm.advance_piece(&ctx, Some(CodePiecePhase::GetContext), CodePiecePhase::Formalizer);
        assert_eq!(pm.piece_phase(&ctx), Some(CodePiecePhase::Formalizer));

        pm.advance_piece(&ctx, Some(CodePiecePhase::Formalizer), CodePiecePhase::Check);
        assert_eq!(pm.piece_phase(&ctx), Some(CodePiecePhase::Check));

        pm.advance_piece(&ctx, Some(CodePiecePhase::Check), CodePiecePhase::Judge);
        assert_eq!(pm.piece_phase(&ctx), Some(CodePiecePhase::Judge));

        pm.advance_piece(&ctx, Some(CodePiecePhase::Judge), CodePiecePhase::Closed);
        assert_eq!(pm.piece_phase(&ctx), Some(CodePiecePhase::Closed));
    }

    #[test]
    #[should_panic(expected = "expected")]
    fn test_piece_invalid_transition_panics() {
        let pm = test_pm();
        let root = ContextId::root();
        let (_piece, ctx) = pm.new_piece(
            root,
            PathBuf::from("test.rs"),
            make_function_id("bar"),
            1,
            5,
            "code".to_string(),
        );

        pm.advance_piece(&ctx, Some(CodePiecePhase::Formalizer), CodePiecePhase::Check);
    }

    #[test]
    fn test_formula_phase_transitions() {
        let pm = test_pm();
        let root = ContextId::root();
        let (_piece, ctx) = pm.new_piece(
            root,
            PathBuf::from("test.rs"),
            make_function_id("baz"),
            1,
            5,
            "code".to_string(),
        );
        let (_formula, fctx) = pm.new_formula(&ctx, "(check-sat)".to_string(), FormulaSource::SmtLib, 0);

        assert_eq!(pm.formula_phase(&fctx), Some(FormulaPhase::Open));

        pm.advance_formula(&fctx, Some(FormulaPhase::Open), FormulaPhase::Check);
        assert_eq!(pm.formula_phase(&fctx), Some(FormulaPhase::Check));

        pm.advance_formula(&fctx, Some(FormulaPhase::Check), FormulaPhase::ClosedUnsat);
        assert_eq!(pm.formula_phase(&fctx), Some(FormulaPhase::ClosedUnsat));
    }

    #[test]
    fn test_formula_fix_loop() {
        let pm = test_pm();
        let root = ContextId::root();
        let (_piece, ctx) = pm.new_piece(
            root,
            PathBuf::from("test.rs"),
            make_function_id("qux"),
            1,
            5,
            "code".to_string(),
        );
        let (_formula, fctx) = pm.new_formula(&ctx, "(check-sat)".to_string(), FormulaSource::SmtLib, 0);

        pm.advance_formula(&fctx, Some(FormulaPhase::Open), FormulaPhase::Check);

        pm.advance_formula(&fctx, Some(FormulaPhase::Check), FormulaPhase::Fix);
        assert_eq!(pm.formula_phase(&fctx), Some(FormulaPhase::Fix));

        pm.expect_formula_phase_and_set(&fctx, &[FormulaPhase::Fix], FormulaPhase::Check);
        assert_eq!(pm.formula_phase(&fctx), Some(FormulaPhase::Check));

        pm.advance_formula(&fctx, Some(FormulaPhase::Check), FormulaPhase::ClosedSat);
        assert_eq!(pm.formula_phase(&fctx), Some(FormulaPhase::ClosedSat));
    }

    #[test]
    fn test_formula_fix_loop_with_translation_retry() {
        let pm = test_pm();
        let root = ContextId::root();
        let (_piece, ctx) = pm.new_piece(
            root,
            PathBuf::from("test.rs"),
            make_function_id("fix_retry"),
            1,
            5,
            "code".to_string(),
        );
        let (_formula, fctx) = pm.new_formula(&ctx, "(check-sat)".to_string(), FormulaSource::SmtLib, 0);

        pm.advance_formula(&fctx, Some(FormulaPhase::Open), FormulaPhase::Check);
        pm.advance_formula(&fctx, Some(FormulaPhase::Check), FormulaPhase::Fix);
        assert_eq!(pm.formula_phase(&fctx), Some(FormulaPhase::Fix));

        // Fix attempt 1: fixer returns formula, but translation fails.
        // Formula stays in Fix — no premature transition to Check.

        // Fix attempt 2: fixer returns formula, translation succeeds.
        pm.expect_formula_phase_and_set(&fctx, &[FormulaPhase::Fix], FormulaPhase::Check);
        pm.advance_formula(&fctx, Some(FormulaPhase::Check), FormulaPhase::ClosedUnsat);

        assert_eq!(pm.formula_phase(&fctx), Some(FormulaPhase::ClosedUnsat));
    }

    #[test]
    fn test_enter_piece_formalizer() {
        let pm = test_pm();
        let root = ContextId::root();
        let (_piece, ctx) = pm.new_piece(
            root,
            PathBuf::from("test.rs"),
            make_function_id("quux"),
            1,
            5,
            "code".to_string(),
        );

        pm.enter_piece_formalizer(&ctx, CodePiecePhase::GetContext);
        assert_eq!(pm.piece_phase(&ctx), Some(CodePiecePhase::GetContext));

        pm.enter_piece_formalizer(&ctx, CodePiecePhase::Formalizer);
        assert_eq!(pm.piece_phase(&ctx), Some(CodePiecePhase::Formalizer));

        pm.expect_piece_phase_and_set(&ctx, &[CodePiecePhase::Formalizer], CodePiecePhase::Check);
        assert_eq!(pm.piece_phase(&ctx), Some(CodePiecePhase::Check));
    }
}
