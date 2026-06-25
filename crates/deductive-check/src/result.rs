use std::path::PathBuf;

use crate::code_piece::FunctionId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedPiece {
    pub file: PathBuf,
    pub function_id: FunctionId,
    pub start_line: u32,
    pub end_line: u32,
    pub unsat_formulas: usize,
    pub sat_formulas: usize,
    pub unknown_formulas: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnverifiedPiece {
    pub file: PathBuf,
    pub function_id: FunctionId,
    pub start_line: u32,
    pub end_line: u32,
    pub sat_models: Vec<String>,
    pub unknown_formulas: Vec<String>,
    pub elaboration: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BugReport {
    pub file: PathBuf,
    pub function_id: FunctionId,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

    pub fn save_to_file(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}