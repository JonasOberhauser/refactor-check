#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiecePhase {
    Forming,
    Solving,
    Fixing,
    Judging,
    Verified,
    Open,
}