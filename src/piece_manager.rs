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