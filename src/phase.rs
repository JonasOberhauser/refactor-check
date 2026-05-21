use std::sync::LazyLock;

use dashmap::DashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiecePhase {
    Forming,
    Fixing,
    Solving,
    Judging,
    Verified,
    Open,
    Resplitting,
}

static PHASES: LazyLock<DashMap<u64, PiecePhase>> = LazyLock::new(DashMap::new);

pub fn advance(piece_id: u64, from: Option<PiecePhase>, to: PiecePhase) {
    let _ = PHASES
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

pub fn expect_any_and_set(piece_id: u64, valid_from: &[PiecePhase], to: PiecePhase) {
    PHASES
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