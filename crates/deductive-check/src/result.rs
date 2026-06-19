use std::path::PathBuf;

use crate::code_piece::FunctionId;

#[derive(Debug, Clone)]
pub struct ClosedPiece {
    pub file: PathBuf,
    pub function_id: FunctionId,
    pub start_line: u32,
    pub end_line: u32,
    pub unsat_formulas: usize,
    pub sat_formulas: usize,
    pub unknown_formulas: usize,
}

#[derive(Debug, Clone)]
pub struct UnverifiedPiece {
    pub file: PathBuf,
    pub function_id: FunctionId,
    pub start_line: u32,
    pub end_line: u32,
    pub sat_models: Vec<String>,
    pub unknown_formulas: Vec<String>,
    pub elaboration: String,
}

#[derive(Debug, Clone)]
pub struct BugReport {
    pub file: PathBuf,
    pub function_id: FunctionId,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct VerificationResult {
    pub closed_pieces: Vec<ClosedPiece>,
    pub unverified_pieces: Vec<UnverifiedPiece>,
    pub bug_reports: Vec<BugReport>,
    pub total_functions: usize,
    pub total_pieces: usize,
}

impl VerificationResult {
    pub fn all_verified(&self) -> bool {
        self.unverified_pieces.is_empty()
    }
}