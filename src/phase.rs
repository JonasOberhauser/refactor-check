#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiecePhase {
    Forming,
    Fixing,
    Solving,
    Judging,
    Verified,
    Open,
}
