use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

use crate::phase::PiecePhase;
use crate::states::CodePiece;

pub trait PieceManager: Send + Sync {
    fn new_piece(&self, label: &str, before: &str, after: &str) -> CodePiece;
    fn advance(&self, piece_id: u64, from: Option<PiecePhase>, to: PiecePhase);
    fn expect_any_and_set(&self, piece_id: u64, valid_from: &[PiecePhase], to: PiecePhase);
    fn enter_generation(&self, piece_id: u64, to: PiecePhase);
}

pub struct DefaultPieceManager {
    next_id: AtomicU64,
    phases: DashMap<u64, PiecePhase>,
}

impl DefaultPieceManager {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            phases: DashMap::new(),
        }
    }
}

impl PieceManager for DefaultPieceManager {
    fn new_piece(&self, label: &str, before: &str, after: &str) -> CodePiece {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        CodePiece::with_id(id, label, before, after)
    }

    fn advance(&self, piece_id: u64, from: Option<PiecePhase>, to: PiecePhase) {
        let _ = self
            .phases
            .entry(piece_id)
            .and_modify(|phase| {
                if let Some(expected) = from {
                    assert_eq!(
                        *phase,
                        expected,
                        "piece {piece_id} expected phase {expected:?} but was {phase:?}"
                    );
                }
                *phase = to;
            })
            .or_insert_with(|| {
                assert!(
                    from.is_none(),
                    "piece {piece_id} expected phase {expected:?} but was absent",
                    expected = from.unwrap(),
                );
                to
            });
    }

    fn expect_any_and_set(&self, piece_id: u64, valid_from: &[PiecePhase], to: PiecePhase) {
        self.phases
            .entry(piece_id)
            .and_modify(|phase| {
                assert!(
                    valid_from.contains(phase),
                    "piece {piece_id} expected one of {valid_from:?} but was {phase:?}"
                );
                *phase = to;
            })
            .or_insert_with(|| {
                panic!(
                    "piece {piece_id} expected one of {valid_from:?} but was absent"
                );
            });
    }

    fn enter_generation(&self, piece_id: u64, to: PiecePhase) {
        self.phases
            .entry(piece_id)
            .and_modify(|phase| {
                assert!(
                    matches!(*phase, PiecePhase::Open | PiecePhase::Forming | PiecePhase::Fixing),
                    "piece {piece_id} expected Open/Forming/Fixing but was {phase:?} when entering generation",
                );
                *phase = to;
            })
            .or_insert_with(|| to);
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
        pm.enter_generation(999, PiecePhase::Fixing);
        assert_eq!(*pm.phases.get(&999).unwrap(), PiecePhase::Fixing);
    }

    #[test]
    fn enter_generation_absent_entry_inserts_forming() {
        let pm = test_pm();
        pm.enter_generation(42, PiecePhase::Forming);
        assert_eq!(*pm.phases.get(&42).unwrap(), PiecePhase::Forming);
    }

    #[test]
    fn enter_generation_transitions_from_open() {
        let pm = test_pm();
        let piece = pm.new_piece("test", "before", "after");
        pm.advance(piece.id(), None, PiecePhase::Solving);
        pm.advance(piece.id(), Some(PiecePhase::Solving), PiecePhase::Open);
        pm.enter_generation(piece.id(), PiecePhase::Fixing);
        assert_eq!(*pm.phases.get(&piece.id()).unwrap(), PiecePhase::Fixing);
    }

    #[test]
    fn enter_generation_transitions_from_forming() {
        let pm = test_pm();
        let piece = pm.new_piece("test", "before", "after");
        pm.enter_generation(piece.id(), PiecePhase::Forming);
        pm.enter_generation(piece.id(), PiecePhase::Fixing);
        assert_eq!(*pm.phases.get(&piece.id()).unwrap(), PiecePhase::Fixing);
    }

    #[test]
    fn enter_generation_transitions_from_fixing() {
        let pm = test_pm();
        let piece = pm.new_piece("test", "before", "after");
        pm.enter_generation(piece.id(), PiecePhase::Fixing);
        pm.enter_generation(piece.id(), PiecePhase::Forming);
        assert_eq!(*pm.phases.get(&piece.id()).unwrap(), PiecePhase::Forming);
    }

    #[test]
    #[should_panic(expected = "expected Open/Forming/Fixing but was")]
    fn enter_generation_panics_from_solving() {
        let pm = test_pm();
        let piece = pm.new_piece("test", "before", "after");
        pm.advance(piece.id(), None, PiecePhase::Solving);
        pm.enter_generation(piece.id(), PiecePhase::Fixing);
    }

    #[test]
    fn enter_generation_new_piece_ids_at_nonzero_iteration() {
        let pm = test_pm();
        let p1 = pm.new_piece("first", "a", "b");
        pm.enter_generation(p1.id(), PiecePhase::Forming);
        pm.expect_any_and_set(p1.id(), &[PiecePhase::Forming], PiecePhase::Solving);
        pm.advance(p1.id(), Some(PiecePhase::Solving), PiecePhase::Open);

        let p2 = pm.new_piece("resplit_a", "c", "d");
        let p3 = pm.new_piece("resplit_b", "e", "f");

        assert_ne!(p2.id(), p1.id());
        assert_ne!(p3.id(), p1.id());
        assert!(!pm.phases.contains_key(&p2.id()));
        assert!(!pm.phases.contains_key(&p3.id()));

        pm.enter_generation(p2.id(), PiecePhase::Fixing);
        pm.enter_generation(p3.id(), PiecePhase::Fixing);

        assert_eq!(*pm.phases.get(&p2.id()).unwrap(), PiecePhase::Fixing);
        assert_eq!(*pm.phases.get(&p3.id()).unwrap(), PiecePhase::Fixing);
    }
}
