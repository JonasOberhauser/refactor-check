use crate::phase::PiecePhase;
use crate::piece::CodePiece;
use refactor_check_core::context_id::ContextId;
use refactor_check_core::phase_tracker::{DefaultPhaseTracker, PhaseTracker};

pub trait PieceManager: Send + Sync {
    fn new_piece(&self, parent_ctx: &ContextId, label: &str, before: &str, after: &str) -> CodePiece;
    fn advance(&self, ctx: &ContextId, from: Option<PiecePhase>, to: PiecePhase);
    fn expect_any_and_set(&self, ctx: &ContextId, valid_from: &[PiecePhase], to: PiecePhase);
    fn enter_generation(&self, ctx: &ContextId, to: PiecePhase);
    fn get_phase(&self, ctx: &ContextId) -> Option<PiecePhase>;
}

pub struct DefaultPieceManager {
    tracker: DefaultPhaseTracker<PiecePhase>,
}

impl DefaultPieceManager {
    pub fn new() -> Self {
        Self {
            tracker: DefaultPhaseTracker::new(),
        }
    }
}

impl PieceManager for DefaultPieceManager {
    fn new_piece(&self, parent_ctx: &ContextId, label: &str, before: &str, after: &str) -> CodePiece {
        let ctx = parent_ctx.new_child();
        let piece = CodePiece::new(ctx, label, before, after);
        piece.with_ctx(|c| self.tracker.advance(c, None, PiecePhase::Open));
        piece
    }

    fn advance(&self, ctx: &ContextId, from: Option<PiecePhase>, to: PiecePhase) {
        self.tracker.advance(ctx, from, to);
    }

    fn expect_any_and_set(&self, ctx: &ContextId, valid_from: &[PiecePhase], to: PiecePhase) {
        self.tracker.expect_any_and_set(ctx, valid_from, to);
    }

    fn enter_generation(&self, ctx: &ContextId, to: PiecePhase) {
        self.tracker.expect_any_and_set(ctx, &[PiecePhase::Open, PiecePhase::Forming, PiecePhase::Fixing], to);
    }

    fn get_phase(&self, ctx: &ContextId) -> Option<PiecePhase> {
        self.tracker.get_phase(ctx)
    }
}

impl Default for DefaultPieceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase::PiecePhase;

    fn test_pm() -> DefaultPieceManager {
        DefaultPieceManager::new()
    }

    #[test]
    fn enter_generation_absent_entry_inserts_without_panic() {
        let pm = test_pm();
        let root = ContextId::root();
        let piece = pm.new_piece(root, "test", "before", "after");
        piece.with_ctx(|ctx| {
            pm.enter_generation(ctx, PiecePhase::Fixing);
            assert_eq!(pm.get_phase(ctx), Some(PiecePhase::Fixing));
        });
    }

    #[test]
    fn enter_generation_absent_entry_inserts_forming() {
        let pm = test_pm();
        let root = ContextId::root();
        let piece = pm.new_piece(root, "test", "before", "after");
        piece.with_ctx(|ctx| {
            pm.enter_generation(ctx, PiecePhase::Forming);
            assert_eq!(pm.get_phase(ctx), Some(PiecePhase::Forming));
        });
    }

    #[test]
    fn enter_generation_transitions_from_open() {
        let pm = test_pm();
        let root = ContextId::root();
        let piece = pm.new_piece(root, "test", "before", "after");
        piece.with_ctx(|ctx| {
            pm.advance(ctx, None, PiecePhase::Solving);
            pm.advance(ctx, Some(PiecePhase::Solving), PiecePhase::Open);
            pm.enter_generation(ctx, PiecePhase::Fixing);
            assert_eq!(pm.get_phase(ctx), Some(PiecePhase::Fixing));
        });
    }

    #[test]
    fn enter_generation_transitions_from_forming() {
        let pm = test_pm();
        let root = ContextId::root();
        let piece = pm.new_piece(root, "test", "before", "after");
        piece.with_ctx(|ctx| {
            pm.enter_generation(ctx, PiecePhase::Forming);
            pm.enter_generation(ctx, PiecePhase::Fixing);
            assert_eq!(pm.get_phase(ctx), Some(PiecePhase::Fixing));
        });
    }

    #[test]
    fn enter_generation_transitions_from_fixing() {
        let pm = test_pm();
        let root = ContextId::root();
        let piece = pm.new_piece(root, "test", "before", "after");
        piece.with_ctx(|ctx| {
            pm.enter_generation(ctx, PiecePhase::Fixing);
            pm.enter_generation(ctx, PiecePhase::Forming);
            assert_eq!(pm.get_phase(ctx), Some(PiecePhase::Forming));
        });
    }

    #[test]
    #[should_panic(expected = "expected one of [Open, Forming, Fixing] but was")]
    fn enter_generation_panics_from_solving() {
        let pm = test_pm();
        let root = ContextId::root();
        let piece = pm.new_piece(root, "test", "before", "after");
        piece.with_ctx(|ctx| {
            pm.advance(ctx, None, PiecePhase::Solving);
            pm.enter_generation(ctx, PiecePhase::Fixing);
        });
    }

    #[test]
    fn enter_generation_new_piece_ids_at_nonzero_iteration() {
        let pm = test_pm();
        let root = ContextId::root();
        let p1 = pm.new_piece(root, "first", "a", "b");
        p1.with_ctx(|ctx| {
            pm.enter_generation(ctx, PiecePhase::Forming);
            pm.expect_any_and_set(ctx, &[PiecePhase::Forming], PiecePhase::Solving);
            pm.advance(ctx, Some(PiecePhase::Solving), PiecePhase::Open);
        });

        let p2 = pm.new_piece(root, "resplit_a", "c", "d");
        let p3 = pm.new_piece(root, "resplit_b", "e", "f");

        assert_ne!(p2.label(), p3.label());

        p2.with_ctx(|ctx| pm.enter_generation(ctx, PiecePhase::Fixing));
        p3.with_ctx(|ctx| pm.enter_generation(ctx, PiecePhase::Fixing));

        p2.with_ctx(|ctx| assert_eq!(pm.get_phase(ctx), Some(PiecePhase::Fixing)));
        p3.with_ctx(|ctx| assert_eq!(pm.get_phase(ctx), Some(PiecePhase::Fixing)));
    }
}
