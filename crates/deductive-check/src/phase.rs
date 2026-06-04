use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodePiecePhase {
    Open,
    GetContext,
    Formalizer,
    Check,
    Judge,
    Closed,
    Unverified,
}

impl fmt::Display for CodePiecePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => write!(f, "Open"),
            Self::GetContext => write!(f, "GetContext"),
            Self::Formalizer => write!(f, "Formalizer"),
            Self::Check => write!(f, "Check"),
            Self::Judge => write!(f, "Judge"),
            Self::Closed => write!(f, "Closed"),
            Self::Unverified => write!(f, "Unverified"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormulaPhase {
    Open,
    Check,
    Fix,
    ClosedUnsat,
    ClosedSat,
    ClosedUnknown,
}

impl FormulaPhase {
    pub fn is_closed(&self) -> bool {
        matches!(
            self,
            Self::ClosedUnsat | Self::ClosedSat | Self::ClosedUnknown
        )
    }

    pub fn is_unsat(&self) -> bool {
        matches!(self, Self::ClosedUnsat)
    }

    pub fn is_sat(&self) -> bool {
        matches!(self, Self::ClosedSat)
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::ClosedUnknown)
    }
}

impl fmt::Display for FormulaPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => write!(f, "Open"),
            Self::Check => write!(f, "Check"),
            Self::Fix => write!(f, "Fix"),
            Self::ClosedUnsat => write!(f, "ClosedUnsat"),
            Self::ClosedSat => write!(f, "ClosedSat"),
            Self::ClosedUnknown => write!(f, "ClosedUnknown"),
        }
    }
}